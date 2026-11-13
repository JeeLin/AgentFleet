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
///   [`Task::needs_manual_acceptance`]，它落在闭集里但语义上仍需人点头。
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

/// 一个任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub state: TaskState,
    pub done_when: DoneWhen,
    /// 谁建的（`user` / `butler` / `external:github` …）。`None` = 早期数据。
    pub created_by: Option<String>,
    /// **默认起点**，不是唯一真相源——见模块文档。
    pub base_commit: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 建任务的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub project_id: String,
    pub title: String,
    pub done_when: DoneWhen,
    pub created_by: Option<String>,
    pub base_commit: Option<String>,
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
    /// 为什么要能问：`update` 按命中的字段逐条执行 `UPDATE`，每条都带
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
}

impl Task {
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
        match self.done_when.criteria.as_deref() {
            None | Some("") => true,
            Some("review_approved") => true,
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
    pub fn create(&self, new: NewTask, now: &str) -> Result<Task> {
        new.done_when.validate()?;
        let conn = self.pool.get()?;
        let id = crate::workspace::new_id();
        conn.execute(
            "INSERT INTO task
                 (id, project_id, title, state, done_when_criteria, done_when_description,
                  created_by, base_commit, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                id,
                new.project_id,
                new.title,
                new.done_when.criteria,
                new.done_when.description,
                new.created_by,
                new.base_commit,
                now
            ],
        )?;
        let raw = conn.query_row(
            &format!("{SELECT_COLS} WHERE id = ?1"),
            params![id],
            row_to_raw,
        )?;
        raw.into_task()
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>> {
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

    /// 一个项目的全部任务，最近建的在前。
    pub fn list_for_project(&self, project_id: &str) -> Result<Vec<Task>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE project_id = ?1 ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![project_id], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawTask::into_task).collect()
    }

    /// 某态的全部任务（调度候选集的入口）。
    pub fn list_by_state(&self, state: TaskState) -> Result<Vec<Task>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE state = ?1 ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(params![state.as_str()], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawTask::into_task).collect()
    }

    /// 改状态。**先校验转移合法性，再写库。**
    ///
    /// 边表在 [`crate::task_state::can_transition`]（唯一定义处），本函数不重复
    /// 判断「哪些态能到哪些态」。
    ///
    /// 写库用 compare-and-swap：`UPDATE ... WHERE id = ? AND state = <读到的旧态>`。
    /// 影响 0 行有两种可能，靠再查一次区分：
    /// - 任务不存在 → `TaskNotFound`
    /// - 任务存在但态已被别人改掉 → `TaskStateChanged`（HTTP 层映射 409）
    ///
    /// 为什么值得多写这一句 `WHERE`：多用户不在本轮范围，但**两次点击的竞态是
    /// 真实的**——详情页上「合入主干」和「打回」挨着，双击或两人同时点会让后写
    /// 静默覆盖先写。一行 `WHERE` 就能挡住。
    pub fn set_state(&self, id: &str, to: TaskState, now: &str) -> Result<Task> {
        let conn = self.pool.get()?;
        // 先读真值：不信任调用方说的 from，也不信任上一次读到的东西。
        let current = self
            .get(id)?
            .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })?;
        crate::task_state::check_transition(current.state, to)?;

        // CAS：`state = 旧态` 是这句 UPDATE 的守卫条件。
        let n = conn.execute(
            "UPDATE task SET state = ?2, updated_at = ?3, version = version + 1
             WHERE id = ?1 AND state = ?4",
            params![id, to.as_str(), now, current.state.as_str()],
        )?;
        if n == 0 {
            // 走到这里说明读完到写之间有人插了一脚：要么被删了，要么改了态。
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

    /// 改任务的标题 / 验收标准 / 约束。返回**新的 `version`**。
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
        if let Some(dw) = &patch.done_when {
            // 先拒绝，再写——理由同上。
            dw.validate()?;
        }
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let mut touched = 0usize;
        if let Some(title) = &patch.title {
            let n = tx.execute(
                "UPDATE task SET title = ?2, version = version + 1, updated_at = ?3
                 WHERE id = ?1",
                params![id, title, now],
            )?;
            touched += n;
        }
        if let Some(dw) = &patch.done_when {
            let n = tx.execute(
                "UPDATE task
                    SET done_when_criteria = ?2, done_when_description = ?3,
                        version = version + 1, updated_at = ?4
                  WHERE id = ?1",
                params![id, dw.criteria, dw.description, now],
            )?;
            touched += n;
        }
        if patch.allowed_devices.is_some()
            || patch.allowed_agents.is_some()
            || patch.required_capabilities.is_some()
            || patch.retry_limit.is_some()
        {
            // 四条约束的赋值**各按自己那一层 Option 决定**，不能统一写成
            // `SET col = ?`——那会把「这次不改」也写成 `NULL`（清空），
            // 正是双层 `Option` 要挡住的那个 bug。
            //
            // 做法是「先读回原值、按需替换、整组写一次」：既保住「四条约束
            // 一条 UPDATE、只涨一次 version」，又让没给的列保持原值。
            let current = load_constraints(&tx, id)?;
            let next = Constraints {
                allowed_devices: pick(patch.allowed_devices.as_ref(), current.allowed_devices),
                allowed_agents: pick(patch.allowed_agents.as_ref(), current.allowed_agents),
                required_capabilities: pick(
                    patch.required_capabilities.as_ref(),
                    current.required_capabilities,
                ),
                retry_limit: pick(patch.retry_limit.as_ref(), current.retry_limit),
            };
            let n = tx.execute(
                "UPDATE task
                    SET allowed_devices = ?2, allowed_agents = ?3,
                        required_capabilities = ?4, retry_limit = ?5,
                        version = version + 1, updated_at = ?6
                  WHERE id = ?1",
                params![
                    id,
                    next.allowed_devices,
                    next.allowed_agents,
                    next.required_capabilities,
                    next.retry_limit,
                    now
                ],
            )?;
            touched += n;
        }

        if touched == 0 {
            // 两条路都走到这里：任务不存在（一次都没命中），或补丁是空的
            // （命中 0 次但任务在）。靠再查一次区分，不猜。
            let exists: Option<i64> = tx
                .query_row("SELECT version FROM task WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            return match exists {
                Some(v) => Ok(v),
                None => Err(StorageError::TaskNotFound { id: id.to_string() }),
            };
        }

        // 故意**在事务里**读回 version：提交之后再读，别人可能又改了一次，
        // 返回的就不是本次的结果了。
        let version: i64 =
            tx.query_row("SELECT version FROM task WHERE id = ?1", params![id], |r| {
                r.get(0)
            })?;
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
    ) -> Result<Vec<Task>> {
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

const SELECT_COLS: &str = "SELECT id, project_id, title, state, done_when_criteria,
        done_when_description, created_by, base_commit, created_at, updated_at FROM task";

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
}

