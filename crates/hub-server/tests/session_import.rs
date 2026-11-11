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
    enroll_device, login, now_secs, send, session_count, session_row, signed_import,
    signed_import_with_version,
};
use serde_json::{json, Value};

const AGENT: &str = "opencode";
const CWD: &str = "/srv/app";

fn ok_rev(head: &str) -> Value {
    json!({
        "op": "rev",
        "not_a_repo": false,
        "head": head,
        "short": &head[..7],
        "branch": "main",
        "detached": false,
    })
}

fn err_code(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| panic!("错误体不是 JSON：{body}"))
}

/// 一个标准的成功导入：设备在线、上报 `opencode` + `git`、探测回 ok。
async fn import_ok(hub: &common::Hub, stub: &common::DeviceStub, acp: &str, nonce: &str) -> Value {
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
    let app = hub_server::build_app(hub.state.clone());
    let (status, body) = send(&app, signed.request()).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    serde_json::from_str(&body).expect("json")
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
    assert_eq!(err_code(&body), "hub_not_initialized");
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
    assert_eq!(err_code(&body), "invalid_request_body");
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
    assert_eq!(err_code(&body), "device_signature_invalid");

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
    assert_eq!(err_code(&body), "device_signature_invalid");

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
    assert_eq!(err_code(&body), "device_signature_invalid");

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
    assert_eq!(err_code(&body), "device_signature_invalid");

    assert_eq!(session_count(&hub), 0, "认证失败不得落任何会话行");
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

    let (status, body) = send(&app, signed.request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(err_code(&body), "import_replayed");
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
    assert_eq!(err_code(&body), "unsupported_import_provider");

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
    assert_eq!(err_code(&body), "import_replayed");
    assert_eq!(session_count(&hub), 0);
}

// ---------------------------------------------------------------------------
// 4. 去重
// ---------------------------------------------------------------------------

/// 同一 `(node_id, agent, acp_session_id)` 第二次导入 409，且**不再探测**。
#[tokio::test]
async fn a_second_import_of_the_same_acp_session_is_conflict() {
    let (hub, stub, app) = common::boot_online_import_fixture().await;

    let first = import_ok(&hub, &stub, "ses_dup", "nonce-dup-1").await;
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
    assert_eq!(err_code(&body), "import_already_exists");
    assert_eq!(session_count(&hub), 1);
    assert_eq!(
        stub.rev_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "重复导入不得再触发 base_rev 探测"
    );
}

/// 同一个 `ses_*` 属于两台设备各自的 opencode 库：两条都成功。
#[tokio::test]
async fn the_same_acp_id_on_another_device_is_not_a_duplicate() {
    let (hub, a, _app) = common::boot_online_import_fixture().await;
    let b = enroll_device(
        &hub,
        &["git"],
        &[AGENT],
        Some(ok_rev("ffffffffffffffffffffffffffffffffffffffff")),
    )
    .await;

    let first = import_ok(&hub, &a, "ses_shared", "nonce-shared-a").await;
    let second = import_ok(&hub, &b, "ses_shared", "nonce-shared-b").await;
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
        assert_eq!(err_code(&body), "unsupported_import_provider");
    }
    assert_eq!(session_count(&hub), 0);
}

// ---------------------------------------------------------------------------
// 6. 预填 + 对话下发
// ---------------------------------------------------------------------------

/// 导入的会话带 `acp_session_id`，起点探测照常完成；开对话时该 id 非空下发。
#[tokio::test]
async fn imported_session_prefills_acp_session_id_and_conversation_open_downlinks_it() {
    let (hub, stub, _app) = common::boot_online_import_fixture().await;

    let body = import_ok(&hub, &stub, "ses_imported", "nonce-prefill").await;
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
    assert_eq!(err_code(&body), "device_signature_invalid");
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

/// Hub 的 `ImportRequest` 序列化字节必须与 `node-cli` 的 DTO 逐字相同。
///
/// 常量是用 `crates/node-cli/src/import_http.rs` 的字段顺序算出来的：
/// `node_id, provider, external_session_id, acp_session_id, cwd, title, v, t,
/// nonce, signature, body_sha256`。改 Hub DTO 的字段顺序 → 字符串与摘要都变，
/// 这条立刻红。
#[test]
fn the_hub_dto_field_order_matches_the_cli_canonical_body() {
    use sha2::{Digest, Sha256};

    let mut req = hub_server::routes::session_import::ImportRequest {
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
    req.signature.clear();
    req.body_sha256.clear();

    let canonical = serde_json::to_vec(&req).expect("canonical");
    assert_eq!(
        String::from_utf8(canonical.clone()).unwrap(),
        "{\"node_id\":\"n_test\",\"provider\":\"opencode\",\"external_session_id\":\"ses_abc\",\"acp_session_id\":\"ses_abc\",\"cwd\":\"/tmp/work\",\"title\":\"demo\",\"v\":1,\"t\":1700000000,\"nonce\":\"nonce_test\",\"signature\":\"\",\"body_sha256\":\"\"}"
    );
    assert_eq!(
        identity::hex_encode(Sha256::digest(&canonical).as_ref()),
        "3c9f205898433f9306adba7bf95aaa299b8d5796555bd1f16783e492db9d0fc5"
    );
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
    assert_eq!(err_code(&body), "device_signature_invalid");

    let req = Request::builder()
        .method("GET")
        .uri(hub_server::import_auth::IMPORT_PATH)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "body: {body}");
}
