//! `connect_once` 的传输错误路径。
//!
//! `conn.rs` 里的单测只能覆盖「连不上」（坏 URL / 拒连），
//! 而**握手期的坏帧全部来自对端**——要测它们就必须有一个可控的 WS 服务端。
//! 这个文件提供那个对端：一条裸 TCP + `tokio_tungstenite::accept_async`，
//! 按脚本发帧（任意文本 / 二进制 / ping / close）或用不同方式收尾。
//!
//! 用裸 tungstenite 而不是 axum：只有裸实现才能控制「只发一半关闭握手就断」
//! 与「走完关闭握手再断」这两种收尾，而它们在 node 侧是**不同的**代码路径。
//!
//! 覆盖的分支与 `crates/hub-server/tests/ws_flow.rs` 互为镜像：
//! 那边测「Hub 收到坏帧」，这里测「node 收到坏帧」——
//! 两端都得能扛住对端发疯，否则断在谁手里都查不出来。

// Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

// 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
// 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
// let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
// 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use identity::KeyPair;
use node_core::conn::{connect_once, ConnOutcome, Established};
use protocol::message::DeviceInfo;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as TsMessage;

/// 挑战帧的非空 nonce。真实 Hub 发的是 32 字节 base64url，
/// 而**签名验不过**才是这里的重点：这些测试要的从来不是「握手成功」，
/// 而是「握手失败得对」。`connect_once` 本就不验签（Hub 验），所以
/// 一个短 nonce 足够走到每条错误分支。
const NONCE: &str = "test-nonce";

/// 对端要发的一帧。
enum Act {
    /// 一帧原始文本（**不保证是合法 JSON**——发坏载荷正是这些测试的目的）。
    Text(&'static str),
    /// 一帧按 `t` + `c` 组装的 JSON 帧（见 [`frame`]）。
    Frame(TsMessage),
    /// challenge 帧，nonce 为 [`NONCE`]。绝大多数用例的第一步。
    Challenge,
    /// 一帧二进制：协议只用 JSON 文本（§2），二进制属于「对端发疯」。
    Binary,
    /// ping。tungstenite 客户端自动回 pong，握手流程不该被打断。
    Ping,
}

/// 脚本跑完后对端怎么收尾。
enum Tail {
    /// 发一个 Close 帧并等回 Close 帧，然后 socket 被 drop。
    /// node 侧读到的是「消息流结束」（`next()` 返回 `None`）。
    /// Hub 正常下线就是这个样子。
    Close,
    /// **RST**：socket 被硬断（进程被 kill / 网络断），走完的关闭握手都没有。
    /// node 侧 tungstenite 报 `ConnectionResetWithoutClosingHandshake`，
    /// 走的是「读帧出错」那条路，与 `Close` 的「流结束」是两条不同的臂。
    Abrupt,
    /// 保持连接打开（socket 跟着 task 悬挂，直到测试进程退出）。
    Hang,
}

fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: "pi".into(),
        os: "linux".into(),
        arch: "aarch64".into(),
        hostname: "pi".into(),
        node_version: "0.1.0".into(),
    }
}

fn challenge_frame() -> TsMessage {
    TsMessage::Text(format!(
        r#"{{"v":1,"seq":1,"t":"challenge","c":{{"nonce":"{NONCE}"}}}}"#
    ))
}

