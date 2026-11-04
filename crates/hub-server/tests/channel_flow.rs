// 子任务 8「通道与流控」的端到端测试。
//
// 测的是 Hub 侧的三件事：通道按 (node_id, channel_id) 分桶、每通道独立信用、
// 以及「关一条不碰其他」。协议层的状态机与信用账本本身在 protocol 的单测里。
//
// **开通道一律走 `POST /api/channels`**（生产入口），不让设备侧自己发
// `open_channel` 帧：那一帧只由 Hub 发（`protocol.md` §4 的消息表）。夹具若替
// 生产代码回答了「谁发起」，整套用例就会绕开真实入口——本轮已经踩过一次：
// 60 条通道用例全绿，而生产路径上账本从未被建立，终端输出 ~32 帧后永久冻结。

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
use common::{connect_enrolled, device_info, Hub};
use http_body_util::BodyExt;
use hub_server::{build_app, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};
use identity::KeyPair;
use node_core::conn::connect_once;
use protocol::channel::{ChannelCredit, ChannelData, CloseChannel, CREDIT_DELTA, CREDIT_START};
use protocol::frame::Frame;
use protocol::message::types;
use serde_json::{json, Value};
/// `Router::oneshot` 是 `tower::util::ServiceExt` 的方法，不导入这个 trait 就
/// 够不着——与 `tests/stream.rs` 的同款夹具保持一致。
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
/// 一个真实浏览器一定会带的 Origin。
const BROWSER_ORIGIN: &str = "http://127.0.0.1:8080";
/// 本文件用例用的 agent：既写进心跳上报（`capabilities.agents`），也用作建会话
/// 的 `agent_id`，两处必须是同一个名字，否则 `POST /api/sessions` 会以
/// 「设备上没有这个 agent」400 / 409。
///
/// **必须是对象形态且 `available: true`**：`parse_agent_report` 对纯字符串
/// 形态取 `available = false`（「只说名字不构成可用的证据」），而可用性门禁
/// 读的就是这个值——写成 `"terminal-fake"` 会让建会话回 409 `agent_unavailable`。
const TEST_AGENT: &str = "terminal-fake";
/// 见 [`TEST_AGENT`]：上报时所带的对象。
fn test_agent_report() -> serde_json::Value {
    json!({"id": TEST_AGENT, "name": TEST_AGENT, "version": "0.0.1", "available": true})
}

/// 页面侧请求夹具：一份真跑起来的 Hub + 同状态路由 + 已登录的两条 cookie。
///
/// 设备侧走真 WS（`common::Hub`），页面侧走 `oneshot` 直打路由，两者共享同一个
/// `AppState`——「账本建了没建」只有同一份状态才作数。
struct Page {
    #[allow(dead_code)]
    hub: Hub,
    app: Router,
    session: String,
    csrf: String,
}

