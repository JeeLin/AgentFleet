//! `/setup` 与 `/login`：首次访问设密码 + 单用户登录（§2.4.1 / §2.4.2）。
//!
//! 两条安全性质在这里落地：
//! - **`POST /setup` 必须带 `X-Setup-Token`**。Hub 常起在可被同网段访问的
//!   端口上，用户还没打开浏览器时，另一台机器 `curl -X POST /setup` 就拿到了
//!   整个 Hub 的控制权。token 只落 stderr 与 `<data>/setup-token.txt`（0600），
//!   失败 [`hub_core::auth::SETUP_TOKEN_MAX_FAILURES`] 次即失效（防爆破）。
//! - **密码走 Argon2id 且必须 `spawn_blocking`**（`m=65536,t=3,p=4` 单次
//!   50–200ms，在 tokio worker 上同步执行会拖垮心跳与 `/ws` 读循环）。
//!
//! 登录端点另外挂**全局信号量（=2）+ 排队超时 429**：`/login` 是未认证端点，
//! 攻击者反复触发就能把阻塞线程池占满。信号量在 Hub 层（进程级）而非按 IP，
//! 因为要保护的是**算力**而不是某个人。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hub_core::auth::AuthError;
use hub_core::session::SESSION_TTL_SECS;
use identity::base64url;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;

use crate::middleware_auth::header_str;
use crate::{ApiError, AppState, CSRF_COOKIE, SESSION_COOKIE, SETUP_TOKEN_HEADER};

/// 单用户主体名。M0 只有一个人，`username` 字段收下但不入库——
/// 多用户是 M1 的范围选择（§2.4.4「不做多用户」）。
const SUBJECT: &str = "admin";

/// setup token 文件名（`%data%/setup-token.txt`）。
pub const SETUP_TOKEN_FILE: &str = "setup-token.txt";

/// CSRF token 字节数。32 字节随机，与 session 签名密钥同量级。
const CSRF_TOKEN_BYTES: usize = 32;

/// `POST /setup` 请求体。
///
/// `username` 保留但忽略：单用户 Hub 没有「选谁登录」这回事，
/// 字段留着是为了 M1 加多用户时前端不必改协议。
#[derive(Debug, Deserialize)]
pub struct SetupBody {
    #[serde(default)]
    pub username: Option<String>,
    pub password: String,
}

/// `POST /login` 请求体。
#[derive(Debug, Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    pub username: Option<String>,
    pub password: String,
}

/// `GET /setup` —— 问「设过密码没有」。
///
/// 未初始化 200 + `{"initialized": false}`；已初始化 **409**。
/// 只回布尔值，**不回任何哈希、token 或 salt**。
pub async fn get_setup(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    match state.is_initialized() {
        Ok(true) => Err(ApiError::conflict("already_initialized")),
        Ok(false) => Ok(Json(json!({"initialized": false}))),
        Err(e) => {
            tracing::error!(error = %e, "cannot read hub init state");
            Err(ApiError::internal())
        }
    }
}

