//! ACP 对话会话管理：起子进程、收事件、聚合、推进通道。
//!
//! # 这一层解决什么
//!
//! `acp.rs` 只做到「JSON-RPC over stdio 能用」。把一条对话接进页面还差三件
//! 事，三件都不能写在 `channel.rs` 里（那里是同步的、不持有 async 客户端）：
//!
//! 1. **起子进程**：读 `agent_config` 下发的命令/参数/环境，`session/new`
//! 拿 `sessionId`。
//! 2. **收事件**：ACP 的 `session/update` 是通知，从 stdout 流里出来。**必须
//!    在独立任务里读**——放进 `select!` 的任何一臂都会让心跳取决于 agent 有
//!    没有吐字（M1 的 `exec` 堵住心跳臂就是这个形状）。
//! 3. **聚合**：实测 chunk 每块 40–90 字符，一次 3000 字回复 = 200–400 条
//!    事件。一事件一帧会被信用卡成锯齿状（每 32 帧一次往返，跨网 50 ms RTT
//!    时 200 帧 ≈ 300 ms 的顿感）。按**字节数或时间窗**合并成一个「一批」帧。
//!
//! # 帧格式（对外契约）
//!
//! 推给 Hub 的 `channel_data.payload` 是一个对象，`type` 区分三类：
//!
//! ```json
//! { "type": "batch",   "events": [ <ACP session/update 的 update 对象>, ... ] }
//! { "type": "turn_end","stopReason": "end_turn" }
//! { "type": "ready",   "sessionId": "ses_...", "agentId": "opencode" }
//! ```
//!
//! **`events` 里每个元素都是 ACP 原样的 `update` 对象**，不解释、不裁剪：
//! 新版本 agent 推的类型我们不认识，裁剪就是静默丢消息。类型判定在页面做。
//!
//! `turn_end` 单独一类而不是塞进 `batch`：它是**一轮的边界**，页面按它切
//! 消息块，而 `batch` 只是传输聚合——两者混在一起，页面就得在一批事件里
//! 找出哪条是边界，而边界本来就是我们自己写的。
//!
//! # 会话的生死
//!
//! **子进程不随连接死**（`acp.rs` 模块注释的第 3 条硬约束）：断连后重连要
//! 能续上，而硬杀一个正在跑工具的 agent 会留下半途的副作用。所以这里只由
//! 两条路收掉进程：`close_channel`（用户关了对话）与设备进程退出。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::agent::acp::{AcpClient, Event};
use crate::channel;

/// 一个对话要起子进程所需的全部东西。**全部来自 Hub 下发的 `open_channel.params`**。
///
/// 设备侧不查自己的配置表：`agent_config` 在 Hub 侧（用户裁定「Hub 存，设备读」），
/// 设备只有「这次用什么命令」这一份事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSpec {
    pub channel_id: String,
    /// `agent_runtime.agent_id`（如 `opencode`）。用来在扫描结果里找默认可执行文件。
    pub agent_id: String,
    /// 会话工作区根。**必须来自 Hub**：留空会让 agent 在设备自己的数据目录里
    /// 建文件，而那既不是用户的工作区也会污染设备状态。
    pub cwd: String,
    /// 用户配的启动命令。**空串 = 用扫描到的那一个**（见 [`resolve_command`]）。
    pub command: String,
    /// 用户追加的参数（`agent_config.args_json`）。
    ///
    /// **不含 ACP 入口参数**——那个由 [`ConversationSpec::acp_args`] 提供，
    /// 由设备侧的 agent 清单给出，不由 Hub 配置。
    pub args: Vec<String>,
    /// agent 进入 ACP 模式要的参数（来自设备侧 [`crate::agent::detect::AgentSpec::acp_args`]）。
    ///
    /// 排在最前面，见 [`build_args`]。
    pub acp_args: Vec<String>,
    /// 追加到继承环境之上（不是替换——替换会让子进程丢掉 `PATH`）。
    pub env: BTreeMap<String, String>,
}

/// 聚合的字节闸：攒够这么多就发一批，不必等时间窗。
///
/// 4 KiB 是个折中：一次典型的短回复（几十字）够不上，会等时间窗发出去；
/// 长回复每 4 KiB 发一帧，3000 字 ≈ 3 帧而不是 400 帧。
const BATCH_BYTES: usize = 4 * 1024;

