//! terminal connector 的**接线**测试：帧进 → PTY → 帧出。
//!
//! `connector/terminal.rs` 里的单元测试验的是零件（尺寸校验、载荷解析、
//! 字节流往返）。这里验的是零件之间那一段：`channel::on_channel_frame`
//! 派发、`channel::pump()` 主动问、信用放行。这段是 M1 第一个演示能否跑起来的
//! 全部依赖，而它在单元测试里是看不见的。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use node_core::channel;
use node_core::connector::names;
use protocol::channel::{ChannelCredit, ChannelData, OpenChannel};
use protocol::frame::Frame;
use protocol::message::types;

/// 通道表是**进程级单例**（`channel.rs` 里的 `static TABLE`），所以这些
/// 场景只能一个一个来：`reset_for_tests` 会把别人的通道一起清掉，
/// `on_disconnect` 的计数断言也会数到别人的通道。
///
/// 这不是测试写法上的洁癖，而是被测对象的形状——一台设备一个进程、
/// 一套通道，所以「串行」本来就是它真实的样子。
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_frame(channel_id: &str, connector: &str, credit: u32) -> Frame {
    let spec = OpenChannel {
        channel_id: channel_id.to_string(),
        connector: connector.to_string(),
        params: serde_json::json!({ "shell": "/bin/sh" }),
        credit_start: credit,
    };
    Frame::push(
        types::OPEN_CHANNEL,
        1,
        serde_json::to_value(&spec).expect("open spec"),
    )
}

fn data_frame(channel_id: &str, payload: serde_json::Value) -> Frame {
    let spec = ChannelData {
        channel_id: channel_id.to_string(),
        payload,
    };
    Frame::push(
        types::CHANNEL_DATA,
        2,
        serde_json::to_value(&spec).expect("data spec"),
    )
}

fn credit_frame(channel_id: &str, delta: u32) -> Frame {
    let spec = ChannelCredit {
        channel_id: channel_id.to_string(),
        delta,
    };
    Frame::push(
        types::CHANNEL_CREDIT,
        3,
        serde_json::to_value(&spec).expect("credit spec"),
    )
}

/// 一轮 `pump`：把所有通道产出的字节拼成一段文本。
fn pump_once() -> String {
    let mut text = String::new();
    for (id, frames) in channel::pump() {
        for f in frames {
            // `channel_data` 的 `c` 是 `ChannelData{channel_id, payload}`
            // 信封（`protocol.md` §4）：connector 的载荷在 **`payload` 里**，
            // 不是 `c` 本身。曾经这里是 `f.c.get("bytes")`——那正是
            // 「通道 ready、输出真的产生了、页面却一片空白」那个 bug 的写法。
            assert_eq!(
                f.c.get("channel_id").and_then(|v| v.as_str()),
                Some(id.as_str()),
                "channel_data 必须带 channel_id 信封：{}",
                f.c
            );
            if let Some(s) = f.c.get("payload").and_then(|p| p.get("bytes")).and_then(|b| b.as_str())
            {
                text.push_str(s);
            }
        }
    }
    text
}

