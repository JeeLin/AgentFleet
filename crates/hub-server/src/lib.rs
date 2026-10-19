//! afhub 的可测试核心：路由装配。
//!
//! 与 `main.rs` 分开是为了能**不起真端口**就测路由：`build_app` 返回
//! `axum::Router`，测试用 `tower::ServiceExt::oneshot` 直接打请求。
//!
//! 约束 1：**Hub 零入站端口**——只监听页面侧的 `127.0.0.1:8080`，
//! 设备一律**主动外联** `/ws`。这里没有任何设备入站路由。
//!
//! 认证闭环在 [`middleware_auth`]：豁免表之外一律要 session cookie
//! （§2.4.3），`/setup` 与 `/login` 在 [`routes::setup`]。

pub mod channels;
pub mod conns;
pub mod middleware_auth;
pub mod presence;
pub mod routes;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hub_core::auth::{AuthError, AuthParams, HubAuth, LoginRateLimiter, SetupToken};
use hub_core::registry::Registry;
use hub_core::{ConnectionRegistry, Reports};
use hub_storage::db::Db;
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::Semaphore;

/// session cookie 名。
pub const SESSION_COOKIE: &str = "af_session";
/// CSRF 双重提交用的 cookie 名。
pub const CSRF_COOKIE: &str = "af_csrf";
/// CSRF 双重提交用的请求头名（小写，HTTP/2 与 `HeaderMap` 一致）。
pub const CSRF_HEADER: &str = "x-csrf-token";
/// `POST /setup` 必带的 token 头。
pub const SETUP_TOKEN_HEADER: &str = "x-setup-token";

/// `/login` 排队超时（秒→Duration 常量）。拿不到信号量即 429。
pub const DEFAULT_LOGIN_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// `/ws` 的握手超时。取 `protocol::HANDSHAKE_TIMEOUT`，不在这里另立一个值。
pub use protocol::handshake::HANDSHAKE_TIMEOUT;

/// 默认心跳周期（毫秒）。**由 Hub 在 `auth_ok` 下发给节点**（§2）——
/// 节点不硬编码，所以这个常量只是 Hub 侧的默认配置值。
pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 15_000;

/// 帧大小上限。与 `protocol::MAX_FRAME_BYTES` 一致（§2）：超限即断开。
pub const WS_MAX_MESSAGE_BYTES: usize = protocol::MAX_FRAME_BYTES;

/// 启动期错误：数据目录写不了 / 数据库读不了，Hub 起不来。
///
/// 与请求期的 [`ApiError`] 分开：启动失败必须**中止进程**，
/// 而不是让 Hub 在「没有 setup token」的半初始化状态上继续服务。
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("hub auth init failed: {0}")]
    Auth(#[from] AuthError),
    #[error("cannot write setup token file: {0}")]
    Io(#[from] std::io::Error),
}

/// 请求期错误。
///
/// **错误体只有固定错误码，绝不夹带 secret**（约束 13）：
/// 密码哈希、setup token、session HMAC 密钥、cookie 值一律不进响应。
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    /// 附加的展示用字段（目前只有 409 的 `status`）。**只放枚举与计数值**，
    /// 与 `code` 一样受「绝不夹带 secret」的约束约束 13 管辖。
    detail: Option<serde_json::Value>,
}

impl ApiError {
    pub fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }

    /// JSON body 缺失/形状不对。
    pub fn bad_request_body() -> Self {
        Self::bad_request("invalid_request_body")
    }

    pub fn unauthorized(code: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code)
    }

    pub fn forbidden(code: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code)
    }

    pub fn conflict(code: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code)
    }

    /// 404 的错误码。**全路由一个码**（不带资源名）：第一版是单用户，
    /// 「不存在」与「存在但不属于你」对页面没有区别——把两者分开只会
    /// 让前端多写一套分支（而那份分支在单用户下永远走不到）。
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found")
    }

    /// 409，且错误体里**额外带一个可显示的字段**（设备离线时要带设备状态）。
    ///
    /// `error` 的值仍然是那个固定 code——`make` 只往 body 里加键，
    /// 所以「错误体是 `{"error": "<code>"}`」那条契约只被破例这一处，
    /// 而破例的理由（页面要显示**是什么**让状态冲突）是可见的。
    pub fn conflict_with(code: &'static str, make: impl FnOnce() -> serde_json::Value) -> Self {
        let mut e = Self::new(StatusCode::CONFLICT, code);
        e.detail = Some(make());
        e
    }

    pub fn too_many_requests(code: &'static str) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code)
    }

    /// 503：设备读不过来（写侧队列打满，§6.1.1）。
    ///
    /// 不用 409：409 是「资源状态不对，重试前要先改点什么」，而这一档的
    /// 正确处置就是**稍后重试同一件事**——把它报成 429 会让前端去退避登录
    /// 那种「永远不该重试」的错误。
    pub fn service_unavailable(code: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code)
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }

    fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            detail: None,
        }
    }

    /// 存储/认证层错误 → 500。
    ///
    /// 刻意**不把 `e` 拼进响应体**：它是 `StorageError` 的消息串，
    /// 可能带数据库路径。细节只进 `tracing`。
    fn from_auth(e: AuthError) -> Self {
        tracing::error!(error = %e, "request failed on storage/auth");
        Self::internal()
    }

    /// 内部错误，附一条只在 `tracing` 里出现的说明。
    ///
    /// 与 [`Self::from_auth`] 同一个理由：响应体绝不带内部消息串
    /// （它可能含数据库路径或密钥材料），细节只进日志。
    pub fn internal_with(context: &'static str, e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, context);
        Self::internal()
    }

    /// 状态码（测试用）。
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// 错误码（测试用，用于断言响应体只含固定 code，绝不夹带 secret）。
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"error": self.code});
        if let (Some(obj), Some(detail)) = (body.as_object_mut(), self.detail) {
            obj.insert("detail".to_string(), detail);
        }
        (self.status, Json(body)).into_response()
    }
}

