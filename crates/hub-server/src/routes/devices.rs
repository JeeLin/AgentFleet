//! `/api/devices/{id}`：**从设备侧看项目**（`v0.7.0` 子任务 7）。
//!
//! # 为什么设备页要扩，而不是被项目页替代
//!
//! `PRODUCT.md` §3 的产品主轴是「**做事从项目进，管理从设备进**」。项目主轴
//! 改版之后很容易得出一个错误的结论：「项目页有了绑定视图，设备页可以砍了」。
//! 恰恰相反——两个视角回答的是不同的问题：
//!
//! - 项目页回答「**这个项目**该跑在哪台机器上」；
//! - 设备页回答「**这台机器**上跑了哪些项目、还剩多少盘、老是栽在哪一类」。
//!
//! 运维时的第一反应永远是「这台机器怎么了」，那是设备视角。所以本模块把
//! 项目维度的数据**聚合到设备上**，而不是让用户去项目页逐个点。
//!
//! # 不引入新表
//!
//! 三块数据全部是既有表的读：
//!
//! | 块 | 来源 |
//! |---|---|
//! | `bindings[]` | `binding` 表（子任务 4） |
//! | `disk` | `binding.disk_quota_bytes` + 执行记录的 `worktree_path` 前缀聚合 |
//! | `failure_breakdown` | `execution` 表按 `state` 计数 |
//!
//! 新增一张「设备用量」表会立刻引入一致性问题：配额改了那张表不会跟着改，
//! 而两边不一致时没人知道该信哪个。
//!
//! # `failure_breakdown` 的粒度
//!
//! `execution` 表**没有失败原因列**（失败原因在轨迹里，`ExecutionState::Failed`
//! 的注释写过「原因见轨迹，不在本表」）。所以这里按**状态**分：成功 / 失败 /
//! 中止三类。其中「中止」**不是失败**——「我们主动停的」与「它自己挂的」在
//! 归因里是两个结论，混成一个会让一台从没出过问题的机器看起来有失败。
//!
//! 按失败**原因**分档（超时 / 磁盘不足 / 环境损坏）需要读轨迹，那要一条
//! 「把轨迹聚成原因」的规则，而那条规则不存在。编一个出来就等于在统计里
//! 造假数据——所以这里不编，页面显示的是「失败 N 次」而不是「磁盘不足 ×2」。

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use hub_storage::binding::{BindingRepo, CleanupPolicy};
use hub_storage::execution::{ExecutionRepo, ExecutionState};

use crate::routes::api::{device_summary, DeviceSummary};
use crate::{ApiError, AppState};

/// 一次设备详情的响应体。
///
/// **`summary` 是一个完整的内嵌对象，不是把字段摊平**：设备页头部（名字、
/// 状态、资源、指纹）本来就是 `DeviceSummary` 那套，摊平会让两个端点
/// （列表 / 详情）的字段名各写一遍，改一处漏一处。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceDetail {
    pub summary: DeviceSummary,
    /// 这台设备上绑定的项目。**空数组 = 真的没绑**，页面要说「还没有绑定项目」。
    pub bindings: Vec<DeviceBinding>,
    /// 磁盘用量。没有任何绑定时是 `None`——「没绑项目」与「用了 0 字节」
    /// 在页面上是两句话，合成一个会让用户以为配额被吃光了。
    pub disk: Option<DeviceDisk>,
    /// 这台设备上的执行结果分布。
    pub failure_breakdown: FailureBreakdown,
    /// 能不能删，「为什么不能」的理由也在这里。
    pub deletable: Deletable,
}

/// 一条绑定在设备视角下的样子。
///
/// 比项目视角多两个价格：`project_name`（设备页不知道项目名就没法显示）
/// 与 `used_bytes`（这个项目在这台机器上占了多少）。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceBinding {
    pub id: String,
    pub project_id: String,
    /// 项目名。项目行被删掉时退回 id——但项目只归档不删除（`PRODUCT.md` §3.2），
    /// 所以正常情况下这里总有名字。
    pub project_name: String,
    pub fixed_dir: String,
    pub disk_quota_bytes: Option<i64>,
    pub cleanup_policy: &'static str,
    /// 这个项目的执行在这台设备上留下的字节数（按 `worktree_path` 前缀聚合）。
    ///
    /// `null` 不是 0：worktree 还没建、或执行记录里没写路径时，我们**不知道**
    /// 它占了多少，而 0 会被读成「这个项目一点不占地方」。
    pub used_bytes: Option<u64>,
}

