//! 掉线重连的退避与抖动（子任务 6，`protocol.md` §7.0）。
//!
//! `conn.rs` 只回答「连一次会发生什么」；这里是包在它外面的循环要用的时间策略。
//! 纯函数，没有副作用——退避策略是最容易写错又最难在真机上复现的部分。

use std::time::Duration;

use rand::Rng;

use crate::conn::ConnOutcome;

/// 退避上限（秒）。公式 `min(30s, 2^attempt)` 的封顶值。
pub const MAX_BACKOFF_SECS: u64 = 30;

/// 抖动系数下界。
pub const JITTER_MIN: f64 = 0.7;

/// 抖动系数上界。
pub const JITTER_MAX: f64 = 1.3;

/// 指数退避基数 `2^attempt`，封顶 [`MAX_BACKOFF_SECS`]。
///
/// 指数封在 6（2^6=64 → 30s）：`saturating_pow` 让 `attempt` 是 u32::MAX 也不溢出，
/// 否则一次连续失败就能把守护进程算进溢出陷阱里。
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt).min(MAX_BACKOFF_SECS))
}

/// 抖动系数：均匀分布到 `[JITTER_MIN, JITTER_MAX]`。
///
/// **为什么必须有抖动**：同机两个进程（或重启后残留的旧进程）同时重连时，
/// 若退避都从 1s 起且无抖动，两次尝试会几乎同时打到 Hub → A 顶掉 B、
/// B 顶掉 A、各自再等 1s → **完全同相**，两条连接永远交替胜出，稳定活锁。
pub fn jitter() -> f64 {
    rand::thread_rng().gen_range(JITTER_MIN..JITTER_MAX)
}

/// 带抖动的退避：`min(30s, 2^attempt) * uniform(0.7, 1.3)`。
///
/// `jitter_factor` 是入参而不是内部现抽：抖动**必须**能被断言——
/// 「连续 10 次间隔的极差 ≥ 0.2s」这种测试不能靠运气（标准①）。
pub fn backoff_with_jitter(attempt: u32, jitter_factor: f64) -> Duration {
    let secs = backoff(attempt).as_secs_f64() * jitter_factor;
    // `Duration::from_secs_f64` 对负数 / NaN / 无穷大会 panic。抖动系数越界时
    // 退化成 0 而不是让无人值守的守护进程崩掉。
    Duration::from_secs_f64(if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        0.0
    })
}

/// 这次连接失败之后要不要重连、退避多久。
///
/// **只有传输层失败才退避重连**：
/// - `Superseded` 是终态（§7.1）——再连一次就是和新连接互抢；
/// - `Rejected` 是 Hub 对身份的明确裁决，重试一万次结果一样（§7.1），
///   该做的是停下来让人看见，而不是把设备刷成日志噪声。
///
/// 握手成功（`Ok`）不在这里出现：那条路直接进消息循环。
/// 但 `Authed` 变体仍被显式列出——合并进终态那两臂会让将来给
/// `ConnOutcome` 加新变体时，编译器不再逼着人想一遍「它该不该重连」。
pub fn retry_after(outcome: &ConnOutcome, attempt: u32, jitter_factor: f64) -> Option<Duration> {
    match outcome {
        ConnOutcome::Transport { .. } => Some(backoff_with_jitter(attempt, jitter_factor)),
        ConnOutcome::Superseded { .. }
        | ConnOutcome::Rejected { .. }
        | ConnOutcome::Authed { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> ConnOutcome {
        ConnOutcome::Transport {
            detail: "connection refused".into(),
        }
    }

    fn authed() -> ConnOutcome {
        ConnOutcome::Authed {
            node_id: "n_1".into(),
            protocol_version: 1,
            gen: 1,
            heartbeat_interval_ms: 15_000,
            enrolled: true,
        }
    }

    /// `min(30s, 2^attempt)`：逐步加倍，封在 30s 不再涨。
    #[test]
    fn backoff_doubles_then_saturates() {
        assert_eq!(backoff(0).as_secs(), 1);
        assert_eq!(backoff(1).as_secs(), 2);
        assert_eq!(backoff(2).as_secs(), 4);
        assert_eq!(backoff(3).as_secs(), 8);
        assert_eq!(backoff(4).as_secs(), 16);
        assert_eq!(backoff(5).as_secs(), 30, "2^5=32 越过上限，截到 30");
        for a in [6u32, 10, 31, 64] {
            assert_eq!(backoff(a).as_secs(), 30);
        }
    }

    /// 一次连续失败不该把守护进程算进溢出陷阱。
    #[test]
    fn backoff_does_not_overflow_on_a_huge_attempt_count() {
        assert_eq!(backoff(u32::MAX).as_secs(), MAX_BACKOFF_SECS);
    }

    /// 抖动是**多退少补**，不是单向放大——只放大会让两台设备错开后又对齐。
    #[test]
    fn jitter_stays_inside_the_declared_band() {
        for _ in 0..200 {
            let j = jitter();
            assert!(
                (JITTER_MIN..JITTER_MAX).contains(&j),
                "jitter {j} escaped [{JITTER_MIN}, {JITTER_MAX})"
            );
        }
    }

    /// 抖动**必须**能被断言（标准①），所以它是入参：这里锁死「乘上去」而不是乘 1。
    #[test]
    fn backoff_with_jitter_scales_the_base_delay() {
        assert_eq!(backoff_with_jitter(0, 1.0).as_secs_f64(), 1.0);
        assert_eq!(backoff_with_jitter(3, 1.3).as_secs_f64(), 10.4);
        assert_eq!(backoff_with_jitter(3, 0.7).as_secs_f64(), 5.6);
    }

    /// 同机两个进程同相重连会稳定活锁，所以随机抖动的极差必须够宽。
    #[test]
    fn repeated_samples_spread_out_in_time() {
        let spread = (0..10).fold((f64::MAX, f64::MIN), |(lo, hi), _| {
            let d = backoff_with_jitter(2, jitter()).as_secs_f64();
            (lo.min(d), hi.max(d))
        });
        assert!(spread.1 - spread.0 >= 0.2, "spread was {:?}", spread);
    }

    /// 越界的抖动系数不能让无人值守的守护进程 panic（`from_secs_f64` 会）。
    #[test]
    fn a_bogus_jitter_factor_degrades_to_zero_delay() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert_eq!(backoff_with_jitter(2, bad), Duration::ZERO, "{bad}");
        }
    }

    /// 只有传输层失败才退避重连。
    #[test]
    fn transport_failures_are_retried() {
        let d = retry_after(&transport(), 1, 1.0).expect("must retry");
        assert_eq!(d.as_secs(), 2);
    }

    /// §7.1：`superseded` 是终态。再连一次就是和新连接互抢。
    #[test]
    fn superseded_is_terminal() {
        let s = ConnOutcome::Superseded { gen: 9 };
        assert!(retry_after(&s, 1, 1.0).is_none());
    }

    /// `Rejected` 是 Hub 的明确裁决，重试一万次结果一样。
    #[test]
    fn a_rejected_identity_is_terminal() {
        let r = ConnOutcome::Rejected {
            reason: "identity mismatch".into(),
        };
        assert!(retry_after(&r, 1, 1.0).is_none());
    }

    /// 握手成功不进这里，但变体必须显式列出：否则将来加新变体时
    /// 编译器不再逼人想一遍「它该不该重连」。
    #[test]
    fn an_authed_outcome_has_nothing_to_retry() {
        assert!(retry_after(&authed(), 1, 1.0).is_none());
    }
}