impl Page {
    async fn new(hub: Hub) -> Self {
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

    fn req(&self, method: &str, uri: &str) -> Request<Body> {
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

    /// `POST /api/channels`，返回 Hub 生成的 `channel_id`。
    async fn open_channel(&self, session_id: &str, connector: &str) -> String {
        let mut req = self.req("POST", "/api/channels");
        *req.body_mut() = Body::from(
            json!({"session_id": session_id, "connector": connector, "params": {}}).to_string(),
        );
        let resp = self.app.clone().oneshot(req).await.expect("app responds");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "开通道必须 201：{}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice::<Value>(&bytes).unwrap()["channel_id"]
            .as_str()
            .expect("channel_id")
            .to_string()
    }

    /// 建一个真会话（返回 id）。
    ///
    /// **`agent_id` 是必填的**（M2：缺了是 400 `agent_required`）。一个
    /// `agent_id` 为 NULL 的会话永远开不了对话，所以 Hub 直接拒绝建立。
    /// 本文件的用例测的是**通道记账**，不测对话，但会话仍得是个真会话——
    /// 所以这里带上 `report_connectors` 上报的那个 agent。
    async fn make_session(&self, node_id: &str) -> String {
        let mut req = self.req("POST", "/api/sessions");
        *req.body_mut() = Body::from(
            json!({"node_id": node_id, "path": "/tmp", "agent_id": TEST_AGENT}).to_string(),
        );
        let resp = self.app.clone().oneshot(req).await.expect("app responds");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "建会话必须 201：{}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
            .as_str()
            .expect("session id")
            .to_string()
    }
}

/// 让设备发一次心跳，把 connector 能力上报出去，并等 Hub 侧真的记下。
///
/// **`POST /api/channels` 会按上报的 connector 硬过滤**（子任务 11）：报了
/// `connectors` 而里面没有目标名字 → 409；「从没上报过」则是「不知道」→ 放行。
/// 夹具显式选「上报过」这条，因为这里要开 `terminal` / `files` / `git`
/// 三种 connector——与 `tests/stream.rs` 的 `enrolled_device` 同款。
///
/// 判据用「心跳已落库」而不是固定 sleep：`on_heartbeat` 先写内存再落库，
/// 所以查得到非空 `last_seen_at` 时内存那份也写完了。
///
/// **`last_seen_at` 只由心跳写**（`DeviceRepo::touch_online`），enroll 不写它，
/// 所以「非空」确实证明这一帧已被处理，不会因为上一次心跳而提前成立。
async fn report_connectors(hub: &Hub, est: &mut node_core::conn::Established) {
    report_capabilities(hub, est, &["terminal", "files", "git"]).await
}

/// 上报 `terminal` + `files`，**不报 `git`**。
///
/// 用在「测通道记账 / 帧形状，但会从设备侧读帧」的用例上。
///
/// # 为什么需要它（`v0.6.0`）
///
/// `v0.6.0` 起，**建会话时会向设备探一次起点**（`routes::base_rev`）：若设备上报了
/// `git`，`POST /api/sessions` 会在这条设备连接上开一条 `git` 通道、下发
/// `{"op":"rev"}`、再关掉它。设备侧本来就在读同一条 WS，于是夹具会在自己的
/// `open_channel` 之前先读到**探针的三帧**（`open_channel` / `channel_data` /
/// `close_channel`），用例里那句「设备收到的第一帧是我的」随之错位。
///
/// 本文件这些用例测的是**通道记账与帧形状**，不是起点，也不开 `git` 通道——
/// 所以它们干脆不上报 `git`：探针的第四道门禁（`not_reported`）会把它直接折成
/// `unavailable`，**一个帧都不发给设备**，帧流因此与 `v0.5.0` 完全一致。
///
/// **这不是把断言改松**：受影响用例的每一条断言都原样保留，只是让夹具不再
/// 往它们的帧流里掺入与测试目标无关的帧。需要 `git` 的用例（`three_channels_...`
/// 的循环、`the_git_connector_opens_a_channel_end_to_end`）仍然走
/// [`report_connectors`]。
async fn report_connectors_without_git(hub: &Hub, est: &mut node_core::conn::Established) {
    report_capabilities(hub, est, &["terminal", "files"]).await
}

/// 上报**指定的** connector 集合，并等 Hub 记下。
///
/// 与 [`report_connectors`] 共用一个实现：两处各抄一份载荷时，改字段
/// （比如加一个新的必填项）只改一处，另一处会静默地少上报。
async fn report_capabilities(
    hub: &Hub,
    est: &mut node_core::conn::Established,
    connectors: &[&str],
) {
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
    .expect("heartbeat must send");
    common::wait_until(Duration::from_secs(2), || {
        common::device_row(hub, &est.auth_ok.node_id, "last_seen_at").is_some_and(|v| !v.is_empty())
    })
    .await;
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

impl Hub {
    fn credit(&self, node_id: &str, channel_id: &str) -> Option<u32> {
        self.state.channels().read().credit(node_id, channel_id)
    }
}

/// 等 Hub 侧账本落到期望值（超时即失败）。
///
/// 补信用与回填是同一把写锁里的一对动作，但本测试是从 WebSocket 外面观测的，
/// 只能轮询等它落地。
async fn wait_for_credit(hub: &Hub, node_id: &str, channel_id: &str, want: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let c = hub.credit(node_id, channel_id);
        if c == Some(want) {
            return want;
        }
        assert!(
            Instant::now() < deadline,
            "Hub 侧账本 3s 内没回填到 {want}（最后一次读到 {c:?}）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 等设备侧真的收到 Hub 发来的 `open_channel`，返回帧里的 `channel_id`。
///
/// 夹具开通道走 `POST /api/channels`，而设备侧何时收到是异步的——账本由 Hub
/// 在建帧时一并写好，但要断言「设备收到了哪条 id」仍得从 WS 里读。
async fn open_frame_on(est: &mut node_core::conn::Established) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = est.next_text().await.unwrap().expect("hub must reply");
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::OPEN_CHANNEL {
            return f
                .payload::<protocol::channel::OpenChannel>()
                .unwrap()
                .channel_id;
        }
        assert!(Instant::now() < deadline, "5s 内没收到 open_channel");
    }
}

/// [`open_frame_on`] 的完整载荷版本：连 `credit_start` 一起断言时用。
///
/// 拆成两个函数而不是让 `open_frame_on` 返回结构体：多数用例只关心
/// 「设备收到的是哪条 id」，为它们引入一个要解载荷的返回值只会让每个
/// 调用点都多写一行 `.channel_id`。
async fn open_spec_on(est: &mut node_core::conn::Established) -> protocol::channel::OpenChannel {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = est.next_text().await.unwrap().expect("hub must reply");
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::OPEN_CHANNEL {
            return f.payload::<protocol::channel::OpenChannel>().unwrap();
        }
        assert!(Instant::now() < deadline, "5s 内没收到 open_channel");
    }
}

fn channel_data(id: &str, seq: u64) -> Frame {
    Frame::push(
        types::CHANNEL_DATA,
        seq,
        serde_json::to_value(ChannelData {
            channel_id: id.to_string(),
            payload: serde_json::json!({"chunk": "x"}),
        })
        .unwrap(),
    )
}

fn close_channel(id: &str) -> Frame {
    Frame::push(
        types::CLOSE_CHANNEL,
        1,
        serde_json::to_value(CloseChannel {
            channel_id: id.to_string(),
            reason: "done".to_string(),
        })
        .unwrap(),
    )
}

/// 标准①：同一条 WS 上开 3 条通道，3 条都各自记账（互不干扰）。
#[tokio::test]
async fn three_channels_on_one_socket_are_accounted_independently() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;

    let mut ids = Vec::new();
    for conn in ["terminal", "files", "git"] {
        let id = page.open_channel(&session, conn).await;
        ids.push(id);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    for id in &ids {
        assert_eq!(page.hub.credit(&node_id, id), Some(32));
    }

    // 只消费第一条的信用，其他两条不动
    est.send(&channel_data(&ids[0], 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(page.hub.credit(&node_id, &ids[0]), Some(31));
    assert_eq!(
        page.hub.credit(&node_id, &ids[1]),
        Some(32),
        "第二条不得被牵连"
    );
    assert_eq!(
        page.hub.credit(&node_id, &ids[2]),
        Some(32),
        "第三条不得被牵连"
    );
}

/// 标准②：信用耗尽后 Hub 补 `channel_credit`。
///
/// **数值口径**：开通道走 `POST /api/channels`，而入口固定填 `CREDIT_START`
/// （32），夹具再也不能自定义小额 `credit_start`——所以这里连发 32 帧
/// `channel_data` 才把账本吃到归零。这正是真实入口的形状：Hub 说的初始
/// 额度是协议常量，设备侧没有任何议价空间。
#[tokio::test]
async fn exhausting_credit_makes_the_hub_issue_more() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START),
        "入口固定按协议常量建账"
    );

    // 发 32 帧把信用吃光
    for seq in 1..=protocol::channel::CREDIT_START as u64 {
        est.send(&channel_data(&ch, seq + 1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 归零的那一帧同时触发「补发」与「回填」：Hub 侧账本回到 CREDIT_DELTA。
    // 补发而不回填的话余额会停在 0，于是**每帧**都补一次信用。
    let balance = wait_for_credit(&page.hub, &node_id, &ch, CREDIT_DELTA).await;
    assert_eq!(balance, CREDIT_DELTA, "32 帧吃光后应被回填");

    // Hub 应当回一帧 channel_credit（余额归零时补）
    let deadline = Instant::now() + Duration::from_secs(3);
    let credit_frame = loop {
        let text = est.next_text().await.unwrap().expect("hub must reply");
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::CHANNEL_CREDIT {
            break f;
        }
        assert!(Instant::now() < deadline, "3s 内没收到 channel_credit");
    };
    let c: ChannelCredit = credit_frame.payload().unwrap();
    assert_eq!(c.channel_id, ch);
    assert!(c.delta > 0, "补的信用必须是正数: {}", c.delta);
}

/// 补信用必须**回填 Hub 侧账本**，否则每帧都在补、node 侧余额无限滚。
///
/// 症状（未修复时）：Hub 侧 `credit` 恒为 0——余额归零后不再回填，下一帧
/// 又立刻归零，于是**每帧**都发一帧 `channel_credit`，node 侧的可用余额
/// 越滚越大（约束 4 的「每通道独立流控」实际失效）。
///
/// 断言三件事：补完之后 Hub 侧余额**恢复为非零**且等于补发的那一帧；
/// 补发出去的增量与回填的量一致；下一帧只是减 1，而不是又归零。
///
/// **数值口径**：入口固定给 `CREDIT_START`（32），所以这里连发 32 帧才归零
/// ——原用例靠 `credit_start = 1` 一帧打光，那个「自定义小额起点」的旋钮
/// 已经跟着设备侧自发帧一起没了。
#[tokio::test]
async fn granting_credit_also_refills_the_hub_side_ledger() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START)
    );

    // 吃掉最后一格 → Hub 回一帧 channel_credit，并把 Hub 侧账本回填
    for seq in 1..=protocol::channel::CREDIT_START as u64 {
        est.send(&channel_data(&ch, seq + 1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let after = wait_for_credit(&page.hub, &node_id, &ch, CREDIT_DELTA).await;
    assert_eq!(after, CREDIT_DELTA, "补信用必须同时回填 Hub 侧账本");

    // 补发出去的那一帧，增量必须与回填的量一致
    let deadline = Instant::now() + Duration::from_secs(3);
    let credit_frame = loop {
        let text = est.next_text().await.unwrap().expect("hub must reply");
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::CHANNEL_CREDIT {
            break f;
        }
        assert!(Instant::now() < deadline, "3s 内没收到 channel_credit");
    };
    let c: ChannelCredit = credit_frame.payload().unwrap();
    assert_eq!(c.channel_id, ch);
    assert_eq!(c.delta, after, "补发出去的增量必须与回填的量一致");

    // 再吃一格：余额减 1，**不是**立刻又归零（那正是「每帧都补」的形状）
    est.send(&channel_data(&ch, 100)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(after - 1),
        "回填之后要按新余额继续记账，而不是每帧都归零"
    );
}

/// 设备掉线后那条**信用**账本必须清干净。
///
/// 留着它，重连后按 channel_id 记账会累积出一个恒 > 0 的余额，等于没有
/// 流控（§6 末条）。**这是两份按 node 记账的账本里的其中一份**（另一份是
/// 页面归属 `Bindings`，由 `stream.rs` 的
/// `a_channel_of_a_disconnected_device_is_forgotten` 验）：两份分处两个
/// crate、数据形状也不同（`Map<node, Registry>` vs `Map<channel, Binding>`），
/// 所以是两个各自的调用，改动一处不会让另一处的测试变红——这正是把它们
/// 收进 `ws.rs` 同一处「账本结算」清单的理由。
#[tokio::test]
async fn disconnecting_drops_the_credit_book() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START),
        "先记上信用"
    );

    drop(est);
    common::wait_until(Duration::from_secs(3), || {
        page.hub.state.connections().get(&node_id).is_none()
    })
    .await;

    // 信用账本：留着它，重连后按 channel_id 记账会累积旧余额，
    // 等于没有流控（§6 末条）。
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        None,
        "设备断开后这份按 node 的信用账本必须清掉"
    );
}

