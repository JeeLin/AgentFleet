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

/// 「Hub 还没设密码」时打给用户的**可操作指引**。
///
/// **它打在 stdout 而不是日志文件里**：用户反馈的原话是「连不上又不报错」，
/// 而日志要 `afcli log` 才看得见——指路的话必须在用户已经盯着的那块屏幕上。
/// 一句「Hub 拒绝了这台设备」只会让人去查设备，而真相是「Hub 还没做首次
/// setup」，那是浏览器里的一个动作。
const SETUP_GUIDANCE: &str = "\
afcli: the Hub has no password yet.
  Open the Hub in a browser and finish the first-run setup (it prints a setup token).
  This device keeps retrying on its own — nothing else is needed here.";

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

/// 会话结束之后该干什么。
#[derive(Debug)]
enum AfterSession {
    /// 退避后重连（短语进日志）。
    Reconnect(String),
    /// 终态：停下，不重连。
    Stop,
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

    /// 阻塞运行，返回进程退出码。正常情况下不返回。
    ///
    /// `out` 是**给用户看的那块屏幕**（进程 stdout）：有些话必须说到用户
    /// 眼前，而不是只留在日志文件里——日志要 `afcli log` 才看得见，而
    /// 「Hub 还没设密码，去浏览器里做完 setup」这句话正是这种话。
    pub fn work<W: std::io::Write>(&mut self, out: W) -> std::process::ExitCode {
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
        self.run_with(identity, out)
    }

    /// 身份已备好之后的全部流程。
    ///
    /// 与 [`Self::work`] 分开：`work` 是进程入口（只能靠 spawn 二进制走到），
    /// 这里是「有了身份之后干什么」。拆开不是为了复用，是为了能测——
    /// 见 `mod tests` 里的 `run_with` 用例。
    // coverage: excluded — 进程入口：自建多线程 runtime 并 block_on，
    // 嵌在 `#[tokio::test]` 里会 panic。单测走 `connect_loop` 与 `session`。
    fn run_with<W: std::io::Write>(
        &mut self,
        mut identity: Identity,
        out: W,
    ) -> std::process::ExitCode {
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
        let _ = rt.block_on(self.connect_loop(&mut identity, node_id.as_deref(), out));
        std::process::ExitCode::SUCCESS
    }

    /// 「连一次 → 跑一次会话 → 断开后按退避重连」的循环。
    ///
    /// `node_id` 为 `None` 时走 enroll 形态；enroll 成功后立刻回填身份文件，
    /// 之后的每一次重连都用重连形态（只带 `node_id` + 签名，绝不上传公钥，§3）。
    ///
    /// 三个终止条件里只有 `superseded` 与「Hub 明确裁决」是终态（§7.1）——
    /// 传输层失败永远退避重试，设备的活儿不该因为 Hub 短暂重启就结束。
    async fn connect_loop<W: std::io::Write>(
        &mut self,
        identity: &mut Identity,
        node_id: Option<&str>,
        mut out: W,
    ) -> i32 {
        let mut attempt: u32 = 0;
        // 「Hub 还没设密码」的指引**一个进程只说一次**：它是同一件事，
        // 反复刷屏只会把真正要看的那一行淹掉。
        let mut setup_notice_shown = false;
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
                Ok(est) => {
                    // 连上了：退避归零（§7.0「每次成功连接后 attempt 归零」）。
                    attempt = 0;
                    // 认证成功过 = Hub 那边已经初始化过了：指引重新变成
                    // 「有可能再遇到」，于是下一次再说一遍。
                    setup_notice_shown = false;
                    match self.run_established(identity, est).await {
                        Ok(end) => end,
                        Err(code) => return code,
                    }
                }
                Err(outcome) => {
                    self.say(&format!("connect failed: {}", outcome_phrase(&outcome)));
                    if !setup_notice_shown && is_hub_not_initialized(&outcome) {
                        setup_notice_shown = true;
                        drop(writeln!(out, "{SETUP_GUIDANCE}"));
                    }
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
            match self.after_session(identity, &end) {
                Err(code) => return code,
                Ok(AfterSession::Stop) => return 0,
                Ok(AfterSession::Reconnect(phrase)) => {
                    attempt = attempt.saturating_add(1);
                    let d = reconnect::backoff_with_jitter(attempt, reconnect::jitter());
                    self.say(&format!(
                        "disconnected ({phrase}); reconnecting in {}s (attempt {attempt})",
                        d.as_secs()
                    ));
                    tokio::time::sleep(d).await;
                }
            }
        }
    }

