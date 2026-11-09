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
//! | `poll(2)` | `PeekNamedPipe`（查可读字节数） |
//! | `kill(pid, SIGKILL)` | `TerminateProcess` |
//! | `waitpid` + `WEXITSTATUS` | `WaitForSingleObject` + `GetExitCodeProcess` |
//!
//! ## 一个必须说清的差别：匿名管道不是「可等待对象」
//!
//! Windows 的匿名管道没有 `O_NONBLOCK`，而且**管道句柄不能交给
//! `WaitForSingleObject` / `WaitForMultipleObjects`**（Windows 的可等待对象
//! 清单里没有匿名管道；传进去返回 `WAIT_FAILED`）。v0.7.2 曾据此判「可读」，
//! 结果是：本文件里 `WAIT_OBJECT_0` 永远等不到 → stdout/stderr 全空、6 条
//! exec 测试一起 FAILED；`pty/windows.rs` 里 `!= WAIT_TIMEOUT` 恒真 →
//! 无数据也去 `ReadFile`，把 `test-desktop` 挂了 53 分钟。
//!
//! 现在的时序是：**只等进程句柄**唤醒（`run()` 的 50ms 轮询切片也保证不会
//! 睡过头），再用 `PeekNamedPipe` 查每根管道有多少字节可读，有字节才
//! `ReadFile`。写端全关时 `ReadFile` 立刻返回 `ERROR_BROKEN_PIPE`（归到 EOF），
//! 所以「关掉了」这个信号照样不会挂住循环。
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
    CloseHandle, SetHandleInformation, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, HANDLE,
    HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    PROCESS_INFORMATION, STARTUPINFOW,
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

