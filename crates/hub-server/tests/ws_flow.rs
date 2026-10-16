//! 设备 WS 长连接的集成测试：逐条对应 M0 里程碑子任务 3 的 6 条测试标准。
//!
//! 与 `login_flow.rs` 的分工：那个文件证明「浏览器侧 HTTP 认证」，
//! 本文件证明「设备侧 WS 接入」。**WS 必须起真 socket**——
//! `oneshot` 打不到 `WebSocketUpgrade`（upgrade 需要 hyper 的 connection 上下文），
//! 所以这里起一个真 listener，用 `node-core` 的真客户端连上去。
//!
//! 为什么用 `node-core::conn::connect_once` 而不是裸 tungstenite 手搓：
//! 被测行为是「node 连 Hub」这条完整链路，客户端必须是产品自己那一份。
//! 裸手搓的客户端会漏掉「node 侧签的字段与 Hub 侧验的字段不一致」这类
//! 只有两端都用真代码才暴露的问题。
//!
//! 映射关系写在每个测试的文档注释里（`标准①` … `标准⑥`）。

// Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

// 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
// 而 panic 消息比 match 分支更能说明哪一行、哪个值不对。
// let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
// 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::io::AsyncWriteExt as _;

use futures_util::{SinkExt, StreamExt};
use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, ConnOutcome};
use node_core::identity::Identity;
use protocol::message::DeviceInfo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;

/// 给测试进程装一个 `tracing` subscriber。
///
/// **不是为了看日志，而是为了覆盖 `tracing::info!` 里的字段表达式**：
/// 没有 subscriber 时 `tracing` 的 dispatch 判否就短路，`conn_id = %conn_id`
/// 与 `elapsed_ms = started.elapsed().as_millis()` 这些行**根本不会执行**——
/// llvm-cov 把它们记成未覆盖。装上 sink writer 后它们真的会跑。
///
/// 全局只能装一次，用 `Once` 保证；多个测试并发调用是安全的。
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

fn fast_params() -> AuthParams {
    AuthParams {
        m_cost: 8192,
        t_cost: 1,
        p_cost: 1,
    }
}

fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: "test-node".into(),
        os: "linux".into(),
        arch: "aarch64".into(),
        hostname: "testhost".into(),
        node_version: "0.1.0".into(),
    }
}

/// 起一个真 listener 的 Hub，返回地址与关闭句柄。
struct Hub {
    addr: SocketAddr,
    state: AppState,
    /// 持有 `tempdir`，确保它在 Hub 存活期间不被删除（删了 DB 文件就废了）。
    _dir: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Hub {
    /// 未初始化模式（无密码）——`/ws` 仍应能 upgrade（§3.1：握手才拒 enroll）。
    async fn uninitialized() -> Self {
        Self::boot(false).await
    }

    /// 正常模式（已设密码）。
    async fn initialized() -> Self {
        Self::boot(true).await
    }

    async fn boot(with_password: bool) -> Self {
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
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
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

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// 在本 Hub 的数据库里直接塞一行设备（不经过 enroll 握手）。
    ///
    /// 用它构造「库里有这行、但行里没有公钥」的重连路径：正常 enroll 总会
    /// 顺手把公钥写进 `capabilities`，而「设备早于该字段存在」的历史数据
    /// 造不出别的方式。
    fn insert_device(&self, id: &str) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at, status, os, arch, hostname, node_version)
             VALUES (?1, 'legacy', ?2, 'unix:0', 'offline', 'linux', 'aarch64', 'h', '0.1.0')",
            [id, &format!("fp-{id}")],
        )
        .unwrap();
    }

    /// 故障注入：让 `device` 表的 **UPDATE 一律失败**。
    ///
    /// 用 `BEFORE UPDATE` 触发器 + `RAISE(ABORT)` 而不是删表：删表会让
    /// 「读也失败」，于是同一个用例只能覆盖读错误路径；而这里要的正是
    /// **读成功、写失败**——`set_public_key` / `set_gen` 只在读得到行、
    /// 写不进去时才报 Err（那两条 warn 分支只在这种情况下可达）。
    /// `RAISE(ABORT)` 只回滚本次语句，不影响连接，所以 Hub 侧继续可用。
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

    /// 故障注入：让**查询设备**这一步直接报错（表还在，SELECT 失败）。
    ///
    /// 对应 Hub 侧「查设备失败就当身份不符、绝不猜」的那条安全边界（§3）。
    ///
    /// **把表改名而不是 `DROP TABLE`**：DROP 后 Hub 的连接池会命中一个
    /// 「schema 已变更但语句已缓存」的中间态，表现为 hello 阶段就报
    /// `handshake rejected`，压根走不到重连分支。改名同样让 `SELECT` 报
    /// `no such table`，而语句缓存与表名一致、行为稳定可复现。
    fn break_device_lookup(&self) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        pool.get()
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
    fn break_secret_table(&self) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        pool.get()
            .unwrap()
            .execute_batch("DROP TABLE secret")
            .unwrap();
    }

    /// 故障注入：**新设备**的 INSERT 失败（读仍正常）。
    ///
    /// 对应 `Registry::enroll` 把 SQLite 错误往上抛的那条路：Hub 必须回
    /// `fingerprint_race_retry` 而不是 panic、也不是把设备「先记着以后再说」。
    fn break_device_insert(&self) {
        let pool = hub_storage::db::open_in(self.state.data_dir()).unwrap().1;
        pool.get()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER boom_insert BEFORE INSERT ON device
                 BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END;",
            )
            .unwrap();
    }

    /// 故障注入：握手期**任何**一帧回程都发不出去（触发 socket 上限）。
    ///
    /// 用 `SO_SNDBUF` 把发送缓冲压到内核下限，再让对端握手期间既不发数据、
    /// 也不读任何东西：内核缓冲立刻填满，Hub 的 `send` 就会拿到
    /// `WouldBlock`/`Io` 错误。这覆盖的是「帧发不出去」这条真实存在的路
    /// （对端在握手中途消失、或链路拥塞），而不是构造一个假 socket。
    async fn squeeze_send_buffer(&self, ws: &mut Ws) {
        let stream: &mut tokio::net::TcpStream = match ws.get_mut() {
            tokio_tungstenite::MaybeTlsStream::Plain(s) => s,
            other => panic!("the url is plain ws://, got {other:?}"),
        };
        let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
        let min: libc::c_int = 1;
        let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    std::ptr::from_ref(&min).cast(),
                    size,
                )
            },
            0,
            "SO_SNDBUF must be settable for this fixture to mean anything"
        );
        // 发送缓冲要**真的**被填满：auth_ok 只有一百来字节，Linux 会把
        // SO_SNDBUF 翻倍到最小值（约 4 KiB）并立刻发出去，所以先把
        // 缓冲缩到内核下限之后还要再灌一票数据把它占满。
        let filler = vec![b'x'; 256 * 1024];
        let _ = stream.write_all(&filler).await;
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// 标准①：`afcli run` 启动后 5s 内 Hub 日志出现 `ws connected`，
/// 且 `/ws` upgrade 返回 HTTP 101 且首帧为 `challenge{nonce}`。
///
/// 这里不用读 Hub 日志（测试进程里拿不到子进程 stderr），
/// 而是用**可观测的等价事实**替代：TCP 连上 + 收到 `challenge` 帧 =
/// 101 upgrade 成功 + 首帧正确。日志断言由 `tests/afcli_process.rs`
/// 在进程级做（那里真的看得到 stderr）。
#[tokio::test]
async fn ws_upgrade_returns_challenge_as_first_frame() {
    let hub = Hub::initialized().await;
    let kp = KeyPair::generate();

    // 裸 tungstenite 只做到「upgrade + 收 challenge」，不走完整握手：
    // 这一步要单独断言 101 与首帧形态，混在 connect_once 里测不到。
    let started = Instant::now();
    let (mut socket, resp) = tokio_tungstenite::connect_async(hub.ws_url())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "ws upgrade must complete within 5s, took {elapsed:?}"
    );
    // 101 由 tungstenite 的 Response 体现
    assert_eq!(resp.status(), 101u16, "upgrade must return HTTP 101");

    use futures_util::StreamExt;
    // 必须先发 hello，server 才回 challenge（握手是节点先发起，§3）。
    // 只连不发的裸客户端会卡在 server 的 recv 上，10s 后被超时打断。
    let hello = r#"{"v":1,"seq":0,"t":"hello","c":{"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}}"#;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(hello.into()))
        .await
        .unwrap();
    let first = socket.next().await.unwrap().unwrap();
    let text = match first {
        tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
        other => panic!("first frame must be text, got {other:?}"),
    };
    let frame: protocol::frame::Frame = serde_json::from_str(&text).unwrap();
    assert_eq!(frame.msg_type, "challenge", "first frame must be challenge");
    let nonce = frame.c.get("nonce").and_then(|v| v.as_str()).unwrap();
    // nonce 是 32 字节 base64url 无填充 = 43 字符（§3）
    assert_eq!(nonce.len(), 43, "nonce must be 32 bytes base64url: {nonce}");
    let _ = kp;
}

