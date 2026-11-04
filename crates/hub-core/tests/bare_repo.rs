//! 子任务 3「Hub 侧裸仓库」的测试。
//!
//! 这一层要证的**不是 git 能不能用**，而是三件更容易做丢的事：
//!
//! 1. **空仓库是一个正常状态，不是崩溃点。** 新建的项目就是一个刚 `init` 的
//!    空仓库，所以「还没有任何产出」这条路径每一次创建都会走到。它要是 panic
//!    或者返回一个空串，症状会是「新项目一建出来首页就 500」。
//! 2. **冲突要可区分。** 合并失败与冲突在 git 那边都是退出码 1，如果接口把
//!    它们揉成同一个错误，上层就只能报「合并失败」——而用户需要知道的是
//!    「要你去选哪边赢」。而且冲突之后仓库必须**回到合并之前**，不能留一个
//!    半合并的工作区给下一次操作。
//! 3. **`git` 这个词不出接口。** 业务层说的是「起点 / 产出 / 合并」。这条是
//!    最容易被无意破坏的：加一个 `run_git()` 方法就破功，而它能编译、能跑、
//!    没有任何测试会红——所以下面有一条用例直接读源码来钉。
//!
//! 用例依赖本机 `git`。项目已经把它当作运行前提（`node-connector` 的 git
//! connector 同样如此），所以这里不做「没有 git 就跳过」——那会让这条门禁在
//! 真正需要它的机器上静默失效。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use hub_core::repo::{BareRepo, RepoError};

/// 在 `dir` 里跑一次 git 并断言成功。
///
/// 测试里可以随便调 git：**被测的是接口，不是「测试也不能碰 git」**。
/// 而且造一个「有产出的源仓库」只能靠 git。
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        // 测试环境里可能没有任何全局 git 身份；提交会因此失败，而失败信息
        // 是「Please tell me who you are」——看起来像代码问题。就地指定。
        .env("GIT_AUTHOR_NAME", "af-test")
        .env("GIT_AUTHOR_EMAIL", "af-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "af-test")
        .env("GIT_COMMITTER_EMAIL", "af-test@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("调不起 git");
    assert!(
        out.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 造一个带一次提交的普通仓库，返回其路径。要用它当「设备侧」的远程。
///
/// 用 `file://` 而不是裸路径：裸路径会让 git 走本地快速路径，允许取「无引用」
/// 之类在真实传输里不成立的东西。用 `file://` 走的才是真传输。
fn source_repo(dir: &Path, file: &str, content: &str) -> String {
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "--quiet", "-m", "first"]);
    format!("file://{}", dir.display())
}

/// 测试标准 1：空仓库的 `rev_parse` 返回约定的「无提交」错误，**不 panic**。
///
/// 这不是一个边角情况：每次新建项目都会走到。返回空串也不行——
/// `""` 会被当成合法 rev 传到下游，然后在离这里很远的地方报「引用无效」。
#[test]
fn a_fresh_repo_reports_empty_rather_than_panicking() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();

    match repo.head() {
        Err(RepoError::Empty) => {}
        other => panic!("空仓库的起点必须是 RepoError::Empty，实际是 {other:?}"),
    }
    // 任意一个具体名字也一样：没有引用 = 还没有任何产出，
    // 而不是「你拼错了」——那个区分见下一条用例。
    match repo.rev_parse("whatever") {
        Err(RepoError::Empty) => {}
        other => panic!("空仓库里解析任何名字都该是 Empty，实际是 {other:?}"),
    }
}

/// `open_or_init` 必须幂等：项目创建、Hub 重启、并发请求都会走到这里，
/// 重复初始化若报错，症状是「重启之后所有项目都打不开」。
#[test]
fn open_or_init_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("hub.git");
    let a = BareRepo::open_or_init(&root).unwrap();
    let b = BareRepo::open_or_init(&root).unwrap();
    assert_eq!(a.root(), b.root(), "两次打开必须指向同一个目录");
    assert!(root.join("HEAD").is_file(), "裸仓库的标志文件要真的在");
}

/// 取回之后，起点能被解析成一个完整产出。
#[test]
fn fetch_then_the_result_is_a_resolvable_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("device");
    std::fs::create_dir_all(&src_dir).unwrap();
    let remote = source_repo(&src_dir, "a.txt", "hello\n");

    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();
    let got = repo.fetch(&remote, "main").unwrap();

    assert_eq!(got.len(), 40, "完整 hash 是 40 个字符，实际是 {got:?}");
    assert_eq!(
        repo.rev_parse(&got).unwrap(),
        got,
        "取回来的产出必须能被再次解析到同一个值"
    );
    // 取回**不动 HEAD**：一次取回可能带来十个产出，「现在用哪个」是业务决定。
    match repo.head() {
        Err(RepoError::Empty) => {}
        other => panic!("fetch 不该顺手改 HEAD，实际是 {other:?}"),
    }
}

