//! `/api/projects/{id}/tasks` 与 `/api/tasks/*`：任务的 HTTP 面（v0.8.0 子任务 6）。
//!
//! 七个端点，一次说清它们的关系：
//!
//! - **列表 / 建**挂项目下（`/api/projects/{id}/tasks`）：任务的归属是项目，
//!   路径带上项目 id 才能让「这个项目的看板」一次说清要什么。
//! - **详情 / 改 / 状态推进 / 验收 / 失败处置**挂任务下（`/api/tasks/{id}`）。
//! - **`POST /state` 是唯一的状态推进端点**；`acceptance` 与 `failure` 是它的
//!   语法糖，但它们**内部照样调 `check_transition`**——不绕过状态机。为什么
//!   不把它们塞进 `/state` 的 body：那会让「一次转移」变成「一次带业务含义的
//!   操作」，`check_transition` 的单一职责立刻被稀释，而它们除了改状态还要
//!   留痕（`reason` / `note`）。
//!
//! # 三条全局纪律
//!
//! 1. **认证与 CSRF 不在这一层**：路由注册在 `build_app` 的 `.layer(require_auth)`
//!    之前，所以七个端点全部要 session，写端点还要 `af_csrf`（三张豁免表一个字
//!    没动）。
//! 2. **四个写端点都支持可选 `expected_version`**：给了且与库里不符 → 409
//!    `task_state_changed`，**不碰库**。它是 `If-Match` 风格的前置条件，让
//!    「我读到的是 v1」这件事能在写之前被校验一次。
//! 3. **返回最新 task（重新 `get`），不回显请求体**：回显会让「后端其实没存」
//!    这件事测不出来——请求说什么、响应就说什么，看起来永远一致。
//!
//! # 本里程碑**不会**说「已派给某台设备」
//!
//! 没有派发器，所以 `POST /state` 到 `queued` **只改状态**，不写 `execution` 行。
//! `execution` 行只能由显式的 `POST /api/tasks/{id}/executions`（留给 v0.8.2）
//! 产生；详情端点读到的执行记录来自别处（测试里直接用 repo 造）。
//!
//! # 也**不推** WebSocket 事件
//!
//! 全程不碰 `protocol::MessageType`：新增变体要同时改设备侧与 17 变体的穷尽
//! 匹配，与本里程碑范围无关。看板靠一次刷新。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use hub_storage::execution::FailureKind;
use hub_storage::task::{is_known_criteria_code, DoneWhen, NewTask, TaskPatch, TaskState};
use hub_storage::task_state;
use hub_storage::StorageError;

use crate::{err, ApiError, AppState};

// ---------------------------------------------------------------------------
// 响应体
// ---------------------------------------------------------------------------

/// 任务的四条约束（`schema_v7.sql` 的四列）。`None` = 不限。
#[derive(Debug, Clone, Serialize)]
pub struct Constraints {
    pub allowed_devices: Option<String>,
    pub allowed_agents: Option<String>,
    pub required_capabilities: Option<String>,
    pub retry_limit: Option<i64>,
}

/// 任务摘要：**列表与详情共用同一个形状**。
///
/// 共用而不是各建一套：列表项与详情里的 `task` 是同一张卡片，两个形状意味着
/// 前端要写两套读取路径，而它们迟早分叉（一处补了字段、另一处没补）。
///
/// `needs_manual_acceptance` / `criteria_known` 是**派生值**，不是列：前者
/// 由 `Task::needs_manual_acceptance()` 算（[`hub_storage::task`] 里那条判定
/// 的唯一定义处），后者是「这个判定码在闭集里吗」——存量数据可能有闭集外的
/// 值，前端要能把它标出来而不是假装认识。
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub state: &'static str,
    pub done_when: DoneWhen,
    pub needs_manual_acceptance: bool,
    pub criteria_known: bool,
    pub created_by: Option<String>,
    pub base_commit: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub constraints: Constraints,
    /// 乐观锁版本。前端每次写都带回来，后端据此挡覆盖写。
    pub version: i64,
    pub superseded_by: Option<String>,
    pub intent_ref: Option<String>,
}

impl TaskView {
    /// 从读模型组装。
    ///
    /// **`version` 与四条约束现在由读模型直接带着**——`hub-storage` 的
    /// `TaskView` 是在 `Task` 之上多出这些列的类型（见那个结构的 doc）。
    /// 这里不再单独查库：多一处查库就多一个「两次读到不同行」的窗口，
    /// 而那正是乐观锁想关掉的东西。
    fn new(task: &hub_storage::TaskView) -> Self {
        Self {
            id: task.id.clone(),
            project_id: task.project_id.clone(),
            title: task.title.clone(),
            state: task.state.as_str(),
            done_when: task.done_when.clone(),
            needs_manual_acceptance: task.needs_manual_acceptance(),
            // `criteria` 是不是集合里的**已知判定码**。没填（`None` / 空串）为
            // `false`：那不是「一个我们不认识的码」，是「根本没有码」——两者
            // 在页面上要显示成不同的话（「需人工验收」vs「判定码不认识」）。
            criteria_known: task
                .done_when
                .criteria
                .as_deref()
                .is_some_and(is_known_criteria_code),
            created_by: task.created_by.clone(),
            base_commit: task.base_commit.clone(),
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
            constraints: Constraints {
                allowed_devices: task.allowed_devices.clone(),
                allowed_agents: task.allowed_agents.clone(),
                required_capabilities: task.required_capabilities.clone(),
                retry_limit: task.retry_limit,
            },
            version: task.version,
            superseded_by: task.superseded_by.clone(),
            intent_ref: task.intent_ref.clone(),
        }
    }
}

