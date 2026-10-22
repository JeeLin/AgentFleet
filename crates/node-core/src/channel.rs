//! 通道帧处理：node 侧的 `open_channel` / `channel_data` / `close_channel`。
//!
//! M0 的 worker 只处理心跳，收到通道帧**没有任何分支**——本模块把这条
//! 接通。它只做「一帧进来 → 该回什么」，不碰 socket、不碰 tokio：
//! 帧从 [`crate::worker::Worker`] 的读循环来、出去由它发，
//! 于是通道语义可以不起连接就测。
//!
//! **Hub 下发 `open_channel`**（§5）：node 找到 connector → 建资源 → 回
//! `channel_ready`（协议里**没有** `channel_opened` 这个消息类型，
//! 见 `protocol.md` §4 与 `crates/protocol/src/message.rs`）。
//! 名字写错不会立刻断连——约束 14 说未知类型被忽略——于是 Hub 一直等不到
//! ready，5s 后回 `channel_error{reason:"unsupported"}`，症状是「终端卡了
//! 5 秒然后说不支持」，与根因相距很远。
//!
//! **通道键是 `(node_id, channel_id)`**（约束 4）：本进程只服务自己这一台
//! 设备，所以表里只有 `channel_id`；`node_id` 那层是 Hub 侧
//! （`hub-core::channels`）。这里绝不能出现全局 `channel_id` 的概念。
//!
//! ## 这张表**不是** `protocol::ChannelRegistry`
//!
//! 协议 crate 里那份 `ChannelRegistry` 是**Hub 侧模型的副本**，而 Hub 真正
//! 跑的是 `hub-core::channels`；node 侧跑的则是本文件的 `ChannelTable`——
//! 信用、积压、放行各有一份实现。**三份不共享任何状态**，改其中一份不会
//! 改到另一份。协议文档里「两侧共用同一张账本」那句话描述的是 Hub 侧，
//! 按它去改协议那份，改不到正在跑的这份。

use std::collections::{BTreeMap, VecDeque};

use protocol::channel::{
    reason as channel_reason, ChannelData, ChannelErrorCode, ChannelErrorPayload, ChannelReady,
    CloseChannel, OpenChannel,
};
use protocol::frame::Frame;
use protocol::message::types;
pub use protocol::payload_or as frame_payload_or;

use crate::connector::{self, BoxedConnector, ConnectorError, OpenReq};

/// 待发数据的**字节**上限。
///
/// 信用保证的是「对端还在收」，不是「对端一定收」：一个不再补信用的对端
/// 会让待发队列无限长。超过上限**不静默丢字节**（那等于让终端输出凭空
/// 消失一段），而是释放资源并回 `channel_error`——用户看到的是「输出过快，
/// 已断开」而不是一段缺了中间的命令。
const MAX_BACKLOG_BYTES: usize = 8 * 1024 * 1024;

/// 处理一帧通道帧的结果。
#[derive(Debug, Clone)]
pub enum Action {
    /// 什么都不回（数据已被吸收、credit 不足在挂起、或这帧不该有应答）。
    None,
    /// 回若干帧（`Vec` 可能为空——空就是 [`Action::None`] 的意思，
    /// 调用方直接遍历即可，不必特判）。
    Send(Vec<Frame>),
    /// 连接整体该结束了（载荷解不开：这不是「没听懂」，是协议分叉）。
    End,
}

/// 本设备当前打开的通道。
///
/// 用 `BTreeMap` 而不是 `HashMap`：**没有按名的顺序**，遍历就是 `channel_id`
/// 字典序，一条洪峰会以完全相同的次序推给 Hub，联调时的 diff 才读得懂。
#[derive(Default)]
pub struct ChannelTable {
    channels: BTreeMap<String, Entry>,
}

/// 手写 `Debug`：表里装着 connector 实例（trait object），它不实现
/// `Debug`。这里只打「有哪些通道、各是什么状态」——那才是排障要看的。
impl std::fmt::Debug for ChannelTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelTable")
            .field("len", &self.channels.len())
            .field("live", &self.live_count())
            .field("sent_frames", &self.sent_frames())
            .field("channels", &self.summary())
            .finish()
    }
}

struct Entry {
    /// 通道的 connector 名（用于日志与排障）。
    connector: String,
    /// 通道的 connector 实例：**持有本地资源**（PTY / 工作区句柄）。
    /// 表一清，资源就跟着走（`close` 与断连路径都先显式 `close()`）。
    resource: BoxedConnector,
    credit: u32,
    /// `false` 表示资源已释放（如 PTY 退出、通道被关）。仍然留在表里，
    /// 是为了让随后那帧 `close_channel` 还能对上号（§5：关一条通道不得
    /// 影响其他通道）。
    live: bool,
    /// 待发队列：connector 一次产出的帧多于信用额度，剩下的排在这里等
    /// `channel_credit`。**PTY 输出不丢字节**是产品承诺（里程碑子任务 8），
    /// 所以这里只能排队，不能截断。
    backlog: VecDeque<Frame>,
    backlog_bytes: usize,
    /// 本次连接上已推给 Hub 的数据帧数。**只用于自证流控真的在节流**，
    /// 不参与任何判定——它一旦参与判定就会变成「按帧数收租」。
    sent: u64,
}