/// 标准③：关掉一条通道，另一条照常可用。
///
/// **数值口径**：两条都从 `POST /api/channels` 开，所以起点都是
/// `CREDIT_START`（32）而不是原来的 `Some(4)`——断言改成「第二条还剩 31」，
/// 即关掉第一条之后第二条只是被消费了一格，没有被牵连也没有被重置。
#[tokio::test]
async fn closing_one_channel_leaves_the_others_working() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;

    let ch_a = page.open_channel(&session, "terminal").await;
    let ch_b = page.open_channel(&session, "files").await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    est.send(&close_channel(&ch_a)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // ch_b 仍能用：继续消费它的信用
    est.send(&channel_data(&ch_b, 3)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch_b),
        Some(protocol::channel::CREDIT_START - 1),
        "关掉一条之后另一条必须还能发"
    );
}

/// 标准④：重连后 credit 重置为 credit_start，不是旧连接的剩余额度。
///
/// 这条是 §6 末条那个「无限累积」陷阱的守卫。
///
/// **所有权**：`Page::new` 会拿走 `hub`，而重连要 `page.hub.ws_url()`
/// ——`hub` 字段是 `pub`，从 `page.hub` 上取即可，不必为了拿 URL 再造一台 Hub。
/// 两条通道用的是同一个 `channel_id`（Hub 自己生成的那一个）：重开同一 id
/// 正是「重置信用」这条不变式的触发条件（`ChannelRegistry::open` 的注释）。
#[tokio::test]
async fn reconnecting_resets_the_credit_to_credit_start() {
    let hub = Hub::boot().await;
    let (mut est, node_id, kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    // 旧连接吃一格，让余额离开起点
    est.send(&channel_data(&ch, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START - 1)
    );

    // 断开：整节点记录丢弃
    drop(est);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if page.hub.state.connections().get(&node_id).is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "连接始终没被摘掉");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(page.hub.credit(&node_id, &ch), None, "断线即清账");

    // 新连接：重新走一遍页面入口（`POST /api/channels`），channel_id 由 Hub
    // 重新生成——判据是「新账本从 CREDIT_START 起算」，而不是旧连接的 31。
    let mut est2 = connect_once(&page.hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    report_connectors(&page.hub, &mut est2).await;
    let session2 = page.make_session(&node_id).await;
    let ch2 = page.open_channel(&session2, "terminal").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch2),
        Some(protocol::channel::CREDIT_START),
        "必须是 credit_start，不是旧连接的剩余额度"
    );
}

