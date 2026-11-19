//! `task_dispatch_ack` 的接收方（v0.8.2 子任务 5）：设备回报 → 任务与执行状态、
//! 失败留痕、重试结论、清理预演。
//!
//! 七条用例逐条对应里程碑 §3 的测试标准：
//!
//! | 标准 | 用例 |
//! |---|---|
//! | ③ `running` → 任务 `queued → running`、执行 `Running` | [`a_running_ack_moves_the_task_to_running`] |
//! | ④ `succeeded` → 任务 `running → review`、执行 `Succeeded` | [`a_succeeded_ack_moves_the_task_to_review`] |
//! | ⑤ `failed{env_broken}` → 三列落库、任务 `failed`、`DifferentDevice` | [`an_env_broken_ack_records_the_failure_and_plans_a_new_device`] |
//! | ⑥ 换设备时磁盘不够 → 没被选中 | [`a_retry_never_picks_a_device_that_fails_the_disk_gate`] |
//! | ⑦ `agent_error` 换 agent 不换设备 | [`an_agent_error_retry_keeps_the_same_device`] |
//! | ⑧ 非法边被拒、状态不倒退 | [`a_stale_ack_is_ignored_and_never_rewinds_the_state`] |
//! | ⑨ `failure_kind` 不在闭集里 → 不落库 | [`a_failure_kind_outside_the_closed_set_is_not_stored`] |
//! | ⑩ `disk_exhausted` → `cleanup::on_failure_ack` 真被调用 | [`a_disk_exhausted_ack_records_the_cleanup_preview`] |
//!
//! # 为什么走内存连接表 + 直接调处理函数
//!
//! 真 WS 上「设备能否收到帧」取决于写协程何时把控制面那一帧取出，而本文件要
//! 证明的是**Hub 侧收到 ack 之后改了什么**。`scheduler_flow.rs` 同一条立场
//! （见它的文件头）。真 WS 的接线由 `an_ack_over_the_ws_reaches_the_state_machine`
//! 一条用例单独钉住——它是「分支真的挂在 `relay` 里」的唯一证据。
//!
//! `clippy.toml` 的 `allow-*-in-tests` 覆盖不到本文件顶层的 helper
//! （见 `AGENTS.md`），所以文件头自带豁免。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

mod common;

use std::time::Duration;

use hub_core::reports::Report;
use hub_core::schedule::RetryPlan;
use hub_server::dispatch_ack::AckOutcome;
use hub_server::scheduler::{TASK_RETRY_PLANNED_KIND, TASK_TRAJECTORY_NAMESPACE};
use hub_storage::execution::{ExecutionRepo, ExecutionState, FailureKind};
use hub_storage::project::{NewProject, ProjectRepo, ProjectShape};
use hub_storage::task::{DoneWhen, NewTask, TaskState, TaskView};
use hub_storage::{BaseRevStatus, BindingRepo, BindingSpec, CleanupPolicy, SessionRepo, TaskRepo};
use protocol::message::{DispatchStage, TaskDispatchAck};
use protocol::Frame;

use crate::common::Hub;

/// 绑定目录。**必须是绝对路径**：`hub_core::schedule` 的「拿得到代码」那一层
/// 用 `fixed_dir.starts_with('/')` 判定。
const FIXED_DIR: &str = "/srv/af/worktrees";

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn task_repo(hub: &Hub) -> TaskRepo {
    TaskRepo::new(hub.pool())
}

fn execution_repo(hub: &Hub) -> ExecutionRepo {
    ExecutionRepo::new(hub.pool())
}

fn build_project(hub: &Hub, name: &str) -> String {
    ProjectRepo::new(hub.pool())
        .create(
            NewProject {
                name: name.to_string(),
                shape: ProjectShape::Central,
                theme_color: None,
                upstream_url: None,
            },
            "unix:1700000000",
        )
        .unwrap()
        .id
}

