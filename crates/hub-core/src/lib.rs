//! hub-core：Hub 的业务核心。
//!
//! 持有注册表、连接注册表、认证与会话。**不直接写 SQL**——全部经
//! `hub-storage` 的 repo；**不做 HTTP**——路由装配在 `hub-server`。

pub mod auth;
pub mod channels;
pub mod connections;
pub mod error;
pub mod limits;
pub mod registry;
pub mod repo;
pub mod reports;
pub mod session;
pub mod time;

pub use auth::{HubAuth, SetupToken};
pub use channels::HubChannels;
pub use connections::{ConnHandle, ConnectionRegistry};
pub use error::CoreError;
pub use identity::{Fingerprint, KeyPair};
pub use limits::ReadLimiter;
pub use registry::Registry;
pub use repo::{BareRepo, RepoError};
pub use reports::{
    merge_into_stored, presence, report_from_heartbeat, Presence, Report, Reports, OFFLINE_AFTER,
};
pub use session::{Session, SessionStore};
