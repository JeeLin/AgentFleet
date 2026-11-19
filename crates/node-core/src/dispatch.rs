//! 设备侧的派发状态机：收 `task_dispatch` → 建 worktree → 校验基线 → 执行 → 回报。
//!
//! 规格是 `scheduling.md` §5（派发与落定）与里程碑 v0.8.2 子任务 4，流程：
//!
//! ```text
//! 收到 TaskDispatch:
//!   stage=Accepted → 回 ack
//!   worktree = 复用已有 / 新建
//!   HEAD == base_commit ?
//!     否 → stage=BaselineMismatch, reason 说清「本地 HEAD 是 X，要求 Y」，**不硬跑**
//!     是 → stage=Running → 执行
//!           │  执行期间按周期取 worktree 实际占用
//!           ├─ 超预算阈值 → **提前中止** stage=Failed{ disk_exhausted }
//!           │               保留前 N 个产物做诊断，不保留整个工作树
//!           └─ 正常结束 → stage=Succeeded / stage=Failed + failure_kind
//! ```
//!
//! # 三条硬约束
//!
//! 1. **`baseline_mismatch` 绝不硬跑**：`HEAD` 不符立刻回报，**一个字节的 agent
//!    都不执行**。在错误基线上跑出来的结果看起来正常但实际不可信，比报错更危险
//!    （`task-model.md` §3.1）。
//! 2. **执行中必须监控磁盘**：`task-model.md` 对 `disk_exhausted` 的定义就是
//!    「执行中磁盘写满 / 超预算阈值被提前中止」。不实现它，生产代码里就没有任何
//!    地方能产出 `disk_exhausted`，下游「`disk_exhausted` → 换设备」成了一条
//!    永远走不到的分支（`scheduling.md` §3.2）。
//! 3. **`prompt` 不是权限批准**：`TaskDispatch.prompt` 只描述要做的工作，
//!    **不构成用户授权**。本模块因此**没有、也不会有**任何「因为收到 dispatch
//!    所以放行」的路径——见 [`crate::dispatch::DispatchOutcome`] 与
//!    `local_permission_verdict` 的注释。
//!
//! # 与 worker 的分工
//!
//! 本模块**不碰 socket**：它只把「一帧进来 → 该回哪些帧」算出来，帧由
//! [`crate::worker::Worker`] 的会话循环发出去（同 `channel` 模块的分工，
//! 好处是这些语义不必起真连接就能测）。帧与帧之间的实时回报（`accepted`
//! 先走、`running` 随后）由 worker 用回调发行，见 [`AckSink`]。

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use protocol::message::{DispatchStage, TaskDispatch, TaskDispatchAck, FAILURE_KINDS};

use crate::worktree::{self, BaselineVerdict};

/// 磁盘监控的默认采样间隔。
///
/// 1 秒是「足够早地发现写满」与「别把设备 IO 打满」之间的折中：一个狂写构建
/// 产物的 agent 每秒能吃几十 MiB，1 秒的滞后最多让超支几十 MiB——相比「等它把
/// 整台机器写满」已经是两个量级。**测试里调成毫秒级**（[`DispatchConfig`] 可注入），
/// 否则一条用例要跑好几秒。
pub const DEFAULT_DISK_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// 磁盘预算的默认阈值（字节）。
///
/// **这是兜底值，不是业务配额**：真正的配额来自绑定的 `quota`
/// （`scheduling.md` §3.2 的「这台机器上这个项目允许多大」），由 Hub 侧决定、
/// 设备侧的绑定配置持有。本期 `TaskDispatch` 帧里**不含预算字段**（协议不为
/// 一件事造两个真相源），所以设备侧先用一个保守的默认值顶上：
/// 8 GiB 够一个中大型仓库的 checkout 加一轮构建产物，又不至于在一台
/// 256 GiB 的机器上刚刚开始就中止。
pub const DEFAULT_DISK_BUDGET_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// 中止时**保留**的产物个数 N（做诊断用）。
///
/// 「保留前 N 个产物，但不保留整个工作树」——整棵树正是要避免的磁盘灾难，
/// 而一个都不留等于把定位问题的线索一起删了。
pub const DEFAULT_KEEP_ARTIFACTS: usize = 8;

/// 被保留产物的存放目录名（在 worktree 的父目录下，不在 worktree 里）。
///
/// **必须在 worktree 之外**：中止时要删掉的是整个工作树，把要保留的东西放在
/// 被删的那棵树里，一删就没了。
pub const ARTIFACTS_DIR_NAME: &str = ".afcli-dispatch-artifacts";

/// 一次派发的可注入配置。
///
/// 监控间隔、预算阈值、保留个数全都从这里来，**不写死**：测试要把阈值调成
/// 几百字节、间隔调成毫秒级才跑得动，而生产默认值（[`DispatchConfig::default`]）
/// 是给人看的量级。这也是本项目一贯的做法——判据要被能测的量驱动。
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// 磁盘采样间隔。
    pub disk_sample_interval: Duration,
    /// 预算阈值（字节）。worktree 实测占用**超过**它就提前中止。
    pub disk_budget_bytes: u64,
    /// 中止时保留的产物个数（最近的 N 个）。
    pub keep_artifacts: usize,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            disk_sample_interval: DEFAULT_DISK_SAMPLE_INTERVAL,
            disk_budget_bytes: DEFAULT_DISK_BUDGET_BYTES,
            keep_artifacts: DEFAULT_KEEP_ARTIFACTS,
        }
    }
}

/// 一条被拒的派发帧：**记下来，不静默吞掉**。
///
/// `stage` 闭集之外的取值、缺必填字段、`execution_id` 为空——这些都是「Hub 与
/// 设备对协议的理解已经分叉」的信号，静默丢弃会让 Hub 那侧永远等不到回报，
/// 表现成「任务卡在 queued」而设备日志一片安静。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// 说得清为什么拒的原因串（进日志）。
    pub reason: String,
    /// 能解出来的 `execution_id`（解不出就是 `None`）——有它才能回一条
    /// 带归因的 `Failed` 帧，让 Hub 知道该结算哪次执行。
    pub execution_id: Option<String>,
}

/// 一帧入站载荷 → 一个动作。
#[derive(Debug, Clone)]
pub enum Incoming {
    /// 认下来的活。
    Dispatch(Box<TaskDispatch>),
    /// 拒掉的帧（原因在里面）。
    Rejected(Rejected),
}