fn bind(hub: &Hub, project_id: &str, device_id: &str) -> String {
    BindingRepo::new(hub.pool())
        .upsert(
            project_id,
            device_id,
            BindingSpec {
                fixed_dir: FIXED_DIR.to_string(),
                // 不设配额：配额会把 ⑤ 的判据变成「配额够不够」，而本组用例要的是
                // 「这台设备本身的余量够不够」。
                disk_quota_bytes: None,
                cleanup_policy: CleanupPolicy::Balanced,
            },
            "unix:1700000000",
        )
        .unwrap()
        .id
}

/// 设备上线：内存心跳 + 一条连接。
///
/// `free_disk_mb` 单独给出来，因为 ⑥ 那条用例要让两台设备的磁盘差出一个量级
/// ——「换一台不管空间」的错实现只会在那种输入下露出来。
fn device_online(
    hub: &Hub,
    node_id: &str,
    free_disk_mb: u64,
) -> tokio::sync::mpsc::Receiver<Frame> {
    hub.state.reports().record(
        node_id,
        Report {
            node_version: "0.1.0".to_string(),
            capabilities: serde_json::json!({
                "connectors": ["git"],
                "agents": [
                    {"id": "opencode", "name": "opencode", "version": "0.0.1", "available": true},
                    {"id": "codex", "name": "codex", "version": "0.0.1", "available": true},
                ],
            }),
            resources: serde_json::json!({"disk": {"workspace_free_mb": free_disk_mb}}),
            workspace_roots: Vec::new(),
            at: std::time::Instant::now(),
        },
    );
    let conn_id = format!("conn-{node_id}");
    let (control_rx, _data_rx) = hub.state.outbound().register(&conn_id);
    hub.state.connections().insert(
        node_id.to_string(),
        hub_core::ConnHandle { conn_id, gen: 1 },
    );
    control_rx
}

impl Hub {
    fn create_task(&self, project_id: &str, title: &str) -> TaskView {
        task_repo(self)
            .create(
                NewTask::new(
                    project_id,
                    title,
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通即可".to_string()),
                    },
                ),
                "unix:1700000000",
            )
            .unwrap()
    }

    /// 建一个带 `allowed_agents` 的任务（`["codex"]` 这类原文 JSON 字符串）。
    ///
    /// 走 `TaskPatch` 而不是直接改库：`task.allowed_agents` 的写入只有那一个
    /// 入口（它同时涨 version），测试不该另开一条能绕过它的路。
    fn create_task_with_agents(&self, project_id: &str, title: &str, agents: &str) -> TaskView {
        let task = self.create_task(project_id, title);
        task_repo(self)
            .update(
                &task.id,
                &hub_storage::TaskPatch {
                    allowed_agents: Some(Some(agents.to_string())),
                    ..hub_storage::TaskPatch::default()
                },
                "unix:1700000005",
            )
            .unwrap();
        task_repo(self).get(&task.id).unwrap().unwrap()
    }
}

/// 派一次任务并返回 `(execution_id, node_id)`。
fn dispatch(hub: &Hub, task: &TaskView) -> (String, String) {
    let (outcome, push) = hub.state.scheduler().route_task(task);
    assert_eq!(push, None, "帧必须真的进了控制面队列：{outcome:?}");
    match outcome {
        hub_server::scheduler::RouteOutcome::Dispatched {
            node_id,
            execution_id,
        } => (execution_id, node_id),
        other => panic!("必须派出去，实际 {other:?}"),
    }
}

fn ack(execution_id: &str, stage: DispatchStage) -> TaskDispatchAck {
    TaskDispatchAck {
        execution_id: execution_id.to_string(),
        stage,
        reason: None,
        evidence: None,
        failure_kind: None,
        items: None,
    }
}

fn failed_ack(execution_id: &str, kind: &str) -> TaskDispatchAck {
    TaskDispatchAck {
        reason: Some(format!("{kind} happened")),
        evidence: Some("exit code 1".to_string()),
        failure_kind: Some(kind.to_string()),
        ..ack(execution_id, DispatchStage::Failed)
    }
}