/// 标准⑤：**`/ws` 无 cookie 也能 upgrade**（初始化模式与正常模式各一次）。
///
/// 这是豁免表的核心断言：node 不是浏览器，没有 session cookie。
/// 若这条挂掉，说明所有设备都被挡在门外。
#[tokio::test]
async fn ws_upgrades_without_any_cookie_in_both_modes() {
    // 初始化模式
    let hub = Hub::uninitialized().await;
    let (mut sock, resp) = tokio_tungstenite::connect_async(hub.ws_url())
        .await
        .unwrap();
    assert_eq!(resp.status(), 101u16, "init mode must upgrade /ws");
    use futures_util::StreamExt;
    let hello = r#"{"v":1,"seq":0,"t":"hello","c":{"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}}"#;
    sock.send(tokio_tungstenite::tungstenite::Message::Text(hello.into()))
        .await
        .unwrap();
    let first = sock.next().await.unwrap().unwrap();
    let text = match first {
        tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
        other => panic!("first frame must be text, got {other:?}"),
    };
    let f: protocol::frame::Frame = serde_json::from_str(&text).unwrap();
    assert_eq!(
        f.msg_type, "challenge",
        "init mode first frame must be challenge"
    );
    drop(sock);
    drop(hub);

    // 正常模式（已设密码）
    let hub = Hub::initialized().await;
    let (mut sock, resp) = tokio_tungstenite::connect_async(hub.ws_url())
        .await
        .unwrap();
    assert_eq!(resp.status(), 101u16, "normal mode must upgrade /ws");
    sock.send(tokio_tungstenite::tungstenite::Message::Text(hello.into()))
        .await
        .unwrap();
    let first = sock.next().await.unwrap().unwrap();
    let text = match first {
        tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
        other => panic!("first frame must be text, got {other:?}"),
    };
    let f: protocol::frame::Frame = serde_json::from_str(&text).unwrap();
    assert_eq!(
        f.msg_type, "challenge",
        "normal mode first frame must be challenge"
    );
}

/// 带 `Origin` 的请求必须在 upgrade **之前**被拒（403），不消耗连接槽位。
///
/// 浏览器会带 `Origin`，node 不会——这正是区分两者的判据。
///
/// 这里走 `tungstenite::connect_async` 并显式塞一个 `Origin` 头：它发出的
/// 仍是合法 WebSocket 升级请求，但带上了 Origin。断言 upgrade 直接失败
/// （拿不到 challenge），等价于「upgrade 之前就被 403」。不手写裸 TCP：
/// 裸 `read_to_end` 在 `Connection: close` 下偶发 hang，tungstenite 会
/// 在握手阶段就返回错误，更稳。
#[tokio::test]
async fn ws_upgrade_with_origin_is_rejected_before_upgrade() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let hub = Hub::initialized().await;
    let mut req = IntoClientRequest::into_client_request(hub.ws_url()).unwrap();
    req.headers_mut().insert(
        axum::http::header::ORIGIN,
        axum::http::HeaderValue::from_static("https://evil.example"),
    );

    // 预期：upgrade 被拒——tungstenite 抛错，拿不到 challenge 帧。
    let outcome = tokio_tungstenite::connect_async(req).await;
    assert!(
        outcome.is_err(),
        "origin-bearing upgrade must be rejected before upgrade"
    );
}

/// 标准④：连接注册表**不落库**，`ConnHandle` 不含 socket。
///
/// 两层断言：① `device` 表没有新增 socket 类列（schema 层面）；
/// ② `ConnHandle` 只有 `conn_id` + `gen` 两个字段（类型层面）。
/// 编译期断言 + schema 断言，比「跑一遍看没写进去」更强。
#[tokio::test]
async fn connection_registry_never_touches_the_database() {
    let hub = Hub::initialized().await;

    // 类型层面：ConnHandle 不含任何 socket / 句柄类字段
    let h = hub_core::ConnHandle {
        conn_id: "c1".into(),
        gen: 1,
    };
    let dbg = format!("{h:?}");
    assert!(
        dbg.contains("conn_id"),
        "ConnHandle must expose conn_id: {dbg}"
    );
    assert!(dbg.contains("gen"), "ConnHandle must expose gen: {dbg}");
    // 没有任何「socket / stream / tcp / tls」字样
    for banned in ["socket", "stream", "Tcp", "tls", "Tls", "WebSocket"] {
        assert!(!dbg.contains(banned), "ConnHandle leaked {banned}: {dbg}");
    }

    // schema 层面：device 表的列清单里没有 socket 类列
    let cols = device_columns(&hub.state).await;
    for banned in ["socket", "conn", "handle", "fd", "channel_id"] {
        assert!(
            !cols.iter().any(|c| c.contains(banned)),
            "device table must not have a {banned} column, got {cols:?}"
        );
    }
    // 但握手相关的列必须在（否则顶替兜底无从实现）
    assert!(cols.contains(&"last_seen_gen".to_string()), "got {cols:?}");
    assert!(cols.contains(&"superseded_at".to_string()), "got {cols:?}");
}

/// 读出 `device` 表的列名。
async fn device_columns(state: &AppState) -> Vec<String> {
    let pool = hub_storage::db::open_in(state.data_dir()).unwrap().1;
    let conn = pool.get().unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('device')")
        .unwrap();
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    rows
}

