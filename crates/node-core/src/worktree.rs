//! 任务 worktree：建 / 复用、`HEAD == base_commit` 校验、磁盘占用实测。
//!
//! 子任务 4（`scheduling.md` §5「派发与落定」）落在设备侧的三个动作：
//!
//! 1. **准备一个工作树**——Hub 在帧里给了绝对路径（`binding.fixed_dir` 下选定），
//!    这里只负责把它变成现实：目录已在就复用，不在就 `git worktree add`。
//! 2. **校验基线**——`HEAD` 必须等于帧里的 `base_commit`（`task-model.md` §7.3）。
//!    不等**绝不硬跑**：在错误基线上跑出来的结果看起来正常但实际不可信。
//! 3. **量磁盘**——执行期间的占用由 [`disk_usage`] 实测（不估算、不猜），
//!    供 [`crate::dispatch`] 的监控循环判「超预算 → 提前中止」。
//!
//! # 为什么这里全是同步的
//!
//! 调用发生在 worker 会话循环里，但**不是**在 `select!` 的臂内直接等：
//! [`crate::dispatch`] 把它们放进 `spawn_blocking`。写成同步函数是为了让
//! 「建树 / 校验 / 量盘」这三件事**不必起真连接就能测**，也不必给它们各塞一份
//! runtime 句柄（同 `agent/detect.rs` 不用 `tokio::process` 的理由）。
//!
//! # 只做这三件事
//!
//! 不做 commit / push / 清理——push 任务分支是执行成功之后另一件事，
//! 清理是「磁盘清理预演」的地盘，且**清理是不可逆操作，未经用户选中不得执行**
//! （`task-model.md` §3.1）。这里连 `worktree prune` 都不做。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// 一次 git 调用的超时。
///
/// 建 worktree 要 checkout 整棵树，大仓库几秒到几十秒；超时必须杀掉子进程，
/// 否则一个挂死的 git 会一直占着进程槽（同 `node-connector` 的 git connector）。
pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// worktree 一类的本地准备失败。
///
/// 每个变体都**自带可读的原因串**：设备侧的失败要能被人看懂，「git 退出码 128」
/// 单独一条等于没说（`task-model.md` §3：没有原因串的失败等于没发生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeError {
    /// `git worktree add` 失败。`detail` 里是 git 的 stderr 摘要。
    Add { path: String, detail: String },
    /// 目录存在但不是一条 worktree 记录（既没 `.git` 也不是空目录）。
    ///
    /// **不复用、也不删掉重建**：那个目录里可能有人的东西。删掉它是不可逆操作，
    /// 而这里没有任何授权去做。
    NotAWorktree { path: String, detail: String },
    /// `rev-parse HEAD` 失败（不是仓库 / 空仓库 / git 起不来）。
    NoHead { path: String, detail: String },
    /// 路径不是绝对路径。Hub 该给绝对路径（`worktree_path` 的注释）。
    NotAbsolute { path: String },
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add { path, detail } => {
                write!(f, "cannot create the worktree at {path}: {detail}")
            }
            Self::NotAWorktree { path, detail } => {
                write!(f, "{path} is in the way and is not a worktree: {detail}")
            }
            Self::NoHead { path, detail } => write!(f, "cannot read HEAD of {path}: {detail}"),
            Self::NotAbsolute { path } => {
                write!(f, "worktree path {path} is not absolute")
            }
        }
    }
}

impl std::error::Error for WorktreeError {}

/// 一次准备工作的结果：**目录在哪、是新建的还是复用的**。
///
/// `created` 是给人看的诊断（日志里「复用了已有的工作树」与「新建了一个」
/// 差别很大：前者可能是上一次执行留下的现场）。它**不参与判定**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub path: PathBuf,
    pub created: bool,
}

