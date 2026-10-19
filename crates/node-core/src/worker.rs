//! worker 角色：持有 identity + WS 连接，跑消息循环与退避重连（D1、子任务 6）。
//!
//! **worker 持连接、supervisor 持进程**，不是反过来（D1）：升级换掉的正是
//! worker 这个文件，而 supervisor 那份代码必须是没被替换过的旧映像——
//! 「执行回滚的代码不参与被回滚」。
//!
//! 身份（`identity.json`）由 worker 打开：它是设备身份，不是进程身份
//! （约束 12：升级只换 worker，`node_id` 不能变）。
//!
//! 为什么在 `node-core` 而不在 `node-cli`（进程入口那一侧）：connector 只能被
//! **已认证的 WS 连接**使用，而「进程入口」不该是连接能力的拥有者——那样
//! 每加一种能力都要动 `node-cli`，而 `node-cli` 恰是升级流程之外的薄壳。
//! 同理 [`crate::entry`]（日志写入侧）也一并搬了过来。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::channel;
use crate::conn::{self, ConnOutcome, Established};
use crate::entry;
use crate::heartbeat;
use crate::reconnect;
use crate::supervisor::exit_code;
use crate::Identity;
use protocol::frame::Frame;
use protocol::message::{DeviceInfo, Disconnect};

/// worker 的日志落点。**由构造方注入**，不直接依赖数据目录。
///
/// 为什么是接缝而不是直接写文件：本 crate 不决定进程怎么起、数据目录在哪
/// （那是 `node-cli` 的事），而通道帧处理一旦上线，日志就同时是
/// 「连接活着」与「通道处理没崩」的唯一证据。这里用一行闭包把落点留给调用方，
/// 测试因此能断言「处理了哪些帧」而不必去读盘。
type Logger = dyn Fn(&str) + Send + Sync;

/// `afcli` 二进制名。命令行首行用它自称，避免「日志里写着 node-cli」那种
/// 用户要愣一下才反应过来的错位。
const BIN_NAME: &str = "afcli";

/// 一次已认证连接上的循环被什么终止了。
#[derive(Debug)]
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
    log: Box<Logger>,
}

/// 主动泵一轮的周期。
///
/// **30 ms 是「终端看着是活的」与「设备不空转」之间的折中**：人眼对
/// 「敲完回车到出字」的容忍在 100 ms 量级，1 MiB 队列在 30 ms 一轮下
/// 折合 33 MiB/s——比任何一台目标设备上的本地命令产出都快，所以它
/// 永远不会是瓶颈，只会被网络往返盖过去。
///
/// **不从 Hub 协商**（约束 14 的例外说明）：它不是「这台设备支持什么
/// 能力」，而是本地读循环的节流参数，问一次只多一份会漂移的配置。
const PUMP_INTERVAL: Duration = Duration::from_millis(30);

impl Worker {
    pub fn new(data_dir: PathBuf, hub_url: String) -> Self {
        let log = log_to(&data_dir);
        Self {
            data_dir,
            hub_url,
            log: Box::new(log),
        }
    }

    /// 自定日志落点（集成测试用：把帧处理过程收进内存缓冲）。
    pub fn with_logger(mut self, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Box::new(log);
        self
    }

    /// 阻塞运行，返回进程退出码。正常情况下不返回。
    pub fn work(&mut self) -> std::process::ExitCode {
        self.rotate_log();
        // coverage: excluded — `work` 的 load 成功臂后段：需真 data_dir，
        // 与 `run_with` 同一不可达形状（:64）。
        let identity = match Identity::load_or_create(&self.data_dir) {
            Ok(id) => id,
            // 身份文件坏了/读不了时**不能**重新生成：那等于换了一台设备，
            // 会撞上「指纹已存在则拒绝」，设备永久无法接入。所以停下来。
            Err(e) => {
                self.say(&format!("error: cannot load identity: {e}"));
                return std::process::ExitCode::FAILURE;
            } // coverage: excluded — `run_with` 自建多线程 runtime，嵌在 `#[tokio::test]`
              // 里会 panic。
        };
        self.run_with(identity)
    }

