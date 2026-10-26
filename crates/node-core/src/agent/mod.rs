//! agent 运行时：探测、缓存与（后续子任务的）ACP 客户端。
//!
//! 本模块**不碰 connector 注册表**：约束 3 要求 connector 只面向页面用户，
//! 不向 agent 暴露。ACP 客户端是另一条通路，不是第四个 connector。
//!
//! # 关于「安装」：本模块不做，也不打算做
//!
//! 原先这里有一个 `install` 子模块（下载 tarball → 校验 → 解包 → 装完复查），
//! 现已整体移除。agent 由用户自己在设备上装（`npm i -g`、下二进制、系统包管理器），
//! 设备只负责**扫描**出装了什么并上报；Hub 侧管的是这些 agent 的**配置**。
//!
//! 这不是省事：让 Hub 往设备上写可执行文件，等于把「一处配置错、全体设备
//! 执行未核对代码」的风险引进来，而它换来的只是省掉用户一条命令。

pub mod acp;
pub mod cache;
pub mod detect;

pub use acp::{AcpClient, AcpError, Event as AcpEvent, ACP_PROTOCOL_VERSION};
pub use cache::{AgentReport, Detected, Snapshot};
pub use detect::{detect_all, detect_one, AgentSpec, Probe, DEFAULT_PROBE_TIMEOUT};
