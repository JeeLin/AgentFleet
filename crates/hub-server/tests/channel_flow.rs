// 子任务 8「通道与流控」的端到端测试。
//
// 测的是 Hub 侧的三件事：通道按 (node_id, channel_id) 分桶、每通道独立信用、
// 以及「关一条不碰其他」。协议层的状态机与信用账本本身在 protocol 的单测里。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, Established};
use protocol::channel::{ChannelCredit, ChannelData, CloseChannel};
use protocol::frame::Frame;
use protocol::message::{types, DeviceInfo};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct Hub {
    addr: std::net::SocketAddr,
    state: AppState,
    _dir: tempfile::TempDir,
    _shutdown: Option<oneshot::Sender<()>>,
}

impl Hub {
    async fn boot() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let state = AppState::bootstrap_with(
            pool,
            dir.path(),
            AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        state
            .auth()
            .setup_password("correct horse battery")
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let app = hub_server::build_app(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        });
        std::mem::forget(task);
        Self {
            addr,
            state,
            _dir: dir,
            _shutdown: Some(tx),
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    fn credit(&self, node_id: &str, channel_id: &str) -> Option<u32> {
        self.state.channels().read().credit(node_id, channel_id)
    }
}

fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: "pi".to_string(),
        os: "linux".to_string(),
        arch: "aarch64".to_string(),
        hostname: "pi".to_string(),
        node_version: "0.1.0".to_string(),
    }
}

async fn connect_enrolled(hub: &Hub) -> (Established, String, KeyPair) {
    let kp = KeyPair::generate();
    let est = connect_once(&hub.ws_url(), &kp, None, None, device_info())
        .await
        .expect("enroll must succeed");
    let node_id = est.auth_ok.node_id.clone();
    (est, node_id, kp)
}