/// 执行当前状态（绕过任何缓存）。
fn exec_state(hub: &Hub, execution_id: &str) -> ExecutionState {
    execution_repo(hub)
        .get(execution_id)
        .unwrap()
        .expect("execution row")
        .state
}

fn task_state(hub: &Hub, task_id: &str) -> TaskState {
    task_repo(hub).get(task_id).unwrap().unwrap().state
}

/// 一个任务在轨迹里的 `(kind, payload)`，按物理顺序。
fn facts(hub: &Hub, task_id: &str) -> Vec<(String, String)> {
    let session_id = format!("{TASK_TRAJECTORY_NAMESPACE}{task_id}");
    hub.state
        .trajectory()
        .list(&session_id, 0, 100)
        .unwrap()
        .into_iter()
        .map(|e| (e.kind, e.payload))
        .collect()
}

/// 最后一个指定 kind 的轨迹载荷。
fn last_fact(hub: &Hub, task_id: &str, kind: &str) -> Option<serde_json::Value> {
    let mut hits: Vec<String> = facts(hub, task_id)
        .into_iter()
        .filter(|(k, _)| k == kind)
        .map(|(_, p)| p)
        .collect();
    hits.pop()
        .map(|p| serde_json::from_str::<serde_json::Value>(&p).unwrap())
}

/// 把任务推到 `queued`（不派发），再用一条执行挂上去——用来测「重复 ack」这类
/// 不经过路由的路径。
fn queued_task_with_execution(hub: &Hub, task: &TaskView, binding_id: &str) -> String {
    task_repo(hub)
        .set_state(&task.id, TaskState::Queued, "unix:1700000001")
        .unwrap();
    execution_repo(hub)
        .create(
            hub_storage::NewExecution {
                task_id: task.id.clone(),
                binding_id: binding_id.to_string(),
                base_commit: Some("deadbeef".to_string()),
                worktree_path: Some(format!("{FIXED_DIR}/{}", task.id)),
            },
            "unix:1700000002",
        )
        .unwrap()
        .id
}

// ---------------------------------------------------------------------------
// ③ running → 任务 queued → running、执行 Running
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_running_ack_moves_the_task_to_running() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_1";
    bind(&hub, &project_id, node_id);
    let _frames = device_online(&hub, node_id, 102_400);

    let task = hub.create_task(&project_id, "跑起来");
    let (execution_id, chosen) = dispatch(&hub, &task);
    assert_eq!(chosen, node_id);
    // 前提：派发之后任务在 `queued`（这是 `queued → running` 的起点）。
    assert_eq!(task_state(&hub, &task.id), TaskState::Queued);

    let outcome = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Running),
    );

    // 1. 结论说清两端都动了。
    assert_eq!(
        outcome,
        AckOutcome::Applied {
            execution: ExecutionState::Running,
            task: Some(TaskState::Running),
        }
    );

    // 2. 执行 → running，且 `started_at` 被填上（「跑了多久」的起点）。
    let exec = execution_repo(&hub)
        .get(&execution_id)
        .unwrap()
        .expect("execution");
    assert_eq!(exec.state, ExecutionState::Running);
    assert!(
        exec.started_at.is_some(),
        "running 必须填 started_at：{exec:?}"
    );
    assert_eq!(exec.ended_at, None, "还没结束，ended_at 必须留空");

    // 3. 任务 queued → running（经 task_state 的边）。
    assert_eq!(task_state(&hub, &task.id), TaskState::Running);

    // 4. 轨迹里留了这条 ack（「设备什么时候开跑的」要查得到）。
    let facts = facts(&hub, &task.id);
    assert!(
        facts
            .iter()
            .any(|(k, p)| k == hub_server::scheduler::TASK_ACK_KIND && p.contains("\"running\"")),
        "ack 必须留痕：{facts:?}"
    );
}

