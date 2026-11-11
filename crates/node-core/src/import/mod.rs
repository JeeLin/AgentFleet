//! 外部 agent 历史会话的 provider 注册表（v0.7.4）。
//!
//! 这里只回答两个问题：这台设备上某个 agent 软件有哪些**本机历史会话**，
//! 以及其中一条的 ACP 会话 id 与工作目录是什么。历史消息本身不搬——
//! 导入 = 给 Hub 会话预填 `acp_session_id`（绑定非搬运，见
//! `docs/architecture/session-and-trajectory.md` 的「会话恢复走 ACP」）。
//!
//! # 加一个 provider 之前先做三件事
//!
//! 门槛与 `agent/detect.rs` 里 `KNOWN_AGENTS` 的「加新条目之前先做三件事」
//! 同源，只是多一条「历史会话从哪读」：
//!
//! 1. 用真机把 `initialize` → `session/new` → `session/prompt` 跑通，确认
//!    这条 ACP 入口的参数形态与本仓 `agent/acp.rs` 的假设一致。
//! 2. 确认历史会话的**来源**（哪个库、哪张表、哪个列是 ACP 会话 id），并
//!    只读验证一遍：读得出来，且读完那个库一个字节都没变。
//! 3. 补一条 `#[ignore]` 真机测试留档（模板见 `agent/conversation.rs` 的
//!    `a_real_opencode_turn_reaches_the_channel_as_a_batch`）。
//!
//! 三步没走完就注册的后果与 detect.rs 描述的完全一样：单测全绿、页面正常，
//! 用户选中一条历史会话后永远等不到回话。**支持的唯一判据不是「库文件存在」**
//! 而是这个 agent 在 `KNOWN_AGENTS` 里 `acp_args` 非空——导入之后要继续对话，
//! 走的就是那条 ACP 入口（[`lookup`] 每次都重新查清单，不是写死的假设）。
//!
//! # 为什么不在这里读 `dirs::data_dir()`
//!
//! provider 的 `data_dir` 是参数：生产传平台数据目录，测试传临时目录。
//! 写在里面读全局路径，单测就只能碰机器上**真实**的 `opencode.db`——
//! 那是用户的数据，测试不该碰它。

use std::path::Path;

use crate::agent::detect::KNOWN_AGENTS;

pub mod opencode;

use opencode::OPENCODE;

/// 已知 agent，但它没有 ACP 入口（`acp_args` 为空）：能扫到，不能对话，
/// 因此也不能导入。
const REASON_NO_ACP_ENTRY: &str = "known agent, but it has no ACP entry: it can be \
     detected, not driven over ACP, so its history cannot be bound";

/// 注册表里有 provider，但对应的 agent 已经没有 ACP 入口了。
///
/// 与 [`REASON_NO_ACP_ENTRY`] 分开，是因为处置不同：这条是**注册表与
/// `KNOWN_AGENTS` 脱节**（有人清空了 `acp_args` 却没摘 provider），
/// 前者只是「这个软件本来就不支持」。
const REASON_PROVIDER_WITHOUT_ACP: &str = "a history provider is registered for this \
     agent, but the agent has no ACP entry now: binding its history could not be resumed";

/// 完全不认识的 agent id（连 `KNOWN_AGENTS` 里都没有）。
const REASON_UNKNOWN_PROVIDER: &str = "unknown provider: no history provider is \
     registered for this agent id";

/// 一条可导入的本机历史会话。只含 CLI 展示与选择所需的最小字段。
///
/// 不含消息、不含 blob、不含凭据：导入是绑定一个 ACP 会话 id，不是搬运历史。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalSession {
    /// 提供方 id（当前只有 `"opencode"`）。
    pub provider: &'static str,
    /// provider 自己的会话 id（opencode 形如 `ses_...`）。原样使用，不改写。
    pub external_session_id: String,
    /// 标题。provider 里为空 / 为 NULL 时是空串，**不编造**一个占位标题。
    pub title: String,
    /// 会话的工作目录（provider 记录的原样路径，未做 `~` 展开）。
    pub cwd: String,
    /// 更新时间。排序用原样值（数字列转成十进制字符串），缺失为 `None`。
    pub updated_at: Option<String>,
}

/// 选中一条历史会话之后交给 Hub 的东西。
///
/// `acp_session_id` 直接取 provider 的会话 id（opencode 的 `ses_*`），
/// **不转换、不加前缀**：Hub 打开对话时把它下发给设备走 `session/load`，
/// 换一个 id 就等于换了一个会话。
///
/// `provider` 同时就是 Hub 侧的 `agent_id`（v1 两者 1:1 同源），不再另设
/// 一个恒等的字段：CLI 发签名请求时直接取它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCandidate {
    /// 提供方 id；同时作为 Hub 的 `agent_id`。
    pub provider: &'static str,
    /// 恢复这条会话要用的 ACP 会话 id。
    pub acp_session_id: String,
    /// 会话的工作目录。
    pub cwd: String,
}