/// 解析不到但仓库不空 → `NoSuchRevision`，带上用户输入的原文。
///
/// 与 `Empty` 分开的意义：前者是「你拼错了」，后者是「这里本来就还没有」。
/// 合成一个错误码，页面上就只能说一句含糊的「取不到」。
#[test]
fn a_bogus_revision_on_a_non_empty_repo_is_no_such_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("device");
    std::fs::create_dir_all(&src_dir).unwrap();
    let remote = source_repo(&src_dir, "a.txt", "hello\n");

    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();
    repo.fetch(&remote, "main").unwrap();

    match repo.rev_parse("no-such-branch") {
        Err(RepoError::NoSuchRevision { wanted }) => assert_eq!(wanted, "no-such-branch"),
        other => panic!("应报 NoSuchRevision，实际是 {other:?}"),
    }
}

/// 取一个不存在的远程 → `FetchFailed`（而不是笼统的 `Failed`）。
#[test]
fn fetching_from_a_missing_remote_is_a_fetch_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();

    match repo.fetch("file:///nonexistent/repo", "main") {
        Err(RepoError::FetchFailed { .. }) => {}
        other => panic!("应报 FetchFailed，实际是 {other:?}"),
    }
}

/// 每个动词都有自己的中文步骤名，且**不是**那句兜底。
///
/// `step_name` 的兜底分支（`_ => "版本管理操作"`）存在是为了「加了新动词但忘了
/// 加翻译」时不至于 panic——但它一旦真的被走到，用户看到的就是一句什么也没说的
/// 错误。所以这里把已知的三个动词逐个走一遍失败路径，证明它们各自翻译到位。
///
/// 三个一起测而不是分三条：它们共用同一段「拼错误文案」的代码，分开写的三条
/// 用例会各自搭一遍仓库，跑得慢而覆盖的东西完全一样。
#[test]
fn every_known_verb_gets_its_own_readable_step_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();

    // fetch：远程不存在。
    let fetch_msg = match repo.fetch("file:///nonexistent/repo", "main") {
        Err(e) => format!("{e:?}"),
        Ok(_) => panic!("取一个不存在的远程不该成功"),
    };
    // rev-parse：解析一个非 commit 的东西（这里是空字符串）。
    let rev_msg = match repo.rev_parse("refs/heads/no-such-branch") {
        Err(e) => format!("{e:?}"),
        Ok(_) => panic!("解析一个不存在的引用不该成功"),
    };
    // merge：仓库里一个起点都没有。
    let merge_msg = match repo.merge("refs/heads/no-such-branch") {
        Err(e) => format!("{e:?}"),
        Ok(_) => panic!("合并一个不存在的起点不该成功"),
    };

    for (verb, msg) in [
        ("fetch", fetch_msg),
        ("rev-parse", rev_msg),
        ("merge", merge_msg),
    ] {
        assert!(
            !msg.contains("版本管理操作"),
            "{verb} 的错误文案落到了兜底那句「版本管理操作」上，用户看不出是哪一步：{msg}"
        );
    }
}

/// 测试标准 2：合并冲突时返回**可区分**的 `Conflict`，且仓库回到合并之前。
///
/// 退出码在 git 那边都是 1，所以「能区分」靠的是我们对输出的判读。
/// 判错的后果分两头：报成通用失败，用户不知道要自己去选哪边赢；反过来报成
/// 冲突，用户会去找一个并不存在的冲突。
#[test]
fn a_conflicting_merge_is_reported_as_conflict_and_rolled_back() {
    let tmp = tempfile::tempdir().unwrap();

    // 设备 A 与设备 B 从同一个起点各自改同一行。
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(src.join("same.txt"), "base\n").unwrap();
    git(&src, &["add", "same.txt"]);
    git(&src, &["commit", "--quiet", "-m", "base"]);

    let device_a = format!("file://{}", src.display());

    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();
    repo.fetch(&device_a, "main").unwrap();
    // 把取回来的那次产出设成起点。接入真实项目的「采纳」动作归后续里程碑，
    // 这里只需要一个非空的起点，好让合并有落点。
    set_head_to_fetched(&repo);

    // A 侧改动先进来。
    std::fs::write(src.join("same.txt"), "from a\n").unwrap();
    git(&src, &["commit", "--quiet", "-am", "a"]);
    let a_rev = repo.fetch(&device_a, "main").unwrap();
    repo.merge(&a_rev).unwrap();
    let after_a = repo.head().unwrap();

    // B 侧从 base 改同一行 → 与 A 冲突。
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    git(&b, &["clone", "--quiet", &device_a, "."]);
    git(&b, &["reset", "--quiet", "--hard", "HEAD~1"]);
    std::fs::write(b.join("same.txt"), "from b\n").unwrap();
    git(&b, &["commit", "--quiet", "-am", "b"]);
    let b_rev = repo
        .fetch(&format!("file://{}", b.display()), "main")
        .unwrap();

    match repo.merge(&b_rev) {
        Err(RepoError::Conflict { detail }) => {
            assert!(!detail.is_empty(), "冲突要带上可读的原因，不能是空串");
        }
        other => panic!("应报 Conflict，实际是 {other:?}"),
    }
    assert_eq!(
        repo.head().unwrap(),
        after_a,
        "冲突之后必须回到合并之前——留一个半合并的工作区，下一次操作会在谁也没预期的状态上继续"
    );
}

