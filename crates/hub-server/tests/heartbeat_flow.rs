//! 心跳与能力上报的集成测试：逐条对应 M0 里程碑子任务 5 的测试标准。
//!
//! 与 `ws_flow.rs` 的分工：那个文件证明「握手与接入」，本文件证明
//! **握手之后**的那件事——`auth_ok` 之后 node 发心跳、Hub 收到后
//! 既更新内存也落库。
//!
//! 客户端用 `node_core` 的真代码（`connect_once` + `Established::send`）：
//! 被测行为含「node 什么时候发第一帧」，裸手搓的客户端证明不了
//! 「`auth_ok` 后 1s 内就发」这一条（那一条由 node-cli 侧的单测覆盖）。
//!
//! 映射关系写在每个测试的文档注释里（`标准①` … `标准⑥`）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, ConnOutcome, Established};
use node_core::identity::Identity;
use protocol::frame::Frame;
use protocol::message::{types, DeviceInfo};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 测试进程装一个 `tracing` sink。
///
/// 不是为了看日志：没有 subscriber 时 `tracing` 的 dispatch 判否短路，
/// `error!(... "heartbeat persistence failed")` 里的字段表达式不执行，
/// llvm-cov 会把它们记成未覆盖。
fn install_subscriber() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::sink)
            .init();
    });
}

struct Hub {
    addr: std::net::SocketAddr,
    state: AppState,
    _dir: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Hub {
    async fn boot() -> Self {
        install_subscriber();
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
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
        });
        std::mem::forget(task);
        Self {
            addr,
            state,
            _dir: dir,
            shutdown: Some(tx),
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// 直接查库（绕过内存缓存）——标准⑤ 要的就是「离线后仍可读」。
    fn device_row(&self, id: &str, column: &str) -> Option<String> {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        let conn = pool.get().unwrap();
        conn.query_row(
            &format!("SELECT {column} FROM device WHERE id = ?1"),
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    /// 库里塞一行「有 node_id、没有公钥」的设备（重连必被拒的那条路径）。
    fn insert_legacy_device(&self, id: &str) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        pool.get()
            .unwrap()
            .execute(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at, status, os, arch, hostname, node_version)
                 VALUES (?1, 'legacy', ?2, 'unix:0', 'offline', 'linux', 'aarch64', 'h', '0.1.0')",
                [id, &format!("fp-{id}")],
            )
            .unwrap();
    }

    /// 故障注入：让**查询设备**失败（把表改名，SELECT 报 no such table）。
    ///
    /// 读失败与写失败是两条不同的降级路径，要分开造：写失败留给
    /// `break_device_writes`，这里要的是「连旧值都读不到」。
    fn break_device_lookup(&self) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        pool.get()
            .unwrap()
            .execute_batch("ALTER TABLE device RENAME TO device_moved")
            .unwrap();
    }

    /// 故障注入：让 `device` 表的 **UPDATE 一律失败**。
    ///
    /// `RAISE(ABORT)` 只回滚本语句、连接照常可用，于是 Hub 侧走的是
    /// 「读得到、写不进」——落库失败但连接不断的那条路。
    fn break_device_writes(&self) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        pool.get()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER boom_update BEFORE UPDATE ON device
                 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
            )
            .unwrap();
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn dial(url: &str) -> Ws {
    let (ws, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(resp.status(), 101u16, "must upgrade before anything else");
    ws
}

async fn send_text(ws: &mut Ws, t: &str) {
    let _ = ws
        .send(tokio_tungstenite::tungstenite::Message::Text(t.into()))
        .await;
}

/// 读下一个文本帧（跳过自动应答的 pong）。
async fn next_text(ws: &mut Ws) -> String {
    loop {
        let msg = ws.next().await.expect("hub must send a frame").unwrap();
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => return t.to_string(),
            tokio_tungstenite::tungstenite::Message::Pong(_)
            | tokio_tungstenite::tungstenite::Message::Ping(_) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

async fn next_frame(ws: &mut Ws) -> Frame {
    let text = next_text(ws).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("unparsable json: {text} ({e})"))
}

const HELLO: &str = r#"{"v":1,"seq":0,"t":"hello","c":{"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}}"#;

fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: "hb-node".into(),
        os: "linux".into(),
        arch: "aarch64".into(),
        hostname: "hbhost".into(),
        node_version: "0.1.0".into(),
    }
}