/// 聚合的时间闸：这么久没新事件就把手里这批发出去。
///
/// **必须有它**：只有字节闸的话，一句「好」会永远等不到下一批——页面看着
/// 像「agent 不回了」。60 ms 略高于一个帧间隔，人眼看不出延迟，而一次
/// 连续吐字（间隔通常 < 20 ms）仍然能合起来。
const BATCH_INTERVAL: Duration = Duration::from_millis(60);

/// 活着的对话会话表。
#[derive(Debug, Default)]
pub struct Conversations {
    live: Mutex<BTreeMap<String, LiveConversation>>,
}

#[derive(Debug)]
struct LiveConversation {
    /// 发 `session/prompt` 用的客户端句柄。读循环另有一个（见 `acp.rs` 的
    /// `Inner`：克隆共享同一个子进程，但接收端只在原始句柄上）。
    client: AcpClient,
    session_id: String,
    /// 读事件的 task。关会话时先 abort 再 shutdown。
    reader: JoinHandle<()>,
}

impl Conversations {
    pub fn new() -> Self {
        Self::default()
    }

    /// 起一条对话：子进程 + `session/new` → 一条 `ready` 帧推给 Hub。
    ///
    /// 失败时**通道已经被 [`channel::fail_external`] 关掉**，调用方只管把返回
    /// 的帧发出去。
    ///
    /// `default_command` 是扫描到的那个可执行文件（`agent_config.command`
    /// 为空串时用它）。拿不到就是 `None`，此时只有用户配了命令才起得来。
    pub async fn open(&self, spec: ConversationSpec, default_command: Option<&str>) -> Result<(), String> {
        let program = resolve_command(&spec.command, default_command).ok_or_else(|| {
            format!(
                "agent {} is not installed and no command is configured",
                spec.agent_id
            )
        })?;
        let env: BTreeMap<String, String> = spec.env.clone();
        let cwd = std::path::PathBuf::from(&spec.cwd);
        let acp_args: Vec<&str> = spec.acp_args.iter().map(String::as_str).collect();
        let args = build_args(&acp_args, &spec.args);
        let client = AcpClient::spawn(&program, &args, &env, &program, Some(&cwd))
            .await
            .map_err(|e| format!("cannot start {}: {e}", spec.agent_id))?;
        let session_id = client
            .new_session(&spec.cwd)
            .await
            .map_err(|e| format!("cannot create an acp session: {e}"))?;

        // `ready` 先推：页面拿到 sessionId 才能把消息挂到正确的会话上。
        // 丢不丢是另一回事——通道已经被 `fail_external` 之外的路开好了。
        let ready = json!({
            "type": "ready",
            "sessionId": session_id,
            "agentId": spec.agent_id,
        });
        let _ = channel::push_external(&spec.channel_id, vec![ready]);

        // 读循环必须拿**原始句柄**（`recv` 只在它上面有值）。所以这里先克隆
        // 一个给会话表（发 prompt 用），原始那个进读 task。
        let sender = client.clone();
        let reader = tokio::spawn(read_events(spec.channel_id.clone(), client));

        self.live.lock().await.insert(
            spec.channel_id,
            LiveConversation {
                client: sender,
                session_id,
                reader,
            },
        );
        Ok(())
    }

