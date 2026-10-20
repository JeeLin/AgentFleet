//! 通道状态机与 credit 信用（`protocol.md` §5–§6）。
//!
//! 纯逻辑、**不含 IO**：状态转移与信用扣补都是同步函数，超时判定是
//! 「状态 + 已流逝时长」的纯判定（计时与断连由 `hub-core` / `node-core` 做），
//! 所以单元测试能不碰 socket 覆盖全部转移。
//!
//! 分层：本模块的 [`ChannelRegistry`] 只管**一台设备内**的
//! `channel_id → Channel`；`(node_id, channel_id)` 的 node_id 那层在
//! `hub-core::channels`。通道消息的载荷结构也放在这里——与状态机同文件，
//! 载荷改了状态机没改会在编译期就露出来。
//!
//! 信用语义（§6）：**发送方**每发一帧 `channel_data` 扣 1，归零即暂停；
//! 接收方补 `channel_credit` 且**只增不减**。两侧共用同一张账本，
//! 但入口按角色分开命名（[`ChannelRegistry::on_data`] 是发送方，
//! [`ChannelRegistry::on_data_received`] 是接收方的镜像），避免调用方
//! 在 Hub 与 node 两侧搞混「Suspend 到底该停读还是该补信用」。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::message::ErrorCode;

/// `channel_error.code`。**与 `err.code` 共用同一套取值**（§4 表格只有一份
/// code 清单），用别名而不是新 enum：第二个枚举只会多一份会漂移的真相源。
pub type ChannelErrorCode = ErrorCode;

/// `channel_error.reason` 的约定取值。
///
/// `reason` 是**面向人的短语**、不是机器 enum（同 `auth_fail.reason` 的取舍）：
/// 这些值会直接出现在 Web 上的错误提示里，新增原因不必改协议版本（约束 14）。
pub mod reason {
    /// §5：`channel_ready` 5s 超时，Hub 显式回 `channel_error{reason:"unsupported"}`。
    pub const UNSUPPORTED: &str = "unsupported";
}

/// `open_channel` 的默认信用额度（§6「如 32」）。
pub const CREDIT_START: u32 = 32;

/// Hub 每次补发的信用增量（§6「补 credit（如 +32）」）。
pub const CREDIT_DELTA: u32 = 32;

/// `channel_ready` 超时 5s（§5）。
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// `open_channel` 载荷（Hub→node，§4、§6）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenChannel {
    pub channel_id: String,
    /// connector 名（`terminal` / `files` / `git`；各自 payload schema 随 M1 定义）。
    pub connector: String,
    /// connector 私有参数。**M0 不解释它**，只做透传。
    #[serde(default)]
    pub params: serde_json::Value,
    /// 初始信用。**缺失按 [`CREDIT_START`]**：不填 `credit_start` 的老节点
    /// 不该被判非法（约束 14）。
    ///
    /// 不派生 `Default`：`Default` 会给出 0，而 serde 的缺省是
    /// [`CREDIT_START`]，两条默认值不一致是最难查的那类配置漂移。
    #[serde(default = "default_credit_start")]
    pub credit_start: u32,
}

fn default_credit_start() -> u32 {
    CREDIT_START
}

impl OpenChannel {
    /// 开一条新通道。初始信用取 [`CREDIT_START`]。
    ///
    /// 调用方不要自己写 `credit_start`：那样 [`Self::credit_start`] 的 serde
    /// 缺省与这里的显式值又会分成两份口径。
    pub fn new(channel_id: impl Into<String>, connector: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            connector: connector.into(),
            params: serde_json::Value::Null,
            credit_start: CREDIT_START,
        }
    }

    /// 带 connector 私有参数的 [`Self::new`]。
    pub fn with_params(
        channel_id: impl Into<String>,
        connector: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            params,
            ..Self::new(channel_id, connector)
        }
    }
}

/// `channel_ready` 载荷（node→Hub，§4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelReady {
    pub channel_id: String,
    /// connector 的能力声明（约束 14：协商靠能力，不靠版本号）。缺失按 null。
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// `channel_data` 载荷（双向，§4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelData {
    pub channel_id: String,
    /// 通道内数据（PTY 字节、文件读写结果等）。schema 随 connector 走，本层不解释。
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// `channel_credit` 载荷（Hub→node，§4、§6）。**独立于 `channel_data`**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCredit {
    pub channel_id: String,
    /// 补充量。
    pub delta: u32,
}

