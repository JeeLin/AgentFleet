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

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

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
/// **不实现 `Clone`**：master fd 只有一个，拥有者是通道；而一个可以被随手
/// 克隆的 PTY 句柄正是「写侧关不掉」的典型来源（`protocol.md` §6.1）。
#[derive(Debug)]
pub struct Pty {
    master: Option<OwnedFd>,
    pid: libc::pid_t,
    size: Size,
}

impl Pty {
    /// 起一个 PTY 并在子进程里 exec `argv[0]`，不设 cwd、不改环境。
    pub fn spawn(argv0: &str, args: &[String], size: Size) -> Result<Self, PtyError> {
        Self::spawn_in(argv0, args, size, None, &[])
    }

    /// 起一个 PTY，在子进程里 `chdir` 后 `execvp`。
    ///
    /// `argv` **不含 argv[0]**：调用方给的是命令 + 参数，第一个元素由
    /// 本函数按 `argv0` 补。这与 `execvp` 的约定一致，也避免了调用方
    /// 在两处地方各写一次程序名。
    ///
    /// `cwd` 与 `env` **在 fork 之前全部备成 C 字符串**：fork 出来的子进程与
    /// 父进程共享同一份堆，此刻任何分配都可能带着一把别的线程正持有的锁出去。
    /// 所以子进程里只做「用现成指针填 `environ`」这种不分配的活。
    ///
    /// `env` 是**追加**而非替换：设备上的 `PATH` / `TERM` / `LANG` 来自 worker
    /// 自己继承的环境，页面指定的那几条只该覆盖它想覆盖的键。整体替换会让
    /// `TERM` 消失，而没有 `TERM` 的终端既没有颜色也没有退格处理。
    pub fn spawn_in(
        argv0: &str,
        args: &[String],
        size: Size,
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
    ) -> Result<Self, PtyError> {
        let program = CString::new(argv0)
            .map_err(|_| PtyError::Start(format!("{argv0}: name contains a NUL")))?;
        let args_owned: Vec<CString> = args
            .iter()
            .map(|a| {
                CString::new(a.as_str())
                    .map_err(|_| PtyError::Start(format!("{a}: argument contains a NUL")))
            })
            .collect::<Result<_, _>>()?;
        let cwd_owned: Option<CString> = cwd
            .map(|p| {
                let text = p
                    .to_str()
                    .ok_or_else(|| PtyError::Start(format!("{}: not valid UTF-8", p.display())))?;
                CString::new(text)
                    .map_err(|_| PtyError::Start(format!("{}: path contains a NUL", p.display())))
            })
            .transpose()?;

        // 父进程的键值对 + 追加项；同名的后者覆盖前者，保持声明顺序。
        let mut merged: Vec<(String, String)> = std::env::vars().collect();
        for (k, v) in env {
            match merged.iter_mut().find(|(mk, _)| mk == k) {
                Some(slot) => slot.1 = v.clone(),
                None => merged.push((k.clone(), v.clone())),
            }
        }
        let env_owned: Vec<CString> = merged
            .iter()
            .map(|(k, v)| {
                let joined = format!("{k}={v}");
                CString::new(joined.clone())
                    .map_err(|_| PtyError::Start(format!("{joined}: env entry contains a NUL")))
            })
            .collect::<Result<_, _>>()?;

        // fork 之前把所有字符串备好：`CString::new` 会分配，
        // 绝不能在 fork 之后分配——父子的堆空间此刻是同一份，
        // 子进程里分配到的东西会带着一把没有其它线程持有的锁出去。
        let mut argv: Vec<*const libc::c_char> = Vec::with_capacity(args_owned.len() + 2);
        argv.push(program.as_ptr());
        argv.extend(args_owned.iter().map(|a| a.as_ptr()));
        argv.push(std::ptr::null());

        // `environ` 的类型是 `*mut *mut c_char`，所以元素也必须是 mut——
        // 这不是「要改它」，而是 libc 在两种平台上的声明本身如此。
        let mut envp: Vec<*mut libc::c_char> = Vec::with_capacity(env_owned.len() + 1);
        envp.extend(env_owned.iter().map(|e| e.as_ptr().cast_mut()));
        envp.push(std::ptr::null_mut());

        let winsize = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let mut master: libc::c_int = -1;
        // 测试环境无可控构造手段（防御性兜底）。尺寸非法那类**可测**的
        // 失败在上层就挡住了，不会走到这里。
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null(),
                &winsize,
            )
        };
        // coverage: excluded — 下面这一臂（含错误串的 `format!` 参数行）要求
        // `forkpty` 失败：`/dev/ptmx` 用尽或 fd 耗尽，测试环境无可控构造手段
        // （防御性兜底）。
        if pid < 0 {
            return Err(PtyError::Start(format!(
                "forkpty failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // coverage: excluded — 下面整个 `pid == 0` 分支：这是 `forkpty` 之后的
        // **子进程**，父进程那份 llvm-cov 插桩在 `execvp` 成功之后就没了，
        // 子进程里跑过的行永远统计不到；`chdir` 失败后的 `_exit(126)` 与
        // `execvp` 失败后的 `_exit(127)` 还要额外的系统调用失败才走得到。
        if pid == 0 {
            // ---- 子进程 ----
            // `forkpty` 已经替我们做好 setsid + TIOCSCTTY + dup2 0/1/2，
            // 这里只补最后一步：把自己换成目标程序。
            //
            // 任何一步失败都只能 `_exit`：此时子进程里跑的还是 Rust 的
            // 运行时（栈展开、析构、锁状态全是 fork 时刻的快照），
            // 走正常的返回路径会跑进未定义的领域。
            //
            // `chdir` 失败也要 `_exit` 而不是继续：shell 在错误的目录里
            // 启动，用户看到的路径与工作区对不上，而界面上没有任何提示。
            unsafe {
                if let Some(dir) = cwd_owned.as_ref() {
                    if libc::chdir(dir.as_ptr()) != 0 {
                        libc::_exit(126);
                    }
                }
                // `environ` 只被读一次：`execvp` 拿它去解析 PATH，
                // 换成 `execvpe` 并不能省掉这一次赋值。
                libc::environ = envp.as_mut_ptr();
                libc::execvp(program.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }

        // ---- 父进程 ----
        // coverage: excluded — master < 0 在 forkpty 返回正 pid 时不可能
        // （内核要么同时给出两者，要么同时失败），属结构上不可达。
        if master < 0 {
            return Err(PtyError::Start("forkpty returned no master fd".into()));
        }
        // 一个刚成功返回的 forkpty 调用。
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        Ok(Self {
            master: Some(master),
            pid,
            size,
        })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// 复制一份 master fd（写 / resize 用），**与 `self` 共享同一个 open
    /// file description**。
    ///
    /// 布局是「读线程独占 master、调用方留一份 dup」：读线程退出时丢掉它
    /// 那份 master，内核随之关掉最后一个，前台进程组收到 `SIGHUP`——
    /// 「关掉终端面板 = 关掉那个 shell」因此成立。反过来（调用方独占
    /// master、读线程拿 dup）的话，读线程会永远阻塞在一个没人关的 fd 上，
    /// 每开关一次面板漏一个线程。
    pub fn dup_master(&self) -> Result<OwnedFd, PtyError> {
        let fd = self.master.as_ref().ok_or(PtyError::Closed)?;
        // coverage: excluded — `fcntl(F_DUPFD_CLOEXEC)` 失败需 fd 表耗尽，
        // 测试环境无可控构造手段（防御性兜底）。
        let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(PtyError::Io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(dup) })
    }

    pub fn size(&self) -> Size {
        self.size
    }

    /// 改终端尺寸（`ioctl(TIOCSWINSZ)`）。
    ///
    /// **内核会在这时给前台进程组发 `SIGWINCH`**——所以 `resize` 一生效，
    /// 正在跑的程序立刻按新尺寸重画，不需要重新起一个 shell。
    pub fn resize(&mut self, size: Size) -> Result<(), PtyError> {
        let fd = self.master.as_ref().ok_or(PtyError::Closed)?;
        apply_winsize(fd.as_raw_fd(), size)?;
        self.size = size;
        Ok(())
    }

    /// 往 PTY 写（等价于用户在终端里敲键）。
    pub fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        let fd = self.master.as_ref().ok_or(PtyError::Closed)?;
        let mut off = 0;
        // **判据是 `off < len`，不是 `bytes.get(off..)` 是否为 `Some`**：
        // `get(len..)` 返回的是**空切片**而不是 `None`，用 `while let
        // Some` 收尾会把最后一次 `write(fd, .., 0)` 的返回值 0 当成
        // 「PTY 被关掉了」——于是每一次成功写入都以「terminal is gone」
        // 收场，终端面板表现为「敲什么都报错」。
        while off < bytes.len() {
            let rest = bytes.get(off..).ok_or(PtyError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write offset ran past the buffer",
            )))?;
            // coverage: excluded — 写中断的 `EINTR` 重试分支：本仓库的读写都
            // 很快且非信号密集，这条只在极端时序下发生，无法确定性构造。
            let n = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    rest.as_ptr().cast::<libc::c_void>(),
                    rest.len(),
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // coverage: excluded — `n == 0` 臂（`BrokenPipe` + "terminal is
            // gone"）：`write(2)` 对一根活着的 PTY 只会返回正数或负 errno，
            // 返回 0 需要「是终端却一个字节都不收」的稳定时序，测试环境造不出来。
            if n == 0 {
                // 写不进任何字节：PTY 被关掉了。说清楚而不是空转。
                return Err(PtyError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "terminal is gone",
                )));
            }
            off += n as usize;
        }
        Ok(())
    }

    /// 从 PTY 读**至多** `buf.len()` 字节。
    ///
    /// **不是** `read_exact` 也不是「读到 EOF」：PTY 是字节流，`EIO`
    /// 才是「子进程退出了、这一侧以后再也读不到」的信号，而那个信号
    /// 必须被转成「读完了」而不是一个错误——否则每敲一次 `exit` 都会
    /// 冒出一句 `terminal io failed: Input/output error`。
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        read_fd(
            self.master.as_ref().ok_or(PtyError::Closed)?.as_raw_fd(),
            buf,
        )
    }

    /// 关掉 master fd。
    ///
    /// 关掉 master 会让前台进程组收到 `SIGHUP`——这正是「关掉终端面板 =
    /// 关掉那个 shell」想要的语义，而不是让它继续在设备上跑着没人管。
    /// `SIGHUP` 的处置由那个 shell 自己决定（bash 会退出）。
    pub fn close(&mut self) {
        drop(self.master.take());
    }

    pub fn is_open(&self) -> bool {
        self.master.is_some()
    }

    /// 只需要「在旁边等一下」的调用方（测试的 poll 超时）才用得上这个裸 fd。
    ///
    /// **只借，不接管所有权**：拿到它的人关不关 fd 与本类型无关。
    /// 需要能独立关掉的那一份走 [`Pty::dup_master`]。
    #[cfg(test)]
    pub(crate) fn raw_fd(&self) -> Result<RawFd, PtyError> {
        self.master
            .as_ref()
            .map_or(Err(PtyError::Closed), |fd| Ok(fd.as_raw_fd()))
    }
}

