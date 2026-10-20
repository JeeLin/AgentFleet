//! 子任务 5a「浏览器实时数据面（SSE）」的集成测试。
//!
//! 覆盖三组，各对应一个踩过的坑：
//!
//! 1. **`.layer` 次序**：所有 `/api/*` 与 SSE 端点未认证必须 **401**。
//!    404 说明路由压根没挂上，200 说明没被认证覆盖——两种「看起来像通过」
//!    的回归正是本文件存在的主要理由（与 `api.rs` 同款）。
//! 2. **CSRF 双重提交**：四个上行端点全是 `STATE_CHANGING_METHODS`，
//!    缺 `X-CSRF-Token` 头必须 **403**。漏了这条的后果是「终端每次键盘输入
//!    都 403 而所有测试全绿」。
//! 3. **`reject_origin` 的豁免边界**：浏览器带 `Origin` 能建 SSE，
//!    而设备 `/ws` 仍被拒——两者不互破。
//!
//! 另外按测试标准打通了浏览器→Hub→node 的**全链路**：
//! 开通道后 node 侧真的收到 `open_channel`，`/send` 后 node 侧真的收到
//! `channel_data`，node 发来的 `channel_data` 真的进了 SSE 流。

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
use protocol::message::types;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
/// 一个真实浏览器一定会带的 Origin。
const BROWSER_ORIGIN: &str = "http://127.0.0.1:8080";

/// 一个真跑起来的 Hub + 一份同状态的路由 + 已登录的两条 cookie。
///
/// 与 `api.rs` 的同名夹具同款：设备侧走真 WS（`common::Hub`），页面侧走
/// `oneshot` 直打路由，两者共享同一个 `AppState`——「帧到了没到」这件事
/// 只有同一份状态才作数。
struct Harness {
    /// 持有它就是持有「这台 Hub 还活着并且 WS serve 还在跑」。
    #[allow(dead_code)]
    hub: common::Hub,
    app: Router,
    session: String,
    csrf: String,
}

impl Harness {
    async fn new() -> Self {
        Self::from_hub(common::Hub::boot().await).await
    }

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
        Self {
            hub,
            app,
            session: cookie_of(&login, SESSION_COOKIE).expect("af_session"),
            csrf: cookie_of(&login, CSRF_COOKIE).expect("af_csrf"),
        }
    }

    /// 一条**带完整 cookie + Origin**的页面请求（浏览器的样子）。
    fn browser_req(&self, method: &str, uri: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                "cookie",
                format!(
                    "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                    self.session, self.csrf
                ),
            )
            .header("origin", BROWSER_ORIGIN);
        if method != "GET" {
            req = req.header(CSRF_HEADER, &self.csrf);
        }
        req.body(Body::empty()).unwrap()
    }

    async fn call(&self, method: &str, uri: &str) -> Reply {
        self.call_with(method, uri, Body::empty()).await
    }

    async fn call_with(&self, method: &str, uri: &str, body: Body) -> Reply {
        let mut req = self.browser_req(method, uri);
        *req.body_mut() = body;
        Reply::new(self.app.clone().oneshot(req).await.expect("app responds")).await
    }

    /// `POST /api/channels`，带 `X-CSRF-Token`。
    async fn open_channel(&self, session_id: &str, connector: &str) -> Reply {
        self.call_with(
            "POST",
            "/api/channels",
            Body::from(
                json!({"session_id": session_id, "connector": connector, "params": {}}).to_string(),
            ),
        )
        .await
    }
}

/// 一条**有 session cookie、有 Origin、就是没有 `X-CSRF-Token` 头**的请求。
///
/// 单独成一个函数而不是逐条内联：这条夹具的三个要素每一个都不能漏，
/// 而漏掉任何一个都会让断言变成别的东西（漏 cookie → 401、漏 Origin →
/// 测的就不是「CSRF 缺头」而是「没带 Origin」）。
fn no_csrf(h: &Harness, method: &str, uri: &str, body: Body) -> Request<Body> {
    let mut req = h.browser_req("GET", "/");
    *req.uri_mut() = uri.parse().unwrap();
    *req.method_mut() = method.parse().unwrap();
    *req.body_mut() = body;
    // `browser_req` 只给 GET 加 CSRF 头；这里显式确保它真的没有
    req.headers_mut().remove(CSRF_HEADER);
    req
}

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
}