/// 开通道失败。**「这台设备没有这个能力」与「这个 connector 没能开起来」
/// 是两件事**：前者回 `unsupported`（§5），后者回它自己的码与理由。
/// 混成同一档会让人以为「终端不支持」，而真相是「工作区根没了」。
#[derive(Debug)]
enum OpenFailure {
    Unsupported,
    Connector(ConnectorError),
}

impl OpenFailure {
    fn code(&self) -> ChannelErrorCode {
        match self {
            Self::Unsupported => ChannelErrorCode::Unsupported,
            Self::Connector(e) => e.code(),
        }
    }

    fn reason(&self) -> String {
        match self {
            Self::Unsupported => channel_reason::UNSUPPORTED.to_string(),
            Self::Connector(e) => e.to_string(),
        }
    }
}

/// 数据面的错误。
///
/// 只有两档，因为 **§6 的「信用归零即挂起」不落在这里**：挂起的实现是
/// 「帧排进待发队列、`channel_credit` 到达时再放行」，于是「信用用完」
/// 这条路上根本不产生错误——[`ChannelTable::flush`] 返回空列表，
/// 调用方什么都不发即可。回一帧 `channel_error` 反而会让 Hub 以为
/// 这条通道死了，而它只是等信用。
#[derive(Debug)]
enum DataError {
    /// 通道不存在 / 已关：**静默**。
    ///
    /// 不回错是有意的：Hub 在重连后会重发 `close_channel`，而它那边可能
    /// 还留着旧连接记的账；对不上号时唯一有意义的动作是「清掉」。
    UnknownChannel,
    Connector(ConnectorError),
}

impl DataError {
    fn reason(&self) -> Option<(ChannelErrorCode, String)> {
        match self {
            Self::UnknownChannel => None,
            Self::Connector(e) => Some((e.code(), e.to_string())),
        }
    }
}

impl ChannelTable {
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// 资源仍然活着的通道数。
    pub fn live_count(&self) -> usize {
        self.channels.values().filter(|e| e.live).count()
    }

    /// 累计推给 Hub 的 `channel_data` 帧数（跨连接累计）。
    pub fn sent_frames(&self) -> u64 {
        self.channels.values().map(|e| e.sent).sum()
    }

    /// 表里通道的概览（排障用；`Debug` 也走它）。
    pub fn summary(&self) -> Vec<(String, String, bool, u32)> {
        self.channels
            .iter()
            .map(|(id, e)| (id.clone(), e.connector.clone(), e.live, e.credit))
            .collect()
    }

    fn has(&self, channel_id: &str) -> bool {
        self.channels.contains_key(channel_id)
    }

    /// 这条通道的资源是否还活着。
    fn live_of(&self, channel_id: &str) -> bool {
        self.channels.get(channel_id).is_some_and(|e| e.live)
    }

    fn credit_of(&self, channel_id: &str) -> Option<u32> {
        self.channels.get(channel_id).map(|e| e.credit)
    }

    fn backlog_len(&self, channel_id: &str) -> usize {
        self.channels.get(channel_id).map_or(0, |e| e.backlog.len())
    }

    /// 开一条通道：找 connector → 建资源 → 回 `channel_ready` + 首帧。
    ///
    /// 重开同一个 `channel_id` **先释放旧资源**（§6：每次 `open_channel`
    /// 都重置状态与信用，含重开），否则上一次那个 PTY 会一直活到设备重启。
    ///
    /// **一次性命令跑完就地收尾**：connector 自己声明「我跑完了」
    /// （[`crate::connector::Connector::finished`]），这里据此发一帧
    /// `close_channel` 并释放资源——不指望页面记得关通道：Hub 自己开的通道
    /// 不会去关，而 node 侧那条通道会一直挂着。
    fn open(&mut self, spec: &OpenChannel, req: OpenReq) -> Result<Vec<Frame>, OpenFailure> {
        self.release(&spec.channel_id);
        // 未注册 = 「这台设备没有这个能力」（§5）：回 unsupported，
        // Hub 也会用同一个理由在超时后拒绝，两边说的话一致。
        let mut conn = connector::create(&spec.connector).ok_or(OpenFailure::Unsupported)?;
        let first = conn.open(req).map_err(OpenFailure::Connector)?;
        let finished = conn.finished();
        self.channels.insert(
            spec.channel_id.clone(),
            Entry {
                connector: spec.connector.clone(),
                resource: conn,
                credit: spec.credit_start,
                live: true,
                backlog: VecDeque::new(),
                backlog_bytes: 0,
                sent: 0,
            },
        );
        let mut frames = vec![ready_frame(
            &spec.channel_id,
            serde_json::json!({ "connector": spec.connector }),
        )];
        frames.extend(
            first
                .into_iter()
                .map(|f| data_frame(&spec.channel_id, f.payload)),
        );
        if finished {
            // **结果帧必须在 close 之前**：页面要先拿到结果，才知道这条
            // 通道为什么结束。
            self.release(&spec.channel_id);
            frames.push(close_frame(&spec.channel_id, FINISHED_REASON));
        }
        Ok(frames)
    }

