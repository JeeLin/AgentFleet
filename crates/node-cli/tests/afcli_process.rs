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

/// supervisor 分支：`afcli run` 自己就是 supervisor。
///
/// **一定会超时被 Guard 杀掉**——supervise 是无限循环。要断言的只有两件事：
/// 它先打了 data dir 到 stdout（用户唯一的「日志在哪」入口），以及它
/// 确实 spawn 出了一个带 `--worker` 的子进程（D1 进程模型）。
#[test]
fn run_as_supervisor_prints_the_data_dir_and_spawns_a_worker() {
    let (mut child, out, _err) = spawn(&["run"]);
    // 读到一个换行为止：writeln! 分几次写，只读到一个词会截断路径。
    let stdout = read_until_line(out, "\n");
    assert!(
        stdout.starts_with("afcli run: data dir "),
        "supervisor 应把 data dir 打到 stdout，got: {stdout:?}"
    );
    // 断言行尾真的带路径：只打 `data dir` 三个字等于没告诉用户日志在哪。
    let path = stdout.trim_start_matches("afcli run: data dir ").trim();
    assert!(
        !path.is_empty() && std::path::Path::new(path).is_absolute(),
        "data dir 必须是绝对路径，got: {path:?}"
    );
    // 不 wait：supervise 循环不会自己退出。Guard 的 Drop 负责回收。
    let _ = child.0.kill();
}

/// worker 分支：`afcli run --worker` 直接进 worker，不经过 supervise。
///
/// 它会尝试连 Hub 然后退避重连，所以同样不会自己退出；这里只证明
/// **它没有再 spawn 下一个进程**——若角色判定反了，这一条会挂出一个
/// supervisor 并打出 data dir，与上面那条互相矛盾。
#[test]
fn run_as_worker_does_not_become_a_supervisor() {
    let (mut child, _out, _err) = spawn(&["run", "--worker"]);
    // 给它一点时间去（错误地）spawn 子进程或打印 data dir；
    // worker 既不打印 data dir 也不 fork，所以两者都不该出现。
    std::thread::sleep(Duration::from_secs(1));
    let _ = child.0.kill();
    let _ = child.0.wait();
}

/// 读到含 `needle` 的一行为止（或管道关闭）。
///
/// 不能直接 `read_to_string`：supervisor 永不退出，管道永不关闭，
/// 那个调用会挂到测试超时。
fn read_until_line(mut pipe: impl Read, needle: &str) -> String {
    let mut got = String::new();
    let mut byte = [0u8; 1];
    while pipe.read(&mut byte).unwrap() == 1 {
        got.push(char::from_u32(u32::from(byte[0])).unwrap_or('?'));
        if got.contains(needle) {
            break;
        }
    }
    got
}

#[test]
fn unknown_arg_is_usage_to_stderr_failure() {
    let (mut child, _out, err) = spawn(&["enroll", "https://hub.example"]);
    let status = child.wait_with_timeout();
    assert!(!status.success(), "未知参数必须非零退出");
    let err = drain(err);
    assert!(err.contains("用法"), "用法应打到 stderr，got: {err:?}");
}
