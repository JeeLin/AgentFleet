//! git connector：**只读**的工作区 git 视图（状态 / diff / 历史提交）。
//!
//! 三个 op 各回答一个不同的问题（`v0.4.0` 设计核对第 11 条）：
//!
//! | 请求 | 回答的问题 |
//! |------|-----------|
//! | `{"op":"status"}` | 工作区有哪些改动、在哪个分支 |
//! | `{"op":"diff","path":…}` / `{"staged":true}` / `{"commit":…}` | 某个改动 / 暂存区 / 某个历史提交改了什么 |
//! | `{"op":"log","limit":N}` | 最近这些提交是什么 |
//!
//! **没有任何写操作，且这不是「暂未实现」**：commit / stage / checkout 要连带决定
//! 提交者身份从哪来、钩子要不要跑、失败怎么回滚，三件都不是「看它改了什么」需要的。
//!
//! # 为什么子进程用线程而不是 `tokio::process`
//!
//! [`Connector`] 的方法**全是同步的**（见 `connector/mod.rs` 的 trait 定义），
//! 而 `open` / `on_data` 是在 worker 的 `select!` 里被调的——在里面等 IO 会把
//! 心跳那条臂一起堵住，`v0.2.0` 的 exec 事故就是这么来的（症状是设备在 Hub 眼里
//! 掉线，本地毫无异常）。既有解法是 `exec`：起线程跑，结果经通道由 `poll` 交出。
//! 本 connector 照此办理，并额外加超时。
//!
//! 不用 `tokio::process` 的理由与 `agent/detect.rs` 里那句一样：它会让一条
//! 同步路径要求一个 runtime 句柄，把 connector 绑死在 tokio 上。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use super::files::resolve_within;
use super::{Connector, ConnectorError, OpenReq, OutFrame};

/// 一次 git 调用的超时。
///
/// 大仓库的 `diff` / `log` 可能跑几秒，所以给得比 exec 的默认 30s 短但在人的
/// 耐心之内；超时**必须杀掉子进程**，否则一个挂死的 git 会一直占着进程槽
/// （同 `agent/detect.rs` 对探测的处理）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// 一次响应载荷的上限。
///
/// 取协议帧上限的保守分数：整帧还要装 JSON 信封（`op`、`path`、字段名和转义），
/// 直接贴着 1 MiB 去截会在序列化后被推过去。超限**在设备侧截断并声明**
/// 比发出去让 Hub 拒帧好——拒帧会断开整条设备连接，那是故障；截断是可读的。
const PAYLOAD_BUDGET: usize = protocol::MAX_FRAME_BYTES * 3 / 4;

/// `log` 缺省取多少条（调用方没给 `limit` 时）。
const DEFAULT_LOG_LIMIT: u32 = 30;

/// `log` 的硬上限：再大也只是把帧撑到截断，不如一开始就压住。
const MAX_LOG_LIMIT: u32 = 500;

/// git connector。绑会话工作区（约束 3）。
pub struct Git {
    /// 规范化后的工作区根。边界判据要它，所以要 canonicalize 过的版本。
    root: Option<PathBuf>,
    /// `git` 可执行文件。**构造期可注入**只为可测性：超时那条测试要换上
    /// 一个挂住的假 git。生产路径用缺省值，等价于直接跑 `git`。
    ///
    /// 不走「改 `PATH`」注入：`std::env::set_var` 动的是整个进程的环境，
    /// 并行用例会互相污染（`pty.rs` 为此专门加了 `env_lock`）。构造期注入
    /// 不需要任何全局状态。
    git_bin: PathBuf,
    timeout: Duration,
    /// 进行中的请求。同一时刻只有一条——通道是串行的，页面也不会并发问两次。
    pending: Option<Receiver<Result<Vec<OutFrame>, ConnectorError>>>,
}

impl Git {
    pub fn new() -> Self {
        Self::with_git_bin(PathBuf::from("git"))
    }

    /// 换一个 git 可执行文件。**仅测试用**：生产走 [`Git::new`]。
    pub fn with_git_bin(git_bin: impl Into<PathBuf>) -> Self {
        Self {
            root: None,
            git_bin: git_bin.into(),
            timeout: DEFAULT_TIMEOUT,
            pending: None,
        }
    }

    /// 换超时。**仅测试用**：生产用 [`DEFAULT_TIMEOUT`]。
    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn root_checked(&self) -> Result<&Path, ConnectorError> {
        self.root.as_deref().ok_or_else(|| ConnectorError::Invalid {
            detail: "git channel is not open".to_string(),
        })
    }

    /// 跑一次 git，拿 stdout。**同步**——调用方负责放到线程里去。
    fn run_git(&self, root: &Path, args: &[&str]) -> Result<Vec<u8>, ConnectorError> {
        let op = args.first().copied().unwrap_or("git").to_string();
        let mut child = Command::new(&self.git_bin)
            .args(args)
            // `cwd` 是工作区根：git 靠它定位仓库，不另传 `-C`
            // （`-C` 的路径要拼进参数，而那正是能拼错的地方）。
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ConnectorError::Failed {
                op: op.clone(),
                detail: format!("cannot run git: {e}"),
            })?;