// ---------------------------------------------------------------------------
// ④ succeeded → 任务 running → review、执行 Succeeded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_succeeded_ack_moves_the_task_to_review() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_1";
    bind(&hub, &project_id, node_id);
    let _frames = device_online(&hub, node_id, 102_400);

    let task = hub.create_task(&project_id, "跑完");
    let (execution_id, _) = dispatch(&hub, &task);
    // 先把「开跑」这一步走过去，`running → review` 才有起点。
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Running),
    );
    assert_eq!(task_state(&hub, &task.id), TaskState::Running);

    let outcome = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Succeeded),
    );
    assert_eq!(
        outcome,
        AckOutcome::Applied {
            execution: ExecutionState::Succeeded,
            task: Some(TaskState::Review),
        }
    );

    let exec = execution_repo(&hub)
        .get(&execution_id)
        .unwrap()
        .expect("execution");
    assert_eq!(exec.state, ExecutionState::Succeeded);
    assert!(exec.started_at.is_some(), "开跑时填过：{exec:?}");
    assert!(exec.ended_at.is_some(), "收尾必须填 ended_at：{exec:?}");
    assert_eq!(exec.failure_kind, None, "成功不该有失败形态");

    // 任务进 review——**不是 done**：跳过 review 就是跳过价值锚点。
    assert_eq!(task_state(&hub, &task.id), TaskState::Review);
}

// ---------------------------------------------------------------------------
// ⑤ failed{env_broken} → 三列落库、任务 failed、DifferentDevice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_env_broken_ack_records_the_failure_and_plans_a_new_device() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_1";
    bind(&hub, &project_id, node_id);
    let _frames = device_online(&hub, node_id, 102_400);

    let task = hub.create_task(&project_id, "环境坏了");
    let (execution_id, _) = dispatch(&hub, &task);
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Running),
    );

    let outcome = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &failed_ack(&execution_id, "env_broken"),
    );
    assert_eq!(
        outcome,
        AckOutcome::Applied {
            execution: ExecutionState::Failed,
            task: Some(TaskState::Failed),
        }
    );

    // 1. 失败三列真的落库（`set_failure` 是唯一写入口）。
    let exec = execution_repo(&hub)
        .get(&execution_id)
        .unwrap()
        .expect("execution");
    assert_eq!(exec.state, ExecutionState::Failed);
    assert_eq!(exec.failure_kind, Some(FailureKind::EnvBroken));
    assert_eq!(exec.reason.as_deref(), Some("env_broken happened"));
    assert_eq!(exec.evidence.as_deref(), Some("exit code 1"));

    // 2. 任务 running → failed。
    assert_eq!(task_state(&hub, &task.id), TaskState::Failed);

    // 3. `RetryPlan::DifferentDevice` 落在轨迹里，带出处。
    let planned =
        last_fact(&hub, &task.id, TASK_RETRY_PLANNED_KIND).expect("失败之后必须留一条重试结论");
    assert_eq!(planned["plan"], RetryPlan::DifferentDevice.as_str());
    assert_eq!(planned["failure_kind"], "env_broken");
    assert_eq!(
        planned["requires_disk_gate"],
        serde_json::json!(true),
        "换设备要重新过 ⑤ 磁盘门槛：换一台 ≠ 换一台不管空间"
    );
    assert_eq!(
        planned["keeps_device"],
        serde_json::json!(false),
        "DifferentDevice 会换掉 device_id"
    );
}

// ---------------------------------------------------------------------------
// ⑥ 换设备时磁盘不够 → 没被选中（复用 ⑤ 硬门槛）
// ---------------------------------------------------------------------------

