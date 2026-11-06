//! Unix 侧实现：`forkpty` + `execvpe` + `ioctl(TIOCSWINSZ)`。
//!
//! 这是原本 `pty.rs` 里唯一真正依赖 libc 的部分，2026-11-05 为 Windows 移植
//! （v0.7.2）从 `pty.rs` 拆出来。平台无关的 `Pty` 类型与 fd 读写留在
//! `pty/mod.rs`；本文件只回答「怎么在 Unix 上起一个 PTY」。
//!
//! **Linux 行为零变化**：代码是整体搬过来的，没有改写逻辑。macOS 的三处差异
//! （`execvpe` 不存在 / `forkpty` 签名不同 / `pipe2` 不存在）在 v0.7.2 子任务 3
//! 里按 `cfg(target_os = "macos")` 处理。

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use super::{Pty, PtyError, Size};

/// Unix 侧的 `Pty` 内部状态（`mod.rs` 的 `Pty` 在 `cfg(unix)` 下只持这个）。
#[derive(Debug)]
pub(super) struct UnixPty {
    pub(super) master: Option<OwnedFd>,
    pub(super) pid: libc::pid_t,
    pub(super) size: Size,
}

impl Pty {
    /// 起一个 PTY 并在子进程里 exec `argv[0]`，不设 cwd、不改环境。
    pub fn spawn(argv0: &str, args: &[String], size: Size) -> Result<Self, PtyError> {
        Self::spawn_in(argv0, args, size, None, &[])
    }

    /// 起一个 PTY，在子进程里 `chdir` 后 `execvpe`（把算好的环境直接交给子进程）。
    ///
    /// `argv` **不含 argv[0]**：调用方给的是命令 + 参数，第一个元素由
    /// 本函数按 `argv0` 补。这与 `execvpe` 的约定一致，也避免了调用方
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

        // `execvpe` 收的是 `*const *const c_char`（不必可变：环境只被读）。
        let mut envp: Vec<*const libc::c_char> = Vec::with_capacity(env_owned.len() + 1);
        envp.extend(env_owned.iter().map(|e| e.as_ptr()));
        envp.push(std::ptr::null());

