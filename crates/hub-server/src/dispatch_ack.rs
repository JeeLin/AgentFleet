//! `task_dispatch_ack` 的接收方（v0.8.2 子任务 5）：设备回报的执行进展落成
//! 「任务状态 + 执行状态 + 失败三列 + 重试结论」。
//!
//! # 它补的是哪一半
//!
//! `routes/ws.rs` 的读循环曾经只有两条业务分派：心跳与通道帧，**其余一律忽略**
//! （约束 14 的前向兼容）。于是设备把 `running` / `failed` 报上来时，Hub 直接
//! 丢掉——任务永远停在 `queued`。调度器（`crate::scheduler`）把任务推出去的那一
//! 半早就有了，回来的这一半没有，端到端就永远走不到 `running`。
//!
//! 本模块就是那一半。它在 [`routes::ws`] 的 `relay` 分派链里挂一条
//! `types::TASK_DISPATCH_ACK` 分支。
//!
//! # 一张表：哪个 stage 改什么
//!
//! ```text
//! accepted          只留痕，**不改任何状态**（设备说「我接单了」，还没跑）
//! running           执行 → running、填 started_at；任务 queued → running
//! succeeded         执行 → succeeded、填 ended_at；任务 running → review
//! baseline_mismatch 执行 → failed + 三列；任务 running → failed；**重新 fetch 一次**
//! failed            执行 → failed + 三列；任务 running → failed；按 RetryPlan 决定下一步
//! ```
//!
//! # 四条硬约束
//!
//! 1. **任务状态一律走 `TaskRepo::set_state`**（内部 `task_state::check_transition`
//!    + CAS）。本模块没有一处裸 `UPDATE task SET state`。
//! 2. **失败形态必须在 `protocol::FAILURE_KINDS` 闭集里**：闭集之外的值**不落库**
//!    ——把脏值写进 `execution.failure_kind` 会让 `ExecutionView` 的读取端报错
//!    （`FailureKind::from_str` 不回落），于是**整条执行从列表里消失**。
//! 3. **幂等，且重复投递不 panic**：设备断线重连后会把同一条 ack 再报一次。
//!    这里靠「执行当前状态」与「任务的合法边」两层判断，把往回推的那种静默
//!    忽略并留一条日志。任何一种重复都不许变成 500 / panic / 断连。
//! 4. **不阻塞读循环**：全程同步库操作（与 `scheduler.rs` 同一个做法），没有
//!    一处 `.await`。
//!
//! # 重试只走到「该换什么」为止
//!
//! `hub_core::schedule::retry_policy` 给出 [`RetryPlan`]，本模块把它与**它的理由**
//! 一起写进轨迹。真正再派一次（建新 execution、过 ⑤ 磁盘门槛、换 agent）
//! 由 `crate::scheduler::Scheduler::retry_failed_execution` 承接，那是同一个事件的
//! 后半段——放在这里会让「收 ack」变成「收 ack 且派活」，两件事的对错判据完全
//! 不同。

use hub_core::schedule::{retry_policy, RetryPlan};
use hub_storage::execution::{ExecutionState, ExecutionView, FailureKind};
use hub_storage::task::{TaskState, TaskView};
use hub_storage::{BindingRepo, TrajectoryRepo};
use protocol::message::{DispatchStage, TaskDispatchAck};
use serde_json::json;

use crate::scheduler::{TASK_ACK_KIND, TASK_RETRY_PLANNED_KIND, TASK_TRAJECTORY_NAMESPACE};
use crate::AppState;

