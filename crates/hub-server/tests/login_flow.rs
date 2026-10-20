//! Hub 登录闭环的集成测试：逐条对应 M0 里程碑子任务 9 的 11 条测试标准。
//!
//! 映射关系写在每个测试的文档注释里（`标准①` … `标准⑪`），
//! 评审时可以直接对着里程碑核。
//!
//! 两条铁律：
//! 1. **不起真端口**：用 `tower::ServiceExt::oneshot` 直打 `build_app` 装配出的
//!    路由。端口与握手噪声不进来，失败信息只跟被测行为有关。
//! 2. **子进程按需才起**：需要真实 socket 的只有标准⑩（证明 tokio worker
//!    没被 Argon2id 堵死，单线程 runtime + 真并发才有说服力），
//!    其余全部走 oneshot。

// Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

// 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
// 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
// let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
// 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::fast_params;
use http_body_util::BodyExt;
use hub_core::auth::{AuthParams, HubAuth, LoginRateLimiter};
use hub_server::middleware_auth::require_auth;
use hub_server::{AppState, CSRF_COOKIE, SESSION_COOKIE};
use hub_storage::repo::DeviceRepo;
use serde_json::json;
use tower::ServiceExt;

/// 登录并发上限外的排队超时。默认 200ms：既能稳定触发 429，又不会拖慢用例。
const QUEUE_TIMEOUT: Duration = Duration::from_millis(200);

/// 一个测试用的 Hub 实例（真 SQLite 库 + 真路由）。
struct Harness {
    dir: tempfile::TempDir,
    app: Router,
    state: AppState,
    pool: hub_storage::db::Db,
}

impl Harness {
    fn with(params: AuthParams, queue: Duration, limiter: LoginRateLimiter) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let state =
            AppState::bootstrap_with(pool.clone(), dir.path(), params, queue, limiter).unwrap();
        let app = hub_server::build_app(state.clone());
        Self {
            dir,
            app,
            state,
            pool,
        }
    }

    fn new() -> Self {
        Self::with(fast_params(), QUEUE_TIMEOUT, LoginRateLimiter::new(10, 60))
    }

    /// 从 `<data>/setup-token.txt` 读 setup token。
    ///
    /// 走文件而不是 `AppState` 的 getter：**这个文件就是运维取 token 的
    /// 正规通道**，测试用同一条路径顺带验证了它真的可用、且权限是 0600。
    fn setup_token(&self) -> String {
        let p = self.dir.path().join("setup-token.txt");
        std::fs::read_to_string(&p).unwrap().trim().to_string()
    }

    async fn send(&self, req: Request<Body>) -> Reply {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        Reply::new(resp).await
    }

    async fn get(&self, path: &str) -> Reply {
        self.send(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
    }

    /// `POST /setup`。`token` 为 `None` 时**不带** `X-Setup-Token` 头。
    async fn post_setup(&self, token: Option<&str>, password: &str) -> Reply {
        let mut b = Request::builder()
            .method("POST")
            .uri("/setup")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("x-setup-token", t);
        }
        self.send(
            b.body(Body::from(
                json!({"username": "me", "password": password}).to_string(),
            ))
            .unwrap(),
        )
        .await
    }

    /// `POST /login`。`ip` 给定时模拟不同来源（限流按 IP 分桶）。
    async fn post_login(&self, password: &str, ip: Option<&str>) -> Reply {
        self.send(login_req(password, ip)).await
    }

    /// 跑完首次初始化（设密码 + 拿一个有效 session cookie）。
    async fn initialized(&self) -> (String, String) {
        let token = self.setup_token();
        let r = self.post_setup(Some(&token), "correct horse battery").await;
        assert_eq!(r.status, StatusCode::OK, "setup failed: {}", r.body);
        self.login().await
    }

    /// 只设密码，不登录。
    async fn set_password(&self) {
        let token = self.setup_token();
        let r = self.post_setup(Some(&token), "correct horse battery").await;
        assert_eq!(r.status, StatusCode::OK, "setup failed: {}", r.body);
    }

    /// 用正确密码登录，返回 `(session cookie 值, csrf cookie 值)`。
    async fn login(&self) -> (String, String) {
        let r = self.post_login("correct horse battery", None).await;
        assert_eq!(r.status, StatusCode::OK, "login failed: {}", r.body);
        (
            r.cookie_value(SESSION_COOKIE).expect("session cookie"),
            r.cookie_value(CSRF_COOKIE).expect("csrf cookie"),
        )
    }
}

