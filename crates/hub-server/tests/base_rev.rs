// 子任务 3「建会话时钉住起点」的端到端测试。
//
// 测的是 Hub 侧：`POST /api/sessions` 在建会话时向设备探一次 `rev`，把它记成
// `base_rev` / `base_rev_status`，而**探测失败绝不阻塞建会话**。
//
// 设备侧走真 WS（`common::Hub` + `node_core::conn`），页面侧走 `oneshot` 直打
// 路由，两边共享同一个 `AppState`——「通道关没关」「哪条通道收到了 rev」只有
// 同一份状态才作数。
//
// **桩设备自己回 `rev` 帧**（`spawn_rev_stub`）：真 `afcli` 会跑真 `git`，而这里
// 要控的是「回什么」（`ok` / `not_a_repo` / `no_head` / 不回 / 回 error），
// 那种场景在真设备上要么造不出来（`no_head` 得先清空一个仓库），要么得等满超时。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::{connect_enrolled, Hub};
use http_body_util::BodyExt;
use hub_server::{build_app, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};
use node_core::conn::Established;
use protocol::channel::{ChannelData, CloseChannel};
use protocol::frame::Frame;
use protocol::message::types;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
/// 一个真实浏览器一定会带的 Origin。
const BROWSER_ORIGIN: &str = "http://127.0.0.1:8080";

/// 本文件用例用的 agent：既写进心跳上报，也用作建会话的 `agent_id`。
///
/// **必须是对象形态且 `available: true`**：`parse_agent_report` 对纯字符串形态
/// 取 `available = false`，而可用性门禁读的就是这个值——写成 `"fake"` 会让建会话
/// 回 409 `agent_unavailable`，于是「起点探测」根本没机会发生。
const TEST_AGENT: &str = "probe-fake";

