//! afcli 的**进程级**集成测试：真的 spawn `afcli` 二进制，覆盖 `main.rs` 的每条路径。
//!
//! `main.rs` 极薄——读 `std::env::args()` 后整条转发给 [`node_cli::run`]——
//! 所以这里的四种 argv 组合（无参 / `--version` / `--help` / 未知参数）正好逐一命中
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

use identity::import_protocol::{
    BODY_SHA256_HEADER, IMPORT_METHOD, IMPORT_PATH, IMPORT_VERSION, NODE_ID_HEADER, NONCE_HEADER,
    SIGNATURE_HEADER,
};

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

/// 读一个完整的 HTTP 请求并回一段固定响应，返回 stub 捕获的请求。
///
/// 两种 stub（单连接 / 带连接计数）共用这一份：改 `Content-Length` 拼法或
/// 头解析只改这里，不会出现「只修了一个 stub」。
fn capture_and_reply(
    mut stream: std::net::TcpStream,
    status: &'static str,
    response_body: &'static str,
) -> Captured {
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
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.flush().unwrap();
    Captured {
        method,
        path,
        headers,
        body,
    }
}

/// `afcli import bind` 的 argv（8 元素，`--hub` 包含在内）。
///
/// 本文件里 8 处手搓过同一串：漏改一处（比如 `--provider` 写成 `--prov`）不会
/// 编译失败，只会在运行时变成「未知参数」——抽成一处，argv 形状只有一个来源。
fn bind_argv<'a>(session: &'a str, hub: &'a str) -> Vec<&'a str> {
    vec![
        "import",
        "bind",
        "--provider",
        "opencode",
        "--session",
        session,
        "--hub",
        hub,
    ]
}

/// 与 [`bind_argv`] 相同但**不带 `--hub`**：地址由 `AFCLI_HUB_URL` 提供。
fn bind_argv_without_hub(session: &str) -> Vec<&str> {
    vec![
        "import",
        "bind",
        "--provider",
        "opencode",
        "--session",
        session,
    ]
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
        let (stream, _) = listener.accept().unwrap();
        let captured = capture_and_reply(stream, status, response_body);
        tx.send(captured).unwrap();
    });
    (port, rx)
}

/// 与 [`start_stub`] 相同，另外把「第二个 TCP 连接」计数报到 `extra`。
///
/// 第一个连接照常回一次 HTTP 响应；之后监听 socket 仍保持，在短窗口内继续
/// accept 并给每个多余连接 +1。`cli_never_calls_connect_once_or_spawns_worker`
/// 用它把「POST 之后没有第二条 WS/HTTP 连接」从注释变成可执行断言。
fn start_stub_with_connection_count(
    status: &'static str,
    response_body: &'static str,
    extra: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> (
    u16,
    std::sync::mpsc::Receiver<Captured>,
    std::sync::mpsc::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener
        .set_nonblocking(true)
        .expect("stub must poll, not block forever");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = accept_with_deadline(&listener, Duration::from_secs(10))
            .expect("stub never received the import request");
        // 与 [`start_stub`] 共用同一份「读请求 + 回响应」：两处若各自维护，
        // 改 `Content-Length` 拼法就必须同时改两遍。
        let captured = capture_and_reply(stream, status, response_body);
        tx.send(captured).unwrap();

        // 子进程拿到响应后若再发起 connect_once / worker 连接，这里会看见。
        // 不做无限 accept：短窗口足够覆盖同进程退出前的第二条连接。
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut extra_stream, _)) => {
                    extra.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = extra_stream.write_all(b"HTTP/1.1 204 No Content\r\n\r\n");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let _ = done_tx.send(());
    });
    (port, rx, done_rx)
}

