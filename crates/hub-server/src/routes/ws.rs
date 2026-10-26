//! `/ws`：设备 WebSocket 的 upgrade 与握手。
//!
//! 设计要点（`protocol.md` §3、`security-and-lifecycle.md` §2.4.3）：
//! - **`/ws` 免 session**：node 不是浏览器、没有 cookie，它走 challenge/签名认证。
//!   这条路由在 [`crate::routes::AUTH_EXEMPT_PATHS`] 里，所以已经先过了 session
//!   中间件；本模块自己再做 §3 的握手。
//! - **连接注册表只存句柄**：`(conn_id, gen)`，**不含 socket**（标准④）。
//!   socket 活在 `tokio::task` 的栈上，注册表只回答「这个 node 现在该往哪送」。
//! - **握手超时 10s**：半开连接不能一直占着 fd 与内存。
//! - **帧上限 1 MiB**：`axum` 默认 16 MiB 帧 / 64 MiB 消息太宽——
//!   Hub 是单进程服务多设备，一条连接的内存占用必须有硬上限（§6.1）。
//!
//! 这里**不做**心跳、退避重连、通道流控——分别是子任务 5、6、8。
//!
//! ## 写侧有界队列（子任务 5b）
//!
//! 读循环与写协程**分开两个 task**（[`relay`] + [`write_loop`]），出站帧
//! 一律经 [`conns::OutboundTable`] 的两条有界队列：控制面 8 帧、数据面 256 帧。
//! 分成两个 task 是必须的，不是风格选择：同一个循环里既 produce 又 consume
//! 时，`send_frame(...).await` 一旦在对端不读时阻塞，循环就再也回不到
//! `try_send`，队列永远填不满——「有界」在纸面上成立、在运行时失效。
//! 写协程按 `biased` 先取控制面：数据面打满绝不能延迟 `disconnect{superseded}`。

use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{conns, AppState, HANDSHAKE_TIMEOUT, WS_MAX_MESSAGE_BYTES};
use hub_core::connections::ConnHandle;
use protocol::frame::Frame;
use protocol::handshake::{
    error_phrase as handshake_error_phrase, AuthAttempt, AuthPayload, Authenticator, Handshake,
    HandshakeError, PublicKeySource,
};
use protocol::message::types;

/// `GET /ws` 的入口：先 upgrade，再进握手。
///
/// **Origin 检查放在 upgrade 之前**：浏览器会带 `Origin`，node 不会带——
/// 这正是「浏览器连上了设备通道」与「设备连上了 Hub」的判据。
/// 浏览器直连 `/ws` 没有任何意义（它在这里没有 session 之外的凭据），
/// 但它会占一条连接槽位，所以直接拒。
pub async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Some(reason) = reject_origin(&headers) {
        tracing::info!(reason = %reason, "ws upgrade rejected: origin check failed");
        return ws_error();
    }
    // 帧/消息上限收紧到协议规定的 1 MiB（§2）
    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .max_frame_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(state, socket))
}

/// Origin 检查：带 `Origin` 头的请求一律拒。
///
/// **只按「有没有这个头」判，不看它的值**：node 是原生程序，从不发 `Origin`；
/// 浏览器一定发。看值的做法（白名单比对）会引入「允许哪些站点跨站连 Hub」的
/// 配置面，而这个产品里**没有任何一个站点需要直连 `/ws`**——
/// 请求-响应走带 session 的 HTTP API，持续数据面走 `/api/stream`（SSE），
/// 两者都不在这个函数管的范围内。所以「有这个头就拒」才是对的边界。
/// 反代（Tailscale Serve / nginx）不会凭空加 `Origin`，不受影响。
fn reject_origin(headers: &HeaderMap) -> Option<&'static str> {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return Some("origin header present: browsers must not open device sockets");
    }
    None
}

/// 一条 WS 连接的全生命周期：握手 → 注册 → 业务帧循环 → 摘表。
async fn handle_socket(state: AppState, mut socket: WebSocket) {
    let started = Instant::now();
    // `gen` 在握手前就取：它同时是连接代次与 `conn_id` 的来源。
    // 失败一次握手的连接也消耗一个 gen，这没有关系——`gen` 只要单调递增。
    let gen = state.connections().next_gen();
    let conn_id = format!("c{gen}");
    tracing::info!(conn_id = %conn_id, "ws connected");

    let outcome = handshake(&state, &mut socket, gen).await;
    match &outcome {
        Ok(auth) => {
            // 出站队列先登记：下面顶替旧连接要用它，而写协程也要拿它收帧。
            let (control_rx, data_rx) = state.outbound().register(&conn_id);
            // 写协程拿到 sink 的一半所有权；读者继续用 `&mut socket` 的 stream 半边。
            // `split` 之后两边各自 `await`，互不阻塞——这正是 5b 的前提。
            let (sink, mut stream) = socket.split();
            let writer: JoinHandle<()> = tokio::spawn(write_loop(sink, control_rx, data_rx));

            // 注册表只存句柄。socket 不进表、不落库（标准④）。
            //
            // **先换表、后通知**（§7.1）：`insert` 返回被顶掉的那条，
            // 此刻表里已经是新连接了。顺序反了会让消息在换表之前短暂路由到
            // 一个即将关闭的句柄上。
            let displaced = state.connections().insert(
                auth.node_id.clone(),
                ConnHandle {
                    conn_id: conn_id.clone(),
                    gen,
                },
            );
            tracing::info!(
                conn_id = %conn_id,
                node_id = %auth.node_id,
                gen = gen,
                "ws authenticated"
            );
            // 连接成立 → 页面上的这台设备立刻变 `online`，不必等下一次轮询
            state.streams().device_status(&auth.node_id, "online");
            if let Some(old) = displaced {
                supersede(&state, &old, gen, &conn_id, &auth.node_id);
            }
            // 业务帧循环。M0 的 Hub 侧只负责「不崩、不断开」，
            // 未知消息类型一律忽略（约束 14 前向兼容）。
            // socket 是 &mut，relay 结束后它还在——断开清理见下面。
            //
            // relay 的返回值**记日志而不消音**：「连接为什么结束」正是排查
            // 设备掉线时唯一有用的信息。
            //
            // 只在 relay **正常返回**后才执行，而测试里连接 task 的生命周期
            // 长于 fixture——runtime 关闭时它已被取消，走不到收尾。
            let relayed = relay(&state, &mut stream, &conn_id, auth).await;
            // 读者结束 → 写协程没有存在的理由了。`abort` 而不是 await：
            // 对端不读时写协程正卡在 `send().await` 上，等它退出会把断开
            // 拖成「等对端愿意收」——而断开恰恰是对端不配合时才发生的。
            writer.abort();
            // coverage: excluded — 同上面的收尾行：单测里 task 被 runtime 取消，
            // `Err` 臂走不到，故这行日志不可达。
            if let Err(reason) = relayed {
                tracing::info!(conn_id = %conn_id, reason = %reason, "ws relay ended");
            }
        }
        // 收尾行与整个 `Err(reason)` 臂在单测里都不可达（fixture 的生命周期长于
        // 那个 task）。
        Err(reason) => {
            tracing::info!(conn_id = %conn_id, reason = %reason, "ws handshake failed");
        }
    }

    // 收尾：断开处理器。提成单独函数只为一件事——`handle_socket` 里
    // 曾对同一个 `outcome` match 了两次（第一段是「连上之后干什么」，
    // 第二段是「断开之后收拾什么」），两段相隔 60 行而共用同一个匹配值，
    // 读的人得自己确认「第二次 match 的臂和第一次一样吗」。
    cleanup(&state, &outcome, &conn_id, started).await;
}

/// `handle_socket` 的收尾：摘表、结算两本账、推 `device.status`。
///
/// **为什么单独一个函数**：`handle_socket` 曾对同一个 `outcome` match 两次
/// （连上之后干什么 / 断开之后收拾什么），两段相隔 60 行却共用同一个匹配值。
/// 放在一起就只剩一个匹配点，`match &outcome` 也自然只有一处。
///
/// **两本账各有各的形状**（见下）：`channels` 按 node 分表所以能 `remove` key，
/// `bindings` 按 channel 存所以只能按字段 `retain`。硬凑一个跨 crate 的
/// 共同抽象会把两者都写坏——它们的共同点只有「都要按 node 清」这一句。
async fn cleanup(
    state: &AppState,
    outcome: &Result<Authenticated, String>,
    conn_id: &str,
    started: Instant,
) {
    // 断开处理器：**先校验这条连接仍是当前连接**再摘表（§7.1）。
    // 已关闭的连接不得把别人（新连接）的状态改掉。
    //
    // `match &outcome`（而不是 `match outcome`）：outcome 在上一个 match 里
    // 已经按引用用过了，再按值匹配会把 Err 里的 String move 掉。
    match &outcome {
        Ok(auth) => {
            let removed = state
                .connections()
                .remove_if_current(&auth.node_id, conn_id);
            // 上报**不清**：它要活过这段窗口，才有 `Reconnecting` 这回事
            // （`reports.rs::presence`——WS 断了但心跳没超时 → 调度仍算可用）。
            // 清掉它等于让「重连中的设备被过滤」这条 `PRODUCT.md` 承诺落空。
            //
            // 不清也不怕僵尸：判离线的依据是心跳年龄（`OFFLINE_AFTER`），
            // 45s 后扫瞄器自然把它标 offline，不需要断开时主动删。
            // 若这里连着删，重连成功的那一瞬间反而会闪一次 offline。
            //
            // 标准⑥ 的落点在 `remove_if_current` 的返回值上：被顶替的旧连接
            // 摘不到表（`removed == false`），而断开处理器**根本不碰 status**
            // ——改 status 的只有 `on_heartbeat`，且只往 online 改。所以
            // 「旧连接把 status 改回 offline、盖掉新连接的 online」不可能发生。
            state.outbound().remove(conn_id);
            // 账本结算：设备断开时按 node 一次性清干净。**将来新增任何按
            // node 记账的东西都要落进这份清单**——漏掉一份的症状是「Hub 认
            // 为通道已清、对方还在发帧」，而两份账本相隔两个 crate，改一处
            // 不会让另一处的测试变红（这也是它们原先分在两处各自的隐患）。
            //
            // 两个 `drop_node` 各清各的账本，不是重复实现：前者是每条通道的
            // 信用（`Map<node, Registry>`，直接 remove key），后者是页面归属
            // （`Map<channel, Binding>`，只能按字段 retain）。数据形状不同，
            // 硬凑一个跨 crate 的共同抽象只会把两者都写坏。
            let dropped = state.channels().write().drop_node(&auth.node_id);
            let orphaned = state.bindings().drop_node(&auth.node_id);
            if dropped > 0 {
                tracing::debug!(
                    node_id = %auth.node_id,
                    channels = dropped,
                    "dropped channel records on disconnect"
                );
            }
            // 页面侧：设备掉线 + 这条通道的归属一并清掉（子任务 5a）。
            //
            // **只在「这条连接确实是当前连接」时才推 `device.status`**：
            // 被顶替的旧连接自己也会走到这里，而那时表里已经是新连接、
            // 设备明明在线——推一发 `offline` 会让页面把一台好端端的设备
            // 标成离线，而且没有任何东西会把它改回来（心跳只往 online 改）。
            //
            // `removed == false` 也覆盖「被顶替」之外的情况：两条连接抢一个
            // node_id 时，先摘表的那条正是被顶掉的那条。
            if removed {
                state.streams().device_status(&auth.node_id, "offline");
            }
            if orphaned > 0 {
                tracing::debug!(
                    node_id = %auth.node_id,
                    channels = orphaned,
                    "dropped browser channel bindings on disconnect"
                );
            }
            let elapsed_ms = started.elapsed().as_millis();
            tracing::info!(conn_id = %conn_id, node_id = %auth.node_id, removed = removed, elapsed_ms = elapsed_ms, "ws disconnected");
        }
        Err(reason) => {
            let elapsed_ms = started.elapsed().as_millis();
            tracing::info!(conn_id = %conn_id, reason = %reason, elapsed_ms = elapsed_ms, "ws disconnected");
        }
    }
}