    /// 通道内数据帧 → 交给该通道的 connector，产出帧入队，然后按信用放行。
    fn on_data(
        &mut self,
        channel_id: &str,
        payload: &serde_json::Value,
    ) -> Result<Vec<Frame>, DataError> {
        {
            let entry = self
                .channels
                .get_mut(channel_id)
                .ok_or(DataError::UnknownChannel)?;
            if !entry.live {
                return Err(DataError::UnknownChannel);
            }
            let out = entry
                .resource
                .on_data(payload)
                .map_err(DataError::Connector)?;
            // 入队与溢出判定都走同一处：`poll_all` 也要入队，
            // 两份拷贝的溢出策略迟早会分叉，而分叉的表现是
            // 「按一下键就不溢出了」。
            self.queue_frames(channel_id, out)?;
        }
        Ok(self.flush(channel_id))
    }

    /// 产出的帧入队 + 信用字节累计。**溢出即关通道并报错**。
    ///
    /// 溢出不截断：`protocol.md` §6 的信用模型是「排进待发队列、等
    /// `channel_credit` 再放行」，所以这里只能排队。排到内存上限还排不下
    /// 说明对端永久不读，唯一能自保的动作是放掉这条通道。
    fn queue_frames(
        &mut self,
        channel_id: &str,
        out: Vec<crate::connector::OutFrame>,
    ) -> Result<(), DataError> {
        let Some(entry) = self.channels.get_mut(channel_id) else {
            return Err(DataError::UnknownChannel);
        };
        for f in out {
            let frame = data_frame(channel_id, f.payload);
            // **记的必须是真正发出去的那一帧**：入队按裸 payload 记、出队按
            // 信封记，两边口径不同就会让每出一帧多减几十字节——差额被
            // `saturating_sub` 静默吞掉，8 MiB 的闸实际松了一大截。
            let bytes = frame.c.to_string().len();
            if entry.backlog_bytes + bytes > MAX_BACKLOG_BYTES {
                // 「关掉」必须走 [`Self::release`] 那一条路，而不能只是把
                // `live` 抹成 false：那会让这条通道变成一只僵尸——PTY 还
                // 开着（子进程一直在跑，设备上看不见也管不着）、8 MiB 积压
                // 还占着（`release` 才清 `backlog` 与字节账），而 Hub 那边
                // 已经收到 `channel_error` 认定它死了。两边对「这条通道还在
                // 不在」的假设正好相反。
                entry.live = false;
                self.release(channel_id);
                return Err(DataError::Connector(ConnectorError::internal(format!(
                    "channel {channel_id} produced more than {MAX_BACKLOG_BYTES} \
                     bytes of unbacked output"
                ))));
            }
            entry.backlog_bytes += bytes;
            entry.backlog.push_back(frame);
        }
        Ok(())
    }

    /// 按信用放行待发队列（§6：每发一帧 −1，归零即挂起）。
    fn flush(&mut self, channel_id: &str) -> Vec<Frame> {
        let Some(entry) = self.channels.get_mut(channel_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while entry.credit > 0 {
            let Some(frame) = entry.backlog.pop_front() else {
                break;
            };
            entry.credit -= 1;
            entry.sent = entry.sent.saturating_add(1);
            entry.backlog_bytes = entry
                .backlog_bytes
                .saturating_sub(frame.c.to_string().len());
            out.push(frame);
        }
        out
    }

    /// 主动问每条活通道「有没有新东西」，入队后按信用放行。
    ///
    /// **必须有这条独立于 `on_data` 的路**：PTY 的输出是子进程写出来的，
    /// 与 Hub 有没有再发下行帧无关。缺了它，`ls` 的输出会一直卡在
    /// connector 里，直到用户碰巧又敲了一个键。
    ///
    /// 返回值是 `(channel_id, 帧列表)`：worker 要按通道分组发，
    /// 而错误**不进这里**——某条通道的 connector 出错由 `queue_frames`
    /// 走既有的错误路径回 `channel_error`，poll 本身不制造新语义。
    fn poll_all(&mut self) -> Vec<(String, Vec<Frame>)> {
        let mut out = Vec::new();
        for id in self.live_ids() {
            // coverage: excluded — `live_ids()` 的每个 id 都来自**这张表**的
            // 键（它遍历的就是 `self.channels`），而这里持的是同一张表的
            // `&mut` 借用，期间不可能被摘掉，所以这个 None 臂不可达。
            let Some(entry) = self.channels.get_mut(&id) else {
                continue;
            };
            let Ok(produced) = entry.resource.poll() else {
                // connector 自己的错误已经由 open/on_data 的路径处理；
                // 这里不重复制造 `channel_error`，交给下一次 poll 再试。
                continue;
            };
            // **「跑完了没有」要在判产出之前取**：终端的 shell 退出后
            // `poll` 返回空，而「跑完了」这个事实恰恰只在没有产出时才有。
            let finished = entry.resource.finished();
            if !produced.is_empty() {
                // 溢出不吞：`queue_frames` 已经把通道标成 dead，
                // 这里把那句 `channel_error` 一起发出去。吞掉它的话，
                // 页面只会看到终端不声不响地停住——而「对端永远不读」
                // 恰恰是最需要说出口的那种故障。
                if let Err(DataError::Connector(e)) = self.queue_frames(&id, produced) {
                    out.push((id.clone(), vec![error_frame(&id, e.code(), &e.to_string())]));
                    continue;
                }
            }
            let mut frames = self.flush(&id);
            if finished && self.backlog_len(&id) == 0 {
                // **PTY 退出后通道被释放**（子任务 8）：资源留着就是一台
                // 设备上没人读、也拿不到信用的僵尸通道。
                self.release(&id);
                frames.push(close_frame(&id, FINISHED_REASON));
            }
            if !frames.is_empty() {
                out.push((id, frames));
            }
        }
        out
    }

    /// 当前活着的通道 id。
    ///
    /// **先收集再遍历**：`poll` 要 `&mut`，不能一边遍历一边借表。
    fn live_ids(&self) -> Vec<String> {
        self.channels
            .iter()
            .filter(|(_, e)| e.live)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// `close_channel`：释放资源并回 `close_channel`（§5）。
    ///
    /// 未知通道**不回错**：Hub 在重连后会重发 `close_channel`（它那边
    /// 可能还留着旧连接记的账），对不上号时唯一有意义的动作是「清掉」。
    fn close(&mut self, channel_id: &str, reason: &str) -> Vec<Frame> {
        self.release(channel_id);
        vec![close_frame(channel_id, reason)]
    }

    /// `channel_credit`：补信用（**只增不减**，§6），再把待发的放出去。
    ///
    /// 补信用立刻 flush 是必须的：PTY 正是靠这一下才继续推——
    /// 挂着的通道除了等它没有别的出口。
    fn credit(&mut self, channel_id: &str, delta: u32) -> Result<Vec<Frame>, DataError> {
        let entry = self
            .channels
            .get_mut(channel_id)
            .ok_or(DataError::UnknownChannel)?;
        // 溢出按饱和处理：debug 下的 `+=` 会 panic，而「只增不减」本就意味着
        // 饱和是语义上正确的终值。
        entry.credit = entry.credit.saturating_add(delta);
        Ok(self.flush(channel_id))
    }

    /// 释放一条通道的资源（幂等）。
    fn release(&mut self, channel_id: &str) {
        if let Some(entry) = self.channels.get_mut(channel_id) {
            if entry.live {
                entry.resource.close();
                entry.live = false;
            }
            entry.backlog.clear();
            entry.backlog_bytes = 0;
        }
    }
}

/// 本设备唯一的通道表：一个进程一台设备（约束 4）。
fn table() -> &'static std::sync::Mutex<ChannelTable> {
    static TABLE: std::sync::OnceLock<std::sync::Mutex<ChannelTable>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(ChannelTable::default()))
}