impl RawTask {
    fn into_task(self) -> Result<Task> {
        Ok(Task {
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
            project_id: project.into(),
            title: "把登录改成 OAuth".into(),
            done_when: DoneWhen {
                criteria: Some("all_tests_pass".into()),
                description: Some("全部测试通过且登录可用".into()),
            },
            created_by: Some("user".into()),
            base_commit: None,
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
        assert_eq!(t.created_by.as_deref(), Some("user"));
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
                NewTask {
                    project_id: "p_a".into(),
                    title: "x".into(),
                    done_when: DoneWhen {
                        criteria: None,
                        description: Some("登录可用".into()),
                    },
                    created_by: None,
                    base_commit: None,
                },
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
        let before = tr.list_for_project("p_a").unwrap().len();
        let e = tr
            .create(
                NewTask {
                    project_id: "p_a".into(),
                    title: "x".into(),
                    done_when: DoneWhen {
                        criteria: None,
                        description: None,
                    },
                    created_by: None,
                    base_commit: None,
                },
                TS,
            )
            .unwrap_err();
        assert!(
            matches!(e, StorageError::InvalidDoneWhenCriteria(_)),
            "{e:?}"
        );
        // 拒绝之后库里不能多出半行。
        assert_eq!(
            tr.list_for_project("p_a").unwrap().len(),
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
    /// 模拟方式：直接打两枪相同形状的 SQL——第一枪成功，第二枪带着已经过期的
    /// `expected` 必然影响 0 行。这样测的是守卫条件本身，不是 `set_state` 的实现细节。
    #[test]
    fn a_concurrent_state_change_is_rejected() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        // 把任务推到 running，两个「请求」都会看到 running。
        tr.set_state(&t.id, TaskState::Queued, TS).unwrap();
        tr.set_state(&t.id, TaskState::Running, TS).unwrap();

        let conn = tr.pool.get().unwrap();
        let cas = "UPDATE task SET state = ?2, updated_at = ?3, version = version + 1
                   WHERE id = ?1 AND state = ?4";
        // 请求 A：running → review，成功。
        let a = conn
            .execute(cas, params![t.id, "review", "unix:1", "running"])
            .unwrap();
        assert_eq!(a, 1);
        // 请求 B：它手上还是「旧态 running」，也想写到 review —— 影响 0 行。
        let b = conn
            .execute(cas, params![t.id, "review", "unix:2", "running"])
            .unwrap();
        assert_eq!(b, 0, "过期的旧态必须被 CAS 挡住");
        drop(conn);

        // 且真实态是 A 写进去的 review。
        assert_eq!(tr.get(&t.id).unwrap().unwrap().state, TaskState::Review);
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
            .list_for_project("p_a")
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
    fn task_with(dw: DoneWhen) -> Task {
        Task {
            id: "t_1".into(),
            project_id: "p_a".into(),
            title: "x".into(),
            state: TaskState::Pending,
            done_when: dw,
            created_by: None,
            base_commit: None,
            created_at: TS.into(),
            updated_at: TS.into(),
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
            assert!(
                dw.validate().is_ok(),
                "{code} 是闭集内的码，必须被接受：{:?}",
                dw.validate()
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
            tr.list_for_project("p_a").unwrap().is_empty(),
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
    fn task_with_all_constraints(tr: &TaskRepo) -> Task {
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
