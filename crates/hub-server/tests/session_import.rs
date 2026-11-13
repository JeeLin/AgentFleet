//! v0.7.4 子任务 3：设备签名 HTTP 导入端点的集成测试。
//!
//! 设备侧走真 WS（`common::Hub` + `node_core::conn`）以保持「在线」与应答
//! `git rev` 探测；页面侧走 `oneshot` 直打路由。两边共享同一个 `AppState`。
//!
//! 签名由 `common::signed_import` 构造：canonical body 与 `node-cli` 的
//! `import_http` 同一算法，签名规范串用 `identity::import_signing_message`。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    enroll_device, login, now_secs, ok_rev, send, session_count, session_row, signed_import,
    signed_import_with_version,
};
use hub_server::import_auth::canonical_digest_for_test;
use serde_json::{json, Value};

const AGENT: &str = "opencode";
const CWD: &str = "/srv/app";

/// 从 canonical body 的**原始字节**里按出现顺序抽出顶层字段名。
///
/// `serde_json::Value` 在没有 `preserve_order` 时是 `BTreeMap`，键序恒为升序
/// ——拿它比「顺序」按构造恒真。这里扫的是序列化出来的字节本身。
fn field_order(canonical: &[u8]) -> Vec<String> {
    let text = std::str::from_utf8(canonical).expect("canonical body is utf8");
    text.split("\",\"")
        .skip(1)
        .filter_map(|chunk| {
            let (key, rest) = chunk.split_once("\":")?;
            // 只留顶层键：值的内部字符串里没有 `"<name>":` 形态（本 DTO 全是
            // 字符串 / 数字），故这个启发式对当前协议是精确的。若将来引入嵌套
            // 对象，这条 helper 需要换成真解析器——届时它会先红。
            (!rest.is_empty()).then(|| key.to_string())
        })
        .collect()
}

fn err_code(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| panic!("错误体不是 JSON：{body}"))
}

/// 一个标准的成功导入：设备在线、上报 `opencode` + `git`、探测回 ok。
///
/// 201 体**恰好两个键**：`session_id` / `acp_session_id`。断言放在这里而不是
/// 某个用例里，是因为所有成功路径都经过它——给 `ImportResponse` 加第三个
/// 字段时，这条会立刻变红，而不会像以前那样 14 个用例全绿。
/// 打一条成功的导入请求，返回 201 体。
///
/// `app` 由调用方传入（就是 `boot_online_import_fixture` 的第三项）：这里**不**
/// 自己 `build_app(hub.state.clone())`。旧写法在每次调用时重建一个等价 `Router`，
/// 于是同一个用例里并存 2–3 个实例——它们指向同一个 `AppState`，所以语义没错，
/// 但「请求打到哪个 app」在测试里就有两个来源，将来给 `Router` 加中间件时
/// 只有其中一个会带上。
async fn import_ok(app: &axum::Router, stub: &common::DeviceStub, acp: &str, nonce: &str) -> Value {
    let signed = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        acp,
        CWD,
        "imported",
        now_secs(),
        nonce,
    );
    let (status, body) = send(app, signed.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_exactly_two_response_keys(&parsed, "import_ok");
    parsed
}

