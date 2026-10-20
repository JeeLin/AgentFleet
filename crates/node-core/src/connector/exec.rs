//! exec connector：一次性命令、无 PTY（子任务 10）。
//!
//! ## 与 terminal 的区别就是「没有交互」
//!
//! 跑完就结束、通道随即关闭。所以这里**不挂读线程、不做流控、不碰信用**：
//! 输出是一次性的一帧（受协议 1 MiB 上限约束），采集是一次同步的
//! 请求-响应。复用 terminal 的 PTY 会有两个害处：PTY 会把输出折成
//! 带回显的行（`cat` 一个二进制文件会被改写），而一个跑不完的命令会在
//! 通道里永久挂着。
//!
//! ## 超时与输出上限都是**必须**的
//!
//! 没有超时，一条 `tail -f` 就能把通道永久占住；没有输出上限，同一条命令
//! 能把内存吃光。所以这两项都在参数里（[`protocol::message::ExecRequest`]
//! 的 `timeout_ms` / `max_output_bytes`），且本模块自己兜一层上限：
//! 调用方可以写 `0` 表示不限（那是它明确的选择），但**我们自己的内存上限
//! 不能由它决定**。
//!
//! ## 工作区边界
//!
//! **复用 [`crate::connector::files::resolve_within`]，不重复实现**（子任务 9）。
//! 两条路必须给出同一个答案：`exec` 的 `cwd` 与 `files` 的 `path` 是同一个
//! 边界检查的两种入口，两份实现必然漂移一次，而漂移的那次就是「终端能
//! 出去、文件出不去」。

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use base64ct::{Base64, Encoding};
use serde::Deserialize;

use super::files::resolve_within;
use super::{Connector, ConnectorError, OpenReq, OutFrame};

/// 本设备上的硬上限：**即使调用方写 `timeout_ms = 0`（不限），也只给这么多**。
///
/// 「不限」是调用方写下来的一个明确选择，但它不能决定我们的内存：一条
/// `yes` 在 10 秒里能产出几百 MiB，而它完全可以在 `timeout_ms = 0`
/// 之外拿到同样的结果。超时与输出上限**两个都要有默认值**，而默认值
/// 才是真正的兜底。
const DEFAULT_TIMEOUT_MS: u32 = 30_000;

/// 输出硬上限（stdout 与 stderr **合计**）。一帧受协议 1 MiB 约束
/// （`protocol.md` §2），而 base64 之后还要膨胀 4/3，所以字节上限必须
/// 留在 1 MiB 以下，否则会在序列化时才炸——那时输出已经全跑了。
const MAX_OUTPUT_BYTES: usize = 512 * 1024;

/// 单次读缓冲。管道是块设备，一次 `read` 可能只给几十字节。
const READ_CHUNK: usize = 64 * 1024;

/// 采集时用的 `poll` 粒度。它同时是「超时判定」的分辨率。
const POLL_SLICE_MS: libc::c_int = 50;

/// `exec` 的参数。**只有 `command` 必填**。
///
/// `timeout_ms` / `max_output_bytes` 缺省 = 本模块的默认值（[`DEFAULT_TIMEOUT_MS`]
/// / [`MAX_OUTPUT_BYTES`]），而不是 0：字段没填与写了 0 是两件不同的事，
/// 前者是「用你的默认」，后者是「我明确要不限」。
#[derive(Debug, Default, Deserialize)]
struct ExecParams {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    /// 相对工作区根。**绝对路径与 `..` 一样被拒**。
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    timeout_ms: Option<u32>,
    #[serde(default)]
    max_output_bytes: Option<u32>,
}

/// 校验过的参数。
struct Plan {
    command: String,
    args: Vec<String>,
    /// 真实文件系统上的绝对工作目录（界内）。
    cwd: Option<PathBuf>,
    timeout: std::time::Duration,
    max_output: usize,
}

impl ExecParams {
    /// 解成一份可执行的计划。**边界检查在这里做一次**，之后 `cwd`
    /// 是真实绝对路径，不再有相对路径可言。
    fn into_plan(self, root: &Path) -> Result<Plan, ConnectorError> {
        let command = self
            .command
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ConnectorError::Invalid {
                detail: "exec needs a non-empty \"command\"".to_string(),
            })?;
        // 空串参数在 `execvp` 里等价于「用 cwd 里的同名程序」，而那正是
        // 一个「以为自己在跑 A、实际跑了 ./A」的实现。
        if self.args.iter().any(|a| a.is_empty()) {
            return Err(ConnectorError::Invalid {
                detail: "exec arguments must not be empty".to_string(),
            });
        }
        let cwd = if self.cwd.is_empty() {
            None
        } else {
            Some(resolve_within(root, &self.cwd)?)
        };
        let timeout_ms = self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        Ok(Plan {
            command,
            args: self.args,
            cwd,
            timeout: std::time::Duration::from_millis(u64::from(timeout_ms)),
            max_output: self
                .max_output_bytes
                .map_or(MAX_OUTPUT_BYTES, |n| n as usize)
                .min(MAX_OUTPUT_BYTES),
        })
    }
}