/// 唯一的另一台设备磁盘不够 → **重派被拒**，任务留在 `failed`，零新增执行。
///
/// 判红方式：把 `retry_failed_execution` 里的 `pick` 换成「取上一次的 binding
/// 直接再建一条执行」，这条会红——那次重派会在 `n_tight` 上成功，而它的余量
/// 只有 100 MiB（低于 64 MiB 基础开销 + 512 MiB 安全余量的门槛）。
#[tokio::test]
async fn a_retry_never_picks_a_device_that_fails_the_disk_gate() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let rich = "n_rich";
    let tight = "n_tight";
    bind(&hub, &project_id, rich);
    bind(&hub, &project_id, tight);
    let _rich_frames = device_online(&hub, rich, 102_400);
    // 100 MiB：远低于 ⑤ 的门槛（576 MiB）。
    let mut tight_frames = device_online(&hub, tight, 100);

    let task = hub.create_task(&project_id, "只能去磁盘够的那台");
    // 第一台（只可能是 n_rich：n_tight 过不了 ⑤）失败在 `task_failed` 上。
    let (execution_id, chosen) = dispatch(&hub, &task);
    assert_eq!(chosen, rich, "首派就该避开磁盘不够的那台");
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        rich,
        &ack(&execution_id, DispatchStage::Running),
    );
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        rich,
        &failed_ack(&execution_id, "task_failed"),
    );
    assert_eq!(task_state(&hub, &task.id), TaskState::Failed);

    // 重派：`DifferentDevice` 把 `n_rich` 排除掉，只剩 `n_tight`——而它过不了 ⑤。
    let failed = execution_repo(&hub).get(&execution_id).unwrap().unwrap();
    let (outcome, push) = hub.state.scheduler().retry_failed_execution(&failed);

    assert_eq!(
        outcome,
        hub_server::scheduler::RouteOutcome::Rejected {
            rejection: hub_core::Rejection::DiskInsufficient,
        },
        "磁盘不够的设备**不许**被选中（§5.1 末句）"
    );
    assert_eq!(push, None);

    // 三件事都没发生：没新执行、任务还在 failed、那台紧设备没收到帧。
    assert_eq!(
        execution_repo(&hub).list_for_task(&task.id).unwrap().len(),
        1,
        "被磁盘门槛挡下时不许建执行"
    );
    assert_eq!(task_state(&hub, &task.id), TaskState::Failed);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), tight_frames.recv())
            .await
            .is_err(),
        "没选出设备就不该有帧要发"
    );
}

// ---------------------------------------------------------------------------
// ⑦ agent_error 换的是 agent 不是设备
// ---------------------------------------------------------------------------

/// 两台设备都在线、都合格 → `agent_error` 的重派**留在同一台设备上**。
///
/// 判红方式：把 `retry_policy` 的 `AgentError` 臂写成 `DifferentDevice`，这条会
/// 红在「新执行的 binding 换成了另一台设备」。
#[tokio::test]
async fn an_agent_error_retry_keeps_the_same_device() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let first = "n_first";
    let other = "n_other";
    let first_binding = bind(&hub, &project_id, first);
    let other_binding = bind(&hub, &project_id, other);
    let _first_frames = device_online(&hub, first, 102_400);
    let _other_frames = device_online(&hub, other, 102_400);

    // 任务点名 `allowed_agents = codex`：换 agent 才有可换的东西。
    let task = hub.create_task_with_agents(&project_id, "agent 崩了", r#"["codex"]"#);
    let (execution_id, chosen) = dispatch(&hub, &task);
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        &chosen,
        &ack(&execution_id, DispatchStage::Running),
    );
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        &chosen,
        &failed_ack(&execution_id, "agent_error"),
    );

    let planned = last_fact(&hub, &task.id, TASK_RETRY_PLANNED_KIND).expect("重试结论");
    assert_eq!(planned["plan"], RetryPlan::DifferentAgent.as_str());
    assert_eq!(
        planned["keeps_device"],
        serde_json::json!(true),
        "换 agent 时 device_id 必须不变"
    );

    // 重派：候选被收窄到上一次那台设备。
    let failed = execution_repo(&hub).get(&execution_id).unwrap().unwrap();
    let (outcome, push) = hub.state.scheduler().retry_failed_execution(&failed);
    assert_eq!(push, None, "{outcome:?}");
    let hub_server::scheduler::RouteOutcome::Dispatched {
        node_id: retried_on,
        execution_id: new_execution,
    } = outcome
    else {
        panic!("agent_error 必须重派出去");
    };
    assert_eq!(retried_on, chosen, "换的是 agent，不是设备");

    // 新执行的绑定 = 同一个绑定（同一台机器），且与首派是两条不同的执行。
    let new = execution_repo(&hub).get(&new_execution).unwrap().unwrap();
    assert_ne!(new.id, execution_id, "重派是一条新执行");
    assert_eq!(
        new.binding_id,
        if first_binding == failed.binding_id {
            first_binding.clone()
        } else {
            // 首派可能落在任一台（两台都合格、tiebreak 取字典序小的），
            // 但**重派必须落在同一台**。
            failed.binding_id.clone()
        }
    );
    assert_ne!(new.binding_id, other_binding, "不许换到另一台设备上");
    // 新旧 `base_commit` 都在：`task-model.md` §7.3 要求两者都可见。
    assert_eq!(new.base_commit, failed.base_commit, "起点没换");
}