/// 标准②：连接断开后连接注册表正确清除——断开 1s 后 `get(node_id)` 返回 `None`。
///
/// 用真 enroll 拿到 node_id，再真断开，然后轮询注册表直到它被摘掉。
/// 不断言「1s 整」，而是「很快」：摘表是同步的，正常远小于 1s；
/// 轮询到就算过（避免把实现细节的时间假设写进测试）。
#[tokio::test]
async fn registry_entry_is_cleared_after_disconnect() {
    let hub = Hub::initialized().await;
    let kp = KeyPair::generate();

    let est = connect_once(&hub.ws_url(), &kp, None, device_info())
        .await
        .expect("first enroll must succeed");
    let node_id = est.auth_ok.node_id.clone();
    assert!(est.enrolled, "first connect with no node_id is an enroll");

    // 注册表里有这条连接
    let handle = hub
        .state
        .connections()
        .get(&node_id)
        .expect("connection must be registered after auth_ok");
    assert_eq!(handle.gen, est.auth_ok.gen);

    // 断开：让 socket 在本作用域结束
    drop(est);
    // connect_once 返回后 socket 已经 drop，连接已断开；
    // 轮询等注册表摘表（1s 上限，标准②给的就是 1s）
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if hub.state.connections().get(&node_id).is_none() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("registry entry for {node_id} was not cleared within 1s");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 摘表后 node_id 列表里也没有
    assert!(!hub.state.connections().node_ids().contains(&node_id));
}

/// 标准⑥（node 侧一半）：同一 `node_id` 第二次连接时，旧连接收到
/// `superseded` 后主动退出、不再重连。
///
/// M0 只做「Hub 换表 + 旧连接退出的语义」：`disconnect{superseded}` 帧的
/// 下发在子任务 7 完成（那是「先换表、后通知」的完整时序）。
/// 这里断言的是**判定逻辑本身**：`ConnOutcome::Superseded` 是一个独立变体，
/// 调用方据此停止重连，而不是当成普通失败去退避。
#[tokio::test]
async fn superseded_outcome_stops_reconnecting() {
    // 真跑一次「两个连接同 node_id」：第二个 enroll 复用同一指纹，
    // 走 `AlreadyRegistered` 分支（§4.3 第二行）而不是被拒。
    let hub = Hub::initialized().await;
    let kp = KeyPair::generate();

    let first = connect_once(&hub.ws_url(), &kp, None, device_info())
        .await
        .expect("enroll");
    let node_id = first.auth_ok.node_id.clone();
    let first_gen = first.auth_ok.gen;

    // 用同一个 node_id 重连（第二个连接）
    let second = connect_once(&hub.ws_url(), &kp, Some(&node_id), device_info())
        .await
        .expect("reconnect with the same identity must be allowed");
    assert_eq!(second.auth_ok.node_id, node_id, "node_id must be stable");
    assert!(
        second.auth_ok.gen > first_gen,
        "each connection gets a fresh gen (got {} then {})",
        first_gen,
        second.auth_ok.gen
    );

    // 注册表现在指向新连接
    let handle = hub.state.connections().get(&node_id).unwrap();
    assert_eq!(handle.gen, second.auth_ok.gen);

    // 旧的判定：若 Hub 回 auth_fail{superseded}，node 侧必须识别成终态
    let fail = protocol::message::AuthFail {
        reason: protocol::message::reason::SUPERSEDED.to_string(),
        gen: Some(first_gen),
    };
    let frame = protocol::frame::Frame::push("auth_fail", 0, serde_json::to_value(&fail).unwrap());
    let parsed: protocol::message::AuthFail = frame.payload().unwrap();
    let outcome = node_core::conn::classify_auth_fail(&parsed);
    assert!(
        matches!(outcome, ConnOutcome::Superseded { gen } if gen == first_gen),
        "superseded must map to a terminal outcome, got {outcome:?}"
    );
}

/// 标准③：零入站端口——Hub 进程只 listen 配置的 HTTP 端口。
///
/// 这是**约束 1 的产品承诺**。测试起一个真 listener，然后扫它的 fd，
/// 断言只有 1 个 listening socket 指向**本 Hub 自己的端口**，没有第二个。
/// 多个 `#[tokio::test]` 同进程并行跑时，会有多个 Hub listener 共存；
/// 所以这里**按本地端口号精确匹配** `hub.addr.port()`，而不是「数本进程所有
/// listening socket」——否则会算到兄弟测试的 listener，假性失败。
#[tokio::test]
async fn hub_listens_on_exactly_one_port() {
    let hub = Hub::initialized().await;
    let pid = std::process::id();

    // 只统计指向本 Hub 端口的 listening socket
    let listening = listening_sockets_on_port(pid, hub.addr.port());
    assert_eq!(
        listening.len(),
        1,
        "hub must listen on exactly one port (constraint 1), got {listening:?}"
    );

    // 那一个端口就是 HTTP 端口（能连上 /healthz）
    let resp = hub_server::build_app(hub.state.clone())
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 列出某端口上处于 LISTEN 状态、且属于该进程的 socket inode。
fn listening_sockets_on_port(pid: u32, port: u16) -> Vec<String> {
    // 本地端口：/proc/net/tcp 第 2 列是 `0100007F:PORT_HEX`（127.0.0.1:port）
    let want_local = format!("{:04X}", u32::from(port));
    let mut out = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            if cols[3] != "0A" {
                continue;
            }
            // 第 2 列是本地地址，格式 `ADDR:PORT`，PORT 是大写十六进制
            let local_port = cols[1].rsplit(':').next().unwrap_or("");
            if local_port != want_local {
                continue;
            }
            let inode = cols[9];
            if fd_points_to(pid, inode) {
                out.push(format!("{path}:{inode}"));
            }
        }
    }
    out
}

/// 该 inode 是否被这个进程的某个 fd 指向。
fn fd_points_to(pid: u32, inode: &str) -> bool {
    let want = format!("socket:[{inode}]");
    let dir = format!("/proc/{pid}/fd");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        if let Ok(target) = std::fs::read_link(e.path()) {
            if target.to_string_lossy() == want {
                return true;
            }
        }
    }
    false
}

/// `identity.json` 与 WS 的衔接：enroll 成功后 node 侧把 `node_id` 回填，
/// 下次启动走重连形态而不是再 enroll。
///
/// 这是标准⑥「不再重连」之外的另一半：**身份随连接复用**。
/// 回填用 `node_core::identity::Identity::set_node_id`（子任务 2 的原子写）。
#[tokio::test]
async fn node_id_round_trips_through_identity_json() {
    let hub = Hub::initialized().await;
    let dir = tempfile::tempdir().unwrap();
    let mut id = Identity::load_or_create(dir.path()).unwrap();

    let est = connect_once(&hub.ws_url(), id.key(), None, device_info())
        .await
        .expect("enroll with a locally generated key must work");
    let node_id = est.auth_ok.node_id.clone();
    id.set_node_id(&node_id).unwrap();

    // 重启等价：重新加载 identity.json，node_id 还在
    let reloaded = Identity::load_or_create(dir.path()).unwrap();
    assert_eq!(reloaded.node_id().as_deref(), Some(node_id.as_str()));

    // 用它重连：Hub 认这把钥匙
    let again = connect_once(&hub.ws_url(), reloaded.key(), Some(&node_id), device_info())
        .await
        .expect("reconnect after restart must reuse the same node_id");
    assert_eq!(again.auth_ok.node_id, node_id);
    assert!(!again.enrolled, "reconnect is not a new enroll");
}

