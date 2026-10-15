//! supervisor 的决策逻辑——**纯函数，不碰进程、不碰文件系统**。
//!
//! 为什么要把决策从阻塞循环里拆出来（`DECISIONS.md` D2）：
//! 参考实现 REX 把「读状态 → 判断 → spawn 子进程 → rename 文件」全部内联在
//! `run_supervisor` 循环里。它没有覆盖率门禁所以无所谓；我们有 **100% 阻断门禁**，
//! 而那种形状的代码**没法测**——要覆盖它的分支就得真的 spawn 子进程、真的删掉
//! 自己正在运行的二进制。
//!
//! 于是拆成两层：
//!
//! ```text//! decide(ExitInfo, Option<UpdateState>, CrashStats) → Decision   ← 本文件，纯函数
//! executor                                                             ← node-cli/src/supervisor.rs
//! ```
//!
//! `Decision` 把「为什么」也带出来（`Decision::reason`），因为 supervisor 要把它
//! 写进日志与状态文件：**下一次崩溃时只有日志能解释上一次为什么回滚了**。

use crate::update::{Reconcile, UpdatePhase, UpdateState};

/// worker 的退出码协议。
///
/// 协议化而非「读日志」：supervisor 不理解业务，唯一可靠的信号就是退出码。
/// 约定见 `DECISIONS.md` D2。
pub mod exit_code {
    /// 正常退出。
    pub const NORMAL: i32 = 0;
    /// worker 已写好 `update-state.json`，请求 supervisor 替换二进制。
    pub const UPDATE_REQUEST: i32 = 10;
    /// worker 自认不健康（升级后没过健康检查）。
    pub const HEALTH_FAILURE: i32 = 11;
    /// worker 崩溃。
    pub const CRASH: i32 = 12;
}

/// 退出快于 [`FAST_CRASH_SECS`] 视为**配置错误**而非偶发崩溃。
///
/// 端口占用、配置写错这类问题重启一万次也不会好，而且重试会掩盖真实错误。
pub const FAST_CRASH_SECS: u64 = 5;

/// 连续退出多少次且间隔够短 → 判定不可恢复。
///
/// 与退出码**无关**（D2-②）：只统计「崩」会漏掉「启动即干净退出」——
/// 那种 worker 以 1s 间隔无限重生，永远不触发回滚，Hub 侧看到的是每秒一次
/// 的心跳风暴，且真实 bug 被无限重试彻底掩盖。
pub const EXIT_STORM_WINDOW_SECS: u64 = 10;
pub const EXIT_STORM_THRESHOLD: u32 = 3;

/// worker 退出的观测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: i32,
    /// 这次 worker 存活了多久。
    pub ran_for_secs: u64,
}

impl ExitInfo {
    pub fn new(code: i32, ran_for_secs: u64) -> Self {
        Self { code, ran_for_secs }
    }

    /// 是否是「启动即退」——短于 [`FAST_CRASH_SECS`] 且非正常退出。
    pub fn is_fast_crash(&self) -> bool {
        self.ran_for_secs < FAST_CRASH_SECS && self.code != exit_code::NORMAL
    }

    /// 是否正常退出。
    pub fn is_normal(&self) -> bool {
        self.code == exit_code::NORMAL
    }

    /// 是否请求升级。
    pub fn is_update_request(&self) -> bool {
        self.code == exit_code::UPDATE_REQUEST
    }
}

/// 连续退出的滑动窗口统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrashStats {
    /// 最近 [`EXIT_STORM_WINDOW_SECS`] 秒内退出了几次。
    pub recent_exits: u32,
    /// 已连续重启的次数（用于退避递增）。
    pub consecutive: u32,
    /// 已回滚次数。
    pub rollback_count: u32,
}

impl CrashStats {
    /// 记一次退出，返回更新后的统计。
    pub fn record_exit(&mut self, info: &ExitInfo) -> Self {
        let _ = info;
        self.recent_exits += 1;
        self.consecutive += 1;
        *self
    }

    /// 窗口滑出后重置（由调用方按时间驱动）。
    pub fn window_elapsed(&mut self) {
        self.recent_exits = 0;
    }

