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
//!
//! ## 错误类型怎么选：`Start` 还是 `Io`
//!
//! 本文件里两处都用得到，判据是**失败发生在哪个阶段**：
//!
//! - **`spawn` 期间**（建管道、`CreatePseudoConsole`、属性列表、
//!   `CreateProcessW`）→ `PtyError::Start`，带上「哪一步 + 参数」的描述。
//!   用户看到的是「终端开不起来」，需要知道是开哪个 shell 时失败的；把
//!   `last_os_error()` 塞进 `Io` 会让上层只会说「terminal io failed」，丢掉
//!   是哪个 shell、哪一步。
//! - **`spawn` 之后**（`read` / `write` / `dup_output`）→ `PtyError::Io`，
//!   直接带 `std::io::Error`：那时终端已经开着了，出错就是那次 IO 的事，
//!   不需要再解释上下文。
//!
//! 同一个 `std::io::Error` 在两种阶段变成两种变体，是刻意的；`mod.rs` 里
//! 两个变体的文案也据此分开（`Start` → "cannot start a terminal"）。

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, HANDLE, S_OK,
};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_NONE;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use super::{Pty, PtyError, Size};
// 引号拼接 / 环境块 / UTF-16 都是纯逻辑，独立成 `crate::win_command`
// （平台无关编译 + Linux 上就能跑的单测）。这里只留系统调用。
//
// 它放在 crate 根而不是 `connector` 下：否则 `pty` 会反向依赖 `connector`，
// 而依赖方向本是 `connector → pty`。
use crate::win_command::{command_line, environment_block, wide};

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

/// 关 ConPTY 前最多花这么久排空输出（见 `close_conpty`）。
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

impl WindowsPty {
    fn close_conpty(&mut self) {
        if self.conpty == NO_CONPTY {
            return;
        }
        // **先排空输出，再 `ClosePseudoConsole`**。MSDN 对此有明确警告：输出
        // 管道里还有没读走的字节时，conhost 的写端会阻塞，`ClosePseudoConsole`
        // 跟着无限等下去——关一个终端面板就能把整个 node 带住。排空是**有上限**
        // 的（`DRAIN_GRACE`），否则 `close` 会变成「读完整个输出流」。
        let deadline = std::time::Instant::now() + DRAIN_GRACE;
        let handle = self.output.as_raw_handle();
        let mut buf = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            if !wait_readable(handle, DRAIN_GRACE) {
                break;
            }
            match read_handle(handle, &mut buf) {
                // 0 = 对端关了（全部输出已读到）；错误同理，没有更多可排的。
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        unsafe { ClosePseudoConsole(self.conpty) };
        self.conpty = NO_CONPTY;
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
    // **缓冲区必须按指针边界对齐**（MSDN：`InitializeProcThreadAttributeList`
    // 的缓冲区 "must be aligned on a pointer boundary"）。`Vec<u8>` 只保证
    // 1 字节对齐——实践中 malloc 往往恰好给 16 字节，于是这个错在 x86_64 上
    // 通常不显形，但它仍是 UB（ARM64 上有对齐要求的访问会直接出问题）。
    // 用 `Vec<u64>` 分配：元素对齐 8，x86_64 / aarch64 的指针边界都满足。
    let words = attrs_size.div_ceil(std::mem::size_of::<u64>());
    let mut attrs_buf = vec![0u64; words];
    let attrs = attrs_buf.as_mut_ptr().cast::<core::ffi::c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST;
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
    // `STARTF_USESTDHANDLES` + 三个 `hStd*` 留 NULL 是 **ConPTY 的硬要求**，
    // 不是可选项：不带这个标志时 `CreateProcessW` 会把我们的 stdout/stderr
    // 复制给子进程，于是 `cmd /c echo` 的输出直接进了 runner 的日志
    // （v0.7.2 的 `CONPTY-MARKER` 就出现在 libtest 输出行尾），ConPTY 的
    // `output.read` 永远读不到东西，用例挂到超时。带上标志且三端为 NULL，
    // 子进程的标准句柄才会落到属性表挂上的伪终端。
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
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
    // `close()` 之后 ConPTY 已被 `ClosePseudoConsole` 释放，`conpty` 只是
    // 归零的整数——再传给 `ResizePseudoConsole` 就是拿一个无效句柄去调内核。
    // Unix 侧对应的是 `master.take()` 之后 `master_side()?` 返回 `Closed`。
    check_open(pty)?;
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
        // 这里是 **spawn 之后**的调用，按文件头的判据该归 `Io` 而不是
        // `Start`：终端明明开着（或刚被关掉），说「开不起来」是错的。
        return Err(PtyError::Io(std::io::Error::other(format!(
            "ResizePseudoConsole failed: HRESULT 0x{hr:08X}"
        ))));
    }
    pty.size = Size { cols, rows };
    Ok(())
}

/// 已关闭的 PTY 不许再用。与 Unix 的 `master_side()?` 同一个判据。
///
/// 为什么需要它：`close_conpty` 只把 `conpty` 归零，`input`/`output` 两根管道
/// 句柄仍在结构体里——`read`/`write` 不查这一下，就会在 `ClosePseudoConsole`
/// 之后继续对那两根管道读写。此时 `Terminal::is_open()` 若只看「`pty` 是
/// 不是 `Some`」还会返回 `true`，于是上层以为终端还活着。
fn check_open(pty: &WindowsPty) -> Result<(), PtyError> {
    if pty.conpty == NO_CONPTY {
        return Err(PtyError::Closed);
    }
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
            // 这个参数**必须为 0**（MSDN 原文 "This parameter must be zero"）：
            // 它不是「沿用原件的属性」，传非 0 是未定义行为。
            0,
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
    check_open(pty)?;
    // **必须循环**：`WriteFile` 对管道会短写（管道缓冲不够时只写进去一部分，
    // 返回 `TRUE` + 一个小于请求长度的 `written`）。只看 `ok` 就返回 `Ok(())`
    // 会把剩下的字节静默丢掉——用户看到的是「粘贴一长段东西只进去一半」。
    // Unix 侧的 `write` 有同样的 `while off < len` 循环。
    let handle = pty.input.as_raw_handle() as HANDLE;
    let mut off = 0usize;
    while off < bytes.len() {
        let rest = bytes.get(off..).unwrap_or(&[]);
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                handle,
                rest.as_ptr(),
                rest.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PtyError::Io(std::io::Error::last_os_error()));
        }
        if written == 0 {
            // 成功但一个字节都没写进去：再转一圈就是死循环。
            return Err(PtyError::Io(std::io::ErrorKind::WriteZero.into()));
        }
        off += written as usize;
    }
    Ok(())
}

