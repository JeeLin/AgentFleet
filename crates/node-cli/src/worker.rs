//! worker 角色：持有 identity + WS 连接，跑消息循环与退避重连（D1、子任务 6）。
//!
//! **worker 持连接、supervisor 持进程**，不是反过来（D1）：升级换掉的正是
//! worker 这个文件，而 supervisor 那份代码必须是没被替换过的旧映像——
//! 「执行回滚的代码不参与被回滚」。
//!
//! 身份（`identity.json`）由 worker 打开：它是设备身份，不是进程身份
//! （约束 12：升级只换 worker，`node_id` 不能变）。

use std::path::PathBuf;
use std::time::Duration;

use crate::filelog::{self, ROLE_WORKER};
use node_core::conn::{self, ConnOutcome, Established};
use node_core::reconnect;
use node_core::supervisor::exit_code;
use node_core::Identity;
use protocol::frame::Frame;
use protocol::message::{DeviceInfo, Disconnect};

/// 一次已认证连接上的循环被什么终止了。
enum SessionEnd {
    /// 对端关闭了 WS（`Close` 帧走完，或读到流结束）。
    PeerClosed,
    /// 连接层失败 → 该退避重连。
    Transport(String),
    /// 被顶替：终态，不重连（§7.1）。
    Superseded { gen: u64 },
}

/// worker 需要的全部东西。
pub struct Worker {
    data_dir: PathBuf,
    hub_url: String,
}

impl Worker {
    pub fn new(data_dir: PathBuf, hub_url: String) -> Self {
        Self { data_dir, hub_url }
    }

    /// 阻塞运行，返回进程退出码。正常情况下不返回。
    pub fn work(&mut self) -> std::process::ExitCode {
        self.rotate_log();
        let mut identity = match Identity::load_or_create(&self.data_dir) {
            Ok(id) => id,
            // 身份文件坏了/读不了时**不能**重新生成：那等于换了一台设备，
            // 会撞上「指纹已存在则拒绝」，设备永久无法接入。所以停下来。
            Err(e) => {
                self.say(&format!("error: cannot load identity: {e}"));
                return std::process::ExitCode::FAILURE;
            }
        };
        let node_id = identity.node_id();
        self.say(&format!(
            "worker starting, node_id={}, hub={}",
            node_id.as_deref().unwrap_or("<none: will enroll>"),
            self.hub_url
        ));

        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            // coverage: excluded — 建不出 OS 线程（fd / 内存耗尽 / 线程数受限）时
            // Tokio 才拒绝；测试环境里无可控构造手段，属防御性兜底。
            Err(e) => {
                self.say(&format!("error: cannot start the async runtime: {e}"));
                return std::process::ExitCode::FAILURE;
            }
        };
        let code = rt.block_on(self.connect_loop(&mut identity, node_id.as_deref()));
        // 退出码协议里的三个非零码都带「这一版坏了」的语义，
        // 而 worker 自己不打算判自己坏——它只会「正常停下」。
        let _ = code;
        std::process::ExitCode::SUCCESS
    }

    /// 「连一次 → 跑一次会话 → 断开后按退避重连」的循环。
    ///
    /// `node_id` 为 `None` 时走 enroll 形态；enroll 成功后立刻回填身份文件，
    /// 之后的每一次重连都用重连形态（只带 `node_id` + 签名，绝不上传公钥，§3）。
    ///
    /// 三个终止条件里只有 `superseded` 与「Hub 明确裁决」是终态（§7.1）——
    /// 传输层失败永远退避重试，设备的活儿不该因为 Hub 短暂重启就结束。
    async fn connect_loop(&mut self, identity: &mut Identity, node_id: Option<&str>) -> i32 {
        let mut attempt: u32 = 0;
        loop {
            let outcome =
                conn::connect_once(&self.hub_url, identity.key(), node_id, device_info()).await;
            let end = match outcome {
                Ok(mut est) => {
                    // 连上了：退避归零（§7.0「每次成功连接后 attempt 归零」）。
                    attempt = 0;
                    if let Err(e) = backfill_node_id(identity, &est) {
                        self.say(&format!("error: cannot record the issued node_id: {e}"));
                        return exit_code::CRASH;
                    }
                    let interval = Duration::from_millis(est.auth_ok.heartbeat_interval_ms);
                    self.session(&mut est, interval).await
                }
                Err(outcome) => {
                    self.say(&format!("connect failed: {}", outcome_phrase(&outcome)));
                    match reconnect::retry_after(&outcome, attempt, reconnect::jitter()) {
                        Some(d) => {
                            attempt = attempt.saturating_add(1);
                            self.say(&format!(
                                "disconnected ({}); reconnecting in {}s (attempt {})",
                                outcome_phrase(&outcome),
                                d.as_secs(),
                                attempt
                            ));
                            tokio::time::sleep(d).await;
                            continue;
                        }
                        None => {
                            // 终态（§7.1）：`superseded` 是被顶替，
                            // `Rejected` 是 Hub 对身份的裁决，两者重试都无用。
                            self.say(&format!(
                                "disconnected ({}); stopping for a human",
                                outcome_phrase(&outcome)
                            ));
                            return 0;
                        }
                    }
                }
            };
            let phrase = match &end {
                SessionEnd::PeerClosed => "peer closed the connection".to_string(),
                SessionEnd::Transport(detail) => detail.clone(),
                SessionEnd::Superseded { gen } => {
                    // §7.1 终态。再连一次就是和新连接互抢；supervisor 读到
                    // 正常退出码会重拉 worker，新 worker 的第一次 auth 又会被
                    // 同一条「同 node_id 顶替」规则挡住——不是无限互抢。
                    self.say(&format!(
                        "disconnected (superseded, gen {gen}); stopping without reconnecting"
                    ));
                    return 0;
                }
            };
            attempt = attempt.saturating_add(1);
            let d = reconnect::backoff_with_jitter(attempt, reconnect::jitter());
            self.say(&format!(
                "disconnected ({phrase}); reconnecting in {}s (attempt {attempt})",
                d.as_secs()
            ));
            tokio::time::sleep(d).await;
        }
    }

    /// 一次已认证连接上的消息循环。
    ///
    /// **接缝**：心跳发送（子任务 5）将来就在这里——周期必须取
    /// `auth_ok.heartbeat_interval_ms`（约束 14：功能由协商决定，不硬编码）。
    /// M0 的 worker 不发任何帧，循环只负责读与分类。
    async fn session(&self, est: &mut Established, heartbeat_interval: Duration) -> SessionEnd {
        self.say(&format!(
            "authed as {} (gen {}, heartbeat every {}ms)",
            est.auth_ok.node_id, est.auth_ok.gen, est.auth_ok.heartbeat_interval_ms
        ));
        let _ = heartbeat_interval;
        loop {
            match est.next_text().await {
                Err(ConnOutcome::Transport { detail }) => return SessionEnd::Transport(detail),
                // `next_text` 只产出 Transport；这条归类是诚实的降级，
                // 不是为了兜住某个具体错误。
                Err(other) => return SessionEnd::Transport(format!("read failed: {other:?}")),
                Ok(None) => return SessionEnd::PeerClosed,
                Ok(Some(text)) => match serde_json::from_str::<Frame>(&text) {
                    Err(e) => return SessionEnd::Transport(format!("bad frame json: {e}")),
                    Ok(frame) => {
                        if let Some(ended) = self.on_control_frame(&frame) {
                            return ended;
                        }
                    }
                },
            }
        }
    }

    /// 控制面帧 → 会话终止判定。不是控制面的一律返回 `None`。
    ///
    /// 约束 14：未知类型一律忽略，不因「没听说过」而断连。
    fn on_control_frame(&self, frame: &Frame) -> Option<SessionEnd> {
        if frame.msg_type != "disconnect" {
            return None;
        }
        let d = match frame.payload::<Disconnect>() {
            Ok(d) => d,
            Err(e) => return Some(SessionEnd::Transport(format!("bad disconnect: {e}"))),
        };
        if conn::should_stop_reconnecting(&d) {
            return Some(SessionEnd::Superseded {
                gen: d.gen.unwrap_or(0),
            });
        }
        self.say(&format!(
            "disconnect({:?}) — the device is fine, so keep reconnecting",
            d.reason
        ));
        Some(SessionEnd::PeerClosed)
    }

    fn rotate_log(&self) {
        let path = node_core::log::log_path(&self.data_dir);
        if let Err(e) = node_core::log::rotate_if_needed(&path) {
            self.say(&format!("cannot rotate {}: {e}", path.display()));
        }
    }

    fn say(&self, msg: &str) {
        filelog::write(&self.data_dir, ROLE_WORKER, "info", msg);
    }
}