/// enroll 一台设备、发一次心跳（带 connector 上报），返回 `(node_id, 连接)`。
///
/// **连接必须被调用方一直拿着**：`Established` 一掉就是 WS 断开，
/// `POST /api/channels` 会立刻回 409 `device_offline`。
async fn enrolled_device(hub: &common::Hub) -> (String, node_core::conn::Established) {
    let (mut est, node_id, _kp) = common::connect_enrolled(hub).await;
    common::send_heartbeat(
        &mut est,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": ["terminal"], "agents": []},
            "resources": null,
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

/// 建一个真会话（返回 id）。
async fn make_session(h: &Harness, node_id: &str) -> String {
    let r = h
        .call_with(
            "POST",
            "/api/sessions",
            Body::from(json!({"node_id": node_id, "path": "~/src/app"}).to_string()),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    r.json()["id"].as_str().expect("session id").to_string()
}

/// 一条 SSE 事件。
#[derive(Debug)]
struct SseEvent {
    id: Option<String>,
    name: Option<String>,
    data: String,
    /// `data:` 逐行；`data` 字段是它们的 `\n` 连接。
    data_lines: Vec<String>,
}

/// 开一条 SSE 连接（`oneshot` 直打路由）。
///
/// **读法不是「一次性 collect」**：SSE 是长连接，一次性收集会永远挂住。
/// 这里只读够用的几条就返回。
async fn open_stream(h: &Harness, last_event_id: Option<&str>) -> (StatusCode, axum::body::Body) {
    let mut req = h.browser_req("GET", "/api/stream");
    if let Some(last) = last_event_id {
        req.headers_mut()
            .insert("last-event-id", last.parse().unwrap());
    }
    let resp = h.app.clone().oneshot(req).await.expect("app responds");
    (resp.status(), resp.into_body())
}

/// 从一个 SSE body 里读满 `want` 条事件（带超时，不许恒真）。
///
/// **收参 `&mut Body` 而不是 `Body`**：用例要「读完第一条 → 拿它的 id 当断点 →
/// 模拟断线 → 用同一个 body 再看有没有更多」。按值收的话第一步就把 body 移动走了。
async fn read_events(body: &mut axum::body::Body, want: usize) -> Vec<SseEvent> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while parse_frames(&buf).len() < want {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                let chunk = frame.into_data().expect("data frame");
                buf.push_str(&String::from_utf8_lossy(&chunk));
            }
            // 流结束、读出错、等超时：这三条都意味着「剩下的读不到了」。
            // **这里只 break，不自己断言**——读到了几条由调用方判。
            // 否则「一条都没收到」与「收到全部」会走同一条路径（恒真断言）。
            Ok(None) | Ok(Some(Err(_))) | Err(_) => break,
        }
    }
    parse_frames(&buf)
}

/// 把 SSE 文本切成一组事件。
///
/// **分帧按「一条事件从 `id: ` 开始」，不是按空行**：axum 0.8 的
/// `Event::finalize` 只在末尾补**一个** `\n`，一条事件的线上字节是
/// `id: 1\nevent: x\ndata: y\n`，中间没有空行。按空行分会让每个事件都
/// 读不满 data（然后在下一个事件到来时被并进去）。
///
/// 空行单独成帧（`: resync: …\n\n` 那种注释帧）——`EventSource` 也不把它
/// 当事件，而测试要能看见「这段订阅有缺口」。
fn parse_frames(text: &str) -> Vec<SseEvent> {
    let mut out = Vec::new();
    let mut cur: Option<SseEvent> = None;
    let flush = |out: &mut Vec<SseEvent>, cur: &mut Option<SseEvent>| {
        if let Some(mut e) = cur.take() {
            e.data = std::mem::take(&mut e.data_lines).join("\n");
            out.push(e);
        }
    };
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        // 新一条事件：上一条如果没被空行收尾，在这儿收（HTTP 传输允许
        // 一次 frame 装多条事件，所以必须按 `id:` 而不是按 chunk 切）
        if let Some(v) = line.strip_prefix("id: ") {
            flush(&mut out, &mut cur);
            cur = Some(SseEvent {
                id: Some(v.to_string()),
                name: None,
                data: String::new(),
                data_lines: Vec::new(),
            });
            continue;
        }
        if line.is_empty() {
            flush(&mut out, &mut cur);
            continue;
        }
        match cur.as_mut() {
            Some(e) => {
                if let Some(v) = line.strip_prefix("event: ") {
                    e.name = Some(v.to_string());
                } else if let Some(v) = line.strip_prefix("data: ") {
                    e.data_lines.push(v.to_string());
                } else if let Some(v) = line.strip_prefix(':') {
                    // 注释帧：`Event` 把它塞在 data 位置（见 stream.rs 的注释）
                    e.data_lines.push(v.trim().to_string());
                } else {
                    e.data_lines.push(line.to_string());
                }
            }
            // 事件还没开始就先来了注释（`: keep-alive`）——记成一条匿名事件，
            // 好让「注释帧」这件事在断言里看得见
            None => {
                if let Some(v) = line.strip_prefix(':') {
                    out.push(SseEvent {
                        id: None,
                        name: None,
                        data: v.trim().to_string(),
                        data_lines: Vec::new(),
                    });
                }
            }
        }
    }
    flush(&mut out, &mut cur);
    out
}

// ---------------------------------------------------------------------------
// 坑 1：`.layer` 次序——未认证必须 401（不是 404，也不是 200）
// ---------------------------------------------------------------------------