/// Exec connector。**只跑一次**：[`Exec::run`] 的结果由 `open` 直接回出去，
/// 没有「再敲一帧跑一条」的形状——要再跑就再 `open_channel`（页面上那个
/// 「重新执行」按钮就是这么发的）。
#[derive(Debug)]
pub struct Exec {
    root: Option<PathBuf>,
    /// 已跑完过一次。第二次 `on_data` 说的是「这条通道是一次性的」，
    /// 而不是「跑第二条命令」。
    spent: bool,
}

impl Exec {
    pub fn new() -> Self {
        Self {
            root: None,
            spent: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.root.is_some()
    }

    /// 规范化后的工作区根（测试与排障用）。
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }
}

impl Connector for Exec {
    /// **`open` 就是执行**：一次性命令没有「先开再发」两步，所以命令
    /// 放在 `open_channel.params` 里而不是 `channel_data` 里。
    ///
    /// 这不违反协议里 `open` 必须 5s 内返回的约定（§5）：[`Plan::timeout`]
    /// 有硬上限，而 [`DEFAULT_TIMEOUT_MS`] 就是那个上限的默认值。
    fn open(&mut self, req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = req.root().ok_or_else(|| ConnectorError::WorkspaceRoot {
            detail: "no workspace root was given for this channel".to_string(),
        })?;
        let real = root
            .canonicalize()
            .map_err(|e| ConnectorError::WorkspaceRoot {
                detail: format!("{} is not usable: {e}", root.display()),
            })?;
        if !real.is_dir() {
            return Err(ConnectorError::WorkspaceRoot {
                detail: format!("{} is not a directory", root.display()),
            });
        }
        let params: ExecParams = req
            .parse("exec")
            .map_err(|e| ConnectorError::Invalid { detail: e })?;
        let plan = params.into_plan(&real)?;
        let result = run(&plan)?;
        self.close();
        self.root = Some(real);
        self.spent = true;
        Ok(vec![OutFrame::new(result_payload(&result))])
    }

    /// 一次性通道：**除了报「这是一次性的」，什么都不做**。
    ///
    /// 不接受第二帧不是保守——它是不给「`tail -f` 跑完还有下一条」留后门，
    /// 而每次执行都带一份自己的超时与输出上限，正是这条约束能立住的原因。
    fn on_data(&mut self, _data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        Err(if self.spent {
            ConnectorError::Invalid {
                detail: "exec runs once per channel; open a new channel to run again".to_string(),
            }
        } else {
            ConnectorError::Invalid {
                detail: "exec channel is not open".to_string(),
            }
        })
    }

    fn close(&mut self) {
        self.root = None;
        self.spent = false;
    }
}

/// 一次执行的产出。
struct Outcome {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    duration: std::time::Duration,
    timed_out: bool,
}

/// `exec_result` 载荷。**stdout / stderr 都是 base64**：`protocol.md` §4
/// 的同一条理由——JSON 只能装文本，而一条 `grep` 打到二进制文件上就会
/// 产出非法 UTF-8。
fn result_payload(o: &Outcome) -> serde_json::Value {
    serde_json::json!({
        "exit_code": o.exit_code,
        "stdout_b64": Base64::encode_string(&o.stdout),
        "stderr_b64": Base64::encode_string(&o.stderr),
        "truncated": o.truncated,
        "duration_ms": u64::try_from(o.duration.as_millis()).unwrap_or(u64::MAX),
        "timed_out": o.timed_out,
    })
}

/// 跑一条命令，采集它的 stdout / stderr 与退出码。
///
/// **不阻塞在任何一次读上**：管道用 `poll(2)` 等可读，粒度
/// [`POLL_SLICE_MS`]，所以「超时」这件事在一片管道数据里也照样被看见。
/// 直接 `read` 到 EOF 的写法在一条不产生输出的命令上会一路等到超时，
/// 而一条 `sleep 60` 的输出量是 0——那一路下超时会失效。
fn run(plan: &Plan) -> Result<Outcome, ConnectorError> {
    let started = std::time::Instant::now();
    let mut child = Child::spawn(plan)?;

    let mut collected = Collector::new(plan.max_output);
    let mut timed_out = false;
    let deadline = started + plan.timeout;

    loop {
        // deadline 每轮都重算：`timeout_ms = 0` 表示调用方明确要不限，
        // 而 `[Duration::ZERO]` 的 `Instant + Duration` 就是「现在」——
        // 那会把「不限」变成「立刻超时」。用 `Option` 才分得清这两种。
        let remaining = if plan.timeout.is_zero() {
            None
        } else {
            Some(deadline.saturating_duration_since(std::time::Instant::now()))
        };
        if let Some(rem) = remaining {
            if rem.is_zero() {
                timed_out = true;
                child.kill();
                break;
            }
        }
        let budget = remaining
            .map(|r| r.as_millis().min(i64::MAX as u128) as i32)
            .unwrap_or(POLL_SLICE_MS)
            .min(POLL_SLICE_MS);
        if !wait_readable(child.fds(), budget) {
            // 一片可读都没有：要么还在跑，要么已经结束而管道刚被填满。
            // 区分靠下一轮的「两个 fd 都关了」。
            continue;
        }
        let got = child.read_into(&mut collected);
        if !child.any_open() {
            // 两个写端都关上了：这就是 EOF。管道缓冲里的字节可能还在，
            // 所以顺序是「先读到没得读，再判 EOF」。
            let _ = got;
            break;
        }
    }
    // 收尾：把管道里剩下的都读干净（超时也读，因为剩下的部分正是用户要的）。
    while child.any_open() && child.read_into(&mut collected) {
        // 读到没得读为止
    }

    let exit_code = child.wait();
    Ok(Outcome {
        exit_code,
        stdout: std::mem::take(&mut collected.stdout),
        stderr: std::mem::take(&mut collected.stderr),
        truncated: collected.truncated,
        duration: started.elapsed(),
        timed_out,
    })
}

