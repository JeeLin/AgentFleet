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

use crate::agent::conversation::CancelError;
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
    /// 本机 agent 探测结果的缓存（要求 1「cli 能上报 agent 运行时」）。
    ///
    /// 心跳**读它**、不探测（探测会起子进程并最多等 2s，放进心跳臂等于让
    /// 心跳节奏取决于本机装了什么 agent）。刷新在 `run_with` 里做，不在
    /// `select!` 的阻塞路径上。
    detected: crate::agent::Detected,
    /// 本连接的 ACP 对话会话表。
    ///
    /// **`Arc` 是因为 `session(&self)` 是不可变借用**，而读事件的 task 要持有
    /// 客户端的克隆。表本身放在 `Mutex` 里（`Conversations` 内部）而不是放
    /// worker 上，是为了让「哪条会话在跑」在 `select!` 的每一臂里都查得到。
    conversations: std::sync::Arc<crate::agent::Conversations>,
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
            data_dir: data_dir.clone(),
            hub_url,
            log: Box::new(log),
            detected: crate::agent::Detected::new(),
            conversations: std::sync::Arc::new(crate::agent::Conversations::new()),
        }
    }

    /// 阻塞运行，返回进程退出码。正常情况下不返回。
    ///
    /// `out` 是**给用户看的那块屏幕**（进程 stdout）：有些话必须说到用户
    /// 眼前，而不是只留在日志文件里——日志要 `afcli log` 才看得见，而
    /// 「Hub 还没设密码，去浏览器里做完 setup」这句话正是这种话。
    pub fn work<W: std::io::Write>(&mut self, out: W) -> std::process::ExitCode {
        self.rotate_log();
        // coverage: excluded — `work` 是进程入口，只能靠 spawn 真二进制走到：
        // `load_or_create` 的成功臂、失败臂与整条 `run_with` 链在单测里都触达不到
        // （`run_with` 自建多线程 runtime，嵌在 `#[tokio::test]` 里会 panic）。
        let identity = match Identity::load_or_create(&self.data_dir) {
            Ok(id) => id,
            // 身份文件坏了/读不了时**不能**重新生成：那等于换了一台设备，
            // 会撞上「指纹已存在则拒绝」，设备永久无法接入。所以停下来。
            Err(e) => {
                self.say(&format!("error: cannot load identity: {e}"));
                return std::process::ExitCode::FAILURE;
            }
        };
        self.run_with(identity, out)
    }

    /// 身份已备好之后的全部流程。
    ///
    /// 与 [`Self::work`] 分开：`work` 是进程入口（只能靠 spawn 二进制走到），
    /// 这里是「有了身份之后干什么」。拆开不是为了复用，是为了能测——
    /// 单测覆盖 `connect_loop` 与 `session`（`run_with` 自身是 `-> ExitCode`
    /// 的进程入口，没有对应用例，见下一行的 `coverage: excluded`）。
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
            // 这一行的豁免由 `run_with` 上的标注（`:141`）给出：它是同一段
            // 「进程入口」代码。**不在这里再写一条 `coverage: excluded`**——
            // 一条指向别处的标注不带自己的理由，`--check` 会把它记成孤儿
            // （没有登记行引用它），门禁就永远红。
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
            Err(e) => {
                // coverage: excluded — 建不出 OS 线程（fd / 内存耗尽 / 线程数受限）
                // 时 Tokio 才拒绝；测试环境里无可控构造手段，属防御性兜底。
                self.say(&format!("error: cannot start the async runtime: {e}"));
                return std::process::ExitCode::FAILURE;
            }
        };
        // 退出码协议里的三个非零码都带「这一版坏了」的语义，
        // 而 worker 自己不打算判自己坏——它只会「正常停下」。
        // coverage: excluded — 同上：`block_on` 是进程入口路径。
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
                        // 本进程要走了（CRASH）：它起的 ACP 子进程没人接手就是孤儿，
                        // 收掉。断连不走这里——那时进程还在，会话要留给重连续上。
                        Err(code) => {
                            self.conversations.close_all().await;
                            return code;
                        }
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
        if let Err(e) = identity.record_gen(est.auth_ok.gen) {
            self.say(&format!("error: cannot record the connection gen: {e}"));
            return Err(exit_code::CRASH);
        }
        let interval = Duration::from_millis(est.auth_ok.heartbeat_interval_ms);
        // 探测本机 agent，**在心跳循环之前、`select!` 之外**（要求 1）。
        //
        // 放这里的两个理由：
        // 1. 探测起子进程、最多等 2s——放进心跳臂会让心跳的准时性取决于本机
        //    装了什么 agent（M1 的 `exec` 堵住心跳臂就是这个形状）。
        // 2. 放在每条连接建立后，而不是进程启动时一次：设备上装完 agent
        //    （子任务 4）之后重连就能报出来，不必重启进程。这正是要求 2
        //    「装完 → 心跳回报就绪」依赖的那条路径。
        //
        // 探测失败不影响连接：拿不到就是空/不可用，**不编造**。
        let snap = self.detected.refresh();
        let available = snap.agents.iter().filter(|a| a.available).count();
        self.say(&format!(
            "detected {} agent(s), {available} available",
            snap.agents.len()
        ));
        let end = self.session(&mut est, interval).await;
        let released = channel::on_disconnect();
        if released > 0 {
            self.say(&format!(
                "released {released} channel(s) together with the connection"
            ));
        }
        // 通道表清了，这些对话再也不会收到 `CLOSE_CHANNEL`：标成孤儿供淘汰挑，
        // **不杀进程**——重连要能续上（见 `conversation` 模块注释「会话的生死」）。
        let orphaned = self.conversations.mark_orphaned();
        if orphaned > 0 {
            self.say(&format!("{orphaned} acp conversation(s) left orphaned"));
        }
        Ok(end) // 上面这段必须在 `on_disconnect()` 之后：清表之后它们才不可达
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
        // 入站消息的轮询周期。**比 `pump` 慢一个量级**：它只承载「用户点了
        // 发送」这种人手速率的动作，30 ms 一轮等于每秒 33 次空查一张表。
        // 100 ms 在人的感知里仍然是「点了就发」。
        let mut inbound = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // coverage: excluded — 心跳 tick 臂：需让 ticker 在 session
                    // 内触发而 read 不返回。
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
                //
                // 本臂（下面 `pump_channel` 那一行）的覆盖性是**非确定**的：
                // `select!` 对多条就绪的臂**随机轮询**，进不进这一臂取决于 Tokio
                // 每次选中 ticker、pump 还是 read。于是同一次代码每次 llvm-cov
                // 实跑的结果都可能不同（实测四次实跑报错 8 / 3 / 34 条且集合
                // 互不重合）。它不是「不可达」而是「非确定可达」——按确定性行
                // 做双向核对，门禁会随机红。判据由 `channel::pump` 自己的用例给。
                _ = pump.tick() => {
                    for (channel_id, frames) in channel::pump() {
                        self.pump_channel(est, &channel_id, &frames).await; // coverage: excluded (racy) — select! 竞争臂，见上。
                    }
                }
                // 对话通道的**入站**消息（用户点了发送）。
                //
                // 与 `pump` 是相反方向、也必须独立成一条臂：`pump` 只处理
                // connector 通道的出站，而对话通道的入站消息要交给 ACP
                // 子进程的 stdin。放在 `pump` 臂里会让「用户发言」依赖于
                // 「有没有 connector 通道在产出」。
                //
                // 本臂同样是一条**竞争臂**（理由同 :347）：llvm-cov 下进不进
                // 取决于 Tokio 的随机轮询，按确定性行做双向核对会随机红。
                _ = inbound.tick() => {
                    self.drain_inbound(est).await; // coverage: excluded (racy) — select! 竞争臂，见上。
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

    /// 把对话通道里的入站消息取出来，交给 ACP。
    ///
    /// # 入站消息的形状（对外契约）
    ///
    /// `channel_data.payload` 有两种，由 `type` 之外的**字段本身**区分：
    ///
    /// ```json
    /// { "text": "..." }   // 用户说的一句话 → session/prompt
    /// { "cancel": true }  // 用户按了停止   → session/cancel
    /// ```
    ///
    /// 判据是「有没有 `text`」而不是「有没有 `cancel`」：老形状（只有 `text`）
    /// 必须继续能发话，所以先认 `text`。
    ///
    /// # 为什么 cancel 不新增一种 `channel_data` 类型
    ///
    /// 协议的消息类型是一个封闭枚举（约束 14），而这次改动不需要新类型：
    /// `channel_data.payload` 本来就是「connector 私有形状、Hub 只透传」
    /// （`protocol.md` §4）。与 `resize` 复用同一条路是同一个决定。
    ///
    /// 别的一律忽略，而不是当成 prompt 的全部内容——把一个不认识的形状
    /// 送进 agent，等于让它去解读一段我们没打算说出口的话。
    ///
    /// 两种消息**都不回帧**：成功时页面靠 `turn_end`（或停止后 agent 回的那条
    /// `turn_end`）知道结果；失败时这次的应答已经在 Hub 那侧的 HTTP 响应里
    /// 给了用户，再补一帧就是同一件事说两遍。
    async fn drain_inbound(&self, est: &mut Established) {
        for channel_id in channel::external_ids() {
            for payload in channel::take_inbound(&channel_id) {
                if payload.get("cancel").is_some() {
                    self.cancel_turn(est, &channel_id).await;
                    continue;
                }
                let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) else {
                    self.say(&format!(
                        "dropped an inbound message on {channel_id}: neither a text nor a cancel"
                    ));
                    continue;
                };
                if text.trim().is_empty() {
                    // 空消息不进 agent：它会被当成一轮空输入，而那一轮的
                    // `stopReason` 会让页面画出一条没有任何内容的消息。
                    continue;
                }
                if let Err(e) = self.conversations.prompt(&channel_id, text).await {
                    // 会话没了（通道还在但进程已退）：说出口，并把通道关掉
                    // ——留着一条永远不会回话的通道，页面会一直等。
                    self.say(&format!("prompt failed on {channel_id}: {e}"));
                    self.fail_channel(est, &channel_id, &e).await;
                }
            }
        }
    }

    /// 用户按了停止：中断这一轮，**不关会话**。
    ///
    /// # 为什么这里不 `fail_external`
    ///
    /// 停止失败**不是通道坏了**。`fail_external` 会 `release` 通道并把
    /// `channel_error` 回给 Hub，于是下一次发话要从头开一条通道、`session/load`
    /// 重放一遍——用户只是想掐掉一句话，代价不该是重建整条对话。所以这里
    /// 只留痕，通道原样留着。
    ///
    /// 「没这条会话」是唯一的例外不成立：那说明通道表里有一条 ACP 通道而会话
    /// 表里没有它（进程已退/被淘汰），这条通道确实再也不会回话了——这跟
    /// `prompt` 失败是同一种事实，用同一条路收掉（`fail_channel` 会真的把
    /// `channel_error` 发给 Hub，页面才不会一直停在「正在回复」）。
    async fn cancel_turn(&self, est: &mut Established, channel_id: &str) {
        match self.conversations.cancel(channel_id).await {
            Ok(()) => {}
            // 会话没了：这条通道永远不会再回话，收掉（与 prompt 失败同一条路）。
            Err(e @ CancelError::NoLiveConversation) => {
                self.say(&format!("cancel failed on {channel_id}: {e}"));
                self.fail_channel(est, channel_id, &e.to_string()).await;
            }
            // 本来就闲着：什么都不用做。用户看到的是「没有一轮在跑」，而
            // 这句话由 Hub 的 409 说（它在下发之前就判过了）；设备这侧只是
            // 一个兜底，不值得为它关通道。
            Err(e @ CancelError::NothingInFlight) => {
                self.say(&format!("cancel ignored on {channel_id}: {e}"));
            }
            Err(e @ CancelError::Acp(_)) => {
                self.say(&format!("cancel failed on {channel_id}: {e}"));
            }
        }
    }

    /// 收掉一条再也不会回话的通道，并把理由回给 Hub。
    async fn fail_channel(&self, est: &mut Established, channel_id: &str, reason: &str) {
        for out in channel::fail_external(channel_id, reason) {
            if let Err(e) = est.send(&out).await {
                self.say(&format!(
                    "channel frame send failed ({}): {}",
                    out.msg_type,
                    outcome_phrase(&e)
                ));
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
        // 见 :347 的 `select!` 竞争臂：本函数只由 `pump_channel` 调用，而
        // `pump_channel` 只在 `_ = pump.tick()` 那一臂被选中时才走到，Tokio 对
        // 多条就绪臂随机轮询，故 `frames` 有时非空、有时为空。`Ok(())` 本身恒
        // 可达，但**是否执行**取决于同一场竞争。
        // `(racy)` 必须是尾注：多行注释块会被算到首行，那样 `--racy` 报出的行号
        // 与真正被豁免的那行差几条（实测差 4 行）。
        Ok(()) // coverage: excluded (racy) — 见上：select! 竞争臂的传递依赖。
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
                // **`close_channel` 要连 ACP 子进程一起收掉**：通道关了而进程
                // 还活着，就是一台设备上一个没人能看见、也没人能管的 agent。
                // 这与「断连不杀进程」不矛盾——断连是意外，关通道是用户的意图。
                if frame.msg_type == protocol::message::types::CLOSE_CHANNEL {
                    if let Ok(spec) = frame.payload::<protocol::channel::CloseChannel>() {
                        self.conversations.close(&spec.channel_id).await;
                    }
                }
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
            channel::Action::OpenAcp {
                frames,
                channel_id,
                spec,
            } => {
                // **`channel_ready` 先发**：Hub 在等它，5 s 超时。起子进程可能
                // 慢（agent 启动 + `session/new`），先发 ready 才不会让一个慢
                // agent 被 Hub 判成「不支持对话」。
                for out in &frames {
                    if let Err(e) = est.send(out).await {
                        self.say(&format!(
                            "channel frame send failed ({}): {}",
                            out.msg_type,
                            outcome_phrase(&e)
                        ));
                    }
                }
                // 默认可执行文件来自扫描缓存（**不探测**：这条路径在 `select!`
                // 的读臂上，探测会起子进程并最多等 2s）。
                let default_command = self.detected_command(&spec.agent_id);
                if let Err(reason) = self
                    .conversations
                    .open(*spec, default_command.as_deref())
                    .await
                {
                    self.say(&format!("acp open failed ({channel_id}): {reason}"));
                    // 通道已经开着但起不来：关掉并把理由回给 Hub。不回的话
                    // 页面永远显示「正在回复」。
                    for out in channel::fail_external(&channel_id, &reason) {
                        if let Err(e) = est.send(&out).await {
                            self.say(&format!(
                                "channel frame send failed ({}): {}",
                                out.msg_type,
                                outcome_phrase(&e)
                            ));
                        }
                    }
                }
                None
            }
        }
    }

    /// 扫描缓存里某个 agent 的可执行文件名（`None` = 没扫到或不可用）。
    ///
    /// **读缓存不探测**：调用点在 `select!` 的读臂上，探测会起子进程并最多
    /// 等 2s，那会让「用户点开一条对话」把心跳推迟到超时。
    fn detected_command(&self, agent_id: &str) -> Option<String> {
        let snap = self.detected.snapshot();
        snap.agents
            .iter()
            .find(|a| a.id == agent_id && a.available)
            .and_then(|a| a.path.clone())
    }

    /// 换掉扫描缓存。**只给测试用**：生产代码的缓存由 `run_with` 里的
    /// 一次 `refresh()` 填，没有第二个写入者。
    #[cfg(test)]
    fn set_detected_for_tests(&mut self, snap: crate::agent::cache::Snapshot) {
        self.detected = crate::agent::Detected::with_snapshot(snap);
    }

    /// 发一份心跳。
    ///
    /// 工作区根留空：M0 还没有「用户配置的工作区」这个概念，而本模块
    /// 明确不许遍历文件系统去猜（§2 路径排除规则）。
    async fn send_heartbeat(&self, est: &mut Established) -> Result<(), ConnOutcome> {
        // 读缓存，**不探测**：这条函数在 `select!` 的心跳臂里被调用，
        // 而探测要起子进程并最多等 2 秒——在这里做等于让心跳的准时性
        // 取决于本机装了什么 agent（M1 的 `exec` 在 `open()` 里同步跑命令
        // 就是这么把心跳臂堵住的）。刷新点在 `run_with`，见那里的注释。
        let agents = &self.detected.snapshot().agents;
        let hb = heartbeat::heartbeat_with_agents(Vec::new(), agents);
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
        // 期望值不能来自自己塞进去的 `detail`：`Transport` 臂就是
        // `detail.clone()`，断言 `== detail` 等于把输入抄一遍当期望值。
        // 这里能失败的地方是「短语退化成直接打 Debug」——那才是要挡的。
        assert_ne!(outcome_phrase(&t), format!("{t:?}"));
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
        // 全句比对而不是 `contains("n_y")`：后者在 `Authed` 臂退化成
        // `node_id.clone()`（只剩一个裸 id、没有任何说明）时仍然绿，
        // 而「握手成功却被当成失败传进来时，日志要点名是哪个 node」
        // 这句话需要的正是那层说明。
        assert_eq!(outcome_phrase(&a), "already authed as n_y");
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
        wait_until(
            || match std::fs::read_to_string(&path) {
                Ok(text) if text.contains(needle) => Some(text),
                _ => None,
            },
            budget,
            &format!("日志里始终没出现 {needle}"),
        )
        .await
    }

    /// 轮询 `cond` 直到为真，超时则 `panic!("{msg}")`。
    ///
    /// **为什么单独提出来**：心跳用例原先自己写这个循环，于是「循环体这次跑没跑」
    /// 取决于心跳到得多快——第一次检查就命中时整个循环体不执行，慢一点才执行。
    /// 那是协程调度时序，同一份代码不同次 `llvm-cov` 实跑会一边覆盖、一边不覆盖
    /// （实测四次实跑 A/C 覆盖、B/D 全 miss）。把循环收进一个助手后，助手体由
    /// **每个**调用者共享覆盖：`wait_for_log` 那条路径在日志写盘前必然等满一轮，
    /// 超时分支则由 `assert!` 的失败语义保证（见 `// coverage: excluded`）。
    async fn wait_until<T>(
        cond: impl Fn() -> Option<T>,
        budget: std::time::Duration,
        msg: &str,
    ) -> T {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(v) = cond() {
                return v;
            }
            // coverage: excluded — 超时兜底：只在被测命题不成立（本该红）时执行。
            assert!(Instant::now() < deadline, "{msg}");
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
        // 20s 周期 + 记录到达时刻：判据是「第一帧的到达时刻远早于一个周期」，
        // 而不是「loop 没退出」——后者在 `ReadOnly` 夹具下恒真（夹具 2 秒后
        // 自己关连接，3 秒超时到点 `code` 必为 Err），即使 `send_heartbeat`
        // 一帧都不发也是绿的。
        let (url, arrivals) = fake_hub_recording_heartbeats_no_channel(20_000);
        let mut w = Worker::new(d.clone(), url);
        let started = Instant::now();
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        // 等 5 秒：远短于 20s 周期，但足够长到「等一个周期」的实现交不出货。
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let first = arrivals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first()
            .copied();
        assert!(
            first.is_some(),
            "等了 5 秒（周期是 20s）对端一帧心跳都没收到：实现退化成等周期了"
        );
        let elapsed = first.unwrap().saturating_duration_since(started);
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "第一帧心跳在 {elapsed:?} 才到，而周期是 20s——它没有「auth_ok 后立刻发」"
        );
        h.abort();
        crate::channel::on_disconnect();
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
    ///
    /// `connector` / `open_params` / `input` 描述「这条通道有多忙」：
    /// terminal 是一条持续输出的 PTY，exec 是一条跑不完的一次性命令。
    /// 两者走的是同一条 worker 循环，但对「忙」的定义完全不同——
    /// 一个堵在读线程，一个堵在 `open` 的同步执行上。
    fn fake_hub_recording_heartbeats(
        interval_ms: u64,
        connector: String,
        open_params: serde_json::Value,
        input: Option<String>,
    ) -> (String, Arc<Mutex<Vec<Instant>>>) {
        fake_hub_recording_heartbeats_inner(interval_ms, Some((connector, open_params, input)))
    }

    /// [`fake_hub_recording_heartbeats`] 的「不开通道」版本：只做握手 +
    /// 记录心跳到达时刻。
    ///
    /// 存在的理由：开通道会让夹具 spawn 一条**真 PTY** 线程，而只测心跳
    /// 节奏的用例（`the_first_heartbeat_arrives_without_waiting_for_the_interval`）
    /// 不需要它。那条 PTY 线程在 `abort()` 之后还会活一阵，与同进程里其它
    /// 心跳用例争 CPU——实测它让一个本来稳定的用例变成 flaky。
    fn fake_hub_recording_heartbeats_no_channel(
        interval_ms: u64,
    ) -> (String, Arc<Mutex<Vec<Instant>>>) {
        fake_hub_recording_heartbeats_inner(interval_ms, None)
    }

    fn fake_hub_recording_heartbeats_inner(
        interval_ms: u64,
        channel: Option<(String, serde_json::Value, Option<String>)>,
    ) -> (String, Arc<Mutex<Vec<Instant>>>) {
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
                // 开一条通道并让它忙碌——「通道繁忙」正是那几个用例的前提。
                // 只测心跳节奏的用例不走这条（`channel` 为 None）：它们不需要
                // 真 PTY，而 PTY 线程会在 abort 之后继续活着拖累别的用例。
                if let Some((connector, open_params, input)) = channel {
                    let _ = send(&mut ws, open_channel_frame(&connector, open_params)).await;
                    if let Some(input) = input {
                        let _ = send(&mut ws, channel_input(&input)).await;
                    }
                }
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

    /// 开一条通道。**信用给得很大**：这条用例要量的是心跳节奏，
    /// 不是信用流控。
    fn open_channel_frame(connector: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"v":1,"seq":3,"t":"open_channel","c":{
            "channel_id":"ch-hb",
            "connector": connector,
            "params": params,
            "credit_start":100000
        }})
    }

    /// 通道内一帧输入（只 terminal 用：exec 的命令随 `open` 一起来）。
    fn channel_input(cmd: &str) -> serde_json::Value {
        serde_json::json!({"v":1,"seq":4,"t":"channel_data","c":{
            "channel_id":"ch-hb",
            "payload":{"bytes": cmd}
        }})
    }

    /// 一次**握手成功**、且把 worker 发出的每一帧都记下来的连接。
    ///
    /// 已有的 `fake_hub` 只记心跳到达时刻，看得到「发了几次心跳」却看不到
    /// 「发了哪些帧」。而 `cancel_turn` 的失败臂要证明的恰恰是**某一帧真的
    /// 发出去了**（`channel_error`），不是「有个副作用发生了」——那个副作用
    /// 在修复前就是被绑到局部变量上丢掉的，任何只看本进程状态的断言都测不出。
    async fn established_recording_frames(d: &Path) -> (Established, Arc<Mutex<Vec<Frame>>>) {
        let frames: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&frames);
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
                // coverage: excluded — 测试夹具：accept / upgrade 失败臂在测试
                // 自己起的监听器上构造不出（同 `fake_hub`）。
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let _ = ws.next().await;
                let _ = send(&mut ws, challenge()).await;
                let _ = ws.next().await;
                let _ = send(&mut ws, auth_ok_with_interval(60_000)).await;
                // 一直读到对端关闭：每条上行帧都记下来，**除心跳**——心跳按
                // 周期刷，留着会把断言淹没在噪音里。
                while let Some(msg) = ws.next().await {
                    let Ok(tokio_tungstenite::tungstenite::Message::Text(text)) = msg else {
                        continue;
                    };
                    // coverage: excluded — 同上：worker 只会发 `Frame::push` 造得
                    // 出的 JSON，这条 continue 在夹具上构造不出。
                    let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
                        continue;
                    };
                    if frame.msg_type != "heartbeat" {
                        sink.lock().unwrap_or_else(|e| e.into_inner()).push(frame);
                    }
                }
            });
        });
        let url = format!("ws://{addr}/ws");
        let id = Identity::load_or_create(d).expect("identity");
        let est = conn::connect_once(&url, id.key(), None, None, device_info())
            .await
            .expect("handshake");
        (est, frames)
    }

    /// **`cancel` 撞上「没有活会话」时，必须把 `channel_error` 真的发给 Hub。**
    ///
    /// 修复前的形状是：`channel::fail_external(...)` 的返回帧被绑到局部变量
    /// 上，既没 `send` 也没 `drop` 掉说明——而它旁边那句注释还写着「回帧必须
    /// 发出去才有意义」。于是 Hub 那头永远等不到 `channel_error`，页面停在
    /// 「正在回复」，直到超时。同一次修复前还有一个 `if frames.is_empty()`
    /// 分支：`fail_external` 恒返回恰好一帧，那个分支永远不成立，是死代码。
    ///
    /// 判据**必须在网络上**：绑到变量上和真发出去，在本进程里都表现为「没有
    /// panic、没有日志差异」，只有对端收到了那一帧才能区分。所以夹具是
    /// [`established_recording_frames`]，断言的是对端**收到的帧**。
    ///
    /// 反向验证：把 `self.fail_channel(est, ...)` 换回绑局部变量的老写法，
    /// 这里收不到任何 `channel_error` 帧，`expect` 当场失败。
    #[tokio::test]
    async fn a_cancel_with_no_live_conversation_actually_tells_the_hub() {
        let (d, w) = worker("cancel-no-live");
        let (mut est, recorded) = established_recording_frames(&d).await;

        // 这条通道上从没开过对话：`cancel()` 走 `NoLiveConversation` 那一臂。
        w.cancel_turn(&mut est, "ch-never-opened").await;

        // 帧由夹具那个线程异步推过来，读之前给它一点时间。
        let mut got = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            got = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if !got.is_empty() {
                break;
            }
        }
        let err = got
            .iter()
            .find(|f| f.msg_type == "channel_error")
            .unwrap_or_else(|| {
                panic!("没收掉一条没有活会话的通道时，Hub 必须收到 channel_error，实收：{got:?}")
            });
        assert_eq!(
            err.c.get("channel_id").and_then(|v| v.as_str()),
            Some("ch-never-opened"),
            "channel_error 必须指着那条通道"
        );
        let _ = std::fs::remove_dir_all(&d);
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
        // 不开通道：本条的判据只是「心跳到过对端一次」，而通道会让夹具
        // spawn 一条真 PTY 线程——它在 `abort()` 之后还活着，与同进程里
        // 其它心跳用例争 CPU。实测那让本用例在全量并发下偶发失败
        // （当时是 `check-coverage` 连跑两次都挂在它这里，单独跑却 5 次全过；
        // 那个脚本在 v0.7.2 已删，改由 `mise run cov-gate` 跑全量插桩，现象同源）。
        // 需要「通道在场」的是 `a_busy_terminal_channel_does_not_starve_the_heartbeat`
        // 那两条，它们仍然用带通道的夹具。
        let (url, arrivals) = fake_hub_recording_heartbeats_no_channel(15_000);
        let mut w = Worker::new(d.clone(), url);
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        let log = wait_for_log(&d, "authed as n_x", std::time::Duration::from_secs(5)).await;
        assert!(!log.contains("heartbeat send failed"), "{log}");
        // 对端真的收到了：这是「发出去了」的唯一判据。
        //
        // **必须等，不能「authed 一落盘就查」**：`wait_for_log` 只保证日志
        // 文件里出现了那行字，而 worker 是在 auth_ok 之后才调
        // `send_heartbeat`——两步之间没有同步。早先这里查到的是「日志有了」
        // 而不是「心跳到了」，于是它在全量并发下偶发失败（实测 5 次里挂 1 次），
        // 而单独跑 5 次全过：慢机器上那两步被拉开，日志先到、心跳还没发。
        wait_until(
            || {
                (!arrivals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty())
                .then_some(())
            },
            std::time::Duration::from_secs(5),
            "等了 5 秒（authed 已完成）对端一帧心跳都没收到",
        )
        .await;
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

    /// **通道繁忙时心跳仍按时发出**（terminal：一条持续输出的 PTY）。
    ///
    /// 心跳 ticker 与通道 pump 是 `select!` 的两条独立臂，PTY 的读另在
    /// 独立线程——但那只是**结构**上的保证。这条用例把它变成**行为**上
    /// 的保证：一条持续产出的终端通道摆在旁边，心跳仍然按
    /// `heartbeat_interval_ms` 到达。
    ///
    /// 退化形态（把 pump 臂改成串行 `await`、或在这一臂里做阻塞 IO）
    /// 会让心跳迟到甚至停摆——那正是这条用例要挡住的。
    #[tokio::test]
    async fn a_busy_terminal_channel_does_not_starve_the_heartbeat() {
        let (d, _w) = worker("hb-busy");
        let (url, arrivals) = fake_hub_recording_heartbeats(
            300,
            "terminal".to_string(),
            serde_json::json!({ "shell": "/bin/sh" }),
            Some(BUSY_COMMAND.to_string()),
        );
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

    /// 同上，但繁忙的是 **exec**：一条跑不完的一次性命令。
    ///
    /// 这条与上面那条不是重复——它们的「忙」堵在不同地方。terminal 堵在
    /// pump 臂（PTY 输出由子进程写出），exec 堵在 `open` 的同步执行上
    /// （旧实现把整条命令跑在 `on_channel_frame` 里）。同一条 `select!`
    /// 要同时扛住这两种形状，而第二种曾经真的让设备变成哑的：
    /// 默认超时 30 秒、`timeout_ms = 0` 无限，期间心跳一帧都发不出去。
    #[tokio::test]
    async fn a_slow_exec_channel_does_not_starve_the_heartbeat() {
        let (d, _w) = worker("hb-exec");
        let (url, arrivals) = fake_hub_recording_heartbeats(
            300,
            "exec".to_string(),
            serde_json::json!({
                "command": "/bin/sh",
                "args": ["-c", "sleep 30; printf done"],
                "timeout_ms": 0,
            }),
            None,
        );
        let mut w = Worker::new(d.clone(), url);
        let mut id = Identity::load_or_create(&d).expect("identity");
        let h = tokio::spawn(async move { w.connect_loop(&mut id, None, std::io::sink()).await });
        // 命令要跑 30 秒，这里只取 4 秒的心跳节奏——够看出堵不堵。
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        h.abort();
        crate::channel::on_disconnect();

        let seen = arrivals.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            seen.len() >= 5,
            "4 秒里只收到 {} 次心跳（周期 300ms）：exec 把 open 堵死了",
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

    // ---- 默认可执行文件取自扫描缓存 ----

    fn report(id: &str, available: bool, path: Option<&str>) -> crate::agent::cache::AgentReport {
        crate::agent::cache::AgentReport {
            id: id.to_string(),
            name: id.to_string(),
            version: Some("1.0.0".to_string()),
            available,
            error: None,
            path: path.map(str::to_string),
        }
    }

    fn snapshot_of(agents: Vec<crate::agent::cache::AgentReport>) -> crate::agent::cache::Snapshot {
        crate::agent::cache::Snapshot {
            agents,
            at: Some(Instant::now()),
        }
    }

    /// `open_channel{acp}` 没给 `command` 时，设备回落到**扫描到的那个**
    /// 可执行文件——这是「扫描 + 配置」里「扫描」那一半真正起作用的地方。
    #[test]
    fn the_default_command_is_the_scanned_path_of_an_available_agent() {
        let (d, mut w) = worker("detected-cmd");
        w.set_detected_for_tests(snapshot_of(vec![report(
            "opencode",
            true,
            Some("/opt/opencode"),
        )]));
        assert_eq!(
            w.detected_command("opencode").as_deref(),
            Some("/opt/opencode")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 不可用的 agent **不给**默认可执行文件。
    ///
    /// 它扫到了但缺 ACP 入口（`claude` 那种）：回落给它等于让会话在
    /// `initialize` 上永远等不到回答，而页面只会显示「正在回复」。
    /// 这里回 `None`，`Conversations::open` 才会报「没有可执行文件」。
    #[test]
    fn an_unavailable_agent_yields_no_default_command() {
        let (d, mut w) = worker("detected-unavail");
        w.set_detected_for_tests(snapshot_of(vec![
            report("opencode", true, Some("/opt/opencode")),
            report("claude", false, Some("/opt/claude")),
            report("bare", true, None),
        ]));
        assert_eq!(w.detected_command("claude"), None, "不可用就不给命令");
        assert_eq!(w.detected_command("bare"), None, "没扫到路径也给不出命令");
        assert_eq!(w.detected_command("never-heard-of-it"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- 入站消息的分派（`channel_data.payload` 的对外契约）----

    /// 往一条外部通道里塞一条入站消息，走**真实那条路**（`on_channel_frame`）——
    /// 直接改表内队列会绕过 `on_data` 的「外部源通道才排队」判定，测的就不是
    /// 生产路径了。
    fn push_inbound(channel_id: &str, payload: serde_json::Value) {
        let f = frame_of(
            "channel_data",
            serde_json::json!({"channel_id": channel_id, "payload": payload}),
        );
        // 外部源通道收下行数据只入队：回的是**空帧列表**（`Action::Send([])`），
        // 不是 `Action::None`——两者在 worker 那边都表现为「没有要发的东西」。
        match channel::on_channel_frame(&f) {
            channel::Action::Send(frames) => {
                assert!(frames.is_empty(), "外部源通道的用户消息不该产生回帧");
            }
            channel::Action::None => {}
            other => panic!("外部源通道的用户消息只该入队，实际 {other:?}"),
        }
    }

    /// 一个把收到的方法名逐行追加到 `methods.log` 的假 agent。
    ///
    /// 断言要落在「agent 到底收到了什么」上：`drain_inbound` 的所有分支在
    /// **本进程**里都只表现为一次 `say()`，从外面看不出它是 prompt 还是丢弃。
    fn method_logging_agent(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("worker-methods.log");
        let path = dir.join("worker-rec-agent");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
LOG = {log:?}

def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    method = m.get("method", "")
    mid = m.get("id")
    with open(LOG, "a") as f:
        f.write(method + "\n")
    if method == "initialize":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"protocolVersion": 1}}}})
    elif method == "session/new":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"sessionId": "ses_worker"}}}})
    elif method == "session/prompt":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"stopReason": "end_turn"}}}})