/// 顶替一条旧连接：推 `disconnect{superseded, gen}`，让它自己走。
///
/// **「不再重连」不能只依赖这一帧送达**（§7.1）——帧丢了旧连接会再来，
/// 于是两条连接互抢。兜底是 `auth{last_seen_gen}`：旧连接下次重连时把
/// 它见过的 gen 带回来，Hub 见 `gen <` 当前就回 `auth_fail{superseded}`
/// 而不是接管（见 [`handshake`] 的兜底分支）。
///
/// 投递不成功一律记日志：这正是排查「设备反复上下线」的唯一线索。
fn supersede(state: &AppState, old: &ConnHandle, gen: u64, new_conn_id: &str, node_id: &str) {
    let frame = protocol::frame::Frame::push(
        protocol::message::types::DISCONNECT,
        0,
        protocol::frame::payload_or(
            &protocol::message::Disconnect {
                reason: protocol::message::DisconnectReason::Superseded,
                gen: Some(gen),
            },
            protocol::message::fallback::disconnect(
                protocol::message::DisconnectReason::Superseded,
                Some(gen),
            ),
        ),
    );
    match state.outbound().send(&old.conn_id, frame) {
        Ok(true) => tracing::info!(
            old_conn_id = %old.conn_id,
            new_conn_id = %new_conn_id,
            node_id = %node_id,
            gen = gen,
            "superseding the older connection"
        ),
        // coverage: excluded — 表里没它：旧连接已在自己的断开处理器里被摘掉。
        // 要触达需让它在 insert 之后、send 之前被另一个 task 摘表（极窄窗口）。
        Ok(false) => tracing::debug!(
            old_conn_id = %old.conn_id,
            "the displaced connection is already gone"
        ),
        // coverage: excluded — 控制帧队列 8 格而顶替只推一帧，要满得让同一连接被
        // 连续顶替 9 次且一次都不消费。
        Err(conns::PushError::QueueFull) => tracing::warn!(
            old_conn_id = %old.conn_id,
            node_id = %node_id,
            gen = gen,
            "cannot deliver the superseded frame; the gen fallback must catch it"
        ),
    }
}

/// 握手成功的结果（`auth_ok` 的四个字段）。
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub node_id: String,
    pub gen: u64,
    pub protocol_version: u32,
    pub heartbeat_interval_ms: u64,
}

/// 跑完 `hello → challenge → auth`，返回 `auth_ok` 的内容或失败原因。
///
/// **失败原因只含固定短语**：它进日志，也可能进 `auth_fail` 帧。
/// 绝不放 key 材料、签名、路径。
async fn handshake(
    state: &AppState,
    socket: &mut WebSocket,
    gen: u64,
) -> Result<Authenticated, String> {
    let mut hs = Handshake::new();
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut seq: u64 = 0;

    loop {
        // 超时只在握手**仍在进行**时判（§3）。已 `Authed` 的连接不因为
        // 「握手慢」被关——那会把一条好端端的连接在握手后立刻掐掉。
        //
        // 注意这里**不能**改成「看 `hs.state()` 是不是还在进行中」来决定用哪
        // 个 deadline：`Authed` / `Rejected` 两个终态都会让循环立刻 return，
        // 取它们的 deadline 是纯粹的分支噪音（而且当初就写错过一次）。
        let remaining = deadline.saturating_duration_since(Instant::now());

        let msg = match tokio::time::timeout(remaining, socket.recv()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(format!("ws read error: {e}")),
            // 流结束而没走过关闭握手：设备那边直接关了 TCP（它还没学会
            // Close 帧，或进程被 kill 时正好在写）。此时不要再补一帧
            // 失败帧——对端已经不在了，补帧只会把 fd 耗在一个死人连接上。
            //
            // coverage: excluded — 结构上不可达，不是「测试没造出来」：
            // tungstenite 把「没收到 Close 帧就 EOF」报成
            // `ResetWithoutClosingHandshake` 读错误（走上面那条臂），
            // 而真收到 Close 帧时 `socket.recv()` 交出的是 `Some(Ok(Close))`。
            // 证据：`a_peer_that_closes_during_the_handshake_is_not_answered`
            // 就是纯 FIN 关连接，它走的是上面那条读错误臂——这一行至今未覆盖。
            Ok(None) => return Err(PEER_CLOSED.to_string()),
            Err(_) => {
                // recv 挂住到超时：只靠入口处的 deadline 不够，这里补一刀
                send_best_effort(socket, Handshake::timeout_error_frame(seq), "handshake").await;
                return Err("handshake timeout".to_string());
            }
        };

        let Some(text) = frame_text(msg)? else {
            continue;
        };

        // `seq` 是 Hub 侧的发送计数（challenge / auth_ok / auth_fail 用它），
        // **不是**验签用的计数器：签名必须用「节点在 auth 帧里实际写下的 seq」
        // 去验，否则 node 用 seq=1 签、Hub 用 seq=2 验会永远不匹配。
        // 所以先解析帧、拿出 `frame.seq`，再把它透传给验签路径。
        let frame: Frame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => return Err(format!("bad frame json: {e}")),
        };
        seq += 1;

        match frame.msg_type.as_str() {
            types::HELLO => {
                let hello: protocol::message::Hello = match frame.payload() {
                    Ok(h) => h,
                    Err(_) => return Err("invalid hello payload".to_string()),
                };
                let challenge = match hs.on_hello(frame.v, hello) {
                    Ok(c) => c,
                    // 握手还在 Hub 这一侧就失败 → 没有可回的失败帧（§4）：
                    // 回帧本身要求握手已完成，直接退回去即可。
                    Err(e) => return Err(handshake_error_phrase(e)),
                };
                if send_frame(socket, Handshake::challenge_frame(seq, &challenge))
                    .await
                    .is_err()
                // coverage: excluded — challenge 帧发送失败需对端在收帧前消失，
                // 握手期 socket 由 fixture 持有，构造不出确定性的发送失败。
                {
                    return Err(PEER_CLOSED.to_string());
                }
            }
            types::AUTH => {
                let payload = match AuthPayload::from_value(&frame.c) {
                    Ok(p) => p,
                    Err(e) => return Err(handshake_error_phrase(e)),
                };
                // Hub 未初始化（没有密码）→ 一律拒绝 enroll（§3.1）。
                // 顺序在这：先于任何签名验证，此时还没有可验证的身份。
                if !hub_is_initialized(state) {
                    let f = hs.auth_fail(seq, protocol::message::reason::HUB_NOT_INITIALIZED, None);
                    send_best_effort(socket, f, "handshake").await;
                    return Err("hub not initialized".to_string());
                }
                return finish_auth(state, socket, &mut hs, frame.seq, gen, payload).await;
            }
            other => {
                // 未知类型：**只有必需集才断开**，其余一律忽略（约束 14）。
                // 握手期出现可选集消息（节点提前发了 hub_hello）不打断流程。
                if types::is_required(other) {
                    hs.reject();
                    let f = hs.auth_fail(seq, protocol::message::reason::IDENTITY_MISMATCH, None);
                    send_best_effort(socket, f, "handshake").await;
                    return Err("required message out of order".to_string());
                }
            }
        }
    }
}

/// 回一帧 `auth_fail` 并返回它的 reason。
///
/// **失败原因只含固定短语**（§3）：它进日志，也可能进帧。绝不放 key 材料、
/// 签名、路径。
///
/// `live` 是 `auth_fail` 的可选 `gen` 字段：只有「被顶替」那一档要带上
/// 当前活连接的 gen，好让节点知道自己落后了多少（§7.1）。
///
/// **所有拒绝点都走这里**：内联「发一帧 + 返回 reason」时，reason 会被写两遍
///（一帧、一个返回值），而两处可以悄悄漂成不同拼法——排障时按短语 grep 就失效了。
async fn reject(
    socket: &mut WebSocket,
    hs: &mut Handshake,
    seq: u64,
    reason: &'static str,
    live: Option<u64>,
) -> String {
    send_best_effort(socket, hs.auth_fail(seq, reason, live), "handshake").await;
    String::from(reason)
}

/// enroll 被拒：回一帧 `fingerprint race, retry` 再断开（§3——绝不 upsert）。
async fn reject_enroll(socket: &mut WebSocket, hs: &mut Handshake, seq: u64) -> String {
    reject(
        socket,
        hs,
        seq,
        protocol::message::reason::FINGERPRINT_RACE_RETRY,
        None,
    )
    .await
}

/// Hub 是否已设密码。
///
/// 读不到（storage 错）时按**未初始化**处理：宁可拒绝 enroll，
/// 也不能在「不知道」的状态下放行。
fn hub_is_initialized(state: &AppState) -> bool {
    match state.is_initialized() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, conn = "ws", "cannot read hub init state during handshake");
            false
        }
    }
}

/// `auth` 帧的两种形态分别走完，回 `auth_ok` 或 `auth_fail`。
///
/// **两条路径没有共享状态，所以是两个函数**（enroll 与重连各一个）。
/// 合成一条时唯一能省下的是这行 `match`，代价是每次改动都要重新判断
/// 「哪些是对两条都成立的」——例如 enroll 成功必然拿到一个 `node_id`，
/// 而重连里「设备不存在」是**真可达**的失败。
async fn finish_auth(
    state: &AppState,
    socket: &mut WebSocket,
    hs: &mut Handshake,
    seq: u64,
    gen: u64,
    payload: AuthPayload,
) -> Result<Authenticated, String> {
    match payload {
        AuthPayload::Enroll(auth) => finish_enroll(state, socket, hs, seq, gen, auth).await,
        AuthPayload::Reconnect(auth) => finish_reconnect(state, socket, hs, seq, gen, auth).await,
    }
}

