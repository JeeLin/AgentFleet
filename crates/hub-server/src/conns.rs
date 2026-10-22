//! 通往各条连接的**出站信道**旁路表（子任务 7、5b）。
//!
//! `hub_core::ConnHandle` 只存 `conn_id` + `gen`，**不能**塞 socket 或 channel——
//! 它是 `PartialEq`/`Clone` 的纯数据，且 `ws_flow` 有一条测试在类型层面断言
//! 它的 `Debug` 输出里没有 socket 类字样（「连接句柄永不落库」的守卫）。
//! 所以「向某条连接推一帧」的能力放在本模块，按 `conn_id` 寻址。
//!
//! **每连接两条队列**（`protocol.md` §6.1.1）：控制面 8 帧、数据面 256 帧。
//! 分队列只有一个目的——终端刷屏时，`disconnect{superseded}` 不能排在
//! 256 帧 PTY 输出后面；设备被顶替却收不到通知，就会和新连接互抢。
//! 控制面容量小是故意的：控制帧只有「被顶替」与「手动下线」两种，
//! 8 格是余量而不是通道。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use protocol::frame::Frame;
use tokio::sync::mpsc;

/// 一帧待发的出站帧。用 `Frame` 而不是 `String`：
/// 序列化在本模块之外决定，调用方不必知道文本格式。
pub type Outbound = Frame;

/// 每条连接的控制帧队列容量。
///
/// 有界是故意的：控制帧只有「被顶替」与「手动下线」两种，一台设备一次只会
/// 收到一个。给 8 是留余量（同一条连接上连续几次顶替），但**不允许无界**——
/// 无界队列会让「Hub 想让旧连接走」这件事永远延迟兑现，而旧连接每多活一秒
/// 就多一次互抢。
pub(crate) const CONTROL_QUEUE: usize = 8;

/// 每条连接的数据帧队列容量（`protocol.md` §6.1.1：写侧有界）。
///
/// 256 帧 ≈ 一次终端滚屏的积压量。有界是**唯一**能让「对端不读」变成
/// 可观测错误的手段：`tungstenite` 的 `max_write_buffer_size` 默认
/// `usize::MAX`（写缓冲无上限），而 `axum` 的 `Sink` 会把它照单全收——
/// 没有这一层，一台不读帧的设备能把 Hub 的内存推成无界增长。
pub(crate) const DATA_QUEUE: usize = 256;

/// 推送失败的原因。命名而不是 `()`：调用方要能区分「它已经走了」与
/// 「它还在但推不进去」，后者是异常且必须留痕。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// 连接还在，但**该条队列**满了——它迟迟不消费。
    /// 这通常意味着那条连接半死不活，gen 兜底会接住它。
    QueueFull,
}

/// 一条连接的两条出站队列。
///
/// 用 `Sender` 保存在表里而不是 `Weak`：连接断开时会显式 `remove`，
/// 而「发送端全被丢弃 → 写协程退出」正是正常的收尾信号。
#[derive(Debug, Clone)]
struct Queues {
    control: mpsc::Sender<Outbound>,
    data: mpsc::Sender<Outbound>,
}

/// 推哪条队列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Control,
    Data,
}

/// `conn_id → 该连接的出站队列`。
#[derive(Debug, Clone, Default)]
pub struct OutboundTable(Arc<RwLock<HashMap<String, Queues>>>);