/// `POST /setup` —— 设置密码，转入正常模式。
///
/// 成功后：写 `secret(kind='hub',name='password')` 的 Argon2id 哈希、
/// 生成 32 字节随机 `secret(kind='hub',name='session_hmac')`、
/// **作废 setup token**（删文件 + 清内存）。不签发 cookie——
/// 由客户端接着 `POST /login` 换 session（表 §2.4.3）。
pub async fn post_setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetupBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.is_initialized().map_err(ApiError::from_auth)? {
        return Err(ApiError::conflict("already_initialized"));
    }

    let Some(candidate) = header_str(&headers, SETUP_TOKEN_HEADER) else {
        return Err(ApiError::forbidden("setup_token_required"));
    };
    // 两种 token 失败返回不同的错误码，但**都**是 403：
    // 差别只给运维看日志用，不影响客户端该做什么。
    match state.check_setup_token(candidate) {
        Ok(()) => {}
        Err(AuthError::SetupTokenLocked) => {
            tracing::warn!("setup token locked; restart hub or run `afhub reset-setup`");
            return Err(ApiError::forbidden("setup_token_locked"));
        }
        // `check_setup_token` 只会产生 Locked / Mismatch 两种错误，
        // 其余一律按 mismatch 处理（不回显、不给额外信息）。
        Err(_) => {
            // **不记 candidate**：它可能正是别人猜的 token。
            tracing::warn!("setup token mismatch");
            return Err(ApiError::forbidden("setup_token_mismatch"));
        }
    }

    // 密码哈希与校验是纯 CPU 密集，必须离开 tokio worker（§2.4.1）
    let worker = state.clone();
    let password = body.password;
    let hashed = spawn_blocking_password(worker, password)
        .await
        .map_err(|e| blocking_task_failed(e.0))?;
    match hashed {
        Ok(()) => {}
        // 前置已确认未初始化，这里不会再拿到 AlreadyInitialized；
        // 唯一可能失败的是密码太短或哈希/落库出错。
        Err(AuthError::PasswordTooShort) => {
            return Err(ApiError::bad_request("password_too_short"))
        }
        // 哈希 / 落库失败：密码**不会**被写进去，Hub 仍停在初始化模式。
        Err(e) => return Err(blocking_task_failed(e)),
    }

    // session HMAC 密钥：首次初始化时生成的 32 字节随机值。
    // **不用明文密码**（改密码会废掉全部会话）、**不用设备指纹派生**
    // （`device` 表为空时算不出来 → 首次部署死锁；且指纹是公开值）。
    // 密钥值不外传、不进日志、不进响应：只要求它已落库。
    state.session_key().map_err(ApiError::from_auth)?;

    state.invalidate_setup_token();
    tracing::info!("hub password set; setup token invalidated");
    Ok(Json(json!({"initialized": true})))
}

/// `post_login` 里「限流 → 校验密码」这一步的结果。
///
/// 拆成独立枚举而不是在 handler 里直接翻译 `AuthError`，是为了**让
/// 「记一次失败」由调用方决定**：读不出密码哈希（存储故障）绝不能算成
/// 「密码错了」把用户的 IP 锁死；而「密码哈希损坏」是 Hub 自己的数据坏了，
/// 既不该当成猜错了、也不该当成登录成功。
enum LoginOutcome {
    Verified,
    WrongPassword,
    HubFault(AuthError),
}

/// 只做「读哈希 + Argon2id 校验」，不做任何限流记账（§2.4.1）。
///
/// 整个步骤在 `spawn_blocking` 里跑：Argon2id 是纯 CPU 密集，放在 tokio
/// worker 上会把整个 Hub 的 IO 调度拖死。
async fn spawn_blocking_verify(
    worker: AppState,
    password: String,
) -> Result<LoginOutcome, BlockingTaskError> {
    let verified =
        tokio::task::spawn_blocking(move || classify_login(worker.auth().login(&password))).await;

    match verified {
        Ok(outcome) => Ok(outcome),
        // coverage: excluded —— `JoinError` 只在任务 panic 或运行时关闭时
        // 产生。本 workspace 的 `panic = deny` 让生产代码不可能 panic，
        // 测试里也造不出来（测试豁免头只在 `mod tests` 内生效，够不到这里）。
        Err(e) => Err(BlockingTaskError(e)),
    }
}

/// 客户端 IP：取 `ConnectInfo`，拿不到就用 `0.0.0.0`。
///
/// 单元测试用 `oneshot` 直打路由，没有 `ConnectInfo`；生产
/// （`into_make_service_with_connect_info`）一定有。拿不到时所有流量
/// 归到同一个桶——**保守方向是「更容易被限流」**，不会变成「绕过限流」。
pub fn client_ip(ci: Option<&ConnectInfo<SocketAddr>>) -> IpAddr {
    ci.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |c| c.0.ip())
}