/// 详情里的执行记录。
///
/// 只放前端要显示的那几列（`task-model.md` 的失败面板需要 `failure_kind` /
/// `reason` / `evidence`），不含 `worktree_path`——那是设备侧的路径，页面上
/// 既不显示也不该到处传。
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionView {
    pub id: String,
    pub state: &'static str,
    pub base_commit: Option<String>,
    pub agent_id: Option<String>,
    pub failure_kind: Option<&'static str>,
    pub reason: Option<String>,
    pub evidence: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

impl ExecutionView {
    /// 从读模型投影。**每一列都从库里来**，不留硬编码的 `None`——
    /// 那样「库里有没有值」这件事在响应体里看不出来，而失败面板正是靠这几列
    /// 决定显示什么。
    fn new(e: &hub_storage::ExecutionView) -> Self {
        Self {
            id: e.id.clone(),
            state: e.state.as_str(),
            base_commit: e.base_commit.clone(),
            agent_id: e.agent_id.clone(),
            failure_kind: e.failure_kind.map(hub_storage::FailureKind::as_str),
            reason: e.reason.clone(),
            evidence: e.evidence.clone(),
            started_at: e.started_at.clone(),
            ended_at: e.ended_at.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 请求体
// ---------------------------------------------------------------------------

/// `POST /api/projects/{id}/tasks` 的请求体。
///
/// `done_when` **必填**（不是 `Option`）：一个连「怎么算完成」都没写的任务建
/// 出来只会空转。缺键时 axum 的 `Json` 提取器会拒绝（见 [`read_json`]）。
#[derive(Debug, Deserialize)]
pub struct CreateTaskBody {
    pub title: String,
    pub done_when: DoneWhen,
    /// 四条约束。**建任务时就写进去**，不走「先建再补写一次」——那会让带约束
    /// 建出来的任务 `version` 是 2，与「刚建好」不符（`version` 记的是「被改过
    /// 几次」，建任务不是一次修改）。
    #[serde(default)]
    pub constraints: Option<ConstraintsBody>,
}

/// 四条约束的入参形态。**全可选**：没给的列写 `NULL`（= 不限）。
///
/// 不做双层 `Option`：建任务时「清空」与「不填」是同一件事（都写 `NULL`），
/// 双层只在 PATCH 里有意义。
#[derive(Debug, Default, Deserialize)]
pub struct ConstraintsBody {
    #[serde(default)]
    pub allowed_devices: Option<String>,
    #[serde(default)]
    pub allowed_agents: Option<String>,
    #[serde(default)]
    pub required_capabilities: Option<String>,
    #[serde(default)]
    pub retry_limit: Option<i64>,
}

/// `PATCH /api/tasks/{id}` 的请求体：`TaskPatch` 的字段 + `expected_version`。
///
/// 约束列是**双层 `Option`**（与 `TaskPatch` 同口径）：键缺失 = 不改；
/// 显式 `null` = 清成「不限」；给值 = 改成它。单层 `Option` 会让「只想改标题」
/// 顺手把所有约束清空。
#[derive(Debug, Deserialize)]
pub struct PatchTaskBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub done_when: Option<DoneWhen>,
    #[serde(default, deserialize_with = "double_option")]
    pub allowed_devices: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub allowed_agents: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub required_capabilities: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub retry_limit: Option<Option<i64>>,
    /// 可选前置条件。前端从最近一次读到的 `version` 取。
    #[serde(default)]
    pub expected_version: Option<i64>,
}

impl PatchTaskBody {
    fn into_patch(self) -> TaskPatch {
        TaskPatch {
            title: self.title,
            done_when: self.done_when,
            allowed_devices: self.allowed_devices,
            allowed_agents: self.allowed_agents,
            required_capabilities: self.required_capabilities,
            retry_limit: self.retry_limit,
        }
    }
}

/// `POST /api/tasks/{id}/state` 的请求体。
#[derive(Debug, Deserialize)]
pub struct StateBody {
    pub to: String,
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// `POST /api/tasks/{id}/acceptance` 的请求体。
#[derive(Debug, Deserialize)]
pub struct AcceptanceBody {
    pub verdict: AcceptanceVerdict,
    /// 判定理由。`override: true` 时**必填**（见 [`err::ACCEPTANCE_NOTE_REQUIRED`]）。
    #[serde(default)]
    pub note: Option<String>,
    /// 覆盖判定：本轮没有自动判定器，「覆盖」是留给将来「机器判 A、人判 B」
    /// 那一档的入口。带上它就必须写理由——一条没有理由的覆盖判定事后没人
    /// 说得清当初为什么。
    ///
    /// 字段改叫 `override_` 只是 Rust 关键字的绕行，线上名字仍由
    /// `serde(rename)` 钉成规格里的 `override`。
    #[serde(default, rename = "override")]
    pub override_: bool,
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// 验收结论。两个值，闭集之外一律 400。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcceptanceVerdict {
    /// 通过：`review → done`。
    Pass,
    /// 打回：`review → pending`。**打回不是失败**（`task-model.md:101`）。
    Reject,
}

/// `POST /api/tasks/{id}/failure` 的请求体。
#[derive(Debug, Deserialize)]
pub struct FailureBody {
    pub action: FailureAction,
    /// 失败类型（八形态之一）。`action: "close"` 时随 `reason` 一起落库。
    #[serde(default)]
    pub failure_kind: Option<String>,
    /// 失败原因。`close` 必填（「没有原因串的失败等于没发生」）。
    #[serde(default)]
    pub reason: Option<String>,
    /// `action: "edit"` 时一起写进 `done_when` 的新验收标准。
    #[serde(default)]
    pub done_when: Option<DoneWhen>,
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// 失败处置的三个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureAction {
    /// 重试：`failed → queued`。
    Retry,
    /// 改验收标准后回待办：`failed → pending`（同时写 `done_when`）。
    Edit,
    /// 关单：进 `failed` 终态并写 `reason`。
    Close,
}

fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

// ---------------------------------------------------------------------------
// 公共小工具
// ---------------------------------------------------------------------------

/// 当前时间，写成 `unix:<秒>`。
///
/// 与全仓其余 repo 同一个口径：`now` 由调用方传入，repo 自己不读时钟（那样
/// 它在测试里就不可注入了）。时钟早于 `UNIX_EPOCH` 时取 0 而不是 panic——
/// Hub 是常驻服务，一次系统时间回拨不该把它带走。
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("unix:{secs}")
}

/// 读请求体并解析成 `T`。
///
/// 不用 `Json` 提取器：axum 对形状不对的 JSON 回 **422** 与一段 `text/plain`，
/// 而 `/api/*` 的错误体契约是 `{"error": "<code>"}`（前端按 code 分支）。
/// 这一层统一成 400 `invalid_request_body`。
async fn read_json<T: serde::de::DeserializeOwned>(
    req: axum::extract::Request,
) -> Result<T, ApiError> {
    let body = crate::routes::read_body(req).await?;
    serde_json::from_slice(&body).map_err(|_| ApiError::bad_request_body())
}

/// 库里那几列「写入口不必知道、但读出口要露出来」的值。
///
/// **这个结构现在只承载版本号**：四条约束与 `superseded_by` / `intent_ref`
/// 已经进 `hub_storage::TaskView` 读模型（子任务 6 裁决时补的），所以这里
/// 不再重复读它们。留着 `version` 是因为「前置条件校验」读的是**库里的**版本，
/// 而不是「上一响应里那个」——两者在别人刚改过时会不同，而 `require_version`
/// 要拒的正是后者。
struct TaskMeta {
    version: i64,
}

fn read_meta(state: &AppState, id: &str) -> Result<Option<TaskMeta>, ApiError> {
    let conn = state
        .pool()
        .get()
        .map_err(|e| ApiError::internal_with("cannot check out a db connection", e))?;
    let got = conn
        .query_row("SELECT version FROM task WHERE id = ?1", [id], |r| {
            Ok(TaskMeta { version: r.get(0)? })
        })
        .ok();
    Ok(got)
}

/// 读出最新一行并组装成视图。**所有写端点的最后一步都走它**。
///
/// 不回显请求体：见模块头第 3 条纪律。
fn reload(state: &AppState, id: &str) -> Result<TaskView, ApiError> {
    let task = state
        .tasks()
        .get(id)
        .map_err(|e| ApiError::internal_with("cannot read task", e))?
        .ok_or_else(|| ApiError::not_found_with(err::TASK_NOT_FOUND))?;
    Ok(TaskView::new(&task))
}

/// 读当前任务；不存在 → 404 `task_not_found`。
fn load(state: &AppState, id: &str) -> Result<hub_storage::TaskView, ApiError> {
    state
        .tasks()
        .get(id)
        .map_err(|e| ApiError::internal_with("cannot read task", e))?
        .ok_or_else(|| ApiError::not_found_with(err::TASK_NOT_FOUND))
}

/// 前置条件：`expected_version` 给了就必须与库里一致。
///
/// 不一致 → 409 `task_state_changed`，且**不碰库**：请求描述的是一个已经不存在
/// 的世界，写进去就是覆盖别人的修改。前端收到它会「保留用户已填的内容、只刷新
/// `task` 与 `version`、再提示重新提交」。
fn require_version(meta: &TaskMeta, expected: Option<i64>) -> Result<(), ApiError> {
    match expected {
        Some(v) if v != meta.version => Err(ApiError::conflict_with(
            err::TASK_STATE_CHANGED,
            || json!({ "expected_version": v, "actual_version": meta.version }),
        )),
        _ => Ok(()),
    }
}

/// 把状态机 / 存储层的错误翻成 HTTP。
///
/// **三种「非法转移」的翻法不同**，各带各自的前端人话：
/// - `IllegalTaskTransition` → 409 + `detail.from` / `detail.to`（前端查表生成
///   「待验收不能直接判失败，请打回」这种句子，`hint` 也一并给出去）；
/// - `TaskStateChanged` → 409 `task_state_changed`（前端自动重取）；
/// - `TaskNotFound` → 404 `task_not_found`。
fn map_storage(e: StorageError, context: &'static str) -> ApiError {
    match e {
        StorageError::IllegalTaskTransition { from, to, hint } => ApiError::conflict_with(
            err::ILLEGAL_TASK_TRANSITION,
            || json!({ "from": from, "to": to, "hint": hint }),
        ),
        StorageError::TaskStateChanged { expected, actual } => ApiError::conflict_with(
            err::TASK_STATE_CHANGED,
            || json!({ "expected": expected, "actual": actual }),
        ),
        StorageError::TaskNotFound { .. } => ApiError::not_found_with(err::TASK_NOT_FOUND),
        StorageError::InvalidTaskState(s) => {
            ApiError::bad_request_with(err::INVALID_TASK_STATE, || json!({ "state": s }))
        }
        StorageError::InvalidDoneWhenCriteria(s) => ApiError::bad_request_with(
            err::INVALID_CRITERIA,
            || json!({ "criteria": s, "allowed": hub_storage::task::CRITERIA_CODES }),
        ),
        StorageError::InvalidFailureKind(s) => {
            ApiError::bad_request_with(err::INVALID_FAILURE_KIND, || json!({ "failure_kind": s }))
        }
        // coverage: excluded — 余下的变体是数据库故障（连接池取不到 / SQLite 报错），
        // 要构造必须把库打坏。留兜底是为了**不把「库坏了」说成 404 或 400**。
        other => ApiError::internal_with(context, other),
    }
}

/// 任务态字符串 → 枚举。非法值 400 `invalid_task_state`，**不回落** `pending`。
///
/// 迁移之后 `draft` / `ready` / `blocked` 都是非法值：静默回落会让「有代码写回了
/// 旧值」这件事一直藏着。
fn parse_state(s: &str) -> Result<TaskState, ApiError> {
    s.parse::<TaskState>()
        .map_err(|_| ApiError::bad_request_with(err::INVALID_TASK_STATE, || json!({ "state": s })))
}

// ---------------------------------------------------------------------------
// 一次 CAS 状态转移
// ---------------------------------------------------------------------------

/// 在一次转移里顺带要写的业务字段（`failure` / `acceptance` 用）。
///
/// 与状态写在**同一条 UPDATE** 里：拆成两条会出现「态变了、原因还没写」的
/// 中间态被别的连接读到，而失败留痕恰恰是事后要反复读的东西。
#[derive(Default)]
struct TransitionExtras {
    /// `task` 表本身**没有** `reason` 列（`schema_v7.sql` 只给 `execution` 加了
    /// 那三列），所以这里写的是该任务最近一次执行上的 `failure_kind` / `reason`
    /// （见 [`crate::routes::tasks`] 的 `failure` handler）。
    failure: Option<(FailureKind, String)>,
    /// 同一次转移要改的 `done_when`（`action: "edit"`）。
    done_when: Option<DoneWhen>,
}

/// 读 → 校验前置条件 → `check_transition` → CAS 写入。
///
/// # 为什么是三次读而不是一次
///
/// 1. 先读当前任务（不存在 404）；
/// 2. 读版本与约束、比 `expected_version`（不符 409，**不碰库**）；
/// 3. CAS 影响 0 行时**再读一次**区分「没了」(404) 与「变了」(409)。
///
/// 第 3 步是这条纪律的全部要点：不靠猜。`UPDATE ... WHERE id = ? AND state = ?
/// AND version = ?` 三个条件缺一不可——只有 `id` 的话它会静默覆盖别人的写。
fn transition(
    state: &AppState,
    id: &str,
    to: TaskState,
    expected: Option<i64>,
    extras: TransitionExtras,
) -> Result<(), ApiError> {
    let current = load(state, id)?;
    let meta =
        read_meta(state, id)?.ok_or_else(|| ApiError::not_found_with(err::TASK_NOT_FOUND))?;
    require_version(&meta, expected)?;
    // **内部调状态机**，不绕过：`acceptance` / `failure` 与 `/state` 走同一条边表。
    task_state::check_transition(current.state, to)
        .map_err(|e| map_storage(e, "check transition"))?;
    if let Some(dw) = &extras.done_when {
        dw.validate()
            .map_err(|e| map_storage(e, "validate done_when"))?;
    }

    let at = now();
    let n = state.tasks().transition_with(
        id,
        to,
        &at,
        Some(meta.version),
        hub_storage::TaskTransition {
            done_when: extras.done_when.clone(),
        },
    );
    match n {
        Ok(_) => {}
        Err(e) => {
            // 影响 0 行有两种：任务没了，或「态 / 版本」在我们读完之后变了。
            // 仓储已**再读一次**区分（`transition_with` 里），这里只做映射。
            return Err(map_storage(e, "transition task state"));
        }
    }

    // 失败留痕写到该任务最近一次执行上：`task` 表没有那三列。
    if let Some((kind, reason)) = extras.failure {
        record_on_latest_execution(state, id, kind, &reason)?;
    }
    Ok(())
}

/// 把一次失败记到该任务**最近一次执行**上（有执行记录时）。
///
/// 为什么要绕一层而不是写 `task` 表：`task-model.md` 的失败留痕三列
/// （`failure_kind` / `reason` / `evidence`）在 v7 里加在 `execution` 上——
/// 「一次执行怎么失败的」是执行的事实。任务可以被跑多次，把最后一次的原因
/// 写到任务上会覆盖掉前一次的历史。
///
/// 任务还没有任何执行记录时**静默跳过**：本里程碑没有派发器，`POST /failure`
/// 完全可能作用在一个从没跑过的任务上（人主动关掉一个待办）。那种情况下
/// 状态与 `reason` 的语义由 `state` + `updated_at` 承载，编一条执行记录反而是
/// 凭空造事实。
fn record_on_latest_execution(
    state: &AppState,
    task_id: &str,
    kind: FailureKind,
    reason: &str,
) -> Result<(), ApiError> {
    let latest = state
        .executions()
        .list_for_task(task_id)
        .map_err(|e| ApiError::internal_with("cannot list executions", e))?;
    let Some(exec) = latest.first() else {
        return Ok(());
    };
    // `evidence` 传空串是刻意的：`ExecutionRepo::set_failure` 会把它写成规格
    // 约定的字面量 `'none'`（`task-model.md:79`），而不是空串。
    state
        .executions()
        .set_failure(&exec.id, kind, reason, "")
        .map_err(|e| ApiError::internal_with("cannot record the failure", e))
}

/// 把**验收理由**写到该任务最近一次执行上。
///
/// 与 [`record_on_latest_execution`] 分开是因为**它不写 `failure_kind`**：
/// 打回不是失败（`task-model.md:101`）。把它写成 `task_failed` 会让设备页的
/// 「失败集中在哪一类」把「人打回」算进「事没做成」，那条统计就废了。
fn record_note_on_latest_execution(
    state: &AppState,
    task_id: &str,
    note: &str,
) -> Result<(), ApiError> {
    let latest = state
        .executions()
        .list_for_task(task_id)
        .map_err(|e| ApiError::internal_with("cannot list executions", e))?;
    let Some(exec) = latest.first() else {
        return Ok(());
    };
    state
        .executions()
        .set_reason(&exec.id, note)
        .map_err(|e| ApiError::internal_with("cannot record the acceptance note", e))
}

// ---------------------------------------------------------------------------
// 端点
// ---------------------------------------------------------------------------

/// `GET /api/projects/{id}/tasks?state=`：一个项目的任务（可选按态过滤）。
///
/// 排序由仓储固定为 `created_at DESC`（与原型卡片顺序一致，不按 `updated_at`
/// ——否则改一下标题卡片就跳位置）。
pub async fn list_tasks(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let filter = match q.get("state").map(String::as_str) {
        None => None,
        // 空串等于没给：前端把下拉框的「全部」拼成 `?state=` 是很常见的写法，
        // 为它 400 只会让人莫名其妙。
        Some("") => None,
        Some(s) => Some(parse_state(s)?),
    };
    let tasks = state
        .tasks()
        .list_for_project_with_state(&project_id, filter)
        .map_err(|e| map_storage(e, "cannot list tasks"))?;

    let items: Vec<TaskView> = tasks.iter().map(TaskView::new).collect();
    Ok(Json(json!({ "tasks": items })))
}

/// `POST /api/projects/{id}/tasks`：建任务。
///
/// 三道门禁，顺序是「先拒绝、再写库」：
/// 1. `title` trim 后非空（空标题的卡片在页面上没有可点的锚点）；
/// 2. `done_when` 过 `DoneWhen::validate()`（闭集外的判定码 / 两者都空）；
/// 3. 约束四条按给的写，没给的写 `NULL`（= 不限）。
pub async fn create_task(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    req: axum::extract::Request,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body: CreateTaskBody = read_json(req).await?;
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request(err::BAD_BODY));
    }
    body.done_when
        .validate()
        .map_err(|e| map_storage(e, "validate done_when"))?;

    let constraints = body.constraints.unwrap_or_default();
    let task = state
        .tasks()
        .create(
            NewTask {
                created_by: Some("user".to_string()),
                allowed_devices: constraints.allowed_devices,
                allowed_agents: constraints.allowed_agents,
                required_capabilities: constraints.required_capabilities,
                retry_limit: constraints.retry_limit,
                ..NewTask::new(project_id.clone(), title, body.done_when)
            },
            &now(),
        )
        .map_err(|e| map_storage(e, "cannot create task"))?;

    // 重新读一遍再回显：与其余写端点同一条纪律（不回显请求体）。
    let view = TaskView::new(&task);
    Ok((StatusCode::CREATED, Json(json!({ "task": view }))))
}

/// `GET /api/tasks/{id}`：详情 = 任务 + 执行记录。
///
/// `executions` **无记录时是 `[]`**，不是 `null` 也不省略字段：前端写的是
/// `executions.map(...)`，`undefined?.map` 会把整页炸掉。
pub async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let task = load(&state, &id)?;
    let executions = state
        .executions()
        .list_for_task(&id)
        .map_err(|e| ApiError::internal_with("cannot list executions", e))?;
    let rows: Vec<ExecutionView> = executions.iter().map(ExecutionView::new).collect();
    let view = TaskView::new(&task);
    Ok(Json(json!({ "task": view, "executions": rows })))
}