/// 201 响应体必须是**对象**且键集**恰好**是那两个，不是「至少包含」。
///
/// 多一个键都是契约变化：CLI 的 `parse_response` 只读 `session_id`，多出来的
/// 字段会静默漂移；少一个键则说明回显契约没了。两边都得红。
fn assert_exactly_two_response_keys(body: &Value, label: &str) {
    let obj = body
        .as_object()
        .unwrap_or_else(|| panic!("{label}: 201 body 不是对象：{body}"));
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["acp_session_id", "session_id"],
        "{label}: 201 体必须恰好两个键"
    );
    for key in ["session_id", "acp_session_id"] {
        assert!(
            obj[key].as_str().is_some_and(|v| !v.is_empty()),
            "{label}: {key} 必须是非空字符串：{body}"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. 未初始化
// ---------------------------------------------------------------------------

/// 未初始化时即使签名正确也 403 `hub_not_initialized`：设备端点不得绕过
/// `INIT_MODE_PATHS` 的语义（它本来就不在那张表里）。
#[tokio::test]
async fn uninitialized_hub_rejects_device_import_with_the_same_forbidden_shape() {
    let hub = common::Hub::uninitialized().await;
    let kp = identity::KeyPair::generate();
    let signed = signed_import(
        &kp,
        "n_any",
        AGENT,
        "ses_x",
        CWD,
        "t",
        now_secs(),
        "nonce-init",
    );
    let app = hub_server::build_app(hub.state.clone());
    let (status, body) = send(&app, signed.request()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::HUB_NOT_INITIALIZED);
}

// ---------------------------------------------------------------------------
// 2. 签名 / 摘要 / 未知设备 / 缺公钥
// ---------------------------------------------------------------------------

/// 签名规范串的第一参数必须是 **body 的 `v`**，验过后才判 Hub 是否支持它。
///
/// 若 Hub 误用 `IMPORT_VERSION` 常量验签，v=2 的有效签名会验不过（401）；
/// 若 Hub 漏判未知版本，v=2 则会走到业务层建会话（201）。正确行为是 400
/// `invalid_request_body`，并且**不消费 nonce**：同 nonce 改回 v=1 重新签名仍能
/// 成功。三件事合起来才证明「版本绑定了签名、未知版本被拒、拒绝不烧 nonce」。
#[tokio::test]
async fn the_signature_binds_body_version_and_an_unknown_version_does_not_burn_the_nonce() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    let nonce = "nonce-version";
    let t = now_secs();

    // v=2 的 body 摘要 / 签名都按 v=2 正确构造，Hub 不该拿 v=1 去验。
    let future = signed_import_with_version(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_future",
        CWD,
        "future",
        t,
        nonce,
        hub_server::import_auth::IMPORT_VERSION + 1,
    );

    let (status, body) = send(&app, future.request()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::BAD_BODY);
    assert_eq!(session_count(&hub), 0, "未知版本不得建会话");

    // 同一个 nonce 改回 v=1 重新签名：若上面已经消费了 nonce，这里会 409。
    let current = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_current",
        CWD,
        "current",
        t,
        nonce,
    );
    let (status, body) = send(&app, current.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_exactly_two_response_keys(&serde_json::from_str(&body).expect("json"), "version retry");
    assert_eq!(session_count(&hub), 1);
}

/// 任何一条认证判据不过都 401 `device_signature_invalid`，且库里没有新会话。
#[tokio::test]
async fn missing_or_invalid_signature_is_unauthorized_and_no_session_row_is_written() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    // 2a. 签名本身是垃圾（body/摘要都自洽，只有 signature 被换掉）。
    let mut bad = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_bad_sig",
        CWD,
        "t",
        now_secs(),
        "nonce-bad-sig",
    );
    bad.parsed.signature = identity::base64url(&[0u8; 64]);
    bad.body = serde_json::to_vec(&bad.parsed).expect("body");
    let (status, body) = send(&app, bad.request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);

    // 2b. body 被改但头里的摘要/签名不动。
    let good = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_tampered",
        CWD,
        "t",
        now_secs(),
        "nonce-tampered",
    );
    let mut req = good.request();
    *req.body_mut() = Body::from(
        String::from_utf8(good.body.clone())
            .unwrap()
            .replace(CWD, "/tmp/other"),
    );
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);

    // 2c. 未知 node（签名本身有效，但库里没有这台设备）。
    let unknown_kp = identity::KeyPair::generate();
    let unknown = signed_import(
        &unknown_kp,
        "n_unknown",
        AGENT,
        "ses_unknown",
        CWD,
        "t",
        now_secs(),
        "nonce-unknown",
    );
    let (status, body) = send(&app, unknown.request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);

    // 2d. 设备在库里但没有公钥（历史数据 / 未登记）。
    common::insert_legacy_device(&hub, "n_nokey");
    let nokey = signed_import(
        &stub.kp,
        "n_nokey",
        AGENT,
        "ses_nokey",
        CWD,
        "t",
        now_secs(),
        "nonce-nokey",
    );
    let (status, body) = send(&app, nokey.request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);

    assert_eq!(session_count(&hub), 0, "认证失败不得落任何会话行");
}

