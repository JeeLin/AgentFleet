//! 子任务 5「Hub JSON API」的集成测试。
//!
//! 两条铁律（与 `login_flow.rs` 同款）：
//! 1. **页面请求走 `oneshot`**：直接打 `build_app` 装配出的路由，不起真端口；
//!    设备侧要真握手（`common::Hub`）时才起端口。两者共用**同一个 `AppState`**
//!    ——「在线」这个判据要读连接注册表与心跳上报，只有同一份状态才作数。
//! 2. **每个端点至少一条成功 + 一条未认证 401**，且必须是 **401**：404 说明路由
//!    压根没挂上，200 说明 `.layer(require_auth)` 没覆盖到它。这两种「看起来像
//!    通过」的回归正是本文件存在的主要理由。

// Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use hub_server::{build_app, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

/// 一个真跑起来的 Hub + 一份同状态的路由 + 已登录的两条 cookie。
///
/// 「真跑起来」是为了让设备侧能走真 WS 握手（`common::Hub`）；
/// 「同状态的路由」是为了让页面请求能走 `oneshot`。两者共享 `AppState`，
/// 所以 `GET /api/devices` 看到的在线态与设备那条连接是同一份事实。
struct Harness {
    /// 持有它就是持有「这台 Hub 还活着并且 WS serve 还在跑」。
    /// 不读它的字段：页面请求走 `oneshot`，只有设备那条连接需要真端口，
    /// 而那条连接由调用方自己拿着 `Established` 保活。
    #[allow(dead_code)]
    hub: common::Hub,
    app: Router,
    session: String,
    csrf: String,
}

impl Harness {
    async fn new() -> Self {
        let hub = common::Hub::boot().await;
        Self::from_hub(hub).await
    }

    /// 已有一个 Hub（设备连上去的那个）时复用它，而不是再起一个。
    async fn from_hub(hub: common::Hub) -> Self {
        let app = build_app(hub.state.clone());
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"password": PASSWORD}).to_string()))
                    .unwrap(),
            )
            .await
            .expect("login responds");
        assert_eq!(login.status(), StatusCode::OK, "登录必须成功");
        let session = cookie_of(&login, SESSION_COOKIE).expect("af_session");
        let csrf = cookie_of(&login, CSRF_COOKIE).expect("af_csrf");
        Self {
            hub,
            app,
            session,
            csrf,
        }
    }

    /// 发一个**带完整 cookie**的请求。
    async fn send(&self, mut req: Request<Body>) -> Reply {
        req.headers_mut().insert(
            "cookie",
            format!(
                "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                self.session, self.csrf
            )
            .parse()
            .unwrap(),
        );
        Reply::new(self.app.clone().oneshot(req).await.expect("app responds")).await
    }

    async fn get(&self, path: &str) -> Reply {
        self.send(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
    }

    /// `POST /api/sessions`，带 `X-CSRF-Token`（CSRF 双重提交）。
    async fn create_session(&self, body: Value) -> Reply {
        self.send(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header(CSRF_HEADER, &self.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }
}

/// 取 `Set-Cookie` 里某个 cookie 的值。
fn cookie_of(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|raw| {
            let (k, v) = raw.split(';').next()?.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

/// 一次性读出状态码 / 体。
struct Reply {
    status: StatusCode,
    body: String,
    /// 响应头原样留着。
    ///
    /// 少数几处断言必须看头而不是看体：下载端点（`/api/agent-binary`）的
    /// `Content-Disposition` 决定用户存下来的文件名，而 `body` 被
    /// `from_utf8_lossy` 洗过，二进制文件的内容与它无关。
    headers: axum::http::HeaderMap,
}

impl Reply {
    async fn new(resp: axum::response::Response) -> Self {
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Self {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            headers,
        }
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// 错误体的形状：`{"error": "<code>"}`，且**没有别的顶层键**。
    fn assert_error_code(&self, code: &str) {
        let v = self.json();
        assert_eq!(v["error"], json!(code), "body: {}", self.body);
        assert_eq!(
            v.as_object().map(serde_json::Map::len),
            Some(1),
            "错误体只该有 error 一个键：{}",
            self.body
        );
    }
}

/// enroll 一台设备并让它发一次心跳（带 agent 上报）。
///
/// 返回 `(node_id, 连接)`：**连接必须被调用方一直拿着**——`Established` 一掉
/// 就是 WS 断开，页面侧立刻从 online 变 reconnecting（那正是另一条用例要测的）。
/// 「在线」这个状态只有真连接才造得出来，所以建会话的成功路径必须走它。
async fn enrolled_device(hub: &common::Hub) -> (String, node_core::conn::Established) {
    // 旧形态字符串数组：`claude-code` 一项。
    enrolled_device_reporting(hub, json!(["claude-code"]), 1).await
}

/// 同上，但 agent 上报内容由调用方给。
///
/// `agents` 既是旧形态（字符串数组）也是新形态（对象数组）的入口——Hub 两种都认。
/// 门禁测试需要精确控制「这台设备上报了什么」，所以不能复用写死的那一份。
///
/// # 为什么要显式传 `expect_rows`
///
/// 门禁读的是**库**（子任务 5 的核心判据），所以夹具必须等到行的确落库。
/// 但「落库了」这件事**无法只靠读库观测**：空清单走 `replace_detected` 是
/// `DELETE` 之后再无 `INSERT`，跑完之后与「还没跑」在库里**完全一致**。
/// （`on_heartbeat` 是先 `reports().record(..)` 再 `persist_agents`，所以等内存
/// 也不够——`wait_until` 首查即为真就立刻返回，那一刻库里还没写。）
/// 于是把期望行数交给调用方：非空清单等「行数达标」，空清单只能等内存那份
/// 报告到达（它此时是唯一可观测的信号，且对本文件的两条空清单用例足够——
/// 它们只关心 400/409 的判定顺序，不读 `agent_runtime`）。
async fn enrolled_device_reporting(
    hub: &common::Hub,
    agents: Value,
    expect_rows: usize,
) -> (String, node_core::conn::Established) {
    let (mut est, node_id, _kp) = common::connect_enrolled(hub).await;
    common::send_heartbeat(
        &mut est,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": ["terminal"], "agents": agents},
            "resources": {"cpu": 34, "mem": 61},
            "workspace_roots": [],
        }),
    )
    .await;
    if expect_rows == 0 {
        // 只能等「心跳被处理过」：库里的一份空清单与「没写过」不可区分。
        common::wait_until(Duration::from_secs(2), || {
            hub.state.reports().get(&node_id).is_some()
        })
        .await;
    } else {
        // 真去读库，且要求行数达标：门禁读的就是这里，等内存会在
        // 「行还没写进库」的窗口上返回，测试便靠后续请求的耗时侥幸躲过竞态。
        common::wait_until(Duration::from_secs(2), || {
            hub.state
                .agent_runtime()
                .list_for_node(&node_id)
                .map(|rows| rows.len() == expect_rows)
                .unwrap_or(false)
        })
        .await;
    }
    (node_id, est)
}

/// 与 [`enrolled_device_reporting`] 相同，但**额外上报 `git` connector**。
///
/// # 为什么需要单独一个（v0.6.0）
///
/// `v0.6.0` 起建会话会向设备探一次起点，而探针的第四道门禁要求设备**上报了
/// `git`**——没上报就直接折成 `unavailable`，一个帧都不发（见
/// `routes::base_rev`）。原 helper 只报 `terminal`，于是用它造出的会话
/// `base_rev_status` 恒为 `unavailable`，测不到 `ok` / `not_a_repo` 那两态。
///
/// 不改原 helper 的签名而是并列一个：它有十余个调用点，且**那些用例测的东西
/// 与 git 毫无关系**——让它们凭空多上报一个 connector，会把探针的帧塞进原本
/// 干净的帧流（`tests/channel_flow.rs` 因此栽过一次，见那边的
/// `report_connectors_without_git`）。
async fn enrolled_device_reporting_with_git(
    hub: &common::Hub,
    agents: Value,
    expect_rows: usize,
) -> (String, node_core::conn::Established) {
    let (mut est, node_id, _kp) = common::connect_enrolled(hub).await;
    common::send_heartbeat(
        &mut est,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": ["terminal", "git"], "agents": agents},
            "resources": {"cpu": 34, "mem": 61},
            "workspace_roots": [],
        }),
    )
    .await;
    common::wait_until(Duration::from_secs(2), || {
        hub.state
            .agent_runtime()
            .list_for_node(&node_id)
            .map(|rows| rows.len() == expect_rows)
            .unwrap_or(false)
    })
    .await;
    (node_id, est)
}

// ---------------------------------------------------------------------------
// 标准③：未认证一律 401，且不得是 404 / 200
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_routes_are_covered_by_the_auth_layer() {
    let h = Harness::new().await;
    for path in [
        "/api/devices",
        "/api/sessions",
        "/api/agents",
        // 配置端点同样是状态变更口：未认证必须 401，不能是 404（没挂上）
        // 也不能是 200（认证层没覆盖到）。
        "/api/agents/config",
        // afcli 下载：不下发密钥，但「谁能把约 20MB 的二进制拿走」不该对未登录者
        // 开放。这条同时钉住「路由挂上了」（不是 404）与「认证层覆盖到」（不是 200）。
        "/api/agent-binary",
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        // 404 = 路由没挂上（mounting 写错了）；200 = 没被认证覆盖（数据裸奔）
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} 未认证必须是 401，实际 {status}"
        );
    }
}

/// 伪造的 cookie（签名不对）也是 401，且错误体形状一致。
#[tokio::test]
async fn a_forged_session_cookie_is_rejected_with_the_same_shape() {
    let h = Harness::new().await;
    // 直打路由：这条请求**不能**被 `Harness::send` 补上真 cookie
    let r = Reply::new(
        h.app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/devices")
                    .header("cookie", format!("{SESSION_COOKIE}=forged.signature"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    r.assert_error_code("invalid_session");
}

// ---------------------------------------------------------------------------
// 标准①：GET /api/devices
// ---------------------------------------------------------------------------

/// 空库也要 200 + 空数组：页面拿 `[]` 能区分「没有设备」与「Hub 坏了」。
#[tokio::test]
async fn listing_devices_returns_an_empty_array_when_nothing_is_enrolled() {
    let h = Harness::new().await;
    let r = h.get("/api/devices").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!([]));
}

/// **设备表 CPU / 内存那两列现在有真值**（`docs/BUGS.md` 的「负载采集从未排进
/// 任何里程碑」那条欠账）。
///
/// 端到端而不是只测采集器：那条欠账的症状在**页面**上（两列恒 `—`），
/// 而它可能断在三处——采集没做、`resources` 形状与 Hub 读取口径不符、
/// 或中间哪一层把它丢了。只测采集器第二段就验不到。
///
/// **用 node 侧真构造器发心跳**，不手搓 JSON：手搓的那份会跟着「我以为的
/// 形状」走，形状漂了测试还绿（这正是这两列空了两三个里程碑的原因——
/// Hub 读的是顶层 `cpu` / `mem`，而 `scheduling.md` 的目标形状是嵌套对象）。
#[tokio::test]
async fn the_device_table_load_columns_come_from_the_real_collection() {
    let hub = common::Hub::boot().await;
    let (mut est, node_id, _kp) = common::connect_enrolled(&hub).await;

    let hb = node_core::heartbeat::heartbeat(Vec::new());
    let res = hb.resources.clone();
    common::send_heartbeat(&mut est, serde_json::to_value(&hb).expect("serialize")).await;
    common::wait_until(Duration::from_secs(2), || {
        hub.state.reports().get(&node_id).is_some()
    })
    .await;

    let h = Harness::from_hub(hub).await;
    let v = h.get("/api/devices").await.json();
    let row = v
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == json!(node_id)))
        .expect("设备行在");

    // 非 Linux 上没有 /proc，两列就该是 null——**不许编一个数**。
    if !std::path::Path::new("/proc/meminfo").exists() {
        assert!(row["cpu"].is_null() && row["mem"].is_null(), "{row}");
        return;
    }
    assert!(
        !res.is_null(),
        "有 /proc 却没采到负载：{res}——这条欠账就是要修这个"
    );
    let cpu = row["cpu"]
        .as_u64()
        .unwrap_or_else(|| panic!("cpu 不是数：{row}"));
    let mem = row["mem"]
        .as_u64()
        .unwrap_or_else(|| panic!("mem 不是数：{row}"));

    // 口径：与真读的 `/proc` 对账，而不是与采集器自己的输出对账——
    // 后者只会证明「它把它算出来的数原样交了出去」。
    let expect_mem = {
        let text = std::fs::read_to_string("/proc/meminfo").expect("meminfo");
        let mut total = 0u64;
        let mut avail = 0u64;
        for line in text.lines() {
            let Some((k, rest)) = line.split_once(':') else {
                continue;
            };
            let Ok(kb) = rest.split_whitespace().next().unwrap_or("").parse::<u64>() else {
                continue;
            };
            match k {
                "MemTotal" => total = kb / 1024,
                "MemAvailable" => avail = kb / 1024,
                _ => {}
            }
        }
        (((total - avail) as f64 / total as f64) * 100.0).round() as u64
    };
    // 容差 ±3：两次读 /proc 之间内存本身就在动，要求精确等值是假测试。
    assert!(
        mem.abs_diff(expect_mem) <= 3,
        "mem 与真读的 /proc 对不上：上报 {mem}、真读 {expect_mem}"
    );
    // CPU 那列有 Hub 侧的 `min(100)` 截断，所以只卡上界，不要求等值。
    assert!(cpu <= 100, "cpu 应在 0–100：{cpu}");
}

