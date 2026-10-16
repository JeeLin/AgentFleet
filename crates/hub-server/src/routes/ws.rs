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

use std::time::{Duration, Instant};

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::{conns, AppState, HANDSHAKE_TIMEOUT, WS_MAX_MESSAGE_BYTES};
use hub_core::connections::ConnHandle;
use protocol::frame::Frame;
use protocol::handshake::{
    AuthAttempt, AuthPayload, Authenticator, Handshake, HandshakeError, PublicKeySource,
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
/// 前端一律走带 session 的 HTTP API。所以「有这个头就拒」才是对的边界。
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
            // 出站队列先登记：下面顶替旧连接要用它，而 relay 也要拿它收帧。
            let mut outbound = state.outbound().register(&conn_id);

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
            if let Some(old) = displaced {
                supersede(&state, &old, gen, &conn_id, &auth.node_id);
            }
            // 业务帧循环。M0 的 Hub 侧只负责「不崩、不断开」，
            // 未知消息类型一律忽略（约束 14 前向兼容）。
            // socket 是 &mut，relay 结束后它还在——断开清理见下面。
            //
            // relay 的返回值**记日志而不消音**：`let _ =` 会被
            // `let_underscore_must_use` deny，且「连接为什么结束」正是排查
            // 设备掉线时唯一有用的信息。
            if let Err(reason) = relay(&state, &mut socket, &conn_id, auth, &mut outbound).await {
                tracing::info!(conn_id = %conn_id, reason = %reason, "ws relay ended");
            }
        }
        Err(reason) => {
            tracing::info!(conn_id = %conn_id, reason = %reason, "ws handshake failed");
        }
    }

    // 断开处理器：**先校验这条连接仍是当前连接**再摘表（§7.1）。
    // 已关闭的连接不得把别人（新连接）的状态改掉。
    //
    // `match &outcome`（而不是 `match outcome`）：outcome 在上一个 match 里
    // 已经按引用用过了，再按值匹配会把 Err 里的 String move 掉。
    match &outcome {
        Ok(auth) => {
            let removed = state
                .connections()
                .remove_if_current(&auth.node_id, &conn_id);
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
            state.outbound().remove(&conn_id);
            // 通道账本**整节点丢弃**（§7.1）。这不是内存回收，而是信用重置的
            // 唯一入口：只要旧记录还在，重连后按 channel_id 记账就会累积出
            // 一个恒 > 0 的余额，等于没有流控（§6 末条）。
            let dropped = state.channels().write().drop_node(&auth.node_id);
            if dropped > 0 {
                tracing::debug!(
                    node_id = %auth.node_id,
                    channels = dropped,
                    "dropped channel records on disconnect"
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
        serde_json::to_value(protocol::message::Disconnect {
            reason: protocol::message::DisconnectReason::Superseded,
            gen: Some(gen),
        })
        .unwrap_or_else(|_| serde_json::json!({"reason": "superseded", "gen": gen})),
    );
    match state.outbound().send(&old.conn_id, frame) {
        Ok(true) => tracing::info!(
            old_conn_id = %old.conn_id,
            new_conn_id = %new_conn_id,
            node_id = %node_id,
            gen = gen,
            "superseding the older connection"
        ),
        // 表里没它：已经断开并被摘掉了，没什么要通知的
        Ok(false) => tracing::debug!(
            old_conn_id = %old.conn_id,
            "the displaced connection is already gone"
        ),
        // 在表里却推不进去：队列满，说明那条连接还在但半死不活。
        // gen 兜底会接住它，但这一条必须留痕——互抢环的成因就藏在这里。
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
            Ok(None) => return Err("peer closed".to_string()),
            Err(_) => {
                // recv 挂住到超时：只靠入口处的 deadline 不够，这里补一刀
                send_best_effort(socket, Handshake::timeout_error_frame(seq), "handshake").await;
                return Err("handshake timeout".to_string());
            }
        };

        // 只收文本帧：本协议的帧全是 JSON 文本（§2）
        let text = match msg {
            axum::extract::ws::Message::Text(t) => t.to_string(),
            axum::extract::ws::Message::Close(_) => return Err("peer closed".to_string()),
            // ping/pong 由 axum 自动应答，这里只是不让它打断握手流程
            axum::extract::ws::Message::Ping(_) | axum::extract::ws::Message::Pong(_) => continue,
            _ => return Err("unexpected frame kind".to_string()),
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
                {
                    return Err("peer closed".to_string());
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

/// enroll 被拒：回一帧 `fingerprint race, retry` 再断开（§3——绝不 upsert）。
async fn reject_enroll(socket: &mut WebSocket, hs: &mut Handshake, seq: u64) -> String {
    let reason = protocol::message::reason::FINGERPRINT_RACE_RETRY;
    send_best_effort(socket, hs.auth_fail(seq, reason, None), "handshake").await;
    String::from(reason)
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
async fn finish_auth(
    state: &AppState,
    socket: &mut WebSocket,
    hs: &mut Handshake,
    seq: u64,
    gen: u64,
    payload: AuthPayload,
) -> Result<Authenticated, String> {
    match payload {
        AuthPayload::Enroll(auth) => {
            let authz = SubmittedKeyAuthenticator;
            let accepted = match hs.on_enroll(seq, &auth, &authz) {
                Ok(a) => a,
                Err(e) => {
                    let reason = auth_fail_reason(e);
                    send_best_effort(socket, hs.auth_fail(seq, reason, None), "handshake").await;
                    return Err(String::from(reason));
                }
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
            let node_id = match outcome.node_id() {
                Some(id) => id.to_string(),
                // coverage: excluded — `None` 即 `EnrollOutcome::Race`，而
                // `Registry::enroll` 构造不出它：`BEGIN IMMEDIATE` 在事务开始
                // 就拿到写锁，事务内 SELECT 之后不可能再有别的事务提交同指纹的行，
                // 那条 INSERT 永远撞不上 UNIQUE（要触达得先破坏单写者不变式）。
                None => return Err(reject_enroll(socket, hs, seq).await),
            };
            // 公钥另存（M0 权宜：库里只有指纹，无法反推公钥，见 BUGS.md）。
            // 失败不阻塞 enroll：重连时缺公钥会走 identity mismatch，
            // 而那比「 enroll 失败」更容易排查。
            if let Err(e) = state
                .registry()
                .devices()
                .set_public_key(&node_id, &accepted.public_key)
            {
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
        AuthPayload::Reconnect(auth) => {
            // 重连：只信库内指纹（§3）。
            let device = match state.registry().device(&auth.node_id) {
                Ok(Some(d)) => d,
                Ok(None) => {
                    let reason = protocol::message::reason::IDENTITY_MISMATCH;
                    send_best_effort(socket, hs.auth_fail(seq, reason, None), "handshake").await;
                    return Err(String::from(reason));
                }
                Err(e) => {
                    tracing::error!(error = %e, "device lookup failed during reconnect");
                    return Err("device lookup failed".to_string());
                }
            };
            let Some(pubkey) = stored_public_key(&device) else {
                let reason = protocol::message::reason::IDENTITY_MISMATCH;
                send_best_effort(socket, hs.auth_fail(seq, reason, None), "handshake").await;
                return Err(String::from(reason));
            };
            let authz = StoredKeyAuthenticator;
            let accepted = match hs.on_reconnect(seq, &auth, &pubkey, &authz) {
                Ok(a) => a,
                Err(e) => {
                    let reason = auth_fail_reason(e);
                    send_best_effort(socket, hs.auth_fail(seq, reason, None), "handshake").await;
                    return Err(String::from(reason));
                }
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
                    let reason = protocol::message::reason::SUPERSEDED;
                    tracing::info!(
                        node_id = %auth.node_id,
                        last_seen_gen = seen,
                        live_gen = live,
                        "rejecting a reconnect from a superseded connection"
                    );
                    send_best_effort(socket, hs.auth_fail(seq, reason, Some(live)), "handshake")
                        .await;
                    return Err(String::from(reason));
                }
            }
            if let Err(e) = state.registry().devices().set_gen(&auth.node_id, gen) {
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
    }
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
        .map_err(handshake_error_phrase)?;
    if send_frame(socket, frame).await.is_err() {
        // auth_ok 发不出去 = 对端已经走了，连接没有成立
        return Err("peer closed".to_string());
    }
    Ok(Authenticated {
        node_id,
        gen,
        protocol_version,
        heartbeat_interval_ms: interval,
    })
}

/// 业务帧循环：读帧、心跳落库、忽略未知类型。
///
/// M0 的 Hub 侧只需要「连接不断、未知消息不崩」。通道、
/// `disconnect{superseded}` 分别是子任务 8、7。
/// 读侧超限后每轮慢读的暂停时长。
///
/// 50ms 而不是更长：慢读的目的是让 TCP 背压传回发送方，而不是把连接冻住。
/// 太长的暂停会让轻微波峰之后的恢复也变慢（一次 `cat` 结束还得等）。
const SLOW_READ_PAUSE_MS: u64 = 50;

/// 读侧超限的应答：`err{code}`，随后断开（§6.1）。
fn flood_frame(e: &hub_core::limits::LimitError) -> Frame {
    let code = match e {
        hub_core::limits::LimitError::FrameTooLarge => protocol::message::ErrorCode::FrameTooLarge,
        hub_core::limits::LimitError::Flood => protocol::message::ErrorCode::Flood,
        // 读侧限流器不会产出写队列错误；列出它是编译期要求穷尽，
        // 真出现了按内部错误报（那是 Hub 自己的 bug，不是对端的）。
        hub_core::limits::LimitError::WriteQueueFull => protocol::message::ErrorCode::Internal,
    };
    Frame::push(
        types::ERR,
        0,
        serde_json::to_value(protocol::message::ErrPayload {
            code: code.clone(),
            message: e.to_string(),
        })
        .unwrap_or_else(|_| serde_json::json!({"code": code})),
    )
}

/// 这一帧是不是通道帧（§5）。
fn is_channel_type(msg_type: &str) -> bool {
    matches!(
        msg_type,
        types::OPEN_CHANNEL
            | types::CHANNEL_READY
            | types::CHANNEL_DATA
            | types::CHANNEL_CREDIT
            | types::CHANNEL_ERROR
            | types::CLOSE_CHANNEL
    )
}

/// 业务帧循环：收 node 发来的帧，同时把 Hub 要推的控制帧发出去。
///
/// 两个方向用 `select!` 而不是串行：控制帧（`disconnect{superseded}`）必须
/// **插队**——它要打断一个可能正阻塞在读上的循环。串行的写法下，一条
/// 什么都不发的旧连接永远收不到「你被顶替了」，于是互抢。
async fn relay(
    state: &AppState,
    socket: &mut WebSocket,
    conn_id: &str,
    auth: &Authenticated,
    outbound: &mut tokio::sync::mpsc::Receiver<Frame>,
) -> Result<(), String> {
    // 每 1s 扫一次「开了但没 Ready」的通道（§5 的 5s 超时）。
    // 1s 而不是 5s：扫描只决定「什么时候发现」，而超时判据在协议层
    // （`ready_expired` 比的是 opened_at + READY_TIMEOUT），扫得勤只让
    // 误差更小，不会提前拒绝。
    let mut sweep = tokio::time::interval(Duration::from_secs(1));
    // 每连接一个读侧令牌桶（§6.1「每连接 semaphore」）。按字节而不是按帧计：
    // 一帧 1 MiB 与一帧 10 字节对内存的压力差五个数量级，按帧计等于
    // 允许「少量大帧」打爆 Hub。
    let mut limiter = hub_core::limits::ReadLimiter::new();
    // 读侧超限的**起始时刻**。它决定「慢读多久升级成断开」——没有它，
    // 每一次超限都无从判断是突发还是持续。
    let mut over_since: Option<Instant> = None;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(msg) = incoming else {
                    return Err("peer closed".to_string());
                };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => return Err(format!("ws read error: {e}")),
                };
                let text = match msg {
                    axum::extract::ws::Message::Text(t) => t,
                    axum::extract::ws::Message::Close(_) => return Err("peer closed".to_string()),
                    axum::extract::ws::Message::Ping(_)
                    | axum::extract::ws::Message::Pong(_) => continue,
                    _ => continue,
                };
                // §6.1：credit 是**发送方的自我约束**，拦不住一个恶意或
                // buggy 的 worker。Hub 的自我保护是自己的读侧令牌桶——
                // 不看对端有没有信用，只看它灌了多少字节。
                //
                // 没有这条，一台设备持续灌数据就能让 Hub 逐帧解析 JSON 写进
                // 内存直到 OOM，连带打垮所有其他设备与 Web 端——而那正是
                // credit 想防的事，却只防了 node 的发送侧。
                //
                // 超限**先慢读，不立刻断开**（§6.1「超限 → 慢读/暂停，
                // 持续 30s 才断开」）。立刻断是错的：`cat` 一个大文件就是
                // 一次合法的 PTY 洪峰，误杀它等于「输出多的命令一律失败」。
                // 慢读让 TCP 背压传回发送方，流量自然降下来。
                loop {
                    match limiter.try_consume(text.len()) {
                        Ok(()) => {
                            over_since = None;
                            break;
                        }
                        // 单帧超硬上限没有「等等就好」这回事——它永远超。
                        Err(hub_core::limits::LimitError::FrameTooLarge) => {
                            tracing::warn!(
                                conn_id = %conn_id,
                                node_id = %auth.node_id,
                                bytes = text.len(),
                                "frame over the hard limit; dropping the connection"
                            );
                            let _ = send_frame(socket, flood_frame(&hub_core::limits::LimitError::FrameTooLarge)).await;
                            return Err("frame over the hard limit".to_string());
                        }
                        Err(hub_core::limits::LimitError::Flood) => {
                            let since = *over_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= hub_core::limits::FLOOD_SUSTAINED {
                                tracing::warn!(
                                    conn_id = %conn_id,
                                    node_id = %auth.node_id,
                                    "read limit exceeded for too long; dropping the connection"
                                );
                                let _ = send_frame(socket, flood_frame(&hub_core::limits::LimitError::Flood)).await;
                                return Err("sustained read limit exceeded".to_string());
                            }
                            // 慢读：等令牌回补再处理这一帧。**不重读**——
                            // 已经读进来的字节丢掉就是丢数据。
                            tokio::time::sleep(Duration::from_millis(SLOW_READ_PAUSE_MS)).await;
                        }
                        Err(hub_core::limits::LimitError::WriteQueueFull) => break,
                    }
                }
                let text = text.to_string();
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
                if is_channel_type(&frame.msg_type) {
                    match crate::channels::on_channel_frame(state, &auth.node_id, &frame) {
                        crate::channels::Reply::Nothing => {}
                        crate::channels::Reply::Frame(out) => {
                            if let Err(e) = send_frame(socket, out).await {
                                return Err(format!("cannot reply to a channel frame: {e}"));
                            }
                        }
                    }
                    continue;
                }
                // 其余（含未知类型）一律忽略：约束 14 前向兼容
            }
            _ = sweep.tick() => {
                let due = crate::channels::sweep_unready(state, &auth.node_id, Instant::now());
                for out in due {
                    if let Err(e) = send_frame(socket, out).await {
                        return Err(format!("cannot reject an unready channel: {e}"));
                    }
                }
            }
            control = outbound.recv() => {
                let Some(frame) = control else {
                    // 发送端全部被丢弃 = 这张表把本连接摘了。
                    // 正常路径上 `handle_socket` 摘表时连接也该结束了，
                    // 所以这里按「连接结束」返回而不是继续空转。
                    return Err("outbound closed".to_string());
                };
                if let Err(e) = send_frame(socket, frame).await {
                    return Err(format!("cannot deliver control frame: {e}"));
                }
            }
        }
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
    if let Err(e) = state.devices().touch_online(
        &auth.node_id,
        &caps,
        &res,
        &hub_core::registry::now_rfc3339(),
    ) {
        tracing::error!(conn_id = %conn_id, node_id = %auth.node_id, error = %e, "heartbeat persistence failed; routing still reads memory");
    }
}

/// 尽力发出一帧，失败只记日志。
///
/// **所有握手失败路径都走这里**：失败帧发不出去时连接马上就断了，
/// 没有第二个渠道能通知节点，所以「发送失败」不是可行动的错误——
/// 但它**必须留痕**（`let _ =` 会被 `let_underscore_must_use` deny，
/// 而且「帧没发出去」正是排查协议不一致的唯一线索）。
///
/// 单独抽出来而不是 11 处各写一遍：11 份 `if let Err` 加日志会漂移，
/// 而「错误帧没送达」这类问题恰恰只在其中一处被记录时最难查。
async fn send_best_effort(socket: &mut WebSocket, frame: Frame, conn_id: &str) {
    if let Err(reason) = send_frame(socket, frame).await {
        tracing::info!(conn_id = %conn_id, reason = %reason, "cannot deliver failure frame");
    }
}

/// 发一帧（JSON 文本）。
async fn send_frame(socket: &mut WebSocket, frame: Frame) -> Result<(), String> {
    let text = frame.to_text().map_err(|e| e.to_string())?;
    socket
        .send(axum::extract::ws::Message::Text(text.into()))
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

/// 从设备行里取库内公钥（M0 权宜：存在 `capabilities.public_key`）。
fn stored_public_key(device: &hub_storage::repo::Device) -> Option<[u8; 32]> {
    let caps = device.capabilities.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(caps).ok()?;
    let b64 = v.get("public_key")?.as_str()?;
    let raw = identity::base64url_decode(b64).ok()?;
    <[u8; 32]>::try_from(raw).ok()
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

/// 握手错误 → 日志短语（不含任何敏感信息）。
fn handshake_error_phrase(e: HandshakeError) -> String {
    match e {
        HandshakeError::InvalidHello(f) => format!("invalid hello field: {f}"),
        HandshakeError::InvalidVersion(v) => format!("invalid protocol version: {v}"),
        _ => "handshake rejected".to_string(),
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

    fn device_with_caps(caps: Option<&str>) -> hub_storage::repo::Device {
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
        }
    }

    #[test]
    fn stored_public_key_reads_from_capabilities() {
        // M0 权宜：公钥存在 capabilities.public_key（BUGS.md 已登记）
        let kp = identity::KeyPair::generate();
        let caps = serde_json::json!({"public_key": identity::base64url(&kp.public_key_bytes())})
            .to_string();
        assert_eq!(
            stored_public_key(&device_with_caps(Some(&caps))).unwrap(),
            kp.public_key_bytes(),
            "capabilities.public_key must round-trip to the raw key"
        );
    }

    #[test]
    fn stored_public_key_absent_when_capabilities_missing_or_wrong() {
        assert!(stored_public_key(&device_with_caps(None)).is_none());
        assert!(stored_public_key(&device_with_caps(Some("not json"))).is_none());
        assert!(stored_public_key(&device_with_caps(Some("{}"))).is_none());
        // 长度不对的 base64 → 也必须是 None，而不是 panic
        let bad = serde_json::json!({"public_key": "AAAA"}).to_string();
        assert!(stored_public_key(&device_with_caps(Some(&bad))).is_none());
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
}