/// 头与 body 的同一事实必须**逐字相等**：四对里任意一对不等都是 401。
///
/// 四条分支各自独立可证伪——把 `import_auth.rs` 的逐字比较删掉或改成
/// 「只看 node_id」，签名仍然有效（body 的摘要与签名都自洽），请求会一路
/// 走到建会话，用例变红。只测「签名坏了」覆盖不到这一层。
/// 「把这个字段改坏」的一步，连同它改的是哪个头一起给出。
type HeaderTamper = (&'static str, fn(&mut common::SignedImport));

#[tokio::test]
async fn a_header_that_disagrees_with_the_signed_body_is_rejected() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let cases: [HeaderTamper; 4] = [
        ("X-AF-Node-Id", |s| s.parsed.node_id.push('x')),
        ("X-AF-Nonce", |s| s.parsed.nonce.push('x')),
        ("X-AF-Signature", |s| s.parsed.signature.push('A')),
        ("X-AF-Body-SHA256", |s| {
            s.parsed.body_sha256 = "0".repeat(64)
        }),
    ];
    for (label, tamper) in cases {
        let mut signed = signed_import(
            &stub.kp,
            &stub.node_id,
            AGENT,
            "ses_header_mismatch",
            CWD,
            "t",
            now_secs(),
            &format!("nonce-head-{label}"),
        );
        // 只改**头**（`request()` 取自 `parsed`），body 字节保持原样。
        let body = signed.body.clone();
        tamper(&mut signed);
        signed.body = body;
        let (status, response) = send(&app, signed.request()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{label}: {response}");
        assert_eq!(
            err_code(&response),
            hub_server::err::DEVICE_SIGNATURE_INVALID,
            "{label}"
        );
    }
    assert_eq!(session_count(&hub), 0, "头与 body 不等不得落任何会话行");
}

/// 时间窗：`t` 早于 / 晚于当前 301 s 都拒，正好 300 s 仍接受。
///
/// 删掉 `import_auth.rs` 的 `abs_diff > TIME_WINDOW_SECS` 判断后，下面两条
/// 「必须拒」的分支会变成 201，用例变红——这条锁住「时间窗真的被校验」。
/// 边界值由 `TIME_WINDOW_SECS` 推出，不写死 300。
#[tokio::test]
async fn a_timestamp_outside_the_window_is_rejected() {
    let (_hub, stub, app) = common::boot_online_import_fixture().await;

    // 边界由常量推出，不写死 300/301：改 `TIME_WINDOW_SECS` 时这条用例跟着走，
    // 不会一边改常量、一边留着按旧值写死的「恰好在窗外」。
    let window = hub_server::import_auth::TIME_WINDOW_SECS as i64;
    for (label, offset) in [("past", -(window + 1)), ("future", window + 1)] {
        let t = (now_secs() as i64 + offset) as u64;
        let signed = signed_import(
            &stub.kp,
            &stub.node_id,
            AGENT,
            "ses_window",
            CWD,
            "t",
            t,
            &format!("nonce-window-{label}"),
        );
        let (status, body) = send(&app, signed.request()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{label}: {body}");
        assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);
    }

    // 边界内侧仍然受理：判据是 `> 300`，不是 `>= 300`。
    let edge = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_window_edge",
        CWD,
        "t",
        now_secs() - hub_server::import_auth::TIME_WINDOW_SECS,
        "nonce-window-edge",
    );
    let (status, body) = send(&app, edge.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
}

/// **坏签名不烧 nonce**：验签失败提前返回，同一个 nonce 随后仍可用于一条
/// 正确签名的请求。
///
/// 把「消费 nonce」挪到验签之前（或干脆在拒绝分支里也消费），第二次请求会
/// 变成 409 `import_replayed`，用例变红。这条锁住「9 步的顺序」。
#[tokio::test]
async fn a_bad_signature_does_not_burn_the_nonce() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    let t = now_secs();

    let mut bad = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_not_burned",
        CWD,
        "t",
        t,
        "nonce-not-burned",
    );
    bad.parsed.signature = identity::base64url(&[0u8; 64]);
    bad.body = serde_json::to_vec(&bad.parsed).expect("body");
    let (status, body) = send(&app, bad.request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");

    // 同一个 nonce、正确签名 → 仍然受理（nonce 没被那次失败消耗掉）。
    let good = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_not_burned",
        CWD,
        "t",
        t,
        "nonce-not-burned",
    );
    let (status, body) = send(&app, good.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(session_count(&hub), 1);
}

/// 版本不匹配（验签通过、`v != IMPORT_VERSION`）回 400，**且不烧 nonce**。
///
/// 这条已有一个用例断言状态码；这里补的是「不消费 nonce」那一半：换成当前
/// 版本的同一 nonce 仍然可用。
#[tokio::test]
async fn an_unknown_version_is_rejected_without_burning_the_nonce() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    let t = now_secs();

    let future_version = hub_server::import_auth::IMPORT_VERSION + 1;
    let ahead = signed_import_with_version(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_version",
        CWD,
        "t",
        t,
        "nonce-version",
        future_version,
    );
    let (status, body) = send(&app, ahead.request()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::BAD_BODY);

    let current = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_version",
        CWD,
        "t",
        t,
        "nonce-version",
    );
    let (status, body) = send(&app, current.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(session_count(&hub), 1);
}

// ---------------------------------------------------------------------------
// 3. nonce 重放
// ---------------------------------------------------------------------------

/// 同一请求原样重发：第二次 409 `import_replayed`，会话仍只有一条。
#[tokio::test]
async fn nonce_cannot_be_replayed() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let signed = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_replay",
        CWD,
        "t",
        now_secs(),
        "nonce-replay",
    );
    let (status, body) = send(&app, signed.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_exactly_two_response_keys(
        &serde_json::from_str(&body).expect("json"),
        "replayed import",
    );

    let (status, body) = send(&app, signed.request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::IMPORT_REPLAYED);
    assert_eq!(session_count(&hub), 1);
}

/// 业务失败也烧 nonce：同一个 nonce 换个（已签名的）body 再试仍是
/// `import_replayed`。这条锁住「nonce 在 handler 之前消费」。
#[tokio::test]
async fn a_business_failure_still_burns_the_nonce() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    // 第一次：provider 不在 allowlist → 409，但 nonce 已被消费。
    let unsupported = signed_import(
        &stub.kp,
        &stub.node_id,
        "claude",
        "ses_burn",
        CWD,
        "t",
        now_secs(),
        "nonce-burn",
    );
    let (status, body) = send(&app, unsupported.request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(
        err_code(&body),
        hub_server::err::UNSUPPORTED_IMPORT_PROVIDER
    );

    // 第二次：换成合法 provider、重新签名，但 nonce 相同 → 重放。
    let retry = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_burn",
        CWD,
        "t",
        now_secs(),
        "nonce-burn",
    );
    let (status, body) = send(&app, retry.request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::IMPORT_REPLAYED);
    assert_eq!(session_count(&hub), 0);
}

