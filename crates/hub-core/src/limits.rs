//! Hub 读侧硬上限与写队列。
//!
//! 设计要点（`protocol.md` §6.1）：
//! - Hub 是单进程服务多设备，**一条连接失控不能拖垮其他人**。
//! - 读侧限的是「单位时间读到的字节数」与「单帧大小」；超限不是断开重连
//!   （那会被恶意节点当放大器），而是 `err{code:"flood"}` 后**主动断开**。
//! - 写侧不丢消息：队列有界，满了就**反压**（停止读），不静默丢弃。

pub mod error;
pub use error::LimitError;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 单帧上限。与 `protocol::MAX_FRAME_BYTES` 一致（§2）。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 读侧令牌桶容量（字节）：允许的突发量。
pub const READ_BUCKET_CAPACITY: usize = 4 * 1024 * 1024;

/// 读侧每秒补充的令牌（字节）：稳态速率上限。
pub const READ_REFILL_PER_SEC: usize = 1024 * 1024;

/// 超过这个速率持续这么久就判定 flood（§6.1）。
pub const FLOOD_SUSTAINED: Duration = Duration::from_secs(30);

/// 写队列容量（帧数）。满即反压，不丢弃。
pub const WRITE_QUEUE_CAPACITY: usize = 1024;

/// 令牌桶：读侧速率限制。
#[derive(Debug)]
pub struct ReadLimiter {
    capacity: usize,
    refill_per_sec: usize,
    tokens: usize,
    last: Instant,
}

impl ReadLimiter {
    pub fn new() -> Self {
        Self::with_params(READ_BUCKET_CAPACITY, READ_REFILL_PER_SEC)
    }

    pub fn with_params(capacity: usize, refill_per_sec: usize) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    /// 尝试消耗 `n` 字节。
    ///
    /// 返回 `Ok(())` 表示放行；`Err` 表示已超限，调用方应回
    /// `err{code:"flood"}` 并断开。
    ///
    /// **单帧上限是固定的 `MAX_FRAME_BYTES`（1 MiB，§2），不是 `capacity`**：
    /// `capacity` 只是突发量（几秒的缓冲），把它当单帧上限会让突发量大的
    /// 部署意外接受超大帧。
    pub fn try_consume(&mut self, n: usize) -> Result<(), LimitError> {
        // 单帧超硬上限：无论桶里有多少令牌都不放行
        if n > MAX_FRAME_BYTES {
            return Err(LimitError::FrameTooLarge);
        }
        self.refill();
        if self.tokens < n {
            return Err(LimitError::Flood);
        }
        self.tokens -= n;
        Ok(())
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        let gained = (elapsed.as_secs_f64() * self.refill_per_sec as f64) as usize;
        if gained > 0 {
            self.tokens = (self.tokens + gained).min(self.capacity);
            self.last = now;
        }
    }
}

impl Default for ReadLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// 有界写队列：满了反压，**不丢帧**。
#[derive(Debug)]
pub struct WriteQueue {
    cap: usize,
    q: VecDeque<Vec<u8>>,
}

impl WriteQueue {
    pub fn new() -> Self {
        Self::with_capacity(WRITE_QUEUE_CAPACITY)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            q: VecDeque::with_capacity(cap.min(4096)),
        }
    }

    /// 入队。返回 `Err` 表示队列已满——调用方应停止读上游，而不是丢帧。
    pub fn push(&mut self, frame: Vec<u8>) -> Result<(), LimitError> {
        if self.q.len() >= self.cap {
            return Err(LimitError::WriteQueueFull);
        }
        self.q.push_back(frame);
        Ok(())
    }

    /// 出队一帧。
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.q.pop_front()
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.q.len() >= self.cap
    }
}

impl Default for WriteQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    // 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
    // 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
    // let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
    // 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::indexing_slicing
    )]
    use super::*;

    #[test]
    fn within_capacity_passes() {
        let mut r = ReadLimiter::new();
        for _ in 0..10 {
            r.try_consume(64 * 1024).unwrap();
        }
    }

    #[test]
    fn burst_beyond_capacity_is_flood() {
        let mut r = ReadLimiter::with_params(1024, 0);
        r.try_consume(1024).unwrap();
        assert!(matches!(r.try_consume(1), Err(LimitError::Flood)));
    }

    #[test]
    fn single_frame_above_hard_limit_rejected_even_with_tokens() {
        let mut r = ReadLimiter::with_params(8 * 1024 * 1024, 0);
        // 桶里明明有 8 MiB，但单帧 5 MiB 仍必须拒（§2）
        assert!(matches!(
            r.try_consume(5 * 1024 * 1024),
            Err(LimitError::FrameTooLarge)
        ));
    }

    #[test]
    fn tokens_refill_over_time() {
        let mut r = ReadLimiter::with_params(1000, 1000);
        r.try_consume(1000).unwrap();
        assert!(r.try_consume(1).is_err());
        std::thread::sleep(Duration::from_millis(20));
        r.try_consume(1).unwrap();
    }

    #[test]
    fn tokens_never_exceed_capacity() {
        let mut r = ReadLimiter::with_params(100, 10_000);
        std::thread::sleep(Duration::from_millis(30));
        r.refill();
        std::thread::sleep(Duration::from_millis(30));
        r.refill();
        // 最多 100
        r.try_consume(100).unwrap();
        assert!(r.try_consume(1).is_err());
    }

    #[test]
    fn write_queue_never_drops_when_full() {
        let mut q = WriteQueue::with_capacity(2);
        q.push(vec![1]).unwrap();
        q.push(vec![2]).unwrap();
        // 满：必须报错让调用方反压，而不是丢
        assert!(matches!(q.push(vec![3]), Err(LimitError::WriteQueueFull)));
        assert_eq!(q.len(), 2);
        // 取出一帧后又能进
        assert_eq!(q.pop(), Some(vec![1]));
        q.push(vec![3]).unwrap();
    }

    #[test]
    fn write_queue_is_fifo() {
        let mut q = WriteQueue::new();
        q.push(vec![1]).unwrap();
        q.push(vec![2]).unwrap();
        assert_eq!(q.pop(), Some(vec![1]));
        assert_eq!(q.pop(), Some(vec![2]));
        assert!(q.is_empty());
    }

    #[test]
    fn write_queue_reports_full() {
        let mut q = WriteQueue::with_capacity(1);
        q.push(vec![0]).unwrap();
        assert!(q.is_full());
        q.pop().unwrap();
        assert!(!q.is_full());
    }

    #[test]
    fn defaults_use_production_constants() {
        // Default 必须等价于 new()：配置漂移会让「默认路径」与
        // 「显式 new」走出两套不同的限额。
        let mut r = ReadLimiter::default();
        assert!(r.try_consume(MAX_FRAME_BYTES).is_ok());
        assert!(matches!(
            r.try_consume(MAX_FRAME_BYTES + 1),
            Err(LimitError::FrameTooLarge)
        ));

        let mut q = WriteQueue::default();
        assert!(q.is_empty());
        assert!(!q.is_full());
        q.push(vec![7]).unwrap();
        assert_eq!(q.pop(), Some(vec![7]));
    }
}