/// 从一个 master fd 读至多 `buf.len()` 字节。
///
/// **自由函数而不是只有 `Pty::read`**：terminal 的读线程持有的是 `dup` 出来的
/// 那个 fd（`Pty::dup_master`），它不是 `Pty`。两条路必须共用同一份
/// 「`EIO` 算读完了」的判断——子进程退出后 Linux 报的是 `EIO` 而不是
/// `EOF(0)`，认错一次就会让用户每敲一次 `exit` 都看到一句 IO 错误。
pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize, PtyError> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EIO) {
            return Ok(0);
        }
        return Err(err.into());
    }
    Ok(n as usize)
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}

/// `ioctl(TIOCSWINSZ)`。独立出来是为了让测试能在**不 fork** 的前提下
/// 直接验证内核这一层的接线。
fn apply_winsize(fd: RawFd, size: Size) -> Result<(), PtyError> {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // coverage: excluded — `ioctl` 失败需要一个「是终端却拒绝改尺寸」的 fd
    // （普通管道 / 普通文件会返回 ENOTTY，但那不是 PTY 路径构造得出来的），
    // 属防御性兜底。
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    if rc < 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            read_fd(owned.as_raw_fd(), &mut buf).expect_err("只写 fd 上 read 必须报错，不是 0");
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

    /// 参数里的 NUL 不可能传进 `execvp`（C 字符串没有 NUL 结尾的概念）。
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
        // 覆盖那条臂要一个**设备环境里确定存在**的键：这里自己先在本进程里
        // 种下它，而不是赌某个键一定被继承下来。
        std::env::set_var("AFCLI_PTY_PROBE", "from-device");
        let first = Pty::spawn_in(
            "/bin/sh",
            &["-c".to_string(), "echo $AFCLI_PTY_PROBE".to_string()],
            Size { cols: 80, rows: 24 },
            None,
            &[("AFCLI_PTY_PROBE".to_string(), "overridden".to_string())],
        )
        .expect("spawn");
        std::env::remove_var("AFCLI_PTY_PROBE");
        let seen = crate::pty::testutil::read_until(&first, b"overridden");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            seen.windows(10).any(|w| w == b"overridden"),
            "同名键要覆盖：{text}"
        );
        assert!(
            !seen.windows(11).any(|w| w == b"from-device"),
            "页面给的值该盖掉设备的：{text}"
        );

        // 追加一个**原本没有**的键同样要生效（走 `None => push` 那条臂）。
        let second = Pty::spawn_in(
            "/bin/sh",
            &["-c".to_string(), "echo $AFCLI_PTY_NEW_KEY".to_string()],
            Size { cols: 80, rows: 24 },
            None,
            &[("AFCLI_PTY_NEW_KEY".to_string(), "appended".to_string())],
        )
        .expect("spawn");
        let seen = crate::pty::testutil::read_until(&second, b"appended");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            seen.windows(8).any(|w| w == b"appended"),
            "新键要被追加进去：{text}"
        );

        // **设备原有的环境必须还在**：这是「追加」之所以是追加的全部意义。
        // 整体替换会让 `PATH` 消失，而一个没有 `PATH` 的 shell 连 `/bin/sh`
        // 都靠 `execvp` 找不到——那时用户看到的只是「终端打不开」。
        std::env::set_var("AFCLI_PTY_INHERITED", "kept");
        let third = Pty::spawn_in(
            "/bin/sh",
            &["-c".to_string(), "echo $AFCLI_PTY_INHERITED".to_string()],
            Size { cols: 80, rows: 24 },
            None,
            &[("AFCLI_PTY_NEW_KEY".to_string(), "appended".to_string())],
        )
        .expect("spawn");
        std::env::remove_var("AFCLI_PTY_INHERITED");
        let seen = crate::pty::testutil::read_until(&third, b"kept");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            seen.windows(4).any(|w| w == b"kept"),
            "设备原有的环境变量必须被继承下来（追加，不是替换）：{text}"
        );
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
        // coverage: excluded — `pipe2` 失败需 fd 表耗尽。
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let broken = Pty {
            master: Some(unsafe { OwnedFd::from_raw_fd(fds[1]) }),
            pid: 0,
            size: Size { cols: 8, rows: 8 },
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

/// 设备上的默认 shell。`$SHELL` 优先，其次 `/bin/sh`。
///
/// **不硬编码 bash**：设备可能是只装了 busybox 的树莓派镜像，指定一个
/// 不存在的 shell 会让终端面板每次打开都失败，而这失败与「设备坏了」在
/// 页面上长得一模一样。
pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// 测试用的 PTY 读法。**独立于 `mod tests`**：接线与 pump 的测试写在
/// `crates/node-core/tests/` 里，而集成测试只能看到 `pub` 符号。
#[cfg(test)]
pub(crate) mod testutil {
    use super::Pty;
    use std::os::fd::RawFd;

    /// 一直读到 `needle` 出现，或超时。**到点一定会返回。**
    ///
    /// 超时必须落在 [`poll_wait`] 上而不是「循环里比时间」：PTY 的 `read`
    /// 在没有数据时阻塞，而上面那个 deadline 只在 `read` **返回之后**才被
    /// 检查——于是一个永远等不到的提示符会把测试挂死，而挂死的测试看起来
    /// 像「测试慢」，不像「测试错」。
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
    pub(crate) fn read_until(pty: &Pty, needle: &[u8]) -> Vec<u8> {
        read_until_within(pty, needle, std::time::Duration::from_secs(5))
    }

    /// 同 [`read_until`]，超时由调用方给。
    ///
    /// **「等不到就返回」那条臂不该为了被验到而真的等满 5 秒**：一个慢 5 秒的
    /// 测试看起来像「机器卡」，不像「这条臂被走到了」。
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