/// 读输出。返回 0 表示「对端关了」（与 Unix 的 `EIO → Ok(0)` 同一口径）。
///
/// **委托给 [`read_handle`]，不另写一份**：判据（`ERROR_BROKEN_PIPE` 归到
/// 「读完了」）只能有一处。抄一份的后果不是「多几行」，而是两处将来必然
/// 漂移一次，而漂移的那次就是用户每敲一次 `exit` 都看到一句 IO 错误。
/// Unix 侧的 `read()` 同样只是 `read_handle(self.master_side()?, buf)`。
pub(super) fn read(pty: &WindowsPty, buf: &mut [u8]) -> Result<usize, PtyError> {
    check_open(pty)?;
    read_handle(pty.output.as_raw_handle(), buf)
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
/// **`ERROR_PIPE_NOT_CONNECTED` 也算 EOF。** 匿名管道上「所有写端关闭」给的是
/// `ERROR_BROKEN_PIPE`，但当 ConPTY 的写端是在**另一侧被 `ClosePseudoConsole`
/// 带走**时，读端在部分 Windows 版本上给的是 `ERROR_PIPE_NOT_CONNECTED`（233）。
/// 把「对端关了」判成 `IO` 错误的代价恰好就是上面那句注释说的症状。当前环境
/// 无法实测是哪一个（要真 Windows），所以两个都归到 EOF：`PIPE_NOT_CONNECTED`
/// 在这种「只剩读端的管道」上本来也不可能是「等一下就有数据」，归到读完了是
/// 安全的。
///
/// **不阻塞**：调用方先用 `wait_readable` 等到可读，那时 `ReadFile` 立刻返回。
pub(crate) fn read_handle(handle: RawHandle, buf: &mut [u8]) -> Result<usize, PtyError> {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};
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
        if matches!(
            err.raw_os_error(),
            Some(code)
                if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32
        ) {
            return Ok(0);
        }
        return Err(PtyError::Io(err));
    }
    Ok(got as usize)
}