/// `channel_error` 载荷（双向，§4、§5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelErrorPayload {
    pub channel_id: String,
    pub code: ChannelErrorCode,
    /// 取值见 [`reason`]。
    pub reason: String,
}

/// `close_channel` 载荷（双向，§4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseChannel {
    pub channel_id: String,
    #[serde(default)]
    pub reason: String,
}

/// 通道状态（§5「开 / 用 / 关」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// 已 `open_channel`，等 `channel_ready`。超时 5s 即由 Hub 回
    /// `channel_error{reason:"unsupported"}`（§5）。
    Opening,
    /// 已 `channel_ready`，可以走数据面。
    Ready,
    /// 终态：`close_channel` 或被显式拒绝。**不可复活**——继续用只能重新
    /// `open_channel`（那会重置信用，§6）或换 `channel_id`。
    Closed,
}

/// 通道状态机的下一步动作。
///
/// 只描述**「调用方接下来该做什么」**，不含帧构造：载荷/序列号/连接都由
/// 调用方掌握。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelAction {
    /// 发一帧 `channel_data`。`suspend_after` 为真表示**发完这一帧必须挂起**
    /// ——信用刚好在本次归零（§6「credit 归零则暂停发」）。
    ///
    /// 载荷不在动作里：PTY 流是唯一的高频路径，registry 再 clone 一次字节
    /// 就多一份内存压力，而那正是 §6 要防的事。调用方手里已经有字节了。
    SendData {
        channel_id: String,
        suspend_after: bool,
    },
    /// 信用为 0：不得再发。写侧停下，等 `channel_credit`（§6）。
    Suspend { channel_id: String },
    /// 回 `channel_error{channel_id, code, reason}`（§4、§5）。
    ReplyError {
        channel_id: String,
        code: ChannelErrorCode,
        reason: String,
    },
    /// 回 `close_channel{channel_id, reason}`（§4）。
    ReplyClose { channel_id: String, reason: String },
}

/// 接收方扣信用之后的余额状态（Hub 侧处理 `channel_data` 时用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditStatus {
    /// 余额 > 0：等下一帧。
    Granted,
    /// 余额归零：该补 `channel_credit{delta}` 了（§6「信用只增不减」）。
    Exhausted,
}

/// 通道错误。**非法转移一律报错，不静默接受**——与握手状态机同口径。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("empty channel id")]
    EmptyChannelId,

    #[error("channel {0} is not open")]
    UnknownChannel(String),

    #[error("channel {0} is already closed")]
    AlreadyClosed(String),

    #[error("channel {0} is already ready")]
    AlreadyReady(String),

    #[error("channel {0} is not ready yet")]
    NotReady(String),
}

/// 一条通道。
#[derive(Debug, Clone)]
pub struct Channel {
    id: String,
    connector: String,
    state: ChannelState,
    /// 数据面信用余额（§6）：发送方每发一帧 −1，`channel_credit` 补回。
    credit: u32,
    opened_at: Instant,
}

impl Channel {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn connector(&self) -> &str {
        &self.connector
    }

    pub fn state(&self) -> ChannelState {
        self.state
    }

    /// 剩余信用。
    pub fn credit(&self) -> u32 {
        self.credit
    }

    /// 是否已 `channel_ready`。
    pub fn is_ready(&self) -> bool {
        self.state == ChannelState::Ready
    }

    /// `channel_ready` 是否已超时（§5）。闭区间用 `>=`：到点就该超时。
    fn ready_expired(&self, now: Instant) -> bool {
        self.state == ChannelState::Opening
            && now.saturating_duration_since(self.opened_at) >= READY_TIMEOUT
    }
}

/// 一台设备内的通道集合：`channel_id → Channel`。
#[derive(Debug, Default)]
pub struct ChannelRegistry {
    channels: HashMap<String, Channel>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一条新通道：`Opening` + `credit = credit_start`。
    ///
    /// **每次 `open_channel` 都重置状态与信用**（§6 末条），包括重开一个已
    /// 存在的 `channel_id`：否则重连后 Hub 按 `channel_id` 记账，「信用只增
    /// 不减」会让余额无限累积，而余额恒 > 0 等于没有流控。
    pub fn open(
        &mut self,
        channel_id: &str,
        connector: &str,
        credit_start: u32,
        now: Instant,
    ) -> Result<(), ChannelError> {
        if channel_id.is_empty() {
            return Err(ChannelError::EmptyChannelId);
        }
        let ch = Channel {
            id: channel_id.to_string(),
            connector: connector.to_string(),
            state: ChannelState::Opening,
            credit: credit_start,
            opened_at: now,
        };
        self.channels.insert(ch.id.clone(), ch);
        Ok(())
    }