    /// 握手成功之后：记账 + 跑一次会话。
    ///
    /// **会话结束 = 通道全部释放**：通道与连接同生共死（约束 4：每设备
    /// 一条 WS）。不释放的话，旧连接留下的 PTY 会在重连后继续往新连接上
    /// 推数据——而 Hub 那边已经把账清了（`stream.rs` 的 `drop_node`），
    /// 两边对「断连后通道还在不在」的假设正好相反。
    async fn run_established(
        &mut self,
        identity: &mut Identity,
        mut est: Established,
    ) -> Result<SessionEnd, i32> {
        if let Err(e) = backfill_node_id(identity, est.enrolled, &est.auth_ok.node_id) {
            self.say(&format!("error: cannot record the issued node_id: {e}"));
            return Err(exit_code::CRASH);
        }
        // 记下本次的 gen，**必须落盘**：进程重启后若从 0 开始，
        // 而 Hub 已经发到 42，这条连接永远被判成「已被顶替」——
        // 设备接不上且无自愈路径（子任务 7 标准⑧）。
        // coverage: excluded — `record_gen` 失败同上：需身份文件中途不可写。
        if let Err(e) = identity.record_gen(est.auth_ok.gen) {
            self.say(&format!("error: cannot record the connection gen: {e}"));
            return Err(exit_code::CRASH);
        }
        let interval = Duration::from_millis(est.auth_ok.heartbeat_interval_ms);
        let end = self.session(&mut est, interval).await;
        let released = channel::on_disconnect();
        if released > 0 {
            self.say(&format!(
                "released {released} channel(s) together with the connection"
            ));
        }
        Ok(end)
    }