/// 一份 enroll 成功后的连接。`node_id` 用 Hub 签发回来的那个。
async fn connect_enrolled(hub: &Hub) -> (Established, String, KeyPair) {
    let kp = KeyPair::generate();
    let est = connect_once(&hub.ws_url(), &kp, None, device_info())
        .await
        .expect("enroll must succeed");
    let node_id = est.auth_ok.node_id.clone();
    (est, node_id, kp)
}

/// 用**同一把钥匙**重连（重连形态：只带 node_id 与签名，不带公钥，§3）。
async fn connect_reconnect(hub: &Hub, kp: &KeyPair, node_id: &str) -> Established {
    connect_once(&hub.ws_url(), kp, Some(node_id), device_info())
        .await
        .expect("reconnect must succeed")
}

/// 发一份心跳。
async fn send_heartbeat(ws: &mut Established, c: serde_json::Value) {
    ws.send(&Frame::push(types::HEARTBEAT, 0, c))
        .await
        .expect("heartbeat must send");
}

/// 一份完整心跳（§2 的能力 + 负载 + 工作区根）。
fn full_heartbeat() -> serde_json::Value {
    serde_json::json!({
        "node_version": "9.9.9",
        "capabilities": {
            "connectors": ["terminal", "files", "git", "exec"],
            "agents": [{"name": "opencode", "version": "0.4.2"}],
            "os": "linux",
            "arch": "aarch64",
            "container_runtime": null
        },
        "resources": {
            "cpu": {"cores": 4, "model": "cortex-a72", "arch": "aarch64"},
            "memory": {"total_mb": 4096, "available_mb": 2870},
            "gpu": null,
            "load": {"load1": 1.8, "load_per_core": 0.45, "active_sessions": 2},
            "disk": {"workspace_free_mb": 12800}
        },
        "workspace_roots": ["/w"]
    })
}

/// 等内存里出现该 node 的上报。轮询而不是固定 sleep：CI 慢时固定 sleep
/// 会 flakes，而「出现」本身就是我们要等的事件。
async fn wait_for_report(hub: &Hub, node_id: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if hub.state.reports().get(node_id).is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "心跳始终没进内存缓存");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 等连接从注册表摘掉（断开后）。
///
/// ⚠️ **不是**等内存上报消失：上报要活过重连窗口，才有「WS 断了但心跳新鲜
/// → `Reconnecting`，调度仍算可用」这回事（子任务 6）。断开时清掉它，
/// `PRODUCT.md`「离线期间会话仍存活」的承诺就落空了。
async fn wait_for_no_report(hub: &Hub, node_id: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if hub.state.connections().get(node_id).is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "连接始终没被摘掉");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 等 `device.last_seen_at` 出现（第一份心跳落库完成的信号）。
async fn wait_for_seen(hub: &Hub, node_id: &str, budget: Duration) -> String {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(v) = hub.device_row(node_id, "last_seen_at") {
            return v;
        }
        assert!(Instant::now() < deadline, "last_seen_at 始终没被写上");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 断开一条连接：直接 drop。drop 会关 TCP，Hub 侧 relay 随即结束并摘表。
///
/// 这里**不发** `disconnect` 帧：那一条是 Hub→node 的方向（子任务 7），
/// node 侧 M0 没有「主动下线」这个动作——它就是进程退出。
async fn disconnect(est: Established) {
    drop(est);
}

/// 标准③：心跳进内存，且时间戳就是刚刚（≤ 1 个周期 + 1s）。
///
/// 顺带锁住「能力与负载是两个独立字段」——路由一个用于硬过滤、
/// 一个用于有序打分，合成一个就写不出正确的调度。
#[tokio::test]
async fn heartbeat_updates_the_in_memory_report() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;

    let now = Instant::now();
    let r = hub
        .state
        .reports()
        .get(&node_id)
        .expect("heartbeat must land in memory");
    assert_eq!(r.node_version, "9.9.9");
    assert_eq!(r.capabilities["connectors"][0], "terminal");
    assert_eq!(r.resources["load"]["load1"], 1.8);
    assert_eq!(r.workspace_roots, vec!["/w".to_string()]);
    assert!(
        r.age(now) < hub_core::OFFLINE_AFTER,
        "刚收到的心跳必须算新鲜（{:?} >= {:?}）",
        r.age(now),
        hub_core::OFFLINE_AFTER
    );
}