/// 一次 ack 的处置结论。
///
/// **不是错误类型**：重复 ack、未知执行、闭集外的失败形态都是「正常会发生的
/// 事」，不 panic 也不断连。返回它只是为了让调用点留痕，并让单测能断言
/// 「这一次到底改了什么」——只看库的话，「什么都没改」有五种可能的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckOutcome {
    /// 状态推进了。
    Applied {
        /// 推进之后执行的状态。
        execution: ExecutionState,
        /// 推进之后任务的状态；`None` = 这次没动任务（例如 `accepted`）。
        task: Option<TaskState>,
    },
    /// 什么都不用做，且不是异常。`why` 是一句可查的理由。
    ///
    /// 四个来源：`accepted`（只留痕）、重复 ack（状态已经推进过）、执行不存在
    /// （设备报了一条 Hub 没有的执行）、任务这边不在这条边的起点上（例如两次
    /// 失败连报）。
    Ignored { why: &'static str },
    /// 回执形状不对，**一个字节都没落库**。
    Rejected { why: &'static str },
}

impl AckOutcome {
    /// 这次 ack 是否改动了库。
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

/// 处理一帧 `task_dispatch_ack`。**同步、不 panic。**
///
/// 返回的结论由调用点（`relay` 的分派分支）记日志；本函数自己也记关键日志，
/// 因为「为什么这条 ack 没生效」的读者往往是翻日志的人，而不是写调用点的人。
pub fn on_dispatch_ack(state: &AppState, node_id: &str, ack: &TaskDispatchAck) -> AckOutcome {
    let outcome = apply(state, node_id, ack);
    match &outcome {
        AckOutcome::Applied { execution, task } => tracing::info!(
            node_id = %node_id,
            execution_id = %ack.execution_id,
            stage = ack.stage.as_str(),
            execution_state = execution.as_str(),
            task_state = task.map(TaskState::as_str),
            "dispatch ack applied"
        ),
        AckOutcome::Ignored { why } => tracing::debug!(
            node_id = %node_id,
            execution_id = %ack.execution_id,
            stage = ack.stage.as_str(),
            reason = why,
            "dispatch ack ignored"
        ),
        AckOutcome::Rejected { why } => tracing::warn!(
            node_id = %node_id,
            execution_id = %ack.execution_id,
            stage = ack.stage.as_str(),
            failure_kind = ack.failure_kind.as_deref(),
            reason = why,
            "dispatch ack rejected; nothing was written"
        ),
    }
    outcome
}

/// [`on_dispatch_ack`] 的实现体：不做日志，只算与写。
fn apply(state: &AppState, node_id: &str, ack: &TaskDispatchAck) -> AckOutcome {
    // ---- ① 找回那条执行。找不到不是异常：设备可能在 Hub 换代前接过活。----
    let current = match state.executions().get(&ack.execution_id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return AckOutcome::Ignored {
                why: "unknown execution",
            }
        }
        Err(e) => {
            tracing::warn!(
                execution_id = %ack.execution_id,
                error = %e,
                "cannot read the execution for this ack"
            );
            return AckOutcome::Ignored {
                why: "execution unreadable",
            };
        }
    };

    match ack.stage {
        // ---- ② 接单：只留痕。设备说「我拿到了」，那时它还没开始跑。 ----
        DispatchStage::Accepted => {
            record(
                state,
                &current.task_id,
                TASK_ACK_KIND,
                json!({
                    "execution_id": ack.execution_id,
                    "node_id": node_id,
                    "stage": ack.stage.as_str(),
                    "reason": ack.reason,
                }),
            );
            AckOutcome::Ignored {
                why: "accepted is an ack, not a state",
            }
        }

        // ---- ③ 开跑：执行 → running，任务 queued → running ----
        DispatchStage::Running => {
            if let Some(why) = stale(&current, ExecutionState::Running) {
                return AckOutcome::Ignored { why };
            }
            let now = crate::routes::now();
            if let Err(e) = state
                .executions()
                .set_state(&current.id, ExecutionState::Running)
            {
                return unreadable(e, &current.id);
            }
            // 重复的 `running` 不该改写首次开始时间——`mark_started` 内部是
            // `WHERE started_at IS NULL`，这里只是把那条口径用上。
            if let Err(e) = state.executions().mark_started(&current.id, &now) {
                tracing::warn!(execution_id = %current.id, error = %e, "cannot mark started_at");
            }
            let task = self_task(state, &current);
            let task_state = task.as_ref().and_then(|t| {
                advance_task(
                    state,
                    t,
                    TaskState::Running,
                    &now,
                    &ack.execution_id,
                    node_id,
                )
            });
            AckOutcome::Applied {
                execution: ExecutionState::Running,
                task: task_state,
            }
        }

        // ---- ④ 跑完：执行 → succeeded，任务 running → review ----
        DispatchStage::Succeeded => {
            if let Some(why) = stale(&current, ExecutionState::Succeeded) {
                return AckOutcome::Ignored { why };
            }
            let now = crate::routes::now();
            if let Err(e) = state
                .executions()
                .set_state(&current.id, ExecutionState::Succeeded)
            {
                return unreadable(e, &current.id);
            }
            if let Err(e) = state.executions().mark_ended(&current.id, &now) {
                tracing::warn!(execution_id = %current.id, error = %e, "cannot mark ended_at");
            }
            let task = self_task(state, &current);
            let task_state = task.as_ref().and_then(|t| {
                advance_task(
                    state,
                    t,
                    TaskState::Review,
                    &now,
                    &ack.execution_id,
                    node_id,
                )
            });
            AckOutcome::Applied {
                execution: ExecutionState::Succeeded,
                task: task_state,
            }
        }

        // ---- ⑤ 失败：两条 stage 走同一条处置，形态固定或来自回执 ----
        DispatchStage::Failed | DispatchStage::BaselineMismatch => {
            handle_failure(state, node_id, ack, &current)
        }
    }
}

/// 一次失败（`failed` / `baseline_mismatch` 两个 stage 的公共处置）。
///
/// `baseline_mismatch` 这个 stage 与 `failed{failure_kind:"baseline_mismatch"}`
/// 表达的是同一件事——设备本地 `HEAD != base_commit`，它**不硬跑**
/// （`task-model.md` §3.1）。所以这里统一成「一条失败」，形态取回执里那个
/// （缺失时用 stage 自己推出来的那个），**不编第三种**。
fn handle_failure(
    state: &AppState,
    node_id: &str,
    ack: &TaskDispatchAck,
    current: &ExecutionView,
) -> AckOutcome {
    // 闭集校验：**先于任何一次写**。写进脏值之后 `ExecutionView` 的读取端会
    // 报错（`FailureKind::from_str` 不回落），于是整条执行从列表里消失。
    let kind_str = match (&ack.failure_kind, ack.stage) {
        (Some(k), _) => k.as_str(),
        // `baseline_mismatch` 是一个独立的 stage，回执不带 `failure_kind` 也
        // 说明得了形态。其余 stage 缺 `failure_kind` 就是形状不对：一条没有
        // 归因的失败，处置动作（换设备 / 换 agent / 不重试）根本无从选择。
        (None, DispatchStage::BaselineMismatch) => FailureKind::BaselineMismatch.as_str(),
        (None, _) => {
            return AckOutcome::Rejected {
                why: "failure_kind is required on a failed ack",
            }
        }
    };
    if !protocol::message::FAILURE_KINDS.contains(&kind_str) {
        return AckOutcome::Rejected {
            why: "failure_kind is outside the closed set",
        };
    }
    let Ok(kind) = kind_str.parse::<FailureKind>() else {
        // 闭集与枚举本该一致（`protocol` 与 `hub-storage` 各存一份，靠
        // `the_protocol_kinds_parse_into_storage_kinds` 那条用例对拍）。这一臂
        // 只在两份清单漂移时成立——那时**更要**不落库。
        return AckOutcome::Rejected {
            why: "failure_kind does not parse into a storage kind",
        };
    };

    // 幂等：已经收了尾的执行不再被第二次失败改写。设备重连后把同一个 failed
    // 再报一次是常态，而 `set_failure` 会覆盖三列（包括人后来写的理由）。
    if current.state.is_terminal() {
        return AckOutcome::Ignored {
            why: "the execution already ended",
        };
    }

    // 规格：没有原因串的失败等于没发生（`task-model.md` §3.1）。设备漏报时
    // 用形态自己的线上串兜一句——比拒绝整条 ack 好：那会把「它失败了」这件
    // 事实一起丢掉，只留下一条 Hub 侧的日志。
    let reason = ack
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| kind.as_str());
    let evidence = ack.evidence.as_deref().unwrap_or("");

    if let Err(e) = state
        .executions()
        .set_failure(&current.id, kind, reason, evidence)
    {
        return unreadable(e, &current.id);
    }
    let now = crate::routes::now();
    if let Err(e) = state
        .executions()
        .set_state(&current.id, ExecutionState::Failed)
    {
        return unreadable(e, &current.id);
    }
    if let Err(e) = state.executions().mark_ended(&current.id, &now) {
        tracing::warn!(execution_id = %current.id, error = %e, "cannot mark ended_at");
    }

    // `disk_exhausted`：**清理预演现在就跑**（`hub-core/src/cleanup.rs` 的调用点，
    // `PRODUCT.md` §5.3 / §6.1「`disk_exhausted` 时列出准备删什么」）。它只算不删、
    // 不改任何状态，结果落成一条 `disk_cleanup_preview` 轨迹事实。
    //
    // 读回刚写的那一行而不是拿 `current`：`FailureReport::of_execution` 的文档
    // 明说它读的是**落库之后**的真值，而不是回执里那个字符串。
    if kind == FailureKind::DiskExhausted {
        run_cleanup_preview(state, ack, &current.id, node_id, &now);
    }

    let task = self_task(state, current);
    let task_state = task.as_ref().and_then(|t| {
        advance_task(
            state,
            t,
            TaskState::Failed,
            &now,
            &ack.execution_id,
            node_id,
        )
    });

    // 重试结论落进轨迹（**不是**在这里真的再派一次）。
    let plan = retry_policy(kind);
    record(
        state,
        &current.task_id,
        TASK_RETRY_PLANNED_KIND,
        json!({
            "execution_id": ack.execution_id,
            "node_id": node_id,
            "failure_kind": kind.as_str(),
            "stage": ack.stage.as_str(),
            // 线上码：页面按它分组，写死字面量而不是 `Debug` 格式
            "plan": plan.as_str(),
            // 「换什么」的同时记下「换的边界」——`different_agent` 的
            // `device_id` 不变，`different_device` 则要重新过 ⑤ 磁盘门槛。
            "keeps_device": plan == RetryPlan::DifferentAgent,
            "requires_disk_gate": plan == RetryPlan::DifferentDevice,
            // 旧起点留痕（`task-model.md` §7.3：新旧 `base_commit` 都要在）。
            // 新起点由重派时新建的那条执行自己带，两条并排就能对比。
            "base_commit": current.base_commit,
            "device_id": binding_device(state, &current.binding_id),
        }),
    );
    record(
        state,
        &current.task_id,
        TASK_ACK_KIND,
        json!({
            "execution_id": ack.execution_id,
            "node_id": node_id,
            "stage": ack.stage.as_str(),
            "failure_kind": kind.as_str(),
        }),
    );

    AckOutcome::Applied {
        execution: ExecutionState::Failed,
        task: task_state,
    }
}

