//! afcli：node 对外的命令入口（run / log）。
//!
//! `run` 是唯一需要用户主动敲的接入命令（首次自动 enroll，见 `DECISIONS.md` D1）；
//! `log` 是排障入口——设备可能**根本没在跑**（supervisor 已退出、升级停在
//! `Failed`），此时要看的正是上一个进程留下的日志。
//!
//! 逻辑放在这里而不是 `main.rs`，是为了**可直接单测**，且让 `main.rs` 保持极薄、
//! 每条分支都能由「spawn 二进制 + 不同 argv」走到（覆盖率门禁要求 main.rs 100%）。
//!
//! 注意：**不要**把连接 / 进程逻辑挪进来——本文件只负责「解析 + 分派」。
//! supervisor 在 [`supervisor`]，worker 在 [`node_core::worker`]：连接能力属于
//! `node-core`（connector 只能被已认证连接使用），本 crate 只决定「起哪个角色」。

pub mod log_cmd;
pub mod runargs;
pub mod supervisor;

use std::io::Write;
use std::path::PathBuf;

/// 二进制名称。命令名必须是 `afcli`（与 Node.js 运行时 `node` 撞名才改的），
/// crate 名是 `node-cli`，二者都不要改。
pub const BIN_NAME: &str = "afcli";

/// `run --foreground`：文档化的**无操作别名**——`run` 本来就前台阻塞，
/// 加它只是让脚本里的意图更明显。不新开一条「前台模式」代码路径：
/// 那会与「永远不 daemon 化」（D1）变成两个必须各自维护的分支。
pub const FOREGROUND_FLAG: &str = "--foreground";

/// 明确拒绝的后台 flag：`afcli` 不 daemon 化，后台运行交给 systemd / Docker。
pub const BACKGROUND_FLAGS: [&str; 2] = ["--background", "--daemon"];

/// 用法文本。无参数 / 未知参数时打到 stderr。
pub const USAGE: &str = "\
afcli — AgentFleet node 命令

用法:
  afcli --version     打印版本并退出 0
  afcli --help        打印本帮助并退出 0
  afcli run [--foreground]   启动 supervisor（默认角色，前台阻塞）
  afcli log [-n N]           打印最近 N 行日志（默认 200）

`run` 永远在前台运行、不会 daemon 化（DECISIONS D1）；要脱离终端请用
systemd / Docker，不要加 --background 或 --daemon（会报错并给出用法）。
`--foreground` 是文档化的无操作别名：`run` 本来就是前台，加上它只让
脚本里的意图更明显。前台启动时 supervisor 与 worker 的日志行会同时回显
到控制台（stdout / stderr），并照常写进日志文件供 `afcli log` 读取。

`run --worker` 是 supervisor 拉起 worker 用的内部形态（DECISIONS D1），
不需要手工敲；手敲了也只会跑成 worker，不会变成 supervisor。

Hub 地址取环境变量 AFCLI_HUB_URL：写 host:port（如 117.72.123.218:9002）
即可自动补成 ws://host:port/ws；完整 ws:// 或 wss:// URL 也照常可用。
IPv6 请用方括号：[::1]:9002。缺省 ws://127.0.0.1:8080/ws。";

/// 解析 argv（不含程序名）。
///
/// 返回 [`ParseResult`]：要么命中一个已知子命令/flag，要么说明
/// 「需要打印用法并失败退出」。
/// 这里**不**碰 `std::env::args()`，参数由调用方传进来——便于单测，也让 `main.rs`
/// 只做「读 argv → 传进来 → 打印」三件事。
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> ParseResult {
    let all: Vec<String> = args.into_iter().collect();
    let mut it = all.iter();
    let Some(first) = it.next() else {
        return ParseResult::Usage;
    };
    match first.as_str() {
        "--version" | "-V" => ParseResult::Version,
        "--help" | "-h" => ParseResult::Help,
        "log" => {
            let rest: Vec<String> = it.cloned().collect();
            ParseResult::Log(rest)
        }
        "run" => {
            let rest: Vec<String> = it.cloned().collect();
            ParseResult::Run(rest)
        }
        _ => ParseResult::Usage,
    }
}

/// 这次 `run` 要以哪个角色执行。
///
/// **角色只由 argv 决定**（`DECISIONS.md` D1）：进 worker 角色的路径有且只有
/// 「`run` 后面含 `--worker`」这一条。第二个信息源（环境变量）会让
/// `ps` 里看不出这个进程是哪个角色，还会顺着继承传给孙进程。
pub enum Role {
    Supervisor,
    Worker,
}

/// 从 `run` 的余下参数里定角色。
pub fn role_of(run_args: &[String]) -> Role {
    if run_args.iter().any(|a| a == runargs::WORKER_FLAG) {
        Role::Worker
    } else {
        Role::Supervisor
    }
}

/// [`parse_args`] 的结果。
enum ParseResult {
    /// 命中 `--version`：打印版本。
    Version,
    /// 命中 `--help`：打印用法（成功退出）。
    Help,
    /// 命中 `log`：把余下参数交给 `log_cmd` 处理。
    Log(Vec<String>),
    /// 命中 `run`：把余下参数交给 supervisor / worker 分派。
    Run(Vec<String>),
    /// 无参数或未知参数：打印用法（失败退出）。
    Usage,
}

