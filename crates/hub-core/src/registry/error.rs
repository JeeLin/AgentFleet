use thiserror::Error;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("fingerprint already registered as node {0}")]
    AlreadyRegistered(String),

    #[error("identity mismatch")]
    IdentityMismatch,

    #[error("no stored public key for this device")]
    NoStoredPublicKey,

    #[error("bad public key")]
    BadPublicKey,
}
