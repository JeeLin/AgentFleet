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
