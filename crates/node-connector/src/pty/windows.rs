//! Windows 侧实现：ConPTY（`CreatePseudoConsole` + `CreateProcessW`）。
//!
//! ## 为什么不是「加个 cfg 分支」
//!
//! Unix 那侧的 `forkpty` 一次系统调用做完了四件事：建 PTY 对、`setsid`、
//! `TIOCSCTTY`、把 0/1/2 重定向到从端。Windows 上**没有 `fork`**，也没有
//! 「子进程继承 fd」这回事——每一件都要显式做：
//!
//! | Unix | Windows |
//! |---|---|
//! | `forkpty` 建对 | `CreatePseudoConsole` 建 ConPTY + 两根匿名管道 |
//! | `fork` 复制本进程 | `CreateProcessW` 起新进程 |
//! | `dup2` 重定向 0/1/2 | `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` |
//! | `execvpe` 换影像 | `CreateProcessW` 的 `lpApplicationName` / `lpCommandLine` |
//! | `ioctl(TIOCSWINSZ)` | `ResizePseudoConsole` |
//! | master fd | 管道的**写端**（我们写进 ConPTY）+ **读端**（读出 ConPTY 输出） |
//!
//! ## 与 `unix.rs` 的接口一致性
//!
//! `Pty::spawn` / `Pty::spawn_in` 的签名两平台必须一致（里程碑「关键设计
//! 问题 1」：同签名、按 `cfg` 分实现，不引入 trait）。但**内部数据布局不同**：
//! Unix 是「一个 master fd」，Windows 是「一个输入写端 + 一个输出读端 + 一个
//! 进程句柄 + 一个 ConPTY 句柄」。所以 `Pty` 的字段在 `mod.rs` 里按 `cfg` 分。
//!
//! ## 行为必然有差异，且不假装没有
//!
//! - `Ctrl-C` 在 ConPTY 里由终端宿主处理成 `VK_CANCEL`，不是 Unix 的 SIGINT；
//! - 关掉面板 = `ClosePseudoConsole`，它会给子进程发 `CTRL_CLOSE_EVENT`，
//!   而不是 Unix 的 `SIGHUP`；
//! - `Pty::pid()` 返回的是 `CreateProcessW` 给的 PID，语义与 Unix 的
//!   `forkpty` 返回值相同（都是直接子进程），但没有进程组概念。
//!
//! 这些差异写进 `docs/DEVELOPMENT.md` 的「已知行为差异」一节（子任务 8）。

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_NONE;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTUPINFOEXW, STARTUPINFOW,
};

use super::{Pty, PtyError, Size};
// 引号拼接 / 环境块 / UTF-16 都是纯逻辑，独立成 `connector::win_command`
// （平台无关编译 + 本地单测）。这里只留系统调用。
use crate::connector::win_command::{command_line, environment_block, wide};

/// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` 是 `UpdateProcThreadAttribute` 的
/// 属性 id。`windows-sys` 把它定义成宏式的常量，直接用即可。

/// 一对匿名管道（ConPTY 需要两根：一根喂输入、一根收输出）。
struct PipePair {
    read: OwnedHandle,
    write: OwnedHandle,
}

