use thiserror::Error;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("fingerprint already registered as node {0}")]
    AlreadyRegistered(String),
}