/// 一次性读出状态码 / 头 / 体。
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    async fn new(resp: axum::response::Response) -> Self {
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Self {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// 取某个 `Set-Cookie` 的值。
    fn cookie_value(&self, name: &str) -> Option<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|raw| {
                let pair = raw.split(';').next()?;
                let (k, v) = pair.trim().split_once('=')?;
                (k == name).then(|| v.to_string())
            })
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// 装配一个**受保护**的路由，用来断言「有 cookie 能访问 / 没 cookie 401」。
///
/// M0 还没有任何 `/api/*` 业务路由（设备 API 属后续子任务），而测试标准⑥
/// 要求证明「有效 cookie 可访问」。用与生产**同一个中间件**包一层探针路由：
/// 认证链是真实的那条，断言的不是自造的假。
fn protected_app(state: &AppState) -> Router {
    Router::new()
        .route("/api/devices", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/nodes", post(|| async { Json(json!({"ok": true})) }))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state.clone())
}

async fn call(app: &Router, req: Request<Body>) -> Reply {
    Reply::new(app.clone().oneshot(req).await.unwrap()).await
}

fn cookie_header(pairs: &[(&str, &str)]) -> String {
    let mut s = String::new();
    for (k, v) in pairs {
        s.push_str(k);
        s.push('=');
        s.push_str(v);
        s.push_str("; ");
    }
    s.trim_end().to_string()
}

fn get_req(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(path);
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(Body::empty()).unwrap()
}

/// `POST /login` 请求（`ip` 给定时带上 `ConnectInfo`，供限流分桶）。
fn login_req(password: &str, ip: Option<&str>) -> Request<Body> {
    let req0 = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "password": password }).to_string()))
        .unwrap();
    let mut req = req0;
    if let Some(ip) = ip {
        let addr: std::net::SocketAddr = format!("{ip}:5555").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
    }
    req
}

// ---------------------------------------------------------------------------
// 标准①：首次启动未初始化，除豁免表外一律 403
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_01_first_start_is_uninitialized_and_blocks_the_rest() {
    let h = Harness::new();

    let r = h.get("/api/setup").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["initialized"], json!(false));

    // 初始化模式下「一切除豁免表外都是 403」，**不是 401**
    assert_eq!(h.get("/api/devices").await.status, StatusCode::FORBIDDEN);
    // `GET /login` 与 `GET /setup` 都是**页面**（SPA 外壳），与 `/` 同属
    // 免认证静态资产，未初始化时也要能打开——否则首次部署时登录页 /
    // setup 页自己加载不出来，而用户手里正拿着一个必须粘进页面的 token。
    assert_eq!(h.get("/login").await.status, StatusCode::OK);
    assert_eq!(h.get("/setup").await.status, StatusCode::OK);
    // `POST /login` 才是认证端点：它在豁免表内（登录前没有 session），
    // 但**不豁免初始化模式门禁**——没有密码可验，403。
    assert_eq!(
        h.post_login("whatever", None).await.status,
        StatusCode::FORBIDDEN
    );
    let login = h.post_login("whatever", None).await;
    assert_eq!(login.json()["error"], json!("hub_not_initialized"));

    // 豁免表内的照常可用
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
    assert!(h.dir.path().join("setup-token.txt").exists());
}

// ---------------------------------------------------------------------------
// 标准②：不带 X-Setup-Token 的 POST /setup → 403
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_02_setup_without_token_is_forbidden() {
    let h = Harness::new();

    let r = h.post_setup(None, "correct horse battery").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"], json!("setup_token_required"));
    // 没设成 → 仍然未初始化，其余路由仍然 403
    assert!(!h.state.is_initialized().unwrap());

    // 错误 token 同样 403，且要计数（错满 10 次即失效）
    for _ in 0..hub_core::auth::SETUP_TOKEN_MAX_FAILURES {
        let r = h
            .post_setup(Some("wrong-token-guess"), "correct horse battery")
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        assert_eq!(r.json()["error"], json!("setup_token_mismatch"));
    }
    // 第 10 次失败之后 token 即失效：连正确 token 也不认（防爆破）
    // 锁死之后连正确 token 也不认（防爆破）
    let r = h
        .post_setup(Some(&h.setup_token()), "correct horse battery")
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"], json!("setup_token_locked"));
    assert!(!h.state.is_initialized().unwrap());
}

// ---------------------------------------------------------------------------
// 标准③：设完密码后 GET /api/setup → 409
//
// 路径自 M1 起是 `/api/setup`：`/setup` 让给了**页面**（见
// `get_setup_serves_the_spa_shell_and_the_probe_lives_at_api_setup`）。
// 标准锁的是探针的**语义**（已初始化 → 409 `already_initialized`），
// 不是它的路径——断言值一个没改。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_03_setup_conflicts_once_initialized() {
    let h = Harness::new();
    let token = h.setup_token();
    let (session, _csrf) = h.initialized().await;

    let r = h.get("/api/setup").await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.json()["error"], json!("already_initialized"));

    // 已经初始化后，POST /setup 无论带不带 token 都是 409（抢初始化这条路彻底关死）
    assert_eq!(
        h.post_setup(None, "another password").await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.post_setup(Some(&token), "another password").await.status,
        StatusCode::CONFLICT
    );
    // 原密码仍然有效；正常模式下 /login 不再 403
    assert_eq!(
        h.post_login("correct horse battery", None).await.status,
        StatusCode::OK
    );
    let _ = session;
}

