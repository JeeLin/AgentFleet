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
    /// 「现在」的出处。生产路径永远是 [`Instant::now`]。
    ///
    /// **这条接缝为什么存在**：令牌桶的语义是「时间 × 速率」，而把时间取成
    /// `Instant::now()` 就等于把「过掉了多久」交给真时钟——于是验证回补速率
    /// 只能真 `sleep` 一段再断言，而 `sleep 250ms` 并攒出 1/8 帧这类断言在
    /// 机器繁忙时会直接变红（观测到的失败：CI 并发跑 34 个测试二进制时超时）。
    /// 把「现在」换成可注入的来源之后，时间由调用方推进，断言变成精确值。
    ///
    /// 只存一个函数指针而不是引入 `trait Clock` + mock 实现：本类型唯一需要
    /// 的时间能力就是「取一次 `Instant`」，而函数指针已经能把「谁来决定现在」
    /// 这件事交给调用方——多一层 trait 只增加生产路径上的虚调用与样板。
    now: fn() -> Instant,
}

impl ReadLimiter {
    pub fn new() -> Self {
        Self::with_params(READ_BUCKET_CAPACITY, READ_REFILL_PER_SEC)
    }

    /// 造一个「现在」由调用方决定的限流器。
    ///
    /// **只给测试用**：生产路径一律 `new()`。注入的时钟一旦走偏（停住、倒退），
    /// 线上限流器会按错误的时间补令牌，而它没有任何自检手段。
    fn with_clock(capacity: usize, refill_per_sec: usize, now: fn() -> Instant) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            fractional: 0.0,
            last: now(),
            now,
        }
    }

    /// 构造非默认限额。**只给测试用**——生产路径一律 `new()`：
    /// 「默认路径」与「显式 new」走出两套限额时，线上限流器与文档各说各话。
    fn with_params(capacity: usize, refill_per_sec: usize) -> Self {
        Self::with_clock(capacity, refill_per_sec, Instant::now)
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
    /// **余量用 `f64` 累加，不逐片截断**：`tokens` 只能是整数，而毫秒级的读片
    /// 上攒出的令牌是小数，每片各自 `as usize` 会把余数一次次丢掉——实测
    /// 高频小额读的实际吞吐只有名义的 ~62%。
    fn refill(&mut self) {
        let now = (self.now)();
        // 时钟**没有**推进时 `duration_since` 会 panic（`Instant` 减出负数）。
        // 注入时钟的调用方可能一次都不推进，而「时间没动」在限流器里就是
        // 「没有令牌可补」，不是错误——所以按 0 处理而不是让调用方先算好。
        let elapsed = now.saturating_duration_since(self.last);
        // `last` 无条件推进：这一段时间已经被计入 `fractional`，再留着它会
        // 在下一次被重复算一遍（凭空生出令牌）。
        self.last = now;
        self.fractional += elapsed.as_secs_f64() * self.refill_per_sec as f64;
        // 只在真的攒出整字节时才落进 `tokens`，余数留在 `fractional`。
        if self.fractional >= 1.0 {
            let whole = self.fractional.floor();
            self.fractional -= whole;
            self.tokens = (self.tokens + whole as usize).min(self.capacity);
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
    use std::cell::Cell;

    // 假时钟的读数。
    //
    // **为什么是线程局部而不是一个 `Clock` trait 的 mock**：限流器的接缝是
    // 一个 `fn() -> Instant` 指针，而闭包捕获不了任何东西——读数只能存在
    // 某个静态位置上，由这个自由函数去取。单线程用例下 `Cell` 足够，
    // 不需要 `Mutex` / `Atomic` 那套并发装置。
    thread_local! {
        static FAKE_NOW: Cell<Instant> = Cell::new(Instant::now());
    }

    /// 交给 [`ReadLimiter::with_clock`] 的那个 `fn() -> Instant`。
    fn fake_now() -> Instant {
        FAKE_NOW.with(Cell::get)
    }

    /// 重置假时钟到真时钟的此刻。
    ///
    /// 必须每个用例先调一次：同一个 worker 线程会连着跑多条用例，
    /// 上一条推进过的时间会漏进下一条。
    fn reset_clock() {
        FAKE_NOW.with(|c| c.set(Instant::now()));
    }

    /// 推进 `d`。**不消耗墙钟时间**——这正是它替代 `sleep` 的地方。
    fn advance(d: Duration) {
        FAKE_NOW.with(|c| c.set(c.get() + d));
    }

    /// 往回调 `d`。`Instant` 早于进程起点时会停在原地（`checked_sub` 返回
    /// `None`），那不是错误：限流器只关心「相对上一次读数过去了多久」。
    fn rewind(d: Duration) {
        FAKE_NOW.with(|c| {
            let now = c.get();
            c.set(now.checked_sub(d).unwrap_or(now));
        });
    }

    /// 造一台「现在由假时钟说了算」的限流器。
    fn fake_limiter(capacity: usize, refill_per_sec: usize) -> ReadLimiter {
        reset_clock();
        ReadLimiter::with_clock(capacity, refill_per_sec, fake_now)
    }

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
    ///
    /// **时间由假时钟推进，不 `sleep`**：这条量的是「速率 × 时长」这个算术，
    /// 而墙钟只负责把它拖慢——并发跑测试时 `sleep(250ms)` 实际等到的远不止
    /// 250ms，于是同一个断言在空闲机器上绿、繁忙机器上红。推进假时钟 250ms
    /// 是**零墙钟耗时**且精确的：攒出的令牌数只由 `READ_REFILL_PER_SEC` 决定。
    #[test]
    fn the_steady_state_rate_is_one_full_frame_per_second() {
        let mut r = fake_limiter(MAX_FRAME_BYTES, MAX_FRAME_BYTES);
        r.try_consume(MAX_FRAME_BYTES).unwrap();
        // 桶已排空：之后再放行的每一字节都来自回补，回补速率就是稳态速率
        assert!(r.try_consume(1).is_err());

        // ① 时间**没**推进：一字节都不该补进来。
        assert!(
            r.try_consume(1).is_err(),
            "时钟没动就不该有令牌——若这里绿了，限流器读的是真时钟"
        );

        // ② 推进 250ms = 1/4 秒，按 1 帧/秒应攒出 1/4 帧。
        advance(Duration::from_millis(250));
        assert!(
            r.try_consume(MAX_FRAME_BYTES / 8).is_ok(),
            "1/4 秒必须攒得出 1/8 帧"
        );
        // 但一整秒的量在 1/4 秒里凑不出来
        assert!(r.try_consume(MAX_FRAME_BYTES).is_err());

        // ③ 再推进到整秒：稳态速率的整帧量这才到齐（扣掉上面已消费的 1/8 帧）。
        advance(Duration::from_millis(750));
        assert!(
            r.try_consume(MAX_FRAME_BYTES - MAX_FRAME_BYTES / 8).is_ok(),
            "满一秒后余下的 7/8 帧必须补进来"
        );
    }

    /// 注入的时钟必须真的被限流器读走：**不推进就补不出令牌**。
    ///
    /// 单独一条而不是塞在上面的用例里：那条最终会把时钟推到 1 秒，
    /// 一旦有人把 `refill()` 改回 `Instant::now()`，它在慢机上**恰好也能绿**
    /// ——真时钟走过的时间本来就够补出令牌。这条一次都不推进，
    /// 于是「限流器读了真时钟」必然表现为红灯。
    #[test]
    fn an_unadvanced_clock_refills_nothing() {
        let mut r = fake_limiter(MAX_FRAME_BYTES, MAX_FRAME_BYTES);
        r.try_consume(MAX_FRAME_BYTES).unwrap();
        // 空桶 + 时钟不动：无论等多久都不该有令牌进来。
        for _ in 0..100 {
            assert!(
                r.try_consume(1).is_err(),
                "时钟没推进就不该补令牌——限流器读到的不是注入的时钟"
            );
        }
        // 推进 1ms @1 MiB/s ≈ 1 KiB，攒得出小片但攒不出整帧：
        // 证明上面的失败来自「时间没走」而不是「桶坏了」。
        advance(Duration::from_millis(1));
        assert!(r.try_consume(1024).is_ok());
        assert!(r.try_consume(MAX_FRAME_BYTES).is_err());
    }

    /// 时钟**倒退**不补令牌也不 panic：`Instant` 相减出负数会 panic，
    /// 而注入时钟的调用方完全可能把时间往回调（重放、对账）。
    #[test]
    fn a_clock_that_goes_backwards_refills_nothing() {
        let mut r = fake_limiter(MAX_FRAME_BYTES, MAX_FRAME_BYTES);
        r.try_consume(MAX_FRAME_BYTES).unwrap();
        advance(Duration::from_millis(500));
        assert!(r.try_consume(MAX_FRAME_BYTES / 2).is_ok());
        // 往回调：既不该补令牌，也不该 panic
        rewind(Duration::from_millis(500));
        assert!(r.try_consume(1).is_err());
    }
}