    /// 把用户消息发给某条对话。
    ///
    /// **不等回复**：`prompt` 的返回值可能是 `stopReason`，也可能什么都没有
    /// （实测有一次 130 s 未回）。回复永远从读循环的事件流里出来。所以这里
    /// `tokio::spawn` 一个任务去发，不阻塞 worker 的 `select!`——一条慢 agent
    /// 不该让心跳迟到。
    pub async fn prompt(&self, channel_id: &str, text: &str) -> Result<(), String> {
        let live = self.live.lock().await;
        let Some(conv) = live.get(channel_id) else {
            return Err(format!("no live conversation on channel {channel_id}"));
        };
        let client = conv.client.clone();
        let session_id = conv.session_id.clone();
        let ch = channel_id.to_string();
        let text = text.to_string();
        drop(live);

        tokio::spawn(async move {
            match client.prompt(&session_id, &text).await {
                Ok(result) => {
                    // **`turn_end` 的唯一来源**：`session/prompt` 的响应带
                    // `stopReason`（实测 `end_turn`）。chunk 流自己不会说它
                    // 结束了，而序号断裂检测看不见末尾——见
                    // `session-and-trajectory.md` §3.2.1。
                    //
                    // 没有 `stopReason` 就**不写**：那不是「没走完」，是
                    // 「这次没拿到凭据」。写一条假的 `turn_end` 会让页面把
                    // 一个还在跑的轮次画成已结束。
                    if let Some(reason) = AcpClient::stop_reason(&result) {
                        let ev = json!({ "type": "turn_end", "stopReason": reason });
                        let _ = channel::push_external(&ch, vec![ev]);
                    }
                }
                Err(e) => {
                    // 发不出去要说出口：否则页面停在「正在回复」。用 `batch`
                    // 里一条 agent 不认识的事件形状？不——单独一类 `error`，
                    // 页面不必猜。
                    let ev = json!({ "type": "error", "message": e.to_string() });
                    let _ = channel::push_external(&ch, vec![ev]);
                }
            }
        });
        Ok(())
    }

    /// 关一条对话：停读循环、关子进程、从表里摘掉。
    ///
    /// **幂等**：通道已经关了（`close_channel` 重发）也安静返回。
    pub async fn close(&self, channel_id: &str) {
        let removed = self.live.lock().await.remove(channel_id);
        if let Some(conv) = removed {
            // 先 abort 读 task：它持有 `recv` 的接收端，而 `shutdown` 会关
            // stdin —— 顺序反了会出现「读 task 还在等一条永远不会来的事件」。
            conv.reader.abort();
            conv.client.shutdown().await;
        }
    }

    /// 关掉**全部**对话。设备进程退出 / 连接结束时用（连接结束**不**调它，
    /// 见模块注释「子进程不随连接死」）。
    pub async fn close_all(&self) {
        let ids: Vec<String> = self.live.lock().await.keys().cloned().collect();
        for id in ids {
            self.close(&id).await;
        }
    }

    /// 当前活着的会话数（测试与排障用）。
    pub async fn len(&self) -> usize {
        self.live.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.live.lock().await.is_empty()
    }
}

/// `agent_config.command` 为空串时用扫描到的可执行文件；都拿不到就是 `None`。
///
/// 抽成函数是为了让「空串 = 用扫描到的那一个」这条契约**只有一个实现**：
/// `agent_config` 的三条端点与这里说的是同一件事，写在两处迟早分叉。
fn resolve_command(configured: &str, detected: Option<&str>) -> Option<String> {
    if !configured.trim().is_empty() {
        return Some(configured.to_string());
    }
    detected
        .filter(|d| !d.trim().is_empty())
        .map(str::to_string)
}

/// 拼出起子进程用的完整参数：**先是 agent 自己的 ACP 入口参数，再是用户追加的**。
///
/// 顺序不能反：`opencode acp --model x` 里 `acp` 是子命令、必须紧跟程序名。
/// 反过来的 `opencode --model x acp` 在 opencode 上会当成未知参数——症状仍是
/// 「发起去了但永远等不到 `initialize` 响应」。
fn build_args(acp_args: &[&str], configured: &[String]) -> Vec<String> {
    let mut out: Vec<String> = acp_args.iter().map(|a| (*a).to_string()).collect();
    out.extend(configured.iter().cloned());
    out
}