/// 处理一帧通道帧。
pub fn on_channel_frame(frame: &Frame) -> Action {
    match frame.msg_type.as_str() {
        types::OPEN_CHANNEL => on_open(frame),
        types::CHANNEL_DATA => on_data(frame),
        types::CLOSE_CHANNEL => on_close(frame),
        types::CHANNEL_CREDIT => on_credit(frame),
        _ => Action::None,
    }
}

fn on_open(frame: &Frame) -> Action {
    let spec: OpenChannel = match frame.payload() {
        Ok(s) => s,
        Err(_) => return Action::End,
    };
    match lock().open(&spec, req_of(&spec)) {
        Ok(frames) => Action::Send(frames),
        Err(e) => Action::Send(vec![error_frame(&spec.channel_id, e.code(), &e.reason())]),
    }
}

/// 把 `open_channel.params` 解成 [`OpenReq`]。
///
/// **`workspace_root` 必须在这里被取出来**：它是 files 与 exec 都要的会话
/// 工作区根，而它是 `OpenReq` 上的一等字段而不是 connector 私有参数。
/// 整份 params 原样塞进 `OpenReq::params` 的话，那个键永远停在 `params`
/// 里，`OpenReq::root()` 恒为 `None`——于是每个 files 通道都开不出来，
/// 症状是「工作区没给」，而真相是「Hub 给了，没人读」。
///
/// 解不开（非对象，或某个字段形状不对）就**回落成整份 params 私传**：
/// 那正是子任务 7 定的旧行为，terminal 的 params 仍然照常工作，
/// 而一个解不开的 `workspace_root` 会在 connector 自己的 `open` 里
/// 变成一句人话（files / exec 都这么说），不会在这里变成一条断连。
fn req_of(spec: &OpenChannel) -> OpenReq {
    serde_json::from_value::<OpenReq>(spec.params.clone())
        .unwrap_or_else(|_| OpenReq::new("", spec.params.clone()))
}

fn on_data(frame: &Frame) -> Action {
    let spec: protocol::channel::ChannelData = match frame.payload() {
        Ok(s) => s,
        Err(_) => return Action::End,
    };
    dispatch(&spec.channel_id, |t| {
        t.on_data(&spec.channel_id, &spec.payload)
    })
}

fn on_close(frame: &Frame) -> Action {
    let spec: CloseChannel = match frame.payload() {
        Ok(s) => s,
        Err(_) => return Action::End,
    };
    Action::Send(lock().close(&spec.channel_id, &spec.reason))
}

fn on_credit(frame: &Frame) -> Action {
    let spec: protocol::channel::ChannelCredit = match frame.payload() {
        Ok(s) => s,
        Err(_) => return Action::End,
    };
    dispatch(&spec.channel_id, |t| t.credit(&spec.channel_id, spec.delta))
}

/// 跑一次表操作，把结果与错误翻译成 [`Action`]。
fn dispatch<F>(channel_id: &str, f: F) -> Action
where
    F: FnOnce(&mut ChannelTable) -> Result<Vec<Frame>, DataError>,
{
    let outcome = {
        let mut t = lock();
        f(&mut t)
    };
    match outcome {
        Ok(frames) => Action::Send(frames),
        Err(e) => match e.reason() {
            Some((code, reason)) => Action::Send(vec![error_frame(channel_id, code, &reason)]),
            None => Action::None,
        },
    }
}

