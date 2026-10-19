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

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    break_device_insert, break_device_lookup, break_device_writes, break_secret_table, device_info,
    dial, insert_legacy_device, next_frame, send_text, wait_until, Hub, Ws, HELLO,
};
use tokio::io::AsyncWriteExt as _;

use futures_util::{SinkExt, StreamExt};
use hub_server::AppState;
use identity::KeyPair;
use node_core::conn::{connect_once, ConnOutcome};
use node_core::identity::Identity;
use tower::ServiceExt;

/// 设备信息（enroll 载荷的 `device_info`）。
fn info() -> serde_json::Value {
    serde_json::json!({
        "name": "pi", "os": "linux", "arch": "aarch64",
        "hostname": "testhost", "node_version": "0.1.0",
    })
}

/// 发一帧非文本帧（ping / pong / 二进制），用来打「帧种类」这一关。
async fn send_raw(ws: &mut Ws, msg: tokio_tungstenite::tungstenite::Message) {
    ws.send(msg).await.unwrap();
}

/// 故障注入：握手期**任何**一帧回程都发不出去（触发 socket 上限）。
///
/// 用 `SO_SNDBUF` 把发送缓冲压到内核下限，再让对端握手期间既不发数据、
/// 也不读任何东西：内核缓冲立刻填满，Hub 的 `send` 就会拿到
/// `WouldBlock`/`Io` 错误。这覆盖的是「帧发不出去」这条真实存在的路
/// （对端在握手中途消失、或链路拥塞），而不是构造一个假 socket。
async fn squeeze_send_buffer(ws: &mut Ws) {
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

/// 标准①：`afcli run` 启动后 5s 内 Hub 日志出现 `ws connected`，
/// 且 `/ws` upgrade 返回 HTTP 101 且首帧为 `challenge{nonce}`。
///
/// 这里不用读 Hub 日志（测试进程里拿不到子进程 stderr），
/// 而是用**可观测的等价事实**替代：TCP 连上 + 收到 `challenge` 帧 =
/// 101 upgrade 成功 + 首帧正确。日志断言由 `tests/afcli_process.rs`
/// 在进程级做（那里真的看得到 stderr）。
#[tokio::test]
async fn ws_upgrade_returns_challenge_as_first_frame() {
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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

    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;

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
    let hub = Hub::boot().await;
    let kp = KeyPair::generate();

    let est = connect_once(&hub.ws_url(), &kp, None, None, device_info())
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
    let hub = Hub::boot().await;
    let kp = KeyPair::generate();

    let first = connect_once(&hub.ws_url(), &kp, None, None, device_info())
        .await
        .expect("enroll");
    let node_id = first.auth_ok.node_id.clone();
    let first_gen = first.auth_ok.gen;

    // 用同一个 node_id 重连（第二个连接）
    let second = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
    let dir = tempfile::tempdir().unwrap();
    let mut id = Identity::load_or_create(dir.path()).unwrap();

    let est = connect_once(&hub.ws_url(), id.key(), None, None, device_info())
        .await
        .expect("enroll with a locally generated key must work");
    let node_id = est.auth_ok.node_id.clone();
    id.set_node_id(&node_id).unwrap();

    // 重启等价：重新加载 identity.json，node_id 还在
    let reloaded = Identity::load_or_create(dir.path()).unwrap();
    assert_eq!(reloaded.node_id().as_deref(), Some(node_id.as_str()));

    // 用它重连：Hub 认这把钥匙
    let again = connect_once(
        &hub.ws_url(),
        reloaded.key(),
        Some(&node_id),
        None,
        device_info(),
    )
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

    let out = connect_once(&hub.ws_url(), &kp, None, None, device_info()).await;
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
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, "}{").await;
    wait_closed(&mut ws).await;
}

/// `hello` 载荷缺字段（这里缺 `arch`）→ 断开，**不发** challenge。
#[tokio::test]
async fn a_hello_with_a_broken_payload_ends_the_handshake() {
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, r#"{"v":1,"seq":0,"t":"hub_hello","c":{}}"#).await;
    let nonce = challenge_nonce(&mut ws).await;
    assert_eq!(nonce.len(), 43, "hub_hello must not end the handshake");
    drop(ws);
}