"#,
            log = log.to_string_lossy()
        );
        std::fs::write(&path, script).expect("写脚本");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        (path, log)
    }

    fn spec_for_worker(ch: &str, bin: &Path) -> crate::agent::ConversationSpec {
        crate::agent::ConversationSpec {
            channel_id: ch.to_string(),
            agent_id: "opencode".to_string(),
            // `cwd` 必须真实存在：`AcpClient::spawn` 会 `current_dir(cwd)`。
            cwd: bin
                .parent()
                .expect("脚本必有父目录")
                .to_string_lossy()
                .into_owned(),
            command: bin.to_string_lossy().into_owned(),
            args: Vec::new(),
            acp_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            acp_session_id: None,
            last_seq: 0,
        }
    }

    /// **一次 `drain_inbound` 里三种形状各走各的**：cancel 走取消、有 `text`
    /// 走 prompt、别的形状**丢掉且不 prompt**。
    ///
    /// 关键在最后一条：把一个不认识的 payload 当成 prompt 送进 agent，等于让
    /// agent 去解读一段我们没打算说出口的话——而页面上看不出区别，只会奇怪
    /// 「它怎么回了这句」。所以这里断言的不是「没崩」，是**没有任何消息被送出**。
    /// **一次 `drain_inbound` 里三种形状各走各的**：cancel 走取消、有 `text`
    /// 走 prompt、别的形状**丢掉且不 prompt**。
    ///
    /// 判据必须落在**agent 收到了什么方法**上。只看 Hub 收到几帧 `channel_error`
    /// 不够：一条认不出的 payload 若被误当成 prompt，会去调一个已经没会话的
    /// 通道，产出与 cancel 完全相同的一句 `no live conversation`——条数与理由
    /// 都一样，那种断言分辨不出对错（这一版的初稿就是这么写的，反向验证当场
    /// 证明它恒真）。所以这里真起一个记录 agent，开一条**活会话**：
    /// 被误送的话，`session/prompt` 会真的出现在它的方法日志里。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn drain_inbound_routes_each_payload_shape_by_its_own_field() {
        let (d, w) = worker("drain-shapes");
        let (mut est, _recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        // 真起一个只记方法名的 agent，并把一条**活会话**挂到 ch-drain 上。
        let (bin, log) = method_logging_agent(&d);
        let _ = channel::open_external("ch-drain");
        w.conversations
            .open(spec_for_worker("ch-drain", &bin), None)
            .await
            .expect("记录 agent 必须能起来");
        // 起进程本身就会打 initialize / session/new，先把基线读走。
        let baseline = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            baseline.contains("initialize"),
            "握手应发生在开会话时：{baseline}"
        );

        // 1. 认不出的形状：既没有 `text` 也没有 `cancel`。
        push_inbound("ch-drain", serde_json::json!({"bytes": "ls -la"}));
        // 2. 空文本：算不出一轮话，必须丢。
        push_inbound("ch-drain", serde_json::json!({"text": "   "}));
        // 3. 一条真的话：必须变成 session/prompt。
        push_inbound("ch-drain", serde_json::json!({"text": "你好"}));

        w.drain_inbound(&mut est).await;

        // prompt 是 spawn 出去的，等它落到 agent 那边。
        let mut prompts = 0;
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let now = std::fs::read_to_string(&log).unwrap_or_default();
            prompts = now.matches("session/prompt").count();
            if prompts >= 1 {
                break;
            }
        }
        let now = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            prompts, 1,
            "只有带 text 的那一条该变成一轮 prompt；认不出的形状与空文本必须静默丢掉。\
             agent 实收方法：{now}"
        );
        // 顺带钉住：认不出的 payload 里的内容**没有**被当成 prompt 发出去。
        assert!(
            !now.contains("session/cancel"),
            "这一轮里没有 cancel 消息，不该出现 session/cancel：{now}"
        );

        w.conversations.close("ch-drain").await;
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 队列被取走之后**不会重复投递**：`take_inbound` 是 drain 语义。
    ///
    /// 这条钉的是「一轮消息只说一次」。若把 `take_inbound` 写成借用/拷贝，
    /// 每次 `drain_inbound` 都会把同一句用户消息再发一遍，而在页面上那表现为
    /// 「agent 自己重复回答」——最难归因的一类 bug。
    ///
    /// **通道必须活着**，所以这里用一条活会话而不是「cancel 一条没开过的
    /// 通道」：后者第一轮就把通道收掉了，第二轮本来就无事可做，那条断言
    /// 无论 drain 还是拷贝都会通过（初稿即如此，反向验证当场证明它恒真）。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn drain_inbound_does_not_replay_the_same_message_twice() {
        let (d, w) = worker("drain-once");
        let (mut est, _recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        let (bin, log) = method_logging_agent(&d);
        let _ = channel::open_external("ch-once");
        w.conversations
            .open(spec_for_worker("ch-once", &bin), None)
            .await
            .expect("记录 agent 必须能起来");

        push_inbound("ch-once", serde_json::json!({"text": "只此一句"}));

        // 连跑三轮：队列若没被取走，就会发出三轮 prompt。
        for _ in 0..3 {
            w.drain_inbound(&mut est).await;
            // 每轮都等一会儿，让 spawn 出去的 prompt 落到 agent 那边，
            // 否则第二轮可能在第一轮取走之前就读到了同一个队列。
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
        // 再给最后一个 prompt 一点时间落地。
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("session/prompt")
            {
                break;
            }
        }

        let now = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            now.matches("session/prompt").count(),
            1,
            "同一条消息只许说一次；三轮 drain 里出现多次就说明队列没被取走。agent 实收：{now}"
        );

        w.conversations.close("ch-once").await;
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- 连接收尾、入站分派与 ACP 开会话的补测 ----

    /// 断连收尾要**说清释放了几条通道、留了几条孤儿会话**。
    ///
    /// `run_established` 在 `session()` 返回之后才做这两件事；前面的用例只用
    /// 空表跑过「released == 0 / orphaned == 0」那两条静默臂。这里挂一条真
    /// 通道 + 一条真会话，让两个计数都大于 0，判据落在日志里的两句话上：
    /// 那是排障时唯一能看出「连接断了以后设备上还留着什么」的地方。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn run_established_reports_released_channels_and_orphaned_conversations() {
        let (d, mut w) = worker("established-cleanup");
        let url = fake_hub(HubTail::Close);
        w.hub_url = url;
        let mut id = Identity::load_or_create(&d).expect("identity");
        let est = conn::connect_once(&w.hub_url, id.key(), None, None, device_info())
            .await
            .expect("handshake");
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let _ = channel::open_external("ch-cleanup");
        let (bin, _log) = method_logging_agent(&d);
        w.conversations
            .open(spec_for_worker("ch-cleanup", &bin), None)
            .await
            .expect("记录 agent 必须能起来");

        // session() 会读到假 Hub 的 Close 并立刻返回 PeerClosed，随后是收尾。
        let end = w
            .run_established(&mut id, est)
            .await
            .expect("收尾不该因日志失败而停下");
        assert!(matches!(end, SessionEnd::PeerClosed), "got {end:?}");

        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(
            log.contains("released 1 channel(s) together with the connection"),
            "要说出释放掉的通道数，实际日志：{log}"
        );
        assert!(
            log.contains("1 acp conversation(s) left orphaned"),
            "要说出留下的孤儿会话数，实际日志：{log}"
        );

        w.conversations.close("ch-cleanup").await;
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 一条 `cancel` 入站消息走的是 `cancel_turn`，**不是 `prompt`**。
    ///
    /// 反例是把 `cancel` 当成普通文本送进 agent：那样 `session/prompt` 会出现
    /// 在 agent 的方法日志里，页面上的「停止」反而又开了一轮话。判据落在
    /// 真 agent 实收的方法名上，不看本进程的日志（两种写法都只留一句话）。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn an_inbound_cancel_goes_to_cancel_turn_not_to_prompt() {
        let (d, w) = worker("inbound-cancel");
        let (mut est, _recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        let (bin, log) = method_logging_agent(&d);
        let _ = channel::open_external("ch-cancel");
        w.conversations
            .open(spec_for_worker("ch-cancel", &bin), None)
            .await
            .expect("记录 agent 必须能起来");
        let baseline = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(baseline.contains("initialize"), "握手应先发生：{baseline}");

        push_inbound("ch-cancel", serde_json::json!({"cancel": true}));
        w.drain_inbound(&mut est).await;
        // 闲着的会话上 cancel 走 NothingInFlight：agent 那边不该多出任何方法。
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let now = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            now, baseline,
            "cancel 只能走取消路径，不该给 agent 追加任何方法：{now}"
        );

        w.conversations.close("ch-cancel").await;
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ACP 会话已经没了时，一句入站 `text` 的 `prompt` 会失败：
    /// 必须关掉通道并把 `channel_error` **发到 Hub**，页面才不会一直等回复。
    ///
    /// 判据在网络对端：只断掉会话表、保留通道表，正是生产里「子进程退了、
    /// 页面还开着」的形状；`fail_channel` 不发帧的话这里收不到任何东西。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn a_failed_prompt_closes_the_channel_and_tells_the_hub() {
        let (d, w) = worker("prompt-failed");
        let (mut est, recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        let (bin, _log) = method_logging_agent(&d);
        let _ = channel::open_external("ch-gone");
        w.conversations
            .open(spec_for_worker("ch-gone", &bin), None)
            .await
            .expect("记录 agent 必须能起来");
        w.conversations.close("ch-gone").await; // 会话没了，通道还在

        push_inbound("ch-gone", serde_json::json!({"text": "还有人吗"}));
        w.drain_inbound(&mut est).await;

        let mut got = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            got = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if got.iter().any(|f| f.msg_type == "channel_error") {
                break;
            }
        }
        let err = got
            .iter()
            .find(|f| f.msg_type == "channel_error")
            .unwrap_or_else(|| panic!("prompt 失败必须让 Hub 收到 channel_error，实收：{got:?}"));
        assert_eq!(
            err.c.get("channel_id").and_then(|v| v.as_str()),
            Some("ch-gone"),
            "错误要指着那条再也不会回话的通道"
        );
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("prompt failed on ch-gone"), "{log}");

        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `cancel_turn` 三种结果各说各的话，不互相混成一句泛泛的「失败」。
    ///
    /// - 没有活会话：要说 `cancel failed`，并回一帧 `channel_error`；
    /// - 会话在但闲着：要说 `cancel ignored`，通道原样留着（不能回错误帧）；
    /// - 一轮真的在飞：`cancel()` 返回 `Ok(())`，什么都不说。
    ///
    /// 第三种用一个「只答握手、不答 `session/prompt`」的 holding agent 构造：
    /// `prompt` 发出去后 `inflight` 一直非零，于是 `cancel` 走 `Ok(())` 那条臂。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn cancel_turn_names_each_kind_of_failure_without_conflating_them() {
        let (d, w) = worker("cancel-kinds");
        let (mut est, recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        // 1. 没会话：回帧 + 说 failed。
        w.cancel_turn(&mut est, "ch-never").await;
        // 2. 有会话但闲着：只说 ignored，不关通道、不回 channel_error。
        let (bin, _log) = method_logging_agent(&d);
        let _ = channel::open_external("ch-idle");
        w.conversations
            .open(spec_for_worker("ch-idle", &bin), None)
            .await
            .expect("记录 agent 必须能起来");
        w.cancel_turn(&mut est, "ch-idle").await;
        // 3. 一轮在飞：holding agent 不回 session/prompt，inflight 保持非零。
        let (holding, _hlog) = holding_agent(&d);
        let _ = channel::open_external("ch-busy");
        let mut spec = spec_for_worker("ch-busy", &holding);
        spec.command = holding.to_string_lossy().into_owned();
        w.conversations
            .open(spec, None)
            .await
            .expect("holding agent");
        push_inbound("ch-busy", serde_json::json!({"text": "别回答"}));
        w.drain_inbound(&mut est).await;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if w.conversations.turn_in_flight("ch-busy").await == Some(true) {
                break;
            }
        }
        assert_eq!(
            w.conversations.turn_in_flight("ch-busy").await,
            Some(true),
            "holding agent 必须让这一轮停在飞状态"
        );
        w.cancel_turn(&mut est, "ch-busy").await;

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(log.contains("cancel failed on ch-never"), "{log}");
        assert!(log.contains("cancel ignored on ch-idle"), "{log}");
        assert!(
            !log.contains("cancel ignored on ch-busy") && !log.contains("cancel failed on ch-busy"),
            "一轮在飞的成功取消不该被说成失败或忽略：{log}"
        );
        let got = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let errs: Vec<_> = got
            .iter()
            .filter(|f| f.msg_type == "channel_error")
            .collect();
        assert_eq!(errs.len(), 1, "只有没会话那次要回错误帧：{got:?}");
        assert_eq!(
            errs[0].c.get("channel_id").and_then(|v| v.as_str()),
            Some("ch-never")
        );

        w.conversations.close("ch-idle").await;
        w.conversations.close("ch-busy").await;
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 只答 `initialize` / `session/new`，**永不答 `session/prompt`**：
    /// 让一轮话停在「在飞」状态，上面那条用例据此走 `cancel()==Ok(())`。
    fn holding_agent(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("holding-methods.log");
        let path = dir.join("worker-holding-agent");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
LOG = {log:?}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    method = m.get("method", "")
    mid = m.get("id")
    with open(LOG, "a") as f:
        f.write(method + "\n")
    if method == "initialize":
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}}) + "\n")
        sys.stdout.flush()
    elif method == "session/new":
        sys.stdout.write(json.dumps({{"jsonrpc":"2.0","id":mid,"result":{{"sessionId":"ses_hold"}}}}) + "\n")
        sys.stdout.flush()
    # session/prompt / session/cancel 都故意不回
