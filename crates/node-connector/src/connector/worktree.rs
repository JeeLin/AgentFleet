//! worktree connector：**按任务分工作区**，并按策略回收磁盘（`v0.7.0` 子任务 5）。
//!
//! # 它解决的是哪一类问题
//!
//! 一个项目在一台设备上有一个**固定目录**（Binding 决定，见子任务 4）。同一个
//! 项目可能同时在跑几个任务，它们不能共用一份检出——否则一个任务的改动会
//! 出现在另一个任务的 diff 里，而两个 agent 会互相覆盖对方的文件。
//!
//! 所以每个任务在 `<固定目录>/<任务 id>` 下有一份**独立检出**，任务结束按策略
//! 回收。这正是 git 的 worktree，但**接口里不出现 `git` 这个词**（同
//! `hub_core::repo` 的理由）：Hub 说的是「建工作区 / 清工作区 / 用了多少盘」。
//!
//! # 四层处置
//!
//! | 层 | 内容 | 在哪 |
//! |---|---|---|
//! | 任务级 | 跑完 → 丢弃未跟踪文件与改动，回到主分支 | [`Worktree::remove`] |
//! | 缓存级 | 依赖缓存按 lockfile 哈希复用；**任务产物必清** | [`Worktree::remove`] |
//! | 配额级 | Binding 上设配额；超 90% 就不再往这台派任务 | [`Worktree::disk_usage`] |
//! | 策略级 | 保守 / 平衡 / 激进决定缓存留多久、产物留不留 | [`Policy`] |
//!
//! 配额过滤**不在这里**做：它必须发生在派活**之前**，而派活是 Hub 的事。
//! 这里只负责把「用了多少、上限多少、还能不能派」报上去，让 Hub 在候选集里
//! 剔除——`PRODUCT.md` §5.3 写死了这一条，等失败了再报 `disk_exhausted`
//! 是把一个「别派」变成「派了再失败」。
//!
//! # 硬规则：依赖目录永远不搬运
//!
//! `node_modules` 这类目录**按 lockfile 在本地重建**，绝不从别处搬。原因不是
//! 省带宽：x86-64 与 aarch64 装出来的不是同一份东西，连 `node_modules/.bin`
//! 里的软链目标都不同。搬一份过去得到的是一个**能跑但跑错**的环境——比跑不起来
//! 难查得多。
//!
//! 这条规则有两个落点，都在本文件里：
//! 1. 建工作区时**不复制**任何依赖目录（我们只检出源码）；
//! 2. 清理时**不动**依赖目录（它属于缓存层，归策略管，归 [`CACHE_DIRS`] 认）。
//!
//! # 为什么它是个 connector 而不是 node-core 里的一个模块
//!
//! 设备侧能力都是 connector（子任务 0 之后全部住在 `node-connector`）。
//! worktree 是**设备侧**能力——它动的是这台机器上的盘，Hub 碰不到。做成
//! connector 复用的是既有那套 `open_channel` / `channel_data.op` 生命周期，
//! 一条新的帧通路不用为它单开。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::{Connector, ConnectorError, OpenReq, OutFrame};

/// 依赖目录**永不搬运、也不由任务级清理动**。
///
/// 这些目录由 lockfile 决定内容，跨机器 / 跨架构搬过去得到的是「能跑但跑错」
/// 的环境。清理时它们是**缓存层**，归策略管（见 [`Policy::cache_ttl`]），
/// 不归任务级的「丢弃改动」管。
pub const CACHE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".tox",
    ".gradle",
    ".cargo",
    "vendor/bundle",
];

/// 配额的使用率超过这个比例就**别再往这台设备派任务**。
///
/// 90% 不是随手取的：留下 10% 给「已经派出去的任务把产物写完」。卡在 100%
/// 才拦的话，那些已经在跑的任务会写失败，而它们的失败会被记成任务失败——
/// 用户看到的是「任务莫名其妙挂了」，不是「磁盘满了」。
pub const QUOTA_STOP_PERCENT: u64 = 90;

/// 三档清理策略。
///
/// **三档的实际差别必须体现在行为上**（里程碑 §5 的表）：只写三个名字而不
/// 改行为，等于给用户一个不起作用的下拉框。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// 依赖缓存留 30 天；产物留最近 3 个做诊断。
    Conservative,
    /// 依赖缓存留 7 天；产物跑完即清。
    Balanced,
    /// 依赖缓存尽快清；产物跑完即清。
    Aggressive,
}

impl Policy {
    /// 依赖缓存保留多久。`None` = 不按时间清（只在配额紧时清）。
    pub fn cache_ttl(self) -> Option<Duration> {
        match self {
            Self::Conservative => Some(Duration::from_secs(30 * 24 * 3600)),
            Self::Balanced => Some(Duration::from_secs(7 * 24 * 3600)),
            // 「尽快清」= 下一个清理回合就清，但**不是立刻**：正在跑的任务
            // 还要用那些依赖。零意味着「下次清理一定选中它」。
            Self::Aggressive => Some(Duration::ZERO),
        }
    }