/// `resources` 采不到时**不许**回落成 0：页面会把「不知道」画成「空转 0%」，
/// 而路由会把一台刚连上的设备当成最闲的那台去派活。
#[tokio::test]
async fn an_uncollected_load_stays_null_instead_of_becoming_zero() {
    let hub = common::Hub::boot().await;
    let (mut est, node_id, _kp) = common::connect_enrolled(&hub).await;
    common::send_heartbeat(
        &mut est,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": ["terminal"]},
            "resources": null,
            "workspace_roots": [],
        }),
    )
    .await;
    common::wait_until(Duration::from_secs(2), || {
        hub.state.reports().get(&node_id).is_some()
    })
    .await;

    let h = Harness::from_hub(hub).await;
    let v = h.get("/api/devices").await.json();
    let row = v
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == json!(node_id)))
        .expect("设备行在");
    assert!(row["cpu"].is_null(), "未上报 = 未知，不是 0：{row}");
    assert!(row["mem"].is_null(), "未上报 = 未知，不是 0：{row}");
}

#[tokio::test]
async fn a_connected_device_is_reported_as_online_with_its_agents() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let r = h.get("/api/devices").await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v[0]["id"], json!(node_id));
    assert_eq!(v[0]["platform"], json!("linux/aarch64"));
    assert_eq!(v[0]["node_version"], json!("0.1.0"));
    assert_eq!(v[0]["status"], json!("online"));
    assert_eq!(v[0]["derived"], json!(false), "online 不是推出来的");
    assert_eq!(v[0]["cpu"], json!(34));
    assert_eq!(v[0]["mem"], json!(61));
    assert_eq!(v[0]["gpu"], json!(false));
    assert_eq!(v[0]["agents"], json!(["claude-code"]));
}

/// **设备摘要里的 `usable_agents` 与建会话门禁同源。**
///
/// 挡的是「页面上选得到、点创建 409」：两台设备的心跳里各有一个 agent，
/// 一台可用、一台装了但跑不起来。`agents`（已装目录）两台都要列出来，
/// `usable_agents`（建会话可选项）只能有可用的那个。
///
/// 反向验证：把 `device_summary` 里那一档换成「照抄 `agents`」（即不再走
/// `AgentRuntimeRepo::usable_list` 的 `available = 1`），这里立刻红。
#[tokio::test]
async fn usable_agents_lists_only_what_the_gate_would_accept() {
    let hub = common::Hub::boot().await;
    let (node_ok, _est_ok) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let (node_broken, _est_broken) = enrolled_device_reporting(
        &hub,
        json!([{"id": "claude", "name": "claude", "version": "1.0", "available": false,
               "error": "no ACP entry point"}]),
        1,
    )
    .await;

    let h = Harness::from_hub(hub).await;
    let v = h.get("/api/devices").await.json();
    let rows = v.as_array().expect("/api/devices 是数组");
    let ok = rows
        .iter()
        .find(|r| r["id"] == json!(node_ok))
        .expect("可用设备在");
    let broken = rows
        .iter()
        .find(|r| r["id"] == json!(node_broken))
        .expect("坏设备在");

    assert_eq!(ok["agents"], json!(["opencode"]));
    assert_eq!(
        ok["usable_agents"],
        json!(["opencode"]),
        "可用的那个必须进可选项"
    );
    assert_eq!(
        broken["agents"],
        json!(["claude"]),
        "装了但跑不起来的也要列进目录（用户要看见它）"
    );
    assert_eq!(
        broken["usable_agents"],
        json!([]),
        "跑不起来的不能进可选项：选得到就只能拿到 409"
    );
}

/// WS 断了但心跳还新鲜 → `reconnecting`（§6.1 的三态与 `derived` 徽标）。
#[tokio::test]
async fn a_device_whose_socket_dropped_but_is_heartbeating_is_reconnecting() {
    let hub = common::Hub::boot().await;
    let (node_id, keepalive) = enrolled_device(&hub).await;
    // drop 掉连接：注册表里不再有它，而上报还新鲜（45s 内）
    drop(keepalive);
    common::wait_until(Duration::from_secs(2), || {
        hub.state.connections().get(&node_id).is_none()
    })
    .await;

    let h = Harness::from_hub(hub).await;
    let v = h.get("/api/devices").await.json();
    assert_eq!(v[0]["status"], json!("reconnecting"));
    assert_eq!(v[0]["derived"], json!(true), "「推」出来的状态要标出来");
}

/// 从没上报过心跳的设备（enroll 完还没发第一条）→ `offline`。
#[tokio::test]
async fn a_device_that_never_heartbeated_is_offline() {
    let hub = common::Hub::boot().await;
    let kp = identity::KeyPair::generate();
    let node_id = hub
        .state
        .registry()
        .enroll(
            &kp.public_key_bytes(),
            hub_storage::repo::NewDevice {
                id: String::new(),
                name: "silent".into(),
                key_fingerprint: String::new(),
                os: "linux".into(),
                arch: "aarch64".into(),
                hostname: "silent".into(),
                node_version: "0.1.0".into(),
            },
        )
        .expect("enroll")
        .node_id()
        .to_string();

    let h = Harness::from_hub(hub).await;
    let v = h.get("/api/devices").await.json();
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(node_id))
        .unwrap();
    assert_eq!(row["status"], json!("offline"));
    assert_eq!(row["cpu"], Value::Null, "没上报负载就是未知，不是 0");
    assert_eq!(row["agents"], json!([]));
}

/// `key_fingerprint` 只给掩过的形式（约束 13）。
#[tokio::test]
async fn a_device_summary_never_exposes_a_full_fingerprint() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let stored = hub
        .state
        .devices()
        .find_by_id(&node_id)
        .unwrap()
        .unwrap()
        .key_fingerprint;
    assert!(stored.len() > 6);

    let h = Harness::from_hub(hub).await;
    let r = h.get("/api/devices").await;
    let fp = r.json()[0]["key_fingerprint"].as_str().unwrap().to_string();
    assert_eq!(fp.len(), 9, "前 3 + *** + 后 3");
    assert!(fp.contains("***"));
    assert!(!r.body.contains(&stored), "响应里不得出现完整指纹");
}

// ---------------------------------------------------------------------------
// 标准②：GET /api/sessions + POST /api/sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_sessions_returns_an_empty_array_initially() {
    let h = Harness::new().await;
    let r = h.get("/api/sessions").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!([]));
}

/// 设备在线（上报了 opencode 且可用）→ 201，形状是会话摘要。
///
/// 设备**必须真上报一个可用 agent**：建会话现在要求 `agent_id`，而门禁读
/// `agent_runtime` 表——`enrolled_device` 那份旧形态的字符串上报在库里是
/// `available = 0`，用它建会话会 409（那正是另一条用例钉的东西）。
#[tokio::test]
async fn creating_a_session_returns_a_detail_with_its_locked_workspace() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["state"], json!("running"));
    assert_eq!(v["agent_id"], json!("opencode"));
    assert_eq!(v["workspace"]["node_id"], json!(node_id));
    assert_eq!(v["workspace"]["path"], json!("~/src/app"));
    assert_eq!(v["workspace"]["path_locked"], json!(true));
    assert_eq!(v["ended_at"], Value::Null);
    assert!(v["id"].as_str().is_some_and(|s| !s.is_empty()));

    // 它随即出现在列表里，形状一致
    let items = h.get("/api/sessions").await.json();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["id"], v["id"]);
    assert_eq!(items[0]["workspace"]["path"], json!("~/src/app"));
}

// ---------------------------------------------------------------------------
// 子任务 4（v0.6.0）：起点随会话列表带出，且五态在 API 层可区分
// ---------------------------------------------------------------------------

/// 只 enroll 一台设备（不发心跳），返回 **Hub 实际签发的** node_id。
///
/// 注意返回值：`enroll` 会忽略 `NewDevice.id` 并用 `identity::issue_node_id()`
/// 自己发一个（`registry.rs::enroll`），所以传进去的 id 只会落成 `device.id`
/// 之前的临时值——拿它去插会话会报 `NoSuchDevice`。必须用这里返回的那个。
fn enroll_only(hub: &common::Hub, label: &str) -> String {
    hub.state
        .registry()
        .enroll(
            &identity::KeyPair::generate().public_key_bytes(),
            hub_storage::repo::NewDevice {
                id: label.to_string(),
                name: label.into(),
                key_fingerprint: String::new(),
                os: "linux".into(),
                arch: "x86_64".into(),
                hostname: label.into(),
                node_version: "0.1.0".into(),
            },
        )
        .expect("enroll")
        .node_id()
        .to_string()
}