/// enroll 成功后把 Hub 签发的 `node_id` 回填进身份文件。
///
/// **只写盘、不改内存**：`Identity::node_id()` 的来源本来就是磁盘，同步一次
/// 内存值只是把「写成功了」这件事又记了一遍，还多出一个「写成功但值没换」
/// 的中间态。这里失败会让设备停在「enroll 了但没记住 node_id」——重启后
/// 重新 enroll 会撞「指纹已存在」，所以必须停下、留下 error 级痕迹。
fn backfill_node_id(identity: &mut Identity, est: &Established) -> Result<(), String> {
    if !est.enrolled {
        return Ok(());
    }
    identity
        .set_node_id(&est.auth_ok.node_id)
        .map_err(|e| e.to_string())
}

/// `ConnOutcome` → 人类可读短语（进日志）。
///
/// 不直接打 `Debug`：`Transport` 的 detail 已经是给人看的，而 `Debug` 会把
/// 枚举名和花括号一起搬上来，排障时反而淹没了真正的错误。
fn outcome_phrase(outcome: &ConnOutcome) -> String {
    match outcome {
        ConnOutcome::Transport { detail } => detail.clone(),
        ConnOutcome::Superseded { gen } => format!("superseded by a newer connection (gen {gen})"),
        ConnOutcome::Rejected { reason } => format!("hub refused: {reason}"),
        // 只在「握手成功却被当成失败传进来」时才可能发生——那是 bug 不是状态。
        ConnOutcome::Authed { node_id, .. } => format!("already authed as {node_id}"),
    }
}

/// enroll 时上传的设备描述。
///
/// `name` / `hostname` 留空：node 不该为这两行展示名引入新依赖（树莓派上的
/// 二进制体积是硬约束），Hub 侧拿指纹与架构就能标识这台设备。
fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: String::new(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: String::new(),
        node_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}