/// `disk_exhausted` 的清理预演。**必须真的接线**（不是留 TODO）。
///
/// 输入全部按 `hub_core::cleanup::on_failure_ack` 文档点名的那几样取：
/// - `FailureReport::of_execution(&exec)`——落库之后的那一行；
/// - `items_from_ack_payload(...)`——设备实测清单（Hub 编不出来）；
/// - 绑定的 `cleanup_policy`——策略的唯一真相在绑定上，不另存一份；
/// - `session_for_execution(...)`——这份事实挂到哪条会话的轨迹上。
///
/// 三处失败（读执行 / 找绑定 / 找会话）都**不影响失败本身已经落库**：预演是
/// 附加事实，丢了它只让收件箱少一份方案，不该让「它失败了」这件事一起消失。
fn run_cleanup_preview(
    state: &AppState,
    ack: &TaskDispatchAck,
    execution_id: &str,
    node_id: &str,
    now: &str,
) {
    let Some(exec) = state.executions().get(execution_id).ok().flatten() else {
        tracing::warn!(
            execution_id = %execution_id,
            "cannot read the execution back for the cleanup preview"
        );
        return;
    };
    let policy = match BindingRepo::new(state.pool().clone()).get_by_id(&exec.binding_id) {
        Ok(Some(b)) => b.cleanup_policy,
        // 绑定没了（项目 / 设备被删）：没有策略就没有预演的依据。不编一个默认档
        // ——`CleanupPolicy` 的默认值是「路由层没给」时的取值，不是「绑定不见了」。
        Ok(None) => {
            tracing::warn!(
                execution_id = %execution_id,
                binding_id = %exec.binding_id,
                "no binding for this execution; skipping the cleanup preview"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                execution_id = %execution_id,
                error = %e,
                "cannot read the binding for the cleanup preview"
            );
            return;
        }
    };
    let sessions = hub_storage::SessionRepo::new(state.pool().clone());
    let session_id = match hub_core::cleanup::session_for_execution(&sessions, execution_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                execution_id = %execution_id,
                error = %e,
                "cannot look up the session for the cleanup preview"
            );
            return;
        }
    };
    // 回执载荷→清单：设备报的是 `items` 数组，形状与 `StoredItem` 一致。
    let payload = ack_payload(ack);
    let items = hub_core::cleanup::items_from_ack_payload(&payload);
    match hub_core::cleanup::on_failure_ack(
        hub_core::cleanup::FailureReport::of_execution(&exec),
        &items,
        &policy,
        session_id.as_deref(),
        state.trajectory(),
        now,
    ) {
        Ok(Some(recorded)) => tracing::info!(
            execution_id = %execution_id,
            node_id = %node_id,
            candidates = recorded.preview.candidates.len(),
            reclaimable_bytes = recorded.preview.reclaimable_bytes,
            "disk_exhausted：清理预演已留痕，等人确认"
        ),
        // `Ok(None)` 有三种正常原因（不是 disk_exhausted / 没会话 / 清单为空），
        // 都在 `on_failure_ack` 的文档里逐条写了；这里不再复述。
        Ok(None) => tracing::debug!(
            execution_id = %execution_id,
            items = items.len(),
            "disk_exhausted：预演没有可留痕的结果"
        ),
        Err(e) => tracing::warn!(
            execution_id = %execution_id,
            error = %e,
            "cannot record the cleanup preview"
        ),
    }
}

