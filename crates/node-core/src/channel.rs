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

/// 外部源驱动的通道在 `Entry.connector` 里记的名字。
///
/// # 为什么需要它
///
/// `Entry` 原本只描述一件事：「这条通道由一个 connector 实例驱动」。ACP 对话
/// 打破了那个假设——帧来自子进程的 stdout，由 worker 里的读循环推进，**没有
/// connector 实例**（`AGENTS.md` 约束 3：connector 只面向页面用户，不向 agent
/// 暴露，所以 ACP 不能做成 connector）。
///
/// 名字有三处要用：`summary()` 与 `Debug` 的排障输出（否则看到一条无名通道）、
/// 日志、以及**测试断言这条通道确实不是 connector**。
pub const EXTERNAL_CONNECTOR_NAME: &str = "acp";

/// 外部源通道的初始信用。
///
/// # 为什么与 `CREDIT_START`（32）不同
///
/// 32 是给**终端按键回显**定的：一次按键回来几十字节，32 帧足够。ACP 的一轮
/// 回复是几百条 chunk，每条几十字节，而**初始信用一旦打光就只能等 Hub 补**。
/// 32 会让每次对话都先卡在「补信用」的往返上，用户看到回复一顿一顿地出。
///
/// 256 是折中：一轮常见的短回复（实测短问答 3~4 条事件）用不到 1%，而长回复
/// 打光时 Hub 早就在补了。**不设成无限**：信用仍然要能节流，否则待发队列
/// 会在对端不读时无界增长。
const EXTERNAL_CREDIT_START: u32 = 256;

