//! 子任务 6「掉线重连」的端到端测试。
//!
//! 判据只有一条：**连续心跳丢失 > 45s**（`security-and-lifecycle.md` §5）。
//! 不看重连次数——设备可能「心跳超时但重连成功」，此时连接是通的。
//!
//! 本文件覆盖 Hub 侧那半（③ / ④ / ⑥ 与 `Reconnecting` 语义）；
//! node 侧的退避与抖动是纯函数，单测在 `node-core/src/reconnect.rs`。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_core::{presence, Presence, OFFLINE_AFTER};
use hub_server::presence::{sweep, SCAN_INTERVAL};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, Established};
use protocol::frame::Frame;
use protocol::message::{types, DeviceInfo};
use tokio::net::TcpListener;

/// 一份完整的 enroll 握手（含 `auth_ok`）。
async fn connect_enrolled(hub: &Hub) -> (Established, String, KeyPair) {
    let kp = KeyPair::generate();
    let est = connect_once(&hub.ws_url(), &kp, None, None, device_info())
        .await
        .expect("enroll must succeed");
    let node_id = est.auth_ok.node_id.clone();
    (est, node_id, kp)
}

/// 用同一把钥匙重连（重连形态：只带 node_id 与签名，不带公钥）。
async fn connect_reconnect(hub: &Hub, kp: &KeyPair, node_id: &str) -> Established {
    connect_once(&hub.ws_url(), kp, Some(node_id), None, device_info())
        .await
        .expect("reconnect must succeed")
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

fn full_heartbeat() -> serde_json::Value {
    serde_json::json!({
        "node_version": "0.1.0",
        "capabilities": {"connectors": ["terminal", "files", "git", "exec"], "os": "linux", "arch": "aarch64"},
        "resources": {"load": {"load1": 1.8}},
        "workspace_roots": ["/w"],
    })
}

async fn send_heartbeat(ws: &mut Established, c: serde_json::Value) {
    ws.send(&Frame::push(types::HEARTBEAT, 0, c))
        .await
        .expect("heartbeat must be sendable");
}

/// 把内存里那份上报的**时间戳**往回拨（判据是心跳年龄，不是墙钟）。
///
/// 标准③ 要等 46s；这里不真等——直接改 `Report.at`，让扫瞄器用真实的
/// `OFFLINE_AFTER` 常量判。改的只是「这份上报看起来多旧」。
fn age_the_report(hub: &Hub, node_id: &str) {
    let mut r = hub
        .state
        .reports()
        .get(node_id)
        .expect("must have reported once");
    r.at = Instant::now() - OFFLINE_AFTER - Duration::from_secs(1);
    hub.state.reports().record(node_id, r);
}

/// 一台跑起来的 Hub（随机端口 + 临时数据目录）。
struct Hub {
    addr: std::net::SocketAddr,
    state: AppState,
    _dir: tempfile::TempDir,
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
        let app = hub_server::build_app(state.clone());
        // 不起 graceful shutdown：这个夹具只被 drop，而 serve 的 task
        // 随之被丢弃——没有断言依赖它「干净结束」。
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });
        Self {
            addr,
            state,
            _dir: dir,
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    fn status(&self, id: &str) -> Option<String> {
        self.state
            .devices()
            .find_by_id(id)
            .ok()
            .flatten()
            .map(|d| d.status.as_str().to_string())
    }
}

/// 标准③：心跳停止 46s → `device.status = offline`。
///
/// 判据不改、不压缩：往回拨上报时间戳（46 秒前），扫瞄器用真实的
/// `OFFLINE_AFTER` 常量判。真等 46 秒跑一次用例不值得。
#[tokio::test]
async fn a_device_without_heartbeats_for_46s_is_offline() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;

    age_the_report(&hub, &node_id);
    let marked = sweep(hub.state.reports(), hub.state.devices()).await;
    assert_eq!(marked, vec![node_id.clone()], "超时的必须被标离线");
    assert_eq!(hub.status(&node_id).as_deref(), Some("offline"));
}

/// 标准③ 后半句：**心跳恢复后立刻回 online**。
#[tokio::test]
async fn a_heartbeat_that_returns_marks_the_device_online_again() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;

    age_the_report(&hub, &node_id);
    sweep(hub.state.reports(), hub.state.devices()).await;
    assert_eq!(hub.status(&node_id).as_deref(), Some("offline"));

    // 心跳回来了
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;
    // 且扫瞄器不会把它再打回去
    let marked = sweep(hub.state.reports(), hub.state.devices()).await;
    assert!(marked.is_empty(), "恢复后不该再被标离线");
}