/// 把回执折成 `items_from_ack_payload` 认识的形状。
///
/// # 为什么绕一圈变成 `serde_json::Value`
///
/// `items_from_ack_payload` 的入口是「回执载荷」，它的存在理由是「Hub 侧不该
/// 依赖协议类型」——所以它读 JSON 而不是 `Vec<DiskItem>`。这里做一次序列化
/// 是那条边界的代价：`DiskItem` 与 `StoredItem` 是两个 crate 里的两套定义，
/// 靠 `hub-core` 的 `the_device_side_ack_payload_parses_into_this_side_items`
/// 那条用例对拍字段名。
///
/// 序列化失败只可能是构造出了非法数字；那时给出一个没有 `items` 的对象，
/// 预演按「设备没报清单」处理——**不编清单**。
fn ack_payload(ack: &TaskDispatchAck) -> serde_json::Value {
    serde_json::to_value(ack).unwrap_or_else(|_| json!({}))
}

/// 执行状态「往回推」的判定。返回 `Some(why)` = 这次 ack 不该生效。
///
/// # 为什么不能只靠边表
///
/// `TaskRepo::set_state` 有边表，非法边会返回 `Err`；但 **`execution` 表没有**
/// 边表（`ExecutionRepo::set_state` 是裸 `UPDATE`）。所以「已经 `Succeeded` 的
/// 执行又收到一个 `running`」在任务那边会被拒，执行那边却会被真的改回去——
/// 状态倒退，而 `ended_at` 还留着。判据放在这里、只写一份。
///
/// 语义：**执行已经收尾（终态）就不再接受任何新的推进**；同状态的重复投递
/// （`running` 又来一次 `running`）也算「不用做」，因为该做的都已经做了
/// （`mark_started` 是幂等的，再调一次什么都不会变）。
fn stale(current: &ExecutionView, want: ExecutionState) -> Option<&'static str> {
    if current.state.is_terminal() {
        return Some("the execution already ended; the ack would move it backwards");
    }
    if current.state == want {
        return Some("already in that state");
    }
    None
}

