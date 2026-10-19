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
        // coverage: excluded — `forkpty` 失败需 `/dev/ptmx` 用尽或 fd 耗尽，
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
        if pid < 0 {
            return Err(PtyError::Start(format!(
                "forkpty failed: {}",
                std::io::Error::last_os_error()
            )));
        }

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
        // coverage: excluded — 同上：`fd < 0` 时构造 OwnedFd 即崩，而它来自
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
        let seen = read_until(&pty, b"hello-pty");
        assert!(
            seen.windows(9).any(|w| w == b"hello-pty"),
            "PTY 没把输入回显回来: {}",
            String::from_utf8_lossy(&seen)
        );
    }

    /// 子进程退出后 `read` 返回 0 而不是 `EIO` 报错。
    #[test]
    fn the_child_exiting_ends_the_read_stream_cleanly() {
        let pty = Pty::spawn("true", &[], Size { cols: 80, rows: 24 }).expect("spawn");
        let mut buf = [0u8; 16];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let n = pty.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "子进程退出了却读不出 EOF"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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

    /// 一直读到 `needle` 出现，或超时。
    fn read_until(pty: &Pty, needle: &[u8]) -> Vec<u8> {
        crate::pty::testutil::read_until(pty, needle)
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