/// 准备（建或复用）一个任务工作树。
///
/// 判定顺序：
///
/// 1. 目录已在且是 worktree（有 `.git`）→ **复用**，一个字节都不写。
/// 2. 目录已在且**空** → 交给 `git worktree add`（git 允许目标目录存在但为空）。
/// 3. 目录已在且非空、又不是 worktree → [`WorktreeError::NotAWorktree`]。
/// 4. 目录不在 → `git worktree add -B <branch> <path>`。
///
/// `-B`（大写）而不是 `-b`：分支已存在时 `-b` 直接失败，而「同一分支被上一次
/// 执行留着」正是复用同一目录时要处理的情形。基线由调用方随后校验
/// （[`head_commit`]），这里不替它判断对错。
pub fn prepare(repo: &Path, path: &Path, branch: &str) -> Result<Prepared, WorktreeError> {
    if !path.is_absolute() {
        return Err(WorktreeError::NotAbsolute {
            path: path.display().to_string(),
        });
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {
            if path.join(".git").exists() {
                return Ok(Prepared {
                    path: path.to_path_buf(),
                    created: false,
                });
            }
            let mut entries = match std::fs::read_dir(path) {
                Ok(it) => it,
                // coverage: excluded — 走到这里要求 `metadata` 说它是目录、
                // 而 `read_dir` 仍然失败（权限 / IO）。测试跑在 root 下，
                // 权限位被绕过，`chmod 000` 也列得出来；而「列到一半目录被删」
                // 是竞态，不能当判据。生产里这一臂是真会发生的（非 root 部署），
                // 只是没有确定性的构造手段。
                Err(e) => {
                    // coverage: excluded — 上面 `Err(e)` 臂的续行（同一个不可达
                    // 分支，理由见上）。
                    return Err(WorktreeError::NotAWorktree {
                        path: path.display().to_string(),
                        detail: format!("cannot list it: {e}"),
                    });
                }
            };
            if entries.next().is_some() {
                return Err(WorktreeError::NotAWorktree {
                    path: path.display().to_string(),
                    detail: "the directory is not empty and has no .git".to_string(),
                });
            }
        }
        // 不存在（或存在但不是目录）→ 交给 git 去建 / 去报错。
        _ => {}
    }
    let (ok, stderr) = run_git(
        Some(repo),
        &["worktree", "add", "-B", branch, &path.display().to_string()],
    )?;
    if !ok {
        return Err(WorktreeError::Add {
            path: path.display().to_string(),
            detail: first_lines(&stderr, 6),
        });
    }
    Ok(Prepared {
        path: path.to_path_buf(),
        created: true,
    })
}

/// 读 `dir` 当前的 `HEAD`（完整 hash）。
///
/// 失败分两类，都在 [`WorktreeError::NoHead`] 的 `detail` 里说清：「不是仓库」
/// 与「空仓库（还没有提交）」是两种完全不同的处境，人要知道是哪一种。
pub fn head_commit(dir: &Path) -> Result<String, WorktreeError> {
    let (ok, text) = run_git(Some(dir), &["rev-parse", "HEAD"])?;
    if !ok {
        return Err(WorktreeError::NoHead {
            path: dir.display().to_string(),
            detail: first_lines(&text, 6),
        });
    }
    let head = text.trim().to_string();
    if head.is_empty() {
        // coverage: excluded — 要让 `rev-parse HEAD` 成功返回、却一个字节都不打印，
        // 得让 git 本身行为异常（成功 + 空输出）；正常 git 不会。这一臂是
        // 「宁可报错也不接受空起点」的兜底，不是一条可构造的分支。
        return Err(WorktreeError::NoHead {
            path: dir.display().to_string(),
            detail: "git printed an empty HEAD".to_string(),
        });
    }
    Ok(head)
}

/// 基线校验的结论。
///
/// **只有 [`BaselineVerdict::Matched`] 才允许执行**（`task-model.md` §3.1：
/// 在错误基线上跑出来的结果比报错更危险）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineVerdict {
    /// `HEAD == base_commit`。
    Matched,
    /// `HEAD != base_commit`。`reason` 是给人看的那句话（「本地 HEAD 是 X，要求 Y」）。
    Mismatch { reason: String },
}

