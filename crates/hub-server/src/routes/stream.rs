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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
use crate::{ApiError, AppState};

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
    next_id: AtomicU64,
    replay: Mutex<VecDeque<Envelope>>,
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
                // 序号从 1 开始：`Last-Event-ID: 0` 与「没有这个头」同义。
                next_id: AtomicU64::new(1),
                replay: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// 广播一个事件，回它的序号。
    ///
    /// **`data` 用 `impl Into<String>` 而不是 `serde_json::Value`**：调用方
    /// （`ws.rs` 那几处）手里已经是 `Frame`，序列化给它比在这里重新拼一个
    /// JSON 对象更不容易出错，也让本模块不必知道任何协议形状。
    pub fn publish(&self, name: &'static str, data: impl Into<String>) -> u64 {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let env = Envelope {
            id,
            name,
            data: data.into(),
        };
        self.push_to_replay(&env);
        // `send` 失败只说明「此刻一个订阅者都没有」——那是常态（页面没开着），
        // 不是错误，更不该让发布方（ws 读循环）因此中断。
        let _ = self.inner.tx.send(env);
        id
    }

    fn push_to_replay(&self, env: &Envelope) {
        let mut buf = self.inner.replay.lock();
        while buf.len() >= REPLAY_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(env.clone());
    }

    /// 回放缓冲里序号 `> last` 的事件。
    fn replay_since(&self, last: Option<u64>) -> Vec<Envelope> {
        let Some(last) = last else {
            return Vec::new();
        };
        self.inner
            .replay
            .lock()
            .iter()
            .filter(|e| e.id > last)
            .cloned()
            .collect()
    }

    /// 当前已用掉的最大事件序号。测试与运维用它核对「续收窗口从哪开始」。
    pub fn last_event_id(&self) -> u64 {
        self.inner.next_id.load(Ordering::Relaxed).saturating_sub(1)
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
        let backlog = self.replay_since(last).into_iter();
        let rx = self.inner.tx.subscribe();
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
}

/// 一条通道的绑定：**谁**（subject）在**哪台设备**上开的。
///
/// **Hub 侧内存，不落库**：通道本来就是连接生命周期内的东西（§7.1 断开即清），
/// 而「谁开的」只在浏览器侧有意义——落库会让一个已经结束的会话留下
/// 一条再也关不掉的通道记录。
#[derive(Debug, Clone)]
struct ChannelBinding {
    node_id: String,
    session_id: String,
    subject: String,
}

/// `channel_id → 绑定`。
#[derive(Clone, Default)]
pub struct Bindings(Arc<Mutex<HashMap<String, ChannelBinding>>>);

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    fn bind(&self, channel_id: &str, b: ChannelBinding) {
        self.0.lock().insert(channel_id.to_string(), b);
    }

    fn unbind(&self, channel_id: &str) -> Option<ChannelBinding> {
        self.0.lock().remove(channel_id)
    }

    fn get(&self, channel_id: &str) -> Option<ChannelBinding> {
        self.0.lock().get(channel_id).cloned()
    }

    /// 某个 node 上被 Hub 侧关掉的通道（关面板时整节点清账之外的补偿）。
    pub fn drop_node(&self, node_id: &str) -> usize {
        let mut map = self.0.lock();
        let before = map.len();
        map.retain(|_, b| b.node_id != node_id);
        before - map.len()
    }
}

/// 端点用的错误码常量。**都是固定 code**（约束 13：绝不夹带 secret）。
mod err {
    pub const BAD_BODY: &str = "invalid_request_body";
    pub const FORBIDDEN: &str = "channel_not_yours";
    pub const OFFLINE: &str = "device_offline";
    pub const SESSION_ENDED: &str = "session_ended";
    pub const BUSY: &str = "device_busy";
    pub const INVALID_SIZE: &str = "invalid_terminal_size";
    /// 设备上报的能力里没有这个 connector（子任务 11 的硬过滤）。
    pub const NOT_REPORTED: &str = "connector_not_reported";
}