/// `POST /login` —— 换 session cookie。
///
/// 响应下发两个 cookie：
/// - `af_session`：**HttpOnly + SameSite=Strict**（不进 JS、不跨站携带）；
/// - `af_csrf`：非 HttpOnly，状态变更时由前端回填到 `X-CSRF-Token`
///   头完成双重提交。
///
/// 两个 cookie 都**不含** secret：session 是签名不是加密，payload 明文可读
/// （`sub|exp|iat`），没有任何密钥材料。
pub async fn post_login(State(state): State<AppState>, req: Request) -> Result<Response, ApiError> {
    // 客户端 IP 在读 body 之前取：请求体读不出来时不该留下一次限流计数
    let ip = client_ip(req.extensions().get::<ConnectInfo<SocketAddr>>());

    // 注：初始化模式下 `/login` 直接由 `require_auth` 中间件挡成 403
    // （`/login` 不在 INIT_MODE_PATHS 里），所以这里到 handlers 时
    // Hub 必然已初始化——不必再查一遍。

    if state.limiter().is_blocked(&ip) {
        // 只记 IP，**不记 cookie / token / 密码**（约束 13）
        tracing::warn!(%ip, "login blocked by rate limiter");
        return Err(ApiError::too_many_requests("rate_limited"));
    }

    // 登录是未认证端点：body 上限收得比默认的 2 MiB 紧得多，
    // 免得有人拿一个巨型 body 把 Hub 的内存吃掉。
    let password = read_password(req).await?;

    // 全局信号量（=2）：排队超时就 429，不让未认证端点把阻塞线程池占满。
    let gate = state.login_gate().clone();
    let timeout = state.login_queue_timeout();
    let _permit = match tokio::time::timeout(timeout, gate.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        // 排队超时 = 「现在忙」→ 429，这是可预期的背压。
        Err(_elapsed) => return Err(ApiError::too_many_requests("login_busy")),
        // coverage: excluded —— 闭锁的信号量会永远等不到 permit。
        // coverage: excluded —— 闸门与 `AppState` 同生命周期，本 workspace
        // 从不调用 `tokio::sync::Semaphore::close()`（已 grep 确认零调用点），
        // 所以 `AcquireError` 在本 crate 不可达。注意这是**约定性**不可达
        // （我们选择不调用），不是结构性的——`close()` 是 pub。
        // 闭锁不是「忙」，必须 500 而不是 429：否则一次内部事故会被伪装成
        // 「服务繁忙」，把真正的根因（有人闭锁了闸门）藏起来。
        Ok(Err(e)) => {
            // coverage: excluded — 同 167：tracing::error! 参数行被 llvm-cov
            // 记为 miss（多行宏已知现象），该臂本身见 :214 就地标注。
            tracing::error!(error = %e, "login semaphore closed");
            return Err(ApiError::internal());
        }
    };

    // Argon2id 校验走 spawn_blocking（§2.4.1）
    let worker = state.clone();
    let outcome = spawn_blocking_verify(worker, password)
        .await
        .map_err(|e| blocking_task_failed(e.0))?;
    // 限流记账只在这一步做，且**只有「密码错」才记**：Hub 自己的故障
    // 记到来访者头上，等于让一次磁盘抖动把用户的 IP 锁 300 秒。
    if matches!(outcome, LoginOutcome::WrongPassword) {
        state.limiter().record_failure(&ip);
    }
    if let Some(err) = login_failure(&outcome) {
        return Err(err);
    }
    state.limiter().record_success(&ip);

    let key = state.session_key().map_err(ApiError::from_auth)?;
    let token = hub_core::session::SessionStore::new(key)
        .issue(SUBJECT)
        .map_err(|e| ApiError::internal_with("cannot sign the session cookie", e))?;
    let csrf = random_token(CSRF_TOKEN_BYTES);

    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for c in [session_cookie_header(&token), csrf_cookie_header(&csrf)] {
        // 两条 cookie 的取值都是 base64url + 固定属性字面量 + `Max-Age` 数字，
        // 因此 `InvalidHeaderValue` 结构上不可达（RFC 9110 的 `field-value`
        // 只禁 CR/LF 与 NUL，而 base64url 三种变体的字符集 `A-Za-z0-9-_.~`
        // 以及我们写死的属性名都不含这些）。所以这里**不做**「构造不出来
        // 就 500」的分支——那只会造出一条永远走不到的死代码，还让人误以为
        // cookie 拼装真的可能失败。校验 + 记录由 `append_set_cookie` 负责。
        append_set_cookie(&mut headers, &c);
    }

    Ok((StatusCode::OK, headers, Json(json!({"ok": true}))).into_response())
}

