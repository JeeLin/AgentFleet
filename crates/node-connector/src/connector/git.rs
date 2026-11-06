//! git connector：**只读**的工作区 git 视图（状态 / diff / 历史提交 / 当前 rev）。
//!
//! 各 op 各回答一个不同的问题（`v0.4.0` 设计核对第 11 条）：
//!
//! | 请求 | 回答的问题 |
//! |------|-----------|
//! | `{"op":"status"}` | 工作区有哪些改动、在哪个分支 |
//! | `{"op":"diff","path":…}` / `{"staged":true}` / `{"commit":…}` | 某个改动 / 暂存区 / 某个历史提交改了什么 |
//! | `{"op":"log","limit":N}` | 最近这些提交是什么 |
//! | `{"op":"rev"}` | 现在在哪个提交上（完整 hash / 短 hash / 分支 / 是否 detached） |
//!
//! 头三个 op 是 `v0.4.0` 就有的；第四个 `rev` 在 `v0.6.0` 加入，**仍然只读**——
//! 它只是把 `rev-parse` 的结果报出来，一个字节都不写。
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
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
///
/// **它刻意不等于 `files.rs` 的 `PAYLOAD_BUDGET`**（那边是 `MAX_FRAME_BYTES
/// - 1024`）：diff 是纯文本，序列化后不膨胀，只需给信封留余量；而 files 的
/// 载荷是 base64（4/3 膨胀）加一份等长 `text`，**不能靠固定余量**——真正的
/// 判据是编码后的大小，装不下退回头尾预览。实测把这里换成 `-1024` 会让
/// `a_huge_diff_is_truncated_on_the_device_and_says_so` 红（截断后 1135928
/// > 1048576，仍超帧），因为那 25% 是 cover 信封与转义的，不是可选的。
const PAYLOAD_BUDGET: usize = protocol::frame::MAX_FRAME_BYTES * 3 / 4;

/// 「通道没开」的文案。
///
/// `on_data` 里两处构造它（开头一次、起线程前一次），各写一份字面量时改文案
/// 要改两处，而改一处不会有测试变红——那条用例只 `contains("not open")`，
/// 两份都成立。
const NOT_OPEN: &str = "git channel is not open";

/// `git` 通道认的 op。
///
/// **白名单与分派表从这里同一张表派生**（`on_data` 校验用 `parse`，线程里
/// 分派也用 `parse`）：早先它们是两处手写字符串——`match op { "status" | "log"
/// | "diff" => {} }` 与 `match op { "status" => …, "diff" => … }`——加第四个 op
/// 时只改一处，另一处会静默落进 `other` 臂，编译不报错、测试也不会红。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitOp {
    Status,
    Diff,
    Log,
    Rev,
}

