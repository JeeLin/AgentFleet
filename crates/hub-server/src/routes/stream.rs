//! `/api/stream`（SSE）与四个上行端点：页面侧实时数据面（子任务 5a）。
//!
//! ## 它与设备 `/ws` 是两条物理上分开的通道
//!
//! - 设备侧：[`crate::routes::ws`]，`/ws`，**免 session**，走 challenge/签名认证，
//!   并且 [`crate::routes::ws::upgrade`] 里的 `reject_origin()` 拒一切带 `Origin`
//!   的请求。
//! - 页面侧：本模块，`/api/*`，**要 session cookie + CSRF**（状态变更方法），
//!   并且**允许 `Origin`**（见下）。
//!
//! 帧格式、认证方式、流控策略三者互不复用：设备侧说的是 `protocol::Frame`，
//! 页面侧说的是 SSE 事件与 JSON。
//!
//! ## `reject_origin` 的豁免边界（本模块必须写清的那条注释）
//!
//! 设备 `/ws` 拒 `Origin`，理由是「node 是原生程序，从不发 `Origin`；浏览器一定发，
//! 所以『有没有这个头』就是『这是不是浏览器』的判据」。
//!
//! 本路由豁免这条检查，**不破坏那层防御**，理由三条：
//!
//! 1. 豁免的是**本路由自己**的 Origin 检查，而本路由**根本不设** Origin 检查
//!    ——豁免一个不存在的检查不构成放宽。真正的防线在本模块之外：
//!    [`crate::middleware_auth`] 仍要求 `af_session` cookie，而
//!    `routes::setup::session_cookie_header` 把那个 cookie 标成 `SameSite=Strict`，
//!    所以跨站页面根本无法让浏览器带上它去建这条流（无 cookie → 401）。
//! 2. 浏览器**必然**带 `Origin`，对这条路由做值比对或一律拒绝，得到的唯一结果是
//!    「控制台完全用不了」，而没有任何安全收益。
//! 3. 与设备侧的隔离是**物理**的：即便一个恶意站点能建这条流，它拿到的也只是
//!    同一登录用户自己订阅的事件流，而 `disconnect` / `auth` 这些控制帧仍只在
//!    设备 `/ws` 上、且那里 `Origin` 照旧被拒。浏览器能看事件，碰不到设备连接。
//!
//! ## 断线语义
//!
//! SSE 的 `Last-Event-ID`：断线重连带上最后一个事件序号，本模块从**有界回放缓冲**
//! 里补齐 `> last` 的事件（[`Streams::subscribe`]）。断开**不等于**会话结束：
//! 通道在 Hub 侧按 `(node_id, channel_id)` 存活，断的只是这条订阅。
//!
//! 上行端点**无状态**：不排队、不重发。断线期间的 `/send` 由前端丢弃并在 UI 上标
//! 「有 N 条已丢」——`protocol.md` §7 的可靠投递只覆盖带 `id` 的消息，
//! `/send` 是裸字节，排队重发会让字符乱序灌进 PTY（见里程碑子任务 5a 的口径）。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures_util::StreamExt as _;
use parking_lot::Mutex;
use protocol::frame::Frame;
use protocol::message::types;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::routes::api::Authenticated;
use crate::{err, ApiError, AppState};

/// 回放缓冲与 broadcast 通道的容量（事件条数）。
///
/// **有界是刻意的**：`Last-Event-ID` 的续收只承诺「一小段窗口内不丢」，
/// 而一条控制台终端的输出速率轻易超过每秒几百条。无界缓冲等于把 Hub 的
/// 内存变成攻击面——而 `protocol.md` §6.1 的整条立场就是「Hub 侧必须有
/// 不依赖对方配合的硬上限」。订阅者读得太慢会被 broadcast 甩掉（`Lagged`），
/// 那种情况下前端标注「已丢 N 条」而不是让 Hub 无限兜着。
pub const REPLAY_CAPACITY: usize = 256;

/// 一个待发的 SSE 事件。
///
/// `event` 与 `data` 都**已经成形**：序列化在这里做完，SSE 写出层不做业务判断，
/// 而 `Envelope` 因此可以用普通 `VecDeque` 回放（它必须能整条存下来）。
#[derive(Debug, Clone)]
pub struct Envelope {
    /// 递增序号（`Last-Event-ID` 就是它）。
    pub id: u64,
    /// 事件名：`device.status` / `channel.frame` / `session.state`。
    pub name: &'static str,
    /// 事件数据（JSON 文本）。
    pub data: String,
}

impl Envelope {
    /// 转成 axum 的 SSE 事件。
    ///
    /// `id` 一并带上：它就是下一轮 `Last-Event-ID` 的取值，断线续收全靠它。
    fn to_event(&self) -> Event {
        Event::default()
            .id(self.id.to_string())
            .event(self.name)
            .data(self.data.clone())
    }
}

struct Inner {
    tx: tokio::sync::broadcast::Sender<Envelope>,
    /// 回放缓冲与下一个事件序号。**同一把锁**。
    ///
    /// 序号分配、入回放、投递必须是一个整体：否则「已发布的最大序号」与
    /// 「缓冲里实际有什么」可能对不上——订阅者据此算出的续收窗口就会漏事件。
    bus: Mutex<Bus>,
}

/// 事件总线的可变部分。
struct Bus {
    /// 下一个要分配的序号（从 1 起）。
    next_id: u64,
    /// 有界回放缓冲。
    replay: VecDeque<Envelope>,
}

/// SSE 事件总线。
///
/// **纯内存、按进程**：Hub 重启后订阅全部消失，而那正是应当发生的事——
/// 页面重新拉一次 `/api/*` 列表即可，没有任何「必须续上」的持久态
/// （通道账本本身也只在连接存活期间有效，`protocol.md` §7.1）。
#[derive(Clone)]
pub struct Streams {
    inner: Arc<Inner>,
}

impl Default for Streams {
    fn default() -> Self {
        Self::new()
    }
}

impl Streams {
    pub fn new() -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(REPLAY_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                tx,
                bus: Mutex::new(Bus {
                    // 序号从 1 开始：`Last-Event-ID: 0` 与「没有这个头」同义。
                    next_id: 1,
                    replay: VecDeque::new(),
                }),
            }),
        }
    }

    /// 广播一个事件，回它的序号。
    ///
    /// **`data` 用 `impl Into<String>` 而不是 `serde_json::Value`**：调用方
    /// （`ws.rs` 那几处）手里已经是 `Frame`，序列化给它比在这里重新拼一个
    /// JSON 对象更不容易出错，也让本模块不必知道任何协议形状。
    pub fn publish(&self, name: &'static str, data: impl Into<String>) -> u64 {
        // **「分配序号 + 入回放 + 投递」在同一把锁里做完**：`subscribe` 在锁内
        // 做「读快照 + 订阅实时流」，两条临界区因此互斥。分开做的话，
        // 发布方在两步之间被切走时，新订阅者会在快照与实时流**各收一次**
        // （终端把同一段输出画两遍），反向交错时该事件**永久丢失**且不产生
        // `resync` 注释帧（那条只在 `Lagged` 时发）。
        let mut bus = self.inner.bus.lock();
        let id = bus.next_id;
        bus.next_id += 1;
        let env = Envelope {
            id,
            name,
            data: data.into(),
        };
        while bus.replay.len() >= REPLAY_CAPACITY {
            bus.replay.pop_front();
        }
        bus.replay.push_back(env.clone());
        // `send` 失败只说明「此刻一个订阅者都没有」——那是常态（页面没开着），
        // 不是错误，更不该让发布方（ws 读循环）因此中断。
        let _ = self.inner.tx.send(env);
        drop(bus);
        id
    }

    /// 回放缓冲里序号 `> last` 的事件。
    ///
    /// **只服务测试**：生产路径的快照由 [`Streams::subscribe_parts`] 在锁内
    /// 一次取走（分两步做会重发或漏事件），所以这里没有生产调用点。
    #[cfg(test)]
    pub(crate) fn replay_since(&self, last: Option<u64>) -> Vec<Envelope> {
        backlog_since(&self.inner.bus.lock().replay, last)
    }

    /// 当前已用掉的最大事件序号。测试与运维用它核对「续收窗口从哪开始」。
    pub fn last_event_id(&self) -> u64 {
        let bus = self.inner.bus.lock();
        bus.next_id.saturating_sub(1)
    }

    /// 订阅一条事件流。
    ///
    /// `last` 是请求头 `Last-Event-ID`：给定时先补齐回放缓冲里 `> last` 的事件，
    /// 然后才是实时流。不给定时**只**收新事件——`EventSource` 首次连接没有这个头，
    /// 把它当成「从头补一遍」会让每次刷新都收到一屏历史输出（而 PTY 的输出
    /// 是不可重放的字节流：重放等于在终端里灌一遍刚才的命令）。
    ///
    /// 三种结束/降级情形：
    /// - `Lagged`（读得太慢，被 broadcast 甩掉）：发一条**注释帧**而不是错误帧。
    ///   页面把它读成「这中间漏了几条」，由 UI 标注；Hub 不为任何单个慢订阅者
    ///   缓冲无界数据。
    /// - `Closed`（最后一个发送端被丢弃，也就是 Hub 正在关闭）：流结束。
    /// - `Cancelled`（浏览器自己断了）：stream 被 drop，随之结束。
    pub fn subscribe(
        &self,
        last: Option<u64>,
    ) -> futures_util::stream::BoxStream<'static, Result<Event, std::convert::Infallible>> {
        let (backlog, rx) = self.subscribe_parts(last);
        let backlog = backlog.into_iter();
        // `BoxStream` 而不是裸 `impl Stream`：`unfold` 的那个 async 块不是
        // `Unpin` 的（它 `await` 在 `rx.recv()` 上），而 `Sse` 的写出路径
        // 与测试里的逐条读取都要一个能 `Box::pin` 的流。装箱还顺带把
        // 返回类型固定成具名的 `BoxStream`，路由与测试不必每次都写那一长串
        // `impl Stream<Item = Result<Event, Infallible>> + Send`。
        futures_util::stream::unfold((backlog, rx), |(mut backlog, mut rx)| async move {
            if let Some(env) = backlog.next() {
                return Some((Ok(env.to_event()), (backlog, rx)));
            }
            match rx.recv().await {
                Ok(env) => Some((Ok(env.to_event()), (backlog, rx))),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "an SSE subscriber fell behind; it will resync");
                    // 注释帧（`:` 开头）：浏览器与 `EventSource` 都会忽略它，
                    // 而本页面的重连逻辑靠它知道「这段订阅有缺口」。
                    Some((
                        Ok(Event::default().comment(format!("resync: {skipped}"))),
                        (backlog, rx),
                    ))
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            }
        })
        .boxed()
    }

    /// 「读回放快照」与「订阅实时流」这一对，**必须在同一把锁里做完**。
    ///
    /// `publish` 在**同一把锁**里完成「入回放 + 投递」，所以任何一次发布
    /// 要么整体落在本次临界区之前（事件在快照里，且广播投递发生在订阅之前，
    /// 不会再来一遍），要么整体落在之后（不在快照里，只走实时流）。
    /// 两条都不重发、不丢。分开做的话，发布方在两步之间被切走时两个方向
    /// 都会出错：新订阅者在快照与实时流**各收一次**（终端把同一段输出画两遍），
    /// 或者该事件**永久丢失**——而 `Lagged` 没发生，页面连一条 `resync`
    /// 注释帧都收不到。
    fn subscribe_parts(
        &self,
        last: Option<u64>,
    ) -> (Vec<Envelope>, tokio::sync::broadcast::Receiver<Envelope>) {
        let bus = self.inner.bus.lock();
        let backlog = backlog_since(&bus.replay, last);
        let rx = self.inner.tx.subscribe();
        (backlog, rx)
    }

    // ---- 事件构造（形状集中在这里，别在调用点各拼一个） ------------------------
    /// `device.status`：设备在线态变化（连接建立 / 断开）。
    pub fn device_status(&self, node_id: &str, status: &str) -> u64 {
        self.publish(
            "device.status",
            json!({"node_id": node_id, "status": status}).to_string(),
        )
    }

    /// `channel.frame`：设备发来的一帧通道数据（PTY 输出的下行方向）。
    ///
    /// **只推 `channel_data` 的载荷**，不推整帧：`Frame` 的 `seq` / `v` 是
    /// 设备侧 WS 的语义（§2），页面不验签也不重放，暴露它们只会让前端
    /// 以为自己要负责序列号。
    pub fn channel_frame(
        &self,
        node_id: &str,
        channel_id: &str,
        payload: serde_json::Value,
    ) -> u64 {
        self.publish(
            "channel.frame",
            json!({
                "node_id": node_id,
                "channel_id": channel_id,
                "payload": payload,
            })
            .to_string(),
        )
    }

    /// `session.state`：会话状态变化（创建 / 结束）。
    pub fn session_state(&self, session_id: &str, state: &str) -> u64 {
        self.publish(
            "session.state",
            json!({"session_id": session_id, "state": state}).to_string(),
        )
    }

    /// `channel.closed`：通道关掉（面板关、设备回 `close_channel`、删除接口）。
    pub fn channel_closed(&self, node_id: &str, session_id: &str, channel_id: &str) -> u64 {
        self.publish(
            "channel.closed",
            json!({
                "node_id": node_id,
                "session_id": session_id,
                "channel_id": channel_id,
            })
            .to_string(),
        )
    }

    /// `channel.ready`：**设备**确认了通道。页面据此才认为「可以发数据」。
    ///
    /// 与「Hub 已下发 `open_channel`」是两件事（缺陷 4）：Hub 下发只说明
    /// 队列收下了这一帧，设备能不能开还不知道。所以这条事件由设备侧
    /// `channel_ready` 帧到达时发，而不是 `open_channel` 处理完就发。
    pub fn channel_ready(&self, node_id: &str, session_id: &str, channel_id: &str) -> u64 {
        self.publish(
            "channel.ready",
            json!({
                "node_id": node_id,
                "session_id": session_id,
                "channel_id": channel_id,
            })
            .to_string(),
        )
    }
}