/// 一直 pump 到 `needle` 出现或超时。**必须带超时**：一个什么都不产出的
/// 实现会在这里挂死，而挂死的测试读起来像「测试慢」，不像「实现坏」。
fn pump_until(needle: &str, budget: std::time::Duration) -> String {
    let deadline = std::time::Instant::now() + budget;
    let mut text = String::new();
    while std::time::Instant::now() < deadline {
        text.push_str(&pump_once());
        if text.contains(needle) {
            return text;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    text
}

#[test]
fn a_typed_command_comes_back_as_channel_data_with_no_further_downlink() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let action = channel::on_channel_frame(&open_frame("ch-e2e", names::TERMINAL, 16));
    assert!(!action.frames().is_empty(), "开通道至少要回 channel_ready");

    channel::on_channel_frame(&data_frame(
        "ch-e2e",
        serde_json::json!({ "bytes": "echo e2e-marker\nexit\n" }),
    ));

    // **只 pump，一个下行帧都不再发**：这就是 M1 要证明的那件事——
    // 终端的输出是被主动问出来的，不依赖用户再敲一个键。
    let text = pump_until("e2e-marker", std::time::Duration::from_secs(60));
    assert!(
        text.contains("e2e-marker"),
        "命令的输出没有主动回来：{text:?}"
    );
}

#[test]
fn the_channel_is_released_once_the_shell_exits() {
    let _guard = exclusive();
    channel::reset_for_tests();
    channel::on_channel_frame(&open_frame("ch-bye", names::TERMINAL, 16));
    assert!(channel::live_count() >= 1);

    channel::on_channel_frame(&data_frame(
        "ch-bye",
        serde_json::json!({ "bytes": "exit\n" }),
    ));

    // shell 退出之后终端不再产出，通道最终由断连/close 释放。
    // **判据是「输出停了」而不是「活通道数降了」**：后者要等到 Hub 发
    // `close_channel` 或连接断开才发生，而本测试里没有那一步——拿它当判据
    // 就只能写成「等到超时也算过」的恒真断言。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut quiet_from: Option<std::time::Instant> = None;
    while std::time::Instant::now() < deadline {
        let chunk = pump_once();
        let now = std::time::Instant::now();
        match (chunk.is_empty(), &quiet_from) {
            (false, _) => quiet_from = None,
            (true, None) => quiet_from = Some(now),
            // 连着 1s 什么都没产出来 —— shell 确实退了。
            (true, Some(from)) if now.duration_since(*from) > std::time::Duration::from_secs(3) => {
                assert!(
                    pump_once().is_empty(),
                    "shell 退出之后还在持续产出，它没真的退出"
                );
                return;
            }
            (true, Some(_)) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("shell 在 20s 内没有退出");
}

/// 输出超过信用额度时**不丢字节**：信用归零后剩下的进 backlog，
/// 补 `channel_credit` 才放行。
#[test]
fn output_beyond_the_credit_budget_is_queued_rather_than_dropped() {
    let _guard = exclusive();
    channel::reset_for_tests();
    channel::on_channel_frame(&open_frame("ch-credit", names::TERMINAL, 2));
    channel::on_channel_frame(&data_frame(
        "ch-credit",
        serde_json::json!({
            "bytes": "i=0; while [ $i -lt 4000 ]; do printf '%08d\\n' $i; i=$((i+1)); done; printf 'DONE\\n'\n"
        }),
    ));

    // 信用只有 2。**先等到 backlog 里真的有东西**再看——刚敲完命令时
    // shell 还没吐出任何东西，此刻 backlog=0 是「还没开始」而不是「丢了」。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while channel::backlog_of("ch-credit") == 0 && std::time::Instant::now() < deadline {
        pump_once();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let stalled = channel::backlog_of("ch-credit");
    assert!(
        stalled > 0,
        "产出远超信用额度却什么都没排队：backlog={stalled}"
    );

    // 补信用并继续 pump，直到拿到结束标记。
    channel::on_channel_frame(&credit_frame("ch-credit", 100_000));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut text = String::new();
    while std::time::Instant::now() < deadline && !text.contains("DONE") {
        text.push_str(&pump_once());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(text.contains("DONE"), "补了信用之后输出仍然没放行出来");
    channel::on_disconnect();
}

/// 断连把所有通道一次释放干净——包括还开着的 PTY 与它的读线程。
#[test]
fn a_disconnect_releases_the_terminal_and_its_reader() {
    let _guard = exclusive();
    channel::reset_for_tests();
    channel::on_channel_frame(&open_frame("ch-dc", names::TERMINAL, 16));
    let released = channel::on_disconnect();
    assert_eq!(released, 1, "断连应当正好释放一条通道");
    assert!(!channel::has_channel("ch-dc"), "断连之后通道还在表里");
    assert!(pump_once().is_empty(), "释放之后再 pump 不该还有输出");
}
