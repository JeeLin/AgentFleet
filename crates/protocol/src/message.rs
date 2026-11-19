//! 消息类型与载荷。
//!
//! 必需集 / 可选集的划分见 `protocol.md` §2：
//! - **必需集**（`hello/challenge/auth/auth_ok/auth_fail/heartbeat/err`）缺失或未知 →
//!   握手 10s 超时关闭并 `err{handshake_timeout}`；节点侧收到未知必需消息断开重连。
//! - **可选集**（`open_channel/channel_*/disconnect`）必须忽略；
//!   通道请求 `channel_ready` 超时 5s 显式回 `channel_error{reason:"unsupported"}`。
//!
//! **一次性命令不走独立消息类型**：exec 的结果随 `channel_data` 帧回来
//! （`stdout_b64` / `stderr_b64` / `timed_out`），协议层不为它定义类型。

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
    pub const ERR: &str = "err";
    pub const TASK_DISPATCH: &str = "task_dispatch";
    pub const TASK_DISPATCH_ACK: &str = "task_dispatch_ack";

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
///
/// **这是 WS 帧里的 reason 域**：取值是给**设备**读的短句（带空格、可展示）。
/// 别与 `hub_server::err::*` 混用——那是**浏览器 / 设备 HTTP 的 `code` 域**，
/// 取值是 `snake_case` 机器码。两域各有一个 `HUB_NOT_INITIALIZED`，
/// **值不同**（`"hub not initialized"` vs `"hub_not_initialized"`）：
/// 跨域复用即协议不匹配（WS 帧回一个 HTTP code，设备端匹配不上；
/// 反之页面按 code 分支会落进默认臂）。按名字全局搜索会同时命中两个，别照抄。
pub mod reason {
    /// Hub 未初始化：`secret` 表无 `hub.password`（§3.1）。
    /// WS 帧的 `auth_fail.reason`；对应 HTTP 侧是
    /// `hub_server::err::HUB_NOT_INITIALIZED`（`"hub_not_initialized"`）。
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

/// 消息类帧载荷的兜底形状（序列化失败时用）。
///
/// 与 [`crate::channel::fallback`] 同规则：兜底形状由协议层给，调用点不手写。
/// 手写的那份会漏字段——`ws.rs` 曾写 `json!({"code": code})` 当 `err` 的兜底，
/// 漏了必填的 `message`，对端解不开，比丢一帧更糟。
pub mod fallback {
    use super::{DisconnectReason, ErrorCode};

    /// `disconnect` 的兜底：`reason` 必填，`gen` 可选。
    pub fn disconnect(reason: DisconnectReason, gen: Option<u64>) -> serde_json::Value {
        serde_json::json!({ "reason": reason, "gen": gen })
    }

    /// `err` 的兜底：`code` 与 `message` 都必填（§4）。
    pub fn err(code: ErrorCode, message: &str) -> serde_json::Value {
        serde_json::json!({ "code": code, "message": message })
    }
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

/// Hub → 设备：去跑这个任务（v0.8.2，`scheduling.md` §5）。
///
/// **这不是权限批准。** `prompt` 只描述「要做的工作」，不构成用户对该动作的
/// 授权——设备侧执行时 agent 的权限确认仍只在**设备本机**进行，Hub 永不下发
/// 「已批准」（`PRODUCT.md` §5.4 的三条通路表：安全许可只在本机，业务决策才进
/// Hub）。把 Hub 变成远程控制面 = 拿到 Hub 就能对设备说「用户已批准执行这条
/// 命令」，而 `AGENTS.md` 约束 1（Hub 不需要任何到设备的入站访问）正是为了堵这
/// 个面。故本结构体**刻意不含** approval / permission 类字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDispatch {
    pub execution_id: String,
    pub task_id: String,
    pub project_id: String,
    /// 设备侧必须校验 `HEAD == base_commit`（`task-model.md` §7.3）。
    pub base_commit: String,
    /// 工作树绝对路径（Hub 在 `binding.fixed_dir` 下选定）。
    pub worktree_path: String,
    /// 任务分支名。
    pub branch: String,
    pub agent_id: String,
    /// 交给 agent 的任务描述。**不是权限批准**，见类型注释。
    pub prompt: String,
}

/// 设备 → Hub：派发的阶段进展与终态（v0.8.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDispatchAck {
    pub execution_id: String,
    pub stage: DispatchStage,
    #[serde(default)]
    pub reason: Option<String>,
    /// 证据（退出码 / stderr 摘要等）。失败时**必须**有（`task-model.md` §3：
    /// 没有原因串的失败等于没发生）。
    #[serde(default)]
    pub evidence: Option<String>,
    /// 失败形态，仅 `Stage::Failed` 时有值，取值见
    /// [`crate::message::FAILURE_KINDS`]。
    #[serde(default)]
    pub failure_kind: Option<String>,
}

