//! afcli：node 对外的命令入口（enroll / run / worker）。
//!
//! 当前里程碑只落地**骨架**：解析 `--version` / `--help` 与「无/未知参数」，
//! 真正的 Hub 连接逻辑（enroll 注册、run 常驻、worker 子进程）留待后续子任务。
//! 逻辑放在这里而不是 `main.rs`，是为了**可直接单测**，且让 `main.rs` 保持极薄、
//! 每条分支都能由「spawn 二进制 + 不同 argv」走到（覆盖率门禁要求 main.rs 100%）。
//!
//! 注意：**不要**自己实现连接逻辑——本文件只负责「解析 + 输出」，网络部分由后续
//! 子任务补。这里任何「看起来像实现」的占位都是为后续接线的正确接缝。

/// 二进制名称。命令名必须是 `afcli`（与 Node.js 运行时 `node` 撞名才改的），
/// crate 名是 `node-cli`，二者都不要改。
pub const BIN_NAME: &str = "afcli";

/// 用法文本。无参数 / 未知参数时打到 stderr。
pub const USAGE: &str = "\
afcli — AgentFleet node 命令（M0 占位骨架）

用法:
  afcli --version     打印版本并退出 0
  afcli --help        打印本帮助并退出 0

后续子任务将实现: enroll / run / worker。";

/// 支持的长选项。
enum Flag {
    Version,
    Help,
}

/// 解析 argv（不含程序名）。
///
/// 返回 [`ParseResult`]：要么命中一个已知 flag，要么说明「需要打印用法并失败退出」。
/// 这里**不**碰 `std::env::args()`，参数由调用方传进来——便于单测，也让 `main.rs`
/// 只做「读 argv → 传进来 → 打印」三件事。
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> ParseResult {
    let mut it = args.into_iter();
    // 只认第一个 token：M0 骨架阶段没有子命令/位置参数，
    // 出现「不止一个 token」也按「未知参数」处理（留给后续子任务扩展）。
    match it.next() {
        None => ParseResult::Usage,
        Some(first) => {
            let flag = match first.as_str() {
                "--version" | "-V" => Some(Flag::Version),
                "--help" | "-h" => Some(Flag::Help),
                _ => None,
            };
            match flag {
                Some(Flag::Version) => ParseResult::Version,
                Some(Flag::Help) => ParseResult::Help,
                None => ParseResult::Usage,
            }
        }
    }
}

/// [`parse_args`] 的结果。
enum ParseResult {
    /// 命中 `--version`：打印版本。
    Version,
    /// 命中 `--help`：打印用法（成功退出）。
    Help,
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
        ParseResult::Usage => {
            drop(writeln!(err, "{USAGE}"));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    // 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
    // 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
    // let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
    // 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::indexing_slicing
    )]
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
}
