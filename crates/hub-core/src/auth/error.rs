use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("hub already initialized")]
    AlreadyInitialized,

    #[error("hub not initialized: set a password first")]
    NotInitialized,

    #[error("wrong password")]
    WrongPassword,

    #[error("password must be at least 8 characters")]
    PasswordTooShort,

    #[error("password hashing failed")]
    HashFailed,

    #[error("stored hash is malformed")]
    MalformedHash,

    #[error("setup token mismatch")]
    SetupTokenMismatch,

    /// 失败次数已达上限；setup token 失效，需重启 Hub。
    #[error("setup token locked after too many failures")]
    SetupTokenLocked,

    #[error("session hmac key is malformed")]
    MalformedHmacKey,

    #[error("storage: {0}")]
    Storage(#[from] hub_storage::StorageError),
}
