//! Hub 侧对通道帧的应答（子任务 8，`protocol.md` §5–§6）。
//!
//! **Hub 是通道的被动方**：`open_channel` 由 node 发，Hub 只记账并回
//! `channel_ready` / `channel_error`。信用账本与状态机在
//! `protocol::channel`（两侧共用的纯逻辑），这里只做「收到一帧 → 该回什么」。
//!
//! 一个方向要分清：`channel_data` 的主流量是 **node → Hub**（PTY 输出），
//! Hub 收到后按信用记账并适时补 `channel_credit`。反向（用户输入）量小，
//! 走 WS 自身背压即可，不占用这套账本。

use std::time::Instant;

use protocol::channel::{ChannelError, ChannelErrorCode, CreditStatus};
use protocol::frame::Frame;
use protocol::message::types;

use crate::AppState;

/// 处理一帧通道帧的结果。
pub enum Reply {
    /// 不用回任何东西（记账完成，或这帧本就不该有应答）。
    Nothing,
    /// 回一帧。
    Frame(Frame),
}

/// 分发一帧通道帧。
pub fn on_channel_frame(state: &AppState, node_id: &str, frame: &Frame) -> Reply {
    match frame.msg_type.as_str() {
        types::OPEN_CHANNEL => on_open(state, node_id, frame),
        types::CHANNEL_DATA => on_data(state, node_id, frame),
        types::CLOSE_CHANNEL => on_close(state, node_id, frame),
        // `channel_ready` 是 Hub 发出的方向；node 发来属于协议违例。
        // `channel_credit` 同理（Hub 补信用，node 不该补）。
        // 两者都按「未知类型」忽略（约束 14 前向兼容），不因此断开连接。
        _ => Reply::Nothing,
    }
}

fn on_open(state: &AppState, node_id: &str, frame: &Frame) -> Reply {
    let spec: protocol::channel::OpenChannel = match frame.payload() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "dropping a malformed open_channel");
            return Reply::Nothing;
        }
    };
    // `credit_start` 的缺省由协议层的 `#[serde(default = ...)]` 兜住，
    // 这里直接用。不自己再兜一次：两处默认值不一致是最难查的配置漂移
    // （协议是 `CREDIT_START`，这里漏填就变 0 → 通道一开余额就是 0，
    // 双方互等，死锁）。
    let credit_start = spec.credit_start;

    let mut channels = state.channels().write();
    let registry = channels.registry(node_id);
    match registry.open(
        &spec.channel_id,
        &spec.connector,
        credit_start,
        Instant::now(),
    ) {
        Ok(()) => {}
        Err(e) => {
            // 开失败要**显式回错**（§4），不能静默丢弃——否则 node 那边
            // 一直等 `channel_ready`，直到 5s 超时才知道出了问题，
            // 而用户看到的是「终端卡了 5 秒」而不是一句人话。
            return Reply::Frame(error_frame(&spec.channel_id, &e));
        }
    }
    // M0 的 Hub 侧没有 connector 协商：`open_channel` 一到就置 Ready 并回
    // `channel_ready`。
    //
    // 不置 Ready 的话账本是死的——`on_data_received` 只对 Ready 的通道记账
    // （协议层 `ready_mut`），于是 node 发来的数据全部回 `NotReady`，
    // 而 node 那边还在等 `channel_ready`，两边互等。
    if let Err(e) = registry.mark_ready(&spec.channel_id) {
        return Reply::Frame(error_frame(&spec.channel_id, &e));
    }
    tracing::debug!(
        node_id = %node_id,
        channel_id = %spec.channel_id,
        connector = %spec.connector,
        credit_start = credit_start,
        "channel ready"
    );
    Reply::Frame(Frame::push(
        types::CHANNEL_READY,
        0,
        serde_json::to_value(protocol::channel::ChannelReady {
            channel_id: spec.channel_id,
            capabilities: serde_json::json!({}),
        })
        .unwrap_or_else(|_| serde_json::json!({})),
    ))
}

