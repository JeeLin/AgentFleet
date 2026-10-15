use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("r2d2 pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("migration {version} failed: {source}")]
    Migration {
        version: i64,
        #[source]
        source: rusqlite::Error,
    },

    #[error("schema version {found} is newer than supported {supported}: refusing to open an older binary against a newer database")]
    SchemaTooNew { found: i64, supported: i64 },

    #[error("no home or data directory could be resolved")]
    NoDataDir,

    /// 库里的 `device.status` 不是 'online'/'offline'。
    ///
    /// schema 上有 `CHECK` 约束，正常途径写不进来；出现即说明库被外部改动，
    /// 或迁移漏了约束。**不静默回落 offline**——那会掩盖数据损坏。
    #[error("invalid device.status in database: {0:?}")]
    InvalidDeviceStatus(String),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StorageError>;
