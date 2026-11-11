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

// ---------------------------------------------------------------------------
// v0.7.4 设备签名导入的夹具
// ---------------------------------------------------------------------------

/// 设备侧收到的一条 `open_channel`（connector + params 原样留下）。
#[derive(Debug, Clone)]
pub struct OpenChannelCall {
    pub connector: String,
    pub params: serde_json::Value,
}

/// 一台**在线**的桩设备：保持 WS 连接（`device_summary` 才会给 `online`），
/// 记录 `open_channel`，并对 `git` 的 `rev` 探测回一个可配置的结果。
///
/// `Established` 被 move 进任务里：`DeviceStub` 一 drop，任务 abort，连接断开。
/// 用例因此不必手动保活，也不会出现「Hub 已 drop 但设备还连着」的跨用例串扰。
pub struct DeviceStub {
    pub node_id: String,
    pub kp: KeyPair,
    pub open_calls: std::sync::Arc<std::sync::Mutex<Vec<OpenChannelCall>>>,
    pub rev_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DeviceStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// enroll 一台设备、上报能力、心跳，并起一个应答 `open_channel` / `rev` 的桩。
///
/// `rev_reply` 是 `git` 的 `rev` 结果帧（`None` = 不回，模拟设备卡住）。
pub async fn enroll_device(
    hub: &Hub,
    connectors: &[&str],
    agents: &[&str],
    rev_reply: Option<serde_json::Value>,
) -> DeviceStub {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    let (mut est, node_id, kp) = connect_enrolled(hub).await;
    let agent_reports: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| serde_json::json!({"id": a, "name": a, "version": "0.0.1", "available": true}))
        .collect();
    est.send(&Frame::push(
        types::HEARTBEAT,
        0,
        serde_json::json!({
            "node_version": "0.1.0",
            "capabilities": {"connectors": connectors, "agents": agent_reports},
            "resources": null,
            "workspace_roots": [],
        }),
    ))
    .await
    .expect("heartbeat must send");
    wait_until(Duration::from_secs(2), || {
        hub.state.reports().get(&node_id).is_some()
    })
    .await;

    let open_calls = Arc::new(Mutex::new(Vec::new()));
    let rev_count = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&open_calls);
    let revs = Arc::clone(&rev_count);
    let task = tokio::spawn(async move {
        let mut est = est;
        while let Some(text) = est.next_text().await.unwrap_or_default() {
            let Ok(f) = serde_json::from_str::<Frame>(&text) else {
                continue;
            };
            match f.msg_type.as_str() {
                types::OPEN_CHANNEL => {
                    let Ok(open) = f.payload::<protocol::channel::OpenChannel>() else {
                        continue;
                    };
                    calls.lock().unwrap().push(OpenChannelCall {
                        connector: open.connector.clone(),
                        params: open.params.clone(),
                    });
                    est.send(&Frame::push(
                        types::CHANNEL_READY,
                        0,
                        serde_json::json!({"channel_id": open.channel_id}),
                    ))
                    .await
                    .ok();
                }
                types::CHANNEL_DATA => {
                    let Ok(data) = f.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    if data.payload.get("op").and_then(serde_json::Value::as_str) != Some("rev") {
                        continue;
                    }
                    revs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(reply) = rev_reply.clone() {
                        est.send(&Frame::push(
                            types::CHANNEL_DATA,
                            0,
                            serde_json::json!({
                                "channel_id": data.channel_id,
                                "payload": reply,
                            }),
                        ))
                        .await
                        .ok();
                    }
                }
                _ => {}
            }
        }
    });

    DeviceStub {
        node_id,
        kp,
        open_calls,
        rev_count,
        task,
    }
}

/// 一条「已初始化 Hub + 在线 opencode 设备 + 可用路由」的导入夹具。
///
/// 14 个端点用例都从这串序列起步（`Hub::boot` → `enroll_device` →
/// `build_app`）；把它收在一处，新增错误码用例就不必再抄一遍。返回
/// `(Hub, DeviceStub, Router)`：`Hub` 与 `DeviceStub` 必须活到请求打完，
/// 否则临时库 / WS 连接会提前消失。
pub async fn boot_online_import_fixture() -> (Hub, DeviceStub, axum::Router) {
    use serde_json::json;

    let hub = Hub::boot().await;
    let stub = enroll_device(
        &hub,
        &["git"],
        &["opencode"],
        Some(json!({
            "op": "rev",
            "not_a_repo": false,
            "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "short": "aaaaaaa",
            "branch": "main",
            "detached": false,
        })),
    )
    .await;
    let app = hub_server::build_app(hub.state.clone());
    (hub, stub, app)
}

/// 一次已签名的导入请求：`body` 是最终字节，`request()` 把它装成带头的请求。
///
/// 签名算法与 `node-cli::import_http` 相同（canonical body = 两个签名字段置空
/// 后的 DTO 序列化；`identity::import_signing_message` 唯一实现）。
pub struct SignedImport {
    pub parsed: hub_server::routes::session_import::ImportRequest,
    pub body: Vec<u8>,
}

