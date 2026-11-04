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
            // coverage: excluded — 建不了目录是权限 / 磁盘故障，
            // 要求把那个路径变成不可写才走得到。
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
            // coverage: excluded — `head()` 余下的失败只有 git 起不来。
            other => other,
        })?;

        // `from` 先在这里解析一次：解析不了就在开工作区**之前**失败，
        // 免得为了一个拼错的传入值去建目录、检代码。
        let from = self.rev_parse(from)?;

        let work = tempfile::Builder::new()
            .prefix("af-merge-")
            .tempdir()
            .map_err(|e| RepoError::Failed {
                // coverage: excluded — 建不了临时目录是磁盘 / 权限故障。
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
            // coverage: excluded — 非冲突的合并失败（git 起不来、仓坏）。
            Err(other) => return Err(other),
        }

        // 合并成功：把结果的提交号读出来，写回基线仓库的主引用。
        let merged = self
            .git_in(work.path(), &["rev-parse", "HEAD"])
            .map_err(|e| RepoError::Failed {
                // coverage: excluded — 刚合并成功的临时工作区读不出 HEAD，
                // 意味着仓已经坏了。
                step: "读取合并结果",
                detail: e.to_string(),
            })?;

        self.git(&["update-ref", "refs/heads/main", &merged, &before], None)
            .map_err(|e| RepoError::Failed {
                // coverage: excluded — 裸仓库写不回引用是盘 / 权限故障。
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
                // coverage: excluded — 走到这里说明 git 二进制根本起不来
                // （没装 / 不可执行），测试机上有 git，构造不出来。
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
            // coverage: excluded — 「stderr 空、stdout 非空」的失败形态。
            // 实测 git 的失败几乎都同时写 stderr；这一档是格式完整性兜底。
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
        // coverage: excluded — 本模块目前不调 `for-each-ref`。
        "for-each-ref" => "列举引用",
        // coverage: excluded — 兜底：加了新动词但忘了翻译时也不 panic。
        // `every_known_verb_gets_its_own_readable_step_name` 保证已知的三个
        // 动词都走不到这一行。
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