/// 拿锁跑一次操作。**锁中毒**（别的持锁者 panic）时退化成空表的一次操作：
/// 它只会让这一次操作失败，不会把设备带下去——而 process 级退出在本项目
/// 是禁止的（见 `AGENTS.md` 生产代码禁 panic 铁律）。
fn lock() -> std::sync::MutexGuard<'static, ChannelTable> {
    table().lock().unwrap_or_else(|e| e.into_inner())
}

/// 连接断了：释放全部资源并清掉表。**返回释放掉的通道数**
/// （测试用计数断言资源确实释放了）。
///
/// **本地 PTY 随连接一起死**：`security-and-lifecycle.md` §7.3 写的是
/// 「PTY 缓冲区在重连后不复位」——它指缓冲区，**不是那个进程**。这里
/// 的读循环是唯一的出站泵，它一停 PTY 输出就只能堆在设备上没人收；
/// 保留一个已经没人读、连信用都没有（信用随连接重置）的通道，
/// 只会在用户下一次打开工作台时看到一屏延迟输出。
pub fn on_disconnect() -> usize {
    // **一次拿锁就够**：下面要遍历的是同一张表，两次 `lock()` 之间表可能被
    // 换掉——于是「收集到的 id」与「释放的表项」根本不是同一批。
    let mut t = lock();
    let ids: Vec<String> = t.channels.keys().cloned().collect();
    let mut released = 0;
    for id in &ids {
        if t.live_of(id) {
            t.release(id);
            released += 1;
        }
    }
    t.channels.clear();
    released
}

/// 重置本进程共享的通道表。**只给测试用**：生产路径上通道表是
/// 一个设备一个进程的全局状态，没有「重置」这回事。
pub fn reset_for_tests() {
    lock().channels.clear();
}

/// 这条通道的信用余额（测试与排障用）。
pub fn credit_of(channel_id: &str) -> Option<u32> {
    lock().credit_of(channel_id)
}

/// 这条通道已推给 Hub 的数据帧数（测试与排障用）。
pub fn sent_frames_of(channel_id: &str) -> u64 {
    lock().channels.get(channel_id).map_or(0, |e| e.sent)
}

/// 这条通道待发的帧数（测试与排障用）。
pub fn backlog_of(channel_id: &str) -> usize {
    lock().backlog_len(channel_id)
}

/// 这条通道待发队列占用的**字节数**（测试与排障用）。
///
/// 与 [`backlog_of`] 配对：帧数说不了「积压了多少字节」，而 8 MiB 的闸
/// 判的是字节。
pub fn backlog_bytes_of(channel_id: &str) -> usize {
    lock()
        .channels
        .get(channel_id)
        .map_or(0, |e| e.backlog_bytes)
}

/// 表里有没有这条通道（测试与排障用）。
pub fn has_channel(channel_id: &str) -> bool {
    lock().has(channel_id)
}

/// 资源仍活着的通道数（测试与排障用）。
pub fn live_count() -> usize {
    lock().live_count()
}

/// 累计推给 Hub 的数据帧数（跨连接累计，测试与排障用）。
pub fn sent_frames_total() -> u64 {
    lock().sent_frames()
}

/// 主动泵一轮：向每条活通道要新数据，按信用放行，返回待发帧。
///
/// **worker 必须定期调它**（[`crate::worker`] 的 PTY 轮询 tick）。它不是
/// 「有下行数据时顺带泵一下」的优化项，而是终端能用的前提：PTY 输出由子
/// 进程写出，Hub 不发下行帧时它照样在涨，不主动问就永远发不出去。
///
/// 返回 `(channel_id, 帧列表)` 而不是一个扁平的 `Vec<Frame>`：分组是为了
/// 让调用方能对每条通道单独判空（一条通道没数据不该让其它通道的帧被丢）。
pub fn pump() -> Vec<(String, Vec<Frame>)> {
    lock().poll_all()
}

/// 这一帧是不是通道帧。
///
/// 口径与 Hub 侧（`hub-server/src/channels.rs::is_channel_type`）一致，
/// 但**各写一份是对的**：两侧的可选集不同——Hub 还收 `channel_ready`，
/// node 这边只收 Hub 会下发的四种。共用一份常量反而要为一个库的差异
/// 让另一方绕路。
pub fn is_channel_type(msg_type: &str) -> bool {
    matches!(
        msg_type,
        types::OPEN_CHANNEL | types::CHANNEL_DATA | types::CLOSE_CHANNEL | types::CHANNEL_CREDIT
    )
}

/// 一条上行 `channel_data`。
///
/// **`c` 必须是 `ChannelData{channel_id, payload}`，不是 connector 的裸载荷**。
/// 协议（`protocol.md` §4）把这个信封定为双向：Hub 下发时包了
/// `channel_id`（`routes/stream.rs` 的 `send_channel_data`），node 上行时
/// 也必须包。裸载荷在 Hub 侧 `frame.payload::<ChannelData>()` 会因缺字段
/// 解析失败，被 `publish_channel_data` 当成「Hub 没法解释的 JSON」静默丢掉
/// ——表现恰好是最难查的一种：`/send` 回 204、通道 `ready`、PTY 真的读到了
/// 字节、**页面上一片空白**，而两侧日志都只有一行 DEBUG。
fn data_frame(channel_id: &str, payload: serde_json::Value) -> Frame {
    let data = ChannelData {
        channel_id: channel_id.to_string(),
        payload,
    };
    push(
        types::CHANNEL_DATA,
        frame_payload_or(&data, protocol::channel::fallback::data(channel_id)),
    )
}