impl SignedImport {
    pub fn request(&self) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(hub_server::import_auth::IMPORT_PATH)
            .header("content-type", "application/json")
            .header("X-AF-Node-Id", &self.parsed.node_id)
            .header("X-AF-Nonce", &self.parsed.nonce)
            .header("X-AF-Signature", &self.parsed.signature)
            .header("X-AF-Body-SHA256", &self.parsed.body_sha256)
            .body(axum::body::Body::from(self.body.clone()))
            .expect("build request")
    }
}

/// 构造并签名一条当前协议版本的导入请求。
///
/// `t` / `nonce` 由调用方给（重放 / 越窗用例要控）。
#[allow(clippy::too_many_arguments)]
pub fn signed_import(
    kp: &KeyPair,
    node_id: &str,
    provider: &str,
    acp_session_id: &str,
    cwd: &str,
    title: &str,
    t: u64,
    nonce: &str,
) -> SignedImport {
    signed_import_with_version(
        kp,
        node_id,
        provider,
        acp_session_id,
        cwd,
        title,
        t,
        nonce,
        hub_server::import_auth::IMPORT_VERSION,
    )
}

/// 构造并签名一条**指定 `v`** 的导入请求。
///
/// 只有版本契约用例应该改 `v`：签名规范串与 body 摘要都用调用方给的值，
/// 以便断言 Hub 真的把 body 的版本绑进验签，而不是拿自己的常量顶替。
#[allow(clippy::too_many_arguments)]
pub fn signed_import_with_version(
    kp: &KeyPair,
    node_id: &str,
    provider: &str,
    acp_session_id: &str,
    cwd: &str,
    title: &str,
    t: u64,
    nonce: &str,
    v: u32,
) -> SignedImport {
    use sha2::{Digest, Sha256};

    let mut parsed = hub_server::routes::session_import::ImportRequest {
        node_id: node_id.to_string(),
        provider: provider.to_string(),
        external_session_id: acp_session_id.to_string(),
        acp_session_id: acp_session_id.to_string(),
        cwd: cwd.to_string(),
        title: title.to_string(),
        v,
        t,
        nonce: nonce.to_string(),
        signature: String::new(),
        body_sha256: String::new(),
    };
    let canonical = serde_json::to_vec(&parsed).expect("canonical body");
    let body_sha256 = identity::hex_encode(Sha256::digest(&canonical).as_ref());
    let message = identity::import_signing_message(
        parsed.v,
        &parsed.node_id,
        hub_server::import_auth::IMPORT_METHOD,
        hub_server::import_auth::IMPORT_PATH,
        &body_sha256,
        &parsed.nonce,
        parsed.t,
    );
    parsed.signature = identity::base64url(&kp.sign(&message));
    parsed.body_sha256 = body_sha256;
    let body = serde_json::to_vec(&parsed).expect("signed body");
    SignedImport { parsed, body }
}

/// 当前 Unix 秒（用例构造 `t` 用；Hub 只接受 ±300 s）。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 打一次 `oneshot`，返回状态码与 body 文本。
pub async fn send(
    app: &axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, String) {
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
    let resp = app.clone().oneshot(req).await.expect("app responds");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// `session` 表当前行数（去重 / 半截会话用例的判据）。
pub fn session_count(hub: &Hub) -> i64 {
    hub.pool()
        .get()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
        .unwrap()
}

/// 一条会话的 `(agent_id, acp_session_id, base_rev_status)`。
pub fn session_row(hub: &Hub, id: &str) -> (Option<String>, Option<String>, String) {
    hub.pool()
        .get()
        .unwrap()
        .query_row(
            "SELECT agent_id, acp_session_id, base_rev_status FROM session WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// 登录拿 `(af_session, af_csrf)` 两条 cookie 值。
pub async fn login(hub: &Hub) -> (String, String) {
    use tower::ServiceExt as _;
    let app = hub_server::build_app(hub.state.clone());
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"password": "correct horse battery"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("login responds");
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "登录必须成功");
    let mut session = None;
    let mut csrf = None;
    for v in resp.headers().get_all("set-cookie") {
        let raw = v.to_str().unwrap();
        let Some((k, rest)) = raw.split_once('=') else {
            continue;
        };
        let val = rest.split(';').next().unwrap_or_default().to_string();
        if k == hub_server::SESSION_COOKIE {
            session = Some(val);
        } else if k == hub_server::CSRF_COOKIE {
            csrf = Some(val);
        }
    }
    (session.expect("af_session"), csrf.expect("af_csrf"))
}

/// 一个带浏览器 cookie 的请求（CSRF 头按需加）。
pub fn browser_request(
    method: &str,
    uri: &str,
    session: &str,
    csrf: &str,
    with_csrf: bool,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "cookie",
            format!(
                "{}={}; {}={}",
                hub_server::SESSION_COOKIE,
                session,
                hub_server::CSRF_COOKIE,
                csrf
            ),
        );
    if with_csrf {
        b = b.header(hub_server::CSRF_HEADER, csrf);
    }
    let payload = body.map_or_else(String::new, |v| v.to_string());
    b.body(axum::body::Body::from(payload))
        .expect("build request")
}