/// Hub 运行期状态（路由共享）。
///
/// 持有 `Arc` 内部块：axum 的 `State` 要 `Clone`，而 `HubAuth` 内部是
/// 连接池（本身是 `Arc`），再包一层避免每次 clone 都重建。
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    auth: HubAuth,
    /// 活动 setup token。正常模式（已设密码）为 `None`。
    setup: Mutex<Option<SetupToken>>,
    /// `/login` 全局并发闸门（= [`hub_core::auth::LOGIN_SEMAPHORE_PERMITS`]）。
    login_gate: Arc<Semaphore>,
    login_queue_timeout: Duration,
    limiter: LoginRateLimiter,
    data_dir: PathBuf,
    /// 设备注册表（持久）。
    registry: Registry,
    /// 连接注册表：`node_id → 连接句柄`，**仅内存**（子任务 3）。
    ///
    /// 这里不存 socket：socket 活在 `tokio::task` 的栈上，
    /// 注册表只回答「这个 node 现在该把消息送到哪条连接」。
    /// 存 socket 会让注册表变成**不可序列化、不可测试、不可迁移**的东西，
    /// 而且跨重启后必然指向一个已关闭的连接。
    connections: Arc<ConnectionRegistry>,
    /// 本进程上的心跳周期。`auth_ok` 下发给节点（§2）。
    heartbeat_interval_ms: u64,
    /// `node_id → 最近一次心跳上报`。**只在心跳时写**，路由读内存不读库
    /// （`scheduling.md` §2.2）。
    reports: Reports,
    /// 设备仓储（心跳落库用）。
    devices: hub_storage::repo::DeviceRepo,
    /// 连接池句柄。**只在路由里读**（`/api/*` 的会话仓储要用它），
    /// 而连接池是 `Arc`——clone 不产生新连接，所以路由各建各的仓储
    /// 没有代价，也不会让心跳路径上的「一次心跳建一个 repo」变成惯例。
    pool: Db,
    /// `conn_id → 出站控制帧队列`（顶替时通知旧连接用，子任务 7）。
    outbound: conns::OutboundTable,
    /// `node_id → 该设备的通道表`（子任务 8）。纯内存，不落库（§5）。
    channels: Arc<parking_lot::RwLock<hub_core::channels::HubChannels>>,
    /// 页面侧事件总线（子任务 5a）：`GET /api/stream` 的唯一数据源。
    ///
    /// 放共享状态而不是每条 SSE 连接自建一条 channel：设备侧的通道帧是
    /// **每个到达点只有一份**，而订阅它的可能有好几个面板（同一会话开两个
    /// 终端）或好几个标签页。要让每个订阅者各自一份，就得在 ws 读循环里
    /// 维护一张订阅表——而那张表的唯一能力就是「把一帧发给所有订阅者」，
    /// `broadcast` 正是它，不需要第二份真相。
    streams: routes::stream::Streams,
    /// `channel_id → 谁在哪台设备上开的这条通道`（子任务 5a）。
    ///
    /// 与 [`Self::channels`] 分开：那个是**协议层账本**（按 `(node_id, channel_id)`，
    /// 断线即清），这个是**页面归属**（谁开的这一条）。混在一张表里的后果是
    /// `drop_node` 会顺手把页面归属也清掉，而归属是用来判 403 的——
    /// 清错了会让「越权访问」看起来像「通道不存在」。
    bindings: routes::stream::Bindings,
}