/// 一条 `channel_error`。
fn error_frame(channel_id: &str, code: ChannelErrorCode, reason: &str) -> Frame {
    let err = ChannelErrorPayload {
        channel_id: channel_id.to_string(),
        code,
        reason: reason.to_string(),
    };
    push(
        types::CHANNEL_ERROR,
        frame_payload_or(
            &err,
            protocol::channel::fallback::error(channel_id, code, reason),
        ),
    )
}

/// 一条上行 `close_channel`。
///
/// node 也会主动关通道（一次性命令跑完、PTY 退出）：**关通道不是 Hub 的
/// 专利**，而页面之外的调用方（Hub 自己开的通道）根本不会去关。
fn close_frame(channel_id: &str, reason: &str) -> Frame {
    let closing = CloseChannel {
        channel_id: channel_id.to_string(),
        reason: reason.to_string(),
    };
    push(
        types::CLOSE_CHANNEL,
        frame_payload_or(
            &closing,
            protocol::channel::fallback::close(channel_id, reason),
        ),
    )
}

/// node 侧主动关通道时给的 reason。
///
/// **它是一句给用户看的话**，不是错误码：通道不是「坏了」，是「跑完了」。
const FINISHED_REASON: &str = "connector finished";

/// 构造一帧通道推送帧（`channel_ready` / `channel_data` 共用）。
pub fn push(msg_type: &str, payload: serde_json::Value) -> Frame {
    Frame::push(msg_type, 0, payload)
}

/// `channel_ready` 帧（约束 14：能力声明靠协商，不靠版本号）。
pub fn ready_frame(channel_id: &str, capabilities: serde_json::Value) -> Frame {
    let ready = ChannelReady {
        channel_id: channel_id.to_string(),
        capabilities,
    };
    push(
        types::CHANNEL_READY,
        frame_payload_or(&ready, protocol::channel::fallback::ready(channel_id)),
    )
}

/// 本文件的通道表单测。
///
/// **为什么写在这里而不是 `tests/channel.rs`**：下面这些探针（`len` /
/// `is_empty` / `summary` / `Debug`）只在本模块内可达——`ChannelTable` 虽是
/// `pub`，但构造入口只有 [`table()`] 那个进程级单例，而它由 `reset_for_tests`
/// 之后的一系列公开函数驱动。把断言写成「开一条通道 → 查探针」的形状，
/// 就不必给外部测试开一个新的构造入口。
#[cfg(test)]
mod tests {
    use super::*;

    /// 通道表是**进程级单例**（约束 4），而 cargo test 默认多线程跑用例：
    /// 两个用例同时调 `reset()` / `on_channel_frame()` 会互相踩对方的账，
    /// 表现为「单跑全绿、全量随机红」。所以每个用例开头都要先拿这把锁，
    /// 拿到之后才 `reset()`——锁只保证互斥，空表由 `reset()` 负责。
    ///
    /// 锁中毒后仍要能跑完剩下的用例（一个红不该拖垮整批），故用
    /// `unwrap_or_else` 取回 guard 而不是 `unwrap()`。
    fn table_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 每个用例都从空表开始：通道表是进程级单例（约束 4）。
    fn reset() {
        reset_for_tests();
    }

    /// 开一条**真能开起来**的 files 通道（工作区根指向一个真目录）。
    fn open_files(channel_id: &str, credit: u32) -> Frame {
        let spec = OpenChannel {
            channel_id: channel_id.to_string(),
            connector: connector::names::FILES.to_string(),
            params: serde_json::json!({ "workspace_root": "/tmp" }),
            credit_start: credit,
        };
        Frame::push(types::OPEN_CHANNEL, 1, serde_json::to_value(&spec).unwrap())
    }

    fn data(channel_id: &str, payload: serde_json::Value) -> Frame {
        let spec = ChannelData {
            channel_id: channel_id.to_string(),
            payload,
        };
        Frame::push(types::CHANNEL_DATA, 2, serde_json::to_value(&spec).unwrap())
    }

