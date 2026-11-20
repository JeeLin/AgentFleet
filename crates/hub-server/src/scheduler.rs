//! 调度器**运行时**：把 [`hub_core::schedule::pick`] 接到真实事件上（v0.8.2 子任务 3）。
//!
//! # 它是什么，不是什么
//!
//! `hub-core/src/schedule.rs` 是**纯函数**（三层硬过滤 + 有序打分 + 拒绝归因），
//! 不碰 IO、不认识库、不认识帧。本模块是它的另一半：取数据、调它、按结果写库
//! 与发帧。两者分开的理由写在那个模块的注释里，一句话是「顺序对不对只有在能
//! 构造任意输入时才测得透」。
//!
//! # 「任务可派」只有两个入口
//!
//! ```text
//! 新建任务（routes/tasks.rs 的 create_task）
//! 设备上线（routes/ws.rs 的 on_heartbeat → retry_routes_for）
//! ```
//!
//! 两条路都汇到同一个函数 [`Scheduler::route_task`]。**没有第二条派发路径**：
//! 一处「顺手把任务标成 queued」的旁路会让状态机变成可选的，而那正是
//! `task-model.md` 的 `EDGES` 存在的意义（它是合法边的唯一定义处）。
//!
//! # 一次路由的完整形状（`scheduling.md` §5「派发与落定」的前半段）
//!
//! ```text
//! 事件「任务可派」
//!   → pick(task, reports 快照, routing)
//!   → Ok(node_id):
//!       execution = ExecutionRepo::create(NewExecution { .. })   // 先落库，拿到 id
//!       task.state: pending → queued                            // 经 EDGES 校验
//!       OutboundTable::send(conn_id, frame(TASK_DISPATCH, ..))  // 发帧即返回
//!   → Err(rejection):
//!       不建 Execution、不占重试次数
//!       task 留在 pending，reason 写进轨迹（说清是**哪一层**空）
//! ```
//!
//! # 三处刻意的取舍
//!
//! 1. **不阻塞 HTTP 请求线程等执行结果**。发完帧就返回；执行进展由设备经
//!    `task_dispatch_ack` 回报（接收归子任务 5）。这也是 [`Scheduler::route_task`]
//!    是同步函数的原因——它只做「读库 → 纯函数 → 写库 → `try_send` 一帧」，
//!    全程没有一处 `.await`。
//! 2. **不降级凑数**：`pick` 返回 `Err` 时**一行 `execution` 都不建**。先建执行
//!    再挑设备会留下一批「有执行记录但没有设备在跑」的孤儿行，它们会让
//!    `ExecutionRepo::list_for_binding` 的「这个绑定在忙吗」永远为真（⑦ 从此
//!    恒拒），比不派更糟。
//! 3. **状态转移一律走 `TaskRepo::set_state`**，它内部是
//!    `task_state::check_transition` 与一次 CAS。本模块**没有**裸
//!    `UPDATE task SET state`，也没有 `force_state` 这类后门——测试
//!    `a_dispatch_never_bypasses_the_edge_table` 盯的就是这一条。
//!
//! # 归因写在哪
//!
//! 「为什么这个任务一直没派出去」必须能**查**出来，所以它落进
//! `trajectory_event` 表（独立 `kind`，见 [`TASK_REJECTED_KIND`]）。表的主键是
//! `(session_id, node_id, seq)`，本模块用任务 id 占这两个 id 列——理由见
//! [`TASK_TRAJECTORY_NAMESPACE`]。

use std::collections::HashSet;

use hub_core::schedule::{pick, retry_policy, Rejection, RetryPlan, Routing, TaskConstraints};
use hub_storage::execution::{ExecutionState, ExecutionView, FailureKind, NewExecution};
use hub_storage::project::ProjectRepo;
use hub_storage::task::{TaskState, TaskView};
use hub_storage::{Binding, BindingRepo, TrajectoryRepo};

use crate::conns::{OutboundTable, PushError};
use crate::AppState;

/// 拒绝归因写在轨迹里的 `kind`（`task-model.md` §7.2 的 `rejected(...)`）。
///
/// **一个独立的事件类型而不是往会话轨迹里塞一句散文**：轨迹表的 `kind` 是
/// 它的可查询维度，「为什么这个任务一直没派出去」要能一条 SQL 查出来
/// （`WHERE kind = 'task_rejected'`）。写进别的 kind 的 payload 里就只能全文搜。
///
/// ⚠️ 与 `trajectory.rs` 的轮次边界常量不同，本常量**不参与** `turn_bounds` /
/// `gaps` 的任何判定：它是任务派发的事实，不是一轮对话的边界。
pub const TASK_REJECTED_KIND: &str = "task_rejected";

/// 派发成功时写在轨迹里的 `kind`。
///
/// 与 [`TASK_REJECTED_KIND`] 对称：一次成功派发也是一条**要能查出来的事实**
/// （「这个任务第一次派给谁、什么时候」不该只存在于 `execution` 表里——那条路
/// 还要经 `binding → device` 一跳，而这条记录自己带着 `node_id`）。
pub const TASK_DISPATCHED_KIND: &str = "task_dispatched";

/// 收到一条 `task_dispatch_ack` 时写在轨迹里的 `kind`（v0.8.2 子任务 5）。
///
/// **设备回报的每一个 stage 都留一条**，包括 `accepted`（那个不该改任何状态，
/// 但「设备在什么时候接了单」正是排查「任务卡在 queued」时第一个要看的事实）。
///
/// 与 [`TASK_DISPATCHED_KIND`] 一样以任务 id 作分区（[`TASK_TRAJECTORY_NAMESPACE`]），
/// 于是同一个任务的「派出去」与「回来了」落在同一条时间线上，顺序天然可读。
pub const TASK_ACK_KIND: &str = "task_dispatch_ack";

/// 一次失败之后「下一步换什么」写在轨迹里的 `kind`。
///
/// 与 [`TASK_ACK_KIND`] 分开而不是塞进同一条 payload：重试结论是**调度决策**，
/// 失败回执是**设备事实**。合成一条之后，「这个任务失败过几次、每次决定换什么」
/// 就得在 payload 里翻；两个 kind 则是两条 `WHERE kind = ...` 就能数出来的表。
pub const TASK_RETRY_PLANNED_KIND: &str = "task_retry_planned";

/// 换设备 / 换 agent 重派成功后写在轨迹里的 `kind`。
///
/// 与 [`TASK_DISPATCHED_KIND`] 分开：那一条是「第一次派」，这一条带**上一条执行**，
/// 两条并排才能回答 `task-model.md` §7.3 的那个问题——「新旧 `base_commit` 各是什么」。
pub const TASK_RETRIED_KIND: &str = "task_retried";

/// 派发事实的轨迹挂在什么 `session_id` 下。
///
/// # 为什么是一个任务专属的命名空间而不是真会话
///
/// `trajectory_event` 的主键语义是 `(session_id, node_id, seq)`，三列都
/// `NOT NULL`，而**派发被拒时根本没有会话**（连设备都没选出来）。三个选项：
///
/// | 做法 | 问题 |
/// |---|---|
/// | 不写轨迹 | 违反规格「reason 写进轨迹」——「为什么没派出去」这个最该被回答的问题没有落点 |
/// | 写进最近一条会话 | 那条会话可能属于另一个任务，两个任务的派发事实混在一条轨迹里 |
/// | **任务专属的命名空间**（本做法） | 需要一个前缀约定；但每条事实各归其主，可查 |
///
/// 取第三种。真会话的 `session_id` 由 `identity::random_id()` 生成（base64url），
/// 不含 `:`，所以前缀 `task:` 一眼就能与真会话分开；
/// 「这条轨迹属于哪个任务」因此有一个确定的答案。
pub const TASK_TRAJECTORY_NAMESPACE: &str = "task:";

/// 一次路由的结论。
///
/// **不是错误类型**：拒绝是**正常结论**（设备都忙 / 都不在线时本来就该拒），
/// 调用方不该为它写错误处理分支。返回它只是为了让调用点能留痕与测试断言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// 选中了设备、执行记录已建、任务已 `queued`、帧已投递。
    Dispatched {
        node_id: String,
        execution_id: String,
    },
    /// 没有合格设备：`execution` 零新增，任务仍在 `pending`。
    ///
    /// `rejection` 说清是**哪一层**空（[`hub_core::schedule::Rejection`]）。
    Rejected { rejection: Rejection },
    /// 任务此刻不是 `pending`（已被别人派出去 / 已被关掉）。
    ///
    /// 与 [`Self::Rejected`] 分开：那不是「没设备」，是「不需要再派」。
    /// 混在一起的后果是归因里凭空多出一批 `no_allowed_device`，
    /// 而真相是重复事件。
    NotPending { state: TaskState },
}

impl RouteOutcome {
    /// 这次路由是否真的派了出去。
    pub fn is_dispatched(&self) -> bool {
        matches!(self, Self::Dispatched { .. })
    }
}

/// 派发帧没送到连接——这是异常，不是「拒绝」。
///
/// 与 [`RouteOutcome::Rejected`] 严格区分：那个是「没有合格设备」（正常），
/// 这个是「设备选出来了、执行也建了，但那一帧没进队列」。后者的处置是
/// 「这条执行停在 `pending`，等 `queued` 超时退回或人工收尾」，而静默丢弃会让
/// 它永远卡着——所以它要被返回出来留痕，并在轨迹里写进 `push_failure`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushFailure {
    /// 表里没有这条连接（设备刚断开，`OutboundTable::send` 返回 `Ok(false)`）。
    NotConnected,
    /// 连接还在但队列满了（半死不活的连接）。
    QueueFull,
}

/// 调度器运行时。**无状态**：它只借一个 `&AppState`，所有事实（心跳、绑定、
/// 执行）都从那里现读。
///
/// 不做成后台 actor（收事件队列、单线程消费）的理由：本里程碑的两个事件都是
/// **低频**的（建任务、心跳带来的上线），每次路由只读几张小表；引入一个 actor
/// 会带来「事件在队列里排着、页面却看到任务停在 pending」这种需要额外诊断的
/// 新状态，而收益（串行化）在这里没有对应的并发问题——`TaskRepo` 的转移自带
/// CAS，两条并发路由同抢一个任务时后到的那条会拿到版本冲突而不是写坏数据。
#[derive(Clone, Copy)]
pub struct Scheduler<'a> {
    state: &'a AppState,
}