/// 五个新端点 + SSE 端点未认证一律 401。
///
/// 这是子任务 5 已经踩过一次的那个坑：新路由若写在 `.layer(require_auth)`
/// **之后**，就不在认证覆盖范围内。四个上行端点全是状态变更方法，
/// 一旦漏掉覆盖层，任何人都能在别人的 Hub 上开终端、往设备打字节。
#[tokio::test]
async fn every_stream_and_channel_route_is_covered_by_the_auth_layer() {
    let h = Harness::new().await;
    let cases: [(&str, &str); 6] = [
        ("GET", "/api/stream"),
        ("POST", "/api/channels"),
        ("POST", "/api/channels/ch-x/send"),
        ("POST", "/api/channels/ch-x/resize"),
        ("DELETE", "/api/channels/ch-x"),
        ("GET", "/api/channels"),
    ];
    for (method, path) in cases {
        let resp = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        // 404 = 路由没挂上；200 = 没被认证覆盖（数据裸奔）
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} 未认证必须是 401，实际 {status}"
        );
    }
}

/// 伪造 cookie 也是 401，且**四个上行端点同样要求 CSRF**：它们是先验 session、
/// 再验 CSRF，伪造 cookie 连第一关都过不去。
#[tokio::test]
async fn a_forged_cookie_cannot_reach_any_uplink_endpoint() {
    let h = Harness::new().await;
    for (method, path) in [
        ("POST", "/api/channels"),
        ("POST", "/api/channels/ch-x/send"),
        ("POST", "/api/channels/ch-x/resize"),
        ("DELETE", "/api/channels/ch-x"),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", format!("{SESSION_COOKIE}=forged.signature"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} 伪造 cookie 必须 401"
        );
    }
}

// ---------------------------------------------------------------------------
// 坑 2：CSRF 双重提交——四个上行端点缺 X-CSRF-Token 头必须 403
// ---------------------------------------------------------------------------

/// **四个上行端点各一条「缺 CSRF 头 → 403」**。
///
/// 这条是这个文件里最要紧的一条：`middleware_auth` 的
/// `STATE_CHANGING_METHODS` 含 POST/DELETE，规则 3 要求
/// `X-CSRF-Token` 头与 `af_csrf` cookie 同值。前端（`http.ts`）负责回填，
/// 漏了就是「终端每一次键盘输入都 403」——而如果这里不测，全绿也照样打不了字。
#[tokio::test]
async fn every_uplink_endpoint_rejects_a_missing_csrf_header() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.body);
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();

    // 有 cookie、有 session，**只是没有那个头**
    let cases: Vec<(&str, String, Body)> = vec![
        (
            "POST",
            "/api/channels".to_string(),
            Body::from(json!({"session_id": session_id, "connector": "terminal"}).to_string()),
        ),
        (
            "POST",
            format!("/api/channels/{channel_id}/send"),
            Body::from("ls\n"),
        ),
        (
            "POST",
            format!("/api/channels/{channel_id}/resize"),
            Body::from(json!({"cols": 100, "rows": 30}).to_string()),
        ),
        (
            "DELETE",
            format!("/api/channels/{channel_id}"),
            Body::empty(),
        ),
    ];
    for (method, uri, body) in cases {
        let req = no_csrf(&h, method, &uri, body);
        let r = Reply::new(h.app.clone().oneshot(req).await.expect("app responds")).await;
        assert_eq!(
            r.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} 缺 X-CSRF-Token 头必须 403，实际 {}（body {}）",
            r.status,
            r.body
        );
        assert_eq!(
            r.json()["error"],
            json!("csrf_token_missing_or_mismatch"),
            "错误码要与 middleware_auth 一致"
        );
    }

    // 头带错值同样 403（同一条规则的两半）
    let mut req = h.browser_req("POST", "/api/channels");
    req.headers_mut()
        .insert(CSRF_HEADER, "not-the-cookie-value".parse().unwrap());
    let r = Reply::new(h.app.clone().oneshot(req).await.expect("app responds")).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "头与 cookie 不同值必须 403"
    );
}

/// `GET /api/stream` 是 GET，不在 `STATE_CHANGING_METHODS` 里，
/// **不需要** CSRF 头——它只读，而读取靠的是 `SameSite=Strict` 的 session cookie。
#[tokio::test]
async fn the_downlink_needs_no_csrf_header() {
    let h = Harness::new().await;
    let (status, _body) = open_stream(&h, None).await;
    assert_eq!(status, StatusCode::OK, "SSE 建连必须 200");
}

// ---------------------------------------------------------------------------
// 坑 3：Origin 豁免边界——浏览器带 Origin 能建 SSE，设备 /ws 仍被拒
// ---------------------------------------------------------------------------

/// 浏览器带 `Origin` 能建 SSE。
///
/// `Harness::browser_req` 每条请求都带 `BROWSER_ORIGIN`，所以上面每一条
/// SSE 用例都已经在证明这件事；这条把它单独钉出来，连同一个响应体形状
/// （`text/event-stream`）一起断言。
#[tokio::test]
async fn a_browser_origin_header_still_builds_the_event_stream() {
    let h = Harness::new().await;
    let resp = h
        .app
        .clone()
        .oneshot(h.browser_req("GET", "/api/stream"))
        .await
        .expect("app responds");
    assert_eq!(resp.status(), StatusCode::OK, "带 Origin 必须能建连");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "SSE 端点必须声明自己的媒体类型，否则浏览器不会当事件流解析"
    );
}

