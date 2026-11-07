//! 设备端日志的写入侧：supervisor 与 worker 两边共用同一份格式。
//!
//! 与 [`crate::log`]（落盘 + 轮转 + 读）分开：那一层是**机制**，这里
//! 是**格式与调用点**。让两个进程各自拼一遍时间戳与级别，格式迟早分叉——
//! 而排障时正是靠 `afcli log` 把两边的记录按时间拼起来读（标准⑨）。
//!
//! 原来这一层在 `node-cli`（进程入口那一侧）。它跟着 worker 一起搬进
//! `node-core`：worker 是**要被升级换掉的文件**（约束 12），而 connector
//! 只能被已认证的 WS 连接使用——把通道处理留在 node-cli 就等于让通道能力
//! 跟着 worker 换版本走，于是「已注册的能力」与「这一版实际能跑的代码」
//! 变成两件事。
//!
//! 行格式（唯一一份，`afcli log` 的读侧不解析它、只按行输出）：
//!
//! ```text
//! 2026-11-05 12:34:56 [supervisor] info supervisor starting, argv: run
//! ```
//!
//! 列首是**人读时间戳**（UTC，见 [`now_timestamp`]），角色排在级别前面。

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::log;

/// supervisor 在日志里的角色名。
pub const ROLE_SUPERVISOR: &str = "supervisor";

/// worker 在日志里的角色名。
pub const ROLE_WORKER: &str = "worker";

/// 写一行到 `<data-dir>/logs/afcli.log`。
///
/// 角色排在级别前面：日志是两边混写的，排障第一眼要能分清这行是谁写的。
pub fn write(data_dir: &Path, role: &str, level: &str, msg: &str) {
    let line = format!("{} [{role}] {level} {msg}", now_timestamp());
    // 写日志失败**不**让调用方退出：磁盘满或目录被删不该把「排障手段」
    // 升级成「设备掉线」。但也不能静默吞掉——唯一还能去的地方是 stderr。
    if let Err(e) = log::append_line(&log::log_path(data_dir), &line) {
        eprintln!("afcli [{role}]: 无法写入日志（{e}）：{msg}");
    }
}

/// 把一行日志同时落到文件与「用户正盯着的屏幕」。
///
/// **为什么需要第二个落点**：`write` 只进日志文件，而日志要 `afcli log` 才看得见。
/// 前台跑 `afcli run` 的人看到的是一块**永远空白**的终端——用户反馈的原话就是
/// 「前台启动时控制台不打日志」，于是在他眼里「设备像是没起来」。
///
/// 落点按角色分（两条通道必须分开，不能合成一条）：
/// - `supervisor` → `console`（进程 stdout）：它是设备接入的**主进程**，
///   用户敲下 `afcli run` 跑的就是它；
/// - 其余角色（`worker`）→ `err`（进程 stderr）：worker 是 supervisor 拉起的
///   子进程，stdout/stderr 被继承，两者最终都会进终端 / journal。分开的理由是
///   约定「stdout 给用户消息，stderr 给诊断」——`afcli run | tee run.log` 只该
///   拿到 supervisor 那一层，worker 的逐帧日志不该混进管道。
///
/// 文件那一半**完全相同**：`afcli log` 读到的仍是同一份、同一格式的记录，
/// 所以文件里绝不出现两份。
pub fn write_console<W: Write, E: Write>(
    data_dir: &Path,
    role: &str,
    level: &str,
    msg: &str,
    mut console: W,
    mut err: E,
) {
    let line = format!("{} [{role}] {level} {msg}", now_timestamp());
    // 控制台写不进去（管道被 `head` 关掉、fd 被 reuse）不许影响文件那一半：
    // 日志是排障手段，不是判定依据。
    let to_console = if role == ROLE_SUPERVISOR {
        writeln!(console, "{line}")
    } else {
        writeln!(err, "{line}")
    };
    if let Err(e) = to_console {
        eprintln!("afcli [{role}]: 控制台不可写（{e}）：{msg}");
    }
    if let Err(e) = log::append_line(&log::log_path(data_dir), &line) {
        eprintln!("afcli [{role}]: 无法写入日志（{e}）：{msg}");
    }
}