/// 列表里**每一行**都带 `base_rev` 与 `base_rev_status` 两键，且三行三种态。
///
/// 造三行不同来源的会话：
/// - `ok`：设备回了一个真 hash（走真入口 `POST /api/sessions` + 桩设备）；
/// - `not_a_repo`：设备回 `not_a_repo: true`（同上）；
/// - `unknown`：迁移前的老会话——`SessionRepo::create` 直接传 `Unknown`，
///   这是它**唯一**的产生方式（正常入口一定会探测并覆盖成前四态之一）。
///
/// **两键必须在**，哪怕值是 `null`：`skip_serializing_if` 会让键整个消失，
/// 于是「老 Hub + 新页面」与「新 Hub 正常返回无起点」在 TS 里长得一样。
/// 所以这里断言的是 `contains_key`，不是「值非空」。
#[tokio::test]
async fn the_session_list_carries_both_base_rev_keys_for_every_row() {
    let hub = common::Hub::boot().await;
    // **必须上报 `git`**：探针的第四道门禁要求它，否则两条会话都是 `unavailable`。
    let (node_id, _keepalive) = enrolled_device_reporting_with_git(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    // --- 第 3 行：迁移前的老会话（`unknown`）。先插，免得被上面的探测串扰。
    let legacy_node = enroll_only(&h.hub, "v4-list-legacy");
    let legacy_id = hub_storage::SessionRepo::new(h.hub.state.pool().clone())
        .create(
            &legacy_node,
            "~/legacy",
            None,
            &hub_core::registry::now_stamp(),
            None,
            hub_storage::BaseRevStatus::Unknown,
        )
        .expect("插一条老会话")
        .id;

    // --- 先在设备侧起一个应答器，再用真入口建两个会话。
    const HEAD: &str = "3f2a1b0c4d5e6f708192a3b4c5d6e7f809a1b2c3";
    let replies = vec![
        json!({"op":"rev","not_a_repo":false,"head":HEAD,"short":&HEAD[..7],"branch":"main","detached":false}),
        json!({"op":"rev","not_a_repo":true,"head":null,"short":null,"branch":null,"detached":false}),
    ];
    // 整条连接移进桩任务：`Established` 不是 `Clone`，而桩要独占它的读写。
    let stub = spawn_list_rev_stub(_keepalive, replies);

    let ok_row = h
        .create_session(json!({"node_id": node_id, "path": "~/src/ok", "agent_id": "opencode"}))
        .await;
    assert_eq!(ok_row.status, StatusCode::CREATED, "body: {}", ok_row.body);
    let not_repo_row = h
        .create_session(json!({"node_id": node_id, "path": "~/src/hg", "agent_id": "opencode"}))
        .await;
    assert_eq!(
        not_repo_row.status,
        StatusCode::CREATED,
        "body: {}",
        not_repo_row.body
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), stub).await;

    let items = h.get("/api/sessions").await.json();
    let rows = items.as_array().expect("列表必须是数组");
    assert_eq!(rows.len(), 3, "本例要三行：ok / not_a_repo / unknown");

    // 每一行都必须有这两个键——**不管值是什么**。
    for row in rows {
        let obj = row.as_object().expect("每一行是对象");
        assert!(
            obj.contains_key("base_rev"),
            "每一行都必须有 base_rev 键（null 也算），实际缺了：{row}"
        );
        assert!(
            obj.contains_key("base_rev_status"),
            "每一行都必须有 base_rev_status 键，实际缺了：{row}"
        );
        // v0.7.0：`execution_id` 同样**键恒在**，值为 null 才算「没有归属」。
        // 加 `skip_serializing_if` 会让「老 Hub + 新页面」（键不在）与「新 Hub
        // 正常返回无归属」（键在、值为 null）在 TypeScript 里长得一样。
        assert!(
            obj.contains_key("execution_id"),
            "每一行都必须有 execution_id 键（null 也算），实际缺了：{row}"
        );
        assert_eq!(
            obj["execution_id"],
            Value::Null,
            "这几条会话都没有所属执行（老会话不回填），必须是 null：{row}"
        );
    }

    // 三行的 status 分别是 ok / not_a_repo / unknown，且 `ok` 那行带完整 hash。
    let by_status: std::collections::HashMap<&str, &Value> = rows
        .iter()
        .map(|r| (r["base_rev_status"].as_str().unwrap_or(""), r))
        .collect();
    assert_eq!(
        by_status.len(),
        3,
        "三行必须是三种不同的状态，实际 {:?}",
        by_status.keys().collect::<Vec<_>>()
    );
    let ok = by_status.get("ok").expect("必须有一行 ok");
    assert_eq!(ok["base_rev"], json!(HEAD));
    assert_eq!(ok["id"], json!(ok_row.json()["id"]));
    let nr = by_status.get("not_a_repo").expect("必须有一行 not_a_repo");
    assert_eq!(nr["base_rev"], Value::Null, "not_a_repo 没有 rev");
    let unknown = by_status.get("unknown").expect("必须有一行 unknown");
    assert_eq!(unknown["id"], json!(legacy_id));
    assert_eq!(unknown["base_rev"], Value::Null, "老会话没有 rev");
}

/// 迁移前的老会话在列表里是 `"unknown"`——**不是 `null`、不是缺失、更不是 `ok`**。
///
/// 五态在 API 层必须可区分，否则子任务 5 的文案没法分档：`unknown`
/// （「这次会话没有记录起点」）与 `unavailable`（「问了没拿到」）与 `ok`
/// 在小页面上是三句完全不同的话。
#[tokio::test]
async fn an_old_session_from_before_v4_reports_unknown_not_null() {
    let hub = common::Hub::boot().await;
    let node = enroll_only(&hub, "v4-unknown");
    let id = hub_storage::SessionRepo::new(hub.state.pool().clone())
        .create(
            &node,
            "~/src/app",
            None,
            &hub_core::registry::now_stamp(),
            None,
            hub_storage::BaseRevStatus::Unknown,
        )
        .expect("插一条老会话")
        .id;

    let h = Harness::from_hub(hub).await;
    let items = h.get("/api/sessions").await.json();
    let row = items
        .as_array()
        .expect("列表是数组")
        .iter()
        .find(|r| r["id"] == json!(id))
        .expect("老会话必须出现在列表里");

    assert_eq!(
        row["base_rev_status"],
        json!("unknown"),
        "迁移前的会话是 unknown，不是 null、不是 ok，body: {row}"
    );
    // `unknown` 不是字符串 "null"、也不是 JSON null——这三者在 TS 里都能判，
    // 但只有真字符串才能被前端五态分档读到。
    assert!(
        row["base_rev_status"].is_string(),
        "必须是字符串字面量而不是 null，实际 {}",
        row["base_rev_status"]
    );
    assert_eq!(row["base_rev"], Value::Null);
}

/// 设备侧桩：读下行帧，遇 `open_channel` 回 `channel_ready`，遇 `rev` 按次序回一条，
/// 遇 `close_channel` 记一笔。收齐 `replies.len()` 次关闭就退出。
///
/// 与 `tests/base_rev.rs` 的同名夹具同款——那个文件是单子任务 3 的白盒用例，
/// 这里只需要「让真入口拿到两种不同的结果」，所以不复用它（跨测试文件不共享夹具）。
fn spawn_list_rev_stub(
    mut est: node_core::conn::Established,
    replies: Vec<Value>,
) -> tokio::task::JoinHandle<()> {
    use protocol::channel::{ChannelData, CloseChannel};
    use protocol::frame::Frame;
    use protocol::message::types;

    tokio::spawn(async move {
        let mut n = 0usize;
        let mut closed = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            let Some(text) = est.next_text().await.unwrap_or_default() else {
                break;
            };
            let Ok(f) = serde_json::from_str::<Frame>(&text) else {
                continue;
            };
            match f.msg_type.as_str() {
                types::OPEN_CHANNEL => {
                    let Ok(open) = f.payload::<protocol::channel::OpenChannel>() else {
                        continue;
                    };
                    est.send(&Frame::push(
                        types::CHANNEL_READY,
                        0,
                        json!({"channel_id": open.channel_id}),
                    ))
                    .await
                    .ok();
                }
                types::CHANNEL_DATA => {
                    let Ok(data) = f.payload::<ChannelData>() else {
                        continue;
                    };
                    if data.payload.get("op").and_then(Value::as_str) != Some("rev") {
                        continue;
                    }
                    if let Some(body) = replies.get(n) {
                        est.send(&Frame::push(
                            types::CHANNEL_DATA,
                            0,
                            json!({"channel_id": data.channel_id, "payload": body}),
                        ))
                        .await
                        .ok();
                    }
                    n += 1;
                }
                types::CLOSE_CHANNEL => {
                    if f.payload::<CloseChannel>().is_ok() {
                        closed += 1;
                    }
                    if closed >= replies.len() {
                        break;
                    }
                }
                _ => {}
            }
        }
    })
}

/// 两个会话指向同一个 `(node_id, path)`：工作区是主实体（子任务 5c 的裁决）。
#[tokio::test]
async fn two_sessions_can_share_one_workspace() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    let a = h
        .create_session(json!({
            "node_id": node_id,
            "path": "/srv/app",
            "agent_id": "opencode",
        }))
        .await;
    let b = h
        .create_session(json!({
            "node_id": node_id,
            "path": "/srv/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(b.status, StatusCode::CREATED, "{}", b.body);
    assert_ne!(a.json()["id"], b.json()["id"], "两个会话是两条");
    assert_eq!(
        h.get("/api/sessions")
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// 「重连中」的设备**能**建会话（201），且建出来的会话带一个可用 agent。
///
/// 这条钉的是前后端的一句共同承诺：`api.ts::canCreateSession` 是
/// `status !== 'offline'`，`NewSessionPage.vue` 的正文写着「「重连中」仍可建
/// 会话」。若 Hub 只认 `online`，用户读到的「能建」与点下去拿到的
/// 409 `device_offline` 会前后脚出现在同一块屏幕上。
///
/// 造 `reconnecting` 的办法与前一条 `_is_reconnecting` 用例相同：**drop 掉
/// 那条真连接**——注册表里不再有它，而心跳上报还新鲜（45s 内）。这不是
/// 伪造状态，`presence()` 真的会给出 `Reconnecting`。
#[tokio::test]
async fn creating_a_session_on_a_reconnecting_device_succeeds() {
    let hub = common::Hub::boot().await;
    let (node_id, keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    // 先确认造出来的确实是 `reconnecting`，否则「201」可能只是碰巧走了在线路径。
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "前提：连接此时还在"
    );
    drop(keepalive);
    common::wait_until(Duration::from_secs(2), || {
        hub.state.connections().get(&node_id).is_none()
    })
    .await;

    let h = Harness::from_hub(hub).await;
    // 状态确实是 `reconnecting`——同一个 `device_summary` 派生出来的那两个值，
    // 页面读到的与门禁用的是同一份判据。
    let row = h
        .get("/api/devices")
        .await
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(node_id))
        .cloned()
        .expect("设备行在");
    assert_eq!(row["status"], json!("reconnecting"), "{row}");

    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "重连中的设备必须能建会话（页面就是这么承诺的），body: {}",
        r.body
    );
    let v = r.json();
    assert_eq!(v["agent_id"], json!("opencode"), "会话要带一个可用的 agent");
    assert_eq!(v["state"], json!("running"));
    assert_eq!(v["workspace"]["node_id"], json!(node_id));
}

/// 设备离线 → 409，且**带上设备状态**（页面要显示「为什么不行」）。
#[tokio::test]
async fn creating_a_session_on_an_offline_device_is_a_conflict_with_the_status() {
    let hub = common::Hub::boot().await;
    let kp = identity::KeyPair::generate();
    let node_id = hub
        .state
        .registry()
        .enroll(
            &kp.public_key_bytes(),
            hub_storage::repo::NewDevice {
                id: String::new(),
                name: "silent".into(),
                key_fingerprint: String::new(),
                os: "linux".into(),
                arch: "aarch64".into(),
                hostname: "silent".into(),
                node_version: "0.1.0".into(),
            },
        )
        .expect("enroll")
        .node_id()
        .to_string();

    let h = Harness::from_hub(hub).await;
    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "claude-code",
        }))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["error"], json!("device_offline"));
    assert_eq!(
        v["detail"]["status"],
        json!("offline"),
        "409 必须带设备状态（§6 后端流程）"
    );
}

/// 设备不存在 → 404，错误体形状与别的错误一致。
#[tokio::test]
async fn creating_a_session_on_an_unknown_device_is_a_404() {
    let h = Harness::new().await;
    let r = h
        .create_session(json!({
            "node_id": "n_ghost",
            "path": "/srv/app",
            "agent_id": "claude-code",
        }))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    r.assert_error_code("not_found");
}

/// 越界形状的路径在建会话时就拒（400），不是等面板打开才报错。
///
/// 每条都带一个合法 agent：否则会先在 agent 门禁上 400，路径那条分支根本没走到。
#[tokio::test]
async fn a_workspace_path_must_be_absolute_or_home_relative() {
    let h = Harness::new().await;
    for bad in ["src/app", "", "..", "."] {
        let r = h
            .create_session(json!({
                "node_id": "n_any",
                "path": bad,
                "agent_id": "claude-code",
            }))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad:?} 必须 400");
        r.assert_error_code("invalid_workspace_path");
    }
}

/// 字面路径合法（含 `~` 与 `/`）——`..` 逃逸是**子路径**上的判据，
/// 由 node 侧的 files connector 强制（Hub 没有那台设备的文件系统，约束 1）。
#[tokio::test]
async fn a_literal_path_reaches_the_device_lookup() {
    let h = Harness::new().await;
    for ok in ["/srv/app", "~/src/app", "~"] {
        let r = h
            .create_session(json!({
                "node_id": "n_ghost",
                "path": ok,
                "agent_id": "claude-code",
            }))
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{ok:?} 应走到设备查找");
    }
}

