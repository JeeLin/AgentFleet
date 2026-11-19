//! 任务仓储：看板上的一张卡片。
//!
//! 六态见 `task-model.md` §2。**`state` 的取值校验在 Rust 侧**，因为
//! 「哪些态之间能转」本来就是业务规则，而 SQLite 的 `CHECK` 只能表达"取值属于
//! 这个集合"，无法表达"`done` 不能直接回到 `pending`"这类转移约束。把校验拆成
//! 两处只会让"什么合法"有两个答案（`schema_v5.sql` 里写了同一个理由）。
//!
//! # `base_commit` 的两份，不要混
//!
//! - `task.base_commit`：**默认起点**，给"还没派发时页面显示什么"用。
//! - `execution.base_commit`：**唯一真相源**（v0.6.0 交接规则）。
//!
//! 任何一处把起点当权威读，都必须读 execution 的那一份。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::Db;

/// 任务六态（`task-model.md` §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    /// 待办：人建好了，还没进派发队列。
    Pending,
    /// 排队中：已选中设备与 Agent，等设备接活。
    Queued,
    /// 执行中：设备已接活，有执行在跑。
    Running,
    /// 待验收：产出已交出，等人判 `done_when`。
    Review,
    /// 验收通过且已合入。
    Done,
    /// 最后一次执行失败，且已不再重试。
    Failed,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Review => "review",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// 这个态是不是「已经结束」。看板用它决定卡片放在"进行中"还是"归档列"。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

impl FromStr for TaskState {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "review" => Ok(Self::Review),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            // 不回落成 `Pending`：v7 迁移之后库里再出现 `draft` / `ready` /
            // `blocked`，说明有代码写回了旧值，静默回落会把它藏起来。
            other => Err(StorageError::InvalidTaskState(other.to_string())),
        }
    }
}

/// 验收标准：**判定码 + 给人看的一句话**（D23）。
///
/// 两列而不是一段自由文本：`criteria` 是机器判的（`all_tests_pass` 等），
/// `description` 是给人读的。合成一段文本会让"按判定码统计哪种验收最常失败"
/// 变成一次自然语言解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneWhen {
    /// 判定码。`None` = 还没定（`pending` 态的正常情况，卡片要标「需人工验收」）。
    pub criteria: Option<String>,
    /// 一句话说明。`None` = 还没写。
    pub description: Option<String>,
}

/// `done_when.criteria` 的**判定码闭集**，共四个。这是唯一定义处。
///
/// 为什么是闭集而不是自由文本：`description` 是给人读的，而 `criteria` 存在的
/// 全部理由就是**能被机器判**（`DECISIONS.md` D23）。自由文本会让「按判定码
/// 统计哪种验收最常失败」永远做不了，而 `schema_v5.sql` 那两列就是为这个统计
/// 拆的。多一个码 = 多一条验收规则，必须是**先有判定实现，再加码**。
///
/// 四个码各自对应的判定：
/// - `all_tests_pass`：项目测试全绿。
/// - `build_passes`：构建成功（可带时长阈值，本轮只存码）。
/// - `review_approved`：**人/模型的主观判定，机器判不了**——见
///   `needs_manual_acceptance`，它落在闭集里但语义上仍需人点头。
/// - `command_exit_zero`：某条命令退出码为 0（`butler.md` §2.2.0 的
///   `{kind:"command", ..., expect:{exit_code:0}}` 的码形态）。
///
/// 大小写敏感：`"ALL_TESTS_PASS"` 不是这四个之一。理由是这些码会被写进
/// 库、被统计、被前端按字面值匹配，容忍大小写等于让同一个判定码有多个写法。
///
/// 不建议调用方自己做 `contains`——用 [`is_known_criteria_code`]，免得每处
/// 各写一遍（含大小写与空白处理）的口径。
pub const CRITERIA_CODES: [&str; 4] = [
    "all_tests_pass",
    "build_passes",
    "review_approved",
    "command_exit_zero",
];

/// 某串判定码**是否在闭集里**。大小写敏感、不做 trim。
///
/// 读路径拿它算 `criteria_known`（本次才引入闭集，库里可能已有闭集外的存量
/// 值），写路径拿它做校验——同一个口径，不写两份。
pub fn is_known_criteria_code(code: &str) -> bool {
    CRITERIA_CODES.contains(&code)
}

/// 拒绝判定码的提示：把四个允许值写进错误信息。
///
/// 只有「不允许」的错误等于把问题丢回给用户——他得知道**能填什么**才能改对。
fn invalid_criteria(code: &str) -> StorageError {
    StorageError::InvalidDoneWhenCriteria(code.to_string())
}

impl DoneWhen {
    /// 写库前校验判定条件：`criteria` 必须在闭集里、且 `criteria` 与
    /// `description` **至少有一个非空**。
    ///
    /// # 为什么 `None` 也合法（但两者不能同时为空）
    ///
    /// `criteria = None` 是**规格内的正常情况**（D23 / `butler.md` §2.2.0：
    /// 只有 `description` 的任务「只能人判」）。拒绝它会把「需人工验收」这条
    /// 产品能力直接堵死。空串同 `None` 处理——前端下拉框选「不设判定码」时
    /// 很可能送上来一个 `""`，那是没填，不是填错。
    ///
    /// 但**两者都空**必须拒绝：那是一个连人也不知道怎么算完成的任务，
    /// 验收环节会彻底空转（`butler.md`「两者至少有一个」）。
    ///
    /// # 为什么校验在仓储层而不只在 API 层
    ///
    /// 直接摸 `TaskRepo` 的路径（测试、未来的 CLI、迁移脚本）同样会写进那两列；
    /// 校验只在 HTTP 层等于给闭集留后门。
    pub fn validate(&self) -> Result<()> {
        let criteria_blank = matches!(self.criteria.as_deref(), None | Some(""));
        let description_blank = matches!(self.description.as_deref(), None | Some(""));
        if criteria_blank && description_blank {
            return Err(invalid_criteria("criteria 与 description 至少要有其一"));
        }
        match self.criteria.as_deref() {
            // 空串 = 没填判定码，与 `None` 同义。
            None | Some("") => Ok(()),
            Some(code) if is_known_criteria_code(code) => Ok(()),
            Some(code) => Err(invalid_criteria(code)),
        }
    }
}

/// 一次状态转移**顺带要写**的业务字段。
///
/// 只有 `done_when`：`failure` 的留痕落在 `execution` 上（`task` 表没有那三列，
/// 见 `schema_v7.sql`），走 [`crate::execution::ExecutionRepo::set_failure`]，
/// 不进这条 `UPDATE`。
#[derive(Debug, Default, Clone)]
pub struct TaskTransition {
    /// 这次转移要一起写进 `done_when` 的新验收标准（`failure` 的 `edit` 动作用）。
    /// `None` = 不动这两列。
    pub done_when: Option<DoneWhen>,
}

/// 一个任务的**读模型**：可写字段 + 只由仓储写、外部只读的元信息。
///
/// # 写入口是 `NewTask`，不是第二个结构体
///
/// 硬塞进写模型会让「建任务的入参」同时是「响应体的形状」，于是 `POST` /
/// `PATCH` 的反序列化也要求客户端送 `version`——那正是乐观锁退化成「客户端
/// 说了算」。所以写入口是 [`NewTask`]（只含能写进去的列），读出口是本类型。
///
/// **早先这里还有一个 `pub struct Task`** 想当「写模型」，但它零构造点
/// （`RawTask::into_task` 直接产出 `TaskView`，`TaskRepo` 的每个方法也返回
/// `TaskView`），留着只会让人以为存在两套结构、并让 `Task::needs_manual_acceptance`
/// 成为一条没有生产调用者的 `pub` API。已删除。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub state: TaskState,
    pub done_when: DoneWhen,
    pub created_by: Option<String>,
    pub base_commit: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// 约束四列，原文 JSON 字符串。`None` = 不限。
    pub allowed_devices: Option<String>,
    pub allowed_agents: Option<String>,
    pub required_capabilities: Option<String>,
    pub retry_limit: Option<i64>,
    /// 每次写入 +1。前端拿它做 `expected_version`（乐观锁）。
    pub version: i64,
    /// 被哪个新任务取代。本里程碑恒为 `None`（没有写者，见 [`TaskView`]）。
    pub superseded_by: Option<String>,
    /// 任务从哪来。本里程碑恒为 `None`（没有写者，见 [`TaskView`]）。
    pub intent_ref: Option<String>,
}

/// 建任务的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub project_id: String,
    pub title: String,
    pub done_when: DoneWhen,
    pub created_by: Option<String>,
    pub base_commit: Option<String>,
    /// 四条约束，建任务时一次写入（`None` = 不限）。
    ///
    /// 单层 `Option` 而不是 [`TaskPatch`] 的双层：建的时候没有「保持原值」这个
    /// 意图——那一行还不存在。
    ///
    /// **为什么必须在这里而不是「建完再补写一次」**：补写成 `update` 会让
    /// `version` 从 1 变 2，而 `version` 记的是「被改过几次」，建任务不是一次
    /// 修改。一个刚建好就显示「改过 2 次」的任务，会让「这条任务被人动过」这个
    /// 信号从第一天起就不可信。
    pub allowed_devices: Option<String>,
    pub allowed_agents: Option<String>,
    pub required_capabilities: Option<String>,
    pub retry_limit: Option<i64>,
}

impl NewTask {
    /// 建一个不带约束的任务（多数调用点与测试用）。
    pub fn new(
        project_id: impl Into<String>,
        title: impl Into<String>,
        done_when: DoneWhen,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            title: title.into(),
            done_when,
            created_by: None,
            base_commit: None,
            allowed_devices: None,
            allowed_agents: None,
            required_capabilities: None,
            retry_limit: None,
        }
    }
}

/// 改任务的补丁：**全字段可选，`None` = 保持原值**。
///
/// # 为什么约束列是双层 `Option`
///
/// 单层 `Option<String>` 里 `None` 只能表达一件事，而这里有两件：
/// 「这次不改这个字段」与「把这个字段清空成不限」。把两者压成同一个值，
/// 就会出现「只想改标题，结果把所有约束清空了」——这正是「全字段覆盖」式
/// 实现的症状。于是：
///
/// - 外层 = **改不改**（`None` = 不动这一列）；
/// - 内层 = **改成什么**（`Some(None)` = 写 `NULL`，即「不限」；
///   `Some(Some(v))` = 写 `v`）。
///
/// `title` / `done_when` 不做双层：它们非空才有意义（`title` 是卡片标题，
/// 清空等于让一张卡变成无名；`DoneWhen` 两列都空会被 [`DoneWhen::validate`]
/// 拒掉），所以「清空」不是一个合法意图，不需要第二层。
#[derive(Debug, Default, Clone)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub done_when: Option<DoneWhen>,
    /// 双层 Option：外层 = 改不改，内层 = 改成什么（`Some(None)` 清空为「不限」）
    pub allowed_devices: Option<Option<String>>,
    pub allowed_agents: Option<Option<String>>,
    pub required_capabilities: Option<Option<String>>,
    pub retry_limit: Option<Option<i64>>,
}

impl TaskPatch {
    /// 一个字的补丁都不含——「这次没有任何字段要改」。
    ///
    /// 为什么要能问：`update_if_version` 按命中的字段逐条执行 `UPDATE`，每条都带
    /// `version = version + 1`，所以**空补丁本来就不会涨 version**（一条都不跑）。
    /// 这个判定留给调用方（HTTP 层用它决定是不是直接回 204），
    /// 省得调用方自己把六个字段数一遍。
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.done_when.is_none()
            && self.allowed_devices.is_none()
            && self.allowed_agents.is_none()
            && self.required_capabilities.is_none()
            && self.retry_limit.is_none()
    }

    /// 这次补丁会不会动四条约束里的至少一条。
    ///
    /// 四条约束共用**一条** `UPDATE`（只涨一次 `version`），所以「动没动」
    /// 是一个整组判断，不能逐列问。用双层 `Option` 的外层判：`Some(None)`
    /// 是「清成不限」，同样算动。
    pub fn touches_constraints(&self) -> bool {
        self.allowed_devices.is_some()
            || self.allowed_agents.is_some()
            || self.required_capabilities.is_some()
            || self.retry_limit.is_some()
    }

    /// 这个补丁应当产生几条 `UPDATE`（标题 / `done_when` / 约束组各算一条）。
    ///
    /// 这是核对「一次都没落空」的基准数：实际影响行数少于它，就说明某条的
    /// `AND version = ?` 落空了。它必须与 `write_patch` 的分支**逐条对应**
    /// ——两处一旦不同步，CAS 就会在该报 409 的时候沉默。
    pub fn update_statements(&self) -> usize {
        usize::from(self.title.is_some())
            + usize::from(self.done_when.is_some())
            + usize::from(self.touches_constraints())
    }
}

