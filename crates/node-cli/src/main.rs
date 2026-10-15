//! afcli：node 对外命令的进程入口。
//!
//! `main.rs` 只做三件事：读 `std::env::args()`、把参数交给 [`node_cli::run`]、
//! 把返回的退出码交出去——不内联任何分支逻辑，保证每一行都能被「spawn 二进制 +
//! 不同 argv」的集成测试走到（覆盖率门禁要求本文件 100%）。

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    node_cli::run(args, std::io::stdout(), std::io::stderr())
}
