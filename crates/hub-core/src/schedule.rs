//! 任务路由的纯函数：三层硬过滤 + 有序打分 + 拒绝归因（v0.8.2 子任务 2）。
//!
//! 一句话：给「任务约束 + 一批设备心跳 + 绑定信息」，算出**选哪台**或**为什么
//! 没有合格的**。这是 `scheduling.md` §3 / §4 与 `task-model.md` §7.1 / §7.2 的
//! 落地。
//!
//! # 为什么是一个纯函数
//!
//! **不碰 IO**：不查库、不读文件、不看表、不 `tracing::info!`。理由不是洁癖——
//! 硬过滤的顺序是规格的一部分（⓪ 必须先于 ①②③），而「顺序对不对」只有在
//! **能构造任意输入**的前提下才测得透。一旦本函数顺手去读一次库，每条用例都得
//! 先造一个库，于是最该被钉住的那几条（哪一层先拒、边界少 1 字节）反而写不出来。
//!
//! 数据从哪来由调用方（子任务 3 的运行时）负责：心跳取
//! [`crate::Reports`]（内存，不读库，`scheduling.md` §2.2），绑定取
//! [`hub_storage::BindingRepo`]。本模块只定义它们**以什么形状**进来。
//!
//! # 过滤与排序是两件事，不许混
//!
//! - **过滤是硬的**：不满足就出局，不靠高分补偿（`scheduling.md` §3）。
//!   能力不够的设备再闲也不能用。
//! - **排序是字典序比较链，不是加权评分**（§4.3）：把负载 / 内存 / 磁盘 / 亲和
//!   揉成一个浮点数，权重全靠拍脑袋，且「为什么选了这台」答不上来。有序链能答。
//!
//! # 本模块**不写**的部分
//!
//! `retry_policy`（失败形态 → 换设备 / 换 agent / 不重试）属于子任务 5，由另一
//! 名开发者在本文件里补。本模块的结构不挡路：它只需要 `FailureKind` 与一个新的
//! `RetryPlan` 枚举，与下面的常量、[`Routing`]、[`pick`] 零交集。

use std::collections::{BTreeSet, HashMap};

use hub_storage::project::ProjectShape;
use hub_storage::Binding;

use crate::reports::Report;

// ---------------------------------------------------------------------------
// 磁盘门槛的三个常量（`scheduling.md` §3.2）
// ---------------------------------------------------------------------------

/// 安全余量：**任务峰值事前不可知**（agent 写多少临时文件、构建产物多大），
/// 所以除了 worktree 基础开销之外还要留一段。
///
/// 取 512 MiB 的理由：它够一次中等规模依赖重建（`npm ci` / `cargo fetch` 的
/// 临时解包峰值）落地，又不至于把一块 2 GiB 的设备直接判死。**这个数是估算，
/// 不是测量**——真省事的做法是编一个精确到 MB 的「预期峰值」，那正是
/// `scheduling.md` §3.2 明令禁止的「不自作聪明猜一个数」。峰值真正靠执行时的
/// 磁盘监控兜底（超预算阈值 → 提前中止并标 `disk_exhausted`）。
pub const DISK_SAFETY_MARGIN_BYTES: u64 = 512 * 1024 * 1024;

/// worktree 基础开销的**下限**。
///
/// 真值是「checked-out 文件总大小 + 依赖重建体积」，只有 git 算得准
/// （`scheduling.md` §3.2）。心跳里没有 tree 的 blob 总大小，所以这里取一个
/// 下限：磁盘余量低于它一定放不下任何工作树。任务可以显式给
/// `TaskConstraints::required_disk_mb` 抬高它。
pub const WORKTREE_BASE_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 负载字段口径：与 `hub-server/src/routes/api.rs` 的 `load_percent` 同源
// ---------------------------------------------------------------------------

/// 心跳 `resources` 里 CPU 百分比的候选字段名。
///
/// **与 `api.rs` 的 `CPU_PERCENT_FIELDS` 逐字相同**，不是巧合也不是抄来的：
/// 两处口径不一致的后果 `scheduling.md` §2 已经写过一次——嵌套对象会让读取端
/// 恒返回 `None`，于是页面上的负载列永远是 `—`，而「采集已落地」这条事实在
/// 页面上完全看不出来。`hub-core` **不能**依赖 `hub-server`（反向依赖会成环），
/// 所以只能各存一份；两份的内容由本文件的
/// `the_load_field_names_match_the_api_side` 一条用例钉住——那边加/改字段名
/// 而这边没跟上时，那条用例会红。
pub(crate) const CPU_PERCENT_FIELDS: &[&str] = &["cpu", "cpu_percent", "load"];

/// 心跳 `resources` 里内存百分比的候选字段名。理由同 [`CPU_PERCENT_FIELDS`]。
pub(crate) const MEM_PERCENT_FIELDS: &[&str] = &["mem", "mem_percent", "memory"];

/// 从心跳上报里取一个负载百分比。**与 `api.rs` 的 `load_percent` 同口径**：
/// 同一批候选键（[`CPU_PERCENT_FIELDS`] / [`MEM_PERCENT_FIELDS`]），同样按
/// `u64` 取（浮点 / 字符串一律不认），同样 `.min(100)` 截断。
///
/// 取不到就是 `None`，**不回落成 0**：路由把「未上报」当成「占满了」会把一台
/// 刚连上的设备判成该关机（`api.rs` 里同一句警告）。
fn load_percent(resources: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    let obj = resources.as_object()?;
    for k in keys {
        if let Some(n) = obj.get(*k).and_then(serde_json::Value::as_u64) {
            return Some(n.min(100));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 任务约束
// ---------------------------------------------------------------------------

/// 任务级约束。**复用既有字段的形状**，不新造一套语义。
///
/// `hub-server/src/routes/tasks.rs` 的 `Constraints` / `TaskView` 已经有
/// `allowed_devices` / `allowed_agents` / `required_capabilities` 三个
/// `Option<String>`（库里存的是**原文 JSON 字符串**，见 `schema_v7.sql`）。
/// `hub-core` 不依赖 `hub-server`，所以这里定义一个**同形的**结构，由
/// `hub-server` 侧做一次转换（子任务 3 负责）。
///
/// # 三个 `Option<String>` 里装什么
///
/// 字符串的解析在 [`TaskConstraints::allowed_devices`] 等访问点上做，**不在
/// 构造时做**：构造它的调用方可能原样搬库里的值，也可能给一个人写的
/// `"a, b"`。两种都能解析——见 [`parse_name_list`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskConstraints {
    /// 只许在这些设备上跑。`None` = 不限。
    pub allowed_devices: Option<String>,
    /// 只许用这些 agent。`None` = 不限。
    pub allowed_agents: Option<String>,
    /// 必须具备的能力（connector 名 / `agent:<id>` / 裸的 `arch:<x>` 之类）。
    /// `None` = 不限。
    pub required_capabilities: Option<String>,
    /// 钉死设备（`scheduling.md` §4.2）：**唯一候选，跳过全部过滤与打分**。
    pub pinned_device: Option<String>,
    /// 任务级磁盘需求（worktree 基础开销的显式值）。`None` = 用
    /// [`WORKTREE_BASE_BYTES`] 那个下限。
    pub required_ram_mb: Option<u64>,
    /// 见 [`TaskConstraints::required_ram_mb`]：同一件事，磁盘那一半。
    pub required_disk_mb: Option<u64>,
}

impl TaskConstraints {
    /// 只要钉死一台设备，其余一律不限。
    pub fn pinned(node_id: impl Into<String>) -> Self {
        Self {
            pinned_device: Some(node_id.into()),
            ..Self::default()
        }
    }

    /// `allowed_agents` 的解析结果（去重 + 稳定顺序 + 大小写不敏感）。
    fn allowed_agents(&self) -> BTreeSet<String> {
        parse_name_list(self.allowed_agents.as_deref())
    }

    /// `required_capabilities` 的解析结果。
    fn required_capabilities(&self) -> BTreeSet<String> {
        parse_name_list(self.required_capabilities.as_deref())
    }

    /// worktree 基础开销（字节）。任务给了 `required_disk_mb` 就用它，
    /// **但不低于 [`WORKTREE_BASE_BYTES`]**：任务作者把一个肯定放不下的数写小，
    /// 不该让硬门槛跟着变松。
    fn worktree_base_bytes(&self) -> u64 {
        match self.required_disk_mb {
            Some(mb) => mb.saturating_mul(1024 * 1024).max(WORKTREE_BASE_BYTES),
            None => WORKTREE_BASE_BYTES,
        }
    }

    /// 磁盘硬门槛：剩下的空间至少得有这么多。
    fn disk_floor_bytes(&self) -> u64 {
        self.worktree_base_bytes()
            .saturating_add(DISK_SAFETY_MARGIN_BYTES)
    }
}

/// 把「名单」字符串解析成集合。**两种形态都认**（与 `api.rs` 认两种 agent 形态
/// 同一立场：认不出新形态会让数据静默消失）。
///
/// - JSON 数组：`["device-a","device-b"]`（库里存的原文）
/// - 逗号分隔的人写名单：`"device-a, device-b"`
/// - 单个名字：`"device-a"`
///
/// 空串 / `null` / 空数组都得到空集合。**「不限」与「限了但名单是空的」是两件
/// 事**：前者是 `None`，后者解析出来是空集合而 `None` 的判断在调用方那里
/// （见 [`gate_zero`]）；把后者当成「不限」会让一条写坏了的约束静默放行全部设备。
fn parse_name_list(raw: Option<&str>) -> BTreeSet<String> {
    let Some(raw) = raw else {
        return BTreeSet::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return BTreeSet::new();
    }
    // JSON 数组优先：库里存的是原文 JSON 字符串，先按它解。
    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        return items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// 拒绝归因
// ---------------------------------------------------------------------------

/// 一层硬过滤为空时的归因（`task-model.md` §7.2、`scheduling.md` §3）。
///
/// **每个变体对应一层**，顺序就是过滤顺序。变体名是 `Rejection` 自己的身份，
/// 映射到 `rejected(...)` 的线上码是调用方（子任务 3）的事——但那映射只能是
/// 一对一的，见 [`Rejection::as_str`]。
///
/// 为什么不用「一个字符串 + 一句话」：字符串会漂移，而枚举的穷尽匹配会让
/// 「加了第 9 层却忘了归因」在编译期就红。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rejection {
    /// ⓪ 任务级 `allowed_devices` / `allowed_agents` / `required_capabilities`。
    /// **必须先于 ①②③**：否则约束降级成偏好（`task-model.md` §7.1）。
    NoAllowedDevice,
    /// ① 项目级隔离策略：隔离式项目只有**绑定了的那台**有资格。
    IsolationBlocked,
    /// ② 这台设备没有任务要求的 agent。
    NoAgent,
    /// ③ 这台设备没有任务要求的 connector / 能力。
    NoConnector,
    /// ④ 剩余内存不够。
    RamInsufficient,
    /// ⑤ 磁盘不够（`scheduling.md` §3.2，**最容易被写漏的一条**）。
    DiskInsufficient,
    /// ⑥ 拿不到代码。
    NoCode,
    /// ⑦ 该绑定已有任务在跑（默认串行，`scheduling.md` §3.3）。
    BindingBusy,
}

impl Rejection {
    /// 线上字符串（`rejected(...)` 的括号里那一句），与 `task-model.md` §7.2 的
    /// 原文一致。单独列出来是给断言用的——测试里写死字符串，不靠自比。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoAllowedDevice => "no_allowed_device",
            Self::IsolationBlocked => "isolation_blocked",
            Self::NoAgent => "no_agent",
            Self::NoConnector => "no_connector",
            Self::RamInsufficient => "ram_insufficient",
            Self::DiskInsufficient => "disk_insufficient",
            Self::NoCode => "no_code",
            Self::BindingBusy => "binding_busy",
        }
    }

    /// 全部变体。新增一层过滤时本数组与上面的 `match` 同时改，否则
    /// `every_rejection_variant_is_reachable_by_its_own_layer` 会红。
    pub const ALL: &'static [Self] = &[
        Self::NoAllowedDevice,
        Self::IsolationBlocked,
        Self::NoAgent,
        Self::NoConnector,
        Self::RamInsufficient,
        Self::DiskInsufficient,
        Self::NoCode,
        Self::BindingBusy,
    ];
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// 路由器
// ---------------------------------------------------------------------------

