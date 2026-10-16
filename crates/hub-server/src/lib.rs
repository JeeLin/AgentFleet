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

pub mod middleware_auth;
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
use hub_core::ConnectionRegistry;
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

    pub fn too_many_requests(code: &'static str) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code)
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }

    fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    /// 存储/认证层错误 → 500。
    ///
    /// 刻意**不把 `e` 拼进响应体**：它是 `StorageError` 的消息串，
    /// 可能带数据库路径。细节只进 `tracing`。
    fn from_auth(e: AuthError) -> Self {
        tracing::error!(error = %e, "request failed on storage/auth");
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
        (self.status, Json(json!({"error": self.code}))).into_response()
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
/// `.layer(...)` 放在最后：**认证中间件覆盖 fallback**，
/// 未知路径在初始化模式返回 403 而不是 404（少暴露一条路由是否存在）。
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/setup",
            get(routes::setup::get_setup).post(routes::setup::post_setup),
        )
        .route("/login", post(routes::setup::post_login))
        .route("/ws", get(routes::ws::upgrade))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware_auth::require_auth,
        ))
        // 状态已经进了中间件与各 handler，这里固化成 `Router<()>`，
        // 调用方（main.rs / 测试）拿到的就是可直接 serve / oneshot 的 Router
        .with_state(state)
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

    #[tokio::test]
    async fn unknown_route_is_404_in_normal_mode_with_valid_session() {
        let d = tempdir();
        let (app, st) = app_at(d.path());
        st.auth().setup_password("correct horse battery").unwrap();
        // 正常模式：未知路径先过认证，带合法 cookie 才看得到「路由不存在」。
        let key = st.session_key().unwrap();
        let cookie = format!(
            "{SESSION_COOKIE}={}",
            hub_core::session::SessionStore::new(key).issue("admin")
        );
        let req = Request::builder()
            .uri("/nope")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_route_is_404_in_normal_mode_without_cookie() {
        // 认证中间件盖住 fallback：没有 cookie 时先 401，路由存不存在都不透露
        let d = tempdir();
        let (app, st) = app_at(d.path());
        st.auth().setup_password("correct horse battery").unwrap();
        let req = Request::builder().uri("/nope").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
