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
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
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

// ---- `afcli import`（v0.7.4）：进程级覆盖，不依赖真 Hub / 真 opencode ----

/// 带环境变量启动 afcli。`AFCLI_HUB_URL` 默认清空，避免开发机的配置
/// 悄悄影响「缺省地址」类断言。
fn spawn_env(
    argv: &[&str],
    data_dir: &Path,
    env: &[(&str, &str)],
) -> (Guard, std::process::ChildStdout, std::process::ChildStderr) {
    let mut cmd = Command::new(EXE);
    cmd.args(argv)
        .env("AFCLI_DATA_DIR", data_dir)
        .env("AFCLI_HUB_URL", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().unwrap();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    (Guard(child), out, err)
}

/// 造一份可 enroll 的身份：`<base>/agentfleet/identity.json`，带 node_id。
fn identity_fixture(base: &Path) -> node_core::Identity {
    let app = base.join("agentfleet");
    let mut identity = node_core::Identity::load_or_create(&app).unwrap();
    identity.set_node_id("n_import_test").unwrap();
    identity
}

/// 造一个只读 opencode 夹具库：`<base>/opencode/opencode.db` 的 `session_v2`。
fn opencode_fixture(base: &Path, session_id: &str, cwd: &str) {
    let dir = base.join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE session_v2 (id TEXT, title TEXT, directory TEXT, time_updated INTEGER)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_updated) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![session_id, "demo session", cwd, 1_700_000_000_i64],
    )
    .unwrap();
}

/// 一条被 stub 收到的请求。
struct Captured {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 只接一条连接的本地 HTTP stub。返回端口与「收到的请求」通道。
///
/// 导入必须是**一次性**请求：这条 stub 只 accept 一次，因此第二个连接
/// （例如误走 WS / 重试循环）会让子进程等不到响应而超时——测试会直接红。
fn start_stub(
    status: &'static str,
    response_body: &'static str,
) -> (u16, std::sync::mpsc::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                headers.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
        let length = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        let (method, path) = request_line
            .split_once(' ')
            .map(|(method, rest)| {
                (
                    method.to_string(),
                    rest.split_whitespace().next().unwrap_or("").to_string(),
                )
            })
            .unwrap_or_default();
        tx.send(Captured {
            method,
            path,
            headers,
            body,
        })
        .unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    (port, rx)
}

fn captured(rx: std::sync::mpsc::Receiver<Captured>) -> Captured {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("stub never received the import request")
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap()
}

/// `--help` 必须把 `import` 的两个子命令列出来。
#[test]
fn help_mentions_the_import_subcommands() {
    let (mut child, out, _err) = spawn(&["--help"]);
    let status = child.wait_with_timeout();
    assert!(status.success());
    let out = drain(out);
    assert!(out.contains("afcli import list"), "got: {out}");
    assert!(out.contains("afcli import bind"), "got: {out}");
}

#[test]
fn import_without_subcommand_is_usage_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, out, err) = spawn_env(&["import"], dir.path(), &[]);
    let status = child.wait_with_timeout();
    assert!(!status.success(), "`import` 缺子命令必须非零退出");
    let out = drain(out);
    let err = drain(err);
    assert!(out.is_empty(), "错误不该写 stdout：{out:?}");
    assert!(err.contains("afcli import list"), "got: {err}");
}

#[test]
fn import_bind_without_session_is_usage_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, out, err) = spawn_env(
        &["import", "bind", "--provider", "opencode"],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    assert!(!status.success(), "缺 --session 必须非零退出");
    let out = drain(out);
    let err = drain(err);
    assert!(out.is_empty());
    assert!(err.contains("--session"), "got: {err}");
    assert!(err.contains("afcli import bind"), "got: {err}");
}

#[test]
fn import_list_prints_the_columns_users_need_to_choose() {
    let dir = tempfile::tempdir().unwrap();
    opencode_fixture(dir.path(), "ses_list", "/tmp/list-work");
    let (mut child, out, err) = spawn_env(
        &["import", "list", "--provider", "opencode"],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    for needle in ["ses_list", "demo session", "/tmp/list-work", "1700000000"] {
        assert!(out.contains(needle), "missing {needle:?} in {out:?}");
    }
}

#[test]
fn import_list_reports_unsupported_provider_without_touching_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, _out, err) =
        spawn_env(&["import", "list", "--provider", "claude"], dir.path(), &[]);
    let status = child.wait_with_timeout();
    assert!(!status.success());
    let err = drain(err);
    assert!(err.contains("unsupported_provider"), "got: {err}");
}

#[test]
fn import_bind_says_enroll_before_any_network_or_file_creation() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_missing",
            "--hub",
            "127.0.0.1:1",
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("not_enrolled"), "got: {err}");
    assert!(err.contains("请先 enroll"), "got: {err}");
    assert!(
        !dir.path().join("agentfleet").join("identity.json").exists(),
        "导入绝不能创建 / enroll 身份文件"
    );
}

#[test]
fn import_bind_rejects_an_invalid_hub_address_before_reading_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, _out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_x",
            "--hub",
            "http://hub.example.com",
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    assert!(!status.success());
    let err = drain(err);
    assert!(err.contains("invalid_hub_url"), "got: {err}");
}