    /// 任务产物留几个。`0` = 跑完即清。
    pub fn keep_artifacts(self) -> usize {
        match self {
            Self::Conservative => 3,
            _ => 0,
        }
    }
}

/// 默认档：**平衡**，与存储层 `CleanupPolicy` 的默认一致。
///
/// 两处各写一个默认值的话，改默认档时必然漏一处——症状是「Hub 上看着是平衡档，
/// 设备实际按激进档清」，而用户只能从「怎么又要重装依赖」里慢慢发现。
impl Default for Policy {
    fn default() -> Self {
        Self::Balanced
    }
}

/// 一次清理要处理的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPlan {
    /// 这次要删掉的任务工作区（按任务 id）。
    pub remove_worktrees: Vec<String>,
    /// 这次要不要顺带清依赖缓存（产物已清、且缓存过期）。
    pub reclaim_cache: bool,
}

/// 一个任务的回收记录。`finished_at` 用来算「留最近几个」。
#[derive(Debug, Clone)]
pub struct Finished {
    pub task_id: String,
    pub finished_at: SystemTime,
}

/// 按策略算这次该删哪些工作区。
///
/// 纯函数，不碰盘：它把「哪些任务跑完了、什么时候跑完的」变成一份计划。
/// **这样测策略时不用造真目录**，而策略本身才是这一层最容易写错的部分
/// （写错了不报错，只是磁盘慢慢满）。
pub fn plan_cleanup(policy: Policy, finished: &[Finished]) -> CleanupPlan {
    let keep = policy.keep_artifacts();
    // 最新的 `keep` 个留下，其余删。**按结束时间排序**而不是按名字：
    // 任务 id 是随机的，按名字排等于随机留几个。
    let mut sorted: Vec<&Finished> = finished.iter().collect();
    // `Reverse` 而不是 `sort_by(|a, b| b.x.cmp(&a.x))`：倒序的意图写在类型里，
    // 读的人不需要在脑子里把 a/b 对调一次。
    sorted.sort_by_key(|a| std::cmp::Reverse(a.finished_at));

    let remove_worktrees = sorted
        .iter()
        .skip(keep)
        .map(|f| f.task_id.clone())
        .collect::<Vec<_>>();

    // 缓存只在**产物都清完了**才动：产物还在说明用户可能正在看，清掉缓存
    // 会让下一次执行重新装一遍依赖，而那时他等的就是这个。
    let reclaim_cache = remove_worktrees.len() == finished.len() && !finished.is_empty();

    CleanupPlan {
        remove_worktrees,
        reclaim_cache,
    }
}

/// 磁盘使用的快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub used_bytes: u64,
    /// `None` = 没设配额。**与 0 分开**：0 是一个会把一切都拦掉的真实配额。
    pub quota_bytes: Option<u64>,
}

impl DiskUsage {
    /// 使用率（百分比）。没设配额时按 0 算——**不是 100**：
    /// 没设配额就等于「不因磁盘原因拦」，报 100 会把它拦掉。
    ///
    /// 配额为 0 是**真实的上限**（一个字节都放不下），不是「没设」：
    /// 用它兼职表示「无限」会让「设成 0」这个动作静默变成「不限额」。
    pub fn percent(self) -> u64 {
        match self.quota_bytes {
            None => 0,
            // 上限 0 且有内容 → 100%。用 `saturating` 而不是除法：
            // 0 做除数会 panic，而约束 21 不许生产代码 panic。
            Some(0) => 100,
            Some(q) => self.used_bytes.saturating_mul(100) / q,
        }
    }

    /// 还能不能往这台设备派任务。
    pub fn accepts_new_work(self) -> bool {
        self.percent() < QUOTA_STOP_PERCENT
    }
}

/// worktree connector。开通道时拿到固定目录（Binding 定的那个），此后不可变。
#[derive(Debug)]
pub struct Worktree {
    root: Option<PathBuf>,
    /// 配额上限，`open` 时从 params 记下。`None` = 不限额。
    quota: Option<u64>,
    /// 清理策略，`open` 时从 params 记下。
    policy: Policy,
}

impl Worktree {
    pub fn new() -> Self {
        Self {
            root: None,
            quota: None,
            policy: Policy::default(),
        }
    }

    fn root_checked(&self) -> Result<&Path, ConnectorError> {
        self.root.as_deref().ok_or_else(|| ConnectorError::Invalid {
            detail: "worktree channel is not open".to_string(),
        })
    }