/// 跑一遍命令逻辑，返回进程退出码。
///
/// 把「副作用」交给调用方传入的 `out` / `err`（单测里换成内存缓冲），
/// `main.rs` 传 `stdout` / `stderr`。
pub fn run<I, W1, W2>(args: I, mut out: W1, mut err: W2) -> std::process::ExitCode
where
    I: IntoIterator<Item = String>,
    W1: std::io::Write,
    W2: std::io::Write,
{
    match parse_args(args) {
        ParseResult::Version => {
            drop(writeln!(out, "{} {}", BIN_NAME, env!("CARGO_PKG_VERSION")));
            std::process::ExitCode::SUCCESS
        }
        ParseResult::Help => {
            drop(writeln!(out, "{USAGE}"));
            std::process::ExitCode::SUCCESS
        }
        ParseResult::Log(rest) => {
            // 数据目录取不到时不能静默打印空日志——那会让用户以为「设备没出过事」，
            // 而真相是「根本没找到日志目录」。当成失败退出并说明。
            match node_core::data_dir() {
                Ok(d) => log_cmd::run(&rest, &d, out, err),
                // `dirs::data_dir()` 在 `HOME` 与 `XDG_DATA_HOME` 都没有时会返回 `None`；
                // 那不是「不可达」，只是要在一个没有家目录的进程里跑（见下面
                // `data_dir_can_fail_for_both_entry_points` 的子进程夹具）。
                Err(e) => {
                    drop(writeln!(err, "afcli log: 无法定位数据目录：{e}"));
                    std::process::ExitCode::FAILURE
                }
            }
        }
        ParseResult::Usage => {
            drop(writeln!(err, "{USAGE}"));
            std::process::ExitCode::FAILURE
        }
        ParseResult::Run(rest) => run_node(&rest, out, err),
    }
}

/// `run` 的修饰符里有没有一个要求「后台运行」的 flag。
///
/// **拒绝而不是忽略**：用户敲 `--background` 是想要一个 afcli 没有的能力，
/// 静默忽略会让他以为进程已经脱离终端，结果一关 SSH 设备就掉线。
/// 这条纯函数同时被 `run_node` 与单测使用。
fn disallowed_background_flag(args: &[String]) -> Option<&'static str> {
    BACKGROUND_FLAGS
        .iter()
        .copied()
        .find(|f| args.iter().any(|a| a == f))
}

/// `run` 的参数里有没有 `--foreground`（无操作别名）。
fn has_foreground_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == FOREGROUND_FLAG)
}

/// 跑 `run`：备好环境（数据目录、Hub 地址）后转 [`dispatch`]。
///
/// **它不做角色判定**：角色只看 argv，而那件事只有 [`role_of`] 一处。
// `dispatch`（可测接缝）。单元测试从 `dispatch` 进入。
fn run_node<W1, W2>(args: &[String], out: W1, mut err: W2) -> std::process::ExitCode
where
    W1: std::io::Write,
    W2: std::io::Write,
{
    // `--background` / `--daemon` 打在 **stderr**（用法/错误），stdout 留给
    // 用户消息；且必须在碰数据目录 / 连 Hub 之前返回。
    if let Some(flag) = disallowed_background_flag(args) {
        drop(writeln!(
            err,
            "afcli run: {flag} 不受支持：afcli 永远在前台运行，不会 daemon 化。\
             请改用 systemd（deploy/afcli.service，Type=simple）或 Docker 让它常驻。"
        ));
        drop(writeln!(err, "{USAGE}"));
        return std::process::ExitCode::FAILURE;
    }
    // 无操作别名：不改写 args（worker 的 argv 要原样继承），也不新开分支。
    let _ = has_foreground_flag(args);

    let data_dir = match runargs::data_dir() {
        Ok(d) => d,
        // 与 `log` 那条同源：没有家目录的进程里 `dirs::data_dir()` 返回 `None`。
        // 覆盖方式见 `data_dir_can_fail_for_both_entry_points`。
        Err(e) => {
            drop(writeln!(err, "afcli run: 无法定位数据目录：{e}"));
            return std::process::ExitCode::FAILURE;
        }
    };
    let hub_url = runargs::hub_url(std::env::var(runargs::ENV_HUB_URL).ok().as_deref());
    dispatch(args, data_dir, hub_url, out, err)
}

/// 按角色分派。`data_dir` 与 `hub_url` 由调用方备好——它们是「环境」，
/// 混在分派里就没法在不真的起进程的前提下测分派本身。
///
/// 角色判定只有 [`role_of`] 一处，但**两处都要用**——本函数决定现在跑谁，
/// supervisor 用 `worker_argv` 决定下次 spawn 谁。两处判定漂移的后果是
/// supervisor 把自己当 worker（或反过来）spawn 出一个 supervisor，
/// 设备变成两个进程互相等对方退出。
fn dispatch<W: std::io::Write, E: std::io::Write>(
    args: &[String],
    data_dir: PathBuf,
    hub_url: String,
    out: W,
    err: E,
) -> std::process::ExitCode {
    dispatch_with_exe(args, data_dir, hub_url, out, err, None)
}