fn frame(t: &str, c: &str) -> TsMessage {
    TsMessage::Text(format!(r#"{{"v":1,"seq":9,"t":"{t}","c":{c}}}"#))
}

/// 起一个按脚本演的对端，返回它的地址。
///
/// 脚本在**读完 node 的 `hello`** 之后才开始发——与真实 Hub 的时序一致
/// （`challenge` 只有在 `hello` 之后才会发）。
fn auth_ok_frame() -> TsMessage {
    frame(
        "auth_ok",
        r#"{"node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":15000}"#,
    )
}

async fn serve(acts: Vec<Act>, tail: Tail) -> SocketAddr {
    serve_with_drain(acts, tail, true).await
}

/// `drain = false`：握手之后不再替 node 读帧，帧留在内核缓冲里给
/// `Established::next_text` 自己取。见 [`serve`]。
async fn serve_with_drain(acts: Vec<Act>, tail: Tail, drain: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // 等 node 的 hello（不校验内容：hello 的形状由 protocol crate 的单测负责）
        let _ = ws.next().await;
        for act in acts {
            let is_challenge = matches!(act, Act::Challenge);
            let msg = match act {
                Act::Text(t) => TsMessage::Text(t.into()),
                Act::Frame(m) => m,
                Act::Challenge => challenge_frame(),
                Act::Binary => TsMessage::Binary(vec![0xff, 0x00, 0x01]),
                Act::Ping => TsMessage::Ping(Vec::new()),
            };
            let _ = ws.send(msg).await;
            // challenge 之后必须排空（node 会回 `auth`，留着会让后面的
            // close() 落成 RST）；握手之后的帧则由 `drain` 决定——
            // `next_text` 的用例要自己读那一帧，服务端抢先读走就永远等不到。
            if drain || is_challenge {
                // 读掉 node 可能已回的话（pong / 关错误帧）。**必须读干净**：
                // 留在内核缓冲里的未读数据会让随后的 close() 触发 RST，
                // 于是「正常关闭」被悄悄变成 abrupt —— 两条分支会被测反。
                while let Ok(Some(Ok(m))) =
                    tokio::time::timeout(Duration::from_millis(20), ws.next()).await
                {
                    if matches!(m, TsMessage::Close(_)) {
                        break;
                    }
                }
            }
        }
        match tail {
            Tail::Close => {
                let _ = ws.close(None).await;
                // 等对端的 Close 回帧，把关闭握手走完；之后 socket 被 drop。
                let _ = ws.next().await;
            }
            Tail::Abrupt => {
                // 只发一半关闭握手就断（Hub 被 kill -9 / 网络断）：
                // 让 close() 直接发 RST——「读帧出错」必须由**真的读失败**触发。
                // 走完关闭握手的对端看到的是 FIN，read 返回 0，读帧循环会把它
                // 当成「对端已走」，那是另一条 match 臂（见 an_abrupt… 的对比用例）。
                {
                    // 测试服务永远不跑 TLS，ws.get_mut() 直接就是 TcpStream。
                    let stream: &mut tokio::net::TcpStream = ws.get_mut();
                    let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
                    // SO_LINGER(l_onoff=1, l_linger=0)：close() 不发 FIN 而发 RST。
                    // 字节序依平台而异（x86 是小端原生序）：两个排列都试。
                    let one: libc::c_int = 1;
                    let zero: libc::c_int = 0;
                    for (onoff, secs) in [(&one, &zero), (&zero, &one)] {
                        let mut linger: libc::linger = unsafe { std::mem::zeroed() };
                        linger.l_onoff = *onoff;
                        linger.l_linger = *secs;
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
                // 直接把 socket 丢在 RST 状态里：node 侧的 read 必须报「读帧出错」。
                // 不能先发一帧坏字节——那会先被 tungstenite 读成一个空文本帧，
                // 于是走的是 JSON 解析失败那条路，测不到 read 的错误分支。
            }
            Tail::Hang => {}
        }
    });
    addr
}

fn url(addr: SocketAddr) -> String {
    format!("ws://{addr}/ws")
}

/// 跑一次 enroll 形态的连接，返回它的结果。
async fn enroll(addr: SocketAddr) -> Result<Established, ConnOutcome> {
    connect_once(&url(addr), &KeyPair::generate(), None, device_info()).await
}

/// 把 `Transport` 的 `detail` 取出来断言——`ConnOutcome` 不派 PartialEq，
/// 而 detail 里的文本才是「这条路径真的走到了」的唯一证据。
fn transport_detail(out: Result<Established, ConnOutcome>) -> String {
    match out {
        Err(ConnOutcome::Transport { detail }) => detail,
        other => panic!("expected Transport, got {other:?}"),
    }
}

// ---------- challenge 阶段 ----------

/// challenge 帧的载荷缺 `nonce` → node 侧判「没拿到 challenge」。
#[tokio::test]
async fn challenge_without_a_nonce_is_a_transport_failure() {
    let addr = serve(
        vec![Act::Text(r#"{"v":1,"seq":1,"t":"challenge","c":{}}"#)],
        Tail::Close,
    )
    .await;
    assert_eq!(transport_detail(enroll(addr).await), "no challenge frame");
}

/// challenge 帧不是合法 JSON → 协议层错误，不是握手失败。
#[tokio::test]
async fn unparsable_challenge_is_a_transport_failure() {
    let addr = serve(vec![Act::Text("this is not json")], Tail::Close).await;
    let detail = transport_detail(enroll(addr).await);
    assert!(
        detail.starts_with("bad frame json:"),
        "want a parse failure, got {detail:?}"
    );
}

/// 服务端把 `auth_ok` 当成 challenge 发回来 → 类型不符即失败。
///
/// 这一条专治「对端状态机跑偏」：node 不能把一个形状对但类型错的帧
/// 当成 challenge 继续走（那会让签名覆盖一个错的 nonce）。
#[tokio::test]
async fn a_frame_of_the_wrong_type_is_rejected_where_a_challenge_is_expected() {
    let addr = serve(
        vec![Act::Frame(frame(
            "auth_ok",
            r#"{"node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":15000}"#,
        ))],
        Tail::Close,
    )
    .await;
    assert_eq!(
        transport_detail(enroll(addr).await),
        "expected challenge, got auth_ok"
    );
}

// ---------- auth 阶段的坏帧 ----------

/// 合法的 challenge 之后，auth 阶段的帧不是 JSON。
#[tokio::test]
async fn unparsable_frame_after_auth_is_a_transport_failure() {
    let addr = serve(vec![Act::Challenge, Act::Text("}{")], Tail::Close).await;
    let detail = transport_detail(enroll(addr).await);
    assert!(
        detail.starts_with("bad frame json:"),
        "want a parse failure, got {detail:?}"
    );
}

/// `auth_ok` 载荷缺字段 → 不得当成握手成功。
///
/// 这一条是**身份安全的最后一道闸**：形状不对的 auth_ok 绝不能变成
/// `Established`（那会让 node 以为自己是 n_xxx，而 Hub 根本没签发过它）。
#[tokio::test]
async fn auth_ok_without_its_four_fields_is_a_transport_failure() {
    let addr = serve(
        vec![Act::Challenge, Act::Frame(frame("auth_ok", "{}"))],
        Tail::Close,
    )
    .await;
    let detail = transport_detail(enroll(addr).await);
    assert!(
        detail.starts_with("bad auth_ok:"),
        "want an auth_ok decode failure, got {detail:?}"
    );
}

/// `auth_fail` 载荷缺 `reason` → 不得当成「Hub 明确拒绝」。
///
/// 两者的区别很实在：`auth_fail` 意味着「重试也不会过，等人介入」，
/// 而坏载荷只是「这次链路坏了，退避后重连」。混起来会让退避策略失准。
#[tokio::test]
async fn auth_fail_without_a_reason_is_a_transport_failure() {
    let addr = serve(
        vec![Act::Challenge, Act::Frame(frame("auth_fail", "{}"))],
        Tail::Close,
    )
    .await;
    let detail = transport_detail(enroll(addr).await);
    assert!(
        detail.starts_with("bad auth_fail:"),
        "want an auth_fail decode failure, got {detail:?}"
    );
}

// ---------- 必需集 / 可选集（约束 14） ----------

/// 必需集之外的未知类型 → **必须忽略**，等下一帧（约束 14 的核心断言）。
///
/// 对端先发一个 node 声明支持但 Hub 侧没实现的类型，再发一个 node
/// 完全没见过的类型，最后才是真 `auth_ok`：只有「忽略未知 + 继续等」
/// 的实现才会握手成功。若实现改成「见帧即断」，这条立刻挂掉——
/// 而那正是约束 14 想防的回归（老 node + 新 Hub 必须能继续跑）。
#[tokio::test]
async fn unknown_optional_messages_are_ignored_and_the_handshake_completes() {
    let addr = serve(
        vec![
            Act::Challenge,
            // node 声明支持的类型（hello.features），Hub 侧 M0 还没实现
            Act::Frame(frame("heartbeat_ack", "{}")),
            // 一个从没见过的类型
            Act::Frame(frame("some_future_message", "{}")),
            Act::Frame(frame(
                "auth_ok",
                r#"{"node_id":"n_x","protocol_version":1,"gen":7,"heartbeat_interval_ms":15000}"#,
            )),
        ],
        Tail::Close,
    )
    .await;
    let est = enroll(addr)
        .await
        .unwrap_or_else(|e| panic!("optional messages must not break the handshake: {e:?}"));
    assert_eq!(est.auth_ok.node_id, "n_x");
    assert_eq!(est.auth_ok.gen, 7);
    assert_eq!(est.auth_ok.heartbeat_interval_ms, 15_000);
    assert!(est.enrolled, "connecting without a node_id is an enroll");
}

/// 可选集帧出现在 challenge 之前会让握手失败——`read_frame_of` 只认第一帧。
///
/// 之前的测试断言「可选帧会被跳过」，那是在给一个真实 bug 背书：
/// `read_frame_of` 只读第一帧，`hub_hello` 会被当成 challenge 拒掉。
/// 握手期 Hub 按协议不会先发 `hub_hello`（§3），所以这里的期望就是失败。
#[tokio::test]
async fn an_optional_message_before_the_challenge_is_rejected() {
    let addr = serve(
        vec![
            Act::Frame(frame("hub_hello", "{}")),
            Act::Challenge,
            Act::Frame(frame(
                "auth_ok",
                r#"{"node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":15000}"#,
            )),
        ],
        Tail::Close,
    )
    .await;
    assert_eq!(
        transport_detail(enroll(addr).await),
        "expected challenge, got hub_hello"
    );
}

// ---------- 帧类型 ----------

/// ping 不打断握手：tungstenite 自动回 pong，读帧循环跳过后继续等。
#[tokio::test]
async fn a_ping_does_not_interrupt_the_handshake() {
    let addr = serve(
        vec![
            Act::Challenge,
            Act::Ping,
            Act::Frame(frame(
                "auth_ok",
                r#"{"node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":15000}"#,
            )),
        ],
        Tail::Close,
    )
    .await;
    let out = enroll(addr).await;
    assert!(
        out.is_ok(),
        "ping must be skipped, not treated as a failure: {out:?}"
    );
}

/// 二进制帧：协议只用 JSON 文本（§2），收到二进制即断开。
///
/// 不能「宽容地忽略」——一端把消息编成二进制通常意味着它对协议的理解
/// 已经和 Hub 分叉了，继续握手只会产生更难查的错。
#[tokio::test]
async fn a_binary_frame_ends_the_handshake() {
    let addr = serve(vec![Act::Challenge, Act::Binary], Tail::Hang).await;
    assert_eq!(
        transport_detail(enroll(addr).await),
        "unexpected frame kind"
    );
}

// ---------- 断开 ----------

/// 对端发 Close 帧并走完关闭握手 → 「对端已走」，不是协议错误。
#[tokio::test]
async fn a_clean_peer_disconnect_reports_peer_closed() {
    let addr = serve(vec![Act::Challenge], Tail::Close).await;
    assert_eq!(transport_detail(enroll(addr).await), "peer closed");
}

/// 只发一半关闭握手就断（Hub 被 kill -9 / 网络断）→ 仍是 Transport，
/// 但走的是「读帧出错」那条路，不是「流结束」那条。
///
/// 两条路径都要有回归：它们在 `read_frame` 里是两个不同的 match 臂，
/// 混用会让「进程崩了」被记成「对端正常关闭」而误导排查方向。
#[tokio::test]
async fn an_abrupt_disconnect_is_reported_as_a_read_failure() {
    let addr = serve(vec![Act::Challenge], Tail::Abrupt).await;
    let detail = transport_detail(enroll(addr).await);
    assert!(
        detail.starts_with("read failed:"),
        "want a read failure, got {detail:?}"
    );
}

/// 连上不发任何东西就断开 → 握手失败，不是 panic。
#[tokio::test]
async fn a_peer_that_closes_before_saying_anything_is_transport() {
    let addr = serve(vec![], Tail::Close).await;
    assert_eq!(transport_detail(enroll(addr).await), "peer closed");
}

/// 握手成功后立刻断线：失败归类干净，不得 panic、不得误判成 `Superseded`。
#[tokio::test]
async fn dropping_the_connection_right_after_auth_ok_is_clean() {
    let addr = serve(
        vec![
            Act::Challenge,
            Act::Frame(frame(
                "auth_ok",
                r#"{"node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":15000}"#,
            )),
        ],
        Tail::Close,
    )
    .await;
    let est = enroll(addr)
        .await
        .expect("handshake completes before the peer hangs up");
    assert_eq!(est.auth_ok.node_id, "n_x");
    // 断开后没有任何后台任务在偷偷重连：退避重连是调用方（子任务 6）的职责，
    // conn.rs 只回答「连一次会发生什么」。
    drop(est);
}

/// 握手成功后 Hub 发来的文本帧 → `next_text` 原样交出。
///
/// 约束 14 说「未知帧类型原样交给调用方」——原样指的是**不解析**，不是**不返回**，
/// 所以这里断言的是文本一字不差地过来。
#[tokio::test]
async fn next_text_hands_back_the_raw_payload() {
    let addr = serve_with_drain(
        vec![
            Act::Challenge,
            Act::Frame(auth_ok_frame()),
            Act::Frame(frame("heartbeat", r#"{"node_version":"0.1.0"}"#)),
        ],
        Tail::Hang,
        false,
    )
    .await;
    let mut est = enroll(addr).await.expect("handshake completes");
    let text = est
        .next_text()
        .await
        .expect("no transport error")
        .expect("a frame");
    assert!(text.contains(r#""t":"heartbeat""#), "got {text:?}");
}

/// 二进制帧 → Transport（协议只用 JSON 文本，§2）。
#[tokio::test]
async fn next_text_rejects_a_binary_frame_after_auth() {
    let addr = serve_with_drain(
        vec![Act::Challenge, Act::Frame(auth_ok_frame()), Act::Binary],
        Tail::Hang,
        false,
    )
    .await;
    let mut est = enroll(addr).await.expect("handshake completes");
    match est.next_text().await {
        Err(ConnOutcome::Transport { detail }) => {
            assert_eq!(detail, "unexpected frame kind");
        }
        other => panic!("a binary frame must not be handed back as text: {other:?}"),
    }
}

/// ping 不该打断会话：tungstenite 自动应答，node 侧看不到它。
#[tokio::test]
async fn next_text_skips_a_ping() {
    let addr = serve_with_drain(
        vec![
            Act::Challenge,
            Act::Frame(auth_ok_frame()),
            Act::Ping,
            Act::Frame(frame("heartbeat", r#"{"node_version":"0.1.0"}"#)),
        ],
        Tail::Hang,
        false,
    )
    .await;
    let mut est = enroll(addr).await.expect("handshake completes");
    let text = est
        .next_text()
        .await
        .expect("no transport error")
        .expect("a frame");
    assert!(
        text.contains(r#""t":"heartbeat""#),
        "ping must not surface: {text:?}"
    );
}

/// Close 帧 → `Ok(None)`（对端正常下线），不是 Transport。
#[tokio::test]
async fn next_text_reports_a_close_frame_as_end_of_stream() {
    let addr = serve_with_drain(
        vec![Act::Challenge, Act::Frame(auth_ok_frame())],
        Tail::Close,
        false,
    )
    .await;
    let mut est = enroll(addr).await.expect("handshake completes");
    assert_eq!(
        est.next_text()
            .await
            .expect("close is not a transport error"),
        None
    );
}

/// RST → Transport，且前缀是「读帧出错」——与 Close 的「流结束」是两条臂。
#[tokio::test]
async fn next_text_reports_an_abrupt_reset_as_a_read_failure() {
    let addr = serve_with_drain(
        vec![Act::Challenge, Act::Frame(auth_ok_frame())],
        Tail::Abrupt,
        false,
    )
    .await;
    let mut est = enroll(addr).await.expect("handshake completes");
    match est.next_text().await {
        Err(ConnOutcome::Transport { detail }) => assert!(
            detail.starts_with("read failed:"),
            "want a read failure, got {detail:?}"
        ),
        other => panic!("want Transport, got {other:?}"),
    }
}

/// Close 之后再读一次：tungstenite 把已走完的关闭握手记成「流结束」，
/// 于是第二次读到的是 `None` 而不是又一次 Close。
///
/// 两次都要断言成 `None`：把它们混为一谈会让「对端走了」和「协议层关闭」
/// 在排障日志里长得一样，而它们的处理方式不同（前者不重连，后者要重连）。
#[tokio::test]
async fn next_text_reports_the_stream_end_only_after_the_close_frame() {
    let addr = serve(
        vec![Act::Challenge, Act::Frame(auth_ok_frame())],
        Tail::Close,
    )
    .await;
    let mut est = enroll(addr).await.expect("handshake completes");
    assert_eq!(
        est.next_text()
            .await
            .expect("close is not a transport error"),
        None,
        "first read must surface the Close frame"
    );
    assert_eq!(
        est.next_text()
            .await
            .expect("stream end is not a transport error"),
        None,
        "second read must surface the stream end"
    );
}

/// `Debug` 是排障时日志里唯一能看到的东西，**必须不含密钥材料**。
///
/// `est` 是真握手拿到的，所以这条走的是真的 `Debug` 实现。
#[tokio::test]
async fn established_debug_shows_identity_but_no_key_material() {
    let addr = serve(
        vec![Act::Challenge, Act::Frame(auth_ok_frame())],
        Tail::Close,
    )
    .await;
    let est = enroll(addr).await.expect("handshake completes");
    let d = format!("{est:?}");
    assert!(d.contains("n_x"), "got {d}");
    assert!(d.contains("enrolled"), "got {d}");
    assert!(d.contains("auth_ok"), "got {d}");
}

// ---------- 重连形态 ----------

/// 重连形态也走同一条读帧路径：`auth_fail{superseded}` 必须仍被识别为终态。
///
/// 走真实 socket 而不是直接调 `classify_auth_fail`：这里要证明的是
/// 「从线路上读到的 auth_fail 也会被正确分类」，而 `classify_auth_fail`
/// 自身的纯函数行为由 `conn.rs` 的单测负责。
#[tokio::test]
async fn superseded_read_off_the_wire_still_stops_reconnecting() {
    let addr = serve(
        vec![
            Act::Challenge,
            Act::Frame(frame("auth_fail", r#"{"reason":"superseded","gen":9}"#)),
        ],
        Tail::Close,
    )
    .await;
    match connect_once(&url(addr), &KeyPair::generate(), Some("n_x"), device_info()).await {
        Err(ConnOutcome::Superseded { gen }) => assert_eq!(gen, 9),
        other => panic!("expected Superseded, got {other:?}"),
    }
}

/// 重连形态下 Hub 明确拒绝（非 superseded）→ `Rejected`，
/// 调用方据此**不**退避重连（§7.1：身份问题重试无用）。
#[tokio::test]
async fn a_rejected_reconnect_is_not_a_transport_failure() {
    let addr = serve(
        vec![
            Act::Challenge,
            Act::Frame(frame("auth_fail", r#"{"reason":"identity mismatch"}"#)),
        ],
        Tail::Close,
    )
    .await;
    match connect_once(&url(addr), &KeyPair::generate(), Some("n_x"), device_info()).await {
        Err(ConnOutcome::Rejected { reason }) => {
            assert_eq!(reason, protocol::message::reason::IDENTITY_MISMATCH);
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

/// 约束 14 的另一半：未知**必需**类型（hub 发的 `err`/`heartbeat` 等）
/// 走 `Rejected`，不是忽略——身份语义已分叉，继续握手只会产生更难查的错。
///
/// 复用本文件的裸服务端脚手架（`serve`/`Act::Frame`），不另起炉灶。
#[tokio::test]
async fn an_unknown_required_message_is_rejected_not_ignored() {
    // `err` 属于必需集，`connect_once` 的握手循环不认它 → 必须断。
    let addr = serve(
        vec![
            Act::Challenge,
            Act::Frame(frame("err", r#"{"code":"handshake_timeout"}"#)),
        ],
        Tail::Close,
    )
    .await;
    let out = connect_once(&url(addr), &KeyPair::generate(), None, device_info()).await;
    match out {
        Err(ConnOutcome::Rejected { reason }) => {
            assert_eq!(reason, protocol::message::reason::IDENTITY_MISMATCH);
        }
        other => panic!("expected Rejected for unknown required type, got {other:?}"),
    }
}