/// 已处理过的 `execution_id` 集合 + 「正在跑」的标记。
///
/// # 为什么「认领」和「完成」分开
///
/// 派发是**长任务**：从 `accepted` 到 `succeeded` 可能是几十分钟。这期间 Hub
/// 若因为重连重发一次（它不知道上一帧有没有到），第二帧必须被挡住——所以
/// `execution_id` 在 [`Self::claim`] 的那一刻就进集合，不是在跑完之后才进。
///
/// 集合**只在内存里**（一个 worker 进程的生命周期内）：进程重启后 Hub 会重新
/// 派活，那时「不记得」正是对的——它要把那条执行继续下去。
#[derive(Debug, Default)]
pub struct Ledger {
    entries: Mutex<HashSet<String>>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 认领一个 `execution_id`。返回 `false` = 已经认领过（重复投递）。
    pub fn claim(&self, execution_id: &str) -> bool {
        match self.entries.lock() {
            Ok(mut set) => set.insert(execution_id.to_string()),
            // 锁中毒（持锁线程 panic 过）：临界区里只有一次 `HashSet::insert`，
            // 走到这里说明别处出了问题，但**幂等必须继续成立**——所以取回内层
            // 集合并照常认领，而不是 panic 或放行。
            Err(poisoned) => poisoned.into_inner().insert(execution_id.to_string()),
        }
    }

    /// 忘掉一个 `execution_id`（执行走到终态之后）。
    ///
    /// **不这样做集合会无限长大**：一台设备跑上一年就是几万条字符串。
    /// 忘掉之后同一条执行若再被派一次，会重新执行——这可以接受：`Hub` 只会
    /// 在**没有收到终态回报**时重派，那种情形下重跑一遍正是它想要的。
    pub fn forget(&self, execution_id: &str) {
        match self.entries.lock() {
            Ok(mut set) => {
                set.remove(execution_id);
            }
            // 同上：中毒之后仍要继续把事做完。
            Err(poisoned) => {
                poisoned.into_inner().remove(execution_id);
            }
        }
    }

    /// 这条执行是否已在处理中 / 刚处理完（只读，给诊断用）。
    pub fn contains(&self, execution_id: &str) -> bool {
        match self.entries.lock() {
            Ok(set) => set.contains(execution_id),
            // 同上。
            Err(poisoned) => poisoned.into_inner().contains(execution_id),
        }
    }
}