impl OutboundTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条新连接，返回 `(控制面收端, 数据面收端)`（交给写协程消费）。
    pub fn register(&self, conn_id: &str) -> (mpsc::Receiver<Outbound>, mpsc::Receiver<Outbound>) {
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let (data_tx, data_rx) = mpsc::channel(DATA_QUEUE);
        self.0.write().insert(
            conn_id.to_string(),
            Queues {
                control: control_tx,
                data: data_tx,
            },
        );
        (control_rx, data_rx)
    }

    /// 向一条连接推一帧**控制帧**（`disconnect` 等）。
    ///
    /// 返回是否真的投递了。两个假结果含义不同，调用方要能区分：
    /// - `false`：表里没有这条连接（它已经断开并被摘掉了）；
    /// - `Err`：**在表里，但队列满了**——连接还在，却迟迟不消费控制帧。
    ///   这属于异常，必须留痕而不是静默丢弃（「帧没发出去」正是排查
    ///   互抢环的唯一线索）。
    pub fn send(&self, conn_id: &str, frame: Outbound) -> Result<bool, PushError> {
        self.push(conn_id, frame, Which::Control)
    }

    /// 向一条连接推一帧**数据帧**（通道回帧，子任务 5b）。
    ///
    /// 与 [`Self::send`] 走**不同的队列**，因此数据面打满不会挤占控制面；
    /// 写协程按 `biased` 先取控制面（见 `ws::write_loop`）。
    /// 队列满时由调用方丢弃这一帧并回 `err{code:"flood"}`：PTY 输出是纯推送，
    /// 丢了就丢了，堵住控制面才是真事故。
    pub fn send_data(&self, conn_id: &str, frame: Outbound) -> Result<bool, PushError> {
        self.push(conn_id, frame, Which::Data)
    }

    /// 两个 `send` 的共同实现。**只有一份**：`try_send` 的失败映射与
    /// 「表里没有这条连接」的返回口径若各写一遍，改一处漏一处的症状是
    /// 「控制帧返回 Gone、数据帧仍返回 `Ok(false)`」，而两个调用点
    /// （`ws.rs` 的 `queue_control` / `queue_data`）对这两种返回的处理不同——
    /// 数据面会静默丢帧而控制面走留痕分支。
    fn push(&self, conn_id: &str, frame: Outbound, which: Which) -> Result<bool, PushError> {
        let Some(q) = self.0.read().get(conn_id).cloned() else {
            return Ok(false);
        };
        // `try_send` 而不是 `send`：这里**不能** await。
        // 顶替发生在握手路径上，await 一个满队列会把整条握手堵死；
        // 而队列满本身就说明对端半死不活，等下去没有意义。
        let sent = match which {
            Which::Control => q.control.try_send(frame),
            Which::Data => q.data.try_send(frame),
        };
        sent.map_err(|_| PushError::QueueFull)?;
        Ok(true)
    }

    /// 摘掉一条连接（断开时）。
    pub fn remove(&self, conn_id: &str) {
        self.0.write().remove(conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::types;

    fn frame() -> Frame {
        Frame::push(
            types::DISCONNECT,
            0,
            serde_json::json!({"reason": "superseded"}),
        )
    }

    fn data_frame() -> Frame {
        Frame::push(
            types::CHANNEL_DATA,
            0,
            serde_json::json!({"channel_id": "ch-1", "payload": {"chunk": "x"}}),
        )
    }

    /// 登记后能收到推过来的帧。
    #[tokio::test]
    async fn a_registered_connection_receives_the_frame() {
        let t = OutboundTable::new();
        let (mut control, _data) = t.register("c1");
        assert!(t.send("c1", frame()).unwrap());
        let got = control.recv().await.expect("must arrive");
        assert_eq!(got.msg_type, types::DISCONNECT);
    }

    /// 没登记过的连接：返回 false（不是错误——它已经走了）。
    #[test]
    fn an_unknown_connection_is_a_no_op() {
        let t = OutboundTable::new();
        assert!(!t.send("nope", frame()).unwrap());
        assert!(!t.send_data("nope", data_frame()).unwrap());
    }

    /// 摘掉之后再推：同样返回 false。
    #[test]
    fn a_removed_connection_is_a_no_op() {
        let t = OutboundTable::new();
        let (_control, _data) = t.register("c1");
        t.remove("c1");
        assert!(!t.send("c1", frame()).unwrap());
        assert!(!t.send_data("c1", data_frame()).unwrap());
    }

    /// 一条连接把队列塞满，**不能连累别的连接**（§6.2「每通道独立信用」
    /// 在控制帧上的对应面）。按 `conn_id` 分桶就是为了这条：一台设备半死不活
    /// 不该让别的设备收不到「你被顶替了」。
    #[tokio::test]
    async fn one_stalled_connection_does_not_block_the_others() {
        let t = OutboundTable::new();
        let (_stalled_control, _stalled_data) = t.register("c_stuck"); // 收端从不消费
        let (mut healthy, _healthy_data) = t.register("c_ok");
        for _ in 0..CONTROL_QUEUE {
            assert!(t.send("c_stuck", frame()).unwrap());
        }
        assert_eq!(t.send("c_stuck", frame()), Err(PushError::QueueFull));

        assert!(t.send("c_ok", frame()).unwrap());
        let got = healthy.recv().await.expect("must arrive");
        assert_eq!(got.msg_type, types::DISCONNECT);
    }

    /// 队列满 → `Err`，且**不至于 panic**。
    ///
    /// 这条锁住「有界」这个决定：换成无界队列，`try_send` 永远不会失败，
    /// 而 Hub 想让旧连接走这件事会被无限延迟。
    #[test]
    fn a_full_queue_reports_an_error_instead_of_growing() {
        let t = OutboundTable::new();
        let (_control, _data) = t.register("c1"); // 收端不消费
        for _ in 0..CONTROL_QUEUE {
            assert!(t.send("c1", frame()).unwrap());
        }
        assert_eq!(
            t.send("c1", frame()),
            Err(PushError::QueueFull),
            "有界队列必须在满时报错，而不是静默扩张"
        );
    }

    /// 5b 的核心：**数据面打满时控制面照常投递**。
    ///
    /// 两者是两条独立队列，所以「终端刷屏」与「设备被顶替」不会互相排队。
    /// 数据面容量就是 [`DATA_QUEUE`]：第 257 帧必须报 `QueueFull`，
    /// 而控制面的第 1 帧在同一时刻仍要成功。
    #[test]
    fn a_full_data_queue_does_not_block_the_control_queue() {
        let t = OutboundTable::new();
        let (mut control, _data) = t.register("c1"); // 数据面收端同样不消费
        for _ in 0..DATA_QUEUE {
            assert!(t.send_data("c1", data_frame()).unwrap());
        }
        assert_eq!(
            t.send_data("c1", data_frame()),
            Err(PushError::QueueFull),
            "数据面必须有界（{DATA_QUEUE} 帧）"
        );
        assert!(
            t.send("c1", frame()).unwrap(),
            "数据面打满时控制帧仍必须能入队"
        );
        assert_eq!(
            control.try_recv().expect("control frame").msg_type,
            types::DISCONNECT
        );
    }
}