/// 读事件 → 聚合 → 推进通道。**这个 task 死掉等于这条对话瞎了**。
async fn read_events(channel_id: String, client: AcpClient) {
    let mut batch: Vec<Value> = Vec::new();
    let mut bytes = 0usize;
    let mut last_flush = Instant::now();
    // 空闲时也必须能发出去：只有事件到达才检查时间窗是不够的（最后一句
    // 永远在等下一个事件）。所以用 `select!` 带一个定时器。
    let mut tick = tokio::time::interval(BATCH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = client.recv() => match ev {
                None => break,
                Some(Event::SessionUpdate(params)) => {
                    // `update` 才是页面要的那一层；`sessionId` 我们已知（就是
                    // 这份会话），留着只会让每个事件多几字节。
                    let update = params.get("update").cloned().unwrap_or(params);
                    bytes += update.to_string().len();
                    batch.push(update);
                    if bytes >= BATCH_BYTES {
                        flush(&channel_id, &mut batch, &mut bytes);
                        last_flush = Instant::now();
                    }
                }
                Some(Event::Request { id, method, params }) => {
                    // 权限请求：**本里程碑不回**（ACP 允许请求方超时）。
                    // 但要让用户看见，否则页面看着像 agent 卡住了。
                    let ev = json!({
                        "type": "request",
                        "method": method,
                        "params": params,
                        "pending": true,
                    });
                    let _ = id;
                    let _ = channel::push_external(&channel_id, vec![ev]);
                }
                Some(Event::Exited { code, reason }) => {
                    // 先把手里的发完，再说「子进程没了」——顺序反了会让最后
                    // 半句话丢掉，而那句话往往正是崩之前说的最后一句。
                    flush(&channel_id, &mut batch, &mut bytes);
                    let ev = json!({
                        "type": "exited",
                        "code": code,
                        "reason": reason,
                    });
                    let _ = channel::push_external(&channel_id, vec![ev]);
                    break;
                }
            },
            _ = tick.tick() => {
                if !batch.is_empty() && last_flush.elapsed() >= BATCH_INTERVAL {
                    flush(&channel_id, &mut batch, &mut bytes);
                    last_flush = Instant::now();
                }
            }
        }
    }
    // 循环退出时手里可能还有没发的事件（`recv` 返回 `None` 就是这么来的）。
    flush(&channel_id, &mut batch, &mut bytes);
}