/// 路由一次的输入：任务约束 + 项目形态 + 绑定。
///
/// `bindings` 是**只读**的绑定表（`(project, device) → Binding` 的投影）。
/// 本模块不查库，所以调用方先把「这个项目上的绑定」读出来再进来——表很小
/// （一个项目通常绑 1~3 台设备），一次路由读一次是可接受的。
///
/// **`execution` 是否在跑**由 [`Routing::busy`] 提供，不由本模块查：
/// 「什么算在跑」是执行状态机的口径（`ExecutionState::is_terminal`），不该在
/// 调度器里再写一遍。
///
/// **不 derive `Debug`**：`busy` 是一个 `&dyn Fn`，它实现不了 `Debug`。为了一个
/// 诊断用的 derive 给它包一层新类型，会让调用方多一次包装；本类型只在一次路由
/// 的栈上活着，不需要被打印。
#[derive(Clone)]
pub struct Routing<'a> {
    /// 项目形态（`hub_storage::ProjectShape`）。隔离式走 ① 的额外限制。
    pub shape: ProjectShape,
    /// `(node_id, binding)` 对。用切片而不是 `HashMap`：**绑定表很小**，而切片
    /// 让调用方（与测试）不必先造一个 map；同一 `node_id` 出现两次时取第一个。
    pub bindings: &'a [(String, Binding)],
    /// 该绑定上的执行是否还在跑（`scheduling.md` §3.3 默认串行）。
    ///
    /// 传函数而不是 `HashSet<String>`：调用方手里的「在跑的执行」可能是
    /// `ExecutionRepo::list_for_binding` 的 `Vec<ExecutionView>`，也可能是内存里的
    /// 一张表；本模块不该规定它是哪一种。
    pub busy: &'a dyn Fn(&str) -> bool,
}

impl<'a> Routing<'a> {
    /// 只给形态与绑定的最小构造：一份绑定都没有，且什么都不忙。
    ///
    /// 生产路径用 [`Routing::new`]（它要 `busy`）；本函数是给「绑定为空」这种
    /// 语义明确的场景与测试用的便捷入口。
    pub fn bare(shape: ProjectShape, bindings: &'a [(String, Binding)]) -> Self {
        Self {
            shape,
            bindings,
            busy: &never_busy,
        }
    }

    pub fn new(
        shape: ProjectShape,
        bindings: &'a [(String, Binding)],
        busy: &'a dyn Fn(&str) -> bool,
    ) -> Self {
        Self {
            shape,
            bindings,
            busy,
        }
    }

    /// 该节点在这个项目上的绑定。
    fn binding_of(&self, node_id: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|(n, _)| n == node_id)
            .map(|(_, b)| b)
    }
}

/// 「什么都不忙」。写成具名函数而不是每处 `&|_| false`：后者在 `Routing` 里
/// 活不下来（闭包临时值的生命周期不够长）。
fn never_busy(_binding_id: &str) -> bool {
    false
}

/// 选一台设备。**过滤顺序固定，不可调换**（`scheduling.md` §3）：
///
/// ```text
/// ⓪ allowed_devices / allowed_agents / required_capabilities → NoAllowedDevice
/// ① 隔离式项目且没绑定这台                                   → IsolationBlocked
/// ② 无所需 agent                                            → NoAgent
/// ③ 无所需 connector / 能力                                  → NoConnector
/// ④ 内存不足                                                 → RamInsufficient
/// ⑤ 磁盘不足                                                 → DiskInsufficient
/// ⑥ 拿不到代码（无 workspace root 覆盖）                       → NoCode
/// ⑦ 该绑定已有任务在跑                                        → BindingBusy
/// ```
///
/// 过滤之后按**有序比较链**选第一名（不是加权评分）：
///
/// ```text
/// 1. pinned_device 命中 → 唯一候选，直接选，跳过全部过滤（§4.2）
/// 2. load_per_core 越低越好
/// 3. 剩余内存越多越好
/// 4. 剩余磁盘充裕度越多越好
/// 5. 已在该节点有工作区 → 优先
/// 6. 已装目标 agent 且版本匹配 → 优先
/// 7. node_id 字典序（稳定 tiebreak）
/// ```
///
/// # 拒绝归因说的是「最靠前的那一层」
///
/// 一台设备可能同时踩三层。返回的是**序号最小的那层**——因为「先按约束筛」
/// 才是规格要求的口径：如果一台设备因为 `allowed_devices` 不匹配而被排除，
/// 报「它磁盘也不够」会让人去清磁盘，而真该做的是改约束（`scheduling.md` §3.2
/// 同一条立场：「必须说清是磁盘不够而不是设备离线」）。
///
/// # 全空时
///
/// `nodes` 是空迭代器 → [`Rejection::NoAllowedDevice`]（「一台可用的设备都没有」
/// 是约束那一层的话；`task-model.md` §7.2 的原文「任务限定在 A/B/C 上跑，
/// 现在都不可用」）。
pub fn pick<'a>(
    task: &TaskConstraints,
    nodes: impl IntoIterator<Item = (&'a str, &'a Report)>,
    routing: &Routing<'_>,
) -> Result<&'a str, Rejection> {
    let collected: Vec<(&'a str, &'a Report)> = nodes.into_iter().collect();

    // ---- 1. 钉死：唯一候选，跳过全部过滤（§4.2）----
    //
    // 放在过滤之前是刻意的：用户钉死一台设备是要访问它上面的串口 / 硬件，
    // 「可是它磁盘不够」不该成为拒绝的理由——那是用户自己选的机器，出问题
    // 也该由执行时的磁盘监控报 `disk_exhausted`。
    if let Some(pinned) = task.pinned_device.as_deref() {
        if let Some(node_id) = find_node(&collected, pinned) {
            return Ok(node_id);
        }
        // 钉死的那台不在候选里（离线 / 不存在）：不能悄悄换成别人——那会
        // 把「必须在 X 上跑」变成「尽量在 X 上跑」。按 ⓪ 归因。
        return Err(Rejection::NoAllowedDevice);
    }

    // ---- 2. 逐层硬过滤 ----
    let mut survivors: Vec<(&'a str, &'a Report)> = Vec::with_capacity(collected.len());
    // 每层第一个被拒的归因，按层号取最小的那个。
    let mut first_rejection: Option<Rejection> = None;
    for (node_id, report) in &collected {
        match filter(task, routing, node_id, report) {
            None => survivors.push((node_id, report)),
            Some(r) => {
                first_rejection = Some(match first_rejection {
                    // `Rejection` 的 `Ord` 就是过滤顺序（变体声明顺序）：
                    // 取最小的那个 = 最靠前的那一层。
                    Some(prev) if prev <= r => prev,
                    _ => r,
                });
            }
        }
    }

    // ---- 3. 有序比较链选第一名 ----
    let best = survivors.into_iter().min_by(|a, b| {
        // `min_by` 要「a 比 b 好就返回 Less」，所以整条链的方向都是「好的在前」。
        compare_candidates(task, routing, a, b)
    });

    match best {
        Some((node_id, _)) => Ok(node_id),
        // 一台都没活下来：报最靠前的那层的归因。真的一台设备都没有时
        // `first_rejection` 是 `None`，归到 ⓪（「没有可用设备」）。
        None => Err(first_rejection.unwrap_or(Rejection::NoAllowedDevice)),
    }
}

/// 在一批候选里按名字找节点，返回**候选里的那个 `&'a str`**（不是入参里的）。
fn find_node<'a>(nodes: &[(&'a str, &'a Report)], wanted: &str) -> Option<&'a str> {
    nodes
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(wanted))
        .map(|(id, _)| *id)
}