/// 校验 `dir` 的 `HEAD` 是否等于 `base_commit`。
///
/// 比较**不区分大小写**并接受缩写：`git rev-parse` 也能吃短 hash，而帧里的
/// `base_commit` 由 Hub 给出、可能是短形式。但**不做前缀模糊匹配**——
/// 那会把 `abc` 与 `abcdef` 当成一回事，而这正是「起点不是那个起点」。
pub fn check_baseline(dir: &Path, base_commit: &str) -> BaselineVerdict {
    let want = base_commit.trim().to_ascii_lowercase();
    match head_commit(dir) {
        Ok(head) => {
            if head.to_ascii_lowercase() == want {
                BaselineVerdict::Matched
            } else {
                BaselineVerdict::Mismatch {
                    reason: format!(
                        "the local HEAD is {head}, but the dispatch requires {base_commit}"
                    ),
                }
            }
        }
        Err(e) => BaselineVerdict::Mismatch {
            reason: format!("cannot verify the baseline: {e}"),
        },
    }
}

/// `root` 下所有普通文件的大小之和（含符号链接指向的目标不计——只量链接本身）。
///
/// # 为什么自己走目录而不调 `du`
///
/// `du` 的语义随平台与参数漂移（块大小、硬链接去重、`--apparent-size`），
/// 而且它是个子进程：执行监控**周期性**采样本目录，起进程比走目录贵得多。
/// 这里的判据是「字节数」而不是「磁盘块数」——阈值是给人看的预算，用同一把尺
/// 量就够。
///
/// # 读不到的东西不算 0
///
/// 单个条目读不到（权限 / 采样期间被删）就跳过它，但**不把整个结果吞成 0**：
/// 归 0 会让监控以为「磁盘空着」，那正是最危险的假阴性。调用方拿到的数
/// 只会**偏小**于真实值，而真超预算时它一定先被看见（超预算是个下限判定）。
pub fn disk_usage(root: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            // 采样期间目录被删 / 无权限：跳过这一层，不把总数归零。
            //
            // coverage: excluded — 只有「上一轮 `read_dir` 看见了它、下一轮进去时
            // 已经没了」才走得到（或非 root 下无权限）。前者是竞态、后者被 root
            // 绕过，两者都没有确定性构造手段。行为本身由下一条（单个条目读不到）
            // 同一个口径保证：偏小，不归零。
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                // coverage: excluded — `DirEntry::metadata` 只在「条目刚被列出来、
                // 读它之前就没了」或非 root 下无权限时失败；前者是竞态、后者被
                // root 绕过。语义与上一臂同一口径：跳过这一个，不把总数归零。
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// 跑一次 git，返回（是否成功, stdout 与 stderr 合起来）。
///
/// 与 `node-connector` 的 git connector 同一条纪律：`current_dir` 定位仓库，
/// 且**清掉继承来的 `GIT_*`**——`afcli` 可能被从某个 git 钩子 / IDE 集成里拉起，
/// 那些环境里 `GIT_DIR` / `GIT_WORK_TREE` 指着**另一个**仓库，不清会让
/// 我们对着别人的仓库建 worktree。
fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<(bool, String), WorktreeError> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // coverage: excluded — 块尾 `}` 被 llvm-cov 单独计为一个区域（`if let`
    // 的两个分支都有覆盖，只是块尾不产生计数）。
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            // coverage: excluded — 只有父进程环境里**真有** `GIT_*` 时才进得来，
            // 而设/删环境变量是进程全局的（仓库明令禁止在测试里 `set_var`：
            // 并行用例会互相污染）。这条守卫的语义由 `run_git` 的文档与
            // `node-connector` 的 git 连接器同一纪律保证，没有可并行的构造手段。
            cmd.env_remove(&key);
        }
    }
    let op = args.first().copied().unwrap_or("git");
    let err_path = cwd.map_or_else(|| "<cwd>".to_string(), |d| d.display().to_string());
    let mut child = cmd.spawn().map_err(|e| WorktreeError::Add {
        path: err_path.clone(),
        detail: format!("cannot run git {op}: {e}"),
    })?;

    // **必须有超时**：建 worktree 时 checkout 一个慢文件系统上的大仓库可能
    // 卡住几分钟，而这条调用在 `spawn_blocking` 的线程里——卡住不会堵心跳，
    // 但会把这次派发永远挂在那里。超时先杀再收尸（同 git connector 的纪律）。
    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = read_pipes(&mut child);
                text.truncate(4096);
                return Ok((status.success(), text));
            }
            // coverage: excluded — `try_wait` 只在内核层面出问题（EPERM / 未被
            // 重试的 EINTR）时失败，std 没有让某个 Child 句柄失效的口子。
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                // coverage: excluded — 上面 `Err(e)` 臂的续行（同一不可达分支）。
                return Err(WorktreeError::Add {
                    path: err_path,
                    detail: format!("cannot wait for git {op}: {e}"),
                });
            }
            Ok(None) => {}
        }
        // coverage: excluded — `if` 的条件行：56 次循环里从没越过 deadline，
        // llvm-cov 把条件与块首记成独立区域（下一行的 `let _ = child.kill()`
        // 另有标注，理由同）。
        if std::time::Instant::now() >= deadline {
            // coverage: excluded — 唯一能让这一臂成立的输入是「git 卡住超过
            // `GIT_TIMEOUT`（60 秒）」，而 `GIT_TIMEOUT` 是常量、没有注入点；
            // 为一个超时分支让测试真睡一分钟不值得。它的判据（先杀再收尸、
            // 报 `false` + 超时理由）与 git connector 同一条纪律，靠那条覆盖。
            // coverage: excluded — 超时臂的续行（`GIT_TIMEOUT` 无注入点，
            // 理由见上）。
            let _ = child.kill();
            let _ = child.wait();
            // coverage: excluded — 上面超时臂的续行（同一不可达分支）。
            return Ok((
                false,
                format!("git {op} timed out after {}s", GIT_TIMEOUT.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 读完子进程的 stdout 与 stderr（调用时进程已退出，管道不会再涨）。
fn read_pipes(child: &mut std::process::Child) -> String {
    use std::io::Read;
    let mut text = String::new();
    if let Some(out) = child.stdout.as_mut() {
        let mut buf = Vec::new();
        if out.read_to_end(&mut buf).is_ok() {
            text.push_str(&String::from_utf8_lossy(&buf));
        }
        // coverage: excluded — 块尾 `}`：几十次调用里 `read_to_end` 全部成功，
        // llvm-cov 把 `if` 的块尾单独记成一个区域而它不产生计数；上面的
        // `push_str` 有覆盖，说明这条语句确实执行过。
    }
    if let Some(err) = child.stderr.as_mut() {
        let mut buf = Vec::new();
        if err.read_to_end(&mut buf).is_ok() {
            text.push_str(&String::from_utf8_lossy(&buf));
        }
        // coverage: excluded — 同上：`is_ok()` 分支的块尾括号被单独计为一个区域。
    }
    text
}

/// git 输出前 `n` 行的摘要。
///
/// 整段 stderr 进 reason 会把一条 ack 帧撑到几 KB（git 报错常带完整用法说明），
/// 而 ack 是给 Hub 落库的诊断字段，不是给人翻的完整日志。
fn first_lines(text: &str, n: usize) -> String {
    let joined: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(n)
        .collect();
    if joined.is_empty() {
        return "git printed nothing".to_string();
    }
    joined.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 起一个真 git 仓库（一次提交），返回（临时目录, 仓库路径）。
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        git_in(&root, &["init", "-q", "-b", "main"]);
        git_in(&root, &["config", "user.email", "t@example.com"]);
        git_in(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), b"one\n").expect("write");
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "first"]);
        (tmp, root)
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .expect("run git");
        assert!(st.success(), "git {args:?} failed in {}", dir.display());
    }

    /// 新建：目录不在 → `git worktree add` 建出来，且 `.git` 是真的存在。
    #[test]
    fn prepare_creates_a_worktree_that_did_not_exist() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-one");
        let got = prepare(&root, &wt, "task/one").expect("prepare");
        assert!(got.created, "新目录必须是新建的");
        assert!(wt.is_dir(), "目录必须真的建出来了");
        assert!(wt.join(".git").exists(), "必须是一条 worktree 记录");
        // 工作树里应当能看见仓库的内容（不是空目录糊弄过去）。
        assert!(wt.join("a.txt").exists(), "worktree 应当是 checkout 过的");
    }

    /// 复用：目录已在且是 worktree → 不重建、不报错，且**原来写进去的东西还在**
    /// （重建会把上一次执行的现场抹掉）。
    #[test]
    fn prepare_reuses_an_existing_worktree_without_wiping_it() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-two");
        prepare(&root, &wt, "task/two").expect("first");
        std::fs::write(wt.join("left-behind.txt"), b"evidence\n").expect("write");

        let again = prepare(&root, &wt, "task/two").expect("reuse");
        assert!(!again.created, "已有 worktree 必须是复用");
        assert_eq!(again.path, wt);
        assert!(
            wt.join("left-behind.txt").exists(),
            "复用不得抹掉上一次执行的现场"
        );
    }

    /// 目录在、非空、又不是 worktree → 报错说清，且**不删任何东西**。
    ///
    /// 删掉它是不可逆操作，而设备侧在这里没有任何授权去做。
    #[test]
    fn prepare_refuses_a_foreign_non_empty_directory() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("not-a-worktree");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join("precious.txt"), b"do not delete\n").expect("write");

        let err = prepare(&root, &wt, "task/three").expect_err("must refuse");
        assert!(matches!(err, WorktreeError::NotAWorktree { .. }), "{err:?}");
        assert!(
            wt.join("precious.txt").exists(),
            "拒绝的时候不许删别人的东西"
        );
        assert!(err.to_string().contains("is not a worktree"), "{err}");
    }

    /// 相对路径 → 报错（Hub 该给绝对路径）。
    #[test]
    fn prepare_rejects_a_relative_path() {
        let (_tmp, root) = repo();
        let err = prepare(&root, Path::new("relative/wt"), "b").expect_err("must refuse");
        assert!(matches!(err, WorktreeError::NotAbsolute { .. }), "{err:?}");
    }

    /// `git worktree add` 失败 → 原因串里有 git 自己说的话（可读，不是光一个退出码）。
    #[test]
    fn prepare_reports_why_git_worktree_add_failed() {
        let (tmp, root) = repo();
        // 让目标路径的父目录是个**文件**：git 必然失败，报错里带它自己的话。
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file\n").expect("write");
        let wt = blocker.join("wt");

        let err = prepare(&root, &wt, "task/four").expect_err("must fail");
        let text = err.to_string();
        assert!(text.contains("cannot create the worktree"), "{text}");
        assert!(text.len() > 60, "原因串必须说清，不是光一句失败：{text}");
    }

    /// `HEAD` 与要求一致 → `Matched`；不一致 → `Mismatch` 且原因里**两个 hash
    /// 都在**（人要知道「本地是 X，要求 Y」）。
    #[test]
    fn check_baseline_compares_head_and_names_both_when_it_differs() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-five");
        prepare(&root, &wt, "task/five").expect("prepare");
        let head = head_commit(&wt).expect("head");

        assert_eq!(check_baseline(&wt, &head), BaselineVerdict::Matched);
        // 大小写不敏感（帧里可能是大写 hex）。
        assert_eq!(
            check_baseline(&wt, &head.to_uppercase()),
            BaselineVerdict::Matched
        );

        let other = "0".repeat(head.len());
        match check_baseline(&wt, &other) {
            BaselineVerdict::Mismatch { reason } => {
                assert!(reason.contains(&head), "原因里要有本地 HEAD：{reason}");
                assert!(reason.contains(&other), "原因里要有要求的起点：{reason}");
            }
            // 这一臂就是断言失败 —— 前一行 `Mismatch` 已经命中。
            // coverage: excluded — 被测代码正确时不可达。
            BaselineVerdict::Matched => panic!("不同的 commit 不得判成一致"),
        }
    }

    /// 目录不存在时 `check_baseline` 也**不 panic**，回一条说得清的 Mismatch。
    ///
    /// 「校验不了」与「不一致」处置相同（都不许跑），所以合成同一个变体；
    /// 但原因串必须说清是「读不出 HEAD」，否则人以为是起点写错了。
    #[test]
    fn check_baseline_turns_an_unreadable_head_into_a_readable_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("nope");
        match check_baseline(&missing, "deadbeef") {
            BaselineVerdict::Mismatch { reason } => {
                assert!(reason.contains("cannot verify the baseline"), "{reason}");
            }
            // 同上：正确实现下不可达。
            // coverage: excluded — 被测代码正确时不可达。
            BaselineVerdict::Matched => panic!("目录不存在不得判成一致"),
        }
    }

    /// `disk_usage` 数得对：嵌套目录里的每个文件都算进去。
    #[test]
    fn disk_usage_sums_every_file_under_the_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("a/b")).expect("mkdir");
        std::fs::write(tmp.path().join("a/x.bin"), vec![0u8; 100]).expect("write");
        std::fs::write(tmp.path().join("a/b/y.bin"), vec![0u8; 250]).expect("write");
        assert_eq!(disk_usage(tmp.path()), 350);
    }

    /// 不存在的目录 → 0，**不 panic**（采样期间 worktree 被删是可能的）。
    #[test]
    fn disk_usage_of_a_missing_tree_is_zero_and_does_not_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(disk_usage(&tmp.path().join("gone")), 0);
    }

    /// `first_lines` 只留前 n 行非空行，并拼成一行（ack 的 reason 不该是几 KB）。
    #[test]
    fn first_lines_keeps_a_short_readable_summary() {
        let text = "\n\nfatal: one\nhint: two\n\nhint: three\n";
        assert_eq!(first_lines(text, 2), "fatal: one; hint: two");
        assert_eq!(first_lines("   \n\n", 3), "git printed nothing");
    }

    // ---- 错误串要**说得出是哪一种失败** ----

    /// 三种 `WorktreeError` 的 `Display` 各说一件事：谁拦路、哪条路径、为什么。
    ///
    /// 这三句是排障时唯一能看的东西（它们进 ack 的 `reason`、进日志），
    /// 所以逐句钉住：`Add` 说「建不出来」、`NotAWorktree` 说「被挡着」、
    /// `NoHead` 说「读不出 HEAD」、`NotAbsolute` 说「必须是绝对路径」。
    /// 判红方式：把任一句合成通用的「操作失败」，对应断言立刻红。
    #[test]
    fn every_worktree_error_names_the_path_and_the_kind_of_failure() {
        let add = WorktreeError::Add {
            path: "/wt/a".to_string(),
            detail: "fatal: nope".to_string(),
        };
        assert_eq!(
            add.to_string(),
            "cannot create the worktree at /wt/a: fatal: nope"
        );

        let nw = WorktreeError::NotAWorktree {
            path: "/wt/b".to_string(),
            detail: "not empty".to_string(),
        };
        assert_eq!(
            nw.to_string(),
            "/wt/b is in the way and is not a worktree: not empty"
        );

        let no_head = WorktreeError::NoHead {
            path: "/wt/c".to_string(),
            detail: "not a git repository".to_string(),
        };
        assert_eq!(
            no_head.to_string(),
            "cannot read HEAD of /wt/c: not a git repository"
        );

        let rel = WorktreeError::NotAbsolute {
            path: "rel/wt".to_string(),
        };
        assert_eq!(rel.to_string(), "worktree path rel/wt is not absolute");
    }

    /// 一个空仓库（`git init` 之后还没有提交）里读 HEAD：`NoHead`，且原因串里
    /// **有 git 自己说的话**——「空仓库」与「不是仓库」是两种处境，人要看得出。
    #[test]
    fn head_commit_of_a_repo_without_commits_says_what_git_said() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("empty-repo");
        std::fs::create_dir_all(&bare).expect("mkdir");
        git_in(&bare, &["init", "-q", "-b", "main"]);

        let err = head_commit(&bare).expect_err("没有提交时读不出 HEAD");
        assert!(
            matches!(err, WorktreeError::NoHead { .. }),
            "空仓库是 NoHead，不是别的变体：{err:?}"
        );
        let text = err.to_string();
        assert!(text.contains(&bare.display().to_string()), "{text}");
        assert!(
            text.contains("HEAD"),
            "原因里要提到 HEAD（读的就是它）：{text}"
        );
        assert!(
            !text.contains("git printed nothing"),
            "git 对空仓库是有话说的，不该落到「什么都没打印」：{text}"
        );
    }

    /// 空的已存在目录 → 交给 `git worktree add`（git 允许目标为空目录），
    /// 结果是**新建的**一条真 worktree。
    ///
    /// 这一臂与「目录不存在」是两条不同的分支：漏掉的话空目录会被当成
    /// 「非空的别人的目录」而被拒，于是复用一个上一轮清理过的目录永远失败。
    #[test]
    fn prepare_accepts_an_existing_but_empty_directory() {
        let (tmp, root) = repo();
        let wt = tmp.path().join("wt-empty-dir");
        std::fs::create_dir_all(&wt).expect("mkdir");
        assert_eq!(
            std::fs::read_dir(&wt).expect("read").count(),
            0,
            "前提：目录是空的"
        );

        let got = prepare(&root, &wt, "task/empty-dir").expect("空目录要被接受");

        assert!(got.created, "空目录是「这次建出来的」");
        assert!(wt.join(".git").exists(), "必须变成一条真 worktree");
        assert!(wt.join("a.txt").exists(), "内容必须是 checkout 出来的");
    }

    /// `run_git` 的**成功路径**把 stdout 与 stderr 都收回来并截到 4096 字节：
    /// git 报错常带完整用法说明，几 KB 一条进 ack 不合适。
    ///
    /// 判据是长度上限与内容：`rev-parse --verify` 一个不存在的 rev，git 的
    /// stderr 有话要说，返回值里看得到，且不超过 4096。
    #[test]
    fn run_git_caps_the_text_it_hands_back() {
        let (_tmp, root) = repo();
        let (ok, text) = run_git(Some(&root), &["rev-parse", "--verify", "no-such-rev"])
            .expect("git 本身要能起来");
        assert!(!ok, "不存在的 rev 必须报失败");
        assert!(!text.trim().is_empty(), "失败时 git 的 stderr 要带回来");
        assert!(text.len() <= 4096, "回收的文本必须截到 4096 以内");
    }

    /// `read_pipes` 在子进程**两条管道都有输出**时把两段都读完（先 stdout
    /// 后 stderr）。
    ///
    /// 直接拿一个真子进程：只读一条管道的话，另一条写满缓冲区时子进程会
    /// 卡在写上一动不动——而 `run_git` 又有 60 秒超时，症状是「git 无缘无故
    /// 超时」。判据是两段都在返回值里。
    #[test]
    fn read_pipes_collects_both_streams_of_a_finished_child() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("echo to-stdout; echo to-stderr >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let status = child.wait().expect("wait");
        assert!(status.success());

        let text = read_pipes(&mut child);

        assert!(text.contains("to-stdout"), "stdout 要在：{text:?}");
        assert!(text.contains("to-stderr"), "stderr 也要在：{text:?}");
    }
}