        let winsize = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let mut master: libc::c_int = -1;
        // 测试环境无可控构造手段（防御性兜底）。尺寸非法那类**可测**的
        // 失败在上层就挡住了，不会走到这里。
        //
        // `forkpty` 的签名两平台不同（v0.7.2 子任务 3 首次真编实测）：
        // macOS 要 `*mut termios` / `*mut winsize`（可写指针），Linux 是
        // `*const`。同一份调用代码编译不过，所以这里分平台写。
        let pid = unsafe { platform_forkpty(&mut master, &winsize) };
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
        // **子进程**，父进程那份 llvm-cov 插桩在 `execvpe` 成功之后就没了，
        // 子进程里跑过的行永远统计不到；`chdir` 失败后的 `_exit(126)` 与
        // `execvpe` 失败后的 `_exit(127)` 还要额外的系统调用失败才走得到。
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
                // 用 `execvpe` 直接把 `envp` 交下去，**不写 `libc::environ`**：
                // 那个符号是 glibc 专有的，musl 上没有；而且环境已经在 `envp`
                // 里算好了，再往 `environ` 塞一遍等于两条真相源。
                //
                // **macOS 没有 `execvpe`**（glibc 专有，v0.7.2 子任务 3 实测
                // `cannot find function execvpe`）。那里的替身是「逐项 setenv
                // 后 `execvp`」：子进程此刻是 fork 出来的孤儿（单线程），
                // `setenv` 会分配但不用担心别的线程持锁——这正是本文件开头
                // 「fork 之前备好一切」那条纪律的例外，只在 macOS 分支成立。
                macos_aware_execvpe(program.as_ptr(), argv.as_ptr(), envp.as_ptr(), &env_owned);
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
            inner: UnixPty {
                master: Some(master),
                pid,
                size,
            },
        })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.inner.pid
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
        let fd = self.inner.master.as_ref().ok_or(PtyError::Closed)?;
        // coverage: excluded — `fcntl(F_DUPFD_CLOEXEC)` 失败需 fd 表耗尽，
        // 测试环境无可控构造手段（防御性兜底）。
        let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(PtyError::Io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(dup) })
    }

    pub fn size(&self) -> Size {
        self.inner.size
    }

    /// 改终端尺寸（`ioctl(TIOCSWINSZ)`）。
    ///
    /// **内核会在这时给前台进程组发 `SIGWINCH`**——所以 `resize` 一生效，
    /// 正在跑的程序立刻按新尺寸重画，不需要重新起一个 shell。
    pub fn resize(&mut self, size: Size) -> Result<(), PtyError> {
        let fd = self.inner.master.as_ref().ok_or(PtyError::Closed)?;
        apply_winsize(fd.as_raw_fd(), size)?;
        self.inner.size = size;
        Ok(())
    }

    /// 往 PTY 写（等价于用户在终端里敲键）。
    pub fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        let fd = self.inner.master.as_ref().ok_or(PtyError::Closed)?;
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
        read_handle(
            self.inner
                .master
                .as_ref()
                .ok_or(PtyError::Closed)?
                .as_raw_fd(),
            buf,
        )
    }

    /// 关掉 master fd。
    ///
    /// 关掉 master 会让前台进程组收到 `SIGHUP`——这正是「关掉终端面板 =
    /// 关掉那个 shell」想要的语义，而不是让它继续在设备上跑着没人管。
    /// `SIGHUP` 的处置由那个 shell 自己决定（bash 会退出）。
    pub fn close(&mut self) {
        drop(self.inner.master.take());
    }

    pub fn is_open(&self) -> bool {
        self.inner.master.is_some()
    }

    /// 只需要「在旁边等一下」的调用方（测试的 poll 超时）才用得上这个裸 fd。
    ///
    /// **只借，不接管所有权**：拿到它的人关不关 fd 与本类型无关。
    /// 需要能独立关掉的那一份走 [`Pty::dup_master`]。
    #[cfg(test)]
    pub(crate) fn raw_fd(&self) -> Result<RawFd, PtyError> {
        self.inner
            .master
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
pub(crate) fn read_handle(fd: RawFd, buf: &mut [u8]) -> Result<usize, PtyError> {
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
pub(crate) fn apply_winsize(fd: RawFd, size: Size) -> Result<(), PtyError> {
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

// ---------------------------------------------------------------------------
// 平台差异的两个封装：`forkpty` 签名、`execvpe` 是否存在
// ---------------------------------------------------------------------------
//
// 两处都是 v0.7.2 子任务 3 首次真编 `aarch64-apple-darwin` 实测出来的编译错误
// （见里程碑文档「首次真编的实测结果」）。封装成函数而不是在调用点内联 `cfg`，
// 是为了让 `spawn_in` 的主流程保持单一路径——「起 PTY」这件事两个 Unix 平台上
// 是同一件事，差异只在两个系统调用怎么调。

/// `forkpty`：macOS 要 `*mut`，Linux 是 `*const`。
#[cfg(target_os = "linux")]
unsafe fn platform_forkpty(master: &mut libc::c_int, winsize: &libc::winsize) -> libc::pid_t {
    libc::forkpty(
        master,
        std::ptr::null_mut(),
        std::ptr::null(),
        winsize as *const libc::winsize,
    )
}

#[cfg(not(target_os = "linux"))]
unsafe fn platform_forkpty(master: &mut libc::c_int, winsize: &libc::winsize) -> libc::pid_t {
    libc::forkpty(
        master,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        winsize as *const libc::winsize as *mut libc::winsize,
    )
}

/// `execvpe(program, argv, envp)`：Linux 有，macOS 没有。
#[cfg(target_os = "linux")]
unsafe fn macos_aware_execvpe(
    program: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    _env: &[CString],
) {
    libc::execvpe(program, argv, envp);
}

/// macOS 的替身：逐项 `setenv` 后 `execvp`。
///
/// 为什么不用 `posix_spawn`：调用点已经在 `forkpty` 的子进程里，再用 spawn
/// 会多起一层进程。子进程此刻是**单线程孤儿**（`forkpty` 之后、`exec` 之前），
/// 所以 `setenv` 内部的分配不会踩到别的线程持有的锁——这正是本文件开头
/// 「fork 之前备好一切」那条纪律唯一成立的例外。
///
/// `env` 里每项已经是 `KEY=VALUE` 形式的 `CString`（调用方在 fork 前算好），
/// 这里按第一个 `=` 拆成两次 `setenv`。
#[cfg(not(target_os = "linux"))]
unsafe fn macos_aware_execvpe(
    program: *const libc::c_char,
    argv: *const *const libc::c_char,
    _envp: *const *const libc::c_char,
    env: &[CString],
) {
    for entry in env {
        // `entry` 是 `KEY=VALUE` 形式的 `CString`（不含内部 NUL）。
        let bytes = entry.as_bytes();
        let Some(eq) = bytes.iter().position(|b| *b == b'=') else {
            continue;
        };
        let key = CString::new(&bytes[..eq]).unwrap_or_default();
        let value = CString::new(&bytes[eq + 1..]).unwrap_or_default();
        libc::setenv(key.as_ptr(), value.as_ptr(), 1);
    }
    libc::execvp(program, argv);
}

/// 等 fd 可读，最多 `budget`。**只等可读**。
///
/// **POLLHUP / POLLERR 也要当「可读」**：PTY 的从端关闭时内核先报挂断，
/// 那一瞬间未必同时带 `POLLIN`。只认 `POLLIN` 的话读循环会永远跳过
/// `read`，于是永远看不到那个代表「子进程退了」的 0 字节。
pub(crate) fn wait_readable(fd: RawFd, budget: std::time::Duration) -> bool {
    let ms = i64::try_from(budget.as_millis().min(i64::MAX as u128)).unwrap_or(i64::MAX);
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // coverage: excluded — `poll` 失败需 fd 无效或 `nfds` 超范围，
    // 这里传的都是内核刚给的 fd（防御性兜底）。
    let rc = unsafe { libc::poll(&raw mut pfd, 1, ms as libc::c_int) };
    rc > 0 && pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}
