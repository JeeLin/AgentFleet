//! 项目的**基线仓库**：中心式 / 镜像式项目在 Hub 上的那一份权威。
//!
//! # 这个模块的存在理由：把 `git` 关在里面
//!
//! `AGENTS.md` 的「git CLI 藏在接口后」是一条硬约束。业务层（任务、调度、看板）
//! 说的是「起点」「产出」「合并」这些**领域概念**，不是「branch」「commit」
//! 「merge --no-ff」。违反它的表现很具体：某个 handler 里直接拼 `git` 命令字符串，
//! 于是「合并被拒」这条业务规则变成了对某个 git 退出码的判断。
//!
//! 所以本文件是 `git` 这个词在 `crates/hub-core` 里**唯一**的去处：
//! 对外只有 [`BareRepo`] 的五个方法，签名里不出现任何 git 词汇。
//!
//! # 为什么用真 git 而不是自研对象库
//!
//! `docs/architecture/project-and-snapshot.md` §3 已论证：内容寻址对象库、
//! 打包文件、增量传输、三方合并，这四件事任何一件自己写都比调 git 更容易错，
//! 而它们**全都不是本产品要解决的问题**。本里程碑不重新论证。
//!
//! # 为什么用 `std::process::Command` 而不是 libgit2 绑定
//!
//! 与 `node-connector` 的 git connector 同一个理由：绑定会把「git 的版本」
//! 钉在编译期，而用户机器上的 git 是他自己装的。命令行是用户与环境之间
//! 唯一稳定的契约。
//!
//! 代价是**解析人类可读输出**，所以下面每次调用都只取 `--porcelain` /
//! `--format=…` 这类机器可读形态，并且在解析失败时给出可读的错误（而不是
//! 一个 `parse().unwrap()`）。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 基线仓库的一次失败。
///
/// **刻意不是 `CoreError` 的一个新变体**：这几种失败在业务层的处理方式各不相同
/// （「还没有提交」是正常状态、「合并冲突」要落成 409 让用户去解、「git 不在
/// PATH 上」是我们自己的环境问题），合并成一个 variant 会让调用方重新按字符串
/// 分支——那正是本模块要消灭的东西。
#[derive(Debug)]
pub enum RepoError {
    /// 仓库里还没有任何提交。**这不是错误状态**，是一个正常的起点：
    /// 刚 `init --bare` 出来的仓库就是它，调用方据此显示「还没有产出」。
    Empty,

    /// 合并时两边改了同一处。**必须在 Hub 侧停下**（`PRODUCT.md` §3.4）：
    /// 自动解冲突会让「谁的改动赢了」变成一个我们编出来的答案。
    Conflict { detail: String },

    /// 要找的起点 / 产出不在这里。带上用户输入的原文，好让人知道拼错在哪。
    NoSuchRevision { wanted: String },

    /// 拉取失败：设备侧拒绝、网络断、或者那个远程根本不存在。
    FetchFailed { detail: String },

    /// 调不起 `git`（不在 PATH 上、没有执行权限）。这是**环境**问题，
    /// 与「仓库坏了」要分开——前者是部署要修的，后者是数据要查的。
    NotAvailable { detail: String },

    /// 其它失败。带上是哪一步失败的，别让运维去猜。
    Failed { step: &'static str, detail: String },
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "这个项目还没有任何产出"),
            Self::Conflict { detail } => write!(f, "改动有冲突，需要人工决定保留哪边：{detail}"),
            Self::NoSuchRevision { wanted } => write!(f, "找不到起点 {wanted}"),
            Self::FetchFailed { detail } => write!(f, "从设备取产出失败：{detail}"),
            Self::NotAvailable { detail } => write!(f, "这台机器上调不起版本管理工具：{detail}"),
            Self::Failed { step, detail } => write!(f, "{step}失败：{detail}"),
        }
    }
}

impl std::error::Error for RepoError {}

pub type Result<T> = std::result::Result<T, RepoError>;

/// 一个项目的基线仓库（磁盘上的一个裸目录）。
///
/// 句柄本身很轻（只有一个路径），可以随时新建；**不对应任何数据库行**——
/// `project` 表只记「有没有基线」，目录存在与否才是真相。这样删库重建时
/// 不会出现「表里有、盘上没有」的半截状态。
#[derive(Debug, Clone)]
pub struct BareRepo {
    root: PathBuf,
}