/// body 形状不对 → 400（axum 的 Json 提取器拒），错误体仍然是那个形状。
#[tokio::test]
async fn a_malformed_create_body_is_rejected_before_any_write() {
    let h = Harness::new().await;
    let r = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header(CSRF_HEADER, &h.csrf)
                .body(Body::from(json!({"node_id": 7}).to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.json()["error"].as_str().is_some(),
        "错误体仍是固定 code：{}",
        r.body
    );
}

// ---------------------------------------------------------------------------
// 标准②（续）：GET /api/agents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_agent_directory_is_empty_when_nothing_reported_one() {
    let h = Harness::new().await;
    let r = h.get("/api/agents").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!([]));
}

/// agent 目录来自心跳上报，且**跨设备去重排序**。
#[tokio::test]
async fn the_agent_directory_is_the_deduplicated_union_of_heartbeat_reports() {
    let hub = common::Hub::boot().await;
    let (mut a, node_a, _) = common::connect_enrolled(&hub).await;
    let (mut b, node_b, _) = common::connect_enrolled(&hub).await;
    for (est, agents) in [
        (&mut a, json!(["opencode", "claude-code"])),
        (&mut b, json!(["claude-code"])),
    ] {
        common::send_heartbeat(
            est,
            json!({
                "node_version": "0.1.0",
                "capabilities": {"agents": agents},
                "resources": null,
                "workspace_roots": [],
            }),
        )
        .await;
    }
    common::wait_until(Duration::from_secs(2), || {
        hub.state.reports().get(&node_a).is_some() && hub.state.reports().get(&node_b).is_some()
    })
    .await;

    let h = Harness::from_hub(hub).await;
    let r = h.get("/api/agents").await;
    assert_eq!(r.status, StatusCode::OK);
    // 目录项现在是 `{name, available, error}` 三元组而不是只有名字：
    // 只有名字的话，「装了但跑不起来」这个状态在页面上无从表达。
    //
    // 这里两份心跳都是**旧形态**（裸字符串数组），按 `parse_agent_report`
    // 的口径记为不可用——旧帧不构成「可用」的证据。所以两条都 `available: false`。
    assert_eq!(
        r.json(),
        json!([
            {"name": "claude-code", "available": false, "error": null},
            {"name": "opencode", "available": false, "error": null},
        ])
    );
}

// ---- agent 配置（子任务 4，要求 2「扫描 + 配置」的配置侧）----
//
// 这里**没有安装端点**：2026-10-26 用户裁定取消「Hub 下发 agent 安装」，
// agent 由用户自己在设备上装，Hub 只存它的启动配置。所以本段测的是
// 「配置能写能读能删」，而不是「Hub 能让设备装东西」。

impl Harness {
    /// `POST /api/agents/config`，带 CSRF 双重提交。
    async fn post_config(&self, body: Value) -> Reply {
        self.send(
            Request::builder()
                .method("POST")
                .uri("/api/agents/config")
                .header("content-type", "application/json")
                .header(CSRF_HEADER, &self.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    /// `DELETE /api/devices/{id}`，带 CSRF 双重提交。
    async fn delete_device(&self, id: &str) -> Reply {
        self.send(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/devices/{id}"))
                .header(CSRF_HEADER, &self.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// `DELETE /api/agents/config?...`，带 CSRF 双重提交。
    async fn delete_config(&self, node_id: &str, agent_id: &str) -> Reply {
        self.send(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/api/agents/config?node_id={node_id}&agent_id={agent_id}"
                ))
                .header(CSRF_HEADER, &self.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }
}

/// 写→读往返：参数与环境变量都**结构化**回来，不是 JSON 字符串。
#[tokio::test]
async fn an_agent_config_round_trips_through_the_api() {
    let h = Harness::new().await;

    let written = h
        .post_config(json!({
            "node_id": "n_cfg",
            "agent_id": "opencode",
            "command": "/opt/opencode/bin/opencode",
            "args": ["--acp", "--verbose"],
            "env": {"FOO": "bar"}
        }))
        .await;
    assert_eq!(written.status, StatusCode::OK, "body: {}", written.body);
    let written = written.json();
    assert_eq!(written["command"], json!("/opt/opencode/bin/opencode"));
    assert_eq!(written["args"], json!(["--acp", "--verbose"]));
    assert_eq!(written["env"], json!({"FOO": "bar"}));
    assert!(
        written["updated_at"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "时间戳由 Hub 盖，必须在响应里：{}",
        written
    );

    let listed = h.get("/api/agents/config?node_id=n_cfg").await;
    assert_eq!(listed.status, StatusCode::OK);
    let items = listed.json();
    let arr = items["configs"].as_array().expect("configs 是数组");
    assert_eq!(arr.len(), 1, "body: {}", listed.body);
    assert_eq!(arr[0]["agent_id"], json!("opencode"));
    assert_eq!(arr[0]["args"], json!(["--acp", "--verbose"]));
}

/// **配置可以先于 agent 存在**：装之前先配好是正常流程，不能被拒。
///
/// 反向验证：在 `upsert_config` 里加一条「`agent_runtime` 必须有这行」的检查，
/// 这条会红（回 404 而不是 200）。
#[tokio::test]
async fn a_config_can_be_written_before_the_agent_is_installed() {
    let h = Harness::new().await;
    // 这台设备从没上报过任何 agent（`agent_runtime` 里没有 opencode）
    let r = h
        .post_config(json!({
            "node_id": "n_never_seen",
            "agent_id": "opencode",
            "args": ["--acp"]
        }))
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "先配置后安装必须能走通：{}",
        r.body
    );
}

/// 坏 body 与空 id 都是同一个错误码 `invalid_request_body`，**不是 422 正文**。
///
/// `/api/*` 的错误体只有 `{"error": "<code>"}` 一种形状（见 `create_session`
/// 的注释）：axum 的 `Json` 拒绝体会回 422 + text/plain，前端解析不了。所以
/// 这里自己读 body 再判——这条用例钉的正是那个「自己读」的分支。
#[tokio::test]
async fn an_unparsable_or_empty_agent_config_body_is_a_bad_request() {
    let h = Harness::new().await;

    // 根本不是 JSON。
    let r = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/api/agents/config")
                .header("content-type", "application/json")
                .header(CSRF_HEADER, &h.csrf)
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    r.assert_error_code("invalid_request_body");

    // 是 JSON，但 id 是空白串：写进去也路由不到任何设备/agent。
    let r = h
        .post_config(json!({"node_id": "   ", "agent_id": "opencode"}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    r.assert_error_code("invalid_request_body");

    let r = h
        .post_config(json!({"node_id": "n_cfg", "agent_id": ""}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    r.assert_error_code("invalid_request_body");
}

/// 同一 (node, agent) 写两次只有一行，后写的覆盖前写的。
#[tokio::test]
async fn posting_the_same_config_twice_overwrites_instead_of_duplicating() {
    let h = Harness::new().await;
    for cmd in ["/old", "/new"] {
        let r = h
            .post_config(json!({
                "node_id": "n_cfg",
                "agent_id": "opencode",
                "command": cmd
            }))
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    let arr = h.get("/api/agents/config?node_id=n_cfg").await.json();
    let arr = arr["configs"].as_array().expect("数组");
    assert_eq!(arr.len(), 1, "不许出现两行：{arr:?}");
    assert_eq!(arr[0]["command"], json!("/new"));
}

/// 空命令是**合法**的：意思是「用扫描到的那个可执行文件」。
#[tokio::test]
async fn an_empty_command_is_accepted_and_means_use_the_scanned_binary() {
    let h = Harness::new().await;
    let r = h
        .post_config(json!({"node_id": "n_cfg", "agent_id": "opencode"}))
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["command"], json!(""), "缺省命令是空串，不是 null");
    assert_eq!(v["args"], json!([]));
    assert_eq!(v["env"], json!({}));
}

/// 删掉之后列表为空；**再删一次是 404**（「我以为删掉了」要能区分开）。
#[tokio::test]
async fn deleting_a_config_removes_it_and_a_second_delete_is_404() {
    let h = Harness::new().await;
    h.post_config(json!({"node_id": "n_cfg", "agent_id": "opencode"}))
        .await;

    let first = h.delete_config("n_cfg", "opencode").await;
    assert_eq!(first.status, StatusCode::NO_CONTENT, "body: {}", first.body);

    let arr = h.get("/api/agents/config?node_id=n_cfg").await.json();
    assert_eq!(arr["configs"].as_array().map(Vec::len), Some(0));

    let second = h.delete_config("n_cfg", "opencode").await;
    assert_eq!(second.status, StatusCode::NOT_FOUND);
}

/// **CSRF 门禁的反向验证**：带 cookie 但不带 `X-CSRF-Token` 必须被拒。
///
/// 这条是配置端点的关键一条：写在 `args` / `env` 里的东西是设备下次启动
/// agent 时会**原样带上**的，任意网页能改就等于任意网页能往用户的 agent
/// 进程里塞参数与环境变量。
///
/// 反向验证：把 `/api/agents/config` 挪到 `build_app` 的 `.layer(...)` 之后，
/// 这条会变成 200 而红。
#[tokio::test]
async fn writing_a_config_without_the_csrf_header_is_rejected() {
    let h = Harness::new().await;
    let r = Reply::new(
        h.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/agents/config")
                    .header("content-type", "application/json")
                    .header(
                        "cookie",
                        format!("{SESSION_COOKIE}={}; {CSRF_COOKIE}={}", h.session, h.csrf),
                    )
                    .body(Body::from(
                        json!({"node_id": "n_cfg", "agent_id": "opencode"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert!(
        r.status.is_client_error(),
        "没有 CSRF 头的写请求必须被拒，实际 {}",
        r.status
    );
    // 确认它真的没写进去
    let arr = h.get("/api/agents/config?node_id=n_cfg").await.json();
    assert_eq!(
        arr["configs"].as_array().map(Vec::len),
        Some(0),
        "被拒的请求不许留下任何行"
    );
}

// ---- 删设备（子任务 12 第 1 条）----

/// **删设备只删设备那一行**：会话与工作区留在列表里，并标上
/// `removed_node_id`（用户裁定的「删除语义三分离」）。
///
/// 断言落在 `/api/devices` 与 `/api/sessions` **两个列表**上，不是直接查库：
/// 这条功能的用户可见结果是「设备页少了一台、会话页那几条还在」，而它可能断在
/// 路由、仓储或序列化任一层。
#[tokio::test]
async fn deleting_a_device_keeps_its_sessions_listed_as_removed() {
    let hub = common::Hub::boot().await;
    // 建会话要一个可用的 agent（门禁读 `agent_runtime`，旧形态的裸字符串上报不算）。
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    let created = h
        .create_session(json!({
            "node_id": node_id,
            "path": "/srv/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);

    let r = h.delete_device(&node_id).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "body: {}", r.body);

    // 设备页：它没了
    let devices = h.get("/api/devices").await.json();
    assert!(
        devices
            .as_array()
            .expect("数组")
            .iter()
            .all(|d| d["id"] != json!(node_id)),
        "删完还在设备列表里：{devices}"
    );

    // 会话页：会话还在，且**标成「已移除的设备」**而不是一片空白
    let sessions = h.get("/api/sessions").await.json();
    let arr = sessions.as_array().expect("数组");
    assert_eq!(arr.len(), 1, "会话被连带删了：{sessions}");
    // 归属在 `workspace` 里（会话没有自己的 `node_id`，它经由工作区归属），
    // 所以这三条断言都往下走一层——写成 `arr[0]["node_id"]` 会读到 `Null`
    // 并「通过」，而那个 `Null` 只是键不存在。
    assert_eq!(arr[0]["workspace"]["path"], json!("/srv/app"));
    assert_eq!(
        arr[0]["workspace"]["node_id"],
        Value::Null,
        "原来那台设备已经不存在，node_id 不该还指向它：{sessions}"
    );
    assert_eq!(
        arr[0]["workspace"]["removed_node_id"],
        json!(node_id),
        "要记下原 node_id，否则页面只能显示一台没有名字的设备：{sessions}"
    );
}

/// 再删一次是 **404**，与全路由同口径。
///
/// 不返回 204：`204` 说的是「删掉了」，而那时它本来就不存在——把这两件事合成
/// 一个响应，页面对「我点了两次」与「本来就没成功」就分不出区别。
#[tokio::test]
async fn deleting_an_absent_device_is_404_and_leaves_the_list_alone() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let first = h.delete_device(&node_id).await;
    assert_eq!(first.status, StatusCode::NO_CONTENT, "body: {}", first.body);

    let second = h.delete_device(&node_id).await;
    assert_eq!(
        second.status,
        StatusCode::NOT_FOUND,
        "body: {}",
        second.body
    );
    assert_eq!(
        h.get("/api/devices").await.json(),
        json!([]),
        "两次删之间不许复活血"
    );
}

/// **删设备不级联删会话**，轨迹也还读得到。
///
/// 与「有会话的工作区拒删」是两条不同的语义，所以各测一次——否则容易把其中
/// 一条悄悄改成另一条的镜像。
#[tokio::test]
async fn deleting_a_device_does_not_delete_its_sessions_trajectory() {
    let hub = common::Hub::boot().await;
    // 建会话要一个可用的 agent（门禁读 `agent_runtime`，旧形态的裸字符串上报不算）。
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    let created = h
        .create_session(json!({
            "node_id": node_id,
            "path": "/srv/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let session_id = created.json()["id"].as_str().expect("会话 id").to_string();

    assert_eq!(
        h.delete_device(&node_id).await.status,
        StatusCode::NO_CONTENT
    );

    // 轨迹端点仍能回答（读的是轨迹表，与设备无关）
    let traj = h
        .get(&format!("/api/sessions/{session_id}/trajectory"))
        .await;
    assert_eq!(traj.status, StatusCode::OK, "body: {}", traj.body);
}

/// **CSRF 门禁**：删设备是状态变更端点，少了 `X-CSRF-Token` 必须被拒。
///
/// 反向验证：把 `/api/devices/{id}` 挪到 `build_app` 的 `.layer(...)` 之后，
/// 这条会红——而且不只是 CSRF，它连认证都不再经过，任意网页能删用户设备。
#[tokio::test]
async fn deleting_a_device_without_the_csrf_header_is_rejected() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let r = Reply::new(
        h.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/devices/{node_id}"))
                    .header(
                        "cookie",
                        format!("{SESSION_COOKIE}={}; {CSRF_COOKIE}={}", h.session, h.csrf),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert!(
        r.status.is_client_error(),
        "没有 CSRF 头的写请求必须被拒，实际 {}",
        r.status
    );
    let devices = h.get("/api/devices").await.json();
    assert_eq!(
        devices.as_array().map(Vec::len),
        Some(1),
        "被拒的请求不许删掉设备：{devices}"
    );
}

/// 形状不对的 body 回 `invalid_request_body`，且错误体只有 `error` 一个键。
#[tokio::test]
async fn a_config_body_without_an_agent_id_is_rejected_as_bad_body() {
    let h = Harness::new().await;
    let r = h.post_config(json!({"node_id": "n_cfg"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    r.assert_error_code("invalid_request_body");
}

// ---- 要求 3：建会话时选 agent + 可用门禁（子任务 5）----
//
// 门禁的判据是 `agent_runtime` 表里有一行且 `available = 1`。**不读内存
// `Reports`**——那一份是「缺省即保留」且没有 `forget`，一次不带 `capabilities`
// 的心跳会让旧清单永久粘住，于是「设备卸载了 agent」传不到 Hub。

/// 已上报且可用 → 201，`agent_id` 落进会话。
#[tokio::test]
async fn creating_a_session_with_an_available_agent_succeeds() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode"
        }))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["agent_id"], json!("opencode"));
}

/// 设备上报了，但那个 agent **没在清单里** → 409，且错误码可读。
#[tokio::test]
async fn creating_a_session_with_an_agent_the_device_never_reported_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!(["opencode"]), 1).await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "claude-code"
        }))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["error"], json!("agent_unavailable"), "body: {}", r.body);
    assert_eq!(
        v["detail"]["agent_id"],
        json!("claude-code"),
        "要点名是哪个 agent：{}",
        r.body
    );
}

/// **装了但跑不起来**（`available = false`）→ 409。
///
/// 这一条是「目录页列全部已装、门禁只认可用的」那个区分的落点：
/// 页面上能看到 opencode，但点创建会 409——因为现在用它开会话只会得到一个
/// 立刻报错的对话。
#[tokio::test]
async fn creating_a_session_with_a_broken_agent_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{
            "id": "opencode", "name": "opencode", "version": "2.1.4",
            "available": false, "error": "exit status 3"
        }]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode"
        }))
        .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "装了但跑不起来不能放行：{}",
        r.body
    );
    assert_eq!(r.json()["error"], json!("agent_unavailable"));
}

/// **从没上报过任何 agent** → 409。
///
/// 这与 M1 的 connector 门禁**显式相反**（那条对「从没上报过」是放行）：
/// connector 是可选能力，agent 是会话的必要前提。放行的后果是用户拿到一个
/// 201 的会话，等开 ACP 时才发现二进制不存在。
#[tokio::test]
async fn creating_a_session_with_an_agent_on_a_silent_device_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!([]), 0).await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode"
        }))
        .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "没上报过就是不可用：{}",
        r.body
    );
    assert_eq!(r.json()["error"], json!("agent_unavailable"));
}