impl TaskView {
    /// 这个任务是不是**只能人判**——看板据此挂「需人工验收」标签。
    ///
    /// 这是个**派生值，不是列**：它由 `done_when` 纯函数算出，落库就要维护
    /// 「两个字段改了、标记没跟上」的一致性，而它没有任何自己的信息。
    ///
    /// # 判定规则（逐条对回规格，不自己编）
    ///
    /// 规格 `task-model.md:290-292`：「**只有 `description` 没有 `criteria`
    /// 的任务**，卡片上必须标『需人工验收』，不能让用户以为有自动判定在跑。」
    /// 同一条在 `DECISIONS.md` D23 与 `butler.md` §2.2.0 写作「只有
    /// `description` → **只能人判**」。
    ///
    /// 于是「自动判得了」需要**两个条件同时成立**：
    ///
    /// 1. 有 `criteria`（非 `None`、非空串）——没有判定码就没有东西在跑；
    /// 2. 那个码**机器判得了**（在闭集里，且不是 `review_approved`）。
    ///
    /// 任何一条不成立 → `true`（需人工验收）：
    /// - `criteria` 为空：规格原话的情形。
    /// - `criteria` 是 `review_approved`：字面就是「评审通过」，那是**判断**，
    ///   不是命令退出码或测试结果。`butler.md` §2.2.0 说 `criteria` 必须
    ///   「判定结果确定，**不依赖模型**」，而这个码的判定离不开人或模型——
    ///   把它当自动可判，等于让卡片假装有个判定在跑，正是规格禁止的那件事。
    /// - `criteria` 是闭集外的值（存量数据 / 手改库）：我们**不知道**它怎么判，
    ///   当作「判不了」比当作「能判」安全——后者会让一个没有判定的任务显示成
    ///   「自动判定中」。
    ///
    /// 这条规则对 `description` 是否为 `None` **不敏感**：`criteria` 为空的
    /// 任务，哪怕描述也没写，也同样是「没人能自动判」，仍要标出来。
    /// 规格只描述「只有 `description`」这一种，但把「两者都空」判成可自动判定
    /// 显然与它同一节的用意相反，且没有第二条规格要求在这里报错
    /// （那是 `DoneWhen::validate` 与 API 层的事）。
    pub fn needs_manual_acceptance(&self) -> bool {
        // 直接内联判定，不再转发一层：`TaskView` 的这个方法就是唯一调用点，
        // 中间那个自由函数只是把这个 match 挪到了下面十行。判定本身一眼能读，
        // 「抽出来好单独测」也不成立——`DoneWhen` 的用例本来就都从这里过。
        match self.done_when.criteria.as_deref() {
            // `criteria` 为空（含空串）：没人能自动判，必须人工。
            None | Some("") => true,
            // 规格里唯一一个「结果是人给的」判定码。
            Some("review_approved") => true,
            // 闭集外的判定码：本版没有它的判定器，同样退到人工。
            Some(code) => !is_known_criteria_code(code),
        }
    }
}

/// 任务仓储。
#[derive(Clone)]
pub struct TaskRepo {
    pool: Db,
}

impl TaskRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 建任务。状态恒为 `pending`——「建好就能派发」会让一个缺验收标准的任务
    /// 直接进调度候选集，而那是最常见的失败来源。`queued` 要求已选中
    /// `(device, agent)`，新建时还没有这个选择，所以起点只能是 `pending`。
    ///
    /// 写库前先过 [`DoneWhen::validate`]：**先拒绝，再写**。放在仓储层而不只放
    /// API 层，是因为直接摸 `TaskRepo` 的路径（测试、未来的 CLI、迁移脚本）
    /// 同样会把值写进那两列；校验只在 HTTP 层等于给闭集留了个后门。
    pub fn create(&self, new: NewTask, now: &str) -> Result<TaskView> {
        new.done_when.validate()?;
        let conn = self.pool.get()?;
        let id = crate::workspace::new_id();
        conn.execute(
            "INSERT INTO task
                 (id, project_id, title, state, done_when_criteria, done_when_description,
                  created_by, base_commit, created_at, updated_at,
                  allowed_devices, allowed_agents, required_capabilities, retry_limit)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                new.project_id,
                new.title,
                new.done_when.criteria,
                new.done_when.description,
                new.created_by,
                new.base_commit,
                now,
                new.allowed_devices,
                new.allowed_agents,
                new.required_capabilities,
                new.retry_limit
            ],
        )?;
        let raw = conn.query_row(
            &format!("{SELECT_COLS} WHERE id = ?1"),
            params![id],
            row_to_raw,
        )?;
        raw.into_task()
    }

    pub fn get(&self, id: &str) -> Result<Option<TaskView>> {
        let conn = self.pool.get()?;
        let raw = conn
            .query_row(
                &format!("{SELECT_COLS} WHERE id = ?1"),
                params![id],
                row_to_raw,
            )
            .optional()?;
        raw.map(RawTask::into_task).transpose()
    }

    /// 某态的全部任务（调度候选集的入口）。
    ///
    /// **本里程碑生产零调用**：v0.8.0 没有派发器，没有「挑一批 `pending` 派出去」
    /// 的代码，本函数的第一个真实调用方是 v0.8.2 的调度器。保留它是因为调度器
    /// 的形状已经由规格定下（`task-model.md` 的 `pending → queued` 边要求
    /// 「按态取候选集」），删掉之后 v0.8.2 还要照原样写回来——那不是精简，是把
    /// 工作推迟到更贵的时刻。它有意外的单测（`list_by_state_filters_on_the_state`），
    /// 所以不是「写了没人验证的死代码」。
    pub fn list_by_state(&self, state: TaskState) -> Result<Vec<TaskView>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE state = ?1 ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(params![state.as_str()], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawTask::into_task).collect()
    }

    /// 一次转移**顺带写业务字段**（HTTP 层 `/state`、`/acceptance`、`/failure`）。
    ///
    /// # 为什么与 [`Self::set_state`] 分成两个方法而不是加参数
    ///
    /// `set_state` 是「只改状态」的**薄转发**——它就是本方法配
    /// `expected_version: None` 与空的 `TaskTransition`。两个名字并存是因为调用
    /// 点的意图不同：「改状态」与「一次带业务字段的转移」在代码里不该长得一样。
    /// 可选参数塞进一个方法，会让「这次转移会不会动别的列」变成要读调用方才知道
    /// 的事。
    ///
    /// # 两道 CAS 缺一不可
    ///
    /// `WHERE id = ? AND state = ?` 只挡状态竞态。两个请求都读 `review`、都写
    /// `done`，第二个会静默覆盖第一个——那正是「打回之后又被合入」的现场。
    /// 加上 `AND version = ?`（`expected_version` 为 `None` 时用刚读到的版本），
    /// 后写的那一个必然影响 0 行。
    ///
    /// # 为什么业务字段与状态同一条 `UPDATE`
    ///
    /// 拆成两条会出现「态变了、原因还没写」的中间态被别的连接读到，而失败留痕
    /// 恰恰是事后要反复读的东西。
    pub fn transition_with(
        &self,
        id: &str,
        to: TaskState,
        now: &str,
        expected_version: Option<i64>,
        extra: TaskTransition,
    ) -> Result<TaskView> {
        // 先读真值：不信任调用方说的 from。
        let current = self
            .get(id)?
            .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })?;
        crate::task_state::check_transition(current.state, to)?;
        if let Some(dw) = &extra.done_when {
            // 与 `update` 同一条纪律：**校验失败不写库、version 也不涨**。
            dw.validate()?;
        }
        let guard_version = match expected_version {
            Some(v) if v != current.version => {
                return Err(StorageError::TaskVersionChanged {
                    expected: v,
                    actual: current.version,
                })
            }
            Some(v) => v,
            None => current.version,
        };

        let conn = self.pool.get()?;
        // `?3 = 1` 是「这次改不改 done_when」的开关：两条分支共用一条语句，
        // 语句形状固定，SQLite 的语句缓存才不会每次落空。
        let has_done_when: i64 = i64::from(extra.done_when.is_some());
        let criteria: Option<&str> = extra.done_when.as_ref().and_then(|d| d.criteria.as_deref());
        let description: Option<&str> = extra
            .done_when
            .as_ref()
            .and_then(|d| d.description.as_deref());
        let n = conn.execute(
            "UPDATE task
                SET state = ?2,
                    done_when_criteria = CASE WHEN ?3 = 1 THEN ?4 ELSE done_when_criteria END,
                    done_when_description = CASE WHEN ?3 = 1 THEN ?5 ELSE done_when_description END,
                    version = version + 1, updated_at = ?6
              WHERE id = ?1 AND state = ?7 AND version = ?8",
            params![
                id,
                to.as_str(),
                has_done_when,
                criteria,
                description,
                now,
                current.state.as_str(),
                guard_version
            ],
        )?;
        if n == 0 {
            // 影响 0 行有两种可能，靠再查一次区分，不靠猜。
            return match self.get(id)? {
                None => Err(StorageError::TaskNotFound { id: id.to_string() }),
                Some(now_row) => Err(StorageError::TaskStateChanged {
                    expected: current.state.as_str(),
                    actual: now_row.state.as_str().to_string(),
                }),
            };
        }
        self.get(id)?
            .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })
    }

    /// 改状态。**先校验转移合法性，再写库。**
    ///
    /// 边表在 [`crate::task_state::can_transition`]（唯一定义处），本函数不重复
    /// 判断「哪些态能到哪些态」。
    ///
    /// 写库走 [`Self::transition_with`]：CAS 守卫（`WHERE id = ? AND state = ?
    /// AND version = ?`）、「影响 0 行靠再查一次区分 `TaskNotFound` 与
    /// `TaskStateChanged`」、以及校验失败不写库这三件事都只有**一份**实现。
    /// 早先这里手抄了同一段 `UPDATE` + 同一段 0 行分支，两处的守卫条件渐渐不
    /// 一样（这版没有 `version`，`transition_with` 有）——而两处守卫不一致就是
    /// 「一个入口挡得住竞态、另一个挡不住」。
    ///
    /// **本里程碑生产零调用**：`/state`、`/acceptance`、`/failure` 三个端点都走
    /// `transition_with`（它们要顺带写 `done_when`，且要 `expected_version` 那道
    /// CAS）。留着它是 `transition_with`「不动业务字段、不带 `expected_version`」
    /// 的那条路径的**独立入口**——v0.8.2 的派发器把 `pending → queued` 推上去时
    /// 正好是这个形状，且它的单测是本文件里 `transition_with` 之外唯一覆盖
    /// 「非法转移不写库」的用例。
    pub fn set_state(&self, id: &str, to: TaskState, now: &str) -> Result<TaskView> {
        self.transition_with(id, to, now, None, TaskTransition::default())
    }

    /// 一次「跨多步」的状态推进，**整个推进在同一个事务里**。
    ///
    /// 用于 `queued` 态关掉任务：规格不实现 `queued → failed` 边
    /// （`task-model.md:114-118`），但「人工在排队时直接把任务关掉」是图里列出的
    /// 终局之一（`:108-110`），于是要经 `queued → pending → failed` 两步。
    /// 两次独立 `transition_with` 会各取一条连接、各自提交，中间崩了就把任务留在
    /// `pending`——而 `pending` 是调度器（v0.8.2）会拾起的可派发态，用户刚说
    /// 「不做了」的任务反而排进了队列。
    ///
    /// 每一跳仍然是**合法边**（逐跳过 `check_transition`），不是绕过状态机直写终态。
    pub fn transition_chain(
        &self,
        id: &str,
        hops: &[TaskState],
        now: &str,
        expected_version: Option<i64>,
    ) -> Result<TaskView> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        // 入口版本在事务里读一次，整条链都按它推，避免中间被人插队。
        let entry = read_version_in(&tx, id)?;
        if let Some(v) = expected_version {
            if v != entry {
                return Err(StorageError::TaskVersionChanged {
                    expected: v,
                    actual: entry,
                });
            }
        }
        // 入口状态**必须在事务内读**。这里早先用 `self.get(id)?`，而
        // `get()` 的第一行是 `self.pool.get()?` —— 另取一条连接，WAL 下看不见
        // 本事务之外的任何未提交写，于是 `from` 可能是「别人提交前的旧态」。
        let mut from = read_state_opt_in(&tx, id)?
            .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })?;
        for (i, to) in hops.iter().enumerate() {
            crate::task_state::check_transition(from, *to)?;
            let n = tx.execute(
                "UPDATE task SET state = ?2, updated_at = ?3, version = version + 1
                  WHERE id = ?1 AND state = ?4 AND version = ?5",
                params![id, to.as_str(), now, from.as_str(), entry + i as i64],
            )?;
            if n == 0 {
                // 事务随之回滚，库上一个字节都没动。
                //
                // `actual` 同样**在事务内读**：早先走 `self.get(id)`（池外连接），
                // 读到的是事务开始前的旧态，而那时 `from` 也取自同一份旧态，
                // 结果 `actual == expected` —— 409 的 `detail.actual` 永远是错的。
                // 读不到就说明这一行在事务中途被删了，按「任务不存在」报。
                let now_state = read_state_opt_in(&tx, id)?
                    .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })?;
                return Err(StorageError::TaskStateChanged {
                    expected: from.as_str(),
                    actual: now_state.as_str().to_string(),
                });
            }
            from = *to;
        }
        tx.commit()?;
        self.get(id)?
            .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })
    }

    /// 改任务的标题 / 验收标准 / 约束。返回**新的 `version`**。
    ///
    /// 这是 [`Self::update_if_version`] 不带 `expected_version` 的薄转发。
    /// **生产零调用**（`PATCH /api/tasks/{id}` 走的是带 CAS 的那个，它要
    /// `expected_version` 挡「两个页面同时改」），但**测试大量使用**——它们是
    /// 「补丁写对了没有」这一层唯一的覆盖。删掉它就得让每个用例多写一个 `None`，
    /// 而那正是「为了少一个两行函数去改三十个调用点」的坏交易。
    ///
    /// # SQL 形状是固定的三条 `UPDATE`
    ///
    /// 按 `patch` 命中的字段各执行一条，**全部在同一事务里**，每条都带
    /// `version = version + 1`：
    ///
    /// - `title` 一条；
    /// - `done_when`（两列）一条——两列必须同进同退，拆成两条会出现
    ///   「criteria 改了、description 没改」的中间态被别的连接读到；
    /// - 四条约束列一条——它们共同描述「这个任务要什么条件」，一次改完。
    ///
    /// 为什么不用 `Option` 拼动态 SQL：拼出来的语句每次形状都不同，
    /// SQLite 那边的语句缓存全部落空，而且「哪些列会被写」这个问题
    /// 变成要读代码才知道。固定三条一眼可数。
    ///
    /// # 为什么先校验再开事务
    ///
    /// `done_when` 先过 [`DoneWhen::validate`]：**校验失败不写库、`version` 也不涨**。
    /// 「报错了但 version 涨了」比不报错更糟——用户看到「改过 N 次」里有一次
    /// 是他没改成的。放在事务外还省掉一次回滚。
    ///
    /// # 空补丁
    ///
    /// 全 `None` 的补丁不改任何列，于是 `version` 也不涨（`version` 记的是
    /// 「任务被改过几次」，没人改就不该 +1）。但要**确认任务存在**，
    /// 否则调用方会以为改成功了。
    pub fn update(&self, id: &str, patch: &TaskPatch, now: &str) -> Result<i64> {
        self.update_if_version(id, patch, now, None)
    }

    /// 带 CAS 的 [`TaskRepo::update`]：`expected_version` 为 `Some(v)` 时，
    /// **每条 `UPDATE` 都带 `AND version = ?`**，且整个补丁在同一事务里；
    /// 任何一条影响 0 行就整体回滚并返回 [`StorageError::TaskVersionChanged`]。
    ///
    /// 为什么版本条件必须在 `WHERE` 里而不是「写完再看涨了几」：涨几步与
    /// 「有没有人插队」不是同一个量（改一个字段也可能涨 1），事后判断
    /// 既拦不住 1–2 次插队，又会在已经提交之后才报 409——那时数据已被覆盖，
    /// 而 409 的语义是「什么都没发生」。
    pub fn update_if_version(
        &self,
        id: &str,
        patch: &TaskPatch,
        now: &str,
        expected_version: Option<i64>,
    ) -> Result<i64> {
        if let Some(dw) = &patch.done_when {
            // 先拒绝，再写——理由同上。
            dw.validate()?;
        }
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        // 有事可做时先记下入口版本：补丁可能命中 1~3 条 `UPDATE`，每条都按
        // 「入口版本 + 已涨次数」做条件，于是三步之间也不会被别人插进来。
        let entry = match read_version_opt_in(&tx, id)? {
            Some(v) => Some(v),
            None if patch.is_empty() => None,
            None => return Err(StorageError::TaskNotFound { id: id.to_string() }),
        };
        if let (Some(v), Some(entry)) = (expected_version, entry) {
            if v != entry {
                return Err(StorageError::TaskVersionChanged {
                    expected: v,
                    actual: entry,
                });
            }
        }

        let touched = write_patch(&tx, id, patch, now, entry)?;
        if touched == 0 {
            // 「一条都没命中」有三种来路，必须分开处置：
            //
            //   a. 空补丁 —— `version` 不涨，是**正确**结果；
            //   b. 任务不存在 —— 404；
            //   c. 任务在、补丁非空，但 `AND version = ?` 全落空 —— 有人在我们
            //      开事务之后插队，必须 409。
            //
            // 早先这里把 b 与 c 合成一条「再读一次，读到就 `Ok(v)`」，于是 c
            // 被当成成功返回，`version` 还是插队者写下的那个——调用方以为改成了，
            // 实际一个字都没写。判据不能是「版本读到了」，而是「这次要不要写东西」。
            let now_version = read_version_opt_in(&tx, id)?;
            return match now_version {
                None => Err(StorageError::TaskNotFound { id: id.to_string() }),
                Some(v) if patch.is_empty() => Ok(v),
                Some(v) => Err(StorageError::TaskVersionChanged {
                    expected: entry.unwrap_or(0) + touched as i64,
                    actual: v,
                }),
            };
        }
        if touched != patch.update_statements() {
            // 命中了但不够数：某条 `UPDATE` 落空、别的命中了，整批回滚。
            let actual = read_version_in(&tx, id)?;
            return Err(StorageError::TaskVersionChanged {
                expected: entry.unwrap_or(0) + touched as i64,
                actual,
            });
        }

        // 故意**在事务里**读回 version：提交之后再读，别人可能又改了一次，
        // 返回的就不是本次的结果了。
        let version = read_version_in(&tx, id)?;
        tx.commit()?;
        Ok(version)
    }

    /// 一个项目的任务，可只取某一态；**固定 `created_at DESC`**。
    ///
    /// 按 `state` 分列由服务层做，这里**不在 SQL 里 pivot**：pivot 出来的列名
    /// 会把六态写死在 SQL 里，加第七态就要改 SQL；而且「看板每列取哪些」
    /// 是展示决策，属于调用方。
    ///
    /// 排序固定 `created_at DESC`（与 `prototype/assets/board.js` 的卡片顺序
    /// 一致），**不按 `updated_at`**——否则改一下标题卡片就跳位置，
    /// 多用户下会看到卡片乱跳。
    pub fn list_for_project_with_state(
        &self,
        project_id: &str,
        state: Option<TaskState>,
    ) -> Result<Vec<TaskView>> {
        let conn = self.pool.get()?;
        // 两串 SQL 分开写而不是拼 `AND state = ?` 片段：两种查询各自是一次
        // 完整的语句，读的人不必在脑子里把片段拼起来才知道会跑什么。
        let raw = match state {
            None => {
                let mut stmt = conn.prepare(&format!(
                    "{SELECT_COLS} WHERE project_id = ?1 ORDER BY created_at DESC, id DESC"
                ))?;
                let rows = stmt.query_map(params![project_id], row_to_raw)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            }
            Some(s) => {
                let mut stmt = conn.prepare(&format!(
                    "{SELECT_COLS} WHERE project_id = ?1 AND state = ?2
                     ORDER BY created_at DESC, id DESC"
                ))?;
                let rows = stmt.query_map(params![project_id, s.as_str()], row_to_raw)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            }
        };
        raw.into_iter().map(RawTask::into_task).collect()
    }

    /// 一个项目里**全部 `pending` 任务**，按 `created_at` 升序（v0.8.2 子任务 3）。
    ///
    /// # 为什么不直接用 `list_for_project_with_state(project_id, Some(Pending))`
    ///
    /// 那个函数固定 `created_at DESC`——它服务的是**看板卡片顺序**（最新的在前，
    /// 与 `prototype/assets/board.js` 一致）。调度器要的是相反的顺序：同一批
    /// `pending` 任务里**先建的先派**，否则一个项目的待办会一直饿着最早的几张。
    /// 在一个本该按队列读音的路径上事后反转一次数组也能得到同样结果，但那样
    /// 「排序口径」就有两处：SQL 一处、调用方一处，改其中一处不会红。
    ///
    /// 状态过滤写在**常量**里而不是参数：本函数的契约就是「`pending` 的」，
    /// 允许传别的态会让人以为它是一份通用的按态查询（那已经有两个了）。
    pub fn list_pending_for_project(&self, project_id: &str) -> Result<Vec<TaskView>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE project_id = ?1 AND state = ?2 ORDER BY created_at ASC, id ASC"
        ))?;
        let rows = stmt.query_map(params![project_id, TaskState::Pending.as_str()], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawTask::into_task).collect()
    }
}

