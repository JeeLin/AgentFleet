//! 路由集合与豁免表装配点。
//!
//! **豁免表**（`security-and-lifecycle.md` §2.4.3）在这里集中声明：
//! 哪些路径**不要求 session**。M0 放行 `/healthz`、`/setup`、`/login`、`/ws`。
//!
//! 单独成文件而不是堆在 `lib.rs`：豁免表是安全边界，必须一眼看全，
//! 散落在各路由文件里迟早会漏。
//!
//! 两张表不是一回事，别合并：
//! - [`AUTH_EXEMPT_PATHS`]：**不要求 session**。含 `/login`——登录前当然
//!   没有 session；也含 `/ws`——**node 不是浏览器、没有 cookie**，
//!   它走 §3 的 challenge/签名认证。没有这一行，「除 `/setup` 外全部要求
//!   session」会把**所有设备挡在门外**。
//! - [`INIT_MODE_PATHS`]：**未设密码时仍然可用**的路径。
//!   `/login` 不在其中：初始化模式下登录必须 403（表 §2.4.3 第二行），
//!   因为此时还没有任何密码可验。

/// 免 session 的路径（精确匹配）。
///
/// 这些是「不需要登录态也能访问」的入口：健康探针、初始化流程本身、
/// 登录端点与设备 WebSocket。**不要**把任何返回设备数据或密钥的接口加进来。
pub const AUTH_EXEMPT_PATHS: &[&str] = &[
    "/healthz", // 子任务 9（Hub 登录）：初始化流程与登录端点
    "/setup", "/login",
    // 子任务 3（设备 WS）：**设备不是浏览器，没有 session cookie**。
    // 它走 §3 的 challenge/签名认证，所以 `/ws` 在**初始化模式与正常模式下
    // 都豁免 session**——否则没有任何设备能连上 Hub。
    "/ws",
];

/// 初始化模式（尚未设密码）下仍可访问的路径（精确匹配）。
///
/// 其余一切（含 `/login`）在初始化模式下一律 **403**，不是 401：
/// 401 暗示「你有身份但没登录」，而此时 Hub 根本还不存在用户。
pub const INIT_MODE_PATHS: &[&str] = &["/healthz", "/setup", "/ws"];

/// 该路径是否豁免 session。
pub fn is_auth_exempt(path: &str) -> bool {
    AUTH_EXEMPT_PATHS.contains(&path)
}

/// 该路径在**初始化模式**下是否可用。
pub fn is_init_mode_path(path: &str) -> bool {
    INIT_MODE_PATHS.contains(&path)
}

pub mod setup;
pub mod ws;