// ---------------------------------------------------------------------------
// ⑧ 非法边被拒：执行已 Succeeded 又来 Running → 不 panic、状态不倒退
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_ack_is_ignored_and_never_rewinds_the_state() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_1";
    let binding_id = bind(&hub, &project_id, node_id);
    let _frames = device_online(&hub, node_id, 102_400);

    // 直接把任务推到 `review` + 执行推到 `succeeded`（重复投递之后的样子）。
    let task = hub.create_task(&project_id, "已经成功");
    let execution_id = queued_task_with_execution(&hub, &task, &binding_id);
    task_repo(&hub)
        .set_state(&task.id, TaskState::Running, "unix:1700000003")
        .unwrap();
    task_repo(&hub)
        .set_state(&task.id, TaskState::Review, "unix:1700000004")
        .unwrap();
    execution_repo(&hub)
        .set_state(&execution_id, ExecutionState::Succeeded)
        .unwrap();

    // 设备重连后把「我在跑」再报一次。
    let outcome = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Running),
    );

    // 1. 不 panic（走到这里就是证明），且结论是「忽略」。
    assert!(
        matches!(outcome, AckOutcome::Ignored { .. }),
        "已收尾的执行收到 running 必须被忽略，实际 {outcome:?}"
    );

    // 2. 两端都没有倒退。
    assert_eq!(exec_state(&hub, &execution_id), ExecutionState::Succeeded);
    assert_eq!(task_state(&hub, &task.id), TaskState::Review);

    // 3. 重复投递也不许把**已经推进过**的那条再推一次（succeeded 报两遍）。
    let again = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Succeeded),
    );
    assert!(
        matches!(again, AckOutcome::Ignored { .. }),
        "重复的 succeeded 必须被忽略，实际 {again:?}"
    );
    assert_eq!(task_state(&hub, &task.id), TaskState::Review);

    // 4. 找不到执行的 ack 也不 panic、不改任何东西。
    let ghost = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack("no-such-execution", DispatchStage::Running),
    );
    assert!(
        matches!(ghost, AckOutcome::Ignored { .. }),
        "未知执行必须被忽略，实际 {ghost:?}"
    );
    assert_eq!(task_state(&hub, &task.id), TaskState::Review);
}