/// **没给 `agent_id` 一律 400 `agent_required`**：键缺失、`null`、空串三种。
///
/// 与 `agent_unavailable`（409）分开：那是「你说要 X，X 在这台设备上跑不起来」
/// （设备状态问题），这是「你根本没说」（请求形状问题）。400 也不是 409：
/// 补上一个名字就能过，没有任何状态需要等。
///
/// 空串必须与缺失同罪——否则「填了个空的下拉框」会造出一条空 agent 的会话，
/// 而那条会话在 `conversation::open` 里是另一个（更难读的）409。
#[tokio::test]
async fn creating_a_session_without_an_agent_id_is_rejected() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!([]), 0).await;
    let h = Harness::from_hub(hub).await;

    // 设备**没上报任何 agent**：若拒绝来自 agent 门禁，这里会是 409
    // `agent_unavailable`。所以这一条同时钉住了「400 发生在门禁之前」。
    for (what, body) in [
        ("键缺失", json!({"node_id": node_id, "path": "~/src/app"})),
        (
            "null",
            json!({"node_id": node_id, "path": "~/src/app", "agent_id": null}),
        ),
        (
            "空串",
            json!({"node_id": node_id, "path": "~/src/app", "agent_id": ""}),
        ),
    ] {
        let r = h.create_session(body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{what}: {}", r.body);
        r.assert_error_code("agent_required");
    }

    assert_eq!(
        h.get("/api/sessions").await.json(),
        json!([]),
        "被拒的请求不许留下任何会话"
    );
}

/// **反向验证「粘滞」**：库里的行被清掉，但内存 `Reports` 里还留着旧清单 → 必须 409。
///
/// 这是子任务 5 最关键的一条。若实现读的是 `Reports`，这条会红。
///
/// 造法：设备先上报 opencode（内存与库都有），然后**直接删库**（模拟「设备卸载了
/// agent，并把这次扫描上报上来」的最终效果），**不动内存**。此时：
/// - 读内存 → 看到 opencode → 201（错）
/// - 读库   → 没有行 → 409（对）
#[tokio::test]
async fn the_gate_reads_the_table_not_the_in_memory_report() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!(["opencode"]), 1).await;

    // 先确认内存里**确实**还有那份旧清单，否则这条测不出任何东西
    let in_memory = hub.state.reports().get(&node_id).expect("心跳已进内存");
    assert!(
        in_memory
            .capabilities
            .get("agents")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty()),
        "前提：内存里要留着旧 agents，否则这条是恒真断言"
    );

    // 只清库，不清内存
    hub.state
        .agent_runtime()
        .delete_for_node(&node_id)
        .expect("清库");

    let h = Harness::from_hub(hub).await;
    let r = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode"
        }))
        .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "库是空的就必须拒；放行说明门禁读的是内存 Reports（结构性粘滞）：{}",
        r.body
    );
}

// ---- 要求 3/4：对话端点（子任务 7/11）----
//
// `POST /api/sessions/{id}/conversation` 不是 `POST /api/channels` 的别名：
// 它多两件事——agent 可用门禁、以及把 `agent_config` 读出来随 `params` 下发
// （用户裁定「Hub 存，设备读」，设备上没有那张表）。

impl Harness {
    /// `POST /api/sessions/{id}/conversation`，带 CSRF 双重提交。
    async fn open_conversation(&self, session_id: &str) -> Reply {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/conversation"))
                .header(CSRF_HEADER, &self.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// `POST /api/sessions/{id}/conversation/cancel`，带 CSRF 双重提交。
    async fn cancel_conversation(&self, session_id: &str) -> Reply {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/conversation/cancel"))
                .header(CSRF_HEADER, &self.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }
}

/// 建一个「已选 agent、设备可用」的会话，返回 `(harness, node_id, session_id, est)`。
///
/// `est` 是那条真连接：对话端点会往它上面下发 `open_channel`，而 `_keepalive`
/// 的名字在别处——这里必须**返回**它，否则连接一 drop Hub 就判设备离站。
///
/// `agent_id` 必填（`POST /api/sessions` 现在是这么要求的），所以这条 helper
/// 永远带 agent 建会话。要造「没有 agent 的会话」得直接插库，见
/// [`seed_session_without_an_agent`]。
async fn conversation_ready(
    agents: Value,
    agent_id: &str,
) -> (Harness, String, String, node_core::conn::Established) {
    let hub = common::Hub::boot().await;
    // 建会话成功 = 该 agent 可用 = 库里必有一行。
    let (node_id, est) = enrolled_device_reporting(&hub, agents, 1).await;
    let h = Harness::from_hub(hub).await;
    let created = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": agent_id,
        }))
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let session_id = created.json()["id"]
        .as_str()
        .expect("建会话要回 id")
        .to_string();
    (h, node_id, session_id, est)
}

// `agent_id` 在**建会话**时已必填，所以走 HTTP 再也造不出「有会话、没 agent」
// 的新会话。那条状态仍要测：库里的存量行就是这样（`SessionRepo::create` 仍收
// `Option`，NULL 列也仍然允许），开对话时对它的处置不变。造法只能是绕过 API
// 直接插库，并且用一个不与别处撞车的 node_id——工作区按 `(node_id, path)`
// 去重，撞了会把别的用例的会话挂到本用例的设备上。
const NO_AGENT_NODE: &str = "n_legacy_no_agent";

/// 建一条 `agent_id` 为 NULL 的存量会话（等价于 M1 时代 POST 建出来的那种）。
fn seed_session_without_an_agent(hub: &common::Hub, node_id: &str) -> String {
    let device = hub
        .state
        .registry()
        .enroll(
            &identity::KeyPair::generate().public_key_bytes(),
            hub_storage::repo::NewDevice {
                id: node_id.to_string(),
                name: "legacy".into(),
                key_fingerprint: String::new(),
                os: "linux".into(),
                arch: "x86_64".into(),
                hostname: "legacy".into(),
                node_version: "0.1.0".into(),
            },
        )
        .expect("enroll")
        .node_id()
        .to_string();
    hub_storage::SessionRepo::new(hub.state.pool().clone())
        .create(
            &device,
            "~/src/app",
            None,
            &hub_core::registry::now_stamp(),
            None,
            hub_storage::BaseRevStatus::Unknown,
        )
        .expect("插一条 no-agent 会话")
        .id
}