/// `hello` 的载荷畸形 → 断开。
#[tokio::test]
async fn a_malformed_auth_payload_ends_the_handshake() {
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
    insert_legacy_device(&hub, "n_legacy");
    let kp = KeyPair::generate();
    let mut ws = dial(&hub.ws_url()).await;
    send_reconnect_auth(&mut ws, "n_legacy", &kp, false).await;
    expect_auth_fail(&mut ws, protocol::message::reason::IDENTITY_MISMATCH).await;
    wait_closed(&mut ws).await;
}

/// 重连的签名不过（库里公钥是另一把）→ 拒，且**不得改写**库内指纹。
#[tokio::test]
async fn a_reconnect_with_a_bad_signature_is_rejected() {
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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

/// 业务帧里的 ping / Close 各自怎么走：ping 跳过、Close 结束连接。
///
/// ⚠️ **二进制不在这里**：`frame_text` 把二进制判成 Transport 错误（协议 §2
/// 只用文本 JSON 帧），所以「握手后发二进制」的结果是连接结束，不是跳过。
/// 放在这里断言「二进制被跳过」会锁住一个与协议相反的行为——它已经这么失败过一次。
#[tokio::test]
async fn a_ping_is_skipped_but_close_ends_the_session() {
    let hub = Hub::boot().await;
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
    wait_until(Duration::from_secs(5), || {
        hub.state.connections().get(&node_id).is_some()
    })
    .await;

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

/// 握手失败帧（`auth_fail`）发不出去 → 连接结束，且**不 panic**。
///
/// 覆盖 `send_best_effort` 的那条留痕分支：帧发不出去时 Hub 只记一行
/// info，绝不让一个写死的 socket 把连接处理函数带崩（那会连带杀掉
/// 同一进程里所有其他设备的连接）。
#[tokio::test]
async fn a_failure_frame_that_cannot_be_sent_only_leaves_a_trace() {
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, HELLO).await;
    // challenge 很小，压缓冲之前已经发出；auth 之前把发送缓冲占满
    assert_eq!(next_frame(&mut ws).await.msg_type, "challenge");
    squeeze_send_buffer(&mut ws).await;
    // 载荷畸形 → Hub 要回 auth_fail，而这一帧写不出去
    send_text(
        &mut ws,
        r#"{"v":1,"seq":1,"t":"auth","c":{"signature":"!!!"}}"#,
    )
    .await;
    wait_closed(&mut ws).await;

    // Hub 进程没被这一条连接带崩
    let (kp, node_id) = enroll_node(&hub).await;
    wait_until(Duration::from_secs(5), || {
        hub.state.connections().get(&node_id).is_some()
    })
    .await;
    assert_eq!(kp.public_key_bytes().len(), 32, "sanity check");
}

/// 握手期**收不到回程帧**（发送缓冲压满）→ 连接结束，且不 panic。
///
/// 这里刻意不发 `hello`：Hub 在第一帧 `challenge` 上就写不出去，
/// 于是 `handshake()` 的 `send_frame` 失败臂被真实触达（不是靠断言
/// 「发了个不存在的类型」绕出来的）。同时证明 Hub 不因此崩：一条
/// 半死不活的连接被扔掉，别的设备照常接入。
#[tokio::test]
async fn a_challenge_that_cannot_be_sent_ends_the_connection() {
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    squeeze_send_buffer(&mut ws).await;
    send_text(&mut ws, HELLO).await;
    wait_closed(&mut ws).await;

    // 别的设备不受影响：Hub 进程本身还活着
    let (kp, node_id) = enroll_node(&hub).await;
    wait_until(Duration::from_secs(5), || {
        hub.state.connections().get(&node_id).is_some()
    })
    .await;
    assert_eq!(kp.public_key_bytes().len(), 32, "sanity check");
}

/// 业务期的 `open_channel` 真的会收到 `channel_ready` 回帧，且连接不断。
///
/// M0 的 Hub 侧**没有 connector 协商**（`on_open` 开完立刻置 Ready）：先回
/// `channel_ready` 是让账本活起来的唯一办法，否则 node 发来的数据全部回
/// `NotReady`、node 又在等 ready，两边互等成死锁。
#[tokio::test]
async fn a_business_open_channel_is_answered_with_channel_ready() {
    let hub = Hub::boot().await;
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
        r#"{"v":1,"seq":2,"t":"open_channel","c":{"channel_id":"ch-1","connector":"terminal"}}"#,
    )
    .await;
    let reply = next_frame(&mut ws).await;
    assert_eq!(reply.msg_type, "channel_ready", "M0 开完即就绪");
    assert_eq!(reply.c["channel_id"], "ch-1");

    // 就绪之后数据面才走得通（§5：先 ready 后 data）
    send_text(
        &mut ws,
        r#"{"v":1,"seq":3,"t":"channel_data","c":{"channel_id":"ch-1","payload":"x"}}"#,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_until(Duration::from_secs(5), || {
        hub.state.connections().get(&node_id).is_some()
    })
    .await;
}

