use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("malformed session cookie")]
    Malformed,

    #[error("bad signature")]
    BadSignature,

    #[error("session expired")]
    Expired,

    /// HMAC 实例化失败。32 字节密钥对 HMAC-SHA256 永远合法，
    /// 出现即说明 key 长度被外部改动——属于编程错误，不是用户输入问题。
    #[error("hmac init failed: {0}")]
    BadKey(String),
}