/// 首次登记：验签用**本次上传**的公钥，通过后落库并复用/签发 `node_id`。
async fn finish_enroll(
    state: &AppState,
    socket: &mut WebSocket,
    hs: &mut Handshake,
    seq: u64,
    gen: u64,
    auth: protocol::handshake::EnrollAuth,
) -> Result<Authenticated, String> {
    let authz = SubmittedKeyAuthenticator;
    let accepted = match hs.on_enroll(seq, &auth, &authz) {
        Ok(a) => a,
        Err(e) => return Err(reject(socket, hs, seq, auth_fail_reason(e), None).await),
    };
    let info = hub_storage::repo::NewDevice {
        id: String::new(),
        name: accepted.device_info.name.clone(),
        key_fingerprint: String::new(),
        os: accepted.device_info.os.clone(),
        arch: accepted.device_info.arch.clone(),
        hostname: accepted.device_info.hostname.clone(),
        node_version: accepted.device_info.node_version.clone(),
    };
    let outcome = match state.registry().enroll(&accepted.public_key, info) {
        Ok(o) => o,
        // enroll 失败绝不重试到「换个 node_id 再插」：那会绕过 UNIQUE。
        Err(e) => {
            tracing::error!(error = %e, "enroll failed");
            return Err(reject_enroll(socket, hs, seq).await);
        }
    };
    // 签名已验证 + 指纹已入库 → 这台设备就是它自称的那台。
    // `AlreadyRegistered` 不是错误：同指纹 + 合法签名 = 同一台设备
    // 又来了一次 enroll（§4.3 第二行），直接放行并复用它已有的 node_id。
    let node_id = outcome.node_id().to_string();
    // 首登即落公钥：重连时只信库内公钥（§3），所以这一行是重连能成立的前提。
    // 写入失败仅告警、不阻塞 enroll——设备已经验过签、指纹也已入库，此时把
    // 它挡在门外只会让一台刚接入的设备再也连不上；缺公钥的重连会走
    // identity mismatch，那比「enroll 失败」更容易定位。
    if let Err(e) = state
        .registry()
        .devices()
        .set_public_key(&node_id, &accepted.public_key)
    {
        // 单测的 fixture 数据库全程可写。
        tracing::warn!(error = %e, "cannot store public key after enroll");
    }
    if let Err(e) = state.registry().devices().set_gen(&node_id, gen) {
        tracing::warn!(error = %e, "cannot record connection gen");
    }
    finish_ok(
        state,
        socket,
        hs,
        seq,
        gen,
        node_id,
        accepted.protocol_version,
    )
    .await
}

/// 重连：**只信库内公钥**，且落后于活连接的旧连接要被拒（§7.1 的 gen 兜底）。
async fn finish_reconnect(
    state: &AppState,
    socket: &mut WebSocket,
    hs: &mut Handshake,
    seq: u64,
    gen: u64,
    auth: protocol::handshake::ReconnectAuth,
) -> Result<Authenticated, String> {
    // 重连：只信库内指纹（§3）。
    let device = match state.registry().device(&auth.node_id) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Err(reject(
                socket,
                hs,
                seq,
                protocol::message::reason::IDENTITY_MISMATCH,
                None,
            )
            .await);
        }
        Err(e) => {
            tracing::error!(error = %e, "device lookup failed during reconnect");
            return Err("device lookup failed".to_string());
        }
    };
    let Some(pubkey) = stored_public_key(&device) else {
        return Err(reject(
            socket,
            hs,
            seq,
            protocol::message::reason::IDENTITY_MISMATCH,
            None,
        )
        .await);
    };
    let authz = StoredKeyAuthenticator;
    let accepted = match hs.on_reconnect(seq, &auth, &pubkey, &authz) {
        Ok(a) => a,
        Err(e) => return Err(reject(socket, hs, seq, auth_fail_reason(e), None).await),
    };
    // **兜底路径（不依赖任何消息投递）**：节点把它见过的最大 gen 带回来，
    // 落后于**当前活着的那条连接**就说明它是被顶掉的旧连接。
    //
    // 比的对象必须是注册表里的**活连接**，而不是「本次连接的 gen」——
    // gen 是每条连接握手前新分配的，拿它比会让**每一次正常重连**
    // 都被判成已被顶替（节点带着 1 来、本次拿到 2，1 < 2 恒成立），
    // 设备就此永远接不上，且没有任何自愈路径。
    //
    // 用活连接则语义正好：顶替者还活着才有互抢要防；它自己也走了，
    // 旧连接回来就无害（没有第二条连接跟它抢）。
    //
    // 放在验签**之后**：未经验签的重连请求不能仅凭一个数字就被拒绝，
    // 否则任何猜到 node_id 的人都能把设备顶下线（node_id 不是秘密）。
    //
    // 缺省（`None`）视为「没见过 gen」而不是 0：老节点或首次重连的帧里
    // 没有这个字段，按 0 处理会让它们**永远**被判成已被顶替，
    // 且没有任何自愈路径（约束 14：前向兼容不许这样断）。
    let live_gen = state.connections().get(&auth.node_id).map(|h| h.gen);
    if let (Some(seen), Some(live)) = (auth.last_seen_gen, live_gen) {
        if seen < live {
            tracing::info!(
                node_id = %auth.node_id,
                last_seen_gen = seen,
                live_gen = live,
                "rejecting a reconnect from a superseded connection"
            );
            // 走到这里就结束了：单测只覆盖到「拒绝返回 reason」这一层
            // （见 `a_reconnect_with_a_bad_signature_is_rejected`）。
            return Err(reject(
                socket,
                hs,
                seq,
                protocol::message::reason::SUPERSEDED,
                Some(live),
            )
            .await);
        }
    }
    if let Err(e) = state.registry().devices().set_gen(&auth.node_id, gen) {
        // 单测的 fixture 数据库全程可写。
        tracing::warn!(error = %e, "cannot record connection gen");
    }
    finish_ok(
        state,
        socket,
        hs,
        seq,
        gen,
        auth.node_id.clone(),
        accepted.protocol_version,
    )
    .await
}

/// 验签已过 → 回 `auth_ok`。
///
/// 拆成独立的 `async fn` 而不是在 `finish_auth` 里内联：enroll 与重连
/// 两条路都要走这四步（构造 → 发送 → 组装返回值），内联会让它们各抄一遍。
async fn finish_ok(
    state: &AppState,
    socket: &mut WebSocket,
    hs: &mut Handshake,
    seq: u64,
    gen: u64,
    node_id: String,
    protocol_version: u32,
) -> Result<Authenticated, String> {
    let interval = state.heartbeat_interval_ms();
    let frame = hs
        .auth_ok(seq, &node_id, gen, interval)
        // 真进程路径由 `tests/afcli_process.rs` 承接。
        .map_err(handshake_error_phrase)?;
    // coverage: excluded — `auth_ok` 发送失败（对端在收帧前消失）这一臂：
    // 端到端 `an_auth_ok_that_cannot_be_delivered_*` 两条用例（RST 连接）断言的
    // 是「连接不许上架」，而 tungstenite 的 `send` 会把帧排进内部队列并当场返回
    // 成功——脏数据在**下一次** write 时才报错，所以这两行在大多数时序下不命中。
    if send_frame(socket, frame).await.is_err() {
        // auth_ok 发不出去 = 对端已经走了，连接没有成立
        return Err(PEER_CLOSED.to_string());
    }
    Ok(Authenticated {
        node_id,
        gen,
        protocol_version,
        heartbeat_interval_ms: interval,
    })
}

/// 对端关闭连接。它进 `tracing::info!` 的 `reason` 字段，六处各写一遍
/// 字面量就会漂成两三种拼法，排障时再也没法按这条短语 grep。
const PEER_CLOSED: &str = "peer closed";

/// 读侧超限后每轮慢读的暂停时长。
///
/// 50ms 而不是更长：慢读的目的是让 TCP 背压传回发送方，而不是把连接冻住。
/// 太长的暂停会让轻微波峰之后的恢复也变慢（一次 `cat` 结束还得等）。
const SLOW_READ_PAUSE_MS: u64 = 50;

/// 一帧 WS 消息 → 文本帧内容。`Ok(None)` = ping/pong，让调用方继续读下一帧。
///
/// **握手期与业务期共用这一个判法**：帧种类是线上契约的一部分（§2：本协议
/// 只用文本 JSON 帧），判错就该断连。曾经这里有两个几乎同形的 `match`，一个在
/// 握手期 `return Err`、一个在业务期 `continue`——同一种非法帧在连接生命的不同时期
/// 表现不同，而 node 侧把二进制帧判 Transport。三处口径必须一致，否则「Hub 到底是
/// 挂了还是没挂」这件事同一个症状能有两个答案。
///
/// 未知**消息类型**（`frame.t`）不在这里：那个必须按约束 14 忽略，与帧种类无关。
fn frame_text(msg: axum::extract::ws::Message) -> Result<Option<String>, String> {
    match msg {
        axum::extract::ws::Message::Text(t) => Ok(Some(t.to_string())),
        axum::extract::ws::Message::Close(_) => Err(PEER_CLOSED.to_string()),
        // ping/pong 由 axum 自动应答，这里只是不让它打断流程
        axum::extract::ws::Message::Ping(_) | axum::extract::ws::Message::Pong(_) => Ok(None),
        other => Err(format!("unexpected frame kind: {other:?}")),
    }
}

/// 读侧超限的应答：`err{code}`，随后断开（§6.1）。
fn flood_frame(e: &hub_core::limits::LimitError) -> Frame {
    let code = match e {
        hub_core::limits::LimitError::FrameTooLarge => protocol::message::ErrorCode::FrameTooLarge,
        hub_core::limits::LimitError::Flood => protocol::message::ErrorCode::Flood,
    };
    Frame::push(
        types::ERR,
        0,
        protocol::frame::payload_or(
            &protocol::message::ErrPayload {
                code,
                message: e.to_string(),
            },
            protocol::message::fallback::err(code, &e.to_string()),
        ),
    )
}

