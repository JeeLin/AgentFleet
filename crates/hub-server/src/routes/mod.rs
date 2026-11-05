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

/// 项目名；项目不存在时退回 id（项目只归档不删除，正常情况不会走到）。
///
/// **定义在这里而不是每个路由模块各写一份**：设备详情与 agent 详情都要把
/// `binding.project_id` 翻成一个人能读的名字，而这两页是同一件事的两个视角
/// ——同一批绑定行，一个按设备看、一个按 agent 看。各写一份的话，改口径
/// （比如将来要在名字后面补上归档标记）必然漏一处，于是同一个项目在两个
/// 页面上显示成两个名字。
///
/// 退回 **id 而不是空串**：空串在页面上是一个没有标题的分组，用户点不进去
/// 也不知道那是谁；id 至少能让人对上是哪个项目。
pub fn project_name(state: &crate::AppState, project_id: &str) -> String {
    match hub_storage::project::ProjectRepo::new(state.pool().clone()).get(project_id) {
        Ok(Some(p)) => p.name,
        // coverage: excluded — 项目只归档不删除，所以 `Ok(None)` 正常走不到；
        // `Err` 是读库失败。两条都退回 id，让页面至少有一个能对上的标签。
        Ok(None) | Err(_) => project_id.to_string(),
    }
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

/// 读请求体（上限 1 MiB，与 WS 帧上限同值）。**`/api/*` 的唯一一份实现**。
///
/// 放在这里而不是各路由模块里：读不出来 / 超上限的错误码只有一处定义
/// （[`crate::ApiError::bad_request_body`]）。两个模块各写一份时，改了错误码
/// 另一份不会报错——而前端是**按 code 分支**的，那一侧会静默落到默认分支，
/// 页面只显示一句「建会话没有成功」而不说清哪里不对。
///
/// 读出错与超上限都归成 400：「你发来的东西我没法处理」。**错误串不进响应体**
/// （约束 13），所以这两种情况在页面看来是同一件事——它们确实是。
pub(crate) async fn read_body(req: axum::extract::Request) -> Result<Vec<u8>, crate::ApiError> {
    axum::body::to_bytes(req.into_body(), crate::WS_MAX_MESSAGE_BYTES)
        .await
        .map(|b| b.to_vec())
        .map_err(|_| crate::ApiError::bad_request_body())
}

#[cfg(test)]
mod tests {
    use super::masked_fingerprint;

    /// 指纹太短时整串打码：不足 6 个字符的串不可能是真的 Ed25519 指纹。
    #[test]
    fn a_short_fingerprint_is_masked_whole() {
        assert_eq!(masked_fingerprint("abc"), "***");
        assert_eq!(masked_fingerprint("abcdef"), "abc***def");
    }
}

pub mod agent_binary;
pub mod agents;
pub mod agents_detail;
pub mod api;
pub mod assets;
pub mod base_rev;
pub mod bindings;
pub mod conversation;
pub mod devices;
pub mod projects;
pub mod setup;
pub mod stream;
pub mod ws;