        let out = match wait_with_deadline(&mut child, self.timeout) {
            Ok(Some(out)) => out,
            Ok(None) => {
                // 超时：先杀再报。杀完还得收尸，否则留下僵尸进程。
                let _ = child.kill();
                let _ = child.wait();
                return Err(ConnectorError::Failed {
                    op,
                    detail: format!("git timed out after {}s", self.timeout.as_secs()),
                });
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ConnectorError::Failed {
                    op,
                    detail: format!("cannot read git output: {e}"),
                });
            }
        };

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ConnectorError::Failed {
                op,
                detail: classify_failure(&stderr),
            });
        }
        Ok(out.stdout)
    }

    // ------------------------------------------------------------------ ops

    fn status(&self) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = self.root_checked()?;
        // `--porcelain=v1 -z`：`-z` 用 NUL 分隔且**不转义路径**，
        // 否则一个含空格或中文的文件名会被引号包起来，页面上显示成带引号的怪东西。
        let raw = self.run_git(root, &["status", "--porcelain=v1", "-z", "--branch"])?;
        let StatusView {
            branch,
            head,
            detached,
            files,
        } = parse_status(&raw);

        // HEAD 用 `rev-parse` 单独取：`status --branch` 只在跟踪分支时给
        // `## main...origin/main`，detached 时给 `## HEAD (no branch)`，
        // 拿不到短 hash。多一次子进程换一个确定的字段，值得。
        let head = match head {
            Some(h) => Some(h),
            None => self.head_short(root).unwrap_or(None),
        };

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "status",
            "branch": branch,
            "head": head,
            "detached": detached,
            "clean": files.is_empty(),
            "files": files,
        }))])
    }

    fn head_short(&self, root: &Path) -> Result<Option<String>, ConnectorError> {
        match self.run_git(root, &["rev-parse", "--short", "HEAD"]) {
            Ok(raw) => Ok(Some(String::from_utf8_lossy(&raw).trim().to_string())),
            // 空仓库没有 HEAD：那不是错误，是「还没有提交」。
            Err(_) => Ok(None),
        }
    }

    fn diff(&self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = self.root_checked()?;
        let args = diff_args(data)?;

        // 越界判据与 files 同一套（`resolve_within`），不重写：
        // 三种逃逸（`..` / 绝对路径 / 链接出界）在那里已经各自被拒。
        let shown = data
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(shown) = &shown {
            resolve_within(root, shown)?;
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let raw = self.run_git(root, &arg_refs)?;
        let (unified, truncated) = truncate_utf8(&raw, PAYLOAD_BUDGET);

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "diff",
            "path": shown,
            "staged": data.get("staged").and_then(serde_json::Value::as_bool).unwrap_or(false),
            "commit": data.get("commit").and_then(serde_json::Value::as_str),
            "unified": unified,
            "truncated": truncated,
        }))])
    }

    fn log(&self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = self.root_checked()?;
        let limit = data
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v.min(u64::from(MAX_LOG_LIMIT)) as u32)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_LOG_LIMIT);

        // 用不可见分隔符而不是换行/制表：提交消息首行里可能有任何可打印字符，
        // 而 `%x00` / `%x1f` 不会出现在正常的提交元数据里。
        let fmt = "--pretty=format:%H%x00%an%x00%aI%x00%s%x1e";
        // **空仓库不是错误**：`git log` 在还没有提交的仓库上以非零退出，
        // 而「还没有提交」是一种正常状态（和「不是 git 仓库」不同）。
        // 把它当失败会让页面在新建仓库上弹一个红框。
        let raw = match self.run_git(root, &["log", fmt, "-n", &limit.to_string()]) {
            Ok(raw) => raw,
            Err(e) if is_empty_repo(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&raw);
        let commits = parse_log(&text);

        // 检查是否还有更多提交：`git log` 拿不到总数，再看一条即可。
        let truncated = commits.len() as u32 >= limit && {
            let more = limit.saturating_add(1).to_string();
            self.run_git(root, &["rev-list", "--count", "HEAD"])
                .map(|c| {
                    String::from_utf8_lossy(&c)
                        .trim()
                        .parse::<u32>()
                        .map(|total| total > limit)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
                && !more.is_empty()
        };

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "log",
            "commits": commits,
            "truncated": truncated,
        }))])
    }
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for Git {
    fn open(&mut self, req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = req.root().ok_or_else(|| ConnectorError::WorkspaceRoot {
            detail: "no workspace root was given for this channel".to_string(),
        })?;
        // 与 files 同一条：根不可用要给一句人话，裸 ENOENT 会让人去查 inode，
        // 而真相通常是工作区记录指向一个已经删掉的目录。
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
        self.close();
        self.root = Some(real);
        Ok(Vec::new())
    }

    fn on_data(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        // 先确认开着：`data` 解不开时错误会指向「帧格式」，而真实原因常常是
        // 这条通道早关了——后者才是用户要看的那句（同 files）。
        self.root_checked()?;
        let op = data
            .get("op")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ConnectorError::Invalid {
                detail: "git data must carry a string \"op\"".to_string(),
            })?
            .to_string();

        // 参数校验放在**起线程之前**：非法请求要当场变成 `channel_error`，
        // 进了线程就只剩 `poll` 的 `Err`，而通道层对那一路的处理与「这次请求
        // 被拒」在页面上长得不一样。
        match op.as_str() {
            "status" | "log" | "diff" => {}
            other => {
                return Err(ConnectorError::Invalid {
                    detail: format!("unknown git operation: {other}"),
                })
            }
        }
        if op == "diff" {
            diff_args(data)?;
        }

        // 副本进线程：`Git` 本身不能跨线程搬（它持着通道），而这次 git 调用
        // 只需要根、可执行文件路径与超时这三样。
        let root = self
            .root
            .clone()
            .ok_or_else(|| ConnectorError::Invalid {
                detail: "git channel is not open".to_string(),
            })?;
        let git_bin = self.git_bin.clone();
        let timeout = self.timeout;
        let payload = data.clone();

        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("afcli-git".to_string())
            .spawn(move || {
                // 线程里重建一个临时 `Git`：它的方法都是 `&self`，不碰通道状态。
                let worker = Git {
                    root: Some(root),
                    git_bin,
                    timeout,
                    pending: None,
                };
                let result = match payload
                    .get("op")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                {
                    "status" => worker.status(),
                    "diff" => worker.diff(&payload),
                    "log" => worker.log(&payload),
                    other => Err(ConnectorError::Invalid {
                        detail: format!("unknown git operation: {other}"),
                    }),
                };
                // 没人接就说明通道已释放（页面走了）：那不是错误。
                let _ = tx.send(result);
            })
            .map_err(|e| ConnectorError::internal(format!("cannot start the git runner: {e}")))?;

        self.pending = Some(rx);
        // 结果由下一轮 `poll` 交出：那时 `channel_ready` 早已发过，
        // 页面不会把结果帧当成「通道还没就绪」。
        Ok(Vec::new())
    }

    fn poll(&mut self) -> Result<Vec<OutFrame>, ConnectorError> {
        let Some(rx) = self.pending.as_ref() else {
            return Ok(Vec::new());
        };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                result
            }
            // 还在跑。空队列不是结束——大仓库的 diff 要几秒。
            Err(TryRecvError::Empty) => Ok(Vec::new()),
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                Err(ConnectorError::internal(
                    "the git command could not be run (see the device log)",
                ))
            }
        }
    }

    fn close(&mut self) {
        self.root = None;
        self.pending = None;
    }
}