fn test_agent_report() -> Value {
    json!({"id": TEST_AGENT, "name": TEST_AGENT, "version": "0.0.1", "available": true})
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

struct Page {
    app: Router,
    session: String,
    csrf: String,
    node_id: String,
}

impl Page {
    async fn new(hub: &Hub) -> Self {
        let app = build_app(hub.state.clone());
        // 登录拿两条 cookie（session + csrf）。
        let mut req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/json")
            .header("origin", BROWSER_ORIGIN)
            .header("host", "127.0.0.1:8080")
            .body(Body::from(json!({"password": PASSWORD}).to_string()))
            .unwrap();
        // 登录端点不想走 CSRF 双重提交（那张豁免表里有它），但登录前没有 cookie。
        let resp = app
            .clone()
            .oneshot(std::mem::replace(
                &mut req,
                Request::builder().body(Body::empty()).unwrap(),
            ))
            .await
            .expect("login responds");
        assert_eq!(resp.status(), StatusCode::OK, "登录必须成功");

        let mut session = None;
        let mut csrf = None;
        for v in resp.headers().get_all("set-cookie") {
            let raw = v.to_str().unwrap();
            let Some((k, rest)) = raw.split_once('=') else {
                continue;
            };
            let val = rest.split(';').next().unwrap_or_default().to_string();
            if k == SESSION_COOKIE {
                session = Some(val);
            } else if k == CSRF_COOKIE {
                csrf = Some(val);
            }
        }
        Self {
            app,
            session: session.expect("session cookie"),
            csrf: csrf.expect("csrf cookie"),
            node_id: String::new(),
        }
    }

    fn browser_req(&self, method: &str, uri: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:8080")
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

    async fn call_with(&self, method: &str, uri: &str, body: Body) -> Reply {
        let mut req = self.browser_req(method, uri);
        *req.body_mut() = body;
        Reply::new(self.app.clone().oneshot(req).await.expect("app responds")).await
    }

    /// `POST /api/sessions`。
    async fn create_session(&self, path: &str) -> Reply {
        self.call_with(
            "POST",
            "/api/sessions",
            Body::from(
                json!({"node_id": self.node_id, "path": path, "agent_id": TEST_AGENT}).to_string(),
            ),
        )
        .await
    }
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

/// enroll 一台设备 + 上报 connector + 心跳，返回连接。
///
/// **连接必须被调用方一直拿着**：`Established` 一掉就是 WS 断开，建会话会撞上
/// 409 `device_offline`，于是测的就不是起点了。
async fn enrolled(hub: &Hub, connectors: &[&str]) -> (Established, String) {
    let (mut est, node_id, _kp) = connect_enrolled(hub).await;
    est.send(&Frame::push(
        types::HEARTBEAT,
        0,
        json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": connectors, "agents": [test_agent_report()]},
            "resources": null,
            "workspace_roots": [],
        }),
    ))
    .await
    .unwrap();
    common::wait_until(Duration::from_secs(2), || {
        hub.state.reports().get(&node_id).is_some()
    })
    .await;
    (est, node_id)
}

/// 桩设备：读设备侧 WS 的下行帧，遇到 `open_channel` 就回 `channel_ready`，
/// 遇到 `channel_data` 且 `payload.op == "rev"` 就按 `replies` 的第 n 次回一帧。
///
/// `replies` 按**探测次序**取值：第 1 次探测用 `replies[0]`，第 2 次用
/// `replies[1]`，越界则用最后一个。`None` = **什么都不回**（模拟设备卡住）。
///
/// 返回一个 `JoinHandle<StubLog>`：`rev_channels` 是每次收到 `rev` 时的
/// `channel_id`，`closed` 是收到的 `close_channel` 帧的 `channel_id` 列表。
/// 两者都要给用例断言——「问了」与「关掉了」是两件事。
struct StubLog {
    rev_channels: Vec<String>,
    closed: Vec<String>,
}

fn spawn_rev_stub(
    mut est: Established,
    replies: Vec<Option<Value>>,
) -> tokio::task::JoinHandle<StubLog> {
    tokio::spawn(async move {
        let mut log = StubLog {
            rev_channels: Vec::new(),
            closed: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let Some(text) = est.next_text().await.unwrap_or_default() else {
                break;
            };
            let Ok(f) = serde_json::from_str::<Frame>(&text) else {
                continue;
            };
            match f.msg_type.as_str() {
                types::OPEN_CHANNEL => {
                    let open = f
                        .payload::<protocol::channel::OpenChannel>()
                        .expect("open_channel payload");
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
                    let n = log.rev_channels.len();
                    log.rev_channels.push(data.channel_id.clone());
                    let reply = if replies.is_empty() {
                        None
                    } else {
                        replies.get(n).unwrap_or(replies.last().unwrap()).clone()
                    };
                    if let Some(body) = reply {
                        est.send(&Frame::push(
                            types::CHANNEL_DATA,
                            0,
                            json!({"channel_id": data.channel_id, "payload": body}),
                        ))
                        .await
                        .ok();
                    }
                }
                types::CLOSE_CHANNEL => {
                    let Ok(c) = f.payload::<CloseChannel>() else {
                        continue;
                    };
                    // **不回 `channel_closed`**：协议里没有这条设备侧消息，
                    // 设备收到 `close_channel` 就自己收尾。Hub 侧的绑定在
                    // `close_channel_inner` 里同步摘掉，不依赖设备回执。
                    log.closed.push(c.channel_id.clone());
                    // 每次探测都以一条 `close_channel` 收尾，所以收到它
                    // 就等于「这一轮探测做完了」。回齐 `replies` 那么多次
                    // 之后就退出——否则桩要等满 20 秒的 deadline，用例的
                    // 5 秒等待必然超时。
                    if log.closed.len() >= replies.len().max(1) {
                        break;
                    }
                }
                _ => {}
            }
        }
        log
    })
}

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

// ---------------------------------------------------------------------------
// 测试标准 1：仓库上建会话，起点等于真 HEAD
// ---------------------------------------------------------------------------

/// 标准 1：设备回一个完整 hash → 会话的 `base_rev` 就是它、状态是 `ok`。
///
/// 「40 位十六进制」这条不是凑数：`short` 也是十六进制、也「像个 hash」，
/// 只有长度与等值比较一起才能把两者分开。
#[tokio::test]
async fn a_new_session_carries_the_base_rev_of_its_workspace() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    const HEAD: &str = "3f2a1b0c4d5e6f708192a3b4c5d6e7f809a1b2c3";
    let stub = spawn_rev_stub(est, vec![Some(ok_rev(HEAD))]);

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let r = page.create_session("~/src/app").await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let body = r.json();

    assert_eq!(
        body["base_rev"],
        json!(HEAD),
        "base_rev 必须是设备回的那个完整 hash，body: {}",
        r.body
    );
    assert_eq!(body["base_rev_status"], json!("ok"));

    let head = body["base_rev"].as_str().expect("base_rev 必须是字符串");
    assert_eq!(head.len(), 40, "必须是完整 40 位，实际 {head:?}");
    assert!(
        head.chars().all(|c| c.is_ascii_hexdigit()),
        "必须是十六进制，实际 {head:?}"
    );

    // 探针真的问了：桩设备收到了恰好一次 `rev`。
    let log = tokio::time::timeout(Duration::from_secs(5), stub)
        .await
        .expect("桩设备必须在 5s 内结束")
        .expect("桩设备不得 panic");
    assert_eq!(log.rev_channels.len(), 1, "探针必须问一次 rev");
}