    /// 会话结束之后的处置。
    ///
    /// **`Superseded` 是终态**（§7.1）：再连一次就是和新连接互抢。它还必须
    /// **持久化**——supervisor 重拉出的新进程什么都不记得，会老老实实再连
    /// 一次，所以由人工介入清除（「被顶替」意味着同 node_id 另有连接活着）。
    fn after_session(
        &mut self,
        identity: &mut Identity,
        end: &SessionEnd,
    ) -> Result<AfterSession, i32> {
        match end {
            SessionEnd::PeerClosed => Ok(AfterSession::Reconnect(
                "peer closed the connection".to_string(),
            )),
            SessionEnd::Transport(detail) => Ok(AfterSession::Reconnect(detail.clone())),
            SessionEnd::Superseded { gen } => {
                // coverage: excluded — `mark_superseded` 失败需身份文件中途不可写。
                if let Err(e) = identity.mark_superseded(&crate::now_secs()) {
                    self.say(&format!("error: cannot persist the superseded mark: {e}"));
                    return Err(exit_code::CRASH);
                }
                self.say(&format!(
                    "disconnected (superseded, gen {gen}); stopping without reconnecting"
                ));
                Ok(AfterSession::Stop)
            }
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
                        self.pump_channel(est, &channel_id, &frames).await;
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

    /// 把一轮 `channel::pump` 的某一组帧发出去。**失败只留痕**，理由同
    /// [`Self::send_channel_frames`]：Hub 判离站靠收不到心跳。
    ///
    /// 单独拆成一个方法（而不是内联在 `session` 的 `select!` 臂里）是因为
    /// 那条臂只能在真连接上触达，而「发不出去时留痕」这件事值得被单独
    /// 断言：它是 `pump` 语义的最后一环，也是唯一一句说得出
    /// 「哪条通道、攒了多少帧、为什么没发走」的日志。
    async fn pump_channel(
        &self,
        est: &mut Established,
        channel_id: &str,
        frames: &[protocol::Frame],
    ) {
        if let Err(e) = self.send_channel_frames(est, channel_id, frames).await {
            self.say(&format!(
                "pumped frame send failed ({} {}): {}",
                channel_id,
                frames.len(),
                outcome_phrase(&e)
            ));
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
        ConnOutcome::Rejected { .. } if is_hub_not_initialized(outcome) => {
            // **这一种必须单独说清楚**：它意味的不是「Hub 拒绝了这台设备」，
            // 而是「Hub 还没有设密码」。两台进程谁都没坏——所以一句
            // `hub refused: hub not initialized` 之后人不知道该动哪边。
            //
            // 最常见的来路是 Hub 换了数据目录 / 重装过：设备带着 M0 的旧身份
            // 去连一台全新的 Hub，而 Hub 此时还没有用户，于是 enroll 一律被拒
            // （`hub-server/src/routes/ws.rs` 的 `hub_is_initialized` 判据）。
            //
            // **它不是终态**：等人在浏览器里做完 setup 就好了，所以设备自己
            // 会一直重试——短语里必须说清这一点，否则用户以为要去重启设备。
            "the Hub has no password yet: complete first-run setup in the browser (it prints a setup token); this device keeps retrying".to_string()
        }
        ConnOutcome::Rejected { reason } => format!("hub refused: {reason}"),
        // 只在「握手成功却被当成失败传进来」时才可能发生——那是 bug 不是状态。
        ConnOutcome::Authed { node_id, .. } => format!("already authed as {node_id}"),
    }
}

/// 这次失败是不是「Hub 还没初始化」。
///
/// **单独一个函数而不是内联比较**：它同时决定「要不要退避重试」
/// （[`crate::reconnect::retry_after`]）与「要不要给用户指路」，
/// 两处必须对同一件事给同一个答案。
fn is_hub_not_initialized(outcome: &ConnOutcome) -> bool {
    matches!(
        outcome,
        ConnOutcome::Rejected { reason } if reason == protocol::message::reason::HUB_NOT_INITIALIZED
    )
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
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

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

    /// 「Hub 还没设密码」要说出下一步做什么，而不是一句谁都看不懂的拒绝。
    ///
    /// 事实来源：`hub_server::routes::ws` 的 `hub_is_initialized` 判据在
    /// enroll 之前先看，未初始化就连签名都不验直接拒。最常见的来路是 Hub
    /// 换了数据目录或重装过，于是设备带旧身份连一台全新 Hub。
    #[test]
    fn an_uninitialized_hub_is_named_as_such_not_a_bare_refusal() {
        let phrase = outcome_phrase(&ConnOutcome::Rejected {
            reason: protocol::message::reason::HUB_NOT_INITIALIZED.into(),
        });
        assert!(
            phrase.contains("setup"),
            "短语必须指出「先去完成首次 setup」：{phrase}"
        );
        assert!(
            phrase.contains("keeps retrying"),
            "短语必须说清设备会自己重试——说成「要重启」会让人以为必须手动干预：{phrase}"
        );
        assert!(
            !phrase.contains("restart"),
            "它不是终态，不该让用户去重启设备：{phrase}"
        );
        assert!(
            !phrase.contains("hub refused"),
            "这句话不该把「Hub 没有密码」说成「Hub 拒绝了设备」：{phrase}"
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
                if matches!(tail, HubTail::HubNotInitialized) {
                    let _ = send(&mut ws, auth_fail_not_initialized()).await;
                    let _ = ws.next().await;
                    return;
                }
                let _ = send(&mut ws, auth_ok()).await;
                // coverage: excluded — `unreachable!`：上面已经 return，结构上不可达。
                match tail {
                    HubTail::Reject => unreachable!("上面已返回"),
                    HubTail::HubNotInitialized => unreachable!("上面已返回"),
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
                    HubTail::UnknownFrameThenHang => {
                        // 一帧「整帧合法、类型名没听说过」的帧，之后**挂着不关**。
                        //
                        // 「不关」是这条用例的判据所在：会话若被未知帧打断，
                        // worker 会去重连，而这个夹具只 accept 一次——于是立刻
                        // 现出「connect failed」。会话若还活着，日志里除了
                        // `authed as n_x` 就再没有别的。
                        let _ = send(&mut ws, unknown_frame()).await;
                        futures_util::future::pending::<()>().await;
                    }
                    HubTail::BadChannelPayload => {
                        // 整帧合法，但 `open_channel` 的载荷不是对象：
                        // 协议分叉，worker 必须按 Transport 断连重连。
                        let _ = send(&mut ws, bad_channel_payload()).await;
                        let _ = ws.next().await;
                    }
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

    /// Hub 还没做首次 setup 时的拒绝。**理由短语必须与协议层一致**——
    /// 它是「重试还是停下」的唯一判据（`reconnect::retry_after`）。
    fn auth_fail_not_initialized() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":2,"t":"auth_fail","c":{
            "reason": protocol::message::reason::HUB_NOT_INITIALIZED
        }})
    }

    fn superseded() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":3,"t":"disconnect","c":{"reason":"superseded","gen":2}})
    }

    /// 一帧「整帧合法、类型名没听说过」的帧。
    fn unknown_frame() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":3,"t":"some_future_type","c":{"anything":1}})
    }