/// 等**输出读端**可读，最多 `budget`。返回 `true` = 可以让调用方去读一次。
///
/// **匿名管道句柄不能交给 `WaitForSingleObject`**：Windows 的可等待对象清单
/// 里没有匿名管道，传进去返回 `WAIT_FAILED`。v0.7.2 这里写的是
/// `rc != WAIT_TIMEOUT`，于是 `WAIT_FAILED` 被当成「可读」→ 无数据也去
/// `ReadFile` → 永久阻塞（`test-desktop (windows)` 挂了 53m51s 的真因）。
///
/// 现在用 `PeekNamedPipe` 查可读字节数：有字节才返回 true；暂时没有就按
/// 调用方的轮询节奏（`terminal.rs` 的 `READ_POLL_INTERVAL` 100ms / 测试用例
/// 的 100ms）睡到下一个切片。`budget` 不再用于内核等待——探测本身零阻塞，
/// 由调用方在循环头用 `Instant` 控制总截止时间（`a_conpty_runs_a_command...`
/// 的 20s deadline），语义与 Unix 侧「`poll` 说可读 → 读 → 0 字节即 EOF」
/// 一致。
///
/// `PeekNamedPipe` 返回 FALSE 时要分清两种情形：
///
/// - `ERROR_BROKEN_PIPE` / `ERROR_PIPE_NOT_CONNECTED`：**所有写端已关**。
///   此时必须返回 true，让调用方去 `read_handle` 拿同一条错误并收敛成
///   `Ok(0)`。若一律按「暂时没字节」处理，读线程会永远轮询下去——
///   `any_open()` 不收敛、`Terminal` 的 `done` 永远不置位。
/// - 其它：才是「暂时没字节」，按调用方的轮询节奏睡到下一个切片。
pub(crate) fn wait_readable(handle: RawHandle, _budget: std::time::Duration) -> bool {
    let mut available: u32 = 0;
    let ok = unsafe {
        PeekNamedPipe(
            handle as HANDLE,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    };
    if ok != 0 {
        return available > 0;
    }
    matches!(
        unsafe { GetLastError() },
        ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED
    )
}

// ---------------------------------------------------------------------------
// Windows 侧的 PTY 用例：**只能在 Windows 上跑**
// ---------------------------------------------------------------------------
//
// `mod.rs` 的 PTY 用例整块被 `#[cfg(all(test, unix))]` 挡着（它们起真的
// `/bin/sh`、用 `stty`、断言 POSIX 终端语义），所以在 `test-desktop` 的
// windows runner 上，PTY 这块**曾经只有编译验证、零行为验证**。这个模块就是
// 补那一块的：它用 ConPTY 真起一个 `cmd.exe`，验证「能起进程 / 能读到输出 /
// 能写进去 / resize 之后尺寸读回变了 / close 之后拒绝再用」。
//
// 断言刻意选**与具体 shell 无关**的部分（`echo` 的回显内容、退出行为），
// 避免把用例绑死在某个 Windows 版本的中文/英文提示符上。
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn size() -> Size {
        Size { cols: 80, rows: 24 }
    }

    /// 在一个真 ConPTY 里跑 `cmd /c echo MARKER`，把输出读回来。
    ///
    /// 这是本文件唯一一条「端到端」用例：它一次证明了**四件事**——
    /// `CreatePseudoConsole` 建起来了、属性表挂对了、`CreateProcessW` 起得来、
    /// 而且输出真的从 ConPTY 流回了我们手里。属性表挂错（比如没
    /// `UpdateProcThreadAttribute`）时子进程会连不上伪终端，输出根本收不到，
    /// 这条会红在「读到空」上。
    #[test]
    fn a_conpty_runs_a_command_and_we_read_its_output() {
        let mut pty = Pty::spawn(
            "cmd.exe",
            &["/c".into(), "echo CONPTY-MARKER".into()],
            size(),
        )
        .expect("spawn a conpty");
        let mut seen = String::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if !wait_readable(pty.inner.output.as_raw_handle(), Duration::from_millis(100)) {
                continue;
            }
            let mut buf = [0u8; 4096];
            match pty.read(&mut buf) {
                Ok(0) => break, // 对端关了：输出已经全拿到
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) => panic!("read failed after {seen:?}: {e}"),
            }
        }
        assert!(
            seen.contains("CONPTY-MARKER"),
            "ConPTY 的输出没流回来（属性表或句柄接错了？），实际读到：{seen:?}"
        );
        pty.close();
    }

    /// `close()` 之后 `read` / `write` / `resize` 必须一律报 `Closed`，
    /// **不能**拿着已经 `ClosePseudoConsole` 掉的句柄继续调内核。
    ///
    /// 这条钉的是 `check_open`。没有它，`conpty` 被归零之后 `ResizePseudoConsole(0, …)`
    /// 照样会被调下去；而 `read`/`write` 用的两根管道句柄在
    /// `ClosePseudoConsole` 之后状态是未定义的。`error` 一栏用 `matches!`
    /// 而不是比较字符串：文案会变，变体不会。
    #[test]
    fn a_closed_pty_refuses_read_write_and_resize() {
        let mut pty = Pty::spawn("cmd.exe", &["/c".into(), "exit".into()], size()).expect("spawn");
        pty.close();
        assert!(!pty.is_open(), "close() 之后 is_open() 必须是 false");

        let mut buf = [0u8; 64];
        assert!(
            matches!(pty.read(&mut buf), Err(PtyError::Closed)),
            "close() 之后 read 必须返回 Closed"
        );
        assert!(
            matches!(pty.write(b"x"), Err(PtyError::Closed)),
            "close() 之后 write 必须返回 Closed"
        );
        assert!(
            matches!(pty.resize(size()), Err(PtyError::Closed)),
            "close() 之后 resize 必须返回 Closed"
        );
    }

    /// `close()` 可以调用多次，`Drop` 再来一次也不炸（`NO_CONPTY` 哨兵挡住
    /// 双重 `ClosePseudoConsole`）。
    #[test]
    fn closing_twice_is_harmless() {
        let mut pty = Pty::spawn("cmd.exe", &["/c".into(), "exit".into()], size()).expect("spawn");
        pty.close();
        pty.close();
        assert!(!pty.is_open());
        drop(pty); // Drop 会再走一次 close_conpty
    }

    /// `resize` 之后 `size()` 读回新值——这是 `ResizePseudoConsole` 真的被
    /// 调到了的直接证据（`Pty::size()` 读的是我们自己记的那个字段）。
    #[test]
    fn resize_updates_the_reported_size() {
        let mut pty = Pty::spawn("cmd.exe", &["/c".into(), "exit".into()], size()).expect("spawn");
        assert_eq!(pty.size(), Size { cols: 80, rows: 24 });
        pty.resize(Size {
            cols: 120,
            rows: 40,
        })
        .expect("resize an open conpty");
        assert_eq!(
            pty.size(),
            Size {
                cols: 120,
                rows: 40
            }
        );
        pty.close();
    }

    /// `dup_master()` 出来的句柄是**另一份**读端：关掉它不影响原句柄。
    /// 读线程靠这个前提独占自己的 fd，否则它一退出就把整个终端关了。
    #[test]
    fn dup_master_gives_an_independent_handle() {
        let pty = Pty::spawn("cmd.exe", &["/c".into(), "exit".into()], size()).expect("spawn");
        let dup = pty.dup_master().expect("dup the output end");
        drop(dup);
        // 原句柄仍然可用：拿它读一次不 panic 就是判据（内容不重要，可能已经 EOF）。
        let mut buf = [0u8; 16];
        let _ = pty.read(&mut buf);
    }

    /// 尺寸下限：`0` 会被当成「未设置」，实际按 1 起（与 Unix 侧同一口径）。
    #[test]
    fn a_zero_size_is_clamped_to_one() {
        let mut pty = Pty::spawn(
            "cmd.exe",
            &["/c".into(), "exit".into()],
            Size { cols: 0, rows: 0 },
        )
        .expect("spawn with a zero size");
        assert_eq!(pty.size(), Size { cols: 1, rows: 1 });
        pty.close();
    }
}