/// 读这次执行所属的任务。读不到返回 `None`（**不 panic**：任务可能被删了）。
fn self_task(state: &AppState, exec: &ExecutionView) -> Option<TaskView> {
    match state.tasks().get(&exec.task_id) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                task_id = %exec.task_id,
                error = %e,
                "cannot read the task for this ack"
            );
            None
        }
    }
}

/// 推进任务状态，**经 `TaskRepo::set_state`（边表 + CAS）**。
///
/// 返回 `Some(新状态)` 表示真的推进了；`None` 表示「这一步不用做 / 做不了」，
/// 分三种，都是幂等要覆盖的：
///
/// 1. 已经在目标态（重复投递）；
/// 2. 边不合法（`task_state.rs` 的 `check_transition` 会附一句「下一步该走哪个态」
///    的提示，那条提示原样进日志，因为读日志的人正是要判断「现在它卡在哪」）；
/// 3. 库读不出来。
///
/// **一律不 panic、不断连**：设备重复投递不该把一条好连接弄掉。
fn advance_task(
    state: &AppState,
    task: &TaskView,
    to: TaskState,
    now: &str,
    execution_id: &str,
    node_id: &str,
) -> Option<TaskState> {
    if task.state == to {
        return None;
    }
    match state.tasks().set_state(&task.id, to, now) {
        Ok(updated) => {
            record(
                state,
                &task.id,
                TASK_ACK_KIND,
                json!({
                    "execution_id": execution_id,
                    "node_id": node_id,
                    "task_from": task.state.as_str(),
                    "task_to": updated.state.as_str(),
                }),
            );
            Some(updated.state)
        }
        Err(e) => {
            // 非法边是**正常**的重复投递信号（设备重连后再报一次 `running`，
            // 而任务已经 `review` 了）。记 info 而不是 warn：它不是故障。
            tracing::info!(
                task_id = %task.id,
                execution_id = %execution_id,
                from = task.state.as_str(),
                to = to.as_str(),
                error = %e,
                "task state was not advanced by this ack"
            );
            None
        }
    }
}