/// 输出采集。**上限是两条流合计**的，不分给 stdout / stderr 各一半——
/// 分一半会让「输出都在 stderr 上」的那条命令永远看不到自己的全部输出，
/// 而调用方给的 `max_output_bytes` 是一个**总量**预算。
struct Collector {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    max: usize,
    truncated: bool,
}

impl Collector {
    fn new(max: usize) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            max,
            truncated: false,
        }
    }

    fn push(&mut self, stream: usize, bytes: &[u8]) {
        let room = self
            .max
            .saturating_sub(self.stdout.len() + self.stderr.len());
        if bytes.len() > room {
            self.truncated = true;
        }
        // 只收下还收得下的那部分：**截断必须在采集处发生**，
        // 等到拼一帧再截的话内存已经爆了——而 `max_output_bytes`
        // 的全部意义就在这里。
        let take = bytes.len().min(room);
        let taken = if stream == 0 {
            &mut self.stdout
        } else {
            &mut self.stderr
        };
        if let Some(slice) = bytes.get(..take) {
            taken.extend_from_slice(slice);
        }
    }
}

/// 子进程 + 它的两根管道。
struct Child {
    pid: libc::pid_t,
    /// `[Some(stdout 读端), Some(stderr 读端)]`。**读到 EOF 就置 `None`**——
    /// `Option` 让「这根已经收完了」成为类型上的事实，而不是一个要靠
    /// 「fd 是不是 -1」在每处重新推断的约定。
    pipes: Vec<Option<OwnedFd>>,
}

impl Child {
    /// `pipe2` 两根管道 → `fork` → 子进程 `dup2` + `chdir` + `execvp`。
    ///
    /// **不建 PTY**：PTY 会把输出折成带回显的行，一个 `cat` 出来的二进制
    /// 文件会被改写，而一次性命令要的是**原样字节**。
    fn spawn(plan: &Plan) -> Result<Self, ConnectorError> {
        let mut out_pipe = [0 as RawFd; 2];
        let mut err_pipe = [0 as RawFd; 2];
        // **`O_NONBLOCK` 是必须的，不是优化**：两根管道依次读，而第一根
        // 有数据、第二根什么都没有时，一次阻塞 `read` 会永远等下去——
        // 那条命令明明早就跑完了。而 `poll` 已经先告诉过我们「至少有一根
        // 可读」，所以非阻塞读立刻返回 `EAGAIN` 就是正确回答。
        // `O_CLOEXEC` 让 fork 出来的子进程不会额外继承不属于它的写端。
        // coverage: excluded — `pipe2` 失败需 fd 表耗尽，测试环境无可控构造
        // 手段（防御性兜底）。
        for p in [&mut out_pipe, &mut err_pipe] {
            if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
                return Err(ConnectorError::Failed {
                    op: "run command".to_string(),
                    detail: format!("pipe2 failed: {}", std::io::Error::last_os_error()),
                });
            }
        }

        // fork 之前把所有字符串备成 C 字符串：fork 出来的子进程与父进程
        // 共享同一份堆，此刻任何分配都可能带着别的线程正持有的锁出去。
        let program = CString::new(plan.command.as_str()).map_err(|_| ConnectorError::Invalid {
            detail: "command name contains a NUL".to_string(),
        })?;
        let args_owned: Vec<CString> = plan
            .args
            .iter()
            .map(|a| {
                CString::new(a.as_str()).map_err(|_| ConnectorError::Invalid {
                    detail: "an argument contains a NUL".to_string(),
                })
            })
            .collect::<Result<_, _>>()?;
        let cwd_owned: Option<CString> = plan
            .cwd
            .as_ref()
            .map(|p| {
                p.to_str()
                    .ok_or_else(|| ConnectorError::Invalid {
                        detail: "cwd is not valid UTF-8".to_string(),
                    })
                    .and_then(|t| {
                        CString::new(t).map_err(|_| ConnectorError::Invalid {
                            detail: "cwd contains a NUL".to_string(),
                        })
                    })
            })
            .transpose()?;

        let mut argv: Vec<*const libc::c_char> = Vec::with_capacity(args_owned.len() + 2);
        argv.push(program.as_ptr());
        argv.extend(args_owned.iter().map(|a| a.as_ptr()));
        argv.push(std::ptr::null());