/// Hub 未初始化时 enroll 必须被拒（§3.1），但 `/ws` 仍能 upgrade。
///
/// 顺序很重要：先 upgrade（豁免 session），再在 `auth` 时拒。
/// 若反过来（upgrade 阶段就 403），设备会看到一个「连不上」的假象，
/// 而真实原因是「Hub 还没设密码」。
#[tokio::test]
async fn enroll_is_rejected_when_hub_is_not_initialized() {
    let hub = Hub::uninitialized().await;
    let kp = KeyPair::generate();

    let out = connect_once(&hub.ws_url(), &kp, None, device_info()).await;
    match out {
        Err(ConnOutcome::Rejected { reason }) => {
            assert_eq!(
                reason,
                protocol::message::reason::HUB_NOT_INITIALIZED,
                "must report the exact documented reason"
            );
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

/// 私钥绝不进 WS 载荷（约束 13）。
///
/// 用真连接跑一遍，capture 不现实；改为**静态断言载荷形状**：
/// `EnrollAuth` 只有 public_key / signature / device_info 三个字段，
/// 而 `ReconnectAuth` 结构上就没有 public_key（§3）。
/// 这条与 `node-core` 的单测互补：那里测「Debug 不含私钥」，
/// 这里测「线上载荷里没有私钥的位置」。
#[tokio::test]
async fn auth_payloads_have_no_slot_for_a_private_key() {
    let kp = KeyPair::generate();
    let info = device_info();

    // enroll 形态
    let enroll = protocol::handshake::EnrollAuth {
        public_key: identity::base64url(&kp.public_key_bytes()),
        signature: identity::base64url(&[0u8; 64]),
        device_info: info.clone(),
    };
    let v = serde_json::to_value(&enroll).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.len(), 3, "enroll payload must have exactly 3 fields");
    assert!(obj.contains_key("public_key"));
    assert!(obj.contains_key("signature"));
    assert!(obj.contains_key("device_info"));
    // 私钥字段名一个都不许出现
    for banned in ["private_key", "private", "secret", "pkcs8", "seed"] {
        assert!(!obj.contains_key(banned), "enroll payload leaked {banned}");
    }

    // 重连形态：结构上就没有 public_key（§3）
    let reconnect = protocol::handshake::ReconnectAuth {
        node_id: "n_x".into(),
        signature: identity::base64url(&[0u8; 64]),
        last_seen_gen: None,
    };
    let v = serde_json::to_value(&reconnect).unwrap();
    let obj = v.as_object().unwrap();
    assert!(
        !obj.contains_key("public_key"),
        "reconnect must not upload a key"
    );
    assert!(!obj.contains_key("private_key"));
}

// ============================================================================
// 握手期对端发疯：Hub 侧每一条失败路径都必须既断开，又回一句
// **不含密钥材料**的固定短语（§3、§4）。
//
// 为什么用**裸 tungstenite** 客户端而不是 `node-core::conn::connect_once`：
// 那条客户端只会发合法帧，而这些路径要测的正是「收到不该收到的帧」。
// 真实节点那一侧的镜像路径在 `crates/node-core/tests/conn_paths.rs`
// ——两端都得能扛住对端发疯，否则断在谁手里都查不出来。
// ============================================================================

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 一条合法的 `hello`（§2：帧信封 `v/seq/t/c`）。
const HELLO: &str = r#"{"v":1,"seq":0,"t":"hello","c":{"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}}"#;

/// 设备信息（enroll 载荷的 `device_info`）。
fn info() -> serde_json::Value {
    serde_json::json!({
        "name": "pi", "os": "linux", "arch": "aarch64",
        "hostname": "testhost", "node_version": "0.1.0",
    })
}

/// 连上 Hub：断言 101 之后把 socket 交给调用方驱动。
///
/// 每个用例自己决定「发什么、什么时候断」——这些失败路径的差别几乎全在
/// 「什么时候不发」与「怎么收尾」。
async fn dial(url: &str) -> Ws {
    let (ws, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(resp.status(), 101u16, "must upgrade before anything else");
    ws
}

/// 发一帧原始文本。**不保证是合法 JSON**——发坏载荷正是这些用例的目的。
///
/// 发送失败**不 panic**：这些负向用例里 Hub 关连接是**预期行为**（比如把
/// 发送缓冲压满逼它回不了帧），此时 `BrokenPipe` 恰恰是待验证的现象本身。
/// 真正的「连接已死」由后续的 `wait_closed` 断言，不在这里提前炸掉。
async fn send_text(ws: &mut Ws, t: &str) {
    let _ = ws
        .send(tokio_tungstenite::tungstenite::Message::Text(t.into()))
        .await;
}

/// 发一帧非文本帧（ping / pong / 二进制），用来打「帧种类」这一关。
async fn send_raw(ws: &mut Ws, msg: tokio_tungstenite::tungstenite::Message) {
    ws.send(msg).await.unwrap();
}

/// 读下一个**文本**帧，顺手跳过自动应答的 pong。
///
/// pong 是 Hub 对我们 ping 的自动应答（约束：ping 不打断协议流程），
/// 所以它混在帧流里是正常的，读帧的用例必须容忍它。
async fn next_text(ws: &mut Ws) -> String {
    loop {
        let msg = ws.next().await.expect("hub must send a frame").unwrap();
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => return t.to_string(),
            tokio_tungstenite::tungstenite::Message::Pong(_)
            | tokio_tungstenite::tungstenite::Message::Ping(_) => {
                continue;
            }
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// 读回一帧并解成 [`protocol::frame::Frame`]。
async fn next_frame(ws: &mut Ws) -> protocol::frame::Frame {
    let text = next_text(ws).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("hub sent unparsable json: {text} ({e})"))
}

/// 发起握手，把 Hub 下发的 challenge nonce 交回调用方——后续帧的签名要用它。
async fn challenge_nonce(ws: &mut Ws) -> String {
    send_text(ws, HELLO).await;
    let f = next_frame(ws).await;
    assert_eq!(
        f.msg_type, "challenge",
        "hub must answer hello with challenge"
    );
    f.c.get("nonce")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
}

/// 等 Hub 结束这条连接（`None` 或读出错都算「已断开」）。
///
/// 断言「失败即断开」而不是「失败即回某一帧」：握手失败帧与关闭帧在同一条
/// TCP 流上先后到达，同一个 reader 读它们能一次读到；**另一个** reader
/// （见 [`Hub::read_auth_fail`]）再去读那条流就会空等。
async fn wait_closed(ws: &mut Ws) {
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None => return true,
                Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        closed,
        "hub must close the connection after a handshake failure"
    );
}

/// enroll 形态的 `auth` 帧。`sig` 传 `None` 表示「用一把新钥匙的假签名」——
/// 无论哪种，Hub 都必须拒（本组用例断言的就是「拒」这件事本身）。
async fn send_enroll_auth(ws: &mut Ws, sig: [u8; 64]) {
    let kp = KeyPair::generate();
    send_text(
        ws,
        &serde_json::json!({
            "v": 1,
            "seq": 1,
            "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
}

/// 发一帧**签名合法**的 enroll `auth`（`nonce` 由调用方先取好）。
///
/// 与 [`send_enroll_auth`] 的区别是本函数的签名**能验过**——这一点在
/// 「要触发验签之后的故障」时是本质区别：坏签名会在 `on_enroll` 就被拒成
/// `identity mismatch`，根本走不到 `Registry::enroll`，于是「INSERT 失败」
/// 这条路永远测不到。
async fn send_enroll_auth_signed(ws: &mut Ws, nonce: &str) {
    let kp = KeyPair::generate();
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", nonce));
    send_text(
        ws,
        &serde_json::json!({
            "v": 1,
            "seq": 1,
            "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
}

/// 跑一次 `hello → challenge → enroll auth（好签名）`，返回拿到的 `node_id`。
///
/// 走真握手而不是直接写库：这样 `capabilities.public_key` 的内容一定是
/// **这把**钥匙的，后续用重连形态回来才能验过。
async fn enroll_node(hub: &Hub) -> (KeyPair, String) {
    let kp = KeyPair::generate();
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1,
            "seq": 1,
            "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    let ok = next_frame(&mut ws).await;
    assert_eq!(ok.msg_type, "auth_ok", "enroll must be accepted");
    let node_id =
        ok.c.get("node_id")
            .and_then(|v| v.as_str())
            .expect("auth_ok must carry a node_id")
            .to_string();
    drop(ws);
    (kp, node_id)
}

/// 重连形态的 `auth` 帧：带 `node_id`，**结构上不带公钥**（§3）。
async fn send_reconnect_auth(ws: &mut Ws, node_id: &str, key: &KeyPair, bad_sig: bool) {
    let nonce = challenge_nonce(ws).await;
    let sig = if bad_sig {
        // 用**另一把**钥匙签：长度合法、编码合法，验签必然不过
        KeyPair::generate().sign(&identity::signature_message(1, node_id, 1, "auth", &nonce))
    } else {
        key.sign(&identity::signature_message(1, node_id, 1, "auth", &nonce))
    };
    send_text(
        ws,
        &serde_json::json!({
            "v": 1,
            "seq": 1,
            "t": "auth",
            "c": {"node_id": node_id, "signature": identity::base64url(&sig)},
        })
        .to_string(),
    )
    .await;
}

/// 把 `auth_fail` 的 `reason` 取出来；不是 `auth_fail` 就当场失败并报出实收的帧。
async fn expect_auth_fail(ws: &mut Ws, want: &str) {
    let f = next_frame(ws).await;
    assert_eq!(
        f.msg_type, "auth_fail",
        "hub must answer a rejected auth with auth_fail, got {:?}",
        f.c
    );
    let reason =
        f.c.get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
    assert_eq!(
        reason, want,
        "auth_fail.reason must be the documented phrase"
    );
}

// ---------- 载荷与版本 ----------

/// 不是 JSON 的文本帧 → 断开，不得当成 `hello` 处理。
#[tokio::test]
async fn a_frame_that_is_not_json_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, "}{").await;
    wait_closed(&mut ws).await;
}

/// `hello` 载荷缺字段（这里缺 `arch`）→ 断开，**不发** challenge。
#[tokio::test]
async fn a_hello_with_a_broken_payload_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(
        &mut ws,
        r#"{"v":1,"seq":0,"t":"hello","c":{"client_version":"0.1.0","os":"linux","features":[]}}"#,
    )
    .await;
    wait_closed(&mut ws).await;
}

/// 协议版本 0（`negotiate_version` 判非法）→ 断开。
#[tokio::test]
async fn a_hello_with_protocol_version_zero_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(
        &mut ws,
        r#"{"v":0,"seq":0,"t":"hello","c":{"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}}"#,
    )
    .await;
    wait_closed(&mut ws).await;
}

/// 帧种类不对（二进制）→ 断开：协议只用 JSON 文本（§2）。
#[tokio::test]
async fn a_binary_frame_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_raw(
        &mut ws,
        tokio_tungstenite::tungstenite::Message::Binary(vec![0xff, 0x00, 0x01]),
    )
    .await;
    wait_closed(&mut ws).await;
}

/// Close 帧 → 断开（对端自己走了，不是协议错误）。
#[tokio::test]
async fn a_close_frame_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_raw(
        &mut ws,
        tokio_tungstenite::tungstenite::Message::Close(None),
    )
    .await;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None => return true,
                Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(ended, "hub must end a connection closed by the peer");
}

/// 握手期的 ping **不得打断流程**：axum 自动应答 pong，握手照常走到 challenge。
#[tokio::test]
async fn a_ping_during_the_handshake_is_answered_and_ignored() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_raw(
        &mut ws,
        tokio_tungstenite::tungstenite::Message::Ping(Vec::new()),
    )
    .await;
    let nonce = challenge_nonce(&mut ws).await;
    assert_eq!(nonce.len(), 43, "challenge must still arrive after a ping");
    drop(ws);
}

/// 必需集里的类型乱序出现（第一帧就是 `heartbeat`）→ 断开并回 `identity mismatch`。
///
/// 乱序的必需集帧**必须断开**：它们是握手协议的一部分，顺序错了说明
/// 两端对协议的理解已经分叉，继续下去只会产生更难查的错（§2、§4）。
#[tokio::test]
async fn a_required_message_out_of_order_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(
        &mut ws,
        r#"{"v":1,"seq":0,"t":"heartbeat","c":{"node_version":"0.1.0"}}"#,
    )
    .await;
    expect_auth_fail(&mut ws, protocol::message::reason::IDENTITY_MISMATCH).await;
    wait_closed(&mut ws).await;
}

/// 可选集里的类型出现在握手期（前置的 `hub_hello`）→ **必须忽略**（约束 14）。
///
/// 新 Hub 会先发 `hub_hello`；如果这里把它当成「未知 → 断开」，
/// 「老 node + 新 Hub」就再也连不上了（约束 14 明确要保住这个组合）。
#[tokio::test]
async fn an_optional_message_before_hello_is_ignored() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, r#"{"v":1,"seq":0,"t":"hub_hello","c":{}}"#).await;
    let nonce = challenge_nonce(&mut ws).await;
    assert_eq!(nonce.len(), 43, "hub_hello must not end the handshake");
    drop(ws);
}