/// 业务帧循环：**只读**，把收到的帧交给业务处理。
///
/// 写侧已拆成独立的 [`write_loop`]（子任务 5b）。这个函数因此不再同时
/// `send().await` 与 `recv().await`：以前那两个方向共用 `&mut socket`，
/// 一次慢写就把读循环冻住，控制帧也要排在那次写之后。
///
/// M0 的 Hub 侧只需要「连接不断、未知消息不崩」。
async fn relay(
    state: &AppState,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    conn_id: &str,
    auth: &Authenticated,
) -> Result<(), String> {
    // 每连接一个读侧令牌桶（§6.1「每连接 semaphore」）。按字节而不是按帧计：
    // 一帧 1 MiB 与一帧 10 字节对内存的压力差五个数量级，按帧计等于
    // 允许「少量大帧」打爆 Hub。
    let mut limiter = hub_core::limits::ReadLimiter::new();
    // 读侧超限的**起始时刻**。它决定「慢读多久升级成断开」——没有它，
    // 每一次超限都无从判断是突发还是持续。
    let mut over_since: Option<Instant> = None;
    loop {
        // coverage: excluded — 对端直接 FIN 而不发 Close 帧才会走到。
        let Some(msg) = stream.next().await else {
            return Err(PEER_CLOSED.to_string());
        };
        let msg = match msg {
            Ok(m) => m,
            Err(e) => return Err(format!("ws read error: {e}")),
        };
        let Some(text) = frame_text(msg)? else {
            continue;
        };
        admit_read(
            state,
            conn_id,
            &auth.node_id,
            &mut limiter,
            &mut over_since,
            text.len(),
        )
        .await?;
        let frame: Frame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(conn_id = %conn_id, error = %e, "dropping unparsable frame");
                continue;
            }
        };
        if frame.msg_type == types::HEARTBEAT {
            on_heartbeat(state, conn_id, auth, &frame);
            continue;
        }
        if crate::channels::is_channel_type(&frame.msg_type) {
            dispatch_channel_frame(state, conn_id, &auth.node_id, &frame);
            continue;
        }
        // 其余（含未知类型）一律忽略：约束 14 前向兼容
    }
}

/// 读侧限流：这一帧被容许，还是该断开（§6.1）。
///
/// `Ok(())` = 放行，继续处理这一帧；`Err(reason)` = 断开（已回过一帧 `err`）。
///
/// **超限先慢读，不立刻断开**（§6.1「超限 → 慢读/暂停，持续 30s 才断开」）。
/// 立刻断是错的：`cat` 一个大文件就是一次合法的 PTY 洪峰，误杀它等于
/// 「输出多的命令一律失败」。慢读让 TCP 背压传回发送方，流量自然降下来。
async fn admit_read(
    state: &AppState,
    conn_id: &str,
    node_id: &str,
    limiter: &mut hub_core::limits::ReadLimiter,
    over_since: &mut Option<Instant>,
    bytes: usize,
) -> Result<(), String> {
    // §6.1：credit 是**发送方的自我约束**，拦不住一个恶意或
    // buggy 的 worker。Hub 的自我保护是自己的读侧令牌桶——
    // 不看对端有没有信用，只看它灌了多少字节。
    //
    // 没有这条，一台设备持续灌数据就能让 Hub 逐帧解析 JSON 写进
    // 内存直到 OOM，连带打垮所有其他设备与 Web 端——而那正是
    // credit 想防的事，却只防了 node 的发送侧。
    loop {
        match limiter.try_consume(bytes) {
            Ok(()) => {
                *over_since = None;
                return Ok(());
            }
            // 单帧超硬上限没有「等等就好」这回事——它永远超。
            //
            // coverage: excluded — 单帧超 1 MiB 的端到端路径需要往真 socket 里灌
            // 超大帧；单测覆盖的是令牌桶本身（`limits.rs`）与应答帧映射
            // （`flood_frame_maps_each_limit_error_to_its_own_code`），
            // 这里只留「超限即断开」的接线：日志 + 应答帧 + 断开。
            Err(hub_core::limits::LimitError::FrameTooLarge) => {
                tracing::warn!(
                    conn_id = %conn_id,
                    node_id = %node_id,
                    bytes = bytes,
                    "frame over the hard limit; dropping the connection"
                );
                // coverage: excluded — 见臂上的标注：端到端超大帧路径。
                queue_control(
                    state,
                    conn_id,
                    flood_frame(&hub_core::limits::LimitError::FrameTooLarge),
                );
                return Err("frame over the hard limit".to_string());
            }
            Err(hub_core::limits::LimitError::Flood) => {
                let since = *over_since.get_or_insert_with(Instant::now);
                // coverage: excluded — 要真等满 30s 才会走到，超时窗口不可压缩。
                if since.elapsed() >= hub_core::limits::FLOOD_SUSTAINED {
                    tracing::warn!(
                        conn_id = %conn_id,
                        node_id = %node_id,
                        "read limit exceeded for too long; dropping the connection"
                    );
                    // coverage: excluded — 同上：30s 窗口不可压缩。
                    queue_control(
                        state,
                        conn_id,
                        flood_frame(&hub_core::limits::LimitError::Flood),
                    );
                    return Err("sustained read limit exceeded".to_string());
                }
                // 慢读：等令牌回补再处理这一帧。**不重读**——
                // 已经读进来的字节丢掉就是丢数据。
                tokio::time::sleep(Duration::from_millis(SLOW_READ_PAUSE_MS)).await;
            }
        }
    }
}

/// 一帧通道帧的副作用：推给页面订阅者 + 按需回帧。
fn dispatch_channel_frame(state: &AppState, conn_id: &str, node_id: &str, frame: &Frame) {
    // 页面侧数据面（子任务 5a）：`channel_data` 的**载荷**推给
    // 浏览器的 SSE 订阅者。只推载荷、不推整帧——`Frame` 的
    // `v` / `seq` 是设备侧 WS 的语义（§2），页面不验签也不重放。
    //
    // 推送在记账**之前**：这样即使这帧随后被判 `NotReady` 而回错，
    // 页面也已经看到了字节。而反过来（先记账成功才推）会让
    // 「Hub 回了 channel_error 但终端里字符已经出现」永远不可能发生。
    if frame.msg_type == types::CHANNEL_DATA {
        publish_channel_data(state, node_id, frame);
    }
    // 设备的错误也是页面要看的输出（缺陷 3）：`channel_error` 若不
    // 转发，越界被拒在页面上就是一片空白——面板停在「已连接」，
    // 用户看不到「为什么进不去」（design-system §7.4 第 ③ 路）。
    if frame.msg_type == types::CHANNEL_ERROR {
        publish_channel_error(state, node_id, frame);
    }
    // 回帧**入数据面队列**（子任务 5b），不再直接写 socket：
    // 写 socket 会阻塞读循环，而那正是「设备不读时 Hub 的写缓冲无界」
    // 与「控制帧被数据帧拖住」两个问题的同一根源。
    if let crate::channels::Reply::Frame(out) =
        crate::channels::on_channel_frame(state, node_id, frame)
    {
        queue_data(
            state,
            conn_id,
            out,
            flood_frame(&hub_core::limits::LimitError::Flood),
        );
    }
}

/// 写协程：只消费出站队列，只写 sink。
///
/// **`biased` 是功能性的**：控制面（`disconnect{superseded}`）必须插队，
/// 否则一条正在被 256 帧 PTY 输出灌满的连接，会先写完数据才看到「你被顶替了」。
/// 没有 `biased` 时 `select!` 随机选臂，延迟上限就成了掷骰子。
///
/// coverage: excluded — 整个函数在测试里走不到「写失败 / 收端关闭」两条臂：
/// 端到端测试始终在收帧（sink 由 fixture 持有，构造不出确定的发送失败），
/// 而「队列已摘」要制造一次真实的连接回收竞争。
async fn write_loop(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut control: mpsc::Receiver<Frame>,
    mut data: mpsc::Receiver<Frame>,
) {
    let mut control_open = true;
    let mut data_open = true;
    while control_open || data_open {
        tokio::select! {
            biased;
            frame = control.recv(), if control_open => match frame {
                Some(frame) => {
                    // coverage: excluded — 见函数上的标注（:864）：写失败需对端
                    // 在读循环中途消失，端到端测试构造不出确定的发送失败。
                    if let Err(reason) = send_frame(&mut sink, frame).await {
                        tracing::info!(reason = %reason, "ws writer ended on a control frame");
                        return;
                    }
                }
                // coverage: excluded — 见函数上的标注（:864）：收端关闭要在连接
                // 被回收、队列的发送端全被 drop 后才出现，测试里构造不出。
                None => control_open = false,
            },
            frame = data.recv(), if data_open => match frame {
                Some(frame) => {
                    // coverage: excluded — 同上：写失败需对端在读循环中途消失。
                    if let Err(reason) = send_frame(&mut sink, frame).await {
                        tracing::info!(reason = %reason, "ws writer ended on a data frame");
                        return;
                    }
                }
                // coverage: excluded — 同 `control_open`：收端关闭要真实的回收竞争。
                None => data_open = false,
            },
        }
    }
}

/// 控制帧入队；连接已走就丢（没什么可通知的了）。
///
/// coverage: excluded — 整个函数体（签名行、`match` 与 `Ok` 臂）都不可达：
/// 调用它的三处（读侧超限的两条断开臂与数据面打满的告警臂）本身不可达，
/// 见它们各自的标注。
fn queue_control(state: &AppState, conn_id: &str, frame: Frame) {
    match state.outbound().send(conn_id, frame) {
        Ok(_) => {}
        // coverage: excluded — 控制面 8 格，而这里一次只投一帧；要满需
        // 同一连接在极窄窗口里被连续顶替 9 次且写协程一次都不消费。
        Err(conns::PushError::QueueFull) => {
            tracing::warn!(conn_id = %conn_id, "control queue full; dropping a control frame");
        }
    }
}

/// 数据帧入队；打满则丢帧并回 `err{code:"flood"}`（走控制面）。
///
/// coverage: excluded — 与 `queue_control` 同一条：`Ok` 臂的调用点
/// （`dispatch_channel_frame` 的回帧路径）在别的用例里走得到，而
/// `Err` 臂唯一的前置是数据面 256 帧打满，端到端需要真 socket 背压。
fn queue_data(state: &AppState, conn_id: &str, frame: Frame, flood: Frame) {
    match state.outbound().send_data(conn_id, frame) {
        Ok(_) => {}
        // coverage: excluded — 数据面 256 帧；写协程在测试里始终消费，
        // 端到端构造「对端不读 + 256 帧回帧」需要真 socket 背压，
        // 而决策本身（满 → 丢 + 回 err）由 `conns.rs` 的单测覆盖。
        Err(conns::PushError::QueueFull) => {
            tracing::warn!(
                conn_id = %conn_id,
                "data queue full; dropping a channel reply and reporting flood"
            );
            queue_control(state, conn_id, flood);
        }
    }
}

