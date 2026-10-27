//! ACP 对话会话管理：起子进程、收事件、聚合、推进通道。
//!
//! # 这一层解决什么
//!
//! `acp.rs` 只做到「JSON-RPC over stdio 能用」。把一条对话接进页面还差三件
//! 事，三件都不能写在 `channel.rs` 里（那里是同步的、不持有 async 客户端）：
//!
//! 1. **起子进程**：读 `agent_config` 下发的命令/参数/环境，`session/new`
//!    拿 `sessionId`。
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
//! { "type": "turn_end","stopReason": "end_turn", "durationMs": <毫秒数 | null> }
//! { "type": "ready",   "sessionId": "ses_...", "agentId": "opencode" }
//! ```
//!
//! `durationMs` 是**设备掐的表**（`session/prompt` 发出到响应回来）。它是轨迹
//! 里「耗时」那一列唯一的真值来源——ACP 事件自身不带时间戳，而 Hub 拿相邻帧的
//! 到达时刻做差分，量到的是**帧间隔**而不是**这一轮跑了多久**。
//!
//! **`events` 里每个元素都是 ACP 原样的 `update` 对象**，不解释、不裁剪：
//! 新版本 agent 推的类型我们不认识，裁剪就是静默丢消息。类型判定在页面做。
//!
//! # 序号（`seq`）
//!
//! ACP 事件**没有序号也没有时间戳**，而序号是 Hub 判断「有没有丢」的唯一依据。
//! 所以它由设备侧生成，且**整条对话共用一个计数器**——读循环与 `prompt` 的
//! `turn_end` 是两个并发 task，各持一个计数器就会撞号，而撞号在 Hub 侧被
//! `UNIQUE` 静默忽略，正好丢掉一条且看不出来。见 [`Seq`]。
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
//!
//! # 但「不随连接死」不等于「没有上界」
//!
//! `channel::on_disconnect()` 清空通道表之后，Hub 再也不会为那些通道发
//! `CLOSE_CHANNEL`（它是断的），而 `close` 的唯一生产调用点就在那个分支上。
//! 重连 N 次就攒 N 份「子进程还在跑、却再也叫不动」的会话。所以断连只做两件
//! 事，**一件都不杀进程**：把每条会话标成 `orphaned`（`worker::run_established`
//! 在 `on_disconnect()` 之后调 [`Conversations::mark_orphaned`]），以及给表一个
//! 上界 [`LIVE_CAP`]——插到超过就**淘汰最旧的孤儿**并真正关掉它，挂在本连接上
//! 的一条都不动（踢掉用户正在聊的对话比多留一个子进程更坏）。
//!
//! 标成孤儿**不等于**弃用：它照样能 `prompt`，重连后 Hub 拿同一个 `channel_id`
//! 发 `open_channel` 时也能 `session/load` 回到同一个 ACP 会话（那是另一次
//! `open()`，不是这张表的命中）。丢掉的是**可达性**，不是**上下文**。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
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
    /// 上一次开对话时 agent 给的 ACP 会话 id（`hub_storage::Session::acp_session_id`）。
    ///
    /// # `Some` 与 `None` 是两种**不同的动作**，不是「带不带参数」
    ///
    /// `Some(id)` → 走 `session/load`，让 agent 恢复这个会话的上下文；
    /// `None` → 走 `session/new`，开一条全新的。
    ///
    /// 实测（M2 里程碑文档 V-2）：**新进程拿同一 `sessionId` 直接 `session/prompt`
    /// 不成立**，agent 不认这个 id，必须先 `session/load` 把上下文喂进去；而 load
    /// 之后它确实记得上一轮说的话。所以这个字段决定「关掉页面再回来还能不能接着聊」。
    ///
    /// # 重放事件**不进轨迹表**
    ///
    /// `session/load` 会把历史以事件流的形式重放一遍。那些事件**不写
    /// `trajectory_event`**：重放是**重建**的（事件数与 live 不同、没有 `seq`、
    /// 没有时间戳、中间态被折叠），写进去等于把同一段对话记两遍且顺序错乱。
    /// 轨迹表是唯一真相源，页面历史只读它。
    pub acp_session_id: Option<String>,
    /// 这条会话**已经落库的最大 `seq`**；本次开对话的编号从这里续上。
    ///
    /// # 为什么设备侧不能自己从 1 数
    ///
    /// `seq` 的契约是「会话内只增」（`hub-storage` 的 `TrajectoryRepo`：断裂处
    /// 即丢失处）。但计数器活在 ACP 子进程里，**每个子进程都从 1 重数**——
    /// 关掉页面再打开、worker 换代、`session/load` 建新进程，全都会把它清零。
    /// 于是新事件带 `seq=1`，撞上库里已有的 `seq=1`，被 `INSERT OR IGNORE`
    /// 静默吞掉：轨迹少一半内容，而 `gaps` 不报警（seq 仍然连续）。
    ///
    /// 实测（M2 子任务 13）：第二次开对话走 `session/load`，agent 重放的
    /// `user_message_chunk` 带 `seq=1`，而库里 `seq=1` 已是
    /// `available_commands_update`——那条用户消息当场消失。
    ///
    /// 所以起点必须来自**跨进程的真相源**（`trajectory_event` 表的
    /// `MAX(seq)`），由 Hub 在 `open_channel.params.last_seq` 里下发。
    /// `0` 表示「这条会话还一条都没有」（`Seq::next()` 让第一条是 1）。
    pub last_seq: i64,
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
    /// 「它是第几条命」的单调来源，只用于淘汰时排序（见 `LiveConversation::epoch`）。
    ///
    /// 不用 `HashMap` 的迭代顺序、也不用 `BTreeMap` 的键序：两者都**不是**
    /// 「谁先来」。键序是用户/协议给的 id 的字典序，淘汰它等于按字母杀掉会话。
    epochs: AtomicI64,
    /// 上界。见 `LIVE_CAP`。
    cap: usize,
}

