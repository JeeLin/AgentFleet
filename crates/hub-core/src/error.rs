use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("storage: {0}")]
    Storage(#[from] hub_storage::StorageError),

    #[error("identity: {0}")]
    Identity(#[from] identity::error::IdentityError),

    #[error("auth: {0}")]
    Auth(#[from] crate::auth::error::AuthError),

    #[error("session: {0}")]
    Session(#[from] crate::session::error::SessionError),

    #[error("limits: {0}")]
    Limits(#[from] crate::limits::error::LimitError),

    #[error("registry: {0}")]
    Registry(#[from] crate::registry::error::RegistryError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] r2d2_sqlite::rusqlite::Error),

    #[error("pool: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("connection {0} not found")]
    NoSuchConnection(String),

    #[error("node {0} not found")]
    NoSuchNode(String),

    /// 清理预演要写入轨迹的那条 `seq` 已经存在。
    ///
    /// 只在 Hub 自己那个轨迹分区里发生（`cleanup::HUB_NODE_ID`），而那个分区
    /// 只有 Hub 一个写者——出现即说明编号逻辑错了。**不静默成功**：调用方以为
    /// 留痕了，而轨迹里其实只有别人的那一份。
    #[error("cleanup preview trajectory seq {seq} is already taken")]
    CleanupPreviewSeqTaken { seq: i64 },

    /// 清理预演写完之后回读不到自己刚写的那一条。
    ///
    /// 与 [`Self::CleanupPreviewSeqTaken`] 同一条立场：留痕没落地就不能报成功。
    #[error("cleanup preview was written but could not be read back from the trajectory")]
    CleanupPreviewNotRecorded,
}

pub type Result<T> = std::result::Result<T, CoreError>;