impl GitOp {
    /// 认不出来就回 `Invalid`（通道层能翻成 `channel_error` 的那一档）。
    fn parse(op: &str) -> Result<Self, ConnectorError> {
        match op {
            "status" => Ok(Self::Status),
            "diff" => Ok(Self::Diff),
            "log" => Ok(Self::Log),
            "rev" => Ok(Self::Rev),
            other => Err(ConnectorError::Invalid {
                detail: format!("unknown git operation: {other}"),
            }),
        }
    }
}

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
    /// 进行中的请求。**约束**是「同一时刻只保留一条」：新请求在旧的还没取走时
    /// 会被 `on_data` 拒（返回 `Invalid`），不会排队也不会顶掉旧的。
    ///
    /// 不要把它写成「页面不会并发问两次」——那是一句**代码没提供的保证**：
    /// 页面连点两个文件就是两次请求，而 `on_data` 返回后 `poll` 才取结果，
    /// 中间完全可能来第二帧。把希望写成事实，正是当初没加守卫的原因。
    pending: Option<Receiver<Result<Vec<OutFrame>, ConnectorError>>>,
    /// 在飞请求的取消旗。`close()` 置位，之后工作线程会在下一个 10ms 轮询点
    /// 杀掉 git 子进程并退出。
    ///
    /// **为什么需要它**：`close()` 只是丢弃 `Receiver`，线程与 git 子进程都还在
    /// 跑——通道被回收（用户关掉面板）后仍会有一个最长 15 秒的 `git` 进程。
    /// 「新请求顶掉旧请求」那条路径已被 `pending` 守卫挡掉，但**回收**这条没有。
    cancel: Arc<AtomicBool>,
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
            cancel: Arc::new(AtomicBool::new(false)),
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
            detail: NOT_OPEN.to_string(),
        })
    }

    /// 跑一次 git，拿 stdout。**同步**——调用方负责放到线程里去。
    fn run_git(&self, root: &Path, args: &[&str]) -> Result<Vec<u8>, ConnectorError> {
        let op = args.first().copied().unwrap_or("git").to_string();
        let mut cmd = Command::new(&self.git_bin);
        cmd.args(args)
            // `cwd` 是工作区根：git 靠它定位仓库，不另传 `-C`
            // （`-C` 的路径要拼进参数，而那正是能拼错的地方）。
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // **必须清掉继承来的 `GIT_*`**：`afcli` 常被从「某个 git 钩子 / IDE 的
        // git 集成 / `git rebase --exec`」里拉起，那些环境里 `GIT_DIR` /
        // `GIT_WORK_TREE` / `GIT_INDEX_FILE` 都指着**另一个**仓库。不清的话
        // `current_dir(root)` 会被 `GIT_DIR` 压过去，connector 于是报出那个
        // 仓库的分支与改动——用户在这台机器上看到的「工作区」根本不是这个会话
        // 的工作区。（实测：`GIT_DIR` 指向别处时不清会读出那个仓库的状态。）
        //
        // 只删 `GIT_*` 而不是 `env_clear()`：清空全部会把 `PATH` / `HOME` 一起
        // 带走，而 git 需要它们（`HOME` 决定 `~/.gitconfig` 与 `core.excludesfile`）。
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&key);
            }
        }
        let mut child = cmd
            .spawn()
            // coverage: excluded (racy) — 只在 `git` 可执行文件缺失 / 无执行权限 /
            // 进程表满时才会走到。`with_git_bin` 能指向一个不存在的路径来构造
            // 这一支，但那验的是「我们自己写的 map_err 有没有语法错」，不是
            // 「git 起不来时产品怎么办」——真实世界里这一支的替代路径是
            // `Err` 被通道层翻成 `channel_error{internal}`，那条已有测试。
            //
            // 标 `(racy)` 而不是普通例外，是因为**这几行是否被记为 miss 跨运行
            // 不稳定**：实测同一份代码两次全量跑，一次报 192-194 三行、一次一行
            // 都不报。原因是那几行落在 `.spawn()` 与 `.map_err(...)` 的 `?` 展开里，
            // llvm-cov 的区域归属随内联与优化单元的组成而变。按确定性行做集合
            // 相等核对会让门禁随机红。
            .map_err(|e| ConnectorError::Failed {
                op: op.clone(),
                detail: format!("cannot run git: {e}"),
            })?;

        let out = match wait_with_deadline(&mut child, self.timeout, &self.cancel) {
            Ok(Some(out)) => out,
            Ok(None) => {
                // 超时或已取消：先杀再报。杀完还得收尸，否则留下僵尸进程。
                let _ = child.kill();
                let _ = child.wait();
                if self.cancel.load(Ordering::SeqCst) {
                    return Err(ConnectorError::Failed {
                        op,
                        detail: "git request cancelled: the channel was closed".to_string(),
                    });
                }
                return Err(ConnectorError::Failed {
                    op,
                    detail: format!("git timed out after {}s", self.timeout.as_secs()),
                });
            }
            // coverage: excluded — `wait_with_deadline` 只在 `try_wait` 本身
            // 出错时回 `Err`，而对一个刚 spawn 出来的子进程 `try_wait` 失败
            // 需要内核层面出问题（EPERM / EINTR 未被重试）。构造它要能在
            // 测试里让某个特定的 children 句柄失效，而 std 没有这个口子；
            // 这一支的价值是「别把错误吞成空输出」，逻辑本身无可测的分叉。
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
        //
        // **「不是 git 仓库」在这里被接住，不是错误**：它是这个工作区的一种
        // 正常状态，页面对它画的是一句中性说明而不是红框。把它交给错误路径
        // 的后果不只是「红框」——通道层只往页面传错误码，detail 里的那句
        // `not a git repository` 到不了页面，于是页面既画不出中性说明，也
        // 分不清「不是仓库」和「git 真的挂了」。所以正常回帧、字段声明事实。
        let raw = match self.run_git(root, &["status", "--porcelain=v1", "-z", "--branch"]) {
            Ok(raw) => raw,
            Err(e) if is_not_a_repo(&e) => {
                return Ok(vec![OutFrame::new(serde_json::json!({
                    "op": "status",
                    "not_a_repo": true,
                    "branch": null,
                    "head": null,
                    "detached": false,
                    // `clean` 用 `null` 而不是 `true`：这里根本没有「干不干净」
                    // 这回事，说 `true` 会让页面在非仓库上来回显示
                    // 「没有未提交的改动」。
                    "clean": null,
                    "files": [],
                }))]);
            }
            Err(e) => return Err(e),
        };
        let StatusView {
            branch,
            detached,
            files,
        } = parse_status(&raw);

        // HEAD 用 `rev-parse` 单独取：`status --branch` 只在跟踪分支时给
        // `## main...origin/main`，detached 时给 `## HEAD (no branch)`，
        // 拿不到短 hash。多一次子进程换一个确定的字段，值得。
        //
        // **不要 `unwrap_or(None)`**：`head_short` 内部已经把「空仓库没有
        // HEAD」折成 `Ok(None)` 了，剩下的 `Err` 是**真错**（git 挂了、
        // 权限不对）。吞掉它会让页面上「我不知道 HEAD」与「读 HEAD 失败了」
        // 显示成同一句话。
        let head = self.head_short(root)?;

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "status",
            // **两个 status 回帧形状必须完全一样**，包括这个字段。只在「不是
            // 仓库」那一支带上它，会让「读 `not_a_repo` 的值」在不同帧上拿到
            // `false` 和 `undefined` 两种结果——本里程碑的测试就撞上过：断言
            // `got["not_a_repo"] == false` 在空仓库上失败，因为键压根不存在。
            "not_a_repo": false,
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
            // **只有「空仓库」折成 `None`，其余 `Err` 原样传播**。
            //
            // 早先这里是 `Err(_) => Ok(None)`：它把 `rev-parse` 的所有失败
            // （git 挂了、权限不对、索引损坏）都显示成「这个仓库还没有 HEAD」，
            // 而这正是调用点 214-217 的注释明说要避免的事——守卫被删在了上游，
            // 却停在一个仍在吞错的地方。`is_empty_repo` 的判据就是
            // 「`does not have any commits yet` / `bad default revision`」，
            // 与 `log` 用的是同一套，不另写。
            Err(e) if is_empty_repo(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 工作区现在在哪个提交上。`v0.6.0` 加的第四个只读 op。
    ///
    /// **不接收任何参数**：三个既有 op 各有参数（`diff` 的 path/staged/commit、
    /// `log` 的 limit），`rev` 一个都不收——它回答的是「现在在哪」，只有一个答案。
    ///
    /// 三帧的键集合**逐字相同**，只有值不同。理由与 `status()` 那段注释一样：
    /// 只在某一支带上某个键，会让读它的值拿到 `true` 和 `undefined` 两种结果。
    fn rev(&self) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = self.root_checked()?;

        // 完整 hash 是 `base_rev` 的**存储形态**，所以这里必须用
        // `rev-parse HEAD`（不带 `--short`）：短 hash 的长度随仓库规模会变
        // （`core.abbrev` / 对象数增长时 git 会自动加长），拿它当基准的话
        // 同一个提交在不同时间可能给出不同的串。
        //
        // 「空仓库」与「不是仓库」都在这里被摘下错误路径，理由是同一个：
        // 通道层只往页面传错误码，detail 里那句 stderr 到不了页面，于是页面
        // 既画不出中性说明，也分不清「没有仓库 / 没有提交」和「git 真的挂了」。
        let head = match self.run_git(root, &["rev-parse", "HEAD"]) {
            Ok(raw) => Some(String::from_utf8_lossy(&raw).trim().to_string()),
            Err(e) if is_not_a_repo(&e) => {
                return Ok(vec![OutFrame::new(serde_json::json!({
                    "op": "rev",
                    "not_a_repo": true,
                    "head": null,
                    "short": null,
                    "branch": null,
                    "detached": false,
                }))]);
            }
            Err(e) if is_empty_repo(&e) => None,
            // 其余 `Err` 原样传播（git 挂了 / 权限不对 / 索引损坏）。**不许
            // `unwrap_or(None)`**——`head_short` 的注释记的就是这条教训：吞掉它
            // 会让「这个仓库还没有 HEAD」与「读 HEAD 失败了」显示成同一句话。
            Err(e) => return Err(e),
        };

        // 没有 HEAD 时**不再跑后面两条命令**：空仓库上 `rev-parse --abbrev-ref HEAD`
        // 同样以非零退出（同一条 `ambiguous argument`），再跑一次只是把同一个
        // 已知事实重新变成一次错误。直接给 null/false。
        let (short, branch, detached) = match &head {
            // 短 hash 从**同一个完整 hash** 截前 7 位，不另起一次
            // `git rev-parse --short`：两次子进程之间 HEAD 可能被别的东西推进
            // （用户的编辑器插件、agent 自己 commit），于是 `head` 与 `short`
            // 指向两个不同提交——而页面会把它们并排显示。
            //
            // 这与 `status()` 的做法不同（那里 `head_short` 独立取一次），
            // **是刻意的差异**：`status` 的 `head` 是给人看的分支状态附注，
            // 而 `rev` 的这一对值要一起落进 `base_rev`，不许有窗口。
            Some(h) => {
                let short = h.chars().take(7).collect::<String>();
                let (branch, detached) = self.branch_of(root)?;
                (Some(short), branch, detached)
            }
            None => (None, None, false),
        };

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "rev",
            "not_a_repo": false,
            "head": head,
            "short": short,
            "branch": branch,
            "detached": detached,
        }))])
    }

    /// 当前分支名 + 是否 detached。detached 时 `branch` 为 `None`。
    ///
    /// 判据与 `parse_status` 用的同一套（detached 时 `--abbrev-ref` 回字面量
    /// `HEAD`），不另写一份「`branch == "HEAD"` 算 detached」的第二处实现——
    /// 两处判据迟早会分叉，而分叉的那天没有任何测试会红。
    fn branch_of(&self, root: &Path) -> Result<(Option<String>, bool), ConnectorError> {
        let raw = self.run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let name = String::from_utf8_lossy(&raw).trim().to_string();
        if name == "HEAD" || name.is_empty() {
            Ok((None, true))
        } else {
            Ok((Some(name), false))
        }
    }

    fn diff(&self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = self.root_checked()?;
        let req = diff_args(data)?;

        // 越界判据与 files 同一套（`resolve_within`），不重写：
        // 三种逃逸（`..` / 绝对路径 / 链接出界）在那里已经各自被拒。
        //
        // 查的是 `req` 里那两个**已经拼进 argv 的同名值**，而不是回头再从
        // `data` 里 `.get()` 一遍：只查一份、用的也是那一份，才不会有
        // 「查的路径」与「交给 git 的路径」在结构上可以是两个值的缝。
        if let Some(path) = &req.path {
            resolve_within(root, path)?;
        }
        // `orig` 同样要过越界检查：它也会被拼进 pathspec 交给 git，
        // 只查 `path` 的话它就成了一条绕过 `resolve_within` 的路径。
        if let Some(orig) = &req.orig {
            resolve_within(root, orig)?;
        }

        let arg_refs: Vec<&str> = req.args.iter().map(String::as_str).collect();
        let raw = self.run_git(root, &arg_refs)?;
        let (unified, truncated) = truncate_utf8(&raw, PAYLOAD_BUDGET);

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "diff",
            "path": req.path,
            "staged": req.staged,
            "commit": req.commit,
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
        // 多取一条：`truncated` 靠它判断「还有没有更早的」（见下方注释）。
        // 页面收到的是前 `limit` 条（由调用侧截断）。
        let probe = limit.saturating_add(1);
        let raw = match self.run_git(root, &["log", fmt, "-n", &probe.to_string()]) {
            Ok(raw) => raw,
            Err(e) if is_empty_repo(&e) => Vec::new(),
            // coverage: excluded — 让 `git log` 在**非**空仓库上以非零退出且
            // 不被 `is_empty_repo` 认出来，需要把仓库搞坏（HEAD 指向不存在的
            // 对象 / 索引损坏）。那种状态下 `status` 也一起崩，页面上先坏的是
            // 另一块；为它造一个损坏仓库来测这一行的传播，测的其实是
            // `run_git` 已有的错误路径。
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&raw);
        let commits = parse_log(&text);

        // 是否还有更早的提交：**多取一条**就知道了（`limit + 1`）。
        //
        // 曾经这里算出 `limit + 1` 却从不使用它，真正的判据是另一次
        // `git rev-list --count HEAD`——那是**全历史计数**，在几十万提交的仓库上
        // 是一次与总提交数成正比的全量遍历，而这里要回答的只是「还有没有第
        // `limit + 1` 条」。多取一条的成本只与 `limit` 有关，且省掉一次子进程。
        let truncated = commits.len() as u32 > limit;
        // 那第 `limit + 1` 条只是为了探测「还有没有」，不留进回帧——
        // 页面按 `limit` 渲染，多一条会让「最近 N 条」这句话不成立。
        let mut commits = commits;
        commits.truncate(limit as usize);

        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "log",
            "commits": commits,
            "truncated": truncated,
        }))])
    }
}

impl Default for Git {
    // coverage: excluded — `Default` 是给 `#[derive(Default)]` 的容器与
    // `unwrap_or_default()` 准备的构造入口，生产代码从不经它拿 `Git`（通道
    // 一律走 `create(name)`）。这一行的存在理由是 trait 完整性，不是业务分支。
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
        // `close()` 会把取消旗置位（那是它的职责），而 `open()` 是**重新**开始：
        // 不复位的话这条通道从此每次请求都立刻被取消——`open` 内部就调了
        // `close()`，所以这不是理论问题，是第一请求就会撞上的。
        self.cancel.store(false, Ordering::SeqCst);
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
        let op = GitOp::parse(&op)?;
        if op == GitOp::Diff {
            diff_args(data)?;
        }

        // 副本进线程：`Git` 本身不能跨线程搬（它持着通道），而这次 git 调用
        // 只需要根、可执行文件路径与超时这三样。
        // coverage: excluded — `on_data` 开头已经 `root_checked()?` 过一次，
        // 所以这一格在这里恒为 `Some`。它是给「将来有人把开头的检查挪走」留的
        // 第二道闸：少了它，那种改动会让这里变成对 `None` 的 `unwrap`。
        let root = self.root.clone().ok_or_else(|| ConnectorError::Invalid {
            detail: NOT_OPEN.to_string(),
        })?;
        let git_bin = self.git_bin.clone();
        let timeout = self.timeout;
        let payload = data.clone();

        // **上一次请求还没取走结果时，拒绝新请求**，而不是顶掉它。
        //
        // 曾经这里无条件 `self.pending = Some(rx)`，后果是两个：
        // (a) 旧 `Receiver` 被丢 → 旧线程的 `tx.send` 失败 → 第一次请求的结果
        //     **永久丢失**，页面上对应那个文件的 `reading` 永不复位（它只在
        //     收到 diff 帧时才清），表现为「正在读取 diff…」一直转；
        // (b) 旧线程仍在跑那个 git 子进程，要等它自己超时（15s）才退出，
        //     所以**每顶掉一次就多一个 15 秒的 git 进程**。
        // 触发路径不是理论：连点两个文件就是两次 `on_data`，而第一次的
        // poll 明显晚于第二次请求到达。
        //
        // 选「拒绝」而不是「排队」：git 面板里连点两次的意图是「改看第二个」，
        // 排队会让用户等第一份（他已经不要的）diff 先回来；而拒绝是一句
        // 可读的 `Invalid`，页面可以立刻复位 `reading`。
        if self.pending.is_some() {
            return Err(ConnectorError::Invalid {
                detail: "a git request is already in flight".to_string(),
            });
        }