        // coverage: excluded — `fork` 失败需进程表耗尽，测试环境无可控构造
        // 手段（防御性兜底）。
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(ConnectorError::Failed {
                op: "run command".to_string(),
                detail: format!("fork failed: {}", std::io::Error::last_os_error()),
            });
        }

        if pid == 0 {
            // ---- 子进程 ----
            // 任何一步失败都只能 `_exit`：此刻子进程里跑的还是 Rust 的
            // 运行时（栈展开、析构、锁状态全是 fork 时刻的快照）。
            unsafe {
                if let Some(dir) = cwd_owned.as_ref() {
                    if libc::chdir(dir.as_ptr()) != 0 {
                        libc::_exit(126);
                    }
                }
                // stdin 关掉：**这不是终端**。留着继承来的 stdin 会让一条
                // `cat` 永远等输入，于是「一次性命令」变成一个挂住的通道。
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                if null < 0 || libc::dup2(null, 0) < 0 {
                    libc::_exit(126);
                }
                if libc::dup2(out_pipe[1], 1) < 0 || libc::dup2(err_pipe[1], 2) < 0 {
                    libc::_exit(126);
                }
                // 父进程那份写端必须关掉，否则 `read` 永远等不到 EOF。
                libc::close(out_pipe[1]);
                libc::close(err_pipe[1]);
                // **子进程这边的 1/2 必须是阻塞的**：`dup2` 会连同
                // `O_NONBLOCK` 一起复制过去，于是被执行的程序会拿到一根
                // 「没数据就立刻报错」的 stdout——`yes`、`jq`、任何自己
                // `write` 的工具全都会在 `EAGAIN` 上崩掉或丢行。
                let flags = libc::fcntl(1, libc::F_GETFL, 0);
                if flags < 0 || libc::fcntl(1, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                    libc::_exit(126);
                }
                let flags = libc::fcntl(2, libc::F_GETFL, 0);
                if flags < 0 || libc::fcntl(2, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                    libc::_exit(126);
                }
                libc::execvp(program.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }

        // ---- 父进程 ----
        // 关掉写端：读端的 EOF 取决于**所有**写端都关上了。
        unsafe {
            libc::close(out_pipe[1]);
            libc::close(err_pipe[1]);
        }
        // coverage: excluded — `from_raw_fd` 在 fd < 0 时即崩，而这两个 fd
        // 来自刚成功的 `pipe2`。
        let pipes = vec![
            Some(unsafe { OwnedFd::from_raw_fd(out_pipe[0]) }),
            Some(unsafe { OwnedFd::from_raw_fd(err_pipe[0]) }),
        ];
        Ok(Self { pid, pipes })
    }

    fn fds(&self) -> [RawFd; 2] {
        let mut out = [-1 as RawFd; 2];
        for (slot, fd) in out.iter_mut().zip(self.pipes.iter()) {
            if let Some(f) = fd {
                *slot = f.as_raw_fd();
            }
        }
        out
    }

    /// 还有一个读端开着吗。**关掉就是 EOF**。
    fn any_open(&self) -> bool {
        self.pipes.iter().any(Option::is_some)
    }

    /// 从仍然开着的那些读端各读一次。**返回有没有真的读到字节**。
    ///
    /// 读端会从 `pipes` 里被移除（`fd = -1`）——那才是 EOF 的记录方式，
    /// 用一个 `Vec<Option<OwnedFd>>` 会让每一处都要判 `Option`。
    fn read_into(&mut self, out: &mut Collector) -> bool {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut got_any = false;
        for stream in 0..self.pipes.len() {
            let Some(fd) = self.pipes.get(stream).and_then(Option::as_ref) else {
                continue;
            };
            let n = unsafe {
                libc::read(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n > 0 {
                let Some(chunk) = buf.get(..n as usize) else {
                    continue;
                };
                out.push(stream, chunk);
                got_any = true;
            } else if n == 0 {
                // 真正的 EOF：这根管道的写端全关上了。
                if let Some(slot) = self.pipes.get_mut(stream) {
                    *slot = None;
                }
            } else if n < 0 {
                // 只有「再来一轮有意义」的 errno 才继续。其余一律把这根管道
                // 当成坏掉摘掉：留着它，`poll` 每轮立刻报可读而 `read` 每轮
                // 立刻失败，主循环就变成一个烧 CPU 的热循环，命令只能等
                // 超时才结束（用户看到的是「超时」而不是「读失败」）。
                //
                // `EBADF` 尤其必须摘：fd 号可能已被别处的 `open` 复用，
                // 继续读它就读到别人那儿去了。
                if !errno_is_retryable() {
                    tracing::warn!(
                        stream,
                        errno = %std::io::Error::last_os_error(),
                        "pipe read failed; dropping it"
                    );
                    if let Some(slot) = self.pipes.get_mut(stream) {
                        *slot = None;
                    }
                }
            }
        }
        got_any
    }

    /// 超时后杀掉子进程。**先 `SIGKILL` 再等**：不杀的话一条 `tail -f`
    /// 会一直占着通道，而 `waitpid` 会跟着它一起等下去。
    ///
    /// **两条都发**：负号形式要求子进程自成进程组（`setpgid`），而我们
    /// 没设——所以真正生效的是第二条。留第一条是因为子进程万一自己
    /// `setpgid` 了（`sh -c 'exec ...'` 里某个工具会这么干），
    /// 只杀 pid 会留下它 fork 出来的孙进程继续往管道里写。
    fn kill(&self) {
        // coverage: excluded — kill 对已退出/已回收的 pid 返回 ESRCH，
        // 忽略即可（那正是我们想看到的）。
        let _ = unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        // coverage: excluded — 同上。
        let _ = unsafe { libc::kill(self.pid, libc::SIGKILL) };
    }

    /// 等子进程退出，取退出码。**-1 = 取不到**（waitpid 非 EINTR 失败）。
    ///
    /// **信号死的子进程不是一个负退出码给页面**：`tail -f` 超时被 `SIGKILL`
    /// 之后，内核报的是「被信号 9 终止」，而用户看到 `-9` 会以为是程序自己
    /// 返回了 -9。这里统一成 `128 + signal`（shell 的约定）。
    fn wait(self) -> i32 {
        let mut status: libc::c_int = 0;
        loop {
            // coverage: excluded — 非 EINTR 的 `waitpid` 失败要求这个 pid
            // 不是我们的子进程（已被别人回收）；本函数是唯一持有它的地方。
            // `EINTR` 则重试，那条路真的会被信号打断。
            let r = unsafe { libc::waitpid(self.pid, &raw mut status, 0) };
            if r >= 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return -1;
            }
        }
        if libc::WIFEXITED(status) {
            return libc::WEXITSTATUS(status);
        }
        // coverage: excluded — `WIFSTOPPED`/`WIFCONTINUED` 只有在带
        // `WUNTRACED`/`WCONTINUED` 的 waitpid 下才可能返回，而这里两个
        // 标志都没给，所以剩下的唯一分支就是「被信号杀掉」。
        128 + libc::WTERMSIG(status)
    }
}

/// 这次 `read` 失败值不值得再来一轮。
///
/// **契约**：返回 `true` 必须是「同一个 fd 上下一次 read 就可能成功」。
/// 除此之外一律让调用方把那根管道摘掉（理由见 `read_into` 里的分支）。
///
/// - `EAGAIN`：`poll` 说可读、但那一瞬被别人读走了（我们设了 `O_NONBLOCK`）。
/// - `EINTR`：被信号打断，数据没丢。
fn errno_is_retryable() -> bool {
    // **必须在别的操作覆盖 errno 之前读它**：中间插一句 IO 或日志就会读到
    // 别的值，于是把 `EBADF` 判成 `EAGAIN`——那个热循环正是本函数要挡的。
    let e = std::io::Error::last_os_error().raw_os_error();
    matches!(e, Some(libc::EAGAIN) | Some(libc::EINTR))
}

/// 等两根管道里**任何一根**可读，最多 `budget` 毫秒。
///
/// **POLLHUP / POLLERR 也要算「可读」**：管道写端关上的那一瞬间内核先报
/// 挂断，那正是 EOF 的信号——只认 `POLLIN` 的话读循环会永远跳过 `read`，
/// 于是永远看不到那个代表「读完了」的 0 字节。
fn wait_readable(fds: [RawFd; 2], budget_ms: i32) -> bool {
    let mut pollfds = [
        libc::pollfd {
            fd: fds[0],
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: fds[1],
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // `fds()` 里已关的那根传的就是 -1，内核会忽略它而 `revents` 恒 0——
    // 语义与「不在表里」相同，所以不需要在这里再筛一遍。
    // coverage: excluded — `poll` 失败需 nfds 超范围或非法 fd，这里传的都是
    // 内核刚给的 fd（防御性兜底）。
    let rc = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            budget_ms,
        )
    };
    rc > 0
        && pollfds
            .iter()
            .any(|p| p.fd >= 0 && p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
}

/// 「能用的默认值」必须真的存在：注册表按名造 connector 时走的就是
/// `Default`（`connector::create` → `Exec::new`）。
impl Default for Exec {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let base = tempfile::tempdir().expect("tempdir");
        std::fs::write(base.path().join("marker.txt"), b"in-workspace\n").expect("write");
        base
    }

    /// 一段产出 `rows * 10` 字节的 shell 脚本。**逐行 `printf` 而不是
    /// 一次大输出**：这样「到底产出了多少字节」是确定的，不会被工具自己
    /// 的缓冲掩盖。
    fn flood(rows: usize) -> String {
        format!("i=0; while [ $i -lt {rows} ]; do printf '0123456789'; i=$((i+1)); done")
    }

    fn run_plan(
        root: &Path,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ConnectorError> {
        let mut e = Exec::new();
        let frames = e.open(OpenReq::new(root.to_string_lossy().into_owned(), params))?;
        let mut frames = frames;
        let frame = frames
            .pop()
            .ok_or_else(|| ConnectorError::internal("no frame"))?;
        Ok(frame.payload)
    }

    /// 测试用：把 errno 设成指定值（`errno_is_retryable` 读的就是它）。
    fn set_errno(code: i32) {
        unsafe { *libc::__errno_location() = code };
    }

    /// errno 的分类：能重试的只有 `EAGAIN` / `EINTR`。
    ///
    /// **反向保护**：`read_into` 曾对所有 `n < 0` 一律 `continue`，注释却声称
    /// 判了 `EAGAIN`。于是 `EBADF`（fd 已关 / 被别人顶替）会让 `poll` 每轮
    /// 立刻报可读、`read` 每轮立刻失败——一个 CPU 打满且命令永不返回的热
    /// 循环。这条用例锁的就是「分类存在且边界正确」。
    ///
    /// **不去真的制造坏 fd**：那要动进程级 fd 表的生命周期，与其它用例并发
    /// 跑时会撞上 Rust 的 IO safety 检查（fd 号复用 → double close abort）。
    #[test]
    fn only_eagain_and_eintr_are_treated_as_retryable() {
        for expect_ok in [libc::EAGAIN, libc::EINTR] {
            set_errno(expect_ok);
            assert!(errno_is_retryable(), "errno {expect_ok} 必须判为可重试");
        }
        // 这几个是「同一个 fd 上再读也不会变好」的代表。
        for expect_drop in [libc::EBADF, libc::EINVAL, libc::EIO, libc::EPIPE] {
            set_errno(expect_drop);
            assert!(
                !errno_is_retryable(),
                "errno {expect_drop} 必须判为不可重试：留着它就是热循环"
            );
        }
    }

    #[test]
    fn stdout_the_exit_code_and_the_duration_all_come_back() {
        let ws = workspace();
        let out = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/bin/sh", "args": ["-c", "echo out-marker; echo err-marker >&2; exit 7"] }),
        )
        .expect("run");
        assert_eq!(out["exit_code"], 7, "退出码要原样回来：{out}");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        assert!(
            String::from_utf8_lossy(&stdout).contains("out-marker"),
            "{out}"
        );
        let stderr = Base64::decode_vec(out["stderr_b64"].as_str().expect("b64")).expect("b64");
        assert!(
            String::from_utf8_lossy(&stderr).contains("err-marker"),
            "{out}"
        );
        assert!(out["duration_ms"].as_u64().expect("duration") < 60_000);
        assert!(!out["truncated"].as_bool().expect("truncated"));
        assert!(!out["timed_out"].as_bool().expect("timed_out"));
    }

    /// **上限是两条流合计，不各给一半**：分一半会让「输出都在 stderr 上」
    /// 的那条命令永远看不到自己 stderr 的后半段，而调用方给的
    /// `max_output_bytes` 是一个**总量**预算，不是每条流各一份。
    #[test]
    fn the_output_budget_is_shared_between_the_two_streams() {
        let ws = workspace();
        let out = run_plan(
            ws.path(),
            serde_json::json!({
                "command": "/bin/sh",
                "args": ["-c", "printf 'xxxxxxxxxx'; printf 'yyyyyyyyyy' >&2"],
                "max_output_bytes": 10,
            }),
        )
        .expect("run");
        assert!(out["truncated"].as_bool().expect("truncated"), "{out}");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        let stderr = Base64::decode_vec(out["stderr_b64"].as_str().expect("b64")).expect("b64");
        assert_eq!(
            stdout.len() + stderr.len(),
            10,
            "合计必须是上限 10，而不是各 10（stdout={} stderr={}）",
            stdout.len(),
            stderr.len()
        );
    }

    /// 超时被杀掉**并且被说出来**：一条 `tail -f` 挂住通道是这一整节存在的理由。
    #[test]
    fn a_command_that_never_finishes_is_killed_at_the_deadline() {
        let ws = workspace();
        let started = std::time::Instant::now();
        let out = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/bin/sh", "args": ["-c", "sleep 30"], "timeout_ms": 300 }),
        )
        .expect("run");
        let elapsed = started.elapsed();
        assert!(out["timed_out"].as_bool().expect("timed_out"), "{out}");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "超时要真的把子进程杀掉，杀了才是 {elapsed:?}"
        );
        // `sleep` 被 SIGKILL → `128 + 9`。它**不是**程序自己返回的 -9。
        assert_eq!(out["exit_code"], 137, "信号死要给 128+signal：{out}");
    }

    #[test]
    fn output_beyond_the_limit_is_truncated_and_says_so() {
        let ws = workspace();
        let out = run_plan(
            ws.path(),
            serde_json::json!({
                "command": "/bin/sh",
                "args": ["-c", flood(2000)],
                "max_output_bytes": 1000,
            }),
        )
        .expect("run");
        assert!(out["truncated"].as_bool().expect("truncated"), "{out}");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        assert_eq!(
            stdout.len(),
            1000,
            "收到 {stdout_len} 字节，应该正好是上限",
            stdout_len = stdout.len()
        );
    }

    /// 缺省上限**不是不限**：一条 `yes` 在调用方什么都没填时也不能把内存吃光。
    #[test]
    fn the_default_output_limit_applies_when_the_caller_fills_nothing_in() {
        let ws = workspace();
        // 产出 1 MB > 默认的 512 KiB。**判据是「正好等于 MAX_OUTPUT_BYTES」**，
        // 而不是「被截断了」——后者在上限调大之后会假红。
        let out = run_plan(
            ws.path(),
            serde_json::json!({
                "command": "/bin/sh",
                "args": ["-c", flood(100_000)],
            }),
        )
        .expect("run");
        assert!(out["truncated"].as_bool().expect("truncated"), "{out}");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        assert_eq!(stdout.len(), MAX_OUTPUT_BYTES, "收到的字节数不是默认上限");
    }

    /// 二进制安全：一次 `cat` 出来的非法 UTF-8 必须**原样**回来。
    #[test]
    fn binary_output_survives_byte_for_byte() {
        let ws = workspace();
        std::fs::write(ws.path().join("blob.bin"), [0x00u8, 0xff, 0xfe, 0x7f]).expect("write");
        let out = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/bin/cat", "args": ["blob.bin"], "cwd": "." }),
        )
        .expect("run");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        assert_eq!(stdout, vec![0x00, 0xff, 0xfe, 0x7f]);
    }

    /// **stdout 与 stderr 是分开的**：合流之后用户分不清哪条是哪条。
    #[test]
    fn the_two_streams_stay_apart() {
        let ws = workspace();
        let out = run_plan(
            ws.path(),
            serde_json::json!({
                "command": "/bin/sh",
                "args": ["-c", "printf A; printf B >&2"],
            }),
        )
        .expect("run");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        let stderr = Base64::decode_vec(out["stderr_b64"].as_str().expect("b64")).expect("b64");
        assert_eq!(stdout, b"A");
        assert_eq!(stderr, b"B");
    }

    #[test]
    fn cwd_takes_effect_and_is_relative_to_the_workspace_root() {
        let ws = workspace();
        std::fs::create_dir(ws.path().join("sub")).expect("mkdir");
        let out = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/bin/pwd", "cwd": "sub" }),
        )
        .expect("run");
        let stdout = Base64::decode_vec(out["stdout_b64"].as_str().expect("b64")).expect("b64");
        let got = String::from_utf8_lossy(&stdout).trim().to_string();
        assert!(
            got.ends_with("/sub"),
            "cwd 必须是**真实文件系统上**的那个目录：{got}"
        );
    }

    #[test]
    fn the_workspace_boundary_applies_to_cwd_too() {
        let ws = workspace();
        for asked in ["..", "/etc", "../../etc"] {
            let err = run_plan(
                ws.path(),
                serde_json::json!({ "command": "/bin/pwd", "cwd": asked }),
            )
            .expect_err("must refuse");
            assert!(
                matches!(err, ConnectorError::OutsideWorkspace { .. }),
                "cwd={asked} 越界却没被拒: {err}"
            );
        }
    }

    #[test]
    fn a_command_that_is_not_there_reports_a_start_failure_not_a_hang() {
        let ws = workspace();
        let out = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/nonexistent/command" }),
        )
        .expect("run");
        // `execvp` 失败 → 子进程 `_exit(127)`，与 shell 的约定一致。
        assert_eq!(out["exit_code"], 127, "{out}");
    }

    #[test]
    fn an_empty_command_is_refused_before_anything_is_spawned() {
        let ws = workspace();
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "command": "   " }),
        ] {
            let err = run_plan(ws.path(), bad).expect_err("must refuse");
            assert!(err.to_string().contains("command"), "got {err}");
        }
    }

    #[test]
    fn an_empty_argument_is_refused_rather_than_becoming_a_cwd_relative_name() {
        let ws = workspace();
        let err = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/bin/ls", "args": [""] }),
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("must not be empty"), "got {err}");
    }

    #[test]
    fn a_command_name_carrying_a_nul_is_refused_before_forking() {
        let ws = workspace();
        let mut name = String::from("/bin/sh");
        name.push('\0');
        name.push('x');
        let err =
            run_plan(ws.path(), serde_json::json!({ "command": name })).expect_err("must refuse");
        assert!(err.to_string().contains("NUL"), "got {err}");
    }

    #[test]
    fn a_cwd_that_is_not_there_leaves_the_exit_code_to_say_it() {
        let ws = workspace();
        let out = run_plan(
            ws.path(),
            serde_json::json!({ "command": "/bin/sh", "args": ["-c", "pwd"], "cwd": "nope" }),
        )
        .expect("run");
        // `chdir` 失败 → 子进程 `_exit(126)`。
        assert_eq!(out["exit_code"], 126, "{out}");
    }

    /// 一次性通道跑完**不接受第二帧**：要再跑就再开通道。
    #[test]
    fn a_spent_channel_says_so_instead_of_running_a_second_command() {
        let ws = workspace();
        let mut e = Exec::new();
        e.open(OpenReq::new(
            ws.path().to_string_lossy().into_owned(),
            serde_json::json!({ "command": "/bin/echo", "args": ["once"] }),
        ))
        .expect("open");
        let err = e.on_data(&serde_json::json!({})).expect_err("must refuse");
        assert!(err.to_string().contains("once per channel"), "got {err}");
    }

    #[test]
    fn data_on_a_never_opened_channel_says_the_channel_is_gone() {
        let mut e = Exec::new();
        let err = e.on_data(&serde_json::json!({})).expect_err("must refuse");
        assert!(err.to_string().contains("not open"), "got {err}");
    }

    #[test]
    fn a_missing_workspace_root_reads_as_a_sentence_not_as_enoent() {
        let mut e = Exec::new();
        let err = e.open(OpenReq::default()).expect_err("no root at all");
        assert!(
            matches!(err, ConnectorError::WorkspaceRoot { .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("no workspace root"), "got {err}");
    }

    #[test]
    fn a_root_that_is_a_file_is_refused() {
        let ws = workspace();
        let file = ws.path().join("marker.txt");
        let mut e = Exec::new();
        let err = e
            .open(OpenReq::new(
                file.to_string_lossy().into_owned(),
                serde_json::json!({ "command": "/bin/echo" }),
            ))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not a directory"), "got {err}");
    }

    #[test]
    fn a_root_that_is_not_there_is_reported_as_a_sentence() {
        let ws = workspace();
        let mut e = Exec::new();
        let err = e
            .open(OpenReq::new(
                ws.path().join("deleted").to_string_lossy().into_owned(),
                serde_json::json!({ "command": "/bin/echo" }),
            ))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not usable"), "got {err}");
    }

    #[test]
    fn bad_params_name_the_connector_they_belong_to() {
        let ws = workspace();
        let err =
            run_plan(ws.path(), serde_json::json!({ "command": 42 })).expect_err("must refuse");
        assert!(err.to_string().contains("exec"), "got {err}");
    }

    #[test]
    fn the_registry_routes_the_exec_name_here() {
        let ws = workspace();
        let mut c = crate::connector::create(crate::connector::names::EXEC).expect("registered");
        c.open(OpenReq::new(
            ws.path().to_string_lossy().into_owned(),
            serde_json::json!({ "command": "/bin/true" }),
        ))
        .expect("open through the registry");
    }

    #[test]
    fn reopening_moves_to_the_new_root_and_closing_twice_is_allowed() {
        let first = workspace();
        let second = tempfile::tempdir().expect("tempdir");
        let mut e = Exec::new();
        e.open(OpenReq::new(
            first.path().to_string_lossy().into_owned(),
            serde_json::json!({ "command": "/bin/true" }),
        ))
        .expect("open");
        assert!(e.is_open());
        e.open(OpenReq::new(
            second.path().to_string_lossy().into_owned(),
            serde_json::json!({ "command": "/bin/true" }),
        ))
        .expect("reopen");
        assert_eq!(
            e.root(),
            Some(second.path().canonicalize().expect("canonical").as_path())
        );
        e.close();
        e.close();
        assert!(!e.is_open());
    }

    /// 一根永久失败的管道必须被摘掉，而不是被无限重试。
    ///
    /// `EBADF` 下 `poll` 每轮立刻报可读、`read` 每轮立刻失败：若 `n < 0`
    /// 一律重来，主循环就变成热循环，命令只能等 `timeout_ms` 到点才结束
    /// ——那正是修复前的行为。这里用 `read_into` 而不是整条 `run`：
    /// 判据是「这根管道被摘掉了」，不是「命令返回了」。
    #[test]
    fn a_pipe_that_fails_permanently_is_dropped_not_retried() {
        // 一个**只写**的 fd：`read` 必给 `EBADF`（永久失败，不是「再来一轮
        // 就有」），而它又是真的打开着的，所以 `OwnedFd` 关它不会触发
        // IO-safety 中止——用未打开的 fd 号会让测试以 SIGABRT 结束。
        let path = std::ffi::CString::new("/dev/null").expect("cstr");
        let wfd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        assert!(wfd >= 0, "open /dev/null");
        let mut child = Child {
            pid: 0,
            pipes: vec![None, Some(unsafe { OwnedFd::from_raw_fd(wfd) })],
        };
        let mut out = Collector::new(MAX_OUTPUT_BYTES);
        assert!(!child.read_into(&mut out));
        assert!(
            child.pipes.get(1).and_then(Option::as_ref).is_none(),
            "永久失败的 errno 必须摘掉这根管道"
        );
    }

    /// 反过来：`EAGAIN` 那种「再来一轮就有」的失败不许摘管道。
    #[test]
    fn a_retryable_read_error_keeps_the_pipe() {
        let mut fds = [-1 as RawFd; 2];
        // coverage: excluded — `pipe2` 失败需 fd 表耗尽。
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        // **写端必须活着交出去**：留在本函数里被 `OwnedFd` 关掉的话，
        // 读端立刻 EOF（`n == 0`），量的就不是 `EAGAIN` 那条路了。
        let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let mut child = Child {
            pid: 0,
            pipes: vec![Some(unsafe { OwnedFd::from_raw_fd(fds[0]) }), None],
        };
        let mut out = Collector::new(MAX_OUTPUT_BYTES);
        assert!(!child.read_into(&mut out));
        assert!(
            child.pipes.first().and_then(Option::as_ref).is_some(),
            "`EAGAIN` 不得摘管道：写端还开着，输出还没来"
        );
        drop(writer);
    }
}
