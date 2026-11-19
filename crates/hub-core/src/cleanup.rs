//! 磁盘清理**预演**（v0.8.2 子任务 6）：只算不删。
//!
//! 一句话：给「绑定的清理策略 + 设备回报的工作区实测清单」，算出**准备删什么、
//! 各占多大、清理后释放多少**。这是 `PRODUCT.md` §5.3 第四层（策略级）与
//! §6.1「磁盘清理预演」那一行的落地。
//!
//! # 为什么是「预演」而不是「清理」
//!
//! `PRODUCT.md` 的原话是「`disk_exhausted` 时列出准备删什么、各占多大、清理后
//! 释放多少，**用户确认后才执行**」，同一份文档还写死了硬规则「**不清理任何
//! 东西之前必须被选中**。磁盘清理是不可逆操作，不能默认执行」。
//!
//! 所以本模块**只有计算，没有删除**：它的输入是调用方给的清单（[`StoredItem`]，
//! 每项自带实测字节数），它不碰文件系统，也没有任何 `remove_*` 入口。这不是
//! 「暂时没写」——预演这个名字就是它的契约；真正执行清理是另一个动作，要人点。
//!
//! # 为什么不是一个孤立的纯函数
//!
//! 本项目踩过「算得对但永远不被调用」的坑（`ExecutionView::agent_id` 至今恒为
//! `None`，因为派发器没来）。一个只在测试里被调用的 [`preview`] 同样不算交付：
//! `PRODUCT.md` 要的是「**`disk_exhausted` 时**」这个时机成立。
//!
//! 所以本模块除了纯函数，还给出**挂着时机的那个入口** [`on_failure_ack`]：它
//! 从设备回执里认失败形态，只有 `disk_exhausted` 才跑预演，并把结果写成一条
//! 轨迹事实（kind = [`CLEANUP_PREVIEW_KIND`]）。将来 Hub 侧收到
//! `task_dispatch_ack` 之后调它。调用点由 `cleanup::tests` 里的端到端用例钉住：
//! 真回执进 → 轨迹里真有那份清单。
//!
//! # 三档策略各自留下什么
//!
//! | 策略 | 工作树 | 依赖目录 | 任务产物 |
//! |---|---|---|---|
//! | `Conservative` | 选中 | **留下** | 留最近 3 个（诊断用） |
//! | `Balanced` | 选中 | 选中 | 全选 |
//! | `Aggressive` | 选中 | 选中 | 全选 |
//!
//! 「产物留 3 个」与设备侧 `node-connector` 的 `Policy::keep_artifacts` 同值
//! （那边保守档给 3、其余 0）；「依赖目录留不留」是设备侧 `cache_ttl` 三档差别的
//! Hub 侧表达。**两处不共用同一个类型**（`hub-core` 不能依赖 `node-connector`），
//! 那份口径靠 `the_conservative_tier_matches_the_device_side_keep_count`
//! 一条用例钉住——那边改了保留数而这边没跟上时会红。

use std::path::{Component, Path};
use std::sync::atomic::{AtomicUsize, Ordering};

use hub_storage::trajectory::{NewEvent, TrajectoryEvent, TrajectoryRepo};
use hub_storage::{CleanupPolicy, ExecutionView, FailureKind, SessionRepo};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// 保守档保留的任务产物个数。与设备侧 `Policy::keep_artifacts` 的保守档同值。
///
/// 这个数只影响预演里哪些产物**不被选中**，不影响执行时真的留几个——真删的
/// 那一刻以设备侧策略为准。两边不一致的症状是「预演说留 3 个，实际留了 0 个」，
/// 那会让人以为清理误删了诊断材料。
pub const CONSERVATIVE_KEEP_ARTIFACTS: usize = 3;