/// 回放缓冲里序号 `> last` 的事件。
///
/// 单独一个函数（而不是写两遍 `filter`）是因为它有两个调用点：
/// `Streams::replay_since`（测试查询）与 `Streams::subscribe_parts`
/// （锁内快照）。两处写两份过滤条件是同一个语义两个实现，改一处不会提醒另一处。
fn backlog_since(buf: &VecDeque<Envelope>, last: Option<u64>) -> Vec<Envelope> {
    let Some(last) = last else {
        return Vec::new();
    };
    buf.iter().filter(|e| e.id > last).cloned().collect()
}

/// 一条通道的绑定：**谁**（subject）在**哪台设备**上开的。
///
/// **Hub 侧内存，不落库**：通道本来就是连接生命周期内的东西（§7.1 断开即清），
/// 而「谁开的」只在浏览器侧有意义——落库会让一个已经结束的会话留下
/// 一条再也关不掉的通道记录。
#[derive(Debug, Clone)]
pub(crate) struct ChannelBinding {
    node_id: String,
    session_id: String,
    subject: String,
    /// 设备回过 `channel_ready` 了吗。
    ///
    /// `channel.ready` 只在它为真之后才推给页面：设备侧 `open()` 的第一个
    /// 回帧才是 ready（`node-core/src/channel.rs`），在它之前发下去的
    /// `channel_data` 会被 node 当成未知通道**静默丢弃**。
    acked: bool,
}

/// `channel_id → 绑定`。
///
/// `Clone` 是 **Arc 克隆**：多个句柄指向**同一张表**，而不是各持一份拷贝。
/// 生产代码里「另一个句柄是一份拷贝」才是灾难——一边绑、一边看不见。
#[derive(Clone, Default)]
pub struct Bindings(Arc<Mutex<HashMap<String, ChannelBinding>>>);

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn bind(&self, channel_id: &str, b: ChannelBinding) {
        self.0.lock().insert(channel_id.to_string(), b);
    }

    /// 无条件摘表。**只给「刚亲手 `bind` 过、现在要撤」的回滚路径用**
    /// （`open_channel` 下发失败）——`channel_id` 是本请求刚生成的，不存在
    /// 归属争议。其余地方的摘表一律走 [`unbind_if_owned`]，那里才要验是谁的。
    fn unbind(&self, channel_id: &str) -> Option<ChannelBinding> {
        self.0.lock().remove(channel_id)
    }

    /// 摘表，**但只摘归属 `subject` 的**。
    ///
    /// **归属校验与摘表在同一把锁里**：拆成「先 `unbind`、不符再 `bind` 放回去」
    /// 会开一个窗口——非 owner 的并发请求在那瞬间 `get` 不到这条通道，被当成
    /// 「已经关了」返回 204；放回去这一步还可能覆盖掉期间别人真的 `bind`。
    /// 「不是你的就别动表」才是原子语义该有的形态。
    pub(crate) fn unbind_if_owned(&self, channel_id: &str, subject: &str) -> UnbindOutcome {
        let mut map = self.0.lock();
        let Some(b) = map.get(channel_id) else {
            return UnbindOutcome::NotFound;
        };
        if b.subject != subject {
            return UnbindOutcome::NotYours;
        }
        // 锁没放过，上面 `get` 又刚命中，所以 `remove` 必给出记录；取不到就
        // 当作 NotFound，不为一个不可能的情形 `unwrap`。
        // coverage: excluded — `else` 臂在同一把锁内结构上不可达：上面的
        // `map.get(channel_id)` 刚命中而锁自始至终没放，`remove` 不可能返回
        // `None`（parking_lot 的 `MutexGuard` 不可重入，中间也没有别的代码
        // 路径能碰到这张表）。写成 `if let` 而不是 `unwrap` 只是不给「不可能」
        // 留一条 panic 路径，不是因为这里打算测它。
        let Some(b) = map.remove(channel_id) else {
            return UnbindOutcome::NotFound;
        };
        UnbindOutcome::Removed(b)
    }
}

/// `unbind_if_owned` 的三种结局。**「不存在」与「不是你的」必须分开**：
/// 前者是幂等的成功（面板早就关过），后者是 403——混成一个 `None` 会让
/// 越权关闭看起来像「关过了」。
pub(crate) enum UnbindOutcome {
    Removed(ChannelBinding),
    NotFound,
    NotYours,
}

impl Bindings {
    pub(crate) fn get(&self, channel_id: &str) -> Option<ChannelBinding> {
        self.0.lock().get(channel_id).cloned()
    }

    /// 设备确认了这条通道：置 `acked` 并回 `(node_id, session_id)`。
    ///
    /// 通道不在（超时已被摘掉）→ `None`。标记与读取在同一把锁里，超时任务
    /// 与 ack 同时到达时只有一个能赢，不会出现「ack 刚记账就被超时摘掉」。
    pub(crate) fn mark_acked(&self, channel_id: &str) -> Option<(String, String)> {
        let mut map = self.0.lock();
        let b = map.get_mut(channel_id)?;
        b.acked = true;
        Some((b.node_id.clone(), b.session_id.clone()))
    }

    /// 超时兜底：只摘**还没被设备确认**的通道。
    ///
    /// 返回 `true` 表示确实摘掉了一条（调用方据此推失败信号）；已确认的
    /// 通道原样留着，交给正常的使用/关闭路径。
    pub(crate) fn take_unacked(&self, channel_id: &str) -> bool {
        let mut map = self.0.lock();
        let Some(b) = map.get(channel_id) else {
            return false;
        };
        if b.acked {
            return false;
        }
        map.remove(channel_id);
        true
    }

    /// 某个 node 上被 Hub 侧关掉的通道（关面板时整节点清账之外的补偿）。
    ///
    /// 与 `HubChannels::drop_node` 是同一件事的两个形状，共用
    /// [`hub_core::channels::retain_other_nodes`] 免得两处各写一遍 `retain`。
    pub fn drop_node(&self, node_id: &str) -> usize {
        let mut map = self.0.lock();
        hub_core::channels::retain_other_nodes(&mut map, |_, b| b.node_id == node_id)
    }
}

/// 该节点上报过 `connector` 吗？（子任务 11 的能力硬过滤）
///
/// 返回 `None` 表示「放行」，`Some(上报的列表)` 表示拒绝。
///
/// **「没报 `connectors` 这个键」与「报了但里面没有它」是两件事**：前者是
/// **不知道**（老节点、只报 `agents` 的部分上报者、设备压根没心跳），后者是
/// **知道并拒绝**。把前者折成空数组等于声称「这台设备一个 connector 都没有」，
/// 于是对 terminal/files/exec 一律 409，页面显示「该设备未声明任何 connector」——
/// 而真相是「它还没说」。
///
/// **设备从未上报过心跳时放行**，理由与 `scheduling.md` §3 的硬过滤口径一致：
/// 那里排的是「能力不够的设备再闲也不能用」，排除条件是**已知能力不足**。
/// 「还没上报」不是「已知不足」——刚 enroll 完、第一个心跳（15s 周期）还没到达的
/// 设备完全可能是全能力的，一律拒会把它在头 15 秒里变成一台废机器，而页面看到
/// 的只是「开不了通道」。
///
/// 反过来说，宁可放行也不静默成功：真开了不支持的 connector 时，node 侧的注册表
/// 查不到这个名字，会回一个明确的 `channel_error`——那不是这个门禁该管的
/// （它只管**已声明能力**与请求是否一致），而是 node 自己也说得出话的场景。
///
/// **为什么门禁在开通道而不是建会话**：`POST /api/sessions` 的请求体里**没有**
/// connector 字段（见 [`crate::routes::api::CreateSession`]）——会话不声明
/// connector，connector 是开面板时才指定的。所以这道门禁在当前 API 形状下
/// **只能**落在开通道那一刻。
fn unreported_connector(state: &AppState, node_id: &str, asked: &str) -> Option<Vec<String>> {
    let report = state.reports().get(node_id)?;
    // 键缺失 / 不是数组 → **不知道** → 放行。
    let arr = report
        .capabilities
        .get("connectors")
        .and_then(|v| v.as_array())?;
    let reported: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect();
    if reported.iter().any(|c| c == asked) {
        None
    } else {
        Some(reported)
    }
}