// ---------------------------------------------------------------------------
// M1 契约：`GET /setup` 是**页面**，JSON 探针在 `GET /api/setup`
//
// 这条是 M1 起的新契约（M0 时两者同路径）。`design-system.md` §5.2 的路由表里
// `/setup` 是「首次 setup」页面；而一条路径不能既回 HTML 又回 JSON——
// 硬加载（整页刷新 / 直接输地址）时浏览器只会拿到其中一种。
// 同构的拆分仓库里早有一份：`POST /login` 是端点、`GET /login` 是页面。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn get_setup_serves_the_spa_shell_and_the_probe_lives_at_api_setup() {
    let h = Harness::new();

    let page = h.get("/setup").await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "硬加载 /setup 必须是页面，不是 JSON 探针"
    );
    assert_eq!(
        page.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8"),
        "/setup 必须回 SPA 外壳（HTML），而不是探针的 JSON"
    );

    // 同一个 Harness 上探针仍照旧：初始化状态下 200 + initialized:false
    let probe = h.get("/api/setup").await;
    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.json()["initialized"], json!(false));
    assert!(h.dir.path().join("setup-token.txt").exists());
}

// ---------------------------------------------------------------------------
// 标准④：密码以 argon2id 哈希存，明文不入库
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_04_password_stored_as_argon2id_never_in_plaintext() {
    let h = Harness::new();
    let token = h.setup_token();
    let secret = "correct horse battery staple";
    let r = h.post_setup(Some(&token), secret).await;
    assert_eq!(r.status, StatusCode::OK);

    // secret 表里是 PHC 串，不是明文
    let stored = hub_storage::repo::SecretRepo::new(h.pool.clone())
        .get("hub", "password")
        .unwrap()
        .expect("password secret must exist");
    assert!(
        stored.starts_with("$argon2id$"),
        "必须是 argon2id: {stored}"
    );
    assert!(!stored.contains(secret));
    // salt 每用户随机 → m= 与 t= 参数齐全，salt 段非空
    assert!(stored.contains("m=") && stored.contains("t=") && stored.contains("p="));

    // 更狠一点：把整个数据库文件（含 WAL）翻一遍，明文一个字节都不许有
    for name in ["agentfleet.db", "agentfleet.db-wal"] {
        let p = h.dir.path().join(name);
        if p.exists() {
            let bytes = std::fs::read(&p).unwrap();
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "{name} 里出现了明文密码"
            );
        }
    }

    // setup token 文件在设置完成后必须消失（token 失效）
    assert!(!h.dir.path().join("setup-token.txt").exists());
}

