//! hub-storage：Hub 的 SQLite 存储层。
//!
//! 本 crate 是**唯一**直接摸 SQLite 的地方（`hub-core` / `hub-server` 不写裸 SQL）。
//! 建库时强制 `journal_mode=WAL` + `busy_timeout`（AGENTS.md「SQLite 铁律」：
//! 多设备高频心跳/状态写入下，零 PRAGMA 必现 `SQLITE_BUSY`）。

pub mod db;
pub mod error;
pub mod migrate;

pub use db::{open, Db, Pool};
pub use error::StorageError;