/// 一帧的范围解析结果：要么是认下的活，要么是拒掉/重复。
///
/// **闭集检查在这里做，不在 `serde` 里做**：`TaskDispatch` 没有 `stage` 字段
/// （它有的话就是设备能改的阶段，语义错位），所以「未知 `stage`」指的是 Hub
/// 把一个设备读不懂的阶段塞进载荷（比如将来加了 `waiting_for_user`）。这种帧
/// 必须被**记下来**，而不是因为 `serde` 忽略了多余键就静默当成正常派发。
pub fn parse_frame(frame_type: &str, payload: &serde_json::Value) -> Incoming {
    // 「未知 stage」：载荷里若带了 `stage`，它必须是本设备认识的闭集成员。
    if let Some(stage) = payload.get("stage").and_then(serde_json::Value::as_str) {
        let known: Vec<&str> = DispatchStage::ALL.iter().map(|s| s.as_str()).collect();
        if !known.contains(&stage) {
            return Incoming::Rejected(Rejected {
                reason: format!(
                    "unknown dispatch stage {stage:?} in a {frame_type} frame (known: {})",
                    known.join(", ")
                ),
                execution_id: payload
                    .get("execution_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    let dispatch: TaskDispatch = match serde_json::from_value(payload.clone()) {
        Ok(d) => d,
        Err(e) => {
            return Incoming::Rejected(Rejected {
                reason: format!("incomplete {frame_type} payload: {e}"),
                execution_id: payload
                    .get("execution_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        }
    };
    // 空 `execution_id` 认不得是哪条执行：回什么都对不上。记下来，别假装认了。
    if dispatch.execution_id.trim().is_empty() {
        return Incoming::Rejected(Rejected {
            reason: format!("a {frame_type} frame with an empty execution_id"),
            execution_id: None,
        });
    }
    Incoming::Dispatch(Box::new(dispatch))
}

/// 一帧执行出去之后的结果，以及要回报的终态。
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// `accepted` 之后的全部 ack，**按时间顺序**。
    pub acks: Vec<TaskDispatchAck>,
}

impl DispatchOutcome {
    /// 终态 ack（序列里最后一条）。流程的每一条路径都以一条终态收尾。
    pub fn terminal(&self) -> Option<&TaskDispatchAck> {
        self.acks.last()
    }
}

/// 帧的实时发行口：`accepted` / `running` 这类**在执行之前或执行之中**就要
/// 让 Hub 看到的阶段，靠它一帧一帧发出去。
///
/// 为什么不把 `accepted` 也算进「一口气返回的 `acks`」：Hub 落 `queued → running`
/// 依赖的是**收到** `running`，而不是设备最终算出来自己跑过。把 `accepted` 攒到
/// 执行结束才发，Hub 那侧在整个执行期间都看不到任何进展。
///
/// 本类型只被 worker 实现一次；测试里用 [`RecordingSink`]。
pub trait AckSink {
    /// 发一帧。**发不出去不算执行失败**：Hub 判离站靠收不到心跳，
    /// 而真正重要的是设备把活干完并留下日志（同 `channel` 的处置）。
    fn emit(&mut self, ack: TaskDispatchAck);
}

/// 把 ack 攒进 `Vec` 的 sink（测试与「一次性拿结果」的调用方用）。
#[derive(Debug, Default)]
pub struct RecordingSink {
    pub acks: Vec<TaskDispatchAck>,
}

impl AckSink for RecordingSink {
    fn emit(&mut self, ack: TaskDispatchAck) {
        self.acks.push(ack);
    }
}

/// 把 ack 经通道实时送出的 sink：`spawn_blocking` 线程 → 会话循环。
///
/// **为什么要有它**：`accepted` / `running` 必须在执行**之前或之中**就到 Hub，
/// 而执行跑在另一个线程上。通道是那条唯一的出口，接收端在
/// [`crate::worker::Worker`] 的会话循环里（`drain_dispatch`）。
///
/// 发送端失败（接收端已 drop，比如连接断了）就**丢掉这一帧**：那不是执行失败，
/// 而 Hub 判离站靠收不到心跳（同 `channel` 的处置）。
pub struct ChannelSink(pub std::sync::mpsc::Sender<TaskDispatchAck>);

impl AckSink for ChannelSink {
    fn emit(&mut self, ack: TaskDispatchAck) {
        let _ = self.0.send(ack);
    }
}

/// 正在跑的那一次派发：`execution_id` + 回报通道。
///
/// worker 的会话循环持有它（至多一个——同一绑定默认串行），每次 tick 把
/// 通道里攒下的 ack 取走发出去。**它不做任何阻塞的事**：真正的执行在
/// `spawn_blocking` 线程里，只往通道里塞。
#[derive(Debug)]
pub struct Running {
    pub execution_id: String,
    /// `Mutex` 而不是裸的 `Receiver`：`Receiver` 不是 `Sync`，而 `Running` 活在
    /// `session` 的 `select!` 里——那里的整个 future 必须是 `Send`（`connect_loop`
    /// 会被 `tokio::spawn`）。包一层 `Mutex` 就满足 `Send + Sync`。
    acks: std::sync::Mutex<std::sync::mpsc::Receiver<TaskDispatchAck>>,
}

impl Running {
    /// 新的「正在跑」记录。
    pub fn new(execution_id: String, acks: std::sync::mpsc::Receiver<TaskDispatchAck>) -> Self {
        Self {
            execution_id,
            acks: std::sync::Mutex::new(acks),
        }
    }

    /// 取走攒下的全部 ack（不阻塞）。
    pub fn take_pending(&self) -> Vec<TaskDispatchAck> {
        let mut out = Vec::new();
        let Ok(guard) = self.acks.lock() else {
            // 锁中毒：临界区里只有 `try_recv`，没有任何能 panic 的代码。
            // 空手返回比 panic 好——终态会在下一次 tick 再取。
            return out;
        };
        while let Ok(ack) = guard.try_recv() {
            out.push(ack);
        }
        out
    }
}

/// 构造一条 ack。
fn ack(
    execution_id: &str,
    stage: DispatchStage,
    reason: Option<String>,
    evidence: Option<String>,
    failure_kind: Option<&str>,
) -> TaskDispatchAck {
    TaskDispatchAck {
        execution_id: execution_id.to_string(),
        stage,
        reason,
        evidence,
        failure_kind: failure_kind.map(str::to_string),
    }
}

/// 屏蔽输出里可能出现的超长单行（agent 的 stderr 常是几 KB 一行的 JSON）。
///
/// ack 的 `evidence` 是给 Hub 落库的诊断字段，不是完整日志：完整日志在设备的
/// `afcli.log` 里。这里只留头部并声明截断，**不假装那就是全部**。
pub fn clip(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max).collect();
    format!("{head}… [truncated, {} bytes total]", trimmed.len())
}

/// 本机的权限判定。**`prompt` 不参与**——这是那条铁线的落点。
///
/// # 签名里为什么没有 `prompt` / `dispatch`
///
/// `PRODUCT.md` §5.4 的界线：「**安全许可**（能不能执行）只在本机；**业务决策**
/// （该怎么做）可以进 Hub」。所以本函数**只看本机策略**（这里由 `local_policy`
/// 闭包代表），一个来自 Hub 的字段都进不来——想放宽也放宽不了，这不是靠注释，
/// 是靠签名。设备侧收到 dispatch 之后调用的仍是**同一个**本机判定。
///
/// 测试里用「本机会拒的命令」验证它在 dispatch 前后给出同一个答案。
pub fn local_permission_verdict<F>(local_policy: F) -> bool
where
    F: FnOnce() -> bool,
{
    local_policy()
}

/// 执行一条派发：建树 → 校验基线 → 执行（含磁盘监控）→ 回报。
///
/// `sink` 收到 `accepted` / `baseline_mismatch` / `running` / 终态各帧；
/// 返回的 [`DispatchOutcome`] 只装**终态之后的** ack（给调用方落日志用）。
///
/// **这是阻塞函数**：`git worktree add` 与子进程等待都是阻塞的，调用方
/// （worker）负责放进 `spawn_blocking`，否则会把心跳臂一起堵住——`v0.2.0` 的
/// exec 事故就是这么来的。
pub fn run<S: AckSink>(
    dispatch: &TaskDispatch,
    config: &DispatchConfig,
    repo: &Path,
    sink: &mut S,
) -> DispatchOutcome {
    let execution_id = dispatch.execution_id.as_str();
    sink.emit(ack(execution_id, DispatchStage::Accepted, None, None, None));

    let worktree_path = Path::new(&dispatch.worktree_path);
    let prepared = match worktree::prepare(repo, worktree_path, &dispatch.branch) {
        Ok(p) => p,
        Err(e) => {
            // 连树都建不出来：这不是「基线不符」而是环境坏了（`env_broken`）。
            // 归到 `baseline_mismatch` 会让 Hub 去「重新 fetch 一次」，而真正的
            // 问题是这台机器上这个目录建不出来。
            let out = ack(
                execution_id,
                DispatchStage::Failed,
                Some(format!("cannot prepare the worktree: {e}")),
                Some(clip(&dispatch.worktree_path, 200)),
                Some("env_broken"),
            );
            sink.emit(out.clone());
            return DispatchOutcome { acks: vec![out] };
        }
    };

    // ---- 基线校验：不符**绝不硬跑** ----
    match worktree::check_baseline(&prepared.path, &dispatch.base_commit) {
        BaselineVerdict::Mismatch { reason } => {
            let out = ack(
                execution_id,
                DispatchStage::BaselineMismatch,
                Some(reason.clone()),
                Some(format!(
                    "worktree {}{}",
                    prepared.path.display(),
                    if prepared.created {
                        " (newly created)"
                    } else {
                        " (reused)"
                    }
                )),
                Some("baseline_mismatch"),
            );
            sink.emit(out.clone());
            // 返回的 `acks` 里已经有了终态；这里**没有** `running`、**没有**执行。
            return DispatchOutcome { acks: vec![out] };
        }
        BaselineVerdict::Matched => {}
    }

    sink.emit(ack(
        execution_id,
        DispatchStage::Running,
        Some(format!(
            "executing {} on branch {}",
            dispatch.agent_id, dispatch.branch
        )),
        None,
        None,
    ));

    let exec = execute(dispatch, config, &prepared.path);

    let terminal = match exec {
        ExecOutcome::Finished {
            success,
            exit_code,
            output,
        } => {
            let evidence = Some(clip(
                &format!(
                    "exit={exit_code} worktree={} branch={} output={}",
                    prepared.path.display(),
                    dispatch.branch,
                    clip(&output, 400)
                ),
                900,
            ));
            if success {
                ack(execution_id, DispatchStage::Succeeded, None, evidence, None)
            } else {
                // **`task_failed` 与 `agent_error` 的区别**：agent 正常退出但事情
                // 没做成（测试红了）是 `task_failed`；agent 自己崩了 / 起不来是
                // `agent_error`。两者的重试处置完全不同（`scheduling.md` §5.1）。
                let kind = if exit_code < 0 {
                    "agent_error"
                } else {
                    "task_failed"
                };
                ack(
                    execution_id,
                    DispatchStage::Failed,
                    Some(format!(
                        "the agent exited with {exit_code} on branch {}",
                        dispatch.branch
                    )),
                    evidence,
                    Some(kind),
                )
            }
        }
        ExecOutcome::DiskExhausted {
            used,
            kept,
            kept_dir,
        } => ack(
            execution_id,
            DispatchStage::Failed,
            Some(format!(
                "aborted: the worktree reached {used} bytes, over the budget of {} bytes; \
                 kept {kept} artifact(s) for diagnosis and dropped the working tree",
                config.disk_budget_bytes
            )),
            Some(format!(
                "disk_exhausted worktree={} kept={} kept_dir={}",
                prepared.path.display(),
                kept,
                kept_dir
            )),
            Some("disk_exhausted"),
        ),
        ExecOutcome::NotStarted { detail } => ack(
            execution_id,
            DispatchStage::Failed,
            Some(format!("cannot start the agent: {detail}")),
            Some(clip(&detail, 400)),
            Some("agent_error"),
        ),
    };
    sink.emit(terminal.clone());
    DispatchOutcome {
        acks: vec![terminal],
    }
}

/// 一次执行的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    /// 跑完了。`exit_code < 0` 表示被信号杀掉（没有退出码）。
    Finished {
        success: bool,
        exit_code: i32,
        output: String,
    },
    /// 超预算被**提前中止**：agent 已被杀，工作树已删，只留了 `kept` 个产物。
    DiskExhausted {
        used: u64,
        kept: usize,
        kept_dir: String,
    },
    /// 子进程根本没起来（命令不存在 / 无执行权限）。
    NotStarted { detail: String },
}

/// 执行 agent 并**在执行期间监控磁盘**。
///
/// # 命令从哪来
///
/// 本期不接真 ACP（那是另一条通路的活，`agent/` 里已有对话实现），
/// 而是用一条**本机命令**跑：`$AFCLI_DISPATCH_CMD`（或 `sh -c` 的默认形态）。
/// 这样做的两个理由：
///
/// - 设备侧的判据是「建树 → 校验基线 → 执行 → 监控 → 回报」这条链路，
///   与 agent 内部怎么对话无关，用一条命令就能把每条分支都走到；
/// - 起子进程这件事本身已经把「能中止」这条要求变成可实现的：
///   `Child::kill` 能真的把一个写满磁盘的 agent 停住。
///
/// **`prompt` 进的是环境变量而不是命令行**：它是一段可能很长的自然语言，
/// 塞进 `sh -c` 的字符串里会被 shell 解释（引号、`$`、反引号）——那是命令
/// 注入，不是「把要做的工作告诉 agent」。
pub fn execute(dispatch: &TaskDispatch, config: &DispatchConfig, worktree: &Path) -> ExecOutcome {
    let mut cmd = dispatch_command(worktree);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(worktree)
        // 工作树绝对路径与分支名给到执行体：真的 agent 要用它们。
        .env("AFCLI_WORKTREE", worktree)
        .env("AFCLI_BRANCH", &dispatch.branch)
        .env("AFCLI_AGENT_ID", &dispatch.agent_id)
        // **`prompt` 只是任务描述**，见模块注释的第 3 条铁线。把它放进环境是
        // 「告诉 agent 要做什么」，不是「批准它做什么」——本进程里没有任何一处
        // 会因为它的存在而改变权限判定。
        .env("AFCLI_PROMPT", &dispatch.prompt)
        .env("AFCLI_EXECUTION_ID", &dispatch.execution_id);

    // 建树之后的第一张快照：这些文件属于仓库内容，不是本次产出。
    let baseline = top_level_files(worktree);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutcome::NotStarted {
                detail: format!("cannot run the dispatch command: {e}"),
            }
        }
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return ExecOutcome::Finished {
                    success: status.success(),
                    exit_code: status.code().unwrap_or(-1),
                    output: collect_output(&mut child),
                }
            }
            // coverage: excluded — `try_wait` 只在内核层面出问题（EPERM / 未被
            // 重试的 EINTR）时失败，而 std 没有让某个 Child 句柄失效的口子；
            // 这一支的价值是「别把错误吞成成功」，逻辑本身无可测的分叉。
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return ExecOutcome::NotStarted {
                    detail: format!("cannot wait for the agent: {e}"),
                };
            }
            Ok(None) => {}
        }
        std::thread::sleep(config.disk_sample_interval);
        // ---- 执行中监控：这不是可选优化，见模块注释第 2 条 ----
        let used = worktree::disk_usage(worktree);
        if used > config.disk_budget_bytes {
            // **先杀再收尸**：只 kill 不 wait 会留僵尸，而僵尸的 stdout 管道
            // 仍然开着，`collect_output` 会一直等下去。
            let _ = child.kill();
            let _ = child.wait();
            let (kept, kept_dir) = quarantine_artifacts(worktree, config.keep_artifacts, &baseline);
            // 删掉整棵工作树：那正是要避免的磁盘灾难（`scheduling.md` §3.2）。
            remove_worktree(worktree);
            return ExecOutcome::DiskExhausted {
                used,
                kept,
                kept_dir,
            };
        }
    }
}