// ---------------------------------------------------------------------------
// 下行：GET /api/stream
// ---------------------------------------------------------------------------

/// `GET /api/stream`（SSE，session 认证）。
///
/// **认证由 [`crate::middleware_auth`] 做**：本路由注册在 `build_app` 的
/// `.layer(require_auth)` **之前**，所以它必须带合法 `af_session` cookie
/// （`tests/stream.rs` 钉住这条：未认证必须 401，404 说明路由没挂上、
/// 200 说明没被认证覆盖）。
///
/// 允许 `Origin`——见模块注释的豁免边界三条理由。这里刻意**没有**任何
/// `reject_origin()` 调用：那是对设备 `/ws` 的判据，对浏览器是反的。
pub async fn stream(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    headers: axum::http::HeaderMap,
) -> Sse<
    axum::response::sse::KeepAliveStream<
        futures_util::stream::BoxStream<'static, Result<Event, std::convert::Infallible>>,
    >,
> {
    let last = crate::routes::last_event_id(&headers);
    // coverage: excluded — `subject = %auth.subject()` 这一行被 llvm-cov 记为 miss。
    // 本 crate 里三处 `%auth.subject()` 字段行全部如此（另两处：`open_channel`、
    // `api::create_session`），而同一宏里 `%字段` 与 `?局部变量` 的行都覆盖——
    // 即「以 `%方法调用()` 记录的字段行」这一类被工具漏记，不是这条分支没执行。
    tracing::info!(
        subject = %auth.subject(),
        last_event_id = ?last,
        "browser subscribed to the event stream"
    );
    // `keep_alive` 会把流包成 `KeepAliveStream<S>`，所以返回类型得是「加过保活」
    // 那个——不是裸 `Sse<S>`。写成裸的那个会在编译期报一个看不出原因的类型不匹配。
    Sse::new(state.streams().subscribe(last)).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// 上行 1/4：POST /api/channels
// ---------------------------------------------------------------------------

/// `POST /api/channels` 的请求体。
///
/// `session_id` 而不是 `node_id`：会话在创建时选定设备并锁定工作区
/// （设计规范 §7.0），面板从这里开的通道必然属于那个会话的设备——
/// 让前端再传一遍 `node_id` 就多了一条能自相矛盾的输入。
#[derive(Debug, Deserialize)]
pub struct OpenChannel {
    pub session_id: String,
    pub connector: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// `POST /api/channels` 的响应（201）。
#[derive(Debug, Serialize)]
pub struct OpenedChannel {
    pub channel_id: String,
    /// 通道所在设备。页面据此把帧对上设备，不必再查一次会话。
    pub node_id: String,
    pub session_id: String,
}

/// 开一条通道：Hub 生成 `channel_id`，转发 `open_channel` 给设备。
///
/// **`channel_id` 由本模块生成**（[`identity::random_id`]，22 字符 base64url），
/// 而不是按 `(node_id, 序号)` 拼：拼出来的 id 要靠 Hub 记账去查「这是哪台设备的
/// 第几条」，而页面拿到的 id 应当自带唯一性——不然两个标签页各开一条通道时，
/// 前端要么 join 要么猜。落到协议帧上时**仍然按 `(node_id, channel_id)` 记账**
/// （约束 4）：`channel_id` 只在设备内唯一这件事在设备侧照旧成立。
///
/// 错误口径（与 `POST /api/sessions` 对齐，前端只写一套分支）：
/// - 会话不存在 → **404**；
/// - 会话已结束 → **409 `session_ended`**（存在但不能用，与 404 不是一回事）；
/// - 设备不在线 → **409 `device_offline`**（页面要显示「为什么不行」）；
/// - 写侧队列打满 → **503 `device_busy`**（设备读不过来，不是页面错了）。
pub async fn open_channel(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    req: Request,
) -> Result<Response, ApiError> {
    let body = crate::routes::read_body(req).await?;
    let body: OpenChannel =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request(err::BAD_BODY))?;
    let opened = open_device_channel(
        &state,
        &auth,
        &body.session_id,
        &body.connector,
        body.params,
        true,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(opened)).into_response())
}

/// 开一条通道并把 `open_channel` 下发到设备。**两条路由共用这一份实现**：
/// 页面直连 connector（[`open_channel`]）与对话通道
/// （`routes::conversation::open`）。
///
/// `check_reported` 区分两者的能力门禁：
/// - connector 通道传 `true`：设备没上报这个 connector 就拒。connector 是
///   **可选能力**，没上报过不代表不支持，但既然上报里没有它，下发一帧注定
///   被拒的 `open_channel` 只会让页面在 `channel.ready` 之后才知道失败。
/// - 对话通道传 `false`：`acp` **不是 connector**（`AGENTS.md` 约束 3），
///   它永远不会出现在设备的 connector 上报里。按 connector 去过滤等于
///   永远开会话失败。它该有的门禁是「agent 可用」，那一条由调用方查
///   `agent_runtime` 给出。
///
/// 校验顺序与错误码见 [`open_channel`] 的文档注释；这里多一层
/// `check_reported`，其余不变。
pub(crate) async fn open_device_channel(
    state: &AppState,
    auth: &Authenticated,
    session_id: &str,
    connector: &str,
    params: serde_json::Value,
    check_reported: bool,
) -> Result<OpenedChannel, ApiError> {
    let detail = hub_storage::SessionRepo::new(state.pool().clone())
        .detail(session_id)
        .map_err(|e| ApiError::internal_with("cannot read the session", e))?;
    let Some(detail) = detail else {
        return Err(ApiError::not_found());
    };
    if detail.session.state != hub_storage::SessionState::Running {
        return Err(ApiError::conflict(err::SESSION_ENDED));
    }
    // 设备已被删除时 `node_id` 是 None：这时候连「在线否」都无从谈起，
    // 回 409 而不是 500——会话本身还在（删除语义：删设备不删会话），
    // 只是它的设备没了，页面该显示「这台设备已被移除」。
    let Some(node_id) = detail.workspace.node_id.clone() else {
        return Err(ApiError::conflict(err::DEVICE_REMOVED));
    };
    if state.connections().get(&node_id).is_none() {
        return Err(ApiError::conflict(err::OFFLINE));
    }
    // 能力硬过滤（子任务 11）：设备上报的 connectors 里没有它，就不下发。
    // 下发一帧注定被 node 拒绝的 `open_channel` 会让页面拿到 `channel.ready`
    // 之后才知道失败——那时通道号已经发出去、面板已经显示「已连接」。
    if check_reported {
        if let Some(reported) = unreported_connector(state, &node_id, connector) {
            return Err(ApiError::conflict_with(
                err::NOT_REPORTED,
                || json!({ "connector": connector, "reported": reported }),
            ));
        }
    }

    let channel_id = identity::random_id();
    let params = params_with_workspace_root(params, &detail.workspace.path);
    let open =
        protocol::channel::OpenChannel::with_params(channel_id.clone(), connector, params);
    let fallback = protocol::channel::fallback::open(&channel_id, connector);
    let frame = Frame::push(
        types::OPEN_CHANNEL,
        0,
        protocol::payload_or(&open, fallback),
    );
    // **先记账再下发**：两本账都要在这之前建好，理由不同但同样致命——
    // 1. 绑定表：设备回 ack 可能比 `send_to_device` 返回还快，而
    //    `announce_ready` 要靠这张表找到会话与节点。反过来写的话，快设备会
    //    撞上「Hub 不认识这条通道」，页面则永远等不到 `channel.ready`。
    // 2. 信用账本：设备从 `channel_ready` 的下一帧起就会推 `channel_data`，
    //    而 Hub 侧每帧上行都要在账本里扣一格信用（`channels::on_data`）。
    //    账本没建的话每帧都判「未知通道」，回一帧 `channel_error` 且
    //    **永不回 `channel_credit`**——设备侧信用扣到 0 之后输出在
    //    ~32 帧后永久冻结，而 Hub 静默无痕。
    // 置 `Ready` 是 M0 的口径（Hub 侧没有 connector 协商）：不置的话账本是
    // 死的——`on_data_received` 只对 Ready 的通道记账（协议层 `ready_mut`），
    // 于是设备发来的数据全部回 `NotReady`，两边互等。
    open_ledger(state, &node_id, &channel_id, connector, open.credit_start)?;
    state.bindings().bind(
        &channel_id,
        ChannelBinding {
            node_id: node_id.clone(),
            session_id: session_id.to_string(),
            subject: auth.subject().to_string(),
            acked: false,
        },
    );
    if let Err(e) = send_to_device(state, &node_id, frame) {
        // 下发失败：绑定跟着撤，别在 Hub 里留下一个只有页面知道的通道号。
        state.bindings().unbind(&channel_id);
        return Err(e);
    }
    spawn_ready_timeout(
        state.clone(),
        node_id.clone(),
        channel_id.clone(),
        protocol::channel::READY_TIMEOUT,
    );
    // coverage: excluded — 与 `stream()`（同文件 :490）同一条：`subject = %auth.subject()`
    // 这一行被 llvm-cov 记为 miss，而同宏里 `%字段` 与 `%局部变量` 的行都覆盖——
    // 是「%方法调用()」这一类的工具漏记，不是这条分支没执行。该宏本身由
    // `tests/stream.rs` 的用例真跑过（201 + 断言 channel_id 与 node_id）。
    tracing::info!(
        node_id = %node_id,
        channel_id = %channel_id,
        session_id = %session_id,
        connector = %connector,
        subject = %auth.subject(),
        "browser opened a channel"
    );
    Ok(OpenedChannel {
        channel_id,
        node_id,
        session_id: session_id.to_string(),
    })
}

// ---------------------------------------------------------------------------
// 上行 2/4：POST /api/channels/{id}/send
// ---------------------------------------------------------------------------

/// `POST /api/channels/{id}/send`：把浏览器发来的请求体转成 `channel_data` 下发。
///
/// **无状态裸字节**（模块注释的断线口径）：Hub 不排队、不重发。
///
/// 载荷形状由 **`Content-Type`** 决定（见 [`PayloadKind`]）：
/// - `application/json` → 请求体原样作为 `payload`。`files` 的
///   `{"op":"list"…}` 走这条——`channel_data.payload` 的 schema 本就是
///   connector 定的、Hub 只透传（`protocol.md` §4），所以原样转发才是对的。
/// - `application/octet-stream` → `{"bytes": <文本>}`，与 node 侧 terminal
///   connector 的载荷形状一致（`node-core/src/connector/terminal.rs`）。
///
/// 声明了 JSON 却解析不出来 → **400**：把它悄悄降级成字节，会让一次写错的
/// 文件请求变成一串打进 PTY 的字面文本，比报错难查得多。
pub async fn send_bytes(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    Path(channel_id): Path<String>,
    req: Request,
) -> Result<Response, ApiError> {
    let kind = payload_kind(
        req.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    );
    let body = crate::routes::read_body(req).await?;
    let payload = payload_of(kind, &body)?;
    let binding = owned_channel(&state, &channel_id, auth.subject())?;
    send_channel_data(&state, &binding.node_id, &channel_id, payload)
}