/// 显式带 `Origin` 连 `/ws`：必须被拒（403 `ws_origin_rejected`）。
///
/// 这就是「浏览器带 Origin 能建 SSE，而设备 `/ws` 仍被拒」的后一半——
/// 用真实 WebSocket 握手手写 `Origin` 头，两侧行为必须相反。
#[tokio::test]
async fn a_websocket_handshake_carrying_origin_is_refused() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let hub = common::Hub::boot().await;
    let mut req = format!("ws://{}/ws", hub.addr)
        .into_client_request()
        .expect("valid ws url");
    req.headers_mut()
        .insert("origin", BROWSER_ORIGIN.parse().unwrap());
    let err = tokio_tungstenite::connect_async(req)
        .await
        .expect_err("带 Origin 的设备 WS 必须被拒");
    // tungstenite 把非 101 的应答包成 `Error::Http(response)`
    let resp = match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => resp,
        other => panic!("被拒的形状应是 HTTP 响应，实际 {other:?}"),
    };
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "被拒的形状与设备侧一致：403 + 固定 code"
    );
}

// ---------------------------------------------------------------------------
// 上行四端点：成功 / 404 / 401 / 403
// ---------------------------------------------------------------------------

/// `POST /api/channels` 成功：201 + `(channel_id, node_id, session_id)`，
/// 且 **node 侧真的收到 `open_channel` 帧**。
#[tokio::test]
async fn opening_a_channel_reaches_the_device_as_an_open_channel_frame() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;

    let r = h
        .call_with(
            "POST",
            "/api/channels",
            Body::from(
                json!({
                    "session_id": session_id,
                    "connector": "terminal",
                    "params": {"cwd": "/srv/app"},
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let v = r.json();
    let channel_id = v["channel_id"].as_str().expect("channel_id").to_string();
    assert_eq!(v["node_id"], json!(node_id), "通道必须落在会话绑定的设备上");
    assert_eq!(v["session_id"], json!(session_id));
    assert!(!channel_id.is_empty());

    // node 侧真的收到了（这不是「Hub 说发出去就当成功」）
    let open = next_frame_of(&mut est, types::OPEN_CHANNEL).await;
    let spec: protocol::channel::OpenChannel = open.payload().unwrap();
    assert_eq!(spec.channel_id, channel_id, "channel_id 要与响应里那个一致");
    assert_eq!(spec.connector, "terminal");
    assert_eq!(spec.params["cwd"], json!("/srv/app"), "params 原样透传");
    assert_eq!(
        spec.credit_start,
        protocol::channel::CREDIT_START,
        "初始信用取协议常量"
    );
}

/// 设备**没上报**这个 connector → 409，且响应里带上「它到底上报了什么」。
///
/// 夹具里的设备上报 `["terminal"]`，所以 exec 必须被拒。关键不在 409 本身，
/// 而在 `reported` 字段：页面要能说清「这台机器只装了 terminal」，而不是给用户
/// 一个没有主语的失败。
#[tokio::test]
async fn opening_a_channel_for_an_unreported_connector_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;

    let r = h.open_channel(&session_id, "exec").await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["error"], json!("connector_not_reported"));
    assert_eq!(v["detail"]["connector"], json!("exec"));
    assert_eq!(
        v["detail"]["reported"],
        json!(["terminal"]),
        "必须回设备真实上报的能力表，页面才能解释为什么开不了"
    );

    // 而且**没有**下发 open_channel：门禁在写 socket 之前就拦住了。
    // 用「等一小段时间确认没有这一帧」而不是 drain 整个流——后者在
    // 「恰好没发」与「发了别的帧」之间分不清。
    let got =
        next_frame_of_or_timeout(&mut est, types::OPEN_CHANNEL, Duration::from_millis(300)).await;
    assert!(got.is_none(), "被拒的请求不得下发 open_channel");
}

/// 反过来：**上报了** 的 connector 照常开通。
///
/// 没有这条，上面那条测试可以靠「永远返回 409」通过——那正是门禁写坏时
/// 最可能的形态。
#[tokio::test]
async fn opening_a_channel_for_a_reported_connector_still_works() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;

    let r = h.open_channel(&session_id, "terminal").await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let open = next_frame_of(&mut est, types::OPEN_CHANNEL).await;
    let spec: protocol::channel::OpenChannel = open.payload().unwrap();
    assert_eq!(spec.connector, "terminal");
}

/// 会话不存在 → 404（与 `POST /api/sessions` 的 404 口径一致）。
#[tokio::test]
async fn opening_a_channel_on_an_unknown_session_is_404() {
    let h = Harness::new().await;
    let r = h.open_channel("s_ghost", "terminal").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"], json!("not_found"));
}