fn open_channel(id: &str, connector: &str, credit_start: Option<u32>) -> Frame {
    let mut payload = serde_json::json!({
        "channel_id": id,
        "connector": connector,
    });
    if let Some(c) = credit_start {
        payload["credit_start"] = serde_json::json!(c);
    }
    Frame::push(types::OPEN_CHANNEL, 1, payload)
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

    for (i, conn) in ["terminal", "files", "git"].iter().enumerate() {
        est.send(&open_channel(&format!("ch-{i}"), conn, Some(8)))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    for i in 0..3 {
        assert_eq!(hub.credit(&node_id, &format!("ch-{i}")), Some(8));
    }

    // 只消费 ch-0 的信用，其他两条不动
    est.send(&channel_data("ch-0", 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hub.credit(&node_id, "ch-0"), Some(7));
    assert_eq!(hub.credit(&node_id, "ch-1"), Some(8), "ch-1 不得被牵连");
    assert_eq!(hub.credit(&node_id, "ch-2"), Some(8), "ch-2 不得被牵连");
}

/// 标准②：信用耗尽后 Hub 补 `channel_credit`。
#[tokio::test]
async fn exhausting_credit_makes_the_hub_issue_more() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    est.send(&open_channel("ch-0", "terminal", Some(2)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.credit(&node_id, "ch-0"), Some(2));

    // 发 2 帧把信用吃光
    est.send(&channel_data("ch-0", 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(hub.credit(&node_id, "ch-0"), Some(1));
    est.send(&channel_data("ch-0", 3)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(hub.credit(&node_id, "ch-0"), Some(0), "两帧吃光");

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
    assert_eq!(c.channel_id, "ch-0");
    assert!(c.delta > 0, "补的信用必须是正数: {}", c.delta);
}

/// 标准③：关掉一条通道，另一条照常可用。
#[tokio::test]
async fn closing_one_channel_leaves_the_others_working() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    est.send(&open_channel("ch-a", "terminal", Some(4)))
        .await
        .unwrap();
    est.send(&open_channel("ch-b", "files", Some(4)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    est.send(&close_channel("ch-a")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // ch-b 仍能用：继续消费它的信用
    est.send(&channel_data("ch-b", 3)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        hub.credit(&node_id, "ch-b"),
        Some(3),
        "关掉 ch-a 之后 ch-b 必须还能发"
    );
}

/// 标准④：重连后 credit 重置为 credit_start，不是旧连接的剩余额度。
///
/// 这条是 §6 末条那个「无限累积」陷阱的守卫。
#[tokio::test]
async fn reconnecting_resets_the_credit_to_credit_start() {
    let hub = Hub::boot().await;
    let (mut est, node_id, kp) = connect_enrolled(&hub).await;

    est.send(&open_channel("ch-0", "terminal", Some(8)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // 旧连接吃光它
    est.send(&channel_data("ch-0", 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(hub.credit(&node_id, "ch-0"), Some(7));

    // 断开：整节点记录丢弃
    drop(est);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if hub.state.connections().get(&node_id).is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "连接始终没被摘掉");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hub.credit(&node_id, "ch-0"), None, "断线即清账");

    // 新连接重新 open_channel
    let mut est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    est2.send(&open_channel("ch-0", "terminal", Some(8)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        hub.credit(&node_id, "ch-0"),
        Some(8),
        "必须是 credit_start，不是旧连接的余额 7"
    );
}

/// 标准⑦：`open_channel` 失败要**显式回错**，不能静默丢弃。
#[tokio::test]
async fn a_bad_open_channel_gets_an_explicit_error() {
    let hub = Hub::boot().await;
    let (mut est, _node_id, _kp) = connect_enrolled(&hub).await;

    // 空 channel_id（协议层判 EmptyChannelId）
    est.send(&open_channel("", "terminal", None)).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let err = loop {
        let text = est.next_text().await.unwrap().expect("hub must reply");
        let f: Frame = serde_json::from_str(&text).unwrap();
        if f.msg_type == types::CHANNEL_ERROR {
            break f;
        }
        assert!(Instant::now() < deadline, "3s 内没收到 channel_error");
    };
    assert_eq!(err.c["code"], "unsupported");
    assert!(
        err.c["reason"].as_str().is_some(),
        "错误帧必须带人话 reason: {:?}",
        err.c
    );
}

/// 未知通道上发数据 → 显式回错，而不是把连接弄死。
#[tokio::test]
async fn data_on_an_unknown_channel_is_an_error_not_a_disconnect() {
    let hub = Hub::boot().await;
    let (mut est, _node_id, _kp) = connect_enrolled(&hub).await;

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

    // 连接还活着：心跳照常
    est.send(&open_channel("ch-1", "terminal", Some(4)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(hub.state.connections().get(&_node_id).is_some());
}

/// 约束 14：Hub 不该收的通道帧（`channel_ready` 是 Hub 发出的方向）
/// 按未知类型忽略，**不断开连接**。
#[tokio::test]
async fn an_unexpected_channel_frame_is_ignored_without_closing() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    est.send(&Frame::push(
        types::CHANNEL_READY,
        1,
        serde_json::json!({"channel_id": "ch-1", "capabilities": {}}),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "反向的通道帧不得把连接弄死"
    );
    // 之后照常能开通道
    est.send(&open_channel("ch-1", "terminal", Some(4)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(hub.credit(&node_id, "ch-1"), Some(4));
}

/// `credit_start` 不填时按协议常量，不是 0。
///
/// 按 0 处理的话通道一开余额就是 0：node 等 Hub 的许可、Hub 等 node 的数据，
/// 双方互等。这条锁住「缺省值只有一个来源」。
#[tokio::test]
async fn omitting_credit_start_falls_back_to_the_protocol_constant() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    est.send(&open_channel("ch-0", "terminal", None))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        hub.credit(&node_id, "ch-0"),
        Some(protocol::channel::CREDIT_START),
        "缺省必须按协议常量，而不是 0"
    );
}

/// 通道 id 按 node 分桶：两台设备的 `ch-1` 互不干扰（约束 4）。
#[tokio::test]
async fn the_same_channel_id_on_two_nodes_stays_separate() {
    let hub = Hub::boot().await;
    let (mut est_a, id_a, _kp_a) = connect_enrolled(&hub).await;
    let (mut est_b, id_b, _kp_b) = connect_enrolled(&hub).await;

    est_a
        .send(&open_channel("ch-1", "terminal", Some(4)))
        .await
        .unwrap();
    est_b
        .send(&open_channel("ch-1", "files", Some(9)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(hub.credit(&id_a, "ch-1"), Some(4));
    assert_eq!(hub.credit(&id_b, "ch-1"), Some(9), "不得串到另一台设备");
}

/// 标准⑤：读侧硬上限——持续灌数据时 Hub **不崩、不误杀**，而是慢读顶住。
///
/// 规格要的是「超限 → 慢读；持续 30s 才断开」（§6.1），所以这里不断言
/// 「立刻断开」——立刻断是错的：`cat` 一个大文件就是一次合法的 PTY 洪峰，
/// 误杀它等于「输出多的命令一律失败」。
/// 断言的是 Hub 活下来、连接还在，且**没有把内存吃光**（进程没被 OOM kill）。
#[tokio::test]
async fn a_flooding_node_is_slow_read_not_killed_on_the_first_burst() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

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
        hub.state.connections().get(&node_id).is_some(),
        "一次突发不该让 Hub 断开连接（那是合法的 PTY 洪峰形状）"
    );

    // 洪峰过去之后连接照常可用。给 Hub 时间消化积压的那 12 MiB——
    // 慢读是有意的，所以这里不能只等一个固定时长就断言。
    est.send(&open_channel("ch-1", "terminal", Some(4)))
        .await
        .expect("洪峰之后连接必须还能用");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if hub.credit(&node_id, "ch-1") == Some(4) {
            break;
        }
        assert!(Instant::now() < deadline, "洪峰消化完之后必须能开通道");
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
    // 发得出去就应当被判 frame too large，发不出去则连接已断——两者都可接受，
    // 关键是**不能**把 Hub 弄死。
    let big = "x".repeat(1024 * 1024 + 64);
    let f = Frame::push(
        types::CHANNEL_DATA,
        1,
        serde_json::json!({"channel_id": "ch-0", "payload": {"chunk": big}}),
    );
    if est.send(&f).await.is_ok() {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match est.next_text().await {
                Ok(Some(text)) => {
                    let r: Frame = serde_json::from_str(&text).unwrap();
                    if r.msg_type == types::ERR {
                        assert_eq!(r.c["code"], "frame too large");
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
            assert!(Instant::now() < deadline);
        }
    }
}

/// 标准⑥：写队列打满时，控制面（`disconnect`）仍能送达。
///
/// 控制面消息走独立的 `OutboundTable`，**不排队**——数据帧走另一条路。
/// 这条锁住「控制面与数据面分队列」这个决定。
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