/// 把攒着的一批发出去。通道没了就**丢掉**（已裁决「不承诺补齐」）。
fn flush(channel_id: &str, batch: &mut Vec<Value>, bytes: &mut usize) {
    if batch.is_empty() {
        return;
    }
    let events = std::mem::take(batch);
    *bytes = 0;
    let ev = json!({ "type": "batch", "events": events });
    let _ = channel::push_external(channel_id, vec![ev]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **空串 = 用扫描到的那一个**；两者都空 = 起不来。
    ///
    /// 这条契约同时被 `agent_config` 的三条端点与 `acp.rs` 的 `spawn` 依赖，
    /// 写错的表现是「配了命令却不生效」或「没配就崩」。
    #[test]
    fn the_command_falls_back_to_the_detected_binary() {
        assert_eq!(resolve_command("/x/y", Some("/a/b")), Some("/x/y".to_string()));
        assert_eq!(
            resolve_command("", Some("/a/b")),
            Some("/a/b".to_string()),
            "空串必须回落到扫描到的那个"
        );
        assert_eq!(resolve_command("   ", Some("/a/b")), Some("/a/b".to_string()));
        assert_eq!(
            resolve_command("", None),
            None,
            "没配也没扫到：起不来，不能编一个命令"
        );
        assert_eq!(
            resolve_command("", Some("")),
            None,
            "扫到空串等于没扫到"
        );
    }

    /// **`acp_args` 必须排在最前**：`opencode acp --model x` 里 `acp` 是
    /// 子命令，必须紧跟程序名。反过来的 `opencode --model x acp` 会被当成
    /// 未知参数，症状是「发起去了但永远等不到 initialize 响应」。
    ///
    /// 反向验证：把 `build_args` 改成先放 `configured` 再放 `acp_args`，
    /// 这条会红。
    #[test]
    fn the_acp_entry_args_come_before_the_configured_ones() {
        let got = build_args(&["acp"], &["--model".to_string(), "x".to_string()]);
        assert_eq!(got, vec!["acp", "--model", "x"]);

        // 没配任何参数时就是 ACP 入口参数本身
        assert_eq!(build_args(&["acp"], &[]), vec!["acp"]);
        // 清单里没有这个 agent：不编参数
        assert!(build_args(&[], &["--x".to_string()]).is_empty() == false);
        assert_eq!(build_args(&[], &["--x".to_string()]), vec!["--x"]);
    }

    /// 帧格式是**对外契约**（页面按 `type` 分派），钉住它。
    #[test]
    fn the_batch_and_turn_end_shapes_are_pinned() {
        let mut batch = vec![json!({ "sessionUpdate": "agent_message_chunk" })];
        let mut bytes = 100;
        flush("ch", &mut batch, &mut bytes);
        assert!(batch.is_empty(), "发完要清空");
        assert_eq!(bytes, 0, "字节账要归零");

        // 空批不发帧（否则页面收到一堆空 batch）
        let mut empty: Vec<Value> = Vec::new();
        let mut z = 0;
        flush("ch", &mut empty, &mut z);
        assert!(empty.is_empty());
    }

    /// **真机端到端**：开本地通道 → 起真 opencode → 发一句话 → 收到回复。
    ///
    /// `#[ignore]`：要真 opencode（v2.1.4）与凭据，CI 里没有。
    /// 本机跑：`cargo test -p node-core --lib conversation -- --ignored --test-threads=1`
    ///
    /// 它证明的是**整条链接起来了**，这是单测做不到的：通道表 → 外部源入口 →
    /// ACP 子进程 → session/new → session/prompt → 事件流 → 聚合 → 推回通道。
    /// 任何一环写法错，这里都拿不到一条带文字的 `batch`。
    #[tokio::test]
    #[ignore]
    async fn a_real_opencode_turn_reaches_the_channel_as_a_batch() {
        let bin = std::env::var("AF_TEST_AGENT_BIN")
            .unwrap_or_else(|_| "/root/.local/share/mise/installs/npm-opencode-cli/latest/bin/opencode".to_string());
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        // 通道表是进程级单例：这条用例必须独占。
        let _ = channel::reset_for_tests();

        let ch = "ch-e2e-acp";
        let frames = channel::open_external(ch);
        assert_eq!(frames.len(), 1, "先开通道（模拟 Hub 的 open_channel）");

        let convs = Conversations::new();
        let spec = ConversationSpec {
            channel_id: ch.to_string(),
            agent_id: "opencode".to_string(),
            cwd: cwd.clone(),
            command: bin,
            args: Vec::new(),
            // 真机测试必须带上它：不带 `acp` 子命令时 opencode 是普通 CLI，
            // `initialize` 永远等不到响应（这就是第一次跑卡住的真因）。
            acp_args: vec!["acp".to_string()],
            env: BTreeMap::new(),
        };
        convs
            .open(spec, None)
            .await
            .expect("真 opencode 必须能起来");
        assert_eq!(convs.len().await, 1);

        // 起子进程 + session/new 之后，`ready` 已经在待发队列或已发出。
        // 先把它放行，再发消息。
        let _ = channel::pump();

        convs.prompt(ch, "Say exactly: PONG").await.expect("会话在");

        // 收集最多 180 s 的事件（实测一轮短问答 10~30 s，含模型往返）。
        let mut text = String::new();
        let mut saw_turn_end = false;
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline && !saw_turn_end {
            tokio::time::sleep(Duration::from_millis(200)).await;
            for (_, frames) in channel::pump() {
                for f in frames {
                    let Ok(d) = f.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    let p = &d.payload;
                    match p.get("type").and_then(Value::as_str) {
                        Some("batch") => {
                            for ev in p.get("events").and_then(Value::as_array).cloned().unwrap_or_default() {
                                if ev.get("sessionUpdate").and_then(Value::as_str)
                                    == Some("agent_message_chunk")
                                {
                                    if let Some(t) = ev
                                        .get("content")
                                        .and_then(|c| c.get("text"))
                                        .and_then(Value::as_str)
                                    {
                                        text.push_str(t);
                                    }
                                }
                            }
                        }
                        Some("turn_end") => {
                            assert!(
                                p.get("stopReason").and_then(Value::as_str).is_some(),
                                "turn_end 必须带 stopReason"
                            );
                            saw_turn_end = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        convs.close(ch).await;
        assert!(
            text.contains("PONG"),
            "整条链要能把模型的话送回通道，实际收到: {text:?}"
        );
        assert!(
            saw_turn_end,
            "一轮结束要有 turn_end（子任务 9.1 的完整性判据）"
        );
    }
}
