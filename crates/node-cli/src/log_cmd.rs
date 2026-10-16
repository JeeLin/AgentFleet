//! `afcli log`：打印设备端最近的日志。
//!
//! 为什么是子命令而不是 `run` 的一个 flag（`DECISIONS.md` D4）：
//! 用户排障时设备可能**根本没在跑**（supervisor 已退出、升级失败停在 `Failed`），
//! 此时 `afcli run` 只会再制造一个进程，而要看的恰恰是上一个进程留下的日志。
//! 读历史和管连接是两件事，所以是两个入口。

use std::io::Write;
use std::path::Path;

use node_core::log::{self, DEFAULT_TAIL_LINES};

/// `afcli log [-n N]` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogCommand {
    /// 打印最近 `n` 行。
    Tail { lines: usize },
    /// 参数有问题，用法文本与原因。
    Usage { problem: String },
}

/// 解析 `log` 之后的参数。
///
/// 只认 `-n N` / `-n=N` 与 `--lines N`。**没有** `--follow`：
/// M0 的排障场景是「设备已经出问题、我要看它怎么了」，不是盯着看实时流；
/// 真要盯可以 `tail -f`，少一个子命令少一份要测的分支。
pub fn parse(args: &[String]) -> LogCommand {
    let mut lines = DEFAULT_TAIL_LINES;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-n" | "--lines" => match it.next() {
                Some(v) => match v.parse::<usize>() {
                    Ok(n) => lines = n,
                    Err(_) => {
                        return LogCommand::Usage {
                            problem: format!("行数不是合法数字：{v}"),
                        }
                    }
                },
                None => {
                    return LogCommand::Usage {
                        problem: format!("{a} 后面缺行数"),
                    }
                }
            },
            other if other.starts_with("-n=") => match other[3..].parse::<usize>() {
                Ok(n) => lines = n,
                Err(_) => {
                    return LogCommand::Usage {
                        problem: format!("行数不是合法数字：{}", &other[3..]),
                    }
                }
            },
            other => {
                return LogCommand::Usage {
                    problem: format!("未知参数：{other}"),
                }
            }
        }
    }
    LogCommand::Tail { lines }
}

