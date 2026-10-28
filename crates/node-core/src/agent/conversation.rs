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
    /// 「第几轮」的单调计数：**每次 `prompt` 开一轮就加一**。
    ///
    /// # 为什么需要它（而不是一个 bool）
    ///
    /// `prompt()` 是「发出去就不等」的（见它的文档），所以「现在有没有一轮在飞」
    /// 只能由发的那一方记。`AtomicBool` 不够：一轮结束时要把它清零，而那时
    /// 可能**下一轮已经开了**——后到的那条 `turn_end` 会把新的一轮误标成
    /// 「没人跑」，于是停止按钮在真正该亮的时候是灰的。带上代数，收尾只在
    /// 「我这一轮还是最新那一轮」时才清。
    turn: Arc<AtomicI64>,
    /// 最近一轮的 `session/prompt` 结果还**没有**回来时的会话号。
    ///
    /// 与 [`Self::turn`] 的比较是原子的一对：`inflight == turn` 才是「有东西
    /// 在飞」。两者都从 0 起步，所以「从没跑过」天然是「没有东西在飞」。
    inflight: Arc<AtomicI64>,
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
                    // 一条还没有跑过轮次的会话：0 == 0，也就是「没有东西在飞」。
                    turn: Arc::new(AtomicI64::new(0)),
                    inflight: Arc::new(AtomicI64::new(0)),
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
        // 开一轮：代数 +1 并**当场**把它记成「在飞」。标记必须在 `spawn`
        // 之前、且在同一把锁里做——否则 `cancel()` 可能落在「`prompt()` 已经
        // 返回、任务还没跑起来」这个窗口上，于是它看见「没有东西在飞」并
        // 拒绝掉一次用户真的点了的停止。
        let turn = conv.turn.clone();
        let inflight = conv.inflight.clone();
        let this_turn = turn.fetch_add(1, Ordering::AcqRel) + 1;
        inflight.store(this_turn, Ordering::Release);
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
            // 这一轮结束（成功、失败、或没有 `stopReason`）：**只在「我还是最新
            // 那一轮」时才清标记**。晚到的那条收尾若把新一代的标记清掉，停止
            // 按钮会在真正该亮的时候是灰的——用户以为「停了」，实际上没有一轮
            // 可以停。
            let _ = inflight.compare_exchange(
                this_turn,
                0, // 0 == 「没有东西在飞」，与 `turn` 从 0 起步的那条不变式一致
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        });
        Ok(())
    }

    /// 中断某条会话**正在跑的那一轮**：发 `session/cancel`，**不关会话**。
    ///
    /// # 三种结局必须分得开（都不许静默成功）
    ///
    /// | 事实 | 返回 |
    /// |---|---|
    /// | 这个 `channel_id` 上没有活会话 | `Err(NoLiveConversation)` |
    /// | 有会话，但没有一轮在飞 | `Err(NothingInFlight)` |
    /// | 中断发出去了 | `Ok(())` |
    ///
    /// 前两条都是「什么都没做」，但用户的下一步完全不同：前者是「这条对话
    /// 还没开（或者说第一句话才会开）」，后者是「它本来就闲着」。合成一个
    /// `Ok` 会让页面在按了停止之后什么都不显示，而用户合理地以为回复被掐断了。
    ///
    /// # 为什么用 `Result<(), CancelError>` 而不是 `Result<(), String>`
    ///
    /// 调用方（`worker::drain_inbound`）要把这两条**分别**回给 Hub，而 Hub 要
    /// 把它们变成两个不同的错误码。字符串在传递途中会被包一层「失败：…」，
    /// 于是「哪一个」就只剩解析消息这一条路——那是最容易被改坏的一种判据。
    pub async fn cancel(&self, channel_id: &str) -> Result<(), CancelError> {
        let live = self.live.lock().await;
        let Some(conv) = live.get(channel_id) else {
            return Err(CancelError::NoLiveConversation);
        };
        // 「有没有一轮在飞」读的是这一对原子：`inflight == 0` 恒等于「闲着」
        // （`prompt()` 只写非零值，收尾只写 0）。先读再发，**不在锁里发**——
        // 写 stdin 是 async 的，把表锁跨在这上面会让同一台设备的其他会话
        // （乃至 `close()`）一起等一次 IO。
        let busy = conv.inflight.load(Ordering::Acquire) != 0;
        let client = conv.client.clone();
        let session_id = conv.session_id.clone();
        drop(live);
        if !busy {
            return Err(CancelError::NothingInFlight);
        }
        client
            .cancel(&session_id)
            .await
            .map_err(|e| CancelError::Acp(e.to_string()))
    }

    /// 这条通道上有没有一轮在飞。Hub 的停止端点用它把「本来就没在跑」与
    /// 「停了」分开回（理由同 [`Conversations::cancel`]）。
    ///
    /// 与 `cancel()` 分开暴露：判据要在**下发中断之前**读，否则读到的已经是
    /// 「被我停掉了」——那样「没有东西可停」这条事实永远观测不到。
    pub async fn turn_in_flight(&self, channel_id: &str) -> Option<bool> {
        let live = self.live.lock().await;
        live.get(channel_id)
            .map(|c| c.inflight.load(Ordering::Acquire) != 0)
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

    /// 当前活着的会话数。
    ///
    /// **生产代码里零调用方**；读它的只有测试：
    /// - `#[ignore]` 的真机端到端用例 `a_real_opencode_turn_reaches_the_channel_as_a_batch`；
    /// - 两条在 CI 里真的跑的单测的**反向验证点**——`the_live_table_never_grows_past_its_cap`
    ///   与 `a_conversation_outlives_a_disconnect_so_it_can_be_resumed` 的注释都写着
    ///   「改坏某处，这句 `len()` 会红」。
    ///
    /// 换句话说它是「计数」而不是「查询」：要断言表里**有哪几条**用 `ids_of`，
    /// 别把这个数当门禁读。
    // 不留 `is_empty()`：它一个调用方都没有（两处 `convs.is_empty()` 是同文件测试里
    // 的断言，已改成 `ids_of(..).is_empty()`），留着就是一段没人读的代码。代价是
    // clippy 的 `len_without_is_empty` 会响——这里成对出现的 `is_empty()` 也只是
    // 测试用的第三个相等物，删掉更诚实。
    #[expect(
        clippy::len_without_is_empty,
        reason = "len() 只被测试读（含 #[ignore] 真机用例），生产零调用方"
    )]
    pub async fn len(&self) -> usize {
        self.live.lock().await.len()
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

/// [`Conversations::cancel`] 的三种结局。**「没这条会话」与「没有一轮在飞」
/// 必须分开**（见那个方法的表格）：两者都是「什么都没做」，但用户的下一步
/// 完全不同，而 Hub 要把它们回成两个不同的错误码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelError {
    /// 这个 `channel_id` 上没有活会话：对话还没开（它是「说第一句话才开」的），
    /// 或者已经被关掉/淘汰了。
    NoLiveConversation,
    /// 会话在，但它现在闲着——没有任何一轮在飞。停下来的东西不存在。
    NothingInFlight,
    /// 通知写出去了但设备侧的 stdin 已经不可写（子进程没了）。
    Acp(String),
}