// ---------------------------------------------------------------------------
// 上行 3/4：POST /api/channels/{id}/resize
// ---------------------------------------------------------------------------

/// 终端尺寸。
///
/// **`u32` 而不是 `u16`**：node 侧 terminal connector 用 `u16`（`TIOCSWINSZ`），
/// 超范围的值在那一侧会被 serde 拒掉，报成一句对用户毫无用处的话。
/// Hub 这一层先拒并说清「col/row 超出范围」，页面才能把它显示成参数错误。
#[derive(Debug, Deserialize)]
pub struct Resize {
    pub cols: u32,
    pub rows: u32,
}

/// `POST /api/channels/{id}/resize`：转成一条**控制帧式的** `channel_data`。
///
/// **不新增协议消息类型**（约束 14）：resize 复用 `channel_data`，载荷是
/// `{"resize": {"cols": N, "rows": N}}`——与 node 侧 terminal connector 的
/// 下行载荷逐字一致（`terminal.rs` 的 `InFrameRaw`）。
pub async fn resize(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    Path(channel_id): Path<String>,
    req: Request,
) -> Result<Response, ApiError> {
    let body = crate::routes::read_body(req).await?;
    let size: Resize =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request(err::BAD_BODY))?;
    // 上界与 u16 对齐；下界 1×1（0 会被内核当「未设置」，见 terminal.rs 的 MIN_DIM）
    const MAX_DIM: u32 = u16::MAX as u32;
    if size.cols == 0 || size.rows == 0 || size.cols > MAX_DIM || size.rows > MAX_DIM {
        return Err(ApiError::bad_request(err::INVALID_SIZE));
    }
    let binding = owned_channel(&state, &channel_id, auth.subject())?;
    send_channel_data(
        &state,
        &binding.node_id,
        &channel_id,
        json!({"resize": {"cols": size.cols, "rows": size.rows}}),
    )
}

// ---------------------------------------------------------------------------
// 上行 4/4：DELETE /api/channels/{id}
// ---------------------------------------------------------------------------

/// `DELETE /api/channels/{id}`：关通道。
///
/// **幂等**：关一条已经在 Hub 侧摘掉的通道仍回 204。前端「关面板」与
/// 「切 tab」会连着打两次（`lib/channel.ts` 的懒建立 / 关面板才关），
/// 让第二次变成 404 只会逼前端写一个「先查再关」的往返。
pub async fn close_channel(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    Path(channel_id): Path<String>,
) -> Result<Response, ApiError> {
    let binding = match state
        .bindings()
        .unbind_if_owned(&channel_id, auth.subject())
    {
        // 幂等：Hub 侧已经没有了（面板早就关过，或设备掉线时整节点清掉了）
        UnbindOutcome::NotFound => return Ok(StatusCode::NO_CONTENT.into_response()),
        UnbindOutcome::NotYours => return Err(ApiError::forbidden(err::FORBIDDEN)),
        UnbindOutcome::Removed(b) => b,
    };
    let closing = protocol::channel::CloseChannel {
        channel_id: channel_id.clone(),
        reason: "closed by the console".to_string(),
    };
    let frame = Frame::push(
        types::CLOSE_CHANNEL,
        0,
        protocol::payload_or(
            &closing,
            protocol::channel::fallback::close(&channel_id, "closed"),
        ),
    );
    // 设备已经不在 → 照样 204：Hub 侧的通道账本随连接一起清掉了（§7.1），
    // 没有需要通知的对象。
    //
    // **但「队列满」不能当成「设备不在」**：那是设备还在、只是读不过来。
    // 一并吞掉会让页面认为通道已关，而设备侧的 PTY 继续跑、继续吃信用。
    // 所以只有 `OFFLINE` 被忽略，其余原样返回（`BUSY` → 503，与开通道时同口径）。
    match send_to_device(&state, &binding.node_id, frame) {
        Ok(()) => {}
        Err(e) if e.code() == err::OFFLINE => {
            tracing::debug!(
                node_id = %binding.node_id,
                channel_id = %channel_id,
                "the device is already gone; nothing to notify"
            );
        }
        Err(e) => {
            // 那一帧 `close_channel` 没发出去，通道在设备上**还开着**：
            // 把归属放回去，页面重试 DELETE 才关得掉它（否则重试会拿到
            // 幂等的 204，页面以为关了而设备的 PTY 还在跑）。
            state.bindings().bind(&channel_id, binding);
            return Err(e);
        }
    }
    state
        .streams()
        .channel_closed(&binding.node_id, &binding.session_id, &channel_id);
    tracing::info!(
        node_id = %binding.node_id,
        channel_id = %channel_id,
        "browser closed a channel"
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// 共用件
// ---------------------------------------------------------------------------

/// `/send` 载荷的三种来源（见 [`send_bytes`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadKind {
    /// `application/json`：请求体就是 connector 的载荷。
    Json,
    /// `application/octet-stream`：请求体是 PTY 裸字节。
    Bytes,
    /// 旧客户端：没有 Content-Type，或是个无关的媒体类型。
    ///
    /// **不能一律当字节**：`scripts/e2e-m1.sh` 的 `/send` 用 curl
    /// `--data-binary`（curl 给它补 `application/x-www-form-urlencoded`），
    /// 而同一条路既要发终端裸字节，又要发 files 的 `{"op":…}`。把它当字节
    /// 会让文件取数在到达 connector 之前就变成一串打进 PTY 的字面文本——
    /// 那正是缺陷 2 最难查的形态。所以这一档按内容判断：JSON 对象原样转发，
    /// 其余仍按字节。
    Legacy,
}

/// 从 `Content-Type` 读载荷种类（媒体类型大小写不敏感，参数忽略）。
fn payload_kind(content_type: Option<&str>) -> PayloadKind {
    let Some(raw) = content_type else {
        return PayloadKind::Legacy;
    };
    let mime = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime == "application/json" || mime.ends_with("+json") {
        PayloadKind::Json
    } else if mime == "application/octet-stream" {
        PayloadKind::Bytes
    } else {
        PayloadKind::Legacy
    }
}

/// 把请求体落成 `channel_data.payload`。
fn payload_of(kind: PayloadKind, body: &[u8]) -> Result<serde_json::Value, ApiError> {
    match kind {
        PayloadKind::Json => {
            serde_json::from_slice(body).map_err(|_| ApiError::bad_request(err::BAD_BODY))
        }
        PayloadKind::Bytes => Ok(json!({ "bytes": utf8_body(body)? })),
        PayloadKind::Legacy => match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(value @ serde_json::Value::Object(_)) => Ok(value),
            _ => Ok(json!({ "bytes": utf8_body(body)? })),
        },
    }
}

/// 字节载荷必须是 UTF-8：把 `�` 悄悄塞进用户的 keystroke 比报错难查得多。
fn utf8_body(body: &[u8]) -> Result<&str, ApiError> {
    std::str::from_utf8(body).map_err(|_| ApiError::bad_request(err::BAD_BODY))
}

/// 把会话的工作区根写进 `open_channel.params`（**覆盖**同名字段）。
///
/// 覆盖而不是「缺省才补」：node 侧的越界强制（`connector/files.rs` 的
/// `resolve_within`）只认这个 root 做判据。允许页面自带就等于让页面传
/// `"/"` 取消边界，约束 3 的「边界强制在 node 侧」会变成一句空话。
/// 真相源是会话的只读工作区（`path_locked` 恒 true），不是请求体。
///
/// `params` 不是对象（缺省 / 字符串 / 数组）时落成只含根的对象：
/// `OpenReq` 的 `#[serde(flatten)]` 要的是对象，留下原值只会让 node
/// 解析失败并回落成「没有工作区」。
fn params_with_workspace_root(params: serde_json::Value, root: &str) -> serde_json::Value {
    let mut map = match params {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    map.insert("workspace_root".to_string(), json!(root));
    serde_json::Value::Object(map)
}

/// 设备确认了通道（`channel_ready` 到达）：这一条才是页面可以开始发数据的信号。
///
/// `channel.ready` 在这里发而不是在 `open_channel` 返回时发——设备侧的
/// `open()` 第一个回帧就是 `channel_ready`（`node-core/src/channel.rs`），
/// 在它之前发下去的 `channel_data` 会被 node 当成未知通道静默丢弃。
pub(crate) fn announce_ready(state: &AppState, channel_id: &str) {
    let Some((node_id, session_id)) = state.bindings().mark_acked(channel_id) else {
        // 超时已被摘掉，或设备在一个 Hub 没记过账的 id 上回 ack：忽略。
        tracing::debug!(
            channel_id = %channel_id,
            "channel_ready for an unknown or already expired channel"
        );
        return;
    };
    state
        .streams()
        .channel_ready(node_id.as_str(), session_id.as_str(), channel_id);
    tracing::debug!(channel_id = %channel_id, "device confirmed the channel");
}

/// 在 Hub 侧信用账本里开一条通道并置 `Ready`（`protocol.md` §5–§6）。
///
/// **这是账本的唯一建表入口**：`channels.rs` 只做「收到一帧 → 该回什么」，
/// 不建表。写在这里是因为开通道的生产路径就是本函数——设备永不发
/// `open_channel`（那是 Hub→node 的方向），所以建表点放别处就等于
/// 建在一段生产上不可达的代码里。
///
/// 失败回 **500**：这不是页面的错，也不是设备的错，是 Hub 自己记不上账。
/// 此刻帧还没下发（调用方紧接着才 `bind` + `send_to_device`），所以撤掉绑定
/// 就够，不留半个通道。
fn open_ledger(
    state: &AppState,
    node_id: &str,
    channel_id: &str,
    connector: &str,
    credit_start: u32,
) -> Result<(), ApiError> {
    let mut channels = state.channels().write();
    let registry = channels.registry(node_id);
    if let Err(e) = registry.open(channel_id, connector, credit_start, Instant::now()) {
        return Err(ApiError::internal_with("cannot open the channel ledger", e));
    }
    if let Err(e) = registry.mark_ready(channel_id) {
        return Err(ApiError::internal_with("cannot mark the channel ready", e));
    }
    Ok(())
}

/// `READY_TIMEOUT` 到点后清掉还没 ack 的通道（协议 §5：`open` 必须 5s 内有结果）。
///
/// **判据是「此刻还没被确认」，不是「5s 前没能确认」**：定时器醒来时那条
/// 通道可能早就 ack 了、或者被正常关闭 / 设备掉线摘掉了。`take_unacked`
/// 在同一把锁里做这个判断，所以迟到的定时器只会变成空操作——不会给一个
/// 用得好好的面板推一条假的「设备没确认」。
fn spawn_ready_timeout(
    state: AppState,
    node_id: String,
    channel_id: String,
    after: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        expire_unacked(&state, &node_id, &channel_id);
    });
}

