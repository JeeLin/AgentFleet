//! 消息类型与载荷。
//!
//! 必需集 / 可选集的划分见 `protocol.md` §2：
//! - **必需集**（`hello/challenge/auth/auth_ok/auth_fail/heartbeat/err`）缺失或未知 →
//!   握手 10s 超时关闭并 `err{handshake_timeout}`；节点侧收到未知必需消息断开重连。
//! - **可选集**（`open_channel/channel_*/exec_*/disconnect`）必须忽略；
//!   通道请求 `channel_ready` 超时 5s 显式回 `channel_error{reason:"unsupported"}`。

use serde::{Deserialize, Serialize};

/// 消息类型常量。
pub mod types {
    pub const HELLO: &str = "hello";
    pub const CHALLENGE: &str = "challenge";
    pub const AUTH: &str = "auth";
    pub const AUTH_OK: &str = "auth_ok";
    pub const AUTH_FAIL: &str = "auth_fail";
    pub const HUB_HELLO: &str = "hub_hello";
    pub const HEARTBEAT: &str = "heartbeat";
    pub const HEARTBEAT_ACK: &str = "heartbeat_ack";
    pub const DISCONNECT: &str = "disconnect";
    pub const OPEN_CHANNEL: &str = "open_channel";
    pub const CHANNEL_READY: &str = "channel_ready";
    pub const CHANNEL_ERROR: &str = "channel_error";
    pub const CHANNEL_DATA: &str = "channel_data";
    pub const CHANNEL_CREDIT: &str = "channel_credit";
    pub const CLOSE_CHANNEL: &str = "close_channel";
    pub const EXEC_REQUEST: &str = "exec_request";
    pub const EXEC_RESULT: &str = "exec_result";
    pub const ERR: &str = "err";

    /// 必需集：缺失或未知即握手失败。
    pub const REQUIRED: &[&str] = &[HELLO, CHALLENGE, AUTH, AUTH_OK, AUTH_FAIL, HEARTBEAT, ERR];

    /// 该类型属于必需集吗。
    ///
    /// **可选集没有对应的清单**：处置只按「在不在 `REQUIRED` 里」分档
    /// （见 [`crate::message::MessageType::unknown_action`]），维护一份
    /// `OPTIONAL` 只会诱使人改错那一份。
    pub fn is_required(t: &str) -> bool {
        REQUIRED.contains(&t)
    }
}

/// 节点 `hello` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub client_version: String,
    pub arch: String,
    pub os: String,
    /// 节点支持的 feature 列表（约束 14：协商靠能力，不靠版本号）。
    #[serde(default)]
    pub features: Vec<String>,
}

/// Hub `challenge` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    /// 32 字节 CSPRNG，base64url。每连接一次，连接关闭即失效。
    pub nonce: String,
}

/// `auth_ok` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthOk {
    pub node_id: String,
    pub protocol_version: u32,
    /// 本进程上的连接代次。单调递增；`last_seen_gen < gen` 即已被顶替（§7.1）。
    pub gen: u64,
    /// 本连接的心跳周期（毫秒）。**节点不得硬编码**。
    pub heartbeat_interval_ms: u64,
}

/// `auth_fail` 载荷：`{ reason, gen? }`（`protocol.md` §4）。
///
/// **`reason` 是人类可读短语，不是机器 enum**——文档 §3/§4 逐字定义了五个值。
/// 用自由字符串的理由：这些值会直接展示给用户在 Web 上看到的错误提示，
/// 且新增原因不必改协议版本（约束 14：协商靠能力，不靠版本号）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthFail {
    /// 取值见 [`reason`] 模块常量。
    pub reason: String,
    /// 被顶替时带上当前 `gen`，供节点对比自己的 `last_seen_gen`（§7.1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen: Option<u64>,
}