#[derive(Debug)]
struct LiveConversation {
    /// 发 `session/prompt` 用的客户端句柄。读循环另有一个（见 `acp.rs` 的
    /// `Inner`：克隆共享同一个子进程，但接收端只在原始句柄上）。
    client: AcpClient,
    session_id: String,
    /// 读事件的 task。关会话时先 abort 再 shutdown。
    reader: JoinHandle<()>,
    /// 与读循环共享的序号发生器。`prompt` 的那条 `turn_end` 也从这里取号，
    /// 否则两条并发路径会撞号（见 [`Seq`] 的注释）。
    seq: Seq,
    /// 插入顺序。淘汰挑**最旧的孤儿**，所以需要一个「谁先来」的事实，
    /// 而表本身的迭代顺序不能提供它。
    epoch: i64,
    /// 开它的那条连接已经没了（见模块注释「但「不随连接死」不等于「没有上界」」）。
    ///
    /// 这**不是**「已弃用」：它照样能 `prompt`、照样能被重连后的同一
    /// `channel_id` 命中。它只影响一件事——淘汰时优先挑它。
    orphaned: bool,
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
    ///
    /// # 上界在这里执行
    ///
    /// 插入之后表若超过 [`LIVE_CAP`]，**最旧的孤儿**会被摘掉并真正关掉
    /// （abort 读 task + 收子进程，见 [`LiveConversation::close`]）。淘汰只挑
    /// 孤儿——挂在本连接上的会话一条都不动，理由见 [`supernumerary`]。
    pub async fn open(
        &self,
        spec: ConversationSpec,
        default_command: Option<&str>,
    ) -> Result<(), String> {
        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed) + 1;
        let program = resolve_command(&spec.command, default_command).ok_or_else(|| {
            format!(
                "agent {} is not installed and no command is configured",
                spec.agent_id
            )
        })?;
        let env: BTreeMap<String, String> = spec.env.clone();
        // `~` 只在这里展开（见 `expand_home` 的文档）：Hub 给的是原样字符串。
        let cwd = std::path::PathBuf::from(expand_home(&spec.cwd));
        let acp_args: Vec<&str> = spec.acp_args.iter().map(String::as_str).collect();
        let args = build_args(&acp_args, &spec.args);
        let client = AcpClient::spawn(&program, &args, &env, &program, Some(&cwd))
            .await
            .map_err(|e| format!("cannot start {}: {e}", spec.agent_id))?;
        // 用**展开后**的路径，不是 `spec.cwd` 原文：agent 拿 `session/new` 的
        // `cwd` 决定在哪儿干活，给它带 `~` 的串等于让它自己猜一遍——而它多半
        // 不会展开，于是建立了一个根在 `~` 字面目录下的会话。
        let cwd_str = cwd.to_string_lossy();
        // load 与 new 二选一，**不是**「能 load 就 load、失败了再 new」：
        // load 失败通常意味着那个 id 在 agent 侧已经不存在（`/tmp` 被清、agent
        // 换了存储位置）。此时自动回落成 new 会**静默地**丢掉用户的上下文，
        // 而页面上看起来一切正常（新会话也有 id、也能回话）。报错让人看见
        // 「这条会话接不上了」，比悄悄开一条新的诚实。
        let session_id = match spec.acp_session_id.as_deref() {
            Some(existing) => {
                client
                    .load_session(existing, &cwd_str)
                    .await
                    .map_err(|e| format!("cannot restore acp session {existing}: {e}"))?;
                existing.to_string()
            }
            None => client
                .new_session(&cwd_str)
                .await
                .map_err(|e| format!("cannot create an acp session: {e}"))?,
        };

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
        // 起点是 Hub 下发的 `last_seq`——不是 0。理由见 [`Seq::new`] 与
        // [`ConversationSpec::last_seq`]：从 1 重数会让重放事件撞号后被静默吞掉。
        let seq = Seq::new(spec.last_seq);
        let reader = tokio::spawn(read_events(spec.channel_id.clone(), client, seq.clone()));

        // epoch 在**起进程之前**取（函数开头）：`open()` 里有三次 await，
        // 后开的那条完全可能先回来，取在插入时的话「最旧的孤儿」就变成了
        // 「起得最快的孤儿」——淘汰挑错人，而顺序上没人看得出来。
        let (evicted, closing) = {
            let mut live = self.live.lock().await;
            live.insert(
                spec.channel_id,
                LiveConversation {
                    client: sender,
                    session_id,
                    reader,
                    seq,
                    epoch,
                    // 刚开的这条挂在**当前**连接上，不是孤儿。
                    orphaned: false,
                },
            );
            let victims = supernumerary(&live, self.cap);
            // **先摘表再关**：关是 async 的，而锁不该跨 await 持有。摘表之后
            // 那条会话已经不可达，`prompt()` 会如实报「没有活会话」，而不是
            // 把一句话写进一个正在被关掉的 stdin。
            let mut closing = Vec::with_capacity(victims.len());
            for id in &victims {
                if let Some(conv) = live.remove(id) {
                    closing.push(conv);
                }
            }
            (victims, closing)
        };
        if !evicted.is_empty() {
            // 留痕：被关掉的是用户的对话，事后要查得出「淘汰了哪几条」。
            // 写 stderr 而不是 `say`（worker 的日志），因为本模块不持有 logger。
            eprintln!(
                "acp: live conversations over the cap of {}, reaped {} orphan(s): {}",
                self.cap,
                closing.len(),
                evicted.join(", ")
            );
        }
        // 关在锁外、**在 `open` 返回之前**：退出的 `open()` 必须交出一条真的
        // 收掉了的子进程，否则「超了上界就收一个」只是记账。
        for conv in closing {
            conv.close().await;
        }
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
        // **同一个计数器**：`turn_end` 与读循环的事件共用一条编号序列。见
        // [`Seq`] 的注释——各用一个会撞号，而撞号在 Hub 侧是静默丢一条。
        let seq = conv.seq.clone();
        let ch = channel_id.to_string();
        let text = text.to_string();
        drop(live);

        tokio::spawn(async move {
            // 表在这里起：它是**这一整轮**的耗时，而一轮的重活全落在
            // `session/prompt` 的这一问一答上（chunk 流是它的产物）。
            // 起点取在**发之前**而不是「响应回来之后倒推」：倒推得有一个
            // 「开始时刻」，而那个时刻只有这里知道。
            let started = Instant::now();
            match client.prompt(&session_id, &text).await {
                Ok(result) => {
                    // **`turn_end` 的唯一来源**：`session/prompt` 的响应带
                    // `stopReason`（实测 `end_turn`）。chunk 流自己不会说它
                    // 结束了，而序号断裂检测看不见末尾——见
                    // `session-and-trajectory.md` 3.2.1。
                    //
                    // 没有 `stopReason` 就**不写**：那不是「没走完」，是
                    // 「这次没拿到凭据」。写一条假的 `turn_end` 会让页面把
                    // 一个还在跑的轮次画成已结束。
                    if let Some(reason) = AcpClient::stop_reason(&result) {
                        let ev = turn_end_frame(&seq, reason, started);
                        let _ = channel::push_external(&ch, vec![ev]);
                    }
                }
                Err(e) => {
                    // 发不出去要说出口：否则页面停在「正在回复」。用 `batch`
                    // 里一条 agent 不认识的事件形状？不——单独一类 `error`，
                    // 页面不必猜。
                    //
                    // `error` **不占序号**：它不是对话事件。
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
            conv.close().await;
        }
    }

    /// 关掉**全部**对话。设备进程要退出时用（连接结束**不**调它，见模块注释
    /// 「会话的生死」）。
    ///
    /// 生产调用点：`worker::connect_loop` 里 `run_established` 返回 `Err`（记账失败、
    /// 进程要按 CRASH 退出）——那时留着子进程就成了没人回收的孤儿。断连**不走
    /// 这里**，走 [`Conversations::mark_orphaned`]。
    pub async fn close_all(&self) {
        let removed: Vec<LiveConversation> = {
            let mut live = self.live.lock().await;
            std::mem::take(&mut *live).into_values().collect()
        };
        for conv in removed {
            conv.close().await;
        }
    }

    /// 当前活着的会话数（测试与排障用）。
    pub async fn len(&self) -> usize {
        self.live.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.live.lock().await.is_empty()
    }

    /// 连接没了：把表里每条会话标成孤儿，**返回这次新标了几条**。
    ///
    /// # 为什么是标记而不是 `close_all`
    ///
    /// 见模块注释「会话的生死」：杀进程会毁掉重连要续上的上下文，且一个正在
    /// 跑工具的 agent 被杀会留下半途的副作用。标记只让淘汰知道「这条没人管
    /// 了」，会话本身仍然可用（还能 `prompt`）。
    ///
    /// 幂等：再调一次返回 0。
    ///
    /// **调用方必须在 `channel::on_disconnect()` 之后调它**：那个函数清的是
    /// 通道表，清完这张表里的每条会话才真的不可达。
    pub fn mark_orphaned(&self) -> usize {
        let Ok(mut live) = self.live.try_lock() else {
            // 试锁失败说明有别的臂正持着表。这个调用点在**同步**的断连收尾里
            // （`run_established` 末尾），等异步锁会把整条连接收尾挂住，所以这里
            // 宁可这一轮不标——下一次断连还会标，而漏标一轮的后果只是「它在淘汰
            // 时排不上孤儿」，不会消失。
            return 0;
        };
        let mut n = 0;
        for conv in live.values_mut() {
            if !conv.orphaned {
                conv.orphaned = true;
                n += 1;
            }
        }
        n
    }

    /// 上界可调的构造。**只给测试用**：生产用 [`Conversations::new`]（上界
    /// [`LIVE_CAP`]），否则测淘汰就得真起 9 个子进程。
    #[cfg(test)]
    fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            ..Self::default()
        }
    }
}