    /// `channel_ready` 到达（或本端 connector 就绪）：`Opening → Ready`。
    ///
    /// 两侧共用同一个转移，**不回帧**——回 `channel_ready` 是 node 侧的事，
    /// 把它做成动作会让 Hub 收到 `channel_ready` 时也拿到一个「回
    /// `channel_ready`」的动作。`channel_ready` 帧由调用方用
    /// [`ChannelReady`] 自己构造。
    pub fn mark_ready(&mut self, channel_id: &str) -> Result<(), ChannelError> {
        let ch = self.live_mut(channel_id)?;
        if ch.is_ready() {
            return Err(ChannelError::AlreadyReady(channel_id.to_string()));
        }
        ch.state = ChannelState::Ready;
        Ok(())
    }

    /// **发送方**消费一格信用。
    ///
    /// §6：`credit 归零则暂停发`——最后一格仍然放行（本次是 `SendData`
    /// 且 `suspend_after` 为真），之后才是 `Suspend`。
    pub fn on_data(&mut self, channel_id: &str) -> Result<ChannelAction, ChannelError> {
        let ch = self.ready_mut(channel_id)?;
        match ch.credit {
            0 => Ok(ChannelAction::Suspend {
                channel_id: ch.id.clone(),
            }),
            1 => {
                ch.credit = 0;
                Ok(ChannelAction::SendData {
                    channel_id: ch.id.clone(),
                    suspend_after: true,
                })
            }
            n => {
                ch.credit = n - 1;
                Ok(ChannelAction::SendData {
                    channel_id: ch.id.clone(),
                    suspend_after: false,
                })
            }
        }
    }

    /// **接收方**（Hub）记账：对方消费掉了一格信用。
    ///
    /// 返回 [`CreditStatus::Exhausted`] 时必须补 `channel_credit`，否则节点
    /// 会永远停在 `Suspend`——这条「补信用」的责任在 Hub 一侧。
    pub fn on_data_received(&mut self, channel_id: &str) -> Result<CreditStatus, ChannelError> {
        let ch = self.ready_mut(channel_id)?;
        ch.credit = ch.credit.saturating_sub(1);
        match ch.credit {
            0 => Ok(CreditStatus::Exhausted),
            _ => Ok(CreditStatus::Granted),
        }
    }

    /// 补信用：**只增不减**（§6）。
    ///
    /// 溢出按饱和处理——debug 下的 `+=` 会 panic（生产代码禁止 panic），
    /// 而「只增不减」本就意味着饱和是语义上正确的终值。
    ///
    /// 不要求通道已 `Ready`：信用是纯粹的加法，提前到的 `channel_credit`
    /// 拒绝它没有任何好处。
    pub fn on_credit(&mut self, channel_id: &str, delta: u32) -> Result<(), ChannelError> {
        let ch = self.live_mut(channel_id)?;
        ch.credit = ch.credit.saturating_add(delta);
        Ok(())
    }

    /// 被动收 `close_channel`：只置终态，**不回帧**。
    pub fn mark_closed(&mut self, channel_id: &str) -> Result<(), ChannelError> {
        let ch = self.channels_mut(channel_id)?;
        ch.state = ChannelState::Closed;
        Ok(())
    }

    /// 显式拒绝：置终态并回 `channel_error{code, reason}`。
    ///
    /// §5 的超时拒绝是它的一个用法：connector 不支持时回
    /// `reason: "unsupported"`（见 [`reason::UNSUPPORTED`]）。
    pub fn reject(
        &mut self,
        channel_id: &str,
        code: ChannelErrorCode,
        reason: &str,
    ) -> Result<ChannelAction, ChannelError> {
        let ch = self.live_mut(channel_id)?;
        ch.state = ChannelState::Closed;
        Ok(ChannelAction::ReplyError {
            channel_id: ch.id.clone(),
            code,
            reason: reason.to_string(),
        })
    }

