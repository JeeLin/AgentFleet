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
}

pub type Result<T> = std::result::Result<T, StorageError>;