/// `live` 的上界。**必须有**：断连不清表（见模块注释「那张表谁管、上界在哪」），
/// 每重连一次就多一份「子进程还在跑、却再也没有调用方」的会话，没有上界就是
/// 无界增长。
///
/// 8 是「一台设备上一个用户可能同时摊开的对话数」的量级：超过它就不该是
/// 正常使用，而是孤儿在攒。淘汰只挑孤儿，所以真正被关掉的就是那些。
pub const LIVE_CAP: usize = 8;

impl LiveConversation {
    /// 收掉一条会话：先停读 task，再关子进程。
    ///
    /// 顺序不能反：读 task 持有 `recv` 的接收端，而 `shutdown` 会关 stdin——
    /// 反了会出现「读 task 还在等一条永远不会来的事件」。
    async fn close(self) {
        self.reader.abort();
        self.client.shutdown().await;
    }
}

/// 表里超过 `cap` 的那部分该淘汰谁——**只挑孤儿，且先挑最旧的**。
///
/// 摘出来单独一个函数，是因为「挑谁」是要被钉住的规则，而它混在 `open()`
/// 的异步锁里时只能靠端到端用例间接观察。
///
/// # 一条挂着的会话都不淘汰是会超界的，这里是故意的
///
/// 表里如果全是**当前连接上**的会话（没有孤儿可挑），结果为空，表就停在
/// `cap + 1`。这不是漏掉了上界，而是两条约束撞在一起时的选择：踢掉一条
/// 用户正在用的对话，比多留一个子进程更坏。无界增长来自**重连**，而每次
/// 重连都会先 [`Conversations::mark_orphaned`]，所以「全是挂着的」只可能
/// 在同一条连接上放超过 `cap` 个对话时出现——用户手速，不是泄漏。
///
/// 返回值按淘汰顺序排好（最旧的孤儿在前），调用方照抄即可。
fn supernumerary(live: &BTreeMap<String, LiveConversation>, cap: usize) -> Vec<String> {
    let over = live.len().saturating_sub(cap);
    if over == 0 {
        return Vec::new();
    }
    let mut orphans: Vec<(&String, i64)> = live
        .iter()
        .filter(|(_, c)| c.orphaned)
        .map(|(id, c)| (id, c.epoch))
        .collect();
    orphans.sort_by_key(|&(_, epoch)| epoch);
    orphans
        .into_iter()
        .take(over)
        .map(|(id, _)| id.clone())
        .collect()
}

/// 把 `agent_config.command` 为空串时的回落与 `cwd` 的展开说清楚。
///
/// # `cwd` 里的 `~` 必须在这里展开
///
/// Hub 存的工作区路径是**原样字符串**（`Workspace::path` 的注释明说「不做 `~`
/// 展开」），因为 Hub 不知道设备上的家目录是哪个。而设备侧执行的是
/// `Command::current_dir(cwd)`——**没有 shell 参与，`~` 不会被展开**。
///
/// 后果不报错：agent 会在设备进程自己的 cwd 里跑起来，然后在那里建文件。症状
/// 是「对话能开，但 agent 说找不到我的项目」。所以展开只能在设备侧做，就在
/// 起子进程之前。
///
/// 只展开**开头**的 `~`，且只认 `~/` 与裸 `~`：`~user/...` 需要查密码库，
/// 而用户的工作区路径不可能是那个形态，认它只会让「展开失败」变成一条没人
/// 看得懂的错。展开不成就原样传下去，让 `current_dir` 报出那句真实错误。
fn expand_home(cwd: &str) -> String {
    let rest = match cwd.strip_prefix('~') {
        Some(rest) => rest,
        None => return cwd.to_string(),
    };
    // `~foo` 不是「家目录」，是一个以 ~ 开头的相对名字。不猜。
    if !(rest.is_empty() || rest.starts_with('/')) {
        return cwd.to_string();
    }
    match std::env::var_os("HOME") {
        Some(home) => format!("{}{rest}", home.to_string_lossy()),
        // 拿不到家目录：原样传下去，让 `current_dir` 自己报错。编一个路径只会
        // 让 agent 在一个不存在的目录里失败，而错误指向的是我们编的那个。
        None => cwd.to_string(),
    }
}

/// 一条对话的序号发生器：**只增，且整条对话共用一个**。
///
/// # 为什么是共享的、而不是每个 task 各一个
///
/// 编号的有两处：读循环的事件流，与 `prompt` 拿到 `stopReason` 后写的
/// `turn_end`。它们是**并发**的两个 task。两个计数器从同一个数开始，于是
/// 第 N 条事件与这一轮的 `turn_end` 会有同一个序号——而在 Hub 侧，同一
/// `(session_id, seq)` 的第二条被 `UNIQUE` **`INSERT OR IGNORE` 掉**。
///
/// 结果是丢一条，且**看不出来**：序号仍然连续，断路检测（`gaps`）不报警，
/// 页面也照样画。
///
/// # 这条约束**靠什么保证**（实测过，别高估类型）
///
/// 曾经以为「除了通过 `Seq` 之外没有办法拿到序号」就能挡住误用。实测不成立：
/// 给 `read_events` 另传一个 `Seq::new()`，全部单测**仍然全绿**——因为单测证
/// 明的是 `Seq` 的**克隆语义**，不是调用点传的是不是同一个。
///
/// 所以真正的约束是：`Seq::new()` 在**生产代码里只出现一次**，就在
/// [`Conversations::open`]。两处编号都从 `open` 传下去的同一份克隆取号。
/// 新增编号处**不要** `Seq::new()`，从会话表里那份 `.clone()`。
#[derive(Debug, Clone)]
pub struct Seq(Arc<AtomicI64>);