    /// 单条通道的 `channel_ready` 超时判定（§5）。
    pub fn ready_timed_out(&self, channel_id: &str, now: Instant) -> bool {
        match self.channels.get(channel_id) {
            Some(ch) => ch.ready_expired(now),
            None => false,
        }
    }

    /// 所有已超时的 `Opening` 通道，**按 `channel_id` 排序**保证输出稳定。
    ///
    /// M0 的 Hub 在 `open_channel` 到达的同一刻就置 Ready，所以这条恒返回空：
    /// 它是 `reject` 的兜底调用点，M1 引入 connector 协商（`open_channel` 要先
    /// 问 connector）之后才会有「开了没 Ready」的通道可扫。
    pub fn expired_ready(&self, now: Instant) -> Vec<String> {
        let mut ids: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, ch)| ch.ready_expired(now))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn get(&self, channel_id: &str) -> Option<&Channel> {
        self.channels.get(channel_id)
    }

    pub fn state(&self, channel_id: &str) -> Option<ChannelState> {
        self.channels.get(channel_id).map(|ch| ch.state)
    }

    pub fn credit(&self, channel_id: &str) -> Option<u32> {
        self.channels.get(channel_id).map(|ch| ch.credit)
    }

    pub fn connector(&self, channel_id: &str) -> Option<&str> {
        self.channels
            .get(channel_id)
            .map(|ch| ch.connector.as_str())
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// 取可变引用，含终态。
    fn channels_mut(&mut self, channel_id: &str) -> Result<&mut Channel, ChannelError> {
        self.channels
            .get_mut(channel_id)
            .ok_or_else(|| ChannelError::UnknownChannel(channel_id.to_string()))
    }

    /// 取可变引用并排除终态：`Closed` 不可再参与数据面。
    fn live_mut(&mut self, channel_id: &str) -> Result<&mut Channel, ChannelError> {
        let ch = self.channels_mut(channel_id)?;
        if ch.state == ChannelState::Closed {
            return Err(ChannelError::AlreadyClosed(channel_id.to_string()));
        }
        Ok(ch)
    }

    /// 取可变引用并要求已 `channel_ready`：数据面只在 `Ready` 上有定义。
    fn ready_mut(&mut self, channel_id: &str) -> Result<&mut Channel, ChannelError> {
        let ch = self.live_mut(channel_id)?;
        if !ch.is_ready() {
            return Err(ChannelError::NotReady(channel_id.to_string()));
        }
        Ok(ch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::message::types;

    fn t0() -> Instant {
        Instant::now()
    }

    /// 开一条通道并置 `Ready`，返回注册表。
    fn ready_registry(channel_id: &str, connector: &str, credit: u32) -> ChannelRegistry {
        let mut reg = ChannelRegistry::new();
        reg.open(channel_id, connector, credit, t0()).unwrap();
        reg.mark_ready(channel_id).unwrap();
        reg
    }

    #[test]
    fn open_walks_opening_to_ready() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        assert!(reg.is_empty());
        reg.open("c1", "terminal", CREDIT_START, t).unwrap();
        assert_eq!(reg.state("c1"), Some(ChannelState::Opening));
        assert_eq!(reg.credit("c1"), Some(CREDIT_START));
        assert_eq!(reg.connector("c1"), Some("terminal"));
        assert_eq!(reg.len(), 1);

        let ch = reg.get("c1").unwrap();
        assert_eq!(ch.id(), "c1");
        assert_eq!(ch.connector(), "terminal");
        assert_eq!(ch.state(), ChannelState::Opening);
        assert_eq!(ch.credit(), CREDIT_START);
        assert!(!ch.is_ready());

        reg.mark_ready("c1").unwrap();
        assert!(reg.get("c1").unwrap().is_ready());
    }

    #[test]
    fn open_resets_credit_and_state_every_time() {
        // §6 末条点名的坑：重连后 Hub 按 channel_id 记账，信用只增不减
        // 会无限累积。每次 open_channel 必须把信用打回 credit_start。
        let t = t0();
        let mut reg = ready_registry("c1", "terminal", 1);
        // 耗尽
        assert!(matches!(
            reg.on_data("c1").unwrap(),
            ChannelAction::SendData {
                suspend_after: true,
                ..
            }
        ));
        assert_eq!(reg.credit("c1"), Some(0));

        // 旧通道上的信用不算数：重新 open 一律重来
        let later = t + Duration::from_secs(10);
        reg.open("c1", "terminal", CREDIT_START, later).unwrap();
        assert_eq!(reg.credit("c1"), Some(CREDIT_START));
        assert_eq!(reg.state("c1"), Some(ChannelState::Opening));
        // ready 计时也重开，否则重连后必然秒超时
        assert!(!reg.ready_timed_out("c1", later + READY_TIMEOUT - Duration::from_secs(1)));
    }

    #[test]
    fn open_rejects_empty_channel_id() {
        let mut reg = ChannelRegistry::new();
        assert!(matches!(
            reg.open("", "terminal", 1, t0()),
            Err(ChannelError::EmptyChannelId)
        ));
        assert!(reg.is_empty());
    }

    #[test]
    fn mark_ready_rejects_unknown_repeat_and_terminal() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        assert!(matches!(
            reg.mark_ready("nope"),
            Err(ChannelError::UnknownChannel(_))
        ));

        reg.open("c1", "files", 1, t).unwrap();
        reg.mark_ready("c1").unwrap();
        assert!(matches!(
            reg.mark_ready("c1"),
            Err(ChannelError::AlreadyReady(_))
        ));

        reg.mark_closed("c1").unwrap();
        assert!(matches!(
            reg.mark_ready("c1"),
            Err(ChannelError::AlreadyClosed(_))
        ));
    }

    #[test]
    fn on_data_consumes_credit_then_suspends() {
        let mut reg = ready_registry("c1", "terminal", 2);
        // 还有余额：放行，继续发
        assert_eq!(
            reg.on_data("c1").unwrap(),
            ChannelAction::SendData {
                channel_id: "c1".into(),
                suspend_after: false
            }
        );
        assert_eq!(reg.credit("c1"), Some(1));
        // 最后一格：仍放行，但发完必须挂起
        assert_eq!(
            reg.on_data("c1").unwrap(),
            ChannelAction::SendData {
                channel_id: "c1".into(),
                suspend_after: true
            }
        );
        assert_eq!(reg.credit("c1"), Some(0));
        // 归零后才停
        assert_eq!(
            reg.on_data("c1").unwrap(),
            ChannelAction::Suspend {
                channel_id: "c1".into()
            }
        );
    }

    #[test]
    fn on_data_rejects_unknown_closed_and_not_ready() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        assert!(matches!(
            reg.on_data("nope"),
            Err(ChannelError::UnknownChannel(_))
        ));

        reg.open("c1", "terminal", 1, t).unwrap();
        assert!(matches!(reg.on_data("c1"), Err(ChannelError::NotReady(_))));

        reg.mark_ready("c1").unwrap();
        reg.mark_closed("c1").unwrap();
        assert!(matches!(
            reg.on_data("c1"),
            Err(ChannelError::AlreadyClosed(_))
        ));
    }

    #[test]
    fn on_data_received_reports_exhaustion() {
        let mut reg = ready_registry("c1", "terminal", 2);
        assert_eq!(reg.on_data_received("c1").unwrap(), CreditStatus::Granted);
        assert_eq!(reg.credit("c1"), Some(1));
        assert_eq!(reg.on_data_received("c1").unwrap(), CreditStatus::Exhausted);
        assert_eq!(reg.credit("c1"), Some(0));
        // 已经归零时再收到一帧（对方不守信用）不发生下溢，只重复报耗尽
        assert_eq!(reg.on_data_received("c1").unwrap(), CreditStatus::Exhausted);
    }

    #[test]
    fn on_data_received_rejects_unknown_closed_and_not_ready() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        assert!(matches!(
            reg.on_data_received("nope"),
            Err(ChannelError::UnknownChannel(_))
        ));

        reg.open("c1", "terminal", 1, t).unwrap();
        assert!(matches!(
            reg.on_data_received("c1"),
            Err(ChannelError::NotReady(_))
        ));

        reg.mark_ready("c1").unwrap();
        reg.mark_closed("c1").unwrap();
        assert!(matches!(
            reg.on_data_received("c1"),
            Err(ChannelError::AlreadyClosed(_))
        ));
    }

    #[test]
    fn on_credit_only_increases_and_saturates() {
        let mut reg = ready_registry("c1", "terminal", 1);
        reg.on_data("c1").unwrap();
        assert_eq!(reg.credit("c1"), Some(0));
        // 恢复：成功即恢复信号（没有单独的 Resume 动作）
        reg.on_credit("c1", CREDIT_DELTA).unwrap();
        assert_eq!(reg.credit("c1"), Some(CREDIT_DELTA));
        assert!(matches!(
            reg.on_data("c1").unwrap(),
            ChannelAction::SendData { .. }
        ));

        // 溢出饱和，不得 panic
        reg.on_credit("c1", u32::MAX).unwrap();
        assert_eq!(reg.credit("c1"), Some(u32::MAX));
    }

    #[test]
    fn on_credit_accepts_channel_still_opening() {
        // 信用是纯加法：提前到的 channel_credit 没有拒绝的理由
        let mut reg = ChannelRegistry::new();
        reg.open("c1", "git", 0, t0()).unwrap();
        reg.on_credit("c1", 4).unwrap();
        assert_eq!(reg.credit("c1"), Some(4));
    }

    #[test]
    fn on_credit_rejects_unknown_and_closed() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        assert!(matches!(
            reg.on_credit("nope", 1),
            Err(ChannelError::UnknownChannel(_))
        ));

        reg.open("c1", "git", 1, t).unwrap();
        reg.mark_closed("c1").unwrap();
        assert!(matches!(
            reg.on_credit("c1", 1),
            Err(ChannelError::AlreadyClosed(_))
        ));
    }

    #[test]
    fn mark_closed_is_silent_and_idempotent() {
        let mut reg = ready_registry("c1", "terminal", 1);
        reg.mark_closed("c1").unwrap();
        reg.mark_closed("c1").unwrap();
        assert_eq!(reg.state("c1"), Some(ChannelState::Closed));
        assert!(matches!(
            reg.mark_closed("nope"),
            Err(ChannelError::UnknownChannel(_))
        ));
    }

    #[test]
    fn reject_returns_reply_error_and_closes_channel() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        reg.open("c1", "ssh", 1, t).unwrap();
        assert_eq!(
            reg.reject("c1", ChannelErrorCode::Unsupported, reason::UNSUPPORTED)
                .unwrap(),
            ChannelAction::ReplyError {
                channel_id: "c1".into(),
                code: ChannelErrorCode::Unsupported,
                reason: "unsupported".into()
            }
        );
        assert_eq!(reg.state("c1"), Some(ChannelState::Closed));
        // 拒绝之后不能再拒绝：已经没有可回帧的对象
        assert!(matches!(
            reg.reject("c1", ChannelErrorCode::Internal, "x"),
            Err(ChannelError::AlreadyClosed(_))
        ));
        assert!(matches!(
            reg.reject("nope", ChannelErrorCode::Internal, "x"),
            Err(ChannelError::UnknownChannel(_))
        ));
    }

    #[test]
    fn ready_timeout_applies_to_opening_channels_only() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        assert!(!reg.ready_timed_out("nope", t + READY_TIMEOUT));
        // 未知通道不是超时，是「没有这条通道」——两者都必须不发 channel_error
        assert!(reg.expired_ready(t + READY_TIMEOUT).is_empty());

        reg.open("c1", "terminal", 1, t).unwrap();
        assert!(!reg.ready_timed_out("c1", t));
        assert!(!reg.ready_timed_out("c1", t + READY_TIMEOUT - Duration::from_millis(1)));
        assert!(reg.ready_timed_out("c1", t + READY_TIMEOUT));

        reg.mark_ready("c1").unwrap();
        assert!(!reg.ready_timed_out("c1", t + READY_TIMEOUT * 10));
        assert!(reg.expired_ready(t + READY_TIMEOUT * 10).is_empty());
    }

    #[test]
    fn expired_ready_lists_only_timed_out_channels_sorted() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        reg.open("c3", "terminal", 1, t).unwrap();
        reg.open("c1", "files", 1, t).unwrap();
        // c2 晚开 4s，所以 t+5s 时它还没到点
        reg.open("c2", "git", 1, t + Duration::from_secs(4))
            .unwrap();

        // 到 5s：c1/c3 恰好到点，c2 只开了 1s 不在内
        assert_eq!(
            reg.expired_ready(t + READY_TIMEOUT),
            vec!["c1".to_string(), "c3".to_string()]
        );
        assert_eq!(
            reg.expired_ready(t + Duration::from_secs(60)),
            vec!["c1".to_string(), "c2".to_string(), "c3".to_string()]
        );
    }

    /// 状态机对「超时判定只看 `Opening`」的另一个推论：Ready 通道**永远不会**
    /// 出现在 `expired_ready` 里，无论过多久。两条混在一起会让一次慢 connector
    /// 的重连把一条已经就绪的通道掐断。
    #[test]
    fn a_ready_channel_is_never_expired() {
        let t = t0();
        let mut reg = ChannelRegistry::new();
        reg.open("c1", "terminal", CREDIT_START, t).unwrap();
        assert!(reg.expired_ready(t).is_empty());

        reg.mark_ready("c1").unwrap();
        assert!(reg
            .expired_ready(t + READY_TIMEOUT + Duration::from_secs(86_400))
            .is_empty());
    }

    /// 重开一个**已经 Ready 过**的 `channel_id` 会把它打回 `Opening`：
    /// §6 末条要求每次 `open_channel` 都重置信用，状态同理——
    /// 否则 Hub 侧会拿着一份陈旧的 `Ready` 去路由数据。
    #[test]
    fn reopening_a_ready_channel_puts_it_back_into_opening() {
        let t = t0();
        let mut reg = ready_registry("c1", "terminal", 4);
        assert!(reg.expired_ready(t).is_empty());

        reg.open("c1", "files", CREDIT_START, t).unwrap();
        assert_eq!(reg.state("c1"), Some(ChannelState::Opening));
        assert_eq!(reg.credit("c1"), Some(CREDIT_START));
        assert_eq!(reg.connector("c1"), Some("files"));
        // 回到 Opening 就重新开始算 5s：刚才那份 Ready 不能让它一开局就超时
        assert!(reg.expired_ready(t).is_empty());
        assert_eq!(reg.expired_ready(t + READY_TIMEOUT), vec!["c1".to_string()]);
    }

    /// 数据面在 `Opening` 期间必须**一条都收不到**（§5：先 `channel_ready`）。
    ///
    /// 这条与「信用耗尽 → Suspend」方向相反：那边是 Ready 但没额度，
    /// 这里是额度满着但还没 Ready。两者混为一谈会让未就绪的 connector
    /// 直接开始吐数据。
    #[test]
    fn an_opening_channel_carries_data_in_neither_direction() {
        let mut reg = ChannelRegistry::new();
        reg.open("c1", "terminal", 0, t0()).unwrap();
        assert!(matches!(reg.on_data("c1"), Err(ChannelError::NotReady(_))));
        assert!(matches!(
            reg.on_data_received("c1"),
            Err(ChannelError::NotReady(_))
        ));

        // 就绪之后同一个 channel_id 立刻恢复可用——不是被 open 拒掉了
        reg.mark_ready("c1").unwrap();
        assert!(reg.on_data("c1").is_ok());
    }

    #[test]
    fn channel_error_code_alias_shares_err_code_wire_values() {
        // 别名而非新 enum：§4 表格只有一份 code 清单
        let p = ChannelErrorPayload {
            channel_id: "c1".into(),
            code: ChannelErrorCode::Unsupported,
            reason: reason::UNSUPPORTED.into(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["code"], "unsupported");
        let f = Frame::push(types::CHANNEL_ERROR, 0, json);
        let back: ChannelErrorPayload = f.payload().unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn open_channel_new_matches_the_serde_default() {
        let built = OpenChannel::new("c1", "terminal");
        assert_eq!(built.credit_start, CREDIT_START);
        assert!(built.params.is_null());

        let back: OpenChannel = Frame::push(
            types::OPEN_CHANNEL,
            0,
            serde_json::to_value(&built).unwrap(),
        )
        .payload()
        .unwrap();
        assert_eq!(back.credit_start, built.credit_start);

        let with = OpenChannel::with_params("c1", "files", serde_json::json!({"path": "."}));
        assert_eq!(with.credit_start, CREDIT_START);
        assert_eq!(with.params["path"], ".");
    }

    #[test]
    fn open_channel_roundtrips_and_defaults_credit_start() {
        let p = OpenChannel {
            channel_id: "c1".into(),
            connector: "terminal".into(),
            params: serde_json::json!({"cols": 80}),
            credit_start: 8,
        };
        let f = Frame::push(types::OPEN_CHANNEL, 0, serde_json::to_value(&p).unwrap());
        let back: OpenChannel = f.payload().unwrap();
        assert_eq!(back.channel_id, "c1");
        assert_eq!(back.connector, "terminal");
        assert_eq!(back.params["cols"], 80);
        assert_eq!(back.credit_start, 8);

        // 老节点不填 credit_start：按 CREDIT_START，不判非法（约束 14）
        let f = Frame::push(
            types::OPEN_CHANNEL,
            0,
            serde_json::json!({"channel_id":"c1","connector":"files"}),
        );
        let back: OpenChannel = f.payload().unwrap();
        assert_eq!(back.credit_start, CREDIT_START);
        assert!(back.params.is_null());
    }

    #[test]
    fn channel_ready_defaults_capabilities() {
        let f = Frame::push(
            types::CHANNEL_READY,
            0,
            serde_json::json!({"channel_id":"c1","capabilities":{"pty":true}}),
        );
        let back: ChannelReady = f.payload().unwrap();
        assert_eq!(back.capabilities["pty"], true);

        let f = Frame::push(
            types::CHANNEL_READY,
            0,
            serde_json::json!({"channel_id":"c1"}),
        );
        let back: ChannelReady = f.payload().unwrap();
        assert!(back.capabilities.is_null());
    }

    #[test]
    fn channel_data_payload_is_opaque_here() {
        // §4：PTY 字节 / 文件读写结果各自 schema 随 M1 定义，本层只透传
        let p = ChannelData {
            channel_id: "c1".into(),
            payload: serde_json::json!("\u{1b}[31mred"),
        };
        let f = Frame::push(types::CHANNEL_DATA, 0, serde_json::to_value(&p).unwrap());
        let back: ChannelData = f.payload().unwrap();
        assert_eq!(back.payload, p.payload);

        let f = Frame::push(
            types::CHANNEL_DATA,
            0,
            serde_json::json!({"channel_id":"c1"}),
        );
        let back: ChannelData = f.payload().unwrap();
        assert!(back.payload.is_null());
    }

    #[test]
    fn channel_credit_delta_is_required() {
        let p = ChannelCredit {
            channel_id: "c1".into(),
            delta: CREDIT_DELTA,
        };
        let f = Frame::push(types::CHANNEL_CREDIT, 0, serde_json::to_value(&p).unwrap());
        let back: ChannelCredit = f.payload().unwrap();
        assert_eq!(back.delta, CREDIT_DELTA);

        // 没有 delta 的 frame 无从补信用，必须解不出而不是当 0 静默放行
        let f = Frame::push(
            types::CHANNEL_CREDIT,
            0,
            serde_json::json!({"channel_id":"c1"}),
        );
        assert!(f.payload::<ChannelCredit>().is_err());
    }

    #[test]
    fn close_channel_defaults_reason() {
        let p = CloseChannel {
            channel_id: "c1".into(),
            reason: "session ended".into(),
        };
        let f = Frame::push(types::CLOSE_CHANNEL, 0, serde_json::to_value(&p).unwrap());
        let back: CloseChannel = f.payload().unwrap();
        assert_eq!(back.reason, "session ended");

        let f = Frame::push(
            types::CLOSE_CHANNEL,
            0,
            serde_json::json!({"channel_id":"c1"}),
        );
        let back: CloseChannel = f.payload().unwrap();
        assert!(back.reason.is_empty());
    }

    #[test]
    fn every_channel_payload_type_is_ignorable() {
        // 约束 14：新增可选消息类型不 bump 版本，老节点必须能忽略
        for t in [
            types::OPEN_CHANNEL,
            types::CHANNEL_READY,
            types::CHANNEL_DATA,
            types::CHANNEL_CREDIT,
            types::CLOSE_CHANNEL,
            types::CHANNEL_ERROR,
        ] {
            assert!(!types::is_required(t), "{t} must stay optional");
        }
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = ChannelRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.is_empty());
        assert_eq!(reg.state("nope"), None);
        assert_eq!(reg.credit("nope"), None);
        assert_eq!(reg.connector("nope"), None);
    }
}