impl AppState {
    /// 生产入口：按 CPU 核数选 Argon2id 参数，排队超时用默认值。
    ///
    /// **未设密码时生成 16 字节 setup token**，只写 stderr 与
    /// `<data>/setup-token.txt`（0600）；已设密码则视为正常模式，
    /// 并删掉可能残留的 token 文件（§2.4.1）。
    pub fn bootstrap(pool: Db, data_dir: &Path) -> Result<Self, BootstrapError> {
        let ncpu = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self::bootstrap_with(
            pool,
            data_dir,
            AuthParams::for_cpu_count(ncpu),
            DEFAULT_LOGIN_QUEUE_TIMEOUT,
            LoginRateLimiter::default(),
        )
    }

    /// 可注入参数的入口（集成测试要用小 Argon2id 参数与短排队超时，
    /// 否则跑一次登录要几百毫秒、还拿不到确定的 429）。
    pub fn bootstrap_with(
        pool: Db,
        data_dir: &Path,
        params: AuthParams,
        login_queue_timeout: Duration,
        limiter: LoginRateLimiter,
    ) -> Result<Self, BootstrapError> {
        let auth = HubAuth::new(pool.clone(), params);
        // 计数器起点取库里的历史最大值：内存计数每次重启归零，
        // 而设备带着 `last_seen_gen` 重连，Hub 发出的 gen 必须单调越过它
        // （§7.1 的顶替兜底）。查不到就退化成 0 —— 记日志，不静默。
        let boot_gen = match hub_storage::repo::DeviceRepo::new(pool.clone()).max_gen() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read max gen; connections may be rejected as superseded");
                0
            }
        };
        // 设备仓储在 bootstrap 时就建好：心跳是**热路径**，每次收到都建一个
        // repo 只是白拿一次池里的连接。
        let devices = hub_storage::repo::DeviceRepo::new(pool.clone());
        let state = Self(Arc::new(Inner {
            auth,
            setup: Mutex::new(None),
            login_gate: Arc::new(Semaphore::new(hub_core::auth::LOGIN_SEMAPHORE_PERMITS)),
            login_queue_timeout,
            limiter,
            data_dir: data_dir.to_path_buf(),
            registry: Registry::new(pool),
            connections: Arc::new(ConnectionRegistry::with_boot_gen(boot_gen)),
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            reports: Reports::new(),
            pool: devices.pool().clone(),
            devices,
            outbound: conns::OutboundTable::new(),
            channels: Arc::new(parking_lot::RwLock::new(
                hub_core::channels::HubChannels::new(),
            )),
            streams: routes::stream::Streams::new(),
            bindings: routes::stream::Bindings::new(),
        }));

        if state.is_initialized()? {
            // 正常模式：残留的 token 文件必须消失，否则会误导运维
            // 以为还能重新初始化（而它其实已经作废了）。
            if let Err(e) = routes::setup::remove_setup_token_file(data_dir) {
                tracing::warn!(error = %e, "cannot remove stale setup token file");
            }
        } else {
            let token = SetupToken::generate();
            let path = routes::setup::write_setup_token_file(data_dir, token.as_str())?;
            // token 只走这两条通道：stderr（容器日志）与 owner-only 文件。
            eprintln!(
                "afhub: first-run setup token (POST /setup with header {}): {}",
                SETUP_TOKEN_HEADER,
                token.as_str()
            );
            tracing::info!(file = %path.display(), "setup token generated");
            *state.0.setup.lock() = Some(token);
        }
        Ok(state)
    }

    /// Hub 是否已设置密码。
    pub fn is_initialized(&self) -> Result<bool, AuthError> {
        self.0.auth.is_initialized()
    }

    /// 认证核心（测试用它调 `change_password` / `rotate_session_hmac`）。
    pub fn auth(&self) -> &HubAuth {
        &self.0.auth
    }

    /// session HMAC 密钥。
    ///
    /// **每次都从库里读**：签名 cookie 无服务端状态，唯一能「撤销全部会话」
    /// 的手段就是轮换 `session_hmac`（§2.4.2）——缓存会让轮换不生效。
    /// 取不到就现生成 32 字节随机值并落库（首次初始化时由 `POST /setup` 触发）。
    pub fn session_key(&self) -> Result<[u8; 32], AuthError> {
        self.0.auth.session_hmac_key()
    }

    /// `/login` 全局闸门。
    pub fn login_gate(&self) -> &Arc<Semaphore> {
        &self.0.login_gate
    }

    /// `/login` 排队超时。
    pub fn login_queue_timeout(&self) -> Duration {
        self.0.login_queue_timeout
    }

    /// 登录速率限制器。
    pub fn limiter(&self) -> &LoginRateLimiter {
        &self.0.limiter
    }

    /// 设备注册表（enroll / 重连验签）。
    pub fn registry(&self) -> &Registry {
        &self.0.registry
    }

    /// 连接注册表（`node_id → 句柄`，仅内存）。
    pub fn connections(&self) -> &Arc<ConnectionRegistry> {
        &self.0.connections
    }

    /// 本进程的心跳周期，`auth_ok` 下发给节点（§2）。
    pub fn heartbeat_interval_ms(&self) -> u64 {
        self.0.heartbeat_interval_ms
    }

    /// 心跳上报缓存（路由读它，不读库）。
    pub fn reports(&self) -> &Reports {
        &self.0.reports
    }

    /// 设备仓储（心跳落库 / 超时判离线）。
    pub fn devices(&self) -> &hub_storage::repo::DeviceRepo {
        &self.0.devices
    }

    /// 连接池句柄（`/api/*` 里的会话仓储用它）。
    pub fn pool(&self) -> &Db {
        &self.0.pool
    }

    /// 起离线判定扫瞄器，返回「关掉它的把手」。
    ///
    /// 由进程入口显式调用并持有返回值：drop 或 send 都会让扫瞄器停下。
    /// 不放进 `bootstrap` 里 fire-and-forget——那样测试一起 AppState 就会
    /// 多出一个写库的后台任务，而 Hub 重启后「所有设备被误判离线」这条
    /// 已知取舍正是靠「扫瞄器随进程生命周期」来收敛的。
    pub fn spawn_presence_scanner(&self) -> tokio::sync::oneshot::Sender<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(presence::run(
            self.0.reports.clone(),
            self.0.devices.clone(),
            presence::SCAN_INTERVAL,
            rx,
        ));
        tx
    }

    /// 出站控制帧表（顶替旧连接时寻址用）。
    pub fn outbound(&self) -> &conns::OutboundTable {
        &self.0.outbound
    }

    /// 通道路由表（子任务 8）。
    ///
    /// 用 `RwLock` 而不是每次 `clone`：`HubChannels` 里的信用账本是**每帧都要动**
    /// 的状态，复制一份就意味着两条连接各记一套账——那正是 §6 末条警告的
    /// 「重连后按 channel_id 无限累积」。
    pub fn channels(&self) -> &parking_lot::RwLock<hub_core::channels::HubChannels> {
        &self.0.channels
    }

    /// 页面侧事件总线（子任务 5a）：`GET /api/stream` 的订阅源。
    ///
    /// 与 [`Self::channels`] 分开的原因写在字段注释里：一个是协议账本，
    /// 一个是事件分发，两者生命周期不同（后者活到进程结束）。
    pub fn streams(&self) -> &routes::stream::Streams {
        &self.0.streams
    }

    /// `channel_id → 页面归属`（子任务 5a），用来判 403 与 404。
    pub fn bindings(&self) -> &routes::stream::Bindings {
        &self.0.bindings
    }

    /// 数据目录（测试用：读 schema / 直接查库）。
    pub fn data_dir(&self) -> &Path {
        &self.0.data_dir
    }

    /// 校验 setup token（失败计数 +1，达上限即失效）。
    pub fn check_setup_token(&self, candidate: &str) -> Result<(), AuthError> {
        let mut guard = self.0.setup.lock();
        match guard.as_mut() {
            Some(t) => t.check(candidate),
            // 正常模式没有活动 token（初始化已完成）→ 一律拒绝
            None => Err(AuthError::SetupTokenMismatch),
        }
    }

    /// 作废 setup token：清内存 + 删文件（§2.4.1「设置完成 → token 失效并删除」）。
    ///
    /// 删文件失败**不影响**初始化结果：密码已经落库、Hub 已经进正常模式，
    /// 这时把用户挡在门外才是真事故。文件残留的后果最多是误导运维，
    /// 记 warn 即可。
    pub fn invalidate_setup_token(&self) {
        *self.0.setup.lock() = None;
        if let Err(e) = routes::setup::remove_setup_token_file(&self.0.data_dir) {
            tracing::warn!(error = %e, "cannot remove setup token file");
        }
    }
}