/// 会话已结束 → 409（存在但不能用，与 404 不是一回事）。
#[tokio::test]
async fn opening_a_channel_on_an_ended_session_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    h.hub
        .state
        .pool()
        .get()
        .unwrap()
        .execute(
            "UPDATE session SET state = 'ended', ended_at = 'unix:1' WHERE id = ?1",
            [&session_id],
        )
        .unwrap();

    let r = h.open_channel(&session_id, "terminal").await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.json()["error"], json!("session_ended"));
}

/// 设备离线 → 409 `device_offline`（页面要显示「为什么不行」）。
#[tokio::test]
async fn opening_a_channel_on_an_offline_device_is_a_conflict() {
    let hub = common::Hub::boot().await;
    let (node_id, keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    drop(keepalive);
    common::wait_until(Duration::from_secs(2), || {
        h.hub.state.connections().get(&node_id).is_none()
    })
    .await;

    let r = h.open_channel(&session_id, "terminal").await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.json()["error"], json!("device_offline"));
}

/// `POST /api/channels/{id}/send`：成功（204）且 **node 侧真的收到
/// `channel_data`**——否则 `/send` 可能到验收日都没被真实执行过一次。
#[tokio::test]
async fn sending_bytes_reaches_the_device_as_a_channel_data_frame() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();
    drain_until(&mut est, types::OPEN_CHANNEL).await;

    let r = h
        .call_with(
            "POST",
            &format!("/api/channels/{channel_id}/send"),
            Body::from("echo hi\n"),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "body: {}", r.body);

    let f = next_frame_of(&mut est, types::CHANNEL_DATA).await;
    let data: protocol::channel::ChannelData = f.payload().unwrap();
    assert_eq!(data.channel_id, channel_id);
    assert_eq!(
        data.payload["bytes"],
        json!("echo hi\n"),
        "原始字节要原样转成 channel_data 的 bytes 载荷"
    );
}

/// 非法 UTF-8 → 400（不是把 `�` 悄悄塞进用户的 keystroke）。
#[tokio::test]
async fn sending_non_utf8_bytes_is_rejected() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();

    let r = h
        .call_with(
            "POST",
            &format!("/api/channels/{channel_id}/send"),
            Body::from(vec![0xffu8, 0xfe]),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], json!("invalid_request_body"));
}

/// `POST /api/channels/{id}/send` 通道不存在 → 404。
#[tokio::test]
async fn sending_to_an_unknown_channel_is_404() {
    let h = Harness::new().await;
    let r = h
        .call_with("POST", "/api/channels/ch-nope/send", Body::from("x"))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"], json!("not_found"));
}

/// `POST /api/channels/{id}/resize`：成功，且转成 `channel_data`
/// 的 `{"resize": {...}}` 载荷（**不新增协议消息类型**，约束 14）。
#[tokio::test]
async fn resizing_is_translated_into_a_channel_data_frame() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();
    drain_until(&mut est, types::OPEN_CHANNEL).await;

    let r = h
        .call_with(
            "POST",
            &format!("/api/channels/{channel_id}/resize"),
            Body::from(json!({"cols": 132, "rows": 43}).to_string()),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "body: {}", r.body);

    let f = next_frame_of(&mut est, types::CHANNEL_DATA).await;
    let data: protocol::channel::ChannelData = f.payload().unwrap();
    assert_eq!(data.payload["resize"]["cols"], json!(132));
    assert_eq!(data.payload["resize"]["rows"], json!(43));
}

/// 越界的尺寸 → 400（`0` 会被内核当「未设置」，超 `u16` 会让 node 侧
/// 报一句对用户毫无用处的话）。
#[tokio::test]
async fn an_out_of_range_terminal_size_is_rejected() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();

    for bad in [
        json!({"cols": 0, "rows": 24}),
        json!({"cols": 80, "rows": 0}),
        json!({"cols": 70000, "rows": 24}),
        json!({"cols": "80", "rows": 24}),
    ] {
        let r = h
            .call_with(
                "POST",
                &format!("/api/channels/{channel_id}/resize"),
                Body::from(bad.to_string()),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad} 必须 400");
    }
}

/// `POST /api/channels/{id}/resize` 通道不存在 → 404。
#[tokio::test]
async fn resizing_an_unknown_channel_is_404() {
    let h = Harness::new().await;
    let r = h
        .call_with(
            "POST",
            "/api/channels/ch-nope/resize",
            Body::from(json!({"cols": 80, "rows": 24}).to_string()),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

/// `DELETE /api/channels/{id}`：成功（204）且设备侧收到 `close_channel`。
#[tokio::test]
async fn closing_a_channel_tells_the_device_to_close_it() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();
    drain_until(&mut est, types::OPEN_CHANNEL).await;

    let r = h
        .call("DELETE", &format!("/api/channels/{channel_id}"))
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "body: {}", r.body);

    let f = next_frame_of(&mut est, types::CLOSE_CHANNEL).await;
    let close: protocol::channel::CloseChannel = f.payload().unwrap();
    assert_eq!(close.channel_id, channel_id);
}