    /// 一个任务的工作区路径：`<固定目录>/<任务 id>`。
    ///
    /// 任务 id 里出现 `/` 或 `..` 就拒：它会把这个路径指到固定目录之外，
    /// 而那条路径是拿来删的。
    pub fn worktree_of(&self, task_id: &str) -> Result<PathBuf, ConnectorError> {
        let bad = |why: &str| ConnectorError::Invalid {
            detail: format!("task id {task_id:?} cannot name a worktree: {why}"),
        };
        if task_id.is_empty() {
            return Err(bad("empty"));
        }
        if task_id.contains('/') || task_id.contains('\\') {
            return Err(bad("contains a path separator"));
        }
        if task_id == "." || task_id == ".." {
            return Err(bad("is a relative path component"));
        }
        Ok(self.root_checked()?.join(task_id))
    }

    /// 算固定目录用了多少盘。
    ///
    /// **不跟随符号链接**：跟了就会把链接目标（可能是另一个盘、甚至是
    /// 用户的 home）算进这个项目的配额里，于是「这个项目用了 500G」而实际
    /// 它只用了 5M。
    pub fn disk_usage(&self) -> Result<DiskUsage, ConnectorError> {
        let root = self.root_checked()?;
        Ok(DiskUsage {
            used_bytes: dir_size(root),
            // 配额由 Binding 决定，经 `open` 的 params 传进来。没传 = 不限额。
            quota_bytes: self.quota_from_open(),
        })
    }

    /// `open` 时记下的配额。**不在每次调用时重读**：配额是开通道那一刻的
    /// 事实，跑着跑着被别处改了，会让同一次派活的判断前后不一致。
    fn quota_from_open(&self) -> Option<u64> {
        self.quota
    }
}

impl Default for Worktree {
    fn default() -> Self {
        Self::new()
    }
}

/// 递归算目录字节数。**不跟随符号链接**（见 [`Worktree::disk_usage`]）。
fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // 读不了就跳过这一层：算盘是为了「够不够派活」，不是为了报表精确到
        // 字节。一次权限错误不该让整次派活失败。
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // coverage: excluded — 要走到这里必须有一个读不进去的子目录
            // （权限被撤 / 目录在遍历中途被删）。这是并发与权限故障，
            // 测试里造不出稳定复现的一刻。
            continue;
        };
        for entry in entries.flatten() {
            // `symlink_metadata` 而不是 `metadata`：后者会跟随链接，于是
            // 「这个项目用了 500G」而实际它只用了 5M——那个 500G 是链接目标
            // 的，可能是另一个盘、甚至是用户的 home。
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                // 链接本身几乎不占空间，**不计入**。不跟随才是这个函数
                // 对配额有意义的唯一前提。
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// `open` 的私有参数。
#[derive(Debug, Deserialize)]
struct OpenParams {
    /// 固定目录的**父**路径由 `workspace_root` 给（Hub 从 Binding 查出）。
    /// 配额上限，`null` = 不限额。
    #[serde(default)]
    disk_quota_bytes: Option<u64>,
    /// 清理策略。不给时用 **balanced**（与存储层默认一致）。
    #[serde(default = "default_policy")]
    cleanup_policy: Policy,
}

fn default_policy() -> Policy {
    Policy::default()
}

impl Connector for Worktree {
    fn open(&mut self, req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = req.root().ok_or_else(|| ConnectorError::WorkspaceRoot {
            // coverage: excluded — `OpenReq::root()` 为 `None` 只在 Hub 没下发
            // 固定目录时发生，而那样它压根不会派活到本连接器。
            detail: "no fixed directory was given for this project on this device".to_string(),
        })?;
        // 固定目录可以还不存在（项目刚绑上、第一次派活），那就建出来。
        // **只建这一层**：`create_dir_all` 会把父目录也建出来，而父目录是
        // 用户的盘子布局，不该由我们来决定。
        if !root.is_dir() {
            std::fs::create_dir(root).map_err(|e| ConnectorError::WorkspaceRoot {
                // coverage: excluded — 建不了固定目录是权限 / 磁盘故障。
                detail: format!("{} is not usable: {e}", root.display()),
            })?;
        }
        let real = root
            .canonicalize()
            .map_err(|e| ConnectorError::WorkspaceRoot {
                // coverage: excluded — 目录刚建出来就 canonicalize 不了，
                // 意味着权限在两步之间被撤了。
                detail: format!("{} is not usable: {e}", root.display()),
            })?;
        let params: OpenParams = req
            .parse("worktree")
            .map_err(|detail| ConnectorError::Invalid { detail })?;
        // 配额是**开通道那一刻**的事实，记下来，不在每次调用时重读：
        // 跑着跑着被别处改了，会让同一次派活的判断前后不一致。
        self.quota = params.disk_quota_bytes;
        self.policy = params.cleanup_policy;
        self.root = Some(real);
        Ok(Vec::new())
    }

    fn on_data(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        self.root_checked()?;
        let op = data
            .get("op")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ConnectorError::Invalid {
                detail: "worktree data must carry a string \"op\"".to_string(),
            })?
            .to_string();
        match op.as_str() {
            "worktree.create" => self.create(data),
            "worktree.remove" => self.remove(data),
            "worktree.disk_usage" => self.usage(),
            other => Err(ConnectorError::Invalid {
                detail: format!("unknown worktree operation: {other}"),
            }),
        }
    }

    fn close(&mut self) {
        self.root = None;
    }
}