    /// 整帧合法，但 `open_channel` 的载荷是数字而不是对象。
    ///
    /// 与 `unknown_frame` 的区别正是这条用例要分的两层：那一层是「没
    /// 听说过」，这一层是「听说过但说岔了」。
    fn bad_channel_payload() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":3,"t":"open_channel","c":5})
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
        /// Hub 还没做首次 setup：`auth_fail{hub not initialized}`。
        HubNotInitialized,
        Hang,
        /// `auth_ok` 之后立刻 RST：worker 的心跳必然发不出去。
        RstAfterAuthOk,
        /// 握手后只读不发，用来抓 worker 主动发出的第一帧。
        ReadOnly,
        /// 握手后发一段不是 JSON 的文本。
        BadJson,
        /// 握手后发一个二进制帧。
        Binary,
        /// 握手后发一帧类型名没听说过的帧，然后**挂着不关**。
        UnknownFrameThenHang,
        /// 握手后发一帧整帧合法、但 `open_channel` 载荷不合协议的帧。
        BadChannelPayload,
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
        let code = w.connect_loop(&mut id, None, std::io::sink()).await;
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
        let code = w.connect_loop(&mut id, None, std::io::sink()).await;
        assert_eq!(code, 0);
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("superseded"), "{log}");
        assert!(log.contains("stopping without reconnecting"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **Hub 还没做首次 setup 不是终态**：设备自己退避重试，并把
    /// 「去浏览器里做完 setup」这句话打到 stdout。
    ///
    /// 判据有三层：① 连接没有停（超时到点 = 还在重试）；② stdout 上有
    /// 可操作指引；③ 日志说的是「keeps retrying」而不是「停下等人」。
    ///
    /// **为什么必须打到 stdout**：用户反馈的原话是「连不上又不报错」，
    /// 而日志要 `afcli log` 才看得见——指路的话必须落在用户已经盯着的
    /// 那块屏幕上。
    #[tokio::test]
    async fn an_uninitialized_hub_keeps_retrying_and_points_at_the_browser() {
        let (d, _w) = worker("hub-not-init");
        let url = fake_hub(HubTail::HubNotInitialized);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut out: Vec<u8> = Vec::new();
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            w.connect_loop(&mut id, None, &mut out),
        )
        .await;
        assert!(code.is_err(), "Hub 没初始化时不该停下来，而该一直重试");
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("setup"), "stdout 必须有可操作指引：{text:?}");
        assert!(
            text.contains("keeps retrying"),
            "指引必须说清设备会自己重试：{text:?}"
        );
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("keeps retrying"), "{log}");
        assert!(
            !log.contains("stopping"),
            "它不是终态，不该说「停下来等人」：{log}"
        );
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
            w.connect_loop(&mut id, None, std::io::sink()),
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
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
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
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
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
            w.connect_loop(&mut id, None, std::io::sink()),
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
            w.connect_loop(&mut id, None, std::io::sink()),
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
        let code = w.connect_loop(&mut id, None, std::io::sink()).await;
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
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
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

    // ---- 通道繁忙时的心跳节奏 ----

    /// 让终端持续产出的命令。
    ///
    /// **必须有间隔**：一条全速输出的命令会把 CPU 与 socket 一起打满，
    /// 那测到的会是「机器被压垮了」而不是「心跳臂被饿死了」。
    const BUSY_COMMAND: &str = "while true; do echo tick; sleep 0.05; done\n";

    /// 一个「握手后一直读」的假 Hub：它记下每次心跳到达的时刻。
    ///
    /// **它必须一直读**：worker 的通道帧与心跳走同一条连接，Hub 不读时
    /// TCP 发送缓冲会满，于是测到的会是「socket 堵了」而不是
    /// 「心跳臂被饿死了」——那是两回事。
    fn fake_hub_recording_heartbeats(interval_ms: u64) -> (String, Arc<Mutex<Vec<Instant>>>) {
        let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&arrivals);
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
                // coverage: excluded — 测试夹具：accept 失败臂在测试自己起的
                // 监听器上构造不出。
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                // coverage: excluded — 同上：upgrade 失败臂。
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let _ = ws.next().await;
                let _ = send(&mut ws, challenge()).await;
                let _ = ws.next().await;
                let _ = send(&mut ws, auth_ok_with_interval(interval_ms)).await;
                // 开一条终端通道并让它持续产出——「通道繁忙」正是本用例的前提。
                let _ = send(&mut ws, open_terminal_channel()).await;
                let _ = send(&mut ws, terminal_input(BUSY_COMMAND)).await;
                while let Some(msg) = ws.next().await {
                    let Ok(tokio_tungstenite::tungstenite::Message::Text(text)) = msg else {
                        continue;
                    };
                    // coverage: excluded — 真 PTY 通道上的断言：worker 只会发
                    // 自己造的 JSON 帧，这条「解析不动」的 continue 在
                    // `fake_hub_recording_heartbeats` 上构造不出（对端发的
                    // 每帧都由 `Frame::push` 保证可解析）。
                    let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
                        continue;
                    };
                    if frame.msg_type == "heartbeat" {
                        sink.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(Instant::now());
                    }
                }
            });
        });
        (format!("ws://{addr}/ws"), arrivals)
    }

    fn auth_ok_with_interval(interval_ms: u64) -> serde_json::Value {
        serde_json::json!({"v":1,"seq":2,"t":"auth_ok","c":{
            "node_id":"n_x","protocol_version":1,"gen":1,"heartbeat_interval_ms":interval_ms
        }})
    }

    /// 开一条终端通道。**信用给得很大**：这条用例要量的是心跳节奏，
    /// 不是信用流控。
    fn open_terminal_channel() -> serde_json::Value {
        serde_json::json!({"v":1,"seq":3,"t":"open_channel","c":{
            "channel_id":"ch-hb",
            "connector":"terminal",
            "params":{"shell":"/bin/sh"},
            "credit_start":100000
        }})
    }

    fn terminal_input(cmd: &str) -> serde_json::Value {
        serde_json::json!({"v":1,"seq":4,"t":"channel_data","c":{
            "channel_id":"ch-hb",
            "payload":{"bytes": cmd}
        }})
    }

    /// 一次**握手成功、随后对端正常关闭**的连接。
    ///
    /// 「帧发不出去」那几条臂都是「只留痕、不断连」，而要让真连接确定地
    /// 写不进去，靠「让对端恰好在两帧之间消失」是个竞态。改用一个确定的
    /// 形状：先读到对端的 Close，此后 tungstenite 的状态就是 Closed，
    /// 再 `send` 必然返回错。
    async fn established_after_peer_close(d: &Path) -> Established {
        let url = fake_hub(HubTail::Close);
        let id = Identity::load_or_create(d).expect("identity");
        let mut est = conn::connect_once(&url, id.key(), None, None, device_info())
            .await
            .expect("handshake");
        assert_eq!(
            est.next_text().await.expect("close"),
            None,
            "对端应当正常关闭"
        );
        est
    }

    /// `auth_ok` 之后那笔**必须落盘**的账（首次 enroll，`node_id` 由 Hub
    /// 签发）写不进去时，worker 必须按「这一版坏了」停下。
    ///
    /// 构造是把身份文件写成不可解析的 JSON：`set_node_id` 走「读-改-原子写」，
    /// 读不动就报错。用损坏文件而不是只读文件，是因为 root 读得穿
    /// `chmod 000`，那条路在 CI 里会静默变成空转（同
    /// `backfill_reports_a_failed_write_instead_of_succeeding_quietly`）。
    #[tokio::test]
    async fn an_identity_that_cannot_record_the_issued_node_id_stops_the_loop() {
        let (d, _w) = worker("record-node-id-fail");
        let mut id = Identity::load_or_create(&d).expect("identity");
        std::fs::write(id.path(), b"{ not json").expect("corrupt");
        // node_id 为 None = enroll 形态：假 Hub 的 auth_ok 就是「刚签发」。
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::Close));
        let code = w.connect_loop(&mut id, None, std::io::sink()).await;
        assert_eq!(code, exit_code::CRASH, "记不下 node_id 不能算正常退出");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("cannot record the issued node_id"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `record_gen` 写不下盘时同样得停下：进程重启后若从 0 开始，而 Hub
    /// 已经发到 42，这条连接永远被判成「已被顶替」——设备接不上且无自愈
    /// 路径（子任务 7 标准⑧）。
    ///
    /// 让 `backfill` 先变成 no-op（带 `node_id` 重连 = `enrolled: false`），
    /// 否则失败会被前一条臂挡住，这一条永远走不到。
    #[tokio::test]
    async fn an_identity_that_cannot_record_the_gen_stops_the_loop() {
        let (d, _w) = worker("record-gen-fail");
        let mut id = Identity::load_or_create(&d).expect("identity");
        std::fs::write(id.path(), b"{ not json").expect("corrupt");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::Close));
        let code = w.connect_loop(&mut id, Some("n_x"), std::io::sink()).await;
        assert_eq!(code, exit_code::CRASH, "gen 没落盘必须停下");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("cannot record the connection gen"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 被顶替要**落盘**；落不下时同样得停下——supervisor 重拉的新进程
    /// 什么都不记得，只看内存的话它会老老实实再连一次，于是和新连接互抢。
    ///
    /// 构造让前面的 `record_gen` 成功而 `mark_superseded` 失败：盘上的
    /// `last_seen_gen` 已高于这次 `auth_ok` 的 gen（`record_gen` 只前进不
    /// 回退，于是它**不写盘**），而残留的 `identity.json.tmp` 让原子写的
    /// `create_new(true)` 每次都失败。
    #[tokio::test]
    async fn a_superseded_mark_that_cannot_be_persisted_stops_the_loop() {
        let (d, _w) = worker("superseded-mark-fail");
        let mut id = Identity::load_or_create(&d).expect("identity");
        id.record_gen(5).expect("record a newer gen");
        std::fs::write(d.join("identity.json.tmp"), b"").expect("leftover tmp");
        // 带 node_id 重连：`enrolled = false`，于是 backfill 是 no-op。
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::Superseded));
        let code = w.connect_loop(&mut id, Some("n_x"), std::io::sink()).await;
        assert_eq!(code, exit_code::CRASH, "裁决没落盘就必须停下");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("cannot persist the superseded mark"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 通道帧**发不出去只留痕**，不当成断连理由：Hub 判离站靠收不到心跳。
    ///
    /// 两个发送点各走一次：泵臂的聚合发送（`send_channel_frames`）与它的
    /// 留痕包装（`pump_channel`）。那句日志是「哪条通道、攒了多少帧、
    /// 为什么没发走」的唯一去处。
    #[tokio::test]
    async fn channel_frames_that_cannot_be_sent_only_leave_a_trace() {
        let (d, _w) = worker("channel-send-fail");
        let w = Worker::new(d.clone(), "ws://hub/ws".to_string());
        let mut est = established_after_peer_close(&d).await;
        let frames = vec![Frame::push("channel_data", 0, serde_json::json!({"c":{}}))];
        let direct = w.send_channel_frames(&mut est, "ch-p", &frames).await;
        assert!(direct.is_err(), "对端已关闭，发送必须报错");
        w.pump_channel(&mut est, "ch-p", &frames).await;
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(
            log.contains("channel frame send failed (ch-p channel_data)"),
            "{log}"
        );
        assert!(log.contains("pumped frame send failed (ch-p 1)"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `on_channel_message` 的三条归类：不是通道帧 / 吸收掉 / 要回帧。
    ///
    /// `Action::None` 那条在真会话里没有任何可观察行为（那正是它存在的
    /// 理由：§5 的未知通道静默），所以只能在这一层单独钉住——否则「静默」
    /// 与「根本没走到这里」在日志上长得一模一样。
    #[tokio::test]
    async fn on_channel_message_sorts_frames_into_none_and_send() {
        let (d, _w) = worker("channel-message-sort");
        let w = Worker::new(d.clone(), "ws://hub/ws".to_string());
        let mut est = established_after_peer_close(&d).await;
        let not_channel = frame_of("heartbeat", serde_json::json!({"anything":1}));
        assert!(w.on_channel_message(&mut est, &not_channel).await.is_none());
        let absorbed = frame_of(
            "channel_data",
            serde_json::json!({"channel_id":"ch-never-opened","payload":{"bytes":"x"}}),
        );
        let absorbed_end = w.on_channel_message(&mut est, &absorbed).await;
        assert!(absorbed_end.is_none(), "打在没开过的通道上必须静默（§5）");
        // 要回帧：`close_channel` 对表里没有的通道也回一帧 `close_channel`。
        let closing = frame_of(
            "close_channel",
            serde_json::json!({"channel_id":"ch-never-opened","reason":"done"}),
        );
        let closing_end = w.on_channel_message(&mut est, &closing).await;
        assert!(
            closing_end.is_none(),
            "回帧发不出去不是协议分叉，会话不该结束"
        );
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(
            log.contains("channel frame send failed (close_channel)"),
            "{log}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- session 内帧分派的各条臂 ----

    /// `on_control_frame` 对**非 disconnect** 的一律忽略，哪怕载荷是坏的。
    ///
    /// 反例就在旁边：把它写成「先解载荷、解不动就断连」，那么 Hub 发一帧
    /// 未知类型（其载荷按 `Disconnect` 解必然失败）就会把设备踢下线——
    /// 这正是约束 14 要挡的。
    #[test]
    fn a_frame_that_is_not_disconnect_never_ends_the_session() {
        let (d, w) = worker("ctrl-not-disconnect");
        for t in ["auth_ok", "disconnectx", "disconnec", "DISCONNECT"] {
            let f = frame_of(t, serde_json::json!({"reason":12345}));
            assert!(
                w.on_control_frame(&f).is_none(),
                "{t} 不是 disconnect，必须忽略（载荷也是坏的）"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `disconnect{admin}` → `PeerClosed`，设备照常退避重连。
    ///
    /// 只有 `superseded` 是终态（§7.1）；用户手动下线意味着**设备没坏**，
    /// 停下来等人是错的——它该自己找回来。
    ///
    /// 反向对照放在同一条里：`superseded` 必须**确实**给出终态，否则
    /// 「一律返回 PeerClosed」的实现会被这条同时放行。
    #[test]
    fn an_admin_disconnect_keeps_the_device_alive() {
        let (d, w) = worker("ctrl-reason");
        let admin = frame_of("disconnect", serde_json::json!({"reason":"admin"}));
        assert!(
            matches!(w.on_control_frame(&admin), Some(SessionEnd::PeerClosed)),
            "手动下线不是终态，设备该继续重连"
        );
        let superseded = frame_of("disconnect", serde_json::json!({"reason":"superseded"}));
        let ended = w.on_control_frame(&superseded);
        assert!(
            matches!(ended, Some(SessionEnd::Superseded { .. })),
            "被顶替才是终态"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `disconnect` 的 `reason` **不在枚举里** → 按 `bad disconnect` 结束。
    ///
    /// 它是协议分叉而不是「这条消息没听懂」：`reason` 决定的是「停下还是
    /// 重连」这种**不可逆**的动作，猜错方向最坏的形状是设备永久停摆
    /// （当成 superseded）或永久撞墙（当成 admin 反复重连）。所以不猜，
    /// 按 Transport 退避重连并留痕。
    #[test]
    fn an_unknown_disconnect_reason_is_not_guessed() {
        let (d, w) = worker("ctrl-unknown-reason");
        for reason in ["shutdown", "some_future_reason", ""] {
            let f = frame_of("disconnect", serde_json::json!({"reason":reason}));
            match w.on_control_frame(&f) {
                Some(SessionEnd::Transport(detail)) => {
                    assert!(detail.starts_with("bad disconnect:"), "{detail}")
                }
                // coverage: excluded — 只在上面这条断言失败时执行；`panic!` 的
                // 参数行被 llvm-cov 记为 miss（多行宏已知现象）。
                other => panic!("{reason} 不该被猜成 {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `outcome_phrase` 的 `Authed` 臂说的是「已经认证过了」，并**点名
    /// node_id**——那是「握手成功却被当成失败传进来」时才可能出现的
    /// 分支（bug 不是状态），日志要能直接指到那个 node。
    #[test]
    fn an_authed_outcome_says_which_node_it_was() {
        let phrase = outcome_phrase(&ConnOutcome::Authed {
            node_id: "n_z".into(),
            protocol_version: 1,
            gen: 5,
            heartbeat_interval_ms: 15_000,
            enrolled: true,
        });
        assert!(phrase.contains("n_z"), "{phrase}");
        assert!(phrase.contains("authed"), "{phrase}");
    }

    /// `is_hub_not_initialized` 只对那一个协议理由为真。
    ///
    /// 它同时决定「要不要退避重试」与「要不要给用户指路」，两处必须对
    /// 同一件事给同一个答案——所以判据在这里单独钉住，包括大小写与
    /// 非 `Rejected` 的反例（把理由串塞进 `Transport` 的 detail 里不算）。
    #[test]
    fn only_the_hub_not_initialized_reason_counts() {
        assert!(is_hub_not_initialized(&ConnOutcome::Rejected {
            reason: protocol::message::reason::HUB_NOT_INITIALIZED.into(),
        }));
        for other in ["identity mismatch", "", "HUB_NOT_INITIALIZED"] {
            assert!(
                !is_hub_not_initialized(&ConnOutcome::Rejected {
                    reason: other.into(),
                }),
                "{other} 不该被当成「Hub 还没初始化」"
            );
        }
        assert!(!is_hub_not_initialized(&ConnOutcome::Transport {
            detail: protocol::message::reason::HUB_NOT_INITIALIZED.into(),
        }));
    }

    /// `after_session` 的三种归类：只有 `Superseded` 是终态，且**必须落盘**。
    ///
    /// 落盘那一半不是附带检查：supervisor 重拉出的新进程什么都不记得，
    /// 只看内存的话它会老老实实再连一次。
    #[test]
    fn after_session_only_stops_on_superseded_and_persists_it() {
        let (d, _w) = worker("after-session");
        let mut w = Worker::new(d.clone(), "ws://hub/ws".to_string());
        let mut id = Identity::load_or_create(&d).expect("identity");

        assert!(matches!(
            w.after_session(&mut id, &SessionEnd::PeerClosed),
            Ok(AfterSession::Reconnect(_))
        ));
        assert!(matches!(
            w.after_session(&mut id, &SessionEnd::Transport("boom".into())),
            Ok(AfterSession::Reconnect(_))
        ));
        assert!(matches!(
            w.after_session(&mut id, &SessionEnd::Superseded { gen: 9 }),
            Ok(AfterSession::Stop)
        ));
        let reread = Identity::load_or_create(&d).expect("reread");
        assert!(reread.is_superseded(), "裁决必须持久化，重启后不再重连");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `send_heartbeat` 在一段**活着的**连接上必须发得出去。
    ///
    /// 这条用例一度是空的：只断言「日志里没有 heartbeat send failed」，
    /// 而把函数体改成直接 `return Ok(())`（一帧都不发）时它照样是绿的——
    /// 因为不发也就不会失败。所以判据改成**在对端计数**：假 Hub 记下行的
    /// 每一帧，`heartbeat` 至少到一次。
    ///
    /// 与 `the_first_heartbeat_arrives_without_waiting_for_the_interval`
    /// 的分工：那条量的是「第一个周期之内就到」，这条量的是「确实到了」。
    #[tokio::test]
    async fn a_heartbeat_on_a_live_connection_is_sent() {
        let (d, _w) = worker("hb-live");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let (url, arrivals) = fake_hub_recording_heartbeats(15_000);
        let mut w = Worker::new(d.clone(), url);
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        let log = wait_for_log(&d, "authed as n_x", std::time::Duration::from_secs(5)).await;
        assert!(!log.contains("heartbeat send failed"), "{log}");
        // 对端真的收到了：这是「发出去了」的唯一判据。
        assert!(
            !arrivals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "对端一帧心跳都没收到"
        );
        h.abort();
        crate::channel::on_disconnect();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **`session` 里来的未知帧类型**：整帧是合法 JSON、类型名却不是
    /// 任何一种通道帧 → 忽略，会话继续。
    ///
    /// 上面 `a_frame_that_is_not_disconnect_never_ends_the_session` 钉的是
    /// `on_control_frame` 那一层；这条钉的是**同一帧继续走到**
    /// `on_channel_message` 之后也返回 `None`。少了它，「未知类型」的
    /// 忽略就只被验证了一半——而 `session` 是先问控制面再问通道面的，
    /// 两处都得给 `None` 才行。
    ///
    /// 判据是「连接没断」：Hub 发完未知帧后再正常 Close，于是 loop 会走
    /// 完整的 `PeerClosed → 重连` 一路；而它断没断，看的是日志里有没有
    /// 「reconnecting」以及**有没有**把它当成协议错误。
    #[tokio::test]
    async fn an_unknown_frame_from_the_hub_does_not_end_the_session() {
        let (d, _w) = worker("unknown-in-session");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::UnknownFrameThenHang));
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        let log = wait_for_log(&d, "authed as n_x", std::time::Duration::from_secs(5)).await;
        assert!(log.contains("authed as n_x"), "{log}");
        // 给未知帧留出被打断的时间：3s 足够一次完整的「断连 → 退避 → 重连」。
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(!log.contains("reconnecting"), "未知帧不该让会话结束：{log}");
        assert!(!log.contains("disconnected"), "未知帧不该让会话结束：{log}");
        assert_eq!(
            log.matches("authed as n_x").count(),
            1,
            "未知帧若打断了会话，worker 会去重连，于是握第二次手：{log}"
        );
        h.abort();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **通道帧载荷解不开** → `Action::End` → 会话按 `Transport` 结束并
    /// 退避重连，日志**点名是哪一种帧坏了**。
    ///
    /// 这与「帧本身不是 JSON」是两层：那一层是整帧解不开（`bad frame
    /// json`），这一层是整帧合法、载荷不合协议。后者是**协议分叉**——
    /// 留着这条连接只会让后面每一帧都解不开，所以必须断。
    ///
    /// 判据取 `bad open_channel payload` 这个完整短语：只断言「重连了」
    /// 的话，把 `Action::End` 改成 `Action::None` 的实现照样是绿的，而
    /// 那个改法会让设备在协议分叉时安静地留着一条坏连接。
    #[tokio::test]
    async fn a_channel_frame_with_an_unparsable_payload_ends_the_session() {
        let (d, _w) = worker("bad-channel-payload");
        let mut id = Identity::load_or_create(&d).expect("identity");
        let mut w = Worker::new(d.clone(), fake_hub(HubTail::BadChannelPayload));
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        let log = wait_for_log(
            &d,
            "bad open_channel payload",
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(
            log.contains("reconnecting"),
            "载荷解不开要按 Transport 重连：{log}"
        );
        h.abort();
        crate::channel::on_disconnect();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **通道繁忙时心跳仍按时发出**。
    ///
    /// 心跳 ticker 与通道 pump 是 `select!` 的两条独立臂，PTY 的读另在
    /// 独立线程——但那只是**结构**上的保证。这条用例把它变成**行为**上
    /// 的保证：一条持续产出的终端通道摆在旁边，心跳仍然按
    /// `heartbeat_interval_ms` 到达。
    ///
    /// 退化形态（把 pump 臂改成串行 `await`、或在这一臂里做阻塞 IO）
    /// 会让心跳迟到甚至停摆——那正是这条用例要挡住的。
    #[tokio::test]
    async fn a_busy_channel_does_not_starve_the_heartbeat() {
        let (d, _w) = worker("hb-busy");
        let (url, arrivals) = fake_hub_recording_heartbeats(300);
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        // 让会话跑起来：握手、开通道、持续产出。
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        h.abort();
        crate::channel::on_disconnect();

        let seen = arrivals.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // coverage: excluded — `assert!` 的格式参数行（多行宏已知现象）：只在
        // 上面这条断言失败时才执行。
        assert!(
            seen.len() >= 5,
            "4 秒里只收到 {} 次心跳（周期 300ms）：心跳被通道饿死了",
            seen.len()
        );
        let mut max_gap = std::time::Duration::ZERO;
        for pair in seen.windows(2) {
            max_gap = max_gap.max(pair[1].saturating_duration_since(pair[0]));
        }
        assert!(
            max_gap < std::time::Duration::from_millis(1500),
            "心跳最大间隔 {max_gap:?} 远超周期 300ms"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