/// 执行体的命令。
///
/// `AFCLI_DISPATCH_CMD` 优先（现场排查时能换成任何脚本），否则用 `sh -c` 跑
/// 一句「告诉 agent 去干活」的占位。**不是** `eval`，也**不是**把 `prompt`
/// 拼进脚本文本。
///
/// 注入值的两种形态：
///
/// - `sh:<脚本>` → `sh -c <脚本>`（测试与现场排查写一行 shell 最省事）；
/// - 其余 → **直接执行那个可执行文件**，没有 `sh` 夹在中间。
///
/// 第二种形态不能省：「agent 起不来」这条失败形态（`agent_error`）要求
/// `spawn` 本身失败。夹一层 `sh` 的话，解释器启动成功、只是内部 exec 失败，
/// 进程会以非零码正常退出——那会**变成 `task_failed`**，把「agent 崩了」误报成
/// 「事情没做成」，而两者的重试处置完全不同（换 agent vs 换设备）。
fn dispatch_command(worktree: &Path) -> Command {
    let custom = std::env::var("AFCLI_DISPATCH_CMD").unwrap_or_default();
    let custom = custom.trim();
    if let Some(script) = custom.strip_prefix("sh:") {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        return c;
    }
    if !custom.is_empty() {
        return Command::new(custom);
    }
    let mut c = Command::new("sh");
    // 默认执行体：把「要做的工作」写进工作树里的一个文件，并以 0 退出。
    // 它存在的意义是「不接真 agent 时链路仍然完整可走」，不是假装
    // 跑了一次推理。
    c.arg("-c").arg(
        "printf 'dispatch %s\\n' \"$AFCLI_EXECUTION_ID\" > .afcli-dispatch.log \
         && printf '%s\\n' \"$AFCLI_PROMPT\" >> .afcli-dispatch.log",
    );
    c.current_dir(worktree);
    c
}