/// 零阻塞地判「这根管道现在能不能读」——`ReadFile` 之前的必要一步。
///
/// Windows 的匿名管道**没有非阻塞模式**：`ReadFile` 在没数据时会阻塞到有数据
/// 或写端关闭。而 Unix 侧读端带 `O_NONBLOCK`，`read` 立刻返回 `EAGAIN`。
/// 这个等价物只能是 `PeekNamedPipe`：管道句柄**不是**可等待对象，把它交给
/// `WaitForSingleObject` 会返回 `WAIT_FAILED`，于是「可读」永远判不出来
/// （v0.7.2 的 6 条 exec 测试就是这么一起红的）。
///
/// `PeekNamedPipe` 返回 FALSE 不等于「暂不可读」：匿名管道所有写端关闭时它
/// 也会失败（`ERROR_BROKEN_PIPE` / `ERROR_PIPE_NOT_CONNECTED`）。这两类必须
/// 判成可读，让 `read_into` 里的 `ReadFile` 拿到同一个错误码并摘掉读端；
/// 否则 `any_open()` 永远为真，`run()` 会一路轮询到超时。`timeout_ms` 参数
/// 保留是为了不改调用点签名（探测本身零阻塞，等待由 `run()` 负责）。
///
/// 判据本身收敛在 `crate::win_pipe`：pty 侧用同一份 `PeekNamedPipe` + 同一组
/// 断管错误码，避免一侧改了另一侧漂移回挂死。
fn wait_handle_readable(handle: HANDLE, _timeout_ms: u32) -> bool {
    crate::win_pipe::pipe_is_readable(handle)
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
        // `lpCommandLine` **是可写参数**：MSDN 原文「the function can modify the
        // contents of this string」，典型行为是就地改掉第一个 token 来生成
        // `argv[0]`。`prepared.command_line` 是共享的 `&Vec<u16>`，把它的
        // `as_ptr()` 转成 `*mut` 交出去，等于允许内核往只读内存里写——即使
        // 当前实现不写，那也是把一个未定义行为留给「哪天它开始写」。
        // 所以拷贝一份**可写**缓冲：多一次约 1 KiB 的拷贝，换掉这条 UB。
        let mut command_line_w = prepared.command_line.clone();
        // `lpEnvironment` 与 `lpCurrentDirectory` 只读，借用即可。
        let dir_w = prepared.cwd.as_deref();
        let env_block_w = &prepared.env;

        let ok = unsafe {
            CreateProcessW(
                ptr::null(), // 留给命令行解析：这样能按 PATH 找到程序
                command_line_w.as_mut_ptr(),
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
        // **先取错误码，再 drop 句柄**：`CreateProcessW` 之后我们还要做三次
        // `CloseHandle`（两个写端 + stdin 读端），而 `CloseHandle` 会改写
        // `LastError`。等三次 drop 完再读，拿到的就不是 `CreateProcessW` 的
        // 失败原因了——典型症状是「程序不存在」时报告 `(6) ERROR_INVALID_HANDLE`
        // 而不是 `(2) 系统找不到指定的文件`，用户完全看不懂。
        let create_err = if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        // 写端与 stdin 读端在我们这边必须立刻关掉：
        // - **写端**：管道读端「所有写端都关上」才 EOF。父进程若留一份，
        //   子进程退出后读端照样等不到 EOF，`run()` 会挂到超时。
        // - **stdin 读端**：它只被用了一次（填进 `si.hStdInput`），
        //   `CreateProcessW` 返回时子进程的句柄复制已经完成，留着只是白占
        //   一个句柄。EOF 是前面 `drop(in_w)` 造成的，与这里无关。
        drop(out_w);
        drop(err_w);
        drop(in_r);

        if let Some(err) = create_err {
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

    /// 等「有数据可读」或「进程已退出」，最多 `budget_ms` 毫秒。
    /// **返回能不能去 `read_into` 试一次**。
    ///
    /// 只等**进程句柄**：匿名管道句柄不是可等待对象，混进
    /// `WaitForMultipleObjects` 会让整次等待立刻以 `WAIT_FAILED` 收场
    /// （v0.7.2 挂死的另一半）。管道里有没有字节，交给 `read_into` 里的
    /// `PeekNamedPipe` 判——那是零阻塞探测，不需要在这里等。
    ///
    /// `WAIT_TIMEOUT` = 预算内没新事件，返回 false 让 `run()` 重新算 deadline；
    /// 进程退出（或等待失败）都返回 true，由 `read_into` 把管道里剩的字节
    /// 与 `ERROR_BROKEN_PIPE` 的 EOF 一起收干净。
    pub(super) fn wait_readable(&self, budget_ms: i32) -> bool {
        let rc = unsafe {
            WaitForSingleObject(
                self.process.as_raw_handle() as HANDLE,
                // `budget_ms` 来自 `exec.rs` 的 `remaining.as_millis() as i32`，
                // 恒非负；`0` 是不可达兜底（「立刻返回」而非「永远等」）。
                u32::try_from(budget_ms).unwrap_or(0),
            )
        };
        rc != WAIT_TIMEOUT
    }

    /// 从仍然开着的那些读端各读一次。**返回有没有真的读到字节**。
    ///
    /// 与 Unix 侧同约定：读到 EOF（`ERROR_BROKEN_PIPE` /
    /// `ERROR_PIPE_NOT_CONNECTED`）就把那根置 `None`，别的错误也摘掉——
    /// 留着它会让主循环每轮立刻失败又立刻重试（热循环）。
    pub(super) fn read_into(&mut self, out: &mut Collector) -> bool {
        let mut got_any = false;
        for stream in 0..self.pipes.len() {
            let Some(handle) = self.pipes.get(stream).and_then(Option::as_ref) else {
                continue;
            };
            // **先确认这根管道真的可读，再 `ReadFile`**。Windows 的匿名管道没有
            // 非阻塞模式：直接 `ReadFile` 会在没数据时**一直等下去**。Unix 侧的
            // 读端是 `O_NONBLOCK`，`read` 没数据立刻返回 `EAGAIN`——这一层
            // 等价物只能在这里补。
            //
            // 这不只是「性能」：`run()` 的收尾 drain 循环（`while child.any_open()
            // && child.read_into(…)`）在没有数据可读时会直接卡在这句 `ReadFile`
            // 上，而它外面那个 `DRAIN_GRACE` 判据**根本没机会执行**。症状是
            // 「超时之后面板永久转圈」，而 `run()` 占着 `afcli-exec` 线程。
            // `PeekNamedPipe` 零阻塞探到「现在有字节」才往下读；没字节就
            // 跳过，让上层去判 `any_open()` 与下一轮轮询。
            if !wait_handle_readable(raw(handle), 0) {
                continue;
            }
            let mut got: u32 = 0;
            let mut buf = vec![0u8; super::exec::READ_CHUNK];
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
            // 走到这里只有两种情形：`ReadFile` 失败（`ok == 0`），或成功但
            // 一个字节都没有（`got == 0`，对端已关）。**只有前者**的
            // `last_os_error()` 有意义——后者读它拿到的是上一轮残留的值，
            // 可能恰好等于 `BROKEN_PIPE` 而把一条该打的日志吞掉。所以判据
            // 先看 `ok`，再看 errno。
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                let eof = matches!(
                    err.raw_os_error(),
                    Some(code)
                        if code == ERROR_BROKEN_PIPE as i32
                            || code == ERROR_PIPE_NOT_CONNECTED as i32
                );
                if !eof {
                    // 字节模式的匿名管道上，这两个 EOF 码之外基本都是
                    // 「这根管子坏了」。**摘掉而不重试**：Unix 侧的
                    // `errno_is_retryable`（EAGAIN/EINTR 继续）在这里没有
                    // 对应物——`ReadFile` 在 `PeekNamedPipe` 说可读
                    // 之后不会返回 EAGAIN，而 EINTR 也不是 Win32 的概念。
                    // 差别如实写在 `docs/DEVELOPMENT.md` 的行为差异一节。
                    tracing::warn!(stream, error = %err, "pipe read failed; dropping it");
                }
            }
            // 成功但 0 字节 = 对端关了；两个断管错误码也是。都算 EOF。
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

// ---------------------------------------------------------------------------
// Windows 侧的命令执行用例：**只能在 Windows 上跑**
// ---------------------------------------------------------------------------
//
// `exec.rs` 的 `mod tests` 整块被 `#[cfg(all(test, unix))]` 挡着（它们全是
// `/bin/sh`、`exit 127`、`/bin/pwd` 这类 POSIX 断言），所以在 `test-desktop`
// 的 windows runner 上，这条后端**曾经只有编译验证**。这个模块补的就是它。
//
// 断言只用 `cmd.exe` 的内建命令（`echo`、`exit`），不依赖任何外部程序，
// 也不会因为 Windows 版本的输出编码差异而变红（`echo` 的 ASCII 输出在
// ConPTY 与匿名管道上都是原样的）。
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::connector::exec::{run, Outcome};
    use std::time::Duration;

    fn plan(command: &str, args: &[&str]) -> Plan {
        Plan {
            command: command.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: None,
            timeout: Duration::from_secs(30),
            max_output: 512 * 1024,
            env: Vec::new(),
        }
    }

    /// `run` 返回的 `Outcome` 里取 stdout。字段名与 `exec.rs` 同形。
    fn stdout_of(out: &Outcome) -> String {
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// 端到端：跑 `cmd /c echo MARKER`，stdout 必须拿到 MARKER，退出码 0。
    ///
    /// **这条是句柄继承模型的判据**。`bInheritHandles = FALSE` 时子进程拿不到
    /// 写端，父进程 `drop(out_w)` 之后读端立刻 EOF——stdout 会是空的。反过来
    /// 若读端没被 `SetHandleInformation` 改成不可继承，子进程握着读端就会让
    /// 「所有写端关闭」永远不成立，这条会挂到超时。两种错法都在这条上现形。
    #[test]
    fn a_command_produces_its_stdout() {
        let p = plan("cmd.exe", &["/c", "echo EXEC-MARKER"]);
        let prepared = prepare(&p).expect("prepare");
        let out = run(&p, &prepared).expect("run");
        assert_eq!(out.exit_code, 0, "cmd /c echo 必须正常退出");
        assert!(
            stdout_of(&out).contains("EXEC-MARKER"),
            "stdout 必须拿到子进程的输出（句柄继承错了？），实际：{:?}",
            stdout_of(&out)
        );
        assert!(!out.timed_out, "不该超时");
        assert!(!out.truncated, "这么点输出不该被截断");
    }

    /// stderr 与 stdout 走两根不同的管道，不会串。
    #[test]
    fn stderr_stays_on_its_own_pipe() {
        let p = plan("cmd.exe", &["/c", "echo OUT-ONE & echo ERR-ONE 1>&2"]);
        let prepared = prepare(&p).expect("prepare");
        let out = run(&p, &prepared).expect("run");
        let so = stdout_of(&out);
        let se = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(so.contains("OUT-ONE"), "stdout 里必须有 OUT-ONE：{so:?}");
        assert!(se.contains("ERR-ONE"), "stderr 里必须有 ERR-ONE：{se:?}");
        assert!(
            !so.contains("ERR-ONE"),
            "stderr 的内容不能跑到 stdout：{so:?}"
        );
    }

    /// 非零退出码原样穿过（`exit 7` → 7）。
    #[test]
    fn the_exit_code_comes_through() {
        let p = plan("cmd.exe", &["/c", "exit 7"]);
        let prepared = prepare(&p).expect("prepare");
        let out = run(&p, &prepared).expect("run");
        assert_eq!(out.exit_code, 7, "exit 7 必须报 7");
    }

    /// **超时必须报 `timed_out` 且退出码是 137**（`EXIT_KILLED`）。
    ///
    /// 这是跨平台约定：Unix 侧靠 `128 + SIGKILL`，Windows 侧靠
    /// `TerminateProcess(handle, EXIT_KILLED)` 自己给那个码。两平台读同一个
    /// 数这件事由 `exec.rs` 的 `EXIT_KILLED` 常量保证，这条用例是它的行为
    /// 判据——也是「超时路径真的把进程杀掉并回收了」的判据（若 `kill` 没
    /// 生效，`wait()` 会 `INFINITE` 挂住，这条测试会卡死而不是红）。
    #[test]
    fn a_timeout_kills_the_child_and_reports_137() {
        // `ping -n 30 127.0.0.1` 会真睡约 30 秒（`-n` 是次数，不是秒数，
        // 所以这里足够长），足够被 300 ms 的超时打断。
        let mut p = plan("cmd.exe", &["/c", "ping -n 30 127.0.0.1 >nul"]);
        p.timeout = Duration::from_millis(300);
        let prepared = prepare(&p).expect("prepare");
        let out = run(&p, &prepared).expect("run");
        assert!(out.timed_out, "必须报超时");
        assert_eq!(
            out.exit_code, EXIT_KILLED,
            "超时被杀必须报 {EXIT_KILLED}（与 Unix 的 128+9 对齐）"
        );
    }

    /// 程序不存在时**当场**报错，错误文案里带 `CreateProcessW` 的真实原因。
    ///
    /// 判据是错误码不是 `ERROR_INVALID_HANDLE`（6）：那说明我们在三次
    /// `CloseHandle` 之后才读 `last_os_error()`，读到的已经是别人的错误。
    /// 正确实现读到的是 `ERROR_FILE_NOT_FOUND`（2）。
    #[test]
    fn a_missing_program_reports_file_not_found_not_a_stale_error() {
        let p = plan("definitely-not-here-afcli-test.exe", &[]);
        let prepared = prepare(&p).expect("prepare should not fail on a missing program");
        let err = run(&p, &prepared).expect_err("must fail");
        let text = err.to_string();
        assert!(
            text.contains("CreateProcessW failed"),
            "错误必须说清是哪一步：{text}"
        );
        // 2 = ERROR_FILE_NOT_FOUND，6 = ERROR_INVALID_HANDLE（陈旧 errno 的症状）。
        assert!(
            text.contains("(2)") || text.contains("os error 2"),
            "必须是「找不到文件」而不是陈旧的句柄错误：{text}"
        );
    }

    /// **stdin 是空的**：子进程一读就拿到 EOF，不会把面板挂住。
    ///
    /// `hStdInput` 接的是一根「写端已在我们这边关掉」的管道，所以
    /// `more` / `set /p` 这类要读输入的命令会立刻返回而不是等下去。
    /// 若 `hStdInput` 继承了我们自己的控制台，这条会挂到超时。
    #[test]
    fn stdin_is_empty_so_reading_commands_do_not_hang() {
        let mut p = plan("cmd.exe", &["/c", "set /p X=& echo AFTER-EOF"]);
        p.timeout = Duration::from_secs(10);
        let prepared = prepare(&p).expect("prepare");
        let out = run(&p, &prepared).expect("run");
        assert!(!out.timed_out, "读 stdin 的命令不该挂住");
        assert!(
            stdout_of(&out).contains("AFTER-EOF"),
            "读完 EOF 之后必须继续跑：{:?}",
            stdout_of(&out)
        );
    }

    /// 参数里的空格与引号被正确带到子进程（引号规则的端到端判据）。
    #[test]
    fn arguments_with_spaces_survive_the_command_line() {
        let p = plan("cmd.exe", &["/c", "echo", "a b"]);
        let prepared = prepare(&p).expect("prepare");
        let out = run(&p, &prepared).expect("run");
        assert!(
            stdout_of(&out).contains("a b"),
            "带空格的参数必须原样到达：{:?}",
            stdout_of(&out)
        );
    }
}