/// 派发阶段。**闭集**——设备报一个 Hub 不认识的阶段不能被静默丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStage {
    /// 收到并认领。
    Accepted,
    /// 设备本地 `HEAD != base_commit`，**不硬跑**。
    BaselineMismatch,
    /// 开始执行。
    Running,
    Succeeded,
    Failed,
}

impl DispatchStage {
    /// 线上字符串。与 `serde` 的 `snake_case` 同一口径，单独列出来是给
    /// 断言用的——测试里写死字符串，不靠 `serde_json::to_value` 自比（那是自证）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::BaselineMismatch => "baseline_mismatch",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    /// 全部变体。新增变体时本数组与上面的 `match` 同时改，否则
    /// `every_dispatch_stage_round_trips` 会红。
    pub const ALL: &'static [Self] = &[
        Self::Accepted,
        Self::BaselineMismatch,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
    ];
}

/// 失败形态的线上字符串（`task-model.md` §3 的八种）。
///
/// 协议层只用字符串、不依赖 `hub-storage`——设备侧不该为了报一个失败形态
/// 而编译整个存储层。
pub const FAILURE_KINDS: &[&str] = &[
    "baseline_mismatch",
    "env_broken",
    "disk_exhausted",
    "agent_error",
    "task_failed",
    "interrupted",
    "rejected",
    "superseded",
];

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
    Err,
    /// Hub → 设备：去跑一个任务（v0.8.2）。
    ///
    /// **属可选集**：老 node 不认识它时走 [`MessageType::unknown_action`] 的
    /// `Ignore`，不断连——这是约束 14 的要求，新增可选类型不得踢掉旧设备。
    TaskDispatch,
    /// 设备 → Hub：进度与终态（v0.8.2）。
    TaskDispatchAck,
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
            ERR => Self::Err,
            TASK_DISPATCH => Self::TaskDispatch,
            TASK_DISPATCH_ACK => Self::TaskDispatchAck,
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

    /// 判定口径：**只有必需集才断开**，其余一律忽略（约束 14 前向兼容）。
    ///
    /// 三处断言合成一条：必需集逐个列全（漏一个 = 那个类型被静默忽略，
    /// 握手会卡到超时），可选集逐个列全（漏一个 = 那个类型被当成非法帧
    /// 而断连），两个集合之外的新类型名一律忽略。
    #[test]
    fn only_the_required_set_disconnects() {
        for &t in types::REQUIRED {
            assert_eq!(
                MessageType::unknown_action(t),
                UnknownAction::Disconnect,
                "{t} is required"
            );
        }
        for t in [
            types::HUB_HELLO,
            types::HEARTBEAT_ACK,
            types::DISCONNECT,
            types::OPEN_CHANNEL,
            types::CHANNEL_READY,
            types::CHANNEL_ERROR,
            types::CHANNEL_DATA,
            types::CHANNEL_CREDIT,
            types::CLOSE_CHANNEL,
        ] {
            assert_eq!(
                MessageType::unknown_action(t),
                UnknownAction::Ignore,
                "{t} must stay ignorable"
            );
        }
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

    // ---- v0.8.2 task_dispatch ----

    /// 两个新常量都走 `FromStr` 往返，且**都不在必需集里**——老 node 收到它们
    /// 必须走 `Ignore` 而不是断连（约束 14）。
    #[test]
    fn task_dispatch_types_round_trip_and_are_optional() {
        assert_eq!(
            types::TASK_DISPATCH.parse::<MessageType>().unwrap(),
            MessageType::TaskDispatch
        );
        assert_eq!(
            types::TASK_DISPATCH_ACK.parse::<MessageType>().unwrap(),
            MessageType::TaskDispatchAck
        );
        for t in [types::TASK_DISPATCH, types::TASK_DISPATCH_ACK] {
            assert!(
                !types::is_required(t),
                "{t} 是可选集：老 node 不认识时不得断连"
            );
            assert_eq!(
                MessageType::unknown_action(t),
                UnknownAction::Ignore,
                "{t} 未知时的处置必须是忽略"
            );
        }
    }

    /// `TaskDispatch` 的载荷字段名**写死在断言里**（不靠 `to_value` 自比——那是自证）。
    /// 同时断言**没有任何 approval / permission 类字段**：这不是权限批准。
    #[test]
    fn task_dispatch_payload_shape_is_exact_and_carries_no_approval() {
        let d = TaskDispatch {
            execution_id: "e1".into(),
            task_id: "t1".into(),
            project_id: "p1".into(),
            base_commit: "abc".into(),
            worktree_path: "/w/t1".into(),
            branch: "agentfleet/t1".into(),
            agent_id: "a1".into(),
            prompt: "do the thing".into(),
        };
        let v = serde_json::to_value(&d).unwrap();
        let obj = v.as_object().expect("must be an object");

        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want = vec![
            "agent_id",
            "base_commit",
            "branch",
            "execution_id",
            "project_id",
            "prompt",
            "task_id",
            "worktree_path",
        ];
        want.sort_unstable();
        assert_eq!(got, want, "字段集合变了就必须显式确认不是加错了东西");

        // 跨信任边界：**没有任何字段表示「已获批准」**。
        for k in obj.keys() {
            let k = k.to_ascii_lowercase();
            assert!(
                !k.contains("approv")
                    && !k.contains("permission")
                    && !k.contains("pre_approved")
                    && !k.contains("authoriz"),
                "TaskDispatch 不得携带授权类字段，出现了 {k}"
            );
        }

        // 逐字段反序列化相等（不靠整体 `==`，那样字段顺序漂移也能过）。
        let back: TaskDispatch = serde_json::from_value(v).unwrap();
        assert_eq!(back.execution_id, "e1");
        assert_eq!(back.task_id, "t1");
        assert_eq!(back.project_id, "p1");
        assert_eq!(back.base_commit, "abc");
        assert_eq!(back.worktree_path, "/w/t1");
        assert_eq!(back.branch, "agentfleet/t1");
        assert_eq!(back.agent_id, "a1");
        assert_eq!(back.prompt, "do the thing");
    }

    /// `DispatchStage` **每个变体**都有往返断言——防将来加变体忘了映射。
    ///
    /// `ALL` 与 `as_str` 的 `match` 必须同时改，两处不一致时本用例会红。
    #[test]
    fn every_dispatch_stage_round_trips() {
        // 先钉住变体总数：加变体时这里与 `ALL` 一起红，逼人回来补断言。
        assert_eq!(DispatchStage::ALL.len(), 5);

        let mut seen = Vec::new();
        for st in DispatchStage::ALL {
            let lit = st.as_str();
            assert!(!seen.contains(&lit), "as_str 出现重复字面量 {lit}");
            seen.push(lit);

            // serde 口径与 as_str 口径必须一致
            let json = serde_json::to_value(st).unwrap();
            assert_eq!(
                json,
                serde_json::Value::String(lit.to_string()),
                "{st:?} 的 serde 与 as_str 不一致"
            );
            // 往返
            let back: DispatchStage = serde_json::from_value(json).unwrap();
            assert_eq!(back, *st);

            // ack 里也能往返（stage 是 ack 的字段，不是孤立的枚举）
            let ack = TaskDispatchAck {
                execution_id: "e".into(),
                stage: *st,
                reason: None,
                evidence: None,
                failure_kind: None,
            };
            let v = serde_json::to_value(&ack).unwrap();
            assert_eq!(v["stage"], serde_json::Value::String(lit.to_string()));
            let ack_back: TaskDispatchAck = serde_json::from_value(v).unwrap();
            assert_eq!(ack_back.stage, *st);
        }
        assert_eq!(seen.len(), DispatchStage::ALL.len());
    }

    /// 失败形态的线上字符串就是 `task-model.md` §3 的八种，一个不多一个不少。
    #[test]
    fn failure_kinds_are_the_eight_specs() {
        assert_eq!(FAILURE_KINDS.len(), 8);
        let mut sorted = FAILURE_KINDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 8, "FAILURE_KINDS 有重复项");

        for k in [
            "baseline_mismatch",
            "env_broken",
            "disk_exhausted",
            "agent_error",
            "task_failed",
            "interrupted",
            "rejected",
            "superseded",
        ] {
            assert!(FAILURE_KINDS.contains(&k), "缺 {k}");
        }
    }

    /// 缺可选字段的 ack 也要能解出来（老设备 / 进度帧只带 stage）。
    #[test]
    fn ack_tolerates_missing_optional_fields() {
        let a: TaskDispatchAck =
            serde_json::from_str(r#"{"execution_id":"e","stage":"running"}"#).unwrap();
        assert_eq!(a.stage, DispatchStage::Running);
        assert!(a.reason.is_none());
        assert!(a.evidence.is_none());
        assert!(a.failure_kind.is_none());
    }
}
