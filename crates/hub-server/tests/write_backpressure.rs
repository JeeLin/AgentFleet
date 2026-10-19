//! 子任务 5b「写侧有界队列」的端到端测试。
//!
//! 规格明确**不许**写「拥塞期间 `on_heartbeat` 仍被调用」这类恒真断言：
//! 拥塞下心跳照样会被调用、只是晚 40s，而本子任务要防的正是「晚」。
//! 按规格给的方案 ② 落地：断言 `disconnect{superseded}` 在数据面打满时
//! 仍能**插到数据积压前面**送达，并把阈值写实。
//!
//! 这里刻意不走「让 node 慢读 PTY 输出」那条路：那条要先把内核 socket
//! 缓冲区灌满，再赌 `axum` 的写缓冲，测出来的东西随内核参数漂移。
//! 直接对 `OutboundTable` 打数据帧、再读真实 WS，判据只剩「控制帧排在
//! 数据帧前面没有」这一条——而那正是两条队列存在的唯一理由。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use protocol::frame::Frame;
use protocol::message::types;

/// 数据面容量（与 `conns::DATA_QUEUE` 同值）。这里手写一份是**故意的**：
/// 测试要能从外部察觉到「256 帧」这个契约变了，而不是跟着实现一起改。
const EXPECTED_DATA_QUEUE: usize = 256;

/// 一个足够大、能把内核 socket 写缓冲填住的 `channel_data`。
///
/// 64 KiB × 若干个就到 MB 级；再大没有意义——判据是「写协程被挡住」，
/// 而不是「某块内存满了」。
fn chunky_data(seq: usize) -> Frame {
    Frame::push(
        types::CHANNEL_DATA,
        seq as u64,
        serde_json::json!({
            "channel_id": "ch-backpressure",
            "payload": {"chunk": "x".repeat(64 * 1024)},
        }),
    )
}

/// 数据面打满时，控制帧仍必须能入队并**先于数据积压**送达。
///
/// 阈值定法：`elapsed <= 2s` 是「读取积压 + 收一帧」的上限量级，取自实测
/// （本机 debug 构建约 10–40ms，取 50 倍余量吸收 CI 抖动）。比阈值更硬的
/// 判据是**帧序**：`disconnect` 必须在最前面的几十帧内出现。写协程多半在
/// 第 1–3 帧就卡在内核缓冲上，一解冻就按 `biased` 先取控制面，所以它排在第
/// `k+1` 帧（k = 卡住前已写进内核的帧数）。取 64 是给不同内核的 socket
/// 缓冲留余量；若控制帧被排在 256 帧数据之后，这个断言必然红。
#[tokio::test]
async fn a_control_frame_overtakes_a_full_data_queue() {
    let hub = common::Hub::boot().await;
    let (mut est, node_id, _kp) = common::connect_enrolled(&hub).await;
    // 等连接进注册表，拿到 conn_id（`send_data` 按它寻址）。
    common::wait_until(Duration::from_secs(2), || {
        hub.state.connections().get(&node_id).is_some()
    })
    .await;
    let conn_id = hub
        .state
        .connections()
        .get(&node_id)
        .expect("connection must be registered")
        .conn_id;

    // node 侧**一次都不读**，写协程迟早卡在内核缓冲上；此前数据队列已积压。
    let mut sent = 0usize;
    let mut full_at = None;
    for seq in 0..(EXPECTED_DATA_QUEUE * 4) {
        match hub.state.outbound().send_data(&conn_id, chunky_data(seq)) {
            Ok(true) => sent += 1,
            Ok(false) => panic!("connection vanished while filling the data queue"),
            Err(_) => {
                full_at = Some(seq);
                break;
            }
        }
    }
    let full_at = full_at.expect("data queue must be bounded and eventually full");
    assert!(
        full_at >= EXPECTED_DATA_QUEUE,
        "队列容量不得小于契约值：满于第 {full_at} 帧"
    );
    assert!(sent >= EXPECTED_DATA_QUEUE, "实际入队 {sent} 帧");

    // 数据面满着，控制面必须照收——这是两条队列的核心断言。
    let before = Instant::now();
    assert!(
        hub.state
            .outbound()
            .send(
                &conn_id,
                Frame::push(
                    types::DISCONNECT,
                    0,
                    serde_json::json!({"reason": "superseded", "gen": 9999}),
                ),
            )
            .expect("control queue must not be full"),
        "control frame must be enqueued while the data queue is full"
    );

    // 开始读：内核缓冲里的数据帧先到（TCP 无法插队），然后必须是 disconnect。
    let mut seen = 0usize;
    loop {
        let text = est
            .next_text()
            .await
            .expect("read must succeed")
            .expect("hub must still be sending");
        let frame: Frame = serde_json::from_str(&text).expect("frame json");
        seen += 1;
        if frame.msg_type == types::DISCONNECT {
            assert_eq!(frame.c["reason"], "superseded");
            break;
        }
        assert!(
            seen < 64,
            "disconnect 必须插到数据积压前面（已读 {seen} 帧仍未见到它）"
        );
    }
    let elapsed = before.elapsed();
    assert!(
        elapsed <= Duration::from_secs(2),
        "控制帧送达耗时 {elapsed:?}，超出 2s 预算"
    );
}
