//! session 认证中间件：豁免表之外一律要 cookie（§2.4 / §2.4.3）。
//!
//! 三条规则，顺序不可换：
//! 1. **初始化模式**（`secret` 表无 `hub.password`）：只有
//!    [`is_init_mode_path`] 表内的路径能过，其余 **403**——
//!    不是 401：此时 Hub 还不存在「用户」，401 会误导成「你没登录」。
//! 2. **正常模式**下的 session 豁免：豁免表内的路径直接过，**但它们照样过 CSRF**
//!    （豁免的是 session，不是 CSRF）。
//! 3. **正常模式**：其余要求 `af_session` cookie，验签失败或缺失一律 **401**；
//!    通过之后，`POST/PUT/PATCH/DELETE` 还要过 CSRF 双重提交——`X-CSRF-Token`
//!    头必须与 `af_csrf` cookie 同值。
//!
//! **session 先于 CSRF**：反过来会让「没带 cookie 的 POST」拿到 403 而不是 401，
//! 而前端的 401 才是「去登录」的信号。
//!
//! CSRF 的唯一白名单是 [`is_csrf_exempt`] 里的 pre-auth 端点：`/setup` 与
//! `/login` 在请求时还没有 `af_csrf` 可回填，防它们靠的是 setup token 与
//! 密码本身（`SameSite=Strict` 也拦不住「同源 XSS 发起的请求」）。
//! **新增豁免路径默认要 CSRF**——fail-closed。

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hub_core::session::SessionStore;

use crate::routes::{api, is_auth_exempt, is_csrf_exempt, is_init_mode_path};
use crate::{err, ApiError, AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};

/// 会改变服务端状态的 HTTP 方法（CSRF 双重提交适用范围）。
pub const STATE_CHANGING_METHODS: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];

/// session 认证中间件。
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();

    // 规则 1：初始化模式
    if !is_init_mode_path(&path) {
        match state.is_initialized() {
            Ok(true) => {}
            Ok(false) => return ApiError::forbidden(err::HUB_NOT_INITIALIZED).into_response(),
            Err(e) => {
                // 不把 AuthError 的内容回显给客户端：它是 storage 层的错误串，
                // 万一将来带上路径/连接串就会从响应里漏出去。
                tracing::error!(path = %path, error = %e, "cannot read hub init state");
                return ApiError::internal().into_response();
            }
        }
    }

    // 规则 2：session 豁免的路径（`/login`、`/setup`、`/ws`…）。
    //
    // **豁免的是 session，不是 CSRF**：这条分支以前直接 `return next`，于是
    // 豁免表里的写操作整段跳过了下面那道 CSRF 检查。现在它自己补上，
    // 白名单只有真正拿不到 `af_csrf` 的 pre-auth 端点。
    if is_auth_exempt(&path) {
        if is_state_changing(req.method().as_str())
            && !is_csrf_exempt(&path)
            && !csrf_ok(req.headers())
        {
            return ApiError::forbidden("csrf_token_missing_or_mismatch").into_response();
        }
        return next.run(req).await;
    }

    // 规则 3：正常模式下的 session 要求。
    //
    // **session 在 CSRF 之前**：顺序反过来会让「没带 cookie 的 POST」拿到 403
    // 而不是 401，而前端的 401 才是「去登录」的信号——把未认证读成 CSRF 错，
    // 用户会看到「CSRF 令牌不匹配」而不是登录框。
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
    let session = match SessionStore::new(key).verify(&token) {
        Ok(s) => s,
        Err(_) => return ApiError::unauthorized("invalid_session").into_response(),
    };

    // 规则 4：已登录的状态变更还要过 CSRF 双重提交。
    if is_state_changing(req.method().as_str()) && !csrf_ok(req.headers()) {
        return ApiError::forbidden("csrf_token_missing_or_mismatch").into_response();
    }

    // 身份在**验签之后**才插进请求扩展：handler 拿到 `Authenticated` 就意味着
    // 「Hub 已经能证明这个 cookie 是自己签的」，不必在每个 handler 里再验一遍。
    // 顺序写在代码里而不是文档里：插早了会让「取密钥失败 → 500」的那条路上
    // 带着一个身份继续往下走。
    req.extensions_mut()
        .insert(api::Authenticated::new(session.subject));
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
