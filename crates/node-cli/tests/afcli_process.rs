//! afcli 的**进程级**集成测试：真的 spawn `afcli` 二进制，覆盖 `main.rs` 的每条路径。
//!
//! `main.rs` 极薄——读 `std::env::args()` 后整条转发给 [`node_cli::run`]——
//! 所以这里的三种 argv 组合（无参 / `--version` / `--help` / 未知参数）正好逐一命中
//! `main.rs` 的每一行（收集 args → 调 `run` → 返回退出码）。覆盖率门禁要求
//! `src/main.rs` 行覆盖 100%，就靠本文件承接。
//!
//! 铁律：子进程一定被回收（[`Guard`] 兜底 kill+wait），且退出**必有上限**
//! （[`wait_with_timeout`] 到点 panic，绝不无限等）。

// Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

// 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
// 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
// let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
// 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// 二进制路径由 Cargo 注入，与 `[[bin]] name = "afcli"` 对应。
const EXE: &str = env!("CARGO_BIN_EXE_afcli");

/// 子进程退出上限。超过即视为挂起并 panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(20);

/// RAII 子进程守卫：异常退出也兜底 kill + wait，不留僵尸。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Guard {
    fn wait_with_timeout(&mut self) -> ExitStatus {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        loop {
            match self.0.try_wait().unwrap() {
                Some(status) => return status,
                None if Instant::now() >= deadline => {
                    panic!("afcli did not exit within {CHILD_TIMEOUT:?}");
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

/// 按 argv 启动 afcli，stdout/stderr 走管道（输出量极小，无需另开读线程）。
fn spawn(argv: &[&str]) -> (Guard, std::process::ChildStdout, std::process::ChildStderr) {
    let mut cmd = Command::new(EXE);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    (Guard(child), out, err)
}

fn drain(mut pipe: impl Read) -> String {
    let mut s = String::new();
    pipe.read_to_string(&mut s).unwrap();
    s
}

#[test]
fn version_succeeds_and_prints_version() {
    let (mut child, out, _err) = spawn(&["--version"]);
    let status = child.wait_with_timeout();
    assert!(status.success(), "--version 必须退出 0");
    let out = drain(out);
    assert!(
        out.starts_with("afcli "),
        "版本行应以 `afcli ` 开头，got: {out:?}"
    );
    assert!(
        out.trim().contains(env!("CARGO_PKG_VERSION")),
        "版本行应含 CARGO_PKG_VERSION，got: {out:?}"
    );
}

#[test]
fn help_succeeds_and_prints_usage() {
    let (mut child, out, _err) = spawn(&["--help"]);
    let status = child.wait_with_timeout();
    assert!(status.success(), "--help 必须退出 0");
    let out = drain(out);
    assert!(out.contains("用法"), "帮助应含用法说明，got: {out:?}");
}

#[test]
fn no_args_is_usage_to_stderr_failure() {
    let (mut child, _out, err) = spawn(&[]);
    let status = child.wait_with_timeout();
    assert!(!status.success(), "无参数必须非零退出");
    let err = drain(err);
    assert!(err.contains("用法"), "用法应打到 stderr，got: {err:?}");
}

#[test]
fn unknown_arg_is_usage_to_stderr_failure() {
    let (mut child, _out, err) = spawn(&["enroll", "https://hub.example"]);
    let status = child.wait_with_timeout();
    assert!(!status.success(), "未知参数必须非零退出");
    let err = drain(err);
    assert!(err.contains("用法"), "用法应打到 stderr，got: {err:?}");
}