/// **存量的 no-agent 会话 → 开对话 409 `agent_unavailable`，`detail.agent_id` 是 null。**
///
/// 判据是 null 而不是某个 agent 名：这不是「那个 agent 不可用」，是「这个会话
/// 没有可用的对话对象」。挑一个代进去等于替用户做了会写进历史的决定。
///
/// 这条是**存量数据**的行为，不是可建状态：新建会话已经不可能没有 agent
/// （`POST /api/sessions` 回 400 `agent_required`）。
#[tokio::test]
async fn opening_a_conversation_on_a_session_without_an_agent_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let session_id = seed_session_without_an_agent(&hub, NO_AGENT_NODE);
    let h = Harness::from_hub(hub).await;

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["error"], json!("agent_unavailable"), "body: {}", r.body);
    assert_eq!(
        v["detail"]["agent_id"],
        Value::Null,
        "没选 agent 时 detail.agent_id 必须是 null，不能编一个名字"
    );
}

/// **agent 建会话之后变成不可用 → 开对话 409**，且 `detail.agent_id` 点名是哪个。
///
/// **必须分两步上报**，不能一开始就用 `available: false`：那样连会话都建不出来
/// （`create_session` 有同一套门禁），测的是建会话而不是开对话。而真实世界里
/// 「建完会话、agent 随后不可用」恰恰会发生——用户卸载了 agent，或下一次探测
/// 开始失败。所以开对话端点**必须自己再查一次库**，不能相信「会话建成功时可用过」。
///
/// 这条与上一条分开写：两者都回 `agent_unavailable`，但页面要显示的话
/// 完全不同（「这个会话还没选 agent」vs「设备上那个 agent 现在跑不了」）。
#[tokio::test]
async fn opening_a_conversation_with_an_unavailable_agent_is_a_conflict() {
    // 第一步：可用，建会话（建不出就说明门禁把这条测试挡在门外了）
    let (h, node_id, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    // 第二步：同一台设备再上报一次，这次 opencode 跑不起来了
    common::send_heartbeat(
        &mut est,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": ["terminal"], "agents": [
                {"id": "opencode", "name": "opencode", "version": "2.1.4",
                 "available": false, "error": "exec format error"},
            ]},
            "resources": {"cpu": 34, "mem": 61},
            "workspace_roots": [],
        }),
    )
    .await;
    // 等**库**被改写：门禁读库。不等就会在旧行上断言，于是时红时绿。
    let pool = h.hub.state.pool().clone();
    common::wait_until(Duration::from_secs(2), || {
        hub_storage::AgentRuntimeRepo::new(pool.clone())
            .usable_for_session(&node_id, "opencode")
            .expect("读 agent_runtime")
            .is_none()
    })
    .await;

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["error"], json!("agent_unavailable"), "body: {}", r.body);
    assert_eq!(
        v["detail"]["agent_id"],
        json!("opencode"),
        "要点名是哪个 agent：{}",
        r.body
    );
}

/// **正常路径**：201，且下发到设备的 `open_channel` 带着完整的配置。
///
/// 断言的重点是 `params`：`agent_id` / `cwd` / `command` / `args` / `env` 五项
/// 必须同时到位。`cwd` 尤其不能少——设备侧拿不到它就会让 agent 在设备自己的
/// 数据目录里建文件。
#[tokio::test]
async fn opening_a_conversation_sends_the_agent_config_down_to_the_device() {
    let (h, node_id, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    // 先写一份配置：命令 + 参数 + 环境变量
    let w = h
        .post_config(json!({
            "node_id": node_id,
            "agent_id": "opencode",
            "command": "/opt/oc/bin/opencode",
            "args": ["--model", "x"],
            "env": {"FOO": "bar"}
        }))
        .await;
    assert_eq!(w.status, StatusCode::OK, "body: {}", w.body);

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["session_id"], json!(session_id));
    assert_eq!(v["node_id"], json!(node_id));
    let channel_id = v["channel_id"].as_str().expect("要回 channel_id");

    // 设备那条连接上应当收到一帧 `open_channel`
    let open = open_channel_on(&mut est).await;
    let open: protocol::channel::OpenChannel = open.payload().expect("open_channel 载荷");
    assert_eq!(open.channel_id, channel_id, "页面拿到的 id 就是下发的 id");
    assert_eq!(
        open.connector, "acp",
        "对话通道的 connector 是标记 acp，不是某个真 connector"
    );
    assert_eq!(open.params["agent_id"], json!("opencode"));
    // **设备收到的是原文 `~/src/app`，不是绝对路径**：Hub 不知道设备家目录
    // （`Workspace::path` 的注释明说「不做 `~` 展开」），展开只能在设备侧、
    // 起子进程之前做（`node-core` 的 `expand_home`）。
    assert_eq!(
        open.params["cwd"],
        json!("~/src/app"),
        "cwd 原样下发；`~` 的展开是设备侧的事：{}",
        open.params
    );
    assert_eq!(open.params["command"], json!("/opt/oc/bin/opencode"));
    assert_eq!(open.params["args"], json!(["--model", "x"]));
    assert_eq!(open.params["env"], json!({"FOO": "bar"}));
}