impl Seq {
    /// **生产代码里只有 [`Conversations::open`] 该调它**——见类型文档的
    /// 「这条约束靠什么保证」。其余地方一律 `.clone()` 已有的那一份。
    ///
    /// `start` 是**这条会话已经落库的最大 `seq`**（Hub 下发，
    /// 见 [`ConversationSpec::last_seq`]）。传 0 表示一条都还没有。
    ///
    /// # 为什么起点不能恒为 0
    ///
    /// 计数器活在 ACP 子进程里，每个子进程从 0 数起；而 `seq` 的契约是
    /// **会话内**只增。重开对话（尤其 `session/load`）会新建子进程，于是
    /// 新事件从 1 重数、撞上库里已有的 1，被 `UNIQUE` + `INSERT OR IGNORE`
    /// 静默吞掉。传历史最大值续号，才让那个唯一约束真正表达「同一事件
    /// 不写两行」而不是「新事件被旧事件顶掉」。
    pub fn new(start: i64) -> Self {
        // 负值没有意义（`seq` 只由本模块发），压到 0 而不是照抄：照抄会让
        // `next()` 吐出 0 或负数，而 0 是「没有序号」的保留值。
        Self(Arc::new(AtomicI64::new(start.max(0))))
    }

    /// 取下一个序号。从 1 开始（`0` 留给「没有序号」）。
    pub fn next(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 已经发到几了。给重连用（续着往下编，而不是重新从 1 开始）。
    pub fn last(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
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

/// 读事件 → 编序号 → 聚合 → 推进通道。**这个 task 死掉等于这条对话瞎了**。
///
/// # `seq` 为什么在这里生成
///
/// ACP 的事件**没有序号也没有时间戳**（实测 `agent_message_chunk` /
/// `tool_call` / `usage_update` 的 payload 里两者皆无）。而序号是 Hub 侧判断
/// 「有没有丢」的唯一依据（`TrajectoryRepo::gaps` 看的是 `seq` 断裂）。所以
/// 序号只能由**唯一见到原始顺序的那一方**生成——就是这个读循环。
///
/// Hub 侧按到达顺序编号是不行的：一次 `flush` 里的多条事件在 Hub 眼里是
/// **一帧**，而一帧内部到达顺序本身就由这里的 `batch` 决定，等于让 Hub 重新
/// 猜一遍它已经失去的信息。
async fn read_events(channel_id: String, client: AcpClient, seq: Seq) {
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
                    let mut update = params.get("update").cloned().unwrap_or(params);
                    // 把序号写进事件本身，而不是让 payload 与序号分两个并行
                    // 数组传：后者一旦两边长度不一致，整批事件的序号全错，
                    // 而错法是**看上去对**（序号连续，只是挂错了事件）。
                    let n = seq.next();
                    if let Value::Object(o) = &mut update {
                        o.insert("seq".to_string(), json!(n));
                    }
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
                    //
                    // 它**占用一个序号**：这是设备上真实发生过的事件，跳过它
                    // 会让 `gaps` 在每一处权限请求上报警告——用户会以为丢了东西。
                    let n = seq.next();
                    let ev = json!({
                        "type": "request",
                        "seq": n,
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
                    // `exited` **不占序号**：它不是对话事件，是传输终态。给它
                    // 序号会让下一轮重连时的序号从错位开始。
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

/// 一帧带计时的 `turn_end`。**帧格式是对外契约**：Hub 侧
/// `hub-server::routes::ws::persist_trajectory` 按 `type` 认它、按
/// `durationMs` 取数，页面按 `stopReason` 切消息块。
///
/// # 为什么耗时在这里算，而不是交给 Hub 猜
///
/// Hub 只见到「这一帧到了」这一个时刻，而它要的是「这一轮跑了多久」。拿
/// 相邻帧的到达时刻做差分，量出来的是**帧间隔**——中间夹着聚合窗口
/// （[`BATCH_INTERVAL`]）与网络往返，与「模型跑了多久」是两件事。只有这里
/// 同时拿着起点与终点。
///
/// # 为什么传 `Option`，而不是一个 `i64`
///
/// `Instant::elapsed()` 恒非负，**这一侧**不会给出 `None`。留 `Option` 是为了
/// 让「毫秒数装不进 `i64`」这种退化有一个明确的出口：写 `u64::MAX` 会让审计
/// 看见一条 5849 亿年的耗时，而写 `null` 的意思是「这一次没测到」。
/// 一轮跑满 `i64::MAX` 毫秒（2.9 亿年）不可能发生，但这行代码不该依赖那个判断。
fn turn_end_frame(seq: &Seq, stop_reason: &str, started: Instant) -> Value {
    let elapsed = started.elapsed();
    json!({
        "type": "turn_end",
        "stopReason": stop_reason,
        "seq": seq.next(),
        "durationMs": i64::try_from(elapsed.as_millis()).ok(),
    })
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
    use std::path::{Path, PathBuf};

    /// **`cwd` 的 `~` 只在设备侧展开，且只展开开头那一个。**
    ///
    /// 这条挡的是「对话能开、agent 在错误目录里跑」——症状是 agent 说找不到
    /// 用户的项目，而没有任何报错。反向验证：把 `expand_home` 改成恒等函数，
    /// 前两条会红。
    ///
    /// 依赖 `HOME` 的用例改进程环境是全局的（与 `detect.rs` 顶部那条注释同样的
    /// 理由），所以这里**不设 `HOME`**，而是按「当前 `HOME` 是什么」算期望值；
    /// 没有 `HOME` 时走「原样传」的断言。两种机器上都成立。
    #[test]
    fn only_a_leading_tilde_is_expanded_on_the_device() {
        match std::env::var_os("HOME") {
            Some(home) => {
                let home = home.to_string_lossy().into_owned();
                assert_eq!(expand_home("~/src/app"), format!("{home}/src/app"));
                assert_eq!(expand_home("~"), home);
            }
            None => {
                assert_eq!(expand_home("~/src/app"), "~/src/app");
                assert_eq!(expand_home("~"), "~");
            }
        }
        // 绝对路径原样：这是最常见的形态，改写它等于自己造 bug。
        assert_eq!(expand_home("/srv/app"), "/srv/app");
        // `~user/x` 需要查密码库，不猜。
        assert_eq!(expand_home("~root/src"), "~root/src");
        // 相对路径里的 `~` 不是家目录。
        assert_eq!(expand_home("a/~/b"), "a/~/b");
        assert_eq!(expand_home(""), "");
    }

    /// **`Seq` 的克隆共享同一个计数**：两个并发 task 各取号也不会撞。
    ///
    /// 这条挡的是「丢一条且看不出来」：撞号在 Hub 侧被 `UNIQUE` 的
    /// `INSERT OR IGNORE` 静默吃掉，而序号仍然连续——`gaps` 不报警、页面照画。
    ///
    /// **它只证明到 `Seq` 自己的克隆语义，证明不到调用点**。实测把
    /// `read_events` 换成另传一个 `Seq::new()`，这条**仍然全绿**。所以
    /// 「两处编号是同一个」靠的是 `Seq::new()` 在生产代码里只出现一次，
    /// 见类型文档的「这条约束靠什么保证」。
    #[test]
    fn a_cloned_seq_keeps_one_numbering_for_the_whole_conversation() {
        let seq = Seq::new(0);
        assert_eq!(seq.last(), 0, "还没取过号");
        assert_eq!(seq.next(), 1, "从 1 开始：0 留给「没有序号」");
        assert_eq!(seq.next(), 2);

        let for_reader = seq.clone();
        assert_eq!(for_reader.next(), 3, "克隆的是同一个计数，不是一份拷贝");
        assert_eq!(seq.last(), 3, "原句柄也看见 3");
        assert_eq!(seq.next(), 4, "接着往下编，不回到 1");
    }

    /// **起点是「这条会话已落库的最大 seq」，不是恒为 0。**
    ///
    /// 这条挡的是重开对话时编号从 1 重数：新事件撞上库里已有的 `seq`，
    /// 被 `UNIQUE` + `INSERT OR IGNORE` 静默吞掉（实测让一条
    /// `user_message_chunk` 整条消失，见 `ConversationSpec::last_seq`）。
    ///
    /// 反向验证：把 `Seq::new(spec.last_seq)` 改回 `Seq::new(0)`，下面第一句
    /// 会红在「起点没用上」。负值压到 0 那一句是给「别人编了个负号」兜底——
    /// `seq` 只由本模块发，负值本不该出现，但不该让它吐 0 号事件。
    #[test]
    fn a_seq_starts_from_the_given_point_so_reopening_does_not_reuse_numbers() {
        let seq = Seq::new(7);
        assert_eq!(seq.last(), 7, "起点就是历史最大值");
        assert_eq!(seq.next(), 8, "续着往下编，不回到 1");
        assert_eq!(
            Seq::new(-3).last(),
            0,
            "负值压到 0：0 是「没有序号」的保留值"
        );
        assert_eq!(Seq::new(-3).next(), 1);
    }

    /// **空串 = 用扫描到的那一个**；两者都空 = 起不来。
    ///
    /// 这条契约同时被 `agent_config` 的三条端点与 `acp.rs` 的 `spawn` 依赖，
    /// 写错的表现是「配了命令却不生效」或「没配就崩」。
    #[test]
    fn the_command_falls_back_to_the_detected_binary() {
        assert_eq!(
            resolve_command("/x/y", Some("/a/b")),
            Some("/x/y".to_string())
        );
        assert_eq!(
            resolve_command("", Some("/a/b")),
            Some("/a/b".to_string()),
            "空串必须回落到扫描到的那个"
        );
        assert_eq!(
            resolve_command("   ", Some("/a/b")),
            Some("/a/b".to_string())
        );
        assert_eq!(
            resolve_command("", None),
            None,
            "没配也没扫到：起不来，不能编一个命令"
        );
        assert_eq!(resolve_command("", Some("")), None, "扫到空串等于没扫到");
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
        assert_eq!(build_args(&[], &["--x".to_string()]), vec!["--x"]);
    }

    /// 帧格式是**对外契约**（页面按 `type` 分派），钉住它。
    ///
    /// **必须从通道里把帧收回来断言**，不能只断言 `batch` 被清空：后者在
    /// `type` 改名、`events` 改名时照样全绿，等于没钉住任何东西。
    /// 取帧走 `pump()`（生产代码放行积压的同一个出口），所以这里断言的
    /// 就是设备真的会发出去的那一帧。
    #[test]
    fn the_batch_and_turn_end_shapes_are_pinned() {
        let _guard = channel::table_lock_for_tests();
        channel::reset_for_tests();
        let ch = "ch-shape-pinned";
        channel::open_external(ch);

        let mut batch = vec![json!({ "sessionUpdate": "agent_message_chunk" })];
        let mut bytes = 100;
        flush(ch, &mut batch, &mut bytes);

        let pumped = channel::pump();
        let frames = pumped
            .iter()
            .find(|(id, _)| id == ch)
            .map(|(_, f)| f.clone())
            .expect("flush 之后通道里必须有一帧待发");
        assert_eq!(
            frames[0].c,
            json!({
                "channel_id": ch,
                "payload": {
                    "type": "batch",
                    "events": [{ "sessionUpdate": "agent_message_chunk" }]
                }
            }),
            "batch 帧的 wire 形状是对外契约：`channel_data` 信封（协议 §4）+ 页面按 type 分派、按 events 取事件"
        );

        // 空批不发帧（否则页面收到一堆空 batch）
        channel::reset_for_tests();
        channel::open_external(ch);
        let mut empty: Vec<Value> = Vec::new();
        let mut z = 0;
        flush(ch, &mut empty, &mut z);
        assert!(
            channel::pump().iter().all(|(_, f)| f.is_empty()),
            "空批不许产生任何帧"
        );
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
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_real_opencode_turn_reaches_the_channel_as_a_batch() {
        let bin = std::env::var("AF_TEST_AGENT_BIN").unwrap_or_else(|_| {
            "/root/.local/share/mise/installs/npm-opencode-cli/latest/bin/opencode".to_string()
        });
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        // 通道表是进程级单例：这条用例必须独占（`_guard` 活到用例结束）。
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();

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
            // 真机这条测的是「一轮问答能不能走通」，不是在测 load，
            // 所以第一次开对话照常新建。
            acp_session_id: None,
            // 同上：全新会话，库里一条事件都没有，起点 0。
            last_seq: 0,
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
                            for ev in p
                                .get("events")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default()
                            {
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

    // ---- 子任务 8：`acp_session_id` 决定走 load 还是 new ----

    /// 一个**把收到的每个 method 名追加到日志**的假 agent。
    ///
    /// 这是本组测试的唯一观测手段：`open()` 走 load 还是 new 是它内部的
    /// 一个分支，从外面看不见——只有 agent 侧知道 Hub 到底发了哪个 RPC。
    /// 用真 opencode 验不了这一条：真 agent 两件事都能做，而我们要断言的是
    /// **哪一件被做了**。
    fn recording_agent(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("methods.log");
        let path = dir.join("rec-agent");
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
        # 带 `loadSession`：不带的话 `load_session` 会先拒绝，于是「续上原会话」
        # 这条判据根本走不到（重连续话正是本组测试要钉的东西）。
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"protocolVersion": 1,
              "agentCapabilities": {{"loadSession": True}}}}}})
    elif method == "session/new":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"sessionId": "ses_brand_new"}}}})
    elif method == "session/load":
        # 真机形状：只有 configOptions，没有条数。重放走事件流。
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"configOptions": []}}}})
    elif method == "session/prompt":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"stopReason": "end_turn"}}}})