"#,
            log = log.to_string_lossy()
        );
        std::fs::write(&path, script).expect("写脚本");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        (path, log)
    }

    /// 通道帧发不出去时只留痕、不断连：判据是日志点名了帧类型，
    /// 且 `on_channel_message` 仍返回 `None`（连接继续用）。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn a_channel_error_that_cannot_be_sent_only_leaves_a_trace() {
        let (d, w) = worker("channel-send-fail");
        let mut est = established_after_peer_close(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        // 一条没有工作区根的 files 通道 → `Action::Send([channel_error])`。
        let f = frame_of(
            "open_channel",
            serde_json::json!({
                "channel_id": "ch-send-fail",
                "connector": "files",
                "params": {},
                "credit_start": 4,
            }),
        );
        let ended = w.on_channel_message(&mut est, &f).await;
        assert!(ended.is_none(), "发不出去不该结束会话：{ended:?}");
        let log = std::fs::read_to_string(crate::log::log_path(&d)).expect("log");
        assert!(
            log.contains("channel frame send failed (channel_error"),
            "要说出是哪一帧没发走，实际：{log}"
        );
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `open_channel{acp}` 走 `Action::OpenAcp`：`channel_ready` **先到 Hub**，
    /// 然后才起子进程。顺序错了会让慢 agent 被 Hub 判成「不支持对话」。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn on_channel_message_opens_an_acp_conversation_and_sends_ready_first() {
        let (d, w) = worker("acp-open-ok");
        let (mut est, recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        let (bin, _log) = method_logging_agent(&d);
        let spec = spec_for_worker("ch-acp-ok", &bin);
        let f = frame_of(
            "open_channel",
            serde_json::json!({
                "channel_id": "ch-acp-ok",
                "connector": "acp",
                "params": {
                    "agent_id": spec.agent_id,
                    "cwd": spec.cwd,
                    "command": spec.command,
                    "args": spec.args,
                },
                "credit_start": 4,
            }),
        );
        let ended = w.on_channel_message(&mut est, &f).await;
        assert!(ended.is_none(), "开会话不该结束连接：{ended:?}");

        let mut got = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            got = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if !got.is_empty() {
                break;
            }
        }
        assert_eq!(got.len(), 1, "成功开会话只回 channel_ready：{got:?}");
        assert_eq!(got[0].msg_type, "channel_ready");
        assert_eq!(
            got[0].c.get("channel_id").and_then(|v| v.as_str()),
            Some("ch-acp-ok")
        );
        assert_eq!(
            w.conversations.turn_in_flight("ch-acp-ok").await,
            Some(false),
            "会话要真的挂进会话表，而不只是回了一帧 ready"
        );

        w.conversations.close("ch-acp-ok").await;
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ACP 子进程起不来时，`channel_ready` 之后必须再回一帧 `channel_error`
    /// 并收掉通道：不回的话页面永远显示「正在回复」。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    #[tokio::test]
    async fn on_channel_message_reports_a_failed_acp_open_to_the_hub() {
        let (d, w) = worker("acp-open-fail");
        let (mut est, recorded) = established_recording_frames(&d).await;
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

        let missing = d.join("no-such-agent-binary");
        let f = frame_of(
            "open_channel",
            serde_json::json!({
                "channel_id": "ch-acp-fail",
                "connector": "acp",
                "params": {
                    "agent_id": "opencode",
                    "cwd": d.to_string_lossy(),
                    "command": missing.to_string_lossy(),
                },
                "credit_start": 4,
            }),
        );
        let ended = w.on_channel_message(&mut est, &f).await;
        assert!(ended.is_none(), "起不来进程不该结束整条连接：{ended:?}");

        let mut got = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            got = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if got.len() >= 2 {
                break;
            }
        }
        assert_eq!(got.len(), 2, "ready 之后还要一帧错误：{got:?}");
        assert_eq!(got[0].msg_type, "channel_ready", "ready 必须先发");
        assert_eq!(got[1].msg_type, "channel_error");
        assert_eq!(
            got[1].c.get("channel_id").and_then(|v| v.as_str()),
            Some("ch-acp-fail")
        );
        assert!(
            !channel::external_ids().contains(&"ch-acp-fail".to_string()),
            "开不起来的通道必须被收掉，否则页面会一直等"
        );
        drop(_guard);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `send_heartbeat` 的成功臂要**真的把心跳帧送到 Hub**。
    ///
    /// 只断言 `Ok(())` 是恒真的：tungstenite 会在对端已关时把帧写进自己的
    /// 缓冲里照样返回 Ok。所以走 `fake_hub_recording_heartbeats_no_channel`，
    /// 判据是对端 arrivals 里多了一次心跳。
    #[tokio::test]
    async fn send_heartbeat_on_a_live_connection_sends_a_heartbeat_frame() {
        let (d, w) = worker("heartbeat-arrives");
        let (url, arrivals) = fake_hub_recording_heartbeats_no_channel(60_000);
        let id = Identity::load_or_create(&d).expect("identity");
        let mut est = conn::connect_once(&url, id.key(), None, None, device_info())
            .await
            .expect("handshake");
        let before = arrivals.lock().unwrap_or_else(|e| e.into_inner()).len();
        w.send_heartbeat(&mut est).await.expect("心跳应当发出去");
        let mut after = before;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            after = arrivals.lock().unwrap_or_else(|e| e.into_inner()).len();
            if after > before {
                break;
            }
        }
        assert_eq!(
            after,
            before + 1,
            "成功的心跳必须在对端留下一次到达，而不是只在本进程返回 Ok"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
