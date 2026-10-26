//! hub-storage：Hub 的 SQLite 存储层。
//!
//! 本 crate 是**唯一**直接摸 SQLite 的地方（`hub-core` / `hub-server` 不写裸 SQL）。
//! 建库时强制 `journal_mode=WAL` + `busy_timeout`（AGENTS.md「SQLite 铁律」：
//! 多设备高频心跳/状态写入下，零 PRAGMA 必现 `SQLITE_BUSY`）。

pub mod agent;
pub mod db;
pub mod error;
pub mod migrate;
pub mod repo;
pub mod session;
pub mod trajectory;
pub mod workspace;

pub use db::{data_dir, open, open_in, set_umask_077, Db};
pub use agent::{AgentConfig, AgentConfigRepo, AgentRuntime, AgentRuntimeRepo};
pub use error::StorageError;
pub use repo::{
    base64url_decode, base64url_of, Device, DeviceRepo, DeviceStatus, NewDevice, SecretRepo,
};
pub use session::{Session, SessionDetail, SessionRepo, SessionState};
pub use trajectory::{
    Gap, NewEvent, TrajectoryEvent, TrajectoryRepo, TurnBounds, TURN_END_KIND,
};
pub use workspace::{
    list_sessions, recent_sessions, SessionListItem, Workspace, WorkspaceRepo, WorkspaceSession,
};