// ---------------------------------------------------------------------------
// ⑨ failure_kind 不在闭集里 → 不落库
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failure_kind_outside_the_closed_set_is_not_stored() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_1";
    bind(&hub, &project_id, node_id);
    let _frames = device_online(&hub, node_id, 102_400);

    let task = hub.create_task(&project_id, "报了个不认识的形态");
    let (execution_id, _) = dispatch(&hub, &task);
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Running),
    );

    // 规格里没有 `test_failure`（规格用的是 `task_failed`），它是最容易被
    // 顺手接受的一个。
    let outcome = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &failed_ack(&execution_id, "test_failure"),
    );
    assert_eq!(
        outcome,
        AckOutcome::Rejected {
            why: "failure_kind is outside the closed set",
        }
    );

    // 一个字节都没写：三列仍是空的，状态仍是 running。
    let exec = execution_repo(&hub)
        .get(&execution_id)
        .unwrap()
        .expect("execution");
    assert_eq!(exec.failure_kind, None, "脏值不许进库：{exec:?}");
    assert_eq!(exec.reason, None);
    assert_eq!(exec.evidence, None);
    assert_eq!(exec.state, ExecutionState::Running);

    // 任务也不动——它还在跑，「设备报了个脏形态」不等于「它失败了」。
    assert_eq!(task_state(&hub, &task.id), TaskState::Running);

    // 前提：同一个任务上一条**合法**的失败仍然落得进去（证明上面不是因为
    // 「失败路径整体不工作」而绿）。
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &failed_ack(&execution_id, "task_failed"),
    );
    let exec = execution_repo(&hub).get(&execution_id).unwrap().unwrap();
    assert_eq!(exec.failure_kind, Some(FailureKind::TaskFailed));
}

// ---------------------------------------------------------------------------
// ⑩ disk_exhausted → cleanup::on_failure_ack 真的被调用（结果留痕）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_disk_exhausted_ack_records_the_cleanup_preview() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_1";
    bind(&hub, &project_id, node_id);
    let _frames = device_online(&hub, node_id, 102_400);

    let task = hub.create_task(&project_id, "磁盘写满了");
    let (execution_id, _) = dispatch(&hub, &task);
    hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &ack(&execution_id, DispatchStage::Running),
    );

    // 预演要挂到一条会话上才有地方留痕（`failure_kind` 的执行常常还没有会话，
    // 那时 `on_failure_ack` 返回 `Ok(None)`——所以这条用例先把会话建出来）。
    //
    // 会话有 `workspace.node_id` 这一列（设备必须存在），所以这里先补一行
    // `device`：`device_online` 只往内存的上报缓存里记，不落库。
    //
    // ⚠️ 这一条**只能**用 `n_1` 之外的做法：`hub.create_task` 走的是
    // `route_task`，它要求设备在 `reports` 里有心跳（内存），与会话要求的
    // 「`device` 表里有这一行」是两件事，两处都得满足。
    let sessions = SessionRepo::new(hub.pool());
    hub.pool()
        .get()
        .unwrap()
        .execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at, status)
             VALUES (?1, 'stub', 'fp-n1', 'unix:0', 'online')",
            [node_id],
        )
        .unwrap();
    let session = sessions
        .create(
            node_id,
            FIXED_DIR,
            None,
            "unix:1700000010",
            None,
            BaseRevStatus::Unavailable,
        )
        .unwrap();
    sessions
        .set_execution_id(&session.id, &execution_id)
        .unwrap();

    let before = hub_core::cleanup::previews_run();
    let outcome = hub_server::dispatch_ack::on_dispatch_ack(
        &hub.state,
        node_id,
        &TaskDispatchAck {
            reason: Some("over budget".to_string()),
            evidence: Some("disk_exhausted worktree=/srv/af/worktrees".to_string()),
            failure_kind: Some("disk_exhausted".to_string()),
            items: Some(vec![
                protocol::DiskItem {
                    path: "/srv/af/worktrees/node_modules".to_string(),
                    bytes: 4000,
                    class: hub_core::ItemClass::DependencyCache.as_str().to_string(),
                    modified_at: Some(1_700_000_000),
                    label: "node_modules".to_string(),
                },
                protocol::DiskItem {
                    path: "/srv/af/worktrees/report.json".to_string(),
                    bytes: 8000,
                    class: hub_core::ItemClass::Artifact.as_str().to_string(),
                    modified_at: None,
                    label: "report.json".to_string(),
                },
            ]),
            ..ack(&execution_id, DispatchStage::Failed)
        },
    );
    assert!(
        outcome.is_applied(),
        "disk_exhausted 的失败必须落库，实际 {outcome:?}"
    );

    // 1. 失败本身落库（预演是附加事实，不是替代）。
    let exec = execution_repo(&hub).get(&execution_id).unwrap().unwrap();
    assert_eq!(exec.failure_kind, Some(FailureKind::DiskExhausted));
    assert_eq!(task_state(&hub, &task.id), TaskState::Failed);

    // 2. **`previews_run` 的增量 ≥ 1**：`preview` 确实被调用过，不是别处凑出来的。
    let after = hub_core::cleanup::previews_run();
    assert!(
        after > before,
        "disk_exhausted 必须真的走进 cleanup::on_failure_ack（计数 {before} → {after}）"
    );

    // 3. **轨迹里真有一条 `disk_cleanup_preview`**，且候选与实测清单对得上。
    let rows = hub.state.trajectory().list(&session.id, 0, 100).unwrap();
    let preview = rows
        .iter()
        .find(|e| e.kind == hub_core::cleanup::CLEANUP_PREVIEW_KIND)
        .unwrap_or_else(|| panic!("轨迹里必须有清理预演：{rows:?}"));
    let payload: serde_json::Value = serde_json::from_str(&preview.payload).unwrap();
    assert_eq!(payload["executionId"], serde_json::json!(execution_id));
    assert_eq!(
        payload["reclaimableBytes"],
        serde_json::json!(12_000_u64),
        "可释放 = 4000 + 8000（平衡档：两类都清）"
    );
    let candidates = payload["candidates"].as_array().expect("候选清单");
    assert_eq!(candidates.len(), 2, "{payload}");

    // 4. 预演**只算不删**：那两列仍在，执行状态没被它改动。
    assert_eq!(exec_state(&hub, &execution_id), ExecutionState::Failed);
}