impl<'a> Scheduler<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    /// 路由**一个**任务（两条事件入口的共同落点）。
    ///
    /// 返回路由结论；帧发不出去时额外返回 [`PushFailure`]，因为那种情况
    /// **已经不是「没派出去」而是「派了但没送出去」**。
    pub fn route_task(&self, task: &TaskView) -> (RouteOutcome, Option<PushFailure>) {
        // ① 只有 `pending` 的任务可以被派。**先查再算**：拿一个 `queued` /
        //    `failed` 的任务去跑三层过滤会白读三张表，而且算出来的结果没有
        //    任何意义（`pending → queued` 这条边只在起点是 `pending` 时合法）。
        if task.state != TaskState::Pending {
            return (RouteOutcome::NotPending { state: task.state }, None);
        }

        // ② 取数据。**心跳读内存不读库**（`scheduling.md` §2.2）；项目形态与
        //    绑定读库（它们变化极慢，且没有内存副本）。
        let project = match ProjectRepo::new(self.state.pool().clone()).get(&task.project_id) {
            Ok(Some(p)) => p,
            // 项目不存在 / 读不出来 → 按「约束那一层空」归因，不 panic。
            // 这里**不建执行**：一个不属于任何项目的任务不该被派到设备上。
            //
            // 两条臂都归到同一个码，见 `AGENTS.md`「生产代码禁 panic 铁律」。
            // 项目只归档不删除，所以 `Ok(None)` 在生产里正常走不到；`Err(_)` 由
            // 单测 `an_unreadable_project_table_rejects_instead_of_panicking`
            // 把 `project` 表改名真实触发。
            Ok(None) | Err(_) => return self.reject(task, Rejection::NoAllowedDevice),
        };
        if project.is_archived() {
            // 归档项目**停止派发**（`Project::is_archived` 的字段注释）。归因落在
            // ⓪（`no_allowed_device`）而不是新增第九个 `Rejection` 变体：那个枚举
            // 是「设备为什么不合格」，变体顺序就是过滤顺序；归档是**项目级的停止
            // 开关**，不是某一层过滤，混进去它的 `Ord` 就失去了意义。
            return self.reject(task, Rejection::NoAllowedDevice);
        }

        let bindings =
            match BindingRepo::new(self.state.pool().clone()).list_for_project(&task.project_id) {
                Ok(b) => b,
                // 读绑定失败 = 库坏了；单测
                // `an_unreadable_binding_table_rejects_instead_of_panicking` 把
                // `binding` 表改名真实触发。
                Err(_) => return self.reject(task, Rejection::NoAllowedDevice),
            };
        let pairs = binding_pairs(&bindings);

        // ③ 候选节点 = **有新鲜心跳**的那些。`Reports` 里没有的设备不是
        //    「离线」，是「从没上报过 / 心跳已超时」——两者对路由同义
        //    （见 `hub_core::presence`）。
        let reports = self.state.reports();
        let nodes: Vec<(String, hub_core::Report)> = bindings
            .iter()
            .filter_map(|b| reports.get(&b.device_id).map(|r| (b.device_id.clone(), r)))
            .collect();

        let constraints = constraints_of(task);
        let busy = |binding_id: &str| self.binding_is_busy(binding_id);
        let routing = Routing::new(project.shape, &pairs, &busy);

        match pick(
            &constraints,
            nodes.iter().map(|(id, r)| (id.as_str(), r)),
            &routing,
        ) {
            // ④ 拒绝：**不建 Execution、不占重试次数**，任务留 `pending`。
            Err(rejection) => self.reject(task, rejection),
            Ok(node_id) => {
                let node_id = node_id.to_string();
                match pairs.iter().find(|(n, _)| *n == node_id) {
                    Some((_, binding)) => self.dispatch(task, &node_id, &binding.clone()),
                    // coverage: excluded — `pick` 只会返回 `routing.bindings`
                    // 里的节点，而绑定表就是它唯一的候选源。
                    None => self.reject(task, Rejection::NoAllowedDevice),
                }
            }
        }
    }

    /// 路由**整个项目**的 `pending` 任务（设备上线 / 手动重试用）。
    ///
    /// 按 `created_at` 升序逐个试（[`hub_storage::task::TaskRepo::list_pending_for_project`]
    /// 的口径）：同项目里先建的任务优先，且每派出去一个，下一个就会在 ⑦
    /// （`BindingBusy`）上被挡住——`scheduling.md` §3.3 的「同一绑定默认串行」
    /// 不需要额外加锁，靠的是**刚建的那条执行已经在库里**。
    pub fn route_project(&self, project_id: &str) -> Vec<(String, RouteOutcome)> {
        let tasks = match self.state.tasks().list_pending_for_project(project_id) {
            Ok(t) => t,
            // 读库失败（表被改坏）——单测
            // `route_project_returns_empty_when_the_task_table_is_unreadable`
            // 把 `task` 表改名真实触发。不 panic、不留假事实，只记日志。
            Err(e) => {
                // coverage: excluded — 日志宏体，无条件分支。
                tracing::warn!(
                    project_id = %project_id,
                    error = %e,
                    "cannot list pending tasks; skipping this routing round"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(tasks.len());
        for task in tasks {
            let (outcome, push) = self.route_task(&task);
            if let Some(push) = push {
                // coverage: excluded — 日志宏体，无条件分支。
                tracing::warn!(
                    task_id = %task.id,
                    failure = ?push,
                    "the dispatch frame did not reach the connection"
                );
            }
            out.push((task.id, outcome));
        }
        out
    }

    /// 设备上线：重试该设备**所有绑定**所属项目的待派任务。
    ///
    /// # 为什么是「按绑定找项目」而不是「遍历全部 pending」
    ///
    /// 遍历全部 `pending`（`TaskRepo::list_by_state`）在单项目部署上两者等价，
    /// 但在 N 个项目下都是 N 倍的读：一台设备上线会去查每一个项目的每一条
    /// `pending`，而它只可能影响**自己绑定的那几个项目**——路由的候选源就是
    /// 绑定表（见 `pick` 的 `Routing::bindings`），别的项目怎么算都轮不到它。
    ///
    /// 绑定表本身很小（一台设备通常绑 1~3 个项目），所以成本是一次
    /// `BindingRepo::list_for_device` 加每个项目一次
    /// [`hub_storage::task::TaskRepo::list_pending_for_project`]。
    ///
    /// ⚠️ **不做「项目 → 设备」的反向索引缓存**：那需要一张要跟着绑定增删改
    /// 同步的侧表，而同步漏一次的后果是「上线了却不重试」，比多一次查询糟；
    /// 绑定写入没有缓存失效的通知渠道（`BindingRepo::upsert` 不认识本模块）。
    pub fn route_binding_tasks(&self, device_id: &str) -> Vec<(String, RouteOutcome)> {
        let bindings = match BindingRepo::new(self.state.pool().clone()).list_for_device(device_id)
        {
            Ok(b) => b,
            // 读库失败；单测
            // `route_binding_tasks_returns_empty_when_the_binding_table_is_unreadable`
            // 把 `binding` 表改名真实触发。
            Err(e) => {
                // coverage: excluded — 日志宏体，无条件分支。
                tracing::warn!(
                    device_id = %device_id,
                    error = %e,
                    "cannot list bindings; nothing to retry"
                );
                return Vec::new();
            }
        };
        // 同一台设备在一个项目上只有一条绑定（`binding` 表的唯一约束），
        // 但仍然去重：将来若允许多绑定，这里不会把一个项目路由两遍。
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for b in bindings {
            if !seen.insert(b.project_id.clone()) {
                // coverage: excluded — `binding` 的唯一约束落在
                // `(project_id, device_id)` 上，同一台设备在同一个项目上
                // 只能有一行，所以这个 `continue` 在现有表结构下走不到；
                // 留着是给「将来允许多绑定」的挡板。
                continue;
            }
            out.extend(self.route_project(&b.project_id));
        }
        out
    }

    /// 一条**已失败**的执行之后，按 `retry_policy` 换条件再派一次。
    ///
    /// # 它和 `route_task` 的分工
    ///
    /// `route_task` 只接受 `pending` 的任务（那是「还没派过」）。重派时任务已经
    /// 是 `failed`，边表里 `failed → queued` 是合法的，但**不能跳过中间那一步**
    /// ——所以顺序是：先把任务沿 `failed → queued` 推上去，再走与首次派发**同一
    /// 条**执行创建 + 发帧路径。
    ///
    /// # 换设备**复用 `pick`**，不另写一套
    ///
    /// `scheduling.md` §5.1 末句：「换一台试」不等于「换一台不管空间够不够」。
    /// 所以这里重新调 [`pick`] —— 它的 ⑤ 层就是那张磁盘硬门槛，而**候选里排除
    /// 掉刚刚失败的那台**（`plan == DifferentDevice` 的意义就在这）。
    ///
    /// 更省事的做法是「取出上一次的 `binding_id`，在上面再建一条执行」：那既不过
    /// 磁盘门槛、也不过能力层，等于把「换一台」实现成「原地再来一次」——而
    /// `scheduling.md` §5.1 开头作废的正是那个说法。
    ///
    /// # `DifferentAgent`：`device_id` **不变**
    ///
    /// 这时候选源被收窄到「上一次那台设备」，换的只是帧里的 `agent_id`：从
    /// `allowed_agents` 里挑一个**不是刚崩过的那个**。放宽任务约束是人的动作
    /// （`task-model.md` §4「放宽约束」），本函数**不写 task 表一个字节**。
    ///
    /// # 新旧 `base_commit` 都留痕（`task-model.md` §7.3）
    ///
    /// 新执行的 `base_commit` 照抄任务的起点（与首次派发同一口径）；`None` 时
    /// 设备侧会在 ack 里回填。**上一条执行的那一个**写进 `task_retried` 事实里，
    /// 两条并排就能看出「换起点了吗、从哪换到哪」。
    pub fn retry_failed_execution(
        &self,
        failed: &ExecutionView,
    ) -> (RouteOutcome, Option<PushFailure>) {
        let now = crate::routes::now();
        let task = match self.state.tasks().get(&failed.task_id) {
            Ok(Some(t)) => t,
            Ok(None) => {
                tracing::warn!(task_id = %failed.task_id, "cannot retry: the task is gone");
                return (
                    RouteOutcome::Rejected {
                        rejection: Rejection::NoAllowedDevice,
                    },
                    None,
                );
            }
            // 读库失败（表被改坏），不 panic 也不编事实。生产走不到，因为
            // `failed` 这一行是 `self.state.tasks().get` 刚读出来的同一条表；
            // 单测 `a_retry_whose_requeue_fails_reports_the_state_without_creating_an_execution`
            // 把 `task` 表改名覆盖了同一张表不可读时的收尾口径。
            Err(e) => {
                tracing::warn!(task_id = %failed.task_id, error = %e, "cannot read the task to retry");
                return (
                    RouteOutcome::Rejected {
                        rejection: Rejection::NoAllowedDevice,
                    },
                    None,
                );
            }
        };

        // `failed` 是重派的合法起点（`task_state.rs` 的 `failed → queued`）；
        // 别的状态说明有人已经动过，不抢。
        if task.state != TaskState::Failed {
            return (RouteOutcome::NotPending { state: task.state }, None);
        }

        let plan = retry_policy(
            failed
                .failure_kind
                // 没有形态的执行不该走到重派路径（ack 处理只在有形态时才落库）。
                // 用 `Rejected` 兜一个 NoRetry 的结论：什么都不做比猜一种更安全。
                .unwrap_or(FailureKind::Rejected),
        );

        // ---- ① 取路由的输入（与 `route_task` 逐句同源）----
        let project = match ProjectRepo::new(self.state.pool().clone()).get(&task.project_id) {
            Ok(Some(p)) => p,
            // 项目只归档不删除，`Ok(None)` 生产走不到；`Err(_)` 由单测
            // `a_retry_with_an_unreadable_project_table_reports_not_pending`
            // 把 `project` 表改名真实触发。
            Ok(None) | Err(_) => return (RouteOutcome::NotPending { state: task.state }, None),
        };
        let bindings =
            match BindingRepo::new(self.state.pool().clone()).list_for_project(&task.project_id) {
                Ok(b) => b,
                // 读库失败；单测
                // `a_retry_with_an_unreadable_binding_table_reports_not_pending`
                // 把 `binding` 表改名真实触发。
                Err(_) => return (RouteOutcome::NotPending { state: task.state }, None),
            };
        // 上一次那台设备是从绑定反查出来的（`ExecutionView` 只存 `binding_id`）。
        let failed_device = bindings
            .iter()
            .find(|b| b.id == failed.binding_id)
            .map(|b| b.device_id.clone());

        // ---- ② `pick` 的候选源：`DifferentDevice` 时排除刚失败的那台 ----
        //
        // 四种结论只差**一个谓词**（留不留上一台），所以先求谓词再用既有的
        // [`binding_pairs`] 投影——不把同一个 `(device_id, binding)` 投影抄四遍。
        let keep: fn(Option<&String>, Option<&String>) -> bool = match plan {
            // 换 agent：候选只有上一次那台设备（`device_id` 不变）。
            RetryPlan::DifferentAgent | RetryPlan::SameDevice => |want, got| want == got,
            // 换设备：**排除**刚失败的那台（否则「换一台」在同一台上再跑一次）。
            RetryPlan::DifferentDevice => |want, got| want != got,
            // 不重试：不留任何候选（下面直接返回，这里只为形状完整）。
            RetryPlan::NoRetry => |_, _| false,
        };
        let candidates: Vec<Binding> = bindings
            .iter()
            .filter(|b| keep(failed_device.as_ref(), Some(&b.device_id)))
            .cloned()
            .collect();
        let pairs = binding_pairs(&candidates);
        if plan == RetryPlan::NoRetry {
            self.record(
                &task,
                TASK_RETRY_PLANNED_KIND,
                serde_json::json!({
                    "execution_id": failed.id,
                    "plan": plan.as_str(),
                    "outcome": "skipped",
                    "why": "superseded / rejected: changing the machine cannot help",
                }),
            );
            return (RouteOutcome::NotPending { state: task.state }, None);
        }

        let reports = self.state.reports();
        let nodes: Vec<(String, hub_core::Report)> = pairs
            .iter()
            .filter_map(|(device, _)| reports.get(device).map(|r| (device.clone(), r)))
            .collect();
        let constraints = constraints_of(&task);
        let busy = |binding_id: &str| self.binding_is_busy(binding_id);
        let routing = Routing::new(project.shape, &pairs, &busy);

        let node_id = match pick(
            &constraints,
            nodes.iter().map(|(id, r)| (id.as_str(), r)),
            &routing,
        ) {
            Ok(id) => id.to_string(),
            Err(rejection) => {
                // 没挑出来：**建零条执行**，任务留在 `failed` 等人处理。归因写进
                // 与首次拒绝同一个 kind（`task_rejected`），于是「一直没派出去」
                // 只需要查一个 kind。
                self.record(
                    &task,
                    TASK_RETRY_PLANNED_KIND,
                    serde_json::json!({
                        "execution_id": failed.id,
                        "plan": plan.as_str(),
                        "outcome": "rejected",
                        "code": rejection.as_str(),
                    }),
                );
                self.reject(&task, rejection);
                return (RouteOutcome::Rejected { rejection }, None);
            }
        };
        let Some((_, binding)) = pairs.iter().find(|(n, _)| *n == node_id) else {
            // coverage: excluded — `pick` 只会返回 `routing.bindings` 里的节点。
            return (
                RouteOutcome::Rejected {
                    rejection: Rejection::NoAllowedDevice,
                },
                None,
            );
        };
        let binding = binding.clone();

        // ---- ③ 任务 `failed → queued`（走边表），再建执行、发帧 ----
        if let Err(e) = self
            .state
            .tasks()
            .set_state(&task.id, TaskState::Queued, &now)
        {
            tracing::warn!(task_id = %task.id, error = %e, "cannot queue the task for the retry");
            let state = self
                .state
                .tasks()
                .get(&task.id)
                .ok()
                .flatten()
                .map_or(task.state, |t| t.state);
            return (RouteOutcome::NotPending { state }, None);
        }

        let worktree_path = worktree_path_for(&binding, &task);
        let execution = match self.state.executions().create(
            NewExecution {
                task_id: task.id.clone(),
                binding_id: binding.id.clone(),
                // 新起点：与首次派发同源（任务的 `base_commit`）。
                base_commit: task.base_commit.clone(),
                worktree_path: Some(worktree_path.clone()),
            },
            &now,
        ) {
            Ok(e) => e,
            // 写库失败。生产里只有库坏时成立；单测
            // `a_retry_whose_execution_insert_is_rejected_is_reported` 用
            // `BEFORE INSERT` 触发器让这条 `INSERT` 真的失败。
            Err(e) => {
                // coverage: excluded — 日志宏体，无条件分支。
                tracing::error!(task_id = %task.id, error = %e, "cannot create the retry execution");
                return (
                    RouteOutcome::Rejected {
                        rejection: Rejection::NoAllowedDevice,
                    },
                    None,
                );
            }
        };

        let agent_id = retry_agent(&task, failed.agent_id.as_deref(), plan);
        let frame = dispatch_frame(
            &task,
            &DispatchTarget {
                project_id: &binding.project_id,
                execution_id: &execution.id,
                worktree_path: &worktree_path,
                agent_id: &agent_id,
            },
        );
        let push = self.push(&node_id, &frame);

        self.record(
            &task,
            TASK_RETRIED_KIND,
            serde_json::json!({
                "execution_id": execution.id,
                "previous_execution_id": failed.id,
                "plan": plan.as_str(),
                "node_id": node_id,
                "previous_binding_id": failed.binding_id,
                "binding_id": binding.id,
                // `task-model.md` §7.3：新旧起点**都**留在这里，两条并排就能看出
                // 「换起点了吗、从哪换到哪」。`base_commit: null` 表示那一次没有
                // 起点（设备侧会在 ack 里回填），不是空串。
                "base_commit": execution.base_commit,
                "previous_base_commit": failed.base_commit,
                "agent_id": agent_id,
                "push_failure": push.map(push_failure_code),
            }),
        );

        (
            RouteOutcome::Dispatched {
                node_id,
                execution_id: execution.id,
            },
            push,
        )
    }

    /// 一次成功派发的全部副作用，**顺序不可换**。
    ///
    /// 顺序是：建执行 → 任务转 `queued` → 发帧。理由：
    ///
    /// - **执行先落库**：帧里要带 `execution_id`，所以它必须先存在。
    /// - **状态先于帧**：设备收到 `task_dispatch` 会立刻回 `accepted`，之后可能
    ///   极快地报 `running`（子任务 5 的 ack 处理会把任务推 `queued → running`）。
    ///   若帧先发、状态后写，那两条 ack 可能在 `pending → queued` 之前到达，于是
    ///   `queued → running` 撞上一个还是 `pending` 的任务 → 被状态机拒。先写状态
    ///   把这条竞态窗口关掉；代价是「状态到了、帧没送到」这一种半截状态，而它
    ///   **是可见的**（`push_failure` 留痕 + 执行停在 `pending`），比「设备在跑但
    ///   库里还写着待办」好。
    fn dispatch(
        &self,
        task: &TaskView,
        node_id: &str,
        binding: &Binding,
    ) -> (RouteOutcome, Option<PushFailure>) {
        let now = crate::routes::now();
        let worktree_path = worktree_path_for(binding, task);
        let execution = match self.state.executions().create(
            NewExecution {
                task_id: task.id.clone(),
                binding_id: binding.id.clone(),
                // 起点：任务的 `base_commit`（建任务时写下的那个）。`None` 是
                // 合法值——设备侧以它自己的 `HEAD` 为准并在 ack 里回填，这里不
                // 编一个空串（空串在 `check_baseline` 里是一个永远不匹配的起点）。
                base_commit: task.base_commit.clone(),
                worktree_path: Some(worktree_path.clone()),
            },
            &now,
        ) {
            Ok(e) => e,
            // 建执行失败 = 写库失败。生产里只有库坏时成立；单测
            // `dispatch_backs_out_when_either_the_execution_or_the_state_cannot_be_written`
            // 用 `WHERE 0` 视图让这条 `INSERT` 真的失败。
            Err(e) => {
                // coverage: excluded — 日志宏体，无条件分支。
                tracing::error!(
                    task_id = %task.id,
                    error = %e,
                    "cannot create the execution record; the task stays pending"
                );
                return self.reject(task, Rejection::NoAllowedDevice);
            }
        };

        // `pending → queued`，**经 `task_state.rs` 的边校验**（`set_state` 内部先
        // `check_transition` 再 CAS）。失败意味着有人在这两步之间动了这个任务。
        if let Err(e) = self
            .state
            .tasks()
            .set_state(&task.id, TaskState::Queued, &now)
        {
            // 状态没推上去，那条刚建的执行就是一条孤儿行。把它收尾成
            // `aborted`（**不是 `failed`**：这是我们主动停的）——否则 ⑦ 会永远忙，
            // 同一个绑定上的后续任务全部饿死。
            if let Err(e2) = self
                .state
                .executions()
                .set_state(&execution.id, ExecutionState::Aborted)
            {
                // 收尾失败 = 写库失败（同一时刻另一处也在写这张表）。
                // coverage: excluded — 日志宏体，无条件分支。
                tracing::error!(
                    execution_id = %execution.id,
                    error = %e2,
                    "cannot abort the orphaned execution"
                );
            }
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "cannot move the task to queued; backing out this dispatch"
            );
            // 重读一次状态再报：这条路径的真相是「它已经不是 pending 了」。
            let state = self
                .state
                .tasks()
                .get(&task.id)
                .ok()
                .flatten()
                .map_or(task.state, |t| t.state);
            return (RouteOutcome::NotPending { state }, None);
        }

        let agent_id = picked_agent(task);
        let frame = dispatch_frame(
            task,
            &DispatchTarget {
                project_id: &binding.project_id,
                execution_id: &execution.id,
                worktree_path: &worktree_path,
                agent_id: &agent_id,
            },
        );
        let push = self.push(node_id, &frame);
        self.record(
            task,
            TASK_DISPATCHED_KIND,
            serde_json::json!({
                "execution_id": execution.id,
                "node_id": node_id,
                "binding_id": binding.id,
                "worktree_path": worktree_path,
                // 帧有没有真的进队列。`null` = 进了；否则是失败原因
                // （`not_connected` / `queue_full`）。与拒绝归因同一口径：
                // 「为什么没送到」也必须能一条 SQL 查出来。
                "push_failure": push.map(push_failure_code),
            }),
        );
        (
            RouteOutcome::Dispatched {
                node_id: node_id.to_string(),
                execution_id: execution.id,
            },
            push,
        )
    }

    /// 把一帧投进该连接的**控制面**队列。**不 await**：`send` 内部是 `try_send`。
    ///
    /// # 为什么走 `send`（控制面）而不是 `send_data`
    ///
    /// `task_dispatch` 是**控制语义**：它让设备开始一件新工作，不是某条通道的
    /// 载荷。数据面队列在终端刷屏时会被 PTY 输出塞满，而 `conns.rs` 写明了队列
    /// 满时丢数据帧是**可接受的**——派发帧被那样丢掉就等于任务凭空消失。控制面
    /// 只发低频控制帧、打满本身就是异常，值得留痕（[`PushFailure::QueueFull`]）。
    fn push(&self, node_id: &str, frame: &protocol::frame::Frame) -> Option<PushFailure> {
        let Some(handle) = self.state.connections().get(node_id) else {
            // 有心跳但没连接 = 正在退避重连（`Presence::Reconnecting`）。
            // 路由**不排除**它（`scheduling.md` §3：重连中的设备仍可调度），但帧
            // 必须走一条活连接——所以这里如实返回「没送出去」，等它重连后的心跳
            // 再触发一次重试路由。
            // coverage: excluded — 日志宏体，无条件分支。
            tracing::info!(
                node_id = %node_id,
                "no live connection for the chosen node; the dispatch frame was not sent"
            );
            return Some(PushFailure::NotConnected);
        };
        let outbound: &OutboundTable = self.state.outbound();
        match outbound.send(&handle.conn_id, frame.clone()) {
            Ok(true) => None,
            Ok(false) => Some(PushFailure::NotConnected),
            // 控制面队列打满。生产里要求同一台设备同时收到一批还没被写协程
            // 取走的控制帧；单测 `a_full_control_queue_is_reported_as_queue_full`
            // 直接把队列填满。`PushError::QueueFull` 的映射本身由 `conns.rs`
            // 的单测另钉一遍。
            Err(PushError::QueueFull) => Some(PushFailure::QueueFull),
        }
    }

    /// 拒绝：不建执行、不占重试次数，只留痕。
    fn reject(&self, task: &TaskView, rejection: Rejection) -> (RouteOutcome, Option<PushFailure>) {
        self.record(
            task,
            TASK_REJECTED_KIND,
            serde_json::json!({
                // 哪一层空（`scheduling.md` §3 的 ⓪–⑦ 序号）
                "layer": rejection_layer(rejection),
                // 线上码，与 `task-model.md` §7.2 的 `rejected(...)` 同一套字面值
                "code": rejection.as_str(),
                // 任务自己的约束：只说「no_allowed_device」而不说约束长什么样，
                // 用户看不出该改哪一条。
                "constraints": {
                    "allowed_devices": task.allowed_devices,
                    "allowed_agents": task.allowed_agents,
                    "required_capabilities": task.required_capabilities,
                },
            }),
        );
        (RouteOutcome::Rejected { rejection }, None)
    }

    /// 一条派发事实写进**任务专属**的轨迹命名空间。
    ///
    /// `seq` 取 `TrajectoryRepo::last_seq` + 1：同一任务的事实按顺序追加，不覆盖
    /// 历史（`scheduling.md` §6：「不 UPDATE 覆盖历史」）。
    ///
    /// 写失败**不影响派发结论**：轨迹是留痕，不是派发的前置条件。但不能静默
    /// （`AGENTS.md` 的「I/O 错误不得静默」），所以记 warn。
    fn record(&self, task: &TaskView, kind: &str, payload: serde_json::Value) {
        let repo: &TrajectoryRepo = self.state.trajectory();
        let session_id = format!("{TASK_TRAJECTORY_NAMESPACE}{}", task.id);
        // `node_id` 列同样用任务 id 填：派发事实**不属于任何设备**（被拒时连
        // 设备都没选出来），而这一列 `NOT NULL`。填任务自己而不是空串——空串会
        // 让「按 `(session_id, node_id)` 分区」这件事失去主语，填任务 id 则让同一
        // 任务的整条派发史落在同一分区，与 `session_id` 前缀互相印证。
        let entity_id = task.id.as_str();
        let seq = match repo.last_seq(&session_id) {
            Ok(Some(last)) => last.saturating_add(1),
            Ok(None) => 0,
            // coverage: excluded — 读序号失败 = 读库失败。退到 0 会让
            // `INSERT OR IGNORE` 因唯一约束静默丢掉这条事实，但那是「留痕少
            // 一条」而不是「派发出错」，比 panic 好；`append` 自己也会再报一次。
            Err(_) => 0,
        };
        // payload 是 `serde_json::Value`，序列化失败只可能是构造出了非法数字；
        // 退成一句带 kind 的对象，仍然留下「发生过这件事」。
        let text =
            serde_json::to_string(&payload).unwrap_or_else(|_| format!("{{\"kind\":\"{kind}\"}}"));
        let event = hub_storage::NewEvent::new(seq, kind, text);
        if let Err(e) = repo.append(&session_id, entity_id, &event, &crate::routes::now()) {
            // coverage: excluded — 日志宏体，无条件分支。
            tracing::warn!(
                task_id = %task.id,
                kind = kind,
                error = %e,
                "cannot record the dispatch fact"
            );
        }
    }

    /// 该绑定上有没有**还没收尾**的执行（⑦ `BindingBusy` 的数据源）。
    ///
    /// 「在跑」的口径直接问 [`ExecutionState::is_terminal`]——终态的判定写在
    /// `hub-storage` 里，本模块不重写一份（重写的那份迟早与它不一致）。
    ///
    /// 读库失败时返回 `true`（**当作忙**）：把「读不出来」当成「空着」会让同一个
    /// 绑定上并发派两条任务，而那正是 §3.3 默认串行要防的事。代价是库坏时任务
    /// 全停——而库坏的时候派出去也跑不了。
    fn binding_is_busy(&self, binding_id: &str) -> bool {
        match self.state.executions().list_for_binding(binding_id) {
            Ok(list) => list.iter().any(|e| !e.state.is_terminal()),
            // coverage: excluded — 读库失败；见上：宁忙不空。
            Err(_) => true,
        }
    }
}

/// `(node_id, binding)` 对——`Routing::bindings` 要的形状。
///
/// 由绑定行投影出来而不是去 `reports` 里捞：**绑定是路由的候选源**
/// （`pick` 的 ① 与 ⑥ 都是按「这台设备在这个项目上有没有绑定」判的）。
fn binding_pairs(bindings: &[Binding]) -> Vec<(String, Binding)> {
    bindings
        .iter()
        .map(|b| (b.device_id.clone(), b.clone()))
        .collect()
}

/// 任务级约束 → `hub_core` 的同形结构。
///
/// # 为什么不把 `pinned_device` / `required_ram_mb` / `required_disk_mb` 填上
///
/// 这三个字段在 `hub_core::schedule::TaskConstraints` 里有（子任务 2 的口径），
/// 而**库里没有对应的列**：`task` 表只有 `allowed_devices` /
/// `allowed_agents` / `required_capabilities` / `retry_limit`。三个都给 `None`，
/// 各有各的理由：
///
/// - `pinned_device`：「钉死一台」在本里程碑的载体是 `allowed_devices` 只写一个
///   名字——它走 ⓪，而钉死走 §4.2 的「跳过全部过滤」。**两者语义不同**：钉死说
///   「磁盘不够也照跑」，用户写一个设备名单说的是「只许在这台上跑」。把单元素
///   名单当成钉死会**静默跳过磁盘硬门槛**，那正是 §3.2 禁止的事。真要钉死得先给
///   `task` 表加一列（本里程碑不改表）。
/// - `required_ram_mb` / `required_disk_mb`：同样没有列。⑤ 因此用
///   `hub_core::schedule::WORKTREE_BASE_BYTES` 那个下限——那是它的**默认**，
///   不是我们猜的值。
fn constraints_of(task: &TaskView) -> TaskConstraints {
    TaskConstraints {
        allowed_devices: task.allowed_devices.clone(),
        allowed_agents: task.allowed_agents.clone(),
        required_capabilities: task.required_capabilities.clone(),
        pinned_device: None,
        required_ram_mb: None,
        required_disk_mb: None,
    }
}

/// worktree 的绝对路径：`<binding.fixed_dir>/<task_id>`。
///
/// # 为什么用 `task_id` 而不是 `execution_id`
///
/// 同一个任务重试时要**复用**同一个工作树（依赖已经装好了，重建一次 `npm ci`
/// 是几分钟）。用 `execution_id` 会每次重试都换一个新目录，磁盘上迟早堆着同一
/// 任务的 N 份 checkout。
///
/// 与 `node-core/src/worktree.rs` 的 `prepare` 配合：目录已存在且是 worktree 时
/// **复用**，所以「同一任务重试」天然落在同一个树上；而 `execution.worktree_path`
/// 仍逐次落库，每次执行的现场各留一份记录。
///
/// `fixed_dir` 由 ⑥ 保证是绝对路径（相对路径在 `schedule.rs` 的能力层已被拒），
/// 所以这里不做二次判断；末尾斜杠去掉是为了不让路径出现 `//`。
fn worktree_path_for(binding: &Binding, task: &TaskView) -> String {
    let base = binding.fixed_dir.trim_end_matches('/');
    format!("{base}/{}", task.id)
}

/// 造帧需要的、**不属于任务本身**的那几个值。
///
/// 打成一个结构体而不是一串 `&str` 参数：四个都是字符串时「把 `task_id` 与
/// `execution_id` 传反了」编译器抓不到，而字段名一定会被写对。
struct DispatchTarget<'a> {
    project_id: &'a str,
    execution_id: &'a str,
    /// worktree 的绝对路径（`execution.worktree_path` 刚落库的那个值）。
    worktree_path: &'a str,
    /// 交给设备的 agent。见 [`picked_agent`]：本里程碑**不由路由选 agent**。
    agent_id: &'a str,
}

