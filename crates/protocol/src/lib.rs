//! protocol：node ↔ Hub 的 WS 协议定义。
//!
//! 只放**帧信封、消息类型、载荷结构、错误码**这些跨进程共享的定义。
//! 不含任何 IO 或业务逻辑——`hub-core` 与 `node-core` 各自实现收发。

pub mod channel;
pub mod frame;
pub mod handshake;
pub mod message;

pub use channel::{
    reason as channel_reason, Channel, ChannelAction, ChannelCredit, ChannelData, ChannelError,
    ChannelErrorCode, ChannelErrorPayload, ChannelReady, ChannelRegistry, ChannelState,
    CloseChannel, CreditStatus, OpenChannel, CREDIT_DELTA, CREDIT_START, READY_TIMEOUT,
};
pub use frame::{Frame, MAX_FRAME_BYTES, PROTOCOL_VERSION};
pub use handshake::{
    negotiate_version, should_close_for_timeout, AuthAttempt, AuthPayload, Authenticator,
    EnrollAccepted, EnrollAuth, Handshake, HandshakeError, HandshakeState, PublicKeySource,
    ReconnectAccepted, ReconnectAuth, HANDSHAKE_TIMEOUT, NONCE_BYTES, PUBLIC_KEY_BYTES,
    SIGNATURE_BYTES,
};
pub use message::{
    reason, AuthFail, AuthOk, Challenge, DeviceInfo, Disconnect, DisconnectReason, ErrPayload,
    ErrorCode, Hello, Message, MessageType,
};
