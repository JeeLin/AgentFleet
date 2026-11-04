//! `/api/agents/{name}`：**从 agent 侧看任务**（`v0.7.0` 子任务 7）。
//!
//! # 这一页回答的问题
//!
//! 设备页回答「这台机器怎么了」，agent 页回答「**这个 agent 靠不靠得住**」。
//! 后者不挂在某台设备上——同一个 agent 可能装在五台机器上，用户想知道的是
//! 「让 claude-code 干活，成不成」。
//!
//! | 字段 | 回答什么 | 来源 |
//! |---|---|---|
//! | `available` | 现在能不能用 | `agent_runtime`（至少一台可用即为真） |
//! | `devices[]` | 装在哪几台机器上 | `agent_runtime` |
//! | `task_history[]` | 最近跑过哪些任务 | `execution` ⋈ `device` |
//! | `success_rate` | 成功率多少 | `task_history` 的聚合 |
//!
//! # `task_history` 的关联方式
//!
//! `execution` 表上**没有 agent 列**。链条是：
//!
//! ```text
//! execution.binding_id → binding.device_id → device.id → agent_runtime(node_id, agent_id)
//! ```
//!
//! 也就是说，「这条执行是哪个 agent 跑的」这件事在库里**没有被记录**，
//! 只能按「那台设备上装了这个 agent」反推。这是本里程碑能给出的**最强**
//! 结论，但它不等于因果：同一台设备上装了三个 agent，它的每条执行都会被
//! 算进这三个 agent 的历史里。
//!
//! 这一点必须写在页面上（见 `task_history_caveat`），不能只写在代码注释里——
//! 用户看到「成功率 83%」时会当真，而如果那个数字的分母包含了别的 agent 干的
//! 活，它就是个错的数字。
//!
//! **不修的做法**是给 `execution` 加一列 `agent_id`。那需要改 schema 与派活
//! 链路（谁在派活时知道 agent？现在没人知道——派活只到「设备 + 连接器」）。
//! 把这个缺口补上是一个独立的设计动作，不在本子任务的范围里；本子任务**如实
//! 标出**，而不是让统计看起来比实际精确。

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use hub_storage::binding::BindingRepo;
use hub_storage::execution::{ExecutionRepo, ExecutionState};

use crate::routes::devices::FailureBreakdown;
use crate::{ApiError, AppState};

/// 一个 agent 的详情。
#[derive(Debug, Clone, Serialize)]
pub struct AgentDetail {
    pub name: String,
    /// 现在能不能用（至少一台设备上可用）。
    pub available: bool,
    /// 说它不可用的原因；设备没给原因时为 `null`（不编一句）。
    pub error: Option<String>,
    /// 装在哪些设备上，各自可不可用。
    pub devices: Vec<AgentOnDevice>,
    /// 这个 agent 所在设备上跑过的任务，最近的在前。
    pub task_history: Vec<AgentTaskRun>,
    /// 执行结果分布。`success_rate` 由它算。
    pub breakdown: FailureBreakdown,
    /// 成功率（0–100 整数）。
    ///
    /// **`null` 不是 0**：一次执行都没有时显示「—」而不是「0%」。
    /// 0/0 是「还没跑过」，0/10 是「一次都没成」——两句话完全不同。
    pub success_rate: Option<u8>,
    /// `task_history` 的口径说明，页面**必须**显示。
    ///
    /// 见文件头：关联是按「那台设备上装了这个 agent」反推的，不是执行记录里
    /// 写着的。用户看到成功率时会当真，所以这句话不能只活在注释里。
    pub task_history_caveat: &'static str,
}

/// 一台设备上的这个 agent。
#[derive(Debug, Clone, Serialize)]
pub struct AgentOnDevice {
    pub node_id: String,
    pub device_name: String,
    pub available: bool,
    pub probe_error: Option<String>,
    /// 这台设备上这个 agent 的执行条数（同样是反推口径）。
    pub executions: u64,
}

/// 一条「跑过的任务」。
#[derive(Debug, Clone, Serialize)]
pub struct AgentTaskRun {
    pub execution_id: String,
    pub task_id: String,
    pub task_title: Option<String>,
    pub project_id: String,
    pub project_name: String,
    pub device_id: String,
    pub state: &'static str,
    pub created_at: String,
}

/// 那句话的原文。前后端共用同一份，避免页面自己编一句不一样的。
pub const TASK_HISTORY_CAVEAT: &str =
    "执行记录里没有记「这条是哪个 agent 跑的」，所以下面按「执行所在的那台设备上装了这个 agent」\
     反推。同一台设备上装了多个 agent 时，它们会共享同一批执行。";

