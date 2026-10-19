//! 端到端测试的公共夹具：一台跑起来的 Hub + 几个握手辅助函数。
//!
//! 为什么不各文件自带一份：WS 端到端测试都要「随机端口 + 临时数据目录 +
//! 优雅关闭」，拷贝出来的第五份在 `Drop` 与 `std::mem::forget(task)` 上
//! 已经互相漂移（有的发 shutdown、有的只 drop）。夹具只有一份，
//! 「关不掉 Hub 会漏线程」这类问题就有一个地方可查。
//!
//! `clippy.toml` 的 `allow-*-in-tests` 覆盖不到本文件的顶层 helper
//! （见 `AGENTS.md`），所以文件头必须自带这行豁免。
//!
//! `dead_code` 同理：`mod common` 会被编进**每一个**端到端测试二进制，
//! 而每个二进制只用得上其中一部分（心跳夹具不需要通道帧构造器）。
//! 不豁免的话，「这个用例用不到」会在另外四个二进制里各报一次警。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

use std::net::SocketAddr;
use std::time::Duration;

use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, Established};
use protocol::frame::Frame;
use protocol::message::{types, DeviceInfo};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message as WsMsg;

/// 裸 WebSocket 流类型。
pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 给测试进程装一个 `tracing` subscriber。
///
/// **不是为了看日志，而是为了覆盖 `tracing::info!` 里的字段表达式**：
/// 没有 subscriber 时 `tracing` 的 dispatch 判否就短路，
/// `conn_id = %conn_id` 这类行**根本不会执行**——llvm-cov 把它们记成未覆盖。
/// 全局只能装一次，用 `Once` 保证。
pub fn install_subscriber() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::sink)
            .init();
    });
}

/// 测试用 Argon2id 参数：最小可用值，跑得快。
///
/// **刻意不用生产参数**（m=65536）：测试要跑几十次登录与几十次 enroll，
/// 生产参数会让整个集成测试从秒级掉到分钟级。「参数大小是否安全」由
/// `hub-core` 的 `AuthParams::for_cpu_count` 单独覆盖，不该由这里的跑得慢来证明。
pub fn fast_params() -> AuthParams {
    AuthParams {
        m_cost: 8192,
        t_cost: 1,
        p_cost: 1,
    }
}

/// 设备自述。名字与主机名在断言里没有判据，统一成同一个值。
pub fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: "test-node".into(),
        os: "linux".into(),
        arch: "aarch64".into(),
        hostname: "testhost".into(),
        node_version: "0.1.0".into(),
    }
}