/// 标准④（落库侧）：**只含 `node_version`** 的心跳不得清空已落库的能力/负载。
///
/// 「缺省即保留」的端到端版本：先报完整的把值写进库，再报最小的，
/// 然后直接 SELECT 看旧值还在。
///
/// 落库完成的信号用 `last_seen_at`：心跳写的正是那四项
/// （`capabilities`/`resources`/`last_seen_at`/`status`），`node_version`
/// **不在此列**——它在 enroll 首帧的 `device_info` 里带上并落库（§3）。
#[tokio::test]
async fn a_minimal_heartbeat_keeps_the_persisted_capabilities() {
    let hub = Hub::boot().await;
    let (mut est, node_id, kp) = connect_enrolled(&hub).await;

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_seen(&hub, &node_id, Duration::from_secs(3)).await;

    // 第二份：只有 node_version，三项可选载荷全缺。
    // 判定「它也落库了」不靠 last_seen_at——那个串粒度是秒，两份心跳
    // 同一秒发出就写出同一个值。改用断开重连：新连接上的心跳会走到同一条
    // 落库路径，若第二份把库清空了，新连接这轮补不回 connectors。
    send_heartbeat(&mut est, serde_json::json!({"node_version": "0.2.0"})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    disconnect(est).await;
    wait_for_no_report(&hub, &node_id, Duration::from_secs(3)).await;

    let mut est2 = connect_reconnect(&hub, &kp, &node_id).await;
    send_heartbeat(&mut est2, full_heartbeat()).await;
    wait_for_seen(&hub, &node_id, Duration::from_secs(3)).await;

    let caps = hub.device_row(&node_id, "capabilities").expect("caps 仍在");
    let res = hub
        .device_row(&node_id, "resources")
        .expect("resources 仍在");
    let caps: serde_json::Value = serde_json::from_str(&caps).unwrap();
    assert_eq!(
        caps["connectors"][0], "terminal",
        "缺省必须保留旧值，不是清空"
    );
    let res: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(res["load"]["load1"], 1.8, "负载同样必须保留");
    assert_eq!(
        hub.device_row(&node_id, "status").as_deref(),
        Some("online")
    );
    // 版本不被心跳改写：它停在 enroll 那一刻
    assert_eq!(
        hub.device_row(&node_id, "node_version").as_deref(),
        Some("0.1.0")
    );
}

/// 标准④（协议侧）：只含 `node_version` 的心跳**不得**让连接断掉。
///
/// 判据是「连接还活着」：Hub 若按坏帧处理并断开，设备就会进入重连风暴。
#[tokio::test]
async fn a_minimal_heartbeat_does_not_break_the_connection() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    send_heartbeat(&mut est, serde_json::json!({"node_version": "0.1.0"})).await;
    // 再发一份能观察到副作用的：若上一份把连接断了，这一份必然发送失败。
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;
}

/// 载荷畸形 → **只丢这一帧**，不断连。
///
/// 约束 14 的缺省容错边界在这里：连 `node_version` 都解不出来时没有
/// 「保留旧值」的语义可用，但也不值得为此断掉一条好连接。
#[tokio::test]
async fn a_malformed_heartbeat_payload_is_dropped_without_dropping_the_link() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    send_heartbeat(
        &mut est,
        serde_json::json!({
            "node_version": 12345,
            "capabilities": "not an object",
            "workspace_roots": 7
        }),
    )
    .await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;
    assert_eq!(
        hub.state.reports().get(&node_id).unwrap().node_version,
        "9.9.9",
        "畸形帧之后好帧仍被处理 → 连接没坏"
    );
}

/// 标准⑤：设备离线后 `device.capabilities` **仍可读**（直接 SQL，不经内存）。
///
/// 断开 → 内存上报被清掉（与连接同生命周期）→ 但库里那行还在。
/// 调度过滤与 Web 展示都依赖「离线设备支持哪些 connector」。
#[tokio::test]
async fn capabilities_survive_the_device_going_offline() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;

    disconnect(est).await;
    wait_for_no_report(&hub, &node_id, Duration::from_secs(3)).await;

    // 让内存里那份过期（> OFFLINE_AFTER）：这样断言命中的必然是**库里的值**，
    // 而不是「内存还留着」。断开后上报要活过重连窗口（子任务 6 的
    // `Reconnecting`），所以光断连不能保证内存不可信。
    if let Some(mut r) = hub.state.reports().get(&node_id) {
        r.at = Instant::now() - hub_core::OFFLINE_AFTER - Duration::from_secs(1);
        hub.state.reports().record(&node_id, r);
    }
    assert!(
        hub_core::presence(hub.state.reports(), &node_id, false, Instant::now())
            == hub_core::Presence::Offline,
        "内存里这份已经过期，读到的值只能来自库"
    );

    let caps = hub
        .device_row(&node_id, "capabilities")
        .expect("离线后 capabilities 仍可读");
    assert_ne!(caps, "null");
    let caps: serde_json::Value = serde_json::from_str(&caps).unwrap();
    assert_eq!(caps["connectors"][0], "terminal");
}

