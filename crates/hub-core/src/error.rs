use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("storage: {0}")]
    Storage(#[from] hub_storage::StorageError),

    #[error("identity: {0}")]
    Identity(#[from] crate::identity::error::IdentityError),

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
}

pub type Result<T> = std::result::Result<T, CoreError>;