/// 一个可共享、可克隆的 [`Write`] 适配器：把「用户正盯着的屏幕」接到
/// `write_console` 上。
///
/// 前台 `afcli run` 的 stdout 出现在三个地方：`main.rs`、分派层的入口行、
/// supervisor 的每一行日志。`W: Write` 是**按值**传的，而 supervisor 会被
/// 存进结构体里反复调用 `say`——直接把 `out` 塞进去就只能借一次，于是
/// 这里把同一个 writer 变成可克隆的句柄：每次 `Clone` 都指向同一块缓冲 /
/// 同一个 fd，日志行因此和入口行落在同一块屏幕上，顺序也不会因为另开一个
/// stdout 句柄而错乱。
///
/// `Arc<Mutex<W>>` 而不是 `Rc<RefCell<W>>`：worker 的日志钩子是
/// `Box<dyn Fn(&str) + Send + Sync>`，它要跨 Tokio 的多线程 runtime 调用，
/// 所以这个适配器本身也必须是 `Send + Sync`。
pub struct ConsoleEcho<W>(Arc<Mutex<W>>);

impl<W> Clone for ConsoleEcho<W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<W> ConsoleEcho<W> {
    /// 包一个 writer（生产里是 `std::io::Stdout`，测试里是 `&mut Vec<u8>`）。
    pub fn new(w: W) -> Self {
        Self(Arc::new(Mutex::new(w)))
    }

    /// 取得内部 writer 的守卫。测试用它读回缓冲；生产代码不需要。
    ///
    /// 单独一个方法而不是 `Deref`：锁的生命周期由调用方显式控制，免得
    /// `lock()` 与 `write_console` 里那次加锁撞成死锁。
    /// `unwrap_or_else(into_inner)` 让一个先前 panic  poisoned 的锁不影响
    /// 日志输出——日志是排障手段，不该因为锁被污染就跟着消失。
    pub fn lock(&self) -> MutexGuard<'_, W> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write> Write for ConsoleEcho<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.lock().flush()
    }
}

/// 启动时轮转一次日志。supervisor 与 worker 各调一次，轮转失败只记一行日志
/// （写不出去正是「轮转失败」的后果之一，再 `eprintln` 一遍是同一件事说两遍）。
///
/// 运行期判断大小会给每个进程加一次 stat 到热路径，而轮转的代价（复制整个文件）
/// 本来就不该由日志频率触发。
pub fn rotate_at_startup(data_dir: &Path, role: &str) {
    let path = log::log_path(data_dir);
    // 的失败路径本身已登记在 node-core/src/log.rs；这里只做「再记一行」。
    if let Err(e) = log::rotate_if_needed(&path) {
        // 的失败路径本身已登记在 node-core/src/log.rs；这里只做「再记一行」。
        write(
            data_dir,
            role,
            "info",
            &format!("cannot rotate {}: {e}", path.display()),
        );
    }
}

/// 日志行首的人读时间戳：`YYYY-MM-DD HH:MM:SS`（UTC）。
///
/// **原来这里是裸 unix 秒**（`1794017148`），用户反馈的原话是「cli 日志没有时间」
/// ——一个 ten-digit 数字对看日志的人不是时间，得先心算才知道是几号。
///
/// 不引 `chrono` / `time` crate：设备侧二进制体积是硬约束（树莓派），而
/// 「秒 → 年月日时分秒」只需要一次 civil-from-days 换算（Howard Hinnant 的
/// `civil_from_days` 算法：把 1970-01-01 起的整天数映射回公历日期，含闰年规则）。
///
/// **UTC 而不是本地时区**：拿本地时区要读 `/etc/localtime` 或调用平台 API，
/// 那是又一个依赖 + 每个平台的又一条分支；而设备日志的价值在于「两台机器上的
/// 记录能对上」，UTC 恰好是唯一不会因时区配置漂移的选择。
pub fn now_timestamp() -> String {
    format_timestamp(now_unix_secs())
}

/// 列首时间戳的格式串（供测试与文档引用，避免两处各写一遍）。
pub const TIMESTAMP_FORMAT: &str = "YYYY-MM-DD HH:MM:SS";