impl PipePair {
    fn new() -> Result<Self, PtyError> {
        let mut read: HANDLE = ptr::null_mut();
        let mut write: HANDLE = ptr::null_mut();
        // 默认继承标志：`CreatePipe` 的两端都不可继承——我们要显式把它们交给
        // ConPTY（由 `CreatePseudoConsole` 持有），而不是让子进程继承。
        let ok = unsafe { CreatePipe(&mut read, &mut write, ptr::null_mut(), 0) };
        if ok == 0 {
            return Err(PtyError::Start(format!(
                "CreatePipe failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self {
            read: unsafe { OwnedHandle::from_raw_handle(read as RawHandle) },
            write: unsafe { OwnedHandle::from_raw_handle(write as RawHandle) },
        })
    }

    fn raw_read(&self) -> HANDLE {
        self.read.as_raw_handle() as HANDLE
    }

    fn raw_write(&self) -> HANDLE {
        self.write.as_raw_handle() as HANDLE
    }
}

/// Windows 侧的 `Pty` 内部状态（`mod.rs` 的 `Pty` 在 `cfg(windows)` 下持这个）。
#[derive(Debug)]
pub(super) struct WindowsPty {
    /// 喂给 ConPTY 的输入写端。写它 = 用户敲键盘。
    pub(super) input: OwnedHandle,
    /// 从 ConPTY 读输出的读端。读它 = 看屏幕。
    pub(super) output: OwnedHandle,
    /// ConPTY 句柄（`isize` 不透明值，`NO_CONPTY` = 0 表示未建）。
    pub(super) conpty: HPCON,
    /// 直接子进程（shell）的 PID。
    ///
    /// **不持进程句柄**：终端不需要等它退出（关了 ConPTY 就是关了它），
    /// 而持着它就得有人负责关。`CreateProcessW` 给的句柄在这里当场关掉。
    pub(super) pid: u32,
    pub(super) size: Size,
}

/// `HPCON` 在 `windows-sys` 里是 `isize`（不透明句柄的整型别名），
/// 不是裸指针——所以判空要用「不等于 0」而不是 `is_null()`。
/// `CreatePseudoConsole` 失败时不会写它，成功时给一个非零的不透明值。
const NO_CONPTY: HPCON = 0;

impl WindowsPty {
    fn close_conpty(&mut self) {
        if self.conpty != NO_CONPTY {
            unsafe { ClosePseudoConsole(self.conpty) };
            self.conpty = NO_CONPTY;
        }
    }
}

impl Drop for WindowsPty {
    fn drop(&mut self) {
        self.close_conpty();
    }
}

/// 起一个 ConPTY 并运行 `program`。
pub(super) fn spawn(
    program: &str,
    args: &[String],
    size: Size,
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
) -> Result<WindowsPty, PtyError> {
    // 尺寸下限与 Unix 侧一致：0 会被当成「未设置」。
    let cols = size.cols.max(1);
    let rows = size.rows.max(1);

    let input = PipePair::new()?;
    let output = PipePair::new()?;

    // ConPTY 收「输入读端」与「输出写端」——注意方向：
    //   我们 write → input.write → input.read → ConPTY → output.write
    //        → output.read → 我们 read
    let mut conpty: HPCON = NO_CONPTY;
    let hr = unsafe {
        CreatePseudoConsole(
            COORD {
                X: cols as i16,
                Y: rows as i16,
            },
            input.raw_read(),
            output.raw_write(),
            0,
            &mut conpty,
        )
    };
    if hr != S_OK {
        return Err(PtyError::Start(format!(
            "CreatePseudoConsole failed: HRESULT 0x{hr:08X}"
        )));
    }

    // 属性表：把 ConPTY 句柄挂到子进程上。这是 Windows 上「让子进程
    // 连上伪终端」的唯一方式。
    let mut attrs_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attrs_size);
    }
    let mut attrs_buf = vec![0u8; attrs_size];
    let attrs = attrs_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    let ok = unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut attrs_size) };
    if ok == 0 {
        unsafe { ClosePseudoConsole(conpty) };
        return Err(PtyError::Start(format!(
            "InitializeProcThreadAttributeList failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            conpty as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        unsafe {
            DeleteProcThreadAttributeList(attrs);
            ClosePseudoConsole(conpty);
        }
        return Err(PtyError::Start(format!(
            "UpdateProcThreadAttribute failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attrs;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let cmd = command_line(program, args);
    let mut cmd_w = wide(&cmd);
    let cwd_w = cwd.map(|p| wide(&p.to_string_lossy()));
    let mut env_block = environment_block(env);

    let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
    let ok = unsafe {
        CreateProcessW(
            ptr::null(), // lpApplicationName：留给命令行解析，支持 PATH 查找
            cmd_w.as_mut_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            0, // 不继承句柄：ConPTY 靠属性表交过去
            flags,
            env_block.as_mut_ptr() as *const core::ffi::c_void,
            cwd_w.as_ref().map_or(ptr::null(), |w| w.as_ptr()),
            &si.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        )
    };

    // 属性表用完即弃；ConPTY 自己的生命周期由 `WindowsPty` 管。
    unsafe { DeleteProcThreadAttributeList(attrs) };

    if ok == 0 {
        let err = std::io::Error::last_os_error();
        unsafe { ClosePseudoConsole(conpty) };
        return Err(PtyError::Start(format!(
            "CreateProcessW failed for {cmd}: {err}"
        )));
    }

    // 两个句柄都不需要：进程身份用 PID 就够，终端的生命周期由 ConPTY 决定。
    unsafe {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }

    Ok(WindowsPty {
        input: input.write,
        output: output.read,
        conpty,
        pid: pi.dwProcessId,
        size: Size { cols, rows },
    })
}

/// 改尺寸：`ResizePseudoConsole`。
pub(super) fn resize(pty: &mut WindowsPty, size: Size) -> Result<(), PtyError> {
    let cols = size.cols.max(1);
    let rows = size.rows.max(1);
    let hr = unsafe {
        ResizePseudoConsole(
            pty.conpty,
            COORD {
                X: cols as i16,
                Y: rows as i16,
            },
        )
    };
    if hr != S_OK {
        return Err(PtyError::Start(format!(
            "ResizePseudoConsole failed: HRESULT 0x{hr:08X}"
        )));
    }
    pty.size = Size { cols, rows };
    Ok(())
}

/// 复制一份输出读端（与 Unix 的 `dup_master` 同义：读线程独占一份，
/// 调用方留一份，关掉时两边互不干扰）。
pub(super) fn dup_output(pty: &WindowsPty) -> Result<OwnedHandle, PtyError> {
    use windows_sys::Win32::Storage::FileSystem::ReOpenFile;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ as READ;
    let handle = unsafe {
        ReOpenFile(
            pty.output.as_raw_handle() as HANDLE,
            READ,
            FILE_SHARE_NONE,
            0, // 与原件同属性
        )
    };
    if handle.is_null() {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}

/// 写输入。与 Unix 的 `write` 同语义：写不进去 = 终端没了。
pub(super) fn write(pty: &WindowsPty, bytes: &[u8]) -> Result<(), PtyError> {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    let mut written: u32 = 0;
    let ok = unsafe {
        WriteFile(
            pty.input.as_raw_handle() as HANDLE,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// 读输出。返回 0 表示「对端关了」（与 Unix 的 `EIO → Ok(0)` 同一口径）。
pub(super) fn read(pty: &WindowsPty, buf: &mut [u8]) -> Result<usize, PtyError> {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    let mut got: u32 = 0;
    let ok = unsafe {
        ReadFile(
            pty.output.as_raw_handle() as HANDLE,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut got,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        // `ERROR_BROKEN_PIPE` = 对端关了：这是「读完了」，不是错误。
        if err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE as i32) {
            return Ok(0);
        }
        return Err(PtyError::Io(err));
    }
    Ok(got as usize)
}

// ---------------------------------------------------------------------------
// `Pty` 的 Windows 方法集：签名与 `unix.rs` 逐一对应
// ---------------------------------------------------------------------------
//
// 为什么方法实现放在这里而不是 `mod.rs`：两个平台的内核对象完全不同
// （fd vs HANDLE/HPCON），共享不了任何实现细节；共享的只有签名。放在各自
// 模块里，`mod.rs` 就退化成纯类型骨架，读代码时不会误以为有共用逻辑。

#[cfg(windows)]
impl Pty {
    /// 起一个 ConPTY 并在里面运行 `argv0`（`args` 不含 argv0）。
    pub fn spawn(argv0: &str, args: &[String], size: Size) -> Result<Self, PtyError> {
        Self::spawn_in(argv0, args, size, None, &[])
    }

    /// 起一个 ConPTY，在 `cwd` 里运行 `argv0`，并在继承环境上**追加** `env`。
    ///
    /// `env` 的语义与 Unix 侧一致（追加而非替换），见 `environment_block`。
    /// `cwd` 交给 `CreateProcessW` 的 `lpCurrentDirectory`，失败时 `CreateProcessW`
    /// 自己报错，不会静默用父进程的目录。
    pub fn spawn_in(
        argv0: &str,
        args: &[String],
        size: Size,
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
    ) -> Result<Self, PtyError> {
        Ok(Self {
            inner: spawn(argv0, args, size, cwd, env)?,
        })
    }

    /// 直接子进程（shell）的 PID。与 Unix 的 `forkpty` 返回值同义。
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// 复制一份**输出读端**，语义等价于 Unix 的 `dup_master`：
    /// 读线程独占一份、调用方留一份，关掉时互不干扰。
    pub fn dup_master(&self) -> Result<OwnedHandle, PtyError> {
        dup_output(&self.inner)
    }

    pub fn size(&self) -> Size {
        self.inner.size
    }

    /// 改终端尺寸（`ResizePseudoConsole`）。ConPTY 会通知子进程重画。
    pub fn resize(&mut self, size: Size) -> Result<(), PtyError> {
        resize(&mut self.inner, size)
    }

    /// 往 ConPTY 写（等价于用户敲键）。
    pub fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        write(&self.inner, bytes)
    }

    /// 从 ConPTY 读输出。返回 0 表示对端关了（与 Unix 的 `EIO → Ok(0)` 同口径）。
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        read(&self.inner, buf)
    }

    /// 关掉终端。`ClosePseudoConsole` 会给子进程发 `CTRL_CLOSE_EVENT`
    /// ——这是 Windows 上「关面板 = 关 shell」的对应物（Unix 是 `SIGHUP`）。
    pub fn close(&mut self) {
        self.inner.close_conpty();
    }

    pub fn is_open(&self) -> bool {
        self.inner.conpty != NO_CONPTY
    }

    /// 只需要「在旁边等一下」的调用方（测试的 poll 超时）才用得上这个裸句柄。
    ///
    /// 名字不叫 `raw_fd`：Windows 上没有 fd，`RawHandle` 是内核句柄，
    /// 两者混用会让人以为它能塞进 `libc::poll` 之类的签名里。
    ///
    /// **Windows 侧目前没有调用点**：`poll_wait` 与用到它的用例都在
    /// `testutil`（`#[cfg(all(test, unix))]`）。这里留着是因为 Windows 的
    /// PTY 用例（需要真 ConPTY、只能在 CI 上跑）要用它来等输出口可读；
    /// 在那些用例落地之前它由 `#[allow]` 压住，不留一个安静的未用警告。
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn raw_read_handle(&self) -> Result<RawHandle, PtyError> {
        if self.inner.conpty == NO_CONPTY {
            return Err(PtyError::Closed);
        }
        Ok(self.inner.output.as_raw_handle())
    }
}

#[cfg(windows)]
impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}

/// 从一个「输出读端」句柄读至多 `buf.len()` 字节。**返回 0 = 对端关了**。
///
/// 与 Unix 的 `read_handle` 同契约：`EIO` 归到「读完了」，这里对应
/// `ERROR_BROKEN_PIPE`。认错一次就会让用户每敲一次 `exit` 都看到一句
/// IO 错误，所以两条路必须共用这同一份判断。
///
/// **不阻塞**：调用方先用 `wait_readable` 等到可读，那时 `ReadFile` 立刻返回。
pub(crate) fn read_handle(handle: RawHandle, buf: &mut [u8]) -> Result<usize, PtyError> {
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    let mut got: u32 = 0;
    let ok = unsafe {
        ReadFile(
            handle as HANDLE,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut got,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            return Ok(0);
        }
        return Err(PtyError::Io(err));
    }
    Ok(got as usize)
}

/// 等句柄可读，最多 `budget`。**进程句柄也算**：进程退出同样意味着
/// 「输出到此为止」，让调用方去读一次就能拿到 `BROKEN_PIPE`。
pub(crate) fn wait_readable(handle: RawHandle, budget: std::time::Duration) -> bool {
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let ms = u32::try_from(budget.as_millis().min(u32::MAX as u128)).unwrap_or(u32::MAX);
    let rc = unsafe { WaitForSingleObject(handle as HANDLE, ms) };
    // `WAIT_TIMEOUT`（258）是唯一「没得读」的结果；其余（`WAIT_OBJECT_0` 等）
    // 都让调用方去读。等待失败也返回值让它去读——真错了会由读路径摘掉。
    rc != 258
}