/// `GET /healthz` —— 存活探针。
///
/// 页面与运维用它确认 Hub 起来了。**不泄露任何内部状态**：只回 `status`，
/// 不回版本、设备数、数据库路径——那些属于 `/api/*`，需要 session。
async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

/// 装配全部路由。
///
/// **`.layer(...)` 只覆盖它之前添加的路由**——这是 axum 的既定语义，也是
/// 这条链式调用里最容易踩的坑：新加的路由若写在 `.layer(require_auth)`
/// **之后**，它就不在认证覆盖范围内（本项目：设备数据免认证可读）。
///
/// 所以这里分成三段，顺序是安全边界：
///
/// 1. **受保护的路由**（`require_auth` 之前）：`/api/*`、`/ws`、
///    `/setup`、`/healthz`、`POST /login`。它们被 `require_auth` 覆盖；
///    `/api/{*rest}` 是认证后的 404 兜底（未认证时先 401）。
/// 2. `.layer(require_auth)`。
/// 3. **免认证的静态资产**（之后）：`/`、`GET /login`、`/assets/*`、SPA fallback。
///    登录页自己得能加载 JS/CSS，而它们不含任何设备数据。
///
/// 第 3 段的路由**绝不**放进 [`routes::AUTH_EXEMPT_PATHS`]：那张表是精确匹配，
/// 而静态路径是前缀；更重要的是，把静态路由排在 layer 之后，语义上就只豁免
/// 这几条，不会顺带把某个将来新增的 `/api/*` 也漏出去。
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/setup",
            get(routes::setup::get_setup).post(routes::setup::post_setup),
        )
        // 这里只注册 `post(...)`——认证端点。`GET /login` 是**页面**（SPA 外壳），
        // 注册在下面 `.layer` 之后（见静态资产段）。
        //
        // 必须拆开：`INIT_MODE_PATHS` 刻意不含 `/login`（初始化模式下没有密码
        // 可验，登录必须 403，§2.4.3 表第二行），而中间件对**所有**经过它的
        // 请求做 init 模式判断。整条 `/login` 留在 layer 之前，就会连
        // `GET /login` 一起 403——首次部署时登录页自己打不开，而 `/sessions/abc`
        // 却因为走 fallback 能出外壳，症状看起来像「只有登录页坏了」。
        .route("/login", post(routes::setup::post_login))
        .route("/ws", get(routes::ws::upgrade))
        // `/api/*`（子任务 5）：设备 / 会话 / agent 列表与建会话。
        // 必须落在下面那条 `.layer(...)` 之前。
        .route("/api/devices", get(routes::api::list_devices))
        .route(
            "/api/sessions",
            get(routes::api::list_sessions).post(routes::api::create_session),
        )
        .route("/api/agents", get(routes::api::list_agents))
        // 页面侧实时数据面（子任务 5a）：SSE 下行 + 四个上行端点。
        //
        // 同样**必须落在下面那条 `.layer(...)` 之前**：`/api/channels*` 是
        // 状态变更端点（POST / DELETE），一旦跑到 `.layer` 之后，它们会绕过
        // session 认证与 CSRF 双重提交——全仓库唯一的、能被任意网页用
        // 「读得着的 cookie」当武器往设备上打字节的口子。
        .route("/api/stream", get(routes::stream::stream))
        .route("/api/channels", post(routes::stream::open_channel))
        .route("/api/channels/{id}/send", post(routes::stream::send_bytes))
        .route("/api/channels/{id}/resize", post(routes::stream::resize))
        .route(
            "/api/channels/{id}",
            axum::routing::delete(routes::stream::close_channel),
        )
        // **认证后的 404 兜底**（子任务 6）：SPA fallback 挂在 layer 之后，
        // 没有这一条的话未匹配的 `/api/*` 会落到 SPA 上、回一个 200 的
        // index.html——前端的 fetch 会把 HTML 当 JSON 解析失败，而「接口不存在」
        // 看起来像「后端坏了」。注册在 layer 之前 = 未认证先 401。
        .route("/api/{*rest}", axum::routing::any(api_not_found))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware_auth::require_auth,
        ))
        // ---- 以下免认证（静态资产）----
        // `GET /login` 是登录**页面**，与 `/` 同样公开：登录页自己得能在
        // 未初始化 / 未登录时加载出 JS/CSS，否则用户没有任何路径走到表单。
        // 与上面那条 `POST /login` **同路径不同方法**：axum 会把它们合并进
        // 同一个 `MethodRouter`，而只有 POST 那一半被 `.layer` 包过——
        // 这正是「页面公开、端点受控」的落点。
        .route("/login", get(routes::assets::index))
        .route(
            "/assets/{*path}",
            get(routes::assets::asset).layer(routes::assets::asset_cache_layer()),
        )
        .route("/", get(routes::assets::index))
        .fallback(routes::assets::spa_index)
        // 状态已经进了中间件与各 handler，这里固化成 `Router<()>`，
        // 调用方（main.rs / 测试）拿到的就是可直接 serve / oneshot 的 Router
        .with_state(state)
}

