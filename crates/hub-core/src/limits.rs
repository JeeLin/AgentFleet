//! Hub 的**读侧**硬上限（`protocol.md` §6.1）。
//!
//! - Hub 是单进程服务多设备，**一条连接失控不能拖垮其他人**。
//! - 限的是「单位时间读到的字节数」与「单帧大小」；超限不是断开重连
//!   （那会被恶意节点当放大器），而是**慢读**、持续 30s 才 `err{code:"flood"}` 断开。
//! - **只有读侧**：M0 的写侧全走 WebSocket 自身的背压，尚无独立队列
//!   （`protocol.md` §6.1 把写侧列成待办）。

pub mod error;
pub use error::LimitError;

use std::time::{Duration, Instant};

/// 单帧上限。**与协议同源**（§2）——两份数值各写一遍，漂移了没有任何东西会报错。
pub use protocol::frame::MAX_FRAME_BYTES;

/// 读侧令牌桶容量（字节）：允许的突发量。
///
/// 4× 单帧上限：允许一次 `cat` 大文件那样的洪峰，而不是第一帧大文件就被慢读。
const READ_BUCKET_CAPACITY: usize = 4 * MAX_FRAME_BYTES;

/// 读侧每秒补充的令牌（字节）：稳态速率上限。
const READ_REFILL_PER_SEC: usize = MAX_FRAME_BYTES;

/// 超过这个速率持续这么久就判定 flood（§6.1）。
pub const FLOOD_SUSTAINED: Duration = Duration::from_secs(30);

/// 令牌桶：读侧速率限制。
#[derive(Debug)]
pub struct ReadLimiter {
    capacity: usize,
    refill_per_sec: usize,
    tokens: usize,
    /// 不足 1 字节、还没进 `tokens` 的那部分余量。
    ///
    /// `tokens` 只能是整数，而「每秒 1 MiB」在毫秒级的读片上是小数；
    /// 每片各自截断的话，高频小额读的实际吞吐会低于标称。
    fractional: f64,
    last: Instant,
}

impl ReadLimiter {
    pub fn new() -> Self {
        Self::with_params(READ_BUCKET_CAPACITY, READ_REFILL_PER_SEC)
    }

    /// 构造非默认限额。**只给测试用**——生产路径一律 `new()`：
    /// 「默认路径」与「显式 new」走出两套限额时，线上限流器与文档各说各话。
    fn with_params(capacity: usize, refill_per_sec: usize) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    /// 尝试消耗 `n` 字节。
    ///
    /// 返回 `Ok(())` 表示放行；`Err` 表示已超限，调用方应回
    /// `err{code:"flood"}` 并断开。
    ///
    /// **单帧上限是固定的 `MAX_FRAME_BYTES`（1 MiB，§2），不是 `capacity`**：
    /// `capacity` 只是突发量（几秒的缓冲），把它当单帧上限会让突发量大的
    /// 部署意外接受超大帧。
    pub fn try_consume(&mut self, n: usize) -> Result<(), LimitError> {
        // 单帧超硬上限：无论桶里有多少令牌都不放行
        if n > MAX_FRAME_BYTES {
            return Err(LimitError::FrameTooLarge);
        }
        self.refill();
        if self.tokens < n {
            return Err(LimitError::Flood);
        }
        self.tokens -= n;
        Ok(())
    }

    /// 按时间补令牌。
    ///
    /// **这个 `gained > 0` 判断不能省**。`gained` 是一次截断（`as usize`），
    /// 不足 1 字节的时间片会被舍成 0；此时若照样推进 `last`，那段余数就永久
    /// 丢了——留在 `last` 之外，它下一次仍会参与累计。
    ///
    /// 把它当成「遗漏更新 `last`」来改是会付出代价的：去掉这个判断并把
    /// `self.last = now` 提到外面，高频小额读（终端滚动那种）的实测放行量
    /// 会从名义速率的 ~99% 掉到 **0**——每一帧的 `(now - last) * rate` 都
    /// 不足 1 字节，却被当成已结算清零。
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        let gained = (elapsed.as_secs_f64() * self.refill_per_sec as f64) as usize;
        if gained > 0 {
            self.tokens = (self.tokens + gained).min(self.capacity);
            self.last = now;
        }
    }
}