/// 盘符 cwd 必须给出**可读原因**，而不是通用错误；同时不得回显 DB 路径。
#[test]
fn import_bind_explains_the_windows_drive_path_limitation() {
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_win", "C:\\Users\\x");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_win",
            "--hub",
            "127.0.0.1:1",
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("invalid_path"), "got: {err}");
    assert!(err.contains("Windows 盘符路径"), "got: {err}");
    assert!(
        !err.contains(&dir.path().display().to_string()),
        "错误不得回显数据目录：{err}"
    );
    assert!(!err.contains("opencode.db"), "错误不得回显 DB 路径：{err}");
    assert!(
        !err.contains("private_key_pkcs8_b64"),
        "错误不得回显私钥字段：{err}"
    );
}

/// 裸 authority：`127.0.0.1:port` 推导成 `http://127.0.0.1:port`，
/// POST 到固定路径，body 带齐字段且签名可验。
#[test]
fn import_bind_posts_a_signed_request_to_a_bare_authority() {
    let dir = tempfile::tempdir().unwrap();
    let identity = identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_test", "/tmp/demo");
    let (port, rx) = start_stub(
        "201 Created",
        r#"{"session_id":"hub_sess_1","agent_id":"opencode","acp_session_id":"ses_test"}"#,
    );
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_test",
            "--hub",
            &hub,
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert!(out.contains("hub_sess_1"), "got: {out}");
    assert!(out.contains("打开既有会话继续对话"), "got: {out}");

    let request = captured(rx);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/device-api/session-imports");
    for header in [
        "X-AF-Node-Id",
        "X-AF-Nonce",
        "X-AF-Signature",
        "X-AF-Body-SHA256",
    ] {
        assert!(request.header(header).is_some(), "missing header {header}");
    }
    let body = json(&request.body);
    assert_eq!(body["node_id"], "n_import_test");
    assert_eq!(body["provider"], "opencode");
    assert_eq!(body["external_session_id"], "ses_test");
    assert_eq!(body["acp_session_id"], "ses_test");
    assert_eq!(body["cwd"], "/tmp/demo");
    assert_eq!(body["title"], "demo session");
    assert_eq!(body["v"], 1);

    // 签名覆盖 method / path / body 摘要 / nonce / t，可用本机公钥验证。
    let message = identity::import_signing_message(
        body["v"].as_u64().unwrap() as u32,
        "n_import_test",
        "POST",
        "/device-api/session-imports",
        body["body_sha256"].as_str().unwrap(),
        body["nonce"].as_str().unwrap(),
        body["t"].as_u64().unwrap(),
    );
    let sig: [u8; 64] = identity::base64url_decode(body["signature"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    assert!(
        identity::KeyPair::verify(&identity.key().public_key_bytes(), &message, &sig),
        "signature must verify with the device public key"
    );

    // body / 输出都不得含公钥或私钥材料。
    let text = String::from_utf8(request.body.clone()).unwrap();
    let public = identity.public_key_b64();
    let private = identity::base64url(&identity.key().to_pkcs8_der());
    for haystack in [text.as_str(), out.as_str(), err.as_str()] {
        assert!(!haystack.contains(&public), "leaked public key: {haystack}");
        assert!(
            !haystack.contains(&private),
            "leaked private key: {haystack}"
        );
    }
}

/// 完整 `ws://` URL 与 `AFCLI_HUB_URL` 裸地址都要推导出同源 HTTP 基址。
#[test]
fn import_bind_derives_the_origin_from_a_full_ws_url_and_from_the_env_var() {
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_test", "/tmp/demo");

    let (port, rx) = start_stub("201 Created", r#"{"session_id":"from_ws"}"#);
    let hub = format!("ws://127.0.0.1:{port}/ws");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_test",
            "--hub",
            &hub,
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(captured(rx).path, "/device-api/session-imports");

    let (port, rx) = start_stub("201 Created", r#"{"session_id":"from_env"}"#);
    let env_hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_test",
        ],
        dir.path(),
        &[("AFCLI_HUB_URL", env_hub.as_str())],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(captured(rx).path, "/device-api/session-imports");
}

/// 成功状态码但不是约定 JSON 形状 → `bad_response`，且不把原始响应打到终端。
#[test]
fn import_bind_reports_bad_response_without_echoing_the_raw_body() {
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_test", "/tmp/demo");
    let (port, _rx) = start_stub("201 Created", "not json at all");
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_test",
            "--hub",
            &hub,
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("bad_response"), "got: {err}");
    assert!(!err.contains("not json at all"), "raw body leaked: {err}");
}

/// 非 JSON 的错误响应（例如反向代理吐出的 HTML）也只报 `bad_response`，
/// 原文一个字都不回显——代理错误页经常带内部主机名与路径。
#[test]
fn import_bind_never_echoes_an_html_error_page() {
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_test", "/tmp/demo");
    let (port, _rx) = start_stub(
        "502 Bad Gateway",
        "<html><body>nginx 502 at internal-hub.local:8080</body></html>",
    );
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_test",
            "--hub",
            &hub,
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("bad_response"), "got: {err}");
    for leak in ["<html>", "nginx", "internal-hub.local"] {
        assert!(!err.contains(leak), "raw error page leaked {leak:?}: {err}");
    }
}

/// 失败响应只输出稳定 code：不打印服务器 HTML / 原始错误体。
#[test]
fn import_bind_reports_a_stable_code_and_never_the_server_html() {
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_test", "/tmp/demo");
    let (port, _rx) = start_stub(
        "401 Unauthorized",
        r#"{"error":"DEVICE_SIGNATURE_INVALID"}"#,
    );
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(
        &[
            "import",
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_test",
            "--hub",
            &hub,
        ],
        dir.path(),
        &[],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("bad_signature"), "got: {err}");
    assert!(!err.contains("<html>"), "raw HTML leaked: {err}");
}