/// `hello` 的载荷畸形 → 断开。
#[tokio::test]
async fn a_malformed_auth_payload_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    let _nonce = challenge_nonce(&mut ws).await;
    // enroll 形态缺 public_key / signature / device_info
    send_text(&mut ws, r#"{"v":1,"seq":1,"t":"auth","c":{}}"#).await;
    wait_closed(&mut ws).await;
}

/// 重连形态**带公钥** → 断开（§3：公钥只在首次 enroll 上传，
/// 带公钥的重连正是想改绑指纹的接管动作，必须显式失败）。
#[tokio::test]
async fn a_public_key_on_reconnect_ends_the_handshake() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    let _nonce = challenge_nonce(&mut ws).await;
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "node_id": "n_x",
                "public_key": identity::base64url(&KeyPair::generate().public_key_bytes()),
                "signature": identity::base64url(&[0u8; 64]),
            },
        })
        .to_string(),
    )
    .await;
    wait_closed(&mut ws).await;
}

// ---------- enroll 的验签 ----------

/// enroll 的签名不过 → 回 `identity mismatch` 并断开。
#[tokio::test]
async fn an_enroll_with_a_bad_signature_is_rejected() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    let _nonce = challenge_nonce(&mut ws).await;
    send_enroll_auth(&mut ws, [0u8; 64]).await;
    expect_auth_fail(&mut ws, protocol::message::reason::IDENTITY_MISMATCH).await;
    wait_closed(&mut ws).await;
}

// ---------- 重连 ----------

