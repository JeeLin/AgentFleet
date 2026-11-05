//! afhub 的**进程级**集成测试：真的 spawn `afhub` 二进制，跑完每条启动路径。
//!
//! 为什么必须起真进程：`main.rs` 里的「拒绝非回环地址」「端口被占」「优雅关闭」
//! 只有跨进程才可观测（退出码 + stderr + 信号），单进程里 mock 掉等于自证自话。
//! 覆盖率门禁要求 `src/main.rs` 行覆盖 100%，这些路径就靠本文件承接。
//!
//! 三条铁律（对应测试设计）：
//! 1. **子进程一定被回收** —— [`Guard`] 的 `Drop` 兜底 kill + wait，panic 也不会漏。
//! 2. **子进程超时一定有上限** —— [`wait_with_timeout`] 到点即 panic，绝不无限等。
//! 3. **环境变量显式设置或删除** —— 不继承测试进程的环境，否则 CI 上的
//!    `RUST_LOG` / `AFHUB_ADDR` 会让同一份代码在本地与 CI 走出不同分支。

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
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// 二进制路径由 Cargo 注入，与 `[[bin]] name = "afhub"` 对应。
const EXE: &str = env!("CARGO_BIN_EXE_afhub");

/// 子进程退出上限。超过即视为挂起并 panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
/// 等待 `/healthz` 就绪的上限（建库 + 迁移 + 监听都算在里面）。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// RAII 子进程守卫：无论测试是正常结束还是 panic，Drop 都会 kill + wait，
/// 不让 afhub 变成僵尸挂在测试进程后面。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        // 正常路径已经显式 wait 过；这里的 kill 只兜底异常退出（此时会返回 Err，忽略）。
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Guard {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn wait_with_timeout(&mut self) -> ExitStatus {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        loop {
            match self.0.try_wait().unwrap() {
                Some(status) => return status,
                None if Instant::now() >= deadline => {
                    let err = drain_stderr(&mut self.0);
                    panic!("afhub did not exit within {CHILD_TIMEOUT:?}; stderr:\n{err}");
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

/// 按环境变量表启动 afhub。表里 `None` 表示**删除**该变量（而不是让它继承）。
fn spawn(env: &[(&str, Option<&str>)]) -> Guard {
    let mut cmd = Command::new(EXE);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, value) in env {
        match value {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
    Guard(cmd.spawn().unwrap())
}

/// 跑一个**预期失败**的启动，返回 (退出状态, stderr)。
///
/// stderr 走管道：afhub 单次启动的日志量是几行，远小于 64 KiB 管道缓冲，
/// 不会因为「写满管道被阻塞」而需要另开线程读。
fn run_to_completion(env: &[(&str, Option<&str>)]) -> (ExitStatus, String) {
    let mut hub = spawn(env);
    let status = hub.wait_with_timeout();
    (status, drain_stderr(&mut hub.0))
}

/// 跑一个**预期失败**的启动，但**不接受「它其实起来了」**。
///
/// [`run_to_completion`] 在预期失败却真起了服务时只能干等 `CHILD_TIMEOUT` 再 panic，
/// 而「拒绝启动」类断言恰恰最需要**快速、清楚**地红：等 20 秒再报「非零退出失败」
/// 既不说明是哪个值放行的，也让本地跑一轮门禁变慢。所以这里给一个短的判定窗口，
/// 超时窗口内没退出 = 它绑上了 = 断言失败，并把用例参数带进消息里。
fn expect_startup_failure(env: &[(&str, Option<&str>)], what: &str) -> (ExitStatus, String) {
    /// 拒绝路径是纯计算（解析 + 判断），毫秒级；3s 只留给进程启动本身。
    const REFUSAL_TIMEOUT: Duration = Duration::from_secs(3);
    let mut hub = spawn(env);
    let deadline = Instant::now() + REFUSAL_TIMEOUT;
    loop {
        if let Some(status) = hub.0.try_wait().unwrap() {
            return (status, drain_stderr(&mut hub.0));
        }
        assert!(
            Instant::now() < deadline,
            "{what}: afhub 仍在运行，说明它接受了这个配置（预期应拒绝启动）"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn drain_stderr(child: &mut Child) -> String {
    let mut out = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut out).unwrap();
    }
    out
}

/// 给子进程发信号。用 `Child::kill` 不行——它固定发 SIGKILL，
/// 走不到 afhub 的优雅关闭路径（那是本文件要验证的行为之一）。
///
/// SAFETY：`kill(2)` 只按 pid 发信号；pid 来自本文件 spawn 的子进程，
/// 且调用点保证它还没被 `wait` 回收（此时至多是僵尸，pid 不会被复用）。
fn signal(pid: u32, sig: i32) {
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    assert_eq!(rc, 0, "kill({pid}, {sig}) failed");
}

/// 借一个当前空闲的回环端口。
///
/// 存在 TOCTOU 窗口（bind 成功后立刻 drop，到 afhub 再 bind 之间可能被抢走）。
/// 测试环境里可接受：真撞上了会表现为「端口占用」用例失败，不会静默通过。
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// 最小 HTTP/1.1 GET。用裸 TCP 而不是 reqwest：只求一个 200 + 响应体，
/// 引入 HTTP 客户端只会让「测试 afhub」的测试多一份依赖。
fn http_get(addr: SocketAddr, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 轮询 `/healthz` 直到 200，返回完整响应（状态行 + 头 + 体）。
fn wait_for_healthz(addr: SocketAddr) -> String {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(resp) = http_get(addr, "/healthz") {
            assert!(
                resp.starts_with("HTTP/1.1 200"),
                "/healthz must answer 200 once afhub is up, got: {resp}"
            );
            return resp;
        }
        if Instant::now() >= deadline {
            panic!("afhub did not serve /healthz within {STARTUP_TIMEOUT:?} at {addr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// 正常起服务：`AFHUB_ADDR` + `AFHUB_DATA_DIR` 都显式给，SIGTERM 后优雅退出 0。
#[test]
fn serves_healthz_and_exits_zero_on_sigterm() {
    let dir = tempdir();
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let mut hub = spawn(&[
        ("AFHUB_ADDR", Some(&format!("127.0.0.1:{port}"))),
        ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
        // 不设 RUST_LOG → 走 EnvFilter 的 fallback 分支
        ("RUST_LOG", None),
    ]);

    let resp = wait_for_healthz(addr);
    assert!(resp.contains("\"status\":\"ok\""), "got: {resp}");
    // 存活探针不泄露内部状态（设备数 / 版本 / 数据库路径）
    assert!(!resp.contains("devices"), "got: {resp}");
    assert!(!resp.contains("version"), "got: {resp}");
    // 数据目录真按 AFHUB_DATA_DIR 落
    assert!(
        dir.path().join("agentfleet.db").exists(),
        "{:?}",
        dir.path()
    );

    signal(hub.pid(), libc::SIGTERM);
    let status = hub.wait_with_timeout();
    assert!(
        status.success(),
        "SIGTERM 应触发优雅关闭并以 0 退出，实际 {status:?}"
    );
}

/// `AFHUB_DATA_DIR` 缺省 → 落 `XDG_DATA_HOME`（`dirs::data_dir()`），
/// 并且 `RUST_LOG` 给出的过滤器要生效（EnvFilter 的 Ok 分支）。
#[test]
fn falls_back_to_xdg_data_home_and_honors_rust_log() {
    let xdg = tempdir();
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let mut hub = spawn(&[
        ("AFHUB_ADDR", Some(&format!("127.0.0.1:{port}"))),
        ("AFHUB_DATA_DIR", None),
        ("XDG_DATA_HOME", Some(xdg.path().to_str().unwrap())),
        ("RUST_LOG", Some("hub_server=debug")),
    ]);

    let resp = wait_for_healthz(addr);
    assert!(resp.contains("\"status\":\"ok\""), "got: {resp}");
    assert!(
        xdg.path().join("agentfleet").join("agentfleet.db").exists(),
        "缺省数据目录应落在 XDG_DATA_HOME/agentfleet 下：{:?}",
        xdg.path()
    );

    signal(hub.pid(), libc::SIGINT);
    let status = hub.wait_with_timeout();
    assert!(
        status.success(),
        "SIGINT 应优雅关闭并退出 0，实际 {status:?}"
    );
}

/// `AFHUB_ADDR` 不接受主机名（含 `localhost`）：是否暴露给局域网不该依赖 DNS 解析，
/// 写 `localhost` 被当作「不是 socket 地址」拒绝，提示改用 `127.0.0.1`。
#[test]
fn rejects_hostname_addr() {
    let dir = tempdir();
    let (status, err) = run_to_completion(&[
        ("AFHUB_ADDR", Some("localhost:9999")),
        ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUST_LOG", None),
    ]);
    assert!(!status.success(), "主机名地址必须非零退出，stderr: {err}");
    assert!(
        err.contains("AFHUB_ADDR") && err.contains("not a socket address"),
        "stderr 应说明地址无法解析: {err}"
    );
}

/// 约束 1（缺省侧）：**不设开关**时非回环地址一律拒绝启动，不存在「先绑上去再说」。
///
/// v0.7.1 加了 `AFHUB_ALLOW_NON_LOOPBACK=1` 这个显式口子（Hub 容器需要绑
/// `0.0.0.0`），所以本用例必须**显式删掉**该变量而不是让它继承——CI 或本地开发
/// shell 里若恰好设了它，这条断言会静默失效，那正是「安全默认」最不该发生的坏法。
#[test]
fn rejects_non_loopback_addr_when_the_opt_in_is_absent() {
    let dir = tempdir();
    let (status, err) = expect_startup_failure(
        &[
            ("AFHUB_ADDR", Some("0.0.0.0:9999")),
            ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
            ("AFHUB_ALLOW_NON_LOOPBACK", None),
            ("RUST_LOG", None),
        ],
        "AFHUB_ALLOW_NON_LOOPBACK 未设置",
    );
    assert!(!status.success(), "非回环地址必须非零退出，stderr: {err}");
    assert!(err.contains("non-loopback"), "stderr 应说明拒绝原因: {err}");
    assert!(err.contains("AFHUB_ADDR"), "stderr 应点名配置项: {err}");
    // 拒绝信息要给出唯一的出路，否则用户只能去读源码
    assert!(
        err.contains("AFHUB_ALLOW_NON_LOOPBACK=1"),
        "stderr 应写明开关与唯一合法取值: {err}"
    );
    // 配置不合法就不该在磁盘上留下任何东西
    assert!(!dir.path().join("agentfleet.db").exists(), "{err}");
}

/// IPv6 通配地址同样拒绝（不是只挡 IPv4 的 `0.0.0.0`）。
#[test]
fn rejects_non_loopback_ipv6_addr() {
    let dir = tempdir();
    let (status, err) = expect_startup_failure(
        &[
            ("AFHUB_ADDR", Some("[::]:9999")),
            ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
            ("AFHUB_ALLOW_NON_LOOPBACK", None),
            ("RUST_LOG", None),
        ],
        "AFHUB_ADDR=[::]:9999",
    );
    assert!(!status.success(), "非回环地址必须非零退出，stderr: {err}");
    assert!(err.contains("non-loopback"), "stderr 应说明拒绝原因: {err}");
}

/// 开关的**反向**：除确切 `1` 以外的每个值都必须当作「没设」。
///
/// 这是安全开关，宽松解析的代价（`=0` 反而放开）在代码里看不出来，
/// 只有被扫到的那天才暴露。所以把常见误写逐个钉死，而不是只验 `1` 能用。
#[test]
fn the_opt_in_only_accepts_the_exact_value_one() {
    for value in ["0", "true", "yes", "", " 1", "1 "] {
        let dir = tempdir();
        let (status, err) = expect_startup_failure(
            &[
                ("AFHUB_ADDR", Some("0.0.0.0:9999")),
                ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
                ("AFHUB_ALLOW_NON_LOOPBACK", Some(value)),
                ("RUST_LOG", None),
            ],
            &format!("AFHUB_ALLOW_NON_LOOPBACK={value:?}"),
        );
        assert!(
            !status.success(),
            "AFHUB_ALLOW_NON_LOOPBACK={value:?} 不该放行，stderr: {err}"
        );
        assert!(
            err.contains("non-loopback"),
            "AFHUB_ALLOW_NON_LOOPBACK={value:?} 应在拒绝信息里说明原因: {err}"
        );
    }
}

/// 开关的**正向**：设成 `1` 之后非回环地址真的绑上了（不是「没报错」）。
///
/// 用**预先绑好**的监听器借端口：先 `bind("0.0.0.0:0")` 让内核挑一个空闲端口、
/// 拿回地址后立刻释放，再把那个端口交给 afhub。不能写 `AFHUB_ADDR=0.0.0.0:0`——
/// afhub 的 `afhub listening` 日志打的是**请求的**地址（`addr=0.0.0.0:0`），
/// 不是实际绑上的，从日志里抠端口这条路走不通（试过，会一直等到超时）。
///
/// 随后从 **127.0.0.1** 访问：绑在通配地址上时回环地址同样可达，这条断言因此
/// 能区分「真绑通了」与「其实只绑了回环」（后者连不上）。
#[test]
fn the_opt_in_lets_a_non_loopback_addr_bind() {
    let dir = tempdir();
    let port = {
        let l = TcpListener::bind("0.0.0.0:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let mut hub = spawn(&[
        ("AFHUB_ADDR", Some(&format!("0.0.0.0:{port}"))),
        ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("AFHUB_ALLOW_NON_LOOPBACK", Some("1")),
        ("RUST_LOG", None),
    ]);

    let resp = wait_for_healthz(addr);
    assert!(resp.contains("\"status\":\"ok\""), "got: {resp}");

    signal(hub.pid(), libc::SIGTERM);
    assert!(hub.wait_with_timeout().success(), "SIGTERM 后应优雅退出 0");
}

/// 配置写错形状（压根不是 socket 地址）→ 非零退出并点名变量。
#[test]
fn rejects_malformed_addr() {
    let dir = tempdir();
    let (status, err) = run_to_completion(&[
        ("AFHUB_ADDR", Some("not-an-address")),
        ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUST_LOG", None),
    ]);
    assert!(!status.success(), "非法地址必须非零退出，stderr: {err}");
    assert!(
        err.contains("AFHUB_ADDR") && err.contains("not a socket address"),
        "stderr 应说明地址无法解析: {err}"
    );
}

/// 端口被占（另一个进程已监听）→ bind 失败即非零退出，且说清是哪个地址。
#[test]
fn exits_nonzero_when_addr_port_taken() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let dir = tempdir();

    // `taken` 活到本函数结束 —— 端口全程被占住，afhub 的 bind 必然失败
    let (status, err) = run_to_completion(&[
        ("AFHUB_ADDR", Some(&format!("127.0.0.1:{port}"))),
        ("AFHUB_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUST_LOG", None),
    ]);

    assert!(!status.success(), "端口被占必须非零退出，stderr: {err}");
    assert!(err.contains("cannot bind"), "stderr 应说明绑定失败: {err}");
    assert!(
        err.contains(&format!("127.0.0.1:{port}")),
        "stderr 应点名绑不上的地址: {err}"
    );
    drop(taken);
}

/// 库回归：afcli/hub-server 都靠 `ensure_dir` 收紧权限，这里顺带确认
/// 集成测试用到的 tempdir 路径确实能落库（防止上面几条因路径问题「假失败」）。
#[test]
fn sanity_tempdir_is_writable() {
    let dir = tempdir();
    let probe = Path::new(dir.path()).join("probe");
    std::fs::write(&probe, b"ok").unwrap();
    assert_eq!(std::fs::read(&probe).unwrap(), b"ok");
}