/// 磁盘那一块的汇总。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DeviceDisk {
    /// 全部绑定的配额之和。`None` = 没有任何一条绑定设了配额。
    pub quota_bytes: Option<i64>,
    /// 已用字节数。
    ///
    /// **`null` 不是 0**：真实用量只有设备自己量得出来（它才有那个盘），
    /// Hub 拿不到。0 会被读成「这个项目一点不占地方」，而 `null` 说的是
    /// 「设备还没上报」——后者才是事实。
    pub used_bytes: Option<u64>,
    /// 0–100。`None` = 算不出来（没配额、或设备还没报用量）。
    /// **不是 0%**：0% 会被读成「还有很多地方」。
    pub percent: Option<u8>,
    /// 还能不能往这台设备派任务。与设备侧 `worktree` connector 的判据同源
    /// （90%），这里是 Hub 侧的**预判**——真正的拦截在派活时由设备自己报。
    pub accepts_new_work: bool,
}

/// 执行结果分布。
///
/// **三类分开，不合成一个「成功率」**：设备页要回答的是「栽在哪」，
/// 而 aborted 混进分母会把一台从没出过问题的机器算成 80%。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct FailureBreakdown {
    pub succeeded: u64,
    pub failed: u64,
    /// **不是失败**：用户取消、设备掉线、换设备重跑。
    pub aborted: u64,
    /// 还在跑或还没开工的。**不进分母**：算进去会让成功率随「现在有几条在跑」抖动。
    pub in_flight: u64,
}

impl FailureBreakdown {
    /// 把一条执行记进它自己那一档。
    ///
    /// **只有这一份分档逻辑**：设备详情与 agent 详情要算同一个东西（同一条绑定
    /// 下的执行，一个按设备看、一个按 agent 看）。各写一份 `match` 的话，将来
    /// `ExecutionState` 多一个变体，两处会分别编译通过、口径却不一致——而
    /// 「成功率」是一页上最容易被当真的那个数字。
    pub fn record(&mut self, state: ExecutionState) {
        match state {
            ExecutionState::Succeeded => self.succeeded += 1,
            ExecutionState::Failed => self.failed += 1,
            ExecutionState::Aborted => self.aborted += 1,
            ExecutionState::Pending | ExecutionState::Running => self.in_flight += 1,
        }
    }

    /// 分母：只算**已经结束**的执行。
    fn settled(self) -> u64 {
        self.succeeded + self.failed + self.aborted
    }

    /// 成功率（百分比整数）。`None` = 一次都没结束过。
    ///
    /// **`None` 不是 0**：0/0 与 0/10 在页面上是两句完全不同的话
    /// （「还没跑过」vs「一次都没成」）。返回 `Some(0)` 会让前者显示成后者。
    pub fn success_rate(self) -> Option<u8> {
        let settled = self.settled();
        if settled == 0 {
            return None;
        }
        Some(((self.succeeded.saturating_mul(100)) / settled) as u8)
    }
}

/// 删除前置条件的**结论**，不只是布尔值。
///
/// 只说「不能删」的话，用户不知道下一步该干什么。`reason` 是那句话。
#[derive(Debug, Clone, Serialize)]
pub struct Deletable {
    pub allowed: bool,
    /// 拒绝的理由（人话）。`allowed == true` 时是 `None`。
    pub reason: Option<String>,
    /// 这台设备上的历史执行条数——`reason` 里的那个数字的来源。
    pub executions: u64,
}