/// 预演结果写进轨迹时用的 `kind`（`trajectory_event.kind`）。
///
/// 单独成常量是因为它有两个读者：写的那一侧（[`record_preview`]）与将来接
/// 页面 / 复盘的那一侧。抄两遍字面量，改一处就会让那份轨迹在页面上变成一条
/// 「不认识的事件」——它还在库里，但没人认它。
pub const CLEANUP_PREVIEW_KIND: &str = "disk_cleanup_preview";

// ---------------------------------------------------------------------------
// 输入：工作区里有什么
// ---------------------------------------------------------------------------

/// 工作区里的一个条目。**这是预演的唯一输入**，不来自文件系统。
///
/// # 为什么由调用方给清单，而不是本模块去读盘
///
/// 待清理的东西在**设备**上（`binding.fixed_dir` 那台机器的路径），Hub 没有
/// 那台机器的文件系统（`AGENTS.md` 约束 1：Hub 不需要任何到设备的入站访问）。
/// 清单随失败回执上来，Hub 只做「按策略挑哪些、加起来多少」。
///
/// 这也是「只算不删」在类型上的兑现：一个 `StoredItem` 只是**一段描述**，
/// 没有任何能删掉它的方法。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredItem {
    /// 设备上那条路径的原文（不展开 `~`、不规范化：Hub 没有那台设备的文件系统）。
    pub path: String,
    /// 实测占用字节数。**必须来自实测**——算错额度只会让下次再满，而编一个大小
    /// 会让用户在「可释放 12.4 GB」上做决定。
    pub bytes: u64,
    /// 这个条目是什么。决定它在三档策略下的去留。
    pub class: ItemClass,
    /// 这条最后一次被写过的时刻（Unix 秒）。`None` = 拿不到。
    ///
    /// 只有「留最近 N 个」那一档用得上（[`CleanupPolicy::Conservative`]）。
    /// **拿不到时不猜**：见 [`preview`] 对「没有时间的产物」的处置。
    pub modified_at: Option<i64>,
    /// 人读的名字（`node_modules` / `target` / 某个产物文件名）。
    ///
    /// 与 `path` 分开：`path` 是设备上的绝对路径，预演的文字要能只说
    /// 「`node_modules` 4.1 GB」，不必把整条路径摊在收件箱里。
    pub label: String,
}

/// 一个条目的类别。**闭集**，因为「留不留」的判定按它分派。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemClass {
    /// 任务的工作树本身（检出出来的源码 + 本次改动）。
    Worktree,
    /// 依赖目录 / 构建缓存（`node_modules` / `target` / `.venv` …）。
    ///
    /// 这些目录**永远不从别处搬**（`PRODUCT.md` §5.3 的缓存级），但清掉之后
    /// 下次执行要重装一遍——所以保守档留它。
    DependencyCache,
    /// 任务产物（agent 跑出来、留在工作树顶层的东西）。
    Artifact,
}

impl ItemClass {
    /// 线上字符串。与 `serde` 的 `snake_case` 同口径，单独列出来是给断言用的
    /// ——测试里写死字符串，不靠 `serde_json` 自比。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::DependencyCache => "dependency_cache",
            Self::Artifact => "artifact",
        }
    }

    /// 全部变体。新增类别时本数组与上面的 `match` 同时改。
    pub const ALL: &'static [Self] = &[Self::Worktree, Self::DependencyCache, Self::Artifact];
}

// ---------------------------------------------------------------------------
// 输出：预演结果
// ---------------------------------------------------------------------------

/// 预演里的一个候选：**准备删什么、多大、为什么**。
///
/// 前三个字段对应 `PRODUCT.md` 那句「列出准备删什么、各占多大」；「为什么会被
/// 删」由 [`Candidate::reason`] 逐条说清——没有它，用户看到的就是一串路径与
/// 数字，他无法判断该不该点确认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// 设备上的路径原文。
    pub path: String,
    /// 人读的名字。
    pub label: String,
    /// 占多大（实测字节）。
    pub bytes: u64,
    /// 类别。
    pub class: ItemClass,
    /// 为什么它会被删。**照着这一句能对上策略**，不是「策略要求」这种空话。
    pub reason: String,
}