// -------------------------------------------------------------------- 纯函数

/// 等到子进程退出或超时。
///
/// `Ok(None)` = 超时。轮询而不是阻塞 `wait()`：标准库没有带超时的 wait，
/// 而 `wait-timeout` 不在 workspace 依赖里——为一件已有现成写法的事加依赖
/// 不划算。这套写法 `agent/detect.rs` 已经跑在生产路径上。
fn wait_with_deadline(
    child: &mut Child,
    timeout: Duration,
) -> std::io::Result<Option<Captured>> {
    let deadline = Instant::now() + timeout;
    // **两个管道各交给一个读取线程**，主线程只负责超时与 wait。
    //
    // 不能「等进程退出再读」：管道缓冲（Linux 上 64 KiB）写满后子进程就阻塞在
    // write 上，永远退不出，而我们在等它退出——双向死锁。大 diff 轻易超过
    // 64 KiB，所以这不是理论问题：本模块的截断测试第一版就撞上了，
    // 症状是「git timed out after 15s」而 git 本身没毛病。
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_reader = stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    });
    let err_reader = stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    });

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        // 10ms 粒度：git 命令多在几十毫秒内结束，再细是忙等，
        // 再粗会让短命令白白多等一个粒度。
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout = out_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = err_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    Ok(Some(Captured { status, stdout, stderr }))
}

/// 一次子进程的产出。
struct Captured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// 把 git 的 stderr 变成一句可读的 detail。
///
/// 「不是 git 仓库」是**正常状态**，不是故障：页面对它画中性说明而不是红框，
/// 所以这里给一个**固定前缀** `not a git repository` 让它能被单独识别——
/// 页面不得靠中文猜。
fn classify_failure(stderr: &str) -> String {
    let lowered = stderr.to_lowercase();
    if lowered.contains("not a git repository") {
        return "not a git repository".to_string();
    }
    // 其余情况原样带出第一行非空：git 自己的错误信息通常已经是人话
    // （「bad revision」「unknown revision」），截断成一句比整段 stderr 可读。
    let first = stderr.lines().map(str::trim).find(|l| !l.is_empty());
    match first {
        Some(line) => line.to_string(),
        None => "git failed without a message".to_string(),
    }
}