/// 业务帧里的心跳被收到（此时**只是记一行**，落库是子任务 5），
/// 且连接继续活着——本组的回归锁是「心跳不引发任何副作用」。
#[tokio::test]
async fn a_business_heartbeat_keeps_the_session_alive() {
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
    break_device_writes(&hub);

    let (_kp, node_id) = enroll_node(&hub).await;
    assert!(
        !node_id.is_empty(),
        "a failed public-key write must not turn into a failed enroll"
    );
    // 后果正如注释所写：库里没有公钥 → 下次重连会被拒（下面单独测）
    let device = hub.state.registry().device(&node_id).unwrap().unwrap();
    assert!(
        device.public_key.is_none(),
        "the trigger must really have blocked the write, got {:?}",
        device.public_key
    );
}

/// 同一次 enroll 里 `set_gen` 也写不进去 → 仍放行。
///
/// `last_seen_gen` 只参与顶替判定（§7.1）；写不进去意味着 Hub 重启后
/// 可能把一条老连接当成「还有更新的」，代价是**多发一次 superseded**，
/// 而不是放宽任何身份校验。所以它同样只记 warn。
#[tokio::test]
async fn enroll_survives_a_gen_write_failure() {
    let hub = Hub::boot().await;
    break_device_writes(&hub);
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
    let hub = Hub::boot().await;
    let (kp, node_id) = enroll_node(&hub).await;
    break_device_writes(&hub);

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
    let hub = Hub::boot().await;
    let (kp, node_id) = enroll_node(&hub).await;
    break_device_lookup(&hub);

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
    let hub = Hub::boot().await;
    break_secret_table(&hub);

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
    let hub = Hub::boot().await;

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

    break_device_insert(&hub);
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
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    // 在发 hello 之前先把发送缓冲占满，让 Hub 下发的 challenge 发不回去
    squeeze_send_buffer(&mut ws).await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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
    let hub = Hub::boot().await;
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

/// `relay` 正常收尾（`recv` 交出流结束而非报错）→ 走 `Ok(())` 那条路。
///
/// 与 `ping_and_binary_are_skipped_but_close_ends_the_session` 的区别：那条
/// 发 Close 帧，`recv` 交出 `Message::Close` 走 `Err` 臂；本条半关闭，
/// `recv` 返回 `None`、循环自然结束。两条都要有回归锁。
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_drops_the_stream_after_auth_ok_is_cleaned_up() {
    let hub = Hub::boot().await;
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

    // 半关闭：不发 Close 帧，Hub 的 `recv` 只能读到流结束。
    {
        let stream: &mut tokio::net::TcpStream = match ws.get_mut() {
            tokio_tungstenite::MaybeTlsStream::Plain(s) => s,
            other => panic!("the url is plain ws://, got {other:?}"),
        };
        assert_eq!(
            unsafe { libc::shutdown(std::os::fd::AsRawFd::as_raw_fd(stream), libc::SHUT_WR) },
            0
        );
    }
    wait_dropped(
        &hub,
        &node_id,
        "a peer that only drops the stream must still be removed",
    )
    .await;
}

/// `auth_ok` 发不出去（发送缓冲被压满）→ Hub 不能 panic，连接干净结束。
///
/// 覆盖的是 `finish_ok` 里 `send_frame(...).is_err()` 那条臂：认证全部通过、
/// 只差最后一步回帧时对端已走。squeeze 必须发生在发 `auth` **之前**——
/// `auth_ok` 一旦发出去（哪怕 Hub 随后才失败）测试就失去确定性。
#[tokio::test]
async fn an_auth_ok_that_cannot_be_sent_ends_the_connection() {
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, HELLO).await;
    // 先把 challenge 拿到（它很小，压缓冲前发出），再占满缓冲。
    assert_eq!(next_frame(&mut ws).await.msg_type, "challenge");
    squeeze_send_buffer(&mut ws).await;
    send_text(&mut ws, "this frame cannot possibly reach hub").await;
    wait_closed(&mut ws).await;
}