        let (tx, rx) = mpsc::sync_channel(1);
        let cancel = Arc::clone(&self.cancel);
        std::thread::Builder::new()
            .name("afcli-git".to_string())
            .spawn(move || {
                let result = run_op(&payload, root, git_bin, timeout, cancel);
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
        // 先让在飞的工作线程停手，再清状态：只丢弃 `Receiver` 的话，线程里那个
        // git 子进程会一直跑到超时（最长 15s），通道已经没了还在烧 CPU / 占 fd。
        self.cancel.store(true, Ordering::SeqCst);
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
    cancel: &AtomicBool,
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
        // 取消与超时共用 `Ok(None)` 这一出口：两者的处置一样（杀掉子进程、
        // 由调用方决定报什么），分成两个返回值只会让调用点多一个必处理的分支。
        // 调用方靠 `cancel` 旗分辨是哪一种。
        if Instant::now() >= deadline || cancel.load(Ordering::SeqCst) {
            return Ok(None);
        }
        // 10ms 粒度：git 命令多在几十毫秒内结束，再细是忙等，
        // 再粗会让短命令白白多等一个粒度。
        std::thread::sleep(Duration::from_millis(10));
    };

    // `join()` 失败（读线程 panic）**不能**被吞成空输出：那会把「管道读失败」
    // 伪装成「git 正常退出且什么都没输出」，而后者在页面上是「这个文件没有改动」——
    // 一条骗人的空结果。这一格在本环境下不可达（`read_to_end` 到 `Vec` 只在
    // OOM 时失败），但**吞错的代价是给出假答案**，所以按错误处理。
    let stdout = join_reader(out_reader, "stdout")?;
    let stderr = join_reader(err_reader, "stderr")?;
    Ok(Some(Captured {
        status,
        stdout,
        stderr,
    }))
}

/// 取回一个读线程的结果。
///
/// `join()` 返回 `Err` 表示那个线程 panic 了——此时它读到的东西不可信，
/// 必须当作 IO 错误往上抛，而不是回一个空 `Vec`
/// （空 `Vec` 会被上层当成「git 成功但没有输出」）。
fn join_reader(
    handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    which: &str,
) -> std::io::Result<Vec<u8>> {
    let Some(handle) = handle else {
        return Ok(Vec::new());
    };
    match handle.join() {
        Ok(buf) => Ok(buf),
        Err(_) => Err(std::io::Error::other(format!(
            "git {which} reader thread panicked"
        ))),
    }
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

/// 这个失败是不是「这里压根不是 git 仓库」。
///
/// 与 `classify_failure` 里那个固定前缀共用同一条判据，但用途相反：那边把
/// 它变成一句可识别的 detail，这边把它**从错误路径上摘下来**——它是正常
/// 状态，不该占一个错误码。
fn is_not_a_repo(e: &ConnectorError) -> bool {
    match e {
        ConnectorError::Failed { detail, .. } => detail.starts_with("not a git repository"),
        _ => false,
    }
}

/// 这个失败是不是「仓库里还没有提交」。
///
/// git 对无提交仓库的 `log` 以非零退出并说 `does not have any commits yet`
/// ——那是**一种正常状态**，不是故障。与「不是 git 仓库」一样，
/// 页面要把它画成中性说明而不是红框。
///
/// **`needed a single revision` 也在表里**：`rev-parse --short HEAD` 在还没有
/// 任何提交的仓库上说的是这句，而不是 `log` 那两句。`status` 要靠这里的判定
/// 才能把「空仓库」与「真读失败了」分开（见 `head_short`）。
///
/// **`ambiguous argument 'head': unknown revision` 也在表里**（`v0.6.0` 加）：
/// `rev-parse HEAD`（**不带** `--short`）在空仓库上说的既不是 `log` 那两句、
/// 也不是 `Needed a single revision`，而是
/// `fatal: ambiguous argument 'HEAD': unknown revision or path not in the working tree.`
/// （git 2.39.5 实测）。少了这一条，`rev()` 会把「刚 init、还没提交」的仓库
/// 当成真错误抛出去，于是 `base_rev_status` 落成 `unavailable`（「没能从设备
/// 读到起点」）而不是 `no_head`（「仓库还没有提交」）——前者把人引去排查设备，
/// 而真相只是这个仓库还空着。
///
/// 判据里带 `'head':` 与 `unknown revision` 两段限制，是为了不与「不是仓库」
/// 那条 stderr（`not a git repository`）交叠：两者必须互斥，否则
/// `rev()` 的分支顺序会决定结果。
fn is_empty_repo(e: &ConnectorError) -> bool {
    match e {
        ConnectorError::Failed { detail, .. } => {
            let d = detail.to_lowercase();
            d.contains("does not have any commits yet")
                || d.contains("does not have any commits")
                || d.contains("bad default revision")
                || d.contains("needed a single revision")
                || d.contains("ambiguous argument 'head': unknown revision")
        }
        _ => false,
    }
}

/// `diff` 一次解析的产物：交给 git 的 argv，以及要回显进回帧的字段。
///
/// **两者必须同源**：早先 argv 由 `diff_args` 算、回显字段由 `diff()` 自己
/// 再从 `data` 里 `.get()` 一遍，同一个字段读两次。当下两处读的是同一份
/// `Value` 所以值一致，但这是「一份事实两个来源」——越界检查用的 `path`
/// 与拼进 argv 的 `path` 在结构上可以不是同一个值。一次解析同时产出两者，
/// 这层不一致就没有了。
struct DiffReq {
    args: Vec<String>,
    path: Option<String>,
    orig: Option<String>,
    staged: bool,
    commit: Option<String>,
}

/// `diff` 的参数。**三种取法互斥**，且都校验形状后才进参数数组。
fn diff_args(data: &serde_json::Value) -> Result<DiffReq, ConnectorError> {
    let staged = data
        .get("staged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let path = data.get("path").and_then(serde_json::Value::as_str);
    let orig = data.get("orig").and_then(serde_json::Value::as_str);
    let commit = data.get("commit").and_then(serde_json::Value::as_str);

    if commit.is_some() && (path.is_some() || staged) {
        return Err(ConnectorError::Invalid {
            detail: "\"commit\" cannot be combined with \"path\" or \"staged\"".to_string(),
        });
    }

    let echo = |args: Vec<String>| DiffReq {
        args,
        path: path.map(str::to_string),
        orig: orig.map(str::to_string),
        staged,
        commit: commit.map(str::to_string),
    };

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
        return Ok(echo(vec![
            "show".to_string(),
            "--format=".to_string(),
            "--patch".to_string(),
            commit.to_string(),
        ]));
    }

    let mut args: Vec<String> = vec!["diff".to_string(), "--no-color".to_string()];
    if staged {
        args.push("--cached".to_string());
    }
    if let Some(path) = path {
        args.push("--".to_string());
        args.push(path.to_string());
        // 重命名 / 复制必须**两条路径都给**，git 才做相似度检测。
        // 只给新路径时，git 把它当「新建文件」——diff 里是 `new file mode`
        // 而不是 `rename from`，页面于是把一次改名显示成「新建了这个文件」。
        // 这是 rename 检测的固有要求，不是可以给 git 加个开关绕开的：
        // pathspec 过滤掉旧路径后，git 无从知道那个被删的文件去哪了。
        if let Some(orig) = orig {
            args.push(orig.to_string());
        }
    }
    Ok(echo(args))
}

/// `status` 的解析结果。
///
/// 用具名结构体而不是四元组：返回四个位置相关的值，调用处得靠顺序记
/// 「第二个是 head 还是 detached」，而这两个都是 `Option`/`bool` 混排，
/// 写反了编译器不一定拦得住。
struct StatusView {
    branch: Option<String>,
    detached: bool,
    files: Vec<serde_json::Value>,
}
// 这里没有 `head` 字段：`status --branch` 的输出里**从来没有**短 hash（只有
// 分支名或 `HEAD (no branch)`），HEAD 一律由调用方 `head_short()` 单独取。
// 曾经留过一个恒为 `None` 的 `head` 字段加一条「解析给不出时补一次」的注释，
// 那句注释会让读者以为它有时给得出——实际那一臂从未被执行过。

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
        let path = path.to_string();

        // 重命名 / 复制：`-z` 下**旧路径是下一条记录**（不是同一行里用 `->` 分隔，
        // 那种形状只在非 `-z` 输出里出现）。
        //
        // **旧路径另放一个字段，不拼进 `path`**。`path` 会被页面原样填回
        // `{"op":"diff","path":…}`（它是「请求用标识」而非「显示串」）；
        // 拼成 `新 <- 旧` 之后，这个串既不是新路径也不是旧路径——
        // `git diff -- "新 <- 旧"` 因路径不存在返回空串，而页面把「空 diff」
        // 显示成「这个文件没有可显示的改动」，于是**重命名过的文件一律点不开，
        // 且理由被错报成「没有改动」**。两个字段各自只担一件事。
        let mut orig = None;
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            if let Some(o) = parts.next() {
                orig = Some(o.to_string());
            }
        }
        if path.is_empty() {
            continue;
        }
        let mut entry = serde_json::json!({
            "x": x.to_string(),
            "y": y.to_string(),
            "path": path,
        });
        if let Some(orig) = orig {
            // 用 `as_object_mut` 而不是 `entry["orig"] = …`：后者是对 `Value`
            // 做索引赋值，形状不对时 panic，而本 crate 禁裸索引
            // （`AGENTS.md` 生产代码禁 panic 铁律）。
            if let Some(map) = entry.as_object_mut() {
                map.insert("orig".to_string(), serde_json::Value::String(orig));
            }
        }
        files.push(entry);
    }

    StatusView {
        branch,
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

/// 跑一次 git op 并给出结果帧。**在 `on_data` 起的那个线程里执行。**
///
/// 分开的两个理由：① 它碰子进程、且要求 `payload`/`root` 等全部是所有权
/// 副本（`Git` 本身不能跨线程搬——它持着通道）；② `on_data` 只做校验与登记，
/// 长度才压得住：这道分工线画在这里，加 op 时改的是分派表而不是校验段。
fn run_op(
    payload: &serde_json::Value,
    root: PathBuf,
    git_bin: PathBuf,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<OutFrame>, ConnectorError> {
    // 线程里重建一个临时 `Git`：它的方法都是 `&self`，不碰通道状态。
    // `cancel` 是从通道那边共享过来的同一份旗：`close()` 一置位，这里的
    // `run_git` 就会在下一个 10ms 轮询点杀掉子进程。
    let worker = Git {
        root: Some(root),
        git_bin,
        timeout,
        pending: None,
        cancel,
    };
    // 分派从 `GitOp` 来，与 `on_data` 的白名单**同一张表**：早先这里是手写的
    // `"status" => … "diff" => … "log" => …`，那是第二张表——加一个 op 时
    // 只改一处、另一处悄悄落进 `other` 臂，编译不会报错。
    match GitOp::parse(
        payload
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    ) {
        Ok(GitOp::Status) => worker.status(),
        Ok(GitOp::Diff) => worker.diff(payload),
        Ok(GitOp::Log) => worker.log(payload),
        Ok(GitOp::Rev) => worker.rev(),
        // coverage: excluded — `on_data` 在起线程之前已经 `GitOp::parse` 过了，
        // 所以线程里这一臂不可达。留着是因为这里拿到的是 `&str`（要重新解析），
        // 而「默认放行」在安全上是洞（未知 op 静默成功）。
        Err(e) => Err(e),
    }
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
        Connector::open(
            &mut g,
            OpenReq::new(ws.to_string_lossy(), serde_json::json!({})),
        )
        .expect("open");
        g
    }

    /// 跑一次 op 并取出结果帧的 payload。
    ///
    /// `on_data` 只起线程、结果由 `poll` 交出——所以这里必须**等**：
    /// 直接断言第一次 `poll` 会拿到空数组，那是设计而不是失败。
    fn call(g: &mut Git, data: serde_json::Value) -> Result<serde_json::Value, ConnectorError> {
        Connector::on_data(g, &data)?;
        call_pending(g)
    }

    /// 只取走已在飞的那条请求的结果（不发新请求）。
    ///
    /// `call` 把「发」与「等」绑在一起，测「在飞时新请求被拒」时必须分开：
    /// 发完不 poll 才是「在飞」这件事本身。
    fn call_pending(g: &mut Git) -> Result<serde_json::Value, ConnectorError> {
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

    // ------------------------------------------------------- join_reader 的处置

    /// 读线程 panic 时**不能**被当成空输出。
    ///
    /// 空 `Vec` 会被上层读成「git 成功但没有输出」，于是 `diff` 报「没有改动」——
    /// 一次 panic 就这样变成了假答案。所以这一格要的是 `Err`。
    #[test]
    fn join_reader_turns_a_panicked_reader_into_an_error() {
        // 不需要让真实的 git 子进程崩：`join_reader` 收的是 `JoinHandle<Vec<u8>>`，
        // 测试里自己 spawn 一个 panic 的线程就是同一个类型。
        let handle = std::thread::spawn(|| -> Vec<u8> { panic!("reader blew up") });
        let err = join_reader(Some(handle), "stdout").expect_err("panic 要变成错误");
        assert!(
            err.to_string()
                .contains("git stdout reader thread panicked"),
            "错误信息要说出是哪个读线程，实际 {err}"
        );
    }

    /// 没有读线程（管道没接管）时是空输出，而不是错误。
    #[test]
    fn join_reader_with_no_thread_is_an_empty_output() {
        let out = join_reader(None, "stderr").expect("没有读线程不算错误");
        assert!(out.is_empty(), "管道没接管时输出为空，实际 {out:?}");
    }

    /// 正常的读线程把字节原样交回来。
    #[test]
    fn join_reader_returns_what_the_thread_read() {
        let handle = std::thread::spawn(|| b"hello".to_vec());
        assert_eq!(join_reader(Some(handle), "stdout").expect("ok"), b"hello");
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

    /// 改本进程环境的用例必须互斥。
    ///
    /// `std::env::set_var` 动的是**整个进程**的环境：两个用例并行跑时，
    /// A 刚种下 `GIT_DIR`，B 的 `remove_var` 就把它删了，或者 A 的子进程
    /// 读到的是 B 种的值。症状是「单跑全绿、全量随机红」。
    ///
    /// 锁本体在 [`crate::pty::env_lock`]：PTY 的用例也改进程环境，各持一份
    /// `static` 时两把锁互不互斥（实测改其中一份的类型，两份都编译通过）。
    /// 那把锁**不在 `testutil` 里**——那个模块限 Unix，而这条用例在 Windows
    /// 上也要跑。
    use crate::pty::env_lock;

    /// 继承来的 `GIT_DIR` 不能改变 connector 认定的工作区。
    ///
    /// 真实症状：`afcli` 被从 git 钩子 / IDE 的 git 集成里拉起时，环境里带着
    /// 指向**另一个**仓库的 `GIT_DIR`。不清的话 `current_dir(root)` 会被
    /// `GIT_DIR` 压过去，页面报出那个仓库的分支——用户看到的根本不是这个会话的
    /// 工作区，而且没有任何迹象说明读错了对象。
    #[test]
    fn an_inherited_git_dir_does_not_hijack_the_workspace() {
        let _guard = env_lock();
        let (_base_a, ws) = sandbox();
        repo_with_commit(&ws);

        // 另造一个**别的**仓库，用 `GIT_DIR` 指着它。它的分支名与本仓库不同，
        // 这样「读到了哪一个」在断言里是可区分的。
        let (_base_b, other) = sandbox();
        repo_with_commit(&other);
        git_in(&other, &["checkout", "-q", "-b", "hijacked"]);

        let other_git = other.join(".git");
        std::env::set_var("GIT_DIR", &other_git);
        let got = call(&mut open(&ws), serde_json::json!({"op": "status"}));
        std::env::remove_var("GIT_DIR");
        let got = got.expect("status");

        assert_eq!(
            got["branch"], "main",
            "继承的 GIT_DIR 不能把工作区换掉（读到 hijacked 说明没清 GIT_*）: {got}"
        );
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
    fn status_outside_a_repo_is_a_normal_frame_saying_so() {
        // 「不是 git 仓库」是**正常状态**，不是错误：页面对它画的是一句中性
        // 说明。走错误路径的话通道层只把错误码传出去，detail 里的那句
        // `not a git repository` 到不了页面——页面既画不出说明，也分不清
        // 「不是仓库」和「git 真挂了」。
        let (_base, ws) = sandbox();
        let mut g = open(&ws);
        let payload = call(&mut g, serde_json::json!({"op": "status"})).expect("非仓库不该报错");

        assert_eq!(payload["op"], "status");
        assert_eq!(payload["not_a_repo"], true);
        // `clean` 必须是 `null` 而不是 `true`：这里没有「干不干净」这回事，
        // 说 `true` 会让页面在非仓库上显示「没有未提交的改动」。
        assert!(
            payload["clean"].is_null(),
            "clean 应是 null，实际 {}",
            payload["clean"]
        );
        assert!(payload["branch"].is_null());
        assert!(payload["head"].is_null());
        assert_eq!(payload["detached"], false);
        assert_eq!(payload["files"].as_array().map(Vec::len), Some(0));
    }

    /// `classify_failure` 在 stderr 全空时有话可说。
    ///
    /// git 有时以非零退出却一句都不说（信号、包装脚本）。这时回空串会让页面上
    /// 的错误框标题后面什么都没有——用户看到「失败：」然后没了。
    #[test]
    fn classify_failure_says_something_when_stderr_is_empty() {
        assert_eq!(classify_failure(""), "git failed without a message");
        // 只有空白也一样：`lines()` 会给出一个空行，`find(|l| !l.is_empty())`
        // 必须跳过它而不是把它当「第一行非空」。
        assert_eq!(
            classify_failure("   \n\t\n"),
            "git failed without a message"
        );
        // 前导空行要被跳过，报的是第一行**非空**内容。
        assert_eq!(
            classify_failure("\n\n  fatal: bad revision 'nope'  \nusage: ..."),
            "fatal: bad revision 'nope'"
        );
    }

    /// `is_not_a_repo` / `is_empty_repo` 对「不是那类错误」必须回 `false`。
    ///
    /// 两条都是把**正常状态**从错误路径上摘下来的判据。判据写宽了会把真正的
    /// 失败当成正常态静默吞掉（`log` 报空列表、`status` 报不是仓库），而那种
    /// 错误页面上完全看不出来。
    #[test]
    fn the_normal_state_predicates_do_not_swallow_other_failures() {
        // `Failed` 之外的错误类型一律不认。
        for e in [
            ConnectorError::Invalid {
                detail: "not a git repository".to_string(),
            },
            ConnectorError::Internal {
                detail: "not a git repository".to_string(),
            },
            ConnectorError::OutsideWorkspace {
                shown: "not a git repository".to_string(),
            },
        ] {
            assert!(!is_not_a_repo(&e), "{e:?} 不是「不是仓库」");
            assert!(!is_empty_repo(&e), "{e:?} 不是「空仓库」");
        }
        // `Failed` 但 detail 不是那两句话。
        let failed = ConnectorError::Failed {
            op: "log".to_string(),
            detail: classify_failure("fatal: bad revision 'nope'"),
        };
        assert!(!is_not_a_repo(&failed));
        assert!(!is_empty_repo(&failed));
        // 空仓库的三种说法都要认（不同 git 版本的措辞不同）。
        for detail in [
            "your current branch 'main' does not have any commits yet",
            "fatal: your current branch does not have any commits",
            "fatal: bad default revision 'HEAD'",
            // 大小写不敏感。
            "FATAL: BAD DEFAULT REVISION 'HEAD'",
        ] {
            let e = ConnectorError::Failed {
                op: "log".to_string(),
                detail: detail.to_string(),
            };
            assert!(is_empty_repo(&e), "{detail:?} 该被认成空仓库");
            assert!(!is_not_a_repo(&e), "{detail:?} 不是「不是仓库」");
        }
    }

    /// `parse_status` 对脏数据只跳过，不 panic、不造出空路径条目。
    ///
    /// `status --porcelain=v1 -z` 的记录形状是 `XY <path>`。手写脏输入比造真
    /// 仓库容易：文件名里有换行 / git 版本多了字段时，我们宁可少一条也不该
    /// 让面板崩掉或列出一条点不开的空条目。
    #[test]
    fn parse_status_skips_records_it_cannot_read() {
        // ① 比两个字节还短的条目 → 跳过（`get(..2)` 拿不到 XY）。
        // ② 正好两个字节、没有路径 → 落到 `rest` 为空的分支，路径为空串。
        // ③ detached：`## HEAD (no branch)` 要认出来，且不留分支名。
        let got = parse_status(b"A\0## HEAD (no branch)\0");
        assert!(got.detached, "detached HEAD 要认出来");
        assert_eq!(got.branch, None, "detached 时没有分支名可写");

        // `## HEAD (detached at abc123)` 是另一种说法。
        let got = parse_status(b"## HEAD (detached at abc123)\0");
        assert!(got.detached);

        // 分支后带上游：`main...origin/main` → 只取 `main`。
        let got = parse_status(b"## main...origin/main\0 M a.txt\0");
        assert_eq!(got.branch.as_deref(), Some("main"));
        assert!(!got.detached);
        // `StatusView` 没实现 `Debug`（生产代码不需要，为测试加 derive 不划算），
        // 所以这里逐字段断言，失败信息里带不出的部分用 count 代替。
        assert_eq!(got.files.len(), 1, "改动条目要留着");
    }

    /// `poll` 在**没有进行中请求**时是空队列，不是错误。
    ///
    /// 通道每 100ms 被问一次 `poll`，绝大多数时候什么都没有。把这一路写成
    /// 错误会让页面每 100ms 收到一个红框。
    #[test]
    fn poll_with_nothing_pending_is_silent() {
        let (_base, ws) = sandbox();
        let mut g = open(&ws);
        let frames = Connector::poll(&mut g).expect("没有进行中的请求不该报错");
        assert!(frames.is_empty(), "空队列就该是空: {frames:?}");
        // 重复问也一样（幂等）。
        assert!(Connector::poll(&mut g).expect("幂等").is_empty());
    }

    /// 空仓库（`git init` 之后一次提交都没有）的 `status` 是正常回帧。
    ///
    /// 这条同时覆盖两条分支：`parse_status` 里没有 `## ` 行时 `head` 为 `None`
    /// （走 `head_short`），而 `rev-parse --short HEAD` 在空仓库上非零退出——
    /// 那一支必须回 `None` 而不是 `Err`，否则新建仓库一打开面板就是一个红框。
    #[test]
    fn status_of_a_repo_without_commits_is_a_normal_frame() {
        let (_base, ws) = sandbox();
        git_in(&ws, &["init", "-q", "-b", "main", "."]);
        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "status"})).expect("空仓库不是错误");

        assert_eq!(got["not_a_repo"], false, "空仓库仍是仓库: {got}");
        assert_eq!(got["clean"], true, "没有文件就干净: {got}");
        assert!(got["head"].is_null(), "空仓库没有 HEAD: {got}");
        // 空仓库的 `git status --branch` 给 `## No commits yet on main`，
        // 不是 `## main`——分支名取不取得到都行，但不能因此报错。
        assert_eq!(got["files"].as_array().map(Vec::len), Some(0));
    }

    /// 各种**形状不对**的请求一律变成 `Invalid`，而不是 panic 或静默成功。
    ///
    /// `Invalid` 是通道层能翻译成 `channel_error` 的那一档；掉进 `Internal`
    /// 在页面上长得一样但日志里丢掉了「是用户发错了」这个信息。
    #[test]
    fn malformed_requests_are_invalid_with_a_readable_reason() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);

        // ① 没有 `op` 键 → `data must carry a string "op"`。
        let mut g = open(&ws);
        match call(&mut g, serde_json::json!({})) {
            Err(ConnectorError::Invalid { detail }) => {
                assert!(
                    detail.contains("must carry a string"),
                    "要说清缺的是什么，实际 {detail:?}"
                );
            }
            other => panic!("缺 op 要落到 Invalid，实际 {other:?}"),
        }

        // ② `op` 不是字符串。
        match call(&mut g, serde_json::json!({"op": 42})) {
            Err(ConnectorError::Invalid { .. }) => {}
            other => panic!("op 不是字符串要落到 Invalid，实际 {other:?}"),
        }

        // ③ 未知 op。
        match call(&mut g, serde_json::json!({"op": "commit"})) {
            Err(ConnectorError::Invalid { detail }) => {
                assert!(
                    detail.contains("unknown git operation"),
                    "要说清是不认识的 op，实际 {detail:?}"
                );
            }
            other => panic!("未知 op 要落到 Invalid，实际 {other:?}"),
        }
    }

    /// 上一次请求还没取走结果时，**新请求被拒**，而不是静默顶掉旧的。
    ///
    /// 这条是「结果永久丢失 + git 子进程泄漏 15s」那个 bug 的可证伪断言：
    /// 曾经 `on_data` 无条件 `self.pending = Some(rx)`，旧 `Receiver` 被丢后
    /// 第一帧结果再也取不到（页面上的 `reading` 永不复位），而旧线程要等
    /// 自己超时才退出。
    #[test]
    fn a_second_request_while_one_is_in_flight_is_rejected() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);

        // 第一次：只发，不 poll——模拟「请求已上路、结果还没取」。
        Connector::on_data(&mut g, &serde_json::json!({"op": "status"})).expect("第一次要收下");
        assert!(g.pending.is_some(), "前提：这时确实有一条在飞");

        // 第二次：必须被拒，且说的是「有一条在飞」而不是别的原因。
        match Connector::on_data(&mut g, &serde_json::json!({"op": "diff", "path": "a.txt"})) {
            Err(ConnectorError::Invalid { detail }) => {
                assert!(
                    detail.contains("already in flight"),
                    "要说清是有一条在飞，实际 {detail:?}"
                );
            }
            other => panic!("有一条在飞时新请求要被拒，实际 {other:?}"),
        }
        // 被拒**不能**把在飞的那条顶掉——顶掉就是丢结果。
        assert!(
            g.pending.is_some(),
            "被拒后原来那条必须还在，否则结果就丢了"
        );

        // 取走结果后，同样的请求就能发了（守卫是「在飞」判据，不是永久锁）。
        let first = call_pending(&mut g).expect("第一次的结果要拿得到");
        assert_eq!(first["op"], "status", "拿到的是第一次的结果: {first}");
        Connector::on_data(&mut g, &serde_json::json!({"op": "diff", "path": "a.txt"}))
            .expect("取走之后要能发新的");
    }

    /// 通道没开时发任何 op 都是 `Invalid`，且说的是「通道没开」而不是别的原因。
    ///
    /// 顺序有讲究：先判「开着吗」再判「op 对不对」。反过来的话，用户在已关闭的
    /// 通道上发一个拼错的 op，看到的是「未知操作」——会去找操作名的问题。
    #[test]
    fn ops_before_open_name_the_real_reason() {
        let mut g = Git::new();
        match Connector::on_data(&mut g, &serde_json::json!({"op": "status"})) {
            Err(ConnectorError::Invalid { detail }) => {
                assert!(
                    detail.contains("not open"),
                    "要先说通道没开，实际 {detail:?}"
                );
            }
            other => panic!("没开通道要落到 Invalid，实际 {other:?}"),
        }
        // 拼错的 op 在没开通道时也报「没开」，不报「未知操作」。
        match Connector::on_data(&mut g, &serde_json::json!({"op": "nope"})) {
            Err(ConnectorError::Invalid { detail }) => {
                assert!(detail.contains("not open"), "实际 {detail:?}");
            }
            other => panic!("实际 {other:?}"),
        }
    }

    /// `open` 一个可用的根之后，再 `open` 一次要换到新根，且**丢掉旧根的进行中请求**。
    ///
    /// 覆盖 `open` 里的 `self.close()`：不清的话上一条通道的 `pending` 会留到
    /// 新根上，`poll` 交出的结果按**旧根**的路径算出来送给新根的页面。
    #[test]
    fn reopening_moves_to_the_new_root_and_drops_the_old_state() {
        let (base, ws) = sandbox();
        repo_with_commit(&ws);
        let other = base.path().join("other");
        std::fs::create_dir_all(&other).expect("mkdir");

        let mut g = open(&ws);
        let first = call(&mut g, serde_json::json!({"op": "status"})).expect("status");
        assert_eq!(first["files"].as_array().map(Vec::len), Some(0));

        // 造一个「进行中的请求」：换根之后它必须不在了。
        let (tx, rx) = mpsc::sync_channel::<Result<Vec<OutFrame>, ConnectorError>>(1);
        g.pending = Some(rx);
        let _keep = tx; // 别 drop，否则 Disconnected 会把断言变成「碰巧对了」

        Connector::open(
            &mut g,
            OpenReq::new(other.to_string_lossy(), serde_json::json!({})),
        )
        .expect("换根");
        assert!(
            g.pending.is_none(),
            "换根要丢掉旧根的进行中请求，否则结果会按旧路径算出来送到新根"
        );

        // 新根不是仓库 → 正常回帧说它不是一个仓库（不是上一根的残留）。
        let second = call(&mut g, serde_json::json!({"op": "status"})).expect("status");
        assert_eq!(
            second["not_a_repo"], true,
            "换根之后要按新根回答，实际 {second}"
        );
    }

    /// `open` 指向一个不存在的路径 → `WorkspaceRoot`，且理由里带上那个路径。
    #[test]
    fn open_of_a_missing_path_says_which_path() {
        let (_base, ws) = sandbox();
        let missing = ws.join("nope");
        let mut g = Git::new();
        match Connector::open(
            &mut g,
            OpenReq::new(missing.to_string_lossy(), serde_json::json!({})),
        ) {
            Err(ConnectorError::WorkspaceRoot { detail }) => {
                assert!(
                    detail.contains("nope"),
                    "理由里要带上是哪个路径，实际 {detail:?}"
                );
            }
            other => panic!("不存在的路径要落到 WorkspaceRoot，实际 {other:?}"),
        }
    }

    /// `poll` 在**结果已经被丢掉**（发送端消失）时回 `Internal`，不是永久空转。
    ///
    /// 这一支只在工作线程带着结果死掉时发生。空转的话页面会永远停在
    /// 「正在读取…」；报一次错至少让用户能重试。
    #[test]
    fn poll_reports_when_the_worker_died_without_answering() {
        let (_base, ws) = sandbox();
        let mut g = open(&ws);
        // 造一个「发送端立刻 drop」的 receiver：与工作线程没发结果就退出同形。
        let (tx, rx) = mpsc::sync_channel::<Result<Vec<OutFrame>, ConnectorError>>(1);
        drop(tx);
        g.pending = Some(rx);

        match Connector::poll(&mut g) {
            Err(ConnectorError::Internal { detail }) => {
                assert!(
                    detail.contains("could not be run"),
                    "要说清是命令没跑起来，实际 {detail:?}"
                );
            }
            other => panic!("工作线程死掉要落到 Internal，实际 {other:?}"),
        }
        // 报过错之后槽位要清掉，不能每次 poll 都再报一遍。
        assert!(
            Connector::poll(&mut g).expect("清过了").is_empty(),
            "第二次 poll 应该是空"
        );
    }

    /// `parse_status` 遇到比 `XY ` 还短的记录只跳过。
    ///
    /// 这些形状不该出现在正常 git 输出里，但解析器面对的是设备上的任意字节，
    /// 「少一条」远好过「整条通道 panic」。
    #[test]
    fn parse_status_skips_a_record_shorter_than_the_status_codes() {
        // `A\0` —— 一个字节，拿不到 XY，必须跳过而不是索引越界。
        let got = parse_status(b"A\0");
        assert_eq!(got.files.len(), 0, "短记录要跳过");
        // ` M a.txt` 是正常形状，不能被上一条的规则误伤。
        let got = parse_status(b" M a.txt\0");
        assert_eq!(got.files.len(), 1, "正常记录要留着");
        assert_eq!(got.files.first().map(|f| f["y"].as_str()), Some(Some("M")));
    }

    /// `parse_log` 遇到空 hash 的记录跳过。
    ///
    /// `%H` 不会是空的，但记录分隔符 `\x1e` 前面可能留下空片段。不跳过的话
    /// 页面会多出一条「点不开、没有 hash」的提交。
    #[test]
    fn parse_log_skips_a_record_with_no_hash() {
        // 两条记录：第一条正常，第二条 hash 为空。
        //
        // 字段用 `\u{0}` / 记录分隔用 `\u{1e}` 而不是 `\0` / `\x1e`：`\0` 后面
        // 紧跟数字（这里是日期 `2026-…`）会被读成**八进制转义**的歧义形状，
        // clippy 的 `octal-escapes` 会拒（`\0` 恒为 NUL，八进制不支持）。
        let text = "abc123\u{0}Ann\u{0}2026-10-29\u{0}feat: x\u{1e}\u{0}Bob\u{0}2026-10-29\u{0}empty\u{1e}";
        let got = parse_log(text);
        assert_eq!(got.len(), 1, "空 hash 的记录要跳过: {got:?}");
        assert_eq!(
            got.first().map(|c| c["hash"].as_str()),
            Some(Some("abc123"))
        );
    }

    /// `parse_status` 跳过**状态码之后没有路径**的记录。
    ///
    /// `XY `（末尾那个空格被 `strip_prefix` 吃掉后剩空串）只会在脏输出里出现，
    /// 但一条空路径的条目在页面上是「点开就报越界」的一行。
    #[test]
    fn parse_status_skips_a_record_with_an_empty_path() {
        // ` M ` —— XY 是 ` M`，去掉分隔空格后路径为空。
        let got = parse_status(b" M \0");
        assert_eq!(got.files.len(), 0, "空路径的记录要跳过");
        // 同一段里正常的那条要留着：跳过不能把整批丢掉。
        let got = parse_status(b" M \0 M a.txt\0");
        assert_eq!(got.files.len(), 1, "正常记录要留着");
    }

    /// 直接调 `status()`（不经 `on_data`）在通道没开时也报 `Invalid`。
    ///
    /// `on_data` 前面有一次 `root_checked()`，所以那里更早返回；但 `status()` 是
    /// `pub` 面之外仍可能被别处调到的入口，它自己的校验不能靠调用方兜。
    #[test]
    fn status_without_a_root_is_invalid() {
        let g = Git::new();
        match g.status() {
            Err(ConnectorError::Invalid { detail }) => {
                assert!(detail.contains("not open"), "实际 {detail:?}");
            }
            other => panic!("没根要落到 Invalid，实际 {other:?}"),
        }
        match g.log(&serde_json::json!({"op": "log"})) {
            Err(ConnectorError::Invalid { .. }) => {}
            other => panic!("log 没根也要落到 Invalid，实际 {other:?}"),
        }
        match g.diff(&serde_json::json!({"op": "diff", "path": "a.txt"})) {
            Err(ConnectorError::Invalid { .. }) => {}
            other => panic!("diff 没根也要落到 Invalid，实际 {other:?}"),
        }
    }

    #[test]
    fn a_failure_outside_a_repo_still_classifies_to_the_fixed_prefix() {
        // `is_not_a_repo` 的判据就是这条前缀。前缀若被改（或 `classify_failure`
        // 被删），`status` 的那条正常回帧会静默失效——变成每次打开面板都报
        // 「git 失败」。所以这条单独钉住判据本身。
        assert_eq!(
            classify_failure(
                "fatal: not a git repository (or any of the parent directories): .git"
            ),
            "not a git repository"
        );
        // 非仓库失败必须被 `is_not_a_repo` 认出来，其它失败不行。
        let not_repo = ConnectorError::Failed {
            op: "status".to_string(),
            detail: classify_failure("fatal: not a git repository: .git"),
        };
        assert!(is_not_a_repo(&not_repo), "非仓库失败必须被认出来");
        let other = ConnectorError::Failed {
            op: "status".to_string(),
            detail: classify_failure("fatal: bad revision 'nope'"),
        };
        assert!(!is_not_a_repo(&other), "别的失败不能被当成非仓库");
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

    /// 越界的 `orig` 同样要被拒——不能只查 `path`。
    ///
    /// `orig` 与 `path` 都会拼进 pathspec 交给 git，只查一个的话另一个就是
    /// 绕过 `resolve_within` 的路径。这条与上一条**各查一边**：上一条只喂
    /// `path`、这条只喂 `orig`，所以删掉任一侧的守卫都会有一条变红。
    #[test]
    fn diff_rejects_an_outside_workspace_orig() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);

        let err = call(
            &mut g,
            serde_json::json!({"op": "diff", "path": "a.txt", "orig": "../../etc/passwd"}),
        )
        .expect_err("越界 orig 必须被拒");

        match err {
            ConnectorError::OutsideWorkspace { shown } => {
                assert_eq!(shown, "../../etc/passwd", "shown 是用户自己写的那个写法");
                assert!(
                    !shown.contains(ws.to_string_lossy().as_ref()),
                    "不得泄漏工作区根的绝对路径"
                );
            }
            other => panic!("越界 orig 要落到 OutsideWorkspace，实际 {other:?}"),
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
        assert!(matches!(
            diff_args(&data),
            Err(ConnectorError::Invalid { .. })
        ));

        let data = serde_json::json!({"op": "diff", "commit": "abc1234", "staged": true});
        assert!(matches!(
            diff_args(&data),
            Err(ConnectorError::Invalid { .. })
        ));
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

    /// `truncated` 的判据是「多取的那一条是否存在」，且**那条不能进回帧**。
    ///
    /// 实现为了判断「还有没有更早的」会向 git 多要一条（`limit + 1`）。
    /// 若忘了截掉，页面收到的就是 `limit + 1` 条——而它对外说的是「最近 N 条」。
    #[test]
    fn log_truncates_the_probe_entry_out_of_the_frame() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        for i in 2..=5 {
            std::fs::write(ws.join("a.txt"), format!("line {i}\n")).expect("write");
            git_in(&ws, &["add", "a.txt"]);
            git_in(&ws, &["commit", "-q", "-m", &format!("commit {i}")]);
        }
        let mut g = open(&ws);
        // 共 5 条提交，limit=3 → 回 3 条 + truncated=true（第 4 条只是探测用）。
        let got = call(&mut g, serde_json::json!({"op": "log", "limit": 3})).expect("log");
        let commits = got["commits"].as_array().expect("commits");
        assert_eq!(
            commits.len(),
            3,
            "回帧里不能出现那第 `limit + 1` 条探测记录: {got}"
        );
        assert_eq!(
            got["truncated"], true,
            "共 5 条却只给 3 条，要声明截断: {got}"
        );
        // 正好给完时不截断：这条锁住「多取一条」不会在边界上误报。
        let all = call(&mut g, serde_json::json!({"op": "log", "limit": 5})).expect("log");
        assert_eq!(all["commits"].as_array().map(Vec::len), Some(5), "{all}");
        assert_eq!(all["truncated"], false, "正好给完不该说截断: {all}");
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

    /// `close()` 必须让在飞的 git 请求**立刻停手**，而不是等它自己超时。
    ///
    /// 通道被回收（用户关掉面板）时若只丢弃 `Receiver`，那个 git 子进程会一直
    /// 跑到 15s 超时——通道已经没了还在烧 CPU / 占 fd。
    ///
    /// 判据是**子进程真的没了**，不是「报了什么错」：`close()` 之后 `pending`
    /// 已被清空，结果本来就取不到（那次请求的结果已无人接收，这是设计）。
    /// 所以这条用挂住的假 git + 长超时（30s）来区分：不等取消的话，那个
    /// `sleep 3600` 会活到 30s 超时，而这里 1 秒内就该消失。
    #[test]
    fn closing_the_channel_cancels_an_in_flight_git() {
        let (_base, ws) = sandbox();
        let bin = _base.path().join("fake-git");
        // 把 pid 写进一个文件，好在事后从外部核它是否还活着。
        let pidfile = _base.path().join("pid");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho $$ > '{}'\nsleep 3600\n", pidfile.display()),
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut g = Git::with_git_bin(&bin).with_timeout(Duration::from_secs(30));
        Connector::open(
            &mut g,
            OpenReq::new(ws.to_string_lossy(), serde_json::json!({})),
        )
        .expect("open");

        Connector::on_data(&mut g, &serde_json::json!({"op": "status"})).expect("收下请求");
        // 等假 git 真的跑起来并写下 pid。
        let start = Instant::now();
        while !pidfile.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "假 git 没起来，这条测不到想测的东西"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("read pid")
            .trim()
            .parse()
            .expect("pid is a number");

        Connector::close(&mut g);

        // 进程消失有延迟（10ms 轮询点 + 收尸），但必须**远早于** 30s 超时。
        let start = Instant::now();
        loop {
            // `/proc/<pid>` 存在即进程还在（真进程、不是自己造的句柄）。
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "close() 之后 git 子进程（pid {pid}）还活着——取消没生效"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

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
        Connector::open(
            &mut g,
            OpenReq::new(ws.to_string_lossy(), serde_json::json!({})),
        )
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

    /// `rev-parse` 的真实失败**必须传播**，不能被当成「空仓库没有 HEAD」。
    ///
    /// 这条守的是 `head_short` 的 `Err(e) => Err(e)` 臂：早先那里是
    /// `Err(_) => Ok(None)`，于是 git 挂了 / 权限不对 / 索引损坏都被显示成
    /// 「这个仓库还没有 HEAD」——一次真故障与一个正常状态长得一模一样。
    ///
    /// 构造：假 git 把 `status` 原样转给真 git（让 `parse_status` 拿到正常数据），
    /// 而 `rev-parse` 一律 `exit 128` 并说一句**不是**空仓库措辞的话。
    #[test]
    fn a_real_rev_parse_failure_is_not_reported_as_a_repo_without_head() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let bin = _base.path().join("fake-git");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             case \"$1\" in\n\
             rev-parse) echo 'fatal: something is genuinely wrong' >&2; exit 128 ;;\n\
             *) exec git \"$@\" ;;\n\
             esac\n",
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut g = Git::with_git_bin(&bin);
        Connector::open(
            &mut g,
            OpenReq::new(ws.to_string_lossy(), serde_json::json!({})),
        )
        .expect("open");

        let err = call(&mut g, serde_json::json!({"op": "status"}))
            .expect_err("rev-parse 真失败时必须报错，不能回 head:null");
        match err {
            ConnectorError::Failed { detail, .. } => assert!(
                detail.contains("genuinely wrong"),
                "detail 要带上 rev-parse 的真实原因，实际 {detail:?}"
            ),
            other => panic!("要落到 Failed，实际 {other:?}"),
        }
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

        // `v0.6.0` 加了第四个 op `rev`：它**不在**上面那份黑名单里，必须能过
        // `parse`。这条正向断言是黑名单与白名单的对偶守卫——将来若有人为了
        // 「收紧只读面」把 `rev` 也塞进那个循环，这里立刻红，而不是让本用例
        // 静默继续绿下去。
        assert!(
            GitOp::parse("rev").is_ok(),
            "\"rev\" 是 v0.6.0 起的合法 op，不得落进未知 op 名单"
        );
        // 反过来也守一条：四个 op 的名字都还在白名单上。
        for op in ["status", "diff", "log", "rev"] {
            assert!(GitOp::parse(op).is_ok(), "{op:?} 必须是合法 op");
        }
    }

    /// `rev` 在正常仓库上指到 HEAD：完整 hash 是 40 位且等于真实 `rev-parse HEAD`，
    /// 短 hash 就是它的前 7 位，分支名与 detached 都对。
    ///
    /// 「40 位十六进制」这条不是凑数：`--short` 也回十六进制、也能被当成
    /// 「像个 hash」，只有长度与等值比较一起才能把两者分开（见突变矩阵）。
    #[test]
    fn rev_of_a_repo_points_at_head() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);

        let got = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev must succeed");

        let head = got["head"].as_str().expect("head must be a string");
        assert_eq!(head.len(), 40, "head 必须是完整 40 位，实际 {head:?}");
        assert!(
            head.chars().all(|c| c.is_ascii_hexdigit()),
            "head 必须是十六进制，实际 {head:?}"
        );

        // 与宿主机上直接跑的真值比：这是本用例唯一的独立来源，不是「读同一个
        // 字段再自比」。
        let want = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&ws)
                .output()
                .expect("run git")
                .stdout,
        )
        .expect("utf8");
        assert_eq!(head, want.trim(), "head 必须等于真实的 rev-parse HEAD");

        let short = got["short"].as_str().expect("short must be a string");
        assert_eq!(short, &head[..7], "短 hash 必须是完整 hash 的前 7 位");
        assert_eq!(got["branch"], serde_json::json!("main"));
        assert_eq!(got["detached"], serde_json::json!(false));
        assert_eq!(got["not_a_repo"], serde_json::json!(false));
        assert_eq!(got["op"], serde_json::json!("rev"));
    }

    /// 再提交一次后 `rev` 报的 hash 必须变——它读的是提交指针，不是缓存。
    #[test]
    fn rev_sees_a_new_commit_after_the_first() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);
        let s1 = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev 1")["head"]
            .as_str()
            .expect("head 1")
            .to_string();

        std::fs::write(ws.join("a.txt"), "two\n").expect("write");
        git_in(&ws, &["commit", "-aqm", "second commit"]);

        let s2 = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev 2")["head"]
            .as_str()
            .expect("head 2")
            .to_string();

        assert_ne!(s2, s1, "新提交之后 rev 必须报不同的 hash");
    }

    /// 非仓库目录：**正常回帧**说 `not_a_repo: true`，不是 `Err`。
    ///
    /// 回 `Err` 的话通道层只会往页面送一个错误码，detail 里那句
    /// `not a git repository` 到不了页面，于是页面分不清「不是仓库」和
    /// 「git 挂了」。
    #[test]
    fn rev_outside_a_repo_is_a_normal_frame_saying_so() {
        let (_base, ws) = sandbox();
        let mut g = open(&ws);

        let got =
            call(&mut g, serde_json::json!({"op": "rev"})).expect("非仓库必须回 Ok 帧，不是 Err");

        assert_eq!(got["not_a_repo"], serde_json::json!(true));
        assert_eq!(got["head"], serde_json::Value::Null);
        assert_eq!(got["short"], serde_json::Value::Null);
        assert_eq!(got["branch"], serde_json::Value::Null);
        assert_eq!(got["detached"], serde_json::json!(false));
    }

    /// 空仓库（`git init` 但没有提交）：回帧 `not_a_repo: false`、`head: null`。
    ///
    /// **与上一条并排写**：两帧的键集合相同，只有值不同——这正是「两帧键集合
    /// 必须逐字相同」这条纪律的可测形态。
    ///
    /// 这条同时是 `is_empty_repo` 判据增补的守卫：`rev-parse HEAD` 在空仓库上
    /// 说的是 `ambiguous argument 'HEAD': unknown revision`，不增补判据时
    /// `rev()` 会回 `Err`，本用例因此红。**不许**为了让它变绿而改成
    /// 「`rev` 也可以回 Err」——那等于把「空仓库」重新混成「读失败」。
    #[test]
    fn rev_of_a_repo_without_commits_has_no_head() {
        let (_base, ws) = sandbox();
        git_in(&ws, &["init", "-q", "-b", "main", "."]);
        let mut g = open(&ws);

        let got =
            call(&mut g, serde_json::json!({"op": "rev"})).expect("空仓库必须回 Ok 帧，不是 Err");

        assert_eq!(
            got["not_a_repo"],
            serde_json::json!(false),
            "空仓库是仓库，只是没有提交"
        );
        assert_eq!(got["head"], serde_json::Value::Null);
        assert_eq!(got["short"], serde_json::Value::Null);
    }

    /// `is_empty_repo` 的判据本身：空仓库的 stderr 匹配，非仓库的**不**匹配。
    ///
    /// 不经过 `rev()`：锁的是判据函数，避免将来有人为了「更稳」把它放宽成
    /// `d.contains("head")` 之类——那样「不是仓库」会被误吞成「空仓库」，
    /// 页面上从此显示「仓库还没有提交」而不是「这个目录不是 git 仓库」。
    ///
    /// **两处 `detail` 都按生产路径的真实形态构造**：`run_git` 走到
    /// `classify_failure(&stderr)`（`:232`）时，`not a git repository` 那条已被
    /// 摘成**不带 `fatal:` 前缀**的固定串，而空仓库那条保留原样的首行。
    /// 手写 `fatal: not a git repository` 会与生产形态不符，于是
    /// `is_not_a_repo` 的 `starts_with` 判据测不到真实输入。
    #[test]
    fn an_empty_repo_is_not_mistaken_for_a_broken_git() {
        let empty = ConnectorError::Failed {
            op: "rev-parse".to_string(),
            detail: "fatal: ambiguous argument 'HEAD': unknown revision or path not in the \
                     working tree."
                .to_string(),
        };
        assert!(
            is_empty_repo(&empty),
            "空仓库的 stderr 必须被 is_empty_repo 认出来"
        );

        // 生产形态：`classify_failure` 把 `not a git repository` 摘成固定串。
        let not_repo = ConnectorError::Failed {
            op: "rev-parse".to_string(),
            detail: classify_failure(
                "fatal: not a git repository (or any of the parent directories): .git",
            ),
        };
        assert_eq!(
            not_repo_detail(&not_repo),
            "not a git repository",
            "classify_failure 必须把这条摘成不带 fatal: 前缀的固定串"
        );
        assert!(
            !is_empty_repo(&not_repo),
            "「不是仓库」不得被 is_empty_repo 误吞"
        );
        // 两者互斥的正面证据：同一个错误只能落进一个分支。
        assert!(is_not_a_repo(&not_repo));
        assert!(!is_not_a_repo(&empty));
    }

    /// 取 `Failed` 的 detail（测试辅助，避免每处都写一遍 `match`）。
    fn not_repo_detail(e: &ConnectorError) -> &str {
        match e {
            ConnectorError::Failed { detail, .. } => detail,
            _ => "",
        }
    }

    /// detached HEAD：`detached: true`、`branch: null`，而 `head` 仍有值
    /// （detached 时那个 hash 是唯一的信息来源）。
    #[test]
    fn rev_in_detached_head_reports_detached() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        git_in(&ws, &["checkout", "-q", "--detach", "HEAD"]);
        let mut g = open(&ws);

        let got = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev");

        assert_eq!(got["detached"], serde_json::json!(true));
        assert_eq!(got["branch"], serde_json::Value::Null);
        let head = got["head"].as_str().expect("detached 时 head 必须有值");
        assert_eq!(head.len(), 40);
    }

    /// 三个回帧（有 HEAD / 非仓库 / 空仓库）的**键集合逐字相同**。
    #[test]
    fn the_three_frames_carry_the_same_keys() {
        fn keys(v: &serde_json::Value) -> Vec<String> {
            let mut k: Vec<String> = v
                .as_object()
                .expect("payload must be an object")
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        }

        // ① 有 HEAD
        let (_b1, ws1) = sandbox();
        repo_with_commit(&ws1);
        let k1 = keys(&call(&mut open(&ws1), serde_json::json!({"op": "rev"})).expect("rev"));

        // ② 非仓库
        let (_b2, ws2) = sandbox();
        let k2 = keys(&call(&mut open(&ws2), serde_json::json!({"op": "rev"})).expect("rev"));

        // ③ 空仓库
        let (_b3, ws3) = sandbox();
        git_in(&ws3, &["init", "-q", "-b", "main", "."]);
        let k3 = keys(&call(&mut open(&ws3), serde_json::json!({"op": "rev"})).expect("rev"));

        assert_eq!(k1, k2, "有 HEAD 与「不是仓库」两帧的键集合必须相同");
        assert_eq!(k1, k3, "有 HEAD 与「空仓库」两帧的键集合必须相同");
        assert_eq!(
            k1,
            vec![
                "branch".to_string(),
                "detached".to_string(),
                "head".to_string(),
                "not_a_repo".to_string(),
                "op".to_string(),
                "short".to_string(),
            ],
            "键集合本身也要钉住，否则三帧一起多出一键时上面两条照样绿"
        );
    }

    /// `rev` 回答的是**提交指针**，不是工作区状态：有未提交改动时 hash 不变。
    #[test]
    fn rev_does_not_read_the_index_or_the_worktree() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        let mut g = open(&ws);
        let before = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev before")["head"]
            .as_str()
            .expect("head before")
            .to_string();

        // 未提交的改动：改了已跟踪文件 + 加一个未跟踪文件。
        std::fs::write(ws.join("a.txt"), "dirty\n").expect("write");
        std::fs::write(ws.join("new.txt"), "untracked\n").expect("write");

        let after = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev after")["head"]
            .as_str()
            .expect("head after")
            .to_string();

        assert_eq!(before, after, "工作区脏不脏不影响 rev 报的提交指针");
    }

    /// 短 hash 来自**同一个**完整 hash，不是第二次子进程。
    ///
    /// 用按参数分支的假 git 让两条命令给出**不同的**值：若实现改回「再跑一次
    /// `rev-parse --short`」，`short` 会变成 `$FAKE_SHORT`，本用例红。
    ///
    /// **脚本必须按 `$2` 分支**——一个对任何参数都回同一值的假 git 会让
    /// `short` 与「第二次调用的输出」恰好相等，于是用例恒绿（同义复述）。
    #[test]
    fn short_is_derived_from_the_same_hash_not_a_second_call() {
        let (_base, ws) = sandbox();
        // 两个前 7 位不同的 40 位十六进制串。
        const FAKE_FULL: &str = "1111111222222222333333334444444455555555";
        const FAKE_SHORT: &str = "9999999888888888777777776666666655555555";
        assert_ne!(&FAKE_FULL[..7], &FAKE_SHORT[..7], "前提：两者前 7 位不同");

        let script = _base.path().join("fake-git.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 case \"$2\" in\n\
                 \x20 HEAD) printf '%s\\n' \"{FAKE_FULL}\" ;;\n\
                 \x20 --short) printf '%s\\n' \"{FAKE_SHORT}\" ;;\n\
                 \x20 --abbrev-ref) printf '%s\\n' \"main\" ;;\n\
                 \x20 *) exit 1 ;;\n\
                 esac\n"
            ),
        )
        .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let mut g = Git::with_git_bin(&script);
        Connector::open(
            &mut g,
            OpenReq::new(ws.to_string_lossy(), serde_json::json!({})),
        )
        .expect("open");

        let got = call(&mut g, serde_json::json!({"op": "rev"})).expect("rev");

        assert_eq!(
            got["head"], FAKE_FULL,
            "假 git 的 HEAD 分支没被调用，本用例没有验证任何东西"
        );
        assert_eq!(
            got["short"],
            &FAKE_FULL[..7],
            "短 hash 必须由同一个完整 hash 截得"
        );
        assert_ne!(
            got["short"],
            &FAKE_SHORT[..7],
            "短 hash 不得来自第二次 rev-parse --short 调用"
        );
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
        let err =
            Connector::open(&mut g, OpenReq::new("", serde_json::json!({}))).expect_err("缺根要拒");
        assert!(
            matches!(err, ConnectorError::WorkspaceRoot { .. }),
            "{err:?}"
        );

        let (_base, ws) = sandbox();
        let file = ws.join("f.txt");
        std::fs::write(&file, "x").expect("write");
        let mut g = Git::new();
        let err = Connector::open(
            &mut g,
            OpenReq::new(file.to_string_lossy(), serde_json::json!({})),
        )
        .expect_err("文件不是目录");
        assert!(
            matches!(err, ConnectorError::WorkspaceRoot { .. }),
            "{err:?}"
        );
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
        assert!(
            unified.contains("truncated"),
            "要留下截断说明: {}",
            &unified[..80.min(unified.len())]
        );
        // 截断后的整帧仍要在协议上限内，否则 Hub 会拒帧并断开设备连接。
        let frame = serde_json::to_vec(&got).expect("serialize");
        assert!(
            frame.len() <= protocol::frame::MAX_FRAME_BYTES,
            "截断后仍超帧上限: {} > {}",
            frame.len(),
            protocol::frame::MAX_FRAME_BYTES
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

    /// 重命名：`-z` 下旧路径是**下一条**记录，另放进 `orig` 字段。
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
        assert_eq!(f["path"], "b.txt", "`path` 要是那条真实路径: {got}");
        assert_eq!(f["orig"], "a.txt", "旧路径另放 `orig`: {got}");
    }

    /// 拿 `status` 给的 `path` **原样**去问 `diff`，必须问得出内容。
    ///
    /// 这条是「 renaming 过的文件点不开」那个 bug 的可证伪断言：曾经 `path`
    /// 被拼成 `b.txt <- a.txt`，页面把它原样回传，`git diff -- "b.txt <- a.txt"`
    /// 因路径不存在返回空串，而页面把空串显示成「这个文件没有可显示的改动」——
    /// 结果是**重命名过的文件一律点不开，且理由被错报成「没有改动」**。
    /// 只断言拼接形状测不出这个 bug（那版测试是全绿的）。
    #[test]
    fn the_path_from_status_is_usable_as_a_diff_argument() {
        let (_base, ws) = sandbox();
        repo_with_commit(&ws);
        // 先把内容**提交**进 HEAD：git 的 rename 检测比的是 HEAD 与索引的
        // 内容相似度。若不提交就改名，比的是「HEAD 里的 one」与「索引里的
        // 新内容」，相似度不足 → git 报 `D a.txt` + `A b.txt` 两条记录，
        // 那时没有 `orig`，这条测试也就测不到想测的东西。
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(ws.join("a.txt"), &body).expect("write");
        git_in(&ws, &["add", "-A"]);
        git_in(&ws, &["commit", "-q", "-m", "grow"]);
        // 改名**并**改一行：真实场景里的重命名通常带着改动，纯改名反而
        // 会让 diff 里除了 rename 头没有内容行，断言强度不够。
        std::fs::rename(ws.join("a.txt"), ws.join("b.txt")).expect("rename");
        std::fs::write(ws.join("b.txt"), body.replace("line 7\n", "line seven\n")).expect("write");
        git_in(&ws, &["add", "-A"]);

        let mut g = open(&ws);
        let got = call(&mut g, serde_json::json!({"op": "status"})).expect("status");
        let files = got["files"].as_array().expect("files");
        assert_eq!(files.len(), 1, "前提：只有一条重命名记录: {got}");
        assert_eq!(
            files[0]["orig"].as_str(),
            Some("a.txt"),
            "前提：这条确实是 rename，否则测的是另一件事: {got}"
        );
        // 关键：`path` 与 `orig` 都从 status 的回帧**原样**取用，不做拆分或还原。
        let path = files[0]["path"].as_str().expect("path is str").to_string();
        let orig = files[0]["orig"].as_str().expect("orig is str").to_string();

        // 只给 `path` 的对照组：这条正是曾经的实现，它把一次改名显示成
        // 「新建文件」。留着它，是因为「为什么必须带 orig」这件事
        // 光看文档记不住，看一条红得起来的对照断言才记得住。
        let only_new = call(
            &mut g,
            serde_json::json!({"op": "diff", "path": path, "staged": true}),
        )
        .expect("diff");
        assert!(
            only_new["unified"]
                .as_str()
                .unwrap_or_default()
                .contains("new file"),
            "对照前提：只给新路径时 git 判成新建文件（rename 检测需要两条路径）: {only_new}"
        );

        let d = call(
            &mut g,
            serde_json::json!({"op": "diff", "path": path, "orig": orig, "staged": true}),
        )
        .expect("diff");
        let unified = d["unified"].as_str().unwrap_or_default();
        assert!(
            unified.contains("rename from") && unified.contains("rename to"),
            "带上 orig 的 diff 要问得出重命名内容，实际: {d}"
        );
        assert!(
            unified.contains("line seven"),
            "重命名带着的内容改动也要在 diff 里，实际: {d}"
        );
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