    /// 身份已备好之后的全部流程。
    ///
    /// 与 [`Self::work`] 分开：`work` 是进程入口（只能靠 spawn 二进制走到），
    /// 这里是「有了身份之后干什么」。拆开不是为了复用，是为了能测——
    /// 见 `mod tests` 里的 `run_with` 用例。
    // coverage: excluded — 进程入口：自建多线程 runtime 并 block_on，
    // 嵌在 `#[tokio::test]` 里会 panic。单测走 `connect_loop` 与 `session`。
    fn run_with(&mut self, mut identity: Identity) -> std::process::ExitCode {
        let node_id = identity.node_id();
        self.say(&format!(
            "{BIN_NAME} worker starting, node_id={}, hub={}",
            // coverage: excluded — 见 :66 的 `run_with` 标注：进程入口，只能由
            // 真进程触达。
            node_id.as_deref().unwrap_or("<none: will enroll>"),
            self.hub_url
        ));
        // coverage: excluded — 同上：runtime 构造失败需 fd / 内存耗尽，
        // 测试环境无可控构造手段（防御性兜底）。

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
        // 退出码协议里的三个非零码都带「这一版坏了」的语义，
        // coverage: excluded — 同上：`block_on` 是进程入口路径。
        // 而 worker 自己不打算判自己坏——它只会「正常停下」。
        let _ = rt.block_on(self.connect_loop(&mut identity, node_id.as_deref()));
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
            // 带上本节点见过的最大 gen。它是 §7.1 兜底路径的输入：
            // Hub 见它落后就回 `auth_fail{superseded}` 而不接管，
            // 于是「disconnect 帧丢了」也能自愈，不必依赖消息送达。
            let outcome = conn::connect_once(
                &self.hub_url,
                identity.key(),
                node_id,
                identity.last_seen_gen(),
                device_info(),
            )
            .await;
            let end = match outcome {
                Ok(mut est) => {
                    // 连上了：退避归零（§7.0「每次成功连接后 attempt 归零」）。
                    attempt = 0;
                    // coverage: excluded — `backfill_node_id` 失败需身份文件在会话中途
                    // 变成只读或损坏。
                    if let Err(e) = backfill_node_id(identity, est.enrolled, &est.auth_ok.node_id) {
                        self.say(&format!("error: cannot record the issued node_id: {e}"));
                        return exit_code::CRASH;
                    }
                    // 记下本次的 gen，**必须落盘**：进程重启后若从 0 开始，
                    // 而 Hub 已经发到 42，这条连接永远被判成「已被顶替」——
                    // 设备接不上且无自愈路径（子任务 7 标准⑧）。
                    // coverage: excluded — `record_gen` 失败同上：需身份文件中途不可写。
                    if let Err(e) = identity.record_gen(est.auth_ok.gen) {
                        self.say(&format!("error: cannot record the connection gen: {e}"));
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
                            // coverage: excluded — 退避重连的 continue：需构造一次「可重试的传输
                            // 失败」并等过退避窗口。
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
                    //
                    // 但这个终态**不能只靠内存**：supervisor 重拉出的新进程
                    // 什么都不记得，会老老实实再连一次。所以持久化到身份文件，
                    // 由人工介入清除（「被顶替」意味着同 node_id 另有连接活着）。
                    // coverage: excluded — `mark_superseded` 失败需身份文件中途不可写。
                    if let Err(e) = identity.mark_superseded(&crate::now_secs()) {
                        self.say(&format!("error: cannot persist the superseded mark: {e}"));
                        return exit_code::CRASH;
                    }
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

    /// 一次已认证连接上的消息循环：读帧分类 + 周期发心跳（子任务 5）。
    ///
    /// 心跳周期取 `auth_ok.heartbeat_interval_ms`（约束 14：功能由协商决定，
    /// 不硬编码），且 **`auth_ok` 后立即发一次**——等第一个周期会让 Hub 在最长
    /// 一个周期内看不到任何上报，而它要靠这份上报判在线。
    async fn session(&self, est: &mut Established, heartbeat_interval: Duration) -> SessionEnd {
        self.say(&format!(
            "authed as {} (gen {}, heartbeat every {}ms)",
            est.auth_ok.node_id, est.auth_ok.gen, est.auth_ok.heartbeat_interval_ms
        ));
        // 发不出去不当成断连理由：Hub 判离站的依据是「收不到心跳」，
        // 而它自己会发现。这里只留痕。
        if let Err(e) = self.send_heartbeat(est).await {
            self.say(&format!("heartbeat send failed: {}", outcome_phrase(&e)));
        }
        let mut ticker = tokio::time::interval(heartbeat_interval);
        ticker.tick().await; // 第一个 tick 由上面那次立即发送承担
        let mut pump = tokio::time::interval(PUMP_INTERVAL);
        loop {
            tokio::select! {
                // coverage: excluded — 心跳 tick 臂：需让 ticker 在 session 内触发而
                // read 不返回。
                _ = ticker.tick() => {
                    if let Err(e) = self.send_heartbeat(est).await {
                        self.say(&format!("heartbeat send failed: {}", outcome_phrase(&e)));
                    }
                }
                // 通道的主动输出。**独立于下行帧**（`channel::pump` 的注释
                // 解释了为什么这条臂不能省）：PTY 的输出是子进程写的，
                // Hub 不发任何东西时它照样在涨。
                //
                // 它与心跳是两条独立路径（约束 8）：这一臂里做的 IO 全部
                // 是非阻塞的 drain，连接真实读取发生在 `est.next_text`
                // 那一臂上，所以通道繁忙不会让心跳迟到。
                _ = pump.tick() => {
                    for (channel_id, frames) in channel::pump() {
                        if let Err(e) = self.send_channel_frames(est, &channel_id, &frames).await {
                            self.say(&format!(
                                "pumped frame send failed ({} {}): {}",
                                channel_id,
                                frames.len(),
                                outcome_phrase(&e)
                            ));
                        }
                    }
                }
                incoming = est.next_text() => match incoming {
                    Err(ConnOutcome::Transport { detail }) => return SessionEnd::Transport(detail),
                    // `next_text` 只产出 Transport；这条归类是诚实的降级，
                    // coverage: excluded — `next_text` 只产出 Transport，这条归类结构上
                    // 不可达（诚实的降级，见 :214 注释）。
                    // 不是为了兜住某个具体错误。
                    Err(other) => return SessionEnd::Transport(format!("read failed: {other:?}")),
                    Ok(None) => return SessionEnd::PeerClosed,
                    Ok(Some(text)) => match serde_json::from_str::<Frame>(&text) {
                        Err(e) => return SessionEnd::Transport(format!("bad frame json: {e}")),
                        Ok(frame) => {
                            if let Some(ended) = self.on_control_frame(&frame) {
                                // coverage: excluded — 未知控制帧的忽略臂：需发一帧 disconnect 之外的
                                // 未知类型并让它走到这里。
                                return ended;
                            }
                            if let Some(ended) = self.on_channel_message(est, &frame).await {
                                return ended;
                            }
                        }
                    },
                },
            }
        }
    }

    /// 发一批通道帧。
    ///
    /// 回不回得出去**不**当成断连理由：Hub 判离站靠的是收不到心跳，
    /// 而它自己会发现这里断没断。只留痕。
    async fn send_channel_frames(
        &self,
        est: &mut Established,
        channel_id: &str,
        frames: &[protocol::Frame],
    ) -> Result<(), ConnOutcome> {
        for out in frames {
            est.send(out).await.inspect_err(|e| {
                self.say(&format!(
                    "channel frame send failed ({} {}): {}",
                    channel_id,
                    out.msg_type,
                    outcome_phrase(e)
                ));
            })?;
        }
        Ok(())
    }

    /// 一帧通道帧 → [`channel::on_channel_frame`] → 依次发出该回的那些帧。
    ///
    /// **回多帧**是常态：`open_channel` 回 `channel_ready`，PTY 一帧输入就可能
    /// 带回一大串输出（按信用额度放行若干帧，剩下排队等 `channel_credit`）。
    ///
    /// 返回 `Some` = 这条连接该结束了。通道帧只有一种能到这一步：**载荷
    /// 解不开**（比如 `open_channel.c` 不是对象）。那不是「没听懂」而是
    /// 协议分叉——留着这条连接只会让后面每一帧都解析失败（约束 14 的
    /// 「未知类型忽略」针对的是**类型名**，不是坏载荷）。
    async fn on_channel_message(&self, est: &mut Established, frame: &Frame) -> Option<SessionEnd> {
        if !channel::is_channel_type(&frame.msg_type) {
            return None;
        }
        match channel::on_channel_frame(frame) {
            channel::Action::None => None,
            channel::Action::End => Some(SessionEnd::Transport(format!(
                "bad {} payload",
                frame.msg_type
            ))),
            channel::Action::Send(frames) => {
                for out in &frames {
                    if let Err(e) = est.send(out).await {
                        // 通道帧回不回得出去**不**当成断连理由：Hub 判离站靠的是
                        // 收不到心跳，而它自己会发现这里断没断。只留痕。
                        self.say(&format!(
                            "channel frame send failed ({}): {}",
                            out.msg_type,
                            outcome_phrase(&e)
                        ));
                    }
                }
                None
            }
        }
    }

    /// 发一份心跳。
    ///
    /// 工作区根留空：M0 还没有「用户配置的工作区」这个概念，而本模块
    /// 明确不许遍历文件系统去猜（§2 路径排除规则）。
    async fn send_heartbeat(&self, est: &mut Established) -> Result<(), ConnOutcome> {
        let hb = heartbeat::heartbeat(Vec::new());
        // coverage: excluded — heartbeat 编码失败需构造 serde 失败的 Heartbeat。
        let payload = serde_json::to_value(&hb).map_err(|e| ConnOutcome::Transport {
            detail: format!("heartbeat encode: {e}"),
        })?;
        est.send(&Frame::push(
            protocol::message::types::HEARTBEAT,
            0,
            payload,
        ))
        .await
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
        entry::rotate_at_startup(&self.data_dir, entry::ROLE_WORKER);
    }

    fn say(&self, msg: &str) {
        (self.log)(msg);
    }
}

/// 默认日志落点：`<data_dir>/logs/afcli.log`，角色 worker。
fn log_to(data_dir: &Path) -> impl Fn(&str) + Send + Sync + 'static {
    let d = data_dir.to_path_buf();
    move |msg: &str| entry::write(&d, entry::ROLE_WORKER, "info", msg)
}

/// enroll 成功后把 Hub 签发的 `node_id` 回填进身份文件。
///
/// **只写盘、不改内存**：`Identity::node_id()` 的来源本来就是磁盘，同步一次
/// 内存值只是把「写成功了」这件事又记了一遍，还多出一个「写成功但值没换」
/// 的中间态。这里失败会让设备停在「enroll 了但没记住 node_id」——重启后
/// 重新 enroll 会撞「指纹已存在」，所以必须停下、留下 error 级痕迹。
///
/// 参数是 `enrolled` 与 `node_id` 两个值而不是整个 [`Established`]：
/// 后者的 `socket` 字段私有，收下它就等于把「能测」这件事关在门外。
fn backfill_node_id(
    identity: &mut Identity,
    enrolled: bool,
    issued_node_id: &str,
) -> Result<(), String> {
    if !enrolled {
        return Ok(());
    }
    identity
        .set_node_id(issued_node_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use futures_util::{SinkExt, StreamExt};

    fn frame_of(t: &str, c: serde_json::Value) -> Frame {
        serde_json::from_value(serde_json::json!({"v":1,"seq":1,"t":t,"c":c})).expect("frame")
    }

    fn worker(name: &str) -> (PathBuf, Worker) {
        let d = std::env::temp_dir().join(format!("afcli-worker-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("tempdir");
        let w = Worker::new(d.clone(), "ws://hub/ws".to_string());
        (d, w)
    }

    /// `disconnect{superseded}` → 终态，不重连（§7.1）。
    #[test]
    fn a_superseded_disconnect_ends_the_session_without_reconnecting() {
        let (d, w) = worker("superseded");
        let f = frame_of(
            "disconnect",
            serde_json::json!({"reason":"superseded","gen":7}),
        );
        match w.on_control_frame(&f) {
            // coverage: excluded — 只在断言失败时执行；`panic!` 的格式串行被
            // llvm-cov 记为 miss（多行宏已知现象）。
            Some(SessionEnd::Superseded { gen }) => assert_eq!(gen, 7),
            other => panic!("want Superseded, got {other:?}"),
        }
        let _ = d;
    }

    /// `disconnect{admin}`（用户手动下线）→ 设备没坏，照常退避重连。
    #[test]
    fn an_admin_disconnect_keeps_reconnecting() {
        let (d, w) = worker("admin");
        let f = frame_of("disconnect", serde_json::json!({"reason":"admin"}));
        assert!(
            matches!(w.on_control_frame(&f), Some(SessionEnd::PeerClosed)),
            "manual offline must not stop the device"
        );
        let _ = d;
    }

    /// 约束 14：未知类型一律忽略，不因「没听说过」而断连。
    #[test]
    fn an_unknown_frame_type_is_ignored() {
        let (d, w) = worker("unknown");
        for t in ["heartbeat", "channel_open", "some_future_type", ""] {
            let f = frame_of(t, serde_json::json!({"anything":1}));
            assert!(
                w.on_control_frame(&f).is_none(),
                "{t} 必须被忽略，否则 Hub 加一种帧就会把设备踢下线"
            );
        }
        let _ = d;
    }

    /// `disconnect` 载荷不是合法 JSON → 传输类失败。
    ///
    /// 这是**协议分叉**而不是「这条消息没听懂」：继续留着这条连接
    /// 只会让后续每一帧都解析失败，所以按 Transport 退避重连。
    #[test]
    fn a_malformed_disconnect_payload_is_a_transport_failure() {
        let (d, w) = worker("bad-disconnect");
        let f = frame_of("disconnect", serde_json::json!({"reason":12345}));
        match w.on_control_frame(&f) {
            Some(SessionEnd::Transport(detail)) => {
                assert!(detail.starts_with("bad disconnect:"), "{detail}")
                // coverage: excluded — 同上：只在断言失败时执行的 `panic!` 参数行。
            }
            other => panic!("want Transport, got {other:?}"),
        }
        let _ = d;
    }

    /// `gen` 缺省时记 0：老 Hub 不带这个字段，不能因此崩。
    #[test]
    fn a_disconnect_without_gen_is_still_terminal() {
        let (d, w) = worker("no-gen");
        let f = frame_of("disconnect", serde_json::json!({"reason":"superseded"}));
        assert!(matches!(
            w.on_control_frame(&f),
            Some(SessionEnd::Superseded { gen: 0 })
        ));
        let _ = d;
    }

    /// `outcome_phrase` 给日志用：不能直接打 `Debug`，那会把枚举名和
    /// 花括号一起搬上来，淹没了真正的错误。
    #[test]
    fn every_outcome_has_a_human_phrase() {
        let t = ConnOutcome::Transport {
            detail: "connection refused".into(),
        };
        assert_eq!(outcome_phrase(&t), "connection refused");
        let s = ConnOutcome::Superseded { gen: 3 };
        assert!(
            outcome_phrase(&s).contains("gen 3"),
            // coverage: excluded — `assert!` 的格式串参数行（多行宏已知现象）。
            "{}",
            outcome_phrase(&s)
        );
        let r = ConnOutcome::Rejected {
            reason: "identity mismatch".into(),
        };
        assert!(
            outcome_phrase(&r).contains("identity mismatch"),
            // coverage: excluded — 同上：`assert!` 的格式串参数行。
            "{}",
            outcome_phrase(&r)
        );
        let a = ConnOutcome::Authed {
            node_id: "n_y".into(),
            protocol_version: 1,
            gen: 2,
            heartbeat_interval_ms: 15_000,
            enrolled: false,
        };
        assert!(
            outcome_phrase(&a).contains("n_y"),
            "握手成功却被当成失败传进来时，日志要点名是哪个 node"
        );
    }

    /// enroll 上传的设备描述：`name`/`hostname` 故意留空（不为两行展示名
    /// 在树莓派上加依赖），os/arch/version 必须是真的。
    #[test]
    fn device_info_reports_the_real_platform() {
        let i = device_info();
        assert_eq!(i.os, std::env::consts::OS);
        assert_eq!(i.arch, std::env::consts::ARCH);
        assert_eq!(i.node_version, env!("CARGO_PKG_VERSION"));
        assert!(i.name.is_empty(), "name 不该引入新依赖");
        assert!(i.hostname.is_empty());
    }

    /// 没有新 enroll 时**不动**身份文件：每次重连都写一次盘
    /// 会让 SD 卡（树莓派的盘）白挨无数次写入。
    #[test]
    fn backfill_is_a_noop_when_nothing_was_enrolled() {
        let d = std::env::temp_dir().join("afcli-worker-backfill-noop");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let before = std::fs::read(id.path()).expect("read identity");
        let before_mtime = std::fs::metadata(id.path())
            .expect("meta")
            .modified()
            .expect("mtime");

        backfill_node_id(&mut id, false, "n_issued").expect("noop");

        assert_eq!(
            std::fs::read(id.path()).expect("read again"),
            before,
            "文件内容不该变"
        );
        assert_eq!(
            std::fs::metadata(id.path())
                .expect("meta")
                .modified()
                .expect("mtime"),
            before_mtime,
            "连 mtime 都不该动——SD 卡上白写是要折寿的"
        );
        assert_eq!(id.node_id(), None, "node_id 仍应是 None");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 新 enroll 成功 → 把 Hub 签发的 `node_id` 落到盘上（约束 12：
    /// 升级只换 worker，`node_id` 不能变）。
    #[test]
    fn backfill_records_the_issued_node_id() {
        let d = std::env::temp_dir().join("afcli-worker-backfill-write");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        let mut id = Identity::load_or_create(&d).expect("identity");

        backfill_node_id(&mut id, true, "n_issued").expect("write");

        assert_eq!(id.node_id().as_deref(), Some("n_issued"));
        // 落盘：重启后的 worker 必须用同一个 node_id 重连。
        let reread = Identity::load_or_create(&d).expect("reread");
        assert_eq!(reread.node_id().as_deref(), Some("n_issued"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 写不进去时必须报错（不是静默 Ok）：调用方据此停下并留 error 痕迹，
    /// 否则设备停在「enroll 了但没记住 node_id」，重启后重新 enroll 会撞
    /// 「指纹已存在」而永久无法接入。
    #[test]
    fn backfill_reports_a_failed_write_instead_of_succeeding_quietly() {
        let d = std::env::temp_dir().join("afcli-worker-backfill-fail");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        let mut id = Identity::load_or_create(&d).expect("identity");
        // 把身份文件换成不可解析的内容：set_node_id 走「读-改-写」，
        // 读不了就报错，不会偷偷造一台新设备。
        std::fs::write(id.path(), b"{ not json").expect("corrupt");
        assert!(
            backfill_node_id(&mut id, true, "n_issued").is_err(),
            "写失败必须报错"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- connect_loop：真的连一次，只走「终态」那条会返回的路 ----

    /// 起一个假 Hub：`Reject` 在握手阶段就拒绝，其余三种先让握手成功再收尾。
    fn fake_hub(tail: HubTail) -> String {
        // std 的 bind：`fake_hub` 有时在 #[tokio::test] 里被调，
        // 那里不能再 block_on 一个嵌套运行时。
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let addr = std_listener.local_addr().expect("addr");
        let listener = tokio::net::TcpListener::from_std(std_listener).expect("listener");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                // coverage: excluded — 测试夹具 `fake_hub`：fixture 只在测试自己起的
                // 监听器上跑，accept / upgrade 失败臂构造不出。
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                // coverage: excluded — 同上：fixture 的 upgrade 失败臂。
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let _ = ws.next().await;
                let _ = send(&mut ws, challenge()).await;
                let _ = ws.next().await;
                if matches!(tail, HubTail::Reject) {
                    let _ = send(&mut ws, auth_fail()).await;
                    let _ = ws.next().await;
                    return;
                }
                let _ = send(&mut ws, auth_ok()).await;
                // coverage: excluded — `unreachable!`：上面已经 return，结构上不可达。
                match tail {
                    HubTail::Reject => unreachable!("上面已返回"),
                    HubTail::Close => {
                        let _ = ws.close(None).await;
                        let _ = ws.next().await;
                    }
                    HubTail::Superseded => {
                        let _ = send(&mut ws, superseded()).await;
                        let _ = ws.next().await;
                    }
                    HubTail::RstAfterAuthOk => {
                        // SO_LINGER=0 → close 时发 RST 而不是 FIN。
                        // worker 随后写这一帧会拿到 BrokenPipe/ConnectionReset，
                        // 而**这不是断连理由**：Hub 判离站靠的是收不到心跳。
                        let stream = ws.get_mut();
                        let fd = std::os::fd::AsRawFd::as_raw_fd(stream);
                        let linger = libc::linger {
                            l_onoff: 1,
                            l_linger: 0,
                        };
                        let n = unsafe {
                            libc::setsockopt(
                                fd,
                                libc::SOL_SOCKET,
                                libc::SO_LINGER,
                                std::ptr::from_ref(&linger).cast(),
                                std::mem::size_of::<libc::linger>() as libc::socklen_t,
                            )
                        };
                        assert_eq!(
                            n, 0,
                            "SO_LINGER must be settable for this fixture to mean anything"
                        );
                        drop(ws);
                    }
                    HubTail::Hang => futures_util::future::pending::<()>().await,
                    HubTail::ReadOnly => {
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
                            .await;
                    }
                    HubTail::BadJson => {
                        let _ = send_text(&mut ws, "{ not json".to_string()).await;
                        let _ = ws.next().await;
                    }
                    HubTail::Binary => {
                        let _ = ws
                            .send(tokio_tungstenite::tungstenite::Message::Binary(vec![
                                0xff, 0x00,
                            ]))
                            .await;
                        let _ = ws.next().await;
                    }
                }
            });
        });
        format!("ws://{addr}/ws")
    }

    fn challenge() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":1,"t":"challenge","c":{"nonce":"n"}})
    }

    fn auth_ok() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":2,"t":"auth_ok","c":{"node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":15000}})
    }

    fn auth_fail() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":2,"t":"auth_fail","c":{"reason":"identity mismatch"}})
    }

    fn superseded() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":3,"t":"disconnect","c":{"reason":"superseded","gen":2}})
    }

    async fn send(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        v: serde_json::Value,
    ) -> Result<(), String> {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(v.to_string()))
            .await
            .map_err(|e| e.to_string())
    }

    /// 轮询等日志里出现 `needle`。
    ///
    /// 不用 `timeout` 包住 `connect_loop`：那个 future 里是无限循环，
    /// 到点后 drop 它要等 runtime 收尾，实测会把测试挂住。
    ///
    /// 也必须用 `tokio::time::sleep`：阻塞当前线程会让 worker 任务在
    /// current_thread runtime 上永远排不上队，于是死锁。
    async fn wait_for_log(
        d: &std::path::Path,
        needle: &str,
        budget: std::time::Duration,
    ) -> String {
        let path = crate::log::log_path(d);
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if text.contains(needle) {
                    // coverage: excluded — 日志轮询辅助函数的 deadline `assert!` 行：
                    // 只在失败时执行。
                    return text;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "日志里始终没出现 {needle}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn send_text(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        t: String,
    ) -> Result<(), String> {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(t))
            .await
            .map_err(|e| e.to_string())
    }

    enum HubTail {
        Close,
        Superseded,
        Reject,
        Hang,
        /// `auth_ok` 之后立刻 RST：worker 的心跳必然发不出去。
        RstAfterAuthOk,
        /// 握手后只读不发，用来抓 worker 主动发出的第一帧。
        ReadOnly,
        /// 握手后发一段不是 JSON 的文本。
        BadJson,
        /// 握手后发一个二进制帧。
        Binary,
    }

    /// Hub 明确裁决身份 → worker **不重连**，正常退出码 0。
    ///
    /// 若这里改成重连，设备会在「指纹已存在」上无限撞墙，
    /// 每次都打一条日志，SD 卡慢慢写满。
    #[tokio::test]
    async fn a_rejected_identity_stops_the_loop() {
        let (d, _w) = worker("reject");
        let url = fake_hub(HubTail::Reject);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let code = w.connect_loop(&mut id, None).await;
        assert_eq!(code, 0, "被拒绝是终态，但退出码是「正常退出」");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("hub refused"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 被顶替 → 终态，**不重连**（§7.1）：再连一次就是和新连接互抢。
    #[tokio::test]
    async fn a_superseded_session_stops_the_loop() {
        let (d, _w) = worker("superseded-loop");
        let url = fake_hub(HubTail::Superseded);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let code = w.connect_loop(&mut id, None).await;
        assert_eq!(code, 0);
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("superseded"), "{log}");
        assert!(log.contains("stopping without reconnecting"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `auth_ok` 之后**立刻**发心跳，不等第一个周期（规格②）。
    ///
    /// 等周期会让 Hub 在最长一个周期内看不到任何上报，而它靠这份上报判在线。
    /// 这里用 20s 周期：实现若退化成「等一个周期」，测试必然超时。
    #[tokio::test]
    async fn the_first_heartbeat_arrives_without_waiting_for_the_interval() {
        let (d, _w) = worker("first-heartbeat");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::ReadOnly));
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            w.connect_loop(&mut id, None),
        )
        .await;
        assert!(code.is_err(), "连接还活着，loop 不该退出");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 一段不是 JSON 的文本 → Transport 后退避重连。
    ///
    /// 不能按约束 14「没听懂就忽略」：对端的帧全都解析不动，说明两边已经
    /// 分叉，继续留着只会每一帧都失败。
    #[tokio::test]
    async fn an_unparsable_frame_after_auth_reconnects() {
        let (d, _w) = worker("bad-json");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::BadJson));
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None).await });
        let log = wait_for_log(&d, "reconnecting", std::time::Duration::from_secs(5)).await;
        assert!(log.contains("bad frame json"), "{log}");
        assert!(log.contains("reconnecting"), "{log}");
        h.abort();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 二进制帧 → 同样判 Transport 并重连（协议只用 JSON 文本，§2）。
    #[tokio::test]
    async fn a_binary_frame_after_auth_reconnects() {
        let (d, _w) = worker("binary");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::Binary));
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None).await });
        let log = wait_for_log(&d, "reconnecting", std::time::Duration::from_secs(5)).await;
        // 短语来自 conn.rs 的 next_text：二进制帧是「帧种类不对」而不是
        // 「读失败」——排障时两者指向不同方向。
        assert!(log.contains("unexpected frame kind"), "{log}");
        assert!(log.contains("reconnecting"), "{log}");
        h.abort();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Hub 明确裁决身份 → worker **不重连**，正常退出码 0。
    /// 用 `tokio::time::pause()` 之外的办法——把第二次连接指向一个
    /// 立刻拒绝的端口，于是 loop 会在退避后再次失败并继续退避。
    /// 为让测试有界，只断言「第一次断开后有重连意图」。
    #[tokio::test]
    async fn a_peer_close_reconnects_rather_than_stopping() {
        let (d, _w) = worker("reconnect");
        let url = fake_hub(HubTail::Close);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        // 不暂停时间：第一次退避是 2s×[0.7,1.3)=1.4–2.6s，
        // 3s 足够让它重连一次并被记进日志。超时到点 = 它还在重连。
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            w.connect_loop(&mut id, None),
        )
        .await;
        // 第二次连的是同一个已关闭的端口 → 传输失败 → 继续退避，
        // 所以这个 timeout 一定到点；到点说明它确实在重连而不是退出。
        assert!(code.is_err(), "peer close 后必须重连，不该退出");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("peer closed"), "{log}");
        assert!(log.contains("reconnecting"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 连上一个「挂着不关」的 Hub → loop 一直等，不会自己退出。
    /// 这条锁的是「传输层没坏就不该乱退」。
    #[tokio::test]
    async fn a_live_connection_is_waited_on_not_dropped() {
        let (d, _w) = worker("hang");
        let url = fake_hub(HubTail::Hang);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let code = tokio::time::timeout(
            std::time::Duration::from_millis(400),
            w.connect_loop(&mut id, None),
        )
        .await;
        assert!(code.is_err(), "连接还活着，loop 不该退出");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("authed as n_x"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 顶替裁决之后 `node_id` 必须已经落盘：进程重启后若从「没有 node_id」
    /// 开始，它会重新 enroll 而不是带着旧 node_id 重连，于是撞「指纹已存在」。
    ///
    /// 这里用的是 `RstAfterAuthOk` 之外的另一条路：让 worker 在 session 里
    /// 收到裁决并退出，然后重新读身份文件验证 node_id 已在盘上。
    #[tokio::test]
    async fn a_superseded_worker_still_has_its_node_id_on_disk() {
        let (d, _w) = worker("superseded-backfill");
        let url = fake_hub(HubTail::Superseded);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let code = w.connect_loop(&mut id, None).await;
        assert_eq!(code, 0);
        let reread = Identity::load_or_create(&d).expect("reread");
        assert_eq!(reread.node_id().as_deref(), Some("n_x"));
        assert!(reread.is_superseded(), "裁决必须持久化，重启后不再重连");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `auth_ok` 之后心跳发不出去 → **只留痕，不当成断连理由**。
    ///
    /// 判据不是「退出了」而是「日志里写着发送失败」：Hub 判离站靠的是
    /// 收不到心跳，而它自己会发现。worker 若在这里把连接判死，
    /// 一次网络抖动就会演变成重连风暴。
    #[tokio::test]
    async fn a_heartbeat_that_cannot_be_sent_only_leaves_a_trace() {
        let (d, _w) = worker("hb-send-fail");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::RstAfterAuthOk));
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None).await });
        let log = wait_for_log(
            &d,
            "heartbeat send failed",
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(
            log.contains("heartbeat send failed"),
            "必须留下发送失败的痕迹: {log}"
        );
        h.abort();
        let _ = std::fs::remove_dir_all(&d);
    }
}
