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

use futures_util::StreamExt as _;
use hub_core::auth::{AuthParams, LoginRateLimiter};
use hub_server::AppState;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const EXE: &str = env!("CARGO_BIN_EXE_afcli");

/// 「等连接成立」的安全网。
///
/// **它不是断言**：正常路径由 Hub 推的 `device.status` 事件唤醒
/// （见 [`Hub::wait_connected`]），这段时长只在「Hub 再也不推事件」时才走到，
/// 作用是避免挂死而不是判快慢。原先这里是一个 20s 的**预算**，于是机器忙时
/// 预算被吃光而用例变红（观测到的失败：24.79s 超时，而单独跑只用 6.0–6.5s）
/// ——把环境慢编成了代码错。
const CONNECT_SAFETY_NET: Duration = Duration::from_secs(120);

/// 轮询间隔（[`Hub::wait_until`]）。
///
/// 它只影响「条件成立后多久被发现」，不影响判定——所以取一个能立刻发现
/// 变化又不会空转的值即可，与机器快慢无关。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Hub 侧的设备事件流：`GET /api/stream` 的那一条，只是不经 HTTP。
///
/// 类型直接取 `Streams::subscribe` 的返回：那条流是 `'static` 的（不借
/// `AppState`），所以夹具能把它存成字段，订阅点也因此能早于 spawn。
type DeviceEvents = futures_util::stream::BoxStream<
    'static,
    Result<axum::response::sse::Event, std::convert::Infallible>,
>;

struct Hub {
    addr: std::net::SocketAddr,
    state: AppState,
    /// **在 spawn 之前**建好的事件订阅（见 [`Hub::wait_connected`]）。
    events: DeviceEvents,
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
            // **订阅必须在 spawn 之前**：设备连接成立时推的
            // `device.status{online}` 只播一次，晚一步订阅就永远收不到，
            // 等的人于是只能等满安全网。
            events: state.streams().subscribe(None),
            addr,
            state,
            _dir: dir,
            _shutdown: Some(tx),
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// 等某台设备的连接成立，返回它的 `node_id`。
    ///
    /// **唤醒源是 Hub 推的事件，不是墙钟**：连接成立在 Hub 侧只有一个可观测
    /// 时刻——`handle_socket` 插表之后推一条 `device.status{online}`
    /// （`hub-server/src/routes/ws.rs`）。所以夹具在 spawn 之前就订阅好，
    /// 这里只 `next()`：连接一成立立刻返回，机器忙只是让事件晚到。
    ///
    /// 原先是「20s 预算里每 50ms 轮询一次注册表」——预算由墙钟给，于是并发
    /// 跑满（34 个测试二进制）时预算被吃光而超时（观测到的失败：24.79s，
    /// 而单独跑这条只用 6.0–6.5s）。改事件唤醒之后墙钟不再进入判定：
    /// 「等多久」由连接何时成立决定，「等到没有」才会失败。
    ///
    /// **判据仍是注册表**（与原先同一个）：事件只决定「什么时候去看一眼」，
    /// `device.status` 是页面语义，不该被当成连接表的替身。
    async fn wait_connected(&mut self) -> String {
        loop {
            let left =
                (Instant::now() + CONNECT_SAFETY_NET).saturating_duration_since(Instant::now());
            // 安全网只防挂死：Hub 一条事件都不推时（设备根本没起来）不会有
            // 唤醒，那时与其等测试进程被 kill，不如现在就失败。
            let arrived = tokio::time::timeout(left, self.events.next()).await;
            assert!(
                matches!(arrived, Ok(Some(Ok(_)))),
                "设备始终没连上来（Hub 一条事件都没推）"
            );
            if let Some(id) = self.connected() {
                return id;
            }
        }
    }

