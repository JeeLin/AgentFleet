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
    let (mut est, node_id, _kp) = common::connect_enrolled(hub).await;
    common::send_heartbeat(
        &mut est,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": ["terminal"], "agents": ["claude-code"]},
            "resources": {"cpu": 34, "mem": 61},
            "workspace_roots": [],
        }),
    )
    .await;
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
    for path in ["/api/devices", "/api/sessions", "/api/agents"] {
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
