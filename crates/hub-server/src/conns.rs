//! 通往各条连接的**出站信道**旁路表（子任务 7）。
//!
//! `hub_core::ConnHandle` 只存 `conn_id` + `gen`，**不能**塞 socket 或 channel——
//! 它是 `PartialEq`/`Clone` 的纯数据，且 `ws_flow` 有一条测试在类型层面断言
//! 它的 `Debug` 输出里没有 socket 类字样（「连接句柄永不落库」的守卫）。
//! 所以「向某条连接推一帧」的能力放在本模块，按 `conn_id` 寻址。
//!
//! 这里只放**控制帧**（`disconnect{superseded|admin}`）。数据流是另一个方向
//! （node→Hub），不需要这张表。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use protocol::frame::Frame;
use tokio::sync::mpsc;

/// 一帧待发的控制帧。用 `Frame` 而不是 `String`：
/// 序列化在本模块之外决定，调用方不必知道文本格式。
pub type Outbound = Frame;

/// 每条连接的控制帧队列容量。
///
/// 有界是故意的：控制帧只有「被顶替」与「手动下线」两种，一台设备一次只会
/// 收到一个。给 8 是留余量（同一条连接上连续几次顶替），但**不允许无界**——
/// 无界队列会让「Hub 想让旧连接走」这件事永远延迟兑现，而旧连接每多活一秒
/// 就多一次互抢。
pub(crate) const CONTROL_QUEUE: usize = 8;

/// 推送失败的原因。命名而不是 `()`：调用方要能区分「它已经走了」与
/// 「它还在但推不进去」，后者是异常且必须留痕。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// 连接还在，但控制帧队列满了——它迟迟不消费。
    /// 这通常意味着那条连接半死不活，gen 兜底会接住它。
    QueueFull,
}

/// `conn_id → 该连接的出站队列`。
#[derive(Debug, Clone, Default)]
pub struct OutboundTable(Arc<RwLock<HashMap<String, mpsc::Sender<Outbound>>>>);

impl OutboundTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条新连接，返回它对应的接收端（交给 relay 去消费）。
    pub fn register(&self, conn_id: &str) -> mpsc::Receiver<Outbound> {
        let (tx, rx) = mpsc::channel(CONTROL_QUEUE);
        self.0.write().insert(conn_id.to_string(), tx);
        rx
    }

    /// 向一条连接推一帧控制帧。
    ///
    /// 返回是否真的投递了。两个假结果含义不同，调用方要能区分：
    /// - `false`：表里没有这条连接（它已经断开并被摘掉了）；
    /// - `Err`：**在表里，但队列满了**——连接还在，却迟迟不消费控制帧。
    ///   这属于异常，必须留痕而不是静默丢弃（「帧没发出去」正是排查
    ///   互抢环的唯一线索）。
    pub fn send(&self, conn_id: &str, frame: Outbound) -> Result<bool, PushError> {
        let tx = match self.0.read().get(conn_id).cloned() {
            Some(tx) => tx,
            None => return Ok(false),
        };
        // `try_send` 而不是 `send`：这里**不能** await。
        // 顶替发生在握手路径上，await 一个满队列会把整条握手堵死；
        // 而队列满本身就说明对端半死不活，等下去没有意义。
        tx.try_send(frame).map_err(|_| PushError::QueueFull)?;
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

    /// 登记后能收到推过来的帧。
    #[tokio::test]
    async fn a_registered_connection_receives_the_frame() {
        let t = OutboundTable::new();
        let mut rx = t.register("c1");
        assert!(t.send("c1", frame()).unwrap());
        let got = rx.recv().await.expect("must arrive");
        assert_eq!(got.msg_type, types::DISCONNECT);
    }

    /// 没登记过的连接：返回 false（不是错误——它已经走了）。
    #[test]
    fn an_unknown_connection_is_a_no_op() {
        let t = OutboundTable::new();
        assert!(!t.send("nope", frame()).unwrap());
    }

    /// 摘掉之后再推：同样返回 false。
    #[test]
    fn a_removed_connection_is_a_no_op() {
        let t = OutboundTable::new();
        let _rx = t.register("c1");
        t.remove("c1");
        assert!(!t.send("c1", frame()).unwrap());
    }

    /// 一条连接把队列塞满，**不能连累别的连接**（§6.2「每通道独立信用」
    /// 在控制帧上的对应面）。按 `conn_id` 分桶就是为了这条：一台设备半死不活
    /// 不该让别的设备收不到「你被顶替了」。
    #[tokio::test]
    async fn one_stalled_connection_does_not_block_the_others() {
        let t = OutboundTable::new();
        let _stalled = t.register("c_stuck"); // 收端从不消费
        let mut healthy = t.register("c_ok");
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
        let _rx = t.register("c1"); // 收端不消费
        for _ in 0..CONTROL_QUEUE {
            assert!(t.send("c1", frame()).unwrap());
        }
        assert_eq!(
            t.send("c1", frame()),
            Err(PushError::QueueFull),
            "有界队列必须在满时报错，而不是静默扩张"
        );
    }
}
