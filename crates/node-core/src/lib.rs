//! node-core：设备侧（node）核心。
//!
//! 约束 2：这里的进程叫 **node**（设备进程），不是 agent。
//! 对外命令名 `afcli`（crate 名 `node-*`）。

pub mod conn;
pub mod identity;
pub mod log;
pub mod reconnect;
pub mod supervisor;
pub mod update;

pub use identity::{data_dir, Identity, IdentityError, IdentityFile};
