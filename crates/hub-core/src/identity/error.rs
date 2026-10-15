use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("malformed key")]
    MalformedKey,

    #[error("bad fingerprint")]
    BadFingerprint,

    #[error("bad base64url")]
    BadBase64,

    #[error("bad hex")]
    BadHex,
}