/// 该节点上报过 `connector` 吗？（子任务 11 的能力硬过滤）
///
/// 返回 `None` 表示「放行」，`Some(上报的列表)` 表示拒绝。
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
/// **为什么门禁在开通道而不是建会话**：M1 的 `POST /api/sessions` 请求体里没有
/// connector 字段——会话不声明 connector，connector 是开面板时才指定的
/// （里程碑子任务 11 的「落地口径修正」）。建会话处做这条校验只能凭空捏一个字段。
fn unreported_connector(state: &AppState, node_id: &str, asked: &str) -> Option<Vec<String>> {
    let report = state.reports().get(node_id)?;
    let reported: Vec<String> = report
        .capabilities
        .get("connectors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
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
    let body = read_body(req).await?;
    let body: OpenChannel =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request(err::BAD_BODY))?;

    let detail = hub_storage::SessionRepo::new(state.pool().clone())
        .detail(&body.session_id)
        .map_err(|e| ApiError::internal_with("cannot read the session", e))?;
    let Some(detail) = detail else {
        return Err(ApiError::not_found());
    };
    if detail.session.state != hub_storage::SessionState::Running {
        return Err(ApiError::conflict(err::SESSION_ENDED));
    }
    let node_id = detail.workspace.node_id.clone();
    if state.connections().get(&node_id).is_none() {
        return Err(ApiError::conflict(err::OFFLINE));
    }
    // 能力硬过滤（子任务 11）：设备上报的 connectors 里没有它，就不下发。
    // 下发一帧注定被 node 拒绝的 `open_channel` 会让页面拿到 `channel.ready`
    // 之后才知道失败——那时通道号已经发出去、面板已经显示「已连接」。
    if let Some(reported) = unreported_connector(&state, &node_id, &body.connector) {
        return Err(ApiError::conflict_with(
            err::NOT_REPORTED,
            || json!({ "connector": body.connector, "reported": reported }),
        ));
    }

    let channel_id = identity::random_id();
    let frame = Frame::push(
        types::OPEN_CHANNEL,
        0,
        serde_json::to_value(protocol::channel::OpenChannel {
            channel_id: channel_id.clone(),
            connector: body.connector.clone(),
            params: body.params.clone(),
            credit_start: protocol::channel::CREDIT_START,
        })
        .unwrap_or_else(|_| json!({"channel_id": channel_id, "connector": body.connector})),
    );
    send_to_device(&state, &node_id, frame)?;

    state.bindings().bind(
        &channel_id,
        ChannelBinding {
            node_id: node_id.clone(),
            session_id: body.session_id.clone(),
            subject: auth.subject().to_string(),
        },
    );
    state.streams().publish(
        "channel.ready",
        json!({"node_id": node_id, "channel_id": channel_id, "session_id": body.session_id})
            .to_string(),
    );
    tracing::info!(
        node_id = %node_id,
        channel_id = %channel_id,
        session_id = %body.session_id,
        connector = %body.connector,
        subject = %auth.subject(),
        "browser opened a channel"
    );
    Ok((
        StatusCode::CREATED,
        Json(OpenedChannel {
            channel_id,
            node_id,
            session_id: body.session_id,
        }),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// 上行 2/4：POST /api/channels/{id}/send
// ---------------------------------------------------------------------------

/// `POST /api/channels/{id}/send`：把浏览器发来的原始字节转成 `channel_data` 下发。
///
/// **无状态裸字节**（模块注释的断线口径）：Hub 不排队、不重发。
///
/// 字节以 **UTF-8 文本**转发（`{"bytes": "..."}`），与 node 侧 terminal
/// connector 的载荷形状一致（`node-core/src/connector/terminal.rs`）。非法 UTF-8
/// 直接 **400**：把 `�` 悄悄塞进用户的 keystroke 比报错难查得多，而键盘输入
/// 本来就总是 UTF-8；二进制通道（files 读写）走各自的 connector 载荷，
/// 不复用这条裸字节入口。
pub async fn send_bytes(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    Path(channel_id): Path<String>,
    req: Request,
) -> Result<Response, ApiError> {
    let body = read_body(req).await?;
    let text = std::str::from_utf8(&body).map_err(|_| ApiError::bad_request(err::BAD_BODY))?;
    let binding = owned_channel(&state, &channel_id, auth.subject())?;
    send_channel_data(
        &state,
        &binding.node_id,
        &channel_id,
        json!({"bytes": text}),
    )
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
    let body = read_body(req).await?;
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
    let Some(binding) = state.bindings().unbind(&channel_id) else {
        // 幂等：Hub 侧已经没有了（面板早就关过，或设备掉线时整节点清掉了）
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    // 归属检查放在摘表**之后**：必须先确认是谁，才能动手删；不是本人的要放回去。
    if binding.subject != auth.subject() {
        state.bindings().bind(&channel_id, binding);
        return Err(ApiError::forbidden(err::FORBIDDEN));
    }
    let frame = Frame::push(
        types::CLOSE_CHANNEL,
        0,
        serde_json::to_value(protocol::channel::CloseChannel {
            channel_id: channel_id.clone(),
            reason: "closed by the console".to_string(),
        })
        .unwrap_or_else(|_| json!({"channel_id": channel_id, "reason": "closed"})),
    );
    // 设备已经不在 → 照样 204：Hub 侧的通道账本随连接一起清掉了（§7.1），
    // 没有需要通知的对象。
    let _ = send_to_device(&state, &binding.node_id, frame);
    state.streams().publish(
        "channel.closed",
        json!({
            "node_id": binding.node_id,
            "session_id": binding.session_id,
            "channel_id": channel_id,
        })
        .to_string(),
    );
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

/// 读请求体（上限 1 MiB，与 WS 帧上限同值）。
///
/// 与 `routes::api::read_body` 同一条口径（同一个上限常量、同一句「我没法处理」
/// 的错误），但**不复用它的函数**：那是 `api.rs` 的私有函数，而把两个模块
/// 绑成一个「谁先改谁就得看对方」的私有依赖没有收益。
async fn read_body(req: Request) -> Result<Vec<u8>, ApiError> {
    axum::body::to_bytes(req.into_body(), crate::WS_MAX_MESSAGE_BYTES)
        .await
        // 读出错与超上限都归成 400。**错误串不进响应体**（约束 13）。
        .map(|b| b.to_vec())
        .map_err(|_| ApiError::bad_request(err::BAD_BODY))
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
    let frame = Frame::push(
        types::CHANNEL_DATA,
        0,
        serde_json::to_value(protocol::channel::ChannelData {
            channel_id: channel_id.to_string(),
            payload,
        })
        .unwrap_or_else(|_| json!({"channel_id": channel_id, "payload": {}})),
    );
    send_to_device(state, node_id, frame)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ChannelBinding {
            node_id: node.to_string(),
            session_id: "s_1".into(),
            subject: subject.into(),
        }
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
        let e = read_body(req).await.expect_err("must reject");
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
}