/// 一次预演的结果。
///
/// **它不产生副作用**：没有任何字段说「已清理」，也没有「执行」的方法。拿着它
/// 只能给人看，或者等用户在收件箱里做出选择（那是后续里程碑的接线）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPreview {
    /// 准备删的条目，按「先大后小、同大小按路径」排序。
    ///
    /// 排序不是为了好看：收件箱里那份清单是给人做决定的，最大的那条排在最上面
    /// 才能一眼看出「清了它够不够」。顺序**稳定**（路径做 tiebreak），否则同一份
    /// 输入两次预演排出两个样子，人会觉得预演在乱说。
    pub candidates: Vec<Candidate>,
    /// 清理后能释放多少字节 = 候选大小之和。
    ///
    /// **不是估算**：它逐字等于 [`Candidate::bytes`] 的累加（用饱和加法，免得
    /// 一组离谱的输入让整个数回绕成一个「看起来合理」的小数字）。
    pub reclaimable_bytes: u64,
    /// 预演用的策略。留着是为了让收件箱能说出「按平衡档清理」这句话——
    /// 用户要判断的第一件事就是「这档是不是我要的」。
    pub policy: CleanupPolicy,
}

// ---------------------------------------------------------------------------
// 纯函数：按策略挑
// ---------------------------------------------------------------------------

/// 一次预演。
///
/// # 判定（逐条对应 [`CleanupPolicy`] 的三个变体）
///
/// - **工作树**：三档都选中。`disk_exhausted` 意味着这次的执行已经中止了，留着
///   它只会让下一次在同一个地方再满。
/// - **依赖缓存**：`Conservative` 不选（保守档的缓存要留 30 天），`Balanced` /
///   `Aggressive` 选中。
/// - **任务产物**：`Conservative` 留最近 [`CONSERVATIVE_KEEP_ARTIFACTS`] 个（按
///   `modified_at` 倒序），其余选中；`Balanced` / `Aggressive` 全选。
///
/// # 没有时间的产物
///
/// `modified_at` 是 `None` 时**留**（不选）：预演的承诺是「留下的那几个是最近的」，
/// 而拿不到时间就排不出先后。按「不知道就留下」处理，最坏结果是少算一点可释放
/// 空间；反过来的做法会让人在不知情时丢掉诊断材料，那是不可逆的。
///
/// 「留下」只对 `Conservative` 的名额而言：有时间的按新→旧先占名额，剩下的名额
/// 给无时间的**按路径字典序**（稳定、可复现），超出名额的才进候选。
/// `Balanced` / `Aggressive` 全选，无时间的也一样。
///
/// # 空清单 / 全不匹配
///
/// 空清单 → 空候选、`reclaimable_bytes == 0`。**不是错误**：一次预演没找到可删的
/// 东西是完全正常的结果（整棵工作树可能已被设备删干净了），把它变成 `Err` 会让
/// 调用方在 `disk_exhausted` 这条路径上再写一段「预演失败了怎么办」——而那段
/// 代码今天没有任何合理的动作可做。
pub fn preview(policy: &CleanupPolicy, items: &[StoredItem]) -> CleanupPreview {
    let kept_artifacts = artifacts_to_keep(policy, items);

    let candidates: Vec<Candidate> = items
        .iter()
        .filter_map(|item| why_removed(*policy, item, &kept_artifacts).map(|reason| (item, reason)))
        .map(|(item, reason)| Candidate {
            path: item.path.clone(),
            label: item.label.clone(),
            bytes: item.bytes,
            class: item.class,
            reason,
        })
        .collect();

    CleanupPreview {
        reclaimable_bytes: sum_bytes(&candidates),
        candidates,
        policy: *policy,
    }
    .sorted()
}

impl CleanupPreview {
    /// 按「先大后小、同大小按路径」排序。排序规则只有这一处。
    fn sorted(mut self) -> Self {
        self.candidates
            .sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        self
    }
}