/// `auth_fail.reason` 的合法取值（`protocol.md` §4 表格逐字一致）。
pub mod reason {
    /// Hub 未初始化：`secret` 表无 `hub.password`（§3.1）。
    pub const HUB_NOT_INITIALIZED: &str = "hub not initialized";
    /// 指纹已被另一 node_id 注册——这才是「另一台机器」。
    pub const FINGERPRINT_ALREADY_REGISTERED: &str = "fingerprint already registered";
    /// 并发 enroll 撞了唯一约束；重试即可。
    pub const FINGERPRINT_RACE_RETRY: &str = "fingerprint race, retry";
    /// node_id 与库内指纹不匹配；**绝不更新库内指纹**。
    pub const IDENTITY_MISMATCH: &str = "identity mismatch";
    /// 本进程上已有更新的连接（被顶替）。
    pub const SUPERSEDED: &str = "superseded";
}

/// `disconnect` 载荷：`{ reason, gen? }`（§4、§7.1）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Disconnect {
    /// 取值见 [`DisconnectReason`]。
    pub reason: DisconnectReason,
    /// 当前连接代次，供节点记录 `last_seen_gen`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen: Option<u64>,
}

/// `disconnect.reason` 取值（§4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    /// 被同 node_id 的新连接顶替（§7.1）。
    #[default]
    Superseded,
    /// 用户在 Hub 手动下线；节点仍按退避重连。
    Admin,
}

/// `err` 载荷：`{ code, message }`（§4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrPayload {
    /// 取值见 [`ErrorCode`]。
    pub code: ErrorCode,
    /// 面向人的说明，可直接展示。
    pub message: String,
}

/// 通用错误码（`err.code`，§4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 握手 10s 超时。
    HandshakeTimeout,
    /// 帧超 1 MiB。
    FrameTooLarge,
    /// 读侧超限（Hub 硬上限，§6.1）。
    Flood,
    /// 通道不被支持（`channel_ready` 5s 超时，§5）。
    Unsupported,
    /// Hub 无该 arch 的二进制（§7.1.4）。
    ArchNotSupported,
    /// 内部错误。
    Internal,
}

/// 心跳载荷（字段见 `scheduling.md` §2）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node_version: String,
    #[serde(default)]
    pub capabilities: serde_json::Value,
    #[serde(default)]
    pub resources: serde_json::Value,
    /// 工作区根目录。**只报根，绝不递归**（§2 路径排除规则）。
    #[serde(default)]
    pub workspace_roots: Vec<String>,
}

/// `device_info`：enroll 时上传的设备描述。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub node_version: String,
}

/// 消息类型枚举（用于 match 分发；未知类型走 `Unknown`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Hello,
    Challenge,
    Auth,
    AuthOk,
    AuthFail,
    HubHello,
    Heartbeat,
    HeartbeatAck,
    Disconnect,
    OpenChannel,
    ChannelReady,
    ChannelError,
    ChannelData,
    ChannelCredit,
    CloseChannel,
    ExecRequest,
    ExecResult,
    Err,
    Unknown,
}

impl std::str::FromStr for MessageType {
    type Err = std::convert::Infallible;

    /// 未知类型一律映射为 [`MessageType::Unknown`]——由调用方按
    /// [`MessageType::unknown_action`] 决定忽略还是断连。
    fn from_str(s: &str) -> std::result::Result<Self, <Self as std::str::FromStr>::Err> {
        use types::*;
        Ok(match s {
            HELLO => Self::Hello,
            CHALLENGE => Self::Challenge,
            AUTH => Self::Auth,
            AUTH_OK => Self::AuthOk,
            AUTH_FAIL => Self::AuthFail,
            HUB_HELLO => Self::HubHello,
            HEARTBEAT => Self::Heartbeat,
            HEARTBEAT_ACK => Self::HeartbeatAck,
            DISCONNECT => Self::Disconnect,
            OPEN_CHANNEL => Self::OpenChannel,
            CHANNEL_READY => Self::ChannelReady,
            CHANNEL_ERROR => Self::ChannelError,
            CHANNEL_DATA => Self::ChannelData,
            CHANNEL_CREDIT => Self::ChannelCredit,
            CLOSE_CHANNEL => Self::CloseChannel,
            EXEC_REQUEST => Self::ExecRequest,
            EXEC_RESULT => Self::ExecResult,
            ERR => Self::Err,
            _ => Self::Unknown,
        })
    }
}