/// 这个失败是不是「仓库里还没有提交」。
///
/// git 对无提交仓库的 `log` 以非零退出并说 `does not have any commits yet`
/// ——那是**一种正常状态**，不是故障。与「不是 git 仓库」一样，
/// 页面要把它画成中性说明而不是红框。
fn is_empty_repo(e: &ConnectorError) -> bool {
    match e {
        ConnectorError::Failed { detail, .. } => {
            let d = detail.to_lowercase();
            d.contains("does not have any commits yet")
                || d.contains("does not have any commits")
                || d.contains("bad default revision")
        }
        _ => false,
    }
}

/// `diff` 的参数。**三种取法互斥**，且都校验形状后才进参数数组。
fn diff_args(data: &serde_json::Value) -> Result<Vec<String>, ConnectorError> {
    let staged = data
        .get("staged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let path = data.get("path").and_then(serde_json::Value::as_str);
    let commit = data.get("commit").and_then(serde_json::Value::as_str);

    if commit.is_some() && (path.is_some() || staged) {
        return Err(ConnectorError::Invalid {
            detail: "\"commit\" cannot be combined with \"path\" or \"staged\"".to_string(),
        });
    }

    if let Some(commit) = commit {
        // 形状校验：只允许 4-40 位十六进制（含 `HEAD~1` 这类写法太宽，
        // 会把「用户以为能填任意 rev」变成事实）。校验不是防注入——子进程
        // 本就不走 shell——而是让非法输入变成一句可读的 `Invalid`，
        // 而不是一段 git 自己的 usage 文本。
        let ok = (4..=40).contains(&commit.len()) && commit.chars().all(|c| c.is_ascii_hexdigit());
        if !ok {
            return Err(ConnectorError::Invalid {
                detail: format!("commit must be a hex object name, got {commit:?}"),
            });
        }
        // `git show --format=` 抑制 commit 头只留 diff。
        // **不能用 `git diff <hash>`**：那比较的是 hash 与**工作区**，
        // 回答的是另一个问题。
        return Ok(vec![
            "show".to_string(),
            "--format=".to_string(),
            "--patch".to_string(),
            commit.to_string(),
        ]);
    }

    let mut args: Vec<String> = vec!["diff".to_string(), "--no-color".to_string()];
    if staged {
        args.push("--cached".to_string());
    }
    if let Some(path) = path {
        args.push("--".to_string());
        args.push(path.to_string());
    }
    Ok(args)
}

/// `status` 的解析结果。
///
/// 用具名结构体而不是四元组：返回四个位置相关的值，调用处得靠顺序记
/// 「第二个是 head 还是 detached」，而这两个都是 `Option`/`bool` 混排，
/// 写反了编译器不一定拦得住。
struct StatusView {
    branch: Option<String>,
    /// `status --branch` 在部分情况下给不出短 hash，此时为 `None`，
    /// 由调用方补一次 `rev-parse`。
    head: Option<String>,
    detached: bool,
    files: Vec<serde_json::Value>,
}

/// 解析 `status --porcelain=v1 -z --branch`。
fn parse_status(raw: &[u8]) -> StatusView {
    let text = String::from_utf8_lossy(raw);
    let mut branch = None;
    let mut detached = false;
    let mut files = Vec::new();

    let mut parts = text.split('\0').filter(|s| !s.is_empty()).peekable();
    while let Some(entry) = parts.next() {
        if let Some(head) = entry.strip_prefix("## ") {
            // 三种形状：`main`、`main...origin/main`、`HEAD (no branch)`。
            if head.starts_with("HEAD (no branch)") || head.starts_with("HEAD (detached") {
                detached = true;
            } else {
                let name = head.split("...").next().unwrap_or(head);
                let name = name.trim();
                if !name.is_empty() {
                    branch = Some(name.to_string());
                }
            }
            continue;
        }
        // 前两字节是 XY 状态码，第三字节是空格，其余是路径。
        // 用 `get` 而不是 `split_at`：位置切片越界是 panic，而「这条记录太短」
        // 只是脏数据，跳过就行。
        let xy = match entry.get(..2) {
            Some(xy) => xy,
            None => continue,
        };
        let rest = entry.get(2..).unwrap_or_default();
        let path = rest.strip_prefix(' ').unwrap_or(rest);
        let x = xy.chars().next().unwrap_or(' ');
        let y = xy.chars().nth(1).unwrap_or(' ');
        let mut path = path.to_string();

        // 重命名 / 复制：`-z` 下**旧路径是下一条记录**（不是同一行里用 `->` 分隔，
        // 那种形状只在非 `-z` 输出里出现）。
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            if let Some(orig) = parts.next() {
                path = format!("{path} <- {orig}");
            }
        }
        if path.is_empty() {
            continue;
        }
        files.push(serde_json::json!({
            "x": x.to_string(),
            "y": y.to_string(),
            "path": path,
        }));
    }

    StatusView {
        branch,
        head: None,
        detached,
        files,
    }
}