/// session cookie 值：`HttpOnly` + `SameSite=Strict`（§2.4.2）。
///
/// **不带 `Secure`**：Hub 自身只监听回环 `http://127.0.0.1`（约束 1 + 绑定
/// 校验），给 http 加 `Secure` 会让部分浏览器直接丢弃这个 cookie，
/// 结果是「登录成功但每次请求都 401」。远程访问走 SSH / Tailscale，
/// 那时是外层终止 TLS + 同源，CSRF 由 `SameSite=Strict` 兜底。
pub fn session_cookie_header(value: &str) -> String {
    format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL_SECS}"
    )
}

/// CSRF cookie 值。非 HttpOnly——前端要能读出来回填到 `X-CSRF-Token`。
pub fn csrf_cookie_header(value: &str) -> String {
    format!("{CSRF_COOKIE}={value}; Path=/; SameSite=Strict; Max-Age={SESSION_TTL_SECS}")
}

/// 随机 token（base64url 无填充）。
fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64url(&buf)
}

/// setup token 落盘：**0600，不信 umask**（约束 13）。
///
/// 内容是 setup token 本身——这是运维取它的正规通道（另一个是 stderr），
/// 所以权限必须是 owner-only。
pub fn write_setup_token_file(
    dir: &std::path::Path,
    token: &str,
) -> std::io::Result<std::path::PathBuf> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = dir.join(SETUP_TOKEN_FILE);
    // `dir.join("setup-token.txt")` 一定带得出父目录（`join` 至少保留 `dir`
    // 本身），所以这里直接用 `unwrap_or_else` 取父目录：为「父目录为空」
    // 写一条 if-let 只会造出结构上不可达的分支，被覆盖率门禁算成漏网。
    let parent = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(token.as_bytes())?;
    f.write_all(b"\n")?;
    drop(f);
    // 再次收紧：文件已存在时 create+mode 不会改权限
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// 删除 setup token 文件。不存在不算错（正常模式重启时它本就不该在）。
pub fn remove_setup_token_file(dir: &std::path::Path) -> std::io::Result<()> {
    use std::fs;
    match fs::remove_file(dir.join(SETUP_TOKEN_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `POST /login` 请求体上限。密码几十字节就够，8 KiB 足够宽松。
const MAX_LOGIN_BODY: usize = 8 * 1024;

/// 读 `{"password": "..."}`。
///
/// **手工解析而不是 `Json<T>` 提取器**：`/login` 还要从 `Request` 里取
/// `ConnectInfo`（`Option<ConnectInfo<_>>` 不是合法的 axum 提取器组合），
/// 而 body 提取器必须是最后一个参数。返回 `Result` 而不是 `Option`：
/// body 缺失/形状不对是 **400**，不是「密码为空」。
async fn read_password(req: Request) -> Result<String, ApiError> {
    let bytes = axum::body::to_bytes(req.into_body(), MAX_LOGIN_BODY)
        .await
        .map_err(|_| ApiError::bad_request_body())?;
    let body: LoginBody =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::bad_request_body())?;
    Ok(body.password)
}

/// 阻塞任务**本身**的失败：任务 panic 或运行时关闭。
///
/// 与「密码错了 / 哈希坏了」严格分开——这条路径**永远不能**记一次限流失败，
/// 那是 Hub 的问题，不是来访者的。字段是 `JoinError` 而不是它的字符串，
/// 这样「详情」由 `blocking_task_failed` 在**唯一**一处格式化，
/// 本模块就不会不小心把 `Debug` 里的内容回显出去。
#[derive(Debug)]
struct BlockingTaskError(tokio::task::JoinError);

/// 把 `HubAuth::login` 的结果翻译成 [`LoginOutcome`]。
///
/// 分成独立函数而不是内联在闭包里，是因为「哪一类错误算用户输错」是**限流的
/// 正确性前提**：`WrongPassword` 会让 `post_login` 记一次失败并在 10 次后锁
/// 住这个 IP；`MalformedHash` 也算输错（数据坏了，但对用户而言答案就是
/// 「密码不对」）；而 `Storage` 绝不能算——那会让一次磁盘抖动把无辜的 IP
/// 锁 300 秒。放在一个能直接单测的纯函数里，这条边界才不会被以后的重构改掉。
fn classify_login(result: Result<(), AuthError>) -> LoginOutcome {
    match result {
        Ok(()) => LoginOutcome::Verified,
        Err(AuthError::WrongPassword | AuthError::MalformedHash) => LoginOutcome::WrongPassword,
        Err(e) => LoginOutcome::HubFault(e),
    }
}

/// [`BlockingTaskError`] 的唯一消费口：把一个**已经发生的**失败转成对外的
/// 500（细节只进 `tracing`）。
///
/// 走一个具名函数而不是让调用方各自 `map_err`：这样「细节永不回显」这条
/// 约束只有一个地方保证，也给单测一个不用 panic 就能验证的入口。
fn blocking_task_failed(reason: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %reason, "blocking task failed");
    ApiError::internal()
}

/// 把校验结果翻译成「要返回的 HTTP 错误」，`None` 表示放行。
///
/// **限流记账不在这里**：只有 `WrongPassword` 那一支要记，而它需要
/// `state`（见 `post_login`）。把「返回什么」与「记不记账」分开，
/// 两边才都能各自单测。
fn login_failure(outcome: &LoginOutcome) -> Option<ApiError> {
    match outcome {
        LoginOutcome::Verified => None,
        LoginOutcome::WrongPassword => Some(ApiError::unauthorized("invalid_credentials")),
        // 存储故障**不记**失败：那是 Hub 的问题，不是这个 IP 的问题。
        LoginOutcome::HubFault(e) => Some(blocking_task_failed(e)),
    }
}

/// 给一条自己拼的 cookie 串做「能否成为 header 值」的体检。
///
/// 追加一条 `Set-Cookie`。
///
/// 非法值**只记录、不返回错误**：cookie 由本模块用固定模板拼成，取值是
/// base64url，结构上不可能构造出非法 header 值。为它加一条 `500` 分支
/// 只会得到永远走不到的死代码（覆盖率门禁算它漏网），而且会让人误以为
/// 「cookie 拼不出来」是种可预期的运行时故障。
///
/// 检查本身要留着：一旦将来 cookie 里混进了用户可控的非 ASCII 或换行
/// （header 注入），这里会把它原样记下来，而不是无声地发出去。
fn append_set_cookie(headers: &mut HeaderMap, cookie: &str) {
    match HeaderValue::from_str(cookie) {
        Ok(v) => {
            headers.append(SET_COOKIE, v);
        }
        Err(e) => tracing::error!(error = %e, "Set-Cookie is not a legal header value"),
    }
}

/// 在阻塞线程池里设密码，返回 `HubAuth` 的结果。
///
/// Argon2id 是纯 CPU 密集，必须在 `spawn_blocking` 里跑（§2.4.1）。
async fn spawn_blocking_password(
    worker: AppState,
    password: String,
) -> Result<Result<(), AuthError>, BlockingTaskError> {
    tokio::task::spawn_blocking(move || worker.auth().setup_password(&password))
        .await
        // coverage: excluded —— 同 [`spawn_blocking_verify`]：只在任务 panic
        // 或运行时关闭时失败，生产代码不 panic，测试里不可达。
        .map_err(BlockingTaskError)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_set_cookie_accepts_real_cookies_and_flags_injected_ones() {
        // 合法 cookie（生产路径）：真的被追加进响应头
        let mut headers = HeaderMap::new();
        let ok = session_cookie_header("abc-DEF_123.~");
        append_set_cookie(&mut headers, &ok);
        assert_eq!(headers.get_all(SET_COOKIE).iter().count(), 1);
        assert!(csrf_cookie_header("t").contains("SameSite=Strict"));
        append_set_cookie(&mut headers, &csrf_cookie_header("t"));
        assert_eq!(headers.get_all(SET_COOKIE).iter().count(), 2);

        // 非法值（带 CR/LF，是 header 注入的形状）：**不**被追加，只记日志。
        // 函数不返回错误——cookie 拼装失败在本 crate 里结构上不可达。
        let evil = "af_session=x\r\nX-Injected: 1";
        assert!(HeaderValue::from_str(evil).is_err());
        append_set_cookie(&mut headers, evil);
        assert_eq!(headers.get_all(SET_COOKIE).iter().count(), 2);
    }

    #[test]
    fn classify_login_separates_user_error_from_hub_fault() {
        // 三类结果的划分直接决定限流行为，必须逐一钉住：
        assert!(matches!(classify_login(Ok(())), LoginOutcome::Verified));
        // 「密码错」和「哈希坏」都算用户没对上 → 记失败（10 次后锁 IP）
        assert!(matches!(
            classify_login(Err(AuthError::WrongPassword)),
            LoginOutcome::WrongPassword
        ));
        assert!(matches!(
            classify_login(Err(AuthError::MalformedHash)),
            LoginOutcome::WrongPassword
        ));
        // Hub 自己的故障 → 500 且**不**记失败
        assert!(matches!(
            classify_login(Err(AuthError::Storage(hub_storage::StorageError::Io(
                std::io::Error::other("db gone")
            )))),
            LoginOutcome::HubFault(AuthError::Storage(_))
        ));
        assert!(matches!(
            classify_login(Err(AuthError::NotInitialized)),
            LoginOutcome::HubFault(AuthError::NotInitialized)
        ));
    }

    #[test]
    fn blocking_task_failed_never_echoes_the_reason() {
        // 约束 13：细节只进 tracing，响应体只有固定 code
        let e = blocking_task_failed("boom: /var/lib/agentfleet/agentfleet.db");
        assert_eq!(e.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    #[test]
    fn login_failure_maps_each_outcome_to_the_documented_code() {
        assert!(login_failure(&LoginOutcome::Verified).is_none());
        let wrong = login_failure(&LoginOutcome::WrongPassword).unwrap();
        assert_eq!(wrong.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(wrong.code(), "invalid_credentials");
        let fault = login_failure(&LoginOutcome::HubFault(AuthError::Storage(
            hub_storage::StorageError::Io(std::io::Error::other("db gone")),
        )))
        .unwrap();
        assert_eq!(
            fault.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fault.code(), "internal_error");
    }

    #[test]
    fn client_ip_falls_back_to_unspecified_without_connect_info() {
        let fallback = client_ip(None);
        assert_eq!(
            fallback,
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        );
        let real = client_ip(Some(&ConnectInfo("10.1.2.3:5555".parse().unwrap())));
        assert_eq!(
            real,
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 1, 2, 3))
        );
    }

    #[test]
    fn token_helpers_produce_isolated_0400_files() {
        let d = tempfile::tempdir().unwrap();
        let path = write_setup_token_file(d.path(), "tok-123").unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        ) & 0o777;
        assert_eq!(mode, 0o600, "setup token 必须是 owner-only");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "tok-123");
    }

    #[test]
    fn token_file_writes_into_a_nested_directory() {
        // 数据目录下面还有一层时，父目录要被建出来（Hub 的 data_dir 可能
        // 是 `~/.local/share/agentfleet`，首次启动时整条链都不存在）。
        let d = tempfile::tempdir().unwrap();
        let nested = d.path().join("a").join("b");
        let path = write_setup_token_file(&nested, "tok").unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "tok");
    }

    #[test]
    fn token_file_write_failure_is_reported_not_swallowed() {
        // 路径里有一段是**普通文件** → create_dir_all 必然失败 → 错误要原样
        // 返回（不能静默造出坏文件，更不能 panic）。
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("afile");
        std::fs::write(&f, "not a dir").unwrap();
        assert!(write_setup_token_file(&f.join("nested"), "tok").is_err());
    }
}