/// 读出这一行的当前版本号；任务不存在时返回 `None`。
///
/// 单独抽出来是因为它被用在**两处语义不同的地方**：`update_if_version` 里要区分
/// 「补丁是空的（命中 0 行但任务在）」与「任务不存在」，所以要 `Option`；
/// 而同函数末尾无条件读回本次结果时，任务一定在，要的是 `i64`。
/// （`update_if_version` 的两个调用点是 `read_version_opt_in` 的唯二使用者；
/// `TaskRepo::update` 只是它的 `None` 转发，体内没有任何版本读取。）
/// 两个 Thin wrapper 共用一条 SQL，避免这条 `SELECT version` 在文件里出现三次。
fn read_version_opt_in(conn: &r2d2_sqlite::rusqlite::Connection, id: &str) -> Result<Option<i64>> {
    let got = conn
        .query_row("SELECT version FROM task WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(got)
}

/// 同上，但调用点已确定这一行存在（读不回来说明有人在事务中途删了它）。
fn read_version_in(conn: &r2d2_sqlite::rusqlite::Connection, id: &str) -> Result<i64> {
    read_version_opt_in(conn, id)?.ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })
}

/// 事务**内**读当前状态（`None` = 这一行不存在）。
///
/// 为什么必须有这个函数：`transition_chain` 早先用 `self.get(id)` 读入口状态，
/// 而 `TaskRepo::get`（`:425` 起）自己的第一行是 `let conn = self.pool.get()?`——
/// **另取一条连接**。WAL 下未提交的写对别的连接不可见，于是事务里拿到的是
/// 「别人提交前的旧状态」：
///   · 第一跳的 `from` 与库里真实状态对不上 → 正常点「关掉」也会命中 `n == 0` / 409；
///   · 409 响应体里的 `actual` 读到的必是旧值，等于把 `expected` 原样还回去，
///     用户看到「状态从 A 变成了 A」。
/// 事务内读一律走 `&tx`（它 Deref 到 `Connection`），与 `read_version_in` 同一个口径。
fn read_state_opt_in(
    conn: &r2d2_sqlite::rusqlite::Connection,
    id: &str,
) -> Result<Option<TaskState>> {
    let got: Option<String> = conn
        .query_row("SELECT state FROM task WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?;
    // 库里存的是 `TaskState::as_str()` 的闭集字面；读不回来意味着有人绕过
    // 状态机写了别的字符串，这时宁可报错也不要猜一个默认态
    // （`FromStr` 的实现正是这条口径：不回落成 `Pending`）。
    got.map(|s| s.parse::<TaskState>()).transpose()
}

/// 四条约束列的当前值（`None` = 库里是 `NULL`，即「不限」）。
struct Constraints {
    allowed_devices: Option<String>,
    allowed_agents: Option<String>,
    required_capabilities: Option<String>,
    retry_limit: Option<i64>,
}

/// 读出四条约束列。任务不存在时返回全 `None`——调用方随后那条 `UPDATE`
/// 会因影响 0 行而报 `TaskNotFound`，这里不必先判断。
fn load_constraints(conn: &r2d2_sqlite::rusqlite::Connection, id: &str) -> Result<Constraints> {
    let got = conn
        .query_row(
            "SELECT allowed_devices, allowed_agents, required_capabilities, retry_limit
               FROM task WHERE id = ?1",
            params![id],
            |r| {
                Ok(Constraints {
                    allowed_devices: r.get(0)?,
                    allowed_agents: r.get(1)?,
                    required_capabilities: r.get(2)?,
                    retry_limit: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(got.unwrap_or(Constraints {
        allowed_devices: None,
        allowed_agents: None,
        required_capabilities: None,
        retry_limit: None,
    }))
}

/// 双层 `Option` 的**唯一**解引用点：外层 `None` 取原值，内层是什么就写什么。
///
/// 写成一份而不是四处 `match`：`Some(None)` 与 `None` 的差别只有这个函数
/// 需要知道，散在四列上迟早有一列写反（而那正是「改标题顺手清空约束」）。
fn pick<T: Clone>(patch: Option<&Option<T>>, current: Option<T>) -> Option<T> {
    match patch {
        None => current,
        Some(v) => v.clone(),
    }
}

impl Constraints {
    /// 把补丁**叠加**到当前值上，得到这一组要整批写下去的四列。
    ///
    /// 四条约束必须是**一条 `UPDATE`**（只涨一次 `version`），所以「叠什么」
    /// 与「怎么写」分开：`load_constraints` 读原值，这里算新值，调用点只剩一条
    /// 语句。四列的叠法全是 [`pick`]——谁漏了这一层，那一列就会被「这次不改」
    /// 写成 `NULL`。
    fn overlay(&self, patch: &TaskPatch) -> Constraints {
        Constraints {
            allowed_devices: pick(patch.allowed_devices.as_ref(), self.allowed_devices.clone()),
            allowed_agents: pick(patch.allowed_agents.as_ref(), self.allowed_agents.clone()),
            required_capabilities: pick(
                patch.required_capabilities.as_ref(),
                self.required_capabilities.clone(),
            ),
            retry_limit: pick(patch.retry_limit.as_ref(), self.retry_limit),
        }
    }
}

/// 把补丁写进库，返回**实际影响的行数之和**。
///
/// 三条 `UPDATE` 各自按「入口版本 + 已涨次数」做条件，所以每一条都必须用
/// **执行前**的 `touched` 算版本号、执行后累加。抽成函数是为了让这个「算版本」
/// 只有一处：写错在这里是**静默**的（后写覆盖先写、`version` 涨错步数），
/// 三份同构代码若有一份算错，测试照样绿而数据已错。
///
/// 影响行数由调用方核对——不在这里报错，因为「少一条」与「任务不存在」的
/// 处置不同（前者 409、后者 404），而这里只看得见前者。
fn write_patch(
    tx: &r2d2_sqlite::rusqlite::Transaction<'_>,
    id: &str,
    patch: &TaskPatch,
    now: &str,
    entry: Option<i64>,
) -> Result<usize> {
    let base = entry.unwrap_or(0);
    let mut touched = 0usize;
    macro_rules! bump {
        ($sql:expr, $args:expr) => {{
            let n = tx.execute($sql, $args)?;
            touched += n;
        }};
    }
    if let Some(title) = &patch.title {
        bump!(
            "UPDATE task SET title = ?2, version = version + 1, updated_at = ?3
             WHERE id = ?1 AND version = ?4",
            params![id, title, now, base + touched as i64]
        );
    }
    if let Some(dw) = &patch.done_when {
        bump!(
            "UPDATE task
                SET done_when_criteria = ?2, done_when_description = ?3,
                    version = version + 1, updated_at = ?4
              WHERE id = ?1 AND version = ?5",
            params![id, dw.criteria, dw.description, now, base + touched as i64]
        );
    }
    if patch.touches_constraints() {
        // 四条约束的赋值**各按自己那一层 Option 决定**，不能统一写成
        // `SET col = ?`——那会把「这次不改」也写成 `NULL`（清空），
        // 正是双层 `Option` 要挡住的那个 bug。做法见 [`Constraints::overlay`]。
        let next = load_constraints(tx, id)?.overlay(patch);
        bump!(
            "UPDATE task
                SET allowed_devices = ?2, allowed_agents = ?3,
                    required_capabilities = ?4, retry_limit = ?5,
                    version = version + 1, updated_at = ?6
              WHERE id = ?1 AND version = ?7",
            params![
                id,
                next.allowed_devices,
                next.allowed_agents,
                next.required_capabilities,
                next.retry_limit,
                now,
                base + touched as i64
            ]
        );
    }
    Ok(touched)
}

const SELECT_COLS: &str = "SELECT id, project_id, title, state, done_when_criteria,
        done_when_description, created_by, base_commit, created_at, updated_at,
        allowed_devices, allowed_agents, required_capabilities, retry_limit,
        version, superseded_by, intent_ref FROM task";

struct RawTask {
    id: String,
    project_id: String,
    title: String,
    state: String,
    done_when_criteria: Option<String>,
    done_when_description: Option<String>,
    created_by: Option<String>,
    base_commit: Option<String>,
    created_at: String,
    updated_at: String,
    allowed_devices: Option<String>,
    allowed_agents: Option<String>,
    required_capabilities: Option<String>,
    retry_limit: Option<i64>,
    version: i64,
    superseded_by: Option<String>,
    intent_ref: Option<String>,
}

impl RawTask {
    fn into_task(self) -> Result<TaskView> {
        Ok(TaskView {
            id: self.id,
            project_id: self.project_id,
            title: self.title,
            state: TaskState::from_str(&self.state)?,
            done_when: DoneWhen {
                criteria: self.done_when_criteria,
                description: self.done_when_description,
            },
            created_by: self.created_by,
            base_commit: self.base_commit,
            created_at: self.created_at,
            updated_at: self.updated_at,
            allowed_devices: self.allowed_devices,
            allowed_agents: self.allowed_agents,
            required_capabilities: self.required_capabilities,
            retry_limit: self.retry_limit,
            version: self.version,
            superseded_by: self.superseded_by,
            intent_ref: self.intent_ref,
        })
    }
}

fn row_to_raw(r: &Row<'_>) -> std::result::Result<RawTask, r2d2_sqlite::rusqlite::Error> {
    Ok(RawTask {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        state: r.get(3)?,
        done_when_criteria: r.get(4)?,
        done_when_description: r.get(5)?,
        created_by: r.get(6)?,
        base_commit: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
        allowed_devices: r.get(10)?,
        allowed_agents: r.get(11)?,
        required_capabilities: r.get(12)?,
        retry_limit: r.get(13)?,
        version: r.get(14)?,
        superseded_by: r.get(15)?,
        intent_ref: r.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, TaskRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(
                "INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_a', 'app', 'central', 'unix:0');
                 INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_b', 'other', 'central', 'unix:0');",
            )
            .unwrap();
        (d, TaskRepo::new(pool))
    }

    fn new_task(project: &str) -> NewTask {
        NewTask {
            created_by: Some("human".into()),
            ..NewTask::new(
                project,
                "把登录改成 OAuth",
                DoneWhen {
                    criteria: Some("all_tests_pass".into()),
                    description: Some("全部测试通过且登录可用".into()),
                },
            )
        }
    }

    #[test]
    fn create_round_trips_every_field() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        assert_eq!(t.project_id, "p_a");
        assert_eq!(t.title, "把登录改成 OAuth");
        assert_eq!(t.state, TaskState::Pending, "新任务一律 pending");
        assert_eq!(t.done_when.criteria.as_deref(), Some("all_tests_pass"));
        assert_eq!(
            t.done_when.description.as_deref(),
            Some("全部测试通过且登录可用")
        );
        assert_eq!(t.created_by.as_deref(), Some("human"));
        assert_eq!(t.created_at, TS);
        assert_eq!(t.updated_at, TS);

        let f = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(f.id, t.id);
        assert_eq!(f.done_when, t.done_when);
    }

    /// **新建恒为 `pending`**：规格里 `pending` 是「等被派活」的起点，
    /// `queued` 要求已选中 `(device, agent)`，新建时还没有这个选择。
    ///
    /// 这里给 `description` 一个值、`criteria` 留空，是**故意的**：它同时证明
    /// 「只有描述没有判定码」的任务可以建出来（`task-model.md:291` 的那种），
    /// 且起点仍是 `pending` 而不是「because 没有判定码所以别的地方」。
    #[test]
    fn a_fresh_task_starts_at_pending_not_queued() {
        let (_d, tr) = setup();
        let t = tr
            .create(
                NewTask::new(
                    "p_a",
                    "x",
                    DoneWhen {
                        criteria: None,
                        description: Some("登录可用".into()),
                    },
                ),
                TS,
            )
            .unwrap();
        assert_eq!(t.state, TaskState::Pending);
        assert_eq!(t.done_when.criteria, None);
        assert!(t.needs_manual_acceptance(), "没有判定码就只能人判");
    }

    /// 两者都空的任务**建不出来**——验收环节会彻底空转。
    ///
    /// 判红方式：把 `create` 里那句 `validate()?` 删掉，这条立刻红。
    #[test]
    fn create_refuses_a_task_with_no_done_when_at_all() {
        let (_d, tr) = setup();
        let before = tr.list_for_project_with_state("p_a", None).unwrap().len();
        let e = tr
            .create(
                NewTask::new(
                    "p_a",
                    "x",
                    DoneWhen {
                        criteria: None,
                        description: None,
                    },
                ),
                TS,
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::InvalidDoneWhenCriteria(_)),
            "{e:?}"
        );
        // 拒绝之后库里不能多出半行。
        assert_eq!(
            tr.list_for_project_with_state("p_a", None).unwrap().len(),
            before,
            "校验失败不得写库"
        );
    }

    #[test]
    fn set_state_stamps_updated_at_and_checks_existence() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        let r = tr.set_state(&t.id, TaskState::Queued, "unix:9999").unwrap();
        assert_eq!(r.state, TaskState::Queued);
        assert_eq!(r.updated_at, "unix:9999");
        assert_eq!(r.created_at, TS, "created_at 不动");

        let e = tr.set_state("nope", TaskState::Queued, TS).unwrap_err();
        assert!(matches!(e, StorageError::TaskNotFound { .. }), "{e:?}");
    }

    /// 非法转移必须**拒绝且不写库**——「报错了但写了」比不报错更糟。
    #[test]
    fn set_state_refuses_an_illegal_transition_without_writing() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        // pending → queued → running：两条合法边。
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        tr.set_state(&t.id, TaskState::Running, TS).unwrap();

        // running → done 是硬规则禁止的（跳过 review）。
        let e = tr.set_state(&t.id, TaskState::Done, TS).unwrap_err();
        assert!(
            matches!(e, StorageError::IllegalTaskTransition { .. }),
            "{e:?}"
        );

        // 关键是这一条：库里必须还是 running。
        let after = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(after.state, TaskState::Running, "拒绝后不得写库");
    }

    /// 未知 id 报 `TaskNotFound`，不是「转移非法」——两个错不能混。
    #[test]
    fn set_state_reports_task_not_found_for_an_unknown_id() {
        let (_d, tr) = setup();
        let e = tr.set_state("nope", TaskState::Queued, TS).unwrap_err();
        assert!(matches!(e, StorageError::TaskNotFound { .. }), "{e:?}");
    }

    /// CAS 挡住并发覆盖：读到旧态之后、写之前，别人把态改掉。
    ///
    /// # 这条用例以前测的不是产品代码
    ///
    /// 旧版在测试里手抄了一遍那条 `UPDATE task SET state = ?2 … WHERE id = ?1
    /// AND state = ?4`，然后断言「第二枪影响 0 行」。它证明的是「SQLite 的
    /// `WHERE` 有用」——一句 SQL 引擎的事实——而不是「`TaskRepo` 真的那样写」。
    /// 产品代码哪天把 `WHERE state = ?` 删了，这条照样绿。
    ///
    /// 现在两半都走真实路径：
    ///
    /// 1. **状态竞态**：A 已把 `running` 改成 `review`；B 手上还是 `running`
    ///    （它就是「另一个已经读过旧值的请求」），它的 `set_state` 必须报
    ///    `IllegalTaskTransition`——因为 `set_state` 在写之前重新读了真值。
    /// 2. **版本竞态**：两个请求都读到同一个 `version`，第一个写成，
    ///    第二个带 `Some(旧版本)` 进 `transition_with`，必须报
    ///    `TaskVersionChanged`（HTTP 层映射 409）。
    ///
    /// 判红方式（已实测）：把 `transition_with` 的 `AND version = ?8` 从
    /// `WHERE` 里删掉，第二段红（第二个请求静默覆盖成功）；把
    /// `set_state` 里那次 `check_transition` 删掉变成「不重读真值」，第一段红。
    #[test]
    fn a_concurrent_state_change_is_rejected() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        tr.set_state(&t.id, TaskState::Running, TS).unwrap();

        // ---- 1. 状态竞态：A 先把它推到 review ----
        tr.set_state(&t.id, TaskState::Review, TS).unwrap();
        // B 手上还是 running，于是它想走 `running → queued`（合法边）——
        // 但现在真值是 review，`review → queued` 不是合法边。`set_state`
        // 在写之前重读了真值，所以必须拒；如果它信了 B 手上的旧态，就会
        // 把一条 review 的任务硬拽回 queued。
        let e = tr.set_state(&t.id, TaskState::Queued, TS).unwrap_err();
        assert!(
            matches!(e, StorageError::IllegalTaskTransition { .. }),
            "B 手上的旧态已经过期，必须被拒（实际 {e:?}）"
        );
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Review,
            "被拒之后真实态仍是 A 写进去的 review"
        );

        // ---- 2. 版本竞态：两个请求读到同一版，第二个必须撞 409 ----
        // 这里重新取一次版本：上面第 1 段的两次写已经把它推高了，而本段要
        // 构造的是「两个请求**都读到**同一版」——A 与 B 的 `seen` 必须相同。
        let seen = tr.get(&t.id).unwrap().unwrap().version;
        // A 带着 `seen` 把 review → pending（打回）。
        tr.transition_with(
            &t.id,
            TaskState::Pending,
            TS,
            Some(seen),
            TaskTransition::default(),
        )
        .unwrap();
        // B 同样带着 `seen`——它读到的版本已经作废，但它**以为自己还在
        // review**，于是想走 `review → done`。真值已经是 `pending`，所以
        // `check_transition` 会先红——这也说明「版本守卫」在状态机之后。
        // 为了只测版本守卫，让 B 走一条**现在也合法**的边：`pending → queued`。
        // 状态机放行它，唯一能挡住的就只有 `AND version = ?`。
        let e = tr
            .transition_with(
                &t.id,
                TaskState::Queued,
                TS,
                Some(seen),
                TaskTransition::default(),
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::TaskVersionChanged { expected, .. } if expected == seen),
            "过期的 version 必须报 TaskVersionChanged（实际 {e:?}）"
        );
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Pending,
            "被拒的第二个请求不得写库"
        );

        // ---- 3. `guard_version` 用的是 `current.version`，不是 `expected` ----
        //
        // 上一段的 409 来自 Rust 侧的 `guard_version` 前置比对，**不是** SQL 里的
        // `AND version = ?8`：把 SQL 里那个条件整个去掉，第 1、2 段仍然全绿
        // （已实测）。也就是说 SQL 那道守卫只挡「`get()` 之后、`execute` 之前
        // 别人插队」这个极窄窗口，单线程测试构造不出来。
        //
        // 所以本段不去假装覆盖 SQL，而是钉住前置比对**拿对了比较对象**：
        // `expected = None` 时它必须用刚读到的 `current.version`（而不是 0、
        // 也不是上一次的 `seen`），否则「不带版本号的写」会永远 0 行。
        tr.transition_with(
            &t.id,
            TaskState::Queued,
            TS,
            None,
            TaskTransition::default(),
        )
        .expect("expected=None 时前置比对必须用 current.version，这条合法边应当写成");
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Queued,
            "不带 expected_version 的合法转移必须真的落库"
        );
    }

    /// `set_state` 成功后 `version` 必须 +1，否则 CAS 守卫形同虚设。
    #[test]
    fn a_successful_transition_bumps_the_version() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        let version_of = |id: &str| -> i64 {
            tr.pool
                .get()
                .unwrap()
                .query_row("SELECT version FROM task WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(version_of(&t.id), 1, "新建任务 version 从 1 开始");
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        assert_eq!(version_of(&t.id), 2);
    }

    #[test]
    fn list_for_project_is_scoped_and_newest_first() {
        let (_d, tr) = setup();
        let a = tr.create(new_task("p_a"), "unix:10").unwrap();
        let b = tr.create(new_task("p_a"), "unix:20").unwrap();
        tr.create(new_task("p_b"), TS).unwrap();

        let got: Vec<String> = tr
            .list_for_project_with_state("p_a", None)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(got, vec![b.id, a.id], "只含本项目，且新的在前");
    }

    /// `list_by_state`：派发候选集的入口。
    #[test]
    fn list_by_state_filters_on_the_state() {
        let (_d, tr) = setup();
        let a = tr.create(new_task("p_a"), TS).unwrap();
        let b = tr.create(new_task("p_a"), "unix:11").unwrap();
        tr.set_state(&b.id, TaskState::Queued, TS).unwrap();

        let pending = tr.list_by_state(TaskState::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, a.id);

        let queued = tr.list_by_state(TaskState::Queued).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, b.id);
    }

    /// `list_pending_for_project`：只含本项目的 `pending`，且**先建的在前**。
    ///
    /// 三条断言各自钉一件事，判红方式都能说出来：
    /// - 混进另一个项目 / 混进非 `pending` → 第一条红（「只含」）；
    /// - 排序反过来（用 `created_at DESC`）→ 第二条红（「先建的在前」）。
    ///   这一条是**与 `list_for_project_with_state` 的唯一行为差别**，没有它
    ///   两个函数就真的重复了。
    #[test]
    fn list_pending_for_project_is_ordered_oldest_first() {
        let (_d, tr) = setup();
        let older = tr.create(new_task("p_a"), "unix:10").unwrap();
        let newer = tr.create(new_task("p_a"), "unix:11").unwrap();
        // 另两个任务**故意用更早的时刻**：万一实现漏了 `state` / `project_id`
        // 过滤，它们会先出现在结果里，断言立刻红。用同样的时刻则要靠 `id` 的
        // 字典序兜底，那是不确定的。
        let other_project = tr.create(new_task("p_b"), "unix:1").unwrap();
        let already_queued = tr.create(new_task("p_a"), "unix:2").unwrap();
        tr.set_state(&already_queued.id, TaskState::Queued, TS)
            .unwrap();

        let got: Vec<String> = tr
            .list_pending_for_project("p_a")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            got,
            vec![older.id.clone(), newer.id.clone()],
            "只含 p_a 的 pending（不含 {} / {}）",
            other_project.id,
            already_queued.id
        );
        assert_eq!(got.first(), Some(&older.id), "先建的必须排在前面");
    }

    /// `is_terminal` 只对 `Done` / `Failed` 为真。
    #[test]
    fn only_done_and_failed_are_terminal() {
        assert!(TaskState::Done.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        for s in [
            TaskState::Pending,
            TaskState::Queued,
            TaskState::Running,
            TaskState::Review,
        ] {
            assert!(!s.is_terminal(), "{s:?} 不该是终态");
        }
    }

    /// 库里的非法状态必须报错，**不回落 `Pending`**——回落会让一个已结束的任务
    /// 显示成待办，用户会以为它从没跑过。
    #[test]
    fn an_unknown_state_in_the_database_is_an_error_not_a_default() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.pool
            .get()
            .unwrap()
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE task SET state = 'sorta_done';",
            )
            .unwrap();
        let e = tr.get(&t.id).unwrap_err();
        assert!(matches!(e, StorageError::InvalidTaskState(_)), "{e:?}");
    }

    #[test]
    fn every_state_round_trips_through_its_literal() {
        for s in [
            TaskState::Pending,
            TaskState::Queued,
            TaskState::Running,
            TaskState::Review,
            TaskState::Done,
            TaskState::Failed,
        ] {
            assert_eq!(TaskState::from_str(s.as_str()).unwrap(), s);
        }
    }

    /// 迁移之后再出现旧态必须报错，**不回落 `pending`**——静默回落会让
    /// 「有代码写回了旧值」这件事藏起来，直到用户看见一个状态不对的任务。
    #[test]
    fn legacy_states_are_rejected_after_v7() {
        for legacy in ["draft", "ready", "blocked"] {
            let e = TaskState::from_str(legacy).unwrap_err();
            assert!(
                matches!(e, StorageError::InvalidTaskState(ref s) if s == legacy),
                "{legacy} 应当报错，实际 {e:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // 子任务 3：`done_when` 判定码闭集与「需人工验收」
    // ---------------------------------------------------------------------

    fn done_when(criteria: Option<&str>, description: Option<&str>) -> DoneWhen {
        DoneWhen {
            criteria: criteria.map(Into::into),
            description: description.map(Into::into),
        }
    }

    /// 只有 `done_when` 关心的 `Task`，其余字段填死值。
    /// 只把 `done_when` 包成一个能问 `needs_manual_acceptance` 的读模型。
    ///
    /// 判定规则只吃 `done_when`，其余字段与这条规则无关；早先这里构造的是
    /// 已删除的 `Task` 写模型。
    fn task_with(dw: DoneWhen) -> TaskView {
        TaskView {
            id: "t_1".into(),
            project_id: "p_a".into(),
            title: "x".into(),
            state: TaskState::Pending,
            done_when: dw,
            created_by: None,
            base_commit: None,
            created_at: TS.into(),
            updated_at: TS.into(),
            allowed_devices: None,
            allowed_agents: None,
            required_capabilities: None,
            retry_limit: None,
            version: 1,
            superseded_by: None,
            intent_ref: None,
        }
    }

    /// 判定码与描述**不能同时为空**——那是一个连人都不知道怎么算完成的任务。
    ///
    /// 判红方式：把 `validate` 里那段 `criteria_blank && description_blank`
    /// 的短路删掉，这条立刻红。
    #[test]
    fn criteria_and_description_cannot_both_be_empty() {
        for (c, d) in [
            (None, None),
            (Some(""), None),
            (None, Some("")),
            (Some(""), Some("")),
        ] {
            let dw = done_when(c, d);
            let e = dw
                .validate()
                .expect_err("两者都空必须被拒绝：验收环节会彻底空转");
            assert!(
                matches!(e, StorageError::InvalidDoneWhenCriteria(_)),
                "{e:?}"
            );
        }
        // 有一边非空就合法，两种各测一遍。
        assert!(done_when(Some("all_tests_pass"), None).validate().is_ok());
        assert!(done_when(None, Some("登录可用")).validate().is_ok());
    }

    /// 四个闭集值逐个必须被接受。
    ///
    /// 判红方式：闭集里写错一个码名（如把 `all_tests_pass` 写成
    /// `all_test_pass`），表驱动里那一行立刻红；或 `validate` 改成多拒一个码。
    #[test]
    fn every_whitelisted_code_is_accepted() {
        for code in CRITERIA_CODES {
            let dw = done_when(Some(code), None);
            let verdict = dw.validate();
            assert!(
                verdict.is_ok(),
                "{code} 是闭集内的码，必须被接受：{verdict:?}"
            );
        }
        // 顺带钉住闭集的**确切内容与顺序**：闭集是公开契约（前端下拉框、
        // API 的 `detail.allowed` 都按它渲染），不是实现细节。
        assert_eq!(
            CRITERIA_CODES,
            [
                "all_tests_pass",
                "build_passes",
                "review_approved",
                "command_exit_zero"
            ]
        );
    }

    /// 闭集外的值必须被拒绝，且错误里带**允许集合**——人得知道能填什么。
    ///
    /// 判红方式：把 `validate` 改成永远 `Ok`（放行自由文本），这条立刻红。
    #[test]
    fn an_unknown_criteria_code_is_rejected() {
        for bad in ["all_tests_pass_v2", "looks_good", "all test pass"] {
            let dw = done_when(Some(bad), Some("人话描述"));
            let e = dw
                .validate()
                .expect_err(&format!("{bad} 不在闭集里，必须被拒绝"));
            let msg = e.to_string();
            assert!(
                matches!(e, StorageError::InvalidDoneWhenCriteria(ref s) if s == bad),
                "错误要带上是哪串值被拒了，实际 {e:?}"
            );
            // 错误信息里必须列全四个允许值，否则用户改不对。
            for allowed in CRITERIA_CODES {
                assert!(msg.contains(allowed), "错误信息缺 {allowed}：{msg}");
            }
        }
    }

    /// 大小写敏感：`ALL_TESTS_PASS` 不是 `all_tests_pass`。
    ///
    /// 判红方式：把 `is_known_criteria_code` 改成 `eq_ignore_ascii_case`（或
    /// 先把 code 转小写），这条立刻红。为什么必须敏感：判定码会落库、被统计、
    /// 被前端按字面值匹配，容忍大小写等于让同一个码有多种写法。
    #[test]
    fn criteria_codes_are_case_sensitive() {
        for code in CRITERIA_CODES {
            let upper = code.to_uppercase();
            assert_ne!(upper, code, "前提：{code} 有大写形态可测");
            let dw = done_when(Some(&upper), Some("人话描述"));
            let e = dw.validate().expect_err(&format!("{upper} 必须被拒绝"));
            assert!(
                matches!(e, StorageError::InvalidDoneWhenCriteria(_)),
                "{e:?}"
            );
            // 反向确认：同一串只要大小写正确就通过——证明拒绝的原因是大小写，
            // 不是别的东西（比如长度）。
            assert!(done_when(Some(code), None).validate().is_ok());
        }
    }

    /// `criteria = None` 必须被接受：那是「需人工验收」这条产品能力的入口。
    ///
    /// 判红方式：把 `validate` 的 `None` 分支改成 `Err`，这条立刻红。
    #[test]
    fn a_missing_criteria_is_accepted() {
        assert!(done_when(None, Some("人话描述")).validate().is_ok());
        // 空串同 `None`：前端下拉框选「不设判定码」很可能送上来 `""`。
        assert!(done_when(Some(""), Some("人话描述")).validate().is_ok());
    }

    /// 「需人工验收」的**真**侧：没有判定码，就只有人能判。
    ///
    /// 规格 `task-model.md:290-292`：「只有 `description` 没有 `criteria` 的
    /// 任务，卡片上必须标『需人工验收』」。
    ///
    /// 判红方式：把 `needs_manual_acceptance` 写成恒 `false`（或让 `None` 走
    /// 「有描述就算自动」的分支），这条立刻红。
    #[test]
    fn only_description_needs_manual_acceptance() {
        let t = task_with(done_when(None, Some("登录可用")));
        assert!(
            t.needs_manual_acceptance(),
            "没有判定码就只能人判，卡片必须标「需人工验收」"
        );
        // 空串也是「没填」。
        assert!(task_with(done_when(Some(""), Some("登录可用"))).needs_manual_acceptance());
        // 两者都空同样是「没人能自动判」——不该显示成「自动判定中」。
        assert!(task_with(done_when(None, None)).needs_manual_acceptance());
    }

    /// 「需人工验收」的**假**侧：有机器判得了的判定码就不打扰人。
    ///
    /// 判红方式：把 `needs_manual_acceptance` 写成恒 `true`，这条立刻红。
    #[test]
    fn a_machine_judgeable_code_does_not_need_manual_acceptance() {
        for code in ["all_tests_pass", "build_passes", "command_exit_zero"] {
            let t = task_with(done_when(Some(code), Some("人话描述")));
            assert!(
                !t.needs_manual_acceptance(),
                "{code} 是机器判得了的，不该标人工验收"
            );
            // 没有描述也一样：自动判定不依赖 `description`。
            assert!(!task_with(done_when(Some(code), None)).needs_manual_acceptance());
        }
    }

    /// `review_approved` 在闭集里，但**机器判不了**：它的判定就是「评审通过」，
    /// 离不开人（或模型），而规格要求 `criteria` 的判定「不依赖模型」
    /// （`butler.md` §2.2.0）。所以它必须标「需人工验收」。
    ///
    /// 判红方式：把 `review_approved` 从 `needs_manual_acceptance` 的 `true`
    /// 分支删掉（让它落进「闭集内即自动」），这条立刻红。
    #[test]
    fn review_approved_is_in_the_closed_set_but_still_needs_a_human() {
        let t = task_with(done_when(Some("review_approved"), Some("评审通过即算完")));
        assert!(is_known_criteria_code("review_approved"), "它在闭集里");
        assert!(t.done_when.validate().is_ok(), "它是合法值，不能被拒");
        assert!(
            t.needs_manual_acceptance(),
            "评审通过是人/模型的判断，不能假装有个自动判定在跑"
        );
    }

    /// 存量数据兜底：闭集外的值当作「判不了」，不许 panic、也不许当成能自动判。
    ///
    /// 判红方式：让 `needs_manual_acceptance` 的 `Some(_) => false` 兜底，这条立刻红。
    #[test]
    fn a_legacy_unknown_code_is_not_treated_as_auto_judgeable() {
        let t = task_with(done_when(Some("looks_good"), Some("旧值")));
        assert!(!is_known_criteria_code("looks_good"));
        assert!(t.needs_manual_acceptance(), "不认识怎么判，就得人来判");
    }

    /// 校验真的挡在写库之前：非法判定码进来了，库里不能多出一行。
    ///
    /// 判红方式：删掉 `TaskRepo::create` 里那句 `validate()`，这条立刻红
    /// （会 `create` 成功并在库里留下 `looks_good`）。
    #[test]
    fn create_refuses_an_unknown_code_and_writes_nothing() {
        let (_d, tr) = setup();
        let mut bad = new_task("p_a");
        bad.done_when.criteria = Some("all_tests_pass_v2".into());
        let e = tr.create(bad, TS).unwrap_err();
        assert!(
            matches!(e, StorageError::InvalidDoneWhenCriteria(ref s) if s == "all_tests_pass_v2"),
            "{e:?}"
        );
        assert!(
            tr.list_for_project_with_state("p_a", None)
                .unwrap()
                .is_empty(),
            "被拒的任务不得落库"
        );
    }

    // ---------------------------------------------------------------------
    // 子任务 5：`TaskPatch` / `update` / `list_for_project_with_state`
    // ---------------------------------------------------------------------

    /// 直接读库里那四列约束的原始值。
    ///
    /// 为什么不经 `Task`：`Task` 结构体**还没有**这四个字段（本里程碑只加了
    /// 列，没扩读模型），所以「列到底有没有被写」只能查 SQL。
    /// 这正是这几条用例要钉的东西，用原始值断言反而更直接。
    fn raw_constraints(
        tr: &TaskRepo,
        id: &str,
    ) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
        tr.pool
            .get()
            .unwrap()
            .query_row(
                "SELECT allowed_devices, allowed_agents, required_capabilities, retry_limit
                   FROM task WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    fn raw_version(tr: &TaskRepo, id: &str) -> i64 {
        tr.pool
            .get()
            .unwrap()
            .query_row("SELECT version FROM task WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    /// 带全套约束的任务（四条约束都非 NULL，好让「有没有被清空」看得出来）。
    fn task_with_all_constraints(tr: &TaskRepo) -> TaskView {
        let t = tr.create(new_task("p_a"), TS).unwrap();
        let v = tr
            .update(
                &t.id,
                &TaskPatch {
                    allowed_devices: Some(Some("[\"n_1\"]".into())),
                    allowed_agents: Some(Some("[\"opencode\"]".into())),
                    required_capabilities: Some(Some("{\"arch\":\"x86_64\"}".into())),
                    retry_limit: Some(Some(2)),
                    ..TaskPatch::default()
                },
                "unix:100",
            )
            .unwrap();
        assert_eq!(v, 2, "前提：一次改四条约束只涨一次 version");
        tr.get(&t.id).unwrap().unwrap()
    }

    /// 只给 `title` 的补丁**只能动 `title`**，其余字段逐字段保持原值。
    ///
    /// `transition_with` **同时**改状态与 `done_when`，且版本只涨一次。
    ///
    /// 判红方式：把那条 `UPDATE` 里 `CASE WHEN ?3 = 1` 的两个分支任一改成
    /// 「总写」或「总不写」 → 这条立刻红（`done_when` 与状态会有一个不对）。
    #[test]
    fn transition_with_writes_state_and_done_when_in_one_step() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        tr.set_state(&t.id, TaskState::Running, TS).unwrap();

        let before = tr.get(&t.id).unwrap().unwrap();
        let r = tr
            .transition_with(
                &t.id,
                TaskState::Review,
                "unix:200",
                None,
                TaskTransition {
                    done_when: Some(DoneWhen {
                        criteria: Some("build_passes".into()),
                        description: None,
                    }),
                },
            )
            .unwrap();
        assert_eq!(r.state, TaskState::Review);
        assert_eq!(r.done_when.criteria.as_deref(), Some("build_passes"));
        assert_eq!(
            r.version,
            before.version + 1,
            "状态与 done_when 同一条 UPDATE，版本只能涨一次"
        );
    }

    /// `transition_with` 的**第二道 CAS**：`expected_version` 不符就报
    /// `TaskVersionChanged`（带真实的版本号，不是状态名）且**不写库**。
    ///
    /// 判红方式：把 `guard_version` 那段删掉（让 `WHERE version = ?` 永远用刚读到
    /// 的值），这条立刻红——一个过期的版本会把别人的改动静默覆盖掉。
    #[test]
    fn transition_with_refuses_a_stale_expected_version() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        tr.set_state(&t.id, TaskState::Running, TS).unwrap();
        let stale = tr.get(&t.id).unwrap().unwrap().version;

        // 别人先改了一次（版本前进）。
        tr.set_state(&t.id, TaskState::Review, TS).unwrap();
        let fresh = tr.get(&t.id).unwrap().unwrap().version;
        assert!(fresh > stale, "前提：版本确实前进了");

        let e = tr
            .transition_with(
                &t.id,
                TaskState::Pending,
                TS,
                Some(stale),
                TaskTransition::default(),
            )
            .unwrap_err();
        // 报的是**版本号**，而且两个数字都与真实值对得上——早先这里报的是
        // `TaskStateChanged`，两个字段填的是同一个状态名，客户端无从判断。
        match e {
            StorageError::TaskVersionChanged { expected, actual } => {
                assert_eq!(expected, stale, "expected 必须是调用方给的那一版");
                assert_eq!(actual, fresh, "actual 必须是库里真实的那一版");
            }
            other => panic!("过期版本必须报 TaskVersionChanged，实际 {other:?}"),
        }
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Review,
            "被拒的转移不得写库"
        );
    }

    /// 建任务时**一次写入约束**，不靠「建完再补写一次」——后者会让刚建好的任务
    /// 显示「改过 2 次」。
    ///
    /// 判红方式：把 `create_task` 改回「建完再 `update` 一次约束」，这条立刻红
    /// （`version` 变成 2）。
    #[test]
    fn create_writes_constraints_without_bumping_the_version() {
        let (_d, tr) = setup();
        let t = tr
            .create(
                NewTask {
                    allowed_devices: Some("[\"n_1\"]".into()),
                    retry_limit: Some(3),
                    ..new_task("p_a")
                },
                TS,
            )
            .unwrap();
        assert_eq!(t.version, 1, "建任务不是一次修改，version 必须还是 1");
        assert_eq!(t.allowed_devices.as_deref(), Some("[\"n_1\"]"));
        assert_eq!(t.retry_limit, Some(3));
        assert_eq!(t.allowed_agents, None);
    }

    /// 判红方式：把 `update` 写成「全字段覆盖」（没给的填 `NULL`），这条立刻红
    /// ——`done_when`、四条约束与 `retry_limit` 都会变成 `None`。
    #[test]
    fn update_only_touches_the_fields_given() {
        let (_d, tr) = setup();
        let before = task_with_all_constraints(&tr);
        let constraints_before = raw_constraints(&tr, &before.id);

        tr.update(
            &before.id,
            &TaskPatch {
                title: Some("改成 OIDC".into()),
                ..TaskPatch::default()
            },
            "unix:200",
        )
        .unwrap();

        let after = tr.get(&before.id).unwrap().unwrap();
        assert_eq!(after.title, "改成 OIDC", "给了的字段要改");
        // 逐字段断言：一条 `assert_eq!(after, before)` 说不清是哪一列被误改了。
        assert_eq!(after.done_when, before.done_when, "done_when 没给，不得动");
        assert_eq!(after.done_when.criteria.as_deref(), Some("all_tests_pass"));
        assert_eq!(
            after.done_when.description.as_deref(),
            Some("全部测试通过且登录可用")
        );
        assert_eq!(
            raw_constraints(&tr, &before.id),
            constraints_before,
            "四条约束没给，不得动（尤其不得被写成 NULL）"
        );
        assert_eq!(after.project_id, before.project_id);
        assert_eq!(after.created_at, before.created_at, "created_at 不动");
    }

    /// 一次 `update` 让 `version` **恰好 +1**。
    ///
    /// 连调两次 → 从 1 到 3（不是 2：那就说明有一次没涨；也不是 4：那说明
    /// 某次涨了两格）。
    ///
    /// 判红方式：把某条 `UPDATE` 里的 `version = version + 1` 删掉（少涨），
    /// 或把三条 `UPDATE` 的更新写成两条会同时命中（多涨），这条立刻红。
    #[test]
    fn update_bumps_version_once_per_call() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        assert_eq!(raw_version(&tr, &t.id), 1, "新建任务 version 从 1 开始");

        let v = tr
            .update(
                &t.id,
                &TaskPatch {
                    title: Some("第一次改".into()),
                    ..TaskPatch::default()
                },
                "unix:100",
            )
            .unwrap();
        assert_eq!(v, 2, "返回值就是新 version");

        let v = tr
            .update(
                &t.id,
                &TaskPatch {
                    title: Some("第二次改".into()),
                    ..TaskPatch::default()
                },
                "unix:200",
            )
            .unwrap();
        assert_eq!(v, 3);
        assert_eq!(raw_version(&tr, &t.id), 3);
    }

    /// `transition_chain` 的两处「读」都必须在**事务内**，不得走池外连接。
    ///
    /// 这条守的是一个曾经真实存在的缺陷：函数体里 `conn.transaction()` 之后
    /// 用 `self.get(id)` 读入口状态，而 `TaskRepo::get` 自己的第一行是
    /// `self.pool.get()?` —— **另取一条连接**。WAL 下事务外的连接看不见本事务
    /// 的未提交写，于是：
    ///
    ///   ① `from` 拿到的是别人提交前的旧态（`expected` 错）；
    ///   ② `n == 0` 分支里的 `actual` 也走同一条池外连接，而那时 `from` 与它
    ///      取自同一份旧快照，结果 **`actual == expected`** —— 409 的
    ///      `detail.actual` 永远是错的（页面会显示「状态从 X 变成 X」）。
    ///
    /// 复现手法与 `set_state_distinguishes_…` 同一个：`BEFORE UPDATE` 触发器在
    /// 写之前把库改掉，模拟「另一个人在这几微秒里提交了」。不用两个线程真跑
    /// 竞态——那样测出来的是「有时红有时绿」。
    ///
    /// 判红方式：
    ///   · 把 `read_state_opt_in(&tx, id)` 改回 `self.get(id)?…state`
    ///     → ② 的 `actual` 断言红（拿到旧态 `pending`，而真值是 `review`）；
    ///   · 把 `:636` 的 `read_state_opt_in(&tx, …)` 改回 `self.get(id)`
    ///     → 同样红。
    ///   两种改法都能被这条用例抓住，因为断言钉的是「`actual` 必须是库里的真值
    ///   且不等于 `expected`」——池外连接永远给不出这个结果。
    #[test]
    fn transition_chain_reads_state_inside_the_transaction() {
        let (_d, tr) = setup();
        let pool = tr.pool.clone();

        // 起点：queued。链的第一跳 `queued → pending` 会因为态被抢改而落空。
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();

        // 把 CAS 的条件列 version 也一起改掉，确保外层 UPDATE 影响 0 行：
        // 只改 state 时，若夹具恰好也命中 `state = from` 就不会走到 `n == 0`。
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER race_before_update BEFORE UPDATE ON task
                   WHEN NEW.id = '{}'
                 BEGIN
                   UPDATE task SET state = 'review', version = version + 100
                    WHERE id = '{}';
                   SELECT RAISE(IGNORE);
                 END;",
                t.id, t.id
            ))
            .unwrap();

        let e = tr
            .transition_chain(&t.id, &[TaskState::Pending, TaskState::Failed], TS, None)
            .unwrap_err();
        let StorageError::TaskStateChanged { expected, actual } = e else {
            panic!("态被抢改必须报 TaskStateChanged，实际 {e:?}");
        };

        // 核心断言：`actual` 是**事务内**读到的真值，不是旧快照。
        assert_eq!(
            actual, "review",
            "409 的 actual 必须在事务内读——取池外连接会拿到旧态 `pending`，\
             于是 detail.actual 等于 expected，前端显示「状态从 pending 变成 pending」"
        );
        assert_ne!(
            actual, expected,
            "actual 与 expected 相等说明读到的是同一份旧快照（池外连接）"
        );

        // 整条链回滚：库里仍是 `queued`。
        //
        // 注意这里**不能**断言 `review`：触发器的写入发生在同一个事务里，
        // 事务回滚会把它一起撤销。`set_state` 那组用例能断言 `review`，是因为
        // 它的 CAS 失败后没有再回滚外层事务。这个差别本身就是证据——说明
        // `transition_chain` 确实整体在一个事务里。
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Queued,
            "被拒的链必须整批回滚（连触发器在同一事务里写的改动也一并撤销）"
        );
        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER race_before_update;")
            .unwrap();
    }

    /// `transition_chain` 的**三条拒绝路径**都不写库。
    ///
    /// 一条链要经 `queued → pending → failed`（规格不实现 `queued → failed` 边）。
    /// 拒绝有三处：入口版本不符、任务不存在、某一跳不是合法边。任何一处都必须
    /// 整批回滚——留下半条链会把任务卡在 `pending`，那是调度器会拾起的可派发态。
    ///
    /// 判红方式：把 `tx` 改成每条 `UPDATE` 自己提交（或去掉 `AND state/version`
    /// 守卫），成功路径之外的某一条会留下痕迹而红。
    #[test]
    fn transition_chain_refuses_every_bad_path_without_writing() {
        let (_d, tr) = setup();

        // ① 版本不符。
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        let v = tr.get(&t.id).unwrap().unwrap().version;
        let e = tr
            .transition_chain(
                &t.id,
                &[TaskState::Pending, TaskState::Failed],
                TS,
                Some(v - 1),
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::TaskVersionChanged { .. }),
            "版本不符必须报 TaskVersionChanged：{e:?}"
        );
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Queued,
            "被拒的链不得改状态"
        );

        // ② 任务不存在。
        let e = tr
            .transition_chain("t_missing", &[TaskState::Pending], TS, None)
            .unwrap_err();
        assert!(matches!(e, StorageError::TaskNotFound { .. }), "{e:?}");

        // ③ 某一跳不是合法边（`queued → review` 不在边表里）。
        let e = tr
            .transition_chain(&t.id, &[TaskState::Review], TS, None)
            .unwrap_err();
        assert!(
            matches!(e, StorageError::IllegalTaskTransition { .. }),
            "非法边必须报 IllegalTaskTransition：{e:?}"
        );
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().state,
            TaskState::Queued,
            "非法链不得改状态"
        );

        // ④ 正常路径：两跳都落地，版本涨两次。
        let v0 = tr.get(&t.id).unwrap().unwrap().version;
        let after = tr
            .transition_chain(&t.id, &[TaskState::Pending, TaskState::Failed], TS, None)
            .unwrap();
        assert_eq!(after.state, TaskState::Failed);
        assert_eq!(after.version, v0 + 2, "两跳各涨一次版本");
    }

    /// `set_state` 的 CAS 影响 0 行时**必须再读一次**区分两种原因。
    ///
    /// 这条覆盖的是 `n == 0` 的两支：任务在读到写入之间被删（`TaskNotFound`），
    /// 或态被改（`TaskStateChanged`）。用 `BEFORE UPDATE` 触发器在写之前
    /// 把库改掉——模拟的正是「另一个人在这几微秒里提交了」。
    ///
    /// 为什么不用两个线程真跑竞态：那样测出来的是「有时红有时绿」。
    ///
    /// 判红方式：把 `n == 0` 那段改成直接 `return Err(TaskStateChanged{..})`
    /// （不再查一次），第 ① 条的 `TaskNotFound` 断言红——那正是「把『没了』
    /// 说成『被人改了』」的早期实现。
    #[test]
    fn set_state_distinguishes_a_deleted_task_from_a_raced_state() {
        let (_d, tr) = setup();
        let pool = tr.pool.clone();

        // ① 读到之后、写之前任务被删 → TaskNotFound。
        let t = tr.create(new_task("p_a"), TS).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER del_before_update BEFORE UPDATE ON task
                   WHEN NEW.id = '{}'
                 BEGIN DELETE FROM task WHERE id = '{}'; END;",
                t.id, t.id
            ))
            .unwrap();
        let e = tr.set_state(&t.id, TaskState::Queued, TS).unwrap_err();
        assert!(
            matches!(e, StorageError::TaskNotFound { .. }),
            "任务没了要报 TaskNotFound（报 TaskStateChanged 会让页面说「别人改了」，\
             而真相是它已经不存在）：{e:?}"
        );
        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER del_before_update;")
            .unwrap();

        // ② 读到之后、写之前态被别人改掉 → TaskStateChanged，且 actual 是真值。
        //
        // 触发器先把态改成 review，再用 `RAISE(IGNORE)` 跳过本行的外层 UPDATE
        // ——`WHERE` 早已判过，只有这样才能让外层「影响 0 行」而态确实变了。
        let t2 = tr.create(new_task("p_a"), TS).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER move_before_update BEFORE UPDATE ON task
                   WHEN NEW.id = '{}'
                 BEGIN
                   UPDATE task SET state = 'review' WHERE id = '{}';
                   SELECT RAISE(IGNORE);
                 END;",
                t2.id, t2.id
            ))
            .unwrap();
        let e = tr.set_state(&t2.id, TaskState::Queued, TS).unwrap_err();
        let StorageError::TaskStateChanged { expected, actual } = e else {
            panic!("态被抢改必须报 TaskStateChanged，实际 {e:?}");
        };
        assert_eq!(expected, "pending", "expected 是我们读到的那个态");
        assert_eq!(actual, "review", "actual 必须是库里现在的真值");
        assert_eq!(
            tr.get(&t2.id).unwrap().unwrap().state,
            TaskState::Review,
            "被拒的转移不得写库（态仍是抢改者写进去的 review）"
        );
        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER move_before_update;")
            .unwrap();
    }

    /// `transition_with` 的 CAS 同样区分「没了」与「被抢改」。
    ///
    /// 与 [`set_state_distinguishes_a_deleted_task_from_a_raced_state`] 同一
    /// 判据，但走的是另一条写路径（`transition_with` 带 `version = ?`）。
    /// 两条路径各有一份 `n == 0` 的再读逻辑——修一处漏另一处是本项目反复
    /// 出现的缺陷形状，所以两条都要有负样本。
    ///
    /// 判红方式：把 `transition_with` 里 `n == 0` 的再读删掉、直接
    /// `Err(TaskStateChanged)`，第 ① 条红。
    #[test]
    fn transition_with_distinguishes_a_deleted_task_from_a_raced_state() {
        let (_d, tr) = setup();
        let pool = tr.pool.clone();

        let t = tr.create(new_task("p_a"), TS).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER del_before_update2 BEFORE UPDATE ON task
                   WHEN NEW.id = '{}'
                 BEGIN DELETE FROM task WHERE id = '{}'; END;",
                t.id, t.id
            ))
            .unwrap();
        let e = tr
            .transition_with(
                &t.id,
                TaskState::Queued,
                TS,
                None,
                TaskTransition::default(),
            )
            .unwrap_err();
        assert!(matches!(e, StorageError::TaskNotFound { .. }), "{e:?}");
        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER del_before_update2;")
            .unwrap();

        let t2 = tr.create(new_task("p_a"), TS).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER move_before_update2 BEFORE UPDATE ON task
                   WHEN NEW.id = '{}'
                 BEGIN
                   UPDATE task SET state = 'review' WHERE id = '{}';
                   SELECT RAISE(IGNORE);
                 END;",
                t2.id, t2.id
            ))
            .unwrap();
        let e = tr
            .transition_with(
                &t2.id,
                TaskState::Queued,
                TS,
                None,
                TaskTransition::default(),
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::TaskStateChanged { ref actual, .. } if actual == "review"),
            "actual 必须是库里现在的真值：{e:?}"
        );
        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER move_before_update2;")
            .unwrap();
    }

    /// `update_if_version` 的 CAS **在 SQL 里**：过期的 `expected_version` 一行都改不动。
    ///
    /// 这条必须打在仓储层，不能在 HTTP 层打：`patch_task` 自己先调
    /// `require_version`，所以走端点永远到不了这里——用端点测，把仓储的
    /// `AND version = ?` 全删掉也照样绿（本人实测过）。
    ///
    /// 判红方式：把函数开头 `expected_version != entry` 那段预检删掉，本用例红
    /// （过期写被接受、`title` 被覆盖成「我的」且返回 `Ok`）。
    ///
    /// **注意这条只测到预检**：入口版本与库一致时预检就会放行，所以「库在我读完
    /// 之后、写之前被别人改了」这种情况它构造不出来。那一层由
    /// `update_if_version_cas_lives_in_the_sql_not_only_in_the_precheck` 用触发器打。
    #[test]
    fn update_if_version_refuses_a_stale_version_without_writing() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        assert_eq!(raw_version(&tr, &t.id), 1);

        // 别人先改一次，版本走到 2。
        tr.update_if_version(
            &t.id,
            &TaskPatch {
                title: Some("插队者的".into()),
                ..TaskPatch::default()
            },
            "unix:100",
            Some(1),
        )
        .unwrap();
        assert_eq!(raw_version(&tr, &t.id), 2);

        // 我拿过期的 1 再改：必须报 TaskVersionChanged，且标题不变。
        let e = tr
            .update_if_version(
                &t.id,
                &TaskPatch {
                    title: Some("我的".into()),
                    ..TaskPatch::default()
                },
                "unix:200",
                Some(1),
            )
            .unwrap_err();
        match e {
            StorageError::TaskVersionChanged { expected, actual } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 2);
            }
            other => panic!("过期版本必须报 TaskVersionChanged，实际 {other:?}"),
        }
        let title = |tr: &TaskRepo| -> String { tr.get(&t.id).unwrap().unwrap().title };
        assert_eq!(title(&tr), "插队者的", "过期版本的写不得覆盖别人的改动");
        assert_eq!(raw_version(&tr, &t.id), 2, "被拒的写不得涨版本");
    }

    /// `update_if_version` 的 CAS **真的在 SQL 里**：入口预检放行之后、`UPDATE`
    /// 执行之前，库被别人改了版本 → 那条 `UPDATE` 影响 0 行 → 必须报
    /// `TaskVersionChanged` 且什么都不写。
    ///
    /// # 为什么这条必须单独存在
    ///
    /// SQL 里的 `AND version = ?` 在**正常的单线程流程里永远拦不到东西**：函数
    /// 一进来就 `SELECT version` 并做预检，两条语句之间没有别人的位置。于是
    /// 把三处 `AND version = ?` 全换成恒真，既有用例**一条都不会红**（本人实测：
    /// 261 passed / 0 failed）——这正是「注释声称的判红方式不成立」。
    ///
    /// # 怎么构造那个窗口
    ///
    /// 用 `BEFORE UPDATE` 触发器：外层 `UPDATE` 进来时先把 `version` 改掉，再
    /// `SELECT RAISE(IGNORE)` 让外层那一行的写入被跳过。于是「读到版本」与
    /// 「用版本」之间就真的隔了一次别人的修改，而且是**确定性的**，不靠双线程。
    ///
    /// 判红方式（逐条实测）：
    /// 1. 把 `title` 那条的 `AND version = ?` 换成恒真、并把
    ///    `touched != patch.update_statements()` 改成永假 → **本用例红**
    ///    （返回 `Ok`，`title` 被覆盖）；
    /// 2. 只删 `AND version = ?` 而留着 `touched != patch.update_statements()`
    ///    → **仍绿**：命中 0 行本来就不会被算成成功。两道都在是**双层**，不是假绿；
    /// 3. 只删 `touched != patch.update_statements()` 而留着 `AND version = ?`
    ///    → **仍绿**：那条 `UPDATE` 本来就改不动。故判红必须同时拆两道。
    #[test]
    fn update_if_version_cas_lives_in_the_sql_not_only_in_the_precheck() {
        let (_d, tr) = setup();
        let pool = tr.pool.clone();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        assert_eq!(raw_version(&tr, &t.id), 1);

        // 触发器在 UPDATE 之前把 version 从 1 推到 7，再 IGNORE 掉这一行的写入。
        // `WHEN NEW.version = 2` 只匹配「入口版本 1、涨一步」的那条语句，避免
        // 触发器把自己的内部 UPDATE 也再次触发（SQLite 默认不重入，但仍写明）。
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER bump_before_update BEFORE UPDATE ON task
                   WHEN NEW.id = '{}' AND NEW.version = 2
                 BEGIN
                   UPDATE task SET version = 7 WHERE id = '{}';
                   SELECT RAISE(IGNORE);
                 END;",
                t.id, t.id
            ))
            .unwrap();

        let e = tr
            .update_if_version(
                &t.id,
                &TaskPatch {
                    title: Some("我的".into()),
                    ..TaskPatch::default()
                },
                "unix:200",
                Some(1),
            )
            .unwrap_err();
        // `actual` 报的是**读到的那一刻**库里的真值（触发器写的 7），而不是
        // 回滚之后的 1：调用方要据此知道「该刷新成几」，回滚掉的是本次事务。
        assert!(
            matches!(
                e,
                StorageError::TaskVersionChanged {
                    expected: 1,
                    actual: 7
                }
            ),
            "版本在写的瞬间变了：必须报 TaskVersionChanged{{1,7}}，实际 {e:?}"
        );
        // 整个事务（连触发器的写）一起回滚，库里回到事务前的样子。
        assert_eq!(raw_version(&tr, &t.id), 1, "整批回滚：版本回到事务前");
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().title,
            "把登录改成 OAuth",
            "被拒的写不得改标题"
        );

        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER bump_before_update;")
            .unwrap();
    }

    /// `update_if_version` 的 CAS 在**多字段补丁**下也成立：三条 UPDATE 中任意一条
    /// 落空，整批回滚（不留半截补丁）。
    ///
    /// 判红方式：把 `touched != patch.update_statements()` 那段改成永假
    /// （`if false && …`），再让 `title` 那条的 `AND version = ?` 落空（例如把
    /// `base + touched` 换成常量 0），本用例会看到 `title` 被改而返回 `Ok`。
    ///
    /// **必须与 `update_if_version_cas_lives_in_the_sql_not_only_in_the_precheck`
    /// 一起看**：本用例走的是「入口版本已过期」，那条路径由函数开头的
    /// `expected_version != entry` 预检先拦下，`write_patch` 根本不会执行。
    /// 所以本用例**测不到** `write_patch` 里的 `AND version = ?`；那一段由另一条
    /// 用例用触发器构造「事务中途被插队」来打。
    ///
    /// 而 `touched != patch.update_statements()` 这条「命中了一部分」的分支，
    /// 两条用例都走不到——它要求**同一次调用里第一条 UPDATE 命中、第二条落空**。
    /// 由 `update_if_version_rolls_back_a_partially_applied_patch` 单独覆盖。
    #[test]
    fn update_if_version_rolls_the_whole_patch_back_on_a_miss() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.update_if_version(
            &t.id,
            &TaskPatch {
                title: Some("插队者的".into()),
                ..TaskPatch::default()
            },
            "unix:100",
            Some(1),
        )
        .unwrap();

        // 三字段一起改，但版本已过期。
        let e = tr
            .update_if_version(
                &t.id,
                &TaskPatch {
                    title: Some("我的标题".into()),
                    done_when: Some(done_when(Some("build_passes"), Some("我的条件"))),
                    retry_limit: Some(Some(5)),
                    ..TaskPatch::default()
                },
                "unix:200",
                Some(1),
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::TaskVersionChanged { .. }),
            "{e:?}"
        );

        let view = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(view.title, "插队者的", "标题不得被改");
        assert_eq!(
            view.done_when.criteria.as_deref(),
            Some("all_tests_pass"),
            "done_when 不得被改"
        );
        assert_eq!(view.retry_limit, None, "约束不得被改");
        assert_eq!(raw_version(&tr, &t.id), 2, "版本不得涨");
    }

    /// `touched != patch.update_statements()`：**第一条命中、第二条落空**，
    /// 整批回滚。这是「半截补丁」唯一可能的入口。
    ///
    /// # 为什么需要单独一条
    ///
    /// 另两条用例都走不到这里：
    /// - 「入口版本过期」→ 函数开头的预检先返回，`write_patch` 不执行；
    /// - 「一条都没命中」→ 走的是 `touched == 0` 那条分支。
    ///
    /// 要走到本分支，必须让**同一次调用里**第一条 `UPDATE` 影响 1 行、第二条
    /// 影响 0 行。用 `BEFORE UPDATE` 触发器按 `NEW.title` 区分是哪一条：
    /// `title` 那条放行，`done_when` 那条 `RAISE(IGNORE)`。
    ///
    /// 判红方式：把 `if touched != patch.update_statements()` 改成 `if false`，
    /// 本用例会拿到 `Ok` 而不是 `TaskVersionChanged`，且 `title` 被改（因为事务
    /// 提交了）——两处断言同时红。
    #[test]
    fn update_if_version_rolls_back_a_partially_applied_patch() {
        let (_d, tr) = setup();
        let pool = tr.pool.clone();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        assert_eq!(raw_version(&tr, &t.id), 1);

        // 只让「写 title 的那条」过，写 done_when 的那条被 IGNORE 掉。
        // 判据必须用**只有那条语句会写的列**：用 `NEW.title` 不行——title 那条
        // 先执行完，done_when 那条的 `NEW.title` 也已经是「我的标题」，会把两条
        // 一起 IGNORE 掉（于是变成 `touched == 0` 那条分支，测不到本分支）。
        // `NEW.done_when_criteria` 只有 done_when 那条会改成 `build_passes`。
        pool.get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER skip_done_when BEFORE UPDATE ON task
                   WHEN NEW.id = '{}' AND NEW.done_when_criteria = 'build_passes'
                 BEGIN SELECT RAISE(IGNORE); END;",
                t.id
            ))
            .unwrap();

        let e = tr
            .update_if_version(
                &t.id,
                &TaskPatch {
                    title: Some("我的标题".into()),
                    done_when: Some(done_when(Some("build_passes"), Some("我的条件"))),
                    ..TaskPatch::default()
                },
                "unix:200",
                Some(1),
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::TaskVersionChanged { .. }),
            "半截补丁必须报 TaskVersionChanged，实际 {e:?}"
        );

        // 事务整体回滚：连那条命中的 title 也不许留下。
        let view = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(
            view.title, "把登录改成 OAuth",
            "命中了一条也必须整批回滚，不得留下半截补丁"
        );
        assert_eq!(
            view.done_when.criteria.as_deref(),
            Some("all_tests_pass"),
            "done_when 不得被改"
        );
        assert_eq!(raw_version(&tr, &t.id), 1, "版本不得涨");

        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER skip_done_when;")
            .unwrap();
    }

    /// `done_when` 两项全空 → 报错，且**库里的 `title` 与 `version` 都没变**。
    ///
    /// 判红方式：把 `update` 开头那句 `dw.validate()?` 删掉，这条立刻红
    /// （会写进一个两项都空的 `done_when`，`version` 也会涨到 2）。
    /// 或者把校验挪到事务提交之后——那会「报错了但 version 涨了」。
    #[test]
    fn update_with_an_invalid_done_when_writes_nothing() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        let version_before = raw_version(&tr, &t.id);

        let e = tr
            .update(
                &t.id,
                &TaskPatch {
                    title: Some("不该生效".into()),
                    done_when: Some(done_when(None, None)),
                    ..TaskPatch::default()
                },
                "unix:100",
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::InvalidDoneWhenCriteria(_)),
            "{e:?}"
        );

        let after = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(
            after.title, "把登录改成 OAuth",
            "同一补丁里的 title 也不得落库"
        );
        assert_eq!(after.done_when, t.done_when, "done_when 不得变");
        assert_eq!(
            raw_version(&tr, &t.id),
            version_before,
            "被拒的补丁不得让 version 涨——「报错了但 version 涨了」比不报错更糟"
        );
    }

    /// `Some(None)` = 显式清空成「不限」（列变 `NULL`）。
    ///
    /// 判红方式：把双层 `Option` 塌成单层（`None` 既表示「不改」又表示「清空」），
    /// 这条和 `update_only_touches_the_fields_given` 会同时红——这正是双层
    /// `Option` 存在的理由。
    #[test]
    fn update_clears_a_constraint_with_double_some_none() {
        let (_d, tr) = setup();
        let before = task_with_all_constraints(&tr);
        assert_eq!(
            raw_constraints(&tr, &before.id).3,
            Some(2),
            "前提：先有限制"
        );

        tr.update(
            &before.id,
            &TaskPatch {
                retry_limit: Some(None),
                ..TaskPatch::default()
            },
            "unix:200",
        )
        .unwrap();

        let (devices, agents, caps, retry) = raw_constraints(&tr, &before.id);
        assert_eq!(retry, None, "Some(None) 必须把 retry_limit 写成 NULL");
        // 其余三条没给，必须原样留着——「清空一条」不等于「清空全部」。
        assert_eq!(devices.as_deref(), Some("[\"n_1\"]"));
        assert_eq!(agents.as_deref(), Some("[\"opencode\"]"));
        assert_eq!(caps.as_deref(), Some("{\"arch\":\"x86_64\"}"));
    }

    /// 四条约束可以一次全设上，且只涨一次 `version`。
    ///
    /// 判红方式：把那条 `UPDATE` 里少写一个列（比如漏 `required_capabilities`），
    /// 这条立刻红；或把 `version = version + 1` 写成不加，也会红。
    #[test]
    fn update_can_set_all_four_constraints_at_once() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();

        let v = tr
            .update(
                &t.id,
                &TaskPatch {
                    allowed_devices: Some(Some("[\"n_1\",\"n_2\"]".into())),
                    allowed_agents: Some(Some("[\"opencode\"]".into())),
                    required_capabilities: Some(Some("{\"arch\":\"aarch64\"}".into())),
                    retry_limit: Some(Some(5)),
                    ..TaskPatch::default()
                },
                "unix:100",
            )
            .unwrap();

        assert_eq!(v, 2, "四条约束同属一条 UPDATE，只涨一次");
        assert_eq!(
            raw_constraints(&tr, &t.id),
            (
                Some("[\"n_1\",\"n_2\"]".to_string()),
                Some("[\"opencode\"]".to_string()),
                Some("{\"arch\":\"aarch64\"}".to_string()),
                Some(5),
            )
        );
    }

    /// 只给 `done_when` 时那两列同进同退，`title` 不动。
    ///
    /// 判红方式：把 `done_when` 那条 `UPDATE` 拆成两条（只写其中一列），
    /// 或把两列的顺序写反（criteria 与 description 互换）。
    #[test]
    fn update_writes_both_done_when_columns_together() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();

        tr.update(
            &t.id,
            &TaskPatch {
                done_when: Some(done_when(Some("build_passes"), Some("构建通过"))),
                ..TaskPatch::default()
            },
            "unix:100",
        )
        .unwrap();

        let after = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(after.done_when.criteria.as_deref(), Some("build_passes"));
        assert_eq!(after.done_when.description.as_deref(), Some("构建通过"));
        assert_eq!(after.title, t.title, "title 没给，不得动");
    }

    /// 未知 id → `TaskNotFound`（不是静默成功）。
    ///
    /// 判红方式：把 `update` 里那段「影响 0 行 → 再查一次」的分支改成
    /// 直接返回 `Ok`，这条立刻红。
    #[test]
    fn update_on_a_missing_task_is_named() {
        let (_d, tr) = setup();
        let e = tr
            .update(
                "nope",
                &TaskPatch {
                    title: Some("x".into()),
                    ..TaskPatch::default()
                },
                TS,
            )
            .unwrap_err();
        assert!(matches!(e, StorageError::TaskNotFound { .. }), "{e:?}");
    }

    /// 空补丁：不写库、`version` 不涨，但任务存在时不报错。
    ///
    /// 判红方式：把 `version = version + 1` 提到「命中判断之外」（无条件涨），
    /// 这条立刻红。
    #[test]
    fn an_empty_patch_changes_nothing_but_still_checks_existence() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        let empty = TaskPatch::default();
        assert!(empty.is_empty());

        let v = tr.update(&t.id, &empty, "unix:100").unwrap();
        assert_eq!(v, 1, "没改任何列就不该涨 version");
        assert_eq!(raw_version(&tr, &t.id), 1);
        assert_eq!(
            tr.get(&t.id).unwrap().unwrap().updated_at,
            TS,
            "updated_at 也不动"
        );

        let e = tr.update("nope", &empty, "unix:100").unwrap_err();
        assert!(
            matches!(e, StorageError::TaskNotFound { .. }),
            "空补丁也要确认任务存在：{e:?}"
        );
    }

    /// 排序与过滤：3 个任务（两个 `pending` 一早一晚、一个 `running`）。
    ///
    /// 判红方式：把 `ORDER BY created_at DESC` 改成 `ASC`（顺序反了）或
    /// 改成 `updated_at DESC`（改标题会跳位置）。
    #[test]
    fn list_for_project_with_state_filters_and_sorts() {
        let (_d, tr) = setup();
        let early = tr.create(new_task("p_a"), "unix:10").unwrap();
        let late = tr.create(new_task("p_a"), "unix:20").unwrap();
        let running = tr.create(new_task("p_a"), "unix:15").unwrap();
        tr.set_state(&running.id, TaskState::Queued, TS).unwrap();
        tr.set_state(&running.id, TaskState::Running, TS).unwrap();

        // 不带 state：3 条，第一条是最晚建的。
        let all = tr.list_for_project_with_state("p_a", None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, late.id, "最晚 created_at 的在最前");
        assert_eq!(
            all.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![late.id.as_str(), running.id.as_str(), early.id.as_str()]
        );

        // 带 pending：2 条，晚 → 早。
        let pending = tr
            .list_for_project_with_state("p_a", Some(TaskState::Pending))
            .unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, late.id);
        assert_eq!(pending[1].id, early.id);
        assert!(pending.iter().all(|t| t.state == TaskState::Pending));

        // 带 running：只剩那一条。
        let only_running = tr
            .list_for_project_with_state("p_a", Some(TaskState::Running))
            .unwrap();
        assert_eq!(only_running.len(), 1);
        assert_eq!(only_running[0].id, running.id);
    }

    /// 另一项目的任务不出现。
    ///
    /// 判红方式：漏写 `WHERE project_id = ?`（只按 state 过滤），这条立刻红
    /// ——`p_b` 的任务会混进来。
    #[test]
    fn list_for_project_scopes_by_project() {
        let (_d, tr) = setup();
        let mine = tr.create(new_task("p_a"), "unix:10").unwrap();
        let theirs = tr.create(new_task("p_b"), "unix:20").unwrap();

        let got = tr.list_for_project_with_state("p_a", None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, mine.id);

        // 带 state 时同样要限定项目：`p_b` 那条也是 pending。
        let got = tr
            .list_for_project_with_state("p_a", Some(TaskState::Pending))
            .unwrap();
        assert_eq!(got.len(), 1, "另一个项目的 pending 任务不得出现");
        assert_eq!(got[0].id, mine.id);
        assert_ne!(got[0].id, theirs.id);
    }
}