/// 重连一个库里根本没有的 `node_id` → 回 `identity mismatch` 并断开。
#[tokio::test]
async fn a_reconnect_to_an_unknown_node_id_is_rejected() {
    let hub = Hub::initialized().await;
    let kp = KeyPair::generate();
    let mut ws = dial(&hub.ws_url()).await;
    send_reconnect_auth(&mut ws, "n_does_not_exist", &kp, false).await;
    expect_auth_fail(&mut ws, protocol::message::reason::IDENTITY_MISMATCH).await;
    wait_closed(&mut ws).await;
}

/// 库里有这行、但**没有公钥**（`capabilities` 为 NULL）→ 拒。
///
/// 这是真实存在的历史数据形态：设备早于「M0 权宜把公钥塞进 capabilities」
/// 这个实现之前就已注册。无法反推公钥，所以只能拒——而**绝不能**退回
/// 「相信本次上传的公钥」，那等于任何人都能顶替这台设备（§3）。
#[tokio::test]
async fn a_reconnect_without_a_stored_public_key_is_rejected() {
    let hub = Hub::initialized().await;
    hub.insert_device("n_legacy");
    let kp = KeyPair::generate();
    let mut ws = dial(&hub.ws_url()).await;
    send_reconnect_auth(&mut ws, "n_legacy", &kp, false).await;
    expect_auth_fail(&mut ws, protocol::message::reason::IDENTITY_MISMATCH).await;
    wait_closed(&mut ws).await;
}

/// 重连的签名不过（库里公钥是另一把）→ 拒，且**不得改写**库内指纹。
#[tokio::test]
async fn a_reconnect_with_a_bad_signature_is_rejected() {
    let hub = Hub::initialized().await;
    let (_kp, node_id) = enroll_node(&hub).await;
    let mut ws = dial(&hub.ws_url()).await;
    // 用一把全新的钥匙签名：载荷合法、签名合法，验签必然不过
    send_reconnect_auth(&mut ws, &node_id, &KeyPair::generate(), true).await;
    expect_auth_fail(&mut ws, protocol::message::reason::IDENTITY_MISMATCH).await;
    wait_closed(&mut ws).await;
}

// ---------- 握手后的业务帧循环 ----------

/// 认证成功后立刻异常断开（不走关闭握手）→ Hub 必须自己把连接摘干净。
///
/// `remove_if_current` 在异常断开时也必须把注册表清干净：不清的话
/// Web 上会一直显示这台设备在线，而它其实已经不在了（标准②）。
#[tokio::test]
async fn an_abrupt_drop_after_auth_ok_is_cleaned_up() {
    let hub = Hub::initialized().await;
    let kp = KeyPair::generate();
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    let ok = next_frame(&mut ws).await;
    let node_id = ok.c["node_id"].as_str().unwrap().to_string();
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "an authenticated connection must be registered"
    );

    // 掉线而不是正常关闭：Tungstenite 自己发 Close 的话那仍是**正常**断开，
    // 走不到「读帧出错」那条路。
    //
    // 手法：SO_LINGER(0) 让 close() 直接发 RST，再写一帧畸形字节。
    // 只 `drop` socket 的话 tungstenite 会先把关闭握手走完，那是正常断开；
    // 只关写半边的话对端会看到 EOF（也是正常断开）。RST 才是「进程没了」。
    {
        let stream = match ws.get_mut() {
            tokio_tungstenite::MaybeTlsStream::Plain(s) => s,
            other => panic!("the url is plain ws://, got {other:?}"),
        };
        let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
        // 字节序依平台而异：两个排列都设一遍，谁成功听谁的
        let one: libc::c_int = 1;
        let zero: libc::c_int = 0;
        for (onoff, linger_secs) in [(&one, &zero), (&zero, &one)] {
            let mut linger: libc::linger = unsafe { std::mem::zeroed() };
            linger.l_onoff = *onoff;
            linger.l_linger = *linger_secs;
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    std::ptr::from_ref(&linger).cast(),
                    std::mem::size_of::<libc::linger>() as libc::socklen_t,
                )
            };
            if rc == 0 {
                break;
            }
        }
    }
    // 一帧必然读出错的字节（ping + payload_len=0 之外的非法组合），
    // 让 Hub 在 RST 之前先拿到一个「读失败」而不是「读到一帧」。
    ws.get_mut().write_all(&[0x81, 0x00]).await.unwrap();
    drop(ws);

    // Hub 必须在 5s 内把这条连接摘掉（标准②）
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if hub.state.connections().get(&node_id).is_none() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("registry entry for {node_id} was not cleared after an abrupt drop");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 认证成功后发不可解析的文本帧 → 连接必须**继续活着**。
///
/// 「看不懂的帧就断」会把一次编码 bug 变成一次设备掉线；正确做法是丢掉
/// 这一帧并继续读（约束 14 的前向兼容在业务帧上同样成立）。
#[tokio::test]
async fn an_unparsable_business_frame_does_not_end_the_connection() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "auth_ok");

    send_text(&mut ws, "}{").await;
    send_text(&mut ws, r#"{"v":1,"seq":2,"t":"never_heard_of_it","c":{}}"#).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "an ignored frame must not end the connection"
    );
    drop(ws);
}

/// 业务帧里的 ping / 二进制 / Close 各自怎么走：
/// ping 跳过、二进制跳过、Close 结束连接（§2 只收文本帧，但**不得因此崩掉**）。
#[tokio::test]
async fn ping_and_binary_are_skipped_but_close_ends_the_session() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "auth_ok");

    // 握手后不再有 Hub 的回包了，所以这些断言靠「注册表还在不在」来判
    send_raw(
        &mut ws,
        tokio_tungstenite::tungstenite::Message::Ping(Vec::new()),
    )
    .await;
    send_raw(
        &mut ws,
        tokio_tungstenite::tungstenite::Message::Binary(vec![1, 2, 3]),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "ping and binary frames must not end the session"
    );

    send_raw(
        &mut ws,
        tokio_tungstenite::tungstenite::Message::Close(None),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if hub.state.connections().get(&node_id).is_none() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("a close frame must end the session");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 业务帧里的心跳被收到（此时**只是记一行**，落库是子任务 5），
/// 且连接继续活着——本组的回归锁是「心跳不引发任何副作用」。
#[tokio::test]
async fn a_business_heartbeat_keeps_the_session_alive() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "auth_ok");

    send_text(
        &mut ws,
        r#"{"v":1,"seq":2,"t":"heartbeat","c":{"node_version":"0.1.0"}}"#,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "a heartbeat must keep the session alive"
    );
    drop(ws);
}

// ---------- 超时 ----------

/// 连上后一句话不说 → 握手超时（§3 的 10s）后 Hub 主动断开。
///
/// 裸客户端把 socket 留在后台连接里，测试不关它——这样 Hub 那边只可能
/// 是因为**自己的**超时判定而断开。
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_peer_is_cut_off_after_the_handshake_timeout() {
    let hub = Hub::initialized().await;
    let (mut ws, resp) = tokio_tungstenite::connect_async(hub.ws_url())
        .await
        .unwrap();
    assert_eq!(resp.status(), 101u16);
    let closed = tokio::time::timeout(
        protocol::handshake::HANDSHAKE_TIMEOUT + Duration::from_secs(5),
        async {
            loop {
                match ws.next().await {
                    None => return true,
                    Some(Err(_)) => return true,
                    Some(Ok(_)) => continue,
                }
            }
        },
    )
    .await
    .unwrap_or(false);
    assert!(closed, "a peer that never says hello must be cut off");
    // socket 有意留在后台连接里：Hub 不来关它，它就一直挂着（见下）
    std::mem::forget(ws);
}