    /// 是否已进入「反复退出、不可恢复」状态。
    pub fn is_exit_storm(&self) -> bool {
        self.recent_exits >= EXIT_STORM_THRESHOLD
    }
}

/// supervisor 的下一步动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 没有升级在进行，正常（或退避后）拉起 worker。
    Spawn {
        /// 退避秒数；0 表示立即拉起。
        backoff_secs: u64,
        /// 人类可读的理由，进日志与状态文件。
        reason: String,
    },
    /// 替换二进制并拉起新版。
    ReplaceAndSpawn { version: String, reason: String },
    /// 换回备份并拉起旧版。
    Rollback { reason: String },
    /// 回滚后仍失败 / 状态自相矛盾——**停下等人**。
    Stop { reason: String },
}

/// 决策所需的全部输入。
///
/// `backup_available` 是**文件系统事实**（备份文件此刻是否真的在），
/// 由 executor 读出来传进来，而不是让 `decide` 自己去 stat——
/// `decide` 必须保持纯函数，而「能不能回滚」取决于文件在不在，
/// 不取决于状态文件里那个路径字符串长什么样。
///
/// 这条区分是 D2-⑥ 的直接后果：`reconcile` 判定 `AlreadyReplaced` 的前提
/// 就是「备份已被 rename 消耗、不复存在」，此时若还按字符串非空就判「可回滚」，
/// 会驱动一次指向不存在文件的回滚。
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub exit: ExitInfo,
    pub state: Option<&'a UpdateState>,
    pub stats: CrashStats,
    pub reconcile: Reconcile,
    /// 备份文件此刻是否真实存在。
    pub backup_available: bool,
}