/// 标准⑦（口径已改）：**设备侧自发 `open_channel` 现在被安全忽略，不断开连接**。
///
/// ## 为什么换口径
///
/// 原用例的设备侧发一个空 `channel_id` 的 `open_channel`，等一帧
/// `channel_error{code:"unsupported"}`。那条路径随着 `channels.rs::on_open`
/// 一起被删了——**删得对**：`open_channel` 只由 Hub 发（`protocol.md` §4 的
/// 消息表），给它留一条臂等于把「账本的建表入口」放在一段生产不可达的代码里。
/// 原意「开通道失败要显式回错」在 Hub 侧换了个位置实现：`POST /api/channels`
/// 的每一步失败都有显式状态码（404 / 409 / 503 / 500），由 `tests/stream.rs`
/// 逐个钉住；而**设备侧**再也没有「开通道失败」这个状态了。
///
/// ## 现在这条断言什么
///
/// 设备侧发 `open_channel` 属于协议违例。按约束 14 前向兼容，它落进
/// `on_channel_frame` 的 `_ => Reply::Nothing`（未知类型忽略），于是：
/// 1. **不回** `channel_error`（没有东西可回：Hub 侧没有这条通道，也没有
///    一个「被拒绝的开通道」可以让它认领）；
/// 2. **不断开**连接——断开会让一个新版本的设备帧直接搞死老 Hub；
/// 3. Hub 侧账本里**什么都不会多出来**。
///
/// 三条一起断言：只验「没断」而不验「没建账」的话，一个偷偷记了账的实现
/// 也能通过，而那个残留记录正是重连后「静默扣一个不存在的账」的来源。
#[tokio::test]
async fn a_bad_open_channel_gets_an_explicit_error() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors_without_git(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;
    // 等设备侧收到那一帧，免得它和下面那帧违例帧在同一时刻被读
    assert_eq!(open_frame_on(&mut est).await, ch);

    // 设备侧自发一帧 open_channel（空 channel_id，协议层判 EmptyChannelId）
    est.send(&Frame::push(
        types::OPEN_CHANNEL,
        1,
        serde_json::json!({"channel_id": "", "connector": "terminal"}),
    ))
    .await
    .unwrap();
    // 再走一次正常数据面：如果上面那帧被当成开通道处理，这里会是
    // 「未知通道」而不是照常记账
    est.send(&channel_data(&ch, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 1. 没有 channel_error：违例帧被忽略，不是被回绝
    // 2. 连接还活着
    // 3. 账本没被那帧碰过（只有我们这一帧 channel_data 扣的 1 格）
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START - 1),
        "设备侧自发的 open_channel 不得在 Hub 侧留下任何账目"
    );
    assert!(
        page.hub.state.connections().get(&node_id).is_some(),
        "协议违例的通道帧不得把连接弄死（约束 14）"
    );
    // 再确认一次「没有 channel_error 排在路上」：发一帧心跳，Hub 会照常处理；
    // 若前面真回过一帧 channel_error，它会在这一轮里被读到。
    common::send_heartbeat(&mut est, serde_json::json!({"node_version": "0.1.0"})).await;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let got = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            est.next_text(),
        )
        .await;
        match got {
            Ok(Ok(Some(text))) => {
                let f: Frame = serde_json::from_str(&text).unwrap();
                assert_ne!(
                    f.msg_type,
                    types::CHANNEL_ERROR,
                    "设备侧自发 open_channel 不该换来一帧 channel_error：它被忽略，不是被回绝"
                );
            }
            // 超时 = 没有更多帧了：违例帧什么都没换来。**这是唯一的正常出口。**
            Err(_) => break,
            // 读错误 / 连接已断：这是失败，不是「没有更多帧」。早先与超时合用
            // 一个 `_ => break`，于是「连接断了」会被当成「等到了底」——
            // 上面那条 `assert_ne!` 一帧都没执行过，绿灯是空的。
            Ok(Err(e)) => panic!("读连接时出错（不是「没有更多帧」）：{e:?}"),
            Ok(Ok(None)) => break,
        }
        assert!(Instant::now() < deadline, "轮询超时");
    }
}