/// 关通道**幂等**：再关一次仍 204（前端「关面板」与「切 tab」会连打两次，
/// 让第二次变成 404 只会逼出一个「先查再关」的往返）。
#[tokio::test]
async fn closing_a_channel_twice_is_idempotent() {
    let hub = common::Hub::boot().await;
    let (node_id, _keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();

    for i in 0..2 {
        let r = h
            .call("DELETE", &format!("/api/channels/{channel_id}"))
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "第 {i} 次必须仍是 204");
    }
}

/// 设备掉线（Hub 侧整节点清账）之后再关通道仍 204——没有需要通知的对象。
#[tokio::test]
async fn closing_a_channel_of_a_gone_device_is_still_204() {
    let hub = common::Hub::boot().await;
    let (node_id, keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();

    drop(keepalive);
    common::wait_until(Duration::from_secs(2), || {
        h.hub.state.connections().get(&node_id).is_none()
    })
    .await;
    let r = h
        .call("DELETE", &format!("/api/channels/{channel_id}"))
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
}

/// 设备掉线后，那条通道的上行端点一律 404（绑定随连接一起清掉了）。
#[tokio::test]
async fn a_channel_of_a_disconnected_device_is_forgotten() {
    let hub = common::Hub::boot().await;
    let (node_id, keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let session_id = make_session(&h, &node_id).await;
    let opened = h.open_channel(&session_id, "terminal").await;
    let channel_id = opened.json()["channel_id"].as_str().unwrap().to_string();

    drop(keepalive);
    common::wait_until(Duration::from_secs(2), || {
        h.hub.state.connections().get(&node_id).is_none()
    })
    .await;
    common::wait_until(Duration::from_secs(2), || {
        h.hub.state.streams().last_event_id() > 0
    })
    .await;

    let r = h
        .call_with(
            "POST",
            &format!("/api/channels/{channel_id}/send"),
            Body::from("x"),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "绑定必须随连接一起清掉，否则 /send 会一直打向一台已经走的设备"
    );
}

/// `DELETE /api/channels/{id}` 通道不存在 → 204（幂等，见上）。
#[tokio::test]
async fn closing_an_unknown_channel_is_idempotent_not_404() {
    let h = Harness::new().await;
    let r = h.call("DELETE", "/api/channels/ch-nope").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
}

/// body 形状不对 → 400，且错误体仍是 `{"error": "<code>"}` 那个形状。
#[tokio::test]
async fn a_malformed_open_body_is_rejected_before_any_write() {
    let h = Harness::new().await;
    let r = h
        .call_with(
            "POST",
            "/api/channels",
            Body::from(json!({"session_id": 7}).to_string()),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], json!("invalid_request_body"));
}

// ---------------------------------------------------------------------------
// 下行：SSE 的事件与 `Last-Event-ID` 续收
// ---------------------------------------------------------------------------

/// 设备发来的 `channel_data` 进 SSE 流（浏览器→Hub→node 的反向方向）。
///
/// 断言的是**形状**：`channel.frame` 事件带 `(node_id, channel_id, payload)`，
/// 而**不含** `Frame` 的 `v` / `seq`（那是设备侧 WS 的语义，页面不重放）。
#[tokio::test]
async fn device_channel_data_reaches_the_browser_event_stream() {
    let hub = common::Hub::boot().await;
    let (node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    // 先订阅，再发数据——顺序反了就只能靠回放缓冲，那会掩盖接线错误
    let (status, mut body) = open_stream(&h, None).await;
    assert_eq!(status, StatusCode::OK);

    est.send(&protocol::frame::Frame::push(
        types::CHANNEL_DATA,
        3,
        json!({"channel_id": "ch-pi", "payload": {"bytes": "hello\n"}}),
    ))
    .await
    .unwrap();

    let events = read_events(&mut body, 1).await;
    let ev = events.first().expect("must receive a channel.frame");
    assert_eq!(ev.name.as_deref(), Some("channel.frame"));
    assert!(
        ev.id.is_some(),
        "每条事件都要带 id（Last-Event-ID 的取值来源）"
    );
    let v: Value = serde_json::from_str(&ev.data).expect("json payload");
    assert_eq!(v["node_id"], json!(node_id));
    assert_eq!(v["channel_id"], json!("ch-pi"));
    assert_eq!(v["payload"]["bytes"], json!("hello\n"));
    assert!(
        v.get("seq").is_none() && v.get("v").is_none(),
        "设备侧 WS 的 seq/v 不该出现在页面事件里：{}",
        ev.data
    );
}

/// 一个 session 多个面板订阅同一通道时，帧**不重复也不丢失**。
///
/// 两个订阅者各收一份（broadcast 的语义），而**各自内部**一条不多一条不少——
/// 「按 channel_id 各自收一份」这件事是设备侧账本的职责，Hub 广播的是同一份
/// 原始数据（约束 4：通道键带 node_id，两台设备的同名通道互不相干）。
#[tokio::test]
async fn several_subscribers_each_see_every_frame_exactly_once() {
    let hub = common::Hub::boot().await;
    let (_node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    let (_s1, body_a) = open_stream(&h, None).await;
    let (_s2, body_b) = open_stream(&h, None).await;
    let (_s3, body_c) = open_stream(&h, None).await;

    // 两条数据（第一条夹带一个会被 Hub 顺手记账的 credit 往返，不影响事件流）
    for i in 0..2u64 {
        est.send(&protocol::frame::Frame::push(
            types::CHANNEL_DATA,
            10 + i,
            json!({"channel_id": "ch-pi", "payload": {"bytes": format!("line-{i}\n")}}),
        ))
        .await
        .unwrap();
    }

    for (who, mut body) in [("A", body_a), ("B", body_b), ("C", body_c)] {
        let events = read_events(&mut body, 2).await;
        let lines: Vec<String> = events
            .iter()
            .filter(|e| e.name.as_deref() == Some("channel.frame"))
            .map(|e| {
                serde_json::from_str::<Value>(&e.data).expect("json")["payload"]["bytes"]
                    .as_str()
                    .expect("bytes")
                    .to_string()
            })
            .collect();
        assert_eq!(lines, vec!["line-0\n", "line-1\n"], "订阅者 {who} 的帧");
        // 序号互不相同（同一个进程发的事件，id 全局单调）
        let ids: std::collections::HashSet<&str> =
            events.iter().filter_map(|e| e.id.as_deref()).collect();
        assert_eq!(ids.len(), events.len(), "id 不得重复");
    }
}

/// `Last-Event-ID` 续收：断线期间的事件补齐，已收过的不重发。
#[tokio::test]
async fn reconnecting_with_last_event_id_replays_the_gap_only() {
    let hub = common::Hub::boot().await;
    let (_node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    // 第一条：拿它的 id 当断点
    let (status, mut first_body) = open_stream(&h, None).await;
    assert_eq!(status, StatusCode::OK);
    est.send(&protocol::frame::Frame::push(
        types::CHANNEL_DATA,
        1,
        json!({"channel_id": "ch-1", "payload": {"bytes": "first\n"}}),
    ))
    .await
    .unwrap();
    let got = read_events(&mut first_body, 1).await;
    let last_id = got.first().and_then(|e| e.id.clone()).expect("an id");
    drop(first_body); // 断线

    // 断线期间又来了两条
    for i in 0..2u64 {
        est.send(&protocol::frame::Frame::push(
            types::CHANNEL_DATA,
            20 + i,
            json!({"channel_id": "ch-1", "payload": {"bytes": format!("gap-{i}\n")}}),
        ))
        .await
        .unwrap();
    }

    let (status, mut second_body) = open_stream(&h, Some(&last_id)).await;
    assert_eq!(status, StatusCode::OK);
    let replayed = read_events(&mut second_body, 2).await;
    let lines: Vec<String> = replayed
        .iter()
        .filter(|e| e.name.as_deref() == Some("channel.frame"))
        .map(|e| {
            serde_json::from_str::<Value>(&e.data).expect("json")["payload"]["bytes"]
                .as_str()
                .expect("bytes")
                .to_string()
        })
        .collect();
    assert_eq!(
        lines,
        vec!["gap-0\n", "gap-1\n"],
        "只补断点之后的；第一条不得重发"
    );
    for e in &replayed {
        let id: u64 = e.id.as_deref().expect("id").parse().expect("numeric");
        assert!(
            id > last_id.parse::<u64>().expect("numeric"),
            "续收的事件序号必须严格大于断点"
        );
    }
}

/// **不带** `Last-Event-ID`（首次连接）→ 一条历史都不补。
///
/// 把它当成「从头补一遍」会让每次刷新都收到一屏旧输出，而 PTY 的输出
/// 是不可重放的字节流——重放等于在终端里灌一遍刚才的命令。
#[tokio::test]
async fn a_first_subscription_gets_no_history() {
    let hub = common::Hub::boot().await;
    let (_node_id, mut est) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;

    // 顺序很关键：**先订阅，再发数据**。
    //
    // 反过来（先发后订阅）测的就不是「回放」而是「实时」——那条数据落在
    // 订阅建立之后，本来就该收得到。这条要钉的是「不带 `Last-Event-ID`
    // 时**先前**发布过的东西一条都不补」，所以事件必须在建流之前存在。
    // 记下当前序号，等「**这一帧**」落地，而不是等「序号 > 0」——
    // enroll 本身已经发过一条 `device.status{online}`（序号 1），拿它当
    // 「已经发过」的判据会在这条数据还没进总线时就放行，于是订阅建立之后
    // 它才到，测到的就变成「实时」而不是「不回放」了。
    let before = h.hub.state.streams().last_event_id();
    est.send(&protocol::frame::Frame::push(
        types::CHANNEL_DATA,
        1,
        json!({"channel_id": "ch-1", "payload": {"bytes": "old\n"}}),
    ))
    .await
    .unwrap();
    common::wait_until(Duration::from_secs(2), || {
        h.hub.state.streams().last_event_id() > before
    })
    .await;

    let (status, mut body) = open_stream(&h, None).await;
    assert_eq!(status, StatusCode::OK);
    // 读 400ms：一条都不该来（否则就是有回放漏出去了）
    let early = read_events_for(&mut body, Duration::from_millis(400)).await;
    assert!(early.is_empty(), "首次连接不得回放历史：{early:?}");
}

/// 设备连接建立/断开 → `device.status` 事件。
///
/// `offline` 只在「这条连接确实是当前连接」时推：被顶替的旧连接自己也会走到
/// 断开处理器，而那时表里已经是新连接、设备明明在线——推一发 offline 会让页面
/// 把一台好端端的设备标成离线，且没有任何东西会把它改回来。
#[tokio::test]
async fn device_status_events_follow_the_connection() {
    let hub = common::Hub::boot().await;
    let (_node_id, keepalive) = enrolled_device(&hub).await;
    let h = Harness::from_hub(hub).await;
    drop(keepalive);
    common::wait_until(Duration::from_secs(2), || {
        h.hub.state.streams().last_event_id() >= 2
    })
    .await;
    assert!(
        h.hub.state.streams().last_event_id() >= 2,
        "前提：online 与 offline 两条事件都已经发过了"
    );

    // 走**回放**（`Last-Event-ID: 0`）把这两条捞回来：它们都发生在建流之前，
    // 而不带断点的订阅刻意不补历史。
    let (status, mut body) = open_stream(&h, Some("0")).await;
    assert_eq!(status, StatusCode::OK);
    let events = read_events(&mut body, 2).await;
    let statuses: Vec<(String, String)> = events
        .iter()
        .filter(|e| e.name.as_deref() == Some("device.status"))
        .map(|e| {
            let v: Value = serde_json::from_str(&e.data).expect("json");
            (
                v["node_id"].as_str().expect("node").to_string(),
                v["status"].as_str().expect("status").to_string(),
            )
        })
        .collect();
    let states: Vec<&str> = statuses.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(
        states,
        vec!["online", "offline"],
        "先 online 后 offline，实际：{statuses:?}"
    );
    let ids: std::collections::HashSet<&str> = statuses.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(ids.len(), 1, "同一台设备的两个事件，node_id 必须一致");
}

/// 读满指定时长就返回（**不是一个恒真断言**：它断言「这段时间里一条都没来」）。
async fn read_events_for(body: &mut axum::body::Body, budget: Duration) -> Vec<SseEvent> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + budget;
    // `while let Ok(Some(Ok(frame)))`：流结束 / 读出错 / 等到超时都会让
    // 这条 `while` 自然退出——三种都是「剩下的读不到了」。
    while let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, body.frame()).await {
        let chunk = frame.into_data().expect("data frame");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
    parse_frames(&buf)
}

/// 读下一帧（带超时），是 `want` 才返回它，否则给 `None` 让调用方继续读。
///
/// **`Established` 没有 `next_frame`**：node 侧公开的读入口只有 `next_text()`
/// （返回 `Option<String>`，把「对端关闭」表达成 `None`），自己解析 JSON 才是
/// 端到端测试该做的事——绕道 `common::Ws` 的那个包装器就得另连一次 WS，
/// 那样测的就不是「设备那条真连接」了。
async fn next_matching(
    ws: &mut node_core::conn::Established,
    want: &str,
    deadline: tokio::time::Instant,
) -> Option<protocol::frame::Frame> {
    let text = tokio::time::timeout_at(deadline, ws.next_text())
        .await
        .expect("hub must reply before the deadline")
        .expect("hub must not close the connection")
        .expect("hub must send a text frame");
    let f: protocol::frame::Frame =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("unparsable json: {text} ({e})"));
    (f.msg_type == want).then_some(f)
}

/// 读到某一类帧为止（跳过别的），带超时；**超时返回 `None` 而不是 panic**。
///
/// 用于断言「Hub 不应该发这个」：那种断言必须能等到超时才成立，
/// 而 [`next_matching`] 在超时时会 panic——它适用于「必须来」的帧。
async fn next_frame_of_or_timeout(
    ws: &mut node_core::conn::Established,
    want: &str,
    budget: Duration,
) -> Option<protocol::frame::Frame> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let Ok(read) = tokio::time::timeout_at(deadline, ws.next_text()).await else {
            return None;
        };
        let Ok(Some(text)) = read else { return None };
        let Ok(f) = serde_json::from_str::<protocol::frame::Frame>(&text) else {
            continue;
        };
        if f.msg_type == want {
            return Some(f);
        }
    }
}

/// 读到某一类帧为止（跳过别的），带超时。
async fn drain_until(ws: &mut node_core::conn::Established, want: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if next_matching(ws, want, deadline).await.is_some() {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "3s 内没等到 {want}");
    }
}

/// 读到某一类帧为止并返回它（`drain_until` 的取值版）。
async fn next_frame_of(
    ws: &mut node_core::conn::Established,
    want: &str,
) -> protocol::frame::Frame {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(f) = next_matching(ws, want, deadline).await {
            return f;
        }
        assert!(tokio::time::Instant::now() < deadline, "3s 内没等到 {want}");
    }
}