fn accept_with_deadline(
    listener: &TcpListener,
    timeout: Duration,
) -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(pair) => return Ok(pair),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
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
    let (mut child, out, err) =
        spawn_env(&bind_argv("ses_missing", "127.0.0.1:1"), dir.path(), &[]);
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
        &bind_argv("ses_x", "http://hub.example.com"),
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
    let (mut child, out, err) = spawn_env(&bind_argv("ses_win", "127.0.0.1:1"), dir.path(), &[]);
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
///
/// 顺带锁住桩响应**恰好两个键**：以前这里是三字段（多一个 `agent_id`），
/// CLI 照常成功，于是「Hub 只返回两个字段」这条契约没有任何可执行约束。
#[test]
fn import_bind_posts_a_signed_request_to_a_bare_authority() {
    let dir = bind_data_dir();
    let identity = node_core::Identity::load(&dir.path().join("agentfleet").join("identity.json"))
        .expect("identity fixture");
    // 与 Hub `ImportResponse` 完全同形：恰好两个键，不带 `agent_id`。
    let (port, rx) = start_stub(
        "201 Created",
        r#"{"session_id":"hub_sess_1","acp_session_id":"ses_test"}"#,
    );
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(&bind_argv("ses_test", &hub), dir.path(), &[]);
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert!(out.contains("hub_sess_1"), "got: {out}");
    assert!(out.contains("打开既有会话继续对话"), "got: {out}");

    let request = captured(rx);
    assert_request_shape(&request);
    let body = assert_body_fields(&request.body);
    assert_signature_verifies(&identity, &body);
    assert_no_key_material(&identity, &request.body, &out, &err);
}

/// 请求行 + 四个签名头都在，且打的是导入的固定方法与路径。
fn assert_request_shape(request: &Captured) {
    assert_eq!(request.method, IMPORT_METHOD);
    assert_eq!(request.path, IMPORT_PATH);
    for header in [
        NODE_ID_HEADER,
        NONCE_HEADER,
        SIGNATURE_HEADER,
        BODY_SHA256_HEADER,
    ] {
        assert!(request.header(header).is_some(), "missing header {header}");
    }
}

/// body 是 CLI 该发的那串字段（值取自 `bind_data_dir` 的夹具）。
fn assert_body_fields(raw: &[u8]) -> serde_json::Value {
    let body = json(raw);
    assert_eq!(body["node_id"], "n_import_test");
    assert_eq!(body["provider"], "opencode");
    assert_eq!(body["external_session_id"], "ses_test");
    assert_eq!(body["acp_session_id"], "ses_test");
    assert_eq!(body["cwd"], "/tmp/demo");
    assert_eq!(body["title"], "demo session");
    assert_eq!(body["v"], IMPORT_VERSION);
    body
}

/// 签名覆盖 method / path / body 摘要 / nonce / `t`，可用本机公钥验证。
///
/// 换成 Hub 侧 `import_auth` 的任一字段拼法都会失败——这条锁住「两端规范串
/// 由 `identity::import_signing_message` 唯一实现」。
fn assert_signature_verifies(identity: &node_core::Identity, body: &serde_json::Value) {
    let message = identity::import_signing_message(
        body["v"].as_u64().unwrap() as u32,
        "n_import_test",
        IMPORT_METHOD,
        IMPORT_PATH,
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
}

/// body 与 stdout / stderr 都不得含公钥或私钥材料（约束 13）。
fn assert_no_key_material(identity: &node_core::Identity, raw: &[u8], out: &str, err: &str) {
    let text = String::from_utf8(raw.to_vec()).unwrap();
    let public = identity.public_key_b64();
    let private = identity::base64url(&identity.key().to_pkcs8_der());
    for haystack in [text.as_str(), out, err] {
        assert!(!haystack.contains(&public), "leaked public key: {haystack}");
        assert!(
            !haystack.contains(&private),
            "leaked private key: {haystack}"
        );
    }
}

/// `import bind` 是**一次性 HTTP**：不接第二条 WS，也不启动 worker / supervisor。
///
/// 两层证据缺一不可：① 生产源码里没有 `connect_once` / `Command::new` / `spawn`；
/// ② 真进程拿到 201 后，stub 在短观察窗口内没有收到第二条 TCP 连接。只做 ①
/// 是注释式断言，只做 ② 又可能被「第二条连接失败也照样绿」骗过。
#[test]
fn cli_never_calls_connect_once_or_spawns_worker() {
    let import_cmd = include_str!("../src/import_cmd.rs");
    let import_http = include_str!("../src/import_http.rs");
    for forbidden in ["connect_once(", "Command::new", ".spawn(", "spawn("] {
        assert!(
            !import_cmd.contains(forbidden),
            "import_cmd.rs must not contain `{forbidden}`"
        );
        assert!(
            !import_http.contains(forbidden),
            "import_http.rs must not contain `{forbidden}`"
        );
    }

    // 这条用例的会话 id 与其它用例不同（它要 `ses_once` 的回显），
    // 所以只有身份那一步能共用 `bind_data_dir`；库另行造。
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_once", "/tmp/demo");
    let extra = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (port, rx, done_rx) = start_stub_with_connection_count(
        "201 Created",
        r#"{"session_id":"hub_sess_once","acp_session_id":"ses_once"}"#,
        extra.clone(),
    );
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(&bind_argv("ses_once", &hub), dir.path(), &[]);
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(captured(rx).method, IMPORT_METHOD);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("stub did not finish observing connections");
    assert_eq!(
        extra.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "import bind opened a second TCP/WS connection"
    );
}

/// 完整 `ws://` URL 与 `AFCLI_HUB_URL` 裸地址都要推导出同源 HTTP 基址。
#[test]
fn import_bind_derives_the_origin_from_a_full_ws_url_and_from_the_env_var() {
    let dir = bind_data_dir();

    let (port, rx) = start_stub(
        "201 Created",
        r#"{"session_id":"from_ws","acp_session_id":"ses_test"}"#,
    );
    let hub = format!("ws://127.0.0.1:{port}/ws");
    let (mut child, out, err) = spawn_env(&bind_argv("ses_test", &hub), dir.path(), &[]);
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(captured(rx).path, IMPORT_PATH);

    let (port, rx) = start_stub(
        "201 Created",
        r#"{"session_id":"from_env","acp_session_id":"ses_test"}"#,
    );
    let env_hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(
        &bind_argv_without_hub("ses_test"),
        dir.path(),
        &[("AFCLI_HUB_URL", env_hub.as_str())],
    );
    let status = child.wait_with_timeout();
    let out = drain(out);
    let err = drain(err);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(captured(rx).path, IMPORT_PATH);
}

/// 「临时数据目录 + 身份文件 + 一条 opencode 历史」三件套。
///
/// 这是 bind 用例共同的前置：三者缺一，`import bind` 会停在
/// `not_enrolled` / `provider_unavailable` / `invalid_session` 上，红的原因
/// 与用例想测的那一层无关。抽成一处，`tempdir` 的生命周期只有一种写法。
///
/// 返回 `TempDir`：**必须**活到子进程退出，否则库与身份文件会先被删掉。
fn bind_data_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    identity_fixture(dir.path());
    opencode_fixture(dir.path(), "ses_test", "/tmp/demo");
    dir
}