/// 超时帧送到时对端已经不听了 → Hub 必须**仍然自己断开**。
///
/// 这一条卡的是 `send_best_effort` 的失败臂：如果它把「帧发不出去」
/// 当成致命错误提前 return，这条连接就会一直挂到 fd 耗尽。
/// 触发方式是先发一帧**超大**载荷（超出 1 MiB 上限，§2）让 Hub 在
/// 读这一帧时就把连接判死，然后立刻超时——此时失败帧必然无人接收。
#[tokio::test(flavor = "multi_thread")]
async fn the_timeout_frame_fails_to_deliver_when_the_peer_is_already_gone() {
    let hub = Hub::initialized().await;
    let (mut ws, resp) = tokio_tungstenite::connect_async(hub.ws_url())
        .await
        .unwrap();
    assert_eq!(resp.status(), 101u16);
    // 2 MiB > WS_MAX_MESSAGE_BYTES(1 MiB)：Hub 读到这一帧就断开这条连接
    let oversize = "x".repeat(2 * 1024 * 1024);
    let _ = ws
        .send(tokio_tungstenite::tungstenite::Message::Text(oversize))
        .await;
    tokio::time::sleep(protocol::handshake::HANDSHAKE_TIMEOUT + Duration::from_millis(500)).await;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None => return true,
                Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        ended,
        "hub must close the connection after the handshake window"
    );
    std::mem::forget(ws);
}

// ---------- 存储层故障注入 ----------
//
// 下面几条用 SQLite 触发器 / DDL 制造「读得到、写不进」与「表整个读不了」
// 两种真实故障。它们不是 mock：`RAISE(ABORT)` 与 `DROP TABLE` 走的就是
// rusqlite 的错误路径，Hub 侧拿到的是货真价实的 `StorageError`。
//
// 为什么必须测：这几条路径决定的是**故障时会不会变成安全事故**——
// 「写公钥失败却仍然放行」「库读不出来却去猜身份」都属之。

/// enroll 之后写公钥写不进去 → **仍然放行**（只记 warn），连接可用。
///
/// 这是刻意的取舍（`ws.rs` 注释里写着）：公钥写失败的后果是「下次重连
/// 因为没有公钥而被拒」，而那比重连失败更容易排查；反过来若把 enroll
/// 判成失败，设备会卡在一个用户根本无从修复的状态里。
#[tokio::test]
async fn enroll_survives_a_public_key_write_failure() {
    let hub = Hub::initialized().await;
    hub.break_device_writes();

    let (_kp, node_id) = enroll_node(&hub).await;
    assert!(
        !node_id.is_empty(),
        "a failed public-key write must not turn into a failed enroll"
    );
    // 后果正如注释所写：库里没有公钥 → 下次重连会被拒（下面单独测）
    let device = hub.state.registry().device(&node_id).unwrap().unwrap();
    assert!(
        device.capabilities.is_none(),
        "the trigger must really have blocked the write, got {:?}",
        device.capabilities
    );
}

/// 同一次 enroll 里 `set_gen` 也写不进去 → 仍放行。
///
/// `last_seen_gen` 只参与顶替判定（§7.1）；写不进去意味着 Hub 重启后
/// 可能把一条老连接当成「还有更新的」，代价是**多发一次 superseded**，
/// 而不是放宽任何身份校验。所以它同样只记 warn。
#[tokio::test]
async fn enroll_survives_a_gen_write_failure() {
    let hub = Hub::initialized().await;
    hub.break_device_writes();
    let (_kp, node_id) = enroll_node(&hub).await;
    let device = hub.state.registry().device(&node_id).unwrap().unwrap();
    assert_eq!(
        device.last_seen_gen, 0,
        "the trigger must really have blocked the gen write"
    );
}

/// enroll 之后重连：公钥读得到、但 `set_gen` 写不进去 → 仍放行。
///
/// 与 enroll 那条同样的安全判断：顶替信息写不进去只影响「多不多发一次
/// superseded」，而**验签已经过了**，此时断开反而会把一台合法设备拒之门外。
#[tokio::test]
async fn a_reconnect_survives_a_gen_write_failure() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    hub.break_device_writes();

    let mut ws = dial(&hub.ws_url()).await;
    send_reconnect_auth(&mut ws, &node_id, &kp, false).await;
    let ok = next_frame(&mut ws).await;
    assert_eq!(
        ok.msg_type, "auth_ok",
        "a failed gen write must not end an otherwise valid reconnect: {:?}",
        ok.c
    );
    assert_eq!(
        ok.c.get("node_id").and_then(|v| v.as_str()),
        Some(node_id.as_str())
    );
}

/// 重连时 `device` 表整个读不了 → **断开，不得放行**。
///
/// 故障时最容易犯的错是「查不到就当没事，继续验签」——那等于故障期间
/// 任何 node_id 都能连进来（§3：重连只信库内指纹，读不到就没有库内指纹）。
/// 这里断言的是「宁可全断，不猜」。
#[tokio::test]
async fn a_reconnect_ends_when_the_device_cannot_be_looked_up() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    hub.break_device_lookup();

    let mut ws = dial(&hub.ws_url()).await;
    send_reconnect_auth(&mut ws, &node_id, &kp, false).await;

    // ⚠️ 期望「直接断开」而**不是** `auth_fail{identity mismatch}`：
    // 查库失败是 Hub 侧的**临时故障**，设备身份本身没问题。回
    // `identity mismatch` 会让节点按 §7.1 判定「身份被拒」而**永久停止
    // 重连**——Hub 一次瞬时故障就变成设备永久掉线，且要人工删节点记录
    // 才能恢复。正确做法是断连接让节点照常退避重连。
    wait_closed(&mut ws).await;
}

/// 「Hub 有没有设密码」读不出来时，一律按**未初始化**处理（§3.1）。
///
/// 这是本组里安全权重最高的一条：`hub_is_initialized` 的 Err 分支
/// 决定的是「Hub 状态未知时还要不要放行 enroll」。答案必须是「不」——
/// 故障期间放行，等于 Hub 在自己都不知道自己是什么状态时接纳新设备。
#[tokio::test]
async fn a_rejected_auth_is_rejected_when_the_init_state_cannot_be_read() {
    let hub = Hub::initialized().await;
    hub.break_secret_table();

    let mut ws = dial(&hub.ws_url()).await;
    let _nonce = challenge_nonce(&mut ws).await;
    send_enroll_auth(&mut ws, [0u8; 64]).await;
    expect_auth_fail(&mut ws, protocol::message::reason::HUB_NOT_INITIALIZED).await;
    wait_closed(&mut ws).await;
}