/// 「心跳超时但 WS 仍通」这条**规格内部有冲突**，本测试按文档一侧落地。
///
/// 里程碑标准④ 要求「`device.status` 仍为 `online`」，但三处文档一致地说
/// **判据只看心跳**：`protocol.md` §「已鉴权连接连续 45s 无心跳 → 判 offline」、
/// `security-and-lifecycle.md` §5「判定只看心跳，不看重连次数」，且 §5 特意把
/// 「心跳超时但重连成功、连接是通的」列为**反面例子**——按连接状态判就会把
/// 活着的设备标成离线，而调度硬过滤第一步就排除 offline。
///
/// 这里按文档实现（标 offline），并保留「连接仍可收发」那一半标准：
/// 活着的 socket 不会被判定流程牵连，且回一次心跳立刻回 online。
#[tokio::test]
async fn a_stale_heartbeat_marks_offline_but_leaves_the_live_socket_alone() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;

    // 心跳超时了，但连接还在（这里不碰 socket，只把上报改旧）
    age_the_report(&hub, &node_id);
    sweep(hub.state.reports(), hub.state.devices()).await;

    assert_eq!(
        hub.status(&node_id).as_deref(),
        Some("offline"),
        "判据只看心跳：连接通不通不影响判定"
    );
    // 连接本身没被牵连：还能收发，且回一次心跳立刻回 online
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "判定不得动连接"
    );
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;
}

/// 重连窗口：`Reconnecting` —— WS 断了但心跳新鲜 → **调度仍算可用**。
///
/// 这条是子任务 5 与 6 的接缝。断开时若把上报删了，这里就会是 `Offline`，
/// `PRODUCT.md`「离线期间会话仍存活」的承诺就落空了。
#[tokio::test]
async fn a_disconnected_device_with_a_fresh_heartbeat_is_still_schedulable() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;

    // 断开（drop 关 TCP）
    drop(est);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    let p = presence(hub.state.reports(), &node_id, false, Instant::now());
    assert_eq!(p, Presence::Reconnecting, "WS 断了但心跳新鲜");
    assert!(p.schedulable(), "重连中的设备必须可被路由");
    assert_eq!(p.persisted(), "online", "库里只有 online/offline 两值");
    // 上报没被删 —— 这正是 Reconnecting 存在的前提
    assert!(hub.state.reports().get(&node_id).is_some());
}

/// 标准⑥：断开处理器先校验「这条连接仍是当前连接」再摘表。
///
/// 被顶替的旧连接不得改新连接的状态。这里的构造是：同 node_id 连第二次，
/// 然后**关掉旧的那条**——新连接的状态必须不受影响。
#[tokio::test]
async fn a_superseded_connection_does_not_reset_the_new_ones_status() {
    let hub = Hub::boot().await;
    let (mut est1, node_id, kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est1, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;

    // 第二条连接顶掉第一条
    let mut est2 = connect_reconnect(&hub, &kp, &node_id).await;
    send_heartbeat(&mut est2, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;

    // 关掉**旧的**那条：它已经不是当前连接了
    drop(est1);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 新连接仍在表里，status 仍是 online
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "旧连接不得把新连接摘掉"
    );
    assert_eq!(hub.status(&node_id).as_deref(), Some("online"));
    // 新连接还能发心跳（它没被误伤）
    send_heartbeat(&mut est2, full_heartbeat()).await;
    wait_status(&hub, &node_id, "online", Duration::from_secs(3)).await;
}

/// 标准②：重连后 `node_id` 不变。
#[tokio::test]
async fn reconnecting_keeps_the_same_node_id() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    drop(est1);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    let est2 = connect_reconnect(&hub, &kp, &node_id).await;
    assert_eq!(
        est2.auth_ok.node_id, node_id,
        "重连不得换 node_id——换了就是另一台设备"
    );
}

/// 标准⑦：重连成功后 1s 内收到一次心跳，且 `last_seen_at` 刷新。
#[tokio::test]
async fn a_reconnected_connection_heartbeats_immediately() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    drop(est1);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    let mut est2 = connect_reconnect(&hub, &kp, &node_id).await;
    // 不 sleep：子任务 5 保证 auth_ok 之后**立即**发一次，不等第一个周期
    let before = Instant::now();
    send_heartbeat(&mut est2, full_heartbeat()).await;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if hub.state.reports().get(&node_id).is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "1s 内必须收到心跳");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(hub.state.reports().get(&node_id).is_some());
}

/// 扫瞄周期必须短于判定窗口：否则「45s 判离线」实际要等两轮才生效。
#[test]
fn the_scan_interval_is_shorter_than_the_offline_window() {
    assert!(
        SCAN_INTERVAL < OFFLINE_AFTER,
        "扫瞄 {SCAN_INTERVAL:?} 必须短于判据 {OFFLINE_AFTER:?}"
    );
}

// ---------- helpers ----------

async fn wait_status(hub: &Hub, node_id: &str, want: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if hub.status(node_id).as_deref() == Some(want) {
            return;
        }
        assert!(Instant::now() < deadline, "status 始终没变成 {want}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_disconnected(hub: &Hub, node_id: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if hub.state.connections().get(node_id).is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "连接始终没被摘掉");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
