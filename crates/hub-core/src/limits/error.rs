use thiserror::Error;

#[derive(Debug, Error)]
pub enum LimitError {
    /// 读侧超速。调用方回 `err{code:"flood"}` 后断开（§6.1）。
    #[error("read rate exceeded (flood)")]
    Flood,

    /// 单帧超过 1 MiB 硬上限（§2）。
    #[error("frame too large")]
    FrameTooLarge,

    /// 写队列满。调用方应反压，**不得丢帧**。
    #[error("write queue full, apply backpressure")]
    WriteQueueFull,
}