/// 秒级精度足够：日志不参与任何判定，也不需要毫秒级排序。
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// 把 unix 秒格式化成 `YYYY-MM-DD HH:MM:SS`（UTC）。
pub fn format_timestamp(unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(i64::MAX);
    let rem = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// 1970-01-01 起的整天数 → 公历年月日（UTC）。
///
/// 算法出处：Howard Hinnant 的 `civil_from_days`（公有领域的日期换算，
/// C++20 `<chrono>` 用的是同一套）。它把「纪元日」先挪到 0000-03-01，
/// 于是闰日恰好落在 4 年周期的末尾，不用逐月查表。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(0),
        u32::try_from(day).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个**独占**目录。
    ///
    /// 曾用固定的 `/tmp/afcli-entry-{name}` + 开头 `remove_dir_all`：
    /// 同一 crate 的测试并行跑时，两个用例撞上同名目录会互删对方刚写的文件
    /// （`create_dir` 报 `AlreadyExists`、读日志读到空文件）。名字里带用例名
    /// 不足以避免——cargo 会同时跑同名用例的多个进程（重跑、多 target）。
    /// `tempfile` 每次给新目录，且随守卫析构自动清理。
    fn dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("afcli-entry-{name}-"))
            .tempdir()
            .expect("tempdir")
    }

    /// 排障时靠 `afcli log` 把 supervisor 与 worker 两边的记录按时间拼起来读，
    /// 所以角色必须排在级别前面，且行尾有换行（否则两条记录会粘成一行）。
    /// 列首现在是人读时间戳（`YYYY-MM-DD HH:MM:SS`）：原来是裸 unix 秒，
    /// 用户反馈的原话是「cli 日志没有时间」。
    #[test]
    fn a_log_line_leads_with_a_human_readable_timestamp() {
        let d = dir("role");
        write(d.path(), ROLE_WORKER, "info", "connected");
        let text = std::fs::read_to_string(log::log_path(d.path())).expect("log file");
        assert!(text.ends_with('\n'), "missing newline: {text:?}");
        let line = text.lines().next().expect("one line");
        let mut words = line.split_whitespace();
        let date = words.next().expect("date");
        let time = words.next().expect("time");
        assert_eq!(date.len(), 10, "日期应是 YYYY-MM-DD：{date}");
        assert_eq!(time.len(), 8, "时刻应是 HH:MM:SS：{time}");
        assert!(date.contains('-'), "日期要带分隔符：{date}");
        assert!(time.contains(':'), "时刻要带分隔符：{time}");
        assert!(line.contains("[worker]"), "got {line}");
        assert!(line.contains("info"), "got {line}");
        assert!(line.contains("connected"), "got {line}");
    }

    /// 列首**不能**是裸 unix 秒：那是「没有时间」这件事本身。
    #[test]
    fn the_timestamp_is_not_a_bare_epoch_number() {
        let d = dir("not-epoch");
        write(d.path(), ROLE_WORKER, "info", "x");
        let text = std::fs::read_to_string(log::log_path(d.path())).expect("log");
        let head = text.split_whitespace().next().expect("head");
        assert!(
            head.parse::<u64>().is_err(),
            "列首仍是裸 unix 秒（{head}），用户看到的就是「没有时间」"
        );
        assert!(head.contains('-'), "to {head}");
        assert!(text.contains(':'), "to {text:?}");
    }

    /// 换算本身要能对上已知时刻——不是「数字变了」，而是「变对了」。
    /// 2000 是「百年但被 400 整除」的闰年，2024 是普通闰年，2100 不是闰年。
    #[test]
    fn unix_seconds_are_converted_to_the_right_utc_calendar_time() {
        let cases: [(u64, &str); 7] = [
            (0, "1970-01-01 00:00:00"),
            (1, "1970-01-01 00:00:01"),
            (951_782_400, "2000-02-29 00:00:00"),
            (1_704_067_199, "2023-12-31 23:59:59"),
            (1_704_067_200, "2024-01-01 00:00:00"),
            (1_709_164_800, "2024-02-29 00:00:00"),
            (4_102_444_800, "2100-01-01 00:00:00"),
        ];
        for (secs, want) in cases {
            assert_eq!(format_timestamp(secs), want, "unix {secs}");
        }
    }

    /// 格式串常量与实现必须是同一件事：两处漂移就是文档骗人。
    #[test]
    fn the_documented_timestamp_format_matches_the_implementation() {
        assert_eq!(TIMESTAMP_FORMAT, "YYYY-MM-DD HH:MM:SS");
        let stamp = format_timestamp(1_798_283_696);
        let mut parts = stamp.split(' ');
        let (date, time) = (parts.next().expect("date"), parts.next().expect("time"));
        assert_eq!(date.len(), "YYYY-MM-DD".len());
        assert_eq!(time.len(), "HH:MM:SS".len());
    }

    /// `write_console` 与 `write` 写进**同一个文件**、同一行：多一个控制台落点
    /// 不该让排障文件里出现两份或另一种格式。
    #[test]
    fn the_console_writer_persists_the_same_line_as_the_file_writer() {
        let d = dir("console-same");
        let mut console: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        write_console(
            d.path(),
            ROLE_SUPERVISOR,
            "info",
            "hello",
            &mut console,
            &mut err,
        );
        let logged = std::fs::read_to_string(log::log_path(d.path())).expect("log");
        let line = logged.lines().next().expect("one line");
        assert_eq!(
            String::from_utf8(console).expect("utf8"),
            format!("{line}\n")
        );
        assert!(err.is_empty(), "supervisor 的行不走 stderr");
        assert_eq!(logged.lines().count(), 1, "文件里不能出现两份：{logged:?}");
    }

    /// 角色的两个落点必须分开：`supervisor` 是用户敲的那个进程（stdout），
    /// `worker` 是它拉起的子进程（stderr 诊断）。
    #[test]
    fn the_role_decides_which_console_stream_gets_the_line() {
        let d = dir("console-streams");
        let mut console: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        write_console(
            d.path(),
            ROLE_WORKER,
            "info",
            "w-line",
            &mut console,
            &mut err,
        );
        let console_text = String::from_utf8(console).expect("utf8");
        let err_text = String::from_utf8(err).expect("utf8");
        assert!(
            console_text.is_empty(),
            "worker 不该占用 supervisor 的 stdout"
        );
        assert!(err_text.contains("[worker] info w-line"), "{err_text}");

        let mut console: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        write_console(
            d.path(),
            ROLE_SUPERVISOR,
            "info",
            "s-line",
            &mut console,
            &mut err,
        );
        let console_text = String::from_utf8(console).expect("utf8");
        assert!(
            console_text.contains("[supervisor] info s-line"),
            "{console_text}"
        );
        assert!(err.is_empty(), "supervisor 的行不该走 stderr");

        let lines = log::tail(&log::log_path(d.path()), 10).expect("tail");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("[worker]"), "{:?}", lines[0]);
        assert!(lines[1].contains("[supervisor]"), "{:?}", lines[1]);
    }

    /// 控制台写不进去（`afcli run | head` 那种读端已关）时**文件那一半照写**：
    /// 日志是排障手段，不能因为终端没了就跟着消失。
    #[test]
    fn a_closed_console_still_gets_the_line_into_the_file() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "reader gone",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let d = dir("console-closed");
        write_console(
            d.path(),
            ROLE_SUPERVISOR,
            "info",
            "survives",
            Closed,
            Closed,
        );
        let logged = std::fs::read_to_string(log::log_path(d.path())).expect("log");
        assert!(
            logged.contains("survives"),
            "控制台关了不代表这句不该进日志：{logged:?}"
        );
        assert!(logged.starts_with("20"), "列首要是年份：{logged:?}");
    }

    /// `ConsoleEcho` 的**同一块屏幕**语义：克隆出来的句柄必须写进同一个缓冲，
    /// 而不是各自复制一份。前台 `afcli run` 的入口行与后续日志行就靠这一点
    /// 保持顺序、不重不漏。
    #[test]
    fn a_console_echo_clone_writes_into_the_same_sink() {
        let echo = ConsoleEcho::new(Vec::<u8>::new());
        let mut a = echo.clone();
        let mut b = echo.clone();
        std::io::Write::write_all(&mut a, b"first\n").expect("write a");
        std::io::Write::write_all(&mut b, b"second\n").expect("write b");
        assert_eq!(
            String::from_utf8(echo.lock().clone()).expect("utf8"),
            "first\nsecond\n"
        );
    }

    /// 两个角色各写一行，读回来必须能按角色分开——混着写也不能互相覆盖。
    #[test]
    fn both_roles_append_to_the_same_file_in_order() {
        let d = dir("both");
        write(d.path(), ROLE_SUPERVISOR, "info", "spawned");
        write(d.path(), ROLE_WORKER, "warn", "retrying");
        let lines = log::tail(&log::log_path(d.path()), 10).expect("tail");
        assert_eq!(lines.len(), 2, "got {lines:?}");
        assert!(lines[0].contains("[supervisor]"), "got {:?}", lines[0]);
        assert!(lines[1].contains("[worker]"), "got {:?}", lines[1]);
    }

    /// 写不进去时**不能**让调用方退出：磁盘满或目录被删不该把「排障手段」
    /// 升级成「设备掉线」。这里用一个普通文件当目录，构造确定性的失败。
    #[test]
    fn a_failed_write_is_survived_not_fatal() {
        let base = dir("fail");
        let blocker = base.path().join("logs");
        std::fs::write(&blocker, "not a directory").expect("blocker file");
        // 不 panic、不返回、不静默：stderr 那一路由 eprintln 负责。
        write(base.path(), ROLE_SUPERVISOR, "error", "disk is full");
    }

    /// `rotate_at_startup` 是两个入口（supervisor / worker）唯一的轮转调用点：
    /// 超限日志在启动时被挪到 `.1`，且**两个角色都会触发**——曾经两个
    /// `rotate_log` 各写一份、只差一个角色常量，一条改了另一条漏掉。
    #[test]
    fn rotate_at_startup_rotates_for_both_roles() {
        for role in [ROLE_SUPERVISOR, ROLE_WORKER] {
            let d = dir("rotate-startup");
            let path = log::log_path(d.path());
            std::fs::create_dir_all(path.parent().expect("logs dir")).expect("mkdir");
            std::fs::write(&path, vec![b'x'; (log::MAX_LOG_BYTES + 1) as usize]).expect("fill");
            rotate_at_startup(d.path(), role);
            assert!(
                path.with_file_name("afcli.log.1").exists(),
                "{role}: oversized log must rotate at startup"
            );
            assert!(!path.exists(), "{role}: current name must be freed");
        }
    }

    /// 小日志不轮转：轮转的代价是复制整个文件，不该每次启动都白付一遍。
    #[test]
    fn a_small_log_is_not_rotated_at_startup() {
        let d = dir("rotate-skip");
        write(d.path(), ROLE_WORKER, "info", "tiny");
        rotate_at_startup(d.path(), ROLE_WORKER);
        let text = std::fs::read_to_string(log::log_path(d.path())).expect("log");
        assert!(text.contains("tiny"), "the original line must stay put");
    }

    /// 轮转失败也**必须留下痕迹**：`rotate_at_startup` 是启动路径，失败时
    /// 只靠 `log.rs` 返回的 `Err` 就静默了——排障时看到「日志没轮转」却
    /// 不知道为什么。这里补写的那行就是原因。
    #[test]
    fn a_failed_rotation_is_recorded_rather_than_silently_dropped() {
        let d = dir("rotate-fail");
        let path = log::log_path(d.path());
        std::fs::create_dir_all(path.parent().expect("logs dir")).expect("mkdir");
        std::fs::write(&path, vec![b'x'; (log::MAX_LOG_BYTES + 1) as usize]).expect("fill");
        // 最旧一代换成目录：`remove_file` 对目录必失败（unlink 目录 = EPERM），
        // 于是轮转整体报错。用目录而不是去掉写权限——root 绕过权限位，
        // 而这个失败与以什么身份运行无关。
        std::fs::create_dir(path.with_file_name("afcli.log.3")).expect("dir blocker");

        rotate_at_startup(d.path(), ROLE_SUPERVISOR);

        let text = std::fs::read_to_string(&path).expect("log still there");
        assert!(
            text.contains("cannot rotate"),
            "轮转失败必须留下痕迹：{text:?}"
        );
        assert!(
            text.contains("remove oldest rotation"),
            "要说清是哪一步失败的，否则这行等于没写：{text:?}"
        );
    }
}