/// 对一台设备跑完 ⓪–⑦。`None` = 通过。
fn filter(
    task: &TaskConstraints,
    routing: &Routing<'_>,
    node_id: &str,
    report: &Report,
) -> Option<Rejection> {
    // ⓪ 任务级约束（设备名单）。**必须先于 ①②③**（`task-model.md` §7.1）：
    // 先按负载挑出「最闲的那台」再检查 allowed_*，约束就降级成偏好了。
    if let Some(r) = gate_zero(task, node_id) {
        return Some(r);
    }

    let binding = routing.binding_of(node_id);

    // ① 项目级隔离策略：隔离式项目的代码只在指定设备上，**只有绑定了的那台有资格**。
    if routing.shape == ProjectShape::Isolated && binding.is_none() {
        return Some(Rejection::IsolationBlocked);
    }

    // ② 所需 agent。⓪ 已经保证「要求的那些都在 allowed_agents 里」，
    // 这里问的是「这台设备**有没有**」。
    if !has_required_agents(task, report) {
        return Some(Rejection::NoAgent);
    }

    // ③ 所需 connector / 能力。
    if !has_required_capabilities(task, report) {
        return Some(Rejection::NoConnector);
    }

    // ④ 剩余内存。
    if let Some(need) = task.required_ram_mb {
        if available_mem_mb(report).unwrap_or(0) < need {
            return Some(Rejection::RamInsufficient);
        }
    }

    // ⑤ 磁盘硬门槛：剩余 − 安全余量 < 基础开销 → 排除（§3.2）。
    if let Some(free) = free_disk_bytes(report, binding) {
        if free < task.disk_floor_bytes() {
            return Some(Rejection::DiskInsufficient);
        }
    }

    // ⑥ 能拿到代码。判定与 `node-core/src/heartbeat.rs` 的 `send_heartbeat`
    // 同一口径：**这一期设备上报的 `workspace_roots` 恒为空**（当前还没有
    // 「用户配置的工作区」这个概念，且不允许遍历文件系统去猜）。所以这一层
    // 的判据是「有没有工作区根**覆盖**这个绑定的固定目录」，而 `fixed_dir`
    // 是绝对路径时它自己就是一个可用的工作区根（`git worktree add` 就在它下面）。
    if gate_six_code(task, binding) {
        return Some(Rejection::NoCode);
    }

    // ⑦ 该绑定已有任务在跑（默认串行，§3.3）。
    if let Some(b) = binding {
        if (routing.busy)(&b.id) {
            return Some(Rejection::BindingBusy);
        }
    }

    None
}

/// ⓪：任务级 `allowed_devices`。
///
/// # 为什么这里**只有设备名单**
///
/// ⓪ 这个序号在 `scheduling.md` §3 里写的是「任务级 `allowed_devices` /
/// `allowed_agents` / `required_capabilities`」三条，但**三条里只有一条是
/// 真正的 ⓪**：
///
/// - `allowed_devices` 是**标量约束**——「这台机器在不在名单里」不需要问设备的
///   任何能力，它问的是身份。它必须先于一切，否则「按负载挑最闲的那台再检查
///   名单」就把约束降级成了偏好。
/// - `allowed_agents` / `required_capabilities` 是**能力约束**。「这台设备有没有
///   codex」这个问题**就是** ② 的回答；把它们也在 ⓪ 里判一遍，② 和 ③ 就永远
///   走不到，那两个归因变成死代码，而 `task-model.md` §7.2 明说了「缺哪个能力」
///   要说成 `no_capable_device` 而不是别的。
///
/// 所以分工是：**⓪ 判身份（设备名单），②③ 判能力（agent / connector）**。
/// 「⓪ 先于 ①②③」这条规格仍然成立且可测——见
/// `layer_zero_runs_before_the_load_sort_has_any_say`。
fn gate_zero(task: &TaskConstraints, node_id: &str) -> Option<Rejection> {
    let raw = task.allowed_devices.as_deref()?;
    let allowed = parse_name_list(Some(raw));
    // 空名单不等于不限：库里的 `""` 是「限了但一个都没写」，那不该放行任何设备。
    if !allowed.contains(&node_id.to_ascii_lowercase()) {
        return Some(Rejection::NoAllowedDevice);
    }
    None
}

/// ②：任务要求的 agent，这台设备**现在可用**的有没有。
///
/// 「要求的 agent」= `allowed_agents` ∪ `required_capabilities` 里的
/// `agent:<id>` 项。两个来源都算：前者是「只许用这些」，后者是「必须能跑它」，
/// 从「这台机器能不能干活」的角度看是同一件事。
///
/// 名字后面可以带 `@<version>` 后缀（`agent:opencode@2.1.4`）：那就是 ⓺ 的
/// 「版本匹配」亲和键。**这一层只比名字**——版本不匹配不该把设备排除（那会把
/// 亲和键变成一个硬门槛），它只影响排序（`agent_affinity`）。
///
/// 空要求 = 不拦（任务没说要哪个 agent，那就不该按 agent 排除）。
fn has_required_agents(task: &TaskConstraints, report: &Report) -> bool {
    let required: BTreeSet<String> = task
        .allowed_agents()
        .into_iter()
        .chain(
            task.required_capabilities()
                .into_iter()
                .filter_map(|c| c.strip_prefix("agent:").map(str::to_string)),
        )
        .map(|spec| agent_spec_name(&spec))
        .collect();
    if required.is_empty() {
        return true;
    }
    let available = available_agents(report);
    required.iter().any(|want| available.contains(want))
}

/// `opencode@2.1.4` → `opencode`（小写）。
fn agent_spec_name(spec: &str) -> String {
    match spec.split_once('@') {
        Some((name, _)) => name.trim().to_ascii_lowercase(),
        None => spec.trim().to_ascii_lowercase(),
    }
}

/// ③：任务要求的 connector / 能力，这台设备**报了**的有几个。
///
/// `agent:<id>` 项跳过（归 ②）。空要求 = 不拦。
fn has_required_capabilities(task: &TaskConstraints, report: &Report) -> bool {
    task.required_capabilities()
        .iter()
        .filter(|c| !c.starts_with("agent:"))
        .all(|want| capability_satisfied(report, want))
}

/// 单项能力的判定（**只判 ③ 的 connector / arch / os 三类**）。
///
/// 认这几种写法（都是「设备自己上报过」的事实，不是本模块编的）：
///
/// - 裸名字 → 必须是 `capabilities.connectors` 里的一项（`git` / `terminal` / …）
/// - `connector:<name>` → 同上，显式写法
/// - `arch:<a>` / `os:<o>` → `capabilities.arch` / `capabilities.os` 相等
///
/// **`agent:<id>` 不在这里判**：它归 ②（`has_required_agents`）。两处都判会让
/// 「缺 agent」可能被报成 `no_connector`，而 `task-model.md` §7.2 要求
/// 「缺哪个能力」这个归因对得上。本函数对 `agent:` 前缀一律返回 `true`
/// （它不在本层的射程里，不是「不满足」）。
///
/// 认不出的写法**判为不满足**（宁严勿松）：把拼错的 `requried:git` 当成通过，
/// 会让一条约束静默失效，而症状是「任务派到了一台没装 git 的机器」。
fn capability_satisfied(report: &Report, want: &str) -> bool {
    let caps = &report.capabilities;
    if want.starts_with("agent:") {
        return true; // 归 ②，见上。
    }
    if let Some(name) = want.strip_prefix("connector:") {
        return reported_connectors(report).contains(name);
    }
    if let Some(arch) = want.strip_prefix("arch:") {
        return caps.get("arch").and_then(serde_json::Value::as_str) == Some(arch);
    }
    if let Some(os) = want.strip_prefix("os:") {
        return caps.get("os").and_then(serde_json::Value::as_str) == Some(os);
    }
    reported_connectors(report).contains(want)
}