/// `GET /api/agents/{name}`。
pub async fn get_agent(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<AgentDetail>, ApiError> {
    let devices = state
        .devices()
        .list()
        .map_err(|e| ApiError::internal_with("cannot list devices", e))?;

    // 一次扫过全部设备，顺手把「这个 agent 装在哪几台机器上」收齐。
    // 设备数量是几十的量级，逐台查是可以的；真到上千台时这条要改成一条
    // 带 `WHERE name = ?` 的查询，而不是加缓存。
    let mut on_devices: Vec<(String, String, bool, Option<String>)> = Vec::new();
    for d in &devices {
        let rows = match state.agent_runtime().list_for_node(&d.id) {
            Ok(rows) => rows,
            Err(e) => {
                // 单台设备读不出来不该让整页 500：其它设备上的事实仍然成立。
                // coverage: excluded — 要走到这里必须把 `agent_runtime` 读坏
                // （连接池取不到 / SQLite 报错）。这条兜底的意义正是「库坏了一台
                // 机器时页面还能显示别的」，而那种故障没有可控的测试手段。
                tracing::warn!(node_id = %d.id, error = %e, "cannot read agent_runtime");
                continue;
            }
        };
        if let Some(r) = rows.into_iter().find(|r| r.name == name) {
            on_devices.push((d.id.clone(), d.name.clone(), r.available, r.probe_error));
        }
    }

    if on_devices.is_empty() {
        // 一次都没装过 → 404。而不是一个「全都不可用」的空壳：后者会让用户
        // 以为装坏了，去查一台根本没装它的机器。
        return Err(ApiError::not_found());
    }

    let binding_repo = BindingRepo::new(state.pool().clone());
    let execution_repo = ExecutionRepo::new(state.pool().clone());
    let mut breakdown = FailureBreakdown::default();
    let mut task_history: Vec<AgentTaskRun> = Vec::new();
    let mut agents = Vec::with_capacity(on_devices.len());

    for (node_id, device_name, available, probe_error) in &on_devices {
        let bindings = binding_repo
            .list_for_device(node_id)
            .map_err(|e| ApiError::internal_with("cannot list bindings", e))?;
        let mut device_runs = 0u64;

        for b in &bindings {
            let runs = execution_repo
                .list_for_binding(&b.id)
                .map_err(|e| ApiError::internal_with("cannot list executions", e))?;
            for run in runs {
                device_runs += 1;
                match run.state {
                    ExecutionState::Succeeded => breakdown.succeeded += 1,
                    ExecutionState::Failed => breakdown.failed += 1,
                    ExecutionState::Aborted => breakdown.aborted += 1,
                    ExecutionState::Pending | ExecutionState::Running => breakdown.in_flight += 1,
                }
                task_history.push(AgentTaskRun {
                    execution_id: run.id,
                    task_title: task_title(&state, &run.task_id),
                    task_id: run.task_id,
                    project_id: b.project_id.clone(),
                    project_name: project_name(&state, &b.project_id),
                    device_id: node_id.clone(),
                    state: run.state.as_str(),
                    created_at: run.created_at,
                });
            }
        }

        agents.push(AgentOnDevice {
            node_id: node_id.clone(),
            device_name: device_name.clone(),
            available: *available,
            probe_error: probe_error.clone(),
            executions: device_runs,
        });
    }

    // **最近的在前**。`created_at` 是 `unix:<秒>` 字符串，定长就能按字典序排；
    // 同一秒里按 id 兜底，免得两次刷新的顺序不一样。
    task_history.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.execution_id.cmp(&a.execution_id))
    });

    // 目录页的口径：**有一台可用即显示可用**（与 `list_agents` 同一条规则）。
    let available = on_devices.iter().any(|(_, _, a, _)| *a);
    let error = if available {
        None
    } else {
        // 全都不行时，把各台的 probe_error 去重后拼起来——只报「不可用」
        // 用户不知道该修哪台机器。没有原因的（None）留空，不编一句。
        let mut reasons: Vec<String> = on_devices
            .iter()
            .filter_map(|(_, _, _, e)| e.clone())
            .collect();
        reasons.sort();
        reasons.dedup();
        (!reasons.is_empty()).then(|| reasons.join("；"))
    };

    let success_rate = breakdown.success_rate();

    Ok(Json(AgentDetail {
        name,
        available,
        error,
        devices: agents,
        task_history,
        breakdown,
        success_rate,
        task_history_caveat: TASK_HISTORY_CAVEAT,
    }))
}

/// 任务标题。任务不存在时 `None`——老执行的 `task_id` 可能指向一个
/// 早已不存在的行（v5 刻意不给 `execution.task_id` 加外键）。编一个
/// 「（已删除）」当标题会让用户以为真有个任务叫这个名字。
fn task_title(state: &AppState, task_id: &str) -> Option<String> {
    hub_storage::task::TaskRepo::new(state.pool().clone())
        .get(task_id)
        .ok()
        .flatten()
        .map(|t| t.title)
}

/// 项目名。不存在时退回 id（项目只归档不删除）。
fn project_name(state: &AppState, project_id: &str) -> String {
    match hub_storage::project::ProjectRepo::new(state.pool().clone()).get(project_id) {
        Ok(Some(p)) => p.name,
        // coverage: excluded — 项目只归档不删除，所以 `Ok(None)` 正常走不到；
        // `Err` 是读库失败。两条都退回 id 是为了「有一个能让人对上的标签」
        // 而不是留一个空标题。
        Ok(None) | Err(_) => project_id.to_string(),
    }
}