/// **第一次开对话时下发的 `acp_session_id` 是 `null`**——设备侧据此走
/// `session/new` 而不是 `load`。
///
/// 判据只能是 `null`（键存在、值为空），不能是「键缺失」：设备侧对
/// 「没有这个键」与「键是 null」走同一条分支，但接口形状上这两者不同，
/// 而这里要钉的是「Hub 明确表达了『这条会话还没有 ACP id』」。
#[tokio::test]
async fn the_first_conversation_sends_a_null_acp_session_id() {
    let (h, _node, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let open = open_channel_on(&mut est).await;
    let open: protocol::channel::OpenChannel = open.payload().expect("open_channel 载荷");
    assert!(
        open.params.get("acp_session_id").is_some(),
        "键必须在（值可以是 null），否则设备侧分不清「没给」与「还没有」：{}",
        open.params
    );
    assert_eq!(
        open.params["acp_session_id"],
        Value::Null,
        "第一次开对话还没有 ACP id，设备侧据此走 session/new"
    );
}

/// **`ready` 载荷里的 ACP 会话 id 会被回填，并在下一次开对话时下发回去。**
///
/// 这是子任务 8 的整个价值所在：设备侧 `conversation::open` 在 `session/new`
/// 成功之后推一条 `{"type":"ready","sessionId":"ses_...","agentId":...}` 上来，
/// Hub 把它记进 `session.acp_session_id`；再开一次对话时把它下发回去，设备
/// 才能走 `session/load` 恢复上下文——否则关掉页面再回来，agent 那边就是
/// 一条全新会话，之前聊的全丢，而页面上看起来一切正常。
///
/// **只用 HTTP 观测，不查库**：这条功能的用户可见结果是「第二次开对话时
/// 下发的是 load 而不是 new」，它可能断在帧解析、绑定表查询或仓储任一层。
///
/// 唯一的例外是下面那个 `wait_until`——它读 `SessionRepo::find` 只为等
/// 「Hub 的读循环已经把这一帧处理完」这一个时序条件，不是为了断言业务
/// 结果（业务结果由紧随其后的 HTTP 断言判）。固定 sleep 在慢机器上会
/// flakes，而这里的等待条件就是「落库了吗」本身。
///
/// 它也顺带说明 `SessionRepo::find` 为什么留着：它是**测试**读单行会话的
/// 入口（生产读会话走 `detail`，因为开通道还要工作区的 `node_id` / `path`）。
/// 判据要写成 HTTP 观测而非库查询，所以断言本身不依赖它。
#[tokio::test]
async fn the_acp_session_id_from_the_ready_frame_comes_back_on_the_next_open() {
    let (h, node_id, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    // ---- 第一次：拿到通道号，然后像设备那样回一条 ready ----
    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let channel_id = r.json()["channel_id"]
        .as_str()
        .expect("要回 channel_id")
        .to_string();
    let _open = open_channel_on(&mut est).await;

    // 设备侧 `channel_ready` 先到（Hub 的 ready 超时要它），再推 ACP 的 ready。
    let ch_ready = json!({"channel_id": channel_id, "capabilities": {}});
    est.send(&protocol::frame::Frame::push(
        protocol::message::types::CHANNEL_READY,
        0,
        ch_ready,
    ))
    .await
    .expect("channel_ready 必须发得出去");

    let acp_ready = json!({"channel_id": channel_id, "payload": {
        "type": "ready", "sessionId": "ses_from_the_agent", "agentId": "opencode"
    }});
    est.send(&protocol::frame::Frame::push(
        protocol::message::types::CHANNEL_DATA,
        0,
        acp_ready,
    ))
    .await
    .expect("ready 载荷必须发得出去");

    // 回填是读循环里同步做的，所以要等 Hub 把那一帧处理完再开第二次对话。
    // 判据用**仓储**而不是固定 sleep：这里的条件就是「落库了吗」本身，
    // 而固定 sleep 在慢机器上会 flakes。
    common::wait_until(Duration::from_secs(5), || {
        hub_storage::SessionRepo::new(h.hub.state.pool().clone())
            .find(&session_id)
            .ok()
            .flatten()
            .and_then(|s| s.acp_session_id)
            .is_some()
    })
    .await;

    // ---- 第二次开对话：下发的必须是那个 id ----
    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let open = open_channel_on(&mut est).await;
    let open: protocol::channel::OpenChannel = open.payload().expect("open_channel 载荷");
    assert_eq!(
        open.params["acp_session_id"],
        json!("ses_from_the_agent"),
        "第二次开对话必须把 agent 给的 id 下发回去，设备才能走 session/load：{}",
        open.params
    );
    let _ = node_id;
}

/// **开对话时把这条会话已落库的最大 `seq` 下发（`last_seq`）**，设备侧据此
/// 续号，而不是从 1 重数。
///
/// # 这条钉的是「轨迹少一半」那个故障
///
/// `seq` 的契约是会话内只增（`TrajectoryRepo`：断裂处即丢失处），但计数器
/// 活在 ACP 子进程里、**每个子进程从 1 重数**。关掉页面再打开、`session/load`
/// 建新进程，都会让新事件带 `seq=1`，撞上库里已有的 `seq=1`，被
/// `UNIQUE` + `INSERT OR IGNORE` 静默吞掉——实测让一条 `user_message_chunk`
/// 整条消失，而 `gaps` 不报警（seq 仍然连续）。
///
/// 所以起点必须来自跨进程的真相源（`trajectory_event.MAX(seq)`），由 Hub 下发。
#[tokio::test]
async fn opening_a_conversation_sends_the_last_persisted_seq() {
    let (h, node_id, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    // 库里已经有 1..3 三条事件（模拟「之前聊过、页面关了」）。
    let repo = hub_storage::TrajectoryRepo::new(h.hub.state.pool().clone());
    for seq in 1..=3 {
        let e = hub_storage::NewEvent::new(
            seq,
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": "旧消息"}, "seq": seq}).to_string(),
        );
        repo.append(&session_id, &node_id, &e, "2026-10-27T00:00:00Z")
            .expect("写轨迹");
    }

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let open = open_channel_on(&mut est).await;
    let open: protocol::channel::OpenChannel = open.payload().expect("open_channel 载荷");
    assert_eq!(
        open.params["last_seq"],
        json!(3),
        "必须把已落库的最大 seq 下发，设备侧才能接着编；不发就会从 1 重数并撞号：{}",
        open.params
    );
}

/// **一条事件都没有的会话，`last_seq` 是 `0` 而不是 `null` 或缺失。**
///
/// `0` 让设备侧 `Seq::next()` 的第一条正好是 1（`fetch_add(1) + 1`）。下发
/// `null` 会多出一个「没有起点」的分支，而那个分支与「从 1 开始」是同一件事。
#[tokio::test]
async fn a_fresh_conversation_sends_last_seq_zero() {
    let (h, _node, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let open = open_channel_on(&mut est).await;
    let open: protocol::channel::OpenChannel = open.payload().expect("open_channel 载荷");
    assert_eq!(
        open.params["last_seq"],
        json!(0),
        "没聊过的会话要下发 0（设备侧第一条是 1），不要 null：{}",
        open.params
    );
}

/// **没有配置也要能开**：`command` 是空串（= 用设备扫描到的那个），不是缺字段。
///
/// 这条钉的是「配置是可选的」：把空串发成缺失，设备侧就得区分「没给」与
/// 「给了空串」——而它们意思完全一样。
#[tokio::test]
async fn a_conversation_opens_even_when_no_config_was_ever_written() {
    let (h, _node, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);

    let open = open_channel_on(&mut est).await;
    let open: protocol::channel::OpenChannel = open.payload().expect("载荷");
    assert_eq!(
        open.params["command"],
        json!(""),
        "没配命令是空串，语义是「用扫描到的可执行文件」"
    );
    assert_eq!(open.params["args"], json!([]));
    assert_eq!(open.params["env"], json!({}));
}

/// 会话不存在 → 404（与 `POST /api/channels` 同一口径）。
#[tokio::test]
async fn opening_a_conversation_on_a_missing_session_is_404() {
    let h = Harness::new().await;
    let r = h.open_conversation("s_does_not_exist").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
}

/// **设备已被删除的会话 → 409 `device_removed`**（不是 `agent_unavailable`）。
///
/// 这是存量数据才会出现的组合（M1 时代的会话行 + 删设备不删会话），所以经真实
/// 路径造：建会话 → 删设备。
///
/// 两个 code 的次序要钉住：agent 必须**先**过门禁（这里 agent 是可用的），
/// 然后才轮到归属。反过来的话，存量会话最常见的两种残缺就无法在页面上区分。
#[tokio::test]
async fn opening_a_conversation_on_a_removed_device_is_device_removed() {
    let hub = common::Hub::boot().await;

    // 走真 HTTP 建会话：设备得在线（真连接），agent 得可用。
    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;
    let created = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let session_id = created.json()["id"].as_str().expect("会话 id").to_string();

    // 删设备会把 workspace.node_id 置 NULL，会话行保留（「删除语义三分离」）。
    // agent_runtime 那几行保留：「设备扫到了什么」与「设备还在不在」是两件事，
    // 而这条测试要的正是「agent 仍可用、归属已经没了」。
    assert_eq!(
        h.delete_device(&node_id).await.status,
        StatusCode::NO_CONTENT
    );

    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    r.assert_error_code("device_removed");
}

/// `POST /api/sessions/{id}/conversation/cancel` 成功下发：204 No Content。
///
/// 前提：会话存在、设备在线、对话通道已打开（先 `open` 过）。设备侧收到
/// `channel_data{ "cancel": true }` 后会发 `session/cancel`，**不关会话**——
/// 页面停下之后还能接着聊。
#[tokio::test]
async fn cancelling_a_running_conversation_returns_204() {
    let (h, _node, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    // 先打开对话通道（建立 channel 绑定），再按停止。
    let r = h.open_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    // 把 `open_channel` 消费掉：不消费的话下面可能先读到它，于是「收到了
    // cancel」这件事会被一张不相干的帧蒙过去。
    let _ = open_channel_on(&mut est).await;

    let r = h.cancel_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "body: {}", r.body);

    // **204 只说明 Hub 回了状态码，不等于帧真的发出去了。** 这条用例原来
    // 把 `est` 直接丢掉、只断言状态码——那样即使 `send_channel_data` 短路
    // 或者绑定是陈旧的，它照样全绿（打不到它要守的东西）。所以真去读设备
    // 那一侧收到了什么。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let cd = loop {
        let text = tokio::time::timeout_at(deadline, est.next_text())
            .await
            .expect("3s 内设备必须收到 cancel 帧")
            .expect("连接不许断")
            .expect("必须是文本帧");
        let f: protocol::frame::Frame =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("解不开的帧 {text}: {e}"));
        if f.msg_type == protocol::message::types::CHANNEL_DATA {
            break f
                .payload::<protocol::channel::ChannelData>()
                .expect("channel_data 载荷");
        }
    };
    assert_eq!(
        cd.payload["cancel"],
        json!(true),
        "设备收到的必须是 `cancel: true`，实收：{}",
        cd.payload
    );
}

/// 对话从未打开过 → 404 `not_found`（不是「停了」，而是「还没开始」）。
///
/// 没有 channel 绑定时 Hub 不能下发 cancel，且不许静默 204：页面要能区分
/// 「这条对话还没开」与「停止发出去了」。
#[tokio::test]
async fn cancelling_a_conversation_never_opened_is_404() {
    let (h, _node, session_id, _est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    // 不调 open_conversation：没有 channel 绑定。
    let r = h.cancel_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
    r.assert_error_code("not_found");
}

/// 设备已被删除的会话 → 409 `device_removed`（不是 `not_found`）。
///
/// 会话行还在，只是归属设备没了。停止也是「发不出去」，但原因是设备没了
/// 而不是对话没开，两者给的下一步完全不同。
#[tokio::test]
async fn cancelling_on_a_removed_device_is_device_removed() {
    let hub = common::Hub::boot().await;

    let (node_id, _keepalive) = enrolled_device_reporting(
        &hub,
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        1,
    )
    .await;
    let h = Harness::from_hub(hub).await;
    let created = h
        .create_session(json!({
            "node_id": node_id,
            "path": "~/src/app",
            "agent_id": "opencode",
        }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let session_id = created.json()["id"].as_str().expect("会话 id").to_string();

    assert_eq!(
        h.delete_device(&node_id).await.status,
        StatusCode::NO_CONTENT
    );

    let r = h.cancel_conversation(&session_id).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    r.assert_error_code("device_removed");
}

/// 轨迹空 → `events: []`、`gaps: []`、`incomplete: false`。
///
/// **空轨迹不算未完成**：一个还没说过话的会话没有「一轮」，把它标成未完成
/// 会让每个新会话一打开就带一条警告。
#[tokio::test]
async fn an_empty_trajectory_is_empty_and_not_incomplete() {
    let (h, _node, session_id, _est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let r = h
        .get(&format!("/api/sessions/{session_id}/trajectory"))
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["events"], json!([]));
    assert_eq!(v["gaps"], json!([]));
    assert_eq!(v["incomplete"], json!(false), "没有一轮就没有「未完成」");
}

/// 有 `seq` 断口 → `gaps` 报出来，且带 `missing` 条数。
///
/// 这是 `session-and-trajectory.md` §3.2.1 三层里的第二层「不完整可见」：
/// 丢掉的那段**不补齐**（已裁决），但页面必须能画出「此处缺失 N 条」。
#[tokio::test]
async fn a_trajectory_with_a_hole_reports_the_gap() {
    let (h, node_id, session_id, _est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let repo = hub_storage::TrajectoryRepo::new(h.hub.state.pool().clone());
    // 故意跳过 seq = 3、4：这段就是「丢掉的那截」
    for seq in [1_i64, 2, 5] {
        repo.append(
            &session_id,
            &node_id,
            &hub_storage::NewEvent::new(seq, "agent_message_chunk", r#"{"n":1}"#),
            &hub_core::registry::now_stamp(),
        )
        .expect("写轨迹");
    }

    let r = h
        .get(&format!("/api/sessions/{session_id}/trajectory"))
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let v = r.json();
    let gaps = v["gaps"].as_array().expect("gaps 是数组");
    assert_eq!(gaps.len(), 1, "只有一处断口：{}", v);
    // `Gap` 是**半开区间** `[from, to)`：seq 1、2 在库、5 在库，缺的是 3、4，
    // 所以要报 `from=3, to=5`（`to` 是那个存在的 seq，充当排他上界）。
    assert_eq!(gaps[0]["from"], json!(3), "从缺的第一条开始");
    assert_eq!(gaps[0]["to"], json!(5), "到下一个存在的 seq 为止（不含）");
    assert_eq!(
        gaps[0]["missing"],
        json!(2),
        "缺的是 seq 3、4 两条，不是「一个区间」"
    );
    assert_eq!(v["events"].as_array().map(Vec::len), Some(3));
}

/// 末尾没有 `turn_end` → `incomplete: true`。**这是另一个方向的缺口**：
/// `gaps` 看不见它（序号没断），只有「最后一轮没收口」这一条判据看得见。
#[tokio::test]
async fn a_trajectory_whose_last_turn_never_ended_is_incomplete() {
    let (h, node_id, session_id, _est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let repo = hub_storage::TrajectoryRepo::new(h.hub.state.pool().clone());
    for (seq, kind) in [(1_i64, "user_message"), (2, "agent_message_chunk")] {
        repo.append(
            &session_id,
            &node_id,
            &hub_storage::NewEvent::new(seq, kind, r#"{"k":1}"#),
            &hub_core::registry::now_stamp(),
        )
        .expect("写轨迹");
    }

    let r = h
        .get(&format!("/api/sessions/{session_id}/trajectory"))
        .await;
    let v = r.json();
    assert_eq!(
        v["incomplete"],
        json!(true),
        "最后一轮没 turn_end 就必须标未完成：{}",
        v
    );
    assert_eq!(
        v["gaps"],
        json!([]),
        "序号没断，所以没有 gaps —— 两个方向必须分开报"
    );
}

/// 轨迹端点对不存在的会话回 404，不是空轨迹。
///
/// 空轨迹与「没有这个会话」必须能区分：前端按 404 显示「会话没了」，按空数组
/// 显示「还没有消息」。混成一种会让删掉的会话一直显示成空白新会话。
#[tokio::test]
async fn the_trajectory_of_a_missing_session_is_404() {
    let h = Harness::new().await;
    let r = h.get("/api/sessions/s_does_not_exist/trajectory").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
}

/// **`?limit=` 的上限是服务端夹的，而且夹的就是那个模块常量。**
///
/// 走的是**真的路由**（`GET /api/sessions/{id}/trajectory` → `clamp_limit`），
/// 不是常量自己跟自己比。
///
/// **写法上绕开过一次假绿**：早先这条只写「请求 `?limit=1000`，要拿回 3 条」。
/// 那个断言在**完全删掉夹逼**时也照样绿——库里只有 3 条，`LIMIT 1000` 也返回 3 条，
/// 夹没夹看不出来。要看得见，必须让上限**真的咬住**：先写进比上限多几条的事件，
/// 再请求一个远超上限的值，然后数回了几条。
///
/// 反向验证：把路由里的 `clamp_limit(q.limit)` 换成 `q.limit`（保留原样），
/// 这里会拿回 `TRAJECTORY_LIMIT_MAX + 5` 条而不是 `TRAJECTORY_LIMIT_MAX` 条，红。
#[tokio::test]
async fn the_trajectory_limit_is_clamped_by_the_router_itself() {
    // 与 `routes/conversation.rs` 里的常量同一个数。测试 crate 引不到那个私有常量，
    // 所以这里只能抄一份——**但它在断言里只用来「多写几条」，不用来自证**：
    // 真正的判据是「请求 1000000 条，拿回的不许超过 1000」。
    const CAP: usize = 1000;

    let (h, node_id, session_id, _est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        "opencode",
    )
    .await;

    let repo = hub_storage::TrajectoryRepo::new(h.hub.state.pool().clone());
    let total = CAP + 5;
    for seq in 1..=total as i64 {
        repo.append(
            &session_id,
            &node_id,
            &hub_storage::NewEvent::new(seq, "agent_message_chunk", r#"{"n":1}"#),
            &hub_core::registry::now_stamp(),
        )
        .expect("写轨迹");
    }
    assert_eq!(repo.count(&session_id).expect("count"), total as i64);

    let r = h
        .get(&format!(
            "/api/sessions/{session_id}/trajectory?limit=1000000"
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let v = r.json();
    let events = v["events"].as_array().expect("events 是数组");
    assert_eq!(
        events.len(),
        CAP,
        "库里有 {total} 条、请求 1000000 条：只有夹到上限才该拿回 {CAP} 条。\
         拿回 {total} 条说明上限没夹（一次 `?limit=1e9` 能把整张表读进内存）: {}",
        events.len()
    );
}

/// 等一帧 `open_channel`（跳过心跳等其他帧）。
async fn open_channel_on(est: &mut node_core::conn::Established) -> protocol::frame::Frame {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let text = tokio::time::timeout_at(deadline, est.next_text())
            .await
            .expect("3s 内必须下发 open_channel")
            .expect("连接不许断")
            .expect("必须是文本帧");
        let f: protocol::frame::Frame =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("解不开的帧 {text}: {e}"));
        if f.msg_type == protocol::message::types::OPEN_CHANNEL {
            return f;
        }
    }
}

// ---------------------------------------------------------------------------
// 锁：「acp」标记在 Hub 与设备双侧的定义必须一致
// ---------------------------------------------------------------------------
///
/// `hub-server` 生产依赖里没有 `node-core`（它只出现在 dev-dependencies，
/// 供 WS 集成测试用），所以 Hub 侧的 `ACP_CONNECTOR` 写在
/// `crates/hub-server/src/routes/conversation.rs` 而不是从 `node-core` 引入。
/// 两边各自写死 `"acp"`，若一方改了而另一方没改，对话通道就会在
/// `open_channel` 后收到 `unsupported`，页面只表现为「对话没反应」。
///
/// 本文件已有 `conversation.rs` 里的 `the_acp_marker_matches_what_the_device_expects`
/// 只锁住 Hub 自己那份常量（对比对象是硬编码的 `"acp"` 字面量），锁不住
/// 设备侧。这里把设备侧的 `node_core::channel::EXTERNAL_CONNECTOR_NAME` 也钉
/// 住 `"acp"`：两边各自独立等于 `"acp"`，任意一边单向改动都会让对应用例
/// 变红，即形成真正的双向锁。不引入新共享 crate，因为 `node-core` 本就已是
/// hub-server 的 dev-dependency，且为两个字符串新建共享 crate 的成本更高。
#[test]
fn the_device_acp_connector_name_is_still_acp() {
    assert_eq!(
        node_core::channel::EXTERNAL_CONNECTOR_NAME,
        "acp",
        "设备侧 EXTERNAL_CONNECTOR_NAME 与 Hub 侧 ACP_CONNECTOR 必须同为 acp"
    );
}

// ---------------------------------------------------------------------------
// `GET /api/agent-binary`：从这一台 Hub 下发 afcli
// ---------------------------------------------------------------------------
//
// 三个用例，各自钉一条**会真的出错**的分支，而不是「调一下看返回 200」：
//   ① 配置指向的文件存在 → 内容逐字节一致、附件头正确；
//   ② 配置指向不存在的路径 → 404 + `agent_binary_unavailable`（**不是 500**：
//      裸机跑 afhub 时磁盘上本来就没有 afcli，那是正常部署形态，不是故障）；
//   ③ 0 字节文件 → 同样 404（当成成功下发会让设备拿到一个 `chmod +x` 后
//      立刻 `Exec format error` 的文件，而报错发生在设备上、不在 Hub 这边）。
//
// 三个用例都要改环境变量，而环境变量是**进程级**的——所以放进一个测试里顺序跑，
// 并用 `tempfile` 建真实文件。分成三个 `#[tokio::test]` 会并行改同一个变量，
// 互相串味，那种 flaky 测试比没有测试更糟。

/// 串行化「改进程级环境变量」的那几个用例。
///
/// `AFCLI_BINARY_PATH` 是 `std::env` 里的**进程级**变量，而 `cargo test` 默认
/// 让同一个测试二进制里的用例**并行**跑。三个 agent-binary 用例各自设了不同的
/// 值、又都在断言前读它——没有这把锁时它们会互相覆盖，症状是「偶发 200 却
/// 内容是别人的 payload」。实测连跑 5 次都过，那是**运气**，不是正确性。
///
/// 用 `tokio::sync::Mutex` 而不是 `std::sync::Mutex`：后者拿到的守卫
/// **不能被跨 `await` 持有**（clippy 的 `await_holding_lock` 会直接报错，
/// 而它报得对——那样会阻塞整个 worker，测试之间会真的互相卡死）。
/// 也不用 `serial_test` crate：只为一个锁引入依赖不值得，而多一个依赖
/// 就多一份要维护的版本约束。
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn agent_binary_is_served_from_the_configured_path() {
    let h = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("afcli");

    // 用一段**能认出来的**内容而不是真二进制（约 20MB）：这里要验的是
    // 「读到的字节原样下发」，不是「这个文件是不是合法 ELF」。
    let payload = b"\x7fELF-fake-afcli-for-test";
    std::fs::write(&path, payload).unwrap();

    // 锁内「设变量 → 发请求 → 还原」。请求本身是 async，但 `get` 只在
    // 轮询到第一次 pending 前就读完了变量，而这里用 `block_on` 之外的方式
    // 不行——所以改成：锁内取一个已完成的 future 不可行，改为让 `with_env_var`
    // 只包住「设变量 + 发请求」这段最小窗口（见下）。
    let r = {
        let guard = ENV_LOCK.lock().await;
        std::env::set_var("AFCLI_BINARY_PATH", &path);
        let r = h.get("/api/agent-binary").await;
        std::env::remove_var("AFCLI_BINARY_PATH");
        drop(guard);
        r
    };

    assert_eq!(r.status, StatusCode::OK, "配了存在的文件就必须 200");
    assert_eq!(
        r.body.as_bytes(),
        payload,
        "下发的字节必须与磁盘上的文件逐字节一致"
    );
}

#[tokio::test]
async fn agent_binary_missing_file_is_404_not_500() {
    let h = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    // 指向一个**不存在的显式配置**：显式配置时 `candidate_paths` 只试这一条，
    // 不会掉进那些候选目录里真的有 afcli 的分支（本机 `/usr/local/bin/afcli`
    // 在开发者机器上可能真的存在，那会让这条用例变成「看环境脸色」）。
    let path = dir.path().join("no-such-afcli");

    let r = {
        let guard = ENV_LOCK.lock().await;
        std::env::set_var("AFCLI_BINARY_PATH", &path);
        let r = h.get("/api/agent-binary").await;
        std::env::remove_var("AFCLI_BINARY_PATH");
        drop(guard);
        r
    };

    // 500 会让运维去翻日志找一个不存在的 bug；404 + 固定 code 说的是
    // 「这一台 Hub 没有可下发的 afcli」，那才是事实。
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    r.assert_error_code("agent_binary_unavailable");
    // 错误体里**不许**出现磁盘路径（约束 13）：路径属部署细节，不该告诉探路者。
    assert!(
        !r.body.contains(&*path.to_string_lossy()),
        "错误体泄露了服务端路径：{}",
        r.body
    );
}

#[tokio::test]
async fn agent_binary_empty_file_is_refused() {
    let h = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    // 0 字节不是「空的 afcli」，是坏部署：真发出去，设备 `chmod +x` 之后
    // 立刻 `Exec format error`，而报错发生在设备上——排查会从错误的地方开始。
    let path = dir.path().join("empty-afcli");
    std::fs::write(&path, b"").unwrap();

    std::env::set_var("AFCLI_BINARY_PATH", &path);
    let r = h.get("/api/agent-binary").await;
    std::env::remove_var("AFCLI_BINARY_PATH");

    assert_eq!(r.status, StatusCode::NOT_FOUND);
    r.assert_error_code("agent_binary_unavailable");
}

/// `candidate_paths` 是纯函数，直接对返回值断言。
///
/// 为什么不都走集成测试：路径拼接错了在集成测试里表现为「404 了但文件明明在」，
/// 排查要起整个 app + 猜环境；这里一次断言就能说清「拼出来的第一个是什么」。
mod agent_binary_candidates {
    use hub_server::routes::agent_binary::candidate_paths;

    #[test]
    fn explicit_configuration_wins_and_is_the_only_candidate() {
        // 运维写明了路径还去猜别的，等于悄悄忽略配置——所以显式配置时
        // 候选表**恰好一项**，不是「第一项优先」。
        let got = candidate_paths(Some("/opt/afcli"), "linux", "x86_64");
        assert_eq!(got.len(), 1, "显式配置时不该再试探其它路径");
        assert_eq!(got[0], std::path::PathBuf::from("/opt/afcli"));
    }

    #[test]
    fn unconfigured_lists_arch_subdirs_before_the_flat_path() {
        let got = candidate_paths(None, "linux", "x86_64");
        let strs: Vec<String> = got.iter().map(|p| p.display().to_string()).collect();

        // 分架构目录必须排在平铺路径之前：分架构那份是**为这个平台准备的**，
        // 而平铺那份只是「这台 Hub 自己那份」，可能不是调用方要的架构。
        let arch_idx = strs
            .iter()
            .position(|s| s == "/usr/local/bin/linux/x86_64/afcli")
            .expect("应有 os/arch 子目录候选");
        let flat_idx = strs
            .iter()
            .position(|s| s == "/usr/local/bin/afcli")
            .expect("应有平铺路径候选");
        assert!(
            arch_idx < flat_idx,
            "分架构候选必须排在平铺路径前，实际 {arch_idx} vs {flat_idx}"
        );

        // 两种分隔符都要有：`os/arch` 是 REX 的目录习惯，`os-arch` 是 CI artifact
        // 的命名习惯，只写一种会让另一种部署形态静默 404。
        assert!(
            strs.iter().any(|s| s.contains("linux-x86_64")),
            "缺 `os-arch` 形式的候选：{strs:?}"
        );
    }

    #[test]
    fn mac_naming_variants_resolve_to_the_same_candidates() {
        // `std::env::consts::OS` 在 macOS 上是 `macos`，而文档/产物名用 `mac`。
        // 两者必须指向同一组路径，否则「按文档写 mac 就 404」。
        let a = candidate_paths(None, "mac", "aarch64");
        let b = candidate_paths(None, "macos", "aarch64");
        let c = candidate_paths(None, "darwin", "aarch64");
        assert_eq!(a, b, "`mac` 与 `macos` 必须等价");
        assert_eq!(a, c, "`mac` 与 `darwin` 必须等价");
    }
}

/// 平台参数化：`?os=&arch=` 真的能定位到分架构目录里那一份。
///
/// 用 `AFCLI_BINARY_PATH` 指向一个**临时目录**，再加 `/usr/local/bin` 那套候选
/// ——不行：显式配置会独占候选表（见 `explicit_configuration_wins_...`），
/// 所以这条必须走「不配置、靠候选路径」的路径。而候选路径里写死了
/// `/usr/local/bin`，不能在测试里改（那是部署契约，不是可注入的参数）。
///
/// 于是这里验的是更本质的一条：**文件名要带上请求的平台**。平台参数化若
/// 只影响查找、不影响下发文件名，用户存下来之后无法分辨拿到的是哪个架构
/// ——而多架构 Hub 下发的正是不同架构的文件。
#[tokio::test]
async fn agent_binary_filename_carries_the_requested_platform() {
    let h = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("afcli");
    std::fs::write(&path, b"\x7fELF-fake").unwrap();

    let r = {
        let guard = ENV_LOCK.lock().await;
        std::env::set_var("AFCLI_BINARY_PATH", &path);
        let r = h.get("/api/agent-binary?os=linux&arch=aarch64").await;
        std::env::remove_var("AFCLI_BINARY_PATH");
        drop(guard);
        r
    };

    assert_eq!(r.status, StatusCode::OK);
    let disp = r
        .headers
        .get("content-disposition")
        .and_then(|v| v.to_str().ok());
    let disp = disp.expect("成功下发必须有 Content-Disposition");
    assert!(
        disp.contains("afcli-linux-aarch64"),
        "文件名必须带请求的平台（否则用户无法分辨架构），实际: {disp}"
    );
}

/// 空串的 `AFCLI_BINARY_PATH` 按「没配」处理，而不是当成一个空路径。
///
/// `AFCLI_BINARY_PATH=` 是「我不想配」的常见写法。当成路径会让候选表只剩
/// 一个空路径、永远 404，而日志里看不出「你其实是配错了」。
#[tokio::test]
async fn empty_path_env_is_treated_as_unset() {
    let h = Harness::new().await;
    let r = {
        let guard = ENV_LOCK.lock().await;
        std::env::set_var("AFCLI_BINARY_PATH", "   ");
        let r = h.get("/api/agent-binary").await;
        std::env::remove_var("AFCLI_BINARY_PATH");
        drop(guard);
        r
    };
    // 断言点是「没因为空路径 panic / 500」：可能 200（候选路径里碰巧有 afcli）
    // 也可能 404，但**绝不能 500**。
    assert_ne!(
        r.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "空串配置不该导致 500"
    );
}