/// 一次执行读不出来时的统一结论（不 panic，只留痕）。
fn unreadable(e: hub_storage::StorageError, execution_id: &str) -> AckOutcome {
    tracing::warn!(
        execution_id = %execution_id,
        error = %e,
        "cannot write the execution state for this ack"
    );
    AckOutcome::Ignored {
        why: "execution write failed",
    }
}

/// 绑定的 `device_id`（轨迹里要能看出「这一次是在哪台机器上失败的」）。
///
/// 取不到就返回 `null`：设备被删之后绑定仍在，那时如实说「不知道」，不猜。
fn binding_device(state: &AppState, binding_id: &str) -> Option<String> {
    match BindingRepo::new(state.pool().clone()).get_by_id(binding_id) {
        Ok(Some(b)) => Some(b.device_id),
        Ok(None) => None,
        // coverage: excluded — 读库失败；轨迹字段少一个值不影响失败已落库的事实。
        Err(_) => None,
    }
}

/// 一条 ack 事实写进**任务专属**的轨迹命名空间。
///
/// `seq` 与 `scheduler.rs` 的 `record` 同一条口径（`last_seq + 1`，不覆盖历史）。
/// 写失败**不影响 ack 的结论**：轨迹是留痕，不是状态推进的前置条件；但不静默
/// （`AGENTS.md` 的「I/O 错误不得静默」），所以记 warn。
fn record(state: &AppState, task_id: &str, kind: &str, payload: serde_json::Value) {
    let repo: &TrajectoryRepo = state.trajectory();
    let session_id = format!("{TASK_TRAJECTORY_NAMESPACE}{task_id}");
    let seq = match repo.last_seq(&session_id) {
        Ok(Some(last)) => last.saturating_add(1),
        Ok(None) => 0,
        // coverage: excluded — 读序号失败 = 读库失败，只有故障注入能造；
        // 退到 0 最坏是被唯一约束挡下一条留痕，比 panic 好。
        Err(_) => 0,
    };
    let text =
        serde_json::to_string(&payload).unwrap_or_else(|_| format!("{{\"kind\":\"{kind}\"}}"));
    let event = hub_storage::NewEvent::new(seq, kind, text);
    if let Err(e) = repo.append(&session_id, task_id, &event, &crate::routes::now()) {
        tracing::warn!(
            task_id = %task_id,
            kind = kind,
            error = %e,
            "cannot record the dispatch-ack fact"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议侧的闭集每一个都能被 `hub-storage` 的 `FailureKind` 解析出来。
    ///
    /// 两份清单在两个 crate 里（`protocol` 不依赖 `hub-storage`，设备侧不该为
    /// 报一个失败形态去编整个存储层），只能靠对拍钉住。漂移的症状很具体：
    /// 设备报了一个 Hub 认不出的名字 → 本模块的 `Rejected` 分支 → **任务永远
    /// 停在 `running`**，而库里那句 `failure_kind` 是干净的（我们什么都没写）。
    #[test]
    fn the_protocol_kinds_parse_into_storage_kinds() {
        assert_eq!(
            protocol::message::FAILURE_KINDS.len(),
            FailureKind::ALL.len(),
            "两边的形态数必须一致"
        );
        for raw in protocol::message::FAILURE_KINDS {
            let parsed = raw
                .parse::<FailureKind>()
                .unwrap_or_else(|e| panic!("{raw} 在协议闭集里，存储层必须认得：{e:?}"));
            assert_eq!(
                parsed.as_str(),
                *raw,
                "同一个形态在两边必须是同一串（否则统计数据会分叉）"
            );
        }
    }

    /// 闭集之外的值必须被挡在 `Rejected` 上，**一个字节都不落库**。
    ///
    /// 这条是纯函数层面的判据（真库那条在 `dispatch_ack_flow.rs`）。它挡的是
    /// 「顺手回落成 `agent_error`」那种做法：那会让缺陷归因里凭空多出一批
    /// `agent_error`，而真问题（拼错 / 新形态没同步）永远不出现。
    #[test]
    fn a_kind_outside_the_closed_set_is_not_accepted() {
        for bad in ["test_failure", "", "Superseded", "disk_full"] {
            assert!(
                !protocol::message::FAILURE_KINDS.contains(&bad),
                "{bad:?} 不该在闭集里"
            );
            assert!(
                bad.parse::<FailureKind>().is_err(),
                "{bad:?} 不该被存储层解析成某个形态"
            );
        }
    }
}