/// 未知通道上发数据 → 显式回错，而不是把连接弄死。
///
/// **这条不改成走页面入口**：它测的就是「一条 Hub 从未开过的通道」，
/// 换成真开的 id 就等于把前提删掉了。只有结尾「连接还活着」那段要换成
/// 用 `page` 真开一条通道——那是唯一需要账本存在的地方。
#[tokio::test]
async fn data_on_an_unknown_channel_is_an_error_not_a_disconnect() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors_without_git(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;
    assert_eq!(open_frame_on(&mut est).await, ch);

    // 一条 Hub 从未开过的通道 id（Hub 生成的 id 是 22 字符 base64url，
    // 这个字面量对不上它）
    est.send(&channel_data("ch-nope", 1)).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let err = loop {
        let text = est.next_text().await.unwrap();
        let Some(text) = text else {
            panic!("hub must not close the connection over an unknown channel");
        };
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::CHANNEL_ERROR {
            break f;
        }
        assert!(Instant::now() < deadline, "3s 内没收到 channel_error");
    };
    assert_eq!(err.c["channel_id"], "ch-nope");

    // 连接还活着：刚开的那条通道照常能消费信用
    est.send(&channel_data(&ch, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(page.hub.state.connections().get(&node_id).is_some());
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START - 1),
        "未知通道报错之后，真通道必须还能用"
    );
}

/// 约束 14：Hub 不该收的通道帧（`channel_ready` 是 Hub 发出的方向）
/// 按未知类型忽略，**不断开连接**。
#[tokio::test]
async fn an_unexpected_channel_frame_is_ignored_without_closing() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;

    est.send(&Frame::push(
        types::CHANNEL_READY,
        1,
        serde_json::json!({"channel_id": "ch-1", "capabilities": {}}),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        page.hub.state.connections().get(&node_id).is_some(),
        "反向的通道帧不得把连接弄死"
    );
    // 之后照常能开通道（走生产入口）
    let ch = page.open_channel(&session, "terminal").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        page.hub.credit(&node_id, &ch),
        Some(protocol::channel::CREDIT_START)
    );
}

/// 入口下发的 `open_channel` 帧里 `credit_start == CREDIT_START`。
///
/// ## 为什么换口径
///
/// 原意是「帧里不带 `credit_start` 时回落协议常量」——那个旋钮在设备侧自发
/// 帧上，现在随 `on_open` 一起没了。入口固定用
/// `OpenChannel::with_params(...)` 建帧，它内部走 `OpenChannel::new`，
/// 于是 `credit_start` 恒定取 `CREDIT_START`（32）。
///
/// 换成断言「**帧里带的那个值**」而不是「账本里的余额」：账本起步也是
/// `CREDIT_START`，但两者是两个数——一个是**对外契约**（设备按它安排自己的
/// 发送节奏），一个是 Hub 内部记账。设备侧按 0 安排节奏的话，
/// 「Hub 账本有 32 格而设备一帧都不敢发」照样是死锁，而那正是原注释里
/// 「双方互等」的坑。协议层的缺省语义仍由 `protocol` 的单测
/// （`open_channel_new_matches_the_serde_default` 等）钉住。
#[tokio::test]
async fn the_open_channel_frame_carries_the_protocol_credit_start() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors_without_git(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let ch = page.open_channel(&session, "terminal").await;

    // 从设备侧读回 Hub 真正发出去的那一帧（不是 Hub 自己记的账）
    let spec = open_spec_on(&mut est).await;
    assert_eq!(spec.channel_id, ch, "设备收到的 id 必须与响应里的一致");
    assert_eq!(
        spec.credit_start,
        protocol::channel::CREDIT_START,
        "入口下发的 credit_start 必须是协议常量，不能是 0"
    );
    assert_eq!(spec.connector, "terminal");
}

/// 通道 id 按 node 分桶：两台设备的同一条 `channel_id` 互不干扰（约束 4）。
///
/// **两台设备、一个 `Page`**：`Page::new` 拿走 `hub`，所以第二条设备也从
/// `page.hub.ws_url()` 连。两台各建一个会话、各开一条通道——`channel_id`
/// 由 Hub 各自生成，所以这里断言的是「Hub 侧的账本按 node 分开」
/// （`Map<node, Registry>`），不是「两个 id 恰好不同」。
#[tokio::test]
async fn the_same_channel_id_on_two_nodes_stays_separate() {
    let hub = Hub::boot().await;
    let (mut est_a, id_a, _kp_a) = connect_enrolled(&hub).await;
    let (mut est_b, id_b, _kp_b) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est_a).await;
    report_connectors(&hub, &mut est_b).await;
    let page = Page::new(hub).await;
    let session_a = page.make_session(&id_a).await;
    let session_b = page.make_session(&id_b).await;

    // 两台都开 `terminal`：Hub 侧是两份各自独立的账本
    let ch_a = page.open_channel(&session_a, "terminal").await;
    let ch_b = page.open_channel(&session_b, "terminal").await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        page.hub.credit(&id_a, &ch_a),
        Some(protocol::channel::CREDIT_START)
    );
    assert_eq!(
        page.hub.credit(&id_b, &ch_b),
        Some(protocol::channel::CREDIT_START),
        "不得串到另一台设备"
    );

    // 只消费 A 的信用：B 的账本一动不动
    est_a.send(&channel_data(&ch_a, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        page.hub.credit(&id_a, &ch_a),
        Some(protocol::channel::CREDIT_START - 1)
    );
    assert_eq!(
        page.hub.credit(&id_b, &ch_b),
        Some(protocol::channel::CREDIT_START),
        "一台设备的消费不得动另一本的账"
    );
}