"#,
            log = log.to_string_lossy()
        );
        std::fs::write(&path, script).expect("写脚本");
        // 脚本要能执行，`AcpClient::spawn` 直接 exec 它。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        (path, log)
    }

    fn spec_for(ch: &str, bin: &Path, acp_session_id: Option<&str>) -> ConversationSpec {
        // `cwd` 必须**真实存在**：`AcpClient::spawn` 会 `current_dir(cwd)`，
        // 给一个不存在的目录时 `spawn` 直接 `No such file or directory`——
        // 而错因看起来像「可执行文件不存在」（它就是个绝对路径）。
        ConversationSpec {
            channel_id: ch.to_string(),
            agent_id: "opencode".to_string(),
            cwd: bin
                .parent()
                .expect("脚本必有父目录")
                .to_string_lossy()
                .into_owned(),
            command: bin.to_string_lossy().into_owned(),
            args: Vec::new(),
            acp_args: Vec::new(),
            env: BTreeMap::new(),
            acp_session_id: acp_session_id.map(str::to_string),
            // 单测里这些会话都是"第一次开"，所以起点 0（= 一条事件都没有）。
            // 续号的行为由 `a_seq_starts_from_the_given_point...` 单独验。
            last_seq: 0,
        }
    }

    fn methods_in(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// **没有 `acp_session_id` → 走 `session/new`，且 `ready` 里是新 id。**
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_conversation_without_an_acp_id_creates_a_new_session() {
        // 通道表是进程级单例：不独占就有别的用例在中间 reset，`ready` 会被抢走
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, log) = recording_agent(d.path());
        let convs = Conversations::default();

        // 生产路径上这一步由 `channel::on_channel_frame` 的 `Action::OpenAcp`
        // 分支做（`lock().open_external`）。直接调 `Conversations::open` 会
        // 跳过它，而通道不存在时 `push_external` 返回 `None`——`ready` 就被
        // 静默丢掉，看起来像「open() 没推 ready」。
        let _ = channel::open_external("ch_new");
        convs
            .open(spec_for("ch_new", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");

        let methods = methods_in(&log);
        assert!(
            methods.contains(&"session/new".to_string()),
            "没有 acp_session_id 时必须新建，实际: {methods:?}"
        );
        assert!(
            !methods.contains(&"session/load".to_string()),
            "没有 acp_session_id 时不该 load，实际: {methods:?}"
        );

        // `ready` 里带的是 agent 给的新 id。
        let frames: Vec<_> = channel::pump().into_iter().flat_map(|(_, f)| f).collect();
        let ready = frames
            .iter()
            .filter_map(|f| f.payload::<protocol::channel::ChannelData>().ok())
            .find(|d| d.payload.get("type").and_then(Value::as_str) == Some("ready"))
            .expect("必须推一条 ready");
        assert_eq!(
            ready.payload["sessionId"],
            json!("ses_brand_new"),
            "ready 要把 agent 给的 sessionId 带上去，Hub 靠它回填"
        );

        convs.close("ch_new").await;
    }

    /// **`open()` 必须把 Hub 下发的 `last_seq` 灌进 `Seq`，不自己从 0 数。**
    ///
    /// 这条挡的是「重开对话时编号从 1 重数」：新事件撞上库里已有的 `seq`，
    /// 被 `UNIQUE` + `INSERT OR IGNORE` 静默吞掉——实测让一条
    /// `user_message_chunk` 整条消失，而 `gaps` 不报警（seq 仍然连续）。
    ///
    /// **反向验证过**：把 `open()` 里的 `Seq::new(spec.last_seq)` 改回
    /// `Seq::new(0)`，这条会红在「第一条事件的 seq 应该接着 4 往下」。
    /// （只测 `Seq::new` 本身是**不够**的——那证明的是构造函数，证明不到
    /// `open()` 有没有把参数传进去。这一条才是接线判据。）
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn open_continues_the_seq_from_the_last_persisted_one() {
        // 通道表是进程级单例：不独占就有别的用例在中间 reset，`turn_end` 会被抢走
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        // 这条判据看的是 `turn_end` 的 `seq`，不看方法序，所以日志不用。
        let (bin, _log) = recording_agent(d.path());
        let convs = Conversations::default();

        let _ = channel::open_external("ch_seq");
        let mut spec = spec_for("ch_seq", &bin, None);
        // 库里这条会话已经有 1..4，设备必须从 5 开始编。
        spec.last_seq = 4;
        convs.open(spec, None).await.expect("假 agent 必须能起来");

        // 发一句话，然后等这一轮的 `turn_end`——它和外发事件共用一个计数器，
        // 所以它的 seq 就是「open() 的起点有没有生效」的直接证据。
        //
        // `prompt()` 是 `tokio::spawn` 出去的（见它的文档：不等回复），所以
        // 这里必须**轮询 yield** 而不是发完就查。
        convs
            .prompt("ch_seq", "hi")
            .await
            .expect("prompt 必须发得出去");
        let mut turn_end_seq = None;
        for _ in 0..200 {
            // 通道表是进程级单例，别的并发用例也会往 `pump()` 里丢帧；
            // 只认自己这条通道，别把别人的 `turn_end` 当成自己的。
            for (ch, frames) in channel::pump() {
                if ch != "ch_seq" {
                    continue;
                }
                for fr in frames {
                    let Ok(data) = fr.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    if data.payload.get("type").and_then(Value::as_str) == Some("turn_end") {
                        turn_end_seq = data.payload.get("seq").and_then(Value::as_i64);
                    }
                }
            }
            if turn_end_seq.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let got = turn_end_seq.expect("本轮必须有一条 turn_end");
        assert!(
            got > 4,
            "seq 起点没用上：turn_end 的 seq 是 {got}，应当接着 4 往下（> 4）。\
             说明 open() 自己从 0 数了，重开对话会与库里已有的 seq 撞号"
        );

        convs.close("ch_seq").await;
    }

    /// **一轮的耗时由设备掐表，随 `turn_end` 帧发出去。**
    ///
    /// 这是轨迹里「耗时」那一列唯一的真值来源：ACP 事件不带时间戳，而 Hub 拿
    /// 相邻帧的到达时刻做差分，量到的是**帧间隔**而不是「这一轮跑了多久」。
    /// 只有这里同时拿着 `session/prompt` 的发出与响应两个时刻。
    ///
    /// **反向验证（实跑过）**：把 `turn_end_frame` 里的 `"durationMs"` 那一对
    /// 删掉，这条会红在「turn_end 必须带 durationMs」——「帧里有这个字段」与
    /// 「Hub 侧读得到它」是两件事，两处各有一条测试钉着。
    ///
    /// 判据只要求**这个键存在且是个数**，不断言它 ≥ 某个值：假 agent 是秒回的，
    /// 而给一个下界会让这条在快机器上红（把「快」判成「没测」）。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_turn_end_carries_the_duration_the_device_measured() {
        // 通道表是进程级单例：不独占就有别的用例在中间 reset，`turn_end` 会被抢走
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, _log) = recording_agent(d.path());
        let convs = Conversations::default();

        let _ = channel::open_external("ch_dur");
        convs
            .open(spec_for("ch_dur", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        convs
            .prompt("ch_dur", "hi")
            .await
            .expect("prompt 必须发得出去");

        // `prompt()` 是 `tokio::spawn` 出去的（见它的文档：不等回复），所以轮询。
        let mut found = None;
        for _ in 0..200 {
            for (ch, frames) in channel::pump() {
                if ch != "ch_dur" {
                    continue;
                }
                for fr in frames {
                    let Ok(data) = fr.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    if data.payload.get("type").and_then(Value::as_str) == Some("turn_end") {
                        found = Some(data.payload.clone());
                    }
                }
            }
            if found.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let payload = found.expect("本轮必须有一条 turn_end");
        let ms = payload
            .get("durationMs")
            .expect("turn_end 必须带 durationMs");
        assert!(
            ms.is_number() || ms.is_null(),
            "durationMs 要么是一个毫秒数、要么是 null（没测到），实际: {ms}"
        );
        assert!(
            ms.as_i64().is_some_and(|v| v >= 0),
            "测到了就必须是非负的毫秒数，实际: {ms}"
        );
        // `stopReason` 仍然在：耗时是**多带**的一个字段，不是把它换掉。
        assert_eq!(
            payload.get("stopReason").and_then(Value::as_str),
            Some("end_turn")
        );

        convs.close("ch_dur").await;
    }

    /// **有 `acp_session_id` → 走 `session/load`，且不新建。**
    ///
    /// 这条是子任务 8 的核心判据。反向验证：把 `open()` 改回恒走
    /// `new_session`，这条会红在「必须 load」那句。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_conversation_with_an_acp_id_loads_instead_of_creating() {
        // 通道表是进程级单例：不独占就有别的用例在中间 reset，`ready` 会被抢走
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, log) = recording_agent(d.path());
        let convs = Conversations::default();

        let _ = channel::open_external("ch_load");
        convs
            .open(spec_for("ch_load", &bin, Some("ses_existing")), None)
            .await
            .expect("假 agent 必须能起来");

        let methods = methods_in(&log);
        assert!(
            methods.contains(&"session/load".to_string()),
            "有 acp_session_id 时必须 load，实际: {methods:?}"
        );
        assert!(
            !methods.contains(&"session/new".to_string()),
            "有 acp_session_id 时不该新建（那会丢掉历史上下文），实际: {methods:?}"
        );

        // load 之后推出去的 `ready` 里必须还是**原来那个 id**，不是 agent 编的
        // 新号：Hub 存的就是它，下一轮还要用它。
        let frames: Vec<_> = channel::pump().into_iter().flat_map(|(_, f)| f).collect();
        let ready = frames
            .iter()
            .filter_map(|f| f.payload::<protocol::channel::ChannelData>().ok())
            .find(|d| d.payload.get("type").and_then(Value::as_str) == Some("ready"))
            .expect("必须推一条 ready");
        assert_eq!(
            ready.payload["sessionId"],
            json!("ses_existing"),
            "load 之后要沿用原来的 id，不能换成别的"
        );

        convs.close("ch_load").await;
    }

    /// **load 失败不回落成 new**：报错，而且不许悄悄新建一条。
    ///
    /// 自动回落是最诱人也最坏的选择：页面上一切正常（新会话有 id、也能回话），
    /// 只是用户的上下文没了。所以这条钉住「宁可失败也要让人看见」。
    #[tokio::test]
    async fn a_failing_load_reports_an_error_instead_of_creating_a_new_session() {
        let d = tempfile::tempdir().expect("tmp");
        let log = d.path().join("methods.log");
        let path = d.path().join("failing-load");
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
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"protocolVersion": 1,
              "agentCapabilities": {{"loadSession": True}}}}}})
    elif method == "session/load":
        send({{"jsonrpc": "2.0", "id": mid,
              "error": {{"code": -32602, "message": "no such session"}}}})
    elif method == "session/new":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"sessionId": "ses_fallback"}}}})
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

        let convs = Conversations::default();
        let err = convs
            .open(spec_for("ch_fail", &path, Some("ses_gone")), None)
            .await
            .expect_err("load 失败必须报错");

        assert!(
            err.contains("ses_gone"),
            "错误里要点名是哪个会话接不上，实际: {err}"
        );
        let methods = methods_in(&log);
        assert!(
            !methods.contains(&"session/new".to_string()),
            "load 失败**不许**悄悄回落成新建（那会静默丢上下文），实际: {methods:?}"
        );
    }

    // ---- 会话表的上界与「断连留下的是孤儿，不是垃圾」 ----

    /// 等一条会话的 `session_id` 变成 `want`（`open()` 是 async 的，`mark_orphaned`
    /// 只读表、不改它，所以这里只是把「表里已经插进去了」变成可轮询的事实）。
    async fn wait_for_session_id(convs: &Conversations, ch: &str, want: &str) -> String {
        for _ in 0..200 {
            let got = {
                let live = convs.live.lock().await;
                live.get(ch).map(|c| c.session_id.clone())
            };
            if let Some(got) = got {
                assert_eq!(got, want, "{ch} 接上的应该是 {want}");
                return got;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{ch} 一直没出现在会话表里");
    }

    /// 把**指定的**几条标成孤儿（其余保持「挂在本连接上」）。
    ///
    /// 用它而不是整表的 `mark_orphaned`，是因为淘汰偏好要验的是「挂着与孤儿
    /// 混在一张表里时挑谁」——整表标记只能造出「全是孤儿」这一种局面，那样
    /// 「只挑孤儿」这个过滤器就验不到了（去掉它照样全绿）。
    async fn mark_only(convs: &Conversations, ids: &[&str]) {
        let mut live = convs.live.lock().await;
        for id in ids {
            if let Some(c) = live.get_mut(*id) {
                c.orphaned = true;
            } else {
                panic!("{id} 不在会话表里");
            }
        }
    }

    /// 表里现在有哪几条（按键序，便于断言）——淘汰是**不存在**的证据，
    /// 只能靠「表里没有它」来看。
    async fn ids_of(convs: &Conversations) -> Vec<String> {
        let live = convs.live.lock().await;
        live.keys().cloned().collect()
    }

    /// **上界是硬的：开超过 cap 条，表就停在 cap。**
    ///
    /// 挡的是「重连 N 次攒 N 份」：断连不清表（子进程要留给续话），而
    /// `channel::on_disconnect()` 之后 Hub 再也不会发 `CLOSE_CHANNEL`，
    /// 于是没有上界的话，每重连一次就多一批「子进程还在跑、谁也再叫不动」的会话。
    ///
    /// 反向验证：把 `open()` 里的 `supernumerary(...)` 换成 `Vec::new()`（= 不淘汰），
    /// 这条会红在 `len()` 上。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn the_live_table_never_grows_past_its_cap() {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, _log) = recording_agent(d.path());
        let convs = Conversations::with_cap(2);

        for i in 0..5 {
            let ch = format!("ch_cap_{i}");
            let _ = channel::open_external(&ch);
            convs
                .open(spec_for(&ch, &bin, None), None)
                .await
                .expect("假 agent 必须能起来");
            // 每开一条就断一次：这才是生产中攒孤儿的形态，也是淘汰唯一会动手的
            // 情形（挂着的会话一律不淘汰，见 `supernumerary`）。
            convs.mark_orphaned();
        }

        assert_eq!(
            convs.len().await,
            2,
            "开了 5 条、上界 2：表必须停在 2，否则重连 N 次就是 N 倍的子进程"
        );
        convs.close_all().await;
        assert!(convs.is_empty().await);
    }

    /// **淘汰只挑孤儿，且不碰挂在本连接上的会话。**
    ///
    /// 挡的是「为了压住上界把用户正在聊的会话打死」：那比多留一个子进程坏得多。
    ///
    /// 三个断言各钉一件事，且都放在**会打破它的那一步**之后：
    /// 1. 挂着的那条不许被淘汰（它是最旧的，只按「最旧」挑的实现会先挑它）；
    /// 2. 有孤儿可挑时，挑的是最旧的那个孤儿；
    /// 3. 孤儿全没了、只剩挂着的那些时，宁可超界也不踢它们。
    ///
    /// 反向验证：把 `supernumerary` 的 `filter(|(_, c)| c.orphaned)` 去掉（= 也
    /// 把挂着的算进候选），断言 1 与 3 会红。
    #[tokio::test]
    async fn eviction_prefers_orphans_and_never_evicts_an_attached_conversation() {
        let d = tempfile::tempdir().expect("tmp");
        let (bin, _log) = recording_agent(d.path());

        // --- 断言 1：表里全是挂在本连接上的会话时，宁可超界也不淘汰 ---
        // cap=1，插两条挂着的 → 超界 1，但一个都不许踢。
        let attached_only = Conversations::with_cap(1);
        attached_only
            .open(spec_for("ch_attached", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        attached_only
            .open(spec_for("ch_attached_2", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        let ids = ids_of(&attached_only).await;
        assert!(
            ids.contains(&"ch_attached".to_string()) && ids.contains(&"ch_attached_2".to_string()),
            "全是挂着的时候宁可超界也不许淘汰（淘汰等于在用户聊到一半时打死它），实际表里: {ids:?}"
        );
        attached_only.close_all().await;

        // --- 断言 2：有孤儿可挑时，动的是孤儿，不是挂着的 ---
        // `ch_attached` 最旧且挂着，`ch_orphan` 晚来但已被标成孤儿。只按「最旧」
        // 挑的实现会先踢 `ch_attached`。
        let convs = Conversations::with_cap(1);
        convs
            .open(spec_for("ch_attached", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        convs
            .open(spec_for("ch_orphan", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        mark_only(&convs, &["ch_orphan"]).await;
        // 再插一条 → 表到 3、cap=1，超 2 条，但孤儿只有 1 个。
        convs
            .open(spec_for("ch_attached_2", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        let ids = ids_of(&convs).await;
        assert!(
            ids.contains(&"ch_attached".to_string()) && ids.contains(&"ch_attached_2".to_string()),
            "挂在本连接上的会话**绝不能**被淘汰（它比淘汰孤儿更坏），实际表里: {ids:?}"
        );
        assert!(
            !ids.contains(&"ch_orphan".to_string()),
            "有孤儿可挑时淘汰的必须是孤儿，实际表里: {ids:?}"
        );
        convs.close_all().await;

        // --- 断言 3：多个孤儿时挑最旧的那个 ---
        let convs = Conversations::with_cap(2);
        convs
            .open(spec_for("ch_keep", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        convs
            .open(spec_for("ch_orphan_old", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        convs
            .open(spec_for("ch_orphan_new", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        mark_only(&convs, &["ch_orphan_old", "ch_orphan_new"]).await;
        // 再插一条挂着的新会话：表到 4、cap=2，超 2 → 淘汰两个最旧的孤儿。
        convs
            .open(spec_for("ch_new", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        let ids = ids_of(&convs).await;
        assert!(
            !ids.contains(&"ch_orphan_old".to_string())
                && !ids.contains(&"ch_orphan_new".to_string()),
            "超 2 条就要淘汰两个孤儿，实际表里: {ids:?}"
        );
        assert!(
            ids.contains(&"ch_keep".to_string()),
            "挂着的不许被淘汰，实际表里: {ids:?}"
        );
        assert_eq!(convs.len().await, 2);
        convs.close_all().await;
    }

    /// **断连是标记，不是清表**：被标记的会话仍然能接上原 ACP 会话（续话）。
    ///
    /// 挡的正是「看起来最省事」的那个改法——把 `close_all` 挂到断连上：它会把
    /// 子进程连同上下文一起扔掉，重连后用户看到一条空的对话，而这件事在页面上
    /// 不报错（新会话也能回话），于是没人发现。
    ///
    /// 反向验证：把 `mark_orphaned` 改成 `self.close_all().await` 那种清表实现
    /// （或让 `open` 在插入前先清表），下面第一句 `len()` 就红。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_conversation_outlives_a_disconnect_so_it_can_be_resumed() {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, log) = recording_agent(d.path());
        // cap 大得离谱：这条要证明的是「标记不清表」，与淘汰无关。
        let convs = Conversations::with_cap(64);

        let _ = channel::open_external("ch_resume");
        convs
            .open(spec_for("ch_resume", &bin, Some("ses_keep_me")), None)
            .await
            .expect("假 agent 必须能起来");
        // 断连：通道表清了，会话表只被标记。
        channel::on_disconnect();
        assert_eq!(
            convs.mark_orphaned(),
            1,
            "断连要把这条会话标成孤儿（淘汰挑它、日志里数它）"
        );

        assert_eq!(
            convs.len().await,
            1,
            "断连之后会话必须还在表里：它不是垃圾，它是用户那条对话的上下文"
        );

        // 真正要守住的东西：**原 ACP 会话还在**。再开一次（重连后 Hub 用同一个
        // channel_id 回来）时走的是 load，而不是一条空的新会话。
        let _ = channel::open_external("ch_resume");
        let n_before = methods_in(&log).len();
        convs
            .open(spec_for("ch_resume", &bin, Some("ses_keep_me")), None)
            .await
            .expect("重连后必须还能接上原来的会话");
        let methods_after = methods_in(&log);
        assert!(
            !methods_after[n_before..].contains(&"session/new".to_string()),
            "重连后不许新建会话（那会丢掉上下文），实际新增: {:?}",
            &methods_after[n_before..]
        );

        // `prompt()` 仍然找得到会话、且用的是原 session_id——「表里还在」没有
        // 意义，能用才有意义。
        let sid = wait_for_session_id(&convs, "ch_resume", "ses_keep_me").await;
        assert_eq!(sid, "ses_keep_me");
        convs
            .prompt("ch_resume", "still there?")
            .await
            .expect("孤儿会话仍要能收消息（它没死，只是没人挂在上面）");
        convs.close_all().await;
        assert!(convs.is_empty().await);
    }
}