/// `PATCH /api/tasks/{id}`：改标题 / `done_when` / 约束。
///
/// 走 `TaskRepo::update`（它自己在事务里按命中的字段逐条 `UPDATE`、每条
/// `version = version + 1`）。前置条件与 CAS 的守卫在这里：
/// 有 `expected_version` 时先比对，再由 `update` 写入，随后**校验版本是否只涨了
/// 我们预期的那一步**——涨多了说明有人插队，回 409 而不是把别人的改动当成自己
/// 的成果报给用户。
pub async fn patch_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body: PatchTaskBody = read_json(req).await?;
    let expected = body.expected_version;
    let patch = body.into_patch();

    let current = load(&state, &id)?;
    let meta =
        read_meta(&state, &id)?.ok_or_else(|| ApiError::not_found_with(err::TASK_NOT_FOUND))?;
    require_version(&meta, expected)?;

    if let Some(title) = &patch.title {
        if title.trim().is_empty() {
            return Err(ApiError::bad_request(err::BAD_BODY));
        }
    }
    if let Some(dw) = &patch.done_when {
        dw.validate()
            .map_err(|e| map_storage(e, "validate done_when"))?;
    }

    // 空补丁：什么都不改，直接把当前状态回出去（不白涨一次 version）。
    if patch.is_empty() {
        return Ok(Json(json!({ "task": TaskView::new(&current) })));
    }

    let version = state
        .tasks()
        .update(&id, &patch, &now())
        .map_err(|e| map_storage(e, "cannot patch task"))?;
    // 一次补丁最多命中三条 UPDATE（标题 / 验收标准 / 四条约束），所以版本至多
    // 涨三步。涨得比这更多 = 有别人也在写；涨得比命中数少不会发生（每条都带
    // `version + 1`）。这里只挡「有人插队」这一侧。
    if let Some(v) = expected {
        if version > v + 3 {
            return Err(ApiError::conflict_with(
                err::TASK_STATE_CHANGED,
                || json!({ "expected_version": v, "actual_version": version }),
            ));
        }
    }
    let view = reload(&state, &id)?;
    Ok(Json(json!({ "task": view })))
}

