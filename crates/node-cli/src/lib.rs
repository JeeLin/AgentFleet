//! afcli：node 对外的命令入口（run / log）。
//!
//! `run` 是唯一需要用户主动敲的接入命令（首次自动 enroll，见 `DECISIONS.md` D1）；
//! `log` 是排障入口——设备可能**根本没在跑**（supervisor 已退出、升级停在
//! `Failed`），此时要看的正是上一个进程留下的日志。
//!
//! 逻辑放在这里而不是 `main.rs`，是为了**可直接单测**，且让 `main.rs` 保持极薄、
//! 每条分支都能由「spawn 二进制 + 不同 argv」走到（覆盖率门禁要求 main.rs 100%）。
//!
//! 注意：**不要**自己实现连接逻辑——本文件只负责「解析 + 输出」，网络部分由后续
//! 子任务补。这里任何「看起来像实现」的占位都是为后续接线的正确接缝。

pub mod log_cmd;

/// 二进制名称。命令名必须是 `afcli`（与 Node.js 运行时 `node` 撞名才改的），
/// crate 名是 `node-cli`，二者都不要改。
pub const BIN_NAME: &str = "afcli";

/// 用法文本。无参数 / 未知参数时打到 stderr。
pub const USAGE: &str = "\
afcli — AgentFleet node 命令

用法:
  afcli --version     打印版本并退出 0
  afcli --help        打印本帮助并退出 0
  afcli log [-n N]    打印最近 N 行日志（默认 200）

后续子任务将实现: run（首次自动 enroll）/ worker。";

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
        // 后续子任务会在这里加 `run`；现在任何其它 token 都算未知参数
        _ => ParseResult::Usage,
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

    /// 子任务 4 之后 `run` 才是主命令；在它落地前，`run` 必须落到
    /// 「未知参数」而不是被静默接受。
    #[test]
    fn run_is_not_yet_a_command() {
        let (code, out, err) = run_capture(&["run"]);
        assert!(code != std::process::ExitCode::SUCCESS);
        assert!(out.is_empty());
        assert!(err.contains("用法"), "{err}");
    }
}