    /// 表探针：`len` / `is_empty` / `summary` 三个口径必须同时说得通。
    ///
    /// 只测一个会漏掉「表里有 1 条但 `is_empty` 说空」这种纯记账 bug——
    /// 而那三个数任何一处撒谎，排障时看到的就是一张假表。
    #[test]
    fn the_table_probes_agree_with_each_other() {
        let _guard = table_lock();
        reset();
        assert_eq!(lock().len(), 0, "空表的 len 必须是 0");
        assert!(lock().is_empty(), "空表必须自报 empty");
        assert!(lock().summary().is_empty(), "空表没有概览");

        // 开一条真通道：三个探针都要认得它
        let action = on_channel_frame(&open_files("ch-p", 4));
        assert!(matches!(action, Action::Send(_)), "{action:?}");
        {
            let t = lock();
            assert_eq!(t.len(), 1, "开了一条通道，len 必须是 1");
            assert!(!t.is_empty(), "有通道就不能自报 empty");
            let s = t.summary();
            assert_eq!(s.len(), 1);
            let (id, conn, live, credit) = s.first().cloned().expect("一条概览");
            assert_eq!(id, "ch-p");
            assert_eq!(conn, connector::names::FILES);
            assert!(live, "刚开的通道资源是活的");
            assert_eq!(credit, 4, "概览里的信用就是开通道时给的");
            assert_eq!(t.live_count(), 1);
            assert_eq!(t.sent_frames(), 0, "还没发过任何帧");
        }
        assert_eq!(live_count(), 1);

        // 关掉它：`live_count` 归零，但表项**留着**——为了让随后那帧
        // `close_channel` 还能对上号（§5：关一条通道不得影响其他通道）。
        let action = on_channel_frame(&close_frame("ch-p", "done"));
        assert!(matches!(action, Action::Send(_)));
        {
            let t = lock();
            assert_eq!(t.live_count(), 0, "关掉之后不能再有活通道");
            assert_eq!(t.len(), 1, "表项留着，等 Hub 重发 close_channel 时对上号");
            assert!(!t.is_empty(), "同上：留着不等于空");
        }
        assert_eq!(live_count(), 0);
        // 而**断连**才真的清表：连接都没了，Hub 那边的账也一起清了。
        on_disconnect();
        assert!(lock().is_empty(), "断连必须清空表");
    }

    /// `Debug` 是一张排障用的表：它打的内容必须真能定位问题。
    ///
    /// 手写 `Debug` 的全部理由就是「trait object 打不出来」；若哪天它退化成
    /// `#[derive]`，这里会红——而那时排障者看到的将是一屏结构体名。
    #[test]
    fn debug_shows_the_channels_not_the_trait_objects() {
        let _guard = table_lock();
        reset();
        let action = on_channel_frame(&open_files("ch-dbg", 3));
        assert!(matches!(action, Action::Send(_)), "{action:?}");
        let text = format!("{:?}", lock());
        assert!(text.contains("ChannelTable"), "{text}");
        assert!(text.contains("ch-dbg"), "Debug 必须报通道 id：{text}");
        assert!(
            text.contains(connector::names::FILES),
            "Debug 必须报 connector 名：{text}"
        );
        assert!(text.contains("len"), "Debug 必须报条数：{text}");
        assert!(text.contains("live"), "Debug 必须报活着的条数：{text}");
        reset();
    }

    /// `channel_credit` 打在**没开过的通道**上：静默（不回错，也不留表项）。
    ///
    /// 回错会让 Hub 以为这条通道死了，而 Hub 侧可能还留着上一次连接的账。
    #[test]
    fn credit_for_an_unknown_channel_is_silent() {
        let _guard = table_lock();
        reset();
        let f = Frame::push(
            types::CHANNEL_CREDIT,
            3,
            serde_json::json!({"channel_id":"ghost","delta":8}),
        );
        assert!(matches!(on_channel_frame(&f), Action::None));
        assert!(!has_channel("ghost"), "补信用不能凭空造出通道");
    }