// ---------------------------------------------------------------------------
// 测试标准 2 与 3：非仓库、空仓库——两态可区分，且都建得成会话
// ---------------------------------------------------------------------------

/// 标准 2：非 git 工作区 → **201**、`base_rev == null`、`not_a_repo`。
#[tokio::test]
async fn a_non_git_workspace_still_creates_the_session() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    let stub = spawn_rev_stub(
        est,
        vec![Some(json!({
            "op": "rev", "not_a_repo": true,
            "head": null, "short": null, "branch": null, "detached": false,
        }))],
    );

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let r = page.create_session("~/not-a-repo").await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "非仓库不是建会话的门禁，必须 201；body: {}",
        r.body
    );
    assert_eq!(r.json()["base_rev"], Value::Null);
    assert_eq!(r.json()["base_rev_status"], json!("not_a_repo"));

    let _ = tokio::time::timeout(Duration::from_secs(5), stub).await;
}

/// 标准 3：空仓库 → **201**、`base_rev == null`、`no_head`。
///
/// **与标准 2 并排**：两帧的键集合相同，只有值不同；两态在 API 上必须可区分，
/// 否则页面只能说一句「没有起点」，而用户无从知道该 `git init` 还是 `git commit`。
#[tokio::test]
async fn a_repo_without_commits_still_creates_the_session() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    let stub = spawn_rev_stub(
        est,
        vec![Some(json!({
            "op": "rev", "not_a_repo": false,
            "head": null, "short": null, "branch": null, "detached": false,
        }))],
    );

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let r = page.create_session("~/empty-repo").await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["base_rev"], Value::Null);
    assert_eq!(
        r.json()["base_rev_status"],
        json!("no_head"),
        "空仓库是 no_head，不是 not_a_repo"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), stub).await;
}

// ---------------------------------------------------------------------------
// 测试标准 4：设备没上报 git —— 「起点不是门禁」的正面证据
// ---------------------------------------------------------------------------

/// 标准 4：设备没上报 `git` connector → **201**、`unavailable`。
///
/// **这条是「起点不是门禁」的正面证据。** 反向验证：把探测的失败分支改成
/// 冒泡成 409/500，本用例立刻红（会拿到冲突状态码而不是 201）——而那个改动
/// 在真实场景里的后果是「设备上没装 git 的用户连会话都开不了」。
#[tokio::test]
async fn a_device_that_never_reported_git_still_creates_the_session() {
    let hub = Hub::boot().await;
    // **只上报 terminal**：`git` 不在清单里，`open_device_channel` 的第四道门禁
    // 会回 409 `not_reported`，而探针必须把它折成 `unavailable`。
    //
    // 连接用 `est` 起名并**一直拿着**：掉线的话建会话先撞 409 `device_offline`，
    // 测到的就不是 `not_reported` 那条路了。
    let (est, node_id) = enrolled(&hub, &["terminal"]).await;
    let _keep = est;

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let r = page.create_session("~/src/app").await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "设备没上报 git 也必须建得成会话；body: {}",
        r.body
    );
    assert_eq!(r.json()["base_rev"], Value::Null);
    assert_eq!(
        r.json()["base_rev_status"],
        json!("unavailable"),
        "没上报 git 是「没拿到」，不是「不是仓库」"
    );
}

// ---------------------------------------------------------------------------
// 测试标准 6：设备不回帧 —— 探测有上限，请求不被挂住
// ---------------------------------------------------------------------------