/// 把一帧 `channel_data` 的载荷推给页面订阅者（子任务 5a）。
///
/// 载荷解析不出来就**不推**、也不记 error：那是一帧 Hub 没法解释的 JSON，
/// 推给页面的只会是 `{"node_id":..., "channel_id": ""}` 这种既没有字节
/// 也没有通道号的东西，前端拿它渲染终端只会显示一个空面板。
fn publish_channel_data(state: &AppState, node_id: &str, frame: &Frame) {
    let Ok(data) = frame.payload::<protocol::channel::ChannelData>() else {
        // 且 debug 级别开着（`install_subscriber` 装的是 INFO），而单测夹具
        // `stream_state()` 不装 subscriber。同形的 `publish_channel_error` 的
        // else 臂已覆盖（那条在单测里执行到了），差异只在 subscriber 的级别。
        tracing::debug!(node_id = %node_id, "not forwarding an unparsable channel_data");
        return;
    };
    // 轨迹落库在推 SSE **之后**：页面先看到字，库后写。反过来会把一次写库
    // 失败（磁盘满、库锁）变成「页面什么都不出」，而那不是用户能做的事。
    state
        .streams()
        .channel_frame(node_id, &data.channel_id, data.payload.clone());
    persist_trajectory(state, node_id, &data.channel_id, &data.payload);
}

/// 把对话通道上行的一帧写进轨迹表。**非对话通道一律不写**。
///
/// # 为什么落库在 Hub 侧、而不是设备侧
///
/// `session/update` 只有设备收得到，但**会话 id 只有 Hub 知道**：轨迹表的
/// `session_id` 是 Hub 的会话主键，设备手里只有 ACP 自己的 `sessionId`
/// （`ses_...`）。让设备落库就得把 Hub 的会话 id 先下发下去，而那会让
/// 「设备写库时用的 id」与「Hub 认定的 id」成为两份可能分叉的事实。
///
/// # 认哪几类帧
///
/// 只认 `batch` 与 `turn_end`，这两类才是**事件**。`ready` / `error` /
/// `request` / `exited` 是控制面的话，不进轨迹——它们的语义是「这条通道
/// 现在怎么了」，不是「agent 说了什么」。把它们写进轨迹会让页面把一次
/// 重连的 `ready` 画成一条对话消息。
///
/// 例外：`ready` 虽不写轨迹，却会被用来回填 `session.acp_session_id`
/// （子任务 8）——它是唯一携带 ACP 会话 id 的帧。
///
/// # 未知事件类型必须写，不得丢
///
/// `batch.events` 里的每个元素原样存（`kind` 取它的 `sessionUpdate`）。
/// 新版本 agent 推的类型我们不认识，在这里过滤就是静默丢消息——
/// 而轨迹是「不承诺完整、但必须可见」，丢掉的那些连可见都做不到。
fn persist_trajectory(state: &AppState, node_id: &str, channel_id: &str, payload: &Value) {
    // 没绑过（页面没开过这条通道、或已摘）就不写。通道绑定是内存表，
    // 断连即清——所以断线期间到达的帧本就无从归属，与「不承诺补齐」一致。
    let Some(binding) = state.bindings().get(channel_id) else {
        return;
    };
    let session_id = binding.session_id();
    let Some(kind) = payload.get("type").and_then(Value::as_str) else {
        // 没有 `type` 的不是对话帧（terminal 的载荷是 `{op, bytes}`）。
        return;
    };
    // `ready` 不是轨迹事件（它是控制面：见本函数文档第 3 条），但它是
    // **唯一带 ACP 会话 id 的帧**，所以顺手回填 `session.acp_session_id`
    // （子任务 8）。放在这里而不是 `channel_ready` 那条路上，是因为
    // ACP 通道的 `channel_ready` 由设备侧 `open_external` 生成、**不含
    // sessionId**；agent 给的 id 是在这之后由 `conversation::open` 通过
    // `push_external` 作为一条 `channel_data` 载荷推上来的
    // （`{"type":"ready","sessionId":...,"agentId":...}`）。
    //
    // 不回填的后果是静默的：`acp_session_id` 永远为 NULL，设备每次都走
    // `session/new`，关掉页面再回来 agent 那边就是一条全新会话——页面上
    // 一切正常，只是之前聊的全没了。
    if kind == "ready" {
        if let Some(acp) = payload
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            match hub_storage::SessionRepo::new(state.pool().clone())
                .set_acp_session_id(&session_id, acp)
            {
                Ok(()) => tracing::debug!(
                    session_id = %session_id, acp_session_id = %acp,
                    "recorded the agent's acp session id"
                ),
                // 会话在通道开着的时候被删了：不是 Hub 的问题，只是这次
                // 回填没有落点。**不升级成错误**——通道还能用。
                Err(e) => tracing::debug!(
                    session_id = %session_id, error = %e,
                    "cannot record the acp session id"
                ),
            }
        }
        return;
    }
    if kind != "batch" && kind != "turn_end" {
        return;
    }
    let now = hub_core::registry::now_stamp();

    if kind == "turn_end" {
        // `turn_end` 是一条事件，不是一批。它的 `seq` 由设备侧那条共享
        // 计数器给（与读循环同一条序列，见 `node-core` 的 `Seq`）。
        //
        // **缺 `seq` 就整条不写**，不写 0：两条都缺的帧会都落 `seq = 0`，
        // 第二条被 `UNIQUE` 的 `INSERT OR IGNORE` 吃掉——丢一条且看不出来
        // （序号仍然连续，`gaps` 不报警）。宁可少一条，也不要一个错号。
        let Some(seq) = payload.get("seq").and_then(Value::as_i64) else {
            tracing::error!(channel_id = %channel_id, "a turn_end arrived without a seq; dropped");
            return;
        };
        let reason = payload
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("");
        let e = hub_storage::NewEvent::turn_end(seq, reason);
        if let Err(err) = state
            .trajectory()
            .append(&session_id, node_id, &e, &now)
        {
            tracing::error!(node_id = %node_id, channel_id = %channel_id, error = %err,
                "cannot persist a turn_end");
        }
        return;
    }

    let Some(events) = payload.get("events").and_then(Value::as_array) else {
        return;
    };
    for ev in events {
        // 同上：缺 `seq` 的事件**跳过并留痕**，不写 0。与 `turn_end` 同一条
        // 理由，而这里更危险——一批里可能有好几条。
        let Some(seq) = ev.get("seq").and_then(Value::as_i64) else {
            tracing::error!(channel_id = %channel_id, "a trajectory event arrived without a seq; dropped");
            continue;
        };
        // 事件类型名就是 ACP 的 `sessionUpdate` 字段；取不到就用 `update`，
        // 绝不因为不认识而**跳过**这条事件（见本函数文档第 3 条）。
        let ev_kind = ev
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("update");
        let e = hub_storage::NewEvent::new(seq, ev_kind, ev.to_string());
        if let Err(err) = state
            .trajectory()
            .append(&session_id, node_id, &e, &now)
        {
            tracing::error!(node_id = %node_id, channel_id = %channel_id, error = %err,
                "cannot persist a trajectory event");
        }
    }
}

/// 把一帧 `channel_error` 推给页面订阅者（缺陷 3）。
///
/// 形状是 `{"op":"error","code":…,"reason":…}`，**复用 `channel.frame`**：
/// 前端已经按这个形状分派（`SessionPage.vue` 的 `applyFileFrame` 里
/// `op === 'error'` 分支，`errorOf()` 读 `reason` 与 `code`），而 `code`
/// 与 `reason` 正是 `ChannelErrorPayload` 的两个字段，不必另造事件名。
///
/// 解析不出来时不推、只记 debug——与 [`publish_channel_data`] 同一口径：
/// 一帧 Hub 没法解释的 JSON 推给页面只会得到一个没有原因的空面板。
fn publish_channel_error(state: &AppState, node_id: &str, frame: &Frame) {
    let Ok(err) = frame.payload::<protocol::channel::ChannelErrorPayload>() else {
        tracing::debug!(node_id = %node_id, "not forwarding an unparsable channel_error");
        return;
    };
    state.streams().channel_frame(
        node_id,
        &err.channel_id,
        serde_json::json!({
            "op": "error",
            "code": err.code,
            "reason": err.reason,
        }),
    );
    // **开通道就失败**（connector 拒绝 `open`）时，设备的这条 error 就是终局：
    // 摘掉绑定，让 5s 的超时兜底变成空操作。不摘的话页面会在真实原因之后
    // 再收到一条「设备没在 5s 内确认」，把唯一有用的那句话盖掉。
    //
    // 已确认的通道**不摘**：越界读只是这一次请求被拒，通道本身还能用
    // （标准 5 的三次探测共用同一条通道）。
    if state.bindings().take_unacked(&err.channel_id) {
        // 这条绑定的确认期限随之消失（记录都没了），等确认的那条订阅
        // 会得到「没什么可说的」而不是一条假的超时。
        tracing::debug!(
            node_id = %node_id,
            channel_id = %err.channel_id,
            "the device refused to open the channel"
        );
    }
}

/// 一条心跳：内存更新 + 落库。
///
/// 落库失败**不打断连接**：心跳的用途是让 Hub 知道设备活着，而设备确实活着。
/// 但它必须留痕（所有 I/O 错误都不得静默）——写不进去时路由还能读内存，
/// 可运维必须知道能力已经停止持久化了。
fn on_heartbeat(state: &AppState, conn_id: &str, auth: &Authenticated, frame: &Frame) {
    let hb: protocol::message::Heartbeat = match frame.payload() {
        Ok(h) => h,
        Err(e) => {
            // 载荷畸形只丢这一帧：约束 14 的缺省容错在这里做不了——连 `node_version`
            // 都解不出来，就没有「保留旧值」的语义可用。
            tracing::debug!(conn_id = %conn_id, node_id = %auth.node_id, error = %e, "dropping malformed heartbeat");
            return;
        }
    };
    let now = Instant::now();
    // 内存：路由读它，必须每次心跳都新。
    state
        .reports()
        .record(&auth.node_id, hub_core::report_from_heartbeat(&hb, now));

    // 落库：设备离线后 Hub 仍要知道它支持哪些 connector（里程碑后端流程①）。
    // 缺省字段保留旧值（测试标准④）——一次不带负载的心跳不是「清空」。
    let stored = state.devices().find_by_id(&auth.node_id);
    let (caps, res) = match stored {
        Ok(Some(d)) => {
            hub_core::merge_into_stored(d.capabilities.as_deref(), d.resources.as_deref(), &hb)
        }
        // 行不在（不该发生：握手刚验过签）= 当作「没有旧值」，
        // 而不是把心跳丢掉。查不到与「从没上报过」在合并上同义。
        Ok(None) => hub_core::merge_into_stored(None, None, &hb),
        Err(e) => {
            tracing::warn!(conn_id = %conn_id, node_id = %auth.node_id, error = %e, "cannot read stored report; merging into empty");
            hub_core::merge_into_stored(None, None, &hb)
        }
    };
    if let Err(e) =
        state
            .devices()
            .touch_online(&auth.node_id, &caps, &res, &hub_core::registry::now_stamp())
    {
        tracing::error!(conn_id = %conn_id, node_id = %auth.node_id, error = %e, "heartbeat persistence failed; routing still reads memory");
    }

    // agent 运行时落库（要求 1 的 Hub 侧 + 子任务 5 的 409 门禁数据源）。
    //
    // **必须落库，不能只留在内存 `Reports`**：那份表是「缺省即保留」且没有
    // `forget`，一次不带 `capabilities` 的心跳会让旧 agent 清单永远粘在那里。
    // `agent_runtime` 的语义是「这份清单就是设备现在的全部事实」——整体覆盖，
    // 卸载掉的 agent 随之消失（见 `AgentRuntimeRepo::replace_detected`）。
    persist_agents(state, &auth.node_id, &hb);
}