/// 把子进程的输出收集起来（不会等待——调用时进程已退出）。
fn collect_output(child: &mut std::process::Child) -> String {
    use std::io::Read;
    let mut text = String::new();
    if let Some(out) = child.stdout.as_mut() {
        let mut buf = Vec::new();
        if out.read_to_end(&mut buf).is_ok() {
            text.push_str(&String::from_utf8_lossy(&buf));
        }
    }
    if let Some(err) = child.stderr.as_mut() {
        let mut buf = Vec::new();
        if err.read_to_end(&mut buf).is_ok() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&buf));
        }
    }
    text
}

/// 删除整棵工作树（中止时用）。
///
/// **尽力而为**：删不掉也要往下走——回报 `disk_exhausted` 比「因为删不掉而卡住」
/// 重要得多，而删不掉这件事本身会留在日志与 ack 的证据里。
fn remove_worktree(worktree: &Path) {
    let _ = std::fs::remove_dir_all(worktree);
}

/// worktree 顶层现有文件名（`.git` 与目录除外）。
///
/// 建树之后立刻拍一张快照当基线，执行中被中止时再拍一张，两相一减就是
/// **本次执行新产出的**顶层文件。
fn top_level_files(worktree: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(worktree) {
        for entry in entries.flatten() {
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_file && name != ".git" {
                out.insert(name);
            }
        }
    }
    out
}