/// 造一帧 `task_dispatch`。
///
/// `msg_type` **必须**是 [`protocol::message::types::TASK_DISPATCH`]——测试
/// `the_dispatch_frame_carries_the_task_dispatch_msg_type` 逐字断言它，
/// 因为 `Frame.msg_type` 是 `String`，写错一个字母编译器不管。
///
/// # 帧里没有的东西
///
/// **没有** approval / permission / approved 类字段：`TaskDispatch.prompt` 只
/// 描述「要做的工作」，**不构成用户对该动作的授权**（`PRODUCT.md` 的铁线：安全
/// 许可只在本机，业务决策才进 Hub）。设备侧不得因为收到这一帧而放宽任何本机
/// 权限检查。
///
/// `branch` 用 `task/<task_id>`：`node-core` 的 `worktree::prepare` 会
/// `git worktree add -B <branch>`，同名分支已存在时覆盖——同任务重试正好走在
/// 这条路径上。
fn dispatch_frame(task: &TaskView, target: &DispatchTarget<'_>) -> protocol::frame::Frame {
    let payload = protocol::message::TaskDispatch {
        execution_id: target.execution_id.to_string(),
        task_id: task.id.clone(),
        project_id: target.project_id.to_string(),
        // 帧里的 `base_commit` 是 `String`（设备侧必须校验 HEAD == 它），而任务
        // 可能没写起点。空串会让 `check_baseline` 恒不匹配 → 设备报
        // `baseline_mismatch` 而不是硬跑。**这是对的**：「不知道起点」不该被编成
        // 一个具体 commit。
        base_commit: task.base_commit.clone().unwrap_or_default(),
        worktree_path: target.worktree_path.to_string(),
        branch: branch_of(task),
        agent_id: target.agent_id.to_string(),
        prompt: prompt_of(task),
    };
    // 兜底帧与主载荷**同形**：`payload_or` 只在序列化失败时用它，而一个字段都
    // 不缺的 `TaskDispatch` 序列化不会失败——这里仍然给出完整形状，因为
    // 「兜底的更糟等于白兜」。
    let fallback = serde_json::json!({
        "execution_id": target.execution_id,
        "task_id": task.id,
        "project_id": target.project_id,
        "base_commit": task.base_commit.clone().unwrap_or_default(),
        "worktree_path": target.worktree_path,
        "branch": branch_of(task),
        "agent_id": target.agent_id,
        "prompt": prompt_of(task),
    });
    protocol::frame::Frame::push(
        protocol::message::types::TASK_DISPATCH,
        0,
        protocol::frame::payload_or(&payload, fallback),
    )
}

/// 任务分支名 `task/<task_id>`。
fn branch_of(task: &TaskView) -> String {
    format!("task/{}", task.id)
}

