//! protocol：node ↔ Hub 的 WS 协议定义。
//!
//! 只放**帧信封、消息类型、载荷结构、错误码**这些跨进程共享的定义。
//! 不含任何 IO 或业务逻辑——`hub-core` 与 `node-core` 各自实现收发。

pub mod frame;
pub mod message;

pub use frame::{Frame, MAX_FRAME_BYTES, PROTOCOL_VERSION};
pub use message::{
    reason, AuthFail, AuthOk, Challenge, DeviceInfo, Disconnect, DisconnectReason, ErrPayload,
    ErrorCode, Hello, Message, MessageType,
};