impl MessageType {
    /// 未知类型的处置：**只有必需集才断开**。
    ///
    /// 口径是「在不在 `REQUIRED` 里」，**不是**「在不在 `OPTIONAL` 里」——
    /// 一个「两套集合都不在」的新类型名必须算可忽略，否则 Hub 每新增一个可选
    /// 消息类型，老 node 就会因遇到不认识的名字而断连（约束 14）。
    ///
    /// 两侧的握手/业务帧循环都读这一处（`hub-server` 的 `routes::ws`、
    /// `node-core` 的 `conn`），各写一份判定就是两份会漂移的真相源。
    pub fn unknown_action(s: &str) -> UnknownAction {
        if types::is_required(s) {
            UnknownAction::Disconnect
        } else {
            UnknownAction::Ignore
        }
    }
}

/// 未知消息类型的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownAction {
    /// 忽略（可选集，以及两套集合之外的新类型名）。
    Ignore,
    /// 断开并退避重连（必需集）。
    Disconnect,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_set_contains_handshake_and_heartbeat() {
        for t in [
            "hello",
            "challenge",
            "auth",
            "auth_ok",
            "auth_fail",
            "heartbeat",
            "err",
        ] {
            assert!(types::is_required(t), "{t} must be required");
        }
    }

    #[test]
    fn unknown_optional_is_ignored() {
        assert_eq!(
            MessageType::unknown_action("channel_quantum_teleport"),
            UnknownAction::Ignore
        );
        assert_eq!(
            MessageType::unknown_action("open_channel"),
            UnknownAction::Ignore
        );
    }

    #[test]
    fn unknown_required_disconnects() {
        // 缺 auth 与缺 heartbeat 是两种最贵的失败，必须断而不是静默
        assert_eq!(
            MessageType::unknown_action("auth"),
            UnknownAction::Disconnect
        );
        assert_eq!(
            MessageType::unknown_action("heartbeat"),
            UnknownAction::Disconnect
        );
    }

    #[test]
    fn auth_ok_carries_gen_and_interval() {
        let f = crate::frame::Frame::push(
            types::AUTH_OK,
            0,
            serde_json::json!({"node_id":"n_x","protocol_version":1,"gen":7,"heartbeat_interval_ms":15000}),
        );
        let ok: AuthOk = f.payload().unwrap();
        assert_eq!(ok.gen, 7);
        assert_eq!(ok.heartbeat_interval_ms, 15000);
    }

    #[test]
    fn auth_fail_carries_reason_and_optional_gen() {
        use reason::*;
        // §4 表格逐字：reason 是人类可读短语，含空格和逗号
        let f = crate::frame::Frame::push(
            types::AUTH_FAIL,
            0,
            serde_json::json!({"reason": FINGERPRINT_RACE_RETRY}),
        );
        let p: AuthFail = f.payload().unwrap();
        assert_eq!(p.reason, "fingerprint race, retry");
        assert_eq!(p.gen, None, "gen must be omitted unless superseded");

        // superseded 必须带 gen（§7.1 兜底）
        let f = crate::frame::Frame::push(
            types::AUTH_FAIL,
            0,
            serde_json::json!({"reason": SUPERSEDED, "gen": 9}),
        );
        let p: AuthFail = f.payload().unwrap();
        assert_eq!(p.reason, "superseded");
        assert_eq!(p.gen, Some(9));
    }

    #[test]
    fn auth_fail_reasons_match_protocol_doc_verbatim() {
        // §4 表格列出的五个值；改文档时必须同步改这里
        assert_eq!(reason::HUB_NOT_INITIALIZED, "hub not initialized");
        assert_eq!(
            reason::FINGERPRINT_ALREADY_REGISTERED,
            "fingerprint already registered"
        );
        assert_eq!(reason::FINGERPRINT_RACE_RETRY, "fingerprint race, retry");
        assert_eq!(reason::IDENTITY_MISMATCH, "identity mismatch");
        assert_eq!(reason::SUPERSEDED, "superseded");
    }

    #[test]
    fn disconnect_reasons_roundtrip() {
        for (s, want) in [
            ("superseded", DisconnectReason::Superseded),
            ("admin", DisconnectReason::Admin),
        ] {
            let f =
                crate::frame::Frame::push(types::DISCONNECT, 0, serde_json::json!({"reason": s}));
            let got: Disconnect = f.payload().unwrap();
            assert_eq!(got.reason, want);
        }
    }

    #[test]
    fn error_codes_roundtrip() {
        for (s, want) in [
            ("handshake_timeout", ErrorCode::HandshakeTimeout),
            ("frame_too_large", ErrorCode::FrameTooLarge),
            ("flood", ErrorCode::Flood),
            ("unsupported", ErrorCode::Unsupported),
            ("arch_not_supported", ErrorCode::ArchNotSupported),
            ("internal", ErrorCode::Internal),
        ] {
            let f = crate::frame::Frame::push(
                types::ERR,
                0,
                serde_json::json!({"code": s, "message": "m"}),
            );
            let got: ErrPayload = f.payload().unwrap();
            assert_eq!(got.code, want);
        }
    }

    #[test]
    fn err_payload_requires_message() {
        // §4：err 是 { code, message }，message 不可省（直接展示给用户）
        let f = crate::frame::Frame::push(types::ERR, 0, serde_json::json!({"code": "flood"}));
        assert!(
            f.payload::<ErrPayload>().is_err(),
            "message must be required"
        );
    }

    #[test]
    fn unknown_types_are_ignorable_unless_they_are_required() {
        // 判定口径：**只有必需集才断开**，其余一律忽略（约束 14 前向兼容）
        assert_eq!(
            MessageType::unknown_action(types::HEARTBEAT),
            UnknownAction::Disconnect,
            "heartbeat is required"
        );
        assert_eq!(
            MessageType::unknown_action(types::CHANNEL_DATA),
            UnknownAction::Ignore,
            "optional type must be ignorable"
        );
        // 不属于必需集的（含将来新增的类型名）→ 可忽略，这是约束 14 的
        // 前向兼容：老 node 遇到新 Hub 的新消息类型必须忽略而不是崩。
        // 早期实现写成 `OPTIONAL.contains(t)`，那会让一个新增的类型名算成
        // 「不可忽略」——本条锁住这个坑不再回来。
        assert_eq!(
            MessageType::unknown_action("teleport_channel"),
            UnknownAction::Ignore
        );
    }

    #[test]
    fn message_type_from_str_maps_every_known_type() {
        // 逐个验证分派表：漏一个分支 = 线上某个消息被误判成 Unknown
        use MessageType::*;
        for (s, want) in [
            ("hello", Hello),
            ("challenge", Challenge),
            ("auth", Auth),
            ("auth_ok", AuthOk),
            ("auth_fail", AuthFail),
            ("hub_hello", HubHello),
            ("heartbeat", Heartbeat),
            ("heartbeat_ack", HeartbeatAck),
            ("disconnect", Disconnect),
            ("open_channel", OpenChannel),
            ("channel_ready", ChannelReady),
            ("channel_error", ChannelError),
            ("channel_data", ChannelData),
            ("channel_credit", ChannelCredit),
            ("close_channel", CloseChannel),
            ("exec_request", ExecRequest),
            ("exec_result", ExecResult),
            ("err", Err),
        ] {
            assert_eq!(s.parse::<MessageType>().unwrap(), want, "type {s}");
        }
        // 未知 → Unknown（而不是报错：约束 14 前向兼容）
        assert_eq!(
            "teleport_channel".parse::<MessageType>().unwrap(),
            MessageType::Unknown
        );
    }

    #[test]
    fn heartbeat_workspace_roots_only() {
        // 只报根目录，不含子目录/文件名
        let h = Heartbeat {
            node_version: "0.1.0".into(),
            workspace_roots: vec!["/w".into()],
            ..Default::default()
        };
        let text = serde_json::to_string(&h).unwrap();
        assert!(text.contains("/w"));
        assert!(!text.contains(".git"));
        assert!(!text.contains(".env"));
    }
}