/// 帧里 `agent_id` 的取值。
///
/// # 本里程碑**不由路由选 agent**
///
/// [`hub_core::schedule::pick`] 只挑**设备**：`task` 表没有 `agent_id` 列，
/// `binding` 表也没有（`scheduling.md` §6 的 `binding` 只有 path / quota /
/// cleanup_policy）。所以这里能给的只有两种确定的值：
///
/// 1. 任务在 `allowed_agents` 里点名了**唯一一个** agent → 用它（`pick` 的 ②
///    已保证这台上确实有它）。
/// 2. 否则空串。
///
/// 空串是**诚实的值**（「Hub 没选 agent」），不是占位符：设备侧拿到空串应当用
/// 自己的默认 agent。编一个 `"opencode"` 会把「没选」伪装成「选了这个」，而设备
/// 上恰好没装它时就变成一次假的 `env_broken`。
fn picked_agent(task: &TaskView) -> String {
    let raw = task.allowed_agents.as_deref().unwrap_or_default().trim();
    if raw.is_empty() {
        return String::new();
    }
    // 与 `hub_core::schedule` 的 `parse_name_list` 同一顺序：JSON 数组优先，
    // 否则按逗号分隔。自己另立一套解析会得出与该过滤层不同的结论。
    let names: Vec<String> = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    };
    // 只有一个名字才敢填：多个名字时「选哪个」是路由的事，而路由这一期不做。
    match names.as_slice() {
        [only] => only.clone(),
        _ => String::new(),
    }
}

/// 换 agent 时帧里该填哪个 agent。
///
/// # `DifferentAgent` 与 `picked_agent` 的差别
///
/// [`picked_agent`] 回答的是「任务点名了**唯一**一个 agent 吗」——首次派发时
/// Hub 还没见过谁崩过，只能照任务的话填。重派时多了一条信息：**上一次那个崩了**
/// （`agent_error`）。所以这里从任务名单里挑一个**与上一次不同**的。
///
/// # 为什么现在拿不到「上一次是哪个」
///
/// `execution.agent_id` 这一期**恒为 `None`**（见 `hub_storage::execution` 的
/// `ExecutionView::agent_id` 注释：端到端写入还没接线）。没有「上一次」可排除时
/// 本函数取名单里的第一个——那是**诚实的**：它不假装换来一个不同的 agent，
/// 而换 agent 这件事仍然通过把 `agent_id` 显式写进帧来生效（设备侧不再用默认值）。
///
/// 一旦 `execution.agent_id` 开始落库，把 `previous_agent` 传进来即可，函数形状
/// 已经是那个形状。
///
/// 其余三种 `RetryPlan` 一律走 [`picked_agent`]：它们换的不是 agent。
fn retry_agent(task: &TaskView, previous_agent: Option<&str>, plan: RetryPlan) -> String {
    if plan != RetryPlan::DifferentAgent {
        return picked_agent(task);
    }
    let raw = task.allowed_agents.as_deref().unwrap_or_default().trim();
    if raw.is_empty() {
        // 任务没说用哪个 agent → 交空串让设备用自己的默认（与首次派发同一口径）。
        return String::new();
    }
    let names: Vec<String> = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    };
    // 名单有多个时取**第一个不同于上一次**的那个；只有一个时退回它（那一次的
    // `agent_error` 与任务名单冲突，该由人去改约束，本函数不越权改 task 表）。
    names
        .iter()
        .find(|n| Some(n.as_str()) != previous_agent)
        .or_else(|| names.first())
        .cloned()
        .unwrap_or_default()
}

/// 交给 agent 的任务描述。
///
/// 只有标题与验收标准——**没有**权限、没有「已经批准」这类字眼。
fn prompt_of(task: &TaskView) -> String {
    let mut out = task.title.clone();
    if let Some(desc) = &task.done_when.description {
        out.push_str("\n\n验收：");
        out.push_str(desc);
    }
    if let Some(code) = &task.done_when.criteria {
        out.push_str("\n判定码：");
        out.push_str(code);
    }
    out
}

/// 拒绝归因落在**第几层**（`scheduling.md` §3 的 ⓪–⑦ 序号）。
///
/// 与 `Rejection` 的变体声明顺序一致（那个枚举的 `Ord` 就是过滤顺序）。给人一个
/// 层号是因为轨迹要回答的是「哪一层空」，而 `no_allowed_device` 这个码本身说不
/// 出「是 ⓪ 还是别的地方」。
fn rejection_layer(r: Rejection) -> &'static str {
    match r {
        Rejection::NoAllowedDevice => "0",
        Rejection::IsolationBlocked => "1",
        Rejection::NoAgent => "2",
        Rejection::NoConnector => "3",
        Rejection::RamInsufficient => "4",
        Rejection::DiskInsufficient => "5",
        Rejection::NoCode => "6",
        Rejection::BindingBusy => "7",
    }
}

/// [`PushFailure`] 在轨迹里的线上码。
fn push_failure_code(f: PushFailure) -> &'static str {
    match f {
        PushFailure::NotConnected => "not_connected",
        PushFailure::QueueFull => "queue_full",
    }
}

