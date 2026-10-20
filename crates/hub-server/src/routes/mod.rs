//! 路由集合与豁免表装配点。
//!
//! **豁免表**（`security-and-lifecycle.md` §2.4.3）在这里集中声明：
//! 哪些路径**不要求 session**。M0 放行 `/healthz`、`/setup`、`/api/setup`、
//! `/login`、`/ws`。
//!
//! 单独成文件而不是堆在 `lib.rs`：豁免表是安全边界，必须一眼看全，
//! 散落在各路由文件里迟早会漏。
//!
//! 两张表不是一回事，别合并：
//! - [`AUTH_EXEMPT_PATHS`]：**不要求 session**。含 `/login`——登录前当然
//!   没有 session；也含 `/ws`——**node 不是浏览器、没有 cookie**，
//!   它走 §3 的 challenge/签名认证。没有这一行，「除 `/setup` 与
//!   `/api/setup` 外全部要求 session」会把**所有设备挡在门外**。
//! - [`INIT_MODE_PATHS`]：**未设密码时仍然可用**的路径。
//!   `/login` 不在其中：初始化模式下登录必须 403（表 §2.4.3 第二行），
//!   因为此时还没有任何密码可验。
//!
//! 第三张表 [`CSRF_EXEMPT_PATHS`] 管的是**另一个横切检查**，与上面两张
//! 正交：会话豁免的路径照样要过 CSRF（见 `middleware_auth`）。
//!
//! `/setup` 与 `/api/setup` 是**同一件事的两半**，两张表里都必须有：
//! `/setup` 是页面（SPA 外壳）与初始化端点（`POST`），`/api/setup` 是
//! 初始化状态探针（`GET`，`lib.rs` 里说明了为什么拆）。少了 `/setup`，
//! 首次部署时 setup 页自己打不开；少了 `/api/setup`，页面问不到状态。

/// 免 session 的路径（精确匹配）。
///
/// 这些是「不需要登录态也能访问」的入口：健康探针、初始化流程本身、
/// 登录端点与设备 WebSocket。**不要**把任何返回设备数据或密钥的接口加进来。
pub const AUTH_EXEMPT_PATHS: &[&str] = &[
    "/healthz", // 子任务 9（Hub 登录）：初始化流程与登录端点
    "/setup",
    "/api/setup",
    "/login",
    // 子任务 3（设备 WS）：**设备不是浏览器，没有 session cookie**。
    // 它走 §3 的 challenge/签名认证，所以 `/ws` 在**初始化模式与正常模式下
    // 都豁免 session**——否则没有任何设备能连上 Hub。
    "/ws",
];

/// 初始化模式（尚未设密码）下仍可访问的路径（精确匹配）。
///
/// 其余一切（含 `/login`）在初始化模式下一律 **403**，不是 401：
/// 401 暗示「你有身份但没登录」，而此时 Hub 根本还不存在用户。
pub const INIT_MODE_PATHS: &[&str] = &["/healthz", "/setup", "/api/setup", "/ws"];

/// 免 CSRF 的路径（精确匹配）。**只有签发 cookie 的 pre-auth 端点**。
///
/// 它们是唯一在请求时还不存在 `af_csrf` 可供回填的路径；
/// 这张表之外的一切状态变更都要双重提交，**包括** [`AUTH_EXEMPT_PATHS`]
/// 里的路径——两张表不是一回事，免 session 不等于免 CSRF。
pub const CSRF_EXEMPT_PATHS: &[&str] = &["/setup", "/login"];

/// 该路径是否豁免 session。
pub fn is_auth_exempt(path: &str) -> bool {
    AUTH_EXEMPT_PATHS.contains(&path)
}

/// 该路径是否豁免 CSRF 双重提交。
pub fn is_csrf_exempt(path: &str) -> bool {
    CSRF_EXEMPT_PATHS.contains(&path)
}

/// 该路径在**初始化模式**下是否可用。
pub fn is_init_mode_path(path: &str) -> bool {
    INIT_MODE_PATHS.contains(&path)
}

/// 设备指纹的显示形式：**前 3 + `***` + 后 3**（约束 13）。
///
/// 太短的值（不足 6 个字符）整串打码：那种串不可能是真的 Ed25519 指纹
/// （hex 64 字符），而露出它的全部反而没有任何价值。页面拿它做「这台设备
/// 我已经认过了」的视觉锚点，不是拿它做身份判据。
pub fn masked_fingerprint(fp: &str) -> String {
    let chars: Vec<char> = fp.chars().collect();
    if chars.len() < 6 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(chars.len() - 3).collect();
    format!("{head}***{tail}")
}

/// 把 `Last-Event-ID` 头取成序号（SSE 断线重连语义，子任务 5a）。
///
/// 定义在这里而不是挂在某个路由模块上：M1 没有 agent 会话，「订阅哪条会话」
/// 与「从第几个事件续收」是同一个问题；把「解析头」和「挂了哪些端点」写在
/// 一处，后来加 `GET /api/sessions/:id/stream` 时就不用复制它。
pub fn last_event_id(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

pub mod api;
pub mod assets;
pub mod setup;
pub mod stream;
pub mod ws;