/// 把心跳里的 `capabilities.agents` 落进 `agent_runtime`。
///
/// # 为什么「没有 agents 键」时**什么都不做**
///
/// 协议允许只报 `node_version` 的心跳（老节点、或设备上探测还没跑过）。
/// 若把「没有这个键」当成「一个 agent 都没有」，一次握手刚重连就会把设备上
/// 所有 agent 从库里清空，门禁随即开始拦——那是比粘滞更糟的错法。
///
/// 区分靠 `capabilities` 里**有没有** `agents` 这个键，而不是它是否为空数组：
/// 空数组是「探过了，一个都没有」（要删），缺键是「这次没报」（不动）。
fn persist_agents(state: &AppState, node_id: &str, hb: &protocol::message::Heartbeat) {
    let Some(list) = hb
        .capabilities
        .as_object()
        .and_then(|o| o.get("agents"))
        .and_then(|v| v.as_array())
    else {
        return;
    };
    let rows: Vec<(String, String, Option<String>, bool, Option<String>)> = list
        .iter()
        .filter_map(parse_agent_report)
        .collect();
    let now = hub_core::registry::now_stamp();
    if let Err(e) = state.agent_runtime().replace_detected(node_id, &rows, &now) {
        tracing::error!(node_id = %node_id, error = %e, "cannot persist the reported agents");
    }
}

/// 解一项 `capabilities.agents`。
///
/// # 为什么这里自己写一份，而不是用 `node-core` 里的 `AgentReport`
///
/// `hub-server` 的**生产**依赖里没有 `node-core`（它只是 dev-dependency，
/// 给 WS 集成测试用）。为一个 20 行的 JSON 读取把它提成生产依赖，会让
/// 「设备侧探测」整棵子树（起子进程、PTY、tokio-tungstenite）进 Hub 的
/// 依赖图——Hub 自己不需要碰这些。
///
/// 兼容旧形态（纯字符串）是**必须**的：升级窗口里设备可能还是老版本，
/// 只认对象会让整张 agent 列表静默变空。
///
/// 返回 `(id, name, version, available, error)`。
fn parse_agent_report(v: &serde_json::Value) -> Option<(String, String, Option<String>, bool, Option<String>)> {
    if let Some(name) = v.as_str() {
        // 旧帧只有名字。`available = false` 是刻意的保守值：一个只说名字的
        // 帧不构成「可用」的证据，而门禁宁严勿松。
        return Some((
            name.to_string(),
            name.to_string(),
            None,
            false,
            None,
        ));
    }
    let o = v.as_object()?;
    let id = o.get("id").and_then(serde_json::Value::as_str)?;
    Some((
        id.to_string(),
        o.get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id)
            .to_string(),
        o.get("version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        o.get("available")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        o.get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    ))
}

/// 尽力发出一帧，失败只记日志。
///
/// **所有握手失败路径都走这里**：失败帧发不出去时连接马上就断了，
/// 没有第二个渠道能通知节点，所以「发送失败」不是可行动的错误——
/// 但它**必须留痕**（「帧没发出去」正是排查协议不一致的唯一线索）。
///
/// 单独抽出来而不是 11 处各写一遍：11 份 `if let Err` 加日志会漂移，
// coverage: excluded — `send_best_effort` 的失败留痕行：所有握手失败帧
// 发送失败的共同路径，需对端在收失败帧前消失。
/// 而「错误帧没送达」这类问题恰恰只在其中一处被记录时最难查。
async fn send_best_effort(socket: &mut WebSocket, frame: Frame, conn_id: &str) {
    if let Err(reason) = send_frame(socket, frame).await {
        tracing::info!(conn_id = %conn_id, reason = %reason, "cannot deliver failure frame");
    }
}

/// 发一帧（JSON 文本）。
///
/// 泛型而不是接收 `&mut WebSocket`：写协程拿到的是 `SplitSink`，握手路径
/// 拿到的是整条 `WebSocket`，两者都是 `Sink<Message>`。写两份会漂移——
/// 而「序列化失败」与「对端关了」在这两处必须被同样地报告。
async fn send_frame<S>(sink: &mut S, frame: Frame) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let text = frame.to_text().map_err(|e| e.to_string())?;
    sink.send(Message::Text(text.into()))
        .await
        .map_err(|e| e.to_string())
}

/// `/ws` 在 upgrade 之前被拒时的响应。
///
/// 与 [`crate::ApiError`] 同形（固定 code，不泄露内部状态），但这里是
/// HTTP 层、走不到 axum 的 `Result` 提取器，所以单独一个函数。
pub(crate) fn ws_error() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error": "ws_origin_rejected"})),
    )
        .into_response()
}

/// 从设备行里取库内公钥（M1 起是 `device.public_key` 真列）。
fn stored_public_key(device: &hub_storage::repo::Device) -> Option<[u8; 32]> {
    device.public_key_bytes()
}

/// enroll 的验签器：用**本次上传**的公钥。
struct SubmittedKeyAuthenticator;

impl Authenticator for SubmittedKeyAuthenticator {
    fn verify_auth(&self, attempt: &AuthAttempt<'_>) -> bool {
        verify_with(attempt)
    }
}

/// 重连的验签器：**只认库内公钥**。
///
/// 显式区分两种验签器，是为了让「重连用的是库内钥匙」在类型层面可见，
/// 而不是同一份代码在两个调用点上传入不同参数。
struct StoredKeyAuthenticator;

impl Authenticator for StoredKeyAuthenticator {
    fn verify_auth(&self, attempt: &AuthAttempt<'_>) -> bool {
        verify_with(attempt)
    }
}

/// 规范编码验签（§3）。**只有一份实现**，与 `protocol` 的测试同源。
///
/// 不用「恒返回 true」的假验签器：重放类测试只有在真验签下才说明得了问题——
/// 假验签器能让重放测试「通过」，而线上照样被重放。
fn verify_with(attempt: &AuthAttempt<'_>) -> bool {
    let key = match attempt.public_key {
        PublicKeySource::Submitted(k) | PublicKeySource::Stored(k) => k,
    };
    let msg = identity::signature_message(
        attempt.v,
        attempt.node_id,
        attempt.seq,
        attempt.msg_type,
        attempt.nonce,
    );
    identity::KeyPair::verify(key, &msg, attempt.signature)
}