/// 设备没在 5s 内确认通道：**在页面上说清楚**，然后解绑。
///
/// 什么都不推的话，面板会永远停在「建立中」——而 Hub 明明知道出事了。
/// 这条失败信号与设备侧的错误走同一条路（`channel_frame` 的
/// `{"op":"error"}`），前端已经按这个形状渲染（`SessionPage.vue` 的
/// `applyFileFrame`），不必为「Hub 自己判的超时」再造一种事件。
fn expire_unacked(state: &AppState, node_id: &str, channel_id: &str) {
    if !state.bindings().take_unacked(channel_id) {
        return;
    }
    tracing::warn!(
        node_id = %node_id,
        channel_id = %channel_id,
        "the device did not confirm the channel in time"
    );
    state.streams().channel_frame(
        node_id,
        channel_id,
        json!({
            "op": "error",
            "code": "timeout",
            "reason": "the device did not confirm the channel within 5s",
        }),
    );
    // 账本也要撤：只撤绑定表的话，这条通道在信用账本里永远停在 `Ready`。
    // 设备此后重连（node_id 不变）并复用同一 `channel_id` 时，`on_data` 会
    // 先在账本里命中这条残留记录——错误从「未知通道」变成「静默扣一个
    // 不存在的账」，而 Hub 的日志里什么都不会出现。
    if let Err(e) = state
        .channels()
        .write()
        .registry(node_id)
        .mark_closed(channel_id)
    {
        // 撤不掉不是异常：设备掉线时 `drop_node` 已经整表清过，这里只是
        // 空操作。记 debug 是因为真正要关心的是「没清掉而设备还在线」。
        tracing::debug!(
            node_id = %node_id,
            channel_id = %channel_id,
            error = %e,
            "could not clear the ledger of a timed-out channel"
        );
    }
    // 设备可能只是慢：晚到的 open 会留下一个再也没人关的 PTY / 工作区句柄。
    let closing = protocol::channel::CloseChannel {
        channel_id: channel_id.to_string(),
        reason: "open timed out".to_string(),
    };
    let frame = Frame::push(
        types::CLOSE_CHANNEL,
        0,
        protocol::payload_or(
            &closing,
            protocol::channel::fallback::close(channel_id, "open timed out"),
        ),
    );
    if let Err(e) = send_to_device(state, node_id, frame) {
        // coverage: excluded — 多行宏的参数行被 llvm-cov 记为 miss（已知现象），
        // 同 `routes/api.rs:348`。该臂本身已由
        // `expiring_an_unacked_channel_survives_a_device_that_will_not_take_the_close`
        // 覆盖（队列满 → 走这里，只留痕不扩散）。
        tracing::debug!(
            channel_id = %channel_id,
            code = %e.code(),
            "could not tell the device to drop a timed-out channel"
        );
    }
}

/// 取通道绑定并校验归属。
///
/// **404 与 403 的分界**：id 根本不存在（含设备掉线后被整节点清掉）→ 404；
/// id 存在但**不是这个 subject 开的** → 403。不把两者混成一个 404 的理由与
/// `ApiError::not_found` 的注释一致：单用户下页面只需要知道「这不能用」，
/// 而一旦有多用户，403 才是「你越权」与「东西不在」之间唯一可区分的信号。
fn owned_channel(
    state: &AppState,
    channel_id: &str,
    subject: &str,
) -> Result<ChannelBinding, ApiError> {
    let Some(b) = state.bindings().get(channel_id) else {
        return Err(ApiError::not_found());
    };
    if b.subject != subject {
        tracing::warn!(
            channel_id = %channel_id,
            subject = %subject,
            "cross-user channel access refused"
        );
        return Err(ApiError::forbidden(err::FORBIDDEN));
    }
    Ok(b)
}

/// 给设备发一帧。
///
/// 队列打满 → **503**：那说明设备读不过来（多半是它自己的数据面队列满了），
/// 这不是页面能修的错，重试有意义；而「设备已经走了」是 409（调用方自行决定）。
fn send_to_device(state: &AppState, node_id: &str, frame: Frame) -> Result<(), ApiError> {
    let Some(handle) = state.connections().get(node_id) else {
        return Err(ApiError::conflict(err::OFFLINE));
    };
    match state.outbound().send(&handle.conn_id, frame) {
        Ok(true) => Ok(()),
        // 表里已经没有这条连接：设备在「查注册表」与「投递」之间断了。
        Ok(false) => Err(ApiError::conflict(err::OFFLINE)),
        Err(crate::conns::PushError::QueueFull) => Err(ApiError::service_unavailable(err::BUSY)),
    }
}