// ---------------------------------------------------------------------------
// 接线：真 WS 上的一帧 ack 走进状态机
// ---------------------------------------------------------------------------

/// **一帧 `task_dispatch_ack` 经 `/ws` 的读循环** 真的把任务推进了。
///
/// 这条是「分支挂在 `relay` 里」的唯一证据：`on_dispatch_ack` 的单测直接调处理
/// 函数，删掉 `relay` 里那条 `types::TASK_DISPATCH_ACK` 分支它们**照样全绿**，
/// 而设备报上来的进展会被静默丢弃——那正是本里程碑要修的那个洞。
#[tokio::test]
async fn an_ack_over_the_ws_reaches_the_state_machine() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");

    let (est, node_id, kp) = common::connect_enrolled(&hub).await;
    drop(est);
    common::wait_disconnected(&hub, &node_id, Duration::from_secs(5)).await;

    let binding_id = bind(&hub, &project_id, &node_id);

    // 先建一个 `queued` 的任务与它的执行——这条用例只测 ack 那一半，
    // 派发那一半由 `scheduler_flow.rs` 覆盖。
    let task = hub.create_task(&project_id, "经 WS 回报");
    let execution_id = queued_task_with_execution(&hub, &task, &binding_id);

    let mut est = common::connect_reconnect(&hub, &kp, &node_id).await;
    est.send(&Frame::push(
        protocol::message::types::TASK_DISPATCH_ACK,
        0,
        serde_json::json!({
            "execution_id": execution_id,
            "stage": "running",
        }),
    ))
    .await
    .expect("ack must send");

    common::wait_until(Duration::from_secs(5), || {
        execution_repo(&hub)
            .get(&execution_id)
            .unwrap()
            .map(|e| e.state)
            == Some(ExecutionState::Running)
    })
    .await;

    assert_eq!(exec_state(&hub, &execution_id), ExecutionState::Running);
    assert_eq!(task_state(&hub, &task.id), TaskState::Running);
}