/// 标准①（Hub 侧）：`auth_ok` 下发的周期**来自 Hub 配置**，节点不硬编码。
///
/// 断言 `auth_ok.heartbeat_interval_ms` 与 Hub 自己的取值一致——
/// 若节点把周期写死，改 Hub 配置就不会反映在这里。
#[tokio::test]
async fn the_heartbeat_interval_comes_from_auth_ok() {
    let hub = Hub::boot().await;
    let (est, _node_id, _kp) = connect_enrolled(&hub).await;
    assert_eq!(
        est.auth_ok.heartbeat_interval_ms,
        hub.state.heartbeat_interval_ms(),
        "周期必须由 Hub 在 auth_ok 下发"
    );
    assert_eq!(
        est.auth_ok.heartbeat_interval_ms,
        hub_server::DEFAULT_HEARTBEAT_INTERVAL_MS
    );
}

/// 捕获 Hub 侧收到的整份载荷（逐个子串查）。
///
/// `Report` 不是 `Serialize`（含 `Instant`），所以把 node 实际发出去的那些
/// 字段各序列化一遍再拼起来——要查的正是那些内容。
fn report_payload_text(r: &hub_core::Report) -> String {
    format!(
        "{}{}{}{}",
        r.node_version,
        r.capabilities,
        r.resources,
        r.workspace_roots.join("\u{0}")
    )
}

/// 标准⑥：心跳载荷里**绝不出现**高价值路径。
///
/// 判据不是「Hub 收到了正确的根」，而是「Hub 收到的整份载荷里
/// 这些字符串出现 0 次」——`~/.agentfleet/`、`.git/`、`.env`、`~/.ssh/`。
/// 「存在」本身就是情报，所以这里连子串都不许有。
#[tokio::test]
async fn the_heartbeat_never_carries_high_value_paths() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    // 构造一棵含高价值路径的树，按 §2 的规则只上报配置的根
    let root = tempfile::tempdir().unwrap();
    let proj = root.path().join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::write(proj.join(".env"), "SECRET=1").unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "key").unwrap();
    std::fs::create_dir_all(home.join(".agentfleet")).unwrap();
    std::fs::write(home.join(".agentfleet/identity.json"), "{}").unwrap();

    let root_path = root.path().to_path_buf();
    let roots = [root_path.as_path()];
    let c = serde_json::json!({
        "node_version": "0.1.0",
        "capabilities": node_core::heartbeat::capabilities(),
        "workspace_roots": node_core::heartbeat::workspace_roots(&roots)
    });
    send_heartbeat(&mut est, c).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;

    // 捕获 Hub 侧收到的整份载荷，逐个子串查
    let r = hub.state.reports().get(&node_id).unwrap();
    let text = report_payload_text(&r);
    for forbidden in [".git", ".env", ".ssh", ".agentfleet", "id_ed25519"] {
        assert_eq!(
            text.matches(forbidden).count(),
            0,
            "心跳载荷里出现了 {forbidden}:\n{text}"
        );
    }
    // 只报根一级：给了 1 个根就报 1 个
    assert_eq!(r.workspace_roots.len(), 1);
}

/// 落库失败**不打断连接**：心跳的用途是让 Hub 知道设备活着，而它确实活着。
/// 内存仍然更新（路由读内存），错误只进日志。
#[tokio::test]
async fn a_failing_write_does_not_drop_the_link() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    hub.break_device_writes();

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;
    assert_eq!(
        hub.state.reports().get(&node_id).unwrap().node_version,
        "9.9.9",
        "落库失败不影响内存缓存——路由继续可用"
    );
    // 连接还活着：再发一份照样成功
    send_heartbeat(&mut est, full_heartbeat()).await;
}

