//! Windows 侧的一次性命令执行：`CreateProcessW` + 两根可继承管道。
//!
//! ## 与 Unix 的对应关系
//!
//! | Unix（`exec.rs` 里的 `Child`） | Windows（本文件） |
//! |---|---|
//! | `pipe2(O_CLOEXEC \| O_NONBLOCK)` | `CreatePipe` + 句柄显式可继承 |
//! | `fork` | `CreateProcessW` |
//! | 子进程 `dup2` 0/1/2 | `STARTUPINFO` 的 `hStdOutput` / `hStdError` |
//! | `execvp`（查 `PATH`） | `CreateProcessW` 的 `lpApplicationName = NULL` + 命令行 |
//! | `poll(2)` | `WaitForMultipleObjects` |
//! | `kill(pid, SIGKILL)` | `TerminateProcess` |
//! | `waitpid` + `WEXITSTATUS` | `WaitForSingleObject` + `GetExitCodeProcess` |
//!
//! ## 一个必须说清的差别：没有非阻塞读
//!
//! Windows 的匿名管道没有 `O_NONBLOCK`。我们用 `WaitForMultipleObjects`
//! 等「至少一根管道可读」，再对**那一根**做阻塞 `ReadFile`——因为它已被
//! 内核标记为可读，阻塞读会立刻返回。这与 Unix 侧「`poll` 说可读 → 非阻塞
//! 读」是同一个时序假设，只是把「非阻塞」换成了「内核已保证可读」。
//!
//! 唯一的例外是 **EOF**：`ReadFile` 在写端全关时立刻返回
//! `ERROR_BROKEN_PIPE`，所以「关掉了」这个信号照样不会挂住循环。
//!
//! ## 退出码同样做「被信号杀死」的映射
//!
//! Unix 上 `128 + signal`；Windows 上 `TerminateProcess(handle, code)` 的 code
//! 由我们给，所以超时杀掉时直接写 `128 + 9`——让上层的「超时/崩溃」诊断
//! 在两个平台上读同一个数。子进程自己退出的码原样穿过（协议约定
//! `exit(10)`=请求更新、`exit(11)`=健康失败，压缩成「非零」就什么也判不出来）。

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, ERROR_BROKEN_PIPE, HANDLE, HANDLE_FLAG_INHERIT,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

// `OP_RUN` / `EXIT_KILLED` / `INFINITE` 都定义在 `exec.rs`：它们是**跨平台的
// 约定值**（错误文案、超时退出码、无限等待），不属于 Windows 后端独有。
use super::exec::{Collector, Plan, EXIT_KILLED, INFINITE, OP_RUN};
use super::ConnectorError;
use crate::win_command::{command_line, environment_block, reject_nul, wide};

/// `CreateProcessW` 之前备好的载荷（全部 UTF-16，含结尾 NUL）。
///
/// 与 Unix 的 `ExecStrings` 同一个用意：所有会在子进程侧出错的东西
/// **在 `open` 里就校验完**，别等到 `poll` 才发现。
pub(super) struct Prepared {
    /// 整条命令行（程序名 + 已加引号的参数）。
    pub(super) command_line: Vec<u16>,
    pub(super) cwd: Option<Vec<u16>>,
    pub(super) env: Vec<u16>,
}

/// 校验并编码参数。`open` 同步调用——错误在这里能当场回给调用方。
pub(super) fn prepare(plan: &Plan) -> Result<Prepared, ConnectorError> {
    // Windows 的进程参数不能含 NUL（会截断命令行），与 Unix 的 C 字符串同理。
    // 判据与文案都走 `win_command::reject_nul`，与 `pty` 侧同一份。
    reject_nul(&plan.command, &plan.args).map_err(|detail| ConnectorError::Invalid { detail })?;
    let cwd = plan.cwd.as_ref().map(|p| {
        let s = p.to_string_lossy();
        wide(&s)
    });
    Ok(Prepared {
        command_line: wide(&command_line(&plan.command, &plan.args)),
        cwd,
        env: environment_block(&plan.env),
    })
}

/// 子进程 + 它那两根管道的**读端**（写端在 spawn 结束时就关掉了）。
pub(super) struct Child {
    process: OwnedHandle,
    /// `[Some(stdout 读端), Some(stderr 读端)]`。**读到 EOF 就置 `None`**，
    /// 与 Unix 侧同一种记录方式。
    pub(super) pipes: Vec<Option<OwnedHandle>>,
}

fn raw(h: &OwnedHandle) -> HANDLE {
    h.as_raw_handle() as HANDLE
}