impl BareRepo {
    /// 打开（必要时创建）`root` 处的基线仓库。
    ///
    /// **幂等**：已经存在就直接用。这点很重要——项目创建、Hub 重启、
    /// 并发的两个请求都可能走到这里，而「重复初始化」不该是一个错误。
    pub fn open_or_init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        // 先把目录建出来：`git init` 的 `current_dir` 必须是已存在的目录，
        // 否则拿到的报错是「No such file or directory」——它看起来像「git 没装」，
        // 而实际只是父目录还没建。这个错位会让排查走很远。
        std::fs::create_dir_all(&root).map_err(|e| RepoError::NotAvailable {
            // 建不了目录是权限 / 磁盘故障；把父目录换成普通文件即可稳定构造。
            detail: format!("建不了仓库目录 {}：{e}", root.display()),
        })?;
        let repo = Self { root };
        if repo.root.join("HEAD").is_file() {
            return Ok(repo);
        }
        // `--initial-branch=main` 而不是默认分支：`init.defaultBranch` 是
        // **用户级配置**，不钉死的话同一份代码在不同机器上会得到 main / master
        // 两个名字，而「回到主分支」这条规则要看名字。
        repo.git(
            &["init", "--bare", "--quiet", "--initial-branch=main"],
            None,
        )?;
        Ok(repo)
    }

    /// 磁盘上的位置。给需要跑 `disk_usage` 的调用方用。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 现在的**起点**（这个仓库当前指向的那次产出）。
    ///
    /// 空仓库返回 [`RepoError::Empty`] 而不是一个空串：`""` 会被调用方
    /// 当成一个合法的 rev 继续用下去，然后在一个更远的地方报「引用无效」。
    pub fn head(&self) -> Result<String> {
        self.rev_parse("HEAD")
    }

    /// 把 `wanted` 解析成一个完整的产出标识。
    ///
    /// 接受短 hash、分支名、`HEAD`——这些都是**用户会输入的东西**，
    /// 而 `rev-parse --verify` 会把它们统一成一个完整 hash。
    pub fn rev_parse(&self, wanted: &str) -> Result<String> {
        // `HEAD` 单独走一条：裸仓库刚 `init` 出来时 `HEAD` 指向
        // `refs/heads/main` 而那个引用还不存在，git 的报错与「拼错了分支名」
        // 一模一样。它是**正常的起点状态**，所以要判在 `has_any_ref` 之前——
        // 否则一次 `fetch`（它留下 `FETCH_HEAD` 但不建引用）之后，
        // 「还没有起点」会变成「找不到 HEAD」，语义就反了。
        if wanted == "HEAD"
            && self
                .git(&["rev-parse", "--verify", "--quiet", "HEAD"], None)
                .is_err()
        {
            return Err(RepoError::Empty);
        }

        // `^{commit}` 结尾要求它**真的是一个提交**：分支名与 tag 都指向提交，
        // 而 `HEAD` 在空仓库里指向一个不存在的引用。
        let out = self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{wanted}^{{commit}}"),
            ],
            None,
        );
        match out {
            Ok(s) => Ok(s),
            // `--quiet` 让 git 在解析失败时**不打印**、只返回非零。所以这里
            // 分不清「空仓库」与「拼错了」。用仓库里有没有产出区分：
            // 一条产出都没有 = 空仓库（正常状态），有产出 = 用户拼错了。
            Err(_) if self.has_any_output() => Err(RepoError::NoSuchRevision {
                wanted: wanted.to_string(),
            }),
            Err(_) => Err(RepoError::Empty),
        }
    }

    /// 从 `remote` 取回产出。`remote` 是**设备侧已经连上的那个仓库地址**，
    /// 由调用方决定（可能是本地路径，也可能是一个 URL）。
    ///
    /// 取回之后**不动 HEAD**：让取回与「采纳」分成两步。一次取回可能带来
    /// 十个产出，而「现在用哪个」是业务决定，不该由传输顺手做掉。
    pub fn fetch(&self, remote: &str, wanted: &str) -> Result<String> {
        self.git(&["fetch", "--quiet", "--no-tags", remote, wanted], None)
            .map_err(|e| match e {
                // 取不回来绝大多数时候是「设备那边没有这个引用」或「连不上」，
                // 两者对用户是同一句话：这次没取到，原因在这里。
                RepoError::Failed { detail, .. } => RepoError::FetchFailed { detail },
                other => other,
            })?;
        // fetch 之后那个提交在 `FETCH_HEAD` 里，取完整 hash 作为这次取回的产出。
        self.rev_parse("FETCH_HEAD")
    }

    /// 把 `from` 合进当前的起点，**不允许快进**。
    ///
    /// `--no-ff` 是刻意的：快进会让「这次合并」在历史里消失，而看板要能回答
    /// 「这个任务是什么时候合进来的」。一条额外的合并提交就是那个答案。
    ///
    /// 冲突时返回 [`RepoError::Conflict`] 并且**不留下任何东西**——临时工作区
    /// 整个丢掉。留着一个半合并的工作区，下一次操作会在一个谁也没预期的
    /// 状态上继续。
    ///
    /// # 为什么要有临时工作区
    ///
    /// `git merge` 要在工作区里做三方合并，而**裸仓库没有工作区**——直接调
    /// 会得到 `fatal: this operation must be run in a work tree`。所以这里
    /// 开一个临时目录、把起点检出过去、在那儿合并，成功后只把**结果**写回
    /// 基线仓库的 `refs/heads/main`，临时目录无论成败都删掉。
    ///
    /// 这不是绕路，是 Hub 侧合并本来的样子：基线仓库是**只存结果的**，
    /// 它不该因为有人点了「合并」而多出一个工作区。
    pub fn merge(&self, from: &str) -> Result<String> {
        // 先记下合并前的起点：失败时要回到这里。
        let before = self.head().map_err(|e| match e {
            // 一条产出都没有却要合并：说明调用方搞错了顺序。
            RepoError::Empty => RepoError::Failed {
                step: "合并",
                detail: "这个项目还没有起点，无从合并".to_string(),
            },
            // `head()` 余下的失败（例如起点不是提交）保持原分类：
            // 见 `merge_keeps_a_non_empty_head_failure_in_its_own_category`。
            other => other,
        })?;

        // `from` 先在这里解析一次：解析不了就在开工作区**之前**失败，
        // 免得为了一个拼错的传入值去建目录、检代码。
        let from = self.rev_parse(from)?;

        let work = tempfile::Builder::new()
            .prefix("af-merge-")
            .tempdir()
            .map_err(|e| RepoError::Failed {
                // 建不了临时目录是磁盘 / 权限故障；子进程夹具用坏掉的
                // `TMPDIR` 稳定走到这里（见下面同名用例）。
                step: "准备合并工作区",
                detail: e.to_string(),
            })?;

        // 检出起点到这个临时目录。`--work-tree` + `--git-dir` 让同一份对象库
        // 可以同时挂一个临时工作区，而不必 `clone`（clone 会复制整份历史）。
        self.git_in(work.path(), &["checkout", "--quiet", "--force", &before])?;

        match self.git_in(work.path(), &["merge", "--no-ff", "--no-edit", &from]) {
            Ok(_) => {}
            Err(RepoError::Failed { detail, .. }) if looks_like_conflict(&detail) => {
                // 临时目录随后被 Drop 掉，不需要 `merge --abort`：
                // 那一步在裸仓库里同样跑不起来，而且它保住的正是我们不要的东西。
                return Err(RepoError::Conflict { detail });
            }
            // 非冲突的合并失败（git 起不来、仓坏）保持原分类；
            // 假 `git` 夹具覆盖了这条臂，见下面同名用例。
            Err(other) => return Err(other),
        }

        // 合并成功：把结果的提交号读出来，写回基线仓库的主引用。
        let merged = self
            .git_in(work.path(), &["rev-parse", "HEAD"])
            .map_err(|e| RepoError::Failed {
                // 刚合并成功的工作区读不出 HEAD 意味着仓已经坏了；
                // 假 `git` 夹具让这一步稳定失败（见下面同名用例）。
                step: "读取合并结果",
                detail: e.to_string(),
            })?;

        self.git(&["update-ref", "refs/heads/main", &merged, &before], None)
            .map_err(|e| RepoError::Failed {
                // 裸仓库写不回引用是盘 / 权限故障；`.lock` 夹具覆盖。
                step: "记录合并结果",
                detail: format!("{e}（合并已在临时工作区完成，但没能写回：{merged}）"),
            })?;

        Ok(merged)
    }

    /// 仓库里有没有**任何**产出。用来把「还没有产出」与「引用不存在」分开。
    ///
    /// `FETCH_HEAD` 也算：取回之后那个提交就在仓库里了，只是还没有引用指向它。
    /// 那时「你拼错了名字」才是对的答案，而不是「这里什么都没有」。
    fn has_any_output(&self) -> bool {
        self.git(&["for-each-ref", "--count=1", "--format=%(refname)"], None)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
            || self.root.join("FETCH_HEAD").is_file()
    }

    /// 跑一次 git，返回**裁剪过的**标准输出。
    ///
    /// 所有对 git 的调用都从这里走：一处设定（工作目录、环境、错误翻译），
    /// 而不是每个方法各自拼一遍 `Command`。
    fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String> {
        let dir = cwd.unwrap_or(&self.root);
        self.run(dir, args, None)
    }

    /// 在某一个**工作区**里跑 git：对象库仍是这个基线仓库，工作区是 `work`。
    ///
    /// 只有合并需要它。分开成两个入口是因为 `--work-tree` 一旦出现在
    /// 普通调用里，git 会开始认为这个裸仓库有工作区，`fetch` / `update-ref`
    /// 这些动作的语义都会跟着变。
    fn git_in(&self, work: &Path, args: &[&str]) -> Result<String> {
        self.run(work, args, Some(work))
    }

    /// `run` 的唯一实现。`work_tree` 为 `None` 时是纯裸仓库调用。
    fn run(&self, dir: &Path, args: &[&str], work_tree: Option<&Path>) -> Result<String> {
        let mut cmd = Command::new("git");
        // 裸仓库没有工作区，`--git-dir` 是唯一可靠的说法：cwd 进来时
        // 不保证 `GIT_DIR` 环境变量干净（用户 shell 里可能带着别的值）。
        cmd.arg("--git-dir").arg(&self.root);
        if let Some(wt) = work_tree {
            // 显式 `--work-tree`：git 才不会把「基线仓库旁边的目录」当成
            // 工作区，也不会因为 `core.bare` 而拒绝合并。
            cmd.arg("--work-tree").arg(wt);
        }
        let out = cmd
            .args(args)
            .current_dir(dir)
            // 关掉交互式提示：没有终端时 git 会一直等一个永远不会来的输入，
            // 症状是请求挂死而不是报错（`GIT_TERMINAL_PROMPT=0` 是官方开关）。
            .env("GIT_TERMINAL_PROMPT", "0")
            // 临时工作区里的合并要能自己提交，而用户机器上未必配了全局身份；
            // 没配时 git 的报错是「Please tell me who you are」——看起来像
            // 代码问题，实则是环境。合并提交的作者写上本产品。
            .env("GIT_AUTHOR_NAME", "AgentFleet")
            .env("GIT_AUTHOR_EMAIL", "hub@agentfleet.invalid")
            .env("GIT_COMMITTER_NAME", "AgentFleet")
            .env("GIT_COMMITTER_EMAIL", "hub@agentfleet.invalid")
            .output()
            .map_err(|e| RepoError::NotAvailable {
                // 走到这里说明 git 二进制根本起不来（没装 / 不可执行）；
                // 子进程把 PATH 指空即可稳定构造，见 AF_GIT_MISSING 用例。
                detail: e.to_string(),
            })?;

        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
        // 两个流都取：**合并冲突的说明打在 stdout 上**（`CONFLICT (content): …`
        // 是进度输出），真正的原因（`Automatic merge failed`）打在 stderr 上。
        // 只看 stderr 会把冲突误判成「退出码 1」，用户就不知道该去选哪边赢了。
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => format!("退出码 {:?}", out.status.code()),
            (true, false) => stderr,
            // 「stderr 空、stdout 非空」的失败形态：真 git 几乎都写 stderr，
            // 用假 `git` 只写 stdout 的夹具覆盖这一档。
            (false, true) => stdout,
            (false, false) => format!("{stderr}\n{stdout}"),
        };
        Err(RepoError::Failed {
            step: step_name(args.first().copied().unwrap_or("")),
            detail,
        })
    }
}

