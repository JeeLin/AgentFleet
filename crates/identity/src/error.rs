use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("malformed key")]
    MalformedKey,

    #[error("bad base64url")]
    BadBase64,
}