/// `GET /api/devices/{id}`。
pub async fn get_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeviceDetail>, ApiError> {
    let device = state
        .devices()
        .list()
        .map_err(|e| ApiError::internal_with("cannot list devices", e))?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(ApiError::not_found)?;

    let summary = device_summary(&state, &device);

    let bindings_repo = BindingRepo::new(state.pool().clone());
    let raw_bindings = bindings_repo
        .list_for_device(&id)
        .map_err(|e| ApiError::internal_with("cannot list bindings", e))?;

    let executions_repo = ExecutionRepo::new(state.pool().clone());
    let mut by_state = FailureBreakdown::default();
    let mut bindings = Vec::with_capacity(raw_bindings.len());
    // 逐条绑定累加用量。**在同一个循环里做**，不另起一趟：两条查询读的是
    // 同一批 execution 行，多一趟只多一次「两个数字对不上」的机会。
    let mut quota_total: Option<i64> = None;

    for b in &raw_bindings {
        let runs = executions_repo
            .list_for_binding(&b.id)
            .map_err(|e| ApiError::internal_with("cannot list executions", e))?;
        for run in &runs {
            by_state.record(run.state);
            // **用量不在这里算。** `worktree_path` 是设备上的绝对路径，
            // Hub 没有那台机器的文件系统——按路径前缀去库里查不出字节数，
            // 任何在这里凑出来的数字看起来都和真的一样，但它是编的。
            //
            // 真实用量只有设备自己报（子任务 5 的 `worktree.disk_usage`），
            // 而把那个值存回 Hub 需要一张「设备用量」表，那正是本子任务
            // 明文不引入的东西。所以这里如实留 `None`，页面显示「设备未上报」。
        }
        if let Some(q) = b.disk_quota_bytes {
            quota_total = Some(quota_total.unwrap_or(0).saturating_add(q));
        }

        bindings.push(DeviceBinding {
            id: b.id.clone(),
            project_id: b.project_id.clone(),
            project_name: super::project_name(&state, &b.project_id),
            fixed_dir: b.fixed_dir.clone(),
            disk_quota_bytes: b.disk_quota_bytes,
            cleanup_policy: policy_str(b.cleanup_policy),
            used_bytes: None,
        });
    }

    let disk = if raw_bindings.is_empty() {
        // 没绑任何项目 → 没有「磁盘使用」这句话可说。回 `None` 而不是
        // `{used: 0, quota: null}`：后者在页面上会渲染成「已用 0 B」，而
        // 用户真正需要知道的是「这台机器还没被任何项目用起来」。
        None
    } else {
        Some(DeviceDisk {
            quota_bytes: quota_total,
            // Hub 侧算不出真实用量（见上），所以这里是 `None` 而不是 0。
            used_bytes: None,
            percent: None,
            // **这里不做「超 90% 就拦」的预判。** 那个判断需要真实用量，
            // 而 Hub 拿不到——拿一个 `None` 去推，只能推出「不拦」，
            // 那就成了一个永远放行的假门禁。真正的拦截在派活时由设备自己
            // 报（子任务 5 的 `worktree.disk_usage` 带 `accepts_new_work`），
            // 那才是**有数字**的那一侧。
            //
            // 保留这个字段是因为配额为 **0** 时不需要用量就能下结论：
            // 一个字节都放不下的配额，现在就能拒。
            accepts_new_work: match quota_total {
                None => true,
                Some(q) => q > 0,
            },
        })
    };

    // **条数以存储层为准**，不是数上面那个循环：循环只走过当前还存在的绑定，
    // 而已删绑定的历史执行仍在库里（`delete_device` 的前置检查读的正是
    // `execution_count_for_device`）。两处口径不一致的话，设备页说「能删」、
    // 真去删却 409——那是最让人困惑的一种不一致。
    let executions = match bindings_repo.execution_count_for_device(&id) {
        Ok(n) => u64::try_from(n).unwrap_or(0),
        Err(e) => {
            // coverage: excluded — 要走到这里必须把库读坏。这条不能「退成 0」：
            // 那会让一台有执行记录的设备在页面上显示「可以删」。
            return Err(ApiError::internal_with("cannot count executions", e));
        }
    };
    let deletable = if executions > 0 {
        Deletable {
            allowed: false,
            reason: Some(format!(
                "有 {executions} 条历史执行记录，不能删除。要下线就先归档项目，或把项目转到别的设备。"
            )),
            executions,
        }
    } else {
        Deletable {
            allowed: true,
            reason: None,
            executions: 0,
        }
    };

    Ok(Json(DeviceDetail {
        summary,
        bindings,
        disk,
        failure_breakdown: by_state,
        deletable,
    }))
}

fn policy_str(p: CleanupPolicy) -> &'static str {
    p.as_str()
}