/// 候选大小之和。**唯一的求和处**——`reclaimable_bytes` 的定义与断言都看它。
fn sum_bytes(candidates: &[Candidate]) -> u64 {
    candidates
        .iter()
        .fold(0u64, |acc, c| acc.saturating_add(c.bytes))
}

/// 这一档策略要留下的产物路径集合。
fn artifacts_to_keep(policy: &CleanupPolicy, items: &[StoredItem]) -> Vec<String> {
    if !matches!(policy, CleanupPolicy::Conservative) {
        return Vec::new();
    }
    let mut timed: Vec<&StoredItem> = items
        .iter()
        .filter(|i| i.class == ItemClass::Artifact && i.modified_at.is_some())
        .collect();
    // 新的在前。同时间的按路径排，保证同一份输入两次跑出同一份保留集合。
    timed.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut keep: Vec<String> = timed
        .iter()
        .take(CONSERVATIVE_KEEP_ARTIFACTS)
        .map(|i| i.path.clone())
        .collect();

    // 名额还有剩：给「拿不到时间」的，按路径字典序。拿不到时间不等于「最旧」，
    // 所以它们排在**有时间的之后**——有时间的证据更硬。
    let remaining = CONSERVATIVE_KEEP_ARTIFACTS.saturating_sub(keep.len());
    if remaining > 0 {
        let mut untimed: Vec<&StoredItem> = items
            .iter()
            .filter(|i| i.class == ItemClass::Artifact && i.modified_at.is_none())
            .collect();
        untimed.sort_by(|a, b| a.path.cmp(&b.path));
        keep.extend(untimed.iter().take(remaining).map(|i| i.path.clone()));
    }
    keep
}

