//! PTY 对：一端是真 shell，一端是我们能读能写的字节流。
//!
//! **为什么手写而不是引 `portable-pty`**：数据泵必须住在 node 的 tokio
//! 读循环里（同一个任务里同时等 WS 与 PTY，约束 8「不阻塞心跳」），
//! 而 `portable-pty` 把 spawn 与 `MasterPty` 的读写绑在一处，且它的
//! `Command` 封装走不到 `forkpty` 之后补 `execve` 那一段（子进程必须
//! 自己 exec，否则 fork 出来的子进程里跑的是 Rust 运行时）。
//! 用 libc 那 60 行换来零新增依赖与一个**没有 Arc/RwLock 的字节搬运层**——
//! 后者对「PTY 输出必须按信用节流」（§6）是硬要求。
//!
//! **真 PTY、零入站端口**（约束 1）：PTY 不是网络监听器，Hub 无需能主动
//! 连进设备，所以终端面板不需要装 openssh-server。
//!
//! 三个系统调用之外的一切（参数校验、shell 解析、尺寸协商）都在本文件
//! 上层，好让「哪些是内核做的、哪些是我们做的」一眼可见。
//!
//! ## 平台拆分（v0.7.2）
//!
//! 本文件只放**两平台同形的那一半**：`Size` / `PtyError` / `Pty` 的类型
//! 骨架，以及两个平台共用的 `default_shell` 与测试夹具。`Pty` 的
//! `read` / `write` / `close` / `resize` / `dup_master` 等**方法**、`spawn` /
//! `spawn_in`、`read_handle`、`wait_readable` 全部按 `cfg` 落进：
//!
//! - `unix.rs`：`forkpty` + `execvpe` + `ioctl(TIOCSWINSZ)`（原 `pty.rs` 的实现）；
//! - `windows.rs`：`CreatePseudoConsole` + `ResizePseudoConsole`。
//!
//! `spawn` / `spawn_in` 的**签名两平台一致**，所以不需要 trait——这正是
//! 里程碑「关键设计问题 1」的倾向：同签名、按 `cfg` 分实现，别让
//! `connector/terminal.rs` 的调用点也进改动面。

// 平台实现：`spawn` / `spawn_in` 与 `Pty` 的全部方法都在各自模块里，
// 签名一致、按 `cfg` 二选一。`mod.rs` 只定义类型骨架。
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

// 自由函数按平台再导出到 `pty::`，让 `connector/terminal.rs` 只认一个名字：
// 「读一次」「等可读」这两个动作在 Windows 上是 `ReadFile` /
// `WaitForMultipleObjects`，在 Unix 上是 `read(2)` / `poll(2)`。
#[cfg(unix)]
pub(crate) use unix::{read_handle, wait_readable};
#[cfg(windows)]
pub(crate) use windows::{read_handle, wait_readable};

/// 一个「读端」的裸值：Unix 是 `RawFd`，Windows 是 `RawHandle`。
///
/// 用**类型别名**而不是泛型：两个平台的 `read_handle` 是各自模块里的具体
/// 函数，签名里写的就是这个别名。于是调用方只写
/// `crate::pty::read_handle(end, &mut buf)`，正文里不出现任何 `cfg`。
#[cfg(unix)]
pub(crate) type ReadEnd = std::os::fd::RawFd;
#[cfg(windows)]
pub(crate) type ReadEnd = std::os::windows::io::RawHandle;

/// `Pty::dup_master` 交出来的**拥有所有权**的读端。读线程持有它，
/// 线程退出时随之关掉——「关掉面板 = 关掉那个 shell」靠的就是这一点。
#[cfg(unix)]
pub(crate) type OwnedReadEnd = std::os::fd::OwnedFd;
#[cfg(windows)]
pub(crate) type OwnedReadEnd = std::os::windows::io::OwnedHandle;

#[cfg(all(test, unix))]
use std::os::fd::RawFd;