/// 标准 6：桩设备**不回帧** → `create_session` 仍在 ~3 秒内返回 201 且
/// `unavailable`，**且总耗时 < 5 秒**。
///
/// 「有一次探测」不等于「探测有上限」：这条同时断言了状态与**耗时**，
/// 少任何一半都测不到超时这件事。
#[tokio::test]
async fn the_probe_times_out_without_hanging_the_request() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    // `None` = 收下 `rev` 但永不回帧。
    let stub = spawn_rev_stub(est, vec![None]);

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let started = Instant::now();
    let r = page.create_session("~/src/app").await;
    let took = started.elapsed();

    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["base_rev_status"], json!("unavailable"));
    assert!(
        took < Duration::from_secs(5),
        "探测必须有上限（探针 3s），实际用了 {took:?}"
    );
    // 也不能太快：立刻返回说明根本没等，那与「探测有上限」是两件事。
    assert!(
        took >= Duration::from_secs(2),
        "应在约 3 秒后放弃，实际只用了 {took:?}——探测没有真正在等"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), stub).await;
}

// ---------------------------------------------------------------------------
// 测试标准 8：先订阅、再发 —— 零延迟回帧也不能丢
// ---------------------------------------------------------------------------

/// 标准 8：桩设备在收到 `rev` 后**立刻**回帧（零延迟），探针仍拿到 hash。
///
/// # 这条**不**验证「先订阅、再发送」的顺序
///
/// 文档原本写的是「把 `subscribe_raw` 移到 `send_channel_data` 之后，本用例必红」。
/// **实测不成立**：`send_channel_data` 只是把帧塞进下行队列（`send_to_device`
/// 里没有任何 `.await`），不让出执行权，所以同一个任务里两种顺序在测试中
/// 观察不到差别——把订阅挪到发送之后，本用例照样全绿。
///
/// 留下它是因为它仍然值钱：它锁的是「零延迟回帧能被正确解读成 `ok`」，
/// 而不是「回帧来得很慢时也能收到」（那种情况由标准 6 覆盖）。
///
/// **顺序的正确性由 `stream.rs` 的单测
/// `subscribing_after_publishing_misses_the_event` 锁住**——那条真的验证了
/// 「broadcast 不重放」这个前提。
#[tokio::test]
async fn the_probe_reads_a_zero_delay_reply() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    const HEAD: &str = "abcdef0123456789abcdef0123456789abcdef01";
    let stub = spawn_rev_stub(est, vec![Some(ok_rev(HEAD))]);

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let started = Instant::now();
    let r = page.create_session("~/src/app").await;
    let took = started.elapsed();

    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(
        r.json()["base_rev"],
        json!(HEAD),
        "零延迟回帧必须被正确解读；body: {}",
        r.body
    );
    assert_eq!(r.json()["base_rev_status"], json!("ok"));
    assert!(
        took < Duration::from_secs(2),
        "零延迟回帧不该等超时，实际 {took:?}"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), stub).await;
}

// ---------------------------------------------------------------------------
// 测试标准 9：通道层的 error 帧是终止信号
// ---------------------------------------------------------------------------

/// 标准 9：设备回一帧 `{"op":"error","code":"timeout"}` → `unavailable`
/// 且请求在 **3 秒之前**返回。
///
/// 只等 `op == "rev"` 的实现会把这个已知失败晾在一边、干等到自己的 3 秒超时；
/// 这条断言的是「认了那帧 error 并立刻收手」。
#[tokio::test]
async fn the_channel_layer_timeout_is_not_mistaken_for_a_rev() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    let stub = spawn_rev_stub(
        est,
        vec![Some(
            json!({"op": "error", "code": "timeout", "reason": "slow"}),
        )],
    );

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let started = Instant::now();
    let r = page.create_session("~/src/app").await;
    let took = started.elapsed();

    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["base_rev_status"], json!("unavailable"));
    assert_eq!(r.json()["base_rev"], Value::Null);
    assert!(
        took < Duration::from_secs(3),
        "收到通道层 error 必须立刻收手，而不是等满自己的 3 秒；实际 {took:?}"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), stub).await;
}

// ---------------------------------------------------------------------------
// 测试标准 5：探测不泄漏通道
// ---------------------------------------------------------------------------

