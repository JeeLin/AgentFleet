//! session 认证中间件：豁免表之外一律要 cookie（§2.4 / §2.4.3）。
//!
//! 三条规则，顺序不可换：
//! 1. **初始化模式**（`secret` 表无 `hub.password`）：只有
//!    [`is_init_mode_path`] 表内的路径能过，其余 **403**——
//!    不是 401：此时 Hub 还不存在「用户」，401 会误导成「你没登录」。
//! 2. **正常模式**：豁免表内的路径（`/login`）直接过；
//!    其余要求 `af_session` cookie，验签失败或缺失一律 **401**。
//! 3. **状态变更**（`POST/PUT/PATCH/DELETE`）且已登录：还要过 CSRF 双重提交
//!    ——`X-CSRF-Token` 头必须与 `af_csrf` cookie 同值。
//!
//! CSRF 只对**带 session 的**状态变更生效：`/setup` 与 `/login` 是 pre-auth
//! 端点，没有可被「骑乘」的登录态，防它们靠的是 setup token 与密码本身
//! （`SameSite=Strict` 也拦不住「同源 XSS 发起的请求」）。

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hub_core::session::SessionStore;

use crate::routes::{is_auth_exempt, is_init_mode_path};
use crate::{ApiError, AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};

/// 会改变服务端状态的 HTTP 方法（CSRF 双重提交适用范围）。
pub const STATE_CHANGING_METHODS: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];

/// session 认证中间件。
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();

    // 规则 1：初始化模式
    if !is_init_mode_path(&path) {
        match state.is_initialized() {
            Ok(true) => {}
            Ok(false) => return ApiError::forbidden("hub_not_initialized").into_response(),
            Err(e) => {
                // 不把 AuthError 的内容回显给客户端：它是 storage 层的错误串，
                // 万一将来带上路径/连接串就会从响应里漏出去。
                tracing::error!(path = %path, error = %e, "cannot read hub init state");
                return ApiError::internal().into_response();
            }
        }
    }

    // 规则 2：正常模式下的 session 要求（豁免表里的路径不受此约束）
    if is_auth_exempt(&path) {
        return next.run(req).await;
    }

    let token = match cookie_value(req.headers(), SESSION_COOKIE) {
        Some(t) => t.to_string(),
        None => return ApiError::unauthorized("missing_session").into_response(),
    };
    let key = match state.session_key() {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(path = %path, error = %e, "cannot load session hmac key");
            return ApiError::internal().into_response();
        }
    };
    if SessionStore::new(key).verify(&token).is_err() {
        return ApiError::unauthorized("invalid_session").into_response();
    }

    // 规则 3：状态变更要 CSRF 双重提交
    if is_state_changing(req.method().as_str()) && !csrf_ok(req.headers()) {
        return ApiError::forbidden("csrf_token_missing_or_mismatch").into_response();
    }

    next.run(req).await
}

/// 方法是否是状态变更方法。
pub fn is_state_changing(method: &str) -> bool {
    STATE_CHANGING_METHODS.contains(&method)
}

/// CSRF 双重提交：`X-CSRF-Token` 头与 `af_csrf` cookie 同值且非空。
///
/// 两侧都由本进程随机生成（见 `routes::setup::post_login`），
/// 所以「头与 cookie 相等」等价于「请求确实带着本浏览器自己的 cookie」。
pub fn csrf_ok(headers: &HeaderMap) -> bool {
    match (
        header_str(headers, CSRF_HEADER),
        cookie_value(headers, CSRF_COOKIE),
    ) {
        (Some(a), Some(b)) => !a.is_empty() && a == b,
        _ => false,
    }
}

/// 取出一个 cookie 的值。找不到返回 `None`。
///
/// 手工解析而不是引 cookie 库：本模块只需要 `name=value` 这一种形状，
/// 而 session / CSRF 的值都是 base64url 或十六进制，不含需要百分号解码的字符。
pub fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    for raw in headers.get_all(axum::http::header::COOKIE).iter() {
        // 非 ASCII 的 header value（obs-text）不是合法 cookie，整条跳过
        let Ok(text) = raw.to_str() else { continue };
        for pair in text.split(';') {
            if let Some((k, v)) = pair.trim().split_once('=') {
                if k == name {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 取出一个 header 的值（非 ASCII 时返回 `None`）。
pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}