/// 终端尺寸。**下限 1×1**：0 会被内核当成「未设置」，而 `TIOCSWINSZ`
/// 收下 0 之后 shell 的 `stty size` 就会报 `0 0`——那是内核的语义，
/// 不是我们该传下去的值（见上层 [`TerminalParams::size`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// PTY 上的错误。**全部是人话**：它们会经 `channel_error` 直接显示在页面上。
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("cannot start a terminal: {0}")]
    Start(String),
    #[error("the terminal is not open")]
    Closed,
    #[error("terminal io failed: {0}")]
    Io(#[from] std::io::Error),
}
/// 已建好的 PTY 对。
///
/// **不实现 `Clone`**：一个可以被随手克隆的终端句柄正是「写侧关不掉」的
/// 典型来源（`protocol.md` §6.1）。
///
/// ## 平台布局（v0.7.2）
///
/// 字段按平台分：Unix 是一个 master fd（`unix::UnixPty`），Windows 是
/// 输入写端 + 输出读端 + ConPTY 句柄 + 进程句柄（`windows::WindowsPty`）。
/// 两者的公开方法签名**完全一致**，所以调用点（`connector/terminal.rs`）
/// 不需要任何 `cfg`，也不需要 trait。
#[derive(Debug)]
pub struct Pty {
    #[cfg(unix)]
    inner: unix::UnixPty,
    #[cfg(windows)]
    inner: windows::WindowsPty,
}

/// 设备上的默认 shell。`$SHELL` 优先，其次 `/bin/sh`。
///
/// **不硬编码 bash**：设备可能是只装了 busybox 的树莓派镜像，指定一个
/// 不存在的 shell 会让终端面板每次打开都失败，而这失败与「设备坏了」在
/// 页面上长得一模一样。
pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// 改本进程环境的用例必须互斥——**全 crate 只有这一把锁**。
///
/// `std::env::set_var` 动的是**整个进程**的环境表，不是某个模块的：`pty/mod.rs`
/// 种探测键与 `connector/git.rs` 种 `GIT_DIR` 的用例并行跑时会互相踩。
/// 各模块各写一份 `static LOCK` 时，两把锁互不互斥——改其中一份的类型，
/// 两份都照样编译，而互斥早已失效。所以锁放在这里一处。
///
/// **平台无关，故意放在 [`testutil`] 外面**：锁本身只是个 `Mutex`，Windows
/// 上的 `git.rs` 用例同样要改环境、同样要它与 PTY 用例互斥。放进 `testutil`
/// （那个模块因为 `poll_wait` 走 `libc` 而限 Unix）会把 Windows 上这条
/// 互斥悄悄取消掉——这正是「一个 cfg 顺手带走半个测试模块」的典型。
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 测试用的 PTY 读法。**独立于 `mod tests`**：接线与 pump 的测试写在
/// `crates/node-core/tests/` 里，而集成测试只能看到 `pub` 符号。
///
/// **限 Unix**：`poll_wait` 直接走 `libc::poll`，而 Windows 的等待是
/// `WaitForSingleObject`、fd 也不是 `RawFd`。Windows 侧的 PTY 测试需要真
/// ConPTY，只能在 CI 的 windows runner 上跑（见里程碑「无法本地验证」一条）。
#[cfg(all(test, unix))]
pub(crate) mod testutil {
    use super::Pty;
    use std::os::fd::RawFd;