/// 导入探测的分类错误。
///
/// **不携带 SQL 片段、DB 绝对路径或原始 DB 字符串**（约束 13 的同一口径）：
/// 这些错误会出现在 CLI 输出里，而 DB 路径本身就不是该给用户看的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImportError {
    #[error("no session database was found for this provider")]
    DbMissing,
    #[error("the session database schema is not one this provider understands")]
    SchemaMismatch,
    #[error("a session row could not be read")]
    Decode,
    #[error("the session workspace path is neither absolute nor `~`-prefixed")]
    InvalidWorkspacePath,
    #[error("the session id is blank")]
    BlankSessionId,
}

/// 一个历史会话提供方。
///
/// 两个方法都收 `data_dir` 而不是自己去读全局路径（见模块注释），
/// 也都**只读**：provider 不允许写对方的库、不允许建表 / 迁移。
pub trait HistoryProvider {
    /// 稳定 id，与 `KNOWN_AGENTS[].id` 同一个命名空间。
    fn id(&self) -> &'static str;

    /// 列出这台设备上该 agent 的历史会话。
    fn list(&self, data_dir: &Path) -> Result<Vec<HistoricalSession>, ImportError>;

    /// 按 provider 自己的会话 id 精确解析一条。
    fn resolve(
        &self,
        data_dir: &Path,
        external_session_id: &str,
    ) -> Result<ImportCandidate, ImportError>;
}

/// 注册表的查询结果。
///
/// `Unsupported` 携带 `agent_id` 而不是一个裸 reason：CLI 要把「是哪个 id
/// 不支持」和「为什么不支持」一起打出来，否则用户无法区分「我拼错了名字」
/// 与「这个软件本来就不能导入」。
pub enum ProviderLookup {
    Supported(&'static dyn HistoryProvider),
    Unsupported {
        agent_id: String,
        reason: &'static str,
    },
}

/// 注册表的一项：取 trait 对象的函数指针。
///
/// 存函数指针而不是 `&'static dyn HistoryProvider`：`static` 要求 `Sync`，
/// 而 `dyn HistoryProvider` 没有这个约束（也不该为了一个注册表把 `Sync`
/// 强加给所有 provider——它们只读一个库，本来无状态）。函数指针是
/// `Sync`，调用时才取 trait 对象，契约因此保持原样。
type ProviderFactory = fn() -> &'static dyn HistoryProvider;

/// 已注册的 provider。**加一条之前先读模块注释里的三步门槛。**
static PROVIDERS: &[ProviderFactory] = &[|| &OPENCODE];

/// 按 `agent_id` 选 provider。
///
/// 支持 = 「注册表里有 provider」**且**「这个 agent 在 `KNOWN_AGENTS` 里
/// `acp_args` 非空」。第二个条件每次都现查清单：哪天有人把 `opencode` 的
/// `acp_args` 清空（它就不再能对话了），这里必须立刻变成 `Unsupported`，
/// 而不是继续把用户往一个接不上的会话上引。
pub fn lookup(agent_id: &str) -> ProviderLookup {
    let provider = PROVIDERS
        .iter()
        .map(|get| get())
        .find(|p| p.id() == agent_id);
    classify(provider, agent_id)
}

/// 判定逻辑本身。`provider` 是参数而不是在函数里查 [`PROVIDERS`]：
/// 「注册了 provider 但清单里已没有 ACP 入口」那条臂在生产注册表下不可达
/// （要它发生得同时改两处），只能靠注入一个假 provider 才测得出来。
fn classify(provider: Option<&'static dyn HistoryProvider>, agent_id: &str) -> ProviderLookup {
    match provider {
        Some(p) if has_acp_entry(agent_id) => ProviderLookup::Supported(p),
        Some(_) => ProviderLookup::Unsupported {
            agent_id: agent_id.to_string(),
            reason: REASON_PROVIDER_WITHOUT_ACP,
        },
        None if is_known_agent(agent_id) => ProviderLookup::Unsupported {
            agent_id: agent_id.to_string(),
            reason: REASON_NO_ACP_ENTRY,
        },
        None => ProviderLookup::Unsupported {
            agent_id: agent_id.to_string(),
            reason: REASON_UNKNOWN_PROVIDER,
        },
    }
}

/// 这个 id 是不是「能被扫描发现的已知 agent」。
///
/// 只用于把错误文案分成两类，**不是**支持判据：已知不等于可对话。
fn is_known_agent(agent_id: &str) -> bool {
    KNOWN_AGENTS.iter().any(|spec| spec.id == agent_id)
}

/// 这个 id 在清单里有没有 ACP 入口（即 `AgentSpec::can_converse()`）。
fn has_acp_entry(agent_id: &str) -> bool {
    KNOWN_AGENTS
        .iter()
        .find(|spec| spec.id == agent_id)
        .is_some_and(|spec| spec.can_converse())
}

#[cfg(test)]
mod tests;