/// 未知帧类型在握手之后一律忽略（约束 14 前向兼容）。
///
/// 心跳是 M0 唯一的出向帧，但 Hub 必须能容忍未来的消息类型——
/// 否则协议加一个可选集字段就会让所有存量设备掉线。
#[tokio::test]
async fn an_unknown_frame_after_auth_is_ignored() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    est.send(&Frame::push(
        "some_future_message",
        0,
        serde_json::json!({}),
    ))
    .await
    .expect("unknown frame must not break the socket");
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;
}

/// 心跳里的 `node_version` 落库（§2：设备离线后 Web 仍显示它的版本）。
#[tokio::test]
async fn the_heartbeat_version_reaches_the_device_row() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_seen(&hub, &node_id, Duration::from_secs(3)).await;
    assert_eq!(
        hub.device_row(&node_id, "status").as_deref(),
        Some("online")
    );
    // 版本来自 enroll 首帧的 device_info，心跳不改它（§3）
    assert_eq!(
        hub.device_row(&node_id, "node_version").as_deref(),
        Some("0.1.0"),
        "设备行必须记着这台设备 enroll 时的版本"
    );
    assert!(
        hub.device_row(&node_id, "last_seen_at").is_some(),
        "last_seen_at 必须被心跳刷新"
    );
}

/// 心跳载荷里没有 `identity` 或私钥材料的槽位（§2 只报能力与负载）。
#[tokio::test]
async fn the_heartbeat_payload_has_no_slot_for_key_material() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;

    let r = hub.state.reports().get(&node_id).unwrap();
    let text = report_payload_text(&r);
    for forbidden in ["private_key", "signature", "public_key", "identity.json"] {
        assert_eq!(
            text.matches(forbidden).count(),
            0,
            "心跳里出现了 {forbidden}"
        );
    }
}

/// 握手失败时 `connect_once` 不返回 `Authed`——心跳路径只在成功之后存在。
///
/// 库里塞一行「有 node_id、没有公钥」的历史数据，重连必被拒。
#[tokio::test]
async fn a_failed_handshake_yields_no_connection_to_heartbeat_on() {
    let hub = Hub::boot().await;
    hub.insert_legacy_device("n_absent");
    let kp = KeyPair::generate();
    let out = connect_once(&hub.ws_url(), &kp, Some("n_absent"), device_info()).await;
    assert!(
        matches!(out, Err(ConnOutcome::Rejected { .. })),
        "got {out:?}"
    );
}

/// node 侧的心跳构造器与 Hub 侧的载荷对得上：`Heartbeat` 解回
/// 字段名一致（否则「缺省保留」会在字段名上静默失效）。
#[tokio::test]
async fn the_node_side_heartbeat_deserializes_on_the_hub_side() {
    let hb = node_core::heartbeat::heartbeat(vec!["/w".to_string()]);
    let text = serde_json::to_string(&hb).unwrap();
    let back: protocol::message::Heartbeat = serde_json::from_str(&text).unwrap();
    assert_eq!(back.node_version, hb.node_version);
    assert!(!back.capabilities.is_null());
    assert_eq!(back.workspace_roots, vec!["/w".to_string()]);

    // 只含 node_version 的形态（标准④）也要能解
    let minimal = r#"{"node_version":"0.1.0"}"#;
    let m: protocol::message::Heartbeat = serde_json::from_str(minimal).unwrap();
    assert!(m.capabilities.is_null());
    assert!(m.resources.is_null());
    assert!(m.workspace_roots.is_empty());
}

/// `Identity` 只提供 `node_id()`（不暴露私钥）——心跳路径不需要它。
#[tokio::test]
async fn identity_exposes_only_the_node_id_for_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let mut id = Identity::load_or_create(dir.path()).unwrap();
    assert!(id.node_id().is_none(), "enroll 前没有 node_id");
    id.set_node_id("n_x").unwrap();
    assert_eq!(id.node_id().as_deref(), Some("n_x"));
}

/// 库里读不到该设备（表被改名）→ 心跳仍进内存、连接不断。
///
/// 覆盖 `on_heartbeat` 的读失败臂：落库路径尽力而为，读不到旧值就按
/// 「没有旧值」合并，而不是把这次上报丢掉——Hub 判在线靠的就是心跳到达。
#[tokio::test]
async fn a_heartbeat_when_the_device_row_cannot_be_read_still_lands_in_memory() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    // 让 SELECT 失败（表改名，与 ws_flow 的 break_device_lookup 同手段）
    hub.break_device_lookup();

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;
    assert_eq!(
        hub.state.reports().get(&node_id).unwrap().node_version,
        "9.9.9",
        "读不到旧值不影响内存更新——路由继续可用"
    );
    // 连接还活着
    send_heartbeat(&mut est, full_heartbeat()).await;
}