// ---------------------------------------------------------------------------
// 4. 去重
// ---------------------------------------------------------------------------

/// 同一 `(node_id, agent, acp_session_id)` 第二次导入 409，且**不再探测**。
#[tokio::test]
async fn a_second_import_of_the_same_acp_session_is_conflict() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let first = import_ok(&app, &stub, "ses_dup", "nonce-dup-1").await;
    assert_eq!(first["acp_session_id"], "ses_dup");
    assert_eq!(stub.rev_count.load(std::sync::atomic::Ordering::SeqCst), 1);

    let second = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_dup",
        CWD,
        "t",
        now_secs(),
        "nonce-dup-2",
    );
    let (status, body) = send(&app, second.request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::IMPORT_ALREADY_EXISTS);
    assert_eq!(session_count(&hub), 1);
    assert_eq!(
        stub.rev_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "重复导入不得再触发 base_rev 探测"
    );
}

/// 两个**不同 nonce** 的并发请求带同一 `(node_id, agent, acp_session_id)`：
/// 去重是单条原子 INSERT，最多一个 201、另一个 409，库里只能有一行。
///
/// # 这条用例能证伪什么、不能证伪什么
///
/// 它锁的是**端点语义**：两个并发导入最终必须是一个 201 + 一个 409，
/// 且库里只有一行（可以挡住「重复返回 201」「两个都 500」这类回归）。
///
/// 它**证伪不了**「去重是不是原子的」：导入 handler 在落库之前没有 `.await`
/// （`create_session_guarded` 全程同步，`probe_base_rev` 在落行之后），所以
/// 两个请求在同一个 runtime 上不可能交错到 `WHERE NOT EXISTS` 里。换回旧的
/// check-then-insert 这条用例仍绿（MUT3 实测）——那一层的证伪在
/// `hub-storage` 的 `concurrent_creates_of_one_imported_key_never_error`，
/// 它用两条真连接 + 真线程。
///
/// 用多线程 runtime 只是让它更接近生产（多个 worker 线程），不作为证伪手段。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_imports_of_one_acp_session_create_exactly_one_row() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    let t = now_secs();
    let first = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_race",
        CWD,
        "race-1",
        t,
        "nonce-race-1",
    );
    let second = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_race",
        CWD,
        "race-2",
        t,
        "nonce-race-2",
    );
    let app2 = app.clone();

    // 让两个请求尽量同时起步：各自在独立任务里先做一次让出，再由 barrier
    // 放行——否则一个可能在另一个被调度前就跑完。
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let (r1, r2) = tokio::join!(
        async {
            b1.wait().await;
            send(&app, first.request()).await
        },
        async {
            b2.wait().await;
            send(&app2, second.request()).await
        }
    );
    let mut created = 0;
    let mut conflict = 0;
    for (status, body) in [&r1, &r2] {
        match *status {
            StatusCode::CREATED => {
                assert_exactly_two_response_keys(
                    &serde_json::from_str(body).expect("json"),
                    "concurrent import",
                );
                created += 1;
            }
            StatusCode::CONFLICT => {
                assert_eq!(
                    err_code(body),
                    hub_server::err::IMPORT_ALREADY_EXISTS,
                    "body: {body}"
                );
                conflict += 1;
            }
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert_eq!(
        created, 1,
        "exactly one import may create a session: {r1:?} {r2:?}"
    );
    assert_eq!(
        conflict, 1,
        "the loser must be a dedup conflict: {r1:?} {r2:?}"
    );
    assert_eq!(session_count(&hub), 1, "no duplicate session row");
}

/// 同一个 `ses_*` 属于两台设备各自的 opencode 库：两条都成功。
#[tokio::test]
async fn the_same_acp_id_on_another_device_is_not_a_duplicate() {
    let (hub, a, app) = common::boot_online_import_fixture().await;
    let b = enroll_device(
        &hub,
        &["git"],
        &[AGENT],
        Some(ok_rev("ffffffffffffffffffffffffffffffffffffffff")),
    )
    .await;

    let first = import_ok(&app, &a, "ses_shared", "nonce-shared-a").await;
    let second = import_ok(&app, &b, "ses_shared", "nonce-shared-b").await;
    assert_ne!(first["session_id"], second["session_id"]);
    assert_eq!(session_count(&hub), 2);
}

// ---------------------------------------------------------------------------
// 5. provider allowlist
// ---------------------------------------------------------------------------

/// 8 个已知无 ACP 入口的 agent + 1 个未知 id 全部 409，且没有会话行。
#[tokio::test]
async fn unsupported_provider_is_conflict_before_any_db_or_session_write() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let unsupported = [
        "claude",
        "gemini",
        "codex",
        "cursor-agent",
        "aider",
        "crush",
        "goose",
        "qwen",
        "not-a-real-agent",
    ];
    for (i, provider) in unsupported.iter().enumerate() {
        let signed = signed_import(
            &stub.kp,
            &stub.node_id,
            provider,
            "ses_unsupported",
            CWD,
            "t",
            now_secs(),
            &format!("nonce-unsupported-{i}"),
        );
        let (status, body) = send(&app, signed.request()).await;
        assert_eq!(status, StatusCode::CONFLICT, "provider {provider}: {body}");
        assert_eq!(
            err_code(&body),
            hub_server::err::UNSUPPORTED_IMPORT_PROVIDER
        );
    }
    assert_eq!(session_count(&hub), 0);
}