    /// 等到 `want` 给出 `Some`。
    ///
    /// **墙钟不参与判定**：循环只在条件成立时退出，[`CONNECT_SAFETY_NET`]
    /// 那条线在正常机器上永远走不到（实际耗时是 1–5s 量级），它唯一的作用
    /// 是把「永远不成立」变成失败而不是挂死。原先这里是「预算内轮询」，
    /// 预算由墙钟给，于是机器忙时被吃光而红的不是代码而是时钟。
    ///
    /// 轮询而不是事件：心跳落库不推事件（只有连接成立/断开与通道帧推），
    /// 而 `status` 正是心跳写的——这类条件没有可订阅的信号。
    async fn wait_until<T>(&self, mut want: impl FnMut(&Hub) -> Option<T>, msg: &str) -> T {
        let deadline = Instant::now() + CONNECT_SAFETY_NET;
        loop {
            if let Some(got) = want(self) {
                return got;
            }
            assert!(Instant::now() < deadline, "{msg}");
            tokio::time::sleep(POLL_INTERVAL).await;
        }
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
    let mut hub = Hub::boot().await;
    let data_dir = tempfile::tempdir().unwrap();

    let mut child = Command::new(EXE)
        .args(["run"])
        .env("AFCLI_HUB_URL", hub.ws_url())
        .env("AFCLI_DATA_DIR", data_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let first = hub.wait_connected().await;

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
    //
    // 循环只在条件成立时退出，超时那条线正常机器上走不到：原先它是 30s
    // 预算，于是「重拉慢了」被判成「身份变了」。
    let back = hub
        .wait_until(
            |hub| hub.connected().filter(|id| id == &first),
            "worker 被 kill 之后必须以同一个 node_id 回来",
        )
        .await;
    assert_eq!(back, first);

    // 且 Hub 侧的**设备只注册过一次**——若换进程就换了身份，库里会有两行。
    //
    // 判「库里有几行」而不是「某个采样窗口里连接表有没有出现过两条」：
    // 后者在慢机器上恒真（窗口里没采到不等于没发生过），而 enroll 落库是
    // 累积事实，与时钟无关。
    let devices = hub.state.devices().list().unwrap();
    assert_eq!(
        devices.len(),
        1,
        "换 worker 之后出现了多个 node_id：{:?}",
        devices.iter().map(|d| &d.id).collect::<Vec<_>>()
    );

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
    let mut hub = Hub::boot().await;
    let data_dir = tempfile::tempdir().unwrap();

    let mut child = Command::new(EXE)
        .args(["run"])
        .env("AFCLI_HUB_URL", hub.ws_url())
        .env("AFCLI_DATA_DIR", data_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let node_id = hub.wait_connected().await;
    // 等一次心跳落库，确保 status 已经是 online。
    // 心跳到达的时刻由设备决定，这里只等「它成立了」，不等「多久内成立」。
    let status = hub
        .wait_until(
            |hub| hub.status_of(&node_id).filter(|s| s == "online"),
            "心跳始终没把 status 刷成 online",
        )
        .await;
    assert_eq!(status, "online");

    // 换 worker 期间不得被判离线。
    if let Some(pid) = worker_pid_of(child.id()) {
        kill_worker(pid);
    }
    // 等「重连成功」这个确定事实，并在**等到的那一刻**断言 status 不是
    // offline，而不是在 20s 窗口里反复采样：窗口那条写法里「不判离线」的
    // 强度取决于采样频率，而频率在慢机器上会掉——采不到 offline 与
    // 没有 offline 混成同一句断言。重连成立时设备必然刚经历过一次断开，
    // 而离线判据是心跳年龄（45s），重连窗口远不到阈值。
    let back = hub
        .wait_until(
            |hub| hub.connected().filter(|id| id == &node_id),
            "换 worker 之后必须以同一个 node_id 重连",
        )
        .await;
    assert_eq!(back, node_id);
    assert_ne!(
        hub.status_of(&node_id).as_deref(),
        Some("offline"),
        "换 worker 期间不得被判离线——那会让进行中的会话被调度剔除"
    );

    let _ = child.kill();
    let _ = child.wait();
}