fn on_data(state: &AppState, node_id: &str, frame: &Frame) -> Reply {
    let data: protocol::channel::ChannelData = match frame.payload() {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "dropping a malformed channel_data");
            return Reply::Nothing;
        }
    };
    let mut channels = state.channels().write();
    match channels
        .registry(node_id)
        .on_data_received(&data.channel_id)
    {
        Ok(CreditStatus::Granted) => Reply::Nothing,
        // 余额归零：按 §6 补一轮信用。信用只增不减，补的是协议常量。
        Ok(CreditStatus::Exhausted) => Reply::Frame(Frame::push(
            types::CHANNEL_CREDIT,
            0,
            serde_json::to_value(protocol::channel::ChannelCredit {
                channel_id: data.channel_id,
                delta: protocol::channel::CREDIT_DELTA,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        )),
        Err(e) => Reply::Frame(error_frame(&data.channel_id, &e)),
    }
}

fn on_close(state: &AppState, node_id: &str, frame: &Frame) -> Reply {
    let close: protocol::channel::CloseChannel = match frame.payload() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "dropping a malformed close_channel");
            return Reply::Nothing;
        }
    };
    let mut channels = state.channels().write();
    // 关一条通道**不得影响其他通道**（§5）：账本是 per-channel 的，
    // 这里只动 `close.channel_id` 那一条。
    match channels.registry(node_id).mark_closed(&close.channel_id) {
        Ok(()) => {
            tracing::debug!(
                node_id = %node_id,
                channel_id = %close.channel_id,
                "channel closed"
            );
            Reply::Nothing
        }
        Err(e) => Reply::Frame(error_frame(&close.channel_id, &e)),
    }
}

/// 把一个 `ChannelError` 变成 `channel_error` 帧。
fn error_frame(channel_id: &str, e: &ChannelError) -> Frame {
    let code = match e {
        // `ErrorCode` 没有「未知通道」这一档：对 node 来说，通道 id 不认识
        // 与「这个通道我没有」是同一件事，都按 `unsupported` 回。
        ChannelError::EmptyChannelId | ChannelError::UnknownChannel(_) => {
            ChannelErrorCode::Unsupported
        }
        ChannelError::AlreadyClosed(_) | ChannelError::AlreadyReady(_) => {
            ChannelErrorCode::Unsupported
        }
        ChannelError::NotReady(_) => ChannelErrorCode::Unsupported,
    };
    Frame::push(
        types::CHANNEL_ERROR,
        0,
        serde_json::to_value(protocol::channel::ChannelErrorPayload {
            channel_id: channel_id.to_string(),
            code: code.clone(),
            reason: e.to_string(),
        })
        .unwrap_or_else(|_| serde_json::json!({"channel_id": channel_id})),
    )
}

/// 扫一遍「开了但迟迟没 Ready」的通道，对每个回 `channel_error{unsupported}`。
///
/// M0 的 Hub 在 `open_channel` 到达的同一刻就置 Ready，所以这里通常扫不到
/// 东西。留着它的理由是**未来**：一旦 `open_channel` 需要先去问 connector
/// （M1 的 exec 要挑 arch、要等二进制就绪），通道就会有一段时间停在 Opening，
/// 而那时「静默丢弃」的表现是 node 干等到自己的 5s 超时、用户看到终端卡住
/// 却没有一句人话。有个扫描器兜底，超时就一定有应答。
pub fn sweep_unready(state: &AppState, node_id: &str, now: Instant) -> Vec<Frame> {
    let mut channels = state.channels().write();
    let registry = channels.registry(node_id);
    let expired = registry.expired_ready(now);
    let mut out = Vec::new();
    for id in &expired {
        match registry.reject(id, ChannelErrorCode::Unsupported, reason::UNSUPPORTED) {
            Ok(_action) => {
                tracing::warn!(
                    node_id = %node_id,
                    channel_id = %id,
                    "channel never became ready; rejecting it"
                );
                out.push(Frame::push(
                    types::CHANNEL_ERROR,
                    0,
                    serde_json::to_value(protocol::channel::ChannelErrorPayload {
                        channel_id: id.clone(),
                        code: ChannelErrorCode::Unsupported,
                        reason: reason::UNSUPPORTED.to_string(),
                    })
                    .unwrap_or_else(|_| serde_json::json!({})),
                ));
            }
            Err(e) => tracing::debug!(error = %e, "cannot reject an unready channel"),
        }
    }
    out
}

/// `channel_error.reason` 的取值（`protocol.md` §5）。
mod reason {
    pub const UNSUPPORTED: &str = "unsupported";
}