// ---------------------------------------------------------------------------
// 5b. 导入端点自己的门禁接线（不靠 `api.rs` 的存量单测兜底）
// ---------------------------------------------------------------------------

/// 三个门禁必须**从导入端点这一侧**各断言一次：
/// `validate_path` / `device_offline` / `agent_unavailable`。
///
/// 以前 12 次 `enroll_device` 全传可用的 `opencode`，13 次请求全传
/// `/srv/app`，设备全程在线——把共享 helper 里的门禁删掉，本文件仍全绿，
/// 只有 `api.rs` 的单测会红，而「导入端点有没有接上」没有任何断言。
/// 每条都顺带断言**不落会话行**和**不触发起点探测**（门禁在 INSERT 之前）。
fn signed_import_for_gate(
    stub: &common::DeviceStub,
    cwd: &str,
    acp: &str,
    nonce: &str,
) -> common::SignedImport {
    signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        acp,
        cwd,
        "gate",
        now_secs(),
        nonce,
    )
}

async fn assert_gate_conflict(
    hub: &common::Hub,
    stub: &common::DeviceStub,
    app: &axum::Router,
    signed: &common::SignedImport,
    expected: &str,
) {
    let (status, body) = send(app, signed.request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(err_code(&body), expected, "body: {body}");
    assert_eq!(session_count(hub), 0, "门禁失败不得落会话行");
    assert_eq!(
        stub.rev_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "门禁失败不得触发 base_rev 探测"
    );
}

/// Hub 侧非法路径：盘符 / 相对路径都不是 `/` 或 `~` 开头，400。
///
/// 注意这个 code 是 **400** `invalid_workspace_path`，与另外两条 409 分开：
/// 请求形状本身错了，重试一百次也不会变对。
#[tokio::test]
async fn import_endpoint_rejects_a_hub_invalid_workspace_path() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    for (i, cwd) in ["C:\\Users\\x", "relative/path"].iter().enumerate() {
        let signed = signed_import_for_gate(&stub, cwd, "ses_bad_path", &format!("nonce-path-{i}"));
        let (status, body) = send(&app, signed.request()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "cwd {cwd}: {body}");
        assert_eq!(
            err_code(&body),
            hub_server::err::INVALID_WORKSPACE_PATH,
            "cwd {cwd}: {body}"
        );
        assert_eq!(session_count(&hub), 0);
    }
    assert_eq!(stub.rev_count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// agent 不可用（`agent_runtime` 里 `available = 0`）→ 409 `agent_unavailable`。
/// 走 `replace_detected` 而不是改心跳内存：门禁读的是**库**，这正是它不读
/// `Reports` 的那条约束。
#[tokio::test]
async fn import_endpoint_rejects_an_unavailable_agent() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    hub.state
        .agent_runtime()
        .replace_detected(
            &stub.node_id,
            &[(
                AGENT.to_string(),
                AGENT.to_string(),
                Some("0.0.1".to_string()),
                false,
                Some("binary not found".to_string()),
            )],
            "unix:1700000000",
        )
        .expect("replace_detected");
    let signed = signed_import_for_gate(&stub, CWD, "ses_no_agent", "nonce-no-agent");
    assert_gate_conflict(
        &hub,
        &stub,
        &app,
        &signed,
        hub_server::err::AGENT_UNAVAILABLE,
    )
    .await;
}

/// 设备离线（心跳已过 `OFFLINE_AFTER`）→ 409 `device_offline`。
///
/// 直接往 `Reports` 里写一份过期上报：判离线只认心跳年龄，不看 WS 是否还连着，
/// 所以 WS 桩照样活着也不影响这条门禁；也不需要真的 sleep 45 秒。
#[tokio::test]
async fn import_endpoint_rejects_an_offline_device() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    let stale = hub_core::reports::Report {
        node_version: "0.1.0".to_string(),
        capabilities: serde_json::json!({"connectors": ["git"], "agents": []}),
        resources: serde_json::Value::Null,
        workspace_roots: Vec::new(),
        at: std::time::Instant::now()
            - hub_core::reports::OFFLINE_AFTER
            - std::time::Duration::from_secs(1),
    };
    hub.state.reports().record(&stub.node_id, stale);
    let signed = signed_import_for_gate(&stub, CWD, "ses_offline", "nonce-offline");
    assert_gate_conflict(&hub, &stub, &app, &signed, "device_offline").await;
}

// ---------------------------------------------------------------------------
// 6. 预填 + 对话下发
// ---------------------------------------------------------------------------