/// 标准⑤：读侧硬上限——持续灌数据时 Hub **不崩、不误杀**，而是慢读顶住。
///
/// 规格要的是「超限 → 慢读；持续 30s 才断开」（§6.1），所以这里不断断言
/// 「立刻断开」——立刻断是错的：`cat` 一个大文件就是一次合法的 PTY 洪峰，
/// 误杀它等于「输出多的命令一律失败」。
/// 断言的是 Hub 活下来、连接还在，且**没有把内存吃光**（进程没被 OOM kill）。
///
/// ## 「之后还能用」的判据为什么换成走入口开通道
///
/// 原用例在洪峰之后让设备自己发一帧 `open_channel{credit_start:4}`，再等
/// Hub 侧出现一条余额 4 的账。那条路随 `channels.rs::on_open` 一起没了：
/// 现在开通道只有 `POST /api/channels` 一个入口，而它固定给
/// `CREDIT_START`（32）——**没有「自定义小额起点」这个旋钮**，于是也没有
/// 「等一个 `Some(4)`」可等。
///
/// 换成走生产入口**在洪峰之前**开一条、洪峰之后再断言它照常记账：
/// 一次盖住三件事——① 连接还在（`send_to_device` 凭 `conn_id` 找得到这条
/// socket，设备真收到了那一帧）；② 出站队列与写协程没被那 12 MiB 拖死；
/// ③ 洪峰积压消化完之后数据面照常记账。
///
/// **顺序是刻意的**：通道必须在洪峰前开并 ack 掉。放到洪峰之后的话，
/// `channel_ready` 会和 12 MiB 积压排在同一个读循环里，5s 的
/// `READY_TIMEOUT` 先到，`expire_unacked` 把通道置成 `Closed`，判据就废了。
#[tokio::test]
async fn a_flooding_node_is_slow_read_not_killed_on_the_first_burst() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors_without_git(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;

    // 洪峰的目标必须是一条**没人在记账的 id**：读侧限流（`admit_read`）在
    // 分派之前，打在没有账本的 id 上照样触发慢读；而如果这里就用上一条
    // 真通道，下面那段「等它扣一格」会在洪峰积压里抢时序。
    let ch = page.open_channel(&session, "terminal").await;
    assert_eq!(
        open_frame_on(&mut est).await,
        ch,
        "先开一条真通道，洪峰之后才有东西可断言"
    );
    // 设备侧回 `channel_ready`：真设备就是这么做的，而这里**必须**这么做——
    // 没人 ack 的话 `expire_unacked` 会在 5s 后把这条通道置成 `Closed`，
    // 而洪峰的积压要 ~12s 才放行得完，等那时候再发数据只会换来一帧
    // `channel_error{AlreadyClosed}`，判据就废了。
    est.send(&Frame::push(
        types::CHANNEL_READY,
        1,
        serde_json::json!({"channel_id": ch, "capabilities": {}}),
    ))
    .await
    .unwrap();

    // 灌 12 MiB（每帧 512 KiB，分 24 帧），远超令牌桶 4 MiB 的容量
    let chunk = "x".repeat(512 * 1024);
    for seq in 1..=24 {
        let f = Frame::push(
            types::CHANNEL_DATA,
            seq,
            serde_json::json!({"channel_id": "ch-0", "payload": {"chunk": chunk}}),
        );
        if est.send(&f).await.is_err() {
            break;
        }
        // 不等：越快越能压出「一次性突发」
    }

    // Hub 必须还活着，且连接**没有被误杀**
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        page.hub.state.connections().get(&node_id).is_some(),
        "一次突发不该让 Hub 断开连接（那是合法的 PTY 洪峰形状）"
    );

    // 洪峰过去之后连接照常可用。**不能只等一个固定时长**就断言「读循环
    // 已经恢复」——慢读是有意的，积压的 12 MiB 要按 1 MiB/s 慢慢放行，
    // 全放完要 ~12s。判据是「洪峰之后这条通道还能一条条记账」：
    // 先记下洪峰前的余额，再等它掉一格。
    // 判据是「再记三帧就恰好扣三格」，而不是「余额变成 before-1」：
    // `before - 1` 的期望值是从被测系统自己的读数算出来的，只要余额动了
    // 一格就算过——那与用例名里的「slow read」（令牌桶速率）毫无关系，
    // 把速率调成瞬间放行它照样绿。差值「三帧 → 降三格」才是记账这件事
    // 本身，且不依赖余额的绝对值（洪峰已经把它扣掉多少都无所谓）。
    let before = page.hub.credit(&node_id, &ch).expect("通道必须在账本里");
    for seq in 1..=3 {
        est.send(&channel_data(&ch, seq)).await.unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if page.hub.credit(&node_id, &ch) == Some(before.saturating_sub(3)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "洪峰消化完之后通道必须还能逐帧记账：发了 3 帧，余额该从 {before} 降到 {}（现在是 {:?}）",
            before.saturating_sub(3),
            page.hub.credit(&node_id, &ch)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 标准⑤ 的另一半：单帧超 1 MiB 是 `frame too large`，不是 `flood`。
///
/// 两者分开的意义在运维侧：前者是对端发错了帧（可能是协议 bug），
/// 后者是对端在攻击或跑飞了。混在一起没法判断要不要拉黑。
#[tokio::test]
async fn an_oversized_single_frame_is_frame_too_large() {
    let hub = Hub::boot().await;
    let (mut est, _node_id, _kp) = connect_enrolled(&hub).await;

    // WS 侧有 WS_MAX_MESSAGE_BYTES 的限制，所以这一帧可能连发都发不出去；
    // 发不出去时断言的是「Hub 还活着、没被这一帧弄死」。
    //
    // `channel_id` 是个随手写的字面量：这一帧在读侧限流（`admit_read`）就被
    // 拦下，根本走不到「按通道认领」那一步，所以不需要一条真开过的通道。
    // 这条用例与开通道入口无关，本轮只把设备侧自发 `open_channel` 的用例
    // 改成走 `POST /api/channels`，它原样保留。
    let big = "x".repeat(1024 * 1024 + 64);
    let f = Frame::push(
        types::CHANNEL_DATA,
        1,
        serde_json::json!({"channel_id": "ch-0", "payload": {"chunk": big}}),
    );
    match est.send(&f).await {
        Ok(()) => {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match est.next_text().await {
                    Ok(Some(text)) => {
                        let r: Frame = serde_json::from_str(&text).unwrap();
                        if r.msg_type == types::ERR {
                            assert_eq!(
                                r.c["code"], "frame too large",
                                "超限必须是 frame too large，不是 flood（ flood 是速率超限）"
                            );
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
                assert!(Instant::now() < deadline);
            }
        }
        // 发送就被拒：WS 层自己拦的。这时要的是「Hub 没死」——**重新连一次**
        // 就是判据。早先这里写成 `if send.is_ok() { …断言… }`，发送失败时
        // **整条用例什么都不验就绿**，而用例名承诺的「是 frame too large」
        // 一次都没被执行到。
        Err(e) => {
            let kp = KeyPair::generate();
            let again = connect_once(&hub.ws_url(), &kp, None, None, device_info()).await;
            assert!(
                again.is_ok(),
                "Hub 进程还该活着（重连应当成功），但它对一次全新握手回了 {again:?}；原发送错误 {e:?}"
            );
        }
    }
}

/// 标准⑥：写队列打满时，控制面（`disconnect`）仍能送达。
///
/// 控制面消息走独立的 `OutboundTable`，**不排队**——数据帧走另一条路。
/// 这条锁住「控制面与数据面分队列」这个决定。
///
/// 该用例不碰开通道入口：它只有两条连接与一帧 `disconnect`，没有任何账本
/// 参与，所以本轮把设备侧自发 `open_channel` 改成走 `POST /api/channels`
/// 时它原样保留。
#[tokio::test]
async fn the_superseded_frame_still_arrives_while_data_is_flooding() {
    let hub = Hub::boot().await;
    let (mut est, node_id, kp) = connect_enrolled(&hub).await;

    let est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    drop(est2);

    // 旧连接在洪峰中仍应收到 disconnect{superseded}
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match est.next_text().await {
            Ok(Some(text)) => {
                let f: Frame = serde_json::from_str(&text).unwrap();
                if f.msg_type == types::DISCONNECT {
                    assert_eq!(f.c["reason"], "superseded");
                    return;
                }
            }
            Ok(None) | Err(_) => panic!("旧连接在被顶替时必须先收到 disconnect"),
        }
        assert!(Instant::now() < deadline, "3s 内没收到 disconnect");
    }
}

/// 生产入口开通道后连发 40 帧上行数据：**第 32 帧后必须收到 `channel_credit`**，
/// 且**一帧 `channel_error` 都不能收到**，Hub 侧账本回到 32 而不是 0。
///
/// 这条锁的是 2026-10-22 修掉的那个 🔴：Hub 侧信用账本的建表入口原本在
/// `channels.rs::on_open`，而它只由设备侧自发的 `open_channel` 帧触发——
/// 真实入口 `POST /api/channels` 一次都不建账本。于是每帧上行都判「未知通道」，
/// 回一帧 `channel_error{unsupported}` 且**永不回 `channel_credit`**，设备侧信用
/// 扣到 0 之后终端输出在 ~32 帧后**永久冻结**。
///
/// 全程静默是这条缺陷最毒的地方：`ws.rs` 在记账**之前**就把 payload 推给 SSE，
/// 所以终端里确实先出现 32 帧字符；而 node 侧 `on_channel_frame` 没有
/// `CHANNEL_ERROR` 臂，那批错帧对设备完全不可见。
///
/// **40 帧而不是 32 帧**：32 是触发回填的临界点，多发的 8 帧用来证明「回填之后
/// 按新余额继续记账」——若回填是假的（只发信用帧不记账），第 33 帧会再次归零，
/// 于是每帧都补一次，`channel_credit` 的数量会对不上。
#[tokio::test]
async fn forty_uplink_frames_get_credit_refill_and_no_error() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    // 设备要先上报能力：`POST /api/sessions` 与 `POST /api/channels` 都拿
    // 心跳里的 connectors 做硬过滤，没上报就是 `device_offline`。
    report_connectors_without_git(&hub, &mut est).await;
    let page = Page::new(hub).await;
    let session = page.make_session(&node_id).await;
    let channel_id = page.open_channel(&session, "terminal").await;

    // 等设备侧真的收到 open_channel：Hub 侧账本在建帧时就地写好，但要断言
    // 「设备收到了哪条 id」仍得从 WS 里读——否则下面的 channel_data 可能
    // 打在一个设备还没开的通道上。
    let seen = open_frame_on(&mut est).await;
    assert_eq!(seen, channel_id, "设备收到的 open_channel 必须与响应同 id");

    let mut credit_frames = 0;
    for seq in 1..=40u64 {
        est.send(&channel_data(&channel_id, seq)).await.unwrap();

        // 每发几帧就把 Hub 的回帧读干。**读不到是正常路径**：Hub 只在余额
        // 归零时回信用帧，不是每帧都回。这里要抓的是「回了一帧
        // channel_error」——那是账本没建起来的症状。
        if seq % 8 == 0 || seq == 40 {
            let deadline = Instant::now() + Duration::from_millis(800);
            loop {
                let Ok(Ok(Some(text))) =
                    tokio::time::timeout(Duration::from_millis(150), est.next_text()).await
                else {
                    break;
                };
                let f: Frame = serde_json::from_str(&text).unwrap();
                if f.msg_type == types::CHANNEL_ERROR {
                    panic!(
                        "第 {seq} 帧后收到 channel_error（不该有任何一帧）: {:?}",
                        f.c
                    );
                }
                if f.msg_type == types::CHANNEL_CREDIT {
                    credit_frames += 1;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }

    // 40 帧的账：初始 32 → 第 32 帧归零时回填 +32 → 余额 32 → 剩下 8 帧
    // 扣到 24。所以**恰好回填一次**，Hub 侧最终余额是 24 而不是 32。
    //
    // 「余额是 24 而不是 32」正是这条断言的重点：回填而不记账的话余额恒为 0，
    // 下一帧又立刻归零，于是每帧都补一次信用，设备侧可用余额越滚越大
    // （约束 4 的每通道独立流控失效）。而 Hub 记了账，余额就落在 24。
    assert_eq!(
        credit_frames, 1,
        "40 帧只该触发一次回填（第 32 帧），实际 {credit_frames} 次——\
         每次都补说明 Hub 侧账本没有跟着记账"
    );
    assert_eq!(
        page.hub.credit(&node_id, &channel_id),
        Some(CREDIT_START - 8),
        "回填之后要按新余额继续记账，余额应是 32-8=24"
    );
}

// ------------------------------------------------------------------ git 面板
//
// v0.4.0 子任务 3：**证明 Hub 侧不需要为 `git` 写任何代码**。
//
// Hub 不认识任何具体的 connector 名——`open_channel` 把 `connector` 当自由
// 字符串原样下发，唯一的门禁是「设备上报过它吗」。所以 `git` 能跑通这件事
// 是注册表那个改动的下游后果，Hub 侧一行没动。下面两条把这件事钉住：
// 能开、且没上报时**仍然会被过滤**（否则「一行没动」就退化成「谁都能过」）。

/// `connector:"git"` 能开通道，且设备侧收到的帧里 `connector` 逐字是 `git`。
///
/// 读设备侧收到的 `open_channel`，**只认 `want` 那条**。
///
/// # 为什么不能直接用 [`open_spec_on`]（`v0.6.0`）
///
/// `v0.6.0` 起建会话会向设备探一次起点：设备上报了 `git` 时，`POST /api/sessions`
/// 会先在这条连接上开一条 `git` 通道探 `rev` 再关掉。于是设备侧在用例真正那条
/// `open_channel` 之前，先收到**探针那条** `open_channel`。
///
/// [`open_spec_on`] 拿到的就会是探针的 id，用例里那句
/// 「设备收到的必须是页面上那条通道」随即失败——而失败的原因与它要测的
/// （Hub 原样透传 `connector` 名）**毫无关系**。
///
/// 所以这里按 id 取：跳过探针那条（以及任何别的 `open_channel`），
/// **断言本身一条没改**，只是不再被无关的帧顶掉。
///
/// 探针那条通道 id 的唯一性由 `identity::random_id()` 保证（22 字符 base64url），
/// 不会与本用例自己那条撞上。
async fn open_spec_for(
    est: &mut node_core::conn::Established,
    want: &str,
) -> protocol::channel::OpenChannel {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = est.next_text().await.unwrap().expect("hub must reply");
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::OPEN_CHANNEL {
            let spec = f
                .payload::<protocol::channel::OpenChannel>()
                .expect("open_channel payload");
            if spec.channel_id == want {
                return spec;
            }
            // 不是我们要的那条（探针的）：继续读，不把它当结果。
            continue;
        }
        assert!(
            Instant::now() < deadline,
            "5s 内没收到 {want} 那条 open_channel"
        );
    }
}

/// 只断言 `channel_id` 对得上是不够的：`open_channel` 的载荷里 `connector`
/// 若在透传中被换成别的名字（或掉了），设备会拿它去查注册表并回
/// `channel_error{unsupported}`——那正是 `v0.2.0` 那种「两侧各自全绿、
/// 真机全坏」的形状。所以这里断言名字本身。
#[tokio::test]
async fn the_git_connector_opens_a_channel_end_to_end() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    report_connectors(&hub, &mut est).await;

    let page = Page::new(hub).await;
    let session_id = page.make_session(&node_id).await;
    let channel_id = page.open_channel(&session_id, "git").await;

    // 按 id 取：设备上报了 `git`，所以建会话时探针已经在同一条连接上开过
    // 一条 `git` 通道（见 `open_spec_for` 的注释）。断言本身没有放宽——
    // 「设备收到的必须是页面上那条通道」照样成立，只是不再被探针那条顶掉。
    let spec = open_spec_for(&mut est, &channel_id).await;
    assert_eq!(
        spec.channel_id, channel_id,
        "设备收到的必须是页面上那条通道"
    );
    assert_eq!(
        spec.connector, "git",
        "Hub 必须原样透传 connector 名；换成别的名字设备会回 unsupported"
    );
}

/// 设备**没上报** git 时，开 git 通道回 409 `connector_not_reported`。
///
/// 这是能力硬过滤作用在 git 上的证据。没有这条，「Hub 不需要改」也可能是
/// 因为那层过滤压根不生效——而一个不生效的过滤会让任务被派给做不到的机器
/// （约束 9），症状是「点开面板永远空着」。
#[tokio::test]
async fn opening_git_without_the_capability_is_refused() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    // 只上报 terminal：**明确不含 git**。
    report_capabilities(&hub, &mut est, &["terminal"]).await;

    let page = Page::new(hub).await;
    let session_id = page.make_session(&node_id).await;

    let mut req = page.req("POST", "/api/channels");
    *req.body_mut() =
        Body::from(json!({"session_id": session_id, "connector": "git", "params": {}}).to_string());
    let resp = page.app.clone().oneshot(req).await.expect("app responds");
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "没上报的 connector 必须被拒，而不是下发一帧注定失败的 open_channel"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    // 错误体形状是 `{error, detail}`（`ApiError::conflict_with` 的口径）。
    assert_eq!(
        body["error"], "connector_not_reported",
        "错误码要是那一个，前端只写一套分支: {body}"
    );
    assert_eq!(
        body["detail"]["reported"],
        json!(["terminal"]),
        "要把设备实际报了什么带回去，用户才知道该去修哪台机器: {body}"
    );
}