impl std::fmt::Display for CancelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CancelError::NoLiveConversation => f.write_str("no live conversation"),
            CancelError::NothingInFlight => f.write_str("no turn in flight"),
            CancelError::Acp(detail) => write!(f, "cannot cancel: {detail}"),
        }
    }
}

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
                    // **`id` 随帧一起走**（`requestId`），不是丢掉：ACP 的应答必须
                    // 带着这个 id 回（`AcpClient::respond`），而它是**读循环里才有**
                    // 的东西——出了这一帧就再也拿不到了。留在这里是为了下个里程碑
                    // 能接应答；本里程碑只是**刻意不回**，而不是「没有 id 可回」。
                    //
                    // 它**占用一个序号**：这是设备上真实发生过的事件，跳过它
                    // 会让 `gaps` 在每一处权限请求上报警告——用户会以为丢了东西。
                    //
                    // **先 flush，再发**：`push_external` 是立刻上线的，而前面
                    // 攒在 `batch` 里的事件要等下一次 flush 才走。不先 flush 的话
                    // 线序会变成「seq 2 先到、seq 1 后到」，而 `gaps()` 的判据是
                    // 「seq 断裂处即丢失处」（`trajectory.rs` 对 `seq` 的定义），
                    // 它假定线序与 seq 序一致——顺序一反，断口就被算在错的位置。
                    flush(&channel_id, &mut batch, &mut bytes);
                    last_flush = Instant::now();
                    let n = seq.next();
                    let ev = json!({
                        "type": "request",
                        "seq": n,
                        "requestId": id,
                        "method": method,
                        "params": params,
                        "pending": true,
                    });
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

    /// 起不来时，**错误里要有是哪个 agent**——否则用户只看到「没装」，
    /// 却不知道是哪一个没装（一台设备上通常装了不止一个）。
    ///
    /// 反向验证：把 `format!` 换成固定字符串，`contains("opencode")` 立刻红。
    #[tokio::test]
    async fn opening_without_a_usable_command_names_the_agent() {
        let convs = Conversations::default();
        let spec = ConversationSpec {
            channel_id: "ch-nocmd".to_string(),
            agent_id: "opencode".to_string(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            // 空 = 用扫描到的那个，而这里也没给 default_command。
            command: String::new(),
            args: Vec::new(),
            acp_args: Vec::new(),
            env: BTreeMap::new(),
            acp_session_id: None,
            last_seq: 0,
        };
        let err = convs
            .open(spec, None)
            .await
            .expect_err("没有可执行文件就不该开");
        assert!(err.contains("opencode"), "错误里要指名是哪个 agent：{err}");
        assert!(
            err.contains("not installed") || err.contains("no command"),
            "要说得清是「没装」还是「没配」：{err}"
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

    /// 假 agent：一轮 prompt 里**反手问一句权限请求**（带固定 id `9001`）。
    ///
    /// 与 `recording_agent` 分开写而不是给它加开关：`recording_agent` 被十几条
    /// 用例共用，改它等于同时改动那些用例的前提（多出一条 `request` 事件会让
    /// 断言事件条数的用例红）。这里要的只是「设备侧真的会收到一条
    /// `Event::Request`」这一个事实。
    fn asking_agent(dir: &Path) -> PathBuf {
        let path = dir.join("ask-agent");
        let script = r#"#!/usr/bin/env python3
import json, sys

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
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1,
              "agentCapabilities": {"loadSession": True}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "ses_ask"}})
    elif method == "session/prompt":
        # 请求方是 agent：它要客户端批准一次工具调用，然后**不等**就结束这一轮
        # （`session/prompt` 的回执照发）。设备侧因此只会看到一条 request。
        send({"jsonrpc": "2.0", "id": 9001, "method": "session/request_permission",
              "params": {"sessionId": "ses_ask", "toolCall": {"title": "rm -rf"}}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
"#;
        std::fs::write(&path, script).expect("写脚本");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }

    /// 一个先推 **sessionUpdate**、再推 **权限请求** 的假 agent。
    ///
    /// 造这个形状是为了证明「线序 == seq 序」：先到的那条进 `batch` 等着，
    /// 后到的 request 若直接 `push_external` 就会**抢在它前面上线**。
    fn update_then_request_agent(dir: &Path) -> PathBuf {
        let path = dir.join("update-then-ask");
        let script = r#"#!/usr/bin/env python3
import json, sys

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
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1,
              "agentCapabilities": {"loadSession": True}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "ses_uq"}})
    elif method == "session/prompt":
        # 先一条普通更新（会被攒进 batch），再一条权限请求（立刻外推）。
        send({"jsonrpc": "2.0", "method": "session/update",
              "params": {"sessionId": "ses_uq",
                         "update": {"sessionUpdate": "agent_message_chunk",
                                    "content": [{"type": "text", "text": "thinking"}]}}})
        send({"jsonrpc": "2.0", "id": 9001, "method": "session/request_permission",
              "params": {"sessionId": "ses_uq", "toolCall": {"title": "rm -rf"}}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
"#;
        std::fs::write(&path, script).expect("写脚本");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
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

    // ---- 停止一轮（`session/cancel`）----

    /// 一个**会拖住 `session/prompt` 直到收到 cancel** 的假 agent。
    ///
    /// 为什么要拖住：`prompt()` 断言的是「这一轮结束了没有」，而假 agent 秒回的
    /// 话，`inflight` 在测试读它之前就已经被清掉了——于是「有东西在飞」这条
    /// 永远观测不到，停止按钮在真正该亮的时候看起来是灰的。真机上「一轮在飞」
    /// 可以持续几分钟，这里用一个**不会自己结束**的轮次把它变成确定的事实。
    ///
    /// `session/cancel` 之后它回一条带 `stopReason: "cancelled"` 的响应——这正是
    /// 真 agent 的行为（中断的那一轮照样有 `stopReason`），于是 `turn_end` 的
    /// 「本轮结束」判据不需要为 cancel 另开一条路。
    fn slow_agent(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("methods.log");
        let path = dir.join("slow-agent");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
LOG = {log:?}
HELD = None

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
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"sessionId": "ses_slow"}}}})
    elif method == "session/prompt":
        # **故意不回**：这一轮要一直挂着，直到 cancel 到来。
        HELD = mid
    elif method == "session/cancel":
        if HELD is not None:
            send({{"jsonrpc": "2.0", "id": HELD, "result": {{"stopReason": "cancelled"}}}})
            HELD = None
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

    /// **没有这条会话 → `NoLiveConversation`；有会话但闲着 → `NothingInFlight`。**
    ///
    /// 两条都是「什么都没做」，但**不许合成一个 `Ok`**：页面在按了停止之后
    /// 什么都不显示时，用户会合理地以为回复被掐断了，而实际上那一轮还在跑。
    /// 这正是本用例的第一句与最后一句要分开的原因。
    ///
    /// 反向验证：把 `cancel` 里那句 `if !busy { return Err(NothingInFlight) }`
    /// 删掉，最后一句会红在「闲着时必须报错」。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn cancelling_says_which_kind_of_nothing_happened() {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, _log) = slow_agent(d.path());
        let convs = Conversations::default();

        // 1. 会话都还没有：不是「闲着」，是「没这条对话」。
        assert_eq!(
            convs.cancel("ch_never_opened").await,
            Err(CancelError::NoLiveConversation),
            "没开过的通道要如实报「没有活会话」，不许静默成功"
        );
        assert_eq!(convs.turn_in_flight("ch_never_opened").await, None);

        // 1b. 往一条没有的对话发消息：要**当场**报错，不许 spawn 完就装作发了。
        //     静默成功比崩溃更糟——页面上那条消息会一直停在「已发送」。
        let err = convs
            .prompt("ch_never_opened", "有人吗")
            .await
            .expect_err("没有活会话就不该接受消息");
        assert!(
            err.contains("ch_never_opened"),
            "错误里要带上是哪条通道：{err}"
        );

        // 2. 会话在，但一轮都没跑过。
        let _ = channel::open_external("ch_idle");
        convs
            .open(spec_for("ch_idle", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        assert_eq!(
            convs.turn_in_flight("ch_idle").await,
            Some(false),
            "刚开的会话没有一轮在飞"
        );
        assert_eq!(
            convs.cancel("ch_idle").await,
            Err(CancelError::NothingInFlight),
            "闲着的时候按停止：要报「没有一轮可以停」，不许假装停了"
        );

        convs.close("ch_idle").await;
    }

    /// `CancelError` 的三句话**是给人看的**，各有各的意思，不能合并成一句。
    ///
    /// 「没有活会话」与「没有一轮在飞」在日志里必须分得开：前者说明用户点的
    /// 那条对话已经死了（关掉/被淘汰），后者说明它好好活着、只是闲着。把两者
    /// 说成同一句「取消失败」，排障时就看不出该往哪查。
    #[test]
    fn each_cancel_error_says_a_different_thing() {
        let dead = CancelError::NoLiveConversation.to_string();
        let idle = CancelError::NothingInFlight.to_string();
        let broken = CancelError::Acp("stdin 已关".to_string()).to_string();
        assert_eq!(dead, "no live conversation");
        assert_eq!(idle, "no turn in flight");
        assert_eq!(broken, "cannot cancel: stdin 已关", "要带上底层原因");
        assert_ne!(dead, idle, "「没这条对话」与「它闲着」不是一回事");
    }

    /// **一轮在飞 → cancel 发得出去，且只有这一轮被中断，会话照旧活着。**
    ///
    /// 这条钉的是「停止」这件事的完整语义，四件事一起验：
    /// 1. `session/cancel` **真的到了 agent 那一侧**（查 agent 的 method 日志）；
    /// 2. 被中断的那一轮照样有 `turn_end`（`stopReason: cancelled`）——页面靠它
    ///    把「正在回复」收尾，否则会一直转；
    /// 3. cancel 之后 `turn_in_flight` 回到 `false`；
    /// 4. **会话还在**：`cancel()` 之后还能再 `prompt` 一轮。停止不等于关闭，
    ///    把 cancel 写成「顺手 close」会让一次停止毁掉整条对话的上下文。
    ///
    /// 反向验证：把 `Conversations::cancel` 改成 `self.close(channel_id).await;
    /// Ok(())`，第 4 条会红在「会话还在」——那正是最坏的一种实现（页面看起来
    /// 停住了，回到对话却是一张白纸）。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn cancelling_interrupts_the_running_turn_without_closing_the_conversation() {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let (bin, log) = slow_agent(d.path());
        let convs = Conversations::default();

        let _ = channel::open_external("ch_cancel");
        convs
            .open(spec_for("ch_cancel", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        convs
            .prompt("ch_cancel", "这一句要被中断")
            .await
            .expect("prompt 必须发得出去");

        // 等「有一轮在飞」成为可观测的事实。`prompt()` 的标记是在 spawn
        // **之前**、在锁里打的（见 `prompt` 的注释：否则 cancel() 可能落在
        // 「已返回、任务还没跑起来」的窗口里并拒绝一次真实的停止）。所以
        // `turn_in_flight == true` 不代表 agent 已经收到 `session/prompt`——下一句
        // 才等那个时点，否则 cancel 可能比 prompt 先到达 stdin，agent 就会
        // 收到一个 `session/cancel` 而后面那个 `session/prompt` 永远不被响应。
        let mut busy = false;
        for _ in 0..200 {
            if convs.turn_in_flight("ch_cancel").await == Some(true) {
                busy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(busy, "prompt 之后必须有一轮在飞，否则停止按钮永远亮不起来");

        // 再等 agent 真的收到了 `session/prompt`：cancel 必须排在 prompt
        // 之后到达 stdin，否则 agent 把「先收到的 cancel」当空挡忽略，prompt
        // 永远不被响应，turn_end 永远不会有。
        for _ in 0..200 {
            if methods_in(&log).iter().any(|m| m == "session/prompt") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            methods_in(&log).iter().any(|m| m == "session/prompt"),
            "agent 必须先收到 session/prompt，实际: {:?}",
            methods_in(&log)
        );

        convs
            .cancel("ch_cancel")
            .await
            .expect("有一轮在飞时 cancel 必须发得出去");

        // 1. cancel 真的到了 agent 那一侧
        let mut saw_cancel = false;
        for _ in 0..200 {
            if methods_in(&log).iter().any(|m| m == "session/cancel") {
                saw_cancel = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            saw_cancel,
            "session/cancel 必须发到 agent，实际收到的方法: {:?}",
            methods_in(&log)
        );

        // 2/3. 被中断的那一轮有 turn_end（stopReason 是 agent 给的），
        //      并且标记回到「闲着」。
        let mut turn_end = None;
        for _ in 0..200 {
            for (ch, frames) in channel::pump() {
                if ch != "ch_cancel" {
                    continue;
                }
                for fr in frames {
                    let Ok(data) = fr.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    if data.payload.get("type").and_then(Value::as_str) == Some("turn_end") {
                        turn_end = Some(data.payload.clone());
                    }
                }
            }
            if turn_end.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let payload =
            turn_end.expect("被中断的那一轮也要有 turn_end，否则页面一直显示「正在回复」");
        assert_eq!(
            payload.get("stopReason").and_then(Value::as_str),
            Some("cancelled"),
            "turn_end 的 stopReason 来自 agent，不该由设备编一个"
        );
        assert_eq!(
            convs.turn_in_flight("ch_cancel").await,
            Some(false),
            "一轮收尾之后必须回到「闲着」"
        );

        // 4. 会话还在：还能再发一轮。停止**不是**关闭。
        assert_eq!(convs.len().await, 1, "cancel 不许把关会话当成副作用");
        convs
            .prompt("ch_cancel", "停了还能接着说")
            .await
            .expect("cancel 之后这条会话必须还能用——停止不等于关闭");
        assert_eq!(
            convs.turn_in_flight("ch_cancel").await,
            Some(true),
            "新一轮开始了，就又是一轮在飞"
        );

        convs.close("ch_cancel").await;
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

    /// 表被别的臂占着时，`mark_orphaned` **这一轮不标、返回 0**，也不卡住。
    ///
    /// 这不是「不重要」的分支：调用点在同步的断连收尾里，等异步锁会把整条
    /// 连接收尾挂住。宁可漏标一轮（下次断连还会标），也不能让收尾卡死。
    /// 反向验证：把 `try_lock` 的失败臂改成 `unwrap_or_else(|_| panic!(..))`
    /// 或改成 `lock().await`（那就不是同步函数了），本用例都会立刻变红。
    #[tokio::test]
    async fn mark_orphaned_skips_a_busy_table_instead_of_blocking() {
        let convs = Conversations::default();
        // 拿着表不放：这正是「别的臂正持着表」的形态。
        let held = convs.live.lock().await;
        assert_eq!(
            convs.mark_orphaned(),
            0,
            "表被占着时只能如实报「这一轮没标」"
        );
        drop(held);
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
        assert!(ids_of(&convs).await.is_empty());
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
        assert!(ids_of(&convs).await.is_empty());
    }

    /// **权限请求的 `id` 必须随帧一起出去**（`requestId`），本里程碑只是不回它。
    ///
    /// 挡的是「看得见但永远答不了」：`Event::Request` 的 `id` 只在读循环里存在，
    /// 出了这一帧就再也拿不到。早先那句 `let _ = id;` 把它显式丢掉，于是**即便**
    /// 下个里程碑要做应答，事件流里也没有可用于回传的 id——只能重造一个，而重造的
    /// id 与 agent 等的那一条对不上。
    ///
    /// 反向验证：把 `"requestId": id` 从 `json!` 里删掉（或恢复 `let _ = id;`），
    /// 这条会红在「帧上必须带 original id」。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_permission_request_reaches_the_page_with_its_id() {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let bin = asking_agent(d.path());
        let convs = Conversations::default();

        let _ = channel::open_external("ch_ask");
        convs
            .open(spec_for("ch_ask", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        // 先清掉 `ready`，免得下面把那条也当成 request。
        let _ = channel::pump();

        convs.prompt("ch_ask", "do it").await.expect("会话在");

        // request 与 turn_end 都是异步推出来的：给到 5s，而不是赌一个时序。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut req: Option<Value> = None;
        while Instant::now() < deadline && req.is_none() {
            for (_, frames) in channel::pump() {
                for f in frames {
                    let Ok(cd) = f.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    if cd.payload.get("type").and_then(Value::as_str) == Some("request") {
                        req = Some(cd.payload);
                    }
                }
            }
            if req.is_none() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        let req = req.expect("权限请求必须推到页面上（否则页面看着像 agent 卡住了）");

        assert_eq!(req["method"], json!("session/request_permission"));
        assert_eq!(
            req["pending"],
            json!(true),
            "本里程碑不回，页面要能画出「等你点头」"
        );
        assert_eq!(
            req["requestId"],
            json!(9001),
            "agent 那条请求的 id 必须原样带出来——应答靠它配对，`let _ = id;` \
             丢掉之后就没有第二条路了：{req}"
        );
        assert!(
            req.get("seq").and_then(Value::as_i64).is_some(),
            "它仍要占一个序号，否则 `gaps` 会在每处权限请求上报警告: {req}"
        );

        convs.close("ch_ask").await;
    }

    /// **「线序 == seq 序」：权限请求不能抢在已攒进 `batch` 的事件前面上线。**
    ///
    /// `sessionUpdate` 先到、被攒进 `batch` 等下一次 flush；`Request` 后到，
    /// 但它走的是 `push_external`（立刻上线）。修前那一段**没有先 flush**，
    /// 于是 seq 1 还没发、seq 2 已经发了——线上顺序变成 2, 1。
    ///
    /// 这条为什么重要：`gaps()` 的判据是「`seq` 断裂处即丢失处」
    /// （`trajectory.rs` 对 `seq` 的定义），它默认「先到的 seq 也先到 Hub」。
    /// 顺序一反，断口就算在错的位置，页面会指着一段其实没丢的区间说「这里丢了」。
    ///
    /// 判据必须是**收到帧的顺序**（不是「都收到了」）：两种写法都能收齐，
    /// 只有顺序能区分。反向验证：去掉 `Request` 分支里那句 `flush(...)`
    /// → 这里实测得到 `[2, 1, ...]`，断言当场红。
    #[tokio::test]
    // guard 跨 `await` 存活是本用例要的语义：互斥必须覆盖整个用例体。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn a_request_never_overtakes_the_events_already_in_the_batch() {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        let d = tempfile::tempdir().expect("tmp");
        let bin = update_then_request_agent(d.path());
        let convs = Conversations::default();

        let _ = channel::open_external("ch_uq");
        convs
            .open(spec_for("ch_uq", &bin, None), None)
            .await
            .expect("假 agent 必须能起来");
        // 先清掉 `ready`，免得它混进下面收的帧里。
        let _ = channel::pump();

        convs.prompt("ch_uq", "do it").await.expect("会话在");

        // 收集「带 seq 的帧」的上线顺序。
        //
        // **不能一到 `turn_end` 就停**：`batch` 是按 60ms 的时间窗发的
        // （`BATCH_INTERVAL`），而 `turn_end` 与 request 是立刻外推的——
        // 先到的是它们。要等到那批 `batch` 真的上线，否则看不到 seq 1，
        // 断言就成了「只比较剩下的两条」而不自知（这正是本条第一版的错法）。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen: Vec<(i64, String)> = Vec::new();
        let mut ended = false;
        while Instant::now() < deadline && !(ended && seen.iter().any(|(_, k)| k == "batch")) {
            for (_, frames) in channel::pump() {
                for f in frames {
                    let Ok(cd) = f.payload::<protocol::channel::ChannelData>() else {
                        continue;
                    };
                    let p = &cd.payload;
                    if p.get("type").and_then(Value::as_str) == Some("turn_end") {
                        ended = true;
                    }
                    // `batch` 里的每一条与 `request` 都带 seq。
                    if let Some(n) = p.get("seq").and_then(Value::as_i64) {
                        let kind = p
                            .get("type")
                            .or_else(|| p.get("sessionUpdate"))
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        seen.push((n, kind));
                    }
                    // 一批里可能有多条，`batch` 帧的 `events` 才是载荷。
                    if p.get("type").and_then(Value::as_str) == Some("batch") {
                        seen.push((-1, "batch".to_string()));
                    }
                    if let Some(evs) = p.get("events").and_then(Value::as_array) {
                        for e in evs {
                            if let Some(n) = e.get("seq").and_then(Value::as_i64) {
                                let kind = e
                                    .get("sessionUpdate")
                                    .and_then(Value::as_str)
                                    .unwrap_or("?")
                                    .to_string();
                                seen.push((n, kind));
                            }
                        }
                    }
                }
            }
            // **每轮都必须 await**：`read_events` 是 `tokio::spawn` 出来的任务，
            // 单线程运行时下这个紧凑循环如果不再让出，就会把它整个饿死——
            // `batch` 的 60ms 窗口一次也不会走，于是「收不到 batch」被误读成
            // 「实现没 flush」。这不是超时设置的问题，是测试自己不让别人跑。
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        convs.close("ch_uq").await;

        let seqs: Vec<i64> = seen.iter().map(|(n, _)| *n).filter(|n| *n >= 0).collect();
        assert!(
            seqs.len() >= 2,
            "这一轮至少要收到「更新」与「请求」两条带 seq 的帧，实收 {seqs:?}"
        );
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(
            seqs, sorted,
            "上线顺序必须与 seq 序一致（seq 断裂处即丢失处），实收 {seen:?}"
        );
    }

    // ---- `read_events` 的四种事件形态 ----
    //
    // 这些用例直接驱动读循环（传一个自己造的 `AcpClient`、一个自己造的
    // `Seq`），而不是走 `open()`——`open()` 会顺带建会话、建 catalog，
    // 那些不是这里要钉的东西，混进去只会让失败点变得模糊。

    /// 握手后**主动吐 `payload` 再退出**的假 agent。
    ///
    /// 主动退出是本组测试的终止信号：`read_events` 收到 `Event::Exited` 就
    /// 结束。**不是靠 `recv()` 返回 `None`**——`AcpClient` 内部自己持着事件
    /// 发送端的一个克隆（见 `acp.rs` 的 `spawn`），那个克隆活到客户端被丢弃，
    /// 所以 `recv()` 永远等不到 `None`，`read_events` 里对应的那一臂因此
    /// 结构上不可达（已登记）。
    fn scripted_agent(dir: &Path, payload: &str) -> PathBuf {
        let path = dir.join("scripted-agent");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys

def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()

def notify(method, params):
    send({{"jsonrpc": "2.0", "method": method, "params": params}})

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    if m.get("method") == "initialize":
        send({{"jsonrpc": "2.0", "id": m.get("id"), "result": {{"protocolVersion": 1,
              "agentCapabilities": {{"loadSession": True}}}}}})
        break

{payload}

"#
        );
        std::fs::write(&path, script).expect("写脚本");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }

    /// 把 `read_events` 跑起来，取回它推进通道的全部帧载荷。
    ///
    /// 通道表是进程级单例，`_guard` 必须活到用例结束，于是它跨 `await`
    /// 存活——这正是本组用例要的语义（互斥覆盖整个用例体），
    /// 与 `channel.rs` 里那几处的写法一致。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn frames_from_read_events(ch: &str, payload: &str, last_seq: i64) -> Vec<Value> {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        channel::open_external(ch);

        let d = tempfile::tempdir().expect("tmp");
        let bin = scripted_agent(d.path(), payload);
        let client = AcpClient::spawn(
            bin.to_str().expect("路径"),
            &[],
            &BTreeMap::new(),
            "",
            Some(d.path()),
        )
        .await
        .expect("握手应成功");
        let seq = Seq::new(last_seq);
        let reader = tokio::spawn(read_events(ch.to_string(), client, seq));

        // 60ms 的批窗口 + 假 agent 的退出，给足预算；但**每轮都要 await**
        // 一次 sleep，否则单线程运行时下 `read_events` 任务会被饿死。
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if reader.is_finished() {
                break;
            }
        }
        let _ = reader.await;
        // 读循环推的是**外发**帧：它们入队等信用放行，`channel::pump()`
        // 才是取出它们的那条路（`take_inbound` 拿的是 Hub 发下来的消息，
        // 方向相反）。外层是帧信封（`channel_data{channel_id, payload}`），
        // 里层的 `payload` 才是读循环写的那个 JSON。
        channel::pump()
            .into_iter()
            .filter(|(id, _)| id == ch)
            .flat_map(|(_, frames)| frames)
            .filter_map(|f| f.payload::<protocol::channel::ChannelData>().ok())
            .map(|d| d.payload)
            .collect()
    }

    /// 与 [`frames_from_read_events`] 同路，但**在 agent 还活着的时候就取帧**。
    ///
    /// 那个版本会等读循环自己结束才取，于是只能在 agent 退出的用例里用。而
    /// 「定时臂把攒着的那批推出去」恰恰要求 agent **不退出**：一退出，
    /// `Exited` 那条路径也会 flush，就分不清到底是谁推的。
    ///
    /// 所以这里给读循环一段固定预算（远大于 `BATCH_INTERVAL`），到点就取帧、
    /// 然后 `abort` 读任务。每轮都 `await` 一次 sleep：单线程运行时下紧凑
    /// 循环会把读任务整个饿死。
    #[expect(
        clippy::await_holding_lock,
        reason = "通道表是进程级单例，整个用例体必须独占它"
    )]
    async fn frames_from_read_events_while_alive(
        ch: &str,
        payload: &str,
        last_seq: i64,
    ) -> Vec<Value> {
        let _guard = channel::table_serial_for_tests();
        channel::reset_for_tests();
        channel::open_external(ch);

        let d = tempfile::tempdir().expect("tmp");
        let bin = scripted_agent(d.path(), payload);
        let client = AcpClient::spawn(
            bin.to_str().expect("路径"),
            &[],
            &BTreeMap::new(),
            "",
            Some(d.path()),
        )
        .await
        .expect("握手应成功");
        let seq = Seq::new(last_seq);
        let reader = tokio::spawn(read_events(ch.to_string(), client, seq));

        // 4 × 60 ms 的定时窗口，足够定时臂至少走两轮。
        for _ in 0..4 {
            tokio::time::sleep(BATCH_INTERVAL).await;
        }
        let frames = channel::pump()
            .into_iter()
            .filter(|(id, _)| id == ch)
            .flat_map(|(_, frames)| frames)
            .filter_map(|f| f.payload::<protocol::channel::ChannelData>().ok())
            .map(|d| d.payload)
            .collect();
        reader.abort();
        let _ = reader.await;
        frames
    }

    /// `session/update` 攒成一批：**一条 `batch` 里带着它的 `seq`**。
    #[tokio::test]
    async fn read_events_batches_updates_and_numbers_them() {
        let payload = r#"
notify("session/update", {"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}})
"#;
        let frames = frames_from_read_events("ch_re_1", payload, 0).await;
        let batch = frames
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("batch"))
            .expect("应当有一批更新");
        let events = batch
            .get("events")
            .and_then(Value::as_array)
            .expect("events");
        assert_eq!(events.len(), 1, "一条更新就是一批里的一个事件");
        assert_eq!(
            events
                .first()
                .and_then(|e| e.get("seq"))
                .and_then(Value::as_i64),
            Some(1),
            "起点 0 → 第一条 seq 是 1"
        );
    }

    /// 一条更新**超过 `BATCH_BYTES`** 时立刻 flush，不等 60 ms 的空闲定时器。
    ///
    /// 攒批是为了减少帧数，不是为了延迟：一条 8 KiB 的更新若还等着「下一批
    /// 再来一起发」，用户会看见一段停顿——而停顿的长度取决于 agent 多久再吐
    /// 一个字，那最坏是无限久。反向验证：去掉 `if bytes >= BATCH_BYTES` 那一臂，
    /// 这条用例仍会过（定时器最终也会 flush），所以断言取在**帧的顺序**上：
    /// 大更新必须在**同一次 pump 之前**就已经成批，而不是靠后面的定时器补。
    #[tokio::test]
    async fn a_large_update_is_flushed_without_waiting_for_the_idle_timer() {
        // 一条远大于 4 KiB 的 update：'x' 重复 8000 次。
        let big = "x".repeat(8000);
        let payload = format!(
            r#"
notify("session/update", {{"sessionId":"s","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{big}"}}}}}})
notify("session/update", {{"sessionId":"s","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"after"}}}}}})
"#
        );
        let frames = frames_from_read_events("ch_re_big", &payload, 0).await;
        let batches: Vec<_> = frames
            .iter()
            .filter(|f| f.get("type").and_then(Value::as_str) == Some("batch"))
            .collect();
        assert!(
            batches.len() >= 2,
            "超过 BATCH_BYTES 就该当场落一批，而不是和后面那条合成一批；实收 {} 批：{frames:?}",
            batches.len()
        );
        // 第一批必须只装着那条大的，它是体积触发的那一批。
        let first_len = batches[0]
            .get("events")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        assert_eq!(
            first_len, 1,
            "体积触发的那一批应该只有它自己：{first_len} 条"
        );
    }

    /// 空闲时也必须发得出去：**一条小更新靠 60 ms 的定时器落批**。
    ///
    /// 攒批是为了少发帧，不是为了延迟。只看「事件到达」是不够的——最后
    /// 一句话永远在等下一个事件，而 agent 可能再也不说下一句。没有这条
    /// 定时臂，页面会一直停在「agent 正在想」。
    ///
    /// 所以假 agent 吐一条**小**更新（远不到 `BATCH_BYTES`）之后**不退出**、
    /// 只是不再说话：把「体积触发的 flush」与「握手的 flush」都排除掉，
    /// 能把它发出去的只剩定时臂。
    ///
    /// 反向验证：把 `_ = tick.tick()` 那一臂的 `flush` 去掉，这里收不到任何 `batch`。
    #[tokio::test]
    async fn read_events_flushes_a_lone_update_on_the_idle_timer() {
        // 脚本：吐一条更新，然后**挂着读 stdin**（既不说话也不退出）。
        let payload = r#"
notify("session/update", {"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"solo"}}})
for _ in sys.stdin:
    pass
"#;
        let frames = frames_from_read_events_while_alive("ch_re_tick", payload, 0).await;
        let batches: Vec<_> = frames
            .iter()
            .filter(|f| f.get("type").and_then(Value::as_str) == Some("batch"))
            .collect();
        assert_eq!(
            batches.len(),
            1,
            "那条小更新必须靠定时臂落批（agent 一直不说话、也不退出）：{frames:?}"
        );
        let events = batches[0]
            .get("events")
            .and_then(Value::as_array)
            .expect("events");
        assert_eq!(events.len(), 1, "这一批里就它一条");
        assert_eq!(
            events[0]
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str),
            Some("solo"),
            "载荷原样，不得改写"
        );
    }

    /// `seq` 从 `last_seq` 续上，不是从 1 重数。
    ///
    /// 从 1 重数的后果是**静默丢事件**：续话时重放的事件带 `seq=1`，与库里
    /// 已有的 `seq=1` 撞 `UNIQUE`，被 `INSERT OR IGNORE` 吞掉。
    #[tokio::test]
    async fn read_events_continues_numbering_from_last_seq() {
        let payload = r#"
notify("session/update", {"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}})
"#;
        let frames = frames_from_read_events("ch_re_2", payload, 41).await;
        let seq = frames
            .iter()
            .filter_map(|f| f.get("events"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|e| e.get("seq").and_then(Value::as_i64))
            .next()
            .expect("应当有一条带 seq 的事件");
        assert_eq!(seq, 42, "last_seq=41 → 下一条是 42");
    }

    /// 权限请求：**先 flush 攒着的那批，再发请求**，且请求占一个序号。
    #[tokio::test]
    async fn read_events_flushes_the_batch_before_a_permission_request() {
        let payload = r#"
notify("session/update", {"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"before"}}})
send({"jsonrpc":"2.0","id":"req_1","method":"session/request_permission","params":{"sessionId":"s"}})
"#;
        let frames = frames_from_read_events("ch_re_3", payload, 0).await;
        let types: Vec<&str> = frames
            .iter()
            .filter_map(|f| f.get("type").and_then(Value::as_str))
            .collect();
        let batch_at = types.iter().position(|t| *t == "batch");
        let req_at = types.iter().position(|t| *t == "request");
        assert!(
            batch_at.is_some() && req_at.is_some(),
            "既要有 batch 也要有 request，实收 {types:?}"
        );
        assert!(
            batch_at.expect("batch") < req_at.expect("request"),
            "batch 必须先上线：否则 seq 1 会排到 seq 2 后面，断口被算在错的位置。实收 {types:?}"
        );
        let req = frames
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("request"))
            .expect("request");
        assert_eq!(
            req.get("requestId").and_then(Value::as_str),
            Some("req_1"),
            "requestId 必须随帧走：出了读循环就再也拿不到，下个里程碑没法回"
        );
        assert_eq!(req.get("pending").and_then(Value::as_bool), Some(true));
        assert!(
            req.get("seq").and_then(Value::as_i64).is_some(),
            "权限请求也要占一个序号，否则 gaps 会在每处请求上报警告"
        );
    }

    /// 子进程退出：**先 flush 再发 `exited`**，且 `exited` 不占序号。
    #[tokio::test]
    async fn read_events_flushes_before_reporting_exit_without_a_seq() {
        let payload = r#"
notify("session/update", {"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"last words"}}})
send({"jsonrpc":"2.0","id":"req_9","method":"session/request_permission","params":{"sessionId":"s"}})
"#;
        let frames = frames_from_read_events("ch_re_4", payload, 0).await;
        let types: Vec<&str> = frames
            .iter()
            .filter_map(|f| f.get("type").and_then(Value::as_str))
            .collect();
        let exited = frames
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("exited"));
        assert!(
            exited.is_some(),
            "agent 退出必须报一条 exited，实收 {types:?}"
        );
        let exited = exited.expect("exited");
        assert!(
            exited.get("seq").is_none(),
            "exited 不是对话事件，给它序号会让重连时序号从错位开始（实收 seq={:?}）",
            exited.get("seq")
        );
        assert!(
            types.iter().position(|t| *t == "exited").expect("exited") == types.len() - 1,
            "exited 必须是最后一条：先 flush 再报退出，否则最后半句话丢掉。实收 {types:?}"
        );
    }
}