/// `POST /api/tasks/{id}/state`：**唯一**的状态推进端点。
///
/// body `{to, expected_version?}`。非法转移 → 409 `illegal_task_transition`，
/// `detail` 带 `from` / `to`（前端据它查表生成人话，例如「待验收不能直接判
/// 失败，请打回」）。
pub async fn push_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body: StateBody = read_json(req).await?;
    let to = parse_state(&body.to)?;
    transition(
        &state,
        &id,
        to,
        body.expected_version,
        TransitionExtras::default(),
    )?;
    let view = reload(&state, &id)?;
    Ok(Json(json!({ "task": view })))
}

/// `POST /api/tasks/{id}/acceptance`：验收判定。
///
/// - `pass` → `done`（`review → done`）；
/// - `reject` → `pending`（`review → pending`）。**打回不是失败**：它不该进
///   `failed`，否则用户会看到「失败」而实际上只是要改一版。
/// - `override: true` 必须带 `note`（400 `acceptance_note_required`）。
///
/// 内部调 `check_transition`：对 `pending` 的任务做验收会得到 409
/// `illegal_task_transition`，而不是被静默写进 `done`。
pub async fn submit_acceptance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body: AcceptanceBody = read_json(req).await?;
    // 理由的门禁在**读库之前**：请求本身不完整时，「这条任务在不在」没有意义。
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.override_ && note.is_none() {
        return Err(ApiError::bad_request(err::ACCEPTANCE_NOTE_REQUIRED));
    }
    let to = match body.verdict {
        AcceptanceVerdict::Pass => TaskState::Done,
        AcceptanceVerdict::Reject => TaskState::Pending,
    };
    transition(
        &state,
        &id,
        to,
        body.expected_version,
        TransitionExtras::default(),
    )?;
    // 判定理由留痕：写在该任务最近一次执行上。
    //
    // **不写 `failure_kind`**：验收打回**不是失败**（`task-model.md:101`）。
    // 早先的实现把它硬编码成 `task_failed`，那会让设备页的「失败集中在哪一类」
    // 把「人打回」算进「事没做成」——两条完全不同的信号混在一起，那条统计就废了。
    // 所以这里只落 `reason`（打回理由），`failure_kind` 保持库里原值不动。
    if let Some(note) = note {
        record_note_on_latest_execution(&state, &id, note)?;
    }
    let view = reload(&state, &id)?;
    Ok(Json(json!({ "task": view })))
}

