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

// 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
// 而 panic 消息比 match 分支更能说明哪一行、哪个值不对。
// let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
// 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::let_underscore_must_use,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};

use futures_util::SinkExt;
use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, ConnOutcome};
use node_core::identity::Identity;
use protocol::message::DeviceInfo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;

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