/// [`dispatch`] 的真正实现。`test_exe` 是**单测接缝**：生产传 `None`
/// （被管理的二进制就是当前可执行文件），单测传一个临时目录里的假二进制，
/// 好让 `supervise` 立刻判 Stop 返回、不必 spawn 一个无限循环。
fn dispatch_with_exe<W: std::io::Write, E: std::io::Write>(
    args: &[String],
    data_dir: PathBuf,
    hub_url: String,
    out: W,
    err: E,
    test_exe: Option<PathBuf>,
) -> std::process::ExitCode {
    // 入口行与后续每一行日志共用一个 writer：不这样做，supervisor 另开一个
    // stdout 句柄，前台看到的顺序会错乱（甚至丢行）。
    let console = node_core::entry::ConsoleEcho::new(out);
    let err_echo = node_core::entry::ConsoleEcho::new(err);
    match role_of(args) {
        Role::Worker => {
            // worker 的日志钩子要 `Send + Sync + 'static`，所以这里接的是**进程
            // stderr**（子进程继承前台终端 / journal）；`err_echo` 的缓冲语义
            // 由单测在 `dispatch` 的 supervisor 分支里验证。
            let mut w =
                node_core::worker::Worker::with_console_err(data_dir, hub_url, std::io::stderr());
            // **stdout 直达用户**：worker 要说的话（「Hub 还没设密码，去浏览器
            // 里做完 setup」）必须落在用户已经盯着的那块屏幕上，而不是只进
            // 日志文件——日志要 `afcli log` 才看得见。
            w.work(console.clone())
        }
        Role::Supervisor => {
            // **完整 argv**（不含程序名）原样传下去：用户给的参数全部自动继承
            // （标准①）；Hub 地址走环境变量，子进程同样继承得到。
            let mut full = vec!["run".to_string()];
            full.extend_from_slice(args);
            // 数据目录打到 stdout：它是「日志在哪」的唯一入口，
            // 而 stdout 在 systemd 下会进 journal，用户不用猜。
            drop(writeln!(
                console.clone(),
                "{} run: data dir {}",
                BIN_NAME,
                data_dir.display()
            ));
            match test_exe {
                Some(exe) => supervisor::Supervisor::with_exe_and_console(
                    full, data_dir, exe, console, err_echo,
                )
                .supervise(),
                None => supervisor::Supervisor::new(full, data_dir, console, err_echo).supervise(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::BufRead;

    fn run_capture(args: &[&str]) -> (std::process::ExitCode, String, String) {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(a, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn version_prints_version_and_succeeds() {
        let (code, out, err) = run_capture(&["--version"]);
        assert!(code == std::process::ExitCode::SUCCESS);
        assert_eq!(
            out.trim(),
            format!("{BIN_NAME} {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(err.is_empty());
    }

    #[test]
    fn version_short_flag() {
        let (code, out, _err) = run_capture(&["-V"]);
        assert!(code == std::process::ExitCode::SUCCESS);
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn help_prints_usage_and_succeeds() {
        let (code, out, err) = run_capture(&["--help"]);
        assert!(code == std::process::ExitCode::SUCCESS);
        assert!(out.contains("用法"));
        assert!(err.is_empty());
    }

    #[test]
    fn no_args_is_usage_to_stderr_failure() {
        let (code, out, err) = run_capture(&[]);
        assert!(code != std::process::ExitCode::SUCCESS);
        assert!(out.is_empty());
        assert!(err.contains("用法"));
    }

    #[test]
    fn unknown_arg_is_usage_to_stderr_failure() {
        let (code, out, err) = run_capture(&["frobnicate"]);
        assert!(code != std::process::ExitCode::SUCCESS);
        assert!(out.is_empty());
        assert!(err.contains("用法"));
    }

    #[test]
    fn log_subcommand_dispatches_with_the_remaining_args() {
        let a: Vec<String> = ["log", "-n", "5"].iter().map(|s| s.to_string()).collect();
        assert!(matches!(parse_args(a), ParseResult::Log(ref r) if r == &["-n", "5"]));
    }

    #[test]
    fn log_without_extra_args_dispatches_empty() {
        let a: Vec<String> = vec!["log".to_string()];
        assert!(matches!(parse_args(a), ParseResult::Log(r) if r.is_empty()));
    }

    /// `afcli log` 的参数错误必须失败退出并给出 `log` 自己的用法，
    /// 而不是掉进顶层用法——用户敲了子命令，提示就应当对着子命令说。
    #[test]
    fn log_with_bad_args_reports_the_log_usage() {
        let (code, out, err) = run_capture(&["log", "--follow"]);
        assert!(code != std::process::ExitCode::SUCCESS);
        assert!(out.is_empty());
        assert!(err.contains("afcli log [-n N]"), "{err}");
    }

    /// `run` 的参数原样交给分派层——supervisor 靠它把参数透传给 worker
    /// （标准①：父进程 argv + `--worker`）。
    #[test]
    fn run_subcommand_dispatches_with_the_remaining_args() {
        let a: Vec<String> = ["run", "--hub", "ws://h/ws"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(matches!(parse_args(a), ParseResult::Run(ref r) if r == &["--hub", "ws://h/ws"]));
    }

    /// **角色只由 argv 决定**（D1）：`run --worker` 是 worker，`run` 是 supervisor。
    /// 没有第二条路径（没有环境变量），所以这两个用例就是全部。
    #[test]
    fn role_comes_from_argv_alone() {
        assert!(matches!(role_of(&[]), Role::Supervisor));
        assert!(matches!(role_of(&["--worker".to_string()]), Role::Worker));
        // flag 在 `run` 之后任意位置都算，不要求紧挨
        assert!(matches!(
            role_of(&[
                "--hub".to_string(),
                "ws://h/ws".to_string(),
                "--worker".to_string()
            ]),
            Role::Worker
        ));
    }

    /// worker 角色：`--worker` 走 worker，不走 supervisor。
    ///
    /// 用一个普通文件当 data_dir，让 `Identity::load_or_create` 立刻失败——
    /// worker 由此返回 FAILURE 而不进入连接循环。若角色判定反了，
    /// 这里会打出一行 data dir 并挂死在 supervise 循环里。
    #[test]
    fn the_worker_role_is_taken_when_the_flag_is_present() {
        let d = std::env::temp_dir().join("afcli-dispatch-worker");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&d);
        std::fs::write(&d, b"not a dir").expect("blocker");
        let mut out = Vec::new();
        let code = dispatch(
            &["run".to_string(), runargs::WORKER_FLAG.to_string()],
            d.clone(),
            "ws://hub/ws".to_string(),
            &mut out,
            Vec::<u8>::new(),
        );
        assert_eq!(code, std::process::ExitCode::FAILURE);
        // 先解码再断言：`assert!` 的消息只在失败时求值；放在消息里的
        // `from_utf8_lossy` 在一条通过的测试里永远覆盖不到。
        let rendered = String::from_utf8_lossy(&out).into_owned();
        assert!(
            out.is_empty(),
            "worker 不该打 data dir——那是 supervisor 的入口行: {rendered:?}"
        );
        let _ = std::fs::remove_file(&d);
    }

    /// supervisor 那一半现在能在同一进程里跑完：`dispatch_with_exe` 允许
    /// 注入假二进制 + Inconsistent 状态，让 `supervise` 立即判 Stop（见
    /// `dispatch_echoes_the_startup_and_supervisor_log_lines_to_the_same_stdout`）。
    /// 这条只额外锁住入口行的**格式本身**：那一行是 `afcli run` 打印出来的
    /// 第一条用户可见消息，进程级测试断言它必须是 `afcli run: data dir <绝对路径>`。
    #[test]
    fn the_supervisor_entry_line_names_the_binary_and_the_dir() {
        // 与 dispatch 里那一行同源：改一处另一处要跟着改，所以只在这里
        // 断言格式本身（进程级含义由集成测试兜）。
        let d = std::path::Path::new("/tmp/afcli-data");
        let line = format!("{} run: data dir {}", BIN_NAME, d.display());
        assert_eq!(line, "afcli run: data dir /tmp/afcli-data");
    }

    /// 用法文本必须说清 `run` 的存在与 `--worker` 的性质——
    /// 用户看到 supervisor 拉起一个 `run --worker` 进程时，
    /// 至少要能从 `--help` 里查出它是什么。
    #[test]
    fn usage_documents_run_and_the_worker_flag() {
        assert!(USAGE.contains("afcli run"), "{USAGE}");
        assert!(USAGE.contains("--worker"), "{USAGE}");
        assert!(USAGE.contains("AFCLI_HUB_URL"), "{USAGE}");
    }

    // ---- Bug 3：前台是唯一形态，`--foreground` 是无操作别名 ----

    /// `--foreground` 与后台 flag 是**两个不同的判据**：前者是无操作别名，
    /// 后者必须拒绝。把两者混成一张表会让「加了 --foreground 就被当成后台
    /// 参数报错」这种回归溜过去，所以分别钉住。
    #[test]
    fn the_foreground_alias_is_neither_a_background_flag_nor_a_new_mode() {
        assert_eq!(FOREGROUND_FLAG, "--foreground");
        assert!(has_foreground_flag(&["--foreground".to_string()]));
        assert!(has_foreground_flag(&[
            "--hub".to_string(),
            "h:9002".to_string(),
            "--foreground".to_string()
        ]));
        assert!(!has_foreground_flag(&[]));
        assert!(!has_foreground_flag(&["--background".to_string()]));
        assert_eq!(
            disallowed_background_flag(&["--foreground".to_string()]),
            None,
            "前台别名不该落进「要求后台」的判据"
        );
        assert_eq!(
            disallowed_background_flag(&["--background".to_string()]),
            Some("--background")
        );
        assert_eq!(
            disallowed_background_flag(&["run".to_string(), "--daemon".to_string()]),
            Some("--daemon")
        );
        assert_eq!(BACKGROUND_FLAGS, ["--background", "--daemon"]);
    }

    /// `--background` / `--daemon` **必须在碰数据目录和连 Hub 之前**报错：
    /// 写进 stderr 的一句「不受支持」+ 用法，stdout 一个字都不给用户消息之外
    /// 的东西。这里不设任何环境变量也能跑完，正是「提前返回」的证据——
    /// 若判定挪到 `dispatch` 之后，这条用例会挂死在 supervisor 循环里。
    #[test]
    fn background_and_daemon_are_rejected_on_stderr_with_usage() {
        for flag in BACKGROUND_FLAGS {
            let (code, out, err) = run_capture(&["run", flag]);
            assert_ne!(code, std::process::ExitCode::SUCCESS, "{flag} 必须失败");
            assert!(out.is_empty(), "{flag} 的 stdout 必须留给用户消息：{out:?}");
            assert!(
                err.contains(flag) && err.contains("不受支持") && err.contains("daemon"),
                "{flag} 要说清「不支持后台 + 不 daemon 化」，实际：{err:?}"
            );
            assert!(
                err.contains("systemd") && err.contains("Docker"),
                "{flag} 要给出替代方案，否则用户只会再试一次：{err:?}"
            );
            assert!(err.contains("用法"), "{flag} 必须附上用法：{err:?}");
        }
    }

    /// 后台 flag 与内部 `--worker` 同时出现时也要拒绝：角色判定在 `dispatch`
    /// 里，而拒绝在它之前。漏掉这条会让 `run --worker --daemon` 悄悄跑成一个
    /// 前台 worker，与用户「要后台」的意图相反。
    #[test]
    fn a_background_flag_is_rejected_even_when_the_worker_flag_is_present() {
        let args = [
            "run".to_string(),
            runargs::WORKER_FLAG.to_string(),
            "--daemon".to_string(),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(args, &mut out, &mut err);
        assert_ne!(code, std::process::ExitCode::SUCCESS);
        assert!(out.is_empty());
        let err = String::from_utf8(err).expect("utf8");
        assert!(err.contains("--daemon 不受支持"), "{err}");
    }

    /// `--help` 必须把这次修的两件事都写清楚：前台别名是 no-op、后台 flag
    /// 会被拒绝。否则报了错的用户还得翻源码才知道该敲什么。
    #[test]
    fn usage_explains_the_foreground_alias_and_the_no_daemon_rule() {
        for phrase in [
            "afcli run [--foreground]",
            "--foreground",
            "--background",
            "--daemon",
            "前台阻塞",
            "不会 daemon 化",
            "控制台",
        ] {
            assert!(USAGE.contains(phrase), "用法里少了 {phrase:?}：{USAGE}");
        }
    }

    /// `run --foreground` 在**真进程**上必须被接受：`run` 本来就前台阻塞，
    /// 这个 flag 只是脚本里的意图标记，不该改角色、不该报用法、也不该
    /// 另开一条代码路径。判据是 supervisor 的入口行照常出现在 stdout。
    ///
    /// 用 `AFCLI_DATA_DIR` 指向临时目录、`AFCLI_HUB_URL=""`（走本机缺省），
    /// 与既有进程级用例同一套夹具。读到第一行就 kill，绝不 `read_to_string`
    /// 等一个不会自己退出的 supervisor。
    #[test]
    #[cfg(unix)] // 子进程由 kill() 回收；Windows 的形态不同
    fn run_foreground_is_accepted_as_the_same_supervisor_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let (mut child, stdout, stderr) = spawn_run(&["run", FOREGROUND_FLAG], dir.path());
        let lines = collect_lines(stdout, 2, std::time::Duration::from_secs(10));
        assert!(
            lines.len() >= 2,
            "前台启动要连着给出入口行与 supervisor 日志行，实际 {lines:?}"
        );
        assert!(
            lines[0].starts_with("afcli run: data dir "),
            "--foreground 不该改掉 supervisor 的入口行：{lines:?}"
        );
        let shown = lines[0].trim_start_matches("afcli run: data dir ").trim();
        assert_eq!(
            std::path::Path::new(shown),
            dir.path().join("agentfleet"),
            "数据目录必须来自这次夹具：{shown}"
        );
        assert!(
            lines[1].contains("[supervisor] info supervisor starting"),
            "前台控制台必须看到 supervisor 的日志行：{lines:?}"
        );
        assert!(
            lines[1].contains("run --foreground"),
            "完整 argv 要原样透传给 supervisor（--foreground 不能被吞）：{lines:?}"
        );
        // 人读时间戳（Bug 2）：`YYYY-MM-DD HH:MM:SS`，不是裸 unix 秒。
        let stamp = lines[1].split_whitespace().next().unwrap_or("");
        assert!(
            stamp.len() == 10 && stamp.contains('-') && lines[1].contains(':'),
            "控制台日志行要带人读时间戳：{lines:?}"
        );
        // 别把 stderr 的读端丢掉：子进程继承的是这个管道，读端一关它写日志
        // 就会撞 SIGPIPE，反而把 worker 弄崩，干扰「前台接受别名」这条判据。
        let _stderr = stderr;
        kill_group(&mut child);
    }

    /// 前台启动时 **supervisor 的日志必须回显到注入的 stdout**：这一条在
    /// **同一进程**里走完 `dispatch` 的 supervisor 分支，不依赖
    /// `target/debug/afcli` 是不是刚编过（进程级用例另有一条兜）。
    ///
    /// 让 `supervise` 立刻返回而不起无限循环：把假二进制旁边的状态文件写成
    /// 「staged 没了、备份还在」的 Inconsistent，`decide` 当即判 Stop。
    /// 注入的 `ConsoleEcho` 于是同时收到入口行与 `say` 的每一行，顺序也可查。
    /// 收尾用 guard 删掉这些临时文件，断言失败时也不留给下一条用例。
    #[test]
    fn dispatch_echoes_the_startup_and_supervisor_log_lines_to_the_same_stdout() {
        let dir = tempfile::tempdir().expect("tmp");
        let exe = dir.path().join("afcli");
        std::fs::write(&exe, b"fake").expect("fake exe");
        let state = node_core::update::state_path_for(&exe);
        let backup = node_core::update::backup_path_for(&exe);
        std::fs::write(&backup, b"old").expect("backup");
        let mut st = node_core::update::UpdateState::requested("9.9.9", &exe, &backup, 1);
        st.phase = node_core::update::UpdatePhase::StartingNew;
        st.staged_path = dir.path().join("missing.new").display().to_string();
        node_core::update::write_state(&state, &st).expect("state");
        let cleanup = RemoveOnDrop(vec![state.clone(), backup.clone()]);

        let console = node_core::entry::ConsoleEcho::new(Vec::<u8>::new());
        let err = node_core::entry::ConsoleEcho::new(Vec::<u8>::new());
        let code = dispatch_with_exe(
            &[FOREGROUND_FLAG.to_string()],
            dir.path().join("data"),
            "host:9002".to_string(),
            console.clone(),
            err.clone(),
            Some(exe.clone()),
        );
        assert_eq!(
            code,
            std::process::ExitCode::from(supervisor::SUPERVISOR_STOPPED),
            "Inconsistent 状态必须让 supervisor 停下而不是 spawn"
        );

        let out = String::from_utf8(console.lock().clone()).expect("utf8");
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("afcli run: data dir "),
            "入口行必须排在第一行：{out:?}"
        );
        assert!(
            lines[1..]
                .iter()
                .any(|l| l.contains("[supervisor] info supervisor starting")
                    && l.contains("run --foreground")),
            "supervisor 的日志行必须回显到同一块 stdout：{out:?}"
        );
        assert!(
            lines[1..]
                .iter()
                .any(|l| l.contains("stopping, human intervention")),
            "停下来等人这件事也要出现在前台：{out:?}"
        );
        assert!(
            out.contains("afcli: stopping, human intervention required"),
            "Stop 的那句用户提示也要直达 stdout：{out:?}"
        );
        // 人读时间戳（Bug 2）：每一条回显行都以 `YYYY-MM-DD HH:MM:SS` 开头。
        for line in &lines[1..] {
            if line.contains("[supervisor] info") {
                let head = line.split_whitespace().next().unwrap_or("");
                assert_eq!(head.len(), 10, "日期应是 YYYY-MM-DD：{line}");
                assert!(head.contains('-'), "日期要带分隔符：{line}");
            }
        }
        let err_text = String::from_utf8(err.lock().clone()).expect("utf8");
        assert!(
            err_text.is_empty(),
            "supervisor 的日志不该走 stderr：{err_text:?}"
        );
        // 同一份文件里也只有一份：`afcli log` 不该看到重复行。
        let logged = std::fs::read_to_string(node_core::log::log_path(&dir.path().join("data")))
            .unwrap_or_default();
        assert!(
            logged.contains("[supervisor] info supervisor starting"),
            "日志文件仍要照常写：{logged:?}"
        );
        drop(cleanup);
    }

    /// 前台启动时 **supervisor 与 worker 的日志都要回显**：supervisor 走
    /// stdout（用户敲命令的进程），worker 走 stderr（诊断通道）。
    ///
    /// 两块屏幕分开读、各自有上限：worker 是 supervisor 拉起的子进程，
    /// 它继承的 stderr 正是前台终端；只读日志文件的话，用户看到的仍是
    /// 那块「启动了却什么都不说」的空白终端（用户反馈原话）。
    #[test]
    #[cfg(unix)]
    fn the_foreground_console_receives_both_supervisor_and_worker_log_lines() {
        let dir = tempfile::tempdir().expect("tmp");
        let (mut child, stdout, stderr) = spawn_run(&["run"], dir.path());

        let out_lines = collect_lines(stdout, 2, std::time::Duration::from_secs(10));
        assert!(
            out_lines
                .iter()
                .any(|l| l.starts_with("afcli run: data dir ")),
            "supervisor 入口行必须在 stdout：{out_lines:?}"
        );
        assert!(
            out_lines
                .iter()
                .any(|l| l.contains("[supervisor] info supervisor starting")),
            "supervisor 的日志行必须回显到 stdout：{out_lines:?}"
        );

        let err_lines = collect_lines(stderr, 1, std::time::Duration::from_secs(15));
        assert!(
            err_lines
                .iter()
                .any(|l| l.contains("[worker] info") && l.contains("afcli worker starting")),
            "worker 的日志行必须回显到 stderr：{err_lines:?}"
        );
        kill_group(&mut child);
    }

    /// `run(["run"])` 的整条链（解析 → `run_node` → `dispatch` → supervisor）
    /// 在**真进程**上走一遍：supervisor 先把数据目录打到 stdout，随后进入
    /// `supervise` 无限循环。读到那一行就够了，剩下的由 `Guard` 回收。
    ///
    /// 为什么必须真起进程：`Supervisor::new` 用 `current_exe()` 当被管理的
    /// 二进制，单测里 `supervise` 会把**测试进程自己**递归 spawn 出来。
    /// 数据目录用 `AFCLI_DATA_DIR` 指到临时目录，避免写进真实家目录。
    #[test]
    #[cfg(unix)] // 子进程由 `kill()` 回收；Windows 的形态不同
    fn run_dispatches_to_the_supervisor_in_a_real_process() {
        let dir = tempfile::tempdir().expect("tmp");
        let bin = sibling_binary();
        let mut child = std::process::Command::new(&bin)
            .arg("run")
            .env("AFCLI_DATA_DIR", dir.path())
            .env("AFCLI_HUB_URL", "")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn afcli");
        let mut pipe = child.stdout.take().expect("stdout");
        let first = read_one_line(&mut pipe);
        assert!(
            first.starts_with("afcli run: data dir "),
            "supervisor 入口行必须先给出数据目录，实际 {first:?}"
        );
        let shown = first.trim_start_matches("afcli run: data dir ").trim();
        assert_eq!(
            std::path::Path::new(shown),
            dir.path().join("agentfleet"),
            "打出来的必须是这次夹具的数据目录，而不是别处的默认目录"
        );
        // `supervise` 不会自己退出：显式回收，不留僵尸也不挂住测试。
        let _ = child.kill();
        let _ = child.wait();
    }

    /// `run(["run", "--worker"])` 在真进程上必须**不**打 supervisor 那一行，
    /// 并在身份加载失败时以 FAILURE 退出。
    ///
    /// 造法：`AFCLI_DATA_DIR` 指向一个普通文件，`data_dir()` 会得到
    /// `<file>/agentfleet`，`Identity::load_or_create` 里 `create_dir_all`
    /// 必报 `NotADirectory`；worker 于是立刻返回 FAILURE，不会进连接循环。
    /// 这条覆盖的是 `run_node` 的成功臂 + `dispatch` 的 worker 臂——
    /// 也就是 `run(["run", ...])` 真正走的那几行。
    #[test]
    #[cfg(unix)]
    fn run_worker_takes_the_data_dir_from_the_environment_and_fails_cleanly() {
        let dir = tempfile::tempdir().expect("tmp");
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("blocker");
        let out = std::process::Command::new(sibling_binary())
            .args(["run", runargs::WORKER_FLAG])
            .env("AFCLI_DATA_DIR", &blocker)
            .env("AFCLI_HUB_URL", "")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run afcli");
        // 先解码再断言：放在 `assert!` 消息里的解码只在失败时求值，
        // 这条用例正常通过时那一行本来覆盖不到。
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            !out.status.success(),
            "身份加载失败必须非零退出：{child_stderr}"
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        assert!(
            !stdout.contains("data dir"),
            "worker 不该打 supervisor 的入口行：{stdout}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("cannot load identity"),
            "失败原因要落在 stderr 上：{stderr}"
        );
    }

    /// **两个入口的 `data_dir()` 失败臂都必须留痕**，而不是静默打印空日志 /
    /// 空跑 supervisor。这在 `HOME` 和 `XDG_DATA_HOME` 都没有、且 passwd 里
    /// 也查不到家目录的进程里是**可达**的（原先那两条注释说它不可达，是错的）。
    ///
    /// 造法：用 `libc::setresuid` 切到一个 passwd 里不存在的 uid，再去掉
    /// `HOME` / `XDG_DATA_HOME` / `AFCLI_DATA_DIR`。此时 `dirs::data_dir()`
    /// 的 `getpwuid_r` 兜底也失败，返回 `None`。改 uid 与去掉环境变量都是
    /// **进程级**的，所以放在子进程里做：父进程只 spawn 一次带哨兵变量的
    /// 同一测试二进制，自己那条环境一点不动。
    #[test]
    #[cfg(unix)] // 用 setresuid + 无 HOME 的进程构造 data_dir() 的 Err
    fn data_dir_can_fail_for_both_entry_points() {
        let out = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "tests::data_dir_can_fail_for_both_entry_points_child",
            ])
            .env("AF_NO_DATA_DIR", "1")
            .env_remove("HOME")
            .env("XDG_DATA_HOME", "")
            .env("AFCLI_DATA_DIR", "")
            .env("AFCLI_HUB_URL", "")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn the child test process");
        // 先解码再断言：`assert!` 的消息只在失败时求值，把两段解码放在
        // 消息里会让它们在一条通过的测试里永远覆盖不到。
        let child_stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "子进程必须绿，stdout：{child_stdout}，stderr：{child_stderr}"
        );
        // `--exact` 拼错名字时 libtest 会「跑了 0 条」并以 0 退出，
        // 只看退出码会把一个什么都没测的子进程当成绿。
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{stdout}"
        );
    }

    /// 上一条的子进程本体。没有 `AF_NO_DATA_DIR` 时什么都不做（no-op，不是跳过）。
    #[test]
    #[cfg(unix)]
    fn data_dir_can_fail_for_both_entry_points_child() {
        if std::env::var_os("AF_NO_DATA_DIR").is_none() {
            return;
        }
        // SAFETY: `setresuid` 只改本子进程的三个 uid，不涉及内存或别名；
        // 这个进程是这条用例专用的，改完就退出，不会影响别的测试。
        let rc = unsafe { libc::setresuid(60_000, 60_000, 60_000) };
        // 先取 errno 再断言：放在 `assert_eq!` 消息里的表达式只在失败时求值，
        // 一条通过的子进程用例覆盖不到那一行。
        let setresuid_errno = std::io::Error::last_os_error();
        assert_eq!(rc, 0, "切到不存在的 uid 失败：{setresuid_errno}");
        let err = runargs::data_dir().expect_err("没有家目录时必须取不到数据目录");
        assert!(!err.is_empty(), "错误信息不能是空串");

        for (args, prefix) in [
            (vec!["log".to_string()], "afcli log: 无法定位数据目录"),
            (vec!["run".to_string()], "afcli run: 无法定位数据目录"),
        ] {
            let mut out = Vec::new();
            let mut err_out = Vec::new();
            let code = run(args.clone(), &mut out, &mut err_out);
            assert_eq!(code, std::process::ExitCode::FAILURE, "{args:?}");
            assert!(out.is_empty(), "{args:?} 不该往 stdout 打任何东西");
            let text = String::from_utf8(err_out).expect("utf8");
            assert!(
                text.starts_with(prefix),
                "{args:?} 的失败原因要说清是哪个命令、哪一步：{text}"
            );
        }
    }

    /// `assert!` 的消息只在断言失败时才求值——所以那个 `from_utf8_lossy`
    /// 不会因为「测一条通过的断言」而被覆盖。这里直接把同一段解码拿出来
    /// 断言一次：非 UTF-8 的 worker 输出必须被无损地转成可读字符串，
    /// 否则失败提示会变成一串看不懂的字节。
    #[test]
    fn a_non_utf8_worker_output_still_has_a_readable_failure_message() {
        let out = vec![0xff, 0xfe, b'\n'];
        let rendered = String::from_utf8_lossy(&out).into_owned();
        assert!(
            rendered.contains('\u{fffd}'),
            "非 UTF-8 字节必须显示成替换字符，实际 {rendered:?}"
        );
        assert!(rendered.ends_with('\n'), "换行要留着：{rendered:?}");
    }

    /// lib 单测里没有 Cargo 注入的 `CARGO_BIN_EXE_afcli`（它只给集成测试），
    /// 但 `current_exe()` 就在 `target/debug/deps/` 下，`afcli` 是它的同级
    /// `../afcli`。断言它存在再返回，缺了就当场报错而不是路径拼错。
    #[cfg(unix)]
    fn sibling_binary() -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("test exe");
        let bin = exe
            .parent()
            .and_then(|deps| deps.parent())
            .expect("target/debug")
            .join(BIN_NAME);
        // 先把路径渲染好再断言：`assert!` 的消息只在失败时求值，把
        // `display()` 放在消息里会让这一行在正常通过的用例里覆盖不到。
        let shown = bin.display().to_string();
        assert!(
            bin.is_file(),
            "找不到被测二进制 {shown}（由 cargo build 产出）"
        );
        bin
    }

    /// 从管道读满一行就返回。`supervise` 永不退出，不能用 `read_to_string`
    /// 等管道关闭——那会挂到测试超时。
    #[cfg(unix)]
    fn read_one_line(pipe: &mut impl std::io::Read) -> String {
        let mut got = String::new();
        let mut byte = [0u8; 1];
        while pipe.read(&mut byte).expect("read stdout") == 1 {
            got.push(char::from(byte[0]));
            if byte[0] == b'\n' {
                break;
            }
        }
        got
    }

    /// 收尾用 guard 删掉这些临时文件：即便断言失败也不会把 `Failed`
    /// 状态留给下一条用例（那会让下一次 `reconcile` 判成 Nothing，
    /// `supervise` 转头去 spawn 一个无限循环）。
    #[cfg(test)]
    struct RemoveOnDrop(Vec<PathBuf>);

    #[cfg(test)]
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// 真跑一次 `afcli run`，stdout / stderr 各自走管道。
    ///
    /// `pre_exec` 把子进程放进**自己的进程组**：supervisor 会再 spawn 一个
    /// worker，只 `child.kill()` 会留下那个继承着管道的孤儿（它会一直重连，
    /// 于是读线程永远等不到 EOF）。收尾用 [`kill_group`] 把整组一起收掉。
    #[cfg(unix)]
    fn spawn_run(
        argv: &[&str],
        data_dir: &std::path::Path,
    ) -> (
        std::process::Child,
        std::process::ChildStdout,
        std::process::ChildStderr,
    ) {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(sibling_binary());
        cmd.args(argv)
            .env("AFCLI_DATA_DIR", data_dir)
            .env("AFCLI_HUB_URL", "")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // 把子进程放进自己的进程组：supervisor 会再 spawn 一个 worker，
        // 收尾时用 `kill_group` 连孙进程一起收掉，不留继承管道的孤儿。
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn afcli");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        (child, stdout, stderr)
    }

    /// 从一块**永不关闭**的管道里收到 `want` 行，或者到点返回已收到的。
    ///
    /// 读放在专属线程里：`ChildStdout` 没有 `set_read_timeout`，而 supervisor
    /// 是无限循环，同步 `read_line` 会把测试挂死。channel + `recv_timeout`
    /// 让「等不到第二行」变成一个普通断言失败，而不是整个测试超时。
    #[cfg(unix)]
    fn collect_lines<R: std::io::Read + Send + 'static>(
        pipe: R,
        want: usize,
        budget: std::time::Duration,
    ) -> Vec<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(pipe);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // 对端关了 / 读失败：没有更多行了
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break; // 收端已丢弃，别再占着管道
                        }
                    }
                }
            }
        });
        let deadline = std::time::Instant::now() + budget;
        let mut got = Vec::new();
        while got.len() < want {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left.min(std::time::Duration::from_millis(100))) {
                Ok(line) => got.push(line),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        got
    }

    /// 回收 supervisor **和它拉起的 worker**：负 pid 发给整个进程组。
    /// 先 `child.kill()` 兜住「pre_exec 还没跑完」的极窄窗口，再 wait 收尸。
    #[cfg(unix)]
    fn kill_group(child: &mut std::process::Child) {
        let _ = child.kill();
        // SAFETY: `kill` 只对上面 `setpgid(0, 0)` 建出来的进程组发信号；
        // pgid 等于子进程 pid，且该组里只有这次测试拉起的 afcli 进程。
        let pgid = -(i32::try_from(child.id()).unwrap_or(i32::MAX));
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
        let _ = child.wait();
    }
}