/// 把 git 的动词翻译成用户看得懂的一步。**不对外暴露**：它只用来拼错误文案。
fn step_name(verb: &str) -> &'static str {
    match verb {
        "init" => "初始化仓库",
        "fetch" => "取回产出",
        "rev-parse" => "解析起点",
        "merge" => "合并",
        // 本模块只在 `has_any_output` 里调 `for-each-ref`，失败时也要有中文名。
        "for-each-ref" => "列举引用",
        // 兜底：加了新动词但忘了翻译时也不 panic。
        // `every_verb_has_a_human_step_name_including_the_fallback` 直接读这条臂。
        _ => "版本管理操作",
    }
}

/// 判断一次失败是不是「冲突」。
///
/// git 的退出码对冲突与其它失败都是 1，所以只能看输出。**这是本模块里唯一一处
/// 依赖 git 人类可读文案的地方**，因此把判据收窄到两条最稳的：
/// `CONFLICT`（`merge` 的进度输出）与 `Automatic merge failed`（合并失败的总句）。
/// 判错的后果是「把冲突报成通用失败」或反过来——前者让用户不知道该去解冲突，
/// 后者会让人去查一个并不存在的冲突。
fn looks_like_conflict(stderr: &str) -> bool {
    stderr.contains("CONFLICT") || stderr.contains("Automatic merge failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在裸仓库 `root` 里跑一次 `git`（带上被测模块同样的身份环境），
    /// 断言成功并返回裁剪过的 stdout。
    ///
    /// **它只用来搭台**：被测代码是 [`BareRepo`]，搭一个「有起点、有第二个
    /// 提交」的仓库只能靠 git。身份与 `GIT_CONFIG_*` 与本模块 `run` 里那几行
    /// 同源——用户机器上未必配了全局身份，不指定就会撞上「Please tell me
    /// who you are」，而那看起来像代码问题。
    fn git_out(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(root);
        cmd.args(args)
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "AgentFleet")
            .env("GIT_AUTHOR_EMAIL", "hub@agentfleet.invalid")
            .env("GIT_COMMITTER_NAME", "AgentFleet")
            .env("GIT_COMMITTER_EMAIL", "hub@agentfleet.invalid");
        let mut child = cmd.spawn().expect("spawn git");
        if let Some(bytes) = stdin {
            use std::io::Write as _;
            let mut pipe = child.stdin.take().expect("stdin");
            pipe.write_all(bytes).expect("write stdin");
        }
        let out = child.wait_with_output().expect("git output");
        // 先解码再断言：放在断言消息里的 `from_utf8_lossy` 只在失败时求值，
        // 一条永远通过的夹具断言覆盖不到那一行。把解码变成无条件的一步，
        // 既让失败信息可读，也让这段测试夹具自己是可覆盖的代码。
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "git {args:?} 失败：{stderr}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// 在 `repo` 里造一个提交（`files` 里的每个文件一个 blob）并把它设成
    /// 起点，返回提交号。
    ///
    /// 走 `hash-object` → `mktree` → `commit-tree` → `update-ref` 这条路，
    /// 而不是 `git add`：裸仓库没有工作区，临时索引那套要额外指向
    /// `--work-tree`，反而更容易写错。有父提交时自动接上，所以连续调用
    /// 得到的就是一条链。
    fn commit_on_main(repo: &BareRepo, msg: &str, files: &[(&str, &str)]) -> String {
        let mut tree_entries = String::new();
        for (name, content) in files {
            let blob = git_out(
                repo.root(),
                &["hash-object", "-w", "--stdin"],
                Some(content.as_bytes()),
            );
            tree_entries.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        let tree = git_out(repo.root(), &["mktree"], Some(tree_entries.as_bytes()));

        let parent = repo.head().ok();
        let mut tree_args: Vec<String> = vec!["commit-tree".into(), tree];
        if let Some(p) = parent {
            tree_args.push("-p".into());
            tree_args.push(p);
        }
        tree_args.push("-m".into());
        tree_args.push(msg.to_string());
        let refs: Vec<&str> = tree_args.iter().map(String::as_str).collect();
        let commit = git_out(repo.root(), &refs, None);
        git_out(
            repo.root(),
            &["update-ref", "refs/heads/main", &commit],
            None,
        );
        commit
    }

    /// `git` 起不来时，`run` 必须报 `NotAvailable`——那是部署问题，
    /// 不能混进「仓库坏了」那一类；`fetch` 也不许把这类错误折成
    /// `FetchFailed`（那会让人去查一个根本不存在的设备侧引用）。
    ///
    /// 改 `PATH` 是**进程级**的，而 cargo 的单测并行跑在同一个进程里
    /// （并发的 `BareRepo` 用例都要真的调 git）。所以这一条在子进程里跑：
    /// 子进程改自己的 `PATH`，父进程的环境表一点不动；子进程退出码就是断言。
    #[test]
    #[cfg(unix)] // 用 `sh` 改 PATH 后重跑同一条测试；Windows 的形态不同
    fn a_missing_git_stays_an_environment_problem_through_fetch() {
        use std::process::Command;

        let d = tempfile::tempdir().expect("tmp");
        let exe = std::env::current_exe().expect("test exe");
        let script = format!(
            "PATH=/nonexistent-af-git; export PATH; \
             exec {exe:?} repo::tests::a_missing_git_stays_an_environment_problem_through_fetch_child --exact"
        );
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("AF_GIT_MISSING", "1")
            .env("AF_GIT_ROOT", d.path().join("hub.git"))
            .output()
            .expect("spawn the child test process");
        // 先解码再断言：`assert!` 的消息只在失败时求值，把 `from_utf8_lossy`
        // 放在消息里会让那一行在一条通过的测试里永远覆盖不到。
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "子进程必须绿，stderr：{stderr}");
        // `--exact` 配错名字时 libtest 会「跑了 0 条用例」并以 0 退出——
        // 只看退出码会把一个什么都没测的子进程当成绿。这里钉住它真的跑了。
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{stdout}"
        );
    }

    /// 上一条的子进程本体：环境变量只在**这个**进程里种下。
    /// 普通运行时该变量不存在，于是本用例什么都不做（是 no-op，不是跳过）。
    #[test]
    #[cfg(unix)]
    fn a_missing_git_stays_an_environment_problem_through_fetch_child() {
        if std::env::var_os("AF_GIT_MISSING").is_none() {
            return;
        }
        let root = std::env::var_os("AF_GIT_ROOT").expect("AF_GIT_ROOT");
        let repo = BareRepo {
            root: PathBuf::from(&root),
        };
        let init = BareRepo::open_or_init(&repo.root).expect_err("git 不在 PATH 上必须失败");
        assert!(
            matches!(init, RepoError::NotAvailable { ref detail } if !detail.is_empty()),
            "git 起不来是环境问题，实际 {init:?}"
        );
        let err = repo
            .fetch("file:///nonexistent/remote", "main")
            .expect_err("fetch 也必须失败");
        assert!(
            matches!(err, RepoError::NotAvailable { .. }),
            "环境问题不许被折成 FetchFailed，实际 {err:?}"
        );
    }

    /// 目录建不出来时必须是 `NotAvailable`，且把那条路径带进 detail：
    /// 否则运维只看到「建不了」，不知道是哪一行配置指错了地方。
    /// 用一个**普通文件**当父目录，`create_dir_all` 必给 `NotADirectory`。
    #[test]
    fn open_or_init_reports_an_unusable_root_as_not_available() {
        let d = tempfile::tempdir().expect("tmp");
        let blocker = d.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("blocker");
        let root = blocker.join("hub.git");
        let err = BareRepo::open_or_init(&root).expect_err("建不出目录必须失败");
        assert!(
            matches!(
                &err,
                RepoError::NotAvailable { detail }
                    if detail.contains(root.to_string_lossy().as_ref())
            ),
            "建不出目录必须报 NotAvailable 且带上路径，实际 {err:?}"
        );
    }

    /// `step_name` 里那两条「平时走不到」的臂也是契约的一部分：
    /// `for-each-ref`（仓库有没有产出时会调）要有中文名，未知动词要有一句
    /// 不 panic 的兜底。直接读这两个函数，不必为了一行翻译去搭一个坏仓库。
    #[test]
    fn every_verb_has_a_human_step_name_including_the_fallback() {
        assert_eq!(step_name("init"), "初始化仓库");
        assert_eq!(step_name("fetch"), "取回产出");
        assert_eq!(step_name("rev-parse"), "解析起点");
        assert_eq!(step_name("merge"), "合并");
        assert_eq!(step_name("for-each-ref"), "列举引用");
        assert_eq!(step_name("brand-new-verb"), "版本管理操作");
        assert_eq!(step_name(""), "版本管理操作");
    }

    /// 失败文案的四档组合都要能拼出来：两个流都空、只有 stderr、只有
    /// stdout、两个都非空。这里直接调 `run` 的失败路径——用一个不存在的
    /// rev 让 git 失败（它写 stderr），再用 `run` 的入参组合补上另外三档
    /// 里能稳定构造的那些。
    #[test]
    fn a_failed_command_carries_the_readable_detail_and_its_step() {
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        let err = repo
            .run(
                d.path(),
                &["rev-parse", "--verify", "--quiet", "no-such"],
                None,
            )
            .expect_err("解析一个不存在的起点必须失败");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "解析起点" && !detail.is_empty()),
            "应为带上非空原因的「解析起点」失败，实际 {err:?}"
        );
    }

    /// `fetch` 的错误翻译只把 `Failed` 折成 `FetchFailed`，其它变体
    /// （例如 `NotAvailable`）必须原样传出去——那是环境问题，不是设备没给产出。
    #[test]
    fn fetch_keeps_a_non_git_failure_in_its_own_category() {
        // 直接构造翻译闭包里那条 `other => other` 的输入：一个空仓库上
        // `fetch` 不存在远程时拿到的就是 `Failed`，这里验的是反方向——
        // `has_any_output` 在没有任何引用且没有 FETCH_HEAD 时必须为假。
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        assert!(!repo.has_any_output(), "刚 init 的裸仓库不该被判成有产出");
        std::fs::write(repo.root().join("FETCH_HEAD"), b"deadbeef\n").expect("fetch head");
        assert!(
            repo.has_any_output(),
            "FETCH_HEAD 也算产出：取回之后拼错名字该报 NoSuchRevision"
        );
        let err = repo
            .rev_parse("typo-branch")
            .expect_err("有 FETCH_HEAD 就该报拼错");
        assert!(
            matches!(&err, RepoError::NoSuchRevision { wanted } if wanted.as_str() == "typo-branch"),
            "应报带上原文的 NoSuchRevision，实际 {err:?}"
        );
    }

    /// `open_or_init` 的幂等路径：**已经初始化过的目录直接返回**，不重跑
    /// `git init`。把 `objects/` 换成一个普通文件再打开第二次——重跑 init
    /// 会撞上「Not a directory」，而走短路则平安返回。用的是本模块自己的
    /// `git`，所以断言的是短路本身，不是 git 的容错。
    #[test]
    fn an_existing_repo_is_opened_without_reinitializing() {
        let d = tempfile::tempdir().expect("tmp");
        let root = d.path().join("hub.git");
        let repo = BareRepo::open_or_init(&root).expect("first open");
        std::fs::remove_dir_all(root.join("objects")).expect("remove objects");
        std::fs::write(root.join("objects"), b"blocker").expect("blocker");

        let again = BareRepo::open_or_init(&root).expect("existing repo must open");
        assert_eq!(again.root(), repo.root());
        assert!(
            root.join("objects").is_file(),
            "不该重跑 init 把夹具换成目录"
        );
    }

    /// `init` 本身失败时 `open_or_init` 必须把 `?` 的错误原样交出去，
    /// 不能当成「仓库已就绪」返回。
    ///
    /// 造法：建好目录、放一个 `objects` 普通文件，但**不放 `HEAD`**——
    /// 于是短路分支不命中，而 `git init --bare` 一定要往 `objects/` 里
    /// 建子目录，稳定报 `Not a directory`。这条同时覆盖了 `step_name("init")`
    /// 在真失败上的表现（它不只是个字符串表项）。
    #[test]
    fn a_failed_init_is_propagated_out_of_open_or_init() {
        let d = tempfile::tempdir().expect("tmp");
        let root = d.path().join("hub.git");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("objects"), b"blocker").expect("blocker");

        let err = BareRepo::open_or_init(&root).expect_err("init must fail");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "初始化仓库" && !detail.is_empty()),
            "init 失败应报带上非空原因的「初始化仓库」失败，实际 {err:?}"
        );
    }

    /// `head()` 只做一件事：把 `"HEAD"` 交给 `rev_parse`。所以一个
    /// **存在但不是提交** 的 HEAD 必须在这一层也表现为「解析失败」，
    /// 而不是被当成可用的起点字符串。
    ///
    /// 造法：`hash-object` 写一个 blob，再把 `refs/heads/main` 直接指到它
    /// （绕过 `update-ref` 的类型校验）。`rev-parse HEAD` 成功，
    /// 但 `HEAD^{commit}` 失败——正好走「有产出 → 拼错了」。
    #[test]
    fn head_rejects_a_revision_that_is_not_a_commit() {
        let d = tempfile::tempdir().expect("tmp");
        let root = d.path().join("hub.git");
        let repo = BareRepo::open_or_init(&root).expect("init");
        let blob = git_out(
            &root,
            &["hash-object", "-w", "--stdin"],
            Some(b"not a commit\n"),
        );
        std::fs::create_dir_all(root.join("refs").join("heads")).expect("refs dir");
        std::fs::write(
            root.join("refs").join("heads").join("main"),
            format!("{blob}\n"),
        )
        .expect("point HEAD at a blob");

        let err = repo.head().expect_err("blob 不是起点");
        assert!(
            matches!(err, RepoError::NoSuchRevision { ref wanted } if wanted == "HEAD"),
            "HEAD 指向非提交应报 NoSuchRevision，实际 {err:?}"
        );
        assert!(
            repo.run(
                d.path(),
                &["rev-parse", "--verify", "--quiet", "HEAD"],
                None
            )
            .is_ok(),
            "前置条件：rev-parse HEAD 本身要能成功"
        );
    }

    /// `merge` 在**没有起点**时要把 `Empty` 翻译成「合并」这一步的失败，
    /// 而不是把「还没有任何产出」直接抛给调用方当成正常状态。
    #[test]
    fn merging_into_an_empty_repo_reports_a_merge_step_failure() {
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        let err = repo.merge("anything").expect_err("空仓库无从合并");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "合并" && detail.contains("还没有起点")),
            "应报「合并」失败并说明还没有起点，实际 {err:?}"
        );
    }

    /// 传入的 `from` 解析不了，必须在**建临时工作区之前**就失败——
    /// 那条路径上的错误是 `rev_parse` 自己的，不该被包装成合并失败。
    #[test]
    fn a_bogus_merge_source_fails_before_the_worktree_is_created() {
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        let commit = commit_on_main(&repo, "base", &[]);
        assert_eq!(repo.head().expect("head"), commit);

        let err = repo
            .merge("no-such-output")
            .expect_err("拼错的产出不能进合并");
        assert!(
            matches!(err, RepoError::NoSuchRevision { ref wanted } if wanted == "no-such-output"),
            "应原样报 NoSuchRevision，实际 {err:?}"
        );
    }

    /// **合并成功之后写不回主引用**：错误必须带上「已经合完了、但没能写回」
    /// 这个事实，否则运维会以为合并白做了、再点一次。
    ///
    /// 造法：根提交 + 第二个提交，然后给 `refs/heads/main` 留一个 `.lock`，
    /// 于是最后的 `update-ref` 必定失败。合并本身（检出 → 三方合并）全在
    /// 临时工作区里，不受影响；HEAD 也仍可解析，正好只剩写回这一步失败。
    #[test]
    fn a_failed_write_back_says_the_merge_already_happened() {
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        let first = commit_on_main(&repo, "base", &[("a.txt", "one\n")]);
        let second = commit_on_main(&repo, "second", &[("b.txt", "two\n")]);
        assert_ne!(first, second);
        std::fs::write(
            repo.root().join("refs").join("heads").join("main.lock"),
            b"",
        )
        .expect("lock the ref");

        let err = repo.merge(&second).expect_err("写不回引用必须报错");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "记录合并结果" && detail.contains("没能写回")),
            "应报「记录合并结果」失败并说明没能写回，实际 {err:?}"
        );
        assert_eq!(
            repo.head().expect("head 不变"),
            second,
            "写不回去时主引用必须留在原处"
        );
    }

    /// 冲突判据钉在两条最稳的文案上，其余失败不许被误判成冲突。
    #[test]
    fn only_real_conflict_text_counts_as_a_conflict() {
        assert!(looks_like_conflict(
            "CONFLICT (content): Merge conflict in a.txt"
        ));
        assert!(looks_like_conflict("Automatic merge failed; fix conflicts"));
        assert!(!looks_like_conflict("fatal: not a git repository"));
        assert!(!looks_like_conflict(""));
    }

    /// `head()` 解析到一个非提交的起点时，`merge` 必须把这个失败**原样**
    /// 交出去，而不是把它改写成「还没有起点」或「合并步骤失败」。
    ///
    /// 造法沿用 [`head_rejects_a_revision_that_is_not_a_commit`]：把
    /// `refs/heads/main` 直接写成一个 blob。此时 `head()` 返回
    /// `NoSuchRevision`，正好走到 `map_err` 的 `other => other` 那一条。
    #[test]
    fn merge_keeps_a_non_empty_head_failure_in_its_own_category() {
        let d = tempfile::tempdir().expect("tmp");
        let root = d.path().join("hub.git");
        let repo = BareRepo::open_or_init(&root).expect("init");
        let blob = git_out(&root, &["hash-object", "-w", "--stdin"], Some(b"blob\n"));
        std::fs::create_dir_all(root.join("refs").join("heads")).expect("refs dir");
        std::fs::write(
            root.join("refs").join("heads").join("main"),
            format!("{blob}\n"),
        )
        .expect("point HEAD at a blob");

        let err = repo.merge("anything").expect_err("起点不是提交必须失败");
        assert!(
            matches!(&err, RepoError::NoSuchRevision { wanted } if wanted == "HEAD"),
            "非 Empty 的 head() 失败必须原样透传，实际 {err:?}"
        );
    }

    /// 临时工作区建不出来时，`merge` 要说清是「准备合并工作区」这一步
    /// 失败，而不是把 `tempfile` 的 `NotADirectory` 裸抛给调用方。
    ///
    /// `TMPDIR` / `PATH` 都是**进程级**环境，和别的并行用例会互相踩，
    /// 所以在子进程里做：父进程只 spawn 同一测试二进制的一条精确用例。
    #[test]
    #[cfg(unix)] // `TMPDIR` 指向普通文件 + 重跑测试二进制
    fn a_missing_worktree_is_reported_as_a_merge_step_failure() {
        let d = tempfile::tempdir().expect("tmp");
        let blocker = d.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("blocker");
        let root = d.path().join("hub.git");
        let repo = BareRepo::open_or_init(&root).expect("init");
        let commit = commit_on_main(&repo, "base", &[("a.txt", "one\n")]);
        assert_eq!(repo.head().expect("head"), commit);

        let exe = std::env::current_exe().expect("test exe");
        let out = Command::new(&exe)
            .args([
                "--exact",
                "repo::tests::a_missing_worktree_is_reported_as_a_merge_step_failure_child",
            ])
            .env("AF_BAD_TMPDIR", &blocker)
            .env("AF_MERGE_ROOT", &root)
            .env("AF_MERGE_FROM", &commit)
            .env("TMPDIR", &blocker)
            .output()
            .expect("spawn the child test process");
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "子进程必须绿：{child_stderr}");
        let child_stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            child_stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{child_stdout}"
        );
    }

    /// 上一条的子进程本体。没有哨兵变量时什么都不做（no-op，不是跳过）。
    #[test]
    #[cfg(unix)]
    fn a_missing_worktree_is_reported_as_a_merge_step_failure_child() {
        if std::env::var_os("AF_BAD_TMPDIR").is_none() {
            return;
        }
        let root = PathBuf::from(std::env::var_os("AF_MERGE_ROOT").expect("AF_MERGE_ROOT"));
        let from = std::env::var("AF_MERGE_FROM").expect("AF_MERGE_FROM");
        let repo = BareRepo { root };
        let err = repo.merge(&from).expect_err("建不出工作区必须失败");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "准备合并工作区" && !detail.is_empty()),
            "应报带上原因的「准备合并工作区」失败，实际 {err:?}"
        );
    }

    /// 在 `PATH` 前面放一个可控的假 `git`，让被测的 `run` 得到指定
    /// stdout/stderr 与退出码。其余子命令仍交给真 `git`，所以夹具能先
    /// 用真工具把仓库造好，再只让**被测调用**走假工具。
    ///
    /// 返回 `(父测试自己用的假 git 目录, 要注入子进程的 PATH)`；路径不含
    /// shell 元字符（来自 `tempfile`），命令用 `Command` 逐参数传入。
    #[cfg(unix)]
    fn install_fake_git(d: &Path, script: &str) -> (PathBuf, std::ffi::OsString) {
        let bin = d.join("fake-bin");
        std::fs::create_dir_all(&bin).expect("fake bin");
        let git = bin.join("git");
        std::fs::write(&git, script).expect("write fake git");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut paths = vec![bin];
        if let Some(old) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&old));
        }
        let path = std::env::join_paths(paths).expect("join PATH");
        (git, path)
    }

    /// 直接调 `run` 喂一个「**失败但只在 stdout 上写了原因**」的假 `git`：
    /// 错误文案必须把 stdout 当成原因，而不是只剩一句「退出码 7」。
    /// 这条同时覆盖 `(false, true)` 与「两个流都空」两档组合。
    #[test]
    #[cfg(unix)] // 假 git 是 `#!/bin/sh` 脚本
    fn a_failed_command_keeps_stdout_when_stderr_is_empty() {
        let d = tempfile::tempdir().expect("tmp");
        let (_git, path) =
            install_fake_git(d.path(), "#!/bin/sh\nprintf 'only on stdout\\n'\nexit 7\n");
        run_with_fake_path(
            "repo::tests::a_failed_command_keeps_stdout_when_stderr_is_empty_child",
            &path,
            &[("AF_FAKE_ROOT", &d.path().join("hub.git"))],
        );

        let (_git2, path2) = install_fake_git(d.path(), "#!/bin/sh\nexit 7\n");
        run_with_fake_path(
            "repo::tests::a_failed_command_reports_the_exit_code_when_both_streams_are_empty_child",
            &path2,
            &[("AF_FAKE_ROOT", &d.path().join("hub.git"))],
        );
    }

    /// 上一条第一段夹具的子进程本体：没有哨兵路径时什么都不做。
    #[test]
    #[cfg(unix)]
    fn a_failed_command_keeps_stdout_when_stderr_is_empty_child() {
        let Some(root) = std::env::var_os("AF_FAKE_ROOT") else {
            return;
        };
        let repo = BareRepo {
            root: PathBuf::from(root),
        };
        let cwd = repo.root.parent().expect("parent").to_path_buf();
        let err = repo
            .run(&cwd, &["rev-parse", "--verify", "x"], None)
            .expect_err("假 git 必须失败");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "解析起点" && detail.contains("only on stdout")),
            "stdout 上的原因要进 detail，实际 {err:?}"
        );
    }

    /// 上一条第二段夹具的子进程本体：没有哨兵路径时什么都不做。
    #[test]
    #[cfg(unix)]
    fn a_failed_command_reports_the_exit_code_when_both_streams_are_empty_child() {
        let Some(root) = std::env::var_os("AF_FAKE_ROOT") else {
            return;
        };
        let repo = BareRepo {
            root: PathBuf::from(root),
        };
        let cwd = repo.root.parent().expect("parent").to_path_buf();
        let err = repo
            .run(&cwd, &["fetch", "x"], None)
            .expect_err("静默失败也要有文案");
        assert!(
            matches!(&err, RepoError::Failed { detail, .. } if detail.contains("退出码")),
            "两个流都空时要报退出码，实际 {err:?}"
        );
    }

    /// 在**子进程**里换 `PATH` 再跑一段被测代码：改环境是进程级的，
    /// 而 `BareRepo::run` 用 `Command::new("git")`，没有注入点。父进程
    /// 只负责 spawn 同一测试二进制的一条精确用例，自己的环境一点不动。
    /// 返回子进程 stdout，并断言它真的跑完了一条用例。
    #[cfg(unix)]
    fn run_with_fake_path(test_name: &str, path: &std::ffi::OsStr, envs: &[(&str, &Path)]) {
        let mut cmd = Command::new(std::env::current_exe().expect("test exe"));
        cmd.args(["--exact", test_name]).env("PATH", path);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn the child test process");
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "子进程必须绿：{child_stderr}");
        let child_stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            child_stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{child_stdout}"
        );
    }

    /// 合并失败但输出里**没有**冲突文案时，必须原样报 `Failed`，不能
    /// 折成 `Conflict`（那会让人去解一个并不存在的冲突）。
    ///
    /// 造法：真 `git` 先造好起点与可合并产出，再在子进程里把 `PATH` 前
    /// 面挂一个假 `git`：只对 `merge` 打印 stdout+stderr 并失败，其它
    /// `checkout` / `rev-parse` 仍交给真 `git`。这样能稳定走到
    /// `Err(other) => return Err(other)` 与 `(false, false)` 文案那一档。
    #[test]
    #[cfg(unix)] // 假 git 是 `#!/bin/sh` 脚本
    fn a_non_conflict_merge_failure_is_not_reported_as_a_conflict() {
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        let first = commit_on_main(&repo, "base", &[("a.txt", "one\n")]);
        let second = commit_on_main(&repo, "second", &[("b.txt", "two\n")]);
        let script = "\
#!/bin/sh
for a in \"$@\"; do
  if [ \"$a\" = merge ]; then
    printf 'merge stdout detail\\n'
    printf 'not a conflict\\n' >&2
    exit 7
  fi
done
exec /usr/bin/git \"$@\"
";
        let (_git, path) = install_fake_git(d.path(), script);
        let exe = std::env::current_exe().expect("test exe");
        let out = Command::new(exe)
            .args([
                "--exact",
                "repo::tests::a_non_conflict_merge_failure_is_not_reported_as_a_conflict_child",
            ])
            .env("AF_FAKE_GIT", "1")
            .env("AF_MERGE_ROOT", repo.root())
            .env("AF_MERGE_FROM", &second)
            .env("PATH", &path)
            .output()
            .expect("spawn the child test process");
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "子进程必须绿：{child_stderr}");
        let child_stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            child_stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{child_stdout}"
        );
        assert_eq!(repo.head().expect("head 不变"), second);
        assert_eq!(first.len(), 40, "前置条件：夹具真的造出了完整 hash");
    }

    /// 上一条的子进程本体。没有哨兵变量时什么都不做（no-op，不是跳过）。
    #[test]
    #[cfg(unix)]
    fn a_non_conflict_merge_failure_is_not_reported_as_a_conflict_child() {
        if std::env::var_os("AF_FAKE_GIT").is_none() {
            return;
        }
        let root = PathBuf::from(std::env::var_os("AF_MERGE_ROOT").expect("AF_MERGE_ROOT"));
        let from = std::env::var("AF_MERGE_FROM").expect("AF_MERGE_FROM");
        let err = BareRepo { root }
            .merge(&from)
            .expect_err("非冲突的合并失败必须报错");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "合并" && detail.contains("not a conflict") && detail.contains("merge stdout detail")),
            "应原样报带上两个流的「合并」失败，实际 {err:?}"
        );
    }

    /// 合并成功后连 `rev-parse HEAD` 都读不出来：`merge` 要把这个失败
    /// 翻译成「读取合并结果」，而不是让一个裸的 git 错误冒出去。
    ///
    /// 造法：假 `git` 对 `merge` 返回成功（不真的改临时工作区），随后
    /// 对 `rev-parse` 返回失败。其余 `checkout` 交给真 `git`，于是能稳定
    /// 走到 `git_in(work, ["rev-parse", "HEAD"])` 的 `map_err`。
    #[test]
    #[cfg(unix)] // 假 git 是 `#!/bin/sh` 脚本
    fn a_failed_read_of_the_merge_result_names_the_merge_step() {
        let d = tempfile::tempdir().expect("tmp");
        let repo = BareRepo::open_or_init(d.path().join("hub.git")).expect("init");
        let second = commit_on_main(&repo, "second", &[("a.txt", "one\n")]);
        // `merge` 之前还要先跑 `head()` / `rev_parse(from)`，它们同样是
        // `rev-parse`。所以假 git 只拦**见过 merge 之后**的那个 `HEAD`：
        // 写一个标记文件，后面的 `rev-parse HEAD` 才失败。
        let marker = d.path().join("merge-happened");
        let script = "\
#!/bin/sh
for a in \"$@\"; do
  case \"$a\" in
    merge)
      : > \"$AF_FAKE_MARKER\"
      exit 0 ;;
    HEAD)
      if [ -f \"$AF_FAKE_MARKER\" ]; then
        printf 'HEAD unreadable\\n' >&2
        exit 9
      fi ;;
  esac
done
exec /usr/bin/git \"$@\"
";
        let (_git, path) = install_fake_git(d.path(), script);
        let exe = std::env::current_exe().expect("test exe");
        let out = Command::new(exe)
            .args([
                "--exact",
                "repo::tests::a_failed_read_of_the_merge_result_names_the_merge_step_child",
            ])
            .env("AF_FAKE_GIT_HEAD", "1")
            .env("AF_FAKE_MARKER", &marker)
            .env("AF_MERGE_ROOT", repo.root())
            .env("AF_MERGE_FROM", &second)
            .env("PATH", &path)
            .output()
            .expect("spawn the child test process");
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "子进程必须绿：{child_stderr}");
        let child_stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            child_stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{child_stdout}"
        );
        assert_eq!(repo.head().expect("head 不变"), second);
    }

    /// 上一条的子进程本体。没有哨兵变量时什么都不做（no-op，不是跳过）。
    #[test]
    #[cfg(unix)]
    fn a_failed_read_of_the_merge_result_names_the_merge_step_child() {
        if std::env::var_os("AF_FAKE_GIT_HEAD").is_none() {
            return;
        }
        let root = PathBuf::from(std::env::var_os("AF_MERGE_ROOT").expect("AF_MERGE_ROOT"));
        let from = std::env::var("AF_MERGE_FROM").expect("AF_MERGE_FROM");
        let err = BareRepo { root }
            .merge(&from)
            .expect_err("读不出合并结果必须报错");
        assert!(
            matches!(&err, RepoError::Failed { step, detail } if *step == "读取合并结果" && detail.contains("HEAD unreadable")),
            "应报带上原因的「读取合并结果」失败，实际 {err:?}"
        );
    }
}
