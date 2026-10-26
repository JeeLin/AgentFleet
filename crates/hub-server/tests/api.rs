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
}

impl Reply {
    async fn new(resp: axum::response::Response) -> Self {
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Self {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
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
    enrolled_device_reporting(hub, json!(["claude-code"])).await
}

/// 同上，但 agent 上报内容由调用方给。
///
/// `agents` 既是旧形态（字符串数组）也是新形态（对象数组）的入口——Hub 两种都认。
/// 门禁测试需要精确控制「这台设备上报了什么」，所以不能复用写死的那一份。
async fn enrolled_device_reporting(
    hub: &common::Hub,
    agents: Value,
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
    // 等**库**里有行，不是等内存 `Reports`：门禁读的是库（子任务 5 的核心判据）。
    // 等内存会让测试在一条「库还没写完」的窗口上跑，于是时红时绿。
    common::wait_until(Duration::from_secs(2), || {
        hub.state.reports().get(&node_id).is_some()
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

#[tokio::test]
async fn creating_a_session_returns_a_detail_with_its_locked_workspace() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({"node_id": node_id, "path": "~/src/app"}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["state"], json!("running"));
    assert_eq!(v["agent_id"], Value::Null, "M1 没有 agent 运行时");
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

/// 两个会话指向同一个 `(node_id, path)`：工作区是主实体（子任务 5c 的裁决）。
#[tokio::test]
async fn two_sessions_can_share_one_workspace() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let a = h
        .create_session(json!({"node_id": node_id, "path": "/srv/app"}))
        .await;
    let b = h
        .create_session(json!({"node_id": node_id, "path": "/srv/app"}))
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
        .create_session(json!({"node_id": node_id, "path": "~/src/app"}))
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
        .create_session(json!({"node_id": "n_ghost", "path": "/srv/app"}))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    r.assert_error_code("not_found");
}

/// 越界形状的路径在建会话时就拒（400），不是等面板打开才报错。
#[tokio::test]
async fn a_workspace_path_must_be_absolute_or_home_relative() {
    let h = Harness::new().await;
    for bad in ["src/app", "", "..", "."] {
        let r = h
            .create_session(json!({"node_id": "n_any", "path": bad}))
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
            .create_session(json!({"node_id": "n_ghost", "path": ok}))
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
    assert_eq!(
        r.json(),
        json!([{"name": "claude-code"}, {"name": "opencode"}])
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
        written["updated_at"].as_str().is_some_and(|s| !s.is_empty()),
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
                        format!(
                            "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                            h.session, h.csrf
                        ),
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
    let (node_id, _keepalive) =
        enrolled_device_reporting(&hub, json!(["opencode"])).await;
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
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!([])).await;
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

/// 不选 agent 仍然放行，`agent_id` 为 NULL。**这条不能退化**。
#[tokio::test]
async fn creating_a_session_without_an_agent_still_works() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!([])).await;
    let h = Harness::from_hub(hub).await;

    let r = h
        .create_session(json!({"node_id": node_id, "path": "~/src/app"}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["agent_id"], Value::Null, "不选就是 NULL");
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
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!(["opencode"])).await;

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
