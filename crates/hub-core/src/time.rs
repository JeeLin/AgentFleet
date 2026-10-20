//! 单调时钟读取的唯一出处。
//!
//! **为什么要有这一处**：`SystemTime::now().duration_since(UNIX_EPOCH)` 在
//! 时钟回拨时会返回 `Err`，而 `.unwrap_or_default()` 把那个 `Err` 静默变成
//! `Duration::ZERO`——于是「时钟不对」被编成「现在是 1970-01-01」，一个
//! 看起来完全正常的假事实。三处各自手写这个兜底时，谁也不知道另外两处
//! 是怎么编的。
//!
//! **为什么是 `saturating_duration_since` 而不是 `unwrap_or(0)`**：
//! 前者不 panic、也不需要一个随手挑的默认值，语义是「早于 epoch 就当 0 秒」，
//! 与后者的结果一致但**不带那个把错误吞掉的 `unwrap_or`**。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 现在距 Unix epoch 的秒数。系统时钟早于 epoch 时得 `0`（不会 panic）。
pub fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 现在距 Unix epoch 的时长。系统时钟早于 epoch 时得 `Duration::ZERO`。
pub fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 时钟正常时三处读到的值应当一致——它们是同一个时钟，不是三个时钟。
    #[test]
    fn both_read_the_same_clock() {
        let a = unix_secs();
        let b = since_epoch().as_secs() as i64;
        assert_eq!(a, b, "两个函数读的是同一个 now()，不该差出秒级");
        assert!(a > 1_700_000_000, "现在不该早于 2023 年：{a}");
    }
}