/// 建一根管道，**只有写端可继承**，返回 `(读端, 写端)`。
///
/// ## 为什么写端可继承、读端不可继承
///
/// 这套模型与微软官方「Creating a Child Process with Redirected Input and
/// Output」示例一致：`SECURITY_ATTRIBUTES.bInheritHandle = TRUE` 建出两端都
/// 可继承的句柄，然后用 `SetHandleInformation(读端, HANDLE_FLAG_INHERIT, 0)`
/// 把读端改回不可继承。两步都不能省：
///
/// - **写端必须可继承**，否则子进程拿不到它，`hStdOutput` 指向一个自己
///   继承不到的句柄——子进程的输出无处可去。
/// - **读端必须不可继承**，否则子进程手里也握着一根读端。「读端 EOF 取决于
///   所有写端都关上」这条判据因此永远不成立：我们这边 `drop(out_w)` 之后
///   子进程仍持有读端，而读端不写数据，EOF 等不来——`run()` 会一路挂到超时。
///
/// 早先这里两端都留在 `bInheritHandle = TRUE`，靠 `bInheritHandles = FALSE`
/// 兜住——那是**两处都错、且互相掩盖**：`FALSE` 让子进程连写端都拿不到
/// （输出恒为空），而两端可继承的写法让人以为机制是对的。微软示例里
/// `bInheritHandles` 传的是 `TRUE`。
fn make_pipe() -> Result<(OwnedHandle, OwnedHandle), ConnectorError> {
    let sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    // `CreatePipe` 只读 `sa`，传 `&sa` 即可（写成 `&mut sa` 会让 clippy 报
    // `needless_pass_by_ref_mut`——那是在说「这个可变借用没人需要」）。
    let ok = unsafe { CreatePipe(&mut read, &mut write, &sa, 0) };
    if ok == 0 {
        return Err(pipe_error("CreatePipe"));
    }
    // 读端留在父进程：改回不可继承，理由见上面文档。
    if unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) } == 0 {
        let err = std::io::Error::last_os_error();
        // 两个句柄已经建出来了，出错时得自己关，否则泄漏。
        unsafe {
            CloseHandle(read);
            CloseHandle(write);
        }
        return Err(ConnectorError::Failed {
            op: OP_RUN.to_string(),
            detail: format!("SetHandleInformation failed: {err}"),
        });
    }
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read as RawHandle),
            OwnedHandle::from_raw_handle(write as RawHandle),
        )
    })
}

fn pipe_error(what: &str) -> ConnectorError {
    ConnectorError::Failed {
        op: OP_RUN.to_string(),
        detail: format!("{what} failed: {}", std::io::Error::last_os_error()),
    }
}

impl Child {
    /// 起进程。
    ///
    /// 标准句柄的接法：`prepared.command_line` 里已经含程序名，所以
    /// `lpApplicationName` 传 `NULL` 让 `CreateProcessW` 自己按 `PATH` 解析。
    ///
    /// **启动期的系统调用失败一律归 `ConnectorError::Failed { op: OP_RUN }`**，
    /// 不用别的变体：`Failed` 渲染成「run command failed: …」，用户看到的是
    /// **哪个操作失败了**，而这正是启动失败该说的。读写期的错误则走各自的
    /// 路径（`read_into` 把管道摘掉并 `warn!`），因为那已经是「命令跑起来了，
    /// 但输出读不下去」——两件事对用户的意义完全不同，不该合并成同一个错误。
    pub(super) fn spawn(_plan: &Plan, prepared: &Prepared) -> Result<Self, ConnectorError> {
        let (out_r, out_w) = make_pipe()?;
        let (err_r, err_w) = make_pipe()?;
        // stdin：**真的**建一根空管道，然后立刻在我们这边关掉写端。
        // 只在那根写端是唯一写端时关掉，子进程的第一次读就能拿到 EOF——
        // 于是 `cat` 之类的命令立刻返回，而不是继承我们的控制台把面板挂住。
        // （Unix 侧对应 `open("/dev/null")` + `dup2(null, 0)`。）
        let (in_r, in_w) = make_pipe()?;
        drop(in_w);

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
        si.hStdInput = raw(&in_r);
        si.hStdOutput = raw(&out_w);
        si.hStdError = raw(&err_w);

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // 三个 `Vec<u16>` 只是为了取 `*mut` 指针，`CreateProcessW` 不写它们；
        // 但 `as_mut_ptr` 需要 `mut` 绑定，所以这里留 `mut`。
        let command_line_w = &prepared.command_line;
        let dir_w = prepared.cwd.as_deref();
        let env_block_w = &prepared.env;

        let ok = unsafe {
            CreateProcessW(
                ptr::null(), // 留给命令行解析：这样能按 PATH 找到程序
                command_line_w.as_ptr() as *mut u16,
                ptr::null_mut(),
                ptr::null_mut(),
                // **必须 TRUE**：子进程靠句柄继承拿到 `si` 里的三个标准句柄。
                // 传 FALSE 那些字段会被忽略，子进程输出无处可去（读端立刻 EOF）。
                // 可继承的范围只有 `make_pipe` 留下的写端——读端已改回不可继承。
                1,
                CREATE_NO_WINDOW,
                env_block_w.as_ptr() as *const core::ffi::c_void,
                dir_w.map_or(ptr::null(), |w| w.as_ptr()),
                &si,
                &mut pi,
            )
        };
        // 写端与 stdin 读端在我们这边必须立刻关掉：
        // - 写端留着，读端就永远等不到 EOF（「所有写端都关上」才 EOF）；
        // - stdin 读端留着，子进程那边的写端就不会…… 其实反过来，读端留着
        //   只是多一个句柄，但一并关掉更干脆。
        drop(out_w);
        drop(err_w);
        drop(in_r);

        if ok == 0 {
            let err = std::io::Error::last_os_error();
            let cmd = String::from_utf16_lossy(&prepared.command_line);
            return Err(ConnectorError::Failed {
                op: OP_RUN.to_string(),
                detail: format!(
                    "CreateProcessW failed for {}: {err}",
                    cmd.trim_end_matches('\0')
                ),
            });
        }
        unsafe { CloseHandle(pi.hThread) };

        Ok(Self {
            process: unsafe { OwnedHandle::from_raw_handle(pi.hProcess as RawHandle) },
            pipes: vec![Some(out_r), Some(err_r)],
        })
    }