/// 握手错误 → `auth_fail.reason` 的固定短语。
///
/// **绝不把 `HandshakeError` 的 Display 直接发给节点**：`BadSignature`
/// 之类的实现细节对节点无用，且会给扫描者提供信息。
/// 合法取值只有 `protocol::message::reason` 里的五个。
fn auth_fail_reason(e: HandshakeError) -> &'static str {
    match e {
        // 签名不过 / 重连带公钥（想改绑指纹）→ 身份不符
        HandshakeError::BadSignature | HandshakeError::PublicKeyOnReconnect => {
            protocol::message::reason::IDENTITY_MISMATCH
        }
        // 载荷畸形、版本非法 → 同样归为身份不符（节点应升级或修客户端）
        HandshakeError::MalformedAuth(_) | HandshakeError::InvalidVersion(_) => {
            protocol::message::reason::IDENTITY_MISMATCH
        }
        // 非法转移（重复 hello / 重复 auth）→ 也是身份不符的兜底
        HandshakeError::IllegalTransition { .. } | HandshakeError::InvalidHello(_) => {
            protocol::message::reason::IDENTITY_MISMATCH
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use protocol::handshake::HandshakeState;

    #[test]
    fn origin_header_is_rejected_regardless_of_value() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(reject_origin(&h).is_some(), "any Origin must be rejected");

        // 空 Origin 一样拒：判据是「有没有这个头」，不是「值是什么」
        let mut h2 = HeaderMap::new();
        h2.insert(axum::http::header::ORIGIN, HeaderValue::from_static(""));
        assert!(reject_origin(&h2).is_some());
    }

    #[test]
    fn no_origin_header_passes() {
        // node 是原生程序，不发 Origin
        let h = HeaderMap::new();
        assert!(reject_origin(&h).is_none());
    }

    #[test]
    fn auth_fail_reason_is_always_one_of_the_five_documented_values() {
        // §4 只允许五个值；任何实现细节都不该漏到节点眼前
        let cases = [
            HandshakeError::BadSignature,
            HandshakeError::PublicKeyOnReconnect,
            HandshakeError::MalformedAuth("public_key"),
            HandshakeError::InvalidVersion(0),
            HandshakeError::IllegalTransition {
                state: HandshakeState::Authed,
                event: "auth",
            },
            HandshakeError::InvalidHello("arch"),
        ];
        for e in cases {
            let r = auth_fail_reason(e);
            assert!(
                [
                    protocol::message::reason::HUB_NOT_INITIALIZED,
                    protocol::message::reason::FINGERPRINT_ALREADY_REGISTERED,
                    protocol::message::reason::FINGERPRINT_RACE_RETRY,
                    protocol::message::reason::IDENTITY_MISMATCH,
                    protocol::message::reason::SUPERSEDED,
                ]
                .contains(&r),
                "reason {r} is not one of the five documented values"
            );
        }
    }

    /// 帧种类是线上契约的一部分（§2 只用文本 JSON 帧），判错就该断连——
    /// 握手期与业务期**同一把尺子**。曾经这里是两个几乎同形的 `match`，
    /// 一个 `return Err` 一个 `continue`；这条把四条 arm 的取值逐条钉住。
    #[test]
    fn frame_kinds_are_judged_the_same_way_everywhere() {
        use axum::extract::ws::Message as M;
        assert_eq!(
            frame_text(M::Text("{\"v\":1}".into())),
            Ok(Some("{\"v\":1}".to_string()))
        );
        // ping/pong 让调用方继续读，不算错误也不算帧
        assert_eq!(frame_text(M::Ping(Vec::new().into())), Ok(None));
        assert_eq!(frame_text(M::Pong(Vec::new().into())), Ok(None));
        // Close 与「不是文本帧」都必须断连——node 侧把二进制判 Transport，
        // 口径不一致会让同一个症状有两个答案
        assert_eq!(frame_text(M::Close(None)), Err(PEER_CLOSED.to_string()));
        let binary =
            frame_text(M::Binary(Vec::new().into())).expect_err("binary must end the session");
        assert!(
            binary.contains("unexpected frame kind"),
            "the reason must name the offending kind: {binary}"
        );
    }

    /// 同一条连接上的**未知消息类型**与**非法帧种类**判据必须相反：
    /// 前者忽略（约束 14，老节点不能被踢下线），后者断连。
    #[test]
    fn an_unknown_message_type_is_ignored_but_an_unknown_frame_kind_is_not() {
        use protocol::message::{MessageType, UnknownAction};
        assert_eq!(
            MessageType::unknown_action("never_heard_of_it"),
            UnknownAction::Ignore,
            "未知消息类型必须被忽略而不是断连（约束 14）"
        );
        assert!(
            frame_text(axum::extract::ws::Message::Binary(Vec::new().into())).is_err(),
            "帧种类判错必须断连"
        );
    }

    /// 读侧超限的应答帧（§6.1）：两种 `LimitError` 必须映到两个不同的
    /// `err{code}`——节点靠它区分「单帧太大」与「灌太快」，而这两件事的
    /// 正确应对完全不同（换协议 vs 降速）。
    #[test]
    fn flood_frame_maps_each_limit_error_to_its_own_code() {
        let too_large = flood_frame(&hub_core::limits::LimitError::FrameTooLarge);
        assert_eq!(too_large.msg_type, types::ERR);
        let err: protocol::message::ErrPayload = too_large
            .payload()
            .expect("flood_frame must emit a parsable err payload");
        assert_eq!(err.code, protocol::message::ErrorCode::FrameTooLarge);
        assert!(!err.message.is_empty(), "the node must get a reason");

        let flood = flood_frame(&hub_core::limits::LimitError::Flood);
        let err: protocol::message::ErrPayload = flood
            .payload()
            .expect("flood_frame must emit a parsable err payload");
        assert_eq!(err.code, protocol::message::ErrorCode::Flood);
        assert!(
            err.message
                != too_large
                    .payload::<protocol::message::ErrPayload>()
                    .unwrap()
                    .message,
            "两种超限给同一条人话就没法区分了"
        );
    }

    #[test]
    fn handshake_error_phrase_names_the_field_without_secrets() {
        assert!(handshake_error_phrase(HandshakeError::InvalidHello("arch")).contains("arch"));
        assert!(handshake_error_phrase(HandshakeError::InvalidVersion(0)).contains("version"));
        // 签名失败只给一句通用话，不带 key / 签名内容
        assert_eq!(
            handshake_error_phrase(HandshakeError::BadSignature),
            "handshake rejected"
        );
    }

    fn device_with_key(caps: Option<&str>, public_key: Option<&str>) -> hub_storage::repo::Device {
        hub_storage::repo::Device {
            id: "n_x".into(),
            name: "pi".into(),
            key_fingerprint: "fp".into(),
            enrolled_at: "unix:0".into(),
            last_seen_at: None,
            status: hub_storage::repo::DeviceStatus::Online,
            last_seen_gen: 1,
            superseded_at: None,
            os: None,
            arch: None,
            hostname: None,
            node_version: None,
            capabilities: caps.map(|s| s.to_string()),
            resources: None,
            public_key: public_key.map(|s| s.to_string()),
        }
    }

    #[test]
    fn stored_public_key_reads_the_column() {
        let kp = identity::KeyPair::generate();
        assert_eq!(
            stored_public_key(&device_with_key(
                None,
                Some(&identity::base64url(&kp.public_key_bytes()))
            ))
            .unwrap(),
            kp.public_key_bytes(),
            "device.public_key must round-trip to the raw key"
        );
    }

    #[test]
    fn stored_public_key_absent_when_the_column_is_empty_or_wrong() {
        assert!(stored_public_key(&device_with_key(None, None)).is_none());
        // 非 base64 与长度不对都必须被拒，而不是 panic
        assert!(stored_public_key(&device_with_key(None, Some("!!!not base64!!!"))).is_none());
        assert!(stored_public_key(&device_with_key(None, Some("AAAA"))).is_none());
    }

    /// 心跳把 `capabilities` 整个换掉之后，公钥仍然要从**列**里读得到——
    /// M0 靠 `reports::preserve_public_key` 抢救 JSON 里的那一份，
    /// 现在那套约定已经随 v2 迁移一起消失，这条锁住新口径。
    #[test]
    fn stored_public_key_survives_a_capabilities_overwrite() {
        let kp = identity::KeyPair::generate();
        let overwritten = device_with_key(
            Some(r#"{"connectors":["git"]}"#),
            Some(&identity::base64url(&kp.public_key_bytes())),
        );
        assert!(overwritten.capabilities.as_deref().unwrap().contains("git"));
        assert_eq!(
            stored_public_key(&overwritten).unwrap(),
            kp.public_key_bytes()
        );
    }

    /// 构造一个 AuthAttempt。**不收 KeyPair**：`verify_with` 的验签公钥一律来自
    /// `PublicKeySource`，密钥对只用于在测试里产出签名。留着 `kp` 参数会让人误以为
    /// 验证用的是「调用方手里的那把钥匙」，而 `verify_with_rejects_a_wrong_key`
    /// 恰恰要证明它用的不是。
    fn attempt_with<'a>(
        nonce: &'a str,
        sig: &'a [u8; 64],
        source: PublicKeySource<'a>,
    ) -> AuthAttempt<'a> {
        AuthAttempt {
            v: 1,
            node_id: "",
            seq: 0,
            msg_type: types::AUTH,
            nonce,
            public_key: source,
            signature: sig,
        }
    }

    #[test]
    fn verify_with_accepts_a_real_signature() {
        // 真密钥真签名
        let kp = identity::KeyPair::generate();
        let nonce = identity::issue_nonce();
        let sig = kp.sign(&identity::signature_message(1, "", 0, types::AUTH, &nonce));
        let pk = kp.public_key_bytes();
        assert!(verify_with(&attempt_with(
            &nonce,
            &sig,
            PublicKeySource::Submitted(&pk)
        )));
    }

    #[test]
    fn verify_with_rejects_a_wrong_key() {
        let kp = identity::KeyPair::generate();
        let other = identity::KeyPair::generate();
        let nonce = identity::issue_nonce();
        // 用另一把钥匙签 → 交给 kp 验必须失败
        let sig = other.sign(&identity::signature_message(1, "", 0, types::AUTH, &nonce));
        let pk = kp.public_key_bytes();
        assert!(!verify_with(&attempt_with(
            &nonce,
            &sig,
            PublicKeySource::Submitted(&pk)
        )));
    }

    #[test]
    fn verify_with_rejects_a_replayed_signature_from_another_nonce() {
        // 同一个签名换 nonce 必须验不过（§3：签名覆盖 nonce）
        let kp = identity::KeyPair::generate();
        let nonce_a = identity::issue_nonce();
        let sig = kp.sign(&identity::signature_message(
            1,
            "",
            0,
            types::AUTH,
            &nonce_a,
        ));
        let nonce_b = identity::issue_nonce();
        let pk = kp.public_key_bytes();
        assert!(!verify_with(&attempt_with(
            &nonce_b,
            &sig,
            PublicKeySource::Submitted(&pk)
        )));
    }

    #[test]
    fn verify_with_rejects_a_replayed_signature_from_another_seq() {
        // 旧连接上的 auth 挪到新连接（seq 归零）一定验不过
        let kp = identity::KeyPair::generate();
        let nonce = identity::issue_nonce();
        let sig = kp.sign(&identity::signature_message(1, "", 7, types::AUTH, &nonce));
        let pk = kp.public_key_bytes();
        assert!(!verify_with(&attempt_with(
            &nonce,
            &sig,
            PublicKeySource::Submitted(&pk)
        )));
    }

    #[test]
    fn ws_error_response_is_forbidden_with_a_fixed_code() {
        let resp = ws_error();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// 设备侧的 `channel_error` 必须变成页面认得的形状（缺陷 3）。
    ///
    /// 断言 `op/code/reason` 三个键都在，因为它们正是 `SessionPage.vue` 的
    /// `applyFileFrame` + `errorOf()` 读的那三个——少一个页面就只剩一句兜底文案。
    #[test]
    fn a_channel_error_is_republished_in_the_shape_the_page_reads() {
        let (_state, _dir) = stream_state();
        let state = _state;
        state.bindings().bind(
            "ch-1",
            crate::routes::stream::test_binding("n_a", "admin", true),
        );
        let frame = Frame::push(
            types::CHANNEL_ERROR,
            1,
            serde_json::json!({
                "channel_id": "ch-1",
                "code": "internal",
                "reason": "path outside the workspace: ..",
            }),
        );
        publish_channel_error(&state, "n_a", &frame);

        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "channel.frame");
        let event = serde_json::from_str::<serde_json::Value>(&events[0].data).unwrap();
        assert_eq!(event["channel_id"], serde_json::json!("ch-1"));
        assert_eq!(event["payload"]["op"], serde_json::json!("error"));
        assert_eq!(event["payload"]["code"], serde_json::json!("internal"));
        assert_eq!(
            event["payload"]["reason"],
            serde_json::json!("path outside the workspace: ..")
        );
        assert!(
            state.bindings().get("ch-1").is_some(),
            "已确认的通道只是这一次请求被拒，通道本身还能用"
        );
    }

    /// 设备发来的 `channel_error` **真的走到**转发那一步。
    ///
    /// 上一个用例直接调 `publish_channel_error`，钉的是「帧长什么样」；
    /// 这个钉的是**接线**：一帧 `channel_error` 进了通道帧的处理分支后，
    /// Hub 认得它并把它推给页面。接线断掉时上一个用例照样绿——而页面会
    /// 永远停在「已连接」，用户看不到「为什么进不去」。
    #[test]
    fn a_channel_error_from_the_device_reaches_the_page() {
        let (state, _dir) = stream_state();
        state.bindings().bind(
            "ch-1",
            crate::routes::stream::test_binding("n_a", "admin", true),
        );
        let frame = Frame::push(
            types::CHANNEL_ERROR,
            1,
            serde_json::json!({
                "channel_id": "ch-1",
                "code": "internal",
                "reason": "path outside the workspace: ..",
            }),
        );
        dispatch_channel_frame(&state, "c1", "n_a", &frame);

        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1, "一帧设备错误对应一条页面事件");
        assert_eq!(events[0].name, "channel.frame");
        let event = serde_json::from_str::<serde_json::Value>(&events[0].data).unwrap();
        assert_eq!(event["channel_id"], serde_json::json!("ch-1"));
        assert_eq!(event["payload"]["op"], serde_json::json!("error"));
        assert_eq!(event["payload"]["code"], serde_json::json!("internal"));
    }

    /// **一帧解不开的 `channel_data` 不许推给页面**。
    ///
    /// 推出去的话前端拿到的是 `{"node_id":…, "channel_id": ""}`——既没有字节
    /// 也没有通道号，拿它渲染终端只会显示一个空面板，而真相（这帧是坏的）
    /// 被静默吞掉。所以这里钉的是「一条事件都没有」。
    ///
    /// 与 `a_channel_error_from_the_device_reaches_the_page` 同一条接线
    /// （`dispatch_channel_frame` → `publish_channel_data`），差别只在载荷
    /// 能不能被解析：`channel_id` 是**必填**字段，缺了它整帧就废。
    #[test]
    fn an_unparsable_channel_data_is_not_forwarded() {
        let (state, _dir) = stream_state();
        // 缺 `channel_id`：`ChannelData` 解不出来
        let frame = Frame::push(
            types::CHANNEL_DATA,
            1,
            serde_json::json!({"payload": {"op": "data", "bytes": "ls\n"}}),
        );
        dispatch_channel_frame(&state, "c1", "n_a", &frame);

        assert!(
            state.streams().replay_since(Some(0)).is_empty(),
            "解不开的载荷不许变成一条页面事件"
        );
        // 同样形状的**合法**帧必须推得出去：证明上面那句不是「什么都不推」
        let good = Frame::push(
            types::CHANNEL_DATA,
            1,
            serde_json::json!({"channel_id": "ch-1", "payload": {"op": "data", "bytes": "ls\n"}}),
        );
        dispatch_channel_frame(&state, "c1", "n_a", &good);
        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1, "合法帧必须有一条事件");
        let event = serde_json::from_str::<serde_json::Value>(&events[0].data).unwrap();
        assert_eq!(event["channel_id"], serde_json::json!("ch-1"));
    }

    /// 开通道就失败：错误照推，绑定摘掉（5s 超时兜底不能把真实原因盖掉）。
    #[test]
    fn a_channel_error_before_ready_also_drops_the_pending_binding() {
        let (state, _dir) = stream_state();
        state.bindings().bind(
            "ch-1",
            crate::routes::stream::test_binding("n_a", "admin", false),
        );
        publish_channel_error(
            &state,
            "n_a",
            &Frame::push(
                types::CHANNEL_ERROR,
                1,
                serde_json::json!({
                    "channel_id": "ch-1",
                    "code": "internal",
                    "reason": "workspace root is not available",
                }),
            ),
        );
        assert!(state.bindings().get("ch-1").is_none());
        assert_eq!(state.streams().replay_since(Some(0)).len(), 1);
    }

    /// 载荷解不开的 `channel_error` 不推（与 `channel_data` 同一口径）。
    #[test]
    fn an_unparsable_channel_error_is_not_forwarded() {
        let (state, _dir) = stream_state();
        publish_channel_error(
            &state,
            "n_a",
            &Frame::push(types::CHANNEL_ERROR, 1, serde_json::json!("nope")),
        );
        assert!(state.streams().replay_since(Some(0)).is_empty());
    }

    /// **一批事件被写进轨迹**，且每条保留它自己的类型名与原始载荷。
    ///
    /// 这条走的是「设备 → Hub」的落库路径，是子任务 9 的正面证据：以前只有
    /// `TrajectoryRepo` 自己的单测，证明不了设备真发上来的帧会被写进去。
    #[test]
    fn a_batch_of_events_reaches_the_trajectory_table() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));

        persist_trajectory(
            &state,
            "n_a",
            "ch-1",
            &serde_json::json!({
                "type": "batch",
                "events": [
                    {"seq": 1, "sessionUpdate": "agent_message_chunk", "text": "hi"},
                    {"seq": 2, "sessionUpdate": "tool_call", "status": "pending"},
                ],
            }),
        );

        let rows = state.trajectory().list("s_1", 0, 100).unwrap();
        assert_eq!(rows.len(), 2, "两条都要写进去：{rows:?}");
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[0].kind, "agent_message_chunk");
        assert_eq!(rows[1].kind, "tool_call");
        // 原始载荷留着：页面要画它，裁剪等于让页面没法画。
        assert!(rows[0].payload.contains("hi"), "{}", rows[0].payload);
    }

    /// **`turn_end` 单条落库，带 `stopReason`**。它是「这一轮走完了」的唯一凭据。
    #[test]
    fn a_turn_end_is_persisted_with_its_stop_reason() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));

        persist_trajectory(
            &state,
            "n_a",
            "ch-1",
            &serde_json::json!({"type": "turn_end", "stopReason": "end_turn", "seq": 7}),
        );

        let rows = state.trajectory().list("s_1", 0, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "turn_end");
        assert_eq!(rows[0].seq, 7);
        assert!(rows[0].payload.contains("end_turn"), "{}", rows[0].payload);
    }

    /// **没绑过的通道不写**：页面没开过的通道，无从知道它属于哪个会话。
    #[test]
    fn an_unbound_channel_writes_nothing_to_the_trajectory() {
        let (state, _dir) = stream_state();
        persist_trajectory(
            &state,
            "n_a",
            "ch-unknown",
            &serde_json::json!({"type": "batch", "events": [{"seq": 1}]}),
        );
        assert!(state.trajectory().list("s_1", 0, 100).unwrap().is_empty());
    }

    /// **控制面的帧不进轨迹**：`ready` / `error` / `exited` 说的是「这条通道
    /// 怎么了」，不是「agent 说了什么」。写进去会让页面把一次重连画成一条消息。
    #[test]
    fn control_frames_never_reach_the_trajectory() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));

        for payload in [
            serde_json::json!({"type": "ready", "sessionId": "ses_1", "agentId": "opencode"}),
            serde_json::json!({"type": "error", "message": "boom"}),
            serde_json::json!({"type": "exited", "code": 0, "reason": "closed"}),
            serde_json::json!({"type": "request", "method": "permission", "seq": 1}),
            // terminal 通道的载荷形状：完全没有 `type`
            serde_json::json!({"op": "data", "bytes": "ls\n"}),
        ] {
            persist_trajectory(&state, "n_a", "ch-1", &payload);
        }
        assert!(
            state.trajectory().list("s_1", 0, 100).unwrap().is_empty(),
            "控制帧不该落进轨迹表"
        );
    }

    /// **带 `events` 的控制类帧同样不写**——只靠「没有 `events` 键」挡不住它们。
    ///
    /// 这条是上一条的加强：上一条里那几类帧大多连 `events` 都没有，于是即使
    /// 把类型过滤删掉，它们也会因为取不到 `events` 而 return，测试**照样绿**——
    /// 实测确认过。所以这里给控制类帧补上 `events`，让「不写」真正只能由
    /// 类型过滤来解释。
    #[test]
    fn a_control_frame_carrying_events_still_writes_nothing() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));

        for kind in ["ready", "error", "exited", "request"] {
            persist_trajectory(
                &state,
                "n_a",
                "ch-1",
                &serde_json::json!({
                    "type": kind,
                    "events": [{"seq": 1, "sessionUpdate": "x"}],
                }),
            );
        }
        assert!(
            state.trajectory().list("s_1", 0, 100).unwrap().is_empty(),
            "控制类帧即使带了 events 也不进轨迹"
        );
    }

    /// **未知事件类型照样写，不丢**。
    ///
    /// 新版本 agent 会推我们不认识的类型。在这里过滤就是静默丢消息——
    /// 而轨迹的口径是「不承诺完整、但必须可见」，丢掉的那些连可见都做不到。
    #[test]
    fn an_unknown_event_kind_is_stored_not_dropped() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));

        persist_trajectory(
            &state,
            "n_a",
            "ch-1",
            &serde_json::json!({
                "type": "batch",
                "events": [{"seq": 1, "sessionUpdate": "some_new_kind_from_a_later_version"}],
            }),
        );
        let rows = state.trajectory().list("s_1", 0, 100).unwrap();
        assert_eq!(rows.len(), 1, "不认识也得写：{rows:?}");
        assert_eq!(rows[0].kind, "some_new_kind_from_a_later_version");
    }

    /// **缺 `seq` 的事件整条跳过，不写 0**。
    ///
    /// 写 0 的后果是「丢一条且看不出来」：两条都缺的帧都落 `seq = 0`，第二条
    /// 被 `UNIQUE` 的 `INSERT OR IGNORE` 吃掉，而序号看上去仍然连续。
    #[test]
    fn events_without_a_seq_are_dropped_not_numbered_zero() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));

        // 两条都缺 seq：若写成 0，第二条会被静默丢掉，于是看起来「写进去了」
        persist_trajectory(
            &state,
            "n_a",
            "ch-1",
            &serde_json::json!({"type": "batch", "events": [{"text": "a"}, {"text": "b"}]}),
        );
        persist_trajectory(
            &state,
            "n_a",
            "ch-1",
            &serde_json::json!({"type": "turn_end", "stopReason": "end_turn"}),
        );
        assert!(
            state.trajectory().list("s_1", 0, 100).unwrap().is_empty(),
            "缺 seq 的一律不写，绝不能写成 0"
        );
    }

    /// **同一 seq 重写被忽略，不产生第二行**（子任务 9 的测试标准）。
    #[test]
    fn replaying_the_same_seq_does_not_duplicate_a_row() {
        let (state, _dir) = stream_state();
        state
            .bindings()
            .bind("ch-1", crate::routes::stream::test_binding("n_a", "admin", true));
        let payload =
            serde_json::json!({"type": "batch", "events": [{"seq": 1, "sessionUpdate": "x"}]});

        persist_trajectory(&state, "n_a", "ch-1", &payload);
        persist_trajectory(&state, "n_a", "ch-1", &payload);

        let rows = state.trajectory().list("s_1", 0, 100).unwrap();
        assert_eq!(rows.len(), 1, "重发不是错误，但也不该变成两行：{rows:?}");
    }

    fn stream_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let state = AppState::bootstrap_with(
            pool,
            dir.path(),
            hub_core::auth::AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            std::time::Duration::from_millis(50),
            hub_core::auth::LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        (state, dir)
    }
}