// ---------------------------------------------------------------------------
// 标准⑤：错误密码被拒且触发速率限制
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_05_wrong_password_is_rejected_and_triggers_rate_limit() {
    // 上限压到 2 次：测试要的是「触发限流」这个行为，不是等 10 次
    let h = Harness::with(fast_params(), QUEUE_TIMEOUT, LoginRateLimiter::new(2, 60));
    h.initialized().await;

    for _ in 0..2 {
        let r = h.post_login("wrong password", None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert_eq!(r.json()["error"], json!("invalid_credentials"));
    }
    // 锁住后**连正确密码也拒**：限流不能被「我知道密码」绕过
    let r = h.post_login("correct horse battery", None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json()["error"], json!("rate_limited"));

    // 不同 IP 独立计数：不能因为一个人爆破就把所有人挡在门外
    let other = h
        .post_login("correct horse battery", Some("10.0.0.9"))
        .await;
    assert_eq!(other.status, StatusCode::OK);
}

/// Hub 自己的故障（读不出密码哈希）→ 500，且**绝不**记成「密码错了」。
///
/// 这条性质很要紧：`login()` 把 storage 错误和 `WrongPassword` 分成两个分支，
/// 如果合并，一个磁盘/连接池抖动就会把用户 IP 锁 300 秒，而用户根本没输错。
#[tokio::test]
async fn storage_failure_during_login_is_500_and_does_not_count_as_a_wrong_password() {
    let h = Harness::with(fast_params(), QUEUE_TIMEOUT, LoginRateLimiter::new(2, 60));
    h.set_password().await;
    let app = hub_server::build_app(h.state.clone());
    // 初始化状态已落库（密码行还在），但登录时读它会失败
    h.pool
        .get()
        .unwrap()
        .execute("DROP TABLE secret", [])
        .unwrap();

    let req = login_req("correct horse battery", None);
    let r = Reply::new(app.clone().oneshot(req).await.unwrap()).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"], json!("internal_error"));
    assert!(
        !r.body.contains("sql"),
        "storage 报错串（含库名/表名）不得回显（约束 13）：{}",
        r.body
    );

    // 限流计数器必须还是 0：故障不是「猜错了」
    let ip = "10.0.0.7".parse().unwrap();
    assert_eq!(h.state.limiter().failures(&ip), 0);
    assert!(!h.state.limiter().is_blocked(&ip));
}

/// 已存的密码哈希损坏（`secret.value` 被改成非 PHC 串）→ 401，
/// 因为它**确实**证明「这个密码对不上」，必须计入失败计数。
///
/// 这是 `spawn_blocking_verify` 里把 `MalformedHash` 归入 `WrongPassword`
/// 的那个分支：数据坏了，但对用户而言答案就是「密码不对」。
#[tokio::test]
async fn corrupted_stored_hash_is_rejected_as_a_wrong_password() {
    let h = Harness::with(fast_params(), QUEUE_TIMEOUT, LoginRateLimiter::new(10, 60));
    h.set_password().await;
    let app = hub_server::build_app(h.state.clone());
    hub_storage::repo::SecretRepo::new(h.pool.clone())
        .set("hub", "password", "not-a-phc-string", "unix:1")
        .unwrap();

    let req = login_req("correct horse battery", Some("10.0.0.8"));
    let r = Reply::new(app.oneshot(req).await.unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"], json!("invalid_credentials"));
    let ip = "10.0.0.8".parse().unwrap();
    assert_eq!(h.state.limiter().failures(&ip), 1);
}

// ---------------------------------------------------------------------------
// 标准⑥：有效 cookie 可访问，缺 cookie 一律 401
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_06_valid_cookie_passes_missing_cookie_is_401() {
    let h = Harness::new();
    let (session, csrf) = h.initialized().await;
    let app = protected_app(&h.state);

    // 有效 cookie → 200
    let r = call(
        &app,
        get_req(
            "/api/devices",
            Some(&cookie_header(&[(SESSION_COOKIE, &session)])),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);

    // 缺 cookie → 401
    assert_eq!(
        call(&app, get_req("/api/devices", None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    // 伪造 / 过期签名 → 401（签名 cookie 不可篡改）
    assert_eq!(
        call(
            &app,
            get_req(
                "/api/devices",
                Some(&cookie_header(&[(SESSION_COOKIE, "forged.deadbeef")]))
            )
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    // 真实 app 上同样：没有 cookie 的 /api/* 是 401
    assert_eq!(h.get("/api/devices").await.status, StatusCode::UNAUTHORIZED);

    // 状态变更必须 CSRF 双重提交（§2.4.2）
    let no_csrf = Request::builder()
        .method("POST")
        .uri("/api/nodes")
        .header("cookie", cookie_header(&[(SESSION_COOKIE, &session)]))
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, no_csrf).await.status, StatusCode::FORBIDDEN);

    let wrong_csrf = Request::builder()
        .method("POST")
        .uri("/api/nodes")
        .header(
            "cookie",
            cookie_header(&[(SESSION_COOKIE, &session), (CSRF_COOKIE, &csrf)]),
        )
        .header("x-csrf-token", "not-the-cookie-value")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, wrong_csrf).await.status, StatusCode::FORBIDDEN);

    let ok_csrf = Request::builder()
        .method("POST")
        .uri("/api/nodes")
        .header(
            "cookie",
            cookie_header(&[(SESSION_COOKIE, &session), (CSRF_COOKIE, &csrf)]),
        )
        .header("x-csrf-token", &csrf)
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, ok_csrf).await.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// 标准⑦：cookie 是 httpOnly + SameSite=Strict
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_07_session_cookie_is_httponly_and_samesite_strict() {
    let h = Harness::new();
    h.initialized().await;

    let r = h.post_login("correct horse battery", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let raw = r
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let session_line = raw
        .lines()
        .find(|l| l.starts_with(&format!("{SESSION_COOKIE}=")))
        .expect("session cookie must be set");
    assert!(session_line.contains("HttpOnly"), "{session_line}");
    assert!(session_line.contains("SameSite=Strict"), "{session_line}");
    assert!(session_line.contains("Path=/"), "{session_line}");
    // 不进 localStorage 的前提：cookie 名固定且只经 Set-Cookie 下发
    assert!(!raw.contains("document"), "{raw}");

    // CSRF cookie 必须**不是** HttpOnly：前端要能读出来回填 X-CSRF-Token
    let csrf_line = raw
        .lines()
        .find(|l| l.starts_with(&format!("{CSRF_COOKIE}=")))
        .expect("csrf cookie must be set");
    assert!(!csrf_line.contains("HttpOnly"), "{csrf_line}");
    assert!(csrf_line.contains("SameSite=Strict"), "{csrf_line}");

    // 响应不得缓存
    assert_eq!(r.headers.get("cache-control").unwrap(), "no-store");
}

// ---------------------------------------------------------------------------
// 标准⑧：/ws 无 cookie 可达（初始化模式与正常模式各一次）
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_08_ws_is_exempt_in_both_modes() {
    let h = Harness::new();

    // 初始化模式：/ws 豁免，不会被 403 挡掉
    let r = h.get("/ws").await;
    assert_ne!(
        r.status,
        StatusCode::FORBIDDEN,
        "设备在初始化模式也必须能连上来：{}",
        r.body
    );
    assert_ne!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);

    let (session, _csrf) = h.initialized().await;

    // 正常模式：同样豁免（node 不是浏览器，没有 cookie）
    let r = h.get("/ws").await;
    assert_ne!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);
    // 豁免不等于「只有 /ws 豁免」：同一时刻别的 API 仍然要 cookie
    assert_eq!(h.get("/api/devices").await.status, StatusCode::UNAUTHORIZED);
    // 对照：带上 cookie 也一样通（WS 不看 cookie）
    let with_cookie = call(
        &h.app,
        get_req("/ws", Some(&cookie_header(&[(SESSION_COOKIE, &session)]))),
    )
    .await;
    assert_eq!(with_cookie.status, r.status);
}

// ---------------------------------------------------------------------------
// 标准⑨：改密码后旧 session 仍有效；轮换 session_hmac 才全部失效
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_09_changing_password_keeps_sessions_rotating_hmac_kills_them() {
    let h = Harness::new();
    let (session, csrf) = h.initialized().await;
    let app = protected_app(&h.state);
    let with_session = || {
        Request::builder()
            .uri("/api/devices")
            .header("cookie", cookie_header(&[(SESSION_COOKIE, &session)]))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(call(&app, with_session()).await.status, StatusCode::OK);

    // 改密码（M0 没有 /api/change-password 路由，走 hub-core 的 API：
    // 路由层是薄封装，安全性质在 hub-core + 中间件里，已分别被本测试覆盖）
    h.state
        .auth()
        .change_password("correct horse battery", "a whole new password")
        .unwrap();
    assert_eq!(call(&app, with_session()).await.status, StatusCode::OK);
    let _ = csrf;

    // 轮换 session HMAC 密钥 —— 这才是「注销全部会话」的手段（§2.4.2）
    h.state.auth().rotate_session_hmac().unwrap();
    assert_eq!(
        call(&app, with_session()).await.status,
        StatusCode::UNAUTHORIZED
    );
    // 重新登录拿到的新 cookie 可用（注意密码已经改成新的了）
    let r = h.post_login("a whole new password", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let session2 = r.cookie_value(SESSION_COOKIE).expect("session cookie");
    assert_ne!(session2, session);
    assert_eq!(
        call(
            &app,
            get_req(
                "/api/devices",
                Some(&cookie_header(&[(SESSION_COOKIE, &session2)]))
            )
        )
        .await
        .status,
        StatusCode::OK
    );
}

// ---------------------------------------------------------------------------
// 标准⑩：Argon2id 校验不阻塞 tokio worker
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "current_thread")]
async fn standard_10_argon2_verification_does_not_block_the_tokio_worker() {
    // 单线程 runtime：**任何在 worker 上的同步阻塞都会让整个运行时停摆**，
    // 这一点让「有没有走 spawn_blocking」变成可观测的事实，而不是拍脑袋。
    let h = Harness::with(
        AuthParams {
            m_cost: 65536,
            t_cost: 2,
            p_cost: 1,
        },
        Duration::from_secs(60),
        LoginRateLimiter::new(1000, 60),
    );
    h.set_password().await;

    // 造一台设备当心跳落库的载体（心跳写库是 M0 里唯一的高频落库路径）
    let devices = DeviceRepo::new(h.pool.clone());
    {
        let conn = devices.pool().get().unwrap();
        conn.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES ('n_hb', 'hb', 'fp_hb', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }

    // 心跳任务：每 5ms 落一次库，统计「登录还在跑的时候」落了几次
    let logins_done = Arc::new(AtomicBool::new(false));
    let beats = Arc::new(AtomicUsize::new(0));
    let hb = {
        let logins_done = Arc::clone(&logins_done);
        let beats = Arc::clone(&beats);
        let pool = h.pool.clone();
        tokio::spawn(async move {
            let devices = DeviceRepo::new(pool);
            while !logins_done.load(Ordering::SeqCst) {
                devices
                    .touch_online("n_hb", "{}", "{}", "2026-01-01T00:00:00Z")
                    .unwrap();
                beats.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    // 让心跳先跑起来（至少落一次），后面统计的才是「登录期间」的增量
    while beats.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let beats_before = beats.load(Ordering::SeqCst);

    // 20 次并发登录，每次都真的跑一遍 Argon2id 校验
    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..20 {
        let app = h.app.clone();
        let ip = format!("10.0.1.{}", i % 250);
        tasks.push(tokio::spawn(async move {
            let mut req = Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"password": "correct horse battery"}).to_string(),
                ))
                .unwrap();
            let addr: std::net::SocketAddr = format!("{ip}:5555").parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
            app.oneshot(req).await.unwrap().status()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    logins_done.store(true, Ordering::SeqCst);
    hb.await.unwrap();
    let during = beats.load(Ordering::SeqCst) - beats_before;

    assert!(
        during > 0,
        "20 次登录期间心跳一次都没落库：Argon2id 校验很可能还在 tokio worker 上阻塞 \
         （登录耗时 {}ms，期间落库 {} 次）",
        started.elapsed().as_millis(),
        during
    );
}

// ---------------------------------------------------------------------------
// 标准⑪：device 表为空时也能完成 /setup + /login（首次部署死锁回归）
// ---------------------------------------------------------------------------
#[tokio::test]
async fn standard_11_first_deploy_with_empty_device_table_can_setup_and_login() {
    let h = Harness::new();

    // 前提：一台设备都没注册
    assert!(
        DeviceRepo::new(h.pool.clone()).list().unwrap().is_empty(),
        "首次部署时 device 表必然为空"
    );

    let (session, _csrf) = h.initialized().await;

    // session HMAC 密钥是**随机生成并落库**的，不依赖任何设备指纹
    let stored = hub_storage::repo::SecretRepo::new(h.pool.clone())
        .get("hub", "session_hmac")
        .unwrap()
        .expect("session_hmac must be persisted at setup time");
    let decoded = identity::base64url_decode(&stored).unwrap();
    assert_eq!(decoded.len(), 32, "session 密钥必须是 32 字节随机值");

    // 拿到的 cookie 立刻可用（登录闭环不依赖设备表有任何行）
    let r = call(
        &protected_app(&h.state),
        get_req(
            "/api/devices",
            Some(&cookie_header(&[(SESSION_COOKIE, &session)])),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// 其余分支：429 排队、400 请求体、健康探针在两种模式下都豁免
// ---------------------------------------------------------------------------

/// `/login` 的全局信号量排队超时 → 429（§2.4.1 末条）。
///
/// 用**真实 Argon2id 耗时**把 2 个许可占满，第三个必然排队超时——
/// 这比「把超时设成 0」更接近真实场景（攻击者反复触发未认证端点）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_queue_timeout_returns_429() {
    let h = Harness::with(
        AuthParams {
            m_cost: 65536,
            t_cost: 3,
            p_cost: 1,
        },
        Duration::from_millis(30),
        LoginRateLimiter::new(1000, 60),
    );
    h.set_password().await;

    let mut tasks = Vec::new();
    for i in 0..8 {
        let app = h.app.clone();
        let ip = format!("10.0.2.{}", i);
        tasks.push(tokio::spawn(async move {
            let mut req = Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"password": "correct horse battery"}).to_string(),
                ))
                .unwrap();
            let addr: std::net::SocketAddr = format!("{ip}:5555").parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
            Reply::new(app.oneshot(req).await.unwrap()).await
        }));
    }
    let mut busy = 0;
    let mut ok = 0;
    for t in tasks {
        let r = t.await.unwrap();
        match r.status {
            StatusCode::OK => ok += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                busy += 1;
                assert_eq!(r.json()["error"], json!("login_busy"));
            }
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(ok + busy, 8);
    assert!(busy > 0, "并发登录必须因排队超时被拒，否则信号量没生效");
}

/// 请求体不是合法 JSON → 400，且**不**计入限流。
#[tokio::test]
async fn login_rejects_malformed_body_without_counting_as_failure() {
    let h = Harness::new();
    h.set_password().await;

    let req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/json")
        .body(Body::from("not json at all"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], json!("invalid_request_body"));

    let empty = Request::builder()
        .method("POST")
        .uri("/login")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(empty).await.status, StatusCode::BAD_REQUEST);

    // 真正的密码依然能登录 → 说明上面两次没被算成爆破失败
    assert_eq!(
        h.post_login("correct horse battery", None).await.status,
        StatusCode::OK
    );
}

/// 密码太短 → 400，且不初始化 Hub。
#[tokio::test]
async fn setup_rejects_short_password() {
    let h = Harness::new();
    let token = h.setup_token();
    let r = h.post_setup(Some(&token), "short").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], json!("password_too_short"));
    assert!(!h.state.is_initialized().unwrap());
    // token 没被作废：用户可以用一个合格的密码重试
    let r = h.post_setup(Some(&token), "correct horse battery").await;
    assert_eq!(r.status, StatusCode::OK);
}

/// 正常模式下重启 Hub：没有新 token、残留文件被清掉（§2.4.1）。
#[tokio::test]
async fn restart_in_normal_mode_does_not_issue_a_new_setup_token() {
    let h = Harness::new();
    h.initialized().await;
    let pool = hub_storage::db::open_in(h.dir.path()).unwrap().1;

    // 人工放一份残留 token 文件，模拟上次初始化中途断电
    let stale = h.dir.path().join("setup-token.txt");
    std::fs::write(&stale, "left-over-token").unwrap();

    let state2 = AppState::bootstrap_with(
        pool,
        h.dir.path(),
        fast_params(),
        QUEUE_TIMEOUT,
        LoginRateLimiter::new(10, 60),
    )
    .unwrap();
    assert!(state2.is_initialized().unwrap());
    assert!(!stale.exists(), "正常模式重启必须清掉残留 token 文件");
    assert!(state2.check_setup_token("left-over-token").is_err());
    // 正常模式下探针一律 409
    let app = hub_server::build_app(state2);
    let r = call(&app, get_req("/api/setup", None)).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

/// 初始化模式与正常模式下 `/healthz` 都豁免（表 §2.4.3 第四行）。
#[tokio::test]
async fn healthz_is_exempt_in_both_modes() {
    let h = Harness::new();
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
    h.initialized().await;
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
}

/// 造一副「secret 表被删」的库，用来驱动 `is_initialized` / `session_key`
/// 的存储错误分支（middleware_auth.rs 的两条 `tracing::error!` → 500 路径）。
///
/// 顺序很重要：先用**完整**的库跑完 `bootstrap`（它要读 secret 表），
/// 再把表删掉。之后所有走存储的查询都会失败。
fn state_with_broken_secret_table() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
    let state = AppState::bootstrap_with(
        pool.clone(),
        dir.path(),
        fast_params(),
        QUEUE_TIMEOUT,
        LoginRateLimiter::new(10, 60),
    )
    .unwrap();
    pool.get()
        .unwrap()
        .execute("DROP TABLE secret", [])
        .unwrap();
    (dir, state)
}

#[tokio::test]
async fn storage_error_in_init_check_returns_500_without_leaking() {
    let (_d, state) = state_with_broken_secret_table();
    let probe = protected_app(&state);
    // 非豁免路径要先过 is_initialized（会失败）→ 500，不是 403/401
    let r = call(&probe, get_req("/api/devices", None)).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"], json!("internal_error"));
    assert!(!r.body.contains("/"));

    // 带 cookie 的路径同样先卡在 is_initialized（此时已 500，轮不到验签）
    let cookie = cookie_header(&[(SESSION_COOKIE, "whatever")]);
    let r = call(&probe, get_req("/api/devices", Some(&cookie))).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!r.body.contains("/"));

    // 真正的 app 上 /login 也要 500：中间件先判初始化状态，读库失败
    let app = hub_server::build_app(state);
    let req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({"password": "x"}).to_string()))
        .unwrap();
    let r = Reply::new(app.oneshot(req).await.unwrap()).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"], json!("internal_error"));
}

/// `setup_password` 真正失败（非法 Argon2id 参数 → HashFailed）→ 500，
/// 且密码**没有**落库（安全性质不能因为哈希失败就放行）。
///
/// 故意给一份会让 `argon2()` 构造失败的参数（m_cost 低于下限），
/// 走 `post_setup` 里 `Err(e) => 500` 那条错误处理分支。
#[tokio::test]
async fn setup_with_unhashable_params_fails_without_storing_password() {
    let dir = tempfile::tempdir().unwrap();
    let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
    let state = AppState::bootstrap_with(
        pool.clone(),
        dir.path(),
        AuthParams {
            m_cost: 0, // 低于 Argon2id 下限 → Params::new 失败 → HashFailed
            t_cost: 1,
            p_cost: 1,
        },
        QUEUE_TIMEOUT,
        LoginRateLimiter::new(10, 60),
    )
    .unwrap();
    let app = hub_server::build_app(state.clone());

    let token_path = dir.path().join("setup-token.txt");
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/setup")
        .header("content-type", "application/json")
        .header("x-setup-token", token)
        .body(Body::from(
            json!({"username": "me", "password": "correct horse battery"}).to_string(),
        ))
        .unwrap();
    let r = Reply::new(app.oneshot(req).await.unwrap()).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    // 哈希失败：密码绝不能入库，Hub 仍然未初始化
    assert!(!state.is_initialized().unwrap());
    assert!(hub_storage::repo::SecretRepo::new(pool)
        .get("hub", "password")
        .unwrap()
        .is_none());
    // token 也没被作废（设置没成功）
    assert!(token_path.exists());
}

/// `GET /api/setup` 在存储层出错时返回 500（驱动 get_setup 里的错误分支）。
#[tokio::test]
async fn get_setup_returns_500_on_storage_error() {
    let (_d, state) = state_with_broken_secret_table();
    let app = hub_server::build_app(state);
    let r = Reply::new(app.oneshot(get_req("/api/setup", None)).await.unwrap()).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"], json!("internal_error"));
}

/// session HMAC 密钥损坏（长度不对 → MalformedHmacKey）→ 中间件验签阶段
/// 直接 500，不把坏密钥漏出去。这条路径覆盖 `require_auth` 里
/// `session_key()` 失败的错误分支（带 cookie 但密钥读不出来）。
#[tokio::test]
async fn corrupted_session_hmac_key_rejects_with_500() {
    let h = Harness::new();
    h.initialized().await;
    // 手动把 session_hmac 写成 16 字节（非法的 32 字节）→ 下次读取必报 Malformed
    hub_storage::repo::SecretRepo::new(h.pool.clone())
        .set(
            "hub",
            "session_hmac",
            &hub_storage::base64url_of(&[9u8; 16]),
            "unix:1",
        )
        .unwrap();
    // 带任意 cookie → 中间件先过 session_key（失败）→ 500
    let r = call(
        &protected_app(&h.state),
        get_req(
            "/api/devices",
            Some(&cookie_header(&[(SESSION_COOKIE, "anything")])),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"], json!("internal_error"));
}

/// 登出把两条 cookie 都清成「立即过期」。
///
/// session 是签名票据、服务端没有会话表可删，所以**清 cookie 是无状态设计
/// 下唯一的注销手段**。这条用例锁的是「两条都清、且是立即过期」这个对外
/// 契约——只断言「有 Set-Cookie」会把「登出成功但票据还活着」放过去。
#[tokio::test]
async fn logout_expires_both_cookies() {
    let h = Harness::new();
    let (session, csrf) = h.initialized().await;
    let app = hub_server::build_app(h.state.clone());

    let before = call(
        &app,
        Request::builder()
            .method("GET")
            .uri("/api/devices")
            .header(
                "cookie",
                cookie_header(&[(SESSION_COOKIE, &session), (CSRF_COOKIE, &csrf)]),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "登出前应当是已认证");

    let out = call(
        &app,
        Request::builder()
            .method("POST")
            .uri("/logout")
            .header(
                "cookie",
                cookie_header(&[(SESSION_COOKIE, &session), (CSRF_COOKIE, &csrf)]),
            )
            .header("x-csrf-token", &csrf)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(out.status, StatusCode::OK);

    // 判据不是「回了两个 Set-Cookie」，而是**过期**：Max-Age=0 / 早于 epoch。
    // 只断言有 Set-Cookie 会把「登出成功但票据还活着」放过去——那正是本条的 bug。
    let set_cookies: Vec<&str> = out
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    for name in [SESSION_COOKIE, CSRF_COOKIE] {
        let hit = set_cookies
            .iter()
            .find(|c| c.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} 没有被清掉：{set_cookies:?}"));
        assert!(hit.contains("Max-Age=0"), "{name} 不是立即过期：{hit}");
    }

    // **这里不写「登出后同一组 cookie 变 401」**：票据是自证的签名串，
    // 服务端没有会话表可删，把同一串手工塞回 `cookie` 头照样验得过——
    // 清 cookie 只作用于**照规矩更新 cookie jar 的浏览器**。判据因此是
    // 「两条 cookie 都被清成立即过期」，而不是「服务端把它拉黑了」。
}

/// 登出是状态变更：没带 CSRF 头就 403。
///
/// 放进 `CSRF_EXEMPT_PATHS` 会让别的站替用户签出——那正是双重提交要挡的。
#[tokio::test]
async fn logout_without_a_csrf_token_is_forbidden() {
    let h = Harness::new();
    let (session, csrf) = h.initialized().await;
    let app = hub_server::build_app(h.state.clone());

    let r = call(
        &app,
        Request::builder()
            .method("POST")
            .uri("/logout")
            .header(
                "cookie",
                cookie_header(&[(SESSION_COOKIE, &session), (CSRF_COOKIE, &csrf)]),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

/// 没登录就登出是 401：登出端点**不**在 `AUTH_EXEMPT_PATHS` 里。
#[tokio::test]
async fn logout_without_a_session_is_unauthorized() {
    let h = Harness::new();
    let (_, _) = h.initialized().await;
    let app = hub_server::build_app(h.state.clone());

    let r = call(
        &app,
        Request::builder()
            .method("POST")
            .uri("/logout")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

/// `AppState::auth()` 暴露的就是 hub-core 的 `HubAuth`，没有第二份状态。
#[test]
fn state_auth_is_the_single_source_of_truth() {
    let h = Harness::new();
    let other = HubAuth::new(h.pool.clone(), fast_params());
    assert!(!other.is_initialized().unwrap());
    h.state
        .auth()
        .setup_password("correct horse battery")
        .unwrap();
    // 两个独立实例看到同一个库 → 状态只有一份
    assert!(other.is_initialized().unwrap());
    assert_eq!(
        other.session_hmac_key().unwrap(),
        h.state.session_key().unwrap()
    );
}