/// 导入的会话带 `acp_session_id`，起点探测照常完成；开对话时该 id 非空下发。
#[tokio::test]
async fn imported_session_prefills_acp_session_id_and_conversation_open_downlinks_it() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let body = import_ok(&app, &stub, "ses_imported", "nonce-prefill").await;
    let session_id = body["session_id"].as_str().expect("session_id").to_string();

    let (agent_id, acp, status) = session_row(&hub, &session_id);
    assert_eq!(agent_id.as_deref(), Some(AGENT));
    assert_eq!(acp.as_deref(), Some("ses_imported"));
    assert_eq!(status, "ok", "导入也走同一条 base_rev 探测链路");

    // 用浏览器 cookie 开对话：设备侧收到的 `open_channel` 参数里必须带这个 id。
    let (session, csrf) = login(&hub).await;
    let app = hub_server::build_app(hub.state.clone());
    let req = common::browser_request(
        "POST",
        &format!("/api/sessions/{session_id}/conversation"),
        &session,
        &csrf,
        true,
        None,
    );
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::CREATED, "conversation open: {body}");

    // 帧是先记账再下发、下发成功就回 201，所以设备侧任务可能还没处理完这一帧。
    common::wait_until(std::time::Duration::from_secs(2), || {
        stub.open_calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.connector == "acp")
    })
    .await;
    let calls = stub.open_calls.lock().unwrap();
    let acp_call = calls
        .iter()
        .find(|c| c.connector == "acp")
        .expect("设备必须收到 acp 的 open_channel");
    assert_eq!(
        acp_call.params["acp_session_id"], "ses_imported",
        "acp_session_id 必须原样下发，设备据此走 session/load"
    );
}

// ---------------------------------------------------------------------------
// 7. 浏览器认证 / CSRF 规则不变
// ---------------------------------------------------------------------------

/// `/api/*` 仍要求 session + CSRF；设备端点不带 cookie 也**不是**
/// `missing_session`，而是设备签名的 401。
#[tokio::test]
async fn browser_session_and_csrf_rules_are_unchanged() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;
    let (session, csrf) = login(&hub).await;

    // 无 cookie 的 /api/* 仍是 401 missing_session。
    let req = Request::builder()
        .method("POST")
        .uri("/api/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"node_id": stub.node_id, "path": CWD, "agent_id": AGENT}).to_string(),
        ))
        .unwrap();
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), "missing_session");

    // 有 cookie 但没有 CSRF 头：403。
    let req = common::browser_request(
        "POST",
        "/api/sessions",
        &session,
        &csrf,
        false,
        Some(json!({"node_id": stub.node_id, "path": CWD, "agent_id": AGENT})),
    );
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(err_code(&body), "csrf_token_missing_or_mismatch");

    // 设备端点带 cookie 但没有签名头：401 device_signature_invalid，不是
    // missing_session（说明它没走浏览器认证层）。
    let req = common::browser_request(
        "POST",
        hub_server::import_auth::IMPORT_PATH,
        &session,
        &csrf,
        true,
        None,
    );
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);
}

// ---------------------------------------------------------------------------
// 8. 豁免表 / 消息类型数不变
// ---------------------------------------------------------------------------

