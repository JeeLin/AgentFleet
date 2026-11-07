//! Hub 侧对通道帧的应答（子任务 8，`protocol.md` §5–§6）。
//!
//! **Hub 是通道的主动方**：`open_channel` 由 **Hub 发**（`routes/stream.rs` 的
//! `POST /api/channels`），设备收下后回 `channel_ready` + 首帧。本模块只做
//! 「收到一帧 → 该回什么」；**账本的建表入口不在本模块**，在 `stream.rs`
//! 的 `open_ledger`——放这里的后果见那里的注释。
//!
//! 一个方向要分清：`channel_data` 的主流量是 **node → Hub**（PTY 输出），
//! Hub 收到后按信用记账并适时补 `channel_credit`。反向（用户输入）量小，
//! 走 WS 自身背压即可，不占用这套账本。

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
/// 兜底由 [`protocol::payload_or`] 给：本函数只走通道帧，而每一档通道帧都靠
/// `channel_id` 认领，所以调用方把兜底形状一并传进来。
fn push(msg_type: &str, payload: impl serde::Serialize, fallback: serde_json::Value) -> Frame {
    Frame::push(msg_type, 0, protocol::payload_or(&payload, fallback))
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
///
/// **没有 `open_channel` 臂是刻意的**：那一帧只由 Hub 发（`protocol.md` §4
/// 的消息表），设备侧发来属于协议违例，按「未知类型」忽略（约束 14 前向
/// 兼容），不因此断开连接。给它留一条臂的后果更糟：账本的建表入口会
/// 落在一段生产不可达的代码里，于是每帧上行都判「未知通道」而 Hub 静默无痕。
pub fn on_channel_frame(state: &AppState, node_id: &str, frame: &Frame) -> Reply {
    match frame.msg_type.as_str() {
        types::CHANNEL_READY => on_ready(state, frame),
        types::CHANNEL_DATA => on_data(state, node_id, frame),
        types::CLOSE_CHANNEL => on_close(state, node_id, frame),
        // `channel_credit` 是 Hub 发出的方向（Hub 补信用，node 不该补），
        // node 发来属于协议违例。按「未知类型」忽略（约束 14 前向兼容），
        // 不因此断开连接。
        _ => Reply::Nothing,
    }
}

/// `channel_ready` 是 **node → Hub** 的方向：`node-core/src/channel.rs` 的
/// `open()` 第一个回帧就是它。Hub 收到它才把「通道可用了」告诉页面
/// （[`crate::routes::stream::announce_ready`]），此前页面一直停在「建立中」。
fn on_ready(state: &AppState, frame: &Frame) -> Reply {
    let ready: protocol::channel::ChannelReady = match frame.payload() {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "dropping a malformed channel_ready");
            return Reply::Nothing;
        }
    };
    crate::routes::stream::announce_ready(state, &ready.channel_id);
    Reply::Nothing
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
        Ok(CreditStatus::Exhausted) => {
            // **回填 Hub 侧账本**：只发 `channel_credit` 而不记账，Hub 侧余额
            // 恒为 0——下一帧又立刻归零，于是**每帧都补一次信用**，node 侧的
            // 余额越滚越大（约束 4 的「每通道独立流控」就此失效）。
            //
            // 补的量与发出去的那一帧一致：Hub 说「再给你 32 格」，它自己的
            // 账本也得有那 32 格。
            //
            // coverage: excluded — `on_credit` 只在通道不存在时失败，而这条通道
            // 刚在同一把锁里被 `on_data_received` 成功记过账：两者之间没有能改
            // 这张表的代码路径（同一把写锁）。
            if let Err(e) = channels
                .registry(node_id)
                .on_credit(&data.channel_id, protocol::channel::CREDIT_DELTA)
            {
                return Reply::Frame(error_frame(&data.channel_id, &e));
            }
            Reply::Frame(push(
                types::CHANNEL_CREDIT,
                protocol::channel::ChannelCredit {
                    channel_id: data.channel_id.clone(),
                    delta: protocol::channel::CREDIT_DELTA,
                },
                protocol::channel::fallback::credit(
                    &data.channel_id,
                    protocol::channel::CREDIT_DELTA,
                ),
            ))
        }
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
    let reason = e.to_string();
    push(
        types::CHANNEL_ERROR,
        protocol::channel::ChannelErrorPayload {
            channel_id: channel_id.to_string(),
            code: ChannelErrorCode::Unsupported,
            reason: reason.clone(),
        },
        protocol::channel::fallback::error(channel_id, ChannelErrorCode::Unsupported, &reason),
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
        assert!(is_nothing(on_channel_frame(
            &state,
            "n_a",
            &frame(types::CHANNEL_READY, serde_json::json!({"oops": 1}))
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

    /// 一张**真**的 Hub 侧账本：开通道、置 Ready，然后把信用扣到只剩一格。
    ///
    /// 直接调 `on_credit` 造余额是另一种写法，但那样测不到 `on_data_received`
    /// 自己的记账——而「收到数据 → 归零 → 补信用」这条链正是这里要锁的。
    fn one_credit_left(state: &AppState, node_id: &str, channel_id: &str, start: u32) {
        let mut channels = state.channels().write();
        let reg = channels.registry(node_id);
        reg.open(channel_id, "terminal", start, std::time::Instant::now())
            .expect("open");
        reg.mark_ready(channel_id).expect("mark_ready");
        for _ in 1..start {
            assert_eq!(
                reg.on_data_received(channel_id).expect("consume credit"),
                CreditStatus::Granted
            );
        }
    }

    /// **余额归零要补一轮信用**，且 Hub 自己的账本同步回填。
    ///
    /// 只发 `channel_credit` 而不记账，Hub 侧余额会恒为 0——下一帧又立刻
    /// 归零，于是每帧都补一次，node 侧余额越滚越大（§6 每通道独立流控失效）。
    #[test]
    fn exhausting_the_credit_replies_with_one_delta_and_replenishes_the_ledger() {
        let (state, _dir) = state();
        one_credit_left(&state, "n_a", "ch-1", protocol::channel::CREDIT_DELTA);

        let reply = on_channel_frame(
            &state,
            "n_a",
            &frame(
                types::CHANNEL_DATA,
                serde_json::json!({"channel_id": "ch-1", "payload": {"op": "x"}}),
            ),
        );
        let f = match reply {
            Reply::Frame(f) => f,
            Reply::Nothing => panic!("归零必须回一帧 channel_credit"),
        };
        assert_eq!(f.msg_type, types::CHANNEL_CREDIT);
        assert_eq!(f.c["channel_id"], "ch-1");
        assert_eq!(f.c["delta"], protocol::channel::CREDIT_DELTA);
        assert_eq!(
            state.channels().read().credit("n_a", "ch-1"),
            Some(protocol::channel::CREDIT_DELTA),
            "Hub 侧账本必须和发出去的那一帧一致"
        );
    }

    /// 余额**没**归零时不必补信用：一张 `Granted` 只记账、不回帧。
    ///
    /// 与上面那条成对：只测「归零回帧」的话，一个每帧都回 credit 的实现
    /// 也能绿。
    #[test]
    fn a_normal_data_frame_only_accounts_and_does_not_reply() {
        let (state, _dir) = state();
        one_credit_left(&state, "n_a", "ch-1", 3);
        // 上面把 3 扣到 1；再收一帧归零之前，先补回 2 格让这帧是 Granted。
        state
            .channels()
            .write()
            .registry("n_a")
            .on_credit("ch-1", 2)
            .expect("top up");

        assert!(is_nothing(on_channel_frame(
            &state,
            "n_a",
            &frame(
                types::CHANNEL_DATA,
                serde_json::json!({"channel_id": "ch-1", "payload": {"op": "x"}}),
            )
        )));
        assert_eq!(state.channels().read().credit("n_a", "ch-1"), Some(2));
    }

    /// 未知通道的数据帧要显式回 `channel_error`，而不是静默丢掉。
    #[test]
    fn data_for_an_unknown_channel_replies_with_an_error() {
        let (state, _dir) = state();
        let reply = on_channel_frame(
            &state,
            "n_a",
            &frame(
                types::CHANNEL_DATA,
                serde_json::json!({"channel_id": "ch-nope", "payload": {"op": "x"}}),
            ),
        );
        match reply {
            Reply::Frame(f) => {
                assert_eq!(f.msg_type, types::CHANNEL_ERROR);
                assert_eq!(f.c["channel_id"], "ch-nope");
            }
            Reply::Nothing => panic!("未知通道必须回一帧错误"),
        }
    }
}