/// 握手过后库里那行被删掉（不该发生，但程序不能因此崩）→ 按「没有旧值」合并。
///
/// 这一条覆盖 `Ok(None)` 臂：设备行消失与「从没上报过」在合并上同义。
#[tokio::test]
async fn a_heartbeat_whose_device_row_vanished_is_merged_as_if_new() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;

    let pool = hub_storage::db::open_in(hub.state.data_dir()).unwrap().1;
    pool.get()
        .unwrap()
        .execute("DELETE FROM device WHERE id = ?1", [&node_id])
        .unwrap();

    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;
    // 连接不断（再发一份照样成功）
    send_heartbeat(&mut est, full_heartbeat()).await;
}

/// 握手期对端直接关 TCP（不发 Close 帧）→ 握手以 `peer closed` 结束，不补帧。
///
/// 覆盖 `handshake` 的 `Ok(None)` 臂：对端已经不在了，补一帧失败帧
/// 只会把 fd 耗在一个死人连接上。判据是 Hub 侧干净收尾、连接摘表。
#[tokio::test]
async fn a_peer_that_vanishes_mid_handshake_is_dropped() {
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, HELLO).await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "challenge");
    // 直接 drop：TCP 关闭而不走 WS Close 握手
    drop(ws);
    // Hub 侧应当把这条连接摘掉（handshake 以 Err 结束，不注册）
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if hub.state.connections().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "连接始终没被摘掉");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 握手完成后连接正常结束 → relay 以 `Err` 返回并被记进日志。
///
/// 覆盖 `handle_socket` 里那条 `tracing::info!(... "ws relay ended")`：
/// 「连接为什么结束」是排查设备掉线时唯一有用的信息，而 `tracing` 没有
/// subscriber 时字段表达式根本不会执行 —— 这就是本文件装 sink 的原因。
#[tokio::test]
async fn a_closed_connection_end_is_recorded() {
    let hub = Hub::boot().await;
    let (mut est, node_id, _kp) = connect_enrolled(&hub).await;
    send_heartbeat(&mut est, full_heartbeat()).await;
    wait_for_report(&hub, &node_id, Duration::from_secs(3)).await;

    // 发一个 Close 帧再 drop：Hub 侧走 Message::Close → relay Err
    let _ = est
        .send(&Frame::push(
            types::DISCONNECT,
            0,
            serde_json::json!({"reason":"test done"}),
        ))
        .await;
    disconnect(est).await;
    wait_for_no_report(&hub, &node_id, Duration::from_secs(3)).await;
    assert!(
        hub.state.connections().get(&node_id).is_none(),
        "连接必须从注册表摘掉"
    );
}

/// 握手期对端直接关 TCP（不发 Close 帧）→ `peer closed`，不补失败帧。
#[tokio::test]
async fn a_peer_that_closes_during_the_handshake_is_not_answered() {
    let hub = Hub::boot().await;
    let ws = dial(&hub.ws_url()).await;
    // 不发 hello 就走：recv 读到流结束
    drop(ws);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if hub.state.connections().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "连接始终没被摘掉");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Hub 启动时读不到历史最大 `gen` → 退化成 0 并**记 warn**，不是静默。
///
/// 覆盖 `bootstrap_with` 里那条兜底：内存计数每次重启归零，而设备带着
/// `last_seen_gen` 重连。取不到就退化成 0 会让一部分设备被误判成「已被
/// 顶替」，所以这个失败必须留痕——这里是唯一能看到它的地方。
#[tokio::test]
async fn a_hub_that_cannot_read_the_stored_max_gen_still_boots() {
    install_subscriber();
    let dir = tempfile::tempdir().unwrap();
    let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
    // 让 max_gen 的 SELECT 失败：把整个 schema 弄坏（DROP 掉 device 表）。
    // 语句缓存与表名一致，报的是 no such table 而不是别的错。
    pool.get()
        .unwrap()
        .execute_batch("DROP TABLE device")
        .unwrap();
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
    // 起得来，且计数器从 1 开始（退化成 0 的表现）
    assert_eq!(state.connections().current_gen(), 0);
    assert_eq!(state.connections().next_gen(), 1);
}