impl Worktree {
    /// `worktree.create`：建一个任务工作区目录。
    ///
    /// **不复制任何东西**：源码由后续的检出步骤放进去，依赖由 lockfile 在本地
    /// 重建（[`CACHE_DIRS`] 的首条）。这里只保证目录存在且是空的——
    /// 一个残留的旧工作区会让新任务继承上一次的改动，而那种「改动不是我做的」
    /// 是最难查的一类问题。
    fn create(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let task_id = task_id_of(data)?;
        let dir = self.worktree_of(&task_id)?;
        if dir.exists() {
            return Err(ConnectorError::Failed {
                op: "worktree.create".to_string(),
                detail: format!("worktree for task {task_id} already exists"),
            });
        }
        std::fs::create_dir(&dir).map_err(|e| ConnectorError::Failed {
            // coverage: excluded — 建不了工作树目录是磁盘 / 权限故障。
            op: "worktree.create".to_string(),
            detail: e.to_string(),
        })?;
        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "worktree.create",
            "task_id": task_id,
            "created": true,
        }))])
    }

    /// `worktree.remove`：删掉一个任务的工作区。
    ///
    /// 四层里的**任务级 + 缓存级**在这里落地：
    /// - 任务产物必清（整目录删掉，产物在里面）；
    /// - 依赖目录属于缓存层，**由策略决定**——保守档要留着让下一个任务复用。
    ///   所以先按策略判断，再决定是整目录删还是只删产物。
    fn remove(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let task_id = task_id_of(data)?;
        let dir = self.worktree_of(&task_id)?;
        let policy = self.policy;
        let kept_cache = if policy.cache_ttl() == Some(Duration::ZERO) {
            // 激进：连缓存一起清。
            remove_dir_all(&dir).map_err(|e| ConnectorError::Failed {
                // coverage: excluded — 删不掉目录是权限故障。
                op: "worktree.remove".to_string(),
                detail: e.to_string(),
            })?;
            false
        } else {
            // 保守 / 平衡：只删产物，留依赖目录。
            remove_products(&dir).map_err(|e| ConnectorError::Failed {
                // coverage: excluded — 同上。
                op: "worktree.remove".to_string(),
                detail: e.to_string(),
            })?;
            true
        };
        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "worktree.remove",
            "task_id": task_id,
            "cache_kept": kept_cache,
        }))])
    }

    /// `worktree.disk_usage`：报用量、上限、还能不能派活。
    fn usage(&mut self) -> Result<Vec<OutFrame>, ConnectorError> {
        let u = self.disk_usage()?;
        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "worktree.disk_usage",
            "used_bytes": u.used_bytes,
            "quota_bytes": u.quota_bytes,
            "percent": u.percent(),
            "accepts_new_work": u.accepts_new_work(),
        }))])
    }
}

/// 从帧里取任务 id。
fn task_id_of(data: &serde_json::Value) -> Result<String, ConnectorError> {
    data.get("task_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ConnectorError::Invalid {
            detail: "worktree data must carry a string \"task_id\"".to_string(),
        })
}

/// 删一个目录树。不存在算成功——`remove` 要能重复调用。
fn remove_dir_all(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        // 「已经不存在」算成功：`remove` 要能重复调用。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // coverage: excluded — 删不掉的其它原因（权限 / 忙）。
        Err(e) => Err(e),
    }
}