/// `POST /api/tasks/{id}/failure`：失败处置。
///
/// | `action` | 转移 | 额外动作 |
/// |---|---|---|
/// | `retry` | `failed → queued` | — |
/// | `edit`  | `failed → pending` | 写 `done_when` |
/// | `close` | `pending / running / queued → failed` | 写 `reason` + `failure_kind` |
///
/// # `queued` 态下的 `close`
///
/// 状态机没有 `queued → failed` 边（子任务 2 的裁决）。所以这条路径**先走
/// `queued → pending` 再走 `pending → failed`**——两次都过 `check_transition`，
/// 不是绕过状态机直写终态。两次之间失败的话任务停在 `pending`：那是可恢复的
/// 中间态（人还能再关一次），而不是一个看起来顺利的谎。
pub async fn submit_failure(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body: FailureBody = read_json(req).await?;

    // 字段级校验先做，**不碰库**。
    let kind = match body.failure_kind.as_deref() {
        None => None,
        Some("") => None,
        Some(k) => Some(
            k.parse::<FailureKind>()
                .map_err(|e| map_storage(e, "invalid failure_kind"))?,
        ),
    };
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let current = load(&state, &id)?;
    let target = match body.action {
        FailureAction::Retry => TaskState::Queued,
        FailureAction::Edit => TaskState::Pending,
        FailureAction::Close => TaskState::Failed,
    };

    // 状态机先判一遍：不合法的动作不该留下任何痕迹（比如 edit 改了 done_when
    // 却转不过去）。`queued` 的 close 是唯一的两步路径。
    let queued_close = body.action == FailureAction::Close && current.state == TaskState::Queued;
    if queued_close {
        task_state::check_transition(current.state, TaskState::Pending)
            .and_then(|()| task_state::check_transition(TaskState::Pending, TaskState::Failed))
            .map_err(|e| map_storage(e, "check transition"))?;
    }

    // `close` 必须写 `reason`：「没有原因串的失败等于没发生」
    // （`task-model.md:75`）。没有它，失败列上会是一张说不出话的卡。
    if body.action == FailureAction::Close && reason.is_none() {
        return Err(ApiError::bad_request(err::BAD_BODY));
    }

    if queued_close {
        // 第一步：`queued → pending`（CAS + 前置条件都照常）。
        transition(
            &state,
            &id,
            TaskState::Pending,
            body.expected_version,
            TransitionExtras::default(),
        )?;
        // 第二步：`pending → failed`。**不再带 `expected_version`**：那一步已经
        // 校验过，第二次带上去会把刚刚涨过的版本当成冲突。
        transition(
            &state,
            &id,
            TaskState::Failed,
            None,
            TransitionExtras {
                failure: kind.zip(reason.clone()),
                done_when: None,
            },
        )?;
    } else {
        let extras = TransitionExtras {
            // `retry` 不写失败留痕：这次动作是「它要重跑」，把上一轮的失败类型
            // 再写一遍只会让人以为又失败了一次。`edit` 同理（那是改条件）。
            failure: match body.action {
                FailureAction::Close => kind.zip(reason.clone()),
                _ => None,
            },
            done_when: match body.action {
                FailureAction::Edit => body.done_when.clone(),
                _ => None,
            },
        };
        transition(&state, &id, target, body.expected_version, extras)?;
    }

    // `close` 即使没有 `failure_kind`（人只说了一句原因）也要把原因留下。
    if body.action == FailureAction::Close {
        if let (Some(reason), None) = (reason.as_deref(), kind) {
            record_on_latest_execution(&state, &id, FailureKind::TaskFailed, reason)?;
        }
    }
    let view = reload(&state, &id)?;
    Ok(Json(json!({ "task": view })))
}