/// 给某条通道发一帧 `channel_data`（上行 2/4 与 3/4 共用）。
fn send_channel_data(
    state: &AppState,
    node_id: &str,
    channel_id: &str,
    payload: serde_json::Value,
) -> Result<Response, ApiError> {
    let data = protocol::channel::ChannelData {
        channel_id: channel_id.to_string(),
        payload,
    };
    let frame = Frame::push(
        types::CHANNEL_DATA,
        0,
        protocol::payload_or(&data, protocol::channel::fallback::data(channel_id)),
    );
    send_to_device(state, node_id, frame)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 测试用：跨模块造一条通道绑定（`ws.rs` 的转发用例）。
#[cfg(test)]
pub(crate) fn test_binding(node_id: &str, subject: &str, acked: bool) -> ChannelBinding {
    ChannelBinding {
        node_id: node_id.to_string(),
        session_id: "s_1".into(),
        subject: subject.into(),
        acked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(subject: &str) -> ChannelBinding {
        test_binding("n_1", subject, true)
    }

    /// 不是自己的通道：403，且**表里那条原样还在**。
    ///
    /// 旧实现是「先 `unbind`、不符再 `bind` 放回去」，中间存在一个窗口，回头
    /// 并发的 owner 请求会 `get` 不到、被当成「已经关了」。这条用例锁的是
    /// 「摘不到就别动表」——它是 TOCTOU 修复唯一可观测的不变式。
    #[test]
    fn unbinding_someone_elses_channel_leaves_it_alone() {
        let b = Bindings::new();
        b.bind("ch_1", bound("alice"));

        assert!(matches!(
            b.unbind_if_owned("ch_1", "bob"),
            UnbindOutcome::NotYours
        ));

        let back = b.get("ch_1").expect("非 owner 的摘表不该碰到表里的记录");
        assert_eq!(back.subject, "alice");
    }

    /// 「不存在」与「不是你的」必须是两种结局：前者 204（幂等），后者 403。
    ///
    /// 混成一个 `None` 会让越权关闭看起来像「关过了」。
    #[test]
    fn a_non_owner_cannot_make_the_binding_briefly_disappear() {
        let b = Bindings::new();
        b.bind("ch_1", bound("alice"));

        let attacker = b.clone();
        let observer = b.clone();
        const ROUNDS: usize = 200_000;

        let attack = std::thread::spawn(move || {
            for _ in 0..ROUNDS {
                // 非 owner：结果必须是 NotYours，且**不许碰到表**
                assert!(matches!(
                    attacker.unbind_if_owned("ch_1", "bob"),
                    UnbindOutcome::NotYours
                ));
            }
        });
        let watch = std::thread::spawn(move || {
            for _ in 0..ROUNDS {
                assert!(
                    observer.get("ch_1").is_some(),
                    "非 owner 的尝试期间，绑定短暂消失了"
                );
            }
        });
        attack.join().expect("panic");
        watch.join().expect("panic");
    }

    /// 不存在、不是你的、是你的 —— 三种结局互不相同。
    #[test]
    fn missing_and_not_yours_are_different_outcomes() {
        let b = Bindings::new();
        assert!(matches!(
            b.unbind_if_owned("ch_gone", "alice"),
            UnbindOutcome::NotFound
        ));

        b.bind("ch_1", bound("alice"));
        assert!(matches!(
            b.unbind_if_owned("ch_1", "bob"),
            UnbindOutcome::NotYours
        ));
    }

    /// owner 本人摘得到，且真的从表里消失。
    #[test]
    fn the_owner_unbinds_and_the_record_is_gone() {
        let b = Bindings::new();
        b.bind("ch_1", bound("alice"));

        assert!(matches!(
            b.unbind_if_owned("ch_1", "alice"),
            UnbindOutcome::Removed(_)
        ));
        assert!(b.get("ch_1").is_none());
    }

    /// 把一条订阅流渲染成 SSE 线上字节，读够 `want` 条事件就停。
    ///
    /// 走 `Sse → Response → Body` 而不是去调 `Event` 的访问器：axum 0.8 的
    /// `Event` **没有任何 getter**（它就是一个字节缓冲），要断言「事件名与
    /// 数据长这样」就只能看线上字节——而那恰恰是浏览器看到的东西。
    ///
    /// **必须带条数上限**：订阅流只在 Hub 关闭时才结束，而 `collect()` 会一路
    /// 等到流结束——把它用在这上面就是一条必然超时的用例（我第一版就是这么
    /// 写挂的）。所以只 `Frame` 到 `want` 条为止：已经够断言了，剩下的
    /// 一律丢掉（HTTP 流的语义里「读够即止」与「读完全部」对调用方没区别）。
    ///
    /// **分帧按「以 `data: ` 开头的那一行」判**，不是按空行：axum 0.8 的
    /// `Event::finalize` 只在末尾补**一个** `\n`，所以一条事件的线上字节是
    /// `id: 1\nevent: x\ndata: y\n`——没有空行。按 `\n\n` 数会数出一条都
    /// 数不到，于是这个循环永远等下去（就是上面那个「写挂」的真正原因）。
    ///
    /// coverage: excluded — `let Some(Ok(frame)) = … else { break }` 这个出口在
    /// 现有用例里不可达：每条用例都读满 `want` 条就停，而 `Streams` 自己握着
    /// 发送端，流从不结束（要它结束得丢最后一个 `Sender`，那是
    /// `the_stream_ends_when_the_last_sender_is_dropped` 的事，那里读的是裸流
    /// 而不是 HTTP body）。这是夹具的收尾，不是被测逻辑的分支。
    async fn render(
        stream: futures_util::stream::BoxStream<'static, Result<Event, std::convert::Infallible>>,
        want: usize,
    ) -> String {
        use http_body_util::BodyExt as _;
        let resp = axum::response::IntoResponse::into_response(Sse::new(stream));
        let mut body = resp.into_body();
        let mut out = String::new();
        while out.lines().filter(|l| l.starts_with("data: ")).count() < want {
            let Some(Ok(frame)) = body.frame().await else {
                // coverage: excluded — 夹具的收尾：`Streams` 自己握着发送端，
                // 流在测试期间从不结束，所以这条 `break` 打不到；它不是被测逻辑的分支。
                break;
            };
            let chunk = frame.into_data().expect("data frame");
            out.push_str(&String::from_utf8_lossy(&chunk));
        }
        out
    }

    /// 一段 SSE 线上字节里**最后一条完整事件**的 `(id, event, data)`。
    ///
    /// 取最后一条而不是第一条：`subscribe(Some(last))` 的回放里前面几条
    /// 是断线期间的历史，断言形状时我们要看的是当前这一条。
    ///
    /// 分帧按「一条事件从 `id: ` 开始」（见 [`render`] 里的口径），不是按空行。
    fn parts(text: &str) -> (String, String, String) {
        let starts: Vec<usize> = text.match_indices("id: ").map(|(i, _)| i).collect();
        let last = starts
            .last()
            .map(|i| &text[*i..])
            .expect("at least one frame");
        parts_of_one(last)
    }

    /// 一条 SSE 事件的线上形状：`id:` / `event:` / `data:` 三行。
    fn parts_of_one(frame: &str) -> (String, String, String) {
        let mut id = String::new();
        let mut name = String::new();
        let mut data = Vec::new();
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("id: ") {
                id = v.to_string();
            } else if let Some(v) = line.strip_prefix("event: ") {
                name = v.to_string();
            } else if let Some(v) = line.strip_prefix("data: ") {
                data.push(v.to_string());
            }
        }
        (id, name, data.join("\n"))
    }

    fn bootstrap(dir: &std::path::Path) -> AppState {
        let pool = hub_storage::db::open_in(dir).unwrap().1;
        AppState::bootstrap_with(
            pool,
            dir,
            hub_core::auth::AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            std::time::Duration::from_millis(50),
            hub_core::auth::LoginRateLimiter::new(10, 60),
        )
        .unwrap()
    }

    fn binding(node: &str, subject: &str) -> ChannelBinding {
        test_binding(node, subject, false)
    }

    /// 当前有几条活跃订阅（测试用来确认「造出来的局面真的是那个局面」）。
    fn inner_receivers(s: &Streams) -> usize {
        s.inner.tx.receiver_count()
    }

    /// 带 `Last-Event-ID` 的订阅者能把**先前**发布的事件收齐，
    /// 形状是 `(id, event:, data:)`。
    ///
    /// 用 `Some(0)` 而不是 `None`：不带断点的订阅是**纯实时**的（见
    /// [`Streams::subscribe`]），先发布后订阅就什么都收不到——那正是
    /// 「首次连接不回放历史」这条口径要的效果，所以这里必须靠回放才能拿到。
    #[tokio::test]
    async fn a_subscriber_with_a_last_event_id_receives_a_published_event() {
        let s = Streams::new();
        let id = s.device_status("n_1", "online");
        assert_eq!(id, 1);
        assert_eq!(s.replay_since(Some(0)).len(), 1);
        let (i, name, data) = parts(&render(s.subscribe(Some(0)), 1).await);
        assert_eq!(i, "1", "id 就是 Last-Event-ID 的取值来源");
        assert_eq!(name, "device.status");
        assert_eq!(data, r#"{"node_id":"n_1","status":"online"}"#);
    }

    /// 事件名就是页面契约：`device.status` / `channel.frame` / `session.state`。
    #[test]
    fn the_three_page_event_names_are_exactly_these() {
        let s = Streams::new();
        s.device_status("n_1", "online");
        s.channel_frame("n_1", "ch-1", json!({"bytes": "hi"}));
        s.session_state("s_1", "running");
        let names: Vec<&str> = s.replay_since(Some(0)).iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec!["device.status", "channel.frame", "session.state"]
        );
        let frames: Vec<String> = s
            .replay_since(Some(0))
            .iter()
            .filter(|e| e.name == "channel.frame")
            .map(|e| e.data.clone())
            .collect();
        // 通道帧带上 (node_id, channel_id)，前端据此把帧对上设备
        assert_eq!(
            frames.first().map(String::as_str),
            Some(r#"{"channel_id":"ch-1","node_id":"n_1","payload":{"bytes":"hi"}}"#),
            "字段顺序由 serde_json 的键排序决定（字典序），别在别处重排"
        );
    }

    /// 序号单调递增，且 `last_event_id()` 是最后用掉的那个。
    #[test]
    fn event_ids_are_monotonic() {
        let s = Streams::new();
        assert_eq!(s.last_event_id(), 0, "还没发过就是 0（与「无头」同义）");
        let a = s.device_status("n_1", "online");
        let b = s.session_state("s_1", "running");
        assert!(b > a);
        assert_eq!(s.last_event_id(), b);
    }

    /// `Default` 与 `new()` 等价：clippy 的 `new_without_default` 要求有它，
    /// 而「有两条构造路径」本身就是一条漂移风险。
    #[test]
    fn the_default_bus_is_the_same_as_a_fresh_one() {
        let s = Streams::default();
        assert_eq!(s.last_event_id(), 0);
        s.device_status("n_1", "online");
        assert_eq!(s.last_event_id(), 1);
    }

    /// `Last-Event-ID` 续收：只补 `> last` 的，不重发已经收过的。
    #[test]
    fn resuming_with_last_event_id_replays_only_the_newer_ones() {
        let s = Streams::new();
        s.device_status("n_1", "online");
        let b = s.session_state("s_1", "running");
        let c = s.session_state("s_1", "ended");
        let replayed: Vec<u64> = s.replay_since(Some(b)).iter().map(|e| e.id).collect();
        assert_eq!(replayed, vec![c], "已收过的那条不许重发");
    }

    /// 没带 `Last-Event-ID` → 一条历史都不补（首次连接不该收到一屏旧输出）。
    #[test]
    fn subscribing_without_last_event_id_replays_nothing() {
        let s = Streams::new();
        s.device_status("n_1", "online");
        assert!(s.replay_since(None).is_empty());
    }

    /// 回放缓冲**有界**：超过容量就丢最老的，而最新一条一条都不能少。
    #[test]
    fn the_replay_buffer_is_bounded_and_keeps_the_newest() {
        let s = Streams::new();
        for i in 0..(REPLAY_CAPACITY + 20) {
            s.session_state("s_1", &format!("running-{i}"));
        }
        let buf = s.replay_since(Some(0));
        assert_eq!(buf.len(), REPLAY_CAPACITY, "缓冲必须封顶");
        assert_eq!(
            buf.last().map(|e| e.id),
            Some(s.last_event_id()),
            "最新一条必须在"
        );
    }

    /// **先订阅、再发布**：这一条走的是实时路径（`broadcast` 那一半），
    /// 而上一条走的是回放路径。两条都要在——它们是同一个函数的两个分支。
    #[tokio::test]
    async fn a_live_subscriber_receives_events_published_after_it_connected() {
        let s = Streams::new();
        let stream = s.subscribe(None);
        s.session_state("s_1", "running");
        let bytes = render(stream, 1).await;
        let (id, name, _) = parts(&bytes);
        assert_eq!(name, "session.state");
        assert_eq!(id, "1");
    }

    /// `Last-Event-ID` 续收时，先补历史再接实时（不重复、不丢失）。
    #[tokio::test]
    async fn a_reconnect_replays_the_gap_and_then_continues_live() {
        let s = Streams::new();
        let a = s.session_state("s_1", "running");
        let b = s.session_state("s_1", "ended");
        // 断开期间又发生了一次
        let c = s.device_status("n_1", "offline");

        s.session_state("s_1", "running-again");
        let bytes = render(s.subscribe(Some(a)), 3).await;
        // 断线期间的那条必须补上
        assert!(
            bytes.contains(&format!("id: {c}\n")),
            "断线期间的那条必须补上：{bytes}"
        );
        assert!(
            bytes.contains(&b.to_string()),
            "刚断线前那条也要补：{bytes}"
        );
        // 补完历史后接的是**新**事件，而不是把 b 补两遍
        assert_eq!(
            bytes.matches(&format!("id: {b}\n")).count(),
            1,
            "已补过的不许再补一次：{bytes}"
        );
    }

    /// 读得太慢的订阅者被 broadcast 甩掉 → 发**注释帧**，不是错误帧。
    ///
    /// `EventSource` 忽略注释帧，而本页面的重连逻辑靠它知道「这段订阅有缺口」——
    /// 发错误帧会让前端以为订阅真的失败了。
    #[tokio::test]
    async fn a_slow_subscriber_gets_a_comment_not_an_error() {
        let s = Streams::new();
        // 先订阅，再灌远超容量的事件：**期间一个字节都不读**——
        // broadcast 的环形缓冲被绕过了两圈，它必然判定 Lagged。
        let mut stream = s.subscribe(None);
        for i in 0..(REPLAY_CAPACITY * 2) {
            s.session_state("s_1", &format!("running-{i}"));
        }
        let ev = stream
            .next()
            .await
            .expect("must not end")
            .expect("infallible");
        // axum 0.8 的 `Event` 没有任何 getter（它就是一个字节缓冲），
        // 所以只能看它自己的 `Debug`：注释帧的缓冲形如 `: resync: 256\n`。
        let debug = format!("{ev:?}");
        assert!(
            debug.contains(r"resync: 256"),
            "必须是 resync 注释帧，实际 {debug}"
        );
        assert!(
            debug.contains(r": resync: 256\n"),
            "注释帧的线上字节是 `: resync: N\\n`（axum 的 finalize 只补一个 \\n）：{debug}"
        );
        assert!(
            !debug.contains("event:"),
            "注释帧不得带事件名（否则前端会把它当一条真事件）：{debug}"
        );
    }

    /// 没有订阅者时发布事件**不是错误**（页面没开着是常态）。
    #[test]
    fn publishing_with_no_subscriber_is_not_an_error() {
        let s = Streams::new();
        assert!(s.device_status("n_1", "offline") > 0);
    }

    /// 整个 Hub 关闭（最后一个发送端被丢弃）→ 流**正常结束**，不是挂死。
    ///
    /// 之所以要专门造这个局面（`Streams` 自己也持有 `tx`）：订阅流在**没有
    /// 任何事件**时是彻底挂住的（这正是一条真 SSE 连接该有的样子——
    /// 一把收集会永远等下去）。所以「Hub 关闭」这个分支不能靠「等它自己结束」
    /// 测出来，只能先把最后一个 `Sender` 丢掉再读一次。
    #[tokio::test]
    async fn the_stream_ends_when_the_last_sender_is_dropped() {
        let s = Streams::new();
        let mut stream = s.subscribe(None);
        assert!(inner_receivers(&s) > 0, "必须真的还有订阅者");
        // 丢的是 `Inner`，而 `s` 自己还握着一份 `Arc`——所以顺带把
        // `Streams` 也丢掉，才能真的让 `Sender` 归零。
        let inner = Arc::clone(&s.inner);
        drop(inner);
        drop(s);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .expect("Hub 关掉后流必须结束，否则页面永远转圈")
                .is_none(),
            "必须结束而不是挂死"
        );
    }

    /// 绑定表：按 channel_id 存取、按 node 清、摘掉再摘是 `None`。
    #[test]
    fn bindings_are_keyed_by_channel_id_and_droppable_per_node() {
        let b = Bindings::new();
        b.bind("c1", binding("n_a", "admin"));
        b.bind("c2", binding("n_b", "admin"));
        assert_eq!(b.get("c1").map(|x| x.node_id.clone()), Some("n_a".into()));
        assert!(b.get("nope").is_none());
        assert_eq!(b.drop_node("n_a"), 1);
        assert!(b.get("c1").is_none(), "掉线设备的通道绑定要一起清");
        assert_eq!(
            b.get("c2").map(|x| x.node_id.clone()),
            Some("n_b".into()),
            "别的设备不得被连坐"
        );
        assert_eq!(b.drop_node("n_missing"), 0);
        assert_eq!(b.unbind("c2").map(|x| x.node_id), Some("n_b".into()));
        assert!(b.unbind("c2").is_none());
    }

    /// `read_body` 超上限 → 400（不是把 1 MiB 悄悄截断）。
    #[tokio::test]
    async fn an_oversized_body_is_rejected_rather_than_truncated() {
        let req = Request::builder()
            .method("POST")
            .uri("/api/channels")
            .body(axum::body::Body::from(
                "x".repeat(crate::WS_MAX_MESSAGE_BYTES + 1),
            ))
            .unwrap();
        let e = crate::routes::read_body(req)
            .await
            .expect_err("must reject");
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        assert_eq!(e.code(), err::BAD_BODY);
    }

    /// `owned_channel`：不存在 → 404；存在但不是这个 subject → 403。
    #[test]
    fn an_unknown_channel_is_404_and_a_foreign_one_is_403() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        assert_eq!(
            owned_channel(&state, "ch-none", "admin")
                .expect_err("404")
                .status(),
            StatusCode::NOT_FOUND
        );
        state.bindings().bind("ch-1", binding("n_a", "admin"));
        assert_eq!(
            owned_channel(&state, "ch-1", "mallory")
                .expect_err("403")
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            owned_channel(&state, "ch-1", "admin")
                .expect("owner")
                .node_id,
            "n_a"
        );
    }

    /// `send_to_device`：设备不在 → 409 `device_offline`（而不是悄悄丢帧）。
    #[test]
    fn sending_to_an_absent_device_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        let e = send_to_device(
            &state,
            "n_ghost",
            Frame::push(types::CHANNEL_CREDIT, 0, json!({})),
        )
        .expect_err("must fail");
        assert_eq!(e.status(), StatusCode::CONFLICT);
        assert_eq!(e.code(), err::OFFLINE);
    }

    /// 连接表里有它、出站队列表里没有它 → **同样 409 `device_offline`**。
    ///
    /// 这是「查注册表」与「投递」之间那一瞬：设备刚断，断开处理器已经把
    /// `conn_id` 从出站表里摘掉，而注册表的清理走的是同一条路径的下一步。
    /// 两个表的清理不是原子的，所以这个窗口真实存在——把它和「压根没连过」
    /// 分开，是因为前者可能发生在一条**合法**请求的中途，而调用方对它只能
    /// 做同一件事：告诉页面设备走了。
    #[test]
    fn sending_to_a_connection_that_left_the_outbound_table_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.connections().insert(
            "n_gone".into(),
            hub_core::connections::ConnHandle {
                conn_id: "c_gone".into(),
                gen: 7,
            },
        );
        // 刻意**不** `outbound().register`：出站表里没有 "c_gone"

        let e = send_to_device(
            &state,
            "n_gone",
            Frame::push(types::CHANNEL_CREDIT, 0, json!({})),
        )
        .expect_err("must fail");
        assert_eq!(e.status(), StatusCode::CONFLICT);
        assert_eq!(e.code(), err::OFFLINE);
    }

    /// 工作区根**覆盖**页面自带的值（约束 3：边界强制在 node 侧）。
    #[test]
    fn the_workspace_root_overrides_whatever_the_page_sent() {
        let v = params_with_workspace_root(json!({"cwd": "/tmp", "workspace_root": "/"}), "/w");
        assert_eq!(
            v["workspace_root"],
            json!("/w"),
            "页面传的 root 不得生效，否则传 \"/\" 就等于取消边界"
        );
        assert_eq!(v["cwd"], json!("/tmp"), "其余参数照旧透传");

        // 非对象 / 缺失也要落成对象：`OpenReq` 的 flatten 只接受对象
        for p in [json!(null), json!("nope"), json!([1, 2])] {
            assert_eq!(
                params_with_workspace_root(p, "/w"),
                json!({"workspace_root": "/w"})
            );
        }
    }

    /// `/send` 的载荷形状由 Content-Type 决定（缺陷 2）。
    #[test]
    fn the_send_body_is_dispatched_by_content_type() {
        assert_eq!(payload_kind(Some("application/json")), PayloadKind::Json);
        assert_eq!(
            payload_kind(Some("application/json; charset=utf-8")),
            PayloadKind::Json
        );
        assert_eq!(
            payload_kind(Some("application/vnd.x+json")),
            PayloadKind::Json
        );
        assert_eq!(
            payload_kind(Some("application/octet-stream")),
            PayloadKind::Bytes
        );
        assert_eq!(
            payload_kind(Some("application/x-www-form-urlencoded")),
            PayloadKind::Legacy
        );
        assert_eq!(payload_kind(None), PayloadKind::Legacy);

        assert_eq!(
            payload_of(PayloadKind::Json, br#"{"op":"list"}"#).unwrap(),
            json!({"op":"list"})
        );
        assert_eq!(
            payload_of(PayloadKind::Bytes, b"ls\n").unwrap(),
            json!({"bytes":"ls\n"})
        );
        // 旧客户端（e2e 夹具的 curl 就是 form Content-Type）带 JSON 体：
        // 原样转发，文件取数才不至于变成打进 PTY 的字面文本
        assert_eq!(
            payload_of(PayloadKind::Legacy, br#"{"op":"read","path":"a.txt"}"#).unwrap(),
            json!({"op":"read","path":"a.txt"})
        );
        // 旧客户端带裸字节：仍是 bytes，终端不受影响
        assert_eq!(
            payload_of(PayloadKind::Legacy, b"stty size\n").unwrap(),
            json!({"bytes":"stty size\n"})
        );

        // 声明了 JSON 却解析不出来 → 400，不降级成 bytes
        let e = payload_of(PayloadKind::Json, b"not json").expect_err("must reject");
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        assert_eq!(e.code(), err::BAD_BODY);
        // 非 UTF-8 的两条字节路都 400（不是把 `�` 塞进 keystroke）
        for kind in [PayloadKind::Bytes, PayloadKind::Legacy] {
            let e = payload_of(kind, &[0xff, 0xfe]).expect_err("must reject");
            assert_eq!(e.status(), StatusCode::BAD_REQUEST);
            assert_eq!(e.code(), err::BAD_BODY);
        }
    }

    /// `channel.ready` 只在设备确认后发（缺陷 4），且已确认的通道不被超时误杀。
    #[test]
    fn a_channel_becomes_ready_only_when_the_device_confirms_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));

        announce_ready(&state, "ch-1");
        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "channel.ready");
        assert!(events[0].data.contains(r#""channel_id":"ch-1""#));

        assert!(!state.bindings().take_unacked("ch-1"), "已确认的不许被摘");
        assert!(state.bindings().get("ch-1").is_some());

        // 不认识的 id 不推事件（设备在一个 Hub 没记过账的 id 上回 ack）
        announce_ready(&state, "ch-ghost");
        assert_eq!(state.streams().replay_since(Some(0)).len(), 1);
    }

    /// 设备不 ack：页面上要看得见一条带原因的失败，且通道要解绑。
    #[test]
    fn an_unconfirmed_channel_times_out_visibly_and_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));

        expire_unacked(&state, "n_a", "ch-1");
        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1, "超时必须在页面上说出来");
        assert_eq!(events[0].name, "channel.frame");
        let event = serde_json::from_str::<serde_json::Value>(&events[0].data).unwrap();
        assert_eq!(event["channel_id"], json!("ch-1"));
        assert_eq!(event["payload"]["op"], json!("error"));
        assert_eq!(event["payload"]["code"], json!("timeout"));
        assert!(
            event["payload"]["reason"].is_string(),
            "页面要拿到一句人话：{event}"
        );
        assert!(state.bindings().get("ch-1").is_none(), "超时要解绑");

        expire_unacked(&state, "n_a", "ch-1");
        assert_eq!(
            state.streams().replay_since(Some(0)).len(),
            1,
            "同一个超时不得推两遍"
        );
    }

    /// 已确认的通道**不会**被超时误杀（迟到的定时器只能是空操作）。
    #[test]
    fn a_confirmed_channel_never_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));
        announce_ready(&state, "ch-1");

        expire_unacked(&state, "n_a", "ch-1");
        assert!(state.bindings().get("ch-1").is_some(), "确认过的通道还在");
        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1, "只该有那一条 channel.ready");
        assert_eq!(events[0].name, "channel.ready");
    }

    /// 超时任务真的会在到点后执行（不是只有直接调 `expire_unacked` 的那条路）。
    ///
    /// 这条钉的是「接线」：`open_channel` 里那个 `spawn` 一旦被删掉，
    /// 失败信号就再也不会出现，而所有直接调 `expire_unacked` 的用例照旧全绿。
    #[tokio::test]
    async fn the_ready_timeout_task_actually_fires() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));
        spawn_ready_timeout(
            state.clone(),
            "n_a".into(),
            "ch-1".into(),
            std::time::Duration::from_millis(5),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(state.bindings().get("ch-1").is_none());
        assert_eq!(state.streams().replay_since(Some(0)).len(), 1);
    }

    /// 记一次心跳上报（能力 + 负载）。
    fn report_caps(st: &AppState, node: &str, caps: serde_json::Value) {
        st.reports().record(
            node,
            hub_core::Report {
                node_version: "0.1.0".into(),
                capabilities: caps,
                resources: serde_json::json!({}),
                workspace_roots: Vec::new(),
                at: std::time::Instant::now(),
            },
        );
    }

    /// **「没报 `connectors` 这个键」不等于「一个 connector 都没有」**。
    ///
    /// 折成空数组的话，一台只报了 `agents` 的设备会被判成「什么都不会」，
    /// 对 terminal/files/exec 一律 409——而真相是「它还没说」。
    #[test]
    fn an_undeclared_connector_key_is_not_an_empty_capability_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());

        // 从没上报过 → 放行
        assert!(unreported_connector(&state, "n_a", "terminal").is_none());

        // 上报了能力，但**没有 `connectors` 键** → 放行
        report_caps(&state, "n_b", serde_json::json!({"agents": ["opencode"]}));
        assert!(unreported_connector(&state, "n_b", "terminal").is_none());

        // 报了 `connectors` 键但值不是数组 → 还是「不知道」→ 放行
        report_caps(&state, "n_c", serde_json::json!({"connectors": "terminal"}));
        assert!(unreported_connector(&state, "n_c", "terminal").is_none());
    }

    /// 报了 `connectors` 键但**里面没有它** → 拒绝，且回出它报过的那几个。
    #[test]
    fn a_connector_missing_from_a_declared_list_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        report_caps(
            &state,
            "n_a",
            serde_json::json!({"connectors": ["terminal", "files"]}),
        );

        assert!(unreported_connector(&state, "n_a", "terminal").is_none());
        let reported = unreported_connector(&state, "n_a", "exec").expect("必须拒绝");
        assert_eq!(reported, vec!["terminal".to_string(), "files".to_string()]);
    }

    /// **关通道时队列满 → 503，而不是「假装关掉了」**。
    ///
    /// `close_channel` 那一帧没发出去，设备侧的 PTY 还在跑；回 204 并推
    /// `channel.closed` 会让页面以为通道已关，而设备还在吃信用。
    #[tokio::test]
    async fn closing_a_channel_on_a_stalled_device_is_a_503_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));
        state.connections().insert(
            "n_a".into(),
            hub_core::connections::ConnHandle {
                conn_id: "c1".into(),
                gen: 1,
            },
        );
        // 控制队列（8 格）塞满且**从不消费** → 下一帧必然 QueueFull
        let (_control, _data) = state.outbound().register("c1");
        for _ in 0..crate::conns::CONTROL_QUEUE {
            state
                .outbound()
                .send("c1", Frame::push(types::CLOSE_CHANNEL, 0, json!({})))
                .expect("入队");
        }

        let resp = close_channel(
            State(state.clone()),
            Extension(Authenticated::new("admin".to_string())),
            Path("ch-1".to_string()),
        )
        .await
        .expect_err("必须失败");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.code(), err::BUSY);
        assert!(
            state.bindings().get("ch-1").is_some(),
            "通道在设备上还开着：归属必须留着，页面重试才关得掉"
        );
        assert!(
            state
                .streams()
                .replay_since(Some(0))
                .iter()
                .all(|e| e.name != "channel.closed"),
            "没关掉就不许推 channel.closed"
        );
    }

    /// **开通道时设备读不过来 → 503，且绑定必须跟着撤**。
    ///
    /// 只回 503 而留着绑定的话，Hub 里就有一条「只有页面知道」的通道号：
    /// 页面上它显示成失败，而 Hub 侧 `expire_unacked` 到点后还会再去找设备关它，
    /// 设备那头从来不知道这条通道存在过。
    ///
    /// 队列塞满的手法与 `closing_a_channel_on_a_stalled_device_is_a_503_not_a_silent_success`
    /// 相同：`register` 之后一次都不消费，控制面 8 格满了就是 `QueueFull`。
    #[tokio::test]
    async fn opening_a_channel_on_a_stalled_device_is_a_503_and_unbinds() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        // 一条真会话：`open_channel` 要先在库里查到它才知道往哪台设备发
        // （会话按外键挂着 device，所以设备行得先存在；`DeviceRepo` 没有
        // 公开的裸插入——正常路径是 enroll 事务，这里只要能满足外键）
        let conn = state.pool().get().expect("拿一条连接");
        conn.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES ('n_a', 'pi', 'fp-a', 'unix:0')",
            [],
        )
        .expect("设备行要插得进去");
        drop(conn);
        let session = hub_storage::SessionRepo::new(state.pool().clone())
            .create("n_a", "/repo", None, "unix:1")
            .expect("会话要建得起来");
        state.connections().insert(
            "n_a".into(),
            hub_core::connections::ConnHandle {
                conn_id: "c1".into(),
                gen: 1,
            },
        );
        // 能力上报：不报 connectors 键 = 「不知道」，按有处理（缺省即保留）
        report_caps(&state, "n_a", serde_json::json!({"agents": []}));

        let (_control, _data) = state.outbound().register("c1");
        for _ in 0..crate::conns::CONTROL_QUEUE {
            state
                .outbound()
                .send("c1", Frame::push(types::OPEN_CHANNEL, 0, json!({})))
                .expect("入队");
        }

        let req = axum::extract::Request::builder()
            .method("POST")
            .uri("/api/channels")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "session_id": session.id,
                    "connector": "terminal",
                    "params": {},
                }))
                .unwrap(),
            ))
            .unwrap();
        let resp = open_channel(
            State(state.clone()),
            Extension(Authenticated::new("admin".to_string())),
            req,
        )
        .await
        .expect_err("队列满必须失败");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.code(), err::BUSY);
        assert!(
            state.bindings().0.lock().is_empty(),
            "下发失败的通道不许在 Hub 里留下绑定"
        );
        assert!(
            state
                .streams()
                .replay_since(Some(0))
                .iter()
                .all(|e| e.name != "channel.ready"),
            "帧根本没发出去，不许推 ready"
        );
    }

    /// 设备收不到「通道超时了，关掉它」那一帧时：**只留痕，不扩散**。
    ///
    /// `expire_unacked` 已经解绑并给页面推了失败信号，此时设备侧那帧发不发得出去
    /// 不影响页面——再抛一次错只会把「超时」这件事变成 500，而页面已经不需要了。
    #[tokio::test]
    async fn expiring_an_unacked_channel_survives_a_device_that_will_not_take_the_close() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));
        state.connections().insert(
            "n_a".into(),
            hub_core::connections::ConnHandle {
                conn_id: "c1".into(),
                gen: 1,
            },
        );
        let (_control, _data) = state.outbound().register("c1");
        for _ in 0..crate::conns::CONTROL_QUEUE {
            state
                .outbound()
                .send("c1", Frame::push(types::CLOSE_CHANNEL, 0, json!({})))
                .expect("入队");
        }

        expire_unacked(&state, "n_a", "ch-1");

        // 超时本身照旧兑现：解绑 + 一条失败信号给页面
        assert!(
            state.bindings().get("ch-1").is_none(),
            "超时通道必须解绑，否则永远等一个不会来的 ack"
        );
        let events = state.streams().replay_since(Some(0));
        assert_eq!(events.len(), 1, "只该有那条超时失败信号");
        assert_eq!(events[0].name, "channel.frame");
    }

    /// **不是你的通道 → 403，而不是「幂等地关掉」。**
    ///
    /// 「不存在」与「不是你的」必须分开：前者是幂等成功（面板早就关过），
    /// 后者是越权——混成一个 204 的话，越权关闭看起来像「关过了」。
    #[tokio::test]
    async fn closing_a_channel_of_another_subject_is_a_403() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state
            .bindings()
            .bind("ch-1", binding("n_a", "someone-else"));

        let resp = close_channel(
            State(state.clone()),
            Extension(Authenticated::new("admin".to_string())),
            Path("ch-1".to_string()),
        )
        .await
        .expect_err("必须失败");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(resp.code(), err::FORBIDDEN);
        assert!(
            state.bindings().get("ch-1").is_some(),
            "不是你的通道就一个字都不能动"
        );
    }

    /// 设备已经不在 → 照样 204 且推 `channel.closed`（幂等收尾）。
    #[tokio::test]
    async fn closing_a_channel_of_a_gone_device_is_a_204() {
        let dir = tempfile::tempdir().unwrap();
        let state = bootstrap(dir.path());
        state.bindings().bind("ch-1", binding("n_a", "admin"));

        let resp = close_channel(
            State(state.clone()),
            Extension(Authenticated::new("admin".to_string())),
            Path("ch-1".to_string()),
        )
        .await
        .expect("必须成功");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(state
            .streams()
            .replay_since(Some(0))
            .iter()
            .any(|e| e.name == "channel.closed"));
    }

    /// 从实时流里取下一条事件。
    ///
    /// 返回 `None` 表示「这一轮订阅者被甩掉了」（`Lagged`）——那一轮不构成
    /// 断言，下一轮重来。
    ///
    /// **抽成助手是为了让 `Lagged` 这条路可确定性地被测到**：写在并发用例
    /// 里它只是「有时会撞上」，撞不撞得上取决于调度——那会让覆盖率每次跑
    /// 都不一样，覆盖率门禁就成了掷骰子。
    fn next_live_event(rx: &mut tokio::sync::broadcast::Receiver<Envelope>) -> Option<Envelope> {
        loop {
            match rx.try_recv() {
                Ok(env) => return Some(env),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    std::thread::yield_now();
                }
                // 订阅者被甩掉了：这一轮不构成断言（下一轮重新来）。
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => return None,
                // coverage: excluded — 发送端是 `Streams` 自己，它还活着
                // （`stop` 只停发布线程，不 drop 总线），`Closed` 结构上不可达。
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    // coverage: excluded — 同上面的 `Closed` 臂：
                    // `panic!` 的参数行被 llvm-cov 记为 miss（多行宏已知现象）。
                    panic!("发送端还在，不该 Closed")
                }
            }
        }
    }

    /// **订阅者被环形缓冲甩掉**：它必须被告知「你落下了」，而不是拿到
    /// 一条更旧的事件。
    ///
    /// 这条路以前只在一个并发用例里顺带撞上——撞不撞得上取决于发布线程
    /// 何时被切走，于是同一份代码每次跑出来的覆盖率不同。
    #[test]
    fn a_subscriber_that_falls_behind_the_ring_buffer_is_told_it_lagged() {
        let s = Streams::new();
        let (_backlog, mut rx) = s.subscribe_parts(None);
        // 推过环形缓冲的容量：订阅者一个都没取。
        for _ in 0..(REPLAY_CAPACITY + 8) {
            s.publish("device.status", "{}");
        }
        assert!(
            next_live_event(&mut rx).is_none(),
            "订阅者被甩掉时必须收到 Lagged，而不是一条更旧的事件"
        );
    }

    /// **快照与实时流之间不留缝**：订阅时读到的回放与随后的实时流首尾相接。
    ///
    /// 「读快照」与「订阅」分开做时，发布方在两步之间被切走会让新订阅者
    /// 在两处各收一次（终端把同一段输出画两遍），反向则永久丢失且没有
    /// `resync` 注释帧。
    #[test]
    fn the_snapshot_and_the_live_stream_leave_no_gap_or_duplicate() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let s = Streams::new();
        let stop = Arc::new(AtomicBool::new(false));
        let publisher = {
            let s = s.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.publish("device.status", "{}");
                }
            })
        };

        let mut checked = 0usize;
        for _ in 0..2_000 {
            let last = s.last_event_id();
            let (backlog, mut rx) = s.subscribe_parts(Some(last));
            // 快照里的最后一条（没有就是 `last`）之后，实时流必须紧接下一条
            let expected = backlog.last().map_or(last, |e| e.id) + 1;
            // 订阅者被甩掉了（`None`）：这一轮不构成断言，下一轮重新来。
            if let Some(env) = next_live_event(&mut rx) {
                assert_eq!(env.id, expected, "实时流必须紧接快照（既不重发也不丢事件）");
                checked += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        publisher.join().expect("publisher");
        assert!(checked > 0, "至少要断言过一次");
    }
}
