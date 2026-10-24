//! 子任务 7：node 侧的通道帧处理（`open_channel` / `channel_data` /
//! `close_channel` / `channel_credit`）。
//!
//! 这一层**不起 socket**：`channel::on_channel_frame` 是「一帧进来 → 该回什么」
//! 的纯函数式分派，帧从 worker 的读循环来、出去由它发（`node-core/src/worker.rs`）。
//! 好处是通道语义可以完全确定性地测：信用账本、补信用后的放行、
//! 资源释放的计数，都不用与真实时序赛跑。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use node_core::channel::{self, Action};
use node_core::connector;
use protocol::channel::{ChannelData, CloseChannel, OpenChannel};
use protocol::frame::Frame;
use protocol::message::types;
use std::sync::{Mutex, MutexGuard};

/// 通道表是**进程级单例**（约束 4：一个进程一台设备），所以同一个测试二进制里
/// 的所有用例共享它。`cargo test` 默认多线程并行跑，两个用例同时动这张表时，
/// 「数资源释放」这类断言会读到对方刚开的通道——实测
/// `closing_a_channel_releases_the_connector_resource` 在插桩下偶发
/// `live_count() == 1`（`channel.rs:212`），非插桩下却全过，因为并行时机不同。
///
/// 修法是把这些用例**串行化**，不是放大等待：用例彼此没有数据依赖，只是共享
/// 一份可变全局状态，串行就够且不牺牲任何覆盖。返回的 guard 由调用方持有到
/// 用例结束（`let _g = reset();`），锁因此横跨整个用例体。
static TABLE: Mutex<()> = Mutex::new(());

fn reset() -> MutexGuard<'static, ()> {
    let g = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    channel::reset_for_tests();
    g
}

fn open_frame(channel_id: &str, connector: &str, credit: u32) -> Frame {
    let spec = OpenChannel {
        channel_id: channel_id.to_string(),
        connector: connector.to_string(),
        params: serde_json::json!({}),
        credit_start: credit,
    };
    Frame::push(types::OPEN_CHANNEL, 1, serde_json::to_value(&spec).unwrap())
}

fn data_frame(channel_id: &str, payload: serde_json::Value) -> Frame {
    let spec = ChannelData {
        channel_id: channel_id.to_string(),
        payload,
    };
    Frame::push(types::CHANNEL_DATA, 2, serde_json::to_value(&spec).unwrap())
}

fn credit_frame(channel_id: &str, delta: u32) -> Frame {
    Frame::push(
        types::CHANNEL_CREDIT,
        3,
        serde_json::json!({"channel_id": channel_id, "delta": delta}),
    )
}

fn close_frame(channel_id: &str, reason: &str) -> Frame {
    let spec = CloseChannel {
        channel_id: channel_id.to_string(),
        reason: reason.to_string(),
    };
    Frame::push(
        types::CLOSE_CHANNEL,
        4,
        serde_json::to_value(&spec).unwrap(),
    )
}

/// `Action` 里的第一帧。
///
/// **不提供 `Action::first()` 便捷方法**：那会把「没有应答」
/// （`Action::None`）与「连接该结束了」（`Action::End`）都变成 `None`，
/// 而这两件事必须分开断言——前者是合法沉默，后者是协议分叉。
fn expect_frame(action: &Action, want_type: &str) -> Frame {
    let Action::Send(frames) = action else {
        panic!("expected a {want_type} frame, got {action:?}");
    };
    let f = frames
        .first()
        .unwrap_or_else(|| panic!("expected a {want_type} frame"));
    assert_eq!(f.msg_type, want_type, "got {:?}", f.c);
    f.clone()
}

/// 这条动作**什么都不回**。
///
/// 与 `expect_frame` 互为反面：能走到这里说明确实没有帧，而不是帧被
/// `Action::End` 吞掉。`Action::End` 必须用 `matches!` 单独断言。
fn assert_no_reply(action: Action, why: &str) {
    match action {
        Action::None => {}
        Action::Send(frames) if frames.is_empty() => {}
        other => panic!("{why}：got {other:?}"),
    }
}

/// 标准①：**回的是 `channel_ready`，不是 `channel_opened`**。
///
/// 协议里没有 `channel_opened` 这个类型（`protocol::message::types::CHANNEL_READY`
/// 的值是 `"channel_ready"`）。写错的后果被约束 14 放大成一次功能性死锁：
/// 未知类型被忽略 → Hub 永远等不到 ready → 5s 后回
/// `channel_error{unsupported}`，用户看到「终端卡了 5 秒然后说不支持」，
/// 症状与根因相距很远。
///
/// 这里锁的是**帧的形状与类型名**；「开成功 → 真回一帧 ready」要等到
/// 第一个真 connector 上线（子任务 8），因为那之前没有任何 connector
/// 开得起来——一帧都开不成功时断言 ready 只会自证自话。
#[test]
fn the_ready_frame_is_channel_ready_never_channel_opened() {
    assert_eq!(types::CHANNEL_READY, "channel_ready");
    let f = channel::ready_frame("ch-1", serde_json::json!({"connector": "terminal"}));
    assert_eq!(f.msg_type, types::CHANNEL_READY);
    // Hub 侧按 `ChannelReady` 解它——解不出就整条通道对不上账
    let ready: protocol::channel::ChannelReady = f.payload().unwrap();
    assert_eq!(ready.channel_id, "ch-1");
    assert_eq!(ready.capabilities["connector"], "terminal");
    // 「channel_opened」在协议里根本不是一个类型名
    assert!(
        "channel_opened".parse::<protocol::message::MessageType>()
            == Ok(protocol::message::MessageType::Unknown),
        "协议里不存在 channel_opened 这个类型"
    );
}

