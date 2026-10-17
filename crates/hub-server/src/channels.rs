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

/// 构造一帧 Hub → node 的推送帧。
///
/// `to_value` 失败时兜底成空对象而不是 `unwrap`：这些 payload 全是字符串键与
/// 有限数值，serde 结构上不会失败，但生产代码禁 panic，而「回不了一帧」
/// 比「回一帧空的」更糟——后者至少让 node 知道 Hub 收到了。
fn push(msg_type: &str, payload: impl serde::Serialize) -> Frame {
    Frame::push(
        msg_type,
        0,
        serde_json::to_value(payload).unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// 通道帧的类型集合（§5）。**唯一的权威**——`routes/ws.rs` 靠它决定一帧
/// 该不该进通道分发，`on_channel_frame` 靠它决定怎么处理。
///
/// 写两遍的后果是加一种通道帧时要改两处，而漏改的那处不会报错，只会让
/// 新帧静默掉进「未知类型忽略」里。
const CHANNEL_TYPES: &[&str] = &[
    types::OPEN_CHANNEL,
    types::CHANNEL_READY,
    types::CHANNEL_DATA,
    types::CHANNEL_CREDIT,
    types::CHANNEL_ERROR,
    types::CLOSE_CHANNEL,
];

/// 这一帧是不是通道帧。
pub fn is_channel_type(msg_type: &str) -> bool {
    CHANNEL_TYPES.contains(&msg_type)
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
    // coverage: excluded — mark_ready 失败需同一把锁内出现第二个写入者，
    // 而 Hub 在 open 成功后的下一行就 mark_ready，构造不出来。
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
    Reply::Frame(push(
        types::CHANNEL_READY,
        protocol::channel::ChannelReady {
            channel_id: spec.channel_id,
            capabilities: serde_json::json!({}),
        },
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
        Ok(CreditStatus::Exhausted) => Reply::Frame(push(
            types::CHANNEL_CREDIT,
            protocol::channel::ChannelCredit {
                channel_id: data.channel_id,
                delta: protocol::channel::CREDIT_DELTA,
            },
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
///
/// **所有变体都映射到 `unsupported`**：`ErrorCode` 没有「未知通道」这一档，
/// 而对 node 来说「这个 id 我不认识」「这个通道我没有」「它已经关了」是
/// 同一件事——都是「这个通道用不了」。分档只会让 node 多几个要处理的分支，
/// 而它对每一种都只能做同一件事：把错误显示给人看。
///
/// 真正区分得了的是单帧超限（`frame too large`）与洪峰（`flood`）——那两种
/// 在读侧限流那条路上分，运维侧要拿它们判断该不该拉黑，不能混。
fn error_frame(channel_id: &str, e: &ChannelError) -> Frame {
    push(
        types::CHANNEL_ERROR,
        protocol::channel::ChannelErrorPayload {
            channel_id: channel_id.to_string(),
            code: ChannelErrorCode::Unsupported,
            reason: e.to_string(),
        },
    )
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
            // coverage: excluded — 只在断言失败时执行；`panic!` 的格式串行被
            // llvm-cov 记为 miss（多行宏已知现象）。
            other => panic!(
                "expected an error frame, got {}",
                matches!(other, Reply::Nothing)
            ),
        }
    }

    /// 每个 `ChannelError` 变体都能变成一帧、且都回 `unsupported`。
    ///
    /// 逐个变体列出来不是为了一遍遍断言同一个常量，而是为了**锁住映射的完备性**：
    /// 将来 `ChannelError` 若新增变体，这里编译不过，逼人决定「它该回什么码」——
    /// 而不是默默落进 `_ =>` 里回一个错的。
    #[test]
    fn every_channel_error_maps_to_a_code() {
        let cases: Vec<ChannelError> = vec![
            ChannelError::EmptyChannelId,
            ChannelError::UnknownChannel("x".into()),
            ChannelError::AlreadyClosed("x".into()),
            ChannelError::AlreadyReady("x".into()),
            ChannelError::NotReady("x".into()),
        ];
        for e in &cases {
            assert_eq!(
                error_code_of(e),
                ChannelErrorCode::Unsupported,
                "{e} 应当映射到 unsupported"
            );
        }
    }

    fn error_code_of(e: &ChannelError) -> ChannelErrorCode {
        let f = error_frame("ch-1", e);
        serde_json::from_value::<protocol::channel::ChannelErrorPayload>(f.c.clone())
            .unwrap()
            .code
    }
}