/// 保留下前 N 个产物做诊断，然后交给调用方删整棵树。
///
/// 「前 N 个」按**文件名的字节序**取——它是确定的、可复现的，也不依赖
/// 文件系统的遍历顺序（`read_dir` 的顺序是未定义的，拿它做「前 N 个」
/// 会让同一份输入两次跑出不同的保留集合）。
///
/// 保留到 worktree **外面**（父目录下的 `ARTIFACTS_DIR_NAME/<execution_id>`），
/// 因为紧接着整棵树就会被删掉。
///
/// `baseline` 是 worktree 建好那一刻就存在的顶层文件名清单：它们属于
/// 「checkout 出来的仓库内容」，不是**本次执行**的产物，因此不参与「前 N 个」的
/// 计数（否则一个刚建好的工作树里 `a.txt` 会挤掉 agent 真正写出来的文件）。
///
/// 返回（保留个数, 保留目录）。
fn quarantine_artifacts(
    worktree: &Path,
    keep: usize,
    baseline: &HashSet<String>,
) -> (usize, String) {
    let parent = worktree.parent().unwrap_or(worktree);
    let leaf = worktree.file_name().map_or_else(
        || "worktree".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let dest = parent.join(ARTIFACTS_DIR_NAME).join(&leaf);

    // 顶层文件按名字排序取前 N 个。跳过的两类：
    //
    // - **目录**：把一棵子目录搬走与「保留前 N 个产物」不是同一件事，
    //   而且它可能很大——那正是我们要避免的；
    // - **`.git` 与 git 的元数据文件**（`.git` / `.gitignore` / `a.txt` 里的
    //   基础内容不属于「本次执行产出」，把它们算成产物会挤掉真正要留的东西）。
    //   判定方式是「在 worktree 建好之时就存在的文件」——由调用方传进来的
    //   基线清单决定，见 [`quarantine_artifacts`] 的 `baseline` 参数。
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(worktree) {
        for entry in entries.flatten() {
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            if !is_file {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || baseline.contains(&name) {
                continue;
            }
            names.push(name);
        }
    }
    names.sort();
    names.truncate(keep);

    let mut kept = 0usize;
    if !names.is_empty() && std::fs::create_dir_all(&dest).is_ok() {
        for name in &names {
            let from = worktree.join(name);
            if std::fs::copy(&from, dest.join(name)).is_ok() {
                kept += 1;
            }
        }
    }
    (kept, dest.display().to_string())
}

/// 失败形态是否在协议的闭集里。写在这里是为了让「设备侧只报闭集内的值」
/// 有一条**可执行的**检查（新增形态时这里会立刻暴露）。
pub fn is_known_failure_kind(kind: &str) -> bool {
    FAILURE_KINDS.contains(&kind)
}

/// 一个小助手：把一串 ack 压成「有没有终态」的判定（给调用方落日志用）。
pub fn terminal_of(acks: &[TaskDispatchAck]) -> Option<&TaskDispatchAck> {
    acks.last()
}

/// 一个派发队列：**同一绑定默认串行**（`scheduling.md` §3.3）。
///
/// 本期只做「不并发」这一件事：worker 一次只跑一条派发，其余的排队等它。
/// 这不是保守，是磁盘决定的——两个任务并行 = 两份依赖 + 两套构建产物，
/// 占用不是翻倍而是乘上并发数。
#[derive(Debug, Default)]
pub struct Queue {
    pending: Mutex<VecDeque<TaskDispatch>>,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 入队。返回队列深度（1 = 前面没人）。
    pub fn push(&self, dispatch: TaskDispatch) -> usize {
        match self.pending.lock() {
            Ok(mut q) => {
                q.push_back(dispatch);
                q.len()
            }
            // 锁中毒：取回内层队列照常入队（丢活比继续入队坏得多）。
            Err(poisoned) => {
                let mut q = poisoned.into_inner();
                q.push_back(dispatch);
                q.len()
            }
        }
    }

    /// 取下一件（先进先出）。
    pub fn pop(&self) -> Option<TaskDispatch> {
        match self.pending.lock() {
            Ok(mut q) => q.pop_front(),
            // 同上。
            Err(poisoned) => poisoned.into_inner().pop_front(),
        }
    }

    /// 当前排队件数。
    pub fn len(&self) -> usize {
        match self.pending.lock() {
            Ok(q) => q.len(),
            // 同上。
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Stdio;

    /// 起一个真 git 仓库（一次提交），返回（临时目录, 仓库路径）。
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        git_in(&root, &["init", "-q", "-b", "main"]);
        git_in(&root, &["config", "user.email", "t@example.com"]);
        git_in(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), b"one\n").expect("write");
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "first"]);
        (tmp, root)
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .expect("run git");
        assert!(st.success(), "git {args:?} failed in {}", dir.display());
    }

    fn head_of(dir: &Path) -> String {
        worktree::head_commit(dir).expect("head")
    }

    /// 造一条指向 `repo` 的派发帧。
    fn dispatch_to(execution_id: &str, repo: &Path, worktree_path: &Path) -> TaskDispatch {
        TaskDispatch {
            execution_id: execution_id.to_string(),
            task_id: "t_1".to_string(),
            project_id: "p_1".to_string(),
            base_commit: head_of(repo),
            worktree_path: worktree_path.display().to_string(),
            branch: format!("task/{execution_id}"),
            agent_id: "fake-agent".to_string(),
            prompt: "do the thing".to_string(),
        }
    }

    /// 测试用的配置：毫秒级采样、几百字节阈值。
    fn fast_config() -> DispatchConfig {
        DispatchConfig {
            disk_sample_interval: Duration::from_millis(5),
            disk_budget_bytes: 400,
            keep_artifacts: 3,
        }
    }

    /// **判据 1：收到 `task_dispatch` 后确实建了 worktree。**
    ///
    /// 断言两件事：目录真的存在且是一条 worktree（有 `.git`），并且 ack 里的
    /// `worktree_path`（证据里）指的就是帧里给的那个路径——不是另外挑了一个。
    #[test]
    fn a_claim_creates_the_worktree_at_the_path_the_frame_named() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-create");
        let frame = dispatch_to("e1", &root, &wt);
        assert!(!wt.exists(), "前提：目录还不存在");

        let mut sink = RecordingSink::default();
        let out = run(&frame, &DispatchConfig::default(), &root, &mut sink);

        assert!(wt.is_dir(), "派发之后 worktree 目录必须存在");
        assert!(wt.join(".git").exists(), "必须是一条真正的 worktree");
        // 路径一致：证据里写着帧给的那条路径。
        let evidence = out
            .terminal()
            .and_then(|a| a.evidence.clone())
            .unwrap_or_default();
        assert!(
            evidence.contains(&wt.display().to_string()),
            "worktree_path 必须与帧里的相等：{evidence}"
        );
        assert_eq!(
            out.terminal().map(|a| a.stage),
            Some(DispatchStage::Succeeded)
        );
    }

    /// **判据 2：`base_commit` 不符 → `BaselineMismatch`，且确实没有执行 agent。**
    ///
    /// 「没执行」的证明方式是**行为性**的：执行体被写成「跑起来就写一个标记文件」，
    /// 断言那个文件**不存在**。只断言返回了 `BaselineMismatch` 是不够的——
    /// 先跑后报也会得到同样的 stage，而那正是这条硬约束要禁的事。
    #[test]
    fn a_baseline_mismatch_is_reported_and_the_agent_never_runs() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-mismatch");
        let marker = tmp.path().join("RAN");
        let mut frame = dispatch_to("e2", &root, &wt);
        // 要求的起点是一个**不存在**的 commit。
        frame.base_commit = "0".repeat(40);

        // 执行体：跑起来就在 `marker` 写一行。用了它说明 agent 被执行了。
        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd(&format!("sh:echo ran > {}", marker.display()), || {
            run(&frame, &fast_config(), &root, &mut sink)
        });

        let terminal = out.terminal().expect("必须有一条终态");
        assert_eq!(terminal.stage, DispatchStage::BaselineMismatch);
        assert_eq!(terminal.failure_kind.as_deref(), Some("baseline_mismatch"));
        let reason = terminal.reason.clone().unwrap_or_default();
        // 原因要说清「本地 HEAD 是 X，要求 Y」。
        assert!(reason.contains("the local HEAD is"), "{reason}");
        assert!(reason.contains(&frame.base_commit), "{reason}");

        // **关键断言：没有执行。**
        assert!(
            !marker.exists(),
            "基线不符时 agent 绝不能被启动（标记文件 {} 不该存在）",
            marker.display()
        );
        // 而且整条 ack 序列里没有 `running`：连「开始执行」都没报过。
        assert!(
            sink.acks.iter().all(|a| a.stage != DispatchStage::Running),
            "基线不符不得报 running：{:?}",
            sink.acks
        );
    }

    /// **判据 3a：执行成功 → `Succeeded`**，且没有任何失败形态。
    #[test]
    fn a_successful_run_reports_succeeded() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-ok");
        let frame = dispatch_to("e3", &root, &wt);

        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd("sh:echo hello-from-agent", || {
            run(&frame, &fast_config(), &root, &mut sink)
        });

        let terminal = out.terminal().expect("终态");
        assert_eq!(terminal.stage, DispatchStage::Succeeded);
        assert_eq!(terminal.failure_kind, None);
        let evidence = terminal.evidence.clone().unwrap_or_default();
        assert!(evidence.contains("hello-from-agent"), "{evidence}");
        // `accepted` 与 `running` 都必须真的发出去过（Hub 靠它们推进状态）。
        let stages: Vec<DispatchStage> = sink.acks.iter().map(|a| a.stage).collect();
        assert_eq!(
            stages,
            vec![
                DispatchStage::Accepted,
                DispatchStage::Running,
                DispatchStage::Succeeded
            ]
        );
    }

    /// **判据 3b：执行失败 → `Failed`，且 `failure_kind` 与 `reason` 都非空。**
    #[test]
    fn a_failing_run_reports_failed_with_a_kind_and_a_reason() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-fail");
        let frame = dispatch_to("e4", &root, &wt);

        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd("sh:echo boom >&2; exit 3", || {
            run(&frame, &fast_config(), &root, &mut sink)
        });

        let terminal = out.terminal().expect("终态");
        assert_eq!(terminal.stage, DispatchStage::Failed);
        let kind = terminal.failure_kind.clone().expect("失败必须有形态");
        assert!(!kind.is_empty());
        assert!(
            is_known_failure_kind(&kind),
            "形态必须在协议的闭集里：{kind}"
        );
        assert_eq!(kind, "task_failed");
        let reason = terminal.reason.clone().unwrap_or_default();
        assert!(!reason.is_empty(), "失败必须有原因串");
        assert!(reason.contains('3'), "原因要说清退出码：{reason}");
    }

    /// 执行体起不来（命令不存在）→ `Failed{ agent_error }`，不是 `task_failed`。
    ///
    /// 两者的重试处置完全不同：`agent_error` 换 agent，`task_failed` 换设备。
    ///
    /// 构造方式：往 worktree 里塞一个可执行权限在、但**解释器不存在**的脚本
    /// （`#!/nonexistent/interpreter`）。`exec` 会在 `spawn` 阶段失败（ENOENT），
    /// 于是走 `NotStarted` 那一支——而不是像 `PATH=""` 那样连 `git` 都找不到、
    /// 先撞上 `env_broken`。
    #[test]
    fn an_agent_that_cannot_start_reports_agent_error() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-nostart");
        let frame = dispatch_to("e5", &root, &wt);
        let config = fast_config();

        let broken = tmp.path().join("broken-agent");
        std::fs::write(&broken, b"#!/nonexistent/interpreter\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd(&broken.display().to_string(), || {
            run(&frame, &config, &root, &mut sink)
        });

        let terminal = out.terminal().expect("终态");
        assert_eq!(terminal.stage, DispatchStage::Failed);
        assert_eq!(terminal.failure_kind.as_deref(), Some("agent_error"));
        assert!(terminal.reason.as_deref().is_some_and(|r| !r.is_empty()));
    }

    /// **判据 4：执行中磁盘监控真的会中止。**
    ///
    /// 执行体在 5 ms 采样周期内写出远超 400 字节预算的东西。断言三件事：
    ///
    /// 1. 执行被**中止**（执行体本该继续跑到写满 2000 字节并写第二阶段的标记；
    ///    那个标记**不存在**证明它在中途被杀，不是跑完才判）；
    /// 2. `failure_kind == "disk_exhausted"`；
    /// 3. **只保留前 N 个产物**（不是整个工作树）：保留目录里恰好 N 个文件，
    ///    而整棵工作树已经被删掉。
    #[test]
    fn a_run_that_blows_the_disk_budget_is_aborted_and_only_n_artifacts_survive() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-disk");
        let late = tmp.path().join("FINISHED");
        let frame = dispatch_to("e6", &root, &wt);

        // 写 5 个产物（每个 ~200 字节 → 共 1000 字节，超 400 的预算），
        // 然后睡一会儿再写 `late`。`late` 存在 = 执行体跑到了终点 = 没被中止。
        let script = format!(
            "sh:for i in 1 2 3 4 5; do printf 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
             AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
             AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
             AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' > artifact-$i.txt; done; \
             sleep 2; echo done > {}",
            late.display()
        );
        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd(&script, || run(&frame, &fast_config(), &root, &mut sink));

        let terminal = out.terminal().expect("终态");
        assert_eq!(terminal.stage, DispatchStage::Failed);
        assert_eq!(
            terminal.failure_kind.as_deref(),
            Some("disk_exhausted"),
            "超预算必须报 disk_exhausted：{terminal:?}"
        );
        let reason = terminal.reason.clone().unwrap_or_default();
        assert!(reason.contains("over the budget"), "{reason}");

        // 1. 被中止：执行体停在半路，第二条标记没写出来。
        assert!(
            !late.exists(),
            "执行必须在中途被中止，而不是等它跑完（{} 不该存在）",
            late.display()
        );

        // 3. 只保留前 N 个：保留目录里恰好 `keep_artifacts` 个文件。
        let kept_dir = tmp
            .path()
            .join(crate::dispatch::ARTIFACTS_DIR_NAME)
            .join("wt-disk");
        let kept: Vec<String> = std::fs::read_dir(&kept_dir)
            .map(|it| {
                it.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(kept.len(), 3, "只保留前 N 个产物（N=3），实际 {kept:?}");
        assert!(kept.iter().all(|n| n.starts_with("artifact-")), "{kept:?}");
        // 而且**不是整个工作树**：那棵树已经被删掉了。
        assert!(
            !wt.exists(),
            "中止时必须删掉整个工作树（它是要避免的磁盘灾难）"
        );
    }

    /// **判据 5：未知 `stage` / 缺字段的帧 → 不 panic、不静默吞掉。**
    #[test]
    fn an_unknown_stage_is_rejected_with_a_readable_reason() {
        let payload = serde_json::json!({
            "execution_id": "e7",
            "stage": "waiting_for_user",
        });
        match parse_frame("task_dispatch", &payload) {
            Incoming::Rejected(r) => {
                assert!(r.reason.contains("unknown dispatch stage"), "{}", r.reason);
                assert_eq!(r.execution_id.as_deref(), Some("e7"));
            }
            other => panic!("未知 stage 必须被拒，实际 {other:?}"),
        }
    }

    /// 缺字段的帧 → 拒绝，原因里指名缺了哪个字段，并尽量带回 `execution_id`。
    #[test]
    fn a_frame_missing_fields_is_rejected_with_the_missing_key_named() {
        let payload = serde_json::json!({
            "execution_id": "e8",
            "task_id": "t_1",
            // 没有 base_commit / worktree_path / branch / agent_id / prompt
        });
        match parse_frame("task_dispatch", &payload) {
            Incoming::Rejected(r) => {
                assert!(
                    r.reason.contains("incomplete task_dispatch"),
                    "{}",
                    r.reason
                );
                assert_eq!(r.execution_id.as_deref(), Some("e8"));
            }
            other => panic!("缺字段必须被拒，实际 {other:?}"),
        }
    }

    /// 连 `execution_id` 都没有、或者它是空串 → 也要拒（回不了 ack，但必须留痕）。
    #[test]
    fn a_frame_without_a_usable_execution_id_is_rejected() {
        let empty = serde_json::json!({
            "execution_id": "   ",
            "task_id": "t", "project_id": "p", "base_commit": "c",
            "worktree_path": "/tmp/x", "branch": "b", "agent_id": "a", "prompt": "x",
        });
        match parse_frame("task_dispatch", &empty) {
            Incoming::Rejected(r) => {
                assert!(r.reason.contains("empty execution_id"), "{}", r.reason);
                assert_eq!(r.execution_id, None);
            }
            other => panic!("空 execution_id 必须被拒，实际 {other:?}"),
        }
    }

    /// 正常的帧解得出全部字段——反向证明上面那些拒绝不是「什么都不收」。
    #[test]
    fn a_well_formed_frame_parses_into_a_claim() {
        let payload = serde_json::json!({
            "execution_id": "e9",
            "task_id": "t", "project_id": "p", "base_commit": "abc123",
            "worktree_path": "/tmp/wt", "branch": "task/e9",
            "agent_id": "claude", "prompt": "do it",
        });
        match parse_frame("task_dispatch", &payload) {
            Incoming::Dispatch(d) => {
                assert_eq!(d.execution_id, "e9");
                assert_eq!(d.base_commit, "abc123");
                assert_eq!(d.worktree_path, "/tmp/wt");
                assert_eq!(d.agent_id, "claude");
                assert_eq!(d.prompt, "do it");
            }
            other => panic!("正常帧必须解出来，实际 {other:?}"),
        }
    }

    /// **判据 6：同 `execution_id` 重复投递不重复执行（幂等）。**
    ///
    /// 判据是**执行次数**：执行体每被跑一次就往计数器文件里追加一行。
    /// 第二帧投进去之后，那个文件仍然只有一行。
    #[test]
    fn the_same_execution_id_is_never_executed_twice() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-idem");
        let counter = tmp.path().join("count");
        let frame = dispatch_to("e10", &root, &wt);

        let ledger = Ledger::new();
        let script = format!("sh:echo x >> {}", counter.display());

        // 第一次：认领成功 → 执行。
        assert!(ledger.claim(&frame.execution_id), "第一次必须认得下来");
        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd(&script, || run(&frame, &fast_config(), &root, &mut sink));
        assert_eq!(
            out.terminal().map(|a| a.stage),
            Some(DispatchStage::Succeeded)
        );

        // 第二次：同一 execution_id → 不认领。
        assert!(
            !ledger.claim(&frame.execution_id),
            "同一个 execution_id 不得被认领两次"
        );

        let lines = std::fs::read_to_string(&counter).expect("counter");
        assert_eq!(lines.lines().count(), 1, "执行体只该跑一次：{lines}");
    }

    /// 幂等的另一半：执行走到终态后 `forget` 能把它清掉（集合不会无限长大）。
    #[test]
    fn the_ledger_forgets_an_execution_after_it_settles() {
        let ledger = Ledger::new();
        assert!(ledger.claim("e11"));
        assert!(ledger.contains("e11"));
        ledger.forget("e11");
        assert!(!ledger.contains("e11"));
        assert!(ledger.claim("e11"), "忘掉之后可以重新认领");
    }

    /// **判据 7a（反向断言）：`TaskDispatch` 序列化后没有任何 approval /
    /// permission 类字段。**
    ///
    /// 白名单逐字相等——不是「我搜了几个词没搜到」。设备侧能松的权限只取决于
    /// 帧里有什么，而帧里**什么都没有**。
    #[test]
    fn a_dispatch_frame_carries_no_approval_or_permission_field() {
        let frame = TaskDispatch {
            execution_id: "e12".to_string(),
            task_id: "t".to_string(),
            project_id: "p".to_string(),
            base_commit: "abc".to_string(),
            worktree_path: "/tmp/wt".to_string(),
            branch: "b".to_string(),
            agent_id: "a".to_string(),
            prompt: "do it".to_string(),
        };
        let value = serde_json::to_value(&frame).expect("serialize");
        let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agent_id",
                "base_commit",
                "branch",
                "execution_id",
                "project_id",
                "prompt",
                "task_id",
                "worktree_path",
            ],
            "帧的 key 集合必须逐字等于白名单（多一个就是多了授权面）"
        );
        for forbidden in [
            "approval",
            "approved",
            "permission",
            "pre_approved",
            "allow",
        ] {
            assert!(
                !keys.iter().any(|k| k.contains(forbidden)),
                "帧里不得出现 {forbidden} 类字段：{keys:?}"
            );
        }
    }

    /// **判据 7b（反向断言）：设备侧收到 dispatch 后不改变本机权限检查的结果。**
    ///
    /// 构造一条**本机会被拒绝**的命令，断言收到 dispatch 前后它**仍然被拒绝**。
    /// 这里用的是同一个本机判定函数 [`local_permission_verdict`]——它的签名
    /// 里根本没有 `prompt` / `dispatch`，所以「因为 dispatch 而放行」在类型层面
    /// 就写不出来；本用例把这一点钉成断言。
    #[test]
    fn a_dispatch_does_not_relax_this_machines_permission_check() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-perm");
        let frame = dispatch_to("e13", &root, &wt);

        // 本机策略：一律拒绝 `rm -rf /`。
        let local_policy = || false;

        // 收到 dispatch 之前。
        let before = local_permission_verdict(local_policy);
        assert!(!before, "前提：本机策略拒绝这条命令");

        // 收到并处理 dispatch 之后——同一个判定，同一个答案。
        let mut sink = RecordingSink::default();
        let out = with_dispatch_cmd("sh:true", || run(&frame, &fast_config(), &root, &mut sink));
        assert_eq!(
            out.terminal().map(|a| a.stage),
            Some(DispatchStage::Succeeded)
        );

        let after = local_permission_verdict(local_policy);
        assert_eq!(before, after, "dispatch 不得改变本机权限检查的结果");
        assert!(!after, "本机会拒绝的命令，收到 dispatch 之后仍然被拒绝");
    }

    /// 建树失败（目标目录被一个非空的外来目录占着）→ `Failed{ env_broken }`，
    /// 并且**不删**那个目录里别人的东西。
    #[test]
    fn a_worktree_that_cannot_be_prepared_fails_as_env_broken() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-blocked");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join("someone-elses.txt"), b"keep me\n").expect("write");
        let frame = dispatch_to("e14", &root, &wt);

        let mut sink = RecordingSink::default();
        let out = run(&frame, &DispatchConfig::default(), &root, &mut sink);

        let terminal = out.terminal().expect("终态");
        assert_eq!(terminal.stage, DispatchStage::Failed);
        assert_eq!(terminal.failure_kind.as_deref(), Some("env_broken"));
        assert!(wt.join("someone-elses.txt").exists(), "不许删别人的东西");
    }

    /// 队列先进先出，深度可读（同一绑定默认串行，`scheduling.md` §3.3）。
    #[test]
    fn the_queue_is_first_in_first_out_and_reports_its_depth() {
        let (tmp, root) = repo();
        let q = Queue::new();
        assert!(q.is_empty());
        assert_eq!(q.push(dispatch_to("q1", &root, &tmp.path().join("w1"))), 1);
        assert_eq!(q.push(dispatch_to("q2", &root, &tmp.path().join("w2"))), 2);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop().map(|d| d.execution_id), Some("q1".to_string()));
        assert_eq!(q.pop().map(|d| d.execution_id), Some("q2".to_string()));
        assert!(q.pop().is_none());
    }

    /// `clip` 短文本原样、长文本截断并声明（ack 的证据字段不该是几 KB）。
    #[test]
    fn clip_keeps_short_text_and_declares_truncation() {
        assert_eq!(clip("  hello  ", 20), "hello");
        let long = "x".repeat(100);
        let got = clip(&long, 10);
        assert!(got.starts_with("xxxxxxxxxx"), "{got}");
        assert!(got.contains("truncated"), "{got}");
    }

    /// `terminal_of` 取最后一条（序列里的终态）。
    #[test]
    fn terminal_of_is_the_last_ack() {
        let acks = vec![
            ack("e", DispatchStage::Accepted, None, None, None),
            ack("e", DispatchStage::Succeeded, None, None, None),
        ];
        assert_eq!(
            terminal_of(&acks).map(|a| a.stage),
            Some(DispatchStage::Succeeded)
        );
    }

    /// 在一段「执行体命令」的作用下跑一个闭包，结束后把环境变量恢复。
    ///
    /// 环境变量是全局的，而 `execute` 读 `AFCLI_DISPATCH_CMD`——测试要注入
    /// 假执行体就只能走这条路。**恢复是必须的**：漏掉的话后面的用例会继承
    /// 上一个用例的脚本，报错的样子会离根因很远。
    fn with_dispatch_cmd<T>(script: &str, f: impl FnOnce() -> T) -> T {
        let saved = std::env::var("AFCLI_DISPATCH_CMD").ok();
        // SAFETY: 单元测试里没有别的线程在读这个变量（`execute` 只在同一个
        // 闭包里被调）；下面立刻恢复。
        unsafe { std::env::set_var("AFCLI_DISPATCH_CMD", script) };
        let out = f();
        match saved {
            Some(v) => unsafe { std::env::set_var("AFCLI_DISPATCH_CMD", v) },
            None => unsafe { std::env::remove_var("AFCLI_DISPATCH_CMD") },
        }
        out
    }
}