/// 只删产物，**保留 [`CACHE_DIRS`] 里的依赖目录**。
///
/// 判据是「这个名字是不是依赖目录」而不是「它大不大」：按大小判会在
/// 恰好只有依赖目录的任务上把依赖删掉，而那时下一个任务要重装一遍——
/// 用户看到的是「明明没改依赖，怎么又装了三分钟」。
fn remove_products(dir: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // 目录已不存在：幂等性兜底（正常路径先查过 exists）。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        // coverage: excluded — 读不进去（权限 / 忙）。
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if CACHE_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

// 单测放在这里而不是 `tests/worktree_cleanup.rs`：下面几条要验的是**私有**
// 的错误臂（`dir_size` / `remove_dir_all` / `remove_products` / `open` 的建目录
// 与坏参数解析），集成测试看不见它们。
#[cfg(test)]
mod tests {
    use super::*;

    fn open_at(root: &Path, params: serde_json::Value) -> Worktree {
        let mut w = Worktree::new();
        w.open(OpenReq::new(root.display().to_string(), params))
            .expect("开通道");
        w
    }

    /// 一次请求的载荷（`on_data` 恰好回一帧）。
    fn op(w: &mut Worktree, data: serde_json::Value) -> serde_json::Value {
        let mut out = w.on_data(&data).expect("on_data");
        let frame = out.pop().expect("一帧结果");
        assert!(out.is_empty(), "每次请求只该回一帧");
        frame.payload
    }

    /// 没给固定目录：`open` 要报「这台设备上没有这个项目的固定目录」，
    /// 而不是拿 `Path::new("")` 往下走。
    ///
    /// 空串在 `OpenReq::root()` 里就是 `None`，所以 `open` 必须在这一步停住：
    /// 继续走下去会把空路径当根，`is_dir()` 为假，接着 `create_dir("")`
    /// 会返回一句裸 `ENOENT` —— 用户读到的是一句系统话。
    #[test]
    fn opening_without_a_fixed_directory_says_so_in_a_sentence() {
        let mut w = Worktree::new();
        let err = w
            .open(OpenReq::new("", serde_json::json!({})))
            .expect_err("必须失败");
        assert!(
            matches!(err, ConnectorError::WorkspaceRoot { .. }),
            "这是「根不可用」，不是「参数不合法」：{err}"
        );
        assert!(
            err.to_string().contains("no fixed directory"),
            "错误要说清缺的是固定目录：{err}"
        );
        assert!(w.root.is_none(), "开不起来就不该记下根");
    }

    /// 固定目录还没建（项目刚绑上、第一次派活）：`open` 把它建出来，
    /// 并且记下的是**规范化之后**的路径。
    ///
    /// 记原路径会让后面每一次 `worktree_of` 拼出来的字符串都带着一个可能是
    /// 相对路径的前缀，而那个路径是拿来删的。
    #[test]
    fn opening_creates_the_fixed_directory_and_remembers_the_real_path() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("not-yet-there");
        let w = open_at(&root, serde_json::json!({}));
        assert!(root.is_dir(), "固定目录该被建出来");
        assert_eq!(
            w.root.as_deref(),
            Some(root.canonicalize().expect("canonicalize").as_path()),
            "记下的必须是规范化之后的路径"
        );
    }

    /// `cleanup_policy` 认得出三档，且**真的按它清**：`balanced` 留依赖，
    /// `aggressive` 连依赖一起删。
    ///
    /// 只验「字段被记下来了」是不够的：那个字段唯一的消费者是 `remove`，
    /// 记对了却不照着做，等于给用户一个不起作用的下拉框。
    #[test]
    fn the_cleanup_policy_from_the_open_params_decides_what_gets_removed() {
        let base = tempfile::tempdir().expect("tempdir");

        // 平衡档：依赖留下，产物删掉。
        let mut balanced = open_at(
            base.path(),
            serde_json::json!({"cleanup_policy": "balanced"}),
        );
        assert_eq!(balanced.policy, Policy::Balanced, "参数要落到策略上");
        op(
            &mut balanced,
            serde_json::json!({"op": "worktree.create", "task_id": "b1"}),
        );
        let dir = base.path().join("b1");
        std::fs::create_dir(dir.join("node_modules")).expect("mkdir");
        std::fs::write(dir.join("node_modules/dep.js"), b"dep").expect("write");
        std::fs::write(dir.join("out.bin"), b"x").expect("write");
        let out = op(
            &mut balanced,
            serde_json::json!({"op": "worktree.remove", "task_id": "b1"}),
        );
        assert_eq!(out["cache_kept"], serde_json::json!(true), "{out}");
        assert!(dir.join("node_modules/dep.js").exists(), "平衡档要留依赖");
        assert!(!dir.join("out.bin").exists(), "产物必清");

        // 激进档：整个目录都不留。
        let mut aggressive = open_at(
            base.path(),
            serde_json::json!({"cleanup_policy": "aggressive"}),
        );
        assert_eq!(aggressive.policy, Policy::Aggressive);
        op(
            &mut aggressive,
            serde_json::json!({"op": "worktree.create", "task_id": "a1"}),
        );
        let dir = base.path().join("a1");
        std::fs::create_dir(dir.join("node_modules")).expect("mkdir");
        let out = op(
            &mut aggressive,
            serde_json::json!({"op": "worktree.remove", "task_id": "a1"}),
        );
        assert_eq!(out["cache_kept"], serde_json::json!(false), "{out}");
        assert!(!dir.exists(), "激进档连缓存一起清");
    }

    /// `cleanup_policy` 写了个不认识的名字：报错，且说的是**坏在 worktree
    /// 的参数上**，而不是「这台设备不支持」。
    ///
    /// 静默回落到默认档是最坏的一种「宽容」：Hub 以为设成激进档了，设备
    /// 实际按平衡档清，用户从「怎么又要重装依赖」里慢慢发现。
    #[test]
    fn an_unknown_cleanup_policy_is_refused_before_anything_is_created() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("policy-root");
        let mut w = Worktree::new();
        let err = w
            .open(OpenReq::new(
                root.display().to_string(),
                serde_json::json!({"cleanup_policy": "whenever"}),
            ))
            .expect_err("未知策略必须被拒");
        let text = err.to_string();
        assert!(
            matches!(err, ConnectorError::Invalid { .. }),
            "这是参数问题，不是根不可用：{text}"
        );
        assert!(text.contains("worktree"), "错误要带上是谁的参数：{text}");
        assert!(w.root.is_none(), "参数没过关就不该算开好了");
    }

    /// 建工作区时**盘上建不出来**：错误归属到 `worktree.create` 这个操作。
    ///
    /// 造法是在任务路径上放一个**悬挂链接**：`dir.exists()` 跟随链接得到
    /// false（所以绕开「已经存在」那条臂），而 `create_dir` 看到链接本身
    /// 已占住名字，给 `EEXIST`。这条臂的职责是「别把 io 错误变成 panic
    /// 或一句空话」——它一定带 `op`，因为页面上「哪个操作失败了」就从这里来。
    #[cfg(unix)]
    #[test]
    fn a_create_that_cannot_touch_the_disk_names_the_operation() {
        let base = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(base.path().join("gone"), base.path().join("t1"))
            .expect("symlink");
        let mut w = open_at(base.path(), serde_json::json!({}));
        assert!(
            !base.path().join("t1").exists(),
            "悬挂链接的 exists() 必须为假，否则测的是另一条臂"
        );
        let err = w
            .on_data(&serde_json::json!({"op": "worktree.create", "task_id": "t1"}))
            .expect_err("名字已被悬挂链接占住，建不出目录");
        match err {
            ConnectorError::Failed { op, detail } => {
                assert_eq!(op, "worktree.create", "要指向建工作区这一步");
                assert!(!detail.is_empty(), "要给出原因，而不是一句空话");
            }
            other => panic!("必须是 Failed 而不是别的变体：{other}"),
        }
    }

    /// 平衡档下 `remove` 读不进工作区：错误同样归属到 `worktree.remove`。
    ///
    /// 造法：工作区路径上是一个**普通文件**，`remove_products` 的 `read_dir`
    /// 给 `ENOTDIR`——它不是 `NotFound`，所以走错误臂而不是幂等臂。
    /// 判据是「错误带上 op」，不是「errno 是几」。
    #[test]
    fn a_remove_that_cannot_read_the_worktree_names_the_operation() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut w = open_at(
            base.path(),
            serde_json::json!({"cleanup_policy": "balanced"}),
        );
        let file = base.path().join("file-task");
        std::fs::write(&file, b"x").expect("write");
        let err = w
            .on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "file-task"}))
            .expect_err("工作区是个文件时必须报错");
        match err {
            ConnectorError::Failed { op, .. } => {
                assert_eq!(op, "worktree.remove", "要指向清工作区这一步");
            }
            other => panic!("必须是 Failed 而不是别的变体：{other}"),
        }
        assert!(file.exists(), "失败路径不该顺手删东西");
    }

    /// `remove_dir_all` 把「已经不存在」当成功（`remove` 要能重复调用），
    /// `remove_products` 也一样；但**其它**失败照实交出去，并且激进档下
    /// 那条 `map_err` 要把 errno 翻成用户能看懂的 `Failed` 帧。
    ///
    /// 两条臂的判据截然不同：前者是幂等性（删两次不该报错），后者是
    /// 「真的没删掉」——把它也当成功的话，磁盘会在用户完全不知情的情况下
    /// 慢慢满起来。
    #[cfg(unix)]
    #[test]
    fn removing_a_missing_tree_is_noop_but_other_failures_are_reported() {
        let base = tempfile::tempdir().expect("tempdir");
        let gone = base.path().join("never-existed");
        // 不存在 → 成功（两条入口各走一遍幂等臂）。
        remove_dir_all(&gone).expect("不存在算成功");
        remove_products(&gone).expect("不存在算成功");

        // 存在但不是目录：两个函数各自把错误交出来。
        let file = base.path().join("plain");
        std::fs::write(&file, b"x").expect("write");
        let err = remove_dir_all(&file).expect_err("文件不是目录，删不掉要报错");
        assert_eq!(err.kind(), std::io::ErrorKind::NotADirectory, "{err}");
        assert!(file.exists(), "失败路径不该把东西删掉");
        let err = remove_products(&file).expect_err("文件不是目录，读不进去要报错");
        assert_eq!(err.kind(), std::io::ErrorKind::NotADirectory, "{err}");

        // ③ 激进档：`remove_dir_all` 的错误经过 `remove` 的 `map_err` 后要
        // 变成 `ConnectorError::Failed`，并带上「删不掉」这件事的原因。
        let mut aggressive = open_at(
            base.path(),
            serde_json::json!({"cleanup_policy": "aggressive"}),
        );
        let err = aggressive
            .on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "plain"}))
            .expect_err("文件不是目录，删不掉必须报错");
        match err {
            ConnectorError::Failed { op, detail } => {
                assert_eq!(op, "worktree.remove");
                assert!(!detail.is_empty(), "要带上 errno，而不是一句空话");
            }
            other => panic!("必须是 Failed 而不是别的变体：{other}"),
        }

        // ④ 平衡档：`remove_products` 的同一类错误也走 `remove` 的 `map_err`。
        let mut balanced = open_at(
            base.path(),
            serde_json::json!({"cleanup_policy": "balanced"}),
        );
        let err = balanced
            .on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "plain"}))
            .expect_err("文件不是目录，清产物必须报错");
        match err {
            ConnectorError::Failed { op, detail } => {
                assert_eq!(op, "worktree.remove");
                assert!(!detail.is_empty(), "要带上 errno，而不是一句空话");
            }
            other => panic!("必须是 Failed 而不是别的变体：{other}"),
        }
    }

    /// `remove_products` 的三种形态各走一遍：**目录**走 `remove_dir_all`，
    /// **普通文件**走 `remove_file`，**符号链接**也走 `remove_file`。
    ///
    /// 最后一条是关键：链接的 `file_type()` 让 `is_dir()` / `is_file()` 都为假，
    /// 于是落进 `else`。若那里写成 `remove_dir_all`，删的是**链接目标**——
    /// 一个指向界外的链接会让我们删掉工作区之外的东西。
    #[cfg(unix)]
    #[test]
    fn remove_products_handles_directories_files_and_symlinks_differently() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws = base.path().join("ws");
        std::fs::create_dir(&ws).expect("mkdir");
        std::fs::create_dir(ws.join("sub")).expect("mkdir");
        std::fs::write(ws.join("sub/deep.txt"), b"deep").expect("write");
        std::fs::write(ws.join("plain.txt"), b"plain").expect("write");
        std::fs::create_dir(ws.join("node_modules")).expect("mkdir");
        std::fs::write(ws.join("node_modules/dep.js"), b"dep").expect("write");

        // 一个连向**界外目录**的链接：正确做法是删链接本身。
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::write(outside.join("keep.txt"), b"keep").expect("write");
        std::os::unix::fs::symlink(&outside, ws.join("escape")).expect("symlink");
        // 一个普通的文件链接：同样只删链接。
        std::os::unix::fs::symlink(outside.join("keep.txt"), ws.join("file-link"))
            .expect("symlink");

        remove_products(&ws).expect("清产物");

        assert!(!ws.join("sub").exists(), "目录要整棵删掉");
        assert!(!ws.join("plain.txt").exists(), "文件要删掉");
        assert!(!ws.join("escape").exists(), "链接本身要被删掉");
        assert!(!ws.join("file-link").exists(), "文件链接也要被删掉");
        assert!(ws.join("node_modules/dep.js").exists(), "依赖目录要留下");
        assert!(
            outside.join("keep.txt").exists(),
            "链接目标在界外，绝不能跟着它删进去"
        );
    }

    /// `dir_size` 只数**普通文件**：目录本身、符号链接、FIFO 都不计入。
    ///
    /// 判据必须是**精确**的字节数——「差不多大」会让「链接不计入」这条验证
    /// 失去意义（一个指向 `/usr` 的链接一旦被跟随，差值就是几个 GB，
    /// 而「差得很多」在不同的机器上是不同的数）。
    #[cfg(unix)]
    #[test]
    fn dir_size_counts_regular_files_only() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir(&root).expect("mkdir");
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("a.bin"), [0u8; 100]).expect("write");
        std::fs::write(root.join("sub/b.bin"), [0u8; 40]).expect("write");
        // 指向界外大目录的链接：不计入。
        std::os::unix::fs::symlink("/usr", root.join("escape")).expect("symlink");
        // 悬挂链接：`metadata()` 跟着走会失败，但这条分支靠 `file_type()` 判。
        std::os::unix::fs::symlink(root.join("gone"), root.join("dangling")).expect("symlink");
        // FIFO：不是普通文件，不计入。
        let fifo = root.join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).expect("cstr");
        assert_eq!(
            unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
            0,
            "mkfifo: {}",
            std::io::Error::last_os_error()
        );

        assert_eq!(dir_size(&root), 140, "只数普通文件：100 + 40");
        // 不存在的目录是 0（`read_dir` 失败那一层被跳过），不是 panic。
        assert_eq!(dir_size(&base.path().join("never-existed")), 0);
    }

    /// `disk_usage` / `usage` 在**通道关掉之后**报「没开」，而不是报一个 0。
    ///
    /// 报 0 会让配额判定说「这台机器空得很，尽管派活」——而工作区根本不存在，
    /// 派过去的任务会全部失败在检出那一步。
    #[test]
    fn disk_usage_on_a_closed_channel_is_refused_instead_of_reporting_zero() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut w = open_at(base.path(), serde_json::json!({}));
        w.close();
        let err = w
            .on_data(&serde_json::json!({"op": "worktree.disk_usage"}))
            .expect_err("关掉之后不能报用量");
        assert!(err.to_string().contains("not open"), "got {err}");
        let err = w.disk_usage().expect_err("关掉之后没有用量");
        assert!(err.to_string().contains("not open"), "got {err}");

        // `usage()` 自己那条 `?` 同样走一遍（它是 `disk_usage` 的调用点）。
        let err = Worktree::new().usage().expect_err("没开就没有用量");
        assert!(err.to_string().contains("not open"), "got {err}");
    }

    /// `quota_from_open` 取的就是 `open` 时记下的那个值。
    ///
    /// 判据同时钉住「不限额 ≠ 配额 0」：`None` 报 `null` + `0%` + `true`，
    /// 而 `Some(0)` 是一个真实的上限（一个字节都放不下），会拦掉一切。
    #[test]
    fn the_quota_recorded_at_open_time_is_what_usage_reports() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut w = open_at(base.path(), serde_json::json!({"disk_quota_bytes": 4096}));
        assert_eq!(w.quota_from_open(), Some(4096));
        let out = op(&mut w, serde_json::json!({"op": "worktree.disk_usage"}));
        assert_eq!(out["op"], serde_json::json!("worktree.disk_usage"));
        assert_eq!(out["quota_bytes"], serde_json::json!(4096), "{out}");
        assert_eq!(out["used_bytes"], serde_json::json!(0), "空目录是 0 字节");
        assert_eq!(out["accepts_new_work"], serde_json::json!(true));

        // 不传 = 不限额：`null` 而不是 0。
        let mut unlimited = open_at(base.path(), serde_json::json!({}));
        assert_eq!(unlimited.quota_from_open(), None);
        let out = op(
            &mut unlimited,
            serde_json::json!({"op": "worktree.disk_usage"}),
        );
        assert!(out["quota_bytes"].is_null(), "{out}");
        assert_eq!(
            out["percent"],
            serde_json::json!(0),
            "没配额 = 0%，不是 100%"
        );
        assert_eq!(out["accepts_new_work"], serde_json::json!(true));
    }

    /// `close` 之后 `on_data` 的**每一条** op 都先说「通道没开」——
    /// 包括未知 op：真实原因常常是这条通道早关了，而用户要看的是后者。
    #[test]
    fn every_operation_on_a_closed_channel_says_the_channel_is_not_open() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut w = open_at(base.path(), serde_json::json!({}));
        w.close();
        assert!(w.root.is_none(), "关掉就该忘掉根");
        for op_name in [
            "worktree.create",
            "worktree.remove",
            "worktree.disk_usage",
            "worktree.teleport",
        ] {
            let err = w
                .on_data(&serde_json::json!({"op": op_name, "task_id": "t1"}))
                .expect_err("关掉的通道不能干活");
            assert!(err.to_string().contains("not open"), "{op_name}: {err}");
        }
    }

    /// `worktree_of` 的四个拒因各自说得清是哪一个，且**都在碰盘之前**被拒。
    ///
    /// 这条走的是私有入口（`worktree_of` 本身），判据是 `Invalid` + 具体原因：
    /// 错误文案会直接进 `channel_error.detail`，一句「路径不合法」不足以让
    /// 写 Hub 的人知道该改哪个字段。
    #[test]
    fn worktree_of_names_the_reason_a_task_id_cannot_be_used() {
        let base = tempfile::tempdir().expect("tempdir");
        let w = open_at(base.path(), serde_json::json!({}));
        for (bad, why) in [
            ("", "empty"),
            ("a/b", "path separator"),
            ("a\\b", "path separator"),
            (".", "relative path component"),
            ("..", "relative path component"),
        ] {
            let err = w.worktree_of(bad).expect_err("必须被拒");
            let text = err.to_string();
            assert!(
                matches!(err, ConnectorError::Invalid { .. }),
                "{bad:?} 该是参数问题：{text}"
            );
            assert!(text.contains(why), "{bad:?} 的原因要写清 {why}：{text}");
        }
        // 正样本：拼出来的就是 `<固定目录>/<任务 id>`。
        assert_eq!(
            w.worktree_of("t-1").expect("合法 id"),
            base.path()
                .canonicalize()
                .expect("canonicalize")
                .join("t-1")
        );
        // 没开通道时同样说「没开」，而不是拼出一个相对路径。
        let err = Worktree::new().worktree_of("t").expect_err("没开就拼不出");
        assert!(err.to_string().contains("not open"), "got {err}");
    }
}