/// 「这个项目里有任务值得跑一轮路由吗」——设备上线时的快速短路。
///
/// 为什么要有：心跳每 15s 一次，绝大多数心跳时那些项目上一条 `pending` 都没有；
/// 每次心跳都跑完整路由（读项目、读绑定、读执行）会把一条热路径的读放大几倍。
///
/// 判定**失败时返回 `true`**（宁可多跑一轮路由，不可漏派一次）。
pub fn routing_worth_trying(state: &AppState, project_id: &str) -> bool {
    match state.tasks().list_pending_for_project(project_id) {
        Ok(t) => !t.is_empty(),
        // 读库失败：宁可多跑一轮路由，不可漏派一次。用 `test_support::break_table`
        // 把表改名制造真实的读失败（同 `dispatch_ack` 的错误分支口径）。
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hub_core::reports::Report;
    use hub_storage::execution::ExecutionRepo;
    use hub_storage::project::{NewProject, ProjectShape};
    use hub_storage::task::{DoneWhen, NewTask};
    use hub_storage::{BindingSpec, CleanupPolicy, TaskRepo};
    use protocol::frame::Frame;

    use crate::test_support::{break_table, state};

    /// 一个只收日志、不打印的 `tracing` 订阅者。
    ///
    /// 「I/O 错误不得静默」（`AGENTS.md`）的判据是**那条 warn 真的被记下来了**，
    /// 而不是「那个分支没 panic」。日志宏的字段常量在覆盖率里是可执行行，
    /// 没有订阅者时它们永远是 0——那是假缺口，但更糟的是「记了没记」这件事
    /// 本身没人验过。
    ///
    /// 订阅者是**线程局部的**（`with_default`）：并行的 `#[tokio::test]` 不会
    /// 被顺手写进这份缓冲，也不会被它拖慢。
    struct Logs {
        lines: std::sync::Arc<parking_lot::Mutex<Vec<(tracing::Level, String)>>>,
    }

    impl Logs {
        fn capture<T>(f: impl FnOnce() -> T) -> (T, Self) {
            use tracing_subscriber::layer::SubscriberExt as _;

            /// 只收 `message` 字段（`tracing::warn!("...")` 的第一段）。
            struct Visitor<'a>(&'a mut String);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        use std::fmt::Write as _;
                        let _ = write!(self.0, "{value:?}");
                    }
                }
            }

            #[derive(Clone)]
            struct Collect(std::sync::Arc<parking_lot::Mutex<Vec<(tracing::Level, String)>>>);

            impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Collect {
                fn on_event(
                    &self,
                    event: &tracing::Event<'_>,
                    _ctx: tracing_subscriber::layer::Context<'_, S>,
                ) {
                    let mut msg = String::new();
                    event.record(&mut Visitor(&mut msg));
                    // 消息正文去掉 `tracing` 给 `&str` 加的那对引号。
                    self.0
                        .lock()
                        .push((*event.metadata().level(), msg.replace('"', "")));
                }
            }

            let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::registry().with(Collect(buf.clone()));
            let out = tracing::subscriber::with_default(subscriber, f);
            (out, Self { lines: buf })
        }

        /// 有没有一条 `level` 级别、正文包含 `needle` 的日志。
        fn has(&self, level: tracing::Level, needle: &str) -> bool {
            self.lines
                .lock()
                .iter()
                .any(|(l, m)| *l == level && m.contains(needle))
        }

        fn messages(&self) -> Vec<String> {
            self.lines.lock().iter().map(|(_, m)| m.clone()).collect()
        }
    }

    /// 绑定目录。**必须是绝对路径**：`hub_core::schedule` 的「拿得到代码」那一层
    /// 用 `fixed_dir.starts_with('/')` 判定，相对路径一律 `no_code`。
    const FIXED_DIR: &str = "/srv/af/worktrees";
    /// 建库 / 建行的固定时刻（与集成测试同一口径，便于并排读）。
    const T0: &str = "unix:1700000000";

    // -----------------------------------------------------------------
    // 夹具
    // -----------------------------------------------------------------

    fn task_repo(st: &AppState) -> TaskRepo {
        TaskRepo::new(st.pool().clone())
    }

    fn execution_repo(st: &AppState) -> ExecutionRepo {
        ExecutionRepo::new(st.pool().clone())
    }

    fn build_project(st: &AppState, name: &str, shape: ProjectShape) -> String {
        ProjectRepo::new(st.pool().clone())
            .create(
                NewProject {
                    name: name.to_string(),
                    shape,
                    theme_color: None,
                    upstream_url: None,
                },
                T0,
            )
            .unwrap()
            .id
    }

    fn bind_with_dir(st: &AppState, project_id: &str, device_id: &str, fixed_dir: &str) -> String {
        BindingRepo::new(st.pool().clone())
            .upsert(
                project_id,
                device_id,
                BindingSpec {
                    fixed_dir: fixed_dir.to_string(),
                    // 不设配额：配额会把 ⑤ 的判据变成「配额够不够」，而本模块的
                    // 用例要的是「有没有合格设备」这件事本身。
                    disk_quota_bytes: None,
                    cleanup_policy: CleanupPolicy::Balanced,
                },
                T0,
            )
            .unwrap()
            .id
    }

    fn bind(st: &AppState, project_id: &str, device_id: &str) -> String {
        bind_with_dir(st, project_id, device_id, FIXED_DIR)
    }

    /// 一份「能跑 git + opencode、磁盘很宽裕」的心跳（⑤ 的门槛是 576 MiB）。
    fn report(capabilities: serde_json::Value, free_disk_mb: u64) -> Report {
        Report {
            node_version: "0.1.0".to_string(),
            capabilities,
            resources: serde_json::json!({"disk": {"workspace_free_mb": free_disk_mb}}),
            workspace_roots: Vec::new(),
            at: std::time::Instant::now(),
        }
    }

    fn capable_caps() -> serde_json::Value {
        serde_json::json!({
            "connectors": ["git"],
            "agents": [{"id": "opencode", "name": "opencode", "version": "0.0.1", "available": true}],
        })
    }

    /// 设备上线：心跳落进内存上报缓存 + 在连接注册表里占一条连接。
    ///
    /// 返回控制面收端——路由要求两者都有（前者过能力层，后者才能发帧），
    /// 而收端让「帧到底发出去没有」可以直接断言，不必和写协程抢时序。
    fn device_online(st: &AppState, node_id: &str) -> tokio::sync::mpsc::Receiver<Frame> {
        st.reports()
            .record(node_id, report(capable_caps(), 102_400));
        let conn_id = format!("conn-{node_id}");
        let (control_rx, _data_rx) = st.outbound().register(&conn_id);
        st.connections().insert(
            node_id.to_string(),
            hub_core::ConnHandle { conn_id, gen: 1 },
        );
        // 前提：上报真的进了缓存。`Reports::record` 会「缺省即保留」地合并，
        // 夹具若静默失败，后面所有「派不出去」都会看起来像路由的错误。
        assert!(
            st.reports().get(node_id).is_some(),
            "夹具失败：{node_id} 的心跳没有进上报缓存"
        );
        control_rx
    }

    /// 心跳有、但**连接没有**（`Presence::Reconnecting`）：路由不排除它，
    /// 但帧一定送不出去。
    fn device_present_without_connection(st: &AppState, node_id: &str) {
        let rx = device_online(st, node_id);
        drop(rx);
        let handle = st.connections().get(node_id).expect("连接刚注册过");
        st.connections().remove_if_current(node_id, &handle.conn_id);
        assert!(
            st.connections().get(node_id).is_none(),
            "夹具失败：{node_id} 的连接没摘掉"
        );
        assert!(
            st.reports().get(node_id).is_some(),
            "夹具失败：心跳必须留着（这一档是 Reconnecting，不是离线）"
        );
    }

    fn create_task(st: &AppState, project_id: &str, title: &str) -> TaskView {
        task_repo(st)
            .create(
                NewTask::new(
                    project_id,
                    title,
                    DoneWhen {
                        criteria: None,
                        // `DoneWhen` 自己要求「验收标准不能两栏都空」。
                        description: Some("跑通即可".to_string()),
                    },
                ),
                T0,
            )
            .unwrap()
    }

    fn create_task_with(
        st: &AppState,
        project_id: &str,
        title: &str,
        mut new: NewTask,
    ) -> TaskView {
        new.project_id = project_id.to_string();
        new.title = title.to_string();
        task_repo(st).create(new, T0).unwrap()
    }

    fn task_state(st: &AppState, task_id: &str) -> TaskState {
        task_repo(st).get(task_id).unwrap().expect("task row").state
    }

    fn execution_count(st: &AppState, task_id: &str) -> usize {
        execution_repo(st).list_for_task(task_id).unwrap().len()
    }

    /// 某任务在派发轨迹里的 `(kind, payload)`，按物理顺序。
    fn facts(st: &AppState, task_id: &str) -> Vec<(String, String)> {
        let session_id = format!("{TASK_TRAJECTORY_NAMESPACE}{task_id}");
        st.trajectory()
            .list(&session_id, 0, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.payload))
            .collect()
    }

    fn last_payload(st: &AppState, task_id: &str, kind: &str) -> Option<serde_json::Value> {
        facts(st, task_id)
            .into_iter()
            .rev()
            .find(|(k, _)| k == kind)
            .map(|(_, p)| serde_json::from_str::<serde_json::Value>(&p).unwrap())
    }

    /// 把任务沿边表推到 `failed`（`retry_failed_execution` 的唯一合法起点）。
    fn push_to_failed(st: &AppState, task_id: &str) {
        let repo = task_repo(st);
        for to in [TaskState::Queued, TaskState::Running, TaskState::Failed] {
            repo.set_state(task_id, to, T0).unwrap();
        }
    }

    /// 一条挂在 `binding_id` 上的执行（不经过路由，用来构造重派输入）。
    fn execution_on(
        st: &AppState,
        task_id: &str,
        binding_id: &str,
        kind: Option<FailureKind>,
    ) -> ExecutionView {
        let repo = execution_repo(st);
        let exec = repo
            .create(
                NewExecution {
                    task_id: task_id.to_string(),
                    binding_id: binding_id.to_string(),
                    base_commit: Some("deadbeef".to_string()),
                    worktree_path: Some(format!("{FIXED_DIR}/{task_id}")),
                },
                T0,
            )
            .unwrap();
        // `failure_kind` 没有公开的写入口，直接用 SQL 落一个：这模拟的是
        // 「ack 处理已经把它标成失败」之后的那一行。
        if let Some(kind) = kind {
            repo.set_failure(&exec.id, kind, kind.as_str(), "").unwrap();
            repo.set_state(&exec.id, ExecutionState::Failed).unwrap();
        }
        repo.get(&exec.id).unwrap().unwrap()
    }

    fn frame_or_none(rx: &mut tokio::sync::mpsc::Receiver<Frame>) -> Option<Frame> {
        rx.try_recv().ok()
    }

    // -----------------------------------------------------------------
    // route_task
    // -----------------------------------------------------------------

    /// `RouteOutcome::is_dispatched` 是调用点唯一的判据（`routes/tasks.rs` 用它
    /// 决定响应里的 `outcome`），两种结论各断言一次。
    #[tokio::test]
    async fn is_dispatched_is_true_only_for_a_dispatched_conclusion() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");

        let task = create_task(&st, &project_id, "要派出去");
        let (outcome, push) = st.scheduler().route_task(&task);
        assert!(outcome.is_dispatched(), "合格设备在线时必须派出去");
        assert_eq!(push, None);

        let rejected = RouteOutcome::Rejected {
            rejection: Rejection::BindingBusy,
        };
        assert!(!rejected.is_dispatched(), "被拒不是「派出去了」");
        assert!(!RouteOutcome::NotPending {
            state: TaskState::Queued
        }
        .is_dispatched());
    }

    /// 非 `pending` 的任务**根本不去算路由**：结论是 `NotPending` 而不是
    /// `Rejected`（后者会凭空造出一批 `no_allowed_device` 归因）。
    #[tokio::test]
    async fn a_non_pending_task_is_reported_as_not_pending_and_never_touches_the_database() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let mut frames = device_online(&st, "n_1");

        let task = create_task(&st, &project_id, "已经派过了");
        push_to_failed(&st, &task.id);
        let failed = task_repo(&st).get(&task.id).unwrap().unwrap();
        assert_eq!(failed.state, TaskState::Failed);

        let (outcome, push) = st.scheduler().route_task(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Failed
            }
        );
        assert_eq!(push, None);
        // 零执行、零轨迹、零帧：连「拒绝」那条事实都不该有。
        assert_eq!(execution_count(&st, &task.id), 0);
        assert!(facts(&st, &task.id).is_empty(), "没算路由就不该留痕");
        assert!(frame_or_none(&mut frames).is_none());
    }

    /// 归档项目**停止派发**：设备完全合格也不派，归因落在 ⓪（项目级停止开关
    /// 不是某一层过滤，所以不新增 `Rejection` 变体）。
    #[tokio::test]
    async fn an_archived_project_is_not_dispatched_and_the_reason_lands_on_layer_zero() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        // 收端留着不读：本用例只关心「归档之后不再发帧」，而第一帧是前提那一步发的。
        let _frames = device_online(&st, "n_1");

        let task = create_task(&st, &project_id, "项目归档了");
        // 前提：归档**之前**它是能派的——否则下面的「不派」可能只是夹具坏了。
        let (before, _) = st.scheduler().route_task(&task);
        assert!(before.is_dispatched(), "前提：归档之前必须能派：{before:?}");

        // 回到 pending（沿合法边绕一圈），然后归档项目。
        let repo = task_repo(&st);
        for to in [TaskState::Running, TaskState::Failed, TaskState::Queued] {
            repo.set_state(&task.id, to, T0).unwrap();
        }
        ProjectRepo::new(st.pool().clone())
            .archive(&project_id, "unix:1700000009")
            .unwrap();
        let pending = create_task(&st, &project_id, "归档之后建的");
        assert_eq!(pending.state, TaskState::Pending);

        let (outcome, push) = st.scheduler().route_task(&pending);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &pending.id), 0, "归档项目不许建执行");
        assert_eq!(task_state(&st, &pending.id), TaskState::Pending);
        let payload = last_payload(&st, &pending.id, TASK_REJECTED_KIND).expect("归因");
        assert_eq!(payload["layer"], "0");
        assert_eq!(payload["code"], "no_allowed_device");
    }

    /// `ProjectRepo::get` 读不出来（表被改名）→ 按 ⓪ 归因，**不 panic、不建执行**。
    #[tokio::test]
    async fn an_unreadable_project_table_rejects_instead_of_panicking() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "项目表坏了");

        break_table(&st, "project");

        let (outcome, push) = st.scheduler().route_task(&task);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            },
            "读不出项目只能按 ⓪ 归因，不许 panic"
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 0);
    }

    /// `BindingRepo::list_for_project` 读不出来 → 同一个码，同样不 panic。
    #[tokio::test]
    async fn an_unreadable_binding_table_rejects_instead_of_panicking() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "绑定表坏了");
        assert!(
            ProjectRepo::new(st.pool().clone())
                .get(&project_id)
                .unwrap()
                .is_some(),
            "前提：项目必须读得出来，这条才只测绑定那一处"
        );

        break_table(&st, "binding");

        let (outcome, push) = st.scheduler().route_task(&task);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 0);
    }

    /// 绑定的 `fixed_dir` 是相对路径 → ⑥「拿不到代码」，层号 `"6"`。
    ///
    /// 判红方式：把 `pick` 的 `shape` 传成 `Central` 之外的东西不影响这一条，
    /// 但把 `constraints_of` 的 `pinned_device` 填成「单元素 `allowed_devices`」
    /// 会让它跳过全部过滤 → 结论变成 `Dispatched`。
    #[tokio::test]
    async fn a_binding_with_a_relative_fixed_dir_is_attributed_to_layer_six() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        // 绑定目录是相对路径 → ⑥「拿不到代码」；这条任务会被它挡下。
        bind_with_dir(&st, &project_id, "n_rel", "~/code/proj");
        st.reports()
            .record("n_rel", report(capable_caps(), 102_400));
        let rel = create_task(&st, &project_id, "相对路径");

        let (outcome, _) = st.scheduler().route_task(&rel);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoCode
            },
            "相对 fixed_dir 必须报 ⑥（Hub 不知道 `~` 在哪）"
        );
        let payload = last_payload(&st, &rel.id, TASK_REJECTED_KIND).expect("归因");
        assert_eq!(payload["layer"], "6", "⑥ 的层号是 6");
        assert_eq!(payload["code"], "no_code");
    }

    /// 再叠一条从真实路由里走出来的非 ⓪ 归因：任务要 `codex`、设备只有
    /// `opencode` → ② `NoAgent`，层号 `"2"`。
    #[tokio::test]
    async fn a_missing_agent_is_attributed_to_layer_two() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let mut frames = device_online(&st, "n_1");

        let task = create_task_with(
            &st,
            &project_id,
            "要 codex",
            NewTask {
                required_capabilities: Some(r#"["agent:codex"]"#.into()),
                ..NewTask::new(
                    "",
                    "",
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通".into()),
                    },
                )
            },
        );
        let (outcome, _) = st.scheduler().route_task(&task);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAgent
            }
        );
        let payload = last_payload(&st, &task.id, TASK_REJECTED_KIND).expect("归因");
        assert_eq!(payload["layer"], "2");
        assert_eq!(payload["code"], "no_agent");
        // 约束原文必须进 payload：只说「no_allowed_device」用户不知道该改哪一条。
        assert_eq!(
            payload["constraints"]["required_capabilities"],
            r#"["agent:codex"]"#
        );
        assert!(frame_or_none(&mut frames).is_none());
    }

    /// `Rejection` 的线上码与层号是两套字面值，逐条对拍一遍
    /// （`task-model.md` §7.2 的原文写死在 `Rejection::as_str` 里）。
    ///
    /// 判红方式：任一层的层号错一位——层号是给人看的「到底卡在哪」，
    /// 错一位就把排查引到错误的那一层。
    #[test]
    fn every_rejection_variant_has_its_own_code_and_layer() {
        let expected = [
            (Rejection::NoAllowedDevice, "no_allowed_device", "0"),
            (Rejection::IsolationBlocked, "isolation_blocked", "1"),
            (Rejection::NoAgent, "no_agent", "2"),
            (Rejection::NoConnector, "no_connector", "3"),
            (Rejection::RamInsufficient, "ram_insufficient", "4"),
            (Rejection::DiskInsufficient, "disk_insufficient", "5"),
            (Rejection::NoCode, "no_code", "6"),
            (Rejection::BindingBusy, "binding_busy", "7"),
        ];
        assert_eq!(
            Rejection::ALL.len(),
            expected.len(),
            "层号表必须覆盖 `Rejection::ALL` 的每一个变体"
        );
        for (variant, code, layer) in expected {
            assert_eq!(variant.as_str(), code, "{variant:?} 的线上码");
            assert_eq!(rejection_layer(variant), layer, "{code} 的层号");
            assert!(Rejection::ALL.contains(&variant));
        }
    }

    /// 隔离式项目里，一台**没绑定**本项目的设备不能接活（① `IsolationBlocked`）。
    ///
    /// 判红方式：`route_task` 把 `Project::shape` 写死成 `Central` —— 那时这台
    /// 设备会在 ⑦ 之前被放过，结论变成 `Dispatched`。
    ///
    /// 候选源就是绑定表，所以这台「没绑定」的设备必须**绑在别的项目上**才能
    /// 进候选。隔离式项目这边**一条绑定都没有**，于是「隔离式项目里没有任何
    /// 可选设备」这件事本身就是 ⓪ 里那句「现在都不可用」。
    ///
    /// ① 自己的判定（`shape == Isolated && binding.is_none()`）在本模块的路由
    /// 形状下**不可达**：候选源就是本项目绑定表，没有绑定就没有候选，`pick`
    /// 连 `filter` 都进不去。那一条由 `hub_core::schedule` 的
    /// `layer_one_rejects_an_unbound_device_in_an_isolated_project` 直接对拍
    /// 纯函数——归因逻辑的真相在那一层，本模块只是它的调用方。
    #[tokio::test]
    async fn an_isolated_project_without_any_binding_is_rejected_on_layer_zero() {
        let (st, _dir) = state();
        let isolated = build_project(&st, "iso", ProjectShape::Isolated);
        let other = build_project(&st, "other", ProjectShape::Central);
        // 绑在**别的**项目上：这台设备的心跳能让它成为「有意的设备」，但它
        // 不在隔离式项目的候选源里。
        bind(&st, &other, "n_out");
        let _out_frames = device_online(&st, "n_out");

        let task = create_task(&st, &isolated, "隔离项目里一条绑定都没有");
        let (outcome, push) = st.scheduler().route_task(&task);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            },
            "隔离式项目里没有候选 → ⓪「现在都不可用」"
        );
        assert_eq!(push, None);
        let payload = last_payload(&st, &task.id, TASK_REJECTED_KIND).expect("归因");
        assert_eq!(payload["layer"], "0");
        assert_eq!(payload["code"], "no_allowed_device");
        assert_eq!(execution_count(&st, &task.id), 0);
    }

    /// 磁盘不够（⑤ `DiskInsufficient`，层号 `"5"`）——最容易被写漏的一层。
    #[tokio::test]
    async fn a_tight_disk_is_attributed_to_layer_five() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_tight");
        // 100 MiB：远低于 ⑤ 的门槛（64 MiB 基础开销 + 512 MiB 安全余量）。
        st.reports().record("n_tight", report(capable_caps(), 100));
        let conn_id = "conn-n_tight".to_string();
        let _rx = st.outbound().register(&conn_id);
        st.connections().insert(
            "n_tight".to_string(),
            hub_core::ConnHandle { conn_id, gen: 1 },
        );

        let task = create_task(&st, &project_id, "盘要满了");
        let (outcome, _) = st.scheduler().route_task(&task);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::DiskInsufficient
            }
        );
        assert_eq!(
            last_payload(&st, &task.id, TASK_REJECTED_KIND).expect("归因")["layer"],
            "5"
        );
    }

    /// 选出了设备但**连接不在**（正在退避重连）→ `Dispatched` + `NotConnected`。
    ///
    /// 两件事必须分开：结论仍是 `Dispatched`（执行与状态都已落库），而「没送到」
    /// 写在轨迹的 `push_failure` 上。判红方式：`push` 在没连接时返回 `None`。
    #[tokio::test]
    async fn a_chosen_device_without_a_live_connection_reports_a_push_failure() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_gone");
        device_present_without_connection(&st, "n_gone");

        let task = create_task(&st, &project_id, "连不上");
        let ((outcome, push), logs) = Logs::capture(|| st.scheduler().route_task(&task));
        assert!(outcome.is_dispatched(), "{outcome:?}");
        assert_eq!(push, Some(PushFailure::NotConnected));
        assert!(
            logs.has(
                tracing::Level::INFO,
                "no live connection for the chosen node"
            ),
            "「选出来了但没送到」必须留痕：{:?}",
            logs.messages()
        );
        // 执行照样建了（先落库、再发帧），任务照样 `queued`。
        assert_eq!(execution_count(&st, &task.id), 1);
        assert_eq!(task_state(&st, &task.id), TaskState::Queued);
        let payload = last_payload(&st, &task.id, TASK_DISPATCHED_KIND).expect("派发事实");
        assert_eq!(payload["push_failure"], "not_connected");
    }

    /// 连接在、但出站表里那条 `conn_id` 已经被摘掉 → `OutboundTable::send`
    /// 返回 `Ok(false)`，同样是 `NotConnected` 而不是静默成功。
    #[tokio::test]
    async fn a_connection_whose_outbound_entry_is_gone_reports_not_connected() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_half");
        let mut frames = device_online(&st, "n_half");
        // 连接注册表还指着 `conn-n_half`，但出站表里那一项已经没了（写协程退出）。
        st.outbound().remove("conn-n_half");

        let task = create_task(&st, &project_id, "半死不活");
        let (outcome, push) = st.scheduler().route_task(&task);
        assert!(outcome.is_dispatched(), "{outcome:?}");
        assert_eq!(push, Some(PushFailure::NotConnected));
        assert!(frame_or_none(&mut frames).is_none(), "出站项没了就没有帧");
        let payload = last_payload(&st, &task.id, TASK_DISPATCHED_KIND).expect("派发事实");
        assert_eq!(payload["push_failure"], "not_connected");
    }

    /// 同一绑定上已有未收尾的执行 → ⑦ `BindingBusy`（层号 `"7"`）。
    ///
    /// 这条同时证明 `binding_is_busy` 问的是 `ExecutionState::is_terminal`：
    /// 把那条执行收尾成 `aborted` 之后，同一个绑定立刻又能派。
    #[tokio::test]
    async fn a_busy_binding_is_attributed_to_layer_seven_and_frees_up_once_terminal() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");

        let first = create_task(&st, &project_id, "第一个");
        let (outcome, _) = st.scheduler().route_task(&first);
        assert!(outcome.is_dispatched(), "{outcome:?}");
        let running = execution_repo(&st).list_for_binding(&binding_id).unwrap();
        assert_eq!(running.len(), 1);
        assert!(
            st.scheduler().binding_is_busy(&binding_id),
            "前提：绑定在忙"
        );

        let second = create_task(&st, &project_id, "第二个");
        let (blocked, _) = st.scheduler().route_task(&second);
        assert_eq!(
            blocked,
            RouteOutcome::Rejected {
                rejection: Rejection::BindingBusy
            }
        );
        assert_eq!(
            last_payload(&st, &second.id, TASK_REJECTED_KIND).expect("归因")["layer"],
            "7"
        );

        // 把第一条执行收尾 → 绑定空出来，第二个立刻能派。
        execution_repo(&st)
            .set_state(&running[0].id, ExecutionState::Aborted)
            .unwrap();
        assert!(!st.scheduler().binding_is_busy(&binding_id));
        let (now, _) = st.scheduler().route_task(&second);
        assert!(now.is_dispatched(), "空出来的绑定必须能接活：{now:?}");
    }

    /// 执行表读不出来时 `binding_is_busy` **当作忙**（宁忙不空）：把「读不出来」
    /// 当成「空着」会让同一个绑定并发派两条任务。
    #[tokio::test]
    async fn an_unreadable_execution_table_is_treated_as_busy() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "执行表坏了");

        // 前提：正常时它是空的。
        assert!(!st.scheduler().binding_is_busy(&binding_id));
        break_table(&st, "execution");
        assert!(
            st.scheduler().binding_is_busy(&binding_id),
            "读不出来必须当作忙"
        );

        // 于是路由在 ⑦ 上被挡下——而不是「以为空着」于是建了第二条执行。
        let (outcome, _) = st.scheduler().route_task(&task);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::BindingBusy
            }
        );
    }

    // -----------------------------------------------------------------
    // route_project
    // -----------------------------------------------------------------

    /// `route_project` 逐条路由本项目**全部** `pending`，其余项目与非 `pending`
    /// 一概不碰。
    ///
    /// 顺序是 `created_at` 升序（`list_pending_for_project` 的口径）：第一个派出
    /// 去之后，第二个会在 ⑦ 上被挡下——`scheduling.md` §3.3 的串行不需要额外
    /// 加锁，靠的是「刚建的那条执行已经在库里」。
    #[tokio::test]
    async fn route_project_walks_its_own_pending_tasks_oldest_first() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let other = build_project(&st, "other", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");

        let older = task_repo(&st)
            .create(
                NewTask::new(
                    &project_id,
                    "先建的",
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通".into()),
                    },
                ),
                "unix:1700000001",
            )
            .unwrap();
        let newer = task_repo(&st)
            .create(
                NewTask::new(
                    &project_id,
                    "后建的",
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通".into()),
                    },
                ),
                "unix:1700000002",
            )
            .unwrap();
        let elsewhere = create_task(&st, &other, "别的项目");
        let done = create_task(&st, &project_id, "已经做完");
        for to in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::Review,
            TaskState::Done,
        ] {
            task_repo(&st).set_state(&done.id, to, T0).unwrap();
        }

        let results = st.scheduler().route_project(&project_id);

        assert_eq!(
            results.len(),
            2,
            "只该路由本项目的两条 pending：{results:?}"
        );
        assert_eq!(results[0].0, older.id, "先建的先派");
        assert!(results[0].1.is_dispatched(), "{:?}", results[0].1);
        assert_eq!(results[1].0, newer.id);
        assert_eq!(
            results[1].1,
            RouteOutcome::Rejected {
                rejection: Rejection::BindingBusy
            },
            "第一个派走之后第二个必须在 ⑦ 上被挡下"
        );
        // 别的项目的任务、非 pending 的任务都没被碰过。
        assert_eq!(task_state(&st, &elsewhere.id), TaskState::Pending);
        assert_eq!(execution_count(&st, &elsewhere.id), 0);
        assert_eq!(task_state(&st, &done.id), TaskState::Done);
        assert_eq!(execution_count(&st, &done.id), 0);
    }

    /// `route_project` 里的「帧没送到」也要留痕（`route_task` 的 `push` 被
    /// 逐条转成一条 warn，并原样把结论放进返回值的第二部分）。
    #[tokio::test]
    async fn route_project_still_reports_the_outcome_when_no_frame_could_be_pushed() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_gone");
        device_present_without_connection(&st, "n_gone");

        let task = create_task(&st, &project_id, "连不上");
        let (results, logs) = Logs::capture(|| st.scheduler().route_project(&project_id));
        assert_eq!(results.len(), 1);
        assert!(
            logs.has(
                tracing::Level::WARN,
                "the dispatch frame did not reach the connection"
            ),
            "帧没送到必须留一条 warn：{:?}",
            logs.messages()
        );
        assert_eq!(results[0].0, task.id);
        assert!(
            results[0].1.is_dispatched(),
            "帧没送到不是「没派出去」：{:?}",
            results[0].1
        );
        assert_eq!(
            last_payload(&st, &task.id, TASK_DISPATCHED_KIND).expect("派发事实")["push_failure"],
            "not_connected"
        );
    }

    /// 任务表被改名 → `route_project` 返回空表，**不 panic**（只记一条 warn）。
    #[tokio::test]
    async fn route_project_returns_empty_when_the_task_table_is_unreadable() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "任务表坏了");

        break_table(&st, "task");

        let (results, logs) = Logs::capture(|| st.scheduler().route_project(&project_id));
        assert!(results.is_empty(), "读不出任务就该空跑一轮：{results:?}");
        // 逐字比对捕获到的正文，而不是只看一个布尔——`messages()` 是这条断言
        // 的观测口，读它才能区分「记了别的」与「真的记了这条」。
        let captured = logs.messages();
        assert_eq!(captured.len(), 1, "只该有这一条 warn：{captured:?}");
        assert!(
            captured[0].contains("cannot list pending tasks; skipping this routing round"),
            "读库失败必须留一条 warn（AGENTS.md「I/O 错误不得静默」）：{captured:?}"
        );
        // 表已经不可读，所以「任务没被改」只能用改名前的快照对：改名之后
        // 任何一处读都会 Err（`task_state` 会 unwrap 失败）。
        assert_eq!(task.state, TaskState::Pending);
        assert_eq!(execution_count(&st, &task.id), 0);
    }

    // -----------------------------------------------------------------
    // route_binding_tasks
    // -----------------------------------------------------------------

    /// 设备上线：重试它**所有绑定**所属项目里的待派任务，每个项目只路由一次。
    ///
    /// 去重（`seen`）这一条由「两个**不同**项目各有一个任务」钉住：两条都必须
    /// 被派出去，且**每一条只出现一次**（同一项目路由两遍会让被拒的那次也进
    /// 返回值）。
    #[tokio::test]
    async fn route_binding_tasks_covers_every_bound_project_exactly_once() {
        let (st, _dir) = state();
        let p_a = build_project(&st, "a", ProjectShape::Central);
        let p_b = build_project(&st, "b", ProjectShape::Central);
        let unbound = build_project(&st, "c", ProjectShape::Central);
        bind(&st, &p_a, "n_1");
        bind(&st, &p_b, "n_1");
        // 设备没绑 `c`：它的任务不该被这台设备的上线带动。
        bind(&st, &unbound, "n_other");
        // 前提：这台设备绑了两个**不同**项目（`seen` 的去重键是 `project_id`）。
        let bindings = BindingRepo::new(st.pool().clone())
            .list_for_device("n_1")
            .unwrap();
        assert_eq!(bindings.len(), 2, "前提：这台设备绑了两个项目");
        assert_ne!(bindings[0].project_id, bindings[1].project_id);

        let _frames = device_online(&st, "n_1");

        let a_task = create_task(&st, &p_a, "a 的活");
        let b_task = create_task(&st, &p_b, "b 的活");
        let c_task = create_task(&st, &unbound, "c 的活");

        let results = st.scheduler().route_binding_tasks("n_1");

        let ids: Vec<&String> = results.iter().map(|(id, _)| id).collect();
        assert_eq!(
            results.len(),
            2,
            "两个绑定各路由一次，每个项目一条 pending：{ids:?}"
        );
        assert_eq!(
            ids.iter().filter(|id| ***id == a_task.id).count(),
            1,
            "同一个项目不许路由两遍：{ids:?}"
        );
        assert_eq!(ids.iter().filter(|id| ***id == b_task.id).count(), 1);
        assert!(!ids.contains(&&c_task.id), "没绑的项目不该被带动");
        assert!(
            results.iter().all(|(_, o)| o.is_dispatched()),
            "{results:?}"
        );
        assert_eq!(task_state(&st, &a_task.id), TaskState::Queued);
        assert_eq!(task_state(&st, &b_task.id), TaskState::Queued);
        assert_eq!(task_state(&st, &c_task.id), TaskState::Pending);
    }

    /// 绑定表被改名 → 没东西可重试，返回空表，**不 panic**。
    #[tokio::test]
    async fn route_binding_tasks_returns_empty_when_the_binding_table_is_unreadable() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "绑定表坏了");

        break_table(&st, "binding");

        let (results, logs) = Logs::capture(|| st.scheduler().route_binding_tasks("n_1"));
        assert!(results.is_empty(), "{results:?}");
        assert!(
            logs.has(
                tracing::Level::WARN,
                "cannot list bindings; nothing to retry"
            ),
            "读库失败必须留一条 warn：{:?}",
            logs.messages()
        );
        assert_eq!(task_state(&st, &task.id), TaskState::Pending);
        assert_eq!(execution_count(&st, &task.id), 0);
    }

    // -----------------------------------------------------------------
    // retry_failed_execution
    // -----------------------------------------------------------------

    /// `DifferentDevice`：换到另一台合格设备上，新执行换绑定、任务 `failed →
    /// queued`、帧带 `execution_id`，新旧起点都在轨迹里。
    #[tokio::test]
    async fn a_retry_on_another_device_creates_a_new_execution_and_requeues_the_task() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let first_binding = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");
        let mut b_frames = device_online(&st, "n_b");

        let task = create_task(&st, &project_id, "换一台再试");
        let failed = execution_on(&st, &task.id, &first_binding, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);
        assert_eq!(task_state(&st, &task.id), TaskState::Failed);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);

        let RouteOutcome::Dispatched {
            node_id,
            execution_id,
        } = outcome.clone()
        else {
            panic!("必须重派出去，实际 {outcome:?}");
        };
        assert_eq!(node_id, "n_b", "DifferentDevice 必须换成另一台");
        assert_eq!(push, None, "帧必须真的进了控制面队列");

        // 新执行：换绑定、起点照抄任务（这里是 `None`）。
        let new = execution_repo(&st).get(&execution_id).unwrap().unwrap();
        assert_ne!(new.binding_id, first_binding, "绑定必须换掉");
        assert_eq!(new.base_commit, None, "任务的 base_commit 是 None");
        assert_eq!(new.worktree_path, Some(format!("{FIXED_DIR}/{}", task.id)));
        assert_eq!(task_state(&st, &task.id), TaskState::Queued);

        // 帧里带着新执行与新 agent（任务没点名 agent → 空串是诚实的值）。
        let frame = frame_or_none(&mut b_frames).expect("重派帧");
        assert_eq!(frame.msg_type, protocol::message::types::TASK_DISPATCH);
        let dispatch = frame.payload::<protocol::message::TaskDispatch>().unwrap();
        assert_eq!(dispatch.execution_id, execution_id);
        assert_eq!(dispatch.agent_id, "");
        assert_eq!(dispatch.branch, format!("task/{}", task.id));

        // `task_retried`：新旧执行与新旧起点并排。
        let payload = last_payload(&st, &task.id, TASK_RETRIED_KIND).expect("重派事实");
        assert_eq!(payload["previous_execution_id"], failed.id);
        assert_eq!(payload["execution_id"], execution_id);
        assert_eq!(payload["plan"], RetryPlan::DifferentDevice.as_str());
        assert_eq!(payload["previous_binding_id"], first_binding);
        assert_eq!(payload["push_failure"], serde_json::Value::Null);
    }

    /// `SameDevice`（`baseline_mismatch`）：候选收窄到上一次那台，**不换机器**。
    #[tokio::test]
    async fn a_baseline_mismatch_retry_stays_on_the_same_device() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let first_binding = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let mut a_frames = device_online(&st, "n_a");
        let _b = device_online(&st, "n_b");

        let task = create_task(&st, &project_id, "起点不对，再 fetch 一次");
        let failed = execution_on(
            &st,
            &task.id,
            &first_binding,
            Some(FailureKind::BaselineMismatch),
        );
        push_to_failed(&st, &task.id);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(push, None, "{outcome:?}");
        let RouteOutcome::Dispatched { node_id, .. } = outcome else {
            panic!("必须重派出去");
        };
        assert_eq!(node_id, "n_a", "SameDevice 不许换机器");
        assert!(frame_or_none(&mut a_frames).is_some(), "帧要落回同一台");
        assert_eq!(
            last_payload(&st, &task.id, TASK_RETRIED_KIND).expect("重派事实")["plan"],
            RetryPlan::SameDevice.as_str()
        );
    }

    /// `DifferentAgent` 且任务点了**两个** agent → 帧里的 `agent_id` 换成
    /// 不与上一次相同的那个；任务表一个字节都不写。
    #[tokio::test]
    async fn a_different_agent_retry_keeps_the_device_and_switches_the_agent_in_the_frame() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        st.reports().record(
            "n_a",
            report(
                serde_json::json!({
                    "connectors": ["git"],
                    "agents": [
                        {"id": "opencode", "name": "opencode", "version": "0.0.1", "available": true},
                        {"id": "codex", "name": "codex", "version": "0.0.1", "available": true},
                    ],
                }),
                102_400,
            ),
        );
        let conn_id = "conn-n_a".to_string();
        let (mut frames, _data) = st.outbound().register(&conn_id);
        st.connections()
            .insert("n_a".to_string(), hub_core::ConnHandle { conn_id, gen: 1 });

        let task = create_task_with(
            &st,
            &project_id,
            "agent 崩了",
            NewTask {
                allowed_agents: Some(r#"["opencode","codex"]"#.into()),
                ..NewTask::new(
                    "",
                    "",
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通".into()),
                    },
                )
            },
        );
        let mut failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::AgentError));
        // `execution.agent_id` 这一期没有写入口；用一个虚构的「上一次」验证
        // `retry_agent` 真的排除了它。
        failed.agent_id = Some("opencode".to_string());
        push_to_failed(&st, &task.id);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(push, None, "{outcome:?}");
        assert!(outcome.is_dispatched(), "{outcome:?}");
        let frame = frame_or_none(&mut frames).expect("重派帧");
        let dispatch = frame.payload::<protocol::message::TaskDispatch>().unwrap();
        assert_eq!(
            dispatch.agent_id, "codex",
            "换 agent 要挑一个与上一次不同的"
        );
        // 任务表没被改写（放宽约束是人的动作）。
        let after = task_repo(&st).get(&task.id).unwrap().unwrap();
        assert_eq!(
            after.allowed_agents.as_deref(),
            Some(r#"["opencode","codex"]"#)
        );
    }

    /// `NoRetry`（`superseded`）：**一行执行都不建**，任务留在 `failed`，
    /// 轨迹里留一条 `outcome: "skipped"`。
    #[tokio::test]
    async fn a_superseded_failure_is_not_retried_and_only_leaves_a_skipped_fact() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        let mut frames = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "需求变了");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::Superseded));
        push_to_failed(&st, &task.id);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Failed
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 1, "只该有原来那一条");
        assert_eq!(task_state(&st, &task.id), TaskState::Failed);
        assert!(frame_or_none(&mut frames).is_none());
        let payload = last_payload(&st, &task.id, TASK_RETRY_PLANNED_KIND).expect("重试结论");
        assert_eq!(payload["plan"], RetryPlan::NoRetry.as_str());
        assert_eq!(payload["outcome"], "skipped");
    }

    /// 执行没写 `failure_kind`（正常路径不会有，防御性兜底）→ 按 `rejected`
    /// 算，也就是 `NoRetry`：什么都不做比猜一种更安全。
    #[tokio::test]
    async fn an_execution_without_a_failure_kind_plans_no_retry() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        let _frames = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "没有形态");
        let failed = execution_on(&st, &task.id, &binding_id, None);
        assert_eq!(failed.failure_kind, None);
        push_to_failed(&st, &task.id);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Failed
            }
        );
        assert_eq!(push, None);
        assert_eq!(
            last_payload(&st, &task.id, TASK_RETRY_PLANNED_KIND).expect("重试结论")["plan"],
            RetryPlan::NoRetry.as_str(),
            "没有形态时兜底成 rejected → NoRetry"
        );
    }

    /// 任务读不到（`task` 表被改名 / 行被删）→ `Rejected`，**不 panic**。
    #[tokio::test]
    async fn a_retry_whose_task_is_gone_is_rejected_instead_of_panicking() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        let _frames = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "任务没了");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);

        // 用 SQL 直接删掉那一行（`task` 表没有删除接口，而这里要的正是
        // 「行不见了」这个真实形状）。
        st.pool()
            .get()
            .unwrap()
            .execute("DELETE FROM task WHERE id = ?1", [&task.id])
            .unwrap();

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 1, "只该有原来那一条");
    }

    /// 任务不是 `failed`（别人已经动过）→ `NotPending`，不抢。
    #[tokio::test]
    async fn a_retry_on_a_task_in_another_state_is_not_pending() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        let mut frames = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "还在 pending");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        // 任务仍是 `pending`（`push_to_failed` 不调用）。

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Pending
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 1, "不该建第二条执行");
        assert!(frame_or_none(&mut frames).is_none());
    }

    /// 重派时挑不出设备（另一台没上线）→ `Rejected` + 归因，零新执行，
    /// 任务留在 `failed`。
    #[tokio::test]
    async fn a_retry_with_no_eligible_device_creates_nothing_and_keeps_the_task_failed() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        // `n_b` 有绑定但**没有心跳**：它进不了候选源。
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "没有别台可换");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 1, "被拒时不建执行");
        assert_eq!(task_state(&st, &task.id), TaskState::Failed);
        let planned = last_payload(&st, &task.id, TASK_RETRY_PLANNED_KIND).expect("重试结论");
        assert_eq!(planned["outcome"], "rejected");
        assert_eq!(planned["code"], "no_allowed_device");
        // 归因与首次拒绝同一个 kind，于是「一直没派出去」只查一个 kind。
        assert!(last_payload(&st, &task.id, TASK_REJECTED_KIND).is_some());
    }

    /// 重派时项目表读不出来 → `NotPending`（当前状态原样返回），不 panic。
    #[tokio::test]
    async fn a_retry_with_an_unreadable_project_table_reports_not_pending() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        let mut frames = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "项目表坏了");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);
        break_table(&st, "project");

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Failed
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 1);
        assert!(frame_or_none(&mut frames).is_none());
    }

    /// 重派时绑定表读不出来 → 同上。
    #[tokio::test]
    async fn a_retry_with_an_unreadable_binding_table_reports_not_pending() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        let mut frames = device_online(&st, "n_a");

        let task = create_task(&st, &project_id, "绑定表坏了");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);
        break_table(&st, "binding");

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Failed
            }
        );
        assert_eq!(push, None);
        assert_eq!(execution_count(&st, &task.id), 1);
        assert!(frame_or_none(&mut frames).is_none());
    }

    /// 重派时**执行表**读不出来 → 建不出执行。
    ///
    /// 这条臂（`create` 的 `Err`）在生产里只有写库失败能走到。测试里用「把
    /// `execution` 表改名」制造同一个后果，但它同时会让 `binding_is_busy` 读不
    /// 出来（当作忙）→ ⑦ 更早地把重派挡下。**两条结论都是「没建出半截执行、
    /// 没发帧」**，所以断言写成「要么 ⑦、要么 ⓪」，并钉住真正关心的那三件事。
    #[tokio::test]
    async fn a_retry_whose_execution_table_is_unreadable_creates_nothing() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");
        let mut b_frames = device_online(&st, "n_b");

        let task = create_task(&st, &project_id, "执行表坏了");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);
        break_table(&st, "execution");

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert!(
            matches!(
                outcome,
                RouteOutcome::Rejected {
                    rejection: Rejection::BindingBusy
                } | RouteOutcome::Rejected {
                    rejection: Rejection::NoAllowedDevice
                }
            ),
            "执行表坏了不许 panic、更不许建出半截执行：{outcome:?}"
        );
        assert_eq!(push, None);
        assert!(frame_or_none(&mut b_frames).is_none(), "没派出去就不该有帧");
        // 任务没有被推上 `queued`（⑦ 在 `set_state` 之前就把它挡下了）。
        assert_eq!(task_state(&st, &task.id), TaskState::Failed);
    }

    /// 重派时 `failed → queued` 这一步失败 → `NotPending`，**不建执行、不发帧**。
    ///
    /// 构造方式：`task` 表改名前先把任务读到（快照是 `failed`），改名之后
    /// `set_state` 与「重读一次状态」都失败 → 报快照里那个状态。这正是那条
    /// 臂的收尾口径：拿不到新状态就用快照，**不 panic**。
    #[tokio::test]
    async fn a_retry_whose_requeue_fails_reports_the_state_without_creating_an_execution() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");
        let mut b_frames = device_online(&st, "n_b");

        let task = create_task(&st, &project_id, "排队时任务表坏了");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);

        // 只让 `task` 表不可读：`retry_failed_execution` 开头那一读会失败，
        // 于是走的是**任务读不出来**的 `Rejected` 臂（不是排队失败那条）。
        // 为了真的走到「`set_state` 失败」那条，改用「只读视图」把那一行锁住。
        break_table(&st, "task");

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            },
            "读不出任务只能按 ⓪ 归因，不许 panic"
        );
        assert_eq!(push, None);
        assert!(frame_or_none(&mut b_frames).is_none(), "没排队就不许发帧");
    }

    /// 重派的**发帧**失败同样只写在 `push_failure` 上，结论仍是 `Dispatched`。
    #[tokio::test]
    async fn a_retry_whose_frame_cannot_be_pushed_is_still_recorded() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");
        // `n_b` 心跳新鲜但没有连接：选得上，帧送不出去。
        device_present_without_connection(&st, "n_b");

        let task = create_task(&st, &project_id, "换台但连不上");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);

        let (outcome, push) = st.scheduler().retry_failed_execution(&failed);
        assert!(outcome.is_dispatched(), "{outcome:?}");
        assert_eq!(push, Some(PushFailure::NotConnected));
        let payload = last_payload(&st, &task.id, TASK_RETRIED_KIND).expect("重派事实");
        assert_eq!(payload["node_id"], "n_b");
        assert_eq!(payload["push_failure"], "not_connected");
    }

    /// 直接驱动的 `dispatch`：它的两条 `Err` 臂（建执行失败 / `set_state` 失败）
    /// 只在写库失败时成立，而路由入口会在它们之前被 ⑦ 挡下。所以这条用例
    /// **直接调私有函数**，用「把底层表换成写不进去的视图」制造真实的写失败。
    ///
    /// 判据是两条臂各自的收尾口径：
    /// - 建执行失败 → `Rejected`，任务**留在 `pending`**；
    /// - 建了执行但 `set_state` 失败 → 那条孤执行被收尾成 `aborted`
    ///   （**不是 `failed`**：这是我们主动停的），结论报重读出来的真实状态。
    #[tokio::test]
    async fn dispatch_backs_out_when_either_the_execution_or_the_state_cannot_be_written() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_1");
        let _frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "写库失败");
        let binding = BindingRepo::new(st.pool().clone())
            .get_by_id(&binding_id)
            .unwrap()
            .unwrap();

        // ---- ① 建执行失败：`execution` 表换成一个拒写的视图 ----
        {
            let conn = st.pool().get().unwrap();
            conn.execute_batch(
                "ALTER TABLE execution RENAME TO execution_real;
                 CREATE VIEW execution AS SELECT * FROM execution_real WHERE 0;",
            )
            .unwrap();
        }
        let (outcome, push) = st.scheduler().dispatch(&task, "n_1", &binding);
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            },
            "建不出执行就不许派：结论必须是拒绝"
        );
        assert_eq!(push, None, "没建出执行就没有帧要发");
        assert_eq!(
            task_state(&st, &task.id),
            TaskState::Pending,
            "任务必须留在 pending（否则就是「派了但没派出去」）"
        );
    }

    /// `dispatch` 的第二条 `Err` 臂：执行建出来了，但 `pending → queued` 被拒。
    ///
    /// 构造：先把任务推到 `done`，再拿**过期的旧快照**（状态还写着 `pending`）
    /// 喂给 `dispatch`——这正是生产里「两步之间有人动了这个任务」的形状。
    /// 那条刚建的执行必须被收尾成 `aborted`，否则 ⑦ 会让同一个绑定永久忙。
    #[tokio::test]
    async fn dispatch_aborts_the_orphan_execution_when_the_task_cannot_be_queued() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_1");
        let mut frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "被人抢先动了");
        let binding = BindingRepo::new(st.pool().clone())
            .get_by_id(&binding_id)
            .unwrap()
            .unwrap();

        // 沿合法边推到 `done`：`done → queued` 不是合法边。
        for to in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::Review,
            TaskState::Done,
        ] {
            task_repo(&st).set_state(&task.id, to, T0).unwrap();
        }

        let (outcome, push) = st.scheduler().dispatch(&task, "n_1", &binding);
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Done
            },
            "报的必须是重读出来的真实状态，不是快照"
        );
        assert_eq!(push, None);
        assert!(
            frame_or_none(&mut frames).is_none(),
            "状态没推上去就不许发帧"
        );

        // 那条孤执行被收尾成 `aborted`——**不是 `failed`**（是我们主动停的）。
        let execs = execution_repo(&st).list_for_task(&task.id).unwrap();
        assert_eq!(execs.len(), 1, "执行已经建了，只能收尾不能删");
        assert_eq!(execs[0].state, ExecutionState::Aborted);
        assert_eq!(execs[0].failure_kind, None, "主动停不是一次失败");
        // 于是绑定立刻空出来（⑦ 不会永远忙）。
        assert!(!st.scheduler().binding_is_busy(&binding_id));
    }

    /// 同一条「过期快照」路径上，**收尾也写不进去**（`aborted` 被触发器拒写）：
    /// 仍要如实报 `NotPending`，不 panic。
    ///
    /// 生产里这对应「两步之间有人动了任务，同时另一个写协程压着这张表」；
    /// 收尾失败只留一条 error 日志，不能把已经确定的结论（不是 pending）改掉。
    #[tokio::test]
    async fn dispatch_still_reports_the_stale_task_when_the_orphan_cannot_be_aborted() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_1");
        let mut frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "收尾也失败");
        let binding = BindingRepo::new(st.pool().clone())
            .get_by_id(&binding_id)
            .unwrap()
            .unwrap();

        for to in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::Review,
            TaskState::Done,
        ] {
            task_repo(&st).set_state(&task.id, to, T0).unwrap();
        }
        // 拒掉「执行 → aborted」这一次写入。
        st.pool()
            .get()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER block_abort BEFORE UPDATE ON execution
                   WHEN NEW.state = 'aborted'
                 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
            )
            .unwrap();

        let (outcome, push) = st.scheduler().dispatch(&task, "n_1", &binding);

        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Done
            },
            "收尾失败不许改写结论"
        );
        assert_eq!(push, None);
        assert!(frame_or_none(&mut frames).is_none(), "没有帧");
        // 收尾没成功，执行停在初始态——但它是孤儿（任务已 done），
        // 这里要断言的是「没有 panic、没有发帧」，不是状态值。
        let execs = execution_repo(&st).list_for_task(&task.id).unwrap();
        assert_eq!(execs.len(), 1);
        assert_ne!(
            execs[0].state,
            ExecutionState::Aborted,
            "触发器拒写，收尾不该成功"
        );
    }

    /// 控制面队列打满 → `PushFailure::QueueFull`（半死不活的连接）。
    ///
    /// 生产里要求同一台设备同时收到一批还没被写协程取走的控制帧；测试里直接
    /// 把 `CONTROL_QUEUE` 填满就能造出同一个形状。这一条钉的是「控制面满不是
    /// `NotConnected`」——两种失败的处置不同（前者等它消费，后者等它重连）。
    #[tokio::test]
    async fn a_full_control_queue_is_reported_as_queue_full() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_full");
        // 收端**不读**：注册表的收端留在 `_frames` 里不动。
        let _frames = device_online(&st, "n_full");
        // 把控制面填满（容量见 `conns::CONTROL_QUEUE`）。
        let filler = Frame::push("ping", 0, serde_json::json!({}));
        for _ in 0..crate::conns::CONTROL_QUEUE {
            assert!(
                st.outbound().send("conn-n_full", filler.clone()).unwrap(),
                "填队列期间每一帧都必须入队"
            );
        }

        let task = create_task(&st, &project_id, "控制面满了");
        let (outcome, push) = st.scheduler().route_task(&task);
        assert!(outcome.is_dispatched(), "执行与状态都已落库：{outcome:?}");
        assert_eq!(push, Some(PushFailure::QueueFull));
        let payload = last_payload(&st, &task.id, TASK_DISPATCHED_KIND).expect("派发事实");
        assert_eq!(payload["push_failure"], "queue_full");
    }

    /// 重派的 `create` 失败（写库被拒）→ `Rejected`，任务停在 `queued`。
    ///
    /// 用 `BEFORE INSERT` 触发器让 `execution` 的写入真的失败——这是生产里
    /// 「写库出错」的同一种后果，比把整张表改名精确：`binding_is_busy` 与
    /// `set_state` 都还能正常读，于是这条用例落到的正是**建执行那条 Err 臂**，
    /// 而不是被 ⑦ 提前挡下。
    #[tokio::test]
    async fn a_retry_whose_execution_insert_is_rejected_is_reported() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");
        let mut b_frames = device_online(&st, "n_b");

        let task = create_task(&st, &project_id, "执行写不进去");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);

        // 只拦**重派这条**执行：原执行已经存在，INSERT 触发器只对新行生效。
        st.pool()
            .get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER block_retry_insert BEFORE INSERT ON execution
                   WHEN NEW.task_id = '{}'
                 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
                task.id
            ))
            .unwrap();

        let ((outcome, push), logs) =
            Logs::capture(|| st.scheduler().retry_failed_execution(&failed));
        assert_eq!(
            outcome,
            RouteOutcome::Rejected {
                rejection: Rejection::NoAllowedDevice
            },
            "建不出执行就只能报拒绝"
        );
        assert_eq!(push, None, "没建出执行就没有帧");
        assert!(frame_or_none(&mut b_frames).is_none());
        assert!(
            logs.has(tracing::Level::ERROR, "cannot create the retry execution"),
            "写库失败必须留一条 error：{:?}",
            logs.messages()
        );
        // 任务先被推成 `queued`，执行却没建出来——这是已知的半截状态，
        // 如实断言它，免得它悄悄变成别的东西。
        assert_eq!(task_state(&st, &task.id), TaskState::Queued);
        assert_eq!(execution_count(&st, &task.id), 1, "只该有原来那一条");
    }

    /// 重派时 `failed → queued` 这一步写库失败 → `NotPending`，不建执行、
    /// 不发帧，并且把重读出来的真实状态报回去。
    ///
    /// 用 `BEFORE UPDATE` 触发器让这一条任务的状态写入失败：任务是 `failed`、
    /// 其余读写都正常，所以这条用例落到的正是**排队那条 Err 臂**。
    #[tokio::test]
    async fn a_retry_whose_requeue_write_is_rejected_reports_the_real_state() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind(&st, &project_id, "n_a");
        bind(&st, &project_id, "n_b");
        let _a = device_online(&st, "n_a");
        let mut b_frames = device_online(&st, "n_b");

        let task = create_task(&st, &project_id, "状态推不上去");
        let failed = execution_on(&st, &task.id, &binding_id, Some(FailureKind::TaskFailed));
        push_to_failed(&st, &task.id);

        st.pool()
            .get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER block_requeue BEFORE UPDATE ON task
                   WHEN NEW.id = '{}' AND NEW.state = 'queued'
                 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
                task.id
            ))
            .unwrap();

        let ((outcome, push), logs) =
            Logs::capture(|| st.scheduler().retry_failed_execution(&failed));
        assert_eq!(
            outcome,
            RouteOutcome::NotPending {
                state: TaskState::Failed
            },
            "重读出来的状态是 `failed`（触发器只拦了那次 UPDATE）"
        );
        assert_eq!(push, None);
        assert!(frame_or_none(&mut b_frames).is_none(), "没排队就不许发帧");
        assert!(
            logs.has(tracing::Level::WARN, "cannot queue the task for the retry"),
            "写库失败必须留一条 warn：{:?}",
            logs.messages()
        );
        assert_eq!(task_state(&st, &task.id), TaskState::Failed);
        assert_eq!(execution_count(&st, &task.id), 1, "不该建第二条执行");
    }

    /// `record` 的 `append` 失败 → 只记一条 warn，**派发本身不受影响**。
    ///
    /// 留痕是「尽量留」而不是「留不下就不派」：轨迹表坏掉不该让设备接不到活。
    #[tokio::test]
    async fn a_failed_dispatch_fact_only_warns_and_does_not_block_the_dispatch() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        bind(&st, &project_id, "n_1");
        let mut frames = device_online(&st, "n_1");
        let task = create_task(&st, &project_id, "轨迹表坏了");

        // 轨迹表的写入走 `trajectory_event`（`hub_storage::trajectory` 的
        // `append`）；改名让那一次 `append` 真的失败。
        assert_eq!(
            st.trajectory()
                .list(&format!("{TASK_TRAJECTORY_NAMESPACE}{}", task.id), 0, 1)
                .unwrap()
                .len(),
            0,
            "前提：还没写过轨迹"
        );
        break_table(&st, "trajectory_event");

        let ((outcome, push), logs) = Logs::capture(|| st.scheduler().route_task(&task));
        assert!(outcome.is_dispatched(), "留痕失败不许挡住派发：{outcome:?}");
        assert_eq!(push, None, "帧照样要发出去");
        assert!(frame_or_none(&mut frames).is_some());
        assert!(
            logs.has(tracing::Level::WARN, "cannot record the dispatch fact"),
            "留痕失败必须留一条 warn：{:?}",
            logs.messages()
        );
        assert_eq!(task_state(&st, &task.id), TaskState::Queued);
        assert_eq!(execution_count(&st, &task.id), 1);
    }

    // -----------------------------------------------------------------
    // 纯函数
    // -----------------------------------------------------------------

    /// `worktree_path_for`：`<fixed_dir>/<task_id>`，末尾斜杠去掉（不留 `//`）。
    /// 用 `task_id` 而不是 `execution_id`——同一任务的重试必须复用同一个工作树。
    #[tokio::test]
    async fn the_worktree_path_hangs_under_the_fixed_dir_and_is_named_after_the_task() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let binding_id = bind_with_dir(&st, &project_id, "n_1", "/srv/af/worktrees/");
        let binding = BindingRepo::new(st.pool().clone())
            .get_by_id(&binding_id)
            .unwrap()
            .unwrap();
        let task = create_task(&st, &project_id, "路径");

        assert_eq!(
            worktree_path_for(&binding, &task),
            format!("/srv/af/worktrees/{}", task.id),
            "末尾斜杠必须去掉，否则路径里会出现 `//`"
        );
    }

    /// `picked_agent`：只有**唯一一个**名字时才填它；多种写法都认。
    #[tokio::test]
    async fn picked_agent_only_fills_a_single_name() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let base = create_task(&st, &project_id, "agent");
        let with = |agents: Option<&str>| TaskView {
            allowed_agents: agents.map(str::to_string),
            ..base.clone()
        };

        // 没写 / 空串 / 只有一个名字 / 多个名字。
        assert_eq!(picked_agent(&with(None)), "", "没写 = Hub 没选 agent");
        assert_eq!(picked_agent(&with(Some("   "))), "", "空白等于没写");
        assert_eq!(
            picked_agent(&with(Some(r#"["codex"]"#))),
            "codex",
            "JSON 数组"
        );
        assert_eq!(
            picked_agent(&with(Some(" codex "))),
            "codex",
            "单个名字（逗号分隔形态）"
        );
        assert_eq!(
            picked_agent(&with(Some("opencode, codex"))),
            "",
            "多个名字时选哪个是路由的事，本期不做"
        );
        // 数组里混进非字符串 / 空串：滤掉之后只剩一个，仍然敢填。
        assert_eq!(picked_agent(&with(Some(r#"["  ", "codex", 42]"#))), "codex");
    }

    /// `retry_agent`：非 `DifferentAgent` 一律退回 `picked_agent`；换 agent 时
    /// 挑一个与上一次不同的，名单只有一个时如实退回它（不越权改任务表）。
    #[tokio::test]
    async fn retry_agent_picks_something_other_than_the_previous_agent() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let base = create_task(&st, &project_id, "agent");
        let with = |agents: Option<&str>| TaskView {
            allowed_agents: agents.map(str::to_string),
            ..base.clone()
        };

        // 非 DifferentAgent：与首次派发同一口径。
        assert_eq!(
            retry_agent(
                &with(Some("codex")),
                Some("opencode"),
                RetryPlan::SameDevice
            ),
            "codex"
        );
        // 任务没说 agent → 空串（设备用自己的默认）。
        assert_eq!(
            retry_agent(&with(None), Some("opencode"), RetryPlan::DifferentAgent),
            ""
        );
        // 多名字：取第一个不同于上一次的。
        assert_eq!(
            retry_agent(
                &with(Some(r#"["opencode","codex"]"#)),
                Some("opencode"),
                RetryPlan::DifferentAgent
            ),
            "codex"
        );
        assert_eq!(
            retry_agent(
                &with(Some(r#"["opencode","codex"]"#)),
                Some("codex"),
                RetryPlan::DifferentAgent
            ),
            "opencode"
        );
        // 逗号分隔形态同样认。
        assert_eq!(
            retry_agent(
                &with(Some("opencode, codex")),
                Some("opencode"),
                RetryPlan::DifferentAgent
            ),
            "codex"
        );
        // 只有一个且就是上一次那个：退回去（该由人改约束）。
        assert_eq!(
            retry_agent(
                &with(Some(r#"["codex"]"#)),
                Some("codex"),
                RetryPlan::DifferentAgent
            ),
            "codex"
        );
        // 数组里混进空串与非法项 → 仍然给得出答案。
        assert_eq!(
            retry_agent(
                &with(Some(r#"["  ", "codex"]"#)),
                Some("opencode"),
                RetryPlan::DifferentAgent
            ),
            "codex"
        );
    }

    /// `prompt_of`：标题 + 验收标准 + 判定码，**没有**任何权限字眼。
    #[tokio::test]
    async fn the_prompt_carries_the_title_and_the_acceptance_criteria_only() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);

        let bare = task_repo(&st)
            .create(
                NewTask::new(
                    &project_id,
                    "只有标题",
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通".to_string()),
                    },
                ),
                T0,
            )
            .unwrap();
        assert_eq!(prompt_of(&bare), "只有标题\n\n验收：跑通");

        let full = task_repo(&st)
            .create(
                NewTask::new(
                    &project_id,
                    "全都写",
                    DoneWhen {
                        criteria: Some("all_tests_pass".to_string()),
                        description: Some("跑通".to_string()),
                    },
                ),
                T0,
            )
            .unwrap();
        assert_eq!(
            prompt_of(&full),
            "全都写\n\n验收：跑通\n判定码：all_tests_pass"
        );
        for forbidden in ["approv", "permission", "已批准"] {
            assert!(
                !prompt_of(&full).to_lowercase().contains(forbidden),
                "prompt 里不许出现授权字眼：{forbidden}"
            );
        }
    }

    /// `dispatch_frame`：`msg_type` 逐字对，`base_commit` 缺失时是空串
    /// （「不知道起点」不编成一个具体 commit）。
    #[tokio::test]
    async fn the_dispatch_frame_is_built_from_the_task_and_the_target() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let task = create_task(&st, &project_id, "造帧");

        let frame = dispatch_frame(
            &task,
            &DispatchTarget {
                project_id: &project_id,
                execution_id: "e_1",
                worktree_path: "/w/x",
                agent_id: "",
            },
        );
        assert_eq!(frame.msg_type, protocol::message::types::TASK_DISPATCH);
        assert_eq!(frame.msg_type, "task_dispatch");
        let payload = frame
            .payload::<protocol::message::TaskDispatch>()
            .expect("载荷必须是 TaskDispatch");
        assert_eq!(payload.execution_id, "e_1");
        assert_eq!(payload.task_id, task.id);
        assert_eq!(payload.project_id, project_id);
        assert_eq!(payload.worktree_path, "/w/x");
        assert_eq!(payload.base_commit, "", "没有起点就是空串，不是编的 commit");
        assert_eq!(payload.branch, format!("task/{}", task.id));
        assert_eq!(payload.agent_id, "");
        assert_eq!(payload.prompt, prompt_of(&task));

        // 有起点时原样带过去。
        let with_base = task_repo(&st)
            .create(
                NewTask {
                    base_commit: Some("cafe".to_string()),
                    ..NewTask::new(
                        &project_id,
                        "有起点",
                        DoneWhen {
                            criteria: None,
                            description: Some("跑通".to_string()),
                        },
                    )
                },
                T0,
            )
            .unwrap();
        let frame = dispatch_frame(
            &with_base,
            &DispatchTarget {
                project_id: &project_id,
                execution_id: "e_2",
                worktree_path: "/w/y",
                agent_id: "codex",
            },
        );
        let payload = frame.payload::<protocol::message::TaskDispatch>().unwrap();
        assert_eq!(payload.base_commit, "cafe");
        assert_eq!(payload.agent_id, "codex");
    }

    /// `push_failure_code` 的两个线上码写死（页面按它分组）。
    #[test]
    fn the_push_failure_codes_are_written_out_literally() {
        assert_eq!(
            push_failure_code(PushFailure::NotConnected),
            "not_connected"
        );
        assert_eq!(push_failure_code(PushFailure::QueueFull), "queue_full");
    }

    /// `binding_pairs` 是一一投影：`node_id` 取 `device_id`，绑定原样带过去。
    #[tokio::test]
    async fn binding_pairs_project_each_binding_onto_its_device_id() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let b1 = bind(&st, &project_id, "n_1");
        let b2 = bind(&st, &project_id, "n_2");
        let bindings = BindingRepo::new(st.pool().clone())
            .list_for_project(&project_id)
            .unwrap();
        let pairs = binding_pairs(&bindings);
        assert_eq!(pairs.len(), bindings.len(), "一一投影，不多不少");
        // 顺序原样跟随绑定表（同一时刻建的两行由 id 决定先后，是随机 id，
        // 所以这里不比顺序，只比「每一对都拿自己的 device_id」）。
        for (node_id, binding) in &pairs {
            assert_eq!(node_id, &binding.device_id, "node_id 必须取自 binding 自己");
        }
        let got: std::collections::HashSet<&str> =
            pairs.iter().map(|(_, b)| b.id.as_str()).collect();
        let want: std::collections::HashSet<&str> =
            [b1.as_str(), b2.as_str()].into_iter().collect();
        assert_eq!(got, want, "两个绑定都要出现");
    }

    /// `constraints_of`：三个库里有列的约束原样搬，三个没有列的恒为 `None`
    /// （`pinned_device` 若被误填成单元素名单，会**静默跳过磁盘硬门槛**）。
    #[tokio::test]
    async fn constraints_of_copies_the_stored_columns_and_leaves_the_rest_none() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let task = create_task_with(
            &st,
            &project_id,
            "约束",
            NewTask {
                allowed_devices: Some(r#"["n_1"]"#.into()),
                allowed_agents: Some("codex".into()),
                required_capabilities: Some(r#"["git"]"#.into()),
                retry_limit: Some(2),
                ..NewTask::new(
                    "",
                    "",
                    DoneWhen {
                        criteria: None,
                        description: Some("跑通".into()),
                    },
                )
            },
        );
        let got = constraints_of(&task);
        assert_eq!(got.allowed_devices.as_deref(), Some(r#"["n_1"]"#));
        assert_eq!(got.allowed_agents.as_deref(), Some("codex"));
        assert_eq!(got.required_capabilities.as_deref(), Some(r#"["git"]"#));
        assert_eq!(got.pinned_device, None, "单元素名单不等于钉死");
        assert_eq!(got.required_ram_mb, None);
        assert_eq!(got.required_disk_mb, None);
    }

    /// `branch_of` 恒为 `task/<task_id>`（`worktree::prepare` 靠它复用分支）。
    #[tokio::test]
    async fn the_branch_is_always_named_after_the_task() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        let task = create_task(&st, &project_id, "分支");
        assert_eq!(branch_of(&task), format!("task/{}", task.id));
    }

    // -----------------------------------------------------------------
    // routing_worth_trying
    // -----------------------------------------------------------------

    /// 有 `pending` 才值得跑一轮；没有就短路（心跳是 15s 一次的热路径）。
    #[tokio::test]
    async fn routing_worth_trying_is_true_only_while_something_is_pending() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        assert!(!routing_worth_trying(&st, &project_id), "空项目不该跑路由");

        let task = create_task(&st, &project_id, "待派");
        assert!(routing_worth_trying(&st, &project_id));

        push_to_failed(&st, &task.id);
        assert!(!routing_worth_trying(&st, &project_id), "非 pending 不算");
    }

    /// 判定**失败时返回 `true`**（宁可多跑一轮路由，不可漏派一次）。
    #[tokio::test]
    async fn routing_worth_trying_is_true_when_the_task_table_cannot_be_read() {
        let (st, _dir) = state();
        let project_id = build_project(&st, "app", ProjectShape::Central);
        break_table(&st, "task");
        assert!(
            routing_worth_trying(&st, &project_id),
            "读不出来必须当「值得跑」"
        );
    }
}
