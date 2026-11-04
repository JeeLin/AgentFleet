//! node-core：设备侧（node）核心。
//!
//! 约束 2：这里的进程叫 **node**（设备进程），不是 agent。
//! 对外命令名 `afcli`（crate 名 `node-*`）。

pub mod agent;
pub mod channel;
pub mod conn;
pub mod entry;
pub mod heartbeat;
pub mod identity;
pub mod log;
pub mod reconnect;
pub mod supervisor;
pub mod update;
pub mod worker;

/// connector 与 PTY 在独立 crate `node-connector` 里（v0.12.0 按需加载
/// 必须靠 crate 边界）。这里 re-export 是为了让既有调用点写
/// `crate::connector::...` 不必全改成 `node_connector::...`；
/// **方向是单向的**：`node-core` → `node-connector`，不反向。
pub use node_connector::{connector, pty};

pub use identity::{data_dir, now_secs, Identity, IdentityError, IdentityFile};