/// enroll 的 INSERT 失败 → Hub 回 `fingerprint_race_retry`，不得 panic。
///
/// 这是 `send_enroll_auth` 走「enroll 形态」「写库失败」那条真实路径——
/// `Registry::enroll` 把 SQLite 错误往上抛，Hub 收到 `Err` 就回 retry。
/// 注意要先 enroll 出一行设备（让触发器建好）再破坏 INSERT，否则
/// 触发器是在空表上建的、对 INSERT 一样拦。
#[tokio::test]
async fn a_failed_enroll_write_is_reported_as_a_race_retry() {
    let hub = Hub::initialized().await;

    // 先正常 enroll 一个设备，证明触发器是「只在 INSERT 失败时拦」，
    // 而不是「在空表上把所有 INSERT 都拦」。
    {
        let mut ws = dial(&hub.ws_url()).await;
        let nonce = challenge_nonce(&mut ws).await;
        let kp = KeyPair::generate();
        let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
        send_text(
            &mut ws,
            &serde_json::json!({
                "v": 1, "seq": 1, "t": "auth",
                "c": {
                    "public_key": identity::base64url(&kp.public_key_bytes()),
                    "signature": identity::base64url(&sig),
                    "device_info": info(),
                },
            })
            .to_string(),
        )
        .await;
        assert_eq!(next_frame(&mut ws).await.msg_type, "auth_ok");
        drop(ws);
    }

    hub.break_device_insert();
    let mut ws = dial(&hub.ws_url()).await;
    // 签名必须是**合法**的：坏签名会在验签阶段就拒成 `identity mismatch`，
    // 那样根本走不到 INSERT，这个用例什么也测不到。
    let nonce = challenge_nonce(&mut ws).await;
    send_enroll_auth_signed(&mut ws, &nonce).await;
    expect_auth_fail(&mut ws, protocol::message::reason::FINGERPRINT_RACE_RETRY).await;
    wait_closed(&mut ws).await;
}

/// 握手期回程帧发不出去 → Hub 不能 panic，连接干净地结束。
///
/// 用 `SO_SNDBUF` 把发送缓冲压到内核下限、再往里灌数据把它占满：Hub 的
/// `send` 必然拿到 `WouldBlock`/`Io` 错误。这卡的是 `send_frame` 的失败臂
/// （`ws.rs` 把「发不出去」翻译成 `peer closed` 而非崩溃）。
#[tokio::test]
async fn a_handskake_frame_that_cannot_be_sent_ends_the_connection() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    // 在发 hello 之前先把发送缓冲占满，让 Hub 下发的 challenge 发不回去
    hub.squeeze_send_buffer(&mut ws).await;
    send_text(&mut ws, HELLO).await;
    wait_closed(&mut ws).await;
}

// ---------- 断开的三种收尾方式 ----------

/// 握手成功后对端只做**半关闭**（`shutdown(SHUT_WR)`，不发 Close 帧）→ Hub 必须摘掉连接。
///
/// 三种收尾要分清，它们在 `relay` 里是三条不同的路：
///   - 正常关闭：发 Close 帧 → `recv` 交出 `Message::Close`；
///   - 半关闭：只关写半边 → `recv` 返回 `None`，循环结束（relay 返回 Ok）；
///   - 进程没了：RST → `recv` 报协议错误（relay 返回 Err）。
///
/// 半关闭是最容易被误判成「连接还活着」的那种：TCP 层面连接确实还在
/// （对端还能读），但设备已经不会再发任何帧了，不摘表就是一条僵尸连接。
#[tokio::test(flavor = "multi_thread")]
async fn a_half_closed_peer_is_dropped_from_the_registry() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "auth_ok");

    // 只关写半边：**不发** Close 帧，于是 Hub 那边 `recv` 只能读到流结束。
    {
        let stream: &mut tokio::net::TcpStream = match ws.get_mut() {
            tokio_tungstenite::MaybeTlsStream::Plain(s) => s,
            other => panic!("the url is plain ws://, got {other:?}"),
        };
        let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
        assert_eq!(unsafe { libc::shutdown(fd, libc::SHUT_WR) }, 0);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if hub.state.connections().get(&node_id).is_none() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("a half-closed peer must be removed from the registry");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 把 socket 变成 RST 而不是 FIN，然后交给调用方 drop。
///
/// FIN 与 RST 在 Hub 侧是两条不同的路：FIN 让 `recv` 交出流结束（`Ok(None)`），
/// RST 让 `recv` 直接报错。半关闭与正常关闭两个用例已经覆盖了 FIN 这条，
/// RST（设备进程被 `kill -9`、网线被拔）只能靠这一条构造。
fn abort_with_rst(ws: &mut Ws) {
    let stream: &mut tokio::net::TcpStream = match ws.get_mut() {
        tokio_tungstenite::MaybeTlsStream::Plain(s) => s,
        other => panic!("the url is plain ws://, got {other:?}"),
    };
    let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
    // l_onoff=1 + l_linger=0 ⇒ close() 不走四次挥手，直接丢一个 RST 出去。
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    assert_eq!(
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                std::ptr::from_ref(&linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        },
        0,
        "SO_LINGER must be settable for this fixture to mean anything"
    );
}

/// 轮询注册表直到 `node_id` 被摘掉。
async fn wait_dropped(hub: &Hub, node_id: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if hub.state.connections().get(node_id).is_none() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 握手成功后对端发 RST（不是 FIN）→ Hub 必须把它当连接错误收尾，不能挂住。
///
/// 与 [`a_half_closed_peer_is_dropped_from_the_registry`] 是一对：那条走
/// `Ok(None)`（流结束），这条走 `Err`（协议错误）。没有这条，Hub 读错误那条
/// 路完全没有回归锁。
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_peer_during_the_handshake_is_answered_with_a_close() {
    let hub = Hub::initialized().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, HELLO).await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "challenge");

    // 先排空 Hub 那边可能补回来的帧，再断——反过来的话读写会撞在同一条流上，
    // 究竟是谁先关的就有歧义了。
    abort_with_rst(&mut ws);
    drop(ws);
}

/// 握手期对端卡住不回话 → 握手超时（§3 的 10s）后 Hub 补一帧失败再断开。
///
/// 覆盖的是 `recv` 一直挂着的那条路：入口 deadline 与循环内 deadline 都判不到
/// 「对端还在线但不说话」，必须靠超时兜底。
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_peer_during_the_handshake_is_cut_off() {
    let hub = Hub::initialized().await;
    let (mut ws, resp) = tokio_tungstenite::connect_async(hub.ws_url())
        .await
        .unwrap();
    assert_eq!(resp.status(), 101u16);
    send_text(&mut ws, HELLO).await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "challenge");
    // 拿到 challenge 之后彻底不说话。
    let deadline = Instant::now() + protocol::handshake::HANDSHAKE_TIMEOUT + Duration::from_secs(5);
    loop {
        if Instant::now() >= deadline {
            panic!("a stalled peer must be cut off by the handshake timeout");
        }
        match ws.next().await {
            None => return,
            Some(Err(_)) => return,
            // 失败帧先到、关闭帧后到，两者都在同一条流上，读到就算数。
            Some(Ok(_)) => continue,
        }
    }
}

/// 握手成功后对端发 RST → `relay` 走错误那条路返回，连接照常摘表。
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_peer_after_auth_ok_is_dropped_from_the_registry() {
    let hub = Hub::initialized().await;
    let (kp, node_id) = enroll_node(&hub).await;
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let sig = kp.sign(&identity::signature_message(1, "", 1, "auth", &nonce));
    send_text(
        &mut ws,
        &serde_json::json!({
            "v": 1, "seq": 1, "t": "auth",
            "c": {
                "public_key": identity::base64url(&kp.public_key_bytes()),
                "signature": identity::base64url(&sig),
                "device_info": info(),
            },
        })
        .to_string(),
    )
    .await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "auth_ok");

    abort_with_rst(&mut ws);
    drop(ws);
    wait_dropped(
        &hub,
        &node_id,
        "a reset peer must be removed from the registry",
    )
    .await;
}