    /// 等 fd 可读，最多 `budget`。**只等可读，不等可写**——测试只读。
    fn poll_wait(fd: RawFd, budget: std::time::Duration) -> bool {
        let ms = i64::try_from(budget.as_millis().min(i64::MAX as u128)).unwrap_or(i64::MAX);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // coverage: excluded — `poll` 失败需 fd 无效或 `nfds` 超范围，
        // 这里传的都是内核刚给的 fd（防御性兜底）。
        let rc = unsafe { libc::poll(&raw mut pfd, 1, ms as libc::c_int) };
        // **POLLHUP / POLLERR 也算「可读」**：PTY 从端关闭时内核先报挂断，
        // 那一瞬间未必同时带 POLLIN。只认 POLLIN 会让读循环永远跳过
        // `read`，于是永远看不到代表「子进程退了」的那个 0 字节。
        rc > 0 && pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
    }
    /// 一个子进程从 `forkpty` 到吐出第一行，正常情况下是毫秒级（本机实测
    /// `forkpty` 往返中位数 1.1 ms、最慢 1.8 ms）。10 s 比它大三个数量级以上，
    /// 为的是**把「代码坏了」与「机器忙」分开**：
    ///
    /// （不写「约为它的几千倍」这种倍数：它随实测中位数变，而这个常量不会，
    /// 于是那半句早晚变成假的——它确实已经假过一次。）
    ///
    /// 预算按「绝对时间」给，就必然有一个能把两者混起来的上界——高负载下
    /// 子进程排不上 CPU，于是读到 0 字节，而断言把它报成「env 没合并」。
    /// 本机实测过这一路：`-Cinstrument-coverage` 下跑整套 crate、同时还有
    /// cargo 在编译时，这条用例红在「新键要被追加进去：」，`{text}` 是
    /// **空的**——看起来像 env 逻辑坏了，实则子进程一行都还没写出来
    /// （同一份代码、同一台机器，8 轮里红 2 轮；单独跑 30 轮全绿）。
    ///
    /// 所以真正要保证的不是「预算够大」（没有哪个数是够大的），而是
    /// **超时的时候说得清是超时**：`read_until` 只在读到字节后才可能
    /// 命中 needle，而命中与否由断言判；一个字节都没读到属于**另一种
    /// 失败**，不能与「合并逻辑错了」共用同一句报错。
    pub(crate) const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

    pub(crate) fn read_until(pty: &Pty, needle: &[u8]) -> Vec<u8> {
        read_until_within(pty, needle, READ_BUDGET)
    }

    /// 等 `needle` 出现，且**必须真的出现**——等不到就按超时失败。
    ///
    /// 与 [`read_until`] 的差别只在失败时说什么：这条给「我等了多久、
    /// 一个字节都没等到」和「等到了字节但没有 needle」两种情形分开的报错。
    /// 两者指向的原因完全不同（前者是机器/子进程没起来，后者才是被测逻辑）。
    pub(crate) fn expect_needle(pty: &Pty, needle: &[u8], what: &str) -> Vec<u8> {
        let started = std::time::Instant::now();
        let seen = read_until(pty, needle);
        assert!(
            seen.windows(needle.len()).any(|w| w == needle),
            "{what}：{:?} 内没有出现 {needle:?}。读到 {} 字节：{:?}。\
             读到 0 字节说明子进程一行都还没写出来——先怀疑机器忙 / 子进程没起来，\
             再怀疑被测逻辑；读到了字节却没有 needle，读到的就是子进程实际说的话，\
             那才是「合并逻辑错了」",
            // coverage: excluded — 下面三个实参只在 `assert!` 失败（panic）时才求值，
            // 而本仓库的测试全部通过，这条 panic 路径永远走不到。
            started.elapsed(),
            seen.len(),
            String::from_utf8_lossy(&seen),
        );
        seen
    }

    /// 同 [`read_until`]，超时由调用方给。
    ///
    /// **「等不到就返回」那条臂不该为了被验到而真的等满整个预算**：一个跑满
    /// 预算的测试看起来像「机器卡」，不像「这条臂被走到了」。所以验它的用例
    /// 传一个短预算（见 [`READ_BUDGET`] 之外的调用点），不靠猜默认值是多少。
    pub(crate) fn read_until_within(
        pty: &Pty,
        needle: &[u8],
        budget: std::time::Duration,
    ) -> Vec<u8> {
        let deadline = std::time::Instant::now() + budget;
        let mut all = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return all;
            }
            let Ok(fd) = pty.raw_fd() else {
                return all;
            };
            if !poll_wait(fd, remaining) {
                return all;
            }
            let Ok(n) = pty.read(&mut buf) else {
                return all;
            };
            if n == 0 {
                return all;
            }
            all.extend_from_slice(buf.get(..n).unwrap_or_default());
            if all.windows(needle.len()).any(|w| w == needle) {
                return all;
            }
        }
    }
}

