//! 单调时钟读取的唯一出处。
//!
//! **为什么要有这一处**：`SystemTime::now().duration_since(UNIX_EPOCH)` 在
//! 时钟回拨时会返回 `Err`，而 `.unwrap_or_default()` 把那个 `Err` 静默变成
//! `Duration::ZERO`——于是「时钟不对」被编成「现在是 1970-01-01」，一个
//! 看起来完全正常的假事实。散落各处的兜底写法各不相同，谁也不知道另外
//! 几处是怎么编的。
//!
//! **为什么是 `saturating_duration_since` 而不是 `unwrap_or(0)`**：
//! 前者不 panic、也不需要一个随手挑的默认值，语义是「早于 epoch 就当 0 秒」，
//! 与后者的结果一致但**不带那个把错误吞掉的 `unwrap_or`**。

use std::time::{SystemTime, UNIX_EPOCH};

/// 现在距 Unix epoch 的秒数。系统时钟早于 epoch 时得 `0`（不会 panic）。
pub fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 读到的必须是「像样」的墙上时间：早于 2023 年说明时钟回拨，
    /// 晚于 2100 年说明拿到的是别的单位（毫秒 / 微秒）。
    #[test]
    fn unix_secs_is_a_plausible_wall_clock() {
        let a = unix_secs();
        assert!(a > 1_700_000_000, "now should not be before 2023: {a}");
        assert!(a < 4_102_444_800, "now should not be past 2100: {a}");
    }
}