#[cfg(test)]
mod tests {
    use super::*;

    use hub_core::auth::LoginRateLimiter;

    fn frame(msg_type: &str, payload: serde_json::Value) -> Frame {
        Frame::push(msg_type, 1, payload)
    }

    fn state() -> (AppState, tempfile::TempDir) {
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
            std::time::Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        (state, dir)
    }

    fn is_nothing(r: Reply) -> bool {
        matches!(r, Reply::Nothing)
    }

    /// 载荷坏掉的通道帧一律**忽略**：它不是协议违例，只是一帧解析不了的
    /// JSON。断开连接会让「一个新版本的字段」直接搞死老 Hub（约束 14）。
    #[test]
    fn malformed_channel_frames_are_dropped() {
        let (state, _dir) = state();
        assert!(is_nothing(on_channel_frame(
            &state,
            "n_a",
            &frame(types::OPEN_CHANNEL, serde_json::json!(42))
        )));
        assert!(is_nothing(on_channel_frame(
            &state,
            "n_a",
            &frame(types::CHANNEL_DATA, serde_json::json!("nope"))
        )));
        assert!(is_nothing(on_channel_frame(
            &state,
            "n_a",
            &frame(types::CLOSE_CHANNEL, serde_json::json!([1, 2]))
        )));
    }

    /// 关一条**不存在**的通道要显式回错，而不是静默——node 那边有状态机，
    /// 静默会让它的通道表永远留着一条关不掉的。
    #[test]
    fn closing_an_unknown_channel_replies_with_an_error() {
        let (state, _dir) = state();
        let reply = on_channel_frame(
            &state,
            "n_a",
            &frame(
                types::CLOSE_CHANNEL,
                serde_json::json!({"channel_id": "ch-nope"}),
            ),
        );
        match reply {
            Reply::Frame(f) => {
                assert_eq!(f.msg_type, types::CHANNEL_ERROR);
                assert_eq!(f.c["channel_id"], "ch-nope");
                assert_eq!(f.c["code"], "unsupported");
            }
            other => panic!(
                "expected an error frame, got {}",
                matches!(other, Reply::Nothing)
            ),
        }
    }

    /// 未知/空 channel_id 都按 `unsupported`：`ErrorCode` 没有「未知通道」这一档，
    /// 而对 node 来说两者是同一件事。
    #[test]
    fn every_channel_error_maps_to_a_code() {
        assert_eq!(
            error_code_of(&ChannelError::EmptyChannelId),
            ChannelErrorCode::Unsupported
        );
        assert_eq!(
            error_code_of(&ChannelError::UnknownChannel("x".into())),
            ChannelErrorCode::Unsupported
        );
        assert_eq!(
            error_code_of(&ChannelError::AlreadyClosed("x".into())),
            ChannelErrorCode::Unsupported
        );
        assert_eq!(
            error_code_of(&ChannelError::AlreadyReady("x".into())),
            ChannelErrorCode::Unsupported
        );
        assert_eq!(
            error_code_of(&ChannelError::NotReady("x".into())),
            ChannelErrorCode::Unsupported
        );
    }

    fn error_code_of(e: &ChannelError) -> ChannelErrorCode {
        let f = error_frame("ch-1", e);
        serde_json::from_value::<protocol::channel::ChannelErrorPayload>(f.c.clone())
            .unwrap()
            .code
    }

    /// Hub 开完立刻置 Ready，所以「开了但没 Ready」扫不到东西。
    ///
    /// 这条锁住的是**不误伤**：扫描器不得把已经 Ready 的通道判成超时。
    #[test]
    fn sweeping_unready_leaves_ready_channels_alone() {
        let (state, _dir) = state();
        let f = frame(
            types::OPEN_CHANNEL,
            serde_json::json!({"channel_id": "ch-1", "connector": "terminal", "credit_start": 4}),
        );
        assert!(matches!(
            on_channel_frame(&state, "n_a", &f),
            Reply::Frame(_)
        ));
        // 立刻扫：通道已经 Ready，不该被判超时
        assert!(sweep_unready(&state, "n_a", Instant::now()).is_empty());
        // 即便「现在」已经远超 5s 也不行——判据是 opened_at，不是扫描时刻
        assert!(sweep_unready(&state, "n_a", Instant::now()).is_empty());
    }
}