/// 三张表逐字不变，`MessageType` 仍 17 个变体。
///
/// 变体数用**穷尽 match** 锁：新增变体会让这个函数编译失败，而不是只让一个
/// 计数断言红掉。
#[test]
fn exempt_tables_and_message_type_count_are_unchanged() {
    use hub_server::routes::{AUTH_EXEMPT_PATHS, CSRF_EXEMPT_PATHS, INIT_MODE_PATHS};

    assert_eq!(
        AUTH_EXEMPT_PATHS,
        &["/healthz", "/setup", "/api/setup", "/login", "/ws"]
    );
    assert_eq!(
        INIT_MODE_PATHS,
        &["/healthz", "/setup", "/api/setup", "/ws"]
    );
    assert_eq!(CSRF_EXEMPT_PATHS, &["/setup", "/login"]);

    use protocol::message::MessageType::*;
    fn index(t: protocol::message::MessageType) -> usize {
        match t {
            Hello => 0,
            Challenge => 1,
            Auth => 2,
            AuthOk => 3,
            AuthFail => 4,
            HubHello => 5,
            Heartbeat => 6,
            HeartbeatAck => 7,
            Disconnect => 8,
            OpenChannel => 9,
            ChannelReady => 10,
            ChannelError => 11,
            ChannelData => 12,
            ChannelCredit => 13,
            CloseChannel => 14,
            Err => 15,
            Unknown => 16,
        }
    }
    let all = [
        Hello,
        Challenge,
        Auth,
        AuthOk,
        AuthFail,
        HubHello,
        Heartbeat,
        HeartbeatAck,
        Disconnect,
        OpenChannel,
        ChannelReady,
        ChannelError,
        ChannelData,
        ChannelCredit,
        CloseChannel,
        Err,
        Unknown,
    ];
    let unique: std::collections::BTreeSet<usize> = all.iter().map(|t| index(*t)).collect();
    assert_eq!(all.len(), 17);
    assert_eq!(unique.len(), 17);

    // 上面这两个 17 由**本地**手写的 `index()` 与本地数组决定，对「协议仍是
    // 17 个变体」这条事实按构造恒真。真正锁住它的是**穷尽 match**：新增第 18
    // 个变体时 `index()` 缺臂 → 编译失败。下面用一段 `match` 把每个变体的
    // 线上名字写死——这才对「变体集合」本身可证伪（改名 / 加变体都会编译失败
    // 或断言失败），而不是数一遍自己写的数组。
    let names: Vec<&str> = [
        Hello,
        Challenge,
        Auth,
        AuthOk,
        AuthFail,
        HubHello,
        Heartbeat,
        HeartbeatAck,
        Disconnect,
        OpenChannel,
        ChannelReady,
        ChannelError,
        ChannelData,
        ChannelCredit,
        CloseChannel,
        Err,
        Unknown,
    ]
    .iter()
    .map(|t| match t {
        // 线上名字取 `protocol::message::types` 的常量，不手抄：改名会让这一
        // 段跟着变（常量是唯一真相源），新增变体则让 match 缺臂、编译失败。
        Hello => protocol::message::types::HELLO,
        Challenge => protocol::message::types::CHALLENGE,
        Auth => protocol::message::types::AUTH,
        AuthOk => protocol::message::types::AUTH_OK,
        AuthFail => protocol::message::types::AUTH_FAIL,
        HubHello => protocol::message::types::HUB_HELLO,
        Heartbeat => protocol::message::types::HEARTBEAT,
        HeartbeatAck => protocol::message::types::HEARTBEAT_ACK,
        Disconnect => protocol::message::types::DISCONNECT,
        OpenChannel => protocol::message::types::OPEN_CHANNEL,
        ChannelReady => protocol::message::types::CHANNEL_READY,
        ChannelError => protocol::message::types::CHANNEL_ERROR,
        ChannelData => protocol::message::types::CHANNEL_DATA,
        ChannelCredit => protocol::message::types::CHANNEL_CREDIT,
        CloseChannel => protocol::message::types::CLOSE_CHANNEL,
        Err => protocol::message::types::ERR,
        // `Unknown` 没有线上名字：它是「解析不出来的兜底」。
        Unknown => "",
    })
    .collect();
    assert_eq!(names.len(), 17);
    // 16 个有线上名字的变体两两不同，且都不是空串。
    let named: std::collections::BTreeSet<&str> =
        names.iter().copied().filter(|n| !n.is_empty()).collect();
    assert_eq!(named.len(), 16, "16 个变体各有唯一线上名字");
    assert_eq!(names.iter().filter(|n| n.is_empty()).count(), 1);
}

// ---------------------------------------------------------------------------
// 9. 普通建会话仍写 NULL
// ---------------------------------------------------------------------------

/// 页面路径建的会话 `acp_session_id` 必须是 `NULL`（导入初值不泄漏过去）。
#[tokio::test]
async fn ordinary_create_session_still_writes_null_acp_session_id() {
    let (hub, _stub, app) = common::boot_online_import_fixture().await;
    let (session, csrf) = login(&hub).await;

    let req = common::browser_request(
        "POST",
        "/api/sessions",
        &session,
        &csrf,
        true,
        Some(json!({"node_id": _stub.node_id, "path": CWD, "agent_id": AGENT})),
    );
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let created: Value = serde_json::from_str(&body).expect("json");
    let id = created["id"].as_str().expect("id");
    assert_eq!(session_row(&hub, id).1, None);
}

// ---------------------------------------------------------------------------
// 10. DTO 字段顺序（与 CLI 的 canonical body 对齐）
// ---------------------------------------------------------------------------