    pub(super) fn any_open(&self) -> bool {
        self.pipes.iter().any(Option::is_some)
    }

    /// 等两根管道里至少一根可读，最多 `budget_ms` 毫秒。**返回能不能读**。
    ///
    /// `WAIT_TIMEOUT` = 都没有；`WAIT_FAILED`（负数）也返回 `false`——
    /// 下一轮会重新等，而真错了会由 `read_into` 的读失败路径摘掉管道。
    pub(super) fn wait_readable(&self, budget_ms: i32) -> bool {
        // `handles` 的第一个永远是进程句柄：进程退出也该唤醒这里的等待，
        // 否则「命令跑完但没输出」会一路等到超时。
        let mut handles: Vec<HANDLE> = vec![self.process.as_raw_handle() as HANDLE];
        // 只等还开着的那几根；已 EOF 的不再等（`pipes` 里被置 `None`）。
        handles.extend(
            self.pipes
                .iter()
                .flatten()
                .map(|h| h.as_raw_handle() as HANDLE),
        );
        let rc = unsafe {
            WaitForMultipleObjects(
                handles.len() as u32,
                handles.as_ptr(),
                0,
                // `budget_ms` 来自 `exec.rs` 的 `remaining.as_millis() as i32`，
                // 恒非负；这里的 `0` 是不可达兜底，`0` 在 `WaitForMultipleObjects`
                // 里是「立刻返回」而不是「永远等」——写成 `u32::MAX` 会把一个
                // 不该发生的溢出变成一次 49 天的挂起。
                u32::try_from(budget_ms).unwrap_or(0),
            )
        };
        if rc == WAIT_TIMEOUT {
            return false;
        }
        // 任何一根可读（或进程退出、或等待失败）都让调用方去 `read_into`：
        // 进程退出后管道写端随之关闭，`ReadFile` 会立刻给 `BROKEN_PIPE`，
        // 把「读完了」这个信息带回来。
        true
    }

    /// 从仍然开着的那些读端各读一次。**返回有没有真的读到字节**。
    ///
    /// 与 Unix 侧同约定：读到 EOF（`ERROR_BROKEN_PIPE`）就把那根置 `None`，
    /// 别的错误则摘掉——留着它会让主循环每轮立刻失败又立刻重试（热循环）。
    pub(super) fn read_into(&mut self, out: &mut Collector) -> bool {
        let mut buf = vec![0u8; super::exec::READ_CHUNK];
        let mut got_any = false;
        for stream in 0..self.pipes.len() {
            let Some(handle) = self.pipes.get(stream).and_then(Option::as_ref) else {
                continue;
            };
            let mut got: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    raw(handle),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut got,
                    ptr::null_mut(),
                )
            };
            if ok != 0 && got > 0 {
                if let Some(chunk) = buf.get(..got as usize) {
                    out.push(stream, chunk);
                    got_any = true;
                }
                continue;
            }
            let err = std::io::Error::last_os_error();
            if ok == 0 && err.raw_os_error() != Some(ERROR_BROKEN_PIPE as i32) {
                tracing::warn!(stream, error = %err, "pipe read failed; dropping it");
            }
            // 成功但 0 字节 = 对端关了；`BROKEN_PIPE` 也是。都算 EOF。
            if let Some(slot) = self.pipes.get_mut(stream) {
                *slot = None;
            }
        }
        got_any
    }

    /// 超时后杀掉。`TerminateProcess` 是全有或全无的，没有「先礼后兵」。
    pub(super) fn kill(&self) {
        unsafe { TerminateProcess(raw(&self.process), EXIT_KILLED as u32) };
    }

    /// 等退出并取退出码。**`-1` = 取不到**。
    pub(super) fn wait(self) -> i32 {
        unsafe {
            let rc = WaitForSingleObject(raw(&self.process), INFINITE);
            if rc != WAIT_OBJECT_0 {
                return -1;
            }
            let mut code: u32 = 0;
            if GetExitCodeProcess(raw(&self.process), &mut code) == 0 {
                return -1;
            }
            // `u32 → i32` 位保真：子进程 `exit(10)` 给 10，`TerminateProcess(…, 137)`
            // 给 137。只有 > i32::MAX 的码会变负，而那不是任何正常程序的退出码。
            code as i32
        }
    }
}