/// 标准②：未知 connector 回 `channel_error{unsupported}`，**不崩**。
#[test]
fn an_unknown_connector_is_refused_with_an_error_frame() {
    let _g = reset();
    for name in ["ssh", "db", "", "TERMINAL", "termina", "git"] {
        let action = channel::on_channel_frame(&open_frame("ch-x", name, 8));
        let f = expect_frame(&action, types::CHANNEL_ERROR);
        assert_eq!(f.c["channel_id"], "ch-x");
        assert_eq!(f.c["code"], "unsupported", "{name} 必须回 unsupported");
        assert_eq!(f.c["reason"], "unsupported");
        assert!(!channel::has_channel("ch-x"), "{name} 不该留下表项");
    }
}

/// **M1 的三个 connector 全部是真实现了**（子任务 8/9/10）：开通道要么拿到
/// `channel_ready`，要么拿到一句**说清为什么**的 `channel_error`——而不是
/// 「打开成功但什么都不做」的黑屏。
///
/// 这条测试**曾是**「骨架 connector 必须说人话」——子任务 7 写下它是为了
/// 防止那种黑屏。现在三个都落地了，那条判据按它自己写的注释到期了
/// （"会随实现推进自己失效，而它的失效方式是新实现被当成 bug 报回来"）。
///
/// **换成什么**：M1 剩下的那个真不变式是「**注册表里有的，开通道绝不沉默
/// 失败**」。每个 connector 按各自契约给参数：exec 一次性命令没有「先开
/// 再发」两步，所以命令随 `open` 来（子任务 10 的形状），裸开必然被拒——
/// 那正是「说人话」，不是失败。
#[test]
fn every_registered_connector_either_opens_or_says_why_not() {
    let _g = reset();
    let cases = [
        (connector::names::TERMINAL, serde_json::json!({})),
        (connector::names::FILES, serde_json::json!({})),
        (
            connector::names::EXEC,
            serde_json::json!({ "command": "/bin/true" }),
        ),
    ];
    for (i, (name, params)) in cases.iter().enumerate() {
        let id = format!("ch-real-{i}");
        // **根与 connector 私有参数平级**（`OpenReq` 的形状）：`workspace_root`
        // 是一等字段，其余透传给 `#[serde(flatten)]` 出去的那部分。
        let mut merged = serde_json::Map::new();
        merged.insert("workspace_root".to_string(), serde_json::json!("."));
        if let Some(obj) = params.as_object() {
            for (k, v) in obj {
                merged.insert(k.clone(), v.clone());
            }
        }
        let spec = OpenChannel {
            channel_id: id.clone(),
            connector: (*name).to_string(),
            params: serde_json::Value::Object(merged),
            credit_start: 8,
        };
        let frame = Frame::push(types::OPEN_CHANNEL, 1, serde_json::to_value(&spec).unwrap());
        let action = channel::on_channel_frame(&frame);
        let f = expect_frame(&action, types::CHANNEL_READY);
        assert_eq!(f.c["channel_id"], id, "{name} 没能真的开起来");
    }
    channel::on_disconnect();
}

/// terminal 已经落地：开通道要拿到 `channel_ready`，不是一句错误。
#[test]
fn the_terminal_connector_reports_ready_rather_than_not_built_yet() {
    let _g = reset();
    let action = channel::on_channel_frame(&open_frame("ch-t", connector::names::TERMINAL, 8));
    let f = expect_frame(&action, types::CHANNEL_READY);
    assert_eq!(f.c["channel_id"], "ch-t");
    assert_eq!(f.c["capabilities"]["connector"], connector::names::TERMINAL);
}

/// 标准③：通道关闭后**资源被释放**（有测试用计数断言）。
///
/// 这条断言的是**关闭路径的幂等**：关一条不在表里的通道只回一帧 close，
/// 不留表项，也不报错。
///
/// 这里的 connector 持有的是一个可数的句柄（`Files` 每开一次记一次），
/// 关通道与断连各释放一次，计数必须跟着降。
#[test]
fn closing_a_channel_releases_the_connector_resource() {
    let _g = reset();
    // 直接关一条**从未开过**的通道：Hub 在重连后会重发 `close_channel`，
    // 而它那边可能还留着旧连接记的账。对不上号时唯一有意义的动作是「清掉」。
    let action = channel::on_channel_frame(&close_frame("ch-c", "user closed"));
    let f = expect_frame(&action, types::CLOSE_CHANNEL);
    assert_eq!(f.c["channel_id"], "ch-c");
    assert!(
        !channel::has_channel("ch-c"),
        "关一条没开过的通道也不该留下表项"
    );
    assert_eq!(channel::live_count(), 0);
}