/// 决定 supervisor 下一步做什么。
///
/// **这是整个升级流程的决策中枢，也是 100% 覆盖率门禁的主要受益者**——
/// 全部可达分支都在这里，不涉及任何真实副作用。
///
/// ⚠️ **顺序至关重要**（D2-①）：先看状态、再判 fast_crash。
/// REX 的顺序相反，于是新版本秒崩时 supervisor 直接 `exit`——**永不回滚，设备变砖**。
pub fn decide(i: Inputs<'_>) -> Decision {
    let Inputs {
        exit,
        state,
        stats,
        reconcile,
        backup_available,
    } = i;

    // ---- 启动对账优先：崩溃在 rename 之后、状态写入之前的情况 ----
    match reconcile {
        Reconcile::Inconsistent => {
            return Decision::Stop {
                reason: "update state inconsistent: staged binary missing but backup present; \
                         refusing to guess"
                    .to_string(),
            };
        }
        Reconcile::AlreadyReplaced => {
            // 替换已完成、只是状态没写上。继续观察新版健康状况。
            //
            // ⚠️ 此处**不能**只按状态文件里 `backup_path` 非空就判「可回滚」：
            // `AlreadyReplaced` 的判定前提恰恰是备份已被 rename 消耗、不复存在。
            // 按字符串判会驱动一次指向不存在文件的回滚 —— 这就是
            // `backup_available` 必须由 executor 传入真实文件系统事实的原因。
            return match state {
                Some(s) if s.rollback_count == 0 && backup_available => Decision::Rollback {
                    reason: format!(
                        "new version {} did not come up after replacement; rolling back",
                        s.version
                    ),
                },
                Some(s) => Decision::Spawn {
                    backoff_secs: 0,
                    reason: format!(
                        "update to {} already applied; starting worker to verify",
                        s.version
                    ),
                },
                None => Decision::Spawn {
                    backoff_secs: 0,
                    reason: "replaced binary present but no state file; starting worker"
                        .to_string(),
                },
            };
        }
        Reconcile::Nothing | Reconcile::ContinueReplace => {}
    }

    // ---- worker 明确请求升级：替换二进制 ----
    if exit.is_update_request() {
        return match state {
            Some(s) if s.phase.in_flight() && s.backup_usable_for() => Decision::ReplaceAndSpawn {
                version: s.version.clone(),
                reason: "worker requested update; backing up then replacing".to_string(),
            },
            Some(s) if s.phase.in_flight() => Decision::Stop {
                reason: format!(
                    "update to {} requested but backup is unusable (generation mismatch or \
                     already rolled back); refusing to swap onto an unknown binary",
                    s.version
                ),
            },
            // exit(10) 却没有状态文件：worker 没按协议写文件就退了。
            // 这不是可恢复的正常路径——按崩溃处理并要求人介入。
            _ => Decision::Stop {
                reason: "worker exited with update request but wrote no update-state.json"
                    .to_string(),
            },
        };
    }

    // ---- 有升级在途：新版本不健康 → 回滚，**绝不 exit**（D2-①） ----
    if let Some(s) = state {
        if s.phase.in_flight() {
            // 三条同时成立才敢回滚：没回滚过、世代号对得上、**且备份文件此刻真在**。
            if s.rollback_count > 0 || !s.backup_usable_for() || !backup_available {
                return Decision::Stop {
                    reason: format!(
                        "update to {} failed and backup is no longer usable; old version is \
                         broken too, manual intervention required",
                        s.version
                    ),
                };
            }
            return Decision::Rollback {
                reason: format!(
                    "update to {} in phase {} did not produce a healthy worker; rolling back",
                    s.version, s.phase
                ),
            };
        }
    }

    // ---- 反复退出（任何退出码）：不可恢复 → 回滚或停止 ----
    if stats.is_exit_storm() {
        return match state {
            Some(s) if s.phase.in_flight() => Decision::Rollback {
                reason: format!(
                    "worker exited {} times within {}s during update to {}; rolling back",
                    EXIT_STORM_THRESHOLD, EXIT_STORM_WINDOW_SECS, s.version
                ),
            },
            _ => Decision::Stop {
                reason: format!(
                    "worker exited {} times within {}s (last code {}, ran {}s); not restarting \
                     in a loop",
                    EXIT_STORM_THRESHOLD, EXIT_STORM_WINDOW_SECS, exit.code, exit.ran_for_secs
                ),
            },
        };
    }

    // ---- 启动即崩且没有升级在途：配置错误，重试无意义 ----
    if exit.is_fast_crash() {
        return Decision::Stop {
            reason: format!(
                "worker exited with code {} after {}s and no update is in flight; treating as a \
                 configuration error rather than restarting forever",
                exit.code, exit.ran_for_secs
            ),
        };
    }

    // ---- 兜底：退避后重拉 ----
    Decision::Spawn {
        backoff_secs: backoff_secs(stats.consecutive),
        reason: if exit.is_normal() {
            "worker exited normally; restarting".to_string()
        } else {
            format!(
                "worker exited with code {} after {}s; restarting with backoff",
                exit.code, exit.ran_for_secs
            )
        },
    }
}

/// 退避时长：随连续重启次数递增，封顶 30s。
pub fn backoff_secs(consecutive: u32) -> u64 {
    (2u64.saturating_pow(consecutive.min(5))).min(30)
}

/// 把决策落到状态文件上（供 executor 写回）。
///
/// 单独抽出来是为了让「决策 → 状态」的映射也能纯测，
/// 而不是散在 executor 的分支里被覆盖率漏掉。
pub fn next_state(current: Option<&UpdateState>, d: &Decision) -> Option<UpdateState> {
    let next = current.cloned();
    match d {
        Decision::ReplaceAndSpawn { version, .. } => {
            let gen = current.map_or(1, |x| x.generation);
            let mut next = next.unwrap_or_else(|| blank(version, gen));
            next.phase = UpdatePhase::StartingNew;
            next.version = version.clone();
            Some(next)
        }
        Decision::Rollback { .. } => {
            // 没有既有状态就无从回滚（executor 不会走到这条路径）
            let mut next = next?;
            next.phase = UpdatePhase::RollingBack;
            next.rollback_count += 1;
            Some(next)
        }
        Decision::Spawn { .. } => Some(next.unwrap_or_else(|| UpdateState {
            phase: UpdatePhase::Idle,
            version: String::new(),
            staged_path: String::new(),
            backup_path: String::new(),
            generation: 0,
            rollback_count: 0,
        })),
        Decision::Stop { .. } => {
            let mut next = next?;
            next.phase = UpdatePhase::Failed;
            Some(next)
        }
    }
}

