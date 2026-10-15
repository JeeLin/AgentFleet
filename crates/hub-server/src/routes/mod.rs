//! 路由集合与豁免表装配点。
//!
//! **豁免表**（`security-and-lifecycle.md` §2.4.3）在这里集中声明：
//! 哪些路径**不要求 session**。M0 先只放行 `/healthz`；
//! `/setup` `/login` 属子任务 9，`/ws` 属子任务 3 —— 各自落地时在此登记。
//!
//! 单独成文件而不是堆在 `lib.rs`：豁免表是安全边界，必须一眼看全，
//! 散落在各路由文件里迟早会漏。

/// 免 session 的路径（精确匹配）。
///
/// 这些是「不需要登录态也能访问」的入口：健康探针与初始化流程本身。
/// **不要**把任何返回设备数据或密钥的接口加进来。
pub const AUTH_EXEMPT_PATHS: &[&str] = &[
    "/healthz",
    // 子任务 3 落地时在此追加 "/ws"（设备走 challenge/签名认证，不是浏览器 session）
    // 子任务 9 落地时在此追加 "/setup" 与 "/login"
];

/// 该路径是否豁免 session。
pub fn is_auth_exempt(path: &str) -> bool {
    AUTH_EXEMPT_PATHS.contains(&path)
}
