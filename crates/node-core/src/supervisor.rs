//! supervisor 的决策逻辑——纯函数，不碰进程与文件系统；executor 在
//! `node-cli/src/supervisor.rs`。
//!
//! 拆开是因为参考实现 REX 把判断内联在阻塞循环里，没有覆盖率门禁所以无所谓；
//! 本项目有 100% 阻断门禁，而那种形状的代码没法测——覆盖一个分支就得真的
//! spawn 子进程、真的删掉正在运行的二进制（`DECISIONS.md` D2）。
//!
//! `Decision` 带 `reason`，因为 supervisor 要写进日志与状态文件：下一次崩溃时
//! 只有日志能解释上一次为什么回滚了。

use crate::update::{Reconcile, UpdateState};

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
    ///
    /// `info` 不参与计数：判据全在 `ExitInfo` 的方法与调用方持有的
    /// `stats` 上（退避、`is_exit_storm` 各自按需读退出信息），
    /// 这里只管「又退了一次」。
    pub fn record_exit(&mut self, _info: &ExitInfo) -> Self {
        self.recent_exits += 1;
        self.consecutive += 1;
        *self
    }

    /// 窗口滑出后重置（由调用方按时间驱动）。
    pub fn window_elapsed(&mut self) {
        self.recent_exits = 0;
    }

    /// 这一次退出**不算崩溃循环**：退避回落，连击清零。
    ///
    /// 判据是「跑满了 [`EXIT_STORM_WINDOW_SECS`]」而不是「没触发 fast_crash」：
    /// 跑满 10s 才退出的 worker 可能是正常下班，也可能是一点点坏掉，但两种
    /// 都不该让此后的每次重启都背 30s。而真正「反复退出」的形态——秒崩、
    /// 热循环——必然跑不满这个时长，于是完全不受这条影响。
    pub fn ran_stably(&mut self) {
        self.consecutive = 0;
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

    // ---- 这一版 worker 不健康 → 有 old 就回滚，没有才停 ----
    //
    // 判据是**有没有 old**，不是 `phase.in_flight()`：后者会漏掉「升级已
    // `Committed`、新版本此时才开始反复崩溃」——`Committed` 是终态，备份却
    // 还在盘上，按 phase 判会把设备留在坏版本里。
    //
    // 回滚也失败的情况无需单列：回滚一次后 `rollback_count >= 1` 使
    // `backup_usable_for()` 为假，同一个判断自然落到 Stop（D2-⑤）。
    let rollback_possible = matches!(state, Some(s) if s.backup_usable_for()) && backup_available;

    let bad_version = stats.is_exit_storm()
        || exit.is_fast_crash()
        || exit.code == exit_code::HEALTH_FAILURE
        || matches!(state, Some(s) if s.phase.in_flight());

    if bad_version {
        let version_hint = state.map_or_else(|| "unknown".to_string(), |s| s.version.clone());
        let why = if stats.is_exit_storm() {
            format!(
                "worker exited {} times within {}s (last code {}, ran {}s)",
                EXIT_STORM_THRESHOLD, EXIT_STORM_WINDOW_SECS, exit.code, exit.ran_for_secs
            )
        } else if exit.is_fast_crash() {
            format!(
                "worker exited with code {} after {}s",
                exit.code, exit.ran_for_secs
            )
        } else {
            format!(
                "worker exited (code {}) while update to {version_hint} was unfinished",
                exit.code
            )
        };
        return if rollback_possible {
            Decision::Rollback {
                reason: format!("{why}; old version is still on disk, rolling back to it"),
            }
        } else {
            Decision::Stop {
                reason: if exit.is_fast_crash() && !stats.is_exit_storm() {
                    format!(
                        "{why}; treating as a configuration error rather than restarting \
                         forever, and no usable old version is left to fall back to"
                    )
                } else {
                    format!("{why}; no usable old version left, manual intervention required")
                },
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::UpdatePhase;
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
        assert!(
            matches!(d, Decision::Spawn { backoff_secs: b, .. } if b == super::backoff_secs(0)),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Spawn { ref reason, .. } if reason.contains("normally")),
            "got {d:?}"
        );
    }

    #[test]
    fn crash_exit_mentions_the_code() {
        let d = call(
            ExitInfo::new(12, 60),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        assert!(
            matches!(d, Decision::Spawn { ref reason, .. } if reason.contains('1')),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Stop { ref reason } if reason.contains("configuration")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Stop { ref reason } if reason.contains('3') && reason.contains('1')),
            "got {d:?}"
        );
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

    /// 升级**已经 `Committed`**（终态、`in_flight()` 为假）之后新版本才开始
    /// 反复崩溃——备份明明还在盘上，必须回滚。
    ///
    /// 这条是判据从「`in_flight()`」换成「有没有 old」的直接理由：
    /// `Committed` 是终态，按旧判据这里会返回 `Stop`，把设备永久留在
    /// 反复崩溃的坏版本上，而可用的旧版本就在旁边躺着。
    #[test]
    fn exit_storm_after_the_update_committed_still_rolls_back() {
        let mut s = st();
        s.phase = UpdatePhase::Committed;
        assert!(!s.phase.in_flight(), "Committed 不是在途状态");
        let stats = CrashStats {
            recent_exits: 3,
            consecutive: 3,
            rollback_count: 0,
        };
        let d = call(ExitInfo::new(12, 30), Some(&s), stats, Reconcile::Nothing);
        assert!(
            matches!(d, Decision::Rollback { .. }),
            "committed 之后崩溃也该回滚，判据是有没有 old，不是 phase：{d:?}"
        );
    }

    /// `RolledBack`（已换回旧版、备份已被消耗）之后还崩溃 → 没有 old 可用，停下。
    /// 这条与上面那条配对，说明判据确实在「有没有 old」而不是「phase 像不像在途」。
    /// 跑满时长才退出 → 连击清零：此后的每次重启不该再背上递增的退避。
    #[test]
    fn a_worker_that_ran_stably_drops_its_consecutive_streak() {
        let mut stats = CrashStats {
            recent_exits: 4,
            consecutive: 7,
            rollback_count: 0,
        };
        stats.ran_stably();
        assert_eq!(stats.consecutive, 0);
        // 连击清零**不是**免死金牌：风暴判据看的是窗口内的退出次数，
        // 所以「跑稳了」不该把风暴记录一起抹掉。
        assert!(stats.is_exit_storm());
    }

    #[test]
    fn exit_storm_after_rollback_has_no_old_left_to_fall_back_on() {
        let mut s = st();
        s.phase = UpdatePhase::RolledBack;
        s.rollback_count = 1;
        let stats = CrashStats {
            recent_exits: 3,
            consecutive: 3,
            rollback_count: 1,
        };
        let d = call(ExitInfo::new(12, 30), Some(&s), stats, Reconcile::Nothing);
        assert!(matches!(d, Decision::Stop { .. }), "{d:?}");
    }

    /// `Committed` 阶段**秒崩**，而备份文件已被 rename 消耗 → 状态说能用、
    /// 文件系统说不存在，按事实停。
    ///
    /// 这里必须用 fast crash：跑满时长才崩的普通崩溃不是「这一版不健康」
    /// 的信号，正常重启即可——判据收窄到信号本身，否则会误停。
    #[test]
    fn committed_fast_crash_without_the_backup_file_stops() {
        let mut s = st();
        s.phase = UpdatePhase::Committed;
        let d = call_with_backup(
            ExitInfo::new(12, 1),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
            false,
        );
        assert!(matches!(d, Decision::Stop { .. }), "{d:?}");
    }

    /// 与上一条对照：`Committed` 之后一次**普通时长的崩溃**不是不健康信号，
    /// 没有 old 可退也不该停——停了设备就永远起不来了。
    #[test]
    fn committed_slow_crash_restarts_even_without_a_backup() {
        let mut s = st();
        s.phase = UpdatePhase::Committed;
        let d = call_with_backup(
            ExitInfo::new(12, 30),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
            false,
        );
        assert!(matches!(d, Decision::Spawn { .. }), "{d:?}");
    }

    /// worker 自认不健康（`exit(11)`）也算「这一版不健康」，不是普通崩溃。
    /// 没有 old 可退时停下，而不是无限重拉同一个坏版本。
    #[test]
    fn health_failure_without_old_stops_instead_of_respawning() {
        let d = call(
            ExitInfo::new(exit_code::HEALTH_FAILURE, 30),
            None,
            CrashStats::default(),
            Reconcile::Nothing,
        );
        assert!(matches!(d, Decision::Stop { .. }), "{d:?}");
    }

    /// `exit(11)` 但备份还在 → 同样回滚，不因退出码不同就区别对待。
    #[test]
    fn health_failure_with_old_rolls_back() {
        let s = st();
        let d = call(
            ExitInfo::new(exit_code::HEALTH_FAILURE, 30),
            Some(&s),
            CrashStats::default(),
            Reconcile::Nothing,
        );
        assert!(matches!(d, Decision::Rollback { .. }), "{d:?}");
    }

    /// 「反复退出」在**有** old 时优先回滚——回滚是一次性动作，
    /// 换回旧版比直接停下更有机会把设备救回来。
    #[test]
    fn exit_storm_prefers_rollback_over_stop_when_old_exists() {
        let s = st();
        let stats = CrashStats {
            recent_exits: 5,
            consecutive: 5,
            rollback_count: 0,
        };
        let d = call(ExitInfo::new(0, 1), Some(&s), stats, Reconcile::Nothing);
        assert!(
            matches!(d, Decision::Rollback { ref reason }
                if reason.contains(&format!("{EXIT_STORM_THRESHOLD} times")) && reason.contains("old version")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::ReplaceAndSpawn { ref version, .. } if version == "2.0.0"),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Stop { ref reason } if reason.contains("backup is unusable")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Stop { ref reason } if reason.contains("no update-state.json")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Stop { ref reason } if reason.contains("manual intervention")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Spawn { ref reason, .. } if reason.contains("already applied")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Spawn { ref reason, .. } if reason.contains("no state file")),
            "got {d:?}"
        );
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
        assert!(
            matches!(d, Decision::Stop { ref reason } if reason.contains("inconsistent")),
            "got {d:?}"
        );
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
}