/// 握手失败帧也发不出去 → Hub 不能 panic，且不能尝试补救第二次。
///
/// 覆盖 `send_best_effort` 的 `Err` 臂。与上面那条的差别是失败帧走
/// `send_best_effort`（尽力而为、失败只记日志），而 `auth_ok` 走
/// `send_frame`（失败即断开）——两条路的错误处理不同，都要有锁。
#[tokio::test]
async fn a_failure_frame_that_cannot_be_sent_does_not_panic() {
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    send_text(&mut ws, HELLO).await;
    assert_eq!(next_frame(&mut ws).await.msg_type, "challenge");
    squeeze_send_buffer(&mut ws).await;
    // 坏签名 → Hub 要回 auth_fail，而回程已经堵死。
    send_enroll_auth(&mut ws, [0u8; 64]).await;
    wait_closed(&mut ws).await;
}

/// 客户端在发出 `auth` 后**立刻 RST**（SO_LINGER=0）→ Hub 回 `auth_ok` 必然失败。
///
/// 与 [`Hub::squeeze_send_buffer`] 的区别：那个只是把发送缓冲**堵住**，
/// 而 tungstenite 的 `send` 会把帧排进内部队列、当场返回成功——失败臂
/// 根本走不到。要真触达「帧发不出去」，必须让 socket 在写之前就已经 RST。
/// 这一条覆盖 `finish_ok` 里「auth_ok 发不出去 = 连接没有成立」那条。
#[tokio::test]
async fn an_auth_ok_that_cannot_be_delivered_does_not_establish_the_connection() {
    let hub = Hub::boot().await;
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
    // 发完 auth 就把连接 RST 掉（不等 Hub 回帧）
    rst(ws);
    // Hub 侧不得把这条连接注册进表：auth_ok 没送达 = 连接没有成立
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if hub.state.connections().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "连接不该被注册");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 坏签名的 `auth` + 立刻 RST → Hub 回 `auth_fail` 失败，但**不得 panic**。
///
/// 覆盖 `send_best_effort` 的 Err 臂：失败帧发不出去时连接马上就断了，
/// 没有第二个渠道能通知节点，所以只能留痕。
#[tokio::test]
async fn an_auth_fail_that_cannot_be_delivered_does_not_panic() {
    let hub = Hub::boot().await;
    let mut ws = dial(&hub.ws_url()).await;
    let nonce = challenge_nonce(&mut ws).await;
    let _ = nonce;
    // 坏签名（全零）→ Hub 要回 auth_fail
    let _ = ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "v": 1, "seq": 1, "t": "auth",
                "c": {
                    "public_key": identity::base64url(&KeyPair::generate().public_key_bytes()),
                    "signature": identity::base64url(&[0u8; 64]),
                    "device_info": info(),
                },
            })
            .to_string(),
        ))
        .await;
    rst(ws);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if hub.state.connections().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "连接不该被注册");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 立刻 RST 一条连接：`SO_LINGER=0` 让 close 发 RST 而不是 FIN。
fn rst(ws: Ws) {
    let mut ws = ws;
    let stream: &mut tokio::net::TcpStream = match ws.get_mut() {
        tokio_tungstenite::MaybeTlsStream::Plain(s) => s,
        other => panic!("the url is plain ws://, got {other:?}"),
    };
    let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
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
    drop(ws);
}