/// 决策 → 状态。路径字段**沿用已有状态**，不由这里猜——
/// 真实路径由 executor 在执行替换时算出，决策层不该凭空造。
fn blank(version: &str, generation: u64) -> UpdateState {
    UpdateState {
        phase: UpdatePhase::Requested,
        version: version.to_string(),
        staged_path: String::new(),
        backup_path: String::new(),
        generation,
        rollback_count: 0,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::let_underscore_must_use,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn st() -> UpdateState {
        UpdateState::requested("2.0.0", &PathBuf::from("/s"), &PathBuf::from("/b"), 1)
    }

    fn call(
        exit: ExitInfo,
        state: Option<&UpdateState>,
        stats: CrashStats,
        reconcile: Reconcile,
    ) -> Decision {
        call_with_backup(exit, state, stats, reconcile, true)
    }

    fn call_with_backup(
        exit: ExitInfo,
        state: Option<&UpdateState>,
        stats: CrashStats,
        reconcile: Reconcile,
        backup_available: bool,
    ) -> Decision {
        decide(Inputs {
            exit,
            state,
            stats,
            reconcile,
            backup_available,
        })
    }

    #[test]
    fn healthy_restart_gets_backoff_that_grows() {
        let d = call(
            ExitInfo::new(12, 30),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Spawn {
                backoff_secs: b, ..
            } => assert_eq!(b, super::backoff_secs(0)),
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert_eq!(backoff_secs(0), 1);
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(5), 30, "capped");
        assert_eq!(backoff_secs(99), 30, "power must not overflow");
    }

    #[test]
    fn normal_exit_restarts_with_a_reason() {
        let d = call(
            ExitInfo::new(0, 60),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Spawn { reason, .. } => assert!(reason.contains("normally"), "{reason}"),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn crash_exit_mentions_the_code() {
        let d = call(
            ExitInfo::new(12, 60),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Spawn { reason, .. } => {
                assert!(reason.contains('1'), "code 12 should appear: {reason}")
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// D2-① 的核心回归：REX 因为 fast_crash 判定在读状态之前，
    /// 新版本秒崩时直接 exit——**永不回滚，设备变砖**。
    /// 我们必须回滚。
    #[test]
    fn fast_crash_during_update_rolls_back_instead_of_stopping() {
        let s = st();
        let d = call(
            ExitInfo::new(12, 1),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
        );
        assert!(
            matches!(d, Decision::Rollback { .. }),
            "a fast crash of a new version must roll back, not stop: {d:?}"
        );
    }

    /// 同上，但跑满 5s 才崩——也不该 stop，因为升级在途。
    #[test]
    fn slow_crash_during_update_also_rolls_back() {
        let s = st();
        let d = call(
            ExitInfo::new(12, 30),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
        );
        assert!(matches!(d, Decision::Rollback { .. }), "{d:?}");
    }

    /// D2-① 的另一半：没有升级在途时，秒崩**才**是配置错误，可以停。
    #[test]
    fn fast_crash_without_update_stops_as_configuration_error() {
        let d = call(
            ExitInfo::new(12, 1),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Stop { reason } => {
                assert!(reason.contains("configuration"), "{reason}");
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn clean_immediate_exit_is_not_treated_as_fast_crash() {
        // code 0 且 <5s —— 不是崩溃，但配合退出风暴会走另一条路
        let info = ExitInfo::new(0, 1);
        assert!(!info.is_fast_crash());
        assert!(info.is_normal());
    }

    /// D2-②：退出码 0 的热循环必须也能触发停止，不能无限 1s 重生。
    #[test]
    fn exit_storm_of_clean_exits_stops_instead_of_looping() {
        let stats = CrashStats {
            recent_exits: 3,
            consecutive: 3,
            rollback_count: 0,
        };
        let d = call(ExitInfo::new(0, 1), None, stats, Reconcile::Nothing);
        match d {
            Decision::Stop { reason } => {
                assert!(reason.contains('3'), "{reason}");
                assert!(reason.contains('1'), "last ran_for should appear: {reason}");
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn exit_storm_during_update_rolls_back() {
        let s = st();
        let stats = CrashStats {
            recent_exits: 3,
            consecutive: 3,
            rollback_count: 0,
        };
        let d = call(ExitInfo::new(12, 30), Some(&s), stats, Reconcile::Nothing);
        assert!(matches!(d, Decision::Rollback { .. }), "{d:?}");
    }

    #[test]
    fn below_threshold_storm_is_not_a_storm() {
        let stats = CrashStats {
            recent_exits: 2,
            consecutive: 2,
            rollback_count: 0,
        };
        let d = call(ExitInfo::new(12, 30), None, stats, Reconcile::Nothing);
        assert!(matches!(d, Decision::Spawn { .. }), "{d:?}");
    }

    #[test]
    fn window_elapsed_clears_the_storm_counter() {
        let mut s = CrashStats {
            recent_exits: 5,
            consecutive: 5,
            rollback_count: 0,
        };
        assert!(s.is_exit_storm());
        s.window_elapsed();
        assert!(!s.is_exit_storm());
        assert_eq!(s.consecutive, 5, "backoff still accumulates");
    }

    #[test]
    fn record_exit_counts_both_windows() {
        let mut s = CrashStats::default();
        s.record_exit(&ExitInfo::new(0, 1));
        assert_eq!(s.recent_exits, 1);
        assert_eq!(s.consecutive, 1);
        s.record_exit(&ExitInfo::new(0, 1));
        assert_eq!(s.recent_exits, 2);
        assert!(!s.is_exit_storm());
    }

    #[test]
    fn update_request_with_valid_state_replaces() {
        let s = st();
        let d = call(
            ExitInfo::new(exit_code::UPDATE_REQUEST, 2),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::ReplaceAndSpawn { version, .. } => assert_eq!(version, "2.0.0"),
            other => panic!("expected ReplaceAndSpawn, got {other:?}"),
        }
    }

    /// D2-③：备份不可用时**拒绝替换**——不能拿一个来历不明的二进制去换。
    #[test]
    fn update_request_with_unusable_backup_stops() {
        let mut s = st();
        s.generation = 0;
        let d = call(
            ExitInfo::new(exit_code::UPDATE_REQUEST, 2),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Stop { reason } => {
                assert!(reason.contains("backup is unusable"), "{reason}")
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    /// exit(10) 却没写状态文件：worker 没按协议来，不能猜。
    #[test]
    fn update_request_without_state_stops() {
        let d = call(
            ExitInfo::new(exit_code::UPDATE_REQUEST, 2),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Stop { reason } => {
                assert!(reason.contains("no update-state.json"), "{reason}")
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    /// D2-⑤：回滚过一次后旧版也不健康 → 停下等人，不无限「回滚 → 失败 → 回滚」。
    #[test]
    fn failure_after_rollback_stops_for_a_human() {
        let mut s = st();
        s.rollback_count = 1;
        let d = call(
            ExitInfo::new(12, 30),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
        );
        match d {
            Decision::Stop { reason } => {
                assert!(reason.contains("manual intervention"), "{reason}")
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    /// D2-⑥：崩溃在 rename 之后、状态写入之前 → 认定已替换，继续验证，
    /// **不得误回滚一次已经成功的升级**。
    ///
    /// 注意这里必须传 `backup_available: false`——`AlreadyReplaced` 的判定前提
    /// 就是备份已被 rename 消耗、不复存在。若此时仍按状态文件里 `backup_path`
    /// 非空就判「可回滚」，会驱动一次指向不存在文件的回滚。
    #[test]
    fn already_replaced_starts_worker_instead_of_rolling_back() {
        let s = st();
        let d = call_with_backup(
            ExitInfo::new(12, 30),
            Some(&s),
            CrashStats::default(),
            Reconcile::AlreadyReplaced,
            false,
        );
        match d {
            Decision::Spawn { reason, .. } => {
                assert!(reason.contains("already applied"), "{reason}")
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// 已经替换完、备份也真的还在、且这一版确实不健康 → 这时才回滚。
    #[test]
    fn already_replaced_with_real_backup_and_bad_worker_rolls_back() {
        let s = st();
        let d = call_with_backup(
            ExitInfo::new(12, 30),
            Some(&s),
            CrashStats::default(),
            Reconcile::AlreadyReplaced,
            true,
        );
        assert!(matches!(d, Decision::Rollback { .. }), "{d:?}");
    }

    #[test]
    fn already_replaced_without_state_starts_worker() {
        let d = call(
            ExitInfo::new(0, 10),
            None,
            CrashStats::default(),
            Reconcile::AlreadyReplaced,
        );
        match d {
            Decision::Spawn { reason, .. } => {
                assert!(reason.contains("no state file"), "{reason}")
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn inconsistent_reconcile_stops_before_anything_else() {
        let s = st();
        let d = call(
            ExitInfo::new(exit_code::UPDATE_REQUEST, 2),
            Some(&s),
            CrashStats::default(),
            Reconcile::Inconsistent,
        );
        match d {
            Decision::Stop { reason } => assert!(reason.contains("inconsistent"), "{reason}"),
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn continue_replace_falls_through_to_update_request_handling() {
        let s = st();
        let d = call(
            ExitInfo::new(exit_code::UPDATE_REQUEST, 2),
            Some(&s),
            CrashStats::default(),
            Reconcile::ContinueReplace,
        );
        assert!(matches!(d, Decision::ReplaceAndSpawn { .. }), "{d:?}");
    }

    #[test]
    fn exit_code_constants_are_stable() {
        assert_eq!(exit_code::NORMAL, 0);
        assert_eq!(exit_code::UPDATE_REQUEST, 10);
        assert_eq!(exit_code::HEALTH_FAILURE, 11);
        assert_eq!(exit_code::CRASH, 12);
    }

    // ---- next_state：决策 → 状态的映射 ----

    #[test]
    fn next_state_after_replace_marks_starting_new() {
        let s = st();
        let d = Decision::ReplaceAndSpawn {
            version: "3.0.0".into(),
            reason: String::new(),
        };
        let n = next_state(Some(&s), &d).unwrap();
        assert_eq!(n.phase, UpdatePhase::StartingNew);
        assert_eq!(n.version, "3.0.0");
    }

    #[test]
    fn next_state_after_rollback_counts_and_marks() {
        let s = st();
        let d = Decision::Rollback {
            reason: String::new(),
        };
        let n = next_state(Some(&s), &d).unwrap();
        assert_eq!(n.phase, UpdatePhase::RollingBack);
        assert_eq!(n.rollback_count, 1);
    }

    #[test]
    fn next_state_rollback_without_prior_state_is_none() {
        let d = Decision::Rollback {
            reason: String::new(),
        };
        assert!(next_state(None, &d).is_none());
    }

    #[test]
    fn next_state_spawn_without_prior_state_is_idle() {
        let d = Decision::Spawn {
            backoff_secs: 1,
            reason: String::new(),
        };
        let n = next_state(None, &d).unwrap();
        assert_eq!(n.phase, UpdatePhase::Idle);
        assert_eq!(n.generation, 0);
    }

    #[test]
    fn next_state_stop_marks_failed() {
        let s = st();
        let d = Decision::Stop {
            reason: String::new(),
        };
        let n = next_state(Some(&s), &d).unwrap();
        assert_eq!(n.phase, UpdatePhase::Failed);
        assert!(n.needs_human());
    }

    #[test]
    fn next_state_stop_without_prior_state_is_none() {
        let d = Decision::Stop {
            reason: String::new(),
        };
        assert!(next_state(None, &d).is_none());
    }

    #[test]
    fn next_state_after_spawn_keeps_existing_state() {
        let s = st();
        let d = Decision::Spawn {
            backoff_secs: 0,
            reason: String::new(),
        };
        let n = next_state(Some(&s), &d).unwrap();
        assert_eq!(n.version, "2.0.0", "must not clobber the recorded version");
    }
}
