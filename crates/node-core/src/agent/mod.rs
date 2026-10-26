//! agent 运行时：探测、缓存与（后续子任务的）ACP 客户端。
//!
//! 本模块**不碰 connector 注册表**：约束 3 要求 connector 只面向页面用户，
//! 不向 agent 暴露。ACP 客户端是另一条通路，不是第四个 connector。

pub mod cache;
pub mod detect;

pub use cache::{AgentReport, Detected, Snapshot};
pub use detect::{detect_all, detect_one, AgentSpec, Probe, DEFAULT_PROBE_TIMEOUT};