/// 认证后的 `/api/*` 未匹配路径：固定 JSON 错误体。
async fn api_not_found() -> crate::ApiError {
    crate::ApiError::not_found()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app_at(dir: &Path) -> (Router, AppState) {
        let pool = hub_storage::db::open_in(dir).unwrap().1;
        let state = AppState::bootstrap_with(
            pool,
            dir,
            AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        (build_app(state.clone()), state)
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let d = tempdir();
        let (app, _st) = app_at(d.path());
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
        // 不泄露内部状态
        assert!(v.get("devices").is_none());
        assert!(v.get("version").is_none());
    }

    /// SPA fallback：**非 API** 的未知路径返回控制台外壳（history 模式的前提）。
    ///
    /// 语义变了，所以断言也得变：以前这里是 404（没有静态资产），现在
    /// `/sessions/abc` 这类前端路由必须能直接打开。关键不在「200」，
    /// 而在**回的是 HTML 外壳而不是任何设备数据**。
    #[tokio::test]
    async fn an_unknown_non_api_route_falls_back_to_the_spa_shell() {
        let d = tempdir();
        let (app, st) = app_at(d.path());
        st.auth().setup_password("correct horse battery").unwrap();
        let key = st.session_key().unwrap();
        let cookie = format!(
            "{SESSION_COOKIE}={}",
            hub_core::session::SessionStore::new(key)
                .issue("admin")
                .unwrap()
        );
        let req = Request::builder()
            .uri("/nope")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "fallback 必须回 HTML 外壳"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("/api/devices"),
            "外壳里不得夹带任何设备数据: {body}"
        );
    }

    /// 静态资产与 SPA fallback **免 session**（登录页自己得能加载 JS/CSS），
    /// 而 `/api/*` 仍要认证——两者的分界由 `.layer` 的注册次序决定。
    ///
    /// 这里把四条边界一起钉住：
    /// - `/` 与 `/login`（GET）无 cookie 也 200；
    /// - `/api/nonexistent` 无 cookie **401**（不是 404、更不是 200）；
    /// - 有 cookie 时 `/api/nonexistent` 是 **404 JSON**（不是 index.html）；
    /// - 未认证的未知非 API 路径也是 200 外壳（没有任何设备数据可漏）。
    #[tokio::test]
    async fn static_routes_are_public_but_api_404s_stay_behind_auth() {
        let d = tempdir();
        let (app, st) = app_at(d.path());
        st.auth().setup_password("correct horse battery").unwrap();

        for uri in ["/", "/login", "/nope"] {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} 必须免认证可读");
        }

        let req = Request::builder()
            .uri("/api/nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "未认证的 /api/* 必须先 401，路由存不存在都不透露"
        );

        let key = st.session_key().unwrap();
        let cookie = format!(
            "{SESSION_COOKIE}={}",
            hub_core::session::SessionStore::new(key)
                .issue("admin")
                .unwrap()
        );
        let req = Request::builder()
            .uri("/api/nonexistent")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap()
                .get("error")
                .and_then(|v| v.as_str()),
            Some("not_found"),
            "API 404 必须是 JSON 错误体，不能被 SPA fallback 吞掉"
        );
    }

    #[tokio::test]
    async fn app_can_bind_and_serve_healthz() {
        // 真起一个监听（随机端口）跑一次，证明路由装配后能真正 serve。
        // 子任务 0 测试标准 ⑥：bind 成功 + GET /healthz 返回 200。
        let d = tempdir();
        let (app, _st) = app_at(d.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 用 graceful shutdown 让 serve 有确定的「正常结束」路径，
        // 否则 fire-and-forget 的 task 在函数返回时被丢弃，.await.unwrap()
        // 分支永不执行（行 78 的覆盖率缺口就来自这里）。
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let serve_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
        });
        // 用 raw TCP 发一个最小 HTTP/1.1 请求，避免为测试引入 reqwest
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 200"), "got: {buf}");
        assert!(buf.contains("\"status\":\"ok\""), "got: {buf}");

        // 让 serve task 真正走完并断言结果（覆盖 `.await.unwrap()` 分支）
        tx.send(()).unwrap();
        let served = serve_task.await.unwrap();
        assert!(served.is_ok(), "graceful serve must complete without error");
    }

    #[tokio::test]
    async fn serve_with_graceful_shutdown_completes() {
        // 直接验证 `axum::serve().with_graceful_shutdown()` 能干净结束并返回 Ok，
        // 覆盖 main.rs 里 run() 使用同一原语的「正常退出」路径。
        let d = tempdir();
        let (app, _st) = app_at(d.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
        });
        tx.send(()).unwrap(); // 立刻触发关闭
        let res = task.await.unwrap();
        assert!(res.is_ok(), "graceful shutdown must yield Ok");
    }

    #[test]
    fn auth_exempt_table_is_explicit_and_narrow() {
        // 豁免表是安全边界：M0 只放行健康探针、初始化流程、登录端点与设备 WS。
        // 任何返回设备数据/密钥的路径出现在表里都是事故。
        assert!(routes::is_auth_exempt("/healthz"));
        assert!(routes::is_auth_exempt("/setup"));
        // `/login` 豁免 session（登录前哪来的 session），但不豁免初始化模式门禁
        assert!(routes::is_auth_exempt("/login"));
        // `/ws`：node 不是浏览器，没有 cookie（§2.4.3）
        assert!(routes::is_auth_exempt("/ws"));
        assert!(routes::is_init_mode_path("/ws"));
        assert!(!routes::is_auth_exempt("/api/devices"));
        assert!(!routes::is_init_mode_path("/login"));
        assert!(!routes::is_init_mode_path("/api/devices"));
        // 前缀不算豁免：`/healthz/../api/devices` 不能因为以 /healthz 开头就放行
        assert!(!routes::is_auth_exempt("/api/devices/healthz"));
        // 表本身要小——豁免越多边界越模糊
        assert!(
            routes::AUTH_EXEMPT_PATHS.len() <= 8,
            "auth exemption table must stay small"
        );
        assert!(
            routes::INIT_MODE_PATHS.len() <= 8,
            "init-mode table must stay small"
        );
    }

    #[test]
    fn bootstrap_uses_available_parallelism_and_default_gate() {
        // `bootstrap` 走 `available_parallelism` 那条路径：
        // 小机器上 m_cost 被下调，但永远不低于 8192（§2.4.1）。
        let d = tempdir();
        let pool = hub_storage::db::open_in(d.path()).unwrap().1;
        let st = AppState::bootstrap(pool, d.path()).unwrap();
        assert!(st.auth().params().m_cost >= 8192);
        assert_eq!(st.login_queue_timeout(), DEFAULT_LOGIN_QUEUE_TIMEOUT);
        assert!(!st.is_initialized().unwrap());
        // 首次启动：token 文件已落，且是 owner-only
        let p = d.path().join(routes::setup::SETUP_TOKEN_FILE);
        assert!(p.exists());
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&p).unwrap().permissions())
                & 0o777;
        assert_eq!(mode, 0o600, "setup token file must be 0600");
        let token = std::fs::read_to_string(&p).unwrap();
        assert!(st.check_setup_token(token.trim()).is_ok());
    }

    #[test]
    fn cookie_helpers_carry_the_required_attributes() {
        let s = routes::setup::session_cookie_header("v");
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Strict"));
        assert!(s.starts_with("af_session=v;"));
        let c = routes::setup::csrf_cookie_header("t");
        assert!(c.starts_with("af_csrf=t;"));
        assert!(
            !c.contains("HttpOnly"),
            "csrf cookie must be readable by JS"
        );
    }

    #[test]
    fn stale_setup_token_file_is_removed_on_normal_boot() {
        // 上一次初始化没走完（写了 token 文件就断电）→ 重启时必须清掉，
        // 否则运维会拿着一份早已作废的 token 反复试到锁定。
        let d = tempdir();
        let stale = routes::setup::write_setup_token_file(d.path(), "stale-token").unwrap();
        assert!(stale.exists());
        let pool = hub_storage::db::open_in(d.path()).unwrap().1;
        let st = AppState::bootstrap_with(
            pool,
            d.path(),
            AuthParams::default(),
            Duration::from_millis(10),
            LoginRateLimiter::new(1, 1),
        )
        .unwrap();
        st.auth().setup_password("correct horse battery").unwrap();
        // 已初始化 → bootstrap 走「正常模式」分支
        let pool2 = hub_storage::db::open_in(d.path()).unwrap().1;
        let _ = AppState::bootstrap_with(
            pool2,
            d.path(),
            AuthParams::default(),
            Duration::from_millis(10),
            LoginRateLimiter::new(1, 1),
        )
        .unwrap();
        assert!(!stale.exists(), "正常模式启动必须删掉残留 token 文件");
        // 正常模式没有活动 token：谁拿旧 token 来都不认
        assert!(st.check_setup_token("stale-token").is_err());
    }

    #[test]
    fn api_error_builders_all_construct_with_only_a_code() {
        // 这些构造器是错误响应的入口，必须各自可构造、且**不带任何 secret**。
        // 错误体只有固定 code：把一段「像是 secret」的文本喂进去也不会泄漏。
        let cases = [
            ApiError::bad_request("x").status(),
            ApiError::unauthorized("x").status(),
            ApiError::forbidden("x").status(),
            ApiError::conflict("x").status(),
            ApiError::too_many_requests("x").status(),
            ApiError::internal().status(),
        ];
        let expected = [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::CONFLICT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
        ];
        assert_eq!(cases, expected);
        // bad_request_body 是带固定 code 的 bad_request 别名
        assert_eq!(ApiError::bad_request_body().code(), "invalid_request_body");
    }

    #[test]
    fn from_auth_maps_storage_errors_to_internal_without_leaking() {
        // 存储层错误 → 500，且错误体只有固定 code，不夹带 storage 的报错串
        // （可能含数据库路径）。这是「初始化/取密钥」失败时的真实返回路径。
        let e = ApiError::from_auth(AuthError::NotInitialized);
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
        let e = ApiError::from_auth(AuthError::Storage(hub_storage::StorageError::Io(
            std::io::Error::other("injected storage failure"),
        )));
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// `internal_with` 与 [`Self::from_auth`] 同一条安全边界：**响应体只有固定
    /// code**，附带的说明只进 `tracing`。M0 唯一调用点是「签发 session cookie 失败」，
    /// 那条错误串可能含密钥材料——泄漏出去等于把签名密钥交出去。
    #[test]
    fn internal_with_never_leaks_the_context_into_the_body() {
        let e = ApiError::internal_with(
            "cannot sign the session cookie",
            AuthError::Storage(hub_storage::StorageError::Io(std::io::Error::other(
                "leak-me",
            ))),
        );
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
        // ApiError 本身不带消息串——它只有 status + code 两个字段，
        // 泄漏的唯一途径就是给这条错误再加一个字段
        assert_eq!(format!("{:?}", e), format!("{:?}", ApiError::internal()));
    }

    #[test]
    fn setup_token_file_is_0600_and_missing_parent_is_creatable() {
        // write_setup_token_file 必须 owner-only；并把嵌套目录也建出来。
        let d = tempdir();
        let nested = d.path().join("a").join("b");
        let p = routes::setup::write_setup_token_file(&nested, "tok").unwrap();
        assert!(p.exists());
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&p).unwrap().permissions())
                & 0o777;
        assert_eq!(mode, 0o600);
        // 读出来就是写进去的
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim(), "tok");
    }

    #[test]
    fn remove_setup_token_file_handles_present_and_absent() {
        let d = tempdir();
        // 没有 token 文件 → 删是 no-op，不报错（正常模式重启时它本就不该在）
        routes::setup::remove_setup_token_file(d.path()).unwrap();
        // 放一个文件名叫 setup-token.txt 再删它，才是「删成功」路径
        let p = d.path().join("setup-token.txt");
        std::fs::write(&p, "x").unwrap();
        routes::setup::remove_setup_token_file(d.path()).unwrap();
        assert!(!p.exists());
        // 再来一次：文件已经没了 → 仍不报错（NotFound 被吞）
        routes::setup::remove_setup_token_file(d.path()).unwrap();
    }

    #[test]
    fn remove_setup_token_file_survives_non_notfound_errors() {
        // `setup-token.txt` 是个**目录**而不是文件 → remove_file 报 IsADirectory
        // （不是 NotFound）→ 必须原样向上返回，不能静默吞掉。
        let d = tempdir();
        let dir = d.path().join("setup-token.txt");
        std::fs::create_dir(&dir).unwrap();
        assert!(routes::setup::remove_setup_token_file(d.path()).is_err());
    }

    #[test]
    fn invalidate_setup_token_warns_but_never_fails_when_token_path_is_a_dir() {
        // 删不掉 token 文件（这里让它是个目录 → remove_file 报 IsADirectory）
        // **不得**让初始化失败、也不得 panic，只是记 warn。
        let d = tempdir();
        let st = AppState::bootstrap_with(
            hub_storage::db::open_in(d.path()).unwrap().1,
            d.path(),
            AuthParams::default(),
            Duration::from_millis(10),
            LoginRateLimiter::new(1, 1),
        )
        .unwrap();
        st.auth().setup_password("correct horse battery").unwrap();
        // 把 token 文件替换成目录 → 删除必失败
        let p = d.path().join("setup-token.txt");
        std::fs::remove_file(&p).ok();
        std::fs::create_dir(&p).unwrap();
        st.invalidate_setup_token(); // 不得 panic / 不得返回错误
        assert!(st.is_initialized().unwrap());
    }

    #[test]
    fn bootstrap_warns_but_still_boots_when_stale_token_path_is_a_dir() {
        // 正常模式重启 + 残留的 token「文件」其实是目录 → 删不掉 → 只 warn、
        // 不阻塞启动（删不掉不影响「Hub 已初始化」这个事实）。
        let d = tempdir();
        let st = AppState::bootstrap_with(
            hub_storage::db::open_in(d.path()).unwrap().1,
            d.path(),
            AuthParams::default(),
            Duration::from_millis(10),
            LoginRateLimiter::new(1, 1),
        )
        .unwrap();
        st.auth().setup_password("correct horse battery").unwrap();
        // 残留：一个删不掉的目录
        let stale = d.path().join("setup-token.txt");
        std::fs::remove_file(&stale).ok();
        std::fs::create_dir(&stale).unwrap();

        let st2 = AppState::bootstrap_with(
            hub_storage::db::open_in(d.path()).unwrap().1,
            d.path(),
            AuthParams::default(),
            Duration::from_millis(10),
            LoginRateLimiter::new(1, 1),
        )
        .unwrap();
        // 启动没有被残留文件挡住，且仍是正常模式
        assert!(st2.is_initialized().unwrap());
        assert!(stale.exists());
    }
}