    /// 8 MiB 待发上限：**溢出不截断，而是关通道并报错**。
    ///
    /// 「不静默丢字节」是产品承诺（PTY 输出不丢），所以排到上限只能放掉
    /// 这条通道；而放掉必须**说出口**——否则页面只会看到终端不声不响停住。
    ///
    /// **用真 shell 造积压**：这条闸判的是 `queue_frames` 的字节账，而它是
    /// `poll_all` 与 `on_data` 共用的那条路。手工塞一份超大 payload 会被
    /// files 自己的载荷预算先拦下，测到的就不是这条闸了。
    #[test]
    fn an_unbacked_backlog_over_8mib_closes_the_channel_and_says_why() {
        let _guard = table_lock();
        reset();
        // 信用给 1：每轮只放一帧出去，于是积压真的会涨过 8 MiB。
        let spec = OpenChannel {
            channel_id: "ch-flood".to_string(),
            connector: connector::names::TERMINAL.to_string(),
            params: serde_json::json!({ "shell": "/bin/sh" }),
            credit_start: 1,
        };
        let f = Frame::push(types::OPEN_CHANNEL, 1, serde_json::to_value(&spec).unwrap());
        assert!(matches!(on_channel_frame(&f), Action::Send(_)));

        // 一条一次吐满一屏、停一下再吐的命令：让读线程有机会把字节交上来。
        let d = data(
            "ch-flood",
            serde_json::json!({"bytes":"while :; do echo fill; done\n"}),
        );
        on_channel_frame(&d);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            for (_id, frames) in pump() {
                if frames.iter().any(|f| f.msg_type == types::CHANNEL_ERROR) {
                    assert_eq!(
                        live_count(),
                        0,
                        "溢出之后资源必须被释放，不能留一只僵尸通道"
                    );
                    assert_eq!(backlog_bytes_of("ch-flood"), 0, "释放要清掉积压字节账");
                    on_disconnect();
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "30s 里积压没越过 8 MiB（backlog_bytes={}）",
                backlog_bytes_of("ch-flood")
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// `queue_frames` / `flush` 对**表里没有的通道**：报错而不是静默丢帧。
    ///
    /// 这两条是「帧凭空来了」时的兜底。静默返回 `Ok(())` 的症状是
    /// 「通道明明在跑，Hub 却一个字节都收不到」——而那正是最像
    /// 「设备坏了」的故障形态，所以这里必须能区分出来。
    #[test]
    fn framing_an_unknown_channel_is_an_error_not_silence() {
        let _guard = table_lock();
        reset();

        let out = vec![connector::OutFrame::new(serde_json::json!({ "k": "v" }))];
        let got = lock().queue_frames("ch-not-there", out);
        assert!(
            matches!(got, Err(DataError::UnknownChannel)),
            "往一条没开的通道上排帧必须报 UnknownChannel：{got:?}"
        );

        assert!(
            lock().flush("ch-not-there").is_empty(),
            "没开的通道没有帧可放行"
        );
    }

    /// `poll_all` 对「表里那条通道刚被摘掉」与「connector 报错」都是跳过，
    /// **不制造新语义**：某条通道的错由 `open` / `on_data` 的既有路径回
    /// `channel_error`，poll 再报一次就成了同一个错误的两次回帧。
    #[test]
    fn poll_all_reports_only_channels_with_frames_to_send() {
        let _guard = table_lock();
        reset();

        // 空表：没有任何活通道，poll 是空转而不是错误。
        assert!(lock().poll_all().is_empty(), "空表上 poll_all 必须空转");

        // 一条活通道但没有任何待发帧：`poll_all` **不出现在结果里**，
        // 而不是塞一个空 Vec 进去。空 Vec 会让 worker 白白发一轮
        // 「这个通道有新帧」的通知，而那正是心跳被吵醒的原因。
        let action = on_channel_frame(&open_files("ch-quiet", 4));
        assert!(matches!(action, Action::Send(_)), "{action:?}");
        assert!(
            lock().poll_all().is_empty(),
            "没有帧可发的通道不该出现在 poll 的结果里"
        );
    }

    /// 一个 `poll` 总是报错的 connector。
    struct FailingPoll;

    impl connector::Connector for FailingPoll {
        fn open(
            &mut self,
            _req: connector::OpenReq,
        ) -> Result<Vec<connector::OutFrame>, connector::ConnectorError> {
            Ok(Vec::new())
        }

        fn on_data(
            &mut self,
            _data: &serde_json::Value,
        ) -> Result<Vec<connector::OutFrame>, connector::ConnectorError> {
            Ok(Vec::new())
        }

        fn poll(&mut self) -> Result<Vec<connector::OutFrame>, connector::ConnectorError> {
            Err(connector::ConnectorError::internal(
                "poll failed on purpose",
            ))
        }

        fn close(&mut self) {}
    }

    /// `poll_all` 遇到一个**自己会报错**的 connector：跳过它，不抛错、
    /// 不回 `channel_error`。
    ///
    /// 为什么不能在这里再报一次错：connector 的错已经由 `open` / `on_data`
    /// 的路径回过 `channel_error` 了，poll 再回一次就是同一个错误的两次
    /// 回帧——Hub 那边看到的是「这条通道连续失败两次」，而真相是
    /// 「一次失败被报了两遍」。
    #[test]
    fn poll_all_skips_a_connector_whose_poll_fails() {
        let _guard = table_lock();
        reset();

        // 直接往表里塞一条「poll 必错」的通道。
        {
            let mut t = lock();
            t.channels.insert(
                "ch-fail".to_string(),
                Entry {
                    connector: "failing".to_string(),
                    resource: Box::new(FailingPoll),
                    credit: 4,
                    live: true,
                    backlog: VecDeque::new(),
                    backlog_bytes: 0,
                    sent: 0,
                },
            );
        }
        assert_eq!(live_count(), 1, "塞进去的是一条活通道");

        // 跳过它：不抛错，也没有帧。
        let produced = lock().poll_all();
        assert!(
            produced.is_empty(),
            "poll 报错的通道要被跳过，而不是冒出来：{produced:?}"
        );
    }

    /// `pump` 在空表上是空转，不是错误。
    ///
    /// worker 每 30ms 调它一次，空表时它必须什么都不做——一次 `Err`
    /// 会让整条通道被拆掉。
    #[test]
    fn pumping_an_empty_table_yields_nothing() {
        let _guard = table_lock();
        reset();
        assert!(pump().is_empty(), "空表泵一轮什么都没有");
        assert_eq!(on_disconnect(), 0, "空表断连不报数");
    }

    /// 不是通道帧的帧一律不改路由（约束 14）。
    ///
    /// 这条直接打 `on_channel_frame` 的 `_ => Action::None` 兜底臂。
    #[test]
    fn an_unrecognised_frame_type_gets_no_reply() {
        let _guard = table_lock();
        reset();
        for t in [
            types::HEARTBEAT,
            types::HEARTBEAT_ACK,
            types::CHANNEL_READY,
            types::CHANNEL_ERROR,
            "some_future_type",
            "",
        ] {
            let f = Frame::push(t, 1, serde_json::json!({"anything":1}));
            assert!(
                matches!(on_channel_frame(&f), Action::None),
                "{t} 不是 Hub 会下发的通道帧，必须没有应答"
            );
        }
    }

    fn close_frame(channel_id: &str, reason: &str) -> Frame {
        let spec = CloseChannel {
            channel_id: channel_id.to_string(),
            reason: reason.to_string(),
        };
        Frame::push(
            types::CLOSE_CHANNEL,
            4,
            serde_json::to_value(&spec).unwrap(),
        )
    }
}