/// 解析 `log --pretty=format:%H%x00%an%x00%aI%x00%s%x1e` 的输出。
fn parse_log(text: &str) -> Vec<serde_json::Value> {
    text.split('\u{1e}')
        .filter(|rec| !rec.trim().is_empty())
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            let mut fields = rec.split('\0');
            let hash = fields.next()?.trim();
            let author = fields.next()?.trim();
            let when = fields.next()?.trim();
            let subject = fields.next().unwrap_or("").trim();
            if hash.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "hash": hash,
                "author": author,
                "when": when,
                "subject": subject,
            }))
        })
        .collect()
}

/// 按字节预算截断，**在字符边界上切**（否则中文字符会被劈成半个，JSON 里变乱码）。
fn truncate_utf8(raw: &[u8], budget: usize) -> (String, bool) {
    // 用 `get` 而不是 `&raw[..budget]`：越界切片在 Rust 里是 panic，
    // 而这里 `budget` 短于 `raw.len()` 只是常态，不是编程错误。
    let Some(head) = raw.get(..budget) else {
        return (String::from_utf8_lossy(raw).into_owned(), false);
    };
    // 退到最近的字符边界。`valid_up_to()` 直接给出「合法前缀有多长」：
    // 它落在 `head` 之内说明断在了一个多字节字符中间，按它切就是整字。
    let boundary = match std::str::from_utf8(head) {
        Ok(valid) => valid.len(),
        // `valid_up_to()` 为 0 意味着第一个字节就不是合法 UTF-8——
        // 退无可退，宁可一个字都不留，也不产出替换字符。
        Err(e) => e.valid_up_to(),
    };
    let mut text = String::from_utf8_lossy(raw.get(..boundary).unwrap_or_default()).into_owned();
    text.push_str("\n… (diff truncated by the device: output exceeded the frame budget)");
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 一个临时工作区 + 一个能开 git 通道的 connector。
    fn sandbox() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::tempdir().expect("tempdir");
        let ws = base.path().join("ws");
        std::fs::create_dir_all(&ws).expect("mkdir");
        (base, ws)
    }

    fn open(ws: &Path) -> Git {
        let mut g = Git::new();
        Connector::open(&mut g, OpenReq::new(ws.to_string_lossy(), serde_json::json!({})))
            .expect("open");
        g
    }

    /// 跑一次 op 并取出结果帧的 payload。
    ///
    /// `on_data` 只起线程、结果由 `poll` 交出——所以这里必须**等**：
    /// 直接断言第一次 `poll` 会拿到空数组，那是设计而不是失败。
    fn call(g: &mut Git, data: serde_json::Value) -> Result<serde_json::Value, ConnectorError> {
        Connector::on_data(g, &data)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let frames = Connector::poll(g)?;
            if let Some(f) = frames.first() {
                return Ok(f.payload.clone());
            }
            assert!(Instant::now() < deadline, "git op did not finish in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 造一个有一次提交的仓库。
    fn repo_with_commit(ws: &Path) {
        git_in(ws, &["init", "-q", "-b", "main", "."]);
        git_in(ws, &["config", "user.email", "t@example.com"]);
        git_in(ws, &["config", "user.name", "Tester"]);
        std::fs::write(ws.join("a.txt"), "one\n").expect("write");
        git_in(ws, &["add", "a.txt"]);
        git_in(ws, &["commit", "-q", "-m", "first commit"]);
    }

    // ---------------------------------------------------------------- 1 status

    /// 测试标准 1：临时仓库上给出正确的 branch 与文件状态码。
    #[test]
    fn status_reports_the_branch_and_the_file_codes() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        // 一个已跟踪文件改掉（工作区侧 ` M`），再加一个未跟踪（`??`）。
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").expect("write");
        std::fs::write(ws.join("new.txt"), "x\n").expect("write");

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "status"})).expect("status");

        assert_eq!(got["branch"], "main", "分支名要读出来: {got}");
        assert_eq!(got["detached"], false);
        assert_eq!(got["clean"], false, "有改动时 clean 必须为假");

        let files = got["files"].as_array().expect("files 是数组");
        let a = files
            .iter()
            .find(|f| f["path"] == "a.txt")
            .unwrap_or_else(|| panic!("a.txt 要在列表里: {got}"));
        assert_eq!(a["x"], " ", "未暂存的修改：index 侧是空格");
        assert_eq!(a["y"], "M", "未暂存的修改：worktree 侧是 M");

        let new = files
            .iter()
            .find(|f| f["path"] == "new.txt")
            .unwrap_or_else(|| panic!("new.txt 要在列表里: {got}"));
        assert_eq!(new["x"], "?");
        assert_eq!(new["y"], "?");

        assert!(
            got["head"].as_str().is_some_and(|h| !h.is_empty()),
            "有提交时必须给出短 HEAD: {got}"
        );
    }

    /// 干净仓库：`clean:true` 且 files 为空（页面据此显示「没有未提交的改动」，
    /// 而不是一个空列表）。
    #[test]
    fn a_clean_repo_reports_clean_with_no_files() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "status"})).expect("status");

        assert_eq!(got["clean"], true, "{got}");
        assert_eq!(got["files"].as_array().map(Vec::len), Some(0), "{got}");
    }

    /// 测试标准 2：非 git 目录回 `Failed`，且 detail 以固定前缀开头。
    ///
    /// 前缀是页面唯一可依赖的识别手段——「不是仓库」要画成中性说明而不是红框，
    /// 页面不得靠中文猜。
    #[test]
    fn status_outside_a_repo_fails_with_the_fixed_prefix() {
        let (_base, ws) = sandbox();
        let mut g = open(&ws);
        let err = call(&mut g, serde_json::json!({"op": "status"})).expect_err("要报错");

        match err {
            ConnectorError::Failed { op, detail } => {
                assert_eq!(op, "status");
                assert!(
                    detail.starts_with("not a git repository"),
                    "detail 必须以固定前缀开头，实际 {detail:?}"
                );
            }
            other => panic!("越界/失败要落到 Failed，实际 {other:?}"),
        }
    }

    // ---------------------------------------------------------------- 2 diff

    /// 测试标准 3：越界 path 回 `OutsideWorkspace`，且 `shown` 不含根路径。
    #[test]
    fn diff_rejects_a_path_outside_the_workspace() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);

        let err = call(
            &mut g,
            serde_json::json!({"op": "diff", "path": "../../etc/passwd"}),
        )
        .expect_err("越界必须被拒");

        match err {
            ConnectorError::OutsideWorkspace { shown } => {
                assert_eq!(shown, "../../etc/passwd", "shown 是用户自己写的那个写法");
                assert!(
                    !shown.contains(ws.to_string_lossy().as_ref()),
                    "不得泄漏工作区根的绝对路径"
                );
            }
            other => panic!("越界要落到 OutsideWorkspace，实际 {other:?}"),
        }
    }

    /// 未提交改动的 diff：`path` 取法。
    #[test]
    fn diff_of_a_modified_file_shows_the_change() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").expect("write");

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "diff", "path": "a.txt"})).expect("diff");

        let unified = got["unified"].as_str().expect("unified 是字符串");
        assert!(unified.contains("+two"), "要含新增的那行: {unified}");
        assert_eq!(got["truncated"], false);
        assert_eq!(got["path"], "a.txt");
    }

    /// 测试标准 7：`commit` 取法**只含该提交的改动**，不掺别的提交。
    ///
    /// 这条锁住「用 `git show` 而不是 `git diff <hash>`」：后者比的是 hash 与
    /// **工作区**，回答的是另一个问题。写错了这里会红。
    #[test]
    fn diff_of_a_commit_shows_only_that_commit() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let first = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&ws)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("utf8");
        let first = first.trim().to_string();

        // 第二次提交：改 a.txt，再加 b.txt。
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").expect("write");
        std::fs::write(ws.join("b.txt"), "bee\n").expect("write");
        git_in(&ws, &["add", "."]);
        git_in(&ws, &["commit", "-q", "-m", "second commit"]);

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "diff", "commit": first}))
            .expect("diff by commit");
        let unified = got["unified"].as_str().expect("unified");

        assert!(
            unified.contains("a.txt") && unified.contains("+one"),
            "第一次提交新增了 a.txt（+one），必须在结果里: {unified}"
        );
        assert!(
            !unified.contains("b.txt"),
            "b.txt 是第二次提交加的，不该出现在第一次提交的 diff 里: {unified}"
        );
        assert!(
            !unified.contains("+two"),
            "`+two` 是第二次提交的改动；出现说明取的是「hash 对工作区」而不是该提交本身: {unified}"
        );
    }

    /// 暂存区的 diff：`staged` 取法。
    #[test]
    fn diff_staged_shows_the_index() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        std::fs::write(ws.join("a.txt"), "one\nstaged\n").expect("write");
        git_in(&ws, &["add", "a.txt"]);
        // 暂存之后又改一次：工作区里那部分**不该**出现在 `staged` 的 diff 里。
        std::fs::write(ws.join("a.txt"), "one\nstaged\nunstaged\n").expect("write");

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "diff", "staged": true})).expect("diff");

        let unified = got["unified"].as_str().expect("unified");
        assert!(unified.contains("+staged"), "{unified}");
        assert!(
            !unified.contains("+unstaged"),
            "`staged:true` 是暂存区，不能把工作区里后来的改动也带进来: {unified}"
        );
    }

    /// 测试标准 8：非法 hash 回 `Invalid`，且**不起子进程**。
    #[test]
    fn diff_rejects_a_malformed_commit() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);

        for bad in ["HEAD; rm -rf /", "zzzz", "", "abc", "HEAD~1"] {
            let err = Connector::on_data(&mut g, &serde_json::json!({"op": "diff", "commit": bad}))
                .expect_err(&format!("{bad:?} 必须在起子进程之前被拒"));
            assert!(
                matches!(err, ConnectorError::Invalid { .. }),
                "{bad:?} 要落到 Invalid，实际 {err:?}"
            );
        }
    }

    /// `commit` 与 `path` / `staged` 互斥：拼在一起是调用方的错，不是「都要」。
    #[test]
    fn diff_refuses_combining_commit_with_path_or_staged() {
        let data = serde_json::json!({"op": "diff", "commit": "abc1234", "path": "a.txt"});
        assert!(matches!(diff_args(&data), Err(ConnectorError::Invalid { .. })));

        let data = serde_json::json!({"op": "diff", "commit": "abc1234", "staged": true});
        assert!(matches!(diff_args(&data), Err(ConnectorError::Invalid { .. })));
    }

    // ----------------------------------------------------------------- 3 log

    /// 测试标准 4：`limit` 生效，且 `truncated` 如实。
    #[test]
    fn log_respects_the_limit_and_reports_truncation() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        for i in 2..=4 {
            std::fs::write(ws.join("a.txt"), format!("line {i}\n")).expect("write");
            git_in(&ws, &["add", "a.txt"]);
            git_in(&ws, &["commit", "-q", "-m", &format!("commit {i}")]);
        }

        let mut g = open(&ws);
        let two = call(&mut g, serde_json::json!({"op": "log", "limit": 2})).expect("log");
        let commits = two["commits"].as_array().expect("commits");
        assert_eq!(commits.len(), 2, "limit=2 只能给 2 条: {two}");
        assert_eq!(two["truncated"], true, "还有更早的提交，必须声明被截断");
        assert_eq!(commits[0]["subject"], "commit 4", "最新的在最前: {two}");
        assert!(
            commits[0]["hash"].as_str().is_some_and(|h| h.len() >= 7),
            "要给出可用于取 diff 的 hash: {two}"
        );

        // limit 覆盖全部提交时不声明截断。
        let all = call(&mut g, serde_json::json!({"op": "log", "limit": 10})).expect("log");
        assert_eq!(all["commits"].as_array().map(Vec::len), Some(4), "{all}");
        assert_eq!(all["truncated"], false, "全给了就不该说截断: {all}");
    }

    /// 空仓库没有提交：`log` 给空列表而不是报错（和「不是仓库」区分开）。
    #[test]
    fn log_of_a_repo_without_commits_is_empty_not_failed() {
        let (_base, ws) = sandbox();
        git_in(&ws, &["init", "-q", "-b", "main", "."]);

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "log", "limit": 10})).expect("log");
        assert_eq!(got["commits"].as_array().map(Vec::len), Some(0), "{got}");
        assert_eq!(got["truncated"], false);
    }

    // ---------------------------------------------------------------- 超时

    /// 测试标准 5：挂住的假 git 必须**在超时附近**返回，而不是挂死。
    ///
    /// 形状照 `agent/detect.rs` 的 `a_hanging_agent_binary_cannot_block_probing_past_the_timeout`：
    /// 超时没生效时这条测试会挂住整个进程而不是失败，所以断言里必须带**余量上限**，
    /// 真挂住时至少能靠测试超时看出问题。
    #[test]
    fn a_hanging_git_cannot_block_past_the_timeout() {
        let (_base, ws) = sandbox();
        let bin = _base.path().join("fake-git");
        std::fs::write(&bin, "#!/bin/sh\nsleep 3600\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let timeout = Duration::from_millis(300);
        let mut g = Git::with_git_bin(&bin).with_timeout(timeout);
        Connector::open(&mut g, OpenReq::new(ws.to_string_lossy(), serde_json::json!({})))
            .expect("open");

        let start = Instant::now();
        let err = call(&mut g, serde_json::json!({"op": "status"})).expect_err("超时必须报错");
        let elapsed = start.elapsed();

        match err {
            ConnectorError::Failed { detail, .. } => assert!(
                detail.starts_with("git timed out after"),
                "detail 要说明超时，实际 {detail:?}"
            ),
            other => panic!("超时要落到 Failed，实际 {other:?}"),
        }
        assert!(
            elapsed < timeout + Duration::from_secs(2),
            "必须在超时附近返回，实际 {elapsed:?}"
        );
    }

    // ------------------------------------------------------------ 其它边界

    /// 测试标准 6：未知 op 回 `Invalid`，不是 panic。
    #[test]
    fn unknown_operations_are_invalid_not_fatal() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);

        for op in ["commit", "push", "STATUS", ""] {
            let err = Connector::on_data(&mut g, &serde_json::json!({"op": op}))
                .expect_err(&format!("{op:?} 必须被拒"));
            assert!(
                matches!(err, ConnectorError::Invalid { .. }),
                "{op:?} 要落到 Invalid，实际 {err:?}"
            );
        }
    }

    /// 通道没开时任何 op 都回 `Invalid`（而不是「解帧失败」）。
    #[test]
    fn ops_on_a_closed_channel_are_invalid() {
        let mut g = Git::new();
        let err = Connector::on_data(&mut g, &serde_json::json!({"op": "status"}))
            .expect_err("没开就要拒");
        assert!(matches!(err, ConnectorError::Invalid { .. }), "{err:?}");
    }

    /// 工作区根缺失 → `WorkspaceRoot`；不是目录 → 同样。
    #[test]
    fn open_requires_a_usable_workspace_root() {
        let mut g = Git::new();
        let err = Connector::open(&mut g, OpenReq::new("", serde_json::json!({})))
            .expect_err("缺根要拒");
        assert!(matches!(err, ConnectorError::WorkspaceRoot { .. }), "{err:?}");

        let (_base, ws) = sandbox();
        let file = ws.join("f.txt");
        std::fs::write(&file, "x").expect("write");
        let mut g = Git::new();
        let err = Connector::open(&mut g, OpenReq::new(file.to_string_lossy(), serde_json::json!({})))
            .expect_err("文件不是目录");
        assert!(matches!(err, ConnectorError::WorkspaceRoot { .. }), "{err:?}");
    }

    /// 超长 diff 在设备侧截断并声明——不能指望协议层兜住。
    #[test]
    fn a_huge_diff_is_truncated_on_the_device_and_says_so() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        // 造一个远超预算的改动。
        let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(ws.join("a.txt"), big).expect("write");

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "diff", "path": "a.txt"})).expect("diff");

        assert_eq!(got["truncated"], true, "超预算必须声明截断: {got}");
        let unified = got["unified"].as_str().expect("unified");
        assert!(unified.contains("truncated"), "要留下截断说明: {}", &unified[..80.min(unified.len())]);
        // 截断后的整帧仍要在协议上限内，否则 Hub 会拒帧并断开设备连接。
        let frame = serde_json::to_vec(&got).expect("serialize");
        assert!(
            frame.len() <= protocol::MAX_FRAME_BYTES,
            "截断后仍超帧上限: {} > {}",
            frame.len(),
            protocol::MAX_FRAME_BYTES
        );
    }

    /// 截断必须落在字符边界：中文被劈成半个会在 JSON 里变乱码。
    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let raw: Vec<u8> = "中文改动".repeat(100).into_bytes();
        // 7 字节落在第三个字（「改」）的中间：正确实现必须退到 6 字节（两个整字）。
        assert!(
            std::str::from_utf8(&raw[..7]).is_err(),
            "前提：7 字节确实切在字符中间，否则这条测的是另一条臂"
        );
        let (text, truncated) = truncate_utf8(&raw, 7);
        assert!(truncated);
        assert!(
            text.starts_with("中文\n"),
            "必须退到字符边界（≤7 字节的完整字是「中文」）: {text:?}"
        );
        assert!(
            !text.contains('\u{FFFD}'),
            "截断不得产出替换字符（半个中文字的典型症状）: {text:?}"
        );
    }

    /// 重命名：`-z` 下旧路径是**下一条**记录，要拼成 `新 <- 旧`。
    ///
    /// 不认这条会让重命名显示成「新文件」+ 一条孤立的旧路径（后者被当成
    /// 一个路径为空或怪异的条目，取决于解析怎么写），用户在页面上看不出
    /// 「这是同一个文件改名了」。
    #[test]
    fn status_pairs_the_old_path_of_a_rename() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        std::fs::rename(ws.join("a.txt"), ws.join("b.txt")).expect("rename");
        git_in(&ws, &["add", "-A"]);

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "status"})).expect("status");
        let files = got["files"].as_array().expect("files");

        assert_eq!(files.len(), 1, "一次重命名是一条记录，不是两条: {got}");
        let f = &files[0];
        assert_eq!(f["x"], "R", "index 侧是 R（已 add）: {got}");
        assert_eq!(f["path"], "b.txt <- a.txt", "要保住新旧两个路径: {got}");
    }

    /// `git status` 里含空格的文件名不该被引号包起来（`-z` 的作用）。
    #[test]
    fn status_keeps_filenames_with_spaces_unquoted() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        std::fs::write(ws.join("a b.txt"), "x\n").expect("write");

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "status"})).expect("status");
        let files = got["files"].as_array().expect("files");
        assert!(
            files.iter().any(|f| f["path"] == "a b.txt"),
            "含空格的文件名要原样给出，不带引号: {got}"
        );
    }
}
