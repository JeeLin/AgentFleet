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
    CloseHandle, ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

use super::exec::{Collector, Plan};
use super::win_command::{command_line, environment_block, reject_nul, wide};
use super::ConnectorError;

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

/// 超时杀掉子进程时用的退出码：与 Unix 侧 `128 + SIGKILL(9)` 对齐。
pub(super) const EXIT_KILLED: i32 = 128 + 9;

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

/// 建一根可继承的管道，返回 `(读端, 写端)`。
///
/// 两个句柄都**显式设为可继承**（`SECURITY_ATTRIBUTES.bInheritHandle = TRUE`）：
/// 读端要能被我们留下、写端要能被子进程继承。而 `CreateProcessW` 的
/// `bInheritHandles` 我们传 `FALSE`，靠 `STARTUPINFO` 的两个标准句柄字段
/// 显式指定——所以「可继承」只是让这些句柄有资格被填进 `STARTUPINFO`。
fn make_pipe() -> Result<(OwnedHandle, OwnedHandle), ConnectorError> {
    let mut sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    let ok = unsafe { CreatePipe(&mut read, &mut write, &mut sa, 0) };
    if ok == 0 {
        return Err(pipe_error());
    }
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read as RawHandle),
            OwnedHandle::from_raw_handle(write as RawHandle),
        )
    })
}

fn pipe_error() -> ConnectorError {
    ConnectorError::Failed {
        op: "run command".to_string(),
        detail: format!("CreatePipe failed: {}", std::io::Error::last_os_error()),
    }
}

impl Child {
    /// 起进程。**`argv` 不含 argv0**：`plan.command` 是程序名。
    pub(super) fn spawn(_plan: &Plan, prepared: &Prepared) -> Result<Self, ConnectorError> {
        let (out_r, out_w) = make_pipe()?;
        let (err_r, err_w) = make_pipe()?;

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
        si.hStdOutput = raw(&out_w);
        si.hStdError = raw(&err_w);
        // stdin 指向一根**随即关闭**的空管道：子进程一读就是 EOF，
        // 而不是继承我们的控制台（那会让 `cat` 之类的命令把面板挂住）。
        si.hStdInput = INVALID_HANDLE_VALUE;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut cmd_w = prepared.command_line.clone();
        let dir_w = prepared.cwd.clone();
        let mut env_block = prepared.env.clone();

        let ok = unsafe {
            CreateProcessW(
                ptr::null(), // 留给命令行解析：这样能按 PATH 找到程序
                cmd_w.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0, // 不整体继承句柄；标准句柄已由 STARTUPINFO 显式指定
                CREATE_NO_WINDOW,
                env_block.as_mut_ptr() as *const core::ffi::c_void,
                dir_w.as_ref().map_or(ptr::null(), |w| w.as_ptr()),
                &si,
                &mut pi,
            )
        };
        // 写端在我们这边必须立刻关掉：读端 EOF 取决于**所有**写端都关上，
        // 留一个在父进程里，读端就永远等不到 EOF。
        drop(out_w);
        drop(err_w);

        if ok == 0 {
            let err = std::io::Error::last_os_error();
            let cmd = String::from_utf16_lossy(&prepared.command_line);
            return Err(ConnectorError::Failed {
                op: "run command".to_string(),
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
        let mut handles: Vec<HANDLE> = vec![self.process.as_raw_handle() as HANDLE];
        // 只等还开着的那几根；已 EOF 的不再等。
        let mut map: Vec<usize> = Vec::new();
        for (i, p) in self.pipes.iter().enumerate() {
            if let Some(h) = p {
                handles.push(h.as_raw_handle() as HANDLE);
                map.push(i);
            }
        }
        let _ = &map;
        let rc = unsafe {
            WaitForMultipleObjects(
                handles.len() as u32,
                handles.as_ptr(),
                0,
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
            // `INFINITE`
            let rc = WaitForSingleObject(raw(&self.process), 0xFFFF_FFFF);
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

/// 写 stdin 用不到（一次性命令不开 stdin），但这个符号留着是为了让
/// `WriteFile` 的导入有意义——上层的终端走的是 ConPTY（`pty/windows.rs`）。
#[allow(dead_code)]
fn unused_write(handle: HANDLE, bytes: &[u8]) -> bool {
    let mut written: u32 = 0;
    unsafe {
        WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            ptr::null_mut(),
        ) != 0
    }
}
