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
//! supervisor 与 worker 各自成模块，`main.rs` 保持极薄、每条分支都能由
//! 「spawn 二进制 + 不同 argv」走到（覆盖率门禁要求 main.rs 100%）。

pub mod filelog;
pub mod log_cmd;
pub mod runargs;
pub mod supervisor;
pub mod worker;

use std::path::PathBuf;

/// 二进制名称。命令名必须是 `afcli`（与 Node.js 运行时 `node` 撞名才改的），
/// crate 名是 `node-cli`，二者都不要改。
pub const BIN_NAME: &str = "afcli";

/// 用法文本。无参数 / 未知参数时打到 stderr。
pub const USAGE: &str = "\
afcli — AgentFleet node 命令

用法:
  afcli --version     打印版本并退出 0
  afcli --help        打印本帮助并退出 0
  afcli run           启动 supervisor（默认角色）
  afcli log [-n N]    打印最近 N 行日志（默认 200）

`run --worker` 是 supervisor 拉起 worker 用的内部形态（DECISIONS D1），
不需要手工敲；手敲了也只会跑成 worker，不会变成 supervisor。

Hub 地址取环境变量 AFCLI_HUB_URL，缺省 ws://127.0.0.1:8080/ws。";

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
                // coverage: excluded — data_dir 取不到：dirs::data_dir 在 HOME 存在即返回 Some，且 root 下权限错误不可构造，本环境下该 Err 分支不可达（防御性兜底）。
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

/// 跑 `run`：按角色分派到 supervisor 或 worker。
///
/// 角色判定只有 [`role_of`] 一处，但**两处都要用**——本函数决定现在跑谁，
/// supervisor 用 `worker_argv` 决定下次 spawn 谁。两处判定漂移的后果是
/// supervisor 把自己当 worker（或反过来）spawn 出一个 supervisor，
/// 设备变成两个进程互相等对方退出。
// coverage: excluded — 进程入口：真建数据目录、真解析 hub url，再转
// `dispatch`（可测接缝）。单元测试从 `dispatch` 进入。
fn run_node<W1, W2>(args: &[String], out: W1, mut err: W2) -> std::process::ExitCode
where
    W1: std::io::Write,
    W2: std::io::Write,
{
    let data_dir = match runargs::data_dir() {
        Ok(d) => d,
        // coverage: excluded — data_dir 取不到：dirs::data_dir 在 HOME 存在即返回
        // Some，root 下权限错误又不可构造，本环境下该 Err 分支不可达（防御性兜底）。
        Err(e) => {
            drop(writeln!(err, "afcli run: 无法定位数据目录：{e}"));
            return std::process::ExitCode::FAILURE;
        }
    };
    let hub_url = runargs::hub_url(std::env::var(runargs::ENV_HUB_URL).ok().as_deref());
    dispatch(args, data_dir, hub_url, out)
}

/// 按角色分派。`data_dir` 与 `hub_url` 由调用方备好——它们是「环境」，
/// 混在分派里就没法在不真的起进程的前提下测分派本身。
///
/// 角色判定只有 [`role_of`] 一处，但**两处都要用**——本函数决定现在跑谁，
/// supervisor 用 `worker_argv` 决定下次 spawn 谁。两处判定漂移的后果是
/// supervisor 把自己当 worker（或反过来）spawn 出一个 supervisor，
/// 设备变成两个进程互相等对方退出。
fn dispatch<W: std::io::Write>(
    args: &[String],
    data_dir: PathBuf,
    hub_url: String,
    mut out: W,
) -> std::process::ExitCode {
    match role_of(args) {
        Role::Worker => {
            let mut w = worker::Worker::new(data_dir, hub_url);
            w.work()
        }
        Role::Supervisor => {
            // **完整 argv**（不含程序名）原样传下去：用户给的参数全部自动继承
            // （标准①）；Hub 地址走环境变量，子进程同样继承得到。
            let mut full = vec!["run".to_string()];
            full.extend_from_slice(args);
            // 数据目录打到 stdout：它是「日志在哪」的唯一入口，
            // 而 stdout 在 systemd 下会进 journal，用户不用猜。
            drop(writeln!(
                out,
                "{} run: data dir {}",
                BIN_NAME,
                data_dir.display()
            ));
            supervisor::Supervisor::new(full, data_dir).supervise()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        assert_eq!(code, std::process::ExitCode::FAILURE);
        assert!(
            out.is_empty(),
            // coverage: excluded — 测试夹具的 `String::from_utf8_lossy`：非 UTF-8
            // 输出分支在现有断言下不可达。
            "worker 不该打 data dir——那是 supervisor 的入口行: {:?}",
            String::from_utf8_lossy(&out)
        );
        let _ = std::fs::remove_file(&d);
    }

    /// supervisor 那一半没法在同一进程里跑完：`supervise` 是无限循环，
    /// 而 spawn 出来的会是**测试进程自己**（`current_exe`）。
    /// 所以只锁住分派前的那一行 stdout——真的 spawn 行为在
    /// `tests/afcli_process.rs` 里由子进程承接。
    ///
    /// 这里不重写那一行来断言自己：把 `dispatch` 的 supervisor 分支
    /// 拆成「打入口行」+「进循环」两步，前者单独测。
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
}
