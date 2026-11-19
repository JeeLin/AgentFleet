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
            // coverage: excluded — 项目只归档不删除，`Ok(None)` 正常走不到；
            // `Err` 要先把库读坏。两条都归到同一个码，见 `AGENTS.md`
            // 「生产代码禁 panic 铁律」。
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
                // coverage: excluded — 读绑定失败 = 库坏了，同上。
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
            // coverage: excluded — 读库失败（表被改坏）只有故障注入能造。
            // 不 panic、不留假事实，只记日志。
            Err(e) => {
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
            // coverage: excluded — 读库失败，见 `route_project` 同一个理由。
            Err(e) => {
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
            // coverage: excluded — 读库失败（表被改坏），不 panic 也不编事实。
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
            // coverage: excluded — 项目只归档不删除；见 `route_task` 同一条臂。
            Ok(None) | Err(_) => return (RouteOutcome::NotPending { state: task.state }, None),
        };
        let bindings =
            match BindingRepo::new(self.state.pool().clone()).list_for_project(&task.project_id) {
                Ok(b) => b,
                // coverage: excluded — 读库失败，同上。
                Err(_) => return (RouteOutcome::NotPending { state: task.state }, None),
            };
        // 上一次那台设备是从绑定反查出来的（`ExecutionView` 只存 `binding_id`）。
        let failed_device = bindings
            .iter()
            .find(|b| b.id == failed.binding_id)
            .map(|b| b.device_id.clone());

        // ---- ② `pick` 的候选源：`DifferentDevice` 时排除刚失败的那台 ----
        let pairs: Vec<(String, Binding)> = match plan {
            // 换 agent：候选只有上一次那台设备（`device_id` 不变）。
            RetryPlan::DifferentAgent => bindings
                .iter()
                .filter(|b| Some(&b.device_id) == failed_device.as_ref())
                .map(|b| (b.device_id.clone(), b.clone()))
                .collect(),
            // 换设备：**排除**刚失败的那台（否则「换一台」在同一台上再跑一次）。
            RetryPlan::DifferentDevice => bindings
                .iter()
                .filter(|b| Some(&b.device_id) != failed_device.as_ref())
                .map(|b| (b.device_id.clone(), b.clone()))
                .collect(),
            // 同设备重来 / 不重试：前者只留那台，后者不留（下面直接返回）。
            RetryPlan::SameDevice => bindings
                .iter()
                .filter(|b| Some(&b.device_id) == failed_device.as_ref())
                .map(|b| (b.device_id.clone(), b.clone()))
                .collect(),
            RetryPlan::NoRetry => Vec::new(),
        };
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
            // coverage: excluded — 写库失败，只有故障注入能造。
            Err(e) => {
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
            // coverage: excluded — 建执行失败 = 写库失败，只有故障注入能造。
            Err(e) => {
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
                // coverage: excluded — 收尾失败 = 写库失败。
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
            // coverage: excluded — 控制面队列打满要求同一台设备同时收到一批
            // 还没被写协程取走的控制帧；本里程碑一条连接一次只会有一个在途
            // 派发。`PushError::QueueFull` 的映射本身由 `conns.rs` 的单测钉住。
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
        // coverage: excluded — 读库失败；见上：宁可多跑。
        Err(_) => true,
    }
}