/// `capabilities.connectors` 的集合（小写）。
///
/// **键缺失 = 空集合**。`hub-server` 的 `unreported_connector` 在键缺失时
/// 「放行」（它回答的是另一件事：「设备没声明」不等于「设备不会」）。这里
/// **必须反过来**：`scheduling.md` §3 的立场是「能力不够的设备再闲也不能用」，
/// 而一份声明不出能力的心跳不足以证明它能跑。两台设备报表不一样时宁严勿松，
/// 因为派错的代价（`env_broken` 归因污染）比少派一次高。
fn reported_connectors(report: &Report) -> BTreeSet<String> {
    report
        .capabilities
        .get("connectors")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// **已装且可用**的 agent 名集合（小写）。认两种形态，与
/// `hub-server/src/routes/api.rs` 的 `available_agent_names` 同口径：
///
/// - M2 起：`[{"id","name","version","available","error"}]`，只收 `available: true`
/// - M1：裸字符串 `["opencode"]`，**不算可用**（只说名字不构成「能对话」的证据）
///
/// **没有「只要已装」的那个孪生函数**：本模块所有 agent 判定问的都是「现在能跑
/// 吗」（② 的硬过滤、⓺ 的版本亲和），而「装了但坏了」对这两件事都是否。多一个
/// `available_only: bool` 参数只会让调用方有机会传错那个 `false`。
fn available_agents(report: &Report) -> BTreeSet<String> {
    let Some(arr) = report
        .capabilities
        .get("agents")
        .and_then(serde_json::Value::as_array)
    else {
        return BTreeSet::new();
    };
    arr.iter()
        .filter_map(|v| {
            // M1 形态：裸字符串只说明「装了」，没有「可用」的证据 → 不收。
            let o = v.as_object()?;
            if o.get("available").and_then(serde_json::Value::as_bool) != Some(true) {
                return None;
            }
            o.get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| o.get("id").and_then(serde_json::Value::as_str))
                .map(|s| s.trim().to_ascii_lowercase())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// ⑥：拿不到代码吗？
///
/// # 这一层当前的判据
///
/// 真规格（`scheduling.md` §3.1）问的是「这台设备能不能拿到代码」——本地没有
/// 就 `fetch`，连不上 origin 由 Hub 去拉。本期**设备的 `workspace_roots` 恒为空**
/// （见 `node-core/src/heartbeat.rs` 的 `send_heartbeat`），所以：
///
/// - 有绑定且 `fixed_dir` 是绝对路径 → **能拿到**（工作树就建在它下面）；
/// - 有绑定但 `fixed_dir` 不是绝对路径 → **拿不到**（`Binding.fixed_dir` 的口径是
///   「不展开 `~`」，Hub 没有那台设备的文件系统，`git worktree add` 要绝对路径——
///   见 `node-core/src/worktree.rs` 的 `prepare` 对 `NotAbsolute` 的判定）；
/// - 没绑定 → 拿不到（没有地方放工作树）。
///
/// 一旦设备开始上报 `workspace_roots`，这里就该改成「固定目录被某个根覆盖」。
/// 留这条注释是为了让下一个人知道**现在为什么是这个判据**，而不是以为它漏了。
fn gate_six_code(_task: &TaskConstraints, binding: Option<&Binding>) -> bool {
    match binding {
        Some(b) => !b.fixed_dir.starts_with('/'),
        None => true,
    }
}

// ---------------------------------------------------------------------------
// 负载读数：心跳 `resources`
// ---------------------------------------------------------------------------

/// 原始 `load_per_core`（浮点）。**打分优先用它**。
///
/// # 为什么不用 `load_percent`
///
/// `load_percent` 有 `min(100)` 截断（`api.rs` 与 `scheduling.md` §2 都写明）。
/// 两台**都满载**的设备用截断值恒读同一个数，分不出高下——那是真实的信息损失：
/// 它们的负载可能差一倍。
///
/// 两处来源，依次尝试：
///
/// 1. `resources.load.load_per_core`——`scheduling.md` §2 的目标形状，原始浮点；
/// 2. `resources.cpu_percent`——M2 实际形状，等于 `load_per_core × 100`
///    （`node-core/src/heartbeat.rs` 的 `collect_resources` 这样算，四舍五入成
///    整数），所以除回 100 能拿回千分位精度。**它比截断百分比好得多**，但仍是
///    近似（原值已被取整过一次）。
///
/// 两处都取不到才退回 [`load_score`] 的截断百分比——那里会退化，见那句注释。
fn raw_load_per_core(report: &Report) -> Option<f64> {
    let obj = report.resources.as_object()?;
    if let Some(v) = obj
        .get("load")
        .and_then(|l| l.get("load_per_core"))
        .and_then(serde_json::Value::as_f64)
    {
        if v.is_finite() && v >= 0.0 {
            return Some(v);
        }
    }
    // 不认顶层 `load` 当作 per-core 数值：M2 的 `resources.load` 是**对象**
    // （`{load1, running}`），而 `load_percent` 的候选键里有 `load` 是为了兼容
    // 更早的扁平上报。真拿到一个数字时它是绝对值，除核数需要核数，而核数可能
    // 在另一处缺失——算了会得到一个假的 per-core。
    if let Some(pct) = obj.get("cpu_percent").and_then(serde_json::Value::as_f64) {
        if pct.is_finite() && pct >= 0.0 {
            return Some(pct / 100.0);
        }
    }
    None
}

/// 负载分（**越低越好**）：有原始 `load_per_core` 就用它，否则退回截断百分比。
///
/// # 退路会退化（必须写明）
///
/// 退路读 `load_percent`，而它有 `min(100)` 截断（与 `api.rs` 的 `load_percent`
/// 同口径）。**两台满载设备在这个数上恒相等，排序键失效**——此时下一键（剩余
/// 内存）会接管，而不是「假装负载分出了高下」。退路只在两个读数都缺时取
/// `u64::MAX`：「没上报负载」既不等于「空着」也不等于「满载」，让它排在最后是
/// 唯一不会把未上报当成事实的选择（`api.rs` 的同一句警告：「取不到就是 `None`，
/// 不回落成 0」）。
fn load_score(report: &Report) -> f64 {
    if let Some(raw) = raw_load_per_core(report) {
        return raw;
    }
    match load_percent(&report.resources, CPU_PERCENT_FIELDS) {
        // 退路：截断到 100 的值。**它分不出两台满载设备的差别。**
        Some(pct) => pct as f64,
        None => f64::MAX,
    }
}

/// 剩余内存（MB）。两处来源，**优先扁平字段**（M2 实际形状），退到嵌套明细。
///
/// `resources.memory.available_mb` 是**可用**量而不是总量：排的是「现在还剩多少
/// 能立刻用」，总量跟「现在忙不忙」无关（`scheduling.md` §2 的 `mem_percent`
/// 为什么用 `MemAvailable` 是同一个理由）。
fn available_mem_mb(report: &Report) -> Option<u64> {
    let obj = report.resources.as_object()?;
    if let Some(v) = obj
        .get("memory")
        .and_then(|m| m.get("available_mb"))
        .and_then(serde_json::Value::as_u64)
    {
        return Some(v);
    }
    // 只有百分比时按「100% − 已用」反推不到字节数（没有 total_mb 就不行）。
    let _ = load_percent(&report.resources, MEM_PERCENT_FIELDS);
    None
}

/// 剩余磁盘（MB）。`resources.disk.workspace_free_mb`（`scheduling.md` §2 的
/// 目标形状；`hub-server/tests/heartbeat_flow.rs` 的样例心跳用的也是它）。
///
/// 认 `disk.workspace_free_mb` 与扁平的 `workspace_free_mb` 两种写法：前者是
/// 目标形状，后者是「扁平上报」那条既有取舍的延续。
fn free_disk_mb(report: &Report) -> Option<u64> {
    let obj = report.resources.as_object()?;
    if let Some(v) = obj
        .get("disk")
        .and_then(|d| d.get("workspace_free_mb"))
        .and_then(serde_json::Value::as_u64)
    {
        return Some(v);
    }
    obj.get("workspace_free_mb")
        .and_then(serde_json::Value::as_u64)
}

/// ⑤ 用的磁盘余量（字节）。**设备实测与绑定配额取更紧的那个**——
/// `scheduling.md` §3.2 说磁盘是两件事：配额回答「允许多大」，余量回答
/// 「跑不跑得动」。两者任一不够都不该派。
///
/// 两者都取不到时返回 `None`（= 不因磁盘原因排除）。这是刻意的：**没有数据
/// 不等于「空间不够」**，把「未上报」判成满盘会得到「所有设备都不可用」，
/// 而真该做的是让设备上报（`scheduling.md` §3.2 末段那张处置表：
/// 「磁盘不够」与「设备离线」是两种完全不同的下一步动作）。
fn free_disk_bytes(report: &Report, binding: Option<&Binding>) -> Option<u64> {
    let measured = free_disk_mb(report).map(|mb| mb.saturating_mul(1024 * 1024));
    let quota = binding
        .and_then(|b| b.disk_quota_bytes)
        // 负配额是坏数据：当「没设」处理，不当「零空间」——后者会把一台好机器
        // 永久判死，而真相是那一列写错了。
        .and_then(|q| u64::try_from(q).ok());
    match (measured, quota) {
        (Some(m), Some(q)) => Some(m.min(q)),
        (Some(m), None) => Some(m),
        (None, Some(q)) => Some(q),
        (None, None) => None,
    }
}

/// 剩余磁盘的排序键（**越多越好**，所以比较时取反方向）。
///
/// 取不到时用 0：它在排序链上排在「剩余内存」之后，是**最弱的信号**
/// （`scheduling.md` §4.1），缺数据时让它不占优即可，不必给一个编出来的大数。
fn disk_score(report: &Report, binding: Option<&Binding>) -> u64 {
    free_disk_bytes(report, binding).unwrap_or(0)
}

/// 亲和键 ⓹：这台设备在任务要求的那些 workspace root 下**已经有**工作区根报了
/// 过来吗？
///
/// 「已在该节点有工作区 → 优先」的字面意思是「省一次 clone」（`scheduling.md`
/// §4.1）。设备上报的 `workspace_roots` 是「这台机器上有哪些根可以路由」，
/// 而绑定的 `fixed_dir` 就落在其中一个下面时，那台机器上已经有一个能用的
/// 工作目录——不必从零建。
///
/// 本期设备**不报** `workspace_roots`（见 [`gate_six_code`]），所以这个键在
/// 生产上恒为 `false`，不影响选择；它在心跳开始上报之后自动生效。
fn workspace_affinity(node_id: &str, report: &Report, binding: Option<&Binding>) -> bool {
    let _ = node_id;
    let Some(b) = binding else {
        return false;
    };
    let dir = b.fixed_dir.as_str();
    report
        .workspace_roots
        .iter()
        .any(|root| dir == root.as_str() || (root != "/" && dir.starts_with(root.as_str())))
}

/// 亲和键 ⓺：已装目标 agent **且版本匹配** → 优先。
///
/// 「版本匹配」的判据：任务在 `required_capabilities` 里写了 `agent:<id>@<ver>`
/// 时，设备上那个 agent 的版本必须与它相等。**没写版本就没有可比的**——这时
/// 键为 `false`（不是 `true`）：把「没要求版本」当成「版本匹配」会让这个键在
/// 所有设备上恒真，那它就不该存在。
fn agent_affinity(task: &TaskConstraints, report: &Report) -> bool {
    let wanted: Vec<(String, String)> = task
        .required_capabilities()
        .into_iter()
        .filter_map(|c| {
            let rest = c.strip_prefix("agent:")?;
            let (id, ver) = rest.split_once('@')?;
            Some((id.trim().to_ascii_lowercase(), ver.trim().to_string()))
        })
        .collect();
    if wanted.is_empty() {
        return false;
    }
    let installed = agent_versions(report);
    wanted.iter().any(|(id, ver)| {
        installed
            .get(id)
            .is_some_and(|got| got.as_str() == ver.as_str())
    })
}

/// `agent id → 版本`（已装且可用、报了版本的那些）。
fn agent_versions(report: &Report) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let Some(arr) = report
        .capabilities
        .get("agents")
        .and_then(serde_json::Value::as_array)
    else {
        return out;
    };
    for v in arr {
        let Some(o) = v.as_object() else { continue };
        if o.get("available").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }
        let Some(id) = o
            .get("name")
            .and_then(serde_json::Value::as_str)
            .or_else(|| o.get("id").and_then(serde_json::Value::as_str))
        else {
            continue;
        };
        if let Some(ver) = o.get("version").and_then(serde_json::Value::as_str) {
            out.insert(id.trim().to_ascii_lowercase(), ver.trim().to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 有序比较链
// ---------------------------------------------------------------------------

/// 两台**已通过全部过滤**的候选的比较。返回 `Less` 表示 `a` 更好。
///
/// 键的顺序就是 `scheduling.md` §4.1 的顺序，**每一键只在相等时才看下一键**。
/// 这是字典序比较链，不是加权评分（§4.3）。
fn compare_candidates(
    task: &TaskConstraints,
    routing: &Routing<'_>,
    a: &(&str, &Report),
    b: &(&str, &Report),
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let (a_id, a_rep) = *a;
    let (b_id, b_rep) = *b;
    let a_bind = routing.binding_of(a_id);
    let b_bind = routing.binding_of(b_id);

    // 2. load_per_core 越低越好。用 `total_cmp`：`f64` 的 `partial_cmp` 在
    // NaN 上返回 `None`，而「比较不出来」被当成相等会把负载键静默丢掉。
    // 上游已滤掉 NaN（`is_finite`），这里是兜底而不是判断。
    let by_load = load_score(a_rep).total_cmp(&load_score(b_rep));
    if by_load != Ordering::Equal {
        return by_load;
    }

    // 3. 剩余内存越多越好。
    let by_mem = available_mem_mb(b_rep).cmp(&available_mem_mb(a_rep));
    if by_mem != Ordering::Equal {
        return by_mem;
    }

    // 4. 剩余磁盘充裕度越多越好（都过了 ⑤ 的硬门槛之后，它是**最弱的信号**）。
    let by_disk = disk_score(b_rep, b_bind).cmp(&disk_score(a_rep, a_bind));
    if by_disk != Ordering::Equal {
        return by_disk;
    }

    // 5. 已在该节点有工作区 → 优先（`true` 更好，所以反着比）。
    let by_ws =
        workspace_affinity(b_id, b_rep, b_bind).cmp(&workspace_affinity(a_id, a_rep, a_bind));
    if by_ws != Ordering::Equal {
        return by_ws;
    }

    // 6. 已装目标 agent 且版本匹配 → 优先。
    let by_agent = agent_affinity(task, b_rep).cmp(&agent_affinity(task, a_rep));
    if by_agent != Ordering::Equal {
        return by_agent;
    }

    // 7. node_id 字典序：**稳定的 tiebreak**，让结果可预测（§4.1）。
    a_id.cmp(b_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_storage::binding::CleanupPolicy;
    use std::time::Instant;

    // -----------------------------------------------------------------
    // 夹具
    // -----------------------------------------------------------------

    /// 一台「什么条件都满足」的设备：有 git、有 opencode、空闲、内存与磁盘充裕、
    /// 无负载。
    fn good_report() -> Report {
        Report {
            node_version: "0.8.2".into(),
            capabilities: serde_json::json!({
                "connectors": ["git", "terminal", "files", "exec", "worktree"],
                "agents": [
                    {"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true}
                ],
                "os": "linux",
                "arch": "x86_64",
            }),
            resources: serde_json::json!({
                "cpu": {"cores": 4},
                "cpu_percent": 10,
                "load": {"load1": 0.4, "load_per_core": 0.1},
                "memory": {"total_mb": 8192, "available_mb": 4096},
                "disk": {"workspace_free_mb": 102400},
            }),
            workspace_roots: Vec::new(),
            at: Instant::now(),
        }
    }

    fn binding_for(device_id: &str) -> Binding {
        Binding {
            id: format!("b_{device_id}"),
            project_id: "p_1".into(),
            device_id: device_id.into(),
            fixed_dir: format!("/w/{device_id}"),
            disk_quota_bytes: None,
            cleanup_policy: CleanupPolicy::Balanced,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    /// 「每台设备都绑定了同一项目」的绑定表。
    fn bindings_for(ids: &[&str]) -> Vec<(String, Binding)> {
        ids.iter()
            .map(|id| ((*id).to_string(), binding_for(id)))
            .collect()
    }

    /// 不带任务的约束（什么都不限）。
    fn open() -> TaskConstraints {
        TaskConstraints::default()
    }

    fn route<'a>(
        task: &TaskConstraints,
        nodes: &'a [(&'a str, Report)],
        bindings: &'a [(String, Binding)],
    ) -> Result<&'a str, Rejection> {
        let routing = Routing::bare(ProjectShape::Central, bindings);
        let iter = nodes.iter().map(|(id, r)| (*id, r));
        pick(task, iter, &routing)
    }

    /// 单台的便捷版：一台设备 + 它自己的绑定。
    fn route_one(
        node_id: &str,
        report: &Report,
        bindings: &[(String, Binding)],
    ) -> Result<String, Rejection> {
        let nodes = [(node_id, report.clone())];
        let routing = Routing::bare(ProjectShape::Central, bindings);
        pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing).map(str::to_string)
    }

    // -----------------------------------------------------------------
    // 每一层过滤单独把唯一合格设备排除（8 个用例，一层一条）
    // -----------------------------------------------------------------

    /// ⓪：`allowed_devices` 不包含这台唯一设备。
    #[test]
    fn layer_zero_rejects_a_device_that_is_not_in_allowed_devices() {
        let binds = bindings_for(&["n_1"]);
        let mut task = open();
        task.allowed_devices = Some(r#"["n_2"]"#.into());
        let nodes = [("n_1", good_report())];
        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::NoAllowedDevice)
        );
    }

    /// ①：隔离式项目，这台设备没绑定。
    #[test]
    fn layer_one_rejects_an_unbound_device_in_an_isolated_project() {
        let binds = bindings_for(&["n_other"]); // 绑定在**别人**身上
        let report = good_report();
        let nodes = [("n_1", report)];
        let routing = Routing::bare(ProjectShape::Isolated, &binds);
        let got = pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing);
        assert_eq!(got, Err(Rejection::IsolationBlocked));
    }

    /// ②：任务点名要 codex，这台设备一个可用 agent 都没有（只有装坏了的 opencode）。
    #[test]
    fn layer_two_rejects_a_device_without_the_required_agent() {
        let binds = bindings_for(&["n_1"]);
        let mut task = open();
        task.required_capabilities = Some(r#"["agent:codex"]"#.into());
        let mut report = good_report();
        report.capabilities = serde_json::json!({
            "connectors": ["git"],
            "agents": [{"id": "opencode", "name": "opencode", "version": "2.1.4", "available": false}],
        });
        let nodes = [("n_1", report)];
        assert_eq!(route(&task, &nodes, &binds), Err(Rejection::NoAgent));
    }

    /// ③：任务要 git connector，这台设备没报。
    #[test]
    fn layer_three_rejects_a_device_without_the_required_connector() {
        let binds = bindings_for(&["n_1"]);
        let mut task = open();
        task.required_capabilities = Some(r#"["git"]"#.into());
        let mut report = good_report();
        report.capabilities = serde_json::json!({
            "connectors": ["terminal"],  // 没有 git
            "agents": [{"id": "opencode", "available": true}],
        });
        let nodes = [("n_1", report)];
        assert_eq!(route(&task, &nodes, &binds), Err(Rejection::NoConnector));
    }

    /// ④：任务要 8 GiB 内存，设备只剩 512 MiB。
    #[test]
    fn layer_four_rejects_a_device_without_enough_memory() {
        let binds = bindings_for(&["n_1"]);
        let mut task = open();
        task.required_ram_mb = Some(8192);
        let mut report = good_report();
        report.resources["memory"] = serde_json::json!({"total_mb": 8192, "available_mb": 512});
        let nodes = [("n_1", report)];
        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::RamInsufficient)
        );
    }

    /// ⑤：设备只剩 100 MiB 磁盘。
    #[test]
    fn layer_five_rejects_a_device_without_enough_disk() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        report.resources["disk"] = serde_json::json!({"workspace_free_mb": 100});
        let nodes = [("n_1", report)];
        assert_eq!(
            route(&open(), &nodes, &binds),
            Err(Rejection::DiskInsufficient)
        );
    }

    /// ⑥：绑定的 `fixed_dir` 不是绝对路径 → 拿不到代码。
    #[test]
    fn layer_six_rejects_a_binding_that_cannot_host_a_worktree() {
        let mut b = binding_for("n_1");
        b.fixed_dir = "~/code/proj".into(); // 不展开 `~`，Hub 不知道它在哪
        let binds = vec![("n_1".to_string(), b)];
        let report = good_report();
        let nodes = [("n_1", report)];
        assert_eq!(route(&open(), &nodes, &binds), Err(Rejection::NoCode));
    }

    /// ⑦：该绑定上已经有一个任务在跑。
    #[test]
    fn layer_seven_rejects_a_binding_that_is_already_running_a_task() {
        let binds = bindings_for(&["n_1"]);
        let report = good_report();
        let nodes = [("n_1", report)];
        let busy = |_: &str| true;
        let routing = Routing::new(ProjectShape::Central, &binds, &busy);
        let got = pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing);
        assert_eq!(got, Err(Rejection::BindingBusy));
    }

    // -----------------------------------------------------------------
    // ⓪ 必须先于 ①②③
    // -----------------------------------------------------------------

    /// (a) 按负载最闲的那台**不在 allowed 里** → 报 ⓪，**不是**「选了别人」。
    ///
    /// 反命题（错实现会红的样子）：先按负载排，挑到 `n_idle`，再发现它不在
    /// allowed 里，于是退回 `n_allowed` 并**返回它**。那样 `allowed_*` 就从
    /// 约束降级成了偏好。
    #[test]
    fn layer_zero_runs_before_the_load_sort_has_any_say() {
        let binds = bindings_for(&["n_allowed", "n_idle"]);

        // `n_idle` 最闲，但不在 allowed 里；`n_allowed` 满载。
        let mut idle = good_report();
        idle.resources["cpu_percent"] = serde_json::json!(0);
        idle.resources["load"] = serde_json::json!({"load_per_core": 0.0});
        let mut allowed = good_report();
        allowed.resources["cpu_percent"] = serde_json::json!(99);
        allowed.resources["load"] = serde_json::json!({"load_per_core": 0.99});

        let nodes = [("n_allowed", allowed), ("n_idle", idle)];
        let mut task = open();
        task.allowed_devices = Some(r#"["n_allowed"]"#.into());

        // `n_allowed` 能过全部过滤 → 选它。这一步证明夹具本身是好的
        // （「按 allowed 选到了那台」），下一条才是真判据。
        assert_eq!(route(&task, &nodes, &binds), Ok("n_allowed"));

        // 现在把 `n_allowed` 也挪出 allowed 名单 → 一台都不合格 → 必须是 ⓪。
        task.allowed_devices = Some(r#"["n_elsewhere"]"#.into());
        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::NoAllowedDevice),
            "allowed 里一台都没有时必须报 ⓪，不能退回去挑最闲的那台"
        );
    }

    /// (b) allowed 里有一台**能力不足** → 走的是 ②③，**不是** ⓪。
    ///
    /// 这条钉的是「⓪ 不越权」：`allowed_devices` 满足了就不该再被 ⓪ 拒，
    /// 于是拒绝必须来自能力那一层。错实现（把能力也塞进 ⓪）会红成
    /// `NoAllowedDevice`。
    #[test]
    fn an_allowed_device_that_lacks_an_agent_is_rejected_by_layer_two_not_zero() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        report.capabilities = serde_json::json!({
            "connectors": ["git"],
            "agents": [],  // 一个可用 agent 都没有
        });
        let nodes = [("n_1", report)];
        let mut task = open();
        task.allowed_devices = Some(r#"["n_1"]"#.into());
        task.allowed_agents = Some(r#"["opencode"]"#.into());

        let got = route(&task, &nodes, &binds);
        assert_eq!(
            got,
            Err(Rejection::NoAgent),
            "设备在 allowed 里，拒它的必须是 ②（缺 agent），不是 ⓪"
        );
    }

    /// (b) 同上，connector 那半边：走 ③ 而不是 ⓪。
    #[test]
    fn an_allowed_device_that_lacks_a_connector_is_rejected_by_layer_three_not_zero() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        report.capabilities = serde_json::json!({
            "connectors": ["terminal"],
            "agents": [{"id": "opencode", "available": true}],
        });
        let nodes = [("n_1", report)];
        let mut task = open();
        task.allowed_devices = Some(r#"["n_1"]"#.into());
        task.required_capabilities = Some(r#"["git"]"#.into());

        assert_eq!(route(&task, &nodes, &binds), Err(Rejection::NoConnector));
    }

    /// (c) **一台设备同时踩 ⓪ 与 ②③ 时，报的必须是 ⓪**——这是那条顺序唯一
    /// 可观测的判据。
    ///
    /// 为什么前面两条测不到它：⓪ 只看设备名单，②③ 只看能力，两组输入不相交，
    /// 于是把 ⓪ 挪到 ②③ 之后**结果一模一样**（变异实验实测：40 条全绿）。
    /// 只有让同一台设备两层都不过，返回值才取决于谁先跑。
    ///
    /// 错实现（把 ⓪ 挪到能力过滤之后）在这里会返回 `NoAgent` / `NoConnector`。
    #[test]
    fn layer_zero_is_reported_ahead_of_the_capability_layers_on_the_same_device() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        // 没装 codex、也没报 git → ② 与 ③ 都会拒。
        report.capabilities = serde_json::json!({
            "connectors": ["terminal"],
            "agents": [],
        });
        let nodes = [("n_1", report)];
        let mut task = open();
        // 同时不在 allowed 名单里 → ⓪ 也拒。
        task.allowed_devices = Some(r#"["n_elsewhere"]"#.into());
        task.allowed_agents = Some(r#"["codex"]"#.into());
        task.required_capabilities = Some(r#"["git"]"#.into());

        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::NoAllowedDevice),
            "同一台设备同时踩 ⓪ 与 ②③ 时，⓪ 必须先跑"
        );
    }

    // -----------------------------------------------------------------
    // 排序键逐键验证（6 个用例，每例只让一个键不同）
    // -----------------------------------------------------------------

    /// 排序键 2：负载 —— 越低越好。
    #[test]
    fn the_load_key_prefers_the_lower_load_per_core() {
        let binds = bindings_for(&["n_busy", "n_idle"]);
        let mut busy = good_report();
        busy.resources["load"] = serde_json::json!({"load_per_core": 0.9});
        let mut idle = good_report();
        idle.resources["load"] = serde_json::json!({"load_per_core": 0.1});
        // 其余键全同：内存、磁盘、亲和、tiebreak 都一样，只有负载不同。
        // **让字典序站在 n_busy 那边**（`n_busy` < `n_idle`），这样如果负载键
        // 没起作用，tiebreak 会选 `n_busy` 而红掉。
        let nodes = [("n_busy", busy), ("n_idle", idle)];
        assert_eq!(route(&open(), &nodes, &binds), Ok("n_idle"));
    }

    /// 排序键 3：剩余内存 —— 越多越好。负载相同。
    #[test]
    fn the_memory_key_prefers_the_node_with_more_available_memory() {
        let binds = bindings_for(&["n_low", "n_high"]);
        let mut low = good_report();
        low.resources["memory"] = serde_json::json!({"total_mb": 8192, "available_mb": 1024});
        let mut high = good_report();
        high.resources["memory"] = serde_json::json!({"total_mb": 8192, "available_mb": 7168});
        // 字典序站在 n_low 那边，同「负载」那条的理由。
        let nodes = [("n_low", low), ("n_high", high)];
        assert_eq!(route(&open(), &nodes, &binds), Ok("n_high"));
    }

    /// 排序键 4：剩余磁盘充裕度 —— 越多越好。负载与内存相同。
    #[test]
    fn the_disk_key_prefers_the_node_with_more_free_disk() {
        let binds = bindings_for(&["n_tight", "n_roomy"]);
        let mut tight = good_report();
        tight.resources["disk"] = serde_json::json!({"workspace_free_mb": 2048});
        let mut roomy = good_report();
        roomy.resources["disk"] = serde_json::json!({"workspace_free_mb": 204800});
        let nodes = [("n_tight", tight), ("n_roomy", roomy)];
        assert_eq!(route(&open(), &nodes, &binds), Ok("n_roomy"));
    }

    /// 排序键 5：工作区亲和 —— 已有工作区的优先。前三键相同。
    #[test]
    fn the_workspace_affinity_key_prefers_the_node_that_already_has_the_root() {
        let binds = bindings_for(&["n_plain", "n_warm"]);
        let plain = good_report();
        let mut warm = good_report();
        // 绑定的固定目录是 `/w/n_warm`，它被报成工作区根 → 亲和成立。
        warm.workspace_roots = vec!["/w/n_warm".into()];
        let nodes = [("n_plain", plain), ("n_warm", warm)];
        assert_eq!(route(&open(), &nodes, &binds), Ok("n_warm"));
    }

    /// 排序键 6：agent 亲和 —— 已装目标 agent **且版本匹配**的优先。
    #[test]
    fn the_agent_affinity_key_prefers_the_matching_version() {
        let binds = bindings_for(&["n_old", "n_new"]);
        let mut old = good_report();
        old.capabilities["agents"] = serde_json::json!([
            {"id": "opencode", "name": "opencode", "version": "2.0.0", "available": true}
        ]);
        let new = good_report(); // opencode 2.1.4
        let mut task = open();
        task.required_capabilities = Some(r#"["agent:opencode@2.1.4"]"#.into());

        let nodes = [("n_old", old), ("n_new", new)];
        assert_eq!(route(&task, &nodes, &binds), Ok("n_new"));
    }

    /// 排序键 7：tiebreak —— 全同，取 `node_id` 字典序最小的。
    #[test]
    fn the_tiebreak_key_is_the_lexicographic_node_id() {
        let binds = bindings_for(&["n_b", "n_a", "n_c"]);
        let nodes = [
            ("n_b", good_report()),
            ("n_a", good_report()),
            ("n_c", good_report()),
        ];
        assert_eq!(route(&open(), &nodes, &binds), Ok("n_a"));
    }

    // -----------------------------------------------------------------
    // 磁盘硬门槛的边界
    // -----------------------------------------------------------------

    /// 剩余空间**恰等于**「基础开销 + 安全余量」→ 通过；少 1 字节 → 排除。
    #[test]
    fn the_disk_threshold_passes_on_the_boundary_and_fails_one_byte_below() {
        let binds = bindings_for(&["n_1"]);
        let floor = WORKTREE_BASE_BYTES + DISK_SAFETY_MARGIN_BYTES;

        // 恰等于门槛 → 通过。用**字节**表达余量不方便，所以用 MB 写：
        // 门槛是 576 MiB，正好整除。
        let mut exact = good_report();
        exact.resources["disk"] = serde_json::json!({"workspace_free_mb": floor / (1024 * 1024)});
        assert_eq!(
            route_one("n_1", &exact, &binds),
            Ok("n_1".to_string()),
            "剩余空间恰等于「基础开销 + 安全余量」时必须通过"
        );

        // 少 1 字节：`workspace_free_mb` 是 MB 单位，所以用**绑定配额**表达
        // 字节级的差——两条口径都在同一条判据上。
        let mut one_byte_short = good_report();
        one_byte_short.resources = serde_json::json!({}); // 设备没报余量
        let mut b = binding_for("n_1");
        b.disk_quota_bytes = Some(i64::try_from(floor - 1).expect("fits in i64"));
        let binds2 = vec![("n_1".to_string(), b)];
        assert_eq!(
            route_one("n_1", &one_byte_short, &binds2),
            Err(Rejection::DiskInsufficient),
            "少 1 字节就必须排除"
        );

        // 证明配额与余量**取更紧的那个**：余量很足，但配额少 1 字节 → 排除。
        let mut rich = good_report();
        rich.resources["disk"] = serde_json::json!({"workspace_free_mb": 102400});
        assert_eq!(
            route_one("n_1", &rich, &binds2),
            Err(Rejection::DiskInsufficient),
            "配额比余量紧时必须按配额判"
        );
    }

    /// 任务的 `required_disk_mb` 抬高门槛，且**不会低于** `WORKTREE_BASE_BYTES`。
    #[test]
    fn the_task_can_raise_the_disk_floor_but_never_lower_it() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        report.resources["disk"] = serde_json::json!({"workspace_free_mb": 600});

        let mut task = open();
        task.required_disk_mb = Some(1); // 想写小 → 被抬回基础开销
        let nodes = [("n_1", report.clone())];
        assert_eq!(route(&task, &nodes, &binds), Ok("n_1"));

        task.required_disk_mb = Some(2048); // 2 GiB + 安全余量 > 600
        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::DiskInsufficient)
        );
    }

    // -----------------------------------------------------------------
    // 钉死
    // -----------------------------------------------------------------

    /// 钉死一台「磁盘不够」的设备 → **仍选中**（跳过全部过滤，§4.2）。
    #[test]
    fn a_pinned_device_is_chosen_even_when_it_fails_every_filter() {
        let binds = bindings_for(&["n_pinned", "n_good"]);
        let mut pinned = good_report();
        pinned.resources["disk"] = serde_json::json!({"workspace_free_mb": 1}); // 磁盘不够
        pinned.capabilities = serde_json::json!({}); // 也没有任何 agent/connector
        let good = good_report();

        let task = TaskConstraints::pinned("n_pinned");
        let nodes = [("n_good", good), ("n_pinned", pinned)];
        assert_eq!(
            route(&task, &nodes, &binds),
            Ok("n_pinned"),
            "钉死的设备跳过全部过滤与打分"
        );
    }

    /// 钉死的那台不在候选里 → ⓪，**不静默换成别人**。
    #[test]
    fn a_pinned_device_that_is_absent_is_not_silently_replaced() {
        let binds = bindings_for(&["n_1"]);
        let task = TaskConstraints::pinned("n_offline");
        let nodes = [("n_1", good_report())];
        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::NoAllowedDevice)
        );
    }

    /// 钉死设备时，同一个项目上的**绑定忙也不拦**——§4.2 的「跳过全部过滤」
    /// 把 ⑦ 也算在内（用户明确指定了那台机器）。
    #[test]
    fn a_pinned_device_ignores_the_busy_binding_too() {
        let binds = bindings_for(&["n_pinned"]);
        let busy = |_: &str| true;
        let routing = Routing::new(ProjectShape::Isolated, &binds, &busy);
        let report = good_report();
        let nodes = [("n_pinned", report)];
        let task = TaskConstraints::pinned("n_pinned");
        let got = pick(&task, nodes.iter().map(|(id, r)| (*id, r)), &routing);
        assert_eq!(got, Ok("n_pinned"));
    }

    // -----------------------------------------------------------------
    // Rejection 每个变体都有断言
    // -----------------------------------------------------------------

    /// `Rejection::ALL` 的每一个变体都能被**它自己那一层**触发出来。
    ///
    /// 这条不是「跑一遍 ALL 就算」，而是逐个构造输入并断言拿到的就是那个变体——
    /// 防将来加一层却忘了接线（那时会红），也防两个变体被折成同一个码。
    #[test]
    fn every_rejection_variant_is_reachable_by_its_own_layer() {
        let binds = bindings_for(&["n_1"]);

        // ⓪
        let mut t = open();
        t.allowed_devices = Some(r#"["n_other"]"#.into());
        assert_eq!(
            route(&t, &[("n_1", good_report())], &binds),
            Err(Rejection::NoAllowedDevice)
        );

        // ①：一台没有任何绑定的设备 + 隔离式项目。
        let mut unbound = good_report();
        unbound.resources["disk"] = serde_json::json!({"workspace_free_mb": 1});
        let empty: Vec<(String, Binding)> = Vec::new();
        let nodes = [("n_1", unbound)];
        let routing = Routing::bare(ProjectShape::Isolated, &empty);
        assert_eq!(
            pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing),
            Err(Rejection::IsolationBlocked)
        );

        // ②：不做成单独一条构造（它已被上面的 `layer_two_*` 覆盖）——
        // 但这条用例的契约是「ALL 里每一个都在下面出现」，所以仍逐个断言一遍。
        let mut t = open();
        t.required_capabilities = Some(r#"["agent:codex"]"#.into());
        assert_eq!(
            route(&t, &[("n_1", good_report())], &binds),
            Err(Rejection::NoAgent)
        );

        // ③
        let mut t = open();
        t.required_capabilities = Some(r#"["exec"]"#.into());
        let mut no_exec = good_report();
        no_exec.capabilities["connectors"] = serde_json::json!(["git"]);
        assert_eq!(
            route(&t, &[("n_1", no_exec)], &binds),
            Err(Rejection::NoConnector)
        );

        // ④
        let mut t = open();
        t.required_ram_mb = Some(65536);
        assert_eq!(
            route(&t, &[("n_1", good_report())], &binds),
            Err(Rejection::RamInsufficient)
        );

        // ⑤
        let mut tight = good_report();
        tight.resources["disk"] = serde_json::json!({"workspace_free_mb": 1});
        assert_eq!(
            route(&open(), &[("n_1", tight)], &binds),
            Err(Rejection::DiskInsufficient)
        );

        // ⑥：绑定缺失。
        let empty: Vec<(String, Binding)> = Vec::new();
        assert_eq!(
            route(&open(), &[("n_1", good_report())], &empty),
            Err(Rejection::NoCode)
        );

        // ⑦
        let busy = |_: &str| true;
        let routing = Routing::new(ProjectShape::Central, &binds, &busy);
        let nodes = [("n_1", good_report())];
        assert_eq!(
            pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing),
            Err(Rejection::BindingBusy)
        );

        // ALL 的每一条都在上面出现过（防加了变体忘了接线）。
        for r in Rejection::ALL {
            assert!(!r.as_str().is_empty(), "每个变体都要有线上码");
        }
        assert_eq!(Rejection::ALL.len(), 8, "八层过滤，八个归因");
    }

    /// 线上码逐字写死（`task-model.md` §7.2 的原文）。
    ///
    /// **不靠 `as_str()` 自比**：那样改一个字符串，测试跟着改，永远绿。
    #[test]
    fn the_rejection_codes_are_written_out_literally() {
        assert_eq!(Rejection::NoAllowedDevice.as_str(), "no_allowed_device");
        assert_eq!(Rejection::IsolationBlocked.as_str(), "isolation_blocked");
        assert_eq!(Rejection::NoAgent.as_str(), "no_agent");
        assert_eq!(Rejection::NoConnector.as_str(), "no_connector");
        assert_eq!(Rejection::RamInsufficient.as_str(), "ram_insufficient");
        assert_eq!(Rejection::DiskInsufficient.as_str(), "disk_insufficient");
        assert_eq!(Rejection::NoCode.as_str(), "no_code");
        assert_eq!(Rejection::BindingBusy.as_str(), "binding_busy");
    }

    /// 一台设备同时踩多层 → 报**最靠前的那一层**。
    #[test]
    fn the_earliest_failing_layer_wins_when_a_device_breaks_several() {
        let binds = bindings_for(&["n_1"]);
        // 内存不够（④）+ 磁盘不够（⑤）。④ 在前。
        let mut report = good_report();
        report.resources["memory"] = serde_json::json!({"available_mb": 1});
        report.resources["disk"] = serde_json::json!({"workspace_free_mb": 1});
        let mut task = open();
        task.required_ram_mb = Some(8192);
        assert_eq!(
            route(&task, &[("n_1", report)], &binds),
            Err(Rejection::RamInsufficient)
        );
    }

    // -----------------------------------------------------------------
    // 逆命题：排序后的第一名 ≠ 迭代顺序的第一名
    // -----------------------------------------------------------------

    /// 迭代顺序的第一台**不是**排序第一名时，返回的必须是排序第一名。
    ///
    /// 这条挡的是「直接取 `nodes.into_iter().next()`」那种错实现：
    /// 它在前面的用例里可能全绿，因为夹具碰巧把好的排在前面。
    #[test]
    fn the_worst_first_input_still_returns_the_sorted_first() {
        let binds = bindings_for(&["n_first", "n_best"]);
        let mut first = good_report();
        first.resources["load"] = serde_json::json!({"load_per_core": 0.95});
        let mut best = good_report();
        best.resources["load"] = serde_json::json!({"load_per_core": 0.05});

        // 迭代顺序：最差的在最前面。
        let nodes = [("n_first", first), ("n_best", best)];
        let got = route(&open(), &nodes, &binds);
        assert_eq!(
            got,
            Ok("n_best"),
            "返回的必须是排序后的第一名，不是迭代顺序的第一名"
        );
        assert_ne!(
            got,
            Ok(nodes[0].0),
            "如果拿的是第一台，这条会红——它钉的正是那个错实现"
        );
    }

    // -----------------------------------------------------------------
    // 负载读数：截断陷阱
    // -----------------------------------------------------------------

    /// 两台都满载（截断值都是 100）时，**原始 `load_per_core` 仍能分出高下**。
    #[test]
    fn the_raw_load_per_core_still_separates_two_saturated_devices() {
        let binds = bindings_for(&["n_a", "n_b"]);
        // 两台 `cpu_percent` 都是 100（截断后完全一样），但原始 per-core 不同。
        let mut a = good_report();
        a.resources["cpu_percent"] = serde_json::json!(100);
        a.resources["load"] = serde_json::json!({"load_per_core": 1.0});
        let mut b = good_report();
        b.resources["cpu_percent"] = serde_json::json!(100);
        b.resources["load"] = serde_json::json!({"load_per_core": 2.0});

        let nodes = [("n_a", a), ("n_b", b)];
        assert_eq!(
            route(&open(), &nodes, &binds),
            Ok("n_a"),
            "原始 load_per_core 取得到就该用它，截断的 100/100 分不出高下"
        );
    }

    /// 退回截断百分比时**确实会退化**：两台满载设备在这一键上相等，由下一键
    /// （剩余内存）接管。这条把退化行为钉成事实，而不是假装它能分高下。
    #[test]
    fn the_truncated_fallback_cannot_separate_two_saturated_devices() {
        let binds = bindings_for(&["n_a", "n_b"]);
        // 没有 `load_per_core`，只有扁平 `cpu_percent` = 100（两台都满载）。
        let mut a = good_report();
        a.resources = serde_json::json!({
            "cpu_percent": 100,
            "memory": {"available_mb": 1024},
            "disk": {"workspace_free_mb": 102400},
        });
        let mut b = good_report();
        b.resources = serde_json::json!({
            "cpu_percent": 100,
            "memory": {"available_mb": 7168},
            "disk": {"workspace_free_mb": 102400},
        });

        let nodes = [("n_a", a), ("n_b", b)];
        // `cpu_percent` 退路（100/100 相等）→ 内存键接管 → 内存多的赢。
        // 字典序是 n_a 在前，所以「内存键没生效」会红。
        assert_eq!(
            route(&open(), &nodes, &binds),
            Ok("n_b"),
            "截断值相等时下一键必须接管"
        );
    }

    /// 完全没报负载的设备排在最后（`load_score` 的 `f64::MAX`），
    /// 而不是被当成「0 = 最闲」。
    #[test]
    fn a_device_that_reports_no_load_is_not_treated_as_idle() {
        let binds = bindings_for(&["n_silent", "n_loaded"]);
        let mut silent = good_report();
        silent.resources = serde_json::json!({
            "memory": {"available_mb": 4096},
            "disk": {"workspace_free_mb": 102400},
        });
        let mut loaded = good_report();
        loaded.resources["cpu_percent"] = serde_json::json!(90);
        loaded.resources["load"] = serde_json::json!({"load_per_core": 0.9});

        let nodes = [("n_silent", silent), ("n_loaded", loaded)];
        assert_eq!(
            route(&open(), &nodes, &binds),
            Ok("n_loaded"),
            "没上报负载不等于空闲；它排在最后"
        );
    }

    // -----------------------------------------------------------------
    // 口径与 api.rs 一致
    // -----------------------------------------------------------------

    /// 字段名列表与 `hub-server/src/routes/api.rs` 逐字相同。
    ///
    /// 这条用例的**输入是写死的字符串**（不是读 api.rs），因为 `hub-core` 不能
    /// 依赖 `hub-server`。它的作用是在这边被人「顺手改一下字段名」时变红——
    /// 改的人会读到这条断言，从而去核对 api.rs 那一侧。
    #[test]
    fn the_load_field_names_match_the_api_side() {
        assert_eq!(CPU_PERCENT_FIELDS, &["cpu", "cpu_percent", "load"]);
        assert_eq!(MEM_PERCENT_FIELDS, &["mem", "mem_percent", "memory"]);
    }

    /// `load_percent` 的 `min(100)` 截断与 api.rs 同口径。
    #[test]
    fn load_percent_truncates_at_one_hundred_like_the_api_side() {
        let r = serde_json::json!({"cpu_percent": 340});
        assert_eq!(load_percent(&r, CPU_PERCENT_FIELDS), Some(100));
        let r = serde_json::json!({"mem_percent": 42});
        assert_eq!(load_percent(&r, MEM_PERCENT_FIELDS), Some(42));
        // 取不到就是 None，**不回落成 0**。
        assert_eq!(
            load_percent(&serde_json::json!({}), CPU_PERCENT_FIELDS),
            None
        );
        assert_eq!(
            load_percent(&serde_json::Value::Null, CPU_PERCENT_FIELDS),
            None
        );
    }

    // -----------------------------------------------------------------
    // 名单解析
    // -----------------------------------------------------------------

    /// 两种名单形态都认：JSON 数组与逗号分隔。
    #[test]
    fn a_name_list_parses_both_json_arrays_and_comma_separated_text() {
        let json = parse_name_list(Some(r#"["Device-A", "device-b"]"#));
        assert_eq!(
            json,
            ["device-a", "device-b"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        );
        let text = parse_name_list(Some("Device-A, device-b ,"));
        assert_eq!(text, json);
        assert!(parse_name_list(Some("")).is_empty());
        assert!(parse_name_list(None).is_empty());
    }

    /// 「不限」与「限了但名单为空」是两件事：
    ///
    /// - `None` = 不限 → 放行；
    /// - `Some("")` = 限了但一个都没写 → **不放行任何设备**。
    ///
    /// 把后者当成「不限」会让一条写坏了的约束静默放行全部设备。
    #[test]
    fn an_empty_but_present_allowed_list_is_not_the_same_as_unlimited() {
        let binds = bindings_for(&["n_1"]);
        let nodes = [("n_1", good_report())];
        assert_eq!(route(&open(), &nodes, &binds), Ok("n_1"));
        let mut task = open();
        task.allowed_devices = Some(String::new());
        assert_eq!(
            route(&task, &nodes, &binds),
            Err(Rejection::NoAllowedDevice)
        );
    }

    /// 隔离式项目的**绑定**设备可以通过 ①（前提是别的层也过）。
    #[test]
    fn an_isolated_project_accepts_its_bound_device() {
        let binds = bindings_for(&["n_1"]);
        let nodes = [("n_1", good_report())];
        let routing = Routing::bare(ProjectShape::Isolated, &binds);
        let got = pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing);
        assert_eq!(got, Ok("n_1"));
    }

    /// 非隔离式项目里没有绑定的设备**不该被 ① 拦**（后面由 ⑥ 判代码可得性）。
    #[test]
    fn a_central_project_does_not_require_a_binding_at_layer_one() {
        // 磁盘充裕（否则 ⑤ 会先拒，这条就测不到 ⑥ 了）。
        let report = good_report();
        let empty: Vec<(String, Binding)> = Vec::new();
        let nodes = [("n_1", report)];
        let routing = Routing::bare(ProjectShape::Central, &empty);
        let got = pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing);
        // 没有绑定时 ⑥ 会拒（NoCode），**不是** ①（IsolationBlocked）——
        // 证明 ① 只对隔离式项目生效。
        assert_eq!(got, Err(Rejection::NoCode));
    }

    /// 空候选列表 → ⓪（「一台可用设备都没有」）。
    #[test]
    fn an_empty_node_list_is_reported_as_no_allowed_device() {
        let empty: Vec<(String, Binding)> = Vec::new();
        let nodes: Vec<(&str, Report)> = Vec::new();
        let routing = Routing::bare(ProjectShape::Central, &empty);
        let got = pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing);
        assert_eq!(got, Err(Rejection::NoAllowedDevice));
    }

    /// `pick` 返回的是**入参里的那个 `&str`**（生命周期绑定在节点迭代器上，
    /// 不是新分配的 `String`）——所以调用方拿它去查注册表不会多一次拷贝。
    #[test]
    fn pick_borrows_the_node_id_from_the_input() {
        let binds = bindings_for(&["n_1"]);
        let report = good_report();
        let owned = String::from("n_1");
        let nodes = [(owned.as_str(), report)];
        let routing = Routing::bare(ProjectShape::Central, &binds);
        let got = pick(&open(), nodes.iter().map(|(id, r)| (*id, r)), &routing);
        let got = got.expect("must pick");
        assert_eq!(got, "n_1");
        assert_eq!(got.as_ptr(), owned.as_ptr(), "返回的必须是入参的切片");
    }

    /// 版本匹配的亲和键：**没写版本就没有可比的**，键恒 `false`。
    #[test]
    fn the_agent_affinity_key_is_off_when_no_version_is_required() {
        let r = good_report();
        let mut task = open();
        task.required_capabilities = Some(r#"["agent:opencode"]"#.into());
        assert!(!agent_affinity(&task, &r));
        task.required_capabilities = Some(r#"["agent:opencode@2.1.4"]"#.into());
        assert!(agent_affinity(&task, &r));
        task.required_capabilities = Some(r#"["agent:opencode@9.9.9"]"#.into());
        assert!(!agent_affinity(&task, &r));
    }

    /// `available: false` 的 agent **不算可用**（② 要的是「现在能跑」）。
    #[test]
    fn an_installed_but_broken_agent_does_not_satisfy_layer_two() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        report.capabilities["agents"] = serde_json::json!([
            {"id": "opencode", "name": "opencode", "version": "2.1.4", "available": false}
        ]);
        let mut task = open();
        task.required_capabilities = Some(r#"["agent:opencode"]"#.into());
        assert_eq!(
            route(&task, &[("n_1", report)], &binds),
            Err(Rejection::NoAgent),
            "装了但跑不起来的 agent 不满足 ②"
        );
    }

    /// M1 的裸字符串 agent 形态：**已装但无可用证据**，同样不满足 ②。
    #[test]
    fn a_bare_string_agent_does_not_satisfy_layer_two_either() {
        let binds = bindings_for(&["n_1"]);
        let mut report = good_report();
        report.capabilities["agents"] = serde_json::json!(["opencode"]);
        let mut task = open();
        task.required_capabilities = Some(r#"["agent:opencode"]"#.into());
        assert_eq!(
            route(&task, &[("n_1", report)], &binds),
            Err(Rejection::NoAgent)
        );
    }
}