/// Hub 与 CLI 的 canonical body 必须逐字相同。
///
/// 旧版只拿 Hub 的 DTO 跟硬编码字面量比、名字却写「matches the CLI」：
/// CLI 侧把 `cwd` / `title` 顺序对调，这条仍全绿，真实请求却以 401 收场。
/// 现在两侧都调 [`ImportRequest::body_digest_hex`]（`identity` 里唯一一份实现），
/// 且把 CLI 的字段顺序做成同一 DTO 的序列化串与摘要常量一起锁住：
/// 顺序、字段名、摘要三者任一漂移都会红，且不再有「另一侧没读」这个名字漏洞。
#[test]
fn the_cli_and_hub_share_one_canonical_body_digest() {
    let req = hub_server::routes::session_import::ImportRequest {
        node_id: "n_test".to_string(),
        provider: "opencode".to_string(),
        external_session_id: "ses_abc".to_string(),
        acp_session_id: "ses_abc".to_string(),
        cwd: "/tmp/work".to_string(),
        title: "demo".to_string(),
        v: 1,
        t: 1_700_000_000,
        nonce: "nonce_test".to_string(),
        signature: String::new(),
        body_sha256: String::new(),
    };

    // `canonical_body` 是 CLI `build_signed_body` 与 Hub `canonical_digest`
    // 共用的那一份字节。改 DTO 字段顺序 → 字符串与摘要同时变。
    let canonical = req.canonical_body().expect("canonical");
    let canonical_text = String::from_utf8(canonical.clone()).unwrap();
    assert_eq!(
        canonical_text,
        "{\"node_id\":\"n_test\",\"provider\":\"opencode\",\"external_session_id\":\"ses_abc\",\"acp_session_id\":\"ses_abc\",\"cwd\":\"/tmp/work\",\"title\":\"demo\",\"v\":1,\"t\":1700000000,\"nonce\":\"nonce_test\",\"signature\":\"\",\"body_sha256\":\"\"}"
    );
    assert_eq!(
        req.body_digest_hex().expect("digest"),
        "3c9f205898433f9306adba7bf95aaa299b8d5796555bd1f16783e492db9d0fc5"
    );

    // 真的走一遍 CLI 的 `build_signed_body`：它构造同一个 `ImportRequest`，
    // 摘要必须与 Hub 用同一函数重算的值相等（不是只比一串手抄常量）。
    let kp = identity::KeyPair::generate();
    let candidate = node_core::import::ImportCandidate {
        provider: "opencode",
        acp_session_id: "ses_abc".to_string(),
        cwd: "/tmp/work".to_string(),
    };
    let signed =
        node_cli::import_http::build_signed_body(&kp, "n_test", &candidate, "demo").expect("sign");
    let cli: hub_server::routes::session_import::ImportRequest =
        serde_json::from_slice(&signed.body).expect("cli body");
    assert_eq!(
        cli.body_digest_hex().expect("cli digest"),
        signed.body_sha256
    );
    // Hub 侧重算的必须是同一个值：`canonical_digest` 是验签 layer 真正调的
    // 那个函数（经 `canonical_digest_for_test` 暴露），不是测试另抄的一份。
    assert_eq!(
        canonical_digest_for_test(&cli).as_deref(),
        Some(signed.body_sha256.as_str()),
        "Hub 验签重算的摘要必须与 CLI 签名时的摘要一致"
    );
    assert_eq!(signed.body_sha256.len(), 64, "hex sha256");
    // 「字段顺序一致」**不能**靠比较两个 `serde_json::Value` 的键序：本仓没开
    // `preserve_order`，`Map` 在无该 feature 时是 `BTreeMap`，两侧键序恒为同一
    // 升序——那两条断言按构造恒真。真正锁顺序的是上面那条 canonical body 的
    // 字符串字面量（字段名与顺序都写在里面）与摘要常量：任一漂移都会红。
    //
    // 这里补的是**可证伪**的那一半：CLI 与 Hub 的 canonical body **字段顺序**
    // 相同。注意不能解析成 `Value` 再比键序（无 `preserve_order` 时两侧都是
    // 同一 `BTreeMap` 升序，恒真），也不能期望两串字节相等——CLI 的 `t` /
    // `nonce` 是本次请求的真实值，与这里的固定夹具必然不同。
    //
    // 所以从**原始字节**里按出现顺序抽出顶层字段名：这才是序列化顺序本身。
    let cli_canonical = cli.canonical_body().expect("cli canonical");
    assert_eq!(
        field_order(&cli_canonical),
        field_order(&canonical),
        "CLI 与 Hub 的 canonical body 字段顺序必须一致"
    );
    // 字段集合再断言一次（少一个字段时上面的解析已失败，这条给出直观定位）。
    let cli_value: Value = serde_json::from_slice(&signed.body).expect("json");
    let mut cli_keys: Vec<&str> = cli_value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    cli_keys.sort_unstable();
    let mut expected_keys = vec![
        "acp_session_id",
        "body_sha256",
        "cwd",
        "external_session_id",
        "node_id",
        "nonce",
        "provider",
        "signature",
        "t",
        "title",
        "v",
    ];
    expected_keys.sort_unstable();
    assert_eq!(cli_keys, expected_keys, "body 字段集合");
}

// ---------------------------------------------------------------------------
// 11. 路由不在浏览器认证层之下
// ---------------------------------------------------------------------------

/// 有效签名 + 无 cookie → 201；只有 cookie + 无签名 → 401
/// `device_signature_invalid`；`GET` → 405（不是 401、不是 SPA 的 200）。
#[tokio::test]
async fn device_import_route_is_not_under_the_browser_auth_layer() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let signed = signed_import(
        &stub.kp,
        &stub.node_id,
        AGENT,
        "ses_route",
        CWD,
        "t",
        now_secs(),
        "nonce-route",
    );
    let (status, body) = send(&app, signed.request()).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "无 cookie 也必须能导入：{body}"
    );
    assert_exactly_two_response_keys(
        &serde_json::from_str(&body).expect("json"),
        "no-cookie route",
    );

    let (session, csrf) = login(&hub).await;
    let req = common::browser_request(
        "POST",
        hub_server::import_auth::IMPORT_PATH,
        &session,
        &csrf,
        true,
        None,
    );
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(err_code(&body), hub_server::err::DEVICE_SIGNATURE_INVALID);

    let req = Request::builder()
        .method("GET")
        .uri(hub_server::import_auth::IMPORT_PATH)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "body: {body}");
}