impl Default for ReadLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_capacity_passes() {
        let mut r = ReadLimiter::new();
        for _ in 0..10 {
            r.try_consume(64 * 1024).unwrap();
        }
    }

    #[test]
    fn burst_beyond_capacity_is_flood() {
        let mut r = ReadLimiter::with_params(1024, 0);
        r.try_consume(1024).unwrap();
        assert!(matches!(r.try_consume(1), Err(LimitError::Flood)));
    }

    #[test]
    fn single_frame_above_hard_limit_rejected_even_with_tokens() {
        let mut r = ReadLimiter::with_params(8 * 1024 * 1024, 0);
        // 桶里明明有 8 MiB，但单帧 5 MiB 仍必须拒（§2）
        assert!(matches!(
            r.try_consume(5 * 1024 * 1024),
            Err(LimitError::FrameTooLarge)
        ));
    }

    /// 回补不得因为「每片不足 1 字节就舍掉」而长期低于名义速率。
    ///
    /// 名义速率 = 实测耗时 × rate。若回补把余数丢掉，每个时间片只进整数部分，
    /// 累计损失可以占到名义的一大块——限流器于是比标称的更严。
    #[test]
    fn fractional_tokens_survive_many_small_refills() {
        // 1500 B/s、每片 ~1ms ⇒ 每片约 1.5 字节：整数化后只剩 1，损失 1/3。
        let mut r = ReadLimiter::with_params(1000, 1500);
        // 先烧掉初始那 1000 个令牌：否则量的是突发量而不是回补速率。
        while r.try_consume(1).is_ok() {}
        let start = Instant::now();
        let mut granted = 0usize;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(1));
            while r.try_consume(1).is_ok() {
                granted += 1;
            }
        }
        let nominal = start.elapsed().as_secs_f64() * 1500.0;
        assert!(
            granted as f64 > nominal * 0.9,
            "放行 {granted} 字节，名义约 {nominal:.0}：回补把余数丢掉了"
        );
    }

    #[test]
    fn tokens_refill_over_time() {
        let mut r = ReadLimiter::with_params(1000, 1000);
        r.try_consume(1000).unwrap();
        assert!(r.try_consume(1).is_err());
        std::thread::sleep(Duration::from_millis(20));
        r.try_consume(1).unwrap();
    }

    #[test]
    fn tokens_never_exceed_capacity() {
        let mut r = ReadLimiter::with_params(100, 10_000);
        std::thread::sleep(Duration::from_millis(30));
        r.refill();
        std::thread::sleep(Duration::from_millis(30));
        r.refill();
        // 最多 100
        r.try_consume(100).unwrap();
        assert!(r.try_consume(1).is_err());
    }

    #[test]
    fn defaults_use_production_constants() {
        // Default 必须等价于 new()：配置漂移会让「默认路径」与
        // 「显式 new」走出两套不同的限额。
        let mut r = ReadLimiter::default();
        assert!(r.try_consume(MAX_FRAME_BYTES).is_ok());
        assert!(matches!(
            r.try_consume(MAX_FRAME_BYTES + 1),
            Err(LimitError::FrameTooLarge)
        ));
    }

    /// 硬上限必须与协议常量同值。两份数值各写一遍，漂移了没有任何东西会报错——
    /// 而 Hub 会按自己那份拒掉一个协议认为合法的帧。
    #[test]
    fn the_hard_limit_is_the_protocol_one() {
        assert_eq!(MAX_FRAME_BYTES, protocol::MAX_FRAME_BYTES);
    }

    /// 默认桶必须放行 4 个满帧的突发量（`READ_BUCKET_CAPACITY = 4 ×
    /// MAX_FRAME_BYTES`）：桶调小会让一次 `cat` 大文件直接进慢读，
    /// 也就是「输出多的命令一律卡住」。
    #[test]
    fn the_default_bucket_holds_four_full_frames() {
        let mut r = ReadLimiter::new();
        for _ in 0..4 {
            r.try_consume(MAX_FRAME_BYTES).unwrap();
        }
        // 断言的是「第五个满帧被拒」，不是「第 1 字节被拒」：桶排空后回补
        // 一直在跑（1 MiB/s），插桩构建下多花几毫秒就攒得出几 KiB。
        // 而一次 `cat` 大文件的洪峰是整帧整帧来的，挡不住整帧就等于没挡。
        assert!(matches!(
            r.try_consume(MAX_FRAME_BYTES),
            Err(LimitError::Flood)
        ));
    }

    /// 慢读语义的终态（§6.1）：超限**不断开**——`Flood` 是「等令牌回补」，
    /// 不是错误码。第 30s 才由调用方拿 `FLOOD_SUSTAINED` 判持久超限而断开。
    #[test]
    fn a_flood_is_a_pause_not_a_disconnect() {
        // refill=0 把时间因素彻底消掉：这个用例只判「超速」与「单帧过大」
        // 是两条不同的路（§6.1 vs §2），不该被回补速率干扰。
        let mut r = ReadLimiter::with_params(MAX_FRAME_BYTES, 0);
        r.try_consume(MAX_FRAME_BYTES).unwrap();
        assert!(matches!(
            r.try_consume(MAX_FRAME_BYTES),
            Err(LimitError::Flood)
        ));
        assert!(matches!(
            r.try_consume(MAX_FRAME_BYTES + 1),
            Err(LimitError::FrameTooLarge)
        ));
    }

    /// 稳态速率就是每秒一个满帧（`READ_REFILL_PER_SEC = MAX_FRAME_BYTES`）：
    /// 限得再低，正常心跳与通道数据就得排队；限得再高，桶的突发量失去意义。
    #[test]
    fn the_steady_state_rate_is_one_full_frame_per_second() {
        let mut r = ReadLimiter::with_params(MAX_FRAME_BYTES, MAX_FRAME_BYTES);
        r.try_consume(MAX_FRAME_BYTES).unwrap();
        // 桶已排空：之后再放行的每一字节都来自回补，回补速率就是稳态速率
        assert!(r.try_consume(1).is_err());
        std::thread::sleep(Duration::from_millis(250));
        // 250ms ≈ 1/4 帧（128 KiB）。给一半的余量防 CI 慢机，仍远小于一整帧
        assert!(
            r.try_consume(MAX_FRAME_BYTES / 8).is_ok(),
            "1/4 秒必须攒得出 1/8 帧"
        );
        // 但一整秒的量在 1/4 秒里凑不出来
        assert!(r.try_consume(MAX_FRAME_BYTES).is_err());
    }
}