/// 一条外部源通道最多积压多少条**未读的入站消息**（Hub → 设备）。
///
/// worker 每轮 `select!` 都取一次，正常不会有第二条在等。这个上限拦的是
/// 「worker 卡住 / 通道已死的路径还在收帧」那种情形——没有上限时用户连点
/// 发送会把设备内存吃光，而症状是「设备变慢」，没人会想到是这条通道。
const INBOUND_LIMIT: usize = 64;

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
    /// 一条 **ACP 对话通道被开出来了**：worker 要去起子进程。
    ///
    /// # 为什么这不是 `Action::Send` 的一部分
    ///
    /// `channel_ready` 必须先发出去（Hub 在等它，5 s 超时），而**起子进程
    /// 可能失败**（agent 没装、配置错、agent 不认 ACP）。两件事分开，才能让
    /// 「通道开了但 agent 起不来」变成一条 `channel_error` 而不是一个 5 秒的
    /// 静默超时。
    ///
    /// 帧**排在起进程之前**发：否则 `session/new` 慢的时候 Hub 先超时，
    /// 拿到的错误是「不支持」——而真相是「agent 慢」。
    OpenAcp {
        frames: Vec<Frame>,
        channel_id: String,
        /// 一个 [`ConversationSpec`]：起子进程要的全部东西。
        ///
        /// **配置随这条路进来，不查设备侧的表**：`agent_config` 存在 Hub 侧
        /// （用户裁定「Hub 存，设备读」），设备没有那张表可查。Hub 在建会话
        /// 时把配置读出来、随 `open_channel.params` 下发——于是设备侧只有
        /// 一份「这次要用什么命令」的事实，不会出现「设备本地配置与 Hub 不一致」。
        ///
        /// 装箱：`Action` 走的是低频控制路径，但枚举的大小等于最大变体，
        /// 不装箱的话每个 `Action::None` / `Action::End` 也要背一个
        /// `ConversationSpec`（九个字段、含两个 `String` 与三个容器）。
        spec: Box<crate::agent::conversation::ConversationSpec>,
    },
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
    ///
    /// 外部源驱动的通道用 `EXTERNAL_CONNECTOR_NAME`：它不是 connector，
    /// 但日志里需要一个名字，否则排障时看到一条无名通道。
    connector: String,
    /// 通道的驱动者：**持有本地资源**（PTY / 工作区句柄）。
    /// 表一清，资源就跟着走（`close` 与断连路径都先显式 `close()`）。
    ///
    /// # 为什么是 `Option`
    ///
    /// ACP 对话的帧来自一个**子进程的 stdout**，不是 connector 实例
    /// （`AGENTS.md` 约束 3：connector 只面向页面用户，不向 agent 暴露，
    /// 所以 ACP 不能做成 connector）。那条通路用 [`Self::external`] 标出，
    /// 本字段为 `None`——没有资源可关，帧由 `push_external` 送进来。
    ///
    /// 早先这里是非 `Option`，于是「没有 connector 的通道」根本无法表达。
    resource: Option<BoxedConnector>,
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
    /// 外部源通道的**入站**（Hub → 设备）载荷，等 worker 取走去发 `session/prompt`。
    ///
    /// # 为什么入站要在这里排队而不是直接给 ACP
    ///
    /// 通道帧从 `on_channel_frame` 进来，那是**同步**函数、不持有 ACP 客户端
    /// （客户端活在 worker 的任务里，而且是 async 的）。在这里直接 await 会
    /// 把「按一下发送」变成「读循环阻塞到 prompt 返回」，而那正是 M1 的
    /// `exec` 堵住心跳臂的错误形状。
    ///
    /// connector 通道这个字段恒为空。
    inbound: VecDeque<serde_json::Value>,
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
                resource: Some(conn),
                credit: spec.credit_start,
                live: true,
                backlog: VecDeque::new(),
                backlog_bytes: 0,
                sent: 0,
                inbound: VecDeque::new(),
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
    ///
    /// **外部源通道的输入不走这里**：ACP 的输入是 `session/prompt`，由 worker
    /// 从 [`Self::take_inbound`] 取走。外部源通道收到 `channel_data` 时**入站
    /// 排队**而不是报错——用户点发送就是这样进来的。
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
            if entry.resource.is_none() {
                // 外部源通道：入站排队，等 worker 取。
                //
                // **上限与待发同一个量级**：用户一条消息超过 8 MiB 不是「对话」
                // 而是把文件粘进输入框，那种用法该走工作区上传而不是本通道。
                // 溢出即关通道（与出站同一条策略），而不是截断。
                if entry.inbound.len() >= INBOUND_LIMIT {
                    entry.live = false;
                    self.release(channel_id);
                    return Err(DataError::Connector(ConnectorError::internal(format!(
                        "channel {channel_id} got more than {INBOUND_LIMIT} unread inbound messages"
                    ))));
                }
                entry.inbound.push_back(payload.clone());
                return Ok(Vec::new());
            }
            let out = entry
                .resource
                .as_mut()
                // 外部源通道（ACP）没有 connector 可转交：它的输入是
                // `session/prompt`，由 worker 直接调 ACP 客户端发出，不走
                // `channel_data`。真收到 `channel_data` 说明 Hub 那边把这条
                // 通道当 connector 通道用了——回 `UnknownChannel` 让它清账，
                // 而不是假装收下然后丢进黑洞。
                .ok_or(DataError::UnknownChannel)?
                .on_data(payload)
                .map_err(DataError::Connector)?;
            // 入队与溢出判定都走同一处：`poll_all` 也要入队，
            // 两份拷贝的溢出策略迟早会分叉，而分叉的表现是
            // 「按一下键就不溢出了」。
            self.queue_frames(channel_id, out, MAX_BACKLOG_BYTES)?;
        }
        Ok(self.flush(channel_id))
    }

    /// 产出的帧入队 + 信用字节累计。**溢出即关通道并报错**。
    ///
    /// 溢出不截断：`protocol.md` §6 的信用模型是「排进待发队列、等
    /// `channel_credit` 再放行」，所以这里只能排队。排到内存上限还排不下
    /// 说明对端永久不读，唯一能自保的动作是放掉这条通道。
    ///
    /// `limit` 是待发字节上限，由调用方给而不是在这里读模块常量：测试要能
    /// 注入一个小上限，从而把「越限即关通道」这条命题与「往真 PTY 里灌
    /// 8 MiB 需要多少秒」解耦开（后者随机器负载变，实测在并发下能从 14 秒
    /// 涨到越过 30 秒的预算）。生产调用点一律传 [`MAX_BACKLOG_BYTES`]。
    fn queue_frames(
        &mut self,
        channel_id: &str,
        out: Vec<crate::connector::OutFrame>,
        limit: usize,
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
            if entry.backlog_bytes + bytes > limit {
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
            // **外部源通道只 flush，不 poll**：它的帧由 `push_external` 入队，
            // 没有资源可问。也不能在这里提前 `continue` —— 那样待发队列就
            // 永远等不到放行，ACP 的回复会卡在 backlog 里直到下一次补信用。
            let Some(resource) = entry.resource.as_mut() else {
                let frames = self.flush(&id);
                if !frames.is_empty() {
                    out.push((id, frames));
                }
                continue;
            };
            let Ok(produced) = resource.poll() else {
                // connector 自己的错误已经由 open/on_data 的路径处理；
                // 这里不重复制造 `channel_error`，交给下一次 poll 再试。
                continue;
            };
            // **「跑完了没有」要在判产出之前取**：终端的 shell 退出后
            // `poll` 返回空，而「跑完了」这个事实恰恰只在没有产出时才有。
            let finished = resource.finished();
            if !produced.is_empty() {
                // 溢出不吞：`queue_frames` 已经把通道标成 dead，
                // 这里把那句 `channel_error` 一起发出去。吞掉它的话，
                // 页面只会看到终端不声不响地停住——而「对端永远不读」
                // 恰恰是最需要说出口的那种故障。
                if let Err(DataError::Connector(e)) =
                    self.queue_frames(&id, produced, MAX_BACKLOG_BYTES)
                {
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
                // 外部源通道没有资源可关（`None`）。它被杀掉的方式是
                // `live = false` + 清 backlog——ACP 子进程的生死归 worker 管，
                // 不归通道表管（见 `push_external` 的注释）。
                if let Some(resource) = entry.resource.as_mut() {
                    resource.close();
                }
                entry.live = false;
            }
            entry.backlog.clear();
            entry.backlog_bytes = 0;
            // 入站也清：通道死了还留着待发消息，会让下一条同名通道
            // （Hub 重开）取到上一轮的旧消息——用户看到自己没发过的话被
            // 发出去了，那比丢一条消息更糟。
            entry.inbound.clear();
        }
    }

    /// 取走某条外部源通道的入站消息（Hub → 设备的用户消息）。
    ///
    /// 返回空 `Vec` 表示没有新消息。**不区分「通道不存在」与「没有消息」**：
    /// 调用方（worker）对两者的反应一样——什么都不做。而报错会让一个刚被
    /// 关掉的通道在下一轮 tick 里刷一条没人管的错误日志。
    fn take_inbound(&mut self, channel_id: &str) -> Vec<serde_json::Value> {
        match self.channels.get_mut(channel_id) {
            Some(entry) => entry.inbound.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// 一条 ACP 通道起进程失败时的收尾：关通道并回 `channel_error`。
    ///
    /// 与 [`Self::release`] 的区别只有一个：这里**要回帧**。Hub 那边已经
    /// 拿到 `channel_ready` 并在等数据，不告诉它一声，页面会一直显示
    /// 「正在回复」——而那个状态永远不会结束。
    fn fail_external(
        &mut self,
        channel_id: &str,
        code: ChannelErrorCode,
        reason: &str,
    ) -> Vec<Frame> {
        self.release(channel_id);
        vec![error_frame(channel_id, code, reason)]
    }

    /// 开一条**外部源驱动**的通道：没有 connector，帧由 [`Self::push_external`] 送入。
    ///
    /// # 为什么要有这条路，而不是把 ACP 做成 connector
    ///
    /// `AGENTS.md` 约束 3 把 connector 定义为**只面向页面用户**的能力，不得向
    /// agent 暴露。ACP 恰恰是「页面 ↔ agent」的通路，把它注册成 connector 会让
    /// agent 能够通过 `open_channel` 直接拿到另一条对话流——那是越界。
    ///
    /// 于是这里只做通道表该做的事：建一个入口、给信用、让 Hub 拿到
    /// `channel_ready`。帧从哪来是调用方的事。
    ///
    /// **幂等**：已存在的同名通道先释放，与 `open` 的口径一致（§6：每次
    /// `open_channel` 都重置状态与信用，含重开）。
    fn open_external(&mut self, channel_id: &str, credit_start: u32) -> Vec<Frame> {
        self.release(channel_id);
        self.channels.insert(
            channel_id.to_string(),
            Entry {
                connector: EXTERNAL_CONNECTOR_NAME.to_string(),
                resource: None,
                credit: credit_start,
                live: true,
                backlog: VecDeque::new(),
                backlog_bytes: 0,
                sent: 0,
                inbound: VecDeque::new(),
            },
        );
        vec![ready_frame(
            channel_id,
            serde_json::json!({ "connector": EXTERNAL_CONNECTOR_NAME }),
        )]
    }

    /// 把一个外部源产生的事件推进某条通道的待发队列。
    ///
    /// **只入队，不放行**：放行统一由 [`Self::poll_all`]（worker 的 `pump` 臂）
    /// 做。这一条不是洁癖——ACP 的事件由读循环在**独立 task** 里产生，那个 task
    /// 手里没有 `Established`，发不出帧。如果这里顺手 flush，帧就被返回给一个
    /// 没有发送能力的调用方，然后被丢掉。
    ///
    /// 这条 bug 真实发生过：第一版在这里 flush 并返回帧，`read_events` 用
    /// `let _ =` 接住，于是**整条对话一个字节都到不了页面**，而所有单测都是绿的
    /// ——因为单测自己取返回值。只有真机端到端（`a_real_opencode_turn_...`）
    /// 才暴露它。
    ///
    /// # 通道不在了怎么办
    ///
    /// 返回 `Err(DataError::UnknownChannel)`，**不重建通道**。这条通路的语义是
    /// 「把已经有的事实送出去」而不是「维持一条通路的存活」：Hub 断了、通道被
    /// 关了，重建出来也没人收，而 ACP 子进程的生死是 worker 的事（见下）。
    ///
    /// # 为什么 ACP 子进程不随这条通道死
    ///
    /// 子进程的 owner 是 **worker 不是 supervisor**，也不随断连死。理由有两条：
    /// 一是重连后 `session/load` 需要一个还活着的进程才能续上（进程一死，
    /// 上下文就没了，重连等于开新会话）；二是 agent 正在跑的工具调用被硬杀会
    /// 留下半途的副作用。所以断连期间事件**入不了队就丢掉**（这是 M2 已裁决的
    /// 「不承诺补齐」，见 `session-and-trajectory.md` §3.2.1），而进程本身活着。
    fn push_external(
        &mut self,
        channel_id: &str,
        payloads: Vec<serde_json::Value>,
        limit: usize,
    ) -> Result<(), DataError> {
        let Some(entry) = self.channels.get_mut(channel_id) else {
            return Err(DataError::UnknownChannel);
        };
        // 外部源通道必须是 `resource: None`。这条断言式的分支在把 ACP 事件
        // 送进一条 connector 通道时触发——那种混淆会让页面看到一半终端字节
        // 一半对话事件，所以宁可报错。
        if entry.resource.is_some() {
            return Err(DataError::UnknownChannel);
        }
        let out: Vec<crate::connector::OutFrame> = payloads
            .into_iter()
            .map(crate::connector::OutFrame::new)
            .collect();
        // 入队走与 connector 同一条路：溢出策略只有一份，两份迟早分叉。
        self.queue_frames(channel_id, out, limit)?;
        Ok(())
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
    // **ACP 对话走另一条路**：它不是 connector（约束 3），所以不能进
    // `connector::create`——那样会拿到 `Unsupported` 并回
    // `channel_error{unsupported}`，页面看到的是「这台设备不支持对话」，
    // 而真相是「我们从来没打算把它做成 connector」。
    if spec.connector == EXTERNAL_CONNECTOR_NAME {
        return on_open_acp(spec);
    }
    match lock().open(&spec, req_of(&spec)) {
        Ok(frames) => Action::Send(frames),
        Err(e) => Action::Send(vec![error_frame(&spec.channel_id, e.code(), &e.reason())]),
    }
}

/// `open_channel{connector:"acp"}` → 开一条外部源通道，并把「去起子进程」交给 worker。
///
/// `params` 要 `agent_id`（用哪个 agent）与 `cwd`（会话工作区根）。**缺一个就回
/// `bad_request`**：没有 `agent_id` 不知道该起谁，没有 `cwd` 子进程会在设备进程
/// 自己的 cwd 里跑——那是设备的数据目录，不是用户的工作区，agent 会在那里
/// 建文件。两者都不能回落成「随便挑一个」。
fn on_open_acp(spec: OpenChannel) -> Action {
    let agent_id = spec
        .params
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let cwd = spec
        .params
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if agent_id.is_empty() || cwd.is_empty() {
        // 码用 `Internal`：`ErrorCode` 是封闭集合，M2 不加协议变体
        // （`protocol.md` §4.1「M2 为什么没有新增消息类型」）。不加 `BadRequest`
        // 是为了一个参数校验去改一套两侧共享的枚举——老 Hub 收到不认识的码
        // 会解不开整帧。`reason` 里说清缺什么，那句才是给人看的。
        return Action::Send(vec![error_frame(
            &spec.channel_id,
            ChannelErrorCode::Internal,
            "acp channel needs params.agent_id and params.cwd",
        )]);
    }
    // 配置项全部可选：不配就「用扫描到的可执行文件、无参数、继承环境」。
    // 这是 `agent_config` 那三条端点的空值语义，设备侧不重复定义（`command`
    // 空串 = 用扫描到的那一个，见 `acp.rs` 的 `spawn`）。
    //
    // **这几处刻意写成显式分支而不是 `unwrap_or_default`**：默认值一个都不
    // 「平凡」——空串 = 回落到扫描结果、空列表 = 不追加、`None` = 新建会话，
    // 与「字段形状错了」不是一回事。唯一平凡的两处（`agent_id` / `cwd`）在
    // 上面用 `unwrap_or_default`：缺了它们这条通道直接报错，默认值就是空串。
    let command = match spec
        .params
        .get("command")
        .and_then(serde_json::Value::as_str)
    {
        Some(v) => v.to_string(),
        None => String::new(),
    };
    let args: Vec<String> = match spec
        .params
        .get("args")
        .and_then(serde_json::Value::as_array)
    {
        Some(a) => a
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    };
    let env: std::collections::BTreeMap<String, String> = match spec
        .params
        .get("env")
        .and_then(serde_json::Value::as_object)
    {
        Some(o) => o
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        None => std::collections::BTreeMap::new(),
    };

    // **ACP 入口参数由设备侧的清单给**，不由 Hub 下发：「怎么进 ACP 模式」
    // 是这个 agent 自己的 CLI 契约。不认识的 agent_id 给空列表——那种情况
    // 起子进程本来就会失败在「没有这个可执行文件」，而不是悄悄地跑成
    // 一个普通 CLI 然后在 `initialize` 上永远等。
    let acp_args: Vec<String> = match crate::agent::detect::KNOWN_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
    {
        Some(spec) => spec.acp_args.iter().map(|s| (*s).to_string()).collect(),
        None => Vec::new(),
    };

    let acp_session_id = match spec
        .params
        .get("acp_session_id")
        .and_then(serde_json::Value::as_str)
    {
        Some(v) if !v.is_empty() => Some(v.to_string()),
        // 空串当成「没有」：`None` 与 `Some("")` 在 `open()` 里是「新建」与
        // 「load 一个叫空字符串的会话」两种行为，后者必然失败。Hub 不该发空串，
        // 但设备侧不该因为一个空串就把一次开对话打挂。
        _ => None,
    };

    // 这条会话已经落库的最大 `seq`。设备侧编号从这里续上——**不续号就会静默
    // 丢事件**（`seq` 在每个 ACP 子进程里从 1 重数，`session/load` 建的就是新
    // 进程，于是重放事件带 `seq=1`，与库里已有的 `seq=1` 撞 `UNIQUE` 被
    // `INSERT OR IGNORE` 吞掉；实测见 `routes/conversation.rs` 的同名注释）。
    //
    // **缺字段或形状不对都当 0**，这是有定义的：0 是「一条都没有」，而
    // `Seq::next()` 是 `fetch_add(1) + 1`，起点 0 让第一条是 1。旧 Hub 不下发
    // 这个字段时行为与从前一致（从 1 开始），不会因为升级顺序把对话开挂。
    // 负数同样当 0：`seq` 是本模块自己发的，负值只可能是别人编的。
    let last_seq = match spec
        .params
        .get("last_seq")
        .and_then(serde_json::Value::as_i64)
    {
        Some(v) if v > 0 => v,
        _ => 0,
    };

    let conv = crate::agent::conversation::ConversationSpec {
        channel_id: spec.channel_id.clone(),
        agent_id: agent_id.to_string(),
        cwd: cwd.to_string(),
        command,
        args,
        acp_args,
        env,
        acp_session_id,
        last_seq,
    };
    let frames = lock().open_external(&spec.channel_id, EXTERNAL_CREDIT_START);
    Action::OpenAcp {
        frames,
        channel_id: spec.channel_id,
        spec: Box::new(conv),
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

/// 取通道表的**测试互斥锁**。同样只给测试用。
///
/// 存在的理由：通道表是进程级单例，两个用例同时 `reset_for_tests()` +
/// 写帧会互相踩账，表现为「单跑全绿、全量随机红」。凡是动表的用例都必须
/// 先拿这把锁再重置。它 `pub` 是因为 `agent::conversation` 的帧形状用例
/// 也要动这张表，而它们在另一个模块里。
///
/// **返回的 guard 不实现 `Send`**（底层是 `std::sync::Mutex`），所以跨
/// `await` 持有它会被 clippy 的 `await_holding_lock` 拦下——这是有意的门闩，
/// 不是障碍：真要在 `async` 用例里持有整个用例体，就显式写
/// `#[expect(clippy::await_holding_lock, reason = "…")]` 说明理由。`agent::conversation`
/// 那几个 `#[tokio::test]` 正是这么做的，拿的也是同一条锁（[`table_serial_for_tests`]）。
pub fn table_lock_for_tests() -> std::sync::MutexGuard<'static, ()> {
    table_mutex()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 通道表锁本体。[`table_lock_for_tests`] 与 [`try_table_lock_for_tests`]
/// 都从这一个 `OnceLock` 取，换成两个 `OnceLock` 就成了两把锁、互斥不成立。
fn table_mutex() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// 取通道表的**测试互斥锁**，给 `async` 用例用：`let _guard = ...;` 绑住，
/// 互斥期与用例体同寿；拿到后自己 `reset_for_tests()`。
///
/// 与 [`table_lock_for_tests`] 是**同一条锁**：换成另一种锁类型就会让「同步
/// 用例」与「`async` 用例」各锁各的，互斥随即不成立。
///
/// 契约：**必须绑成命名变量且活到用例结束**（`let _guard = ...`）。写成
/// `drop(table_serial_for_tests())` / `let _ = ...` 会当场释放，互斥归零——
/// 看起来像「拿了锁」，实际是无锁裸奔。
///
/// 调用方的 `#[tokio::test]` 因此要 `#[expect(clippy::await_holding_lock)]`：
/// 那里 guard 跨 `await` 存活是**故意的**（互斥正是要覆盖整个用例体），
/// 运行时是 current-thread，`await` 换不出去也就不会有人在这期间等这把锁。
pub fn table_serial_for_tests() -> std::sync::MutexGuard<'static, ()> {
    table_lock_for_tests()
}

/// 非阻塞地试拿通道表锁：`true` = 此刻没人持有。
///
/// 只给「互斥是否真的成立」这类探针用——`lock()` 问不出这个问题（同一线程
/// 重入会直接自锁，别的线程则会被挂住）。生产路径不调用。
pub fn try_table_lock_for_tests() -> bool {
    // 与 `table_lock_for_tests` 共用同一个 `LOCK`：探针问的必须是被验的那把锁。
    table_mutex().try_lock().is_ok()
}

// 以下一组访问器是**测试与排障 API**：读通道表的内部计数（信用、积压、
// 已发帧数、存活数）。生产路径一个都不调用——worker 与 connector 直接用
// `pump()` / `push()` 的返回值，不需要回头看表里记了什么。
//
// 为什么不加 `#[cfg(test)]`：`crates/node-core/tests/` 下的**集成测试**
// （`channel_flow.rs` / `heartbeat_flow.rs`）也要用它们，而集成测试是另一个
// crate，`#[cfg(test)]` 只在编译本 crate 的测试目标时生效，加了它们就编不过。
// `cfg(test)` 在这里不是「收紧」而是「把集成测试全打断」，所以这一组保持
// `pub`，靠本条说明与 `AGENTS.md` 的「测试与排障 API」条目约束。
//
// 新增成员前先想清楚：它是测试需要的，还是「顺手想看」的？后者会让这张表
// 变成公开内部状态的缺口，而那正是通道表要封装起来的原因。

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

/// 开一条外部源驱动的通道（ACP 对话），返回待发的 `channel_ready`。
///
/// 与 [`on_channel_frame`] 里的 `open_channel` 是**两条路**，不是同一件事：
/// 那条路要 Hub 发起并带 connector 名，这条路由 worker 在 ACP 会话建好之后
/// 自己开。选后者是因为 Hub 侧还不知道这条会话的 agent 是不是真的起来了——
/// 让 Hub 先发 `open_channel` 再等设备回 `unsupported`，等于把「agent 启动
/// 失败」伪装成「这台设备不支持对话」。
pub fn open_external(channel_id: &str) -> Vec<Frame> {
    lock().open_external(channel_id, EXTERNAL_CREDIT_START)
}

/// 把一个 ACP 事件推进对话通道。**只入队**，放行由 [`pump`] 做。
///
/// 一批一起推而不是一条一次：同一批的溢出判定与字节账是同一把锁。
///
/// # 为什么返回 `Option<()>` 而不是 `Option<Vec<Frame>>`
///
/// 调用方（ACP 读循环）**没有发送能力**——它跑在独立 task 里，手里没有
/// `Established`。第一版返回帧，调用方用 `let _ =` 接住，结果整条对话一个
/// 字节都到不了页面。让类型上就拿不到帧，那个 bug 不会再被写出来。
///
/// `None` = 通道不在了，这批事件丢掉（已裁决「不承诺补齐」）。
pub fn push_external(channel_id: &str, payloads: Vec<serde_json::Value>) -> Option<()> {
    lock()
        .push_external(channel_id, payloads, MAX_BACKLOG_BYTES)
        .ok()
}

/// 取走某条对话通道的入站消息（Hub 发来的用户消息）。
pub fn take_inbound(channel_id: &str) -> Vec<serde_json::Value> {
    lock().take_inbound(channel_id)
}

/// ACP 起进程失败：关通道并把理由回给 Hub。
pub fn fail_external(channel_id: &str, reason: &str) -> Vec<Frame> {
    lock().fail_external(channel_id, ChannelErrorCode::Internal, reason)
}

/// 当前活着的**外部源通道** id。
///
/// worker 靠它知道「现在有哪些对话会话该收消息、该送事件」。connector 通道
/// 不在里面——它们由 `pump` 主动拉，不需要 worker 记账。
pub fn external_ids() -> Vec<String> {
    lock()
        .channels
        .iter()
        .filter(|(_, e)| e.live && e.resource.is_none())
        .map(|(id, _)| id.clone())
        .collect()
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

    /// 通道表是**进程级单例**（约束 4）：两个用例同时调 `reset()` /
    /// `on_channel_frame()` 会互相踩对方的账。所以每个用例开头都要先拿这把锁，
    /// 拿到之后才 `reset()`——锁只保证互斥，空表由 `reset()` 负责。
    ///
    /// 这把锁**不依赖** `.cargo/config.toml` 的 `RUST_TEST_THREADS = "1"`。
    /// 那个设置只覆盖 cargo 起的测试进程；下面 `the_table_lock_*` 自己起线程
    /// 验互斥，`#[ignore]` 的真机用例也常常被单独拉起——单线程假设一旦不成立，
    /// 空表就会在断言中途被清掉，表现为「单跑全绿、全量随机红」。
    ///
    /// 实现已提到模块级的 `table_lock_for_tests()`：`agent::conversation` 的
    /// `async` 用例也在动这张表，**两处必须是同一把锁**，否则互斥不成立。
    fn table_lock() -> std::sync::MutexGuard<'static, ()> {
        table_lock_for_tests()
    }

    /// 另一条线程若还能拿到这把锁，互斥就是假的——「拿到锁」变成原地丢掉
    /// guard 时，两个用例会同时动表，症状是随机红而非稳定红，极难归因。
    ///
    /// 反向验证：把 `table_serial_for_tests` 改回丢掉 guard，或把调用点写成
    /// `drop(channel::table_serial_for_tests());`，这里都会红。
    #[test]
    fn the_table_lock_is_held_across_the_caller_body() {
        let _guard = table_serial_for_tests();
        let free = std::thread::spawn(try_table_lock_for_tests)
            .join()
            .expect("探测线程不该 panic");
        assert!(!free, "调用方仍持有通道表锁时，别的线程必须拿不到它");
    }

    /// `std::sync::Mutex` 不可重入：guard 真被持有的话，同线程再试也拿不到。
    /// 用非阻塞探测而不是再 `lock()`，否则在坏实现下会把用例挂死而不是报红。
    #[test]
    fn the_table_lock_is_not_reentrant() {
        let _guard = table_serial_for_tests();
        assert!(
            !try_table_lock_for_tests(),
            "std::sync::Mutex 不可重入：持有期内同线程再拿必须失败"
        );
    }

    /// 每个用例都从空表开始：通道表是进程级单例（约束 4）。
    fn reset() {
        reset_for_tests();
    }

    /// 泵一轮，取出**某一条通道**放行的帧。
    ///
    /// 生产路径就是这样取帧的（worker 的 `pump` 臂），所以测试用它而不是
    /// 让 `push_external` 返回帧——后者会让「入队」与「放行」在测试里混成
    /// 一步，而真机上它们是两个 task 里的事。
    fn pumped_frames(channel_id: &str) -> Vec<Frame> {
        pump()
            .into_iter()
            .filter(|(id, _)| id == channel_id)
            .flat_map(|(_, frames)| frames)
            .collect()
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

    // ---- 外部源驱动的通道（ACP 对话）----
    //
    // 这一组测的是本文件新增的能力：**一条没有 connector 实例的通道**。
    // ACP 不能做成 connector（`AGENTS.md` 约束 3），而 `Entry.resource` 早先
    // 是非 `Option`，于是这种通道根本无法表达。

    /// 开一条外部源通道：`channel_ready` 里的 connector 名是标记而非真 connector。
    ///
    /// 断言的第二句是关键的：它证明 `open_external` **没有**去 `connector::create`。
    /// 若哪天有人把它改回走 connector 注册表，这里会因 `Unsupported` 而红。
    #[test]
    fn an_external_channel_opens_without_a_connector() {
        let _guard = table_lock();
        reset();

        let frames = open_external("ch-acp");
        assert_eq!(frames.len(), 1, "只有一条 channel_ready");
        assert_eq!(frames[0].msg_type, types::CHANNEL_READY);
        let ready: protocol::channel::ChannelReady = frames[0].payload().unwrap();
        assert_eq!(
            ready.capabilities["connector"],
            serde_json::json!(EXTERNAL_CONNECTOR_NAME)
        );
        assert!(has_channel("ch-acp"));
        assert_eq!(live_count(), 1);
        assert_eq!(
            credit_of("ch-acp"),
            Some(EXTERNAL_CREDIT_START),
            "外部源通道用自己那档初始信用，不是终端的 32"
        );
    }

    /// 推事件 → `pump` 按信用放行。放行出来的必须是**数据帧**，且载荷原样。
    ///
    /// **取帧必须走 `pump`**，不是走 `push_external` 的返回值——生产路径就是
    /// 这样：ACP 读循环只入队，worker 的 `pump` 臂放行并发送。第一版让
    /// `push_external` 自己 flush 并返回帧，而读循环用 `let _ =` 接住，
    /// 于是真机上整条对话一个字节都到不了页面，而这条测试是绿的（它自己取了
    /// 返回值）。走 `pump` 才能让测试与生产同一条路。
    #[test]
    fn pushing_external_events_releases_data_frames_within_credit() {
        let _guard = table_lock();
        reset();
        open_external("ch-acp");

        push_external(
            "ch-acp",
            vec![
                serde_json::json!({ "sessionUpdate": "agent_message_chunk", "n": 1 }),
                serde_json::json!({ "sessionUpdate": "agent_message_chunk", "n": 2 }),
            ],
        )
        .expect("通道在，必须成功");

        // 入队但还没放行：信用只在 flush 时扣。
        assert_eq!(backlog_of("ch-acp"), 2, "入队等放行");

        let out = pumped_frames("ch-acp");
        assert_eq!(out.len(), 2, "信用够，两条都该放行");
        assert!(out.iter().all(|f| f.msg_type == types::CHANNEL_DATA));
        let d0: protocol::channel::ChannelData = out[0].payload().unwrap();
        let d1: protocol::channel::ChannelData = out[1].payload().unwrap();
        assert_eq!(d0.payload["n"], serde_json::json!(1), "载荷原样，不得改写");
        assert_eq!(d1.payload["n"], serde_json::json!(2));
        assert_eq!(
            credit_of("ch-acp"),
            Some(EXTERNAL_CREDIT_START - 2),
            "每放行一帧扣一个信用"
        );
        assert_eq!(backlog_of("ch-acp"), 0);
    }

    /// **信用打光时事件排队而不是丢**，且补信用后能放出来。
    ///
    /// 这是流控真的在起作用的证明：如果 `push_external` 无视信用地直接吐帧，
    /// 这条用例的 `backlog_of` 会是 0 而 `out.len()` 会是 3。
    #[test]
    fn external_events_queue_up_when_credit_runs_out() {
        let _guard = table_lock();
        reset();
        lock().open_external("ch-tight", 1);

        push_external(
            "ch-tight",
            vec![
                serde_json::json!({ "n": 1 }),
                serde_json::json!({ "n": 2 }),
                serde_json::json!({ "n": 3 }),
            ],
        )
        .expect("通道在");

        let out = pumped_frames("ch-tight");
        assert_eq!(out.len(), 1, "只剩 1 个信用，只能放 1 条");
        assert_eq!(backlog_of("ch-tight"), 2, "剩下两条排队，不丢");
        assert_eq!(credit_of("ch-tight"), Some(0));

        // 补信用后必须放出来
        let back = lock().credit("ch-tight", 5).unwrap();
        assert_eq!(back.len(), 2, "补了信用，队列要能排空");
        assert_eq!(backlog_of("ch-tight"), 0);
    }

    /// **`pump` 对一条外部源通道绝不能去 poll**（没有资源），但**必须 flush**。
    ///
    /// 这条用例防的是一个很容易写出来的 bug：在 `poll_all` 里对
    /// `resource: None` 直接 `continue`。那样代码照样编译、开通道照样成功，
    /// 但待发队列永远等不到放行——ACP 的回复会卡到下一次补信用，用户看到
    /// 的是「问了没反应」。
    #[test]
    fn pumping_an_external_channel_flushes_its_backlog_without_polling() {
        let _guard = table_lock();
        reset();
        lock().open_external("ch-acp", 1);

        push_external(
            "ch-acp",
            vec![serde_json::json!({ "n": 1 }), serde_json::json!({ "n": 2 })],
        )
        .expect("通道在");
        // 第一个信用打掉、第二条排队
        assert_eq!(pumped_frames("ch-acp").len(), 1);
        assert_eq!(backlog_of("ch-acp"), 1, "第二条排着");
        assert_eq!(credit_of("ch-acp"), Some(0), "信用打光");

        // 补信用（Hub 会发 `channel_credit`）。**补信用本身就会 flush**——
        // `credit()` 的注释说了这是必须的（PTY 正是靠这一下才继续推）。所以
        // 排队的帧从它的返回值里出来，不必等下一轮泵。
        let mine = lock().credit("ch-acp", 5).expect("补信用");
        assert_eq!(mine.len(), 1, "补信用必须把排队的帧放出来");
        let d: protocol::channel::ChannelData = mine[0].payload().unwrap();
        assert_eq!(d.payload["n"], serde_json::json!(2));
        assert_eq!(backlog_of("ch-acp"), 0);
        // 队列空了之后再泵一轮：没有帧可放，但通道不许被误关。
        assert!(pumped_frames("ch-acp").is_empty(), "没东西可放就是空的");
        // 没有资源可 poll，通道仍然活着（外部源通道不因泵而关闭）
        assert_eq!(live_count(), 1);
    }

    /// 通道不在了：返回 `None`，**不重建通道**。
    ///
    /// 语义是「把已有的事实送出去」，不是「维持通路存活」。
    #[test]
    fn pushing_to_a_missing_channel_reports_none_and_creates_nothing() {
        let _guard = table_lock();
        reset();
        assert!(push_external("ch-nope", vec![serde_json::json!({ "n": 1 })]).is_none());
        assert!(!has_channel("ch-nope"), "不许顺手重建一条通道");
        assert_eq!(live_count(), 0);
    }

    /// **往一条 connector 通道推 ACP 事件必须被拒**。
    ///
    /// 混淆的后果是页面同时看到一半终端字节、一半对话事件，而且两边都
    /// 以为自己在跟一条正常的通道说话。宁可报错。
    #[test]
    fn pushing_external_events_into_a_connector_channel_is_refused() {
        let _guard = table_lock();
        reset();
        let _ = on_channel_frame(&open_files("ch-files", 4));
        assert!(has_channel("ch-files"));

        assert!(
            push_external("ch-files", vec![serde_json::json!({ "n": 1 })]).is_none(),
            "connector 通道不接受外部源事件"
        );
        assert_eq!(backlog_of("ch-files"), 0, "一个字节都不许混进去");
    }

    /// 外部源通道被关掉时**不得去碰 `resource`**（它是 `None`），且关得干净。
    ///
    /// 反向验证：把 `release` 里的 `if let Some(...)` 改成直接
    /// `entry.resource.as_mut().unwrap().close()` → 这条会 panic 而红。
    #[test]
    fn closing_an_external_channel_needs_no_resource() {
        let _guard = table_lock();
        reset();
        open_external("ch-acp");

        let frames = lock().close("ch-acp", "user closed");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].msg_type, types::CLOSE_CHANNEL);
        assert_eq!(live_count(), 0, "关掉之后不再算活通道");
        assert_eq!(backlog_of("ch-acp"), 0, "待发队列要清干净");
        // 再关一次必须幂等
        let again = lock().close("ch-acp", "user closed");
        assert_eq!(again.len(), 1);
    }

    /// 重开同名外部通道：重置信用与待发队列（§6：每次开都重置，含重开）。
    #[test]
    fn reopening_an_external_channel_resets_credit_and_backlog() {
        let _guard = table_lock();
        reset();
        lock().open_external("ch-acp", 1);
        let _ = push_external(
            "ch-acp",
            vec![serde_json::json!({ "n": 1 }), serde_json::json!({ "n": 2 })],
        );
        // `push_external` 只入队（放行归 `pump`），所以两条都还在队列里。
        assert_eq!(backlog_of("ch-acp"), 2, "入队 2 条，一条都没放行");

        let frames = open_external("ch-acp");
        assert_eq!(frames.len(), 1, "重开只回一条 ready");
        assert_eq!(
            credit_of("ch-acp"),
            Some(EXTERNAL_CREDIT_START),
            "信用打回初始值"
        );
        assert_eq!(backlog_of("ch-acp"), 0, "旧待发不许留");
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

    /// 待发字节上限：**溢出不截断，而是关通道并报错**。
    ///
    /// 「不静默丢字节」是产品承诺（PTY 输出不丢），所以排到上限只能放掉
    /// 这条通道；而放掉必须**说出口**——否则页面只会看到终端不声不响停住。
    ///
    /// **注入一个小上限，不往真 PTY 里灌 8 MiB**：这条闸判的是字节账与
    /// 越限后的三个动作（关通道、报错、释放资源），与「上限具体是多大」无关。
    /// 早先这里靠 `while :; do echo fill; done` 真灌满 8 MiB，实测单独跑约
    /// 14 秒，而全量并发下会涨到越过 30 秒的预算——**那条用例因此随机红**，
    /// 而随机红的用例等于没有用例。注入上限让被测命题与机器吞吐解耦，
    /// 生产默认值（`MAX_BACKLOG_BYTES`）一个字节都没动。
    #[test]
    fn an_unbacked_backlog_closes_the_channel_and_says_why() {
        let _guard = table_lock();
        reset();
        // 真开一条 terminal 通道：闸判在 `queue_frames`，而它是 `poll_all`
        // 与 `on_data` 共用的那条路——所以通道必须是真开起来的。
        let spec = OpenChannel {
            channel_id: "ch-flood".to_string(),
            connector: connector::names::TERMINAL.to_string(),
            params: serde_json::json!({ "shell": "/bin/sh" }),
            credit_start: 1,
        };
        let f = Frame::push(types::OPEN_CHANNEL, 1, serde_json::to_value(&spec).unwrap());
        assert!(matches!(on_channel_frame(&f), Action::Send(_)));

        // 上限给 1：第一帧就越限。判据是「字节账越限」这个动作本身，
        // 不是「攒够多少字节」——那随机器变，正是要摆脱的东西。
        let limit = 1usize;
        let out = vec![connector::OutFrame::new(serde_json::json!({ "k": "v" }))];
        let got = lock().queue_frames("ch-flood", out, limit);
        assert!(
            matches!(got, Err(DataError::Connector(_))),
            "越过待发上限必须报错，而不是静默丢字节：{got:?}"
        );
        assert_eq!(
            live_count(),
            0,
            "溢出之后资源必须被释放，不能留一只僵尸通道"
        );
        assert_eq!(backlog_bytes_of("ch-flood"), 0, "释放要清掉积压字节账");
        // 表里**仍留着**这条通道：`release` 只释放资源，条目要留到随后那帧
        // `close_channel` 对上号（§5：关一条通道不得影响其他通道）。判「资源
        // 放没放」看 `live_count`，不是看表里还有没有这个名字。
        assert!(has_channel("ch-flood"), "条目要留着等 close_channel 对上号");
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
        let got = lock().queue_frames("ch-not-there", out, MAX_BACKLOG_BYTES);
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
    ///
    /// 只为 `poll_all_skips_a_connector_whose_poll_fails` 一条用例存在，
    /// 那条用例把实例直接塞进通道表、只调 `poll_all`。
    // coverage: excluded — `FailingPoll` 夹具只被 `poll_all_skips_a_connector_
    // whose_poll_fails` 一条用例使用，那条用例把实例直接塞进通道表后只调
    // `poll_all`。`open` / `on_data` 是 `Connector` trait 的另外两个方法，
    // 本例没有任何路径会调到：它们是夹具的结构性未用部分，不是漏测。
    // （标注与上面的文档注释之间**不留空行**：`comment_blocks` 把紧邻的整行
    // 注释算成同一段，隔开它就会被算到上一段去，理由文本会串。）
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
                    resource: Some(Box::new(FailingPoll)),
                    credit: 4,
                    live: true,
                    backlog: VecDeque::new(),
                    backlog_bytes: 0,
                    sent: 0,
                    inbound: VecDeque::new(),
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

    // ---- `open_channel{connector:"acp"}` 的参数解码 ----
    //
    // 这一组测的是 `on_open_acp` 把 `params` 翻成 `ConversationSpec` 的那段。
    // 它是**唯一**一条「Hub 说的话 → 设备起哪个子进程」的翻译，而在此之前
    // 没有任何用例走过它：整段只由真实 Hub 开对话时执行，于是参数语义
    // （空串算什么、缺字段算什么、哪些字段设备侧说了算）全靠读代码保证。

    fn open_acp(channel_id: &str, params: serde_json::Value) -> Frame {
        let spec = OpenChannel {
            channel_id: channel_id.to_string(),
            connector: EXTERNAL_CONNECTOR_NAME.to_string(),
            params,
            credit_start: 4,
        };
        Frame::push(types::OPEN_CHANNEL, 1, serde_json::to_value(&spec).unwrap())
    }

    /// 取 `open_channel{acp}` 的 `ConversationSpec`，断言它走到了
    /// `Action::OpenAcp`（而不是某个错误帧）。
    fn acp_spec(params: serde_json::Value) -> crate::agent::conversation::ConversationSpec {
        match on_open_acp(OpenChannel {
            channel_id: "ch-acp".to_string(),
            connector: EXTERNAL_CONNECTOR_NAME.to_string(),
            params,
            credit_start: 4,
        }) {
            Action::OpenAcp { spec, .. } => *spec,
            other => panic!("acp 的 open_channel 必须走到 OpenAcp，实际是 {other:?}"),
        }
    }

    /// 缺 `agent_id` 或 `cwd` 就回错误帧，**不回落成「随便挑一个」**。
    ///
    /// 两条都测：只有一条缺也会误起一个在设备数据目录里乱建文件的 agent。
    #[test]
    fn an_acp_channel_without_an_agent_or_a_cwd_is_refused() {
        let _guard = table_lock();
        reset();
        for (name, params) in [
            ("两个都缺", serde_json::json!({})),
            ("缺 agent_id", serde_json::json!({"cwd": "/tmp"})),
            ("缺 cwd", serde_json::json!({"agent_id": "opencode"})),
            (
                "给的是空串",
                serde_json::json!({"agent_id": "", "cwd": "/tmp"}),
            ),
        ] {
            match on_channel_frame(&open_acp("ch-acp", params)) {
                Action::Send(frames) => {
                    assert_eq!(frames.len(), 1, "{name}：回一帧错误就够了");
                    let f = frames.first().expect("回了一帧");
                    let text = serde_json::to_string(&f.c).unwrap_or_default();
                    assert!(
                        text.contains("agent_id") && text.contains("cwd"),
                        "{name}：理由里要点名缺的是哪个参数，实际 {text}"
                    );
                }
                other => panic!("{name}：应当回错误帧，实际 {other:?}"),
            }
        }
    }

    /// 配置项缺省时的语义：`command` 空串 = 用扫描到的那个、`args` 空 =
    /// 不追加、`env` 空 = 继承环境、`acp_session_id` 空 = 新建会话。
    #[test]
    fn an_acp_channel_without_config_asks_for_the_scanned_binary() {
        let _guard = table_lock();
        reset();
        let spec = acp_spec(serde_json::json!({"agent_id": "opencode", "cwd": "/tmp"}));
        assert_eq!(spec.channel_id, "ch-acp");
        assert_eq!(spec.agent_id, "opencode");
        assert_eq!(spec.cwd, "/tmp");
        assert_eq!(spec.command, "", "空 command = 用扫描到的那一个");
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert_eq!(spec.acp_session_id, None);
        assert_eq!(spec.last_seq, 0);
    }

    /// `acp_args` 由**设备侧清单**给，不由 Hub 下发；不认识的 agent 给空列表。
    #[test]
    fn the_acp_entry_args_come_from_the_device_manifest_not_the_hub() {
        let _guard = table_lock();
        reset();
        let known = acp_spec(serde_json::json!({"agent_id": "opencode", "cwd": "/tmp"}));
        assert!(
            !known.acp_args.is_empty(),
            "opencode 有 ACP 入口，清单里该有 acp_args"
        );

        // Hub 即使塞了 `acp_args` 也不该被采信：怎么进 ACP 模式是这个
        // agent 自己的 CLI 契约。
        let unknown = acp_spec(serde_json::json!({
            "agent_id": "not-a-real-agent",
            "cwd": "/tmp",
            "acp_args": ["--hijacked"],
        }));
        assert!(
            unknown.acp_args.is_empty(),
            "不认识的 agent 给空列表，不是把 Hub 下发的塞进去"
        );
    }

    /// `last_seq` 的三种坏输入都当 0——包括负数与字符串。
    #[test]
    fn a_last_seq_that_is_missing_or_nonsense_starts_from_zero() {
        let _guard = table_lock();
        reset();
        for bad in [
            serde_json::json!(null),
            serde_json::json!(-5),
            serde_json::json!("12"),
            serde_json::json!(0),
        ] {
            let spec = acp_spec(serde_json::json!({
                "agent_id": "opencode", "cwd": "/tmp", "last_seq": bad,
            }));
            assert_eq!(spec.last_seq, 0, "last_seq={bad} 必须当 0");
        }
        let spec = acp_spec(serde_json::json!({
            "agent_id": "opencode", "cwd": "/tmp", "last_seq": 42,
        }));
        assert_eq!(
            spec.last_seq, 42,
            "正的 last_seq 要原样带上，否则会静默丢事件"
        );
    }

    /// 空串的 `acp_session_id` 当「没有」：`Some("")` 会让 `open()` 去 load
    /// 一个叫空字符串的会话，必然失败。
    #[test]
    fn an_empty_acp_session_id_means_a_new_session() {
        let _guard = table_lock();
        reset();
        let empty = acp_spec(serde_json::json!({
            "agent_id": "opencode", "cwd": "/tmp", "acp_session_id": "",
        }));
        assert_eq!(empty.acp_session_id, None);

        let given = acp_spec(serde_json::json!({
            "agent_id": "opencode", "cwd": "/tmp", "acp_session_id": "ses_abc",
        }));
        assert_eq!(given.acp_session_id.as_deref(), Some("ses_abc"));
    }

    /// `args` / `env` 里的**非字符串元素被丢掉**，不是整条拒绝。
    ///
    /// `args` 里的 `null` 交给 `execvp` 就是一次空指针；`env` 的值不是字符串
    /// 同理。丢弃比拒绝好：一个坏元素不该让整场对话打不开。
    #[test]
    fn non_string_args_and_env_entries_are_dropped_not_fatal() {
        let _guard = table_lock();
        reset();
        let spec = acp_spec(serde_json::json!({
            "agent_id": "opencode",
            "cwd": "/tmp",
            "command": "/usr/bin/custom",
            "args": ["--model", 7, null, "fast"],
            "env": {"A": "1", "B": 2, "C": "3"},
        }));
        assert_eq!(spec.command, "/usr/bin/custom");
        assert_eq!(spec.args, vec!["--model".to_string(), "fast".to_string()]);
        assert_eq!(
            spec.env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            vec![("A", "1"), ("C", "3")],
            "值不是字符串的 env 项要丢掉"
        );
    }

    /// `connector:"acp"` 走的是 ACP 那条路，**不进 `connector::create`**：
    /// 进去会拿到 `Unsupported`，页面看到「这台设备不支持对话」，
    /// 而真相是「我们从来没打算把它做成 connector」。
    #[test]
    fn an_acp_open_frame_never_reaches_the_connector_registry() {
        let _guard = table_lock();
        reset();
        let action = on_channel_frame(&open_acp(
            "ch-acp",
            serde_json::json!({"agent_id": "opencode", "cwd": "/tmp"}),
        ));
        assert!(
            matches!(action, Action::OpenAcp { .. }),
            "acp 的 open_channel 必须走 OpenAcp，实际 {action:?}"
        );
    }

    // ---- 外部源通道的入站队列 ----

    /// 入站消息**取走即清空**，且再取是空——`take_inbound` 是 drain 语义。
    ///
    /// 若改成借用/拷贝，worker 每一轮 tick 都会把同一句用户消息再送进 agent，
    /// 页面上表现为「它自己又答了一遍」——最难归因的一类 bug。
    #[test]
    fn take_inbound_drains_the_queue_and_then_yields_nothing() {
        let _guard = table_lock();
        reset();
        let _ = open_external("ch-in");

        let f = data_frame("ch-in", serde_json::json!({ "text": "你好" }));
        let _ = on_channel_frame(&f);

        let first = take_inbound("ch-in");
        assert_eq!(first.len(), 1, "入队的消息要取得到：{first:?}");
        assert_eq!(first[0]["text"], serde_json::json!("你好"), "载荷原样");

        assert!(
            take_inbound("ch-in").is_empty(),
            "取过一次之后必须清空，不能重复投递"
        );
    }

    /// 通道**不存在**时取入站消息 → 空，而不是报错或 panic。
    ///
    /// 调用方（worker）对「通道不在」与「没有消息」的反应完全一样，都是什么
    /// 都不做。报错会让一条刚被关掉的通道在下一轮 tick 里刷一条没人管的日志。
    #[test]
    fn take_inbound_on_an_unknown_channel_is_empty_not_an_error() {
        let _guard = table_lock();
        reset();
        assert!(take_inbound("ch-never-opened").is_empty());

        // 开过又关掉的通道走的是同一个 None 分支（关 = 从表里摘掉）。
        let _ = open_external("ch-closed");
        let _ = on_channel_frame(&close_frame("ch-closed", "done"));
        assert!(
            take_inbound("ch-closed").is_empty(),
            "已关掉的通道不该还有入站消息"
        );
    }

    /// 入站积压**超过上限就关通道并报错**，不是无限吞下去。
    ///
    /// 上限与待发同一个量级：用户一条消息超过 8 MiB 不是「对话」而是把文件
    /// 粘进输入框，那种用法该走工作区上传。溢出即关（与出站同一条策略），
    /// 因为**截断**会让用户看到一句被砍掉的话而毫无提示。
    #[test]
    fn too_many_unread_inbound_messages_close_the_channel() {
        let _guard = table_lock();
        reset();
        let _ = open_external("ch-flood");

        // 塞满上限：每一条都要成功入队。
        for i in 0..INBOUND_LIMIT {
            let f = data_frame("ch-flood", serde_json::json!({ "text": format!("m{i}") }));
            let _ = on_channel_frame(&f);
        }
        assert_eq!(
            take_inbound("ch-flood").len(),
            INBOUND_LIMIT,
            "上限内的消息全都该在"
        );

        // 重新填满，再多来一条：这条必须被拒，并且通道被关掉。
        let _ = open_external("ch-flood2");
        for i in 0..INBOUND_LIMIT {
            let f = data_frame("ch-flood2", serde_json::json!({ "text": format!("m{i}") }));
            let _ = on_channel_frame(&f);
        }
        let over = data_frame("ch-flood2", serde_json::json!({ "text": "one too many" }));
        // 溢出走 `DataError::Connector`：`dispatch` 把它翻成一帧 `channel_error`
        // 发回 Hub（而不是 `Action::End`——那条是协议解析失败的出口）。
        match on_channel_frame(&over) {
            Action::Send(frames) => {
                assert_eq!(frames.len(), 1, "溢出要回恰好一帧：{frames:?}");
                assert_eq!(frames[0].msg_type, types::CHANNEL_ERROR, "要说得出口");
            }
            other => panic!("入站溢出必须回 channel_error，实际 {other:?}"),
        }
        assert!(
            !external_ids().contains(&"ch-flood2".to_string()),
            "溢出的通道必须从活通道表里摘掉，否则它会一直占着槽位"
        );
    }
}
