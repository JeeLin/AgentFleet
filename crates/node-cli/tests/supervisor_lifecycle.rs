//! 子任务 4 的**进程级**测试：真 Hub + 真 `afcli` supervisor/worker。
//!
//! 测的是 D1/D2 那条承诺：**换 worker 期间设备身份不变，Hub 不判离线**。
//! supervisor 根本不持有 WS，所以它自身不断线；被换掉的只是 worker 进程，
//! 而身份（密钥对 + `node_id`）活在 worker 里、由 Hub 按指纹认。
//!
//! 这些行为只能在真进程上验证——「spawn → kill → 重拉」在单测里等于递归起自己。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const EXE: &str = env!("CARGO_BIN_EXE_afcli");

struct Hub {
    addr: std::net::SocketAddr,
    state: AppState,
    _dir: tempfile::TempDir,
    _shutdown: Option<oneshot::Sender<()>>,
}

impl Hub {
    async fn boot() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let state = AppState::bootstrap_with(
            pool,
            dir.path(),
            AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        state
            .auth()
            .setup_password("correct horse battery")
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let app = hub_server::build_app(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        });
        std::mem::forget(task);
        Self {
            addr,
            state,
            _dir: dir,
            _shutdown: Some(tx),
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// 等某台设备的连接出现在注册表里。
    ///
    /// **必须走 `spawn_blocking`**：它内部是同步轮询，直接在 `#[tokio::test]`
    /// 里调会占住当前线程，而另一个 test（或本 test 的其它 await）就没机会
    /// 推进——表现为整体挂死而不是某一条失败。
    async fn wait_connected(&self, budget: Duration) -> String {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + budget;
            loop {
                let found = state.connections().node_ids().into_iter().next();
                if let Some(id) = found {
                    return id;
                }
                assert!(Instant::now() < deadline, "设备始终没连上来");
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .await
        .expect("wait_connected 的阻塞线程不得 panic")
    }

    fn connected(&self) -> Option<String> {
        self.state.connections().node_ids().into_iter().next()
    }

    fn status_of(&self, node_id: &str) -> Option<String> {
        self.state
            .devices()
            .find_by_id(node_id)
            .unwrap()
            .map(|d| d.status.as_str().to_string())
    }
}

/// 标准②③：supervisor 拉起 worker 连上 Hub；kill 掉 worker 之后
/// **同一个 node_id** 重新连上来（身份不随进程变）。
///
/// 这是 D1 的整机承诺：身份在 worker 里、由 Hub 按指纹认，所以换进程
/// 不等于换设备。若身份挂在 supervisor 上或每次 spawn 都重新 enroll，
/// 这里会看到两个不同的 node_id。
#[tokio::test]
async fn a_killed_worker_comes_back_with_the_same_identity() {
    let hub = Hub::boot().await;
    let data_dir = tempfile::tempdir().unwrap();

    let mut child = Command::new(EXE)
        .args(["run"])
        .env("AFCLI_HUB_URL", hub.ws_url())
        .env("AFCLI_DATA_DIR", data_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let first = hub.wait_connected(Duration::from_secs(20)).await;

    // 找到并 kill 掉 worker（worker 是 supervisor 的子进程）
    let Some(pid) = worker_pid_of(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("找不到 worker 子进程");
    };
    kill_worker(pid);

    // 同一个 node_id 必须回来（身份不随进程变）。
    // 只断言「回来」，不断言「先消失」：WS 关闭到 Hub 摘表之间存在一个窗口，
    // 期间连接仍在表里，而 supervisor 重拉很快。允许「从未去掉」是故意的——
    // 这条测的是身份稳定性，不是「必须断一次」。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if hub.connected().as_deref() == Some(first.as_str()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker 被 kill 之后必须以同一个 node_id 回来"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 且 Hub 侧没有第二个 node_id——若换进程就换了身份，这里会有两条记录
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let ids = hub.state.connections().node_ids();
        if ids.len() > 1 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("换 worker 之后出现了多个 node_id：{ids:?}");
        }
        if Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// `kill -9` 一个 worker。
///
/// `-9` 是标准②③ 要的形状：被信号杀死时 `ExitStatus::code()` 是 `None`，
/// supervisor 必须把它当成 `CRASH`(12) 而不是让 `-9` 落进 i32 里。
fn kill_worker(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// 取 supervisor 的子进程 pid（worker）。
fn worker_pid_of(parent: u32) -> Option<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().and_then(|l| l.trim().parse().ok())
}

/// 标准③ 的后半：换 worker 期间 Hub **不判离线**。
///
/// supervisor 自身不持连接，所以「换 worker」对 Hub 来说只是「这条 WS 断了
/// 又连上」；而离线判据是心跳年龄（45s），2–5s 的重连窗口远不到阈值。
/// 这条锁住的是「不清内存上报」这个决定（子任务 6）——若断开即清，
/// 换 worker 的瞬间设备就会从调度视野里消失。
#[tokio::test]
async fn swapping_the_worker_does_not_mark_the_device_offline() {
    let hub = Hub::boot().await;
    let data_dir = tempfile::tempdir().unwrap();

    let mut child = Command::new(EXE)
        .args(["run"])
        .env("AFCLI_HUB_URL", hub.ws_url())
        .env("AFCLI_DATA_DIR", data_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let node_id = hub.wait_connected(Duration::from_secs(20)).await;
    // 等一次心跳落库，确保 status 已经是 online
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if hub.status_of(&node_id).as_deref() == Some("online") {
            break;
        }
        assert!(Instant::now() < deadline, "心跳始终没把 status 刷成 online");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 换 worker 期间持续采样：status 不得出现 offline
    if let Some(pid) = worker_pid_of(child.id()) {
        kill_worker(pid);
    }
    // 采样窗口 20s：supervisor 退避从 1s 起，worker 启动 + 握手 + 首次心跳
    // 都在里面。远小于 45s 的离线阈值，所以「不判离线」在这个窗口内仍是强断言。
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        assert_ne!(
            hub.status_of(&node_id).as_deref(),
            Some("offline"),
            "换 worker 期间不得被判离线——那会让进行中的会话被调度剔除"
        );
        if hub.connected().as_deref() == Some(node_id.as_str()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        hub.connected().as_deref(),
        Some(node_id.as_str()),
        "换 worker 之后必须以同一个 node_id 重连"
    );

    let _ = child.kill();
    let _ = child.wait();
}