/// 把 `origin/main` 指到刚取回来那个产出，并让起点指向它。
///
/// 这是测试里的搭台动作：真实代码里「采纳」是业务层的一步，本里程碑不实现。
fn set_head_to_fetched(repo: &BareRepo) {
    let dir = repo.root().display().to_string();
    let fetched = repo.rev_parse("FETCH_HEAD").unwrap();
    git(
        repo.root(),
        &["--git-dir", &dir, "update-ref", "refs/heads/main", &fetched],
    );
}

/// 没有冲突的合并要成功，并且**留下一条合并提交**（`--no-ff`）。
///
/// 快进会让「这次合并」在历史里消失，而看板要能回答「这个任务是什么时候
/// 合进来的」。所以这条用例断言父子关系，而不只是「HEAD 变了」。
#[test]
fn a_clean_merge_lands_and_keeps_a_merge_commit() {
    let tmp = tempfile::tempdir().unwrap();

    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let remote = source_repo(&src, "a.txt", "base\n");

    let repo = BareRepo::open_or_init(tmp.path().join("hub.git")).unwrap();
    let base = repo.fetch(&remote, "main").unwrap();
    set_head_to_fetched(&repo);

    // 另一条产出：加一个文件，不碰 a.txt。
    std::fs::write(src.join("b.txt"), "more\n").unwrap();
    git(&src, &["add", "b.txt"]);
    git(&src, &["commit", "--quiet", "-m", "second"]);
    let second = repo.fetch(&remote, "main").unwrap();

    let merged = repo.merge(&second).unwrap();
    assert_ne!(merged, second, "--no-ff 要产生新提交，不能是快进");
    assert_ne!(merged, base);

    let parents = git(
        repo.root(),
        &[
            "--git-dir",
            &repo.root().display().to_string(),
            "log",
            "-1",
            "--format=%P",
        ],
    );
    assert_eq!(
        parents.split_whitespace().count(),
        2,
        "合并提交必须有两个父提交，实际是 {parents:?}"
    );
}

/// 测试标准 3：`git` 这个词不出接口。
///
/// 这条用例读源码。看起来笨，但它是**唯一**能拦住「顺手加一个 `run_git()`」
/// 的办法——那种改动能编译、能跑、其它用例全绿，而它一旦进来，下一个 handler
/// 就会开始拼命令字符串。
#[test]
fn the_public_interface_never_says_git() {
    let src = include_str!("../src/repo.rs");

    // 只看 `pub` 那些行的签名部分：实现内部当然要出现 `git`，
    // 那是它存在的理由。
    for line in src.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("pub ") {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        assert!(
            !lower.contains("git"),
            "公开接口里出现了 git 字样：{trimmed}\n\
             业务层说的是「起点 / 产出 / 合并」，不是 git 的词汇。"
        );
    }
}

/// 错误文案是给用户看的，不能出现 git 的术语。
///
/// 这条比上一条更实际：`FetchFailed` 之类会原样进错误体，用户读到的是它。
#[test]
fn error_messages_are_written_for_people_not_for_git() {
    let samples = [
        RepoError::Empty,
        RepoError::Conflict { detail: "x".into() },
        RepoError::NoSuchRevision {
            wanted: "abc".into(),
        },
        RepoError::FetchFailed { detail: "x".into() },
        RepoError::NotAvailable { detail: "x".into() },
        RepoError::Failed {
            step: "合并",
            detail: "x".into(),
        },
    ];
    for e in samples {
        let text = e.to_string();
        let lower = text.to_ascii_lowercase();
        for word in ["git", "commit", "branch", "merge", "hash", "rev"] {
            assert!(!lower.contains(word), "错误文案里出现了 {word}：{text}");
        }
    }
}