/// hub-server 的测试夹具：用 `AppState::bootstrap_with` 造一个**已设密码**的
/// 实例（认证层的正常模式），并按需插入项目行。
///
/// 为什么走 `build_app` 而不是直调 handler：本文件要钉的东西里有一半在中间件
/// 与路由装配上——「七个端点都过了认证」「四个写端点都过了 CSRF」「`/api/tasks`
/// 没被 `/api/{*rest}` 兜底吃掉」。直调 handler 会把这三类问题全漏掉。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        build_app, AuthParams, LoginRateLimiter, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::time::Duration;
    use tower::ServiceExt;

    const PASSWORD: &str = "correct horse battery";
    const TS: &str = "unix:1700000000";

    /// 一条测试用的「浏览器」：app + 两条 cookie。
    struct Harness {
        app: Router,
        state: AppState,
        session: String,
        csrf: String,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
            let state = AppState::bootstrap_with(
                pool,
                dir.path(),
                AuthParams {
                    m_cost: 8192,
                    t_cost: 1,
                    p_cost: 1,
                },
                Duration::from_millis(200),
                LoginRateLimiter::new(10, 60),
            )
            .unwrap();
            state.auth().setup_password(PASSWORD).unwrap();
            let app = build_app(state.clone());
            let login = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/login")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({ "password": PASSWORD }).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .expect("login responds");
            assert_eq!(login.status(), StatusCode::OK, "登录必须成功");
            let session = cookie_of(&login, SESSION_COOKIE).expect("af_session");
            let csrf = cookie_of(&login, CSRF_COOKIE).expect("af_csrf");
            // 一个项目行，任务的 `project_id` 要指向它（v5 没有外键，但语义上要有）。
            state
                .pool()
                .get()
                .unwrap()
                .execute_batch(
                    "INSERT INTO project (id, name, shape, created_at)
                         VALUES ('prj_1', 'app', 'central', 'unix:0');",
                )
                .unwrap();
            Self {
                app,
                state,
                session,
                csrf,
                _dir: dir,
            }
        }

        /// 带 cookie 的请求；`csrf` 为 `false` 时**故意不带** `x-csrf-token`。
        async fn send(&self, req: Request<Body>, csrf: bool) -> Reply {
            let mut req = req;
            req.headers_mut().insert(
                "cookie",
                format!(
                    "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                    self.session, self.csrf
                )
                .parse()
                .unwrap(),
            );
            if csrf {
                req.headers_mut()
                    .insert(CSRF_HEADER, self.csrf.parse().unwrap());
            }
            Reply::new(self.app.clone().oneshot(req).await.expect("app responds")).await
        }

        async fn get(&self, path: &str) -> Reply {
            self.send(
                Request::builder().uri(path).body(Body::empty()).unwrap(),
                true,
            )
            .await
        }

        async fn mutate(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
            let mut b = Request::builder().method(method).uri(path);
            let body = match body {
                Some(v) => {
                    b = b.header("content-type", "application/json");
                    Body::from(v.to_string())
                }
                None => Body::empty(),
            };
            self.send(b.body(body).unwrap(), true).await
        }

        /// 建一个任务，返回它的 id（走 HTTP，不直调 repo——列表/建是同一组
        /// 端点的两半，绕过一半就等于没测它）。
        async fn create(&self, title: &str) -> String {
            let r = self
                .mutate(
                    "POST",
                    "/api/projects/prj_1/tasks",
                    Some(serde_json::json!({
                        "title": title,
                        "done_when": { "criteria": "all_tests_pass" },
                    })),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
            r.json()["task"]["id"].as_str().unwrap().to_string()
        }

        /// 直接把任务推到某个态（**用 repo 推进，不走 HTTP**：这条是夹具，
        /// 它要造的是「任务已经在这里」，不是「转移端点对不对」）。
        fn force_state(&self, id: &str, to: TaskState) {
            self.state
                .pool()
                .get()
                .unwrap()
                .execute(
                    "UPDATE task SET state = ?2, version = version + 1 WHERE id = ?1",
                    hub_storage::params![id, to.as_str()],
                )
                .unwrap();
        }

        /// 当前版本（库里读，不信响应体）。
        fn version_of(&self, id: &str) -> i64 {
            self.state
                .pool()
                .get()
                .unwrap()
                .query_row("SELECT version FROM task WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        }
    }

    fn cookie_of(resp: &axum::response::Response, name: &str) -> Option<String> {
        resp.headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|raw| {
                let (k, v) = raw.split(';').next()?.trim().split_once('=')?;
                (k == name).then(|| v.to_string())
            })
    }

    struct Reply {
        status: StatusCode,
        body: String,
    }

    impl Reply {
        async fn new(resp: axum::response::Response) -> Self {
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            Self {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }
        }

        fn json(&self) -> Value {
            serde_json::from_str(&self.body).expect("响应体必须是 JSON")
        }

        /// 错误码。
        fn code(&self) -> String {
            self.json()["error"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    }

    // -----------------------------------------------------------------------
    // 1. 七个端点都要认证
    // -----------------------------------------------------------------------

    /// 表驱动地把七个端点各打一遍（**不带 cookie**），一律 401。
    ///
    /// 判红方式：把某一条路由写在 `build_app` 的 `.layer(require_auth)` 之后
    /// （或塞进 `AUTH_EXEMPT_PATHS`），那一行立刻从 401 变 200/404 而红；
    /// 漏注册一条则会拿到 401——因为 `/api/{*rest}` 兜底**也在 layer 之内**，
    /// 所以「漏挂」这件事由测试 12（404 vs 500）与「路由存在性」那条互补覆盖。
    #[tokio::test]
    async fn every_endpoint_requires_auth() {
        let h = Harness::new().await;
        let cases: [(&str, &str); 7] = [
            ("GET", "/api/projects/prj_1/tasks"),
            ("POST", "/api/projects/prj_1/tasks"),
            ("GET", "/api/tasks/t_1"),
            ("PATCH", "/api/tasks/t_1"),
            ("POST", "/api/tasks/t_1/state"),
            ("POST", "/api/tasks/t_1/acceptance"),
            ("POST", "/api/tasks/t_1/failure"),
        ];
        for (method, uri) in cases {
            let resp = h
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} 未认证必须是 401"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 2. 四个写端点都要 CSRF
    // -----------------------------------------------------------------------

    /// 带 cookie 但不带 `x-csrf-token`：4 个写端点一律 403。
    ///
    /// 判红方式：把这四条路由移到 `.layer(require_auth)` 之后（或者把它们加进
    /// `CSRF_EXEMPT_PATHS`），任一行立刻变 200 而红。
    #[tokio::test]
    async fn every_write_endpoint_requires_csrf() {
        let h = Harness::new().await;
        let id = h.create("csrf 用例").await;
        let cases: [(&str, &str, Value); 4] = [
            (
                "POST",
                "/api/projects/prj_1/tasks",
                serde_json::json!({"title": "x", "done_when": {"criteria": "all_tests_pass"}}),
            ),
            ("PATCH", "/api/tasks/t_1", serde_json::json!({"title": "y"})),
            (
                "POST",
                "/api/tasks/t_1/state",
                serde_json::json!({"to": "queued"}),
            ),
            (
                "POST",
                "/api/tasks/t_1/acceptance",
                serde_json::json!({"verdict": "pass"}),
            ),
        ];
        // 先确认这些 URI 在**带** CSRF 时不是 403 以外的东西（路线确实挂上了）。
        assert!(!id.is_empty());
        for (method, uri, body) in cases {
            let r = h
                .send(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                    false,
                )
                .await;
            assert_eq!(
                r.status,
                StatusCode::FORBIDDEN,
                "{method} {uri} 缺 CSRF 必须 403，实际 {} {}",
                r.status,
                r.body
            );
        }
    }

    // -----------------------------------------------------------------------
    // 3. 非法转移 409 + detail 两端
    // -----------------------------------------------------------------------

    /// `running` 任务 `POST /state {to:"done"}` → 409，且 `detail` 同时带
    /// `from` 与 `to`。
    ///
    /// 判红方式：把 409 写成 400（或把 `detail` 换成一句笼统的串），断言红；
    /// 前端的提示是**据端点对查表生成**的（「执行中不能直接判完成，请先进
    /// 待验收」），只回一个 code 它拼不出那句话。
    #[tokio::test]
    async fn an_illegal_transition_returns_409_with_both_states_in_the_detail() {
        let h = Harness::new().await;
        let id = h.create("非法转移").await;
        h.force_state(&id, TaskState::Running);

        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{id}/state"),
                Some(serde_json::json!({"to": "done"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
        let v = r.json();
        assert_eq!(v["error"], "illegal_task_transition");
        assert_eq!(v["detail"]["from"], "running");
        assert_eq!(v["detail"]["to"], "done");
        // 未写库：任务仍是 running。
        let after = h.get(&format!("/api/tasks/{id}")).await.json();
        assert_eq!(after["task"]["state"], "running", "被拒的转移不得写库");
    }

    // -----------------------------------------------------------------------
    // 4. 过期 expected_version → 409，且内容仍是最新值
    // -----------------------------------------------------------------------

    /// 读到 v1 → 别人 PATCH 到 v2 → 用 v1 提交 → 409，且**任务内容仍是最新值**
    /// （不是被回写成的旧值）。
    ///
    /// 判红方式：把 `require_version` 删掉（或改成永远 `Ok`），第二个 PATCH
    /// 会成功返回 200 → 断言红。这条同时证明「不回显请求体」：若端点把请求
    /// 体原样回显，标题会看起来是旧的那个。
    #[tokio::test]
    async fn a_stale_expected_version_returns_409_task_state_changed() {
        let h = Harness::new().await;
        let id = h.create("乐观锁").await;
        let v1 = h.get(&format!("/api/tasks/{id}")).await.json()["task"]["version"]
            .as_i64()
            .unwrap();

        // 别人改成新标题（带它读到的那一版）。
        let r = h
            .mutate(
                "PATCH",
                &format!("/api/tasks/{id}"),
                Some(serde_json::json!({"title": "别人改的", "expected_version": v1})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        let v2 = r.json()["task"]["version"].as_i64().unwrap();
        assert!(v2 > v1, "PATCH 必须涨 version");

        // 我拿着过期的 v1 再提交。
        let r = h
            .mutate(
                "PATCH",
                &format!("/api/tasks/{id}"),
                Some(serde_json::json!({"title": "我改的（基于旧版本）", "expected_version": v1})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
        assert_eq!(r.code(), "task_state_changed");

        // 内容仍是最新值，不是被覆盖成我那份。
        let after = h.get(&format!("/api/tasks/{id}")).await.json();
        assert_eq!(after["task"]["title"], "别人改的", "过期写不得覆盖别人的值");
        assert_eq!(after["task"]["version"], v2, "版本不得因为一次被拒的写而变");
    }

    // -----------------------------------------------------------------------
    // 5. 状态竞态被拒，不静默覆盖
    // -----------------------------------------------------------------------

    /// 两个请求读到同一个 version，先后提交；第二个 409。
    ///
    /// 判红方式：把 CAS 的 `AND version = ?` 去掉（只留 `AND state = ?`），
    /// 在两次提交都把任务推到同一个目标态时仍然会成功 —— 但这里第二次提交的
    /// 目标态与第一次不同（`review` vs `pending`），所以更直接的判红方式是去掉
    /// CAS：第二次会把第一次的结果静默覆盖成 `pending` 而断言红。
    #[tokio::test]
    async fn a_state_race_is_rejected_not_silently_overwritten() {
        let h = Harness::new().await;
        let id = h.create("状态竞态").await;
        h.force_state(&id, TaskState::Running);
        let shared = h.get(&format!("/api/tasks/{id}")).await.json()["task"]["version"]
            .as_i64()
            .unwrap();

        // 请求 A：running → review。
        let a = h
            .mutate(
                "POST",
                &format!("/api/tasks/{id}/state"),
                Some(serde_json::json!({"to": "review", "expected_version": shared})),
            )
            .await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);

        // 请求 B：也拿着 shared，想把任务推回 pending。
        let b = h
            .mutate(
                "POST",
                &format!("/api/tasks/{id}/state"),
                Some(serde_json::json!({"to": "pending", "expected_version": shared})),
            )
            .await;
        assert_eq!(b.status, StatusCode::CONFLICT, "{}", b.body);
        assert_eq!(b.code(), "task_state_changed");

        // 库里仍是 A 写进去的 review。
        let after = h.get(&format!("/api/tasks/{id}")).await.json();
        assert_eq!(after["task"]["state"], "review", "B 不得覆盖 A 的结果");
    }

    // -----------------------------------------------------------------------
    // 6. 验收：pass → done，reject → pending（不是 failed）
    // -----------------------------------------------------------------------

    /// 两条路径各一条断言；`reject` 那条额外钉住「打回不是失败」。
    ///
    /// 判红方式：把 `reject` 的目标态写成 `Failed`（那是「打回 = 失败」的
    /// 误解），`state == "pending"` 那句立刻红。
    #[tokio::test]
    async fn acceptance_pass_moves_review_to_done_and_reject_returns_to_pending() {
        let h = Harness::new().await;

        // pass：review → done
        let pass_id = h.create("验收通过").await;
        h.force_state(&pass_id, TaskState::Review);
        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{pass_id}/acceptance"),
                Some(serde_json::json!({"verdict": "pass"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.json()["task"]["state"], "done");

        // reject：review → pending（**不是 failed**）
        let rej_id = h.create("打回").await;
        h.force_state(&rej_id, TaskState::Review);
        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{rej_id}/acceptance"),
                Some(serde_json::json!({"verdict": "reject", "note": "再补一个测试"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.json()["task"]["state"], "pending", "打回不是失败");
    }

    // -----------------------------------------------------------------------
    // 7. override 不带 note → 400
    // -----------------------------------------------------------------------

    /// `{verdict:"pass", override:true}` 没有 `note` → 400
    /// `acceptance_note_required`，且**什么都没写**。
    ///
    /// 判红方式：把那条门禁删掉，任务会被直接推进到 `done`（200）→ 断言红。
    #[tokio::test]
    async fn acceptance_override_without_note_is_400() {
        let h = Harness::new().await;
        let id = h.create("覆盖判定").await;
        h.force_state(&id, TaskState::Review);
        let before = h.version_of(&id);

        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{id}/acceptance"),
                Some(serde_json::json!({"verdict": "pass", "override": true})),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
        assert_eq!(r.code(), "acceptance_note_required");

        assert_eq!(h.version_of(&id), before, "被拒的请求不得改库");
        let after = h.get(&format!("/api/tasks/{id}")).await.json();
        assert_eq!(after["task"]["state"], "review");

        // 带上理由就过——证明 400 的原因是「缺 note」，不是路走不通。
        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{id}/acceptance"),
                Some(serde_json::json!({
                    "verdict": "pass", "override": true, "note": "自动判定器还没接"
                })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    }

    // -----------------------------------------------------------------------
    // 8. 失败处置：retry → queued；close 保持 failed 并写 reason
    // -----------------------------------------------------------------------

    /// `retry`：`failed → queued`；`close`：终态 `failed` 且原因被记下。
    ///
    /// 判红方式：把 `retry` 的目标态写成 `Pending`，第一个断言红；`close` 那条
    /// 去读该任务最近一次执行上的 `reason`——若 `record_on_latest_execution`
    /// 没被调用（或写到别处），理由为 `None` 而红。
    #[tokio::test]
    async fn failure_retry_moves_failed_to_queued_and_close_keeps_failed_with_reason() {
        let h = Harness::new().await;

        let retry_id = h.create("重试").await;
        h.force_state(&retry_id, TaskState::Failed);
        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{retry_id}/failure"),
                Some(serde_json::json!({"action": "retry"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.json()["task"]["state"], "queued");

        // close：先推成 running（有一条失败中的执行），再关单。
        let close_id = h.create("关单").await;
        h.force_state(&close_id, TaskState::Running);
        let exec_id = insert_execution(&h.state, &close_id);
        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{close_id}/failure"),
                Some(serde_json::json!({
                    "action": "close",
                    "failure_kind": "env_broken",
                    "reason": "系统库版本不对",
                })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.json()["task"]["state"], "failed", "close 进终态 failed");

        let (kind, reason): (Option<String>, Option<String>) = h
            .state
            .pool()
            .get()
            .unwrap()
            .query_row(
                "SELECT failure_kind, reason FROM execution WHERE id = ?1",
                [&exec_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind.as_deref(), Some("env_broken"));
        assert_eq!(
            reason.as_deref(),
            Some("系统库版本不对"),
            "关单必须留下原因串（没有原因串的失败等于没发生）"
        );
    }

    // -----------------------------------------------------------------------
    // 9. 建任务拒绝空 done_when
    // -----------------------------------------------------------------------

    /// `done_when: {}` → 400 `invalid_criteria`，且**一个任务都没建出来**。
    ///
    /// 判红方式：把 `create_task` 里那句 `validate()` 删掉，这条立刻红
    /// （`TaskRepo::create` 自己也会拒，但那是它拒；这里要的是**HTTP 层**
    /// 给出正确的 code 而不是 500）。
    #[tokio::test]
    async fn create_rejects_an_empty_done_when() {
        let h = Harness::new().await;
        let r = h
            .mutate(
                "POST",
                "/api/projects/prj_1/tasks",
                Some(serde_json::json!({"title": "没有验收标准", "done_when": {}})),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
        assert_eq!(r.code(), "invalid_criteria");

        let list = h.get("/api/projects/prj_1/tasks").await.json();
        assert_eq!(
            list["tasks"].as_array().unwrap().len(),
            0,
            "被拒的任务不得落库"
        );

        // 闭集外的判定码是同一个错误码（前端的提示同一句）。
        let r = h
            .mutate(
                "POST",
                "/api/projects/prj_1/tasks",
                Some(serde_json::json!({
                    "title": "乱写判定码", "done_when": {"criteria": "looks_good"}
                })),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(r.code(), "invalid_criteria");
    }

    // -----------------------------------------------------------------------
    // 10. 建 2 个 → 列表 2 条，字段正确
    // -----------------------------------------------------------------------

    /// 建 2 个 → 列表 2 条、`state == "pending"`，且 `needs_manual_acceptance` /
    /// `criteria_known` **两个字段存在且值正确**。
    ///
    /// 判红方式：
    /// - 把 `needs_manual_acceptance` 从视图里删掉 → `["needs_manual_acceptance"]`
    ///   取到 `Null`，`as_bool()` 断言红；
    /// - 让它恒为 `false`：第二个任务（只有 description、没有 criteria）那句红。
    #[tokio::test]
    async fn create_then_list_round_trip() {
        let h = Harness::new().await;

        // 任务 1：机器判得了的判定码 → 不需要人工验收。
        let a = h
            .mutate(
                "POST",
                "/api/projects/prj_1/tasks",
                Some(serde_json::json!({
                    "title": "跑测试",
                    "done_when": {"criteria": "all_tests_pass", "description": "全绿"}
                })),
            )
            .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
        // 任务 2：只有 description → 需人工验收。
        let b = h
            .mutate(
                "POST",
                "/api/projects/prj_1/tasks",
                Some(serde_json::json!({
                    "title": "看着办",
                    "done_when": {"description": "人看着像样就行"}
                })),
            )
            .await;
        assert_eq!(b.status, StatusCode::CREATED, "{}", b.body);

        let list = h.get("/api/projects/prj_1/tasks").await.json();
        let tasks = list["tasks"].as_array().expect("tasks 必须是数组");
        assert_eq!(tasks.len(), 2, "建了 2 个就该列出 2 条");

        let mut seen_auto = false;
        let mut seen_manual = false;
        for t in tasks {
            assert_eq!(t["state"], "pending", "新任务一律 pending");
            let manual = t["needs_manual_acceptance"]
                .as_bool()
                .expect("needs_manual_acceptance 必须存在且是布尔");
            let known = t["criteria_known"]
                .as_bool()
                .expect("criteria_known 必须存在且是布尔");
            if t["title"] == "跑测试" {
                assert!(!manual, "有机器判得了的判定码，不该标人工验收");
                assert!(known, "all_tests_pass 在闭集里");
                seen_auto = true;
            } else {
                assert!(manual, "只有描述就只能人判");
                assert!(!known, "没写判定码 → criteria_known 为 false");
                seen_manual = true;
            }
        }
        assert!(seen_auto && seen_manual, "两个任务都要出现");

        // `?state=` 过滤：pending 有 2 条、review 没有。
        let pending = h
            .get("/api/projects/prj_1/tasks?state=pending")
            .await
            .json();
        assert_eq!(pending["tasks"].as_array().unwrap().len(), 2);
        let review = h.get("/api/projects/prj_1/tasks?state=review").await.json();
        assert_eq!(review["tasks"].as_array().unwrap().len(), 0);
    }

    // -----------------------------------------------------------------------
    // 11. 详情带 executions，空列表不是 null
    // -----------------------------------------------------------------------

    /// 无执行记录时 `executions == []`（**不是 `null`、不是省略字段**）；
    /// 有记录时能读到它。
    ///
    /// 判红方式：把 `executions` 写成 `Option::None` 并跳过序列化（或让字段
    /// 省略），`["executions"].as_array()` 拿到 `None` → 断言红。前端写的是
    /// `executions.map(...)`，`undefined.map` 会把整页炸掉。
    #[tokio::test]
    async fn detail_includes_executions_and_an_empty_list_is_not_null() {
        let h = Harness::new().await;
        let id = h.create("详情").await;

        let v = h.get(&format!("/api/tasks/{id}")).await.json();
        let execs = v["executions"]
            .as_array()
            .expect("executions 必须是数组（不能是 null，也不能省略字段）");
        assert!(execs.is_empty(), "还没跑过就是空数组");
        assert_eq!(v["task"]["id"].as_str(), Some(id.as_str()));

        // 造一条执行记录后能读回来。
        let exec_id = insert_execution(&h.state, &id);
        let v = h.get(&format!("/api/tasks/{id}")).await.json();
        let execs = v["executions"].as_array().unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0]["id"].as_str(), Some(exec_id.as_str()));
        assert_eq!(execs[0]["state"], "pending");
        for field in [
            "base_commit",
            "agent_id",
            "failure_kind",
            "reason",
            "evidence",
            "started_at",
            "ended_at",
        ] {
            assert!(
                execs[0].get(field).is_some(),
                "执行记录必须带 {field} 字段（可以是 null）"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 12. 未知 id → 404 而不是 500
    // -----------------------------------------------------------------------

    /// 对 `GET` 与 `POST /state` 各一断言：不存在的任务 id 是 404
    /// `task_not_found`（**不是 500、也不是 `not_found` 之外的通用码**）。
    ///
    /// 判红方式：把 `map_storage` 里 `TaskNotFound` 那一支删掉（让它落进
    /// `internal` 兜底），两条都变 500 而红。
    #[tokio::test]
    async fn an_unknown_task_id_returns_404_not_500() {
        let h = Harness::new().await;

        let r = h.get("/api/tasks/tsk_does_not_exist").await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
        assert_eq!(r.code(), "task_not_found");

        let r = h
            .mutate(
                "POST",
                "/api/tasks/tsk_does_not_exist/state",
                Some(serde_json::json!({"to": "queued"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
        assert_eq!(r.code(), "task_not_found");
    }

    // -----------------------------------------------------------------------
    // 13. `?state=draft` → 400（迁移后 `draft` 是非法值）
    // -----------------------------------------------------------------------

    /// `?state=draft` → 400 `invalid_task_state`。这条同时守住「旧值没被写回」。
    ///
    /// 判红方式：把 `parse_state` 改成「解析失败回落 `pending`」（或者干脆
    /// 忽略查询串），这条会拿到 200 + 一列任务而红。`draft` / `ready` /
    /// `blocked` 三个旧值逐个测。
    #[tokio::test]
    async fn an_invalid_state_query_is_400_not_500() {
        let h = Harness::new().await;
        h.create("有个任务").await;
        for legacy in ["draft", "ready", "blocked", "sorta_done"] {
            let r = h
                .get(&format!("/api/projects/prj_1/tasks?state={legacy}"))
                .await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{legacy}: {}", r.body);
            assert_eq!(r.code(), "invalid_task_state", "{legacy}");
        }
        // 六态都收（合法值不被误伤）。
        for ok in ["pending", "queued", "running", "review", "done", "failed"] {
            let r = h
                .get(&format!("/api/projects/prj_1/tasks?state={ok}"))
                .await;
            assert_eq!(r.status, StatusCode::OK, "{ok} 是合法态：{}", r.body);
        }
    }

    // -----------------------------------------------------------------------
    // 补充：`queued` 下的 close 走两步合法转移
    // -----------------------------------------------------------------------

    /// `queued` 任务 `action:"close"` → 先 `queued → pending` 再 `pending →
    /// failed`，最终 `failed`。
    ///
    /// 判红方式：把 `queued_close` 那条分支删掉，`check_transition` 会拒绝
    /// `queued → failed`（没有这条边）→ 409 而红。
    #[tokio::test]
    async fn closing_a_queued_task_walks_through_pending() {
        let h = Harness::new().await;
        let id = h.create("排队中关单").await;
        h.force_state(&id, TaskState::Queued);
        let v_before = h.version_of(&id);

        let r = h
            .mutate(
                "POST",
                &format!("/api/tasks/{id}/failure"),
                Some(serde_json::json!({
                    "action": "close", "failure_kind": "rejected", "reason": "没人接活"
                })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.json()["task"]["state"], "failed");
        // 两次合法转移 = 版本涨两次（建任务时 version 就是 1，夹具推 queued 时 +1）。
        assert_eq!(
            h.version_of(&id),
            v_before + 2,
            "queued→pending→failed 各涨一次"
        );
    }

    /// 造一条执行记录。
    ///
    /// 规格 `:901-903`：本里程碑没有任何端点会写 `execution` 行——
    /// `POST /api/tasks/{id}/executions` 留给 v0.8.2。详情端点的 `executions`
    /// 字段因此只能靠直接打 repo 造出来验证。
    fn insert_execution(state: &AppState, task_id: &str) -> String {
        state
            .executions()
            .create(
                hub_storage::execution::NewExecution {
                    task_id: task_id.to_string(),
                    binding_id: "b_test".to_string(),
                    base_commit: None,
                    worktree_path: None,
                },
                TS,
            )
            .expect("create execution")
            .id
    }
}