/// 这个条目会不会被删？会的话，理由是什么。`None` = 留下。
fn why_removed(
    policy: CleanupPolicy,
    item: &StoredItem,
    kept_artifacts: &[String],
) -> Option<String> {
    match item.class {
        ItemClass::Worktree => Some(format!(
            "工作树：这次执行已中止，留着它下次还在同一处满（{} 档）",
            policy.as_str()
        )),
        ItemClass::DependencyCache => match policy {
            CleanupPolicy::Conservative => None,
            CleanupPolicy::Balanced => Some("依赖缓存：平衡档只留 7 天，已过期".to_string()),
            CleanupPolicy::Aggressive => Some("依赖缓存：激进档跑完即清".to_string()),
        },
        ItemClass::Artifact => {
            if kept_artifacts.iter().any(|p| p == &item.path) {
                return None;
            }
            match policy {
                CleanupPolicy::Conservative => Some(format!(
                    "任务产物：保守档只留最近 {CONSERVATIVE_KEEP_ARTIFACTS} 个做诊断，它不在其中"
                )),
                CleanupPolicy::Balanced => Some("任务产物：平衡档跑完即清".to_string()),
                CleanupPolicy::Aggressive => Some("任务产物：激进档跑完即清".to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 调用点：挂在 disk_exhausted 回报上
// ---------------------------------------------------------------------------

/// 预演被调用过多少次（进程级累计）。
///
/// 存在的理由是那条**调用点断言**：里程碑要求证明「`disk_exhausted` 进来 →
/// `preview` 真的被调用」，而「结果有值」不足以证明——一次实现完全可以在别处
/// 凑出同样的结果而从不进 [`preview`]。这个计数器把「被调用」变成可观测的事实。
///
/// **进程级而非注入一个记录器**：注入要给每个调用方（将来的 ack 处理）多一个
/// 参数，那是为测试往生产签名里加东西。进程级的代价只在测试里——并发跑的多条
/// 用例共享同一个计数，所以断言一律写**相对增量**（前后读数之差），不写绝对值。
static PREVIEWS_RUN: AtomicUsize = AtomicUsize::new(0);

/// 预演被调用过多少次（进程级累计）。见 [`PREVIEWS_RUN`]。
pub fn previews_run() -> usize {
    PREVIEWS_RUN.load(Ordering::Relaxed)
}

/// 一份已经落成轨迹事实的预演。
///
/// 与 [`CleanupPreview`] 分开，是为了让「算出来了」与「它落到了轨迹哪一条上」
/// 是两件事：只返回前者的话，调用方没法证明留痕真的发生了。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedPreview {
    /// 算了什么。
    pub preview: CleanupPreview,
    /// 这条事实的 `trajectory_event` 行。
    pub event: TrajectoryEvent,
}

/// 一次**失败回执**里预演要用到的输入。
///
/// 刻意不含 `execution_id` 之外的任何执行字段：预演只看「这次执行失败了，形态
/// 是什么」，不看它在哪台设备、跑了多久。把整个 `ExecutionView` 传进来会让
/// 本函数有机会顺手改它——而「不改任何东西」是这条路径的硬约束。
#[derive(Debug, Clone, Copy)]
pub struct FailureReport<'a> {
    /// 刚失败的那次执行的 id（用来在轨迹里指回它）。
    pub execution_id: &'a str,
    /// 失败形态。`None` = 回执没报形态，预演不启动。
    ///
    /// 只认 [`FailureKind::DiskExhausted`]（`PRODUCT.md` 的触发时机就是
    /// 「`disk_exhausted` 时」，不是「磁盘用得多时」——后者是调度层的 `/90%`
    /// 判断，属另一件事）。
    pub failure_kind: Option<FailureKind>,
}

impl<'a> FailureReport<'a> {
    /// 从一次**已经记在库里的**执行读回执：失败形态取
    /// `ExecutionView::failure_kind`。
    ///
    /// 生产路径该用这个（ack 处理先 `set_failure` 落库，再问预演），因为它读的
    /// 是**落库之后**的真值，而不是回执里那个可能没通过校验的字符串。
    pub fn of_execution(exec: &'a ExecutionView) -> Self {
        Self {
            execution_id: exec.id.as_str(),
            failure_kind: exec.failure_kind,
        }
    }
}

/// **调用点**：处理一次失败回执，`disk_exhausted` 时跑预演并留痕。
///
/// # 时机
///
/// 设备回报 `task_dispatch_ack{ stage: Failed, failure_kind: "disk_exhausted" }`
/// 之后，Hub 侧在**同一条**处理链里调它。这就是 `PRODUCT.md` 要的那个时机：
/// 「**`disk_exhausted` 时**」。不是定时任务，也不是某个独立端点。
///
/// # 参数
///
/// - `report`：失败回执的最小投影（见 [`FailureReport`]）。
/// - `items`：设备实测清单（见 [`StoredItem`]）。Hub 编不出来，只能等设备报。
/// - `policy`：绑定的清理策略（`Binding::cleanup_policy`）。**从这里读，不另存**
///   ——策略的唯一真相在绑定上，抄一份到别处迟早对不上。
/// - `session_id`：这份事实挂到哪条会话的轨迹上。`disk_exhausted` 的执行不一定
///   有会话，所以是 `Option`：没有会话就**不跑**（跑出来的结果没处放）。
/// - `trajectory`：轨迹仓储。
/// - `now`：写入时刻（由调用方给，便于测试与幂等）。
///
/// # 返回 `Ok(None)` 的三种情况（都不是错误）
///
/// 1. 失败形态不是 `disk_exhausted`：本函数的时机只有那一种。别的失败也预演会把
///    「换个 agent 再来」变成「先清一次盘」，那是错的动作。
/// 2. 没有会话可挂：没法留痕就不算预演过。**`PREVIEWS_RUN` 也不会涨**——那
///    计数器的契约是「真的跑了一次预演并打算留痕」。
/// 3. 设备没给清单（空清单）：预演算出「没有可释放的东西」是**诚实的答案**，
///    但它不值得留一条轨迹。这种情况仍算「跑过预演」（计数会涨），因为
///    `preview` 确实被调用了——这正是调用点断言要看的东西。
///
/// # 只算不删、不改任务状态
///
/// 本函数不写 `task` 表一个字节，不写 `execution` 表一个字节，不碰磁盘。
/// `task-model.md` §3.1 对 `disk_exhausted` 的处置是「不直接判失败，先转
/// `running.waiting_for_user` 把清理方案放进收件箱」——「转状态」是调用方的事，
/// 不是预演的。这条不是「本期先这样」：预演的定义就是不产生副作用。
pub fn on_failure_ack(
    report: FailureReport<'_>,
    items: &[StoredItem],
    policy: &CleanupPolicy,
    session_id: Option<&str>,
    trajectory: &TrajectoryRepo,
    now: &str,
) -> Result<Option<RecordedPreview>> {
    if report.failure_kind != Some(FailureKind::DiskExhausted) {
        return Ok(None);
    }
    let Some(session_id) = session_id else {
        return Ok(None);
    };

    PREVIEWS_RUN.fetch_add(1, Ordering::Relaxed);
    let result = preview(policy, items);
    if result.candidates.is_empty() {
        // 跑了预演，但没什么可说的：不写轨迹（省一条毫无信息量的行），也不
        // 假装留过痕。`PREVIEWS_RUN` 已经涨过——它记的是「preview 被调用」。
        return Ok(None);
    }

    let event = record_preview(trajectory, session_id, report.execution_id, &result, now)?;
    tracing::info!(
        execution_id = report.execution_id,
        candidates = result.candidates.len(),
        reclaimable_bytes = result.reclaimable_bytes,
        policy = policy.as_str(),
        "disk_exhausted：已算出清理预演（只算不删），等人确认"
    );
    Ok(Some(RecordedPreview {
        preview: result,
        event,
    }))
}

/// 把一份预演写成轨迹事实。
///
/// `payload` 的形状就是 [`CleanupPreview`] 本身（外加执行 id），所以将来接页面时
/// **不需要另一套 DTO**：读出来的 JSON 直接就是那份清单。这与既有的「轨迹存原始
/// JSON」是同一条口径。
fn record_preview(
    trajectory: &TrajectoryRepo,
    session_id: &str,
    execution_id: &str,
    result: &CleanupPreview,
    now: &str,
) -> Result<TrajectoryEvent> {
    let seq = next_hub_seq(trajectory, session_id)?;
    let payload = serde_json::to_string(&serde_json::json!({
        "executionId": execution_id,
        "policy": result.policy.as_str(),
        "reclaimableBytes": result.reclaimable_bytes,
        "candidates": result.candidates,
    }))?;
    let event = NewEvent::new(seq, CLEANUP_PREVIEW_KIND, payload);
    if !trajectory.append(session_id, HUB_NODE_ID, &event, now)? {
        // `append` 返回 `false` 说明唯一约束把这条挡下了（同一 `seq` 已经写过）。
        // 那意味着会话上同时有两次预演在抢号——**不静默成功**：调用方以为留痕了，
        // 而轨迹里只有一份。
        return Err(CoreError::CleanupPreviewSeqTaken { seq });
    }
    // 回读刚写的那一条：`append` 只回「写进去了没有」，而调用方要拿到行（含 id）
    // 才证明得了「留痕落在哪一条上」。
    trajectory
        .list(session_id, 0, i64::MAX)?
        .into_iter()
        .rev()
        .find(|e| e.kind == CLEANUP_PREVIEW_KIND && e.node_id == HUB_NODE_ID && e.seq == seq)
        .ok_or(CoreError::CleanupPreviewNotRecorded)
}

/// Hub 侧下一条 `seq`。
///
/// # 为什么 Hub 要自己编号
///
/// 轨迹的 `seq` 是**设备侧**会话内序号，`UNIQUE(session_id, node_id, seq)` 是它
/// 的闸门。Hub 产生的事实（预演、将来管家的总结）若与设备共用同一个 `node_id`
/// 分区，编出来的号迟早撞上设备那条**真实**事件，于是后者被 `INSERT OR IGNORE`
/// 悄悄吞掉——轨迹少一条而没人知道。
///
/// 所以 Hub 的事件写在自己的分区（[`HUB_NODE_ID`]）里，编号从该分区的最大值
/// +1 取。**该分区只有 Hub 一个写者**，所以「读 MAX 再 +1」不会有并发争号。
fn next_hub_seq(trajectory: &TrajectoryRepo, session_id: &str) -> Result<i64> {
    let last = trajectory
        .list(session_id, 0, i64::MAX)?
        .into_iter()
        .filter(|e| e.node_id == HUB_NODE_ID)
        .map(|e| e.seq)
        .max();
    Ok(last.map_or(0, |s| s.saturating_add(1)))
}

/// Hub 自己写轨迹时用的 `node_id`。
///
/// 轨迹表按 `(session_id, node_id, seq)` 分区，而**设备**分区归设备写。Hub 产生的
/// 事实必须有一个自己的分区，否则会与设备那条同号（见 [`next_hub_seq`]）。
const HUB_NODE_ID: &str = "hub";

// ---------------------------------------------------------------------------
// 从回执里认那份清单
// ---------------------------------------------------------------------------

/// 设备回执里挂在载荷上的「工作区实测清单」字段名。
///
/// `task_dispatch_ack` 的 `evidence` 是给人看的自由文本，预演要的是**逐项大小**，
/// 那是结构化数据，塞不进一句话。约定：载荷上可选带一个 `items` 数组，形状就是
/// [`StoredItem`]。
pub const ACK_ITEMS_FIELD: &str = "items";

/// 从回执载荷里取那份清单。
///
/// # 拿不到就是空清单
///
/// 设备没报（键缺失 / 不是数组）→ 空清单 → 预演算出「没有可释放的东西」。
/// 这是**诚实的答案**：设备没告诉我们每一项多大，我们就不编。
///
/// # 单个条目坏了只跳过它
///
/// 一条解不出来的条目不该让整份清单作废。`PRODUCT.md` 说「磁盘满了不是性能问题，
/// 是任务会静默失败的问题」，而一条预演静默变成空清单属于同一类——能留住多少
/// 信息就留多少，比整份丢掉好。
pub fn items_from_ack_payload(payload: &serde_json::Value) -> Vec<StoredItem> {
    let Some(arr) = payload
        .get(ACK_ITEMS_FIELD)
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| serde_json::from_value::<StoredItem>(v.clone()).ok())
        .collect()
}

/// 路径是否是「设备侧的一个绝对路径」。
///
/// 只是给调用方一个判定（接到一份明显不对的清单时留一条日志）。预演**不据此
/// 排除**任何条目：清不清理是策略的事，不是路径形状的事。
/// `Binding.fixed_dir` 的口径同样「不展开 `~`」，所以 `~` 开头的一律不是绝对路径。
pub fn is_absolute_device_path(path: &str) -> bool {
    Path::new(path)
        .components()
        .next()
        .is_some_and(|c| matches!(c, Component::RootDir))
}

/// 从会话仓储里找「挂在这条执行上」的会话 id。
///
/// 给 ack 处理用：它手里有 `SessionRepo`，但 `hub-storage` **没有
/// `execution_id → session` 的反向查询**（`session.execution_id` 只有写入与逐条
/// 读）。这条函数把那个查询收在一处，免得将来第二个调用方各写一遍、两处口径不同。
///
/// 找不到（`disk_exhausted` 的执行常常还没人点开过对话）→ `None`。那不是错误：
/// 预演会因此不跑（见 [`on_failure_ack`] 的第 2 种 `Ok(None)`）。
pub fn session_for_execution(sessions: &SessionRepo, execution_id: &str) -> Result<Option<String>> {
    let found = sessions
        .list()?
        .into_iter()
        .find(|item| item.session.execution_id.as_deref() == Some(execution_id));
    Ok(found.map(|item| item.session.id))
}

#[cfg(test)]
mod tests;