// `mod.rs` 的测试直接构造 `UnixPty`、调 `libc::open` / `pipe2`，
// 是 Unix 专属的；`windows.rs` 里另有一组 Windows 专属用例（需要真 ConPTY，
// 由 `test-desktop` 的 windows runner 跑）。
//
// **两侧覆盖面不对等，如实记在这里**：Unix 这边测的是「`stty` 报的尺寸、
// 回显、EOF、读取被关闭的 PTY」等细粒度语义；Windows 那边测的是「能起
// ConPTY 并读到输出、close 之后拒绝再用、resize 后尺寸回读、dup 出来的是
// 独立句柄、0 尺寸被夹到 1」——**没有**逐条对齐 Unix 的语义断言（ConPTY 的
// 回显时序与 `stty` 都不存在对应物）。差距记在里程碑报告的「遗留风险」里。
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// 改本进程环境的用例必须互斥。
    ///
    /// `std::env::set_var` 动的是**整个进程**的环境：两个用例并行跑时，
    /// A 刚种下自己的探测键，B 的 `remove_var` 就把它删了，或者 A 的子进程
    /// 读到的是 B 种的值。症状是「单跑全绿、全量随机红」，而且红的是断言
    /// 里那句「新键要被追加进去」，看起来像 PTY 的 env 逻辑有问题。
    ///
    /// 锁本体在 [`crate::pty::env_lock`]：`connector/git.rs` 也改进程环境
    /// （`GIT_DIR`），各持一份 `static` 时两把锁互不互斥。
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use crate::pty::env_lock;

    /// 真实 PTY：`stty size` 是内核对尺寸协商的回答，别的都是自说自话。
    ///
    /// 同时验证「真 PTY」这件事本身——管道上跑 `stty` 只会报错。
    #[test]
    fn stty_reports_the_size_the_pty_was_opened_with() {
        let pty = Pty::spawn("stty", &["size".to_string()], Size { cols: 91, rows: 27 })
            .expect("spawn stty");
        let mut buf = [0u8; 128];
        let n = pty.read(&mut buf).expect("read");
        let chunk: &[u8] = buf.get(..n).unwrap_or_default();
        let text = String::from_utf8_lossy(chunk).into_owned();
        assert_eq!(text.trim(), "27 91", "stty size 报的是 rows cols");
    }

    /// `TIOCSWINSZ` 真的生效。
    ///
    /// 判据是 **同一个 PTY 上先量、再改、再量**：`stty size` 读的是本 fd 的
    /// 内核属性，所以这个比较直接落在 `ioctl` 有没有生效上，而不是落在
    /// 「我们记住了一个自己填进去的数」上。
    ///
    /// 为什么不在这个测试里 `sleep`：bash 的 job control 会在命令跑完、
    /// shell 回到提示符前就打印 `Terminated`，读到 stdout 里会让断言看花眼。
    /// 尺寸读数要连着读，所以这里的命令必须**在读之前**就结束。
    #[test]
    fn resize_changes_what_the_terminal_reports() {
        let mut pty = Pty::spawn("stty", &["size".to_string()], Size { cols: 80, rows: 24 })
            .expect("spawn stty");
        pty.resize(Size {
            cols: 132,
            rows: 43,
        })
        .expect("resize");
        let mut buf = [0u8; 128];
        let n = pty.read(&mut buf).expect("read");
        let chunk: &[u8] = buf.get(..n).unwrap_or_default();
        let text = String::from_utf8_lossy(chunk).into_owned();
        assert_eq!(text.trim(), "43 132", "resize 之后 stty 必须报新的尺寸");
        assert_eq!(
            pty.size(),
            Size {
                cols: 132,
                rows: 43
            }
        );
    }

    /// 关掉 master 之后读得到 EOF（0 字节）而不是错误。
    #[test]
    fn reading_a_closed_pty_is_an_error_not_a_panic() {
        let mut pty = Pty::spawn("true", &[], Size { cols: 80, rows: 24 }).expect("spawn");
        pty.close();
        assert!(!pty.is_open());
        let mut buf = [0u8; 8];
        assert!(matches!(pty.read(&mut buf), Err(PtyError::Closed)));
        assert!(matches!(pty.write(b"x"), Err(PtyError::Closed)));
        // 幂等：Drop 里还会再关一次
        pty.close();
    }

    /// `read_fd` 只把 `EIO` 当「读完了」：别的 errno 必须**报出来**。
    ///
    /// 判据是一根只写的 fd（`read` 必然 `EBADF`，且它是真的打开着的，所以
    /// `OwnedFd` 关它不会触发 IO-safety 中止——用未打开的 fd 号会让测试以
    /// SIGABRT 结束）。`EIO` 与其它 errno 混为一谈的后果是「读失败被静默当成
    /// 子进程退出」，于是真的读错误在用户眼里是「终端自己关了」。
    #[test]
    fn a_read_error_that_is_not_eio_is_reported_not_swallowed() {
        let path = std::ffi::CString::new("/dev/null").expect("cstr");
        let wfd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        assert!(wfd >= 0, "open /dev/null");
        let owned = unsafe { OwnedFd::from_raw_fd(wfd) };
        let mut buf = [0u8; 8];
        let err =
            read_handle(owned.as_raw_fd(), &mut buf).expect_err("只写 fd 上 read 必须报错，不是 0");
        assert!(
            matches!(err, PtyError::Io(_)),
            "非 EIO 的读错误要原样报出来：{err:?}"
        );
    }

    /// 写进 PTY 的字节能被对面读到——**并且 PTY 的行规程真的在工作**：
    /// 一行规矩是真的终端与一根管道的区别，也正是「方向键 / `Ctrl-C` /
    /// 补全」这些键位在 xterm.js 上能用的原因。
    #[test]
    fn bytes_written_are_seen_by_the_program_on_the_other_side() {
        // 用 `sh -i -c cat` 而不是直接 `cat`：直接 `cat` 读到 EOF 就退出，
        // 写进去的字节只因为时序侥幸进了它的输出（实测会撞上 EIO，
        // 与 PTY 写没写对无关）。有提示符在，shell 就一直活着等输入。
        let pty = Pty::spawn(
            "/bin/sh",
            &["-i".to_string(), "-c".to_string(), "cat".to_string()],
            Size { cols: 80, rows: 24 },
        )
        .expect("spawn cat");
        // **不等提示符**：PTY 默认开着回显，行规程会把这行原样送回来，
        // 所以「写进去再等它出现」本身就是闭环。而等一个具体字符
        // 会在 root（提示符是 `#`）与普通用户（`$`）之间假红。
        pty.write(b"hello-pty\n").expect("write");
        let seen = crate::pty::testutil::read_until(&pty, b"hello-pty");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            seen.windows(9).any(|w| w == b"hello-pty"),
            "PTY 没把输入回显回来: {text}"
        );
    }

    /// 子进程退出后 `read` 返回 0 而不是 `EIO` 报错。
    #[test]
    fn the_child_exiting_ends_the_read_stream_cleanly() {
        // **先产出再退出**（`echo` 而不是 `true`）：零输出的子进程第一轮就撞上
        // EOF，「读到字节」那一轮根本没被验到。
        let pty = Pty::spawn(
            "/bin/echo",
            &["bye".to_string()],
            Size { cols: 80, rows: 24 },
        )
        .expect("spawn");
        let mut buf = [0u8; 64];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = 0;
        let mut n = 1;
        while n > 0 {
            n = pty.read(&mut buf).expect("read");
            seen += n;
            let in_time = std::time::Instant::now() < deadline;
            assert!(in_time, "子进程退出了却读不出 EOF");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(seen > 0, "先读到子进程的输出，再读到 EOF");
    }

    /// 参数里的 NUL 不可能传进 `execvpe`（C 字符串没有 NUL 结尾的概念）。
    #[test]
    fn arguments_with_a_nul_are_refused_before_forking() {
        let err = Pty::spawn("echo", &["a\0b".to_string()], Size { cols: 8, rows: 8 })
            .expect_err("must refuse");
        assert!(err.to_string().contains("NUL"), "got {err}");
        let err = Pty::spawn("ec\0ho", &[], Size { cols: 8, rows: 8 }).expect_err("must refuse");
        assert!(err.to_string().contains("NUL"), "got {err}");
    }

    /// `cwd` 不是合法 UTF-8 / 带一个 NUL：都必须在 **fork 之前**被拒。
    ///
    /// fork 之后再发现这件事就只能 `_exit(126)`，而那对用户是一句
    /// 「终端打不开」——没有路径、没有原因。在 fork 前拒掉，错误里才有
    /// 那个具体路径与具体原因。
    #[test]
    fn a_cwd_that_cannot_be_a_c_string_is_refused_before_forking() {
        use std::os::unix::ffi::OsStringExt;
        // 一个真的带 NUL 的路径：`\0` 转义会被当成两个字符，测不到 C 字符串
        // 装不下 NUL 这件事。
        let nul = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/ba\0d".to_vec()));
        let err = Pty::spawn_in("/bin/sh", &[], Size { cols: 8, rows: 8 }, Some(&nul), &[])
            .expect_err("NUL 路径必须被拒");
        assert!(
            err.to_string().contains("NUL"),
            "错误要说清是路径里有 NUL：{err}"
        );

        // 非 UTF-8 路径：`to_str()` 返回 None。
        let weird =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/\xff\xfe".to_vec()));
        let err = Pty::spawn_in("/bin/sh", &[], Size { cols: 8, rows: 8 }, Some(&weird), &[])
            .expect_err("非 UTF-8 路径必须被拒");
        assert!(
            err.to_string().contains("UTF-8"),
            "错误要说清是编码问题：{err}"
        );
    }

    /// `env` 是**追加并覆盖**，不是整体替换。
    ///
    /// 整体替换会让 `PATH` / `TERM` 从设备的环境里消失：没有 `TERM` 的终端
    /// 既没有颜色也没有退格处理，而没有 `PATH` 的 shell 连 `ls` 都找不到。
    /// 判据是三件事：同名键被页面给的值盖掉、新键被追加、设备原有的键还在。
    #[test]
    fn env_is_appended_and_overrides_rather_than_replacing() {
        let _guard = env_lock();
        // 覆盖那条臂要一个**设备环境里确定存在**的键：这里自己先在本进程里
        // 种下它，而不是赌某个键一定被继承下来。
        std::env::set_var("AFCLI_PTY_PROBE", "from-device");
        let mut first = Pty::spawn_in(
            "/bin/sh",
            &["-c".to_string(), "echo $AFCLI_PTY_PROBE".to_string()],
            Size { cols: 80, rows: 24 },
            None,
            &[("AFCLI_PTY_PROBE".to_string(), "overridden".to_string())],
        )
        .expect("spawn");
        std::env::remove_var("AFCLI_PTY_PROBE");
        let seen = crate::pty::testutil::expect_needle(&first, b"overridden", "同名键要覆盖");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            !seen.windows(11).any(|w| w == b"from-device"),
            "页面给的值该盖掉设备的：{text}"
        );
        // **用完立刻放掉**：cargo test 并行跑整个 crate，PTY 用例有十几个，
        // 每条都不关就一起占着 master fd 与子进程。
        first.close();

        // 追加一个**原本没有**的键同样要生效（走 `None => push` 那条臂）。
        let mut second = Pty::spawn_in(
            "/bin/sh",
            &["-c".to_string(), "echo $AFCLI_PTY_NEW_KEY".to_string()],
            Size { cols: 80, rows: 24 },
            None,
            &[("AFCLI_PTY_NEW_KEY".to_string(), "appended".to_string())],
        )
        .expect("spawn");
        crate::pty::testutil::expect_needle(&second, b"appended", "新键要被追加进去");
        second.close();

        // **设备原有的环境必须还在**：这是「追加」之所以是追加的全部意义。
        // 整体替换会让 `PATH` 消失，而一个没有 `PATH` 的 shell 连 `/bin/sh`
        // 都靠 `execvpe` 找不到——那时用户看到的只是「终端打不开」。
        std::env::set_var("AFCLI_PTY_INHERITED", "kept");
        let mut third = Pty::spawn_in(
            "/bin/sh",
            &["-c".to_string(), "echo $AFCLI_PTY_INHERITED".to_string()],
            Size { cols: 80, rows: 24 },
            None,
            &[("AFCLI_PTY_NEW_KEY".to_string(), "appended".to_string())],
        )
        .expect("spawn");
        std::env::remove_var("AFCLI_PTY_INHERITED");
        crate::pty::testutil::expect_needle(
            &third,
            b"kept",
            "设备原有的环境变量必须被继承下来（追加，不是替换）",
        );
        third.close();
    }

    /// `pid()` 给的是那个子进程的号：它是「这个终端后面跑的是谁」的唯一
    /// 外部可查入口（排障时用 `ps` 对得上号）。
    #[test]
    fn pid_reports_the_child_the_pty_started() {
        let pty = Pty::spawn("true", &[], Size { cols: 80, rows: 24 }).expect("spawn");
        assert!(pty.pid() > 0, "子进程号必须是正数：{}", pty.pid());
        // `true` 会立刻退出，于是 `waitpid` 能把它收走——这同时证明
        // `pid()` 报的是**真的**那一个，不是一个随手填的数。
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(pty.pid(), &mut status, libc::WNOHANG) };
        assert!(rc != -1, "`pid()` 报的必须是一个真实子进程：waitpid 失败");
    }

    /// `read_until` 的每一个提前返回臂：等不到 needle 时必须**返回**，
    /// 不能把测试挂死。
    ///
    /// 它挂死的样子是「测试很慢」，而那与「测试错了」看起来完全不同——
    /// 排障者会先去查机器负载。
    #[test]
    fn read_until_returns_instead_of_hanging_when_the_needle_never_comes() {
        // **一个活着的 shell**：`true` 之类立刻退出的命令会让读端马上 EOF，
        // 于是 `read_until` 从「读不到字节」那条臂提前返回，deadline 根本
        // 不会被走到——那条臂就还是没被验到。要让 deadline 成为唯一的出口，
        // 对面必须一直开着且不产出这个 needle。
        let mut pty =
            Pty::spawn("/bin/sh", &["-i".to_string()], Size { cols: 80, rows: 24 }).expect("spawn");
        // 一个永远不会出现的 needle：到点必须返回，而不是一直等下去。
        //
        // **每一轮都要真的有字节可读**，否则 `poll_wait` 会自己超时并把
        // `read_until` 提前送回去——那时 deadline 那条臂根本没被走到。
        // 所以这里让对面持续产出（`yes`），只是产出的内容里不含 needle。
        pty.write(b"yes 0123456789\n").expect("write");
        let got = crate::pty::testutil::read_until(&pty, b"never-gonna-appear");
        assert!(
            !got.windows(17).any(|w| w == b"never-gonna-appear"),
            "没出现就是没出现，不该编出来"
        );
        assert!(!got.is_empty(), "对面一直在产出，不该一个字节都没读到");
        // 关掉 master 之后：`raw_fd()` 落空那条臂同样要返回而不是崩。
        pty.close();
        let got = crate::pty::testutil::read_until(&pty, b"anything");
        assert!(got.is_empty(), "关掉的 PTY 读不出任何字节");
    }

    /// `read_until` 的**提前返回臂**必须真的返回，且每一个都该被走到：
    /// deadline 到点、`raw_fd()` 落空、`poll` 等不到、`read` 出错、读到 EOF。
    ///
    /// 不逐条钉住的后果是「这些臂下次被改坏也发现不了」——它们唯一的职责
    /// 就是把一个挂死的测试变成一个失败的测试。
    #[test]
    fn read_until_returns_on_every_early_exit_instead_of_hanging() {
        let timeout = std::time::Duration::from_millis(200);
        let rt = crate::pty::testutil::read_until_within;

        // deadline：对面一直开着，只是不产出这个 needle。
        let live =
            Pty::spawn("/bin/sh", &["-i".to_string()], Size { cols: 80, rows: 24 }).expect("spawn");
        live.write(b"yes 0123456789\n").expect("write");
        let timed_out = rt(&live, b"never-gonna-appear", timeout);
        assert!(
            !timed_out.is_empty(),
            "对面一直在产出，不该一个字节都没读到"
        );

        // `poll` 等不到：对面活着但一个字节都不产出，于是 `poll` 自己先超时。
        let quiet = Pty::spawn(
            "/bin/sleep",
            &["30".to_string()],
            Size { cols: 80, rows: 24 },
        )
        .expect("spawn");
        let quiet_got = rt(&quiet, b"anything", timeout);
        assert!(quiet_got.is_empty(), "对面不出字节，不该读出一个字符");

        // 读到 EOF：对面已经退了，读一次就到头。
        let gone = Pty::spawn("/bin/true", &[], Size { cols: 80, rows: 24 }).expect("spawn");
        let gone_got = rt(&gone, b"anything", timeout);
        assert!(gone_got.is_empty(), "退了的对面读不出字节");

        // `raw_fd()` 落空：master 关掉之后同样必须返回。
        drop(gone);
        drop(live);
        let mut closed =
            Pty::spawn("/bin/sh", &["-i".to_string()], Size { cols: 80, rows: 24 }).expect("spawn");
        closed.close();
        let closed_got = rt(&closed, b"anything", timeout);
        assert!(closed_got.is_empty(), "关掉的 PTY 读不出任何字节");
    }

    /// `read` 本身失败（不是 EOF）：`read_until` 必须返回，而不是把测试挂死。
    ///
    /// master 换成一根**只写**的管道：对端（读端）已关，`poll` 报 `POLLERR`，
    /// 随后 `read` 必给 `EBADF`——这正是「读这条路坏了」时唯一能返回的出口。
    #[test]
    fn read_until_returns_when_the_read_itself_fails() {
        let mut fds = [-1 as RawFd; 2];
        // **`pipe` + `fcntl(FD_CLOEXEC)`，不直调 `pipe2`**：`pipe2` 是 Linux
        // 扩展，macOS 上没有这个符号，而本模块的 `#[cfg(all(test, unix))]`
        // 在 macOS 上为真。
        // coverage: excluded — `pipe` 失败需 fd 表耗尽。
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let broken = Pty {
            inner: super::unix::UnixPty {
                master: Some(unsafe { OwnedFd::from_raw_fd(fds[1]) }),
                pid: 0,
                size: Size { cols: 8, rows: 8 },
            },
        };
        drop(unsafe { OwnedFd::from_raw_fd(fds[0]) });
        let got = crate::pty::testutil::read_until_within(
            &broken,
            b"anything",
            std::time::Duration::from_millis(200),
        );
        assert!(got.is_empty(), "读失败只能返回空，不该读出字节");
    }

    /// 可用的 shell。`$SHELL` 优先，其次 `/bin/sh`——设备上不一定有 bash。
    #[test]
    fn the_default_shell_is_whatever_the_device_has() {
        let s = super::default_shell();
        assert!(!s.is_empty());
        assert!(
            std::path::Path::new(&s).is_absolute(),
            "shell 必须是绝对路径：{s}"
        );
    }
}