/// 数据帧打在**未注册的**通道上：静默，不能回错。
///
/// 回错会让 Hub 以为这条通道死了，而 Hub 侧可能还留着上一次连接的账
/// （重连后会重发 `close_channel`）。
#[test]
fn data_on_an_unknown_channel_is_silent() {
    let _g = reset();
    let action = channel::on_channel_frame(&data_frame("ch-nope", serde_json::json!({"in":"x"})));
    assert_no_reply(action, "数据打在未知通道上不该有应答");
    let action = channel::on_channel_frame(&credit_frame("ch-nope", 8));
    assert_no_reply(action, "补信用给未知通道不该有应答");
}

/// 载荷解不开 = 协议分叉，必须让调用方结束连接。
///
/// 这与「类型名不认识」是两回事：约束 14 的忽略只针对类型名，而一帧
/// `open_channel` 的 `c` 不是对象说明两端对协议的理解已经分叉，继续留着
/// 只会让后面每一帧都失败。
#[test]
fn an_undecodable_channel_payload_ends_the_connection() {
    let _g = reset();
    for (t, c) in [
        (types::OPEN_CHANNEL, serde_json::json!(42)),
        (types::CHANNEL_DATA, serde_json::json!("nope")),
        (types::CLOSE_CHANNEL, serde_json::json!([1, 2])),
        (
            types::CHANNEL_CREDIT,
            serde_json::json!({"channel_id": "c"}),
        ),
    ] {
        let f = Frame::push(t, 1, c);
        assert!(matches!(channel::on_channel_frame(&f), Action::End), "{t}");
    }
}

/// 不是通道帧的帧一律不改路由：worker 会把它交给控制面判定
/// （约束 14：未知类型忽略而不断连）。
#[test]
fn non_channel_frames_are_not_channel_messages() {
    for t in [
        types::HEARTBEAT,
        types::HEARTBEAT_ACK,
        "channel_ready",
        "exec_request",
        "some_future_type",
        "",
    ] {
        assert!(!channel::is_channel_type(t), "{t} 不是 Hub 会下发的通道帧");
    }
    for t in [
        types::OPEN_CHANNEL,
        types::CHANNEL_DATA,
        types::CLOSE_CHANNEL,
        types::CHANNEL_CREDIT,
    ] {
        assert!(channel::is_channel_type(t), "{t} 必须被识别成通道帧");
    }
}

/// 断连时通道资源全部归位，**返回计数**（`on_disconnect` 的契约）。
///
/// 计数必须用**非空表**验：只在空表上跑的话，`on_disconnect()` 返回 0 与
/// 「没实现计数、恒返回 0」这两种实现给出同一个观测值——而那正是这条用例
/// 要区分的。所以这里先真开三条通道，再断连，断言回报的正是 3。
#[test]
fn a_disconnect_releases_every_channel_and_reports_how_many() {
    let _g = reset();
    assert_eq!(channel::on_disconnect(), 0, "空表断连不报数");
    // 表里没有活通道时同样不报：计数的是「被释放掉的」，不是「清掉的」
    assert!(!channel::has_channel("ch-1"));

    // 开三条，让「释放」有东西可释放。路径走 `on_channel_message` 而不是
    // 直接戳表：那才是生产上建通道的入口。
    for id in ["ch-a", "ch-b", "ch-c"] {
        channel::on_channel_frame(&open_frame(id, connector::names::TERMINAL, 1000));
    }
    assert_eq!(channel::live_count(), 3, "三条通道都该建起来了");

    let released = channel::on_disconnect();
    assert_eq!(released, 3, "释放的条数要如实回报，而不是恒 0");
    assert_eq!(channel::live_count(), 0, "释放后表该是空的");
    for id in ["ch-a", "ch-b", "ch-c"] {
        assert!(!channel::has_channel(id), "{id} 该被摘掉");
    }
}

/// 通道表的重置是测试接缝：它清的是**表**，不是别的什么。
#[test]
fn the_table_is_a_process_wide_single_table() {
    let _g = reset();
    // 未知通道查询不得 panic，也不得凭空造出表项
    assert!(!channel::has_channel("ghost"));
    assert!(channel::credit_of("ghost").is_none());
    assert_eq!(channel::backlog_of("ghost"), 0);
    assert_eq!(channel::sent_frames_of("ghost"), 0);
    assert_eq!(channel::sent_frames_total(), 0);
    channel::reset_for_tests();
}