/// 标准 5：探测完成后探针开的那条通道被**真的关掉**了。
///
/// 反向验证：注释掉 `close_channel_inner` 那一步，本用例红——桩设备收不到
/// `close_channel` 帧，`log.closed` 为空。
///
/// **判据为什么取「设备收到的 `close_channel`」而不是「Hub 绑定表里还有没有」**：
/// `Bindings` 的读方法都是 `pub(crate)`，集成测试够不着；而「设备侧收到了关闭
/// 帧」是更强的证据——绑定表被摘掉但设备没收到帧，正是
/// `close_channel_inner` 的那个「`close_channel` 没发出去就把归属放回去」分支
/// 要防的事。同时断言关闭的 `channel_id` 正是探针用过的那条，而不是别的。
#[tokio::test]
async fn probing_does_not_leak_a_channel() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;
    const HEAD: &str = "1122334455667788990011223344556677889900";
    let stub = spawn_rev_stub(est, vec![Some(ok_rev(HEAD))]);

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let r = page.create_session("~/src/app").await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["base_rev_status"], json!("ok"), "前提：探测成功");

    // 等桩设备把 close_channel 收下来。**必须有超时**：等不到就是「泄漏了」，
    // 而不是让用例挂死。
    let log = tokio::time::timeout(Duration::from_secs(5), stub)
        .await
        .expect("桩设备必须在 5s 内看到 close_channel")
        .expect("桩设备不得 panic");

    assert_eq!(
        log.rev_channels.len(),
        1,
        "探针应当只问一次 rev，实际 {:?}",
        log.rev_channels
    );
    assert_eq!(
        log.closed, log.rev_channels,
        "探针开的那条通道必须被关掉：关掉的 {:?} 与用过的 {:?} 不一致",
        log.closed, log.rev_channels
    );
}

// ---------------------------------------------------------------------------
// 测试标准 7：起点是「建会话那一刻」取的，不是复制工作区的
// ---------------------------------------------------------------------------

/// 标准 7：同一路径上先后建两个会话，中间 HEAD 前进 → 两个 `base_rev` 不同，
/// 且第二个等于新的 HEAD。
///
/// **这条同时反向验证「起点是建会话那一刻取的，而不是复制工作区的」**：
/// 若实现改成「把工作区第一次记下的起点复用给后续会话」，第二个会话会拿到
/// FIRST，本用例红。
#[tokio::test]
async fn two_sessions_on_the_same_path_get_each_its_own_base() {
    let hub = Hub::boot().await;
    let (est, node_id) = enrolled(&hub, &["git"]).await;

    const FIRST: &str = "aaaa1111bbbb2222cccc3333dddd4444eeee5555";
    const SECOND: &str = "bbbb2222cccc3333dddd4444eeee5555ffff6666";
    // 同一条连接上按探测次序回不同的值：第 1 次 FIRST、第 2 次 SECOND。
    // 这等价于「设备侧有人提交了一次」，而不必真的起 git。
    let stub = spawn_rev_stub(est, vec![Some(ok_rev(FIRST)), Some(ok_rev(SECOND))]);

    let mut page = Page::new(&hub).await;
    page.node_id = node_id;

    let r1 = page.create_session("~/src/app").await;
    assert_eq!(r1.status, StatusCode::CREATED, "body: {}", r1.body);
    assert_eq!(r1.json()["base_rev"], json!(FIRST));

    let r2 = page.create_session("~/src/app").await;
    assert_eq!(r2.status, StatusCode::CREATED, "body: {}", r2.body);
    assert_eq!(
        r2.json()["base_rev"],
        json!(SECOND),
        "第二个会话必须取建它那一刻的 HEAD；body: {}",
        r2.body
    );
    assert_ne!(
        r2.json()["base_rev"],
        r1.json()["base_rev"],
        "两次探测必须各自取值，不是复制工作区的"
    );

    let log = tokio::time::timeout(Duration::from_secs(5), stub)
        .await
        .expect("桩设备必须在 5s 内结束")
        .expect("桩设备不得 panic");
    assert_eq!(
        log.rev_channels.len(),
        2,
        "两个会话应当各探测一次，实际 {:?}",
        log.rev_channels
    );
}