/// 五个 bind 响应用例共有的夹具：临时数据目录 + 身份 + opencode 库 + stub。
///
/// `tempdir` 必须活到子进程退出，所以和 `(port, receiver)` 一起返回。
/// 新增一个 4xx code 用例只需给 `status` / `body` 两个参数 + 末尾断言，
/// 不再复制七元 argv 与三份 fixture 调用。
fn bind_fixture(
    status: &'static str,
    response_body: &'static str,
) -> (tempfile::TempDir, u16, std::sync::mpsc::Receiver<Captured>) {
    let dir = bind_data_dir();
    let (port, rx) = start_stub(status, response_body);
    (dir, port, rx)
}

/// 用夹具跑一遍 `import bind`，返回 `(退出码, stdout, stderr)`。
fn run_bind_fixture(port: u16, dir: &Path) -> (ExitStatus, String, String) {
    let hub = format!("127.0.0.1:{port}");
    let (mut child, out, err) = spawn_env(&bind_argv("ses_test", &hub), dir, &[]);
    let status = child.wait_with_timeout();
    (status, drain(out), drain(err))
}

/// 成功状态码但不是约定 JSON 形状 → `bad_response`，且不把原始响应打到终端。
#[test]
fn import_bind_reports_bad_response_without_echoing_the_raw_body() {
    let (dir, port, _rx) = bind_fixture("201 Created", "not json at all");
    let (status, out, err) = run_bind_fixture(port, dir.path());
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("bad_response"), "got: {err}");
    assert!(!err.contains("not json at all"), "raw body leaked: {err}");
}

/// 非 JSON 的错误响应（例如反向代理吐出的 HTML）也只报 `bad_response`，
/// 原文一个字都不回显——代理错误页经常带内部主机名与路径。
#[test]
fn import_bind_never_echoes_an_html_error_page() {
    let (dir, port, _rx) = bind_fixture(
        "502 Bad Gateway",
        "<html><body>nginx 502 at internal-hub.local:8080</body></html>",
    );
    let (status, out, err) = run_bind_fixture(port, dir.path());
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
    let (dir, port, _rx) = bind_fixture(
        "401 Unauthorized",
        r#"{"error":"DEVICE_SIGNATURE_INVALID"}"#,
    );
    let (status, out, err) = run_bind_fixture(port, dir.path());
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(err.contains("bad_signature"), "got: {err}");
    assert!(!err.contains("<html>"), "raw HTML leaked: {err}");
}