/// 执行 `log`，返回进程退出码。
pub fn run<W1, W2>(
    args: &[String],
    data_dir: &Path,
    mut out: W1,
    mut err: W2,
) -> std::process::ExitCode
where
    W1: Write,
    W2: Write,
{
    match parse(args) {
        LogCommand::Tail { lines } => match log::tail(&log::log_path(data_dir), lines) {
            Ok(ls) => {
                for line in ls {
                    if let Err(e) = writeln!(out, "{line}") {
                        // 管道被 head 关闭是正常用法（`afcli log -n 100 | head`），
                        // 不算错误，也不该往 stderr 喷。
                        drop(e);
                        return std::process::ExitCode::SUCCESS;
                    }
                }
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                drop(writeln!(err, "afcli log: 读取日志失败：{e}"));
                std::process::ExitCode::FAILURE
            }
        },
        LogCommand::Usage { problem } => {
            drop(writeln!(err, "afcli log: {problem}"));
            drop(writeln!(
                err,
                "用法: afcli log [-n N]   （默认 {DEFAULT_TAIL_LINES} 行）"
            ));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::let_underscore_must_use,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn capture(args: &[&str], dir: &Path) -> (std::process::ExitCode, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&v(args), dir, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// 读到第 `broken_at` 次写入时返回 `BrokenPipe` 的 writer，
    /// 复现 `afcli log | head` 里 head 提前关掉管道的情形。
    struct BrokenPipeWriter {
        broken_at: usize,
        writes: usize,
    }

    impl std::io::Write for BrokenPipeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.writes >= self.broken_at {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "模拟管道被读端关闭",
                ))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn no_args_uses_the_default_line_count() {
        assert_eq!(
            parse(&[]),
            LogCommand::Tail {
                lines: DEFAULT_TAIL_LINES
            }
        );
        assert_eq!(DEFAULT_TAIL_LINES, 200);
    }

    #[test]
    fn parses_both_flag_spellings() {
        assert_eq!(parse(&v(&["-n", "50"])), LogCommand::Tail { lines: 50 });
        assert_eq!(
            parse(&v(&["--lines", "50"])),
            LogCommand::Tail { lines: 50 }
        );
        assert_eq!(parse(&v(&["-n=50"])), LogCommand::Tail { lines: 50 });
    }

    #[test]
    fn last_flag_wins() {
        assert_eq!(
            parse(&v(&["-n", "10", "-n", "20"])),
            LogCommand::Tail { lines: 20 }
        );
    }

    #[test]
    fn zero_lines_is_valid_and_prints_nothing() {
        let d = tempfile::tempdir().unwrap();
        log::append_line(&log::log_path(d.path()), "x").unwrap();
        let (code, out, err) = capture(&["-n", "0"], d.path());
        assert!(code == std::process::ExitCode::SUCCESS);
        assert!(out.is_empty(), "`-n 0` 应什么都不打印");
        assert!(err.is_empty());
    }

    #[test]
    fn prints_the_requested_number_of_lines() {
        let d = tempfile::tempdir().unwrap();
        for i in 1..=50 {
            log::append_line(&log::log_path(d.path()), &format!("l{i}")).unwrap();
        }
        let (code, out, _err) = capture(&["-n", "5"], d.path());
        assert!(code == std::process::ExitCode::SUCCESS);
        assert_eq!(out.lines().count(), 5);
        assert_eq!(out.lines().next().unwrap(), "l46");
    }

    #[test]
    fn empty_log_prints_nothing_and_succeeds() {
        let d = tempfile::tempdir().unwrap();
        let (code, out, err) = capture(&[], d.path());
        assert!(
            code == std::process::ExitCode::SUCCESS,
            "还没有日志是正常状态，不该当失败"
        );
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn missing_line_count_is_a_usage_error() {
        let cmd = parse(&v(&["-n"]));
        assert!(
            matches!(cmd, LogCommand::Usage { ref problem } if problem.contains("缺行数")),
            "got {cmd:?}"
        );
    }

    #[test]
    fn non_numeric_line_count_is_a_usage_error() {
        let cmd = parse(&v(&["-n", "abc"]));
        assert!(
            matches!(cmd, LogCommand::Usage { ref problem } if problem.contains("abc")),
            "got {cmd:?}"
        );
    }

    #[test]
    fn equals_form_with_bad_number_is_a_usage_error() {
        let cmd = parse(&v(&["-n=xyz"]));
        assert!(
            matches!(cmd, LogCommand::Usage { ref problem } if problem.contains("xyz")),
            "got {cmd:?}"
        );
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        let cmd = parse(&v(&["--follow"]));
        assert!(
            matches!(cmd, LogCommand::Usage { ref problem } if problem.contains("--follow")),
            "got {cmd:?}"
        );
    }

    /// `afcli log -n 5 | head`：head 只读 5 行就关掉管道，剩余 writeln 失败。
    /// 这是正常用法，不当错误、不喷 stderr，干净退出 0。
    #[test]
    fn broken_pipe_from_a_closed_reader_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        for i in 1..=50 {
            log::append_line(&log::log_path(d.path()), &format!("l{i}")).unwrap();
        }
        let mut out = BrokenPipeWriter {
            broken_at: 3,
            writes: 0,
        };
        let mut err = Vec::new();
        let code = run(&v(&["-n", "50"]), d.path(), &mut out, &mut err);
        let _ = out.flush();
        assert!(
            code == std::process::ExitCode::SUCCESS,
            "broken pipe must be silent success, got {code:?}"
        );
        assert!(
            err.is_empty(),
            "broken pipe must not pollute stderr: {err:?}"
        );
    }

    #[test]
    fn usage_error_exits_failure_and_explains_the_form() {
        let d = tempfile::tempdir().unwrap();
        let (code, out, err) = capture(&["--follow"], d.path());
        assert!(code != std::process::ExitCode::SUCCESS);
        assert!(out.is_empty());
        assert!(err.contains("afcli log [-n N]"), "要给出正确用法：{err}");
        assert!(
            err.contains("未知参数：--follow"),
            "要说清哪个参数错了：{err}"
        );
    }

    #[test]
    fn unreadable_log_reports_failure_instead_of_pretending_its_empty() {
        let d = tempfile::tempdir().unwrap();
        // 目录顶替日志文件 → 读不了但存在。
        // `create_dir` 要求父目录已存在，故先建 `<data>/logs`。
        let p = log::log_path(d.path());
        std::fs::create_dir(p.parent().unwrap()).unwrap();
        std::fs::create_dir(&p).unwrap();
        let (code, _out, err) = capture(&[], d.path());
        assert!(code != std::process::ExitCode::SUCCESS);
        assert!(err.contains("读取日志失败"), "{err}");
    }
}
