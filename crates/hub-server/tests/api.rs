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
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let created = h
        .create_session(json!({"node_id": node_id, "path": "/srv/app"}))
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
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    let created = h
        .create_session(json!({"node_id": node_id, "path": "/srv/app"}))
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
    let (node_id, _keepalive) = enrolled_device_reporting(&hub, json!(["opencode"])).await;
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
}

/// 建一个「已选 agent、设备可用」的会话，返回 `(harness, node_id, session_id, est)`。
///
/// `est` 是那条真连接：对话端点会往它上面下发 `open_channel`，而 `_keepalive`
/// 的名字在别处——这里必须**返回**它，否则连接一 drop Hub 就判设备离站。
async fn conversation_ready(
    agents: Value,
    with_agent: Option<&str>,
) -> (Harness, String, String, node_core::conn::Established) {
    let hub = common::Hub::boot().await;
    let (node_id, est) = enrolled_device_reporting(&hub, agents).await;
    let h = Harness::from_hub(hub).await;
    let mut body = json!({"node_id": node_id, "path": "~/src/app"});
    if let Some(a) = with_agent {
        body["agent_id"] = json!(a);
    }
    let created = h.create_session(body).await;
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

/// **会话没选 agent → 409 `agent_unavailable`，`detail.agent_id` 是 null。**
///
/// 判据是 null 而不是某个 agent 名：这不是「那个 agent 不可用」，是「这个会话
/// 没有可用的对话对象」。挑一个代进去等于替用户做了会写进历史的决定。
#[tokio::test]
async fn opening_a_conversation_on_a_session_without_an_agent_is_a_conflict() {
    let (h, _node, session_id, _est) = conversation_ready(json!([]), None).await;

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
        Some("opencode"),
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
        Some("opencode"),
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
        Some("opencode"),
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
#[tokio::test]
async fn the_acp_session_id_from_the_ready_frame_comes_back_on_the_next_open() {
    let (h, node_id, session_id, mut est) = conversation_ready(
        json!([{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}]),
        Some("opencode"),
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
        Some("opencode"),
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
        Some("opencode"),
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
        Some("opencode"),
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

/// 轨迹空 → `events: []`、`gaps: []`、`incomplete: false`。
///
/// **空轨迹不算未完成**：一个还没说过话的会话没有「一轮」，把它标成未完成
/// 会让每个新会话一打开就带一条警告。
#[tokio::test]
async fn an_empty_trajectory_is_empty_and_not_incomplete() {
    let (h, _node, session_id, _est) = conversation_ready(json!([]), None).await;

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
    let (h, node_id, session_id, _est) = conversation_ready(json!([]), None).await;

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
    let (h, node_id, session_id, _est) = conversation_ready(json!([]), None).await;

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