/// 一台跑起来的 Hub（随机端口 + 临时数据目录）。
pub struct Hub {
    pub addr: SocketAddr,
    pub state: AppState,
    /// 持有 `tempdir`，确保它在 Hub 存活期间不被删除（删了 DB 文件就废了）。
    _dir: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Hub {
    /// 已设密码的正常模式。
    pub async fn boot() -> Self {
        Self::boot_with_password(true).await
    }

    /// 未初始化模式（无密码）——`/ws` 仍应能 upgrade（§3.1：握手才拒 enroll）。
    pub async fn uninitialized() -> Self {
        Self::boot_with_password(false).await
    }

    pub async fn boot_with_password(with_password: bool) -> Self {
        install_subscriber();
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let state = AppState::bootstrap_with(
            pool,
            dir.path(),
            fast_params(),
            Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        if with_password {
            state
                .auth()
                .setup_password("correct horse battery")
                .unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let app = hub_server::build_app(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        });
        // 保留 join handle 的任务 alive：task 被 drop 会让 serve 提前结束
        std::mem::forget(task);
        Self {
            addr,
            state,
            _dir: dir,
            shutdown: Some(tx),
        }
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// 借一个直连本 Hub 数据目录的连接池。
    ///
    /// `AppState` 持的是池句柄，而测试要往里塞故障触发器 / 直接查库——
    /// 走同一个目录另开一个池，才能改到那些测试自己的连接。
    pub fn pool(&self) -> hub_storage::db::Db {
        hub_storage::db::open_in(self.state.data_dir()).unwrap().1
    }
}

/// 直接查库（绕过内存缓存）。
pub fn device_row(hub: &Hub, id: &str, column: &str) -> Option<String> {
    hub.pool()
        .get()
        .unwrap()
        .query_row(
            &format!("SELECT {column} FROM device WHERE id = ?1"),
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

/// 在库里塞一行「有 node_id、没有公钥」的设备（重连必被拒的那条路径）。
///
/// 正常 enroll 总会顺手把公钥写进 `capabilities`，而「设备早于该字段存在」
/// 的历史数据造不出别的方式。
pub fn insert_legacy_device(hub: &Hub, id: &str) {
    hub.pool()
        .get()
        .unwrap()
        .execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at, status, os, arch, hostname, node_version)
             VALUES (?1, 'legacy', ?2, 'unix:0', 'offline', 'linux', 'aarch64', 'h', '0.1.0')",
            [id, &format!("fp-{id}")],
        )
        .unwrap();
}

/// 故障注入：让 `device` 表的 **UPDATE 一律失败**（读仍正常）。
///
/// 用 `BEFORE UPDATE` 触发器 + `RAISE(ABORT)` 而不是删表：删表会让「读也失败」，
/// 于是同一个用例只能覆盖读错误路径；而这里要的正是**读成功、写失败**——
/// `set_public_key`（一个事务里的两条 UPDATE）与 `set_gen` 只在读得到行、
/// 写不进去时才报 Err。
/// `RAISE(ABORT)` 只回滚本次语句，不影响连接，所以 Hub 侧继续可用。
pub fn break_device_writes(hub: &Hub) {
    hub.pool()
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER boom_update BEFORE UPDATE ON device
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();
}

/// 故障注入：**新设备**的 INSERT 失败（读仍正常）。
///
/// 对应 `Registry::enroll` 把 SQLite 错误往上抛的那条路：Hub 必须回
/// `fingerprint_race_retry` 而不是 panic、也不是把设备「先记着以后再说」。
pub fn break_device_insert(hub: &Hub) {
    hub.pool()
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER boom_insert BEFORE INSERT ON device
             BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END;",
        )
        .unwrap();
}

/// 故障注入：让**查询设备**这一步直接报错（表还在，SELECT 失败）。
///
/// **把表改名而不是 `DROP TABLE`**：DROP 后 Hub 的连接池会命中一个
/// 「schema 已变更但语句已缓存」的中间态，表现为 hello 阶段就报
/// `handshake rejected`，压根走不到重连分支。改名同样让 `SELECT` 报
/// `no such table`，而语句缓存与表名一致、行为稳定可复现。
pub fn break_device_lookup(hub: &Hub) {
    hub.pool()
        .get()
        .unwrap()
        .execute_batch("ALTER TABLE device RENAME TO device_moved")
        .unwrap();
}

/// 故障注入：让「Hub 是否已初始化」这个问题本身**读不出答案**。
///
/// `is_initialized` 读的是 `secret` 表，所以要单独破坏 `secret`
/// （`break_device_lookup` 动的是另一张表）。对应 `hub_is_initialized`
/// 的取值规则：**读不出来按「未初始化」处理**——宁可拒绝 enroll，
/// 也不能在「不知道」的状态下放行（§3.1）。
pub fn break_secret_table(hub: &Hub) {
    hub.pool()
        .get()
        .unwrap()
        .execute_batch("DROP TABLE secret")
        .unwrap();
}

impl Drop for Hub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// 裸 tungstenite 连上去，并断言确实升级到 101。
pub async fn dial(url: &str) -> Ws {
    let (ws, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(resp.status(), 101u16, "must upgrade before anything else");
    ws
}

pub async fn send_text(ws: &mut Ws, t: &str) {
    use futures_util::SinkExt as _;
    let _ = ws.send(WsMsg::Text(t.into())).await;
}

/// 读下一个文本帧（跳过自动应答的 pong）。
pub async fn next_text(ws: &mut Ws) -> String {
    use futures_util::StreamExt as _;
    loop {
        let msg = ws.next().await.expect("hub must send a frame").unwrap();
        match msg {
            WsMsg::Text(t) => return t.to_string(),
            WsMsg::Pong(_) | WsMsg::Ping(_) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

pub async fn next_frame(ws: &mut Ws) -> Frame {
    let text = next_text(ws).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("unparsable json: {text} ({e})"))
}

/// 节点侧的 `hello`（§3：握手由节点先发起）。
pub const HELLO: &str = r#"{"v":1,"seq":0,"t":"hello","c":{"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}}"#;

/// 一份 enroll 成功后的连接。`node_id` 用 Hub 签发回来的那个。
pub async fn connect_enrolled(hub: &Hub) -> (Established, String, KeyPair) {
    let kp = KeyPair::generate();
    let est = connect_once(&hub.ws_url(), &kp, None, None, device_info())
        .await
        .expect("enroll must succeed");
    let node_id = est.auth_ok.node_id.clone();
    (est, node_id, kp)
}

/// 用**同一把钥匙**重连（重连形态：只带 node_id 与签名，不带公钥，§3）。
pub async fn connect_reconnect(hub: &Hub, kp: &KeyPair, node_id: &str) -> Established {
    connect_once(&hub.ws_url(), kp, Some(node_id), None, device_info())
        .await
        .expect("reconnect must succeed")
}

pub async fn send_heartbeat(ws: &mut Established, c: serde_json::Value) {
    ws.send(&Frame::push(types::HEARTBEAT, 0, c))
        .await
        .expect("heartbeat must send");
}

/// 等某台设备的连接从注册表里摘掉。
pub async fn wait_disconnected(hub: &Hub, node_id: &str, budget: Duration) {
    wait_until(budget, || hub.state.connections().get(node_id).is_none()).await;
}

/// 等到 `cond` 为真。轮询而不是固定 sleep：CI 慢时固定 sleep 会 flakes，
/// 而「条件成立」本身就是我们要等的事件。
pub async fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) {
    use std::time::Instant;
    let deadline = Instant::now() + budget;
    loop {
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "等待超时（{budget:?}）");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
