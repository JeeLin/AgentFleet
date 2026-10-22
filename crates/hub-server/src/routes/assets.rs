//! 控制台静态资产（子任务 6）：让单进程即可打开控制台，`file://` 不再是唯一方式。
//!
//! ## 单二进制：`include_dir!` 是唯一来源
//!
//! `DEVELOPMENT.md` §2.1 的 CI 蓝图写死「Rust 侧 include_dir! 编译期嵌入，
//! 运行时不需要 dist 目录」。所以这里**不用 `ServeDir`**：那会让「二进制旁边
//! 得有 packages/web/dist」成为一条只写在部署文档里的隐含前提，而自托管场景
//! 里最常漏的就是这一步。`build.rs` 把目录树位置写进 `AF_WEB_DIST`（产物缺失
//! 时指向占位页），本模块用 `include_dir!` 把它编进来。
//!
//! 里程碑的静态资产小节仍写着 `tower-http` 的 `ServeDir`——**那段规格文本
//! 尚未与实现同步**，按实现为准。
//!
//! ## 为什么仍然 `use tower_http`
//!
//! `tower-http` 原本是「引了不用」的死依赖（`docs/BUGS.md`）。它的
//! [`SetResponseHeaderLayer`] 在这里有真实用途：`/assets/*` 的文件名带内容哈希，
//! 可以缓存一年；而 `index.html` 必须每次回源（否则一次前端发版会被浏览器
//! 缓存成永久旧壳）。用一个中间件层表达「只给 /assets 加长缓存」，比在每个
//! handler 里手写 header 更不容易漏。
//!
//! ## 认证边界
//!
//! 静态路由**不**进 [`crate::routes::AUTH_EXEMPT_PATHS`]，而是注册在
//! `.layer(require_auth)` **之后**——那正是 axum 的语义：`.layer()` 只覆盖
//! 它之前添加的路由。这样做的两个后果都是要的：
//!
//! - `/`、`/login`、`/assets/*` 免 session（登录页自己就得能加载 JS/CSS）；
//! - `/api/*` 仍全部要 session：`/api/{*rest}` 的 404 兜底注册在 layer **之前**，
//!   未认证时先 401（路由存不存在都不透露），认证后才是 404 JSON。

use axum::extract::Path;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};
use tower_http::set_header::SetResponseHeaderLayer;

/// `vite build` 的产物目录。路径由 `build.rs` 决定（缺失时指向占位页）。
pub static WEB_DIST: Dir<'static> = include_dir!("$AF_WEB_DIST");

/// `/assets/*` 的缓存策略：文件名带内容哈希，改内容必然换名字。
pub const ASSET_CACHE: &str = "public, max-age=31536000, immutable";

/// `index.html` 的缓存策略：**不缓存**。
///
/// 反过来会把发版变成「用户永远看到旧壳，却去请求已经删掉的老 hash 资源」，
/// 而症状是白屏 + 404——排查时会以为是构建坏了。
pub const INDEX_CACHE: &str = "no-cache";

/// 给 `/assets/*` 加长缓存头的中间件。
///
/// `overriding` 而不是 `if_not_present`：这些文件的缓存策略由构建产物决定，
/// 不该让别处的默认值覆盖它。
pub fn asset_cache_layer() -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static(ASSET_CACHE))
}

/// `GET /assets/{*path}`。
///
/// 未命中 → 404：**不回落到 index.html**。一条缺失的 JS 被当成 HTML 返回，
/// 浏览器报的是「Unexpected token '<'」，而根因是构建没跑——那种错很难查。
pub async fn asset(Path(path): Path<String>) -> Response {
    let Some(file) = WEB_DIST.get_file(&path) else {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    };
    let mime = content_type(&path);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (header::CACHE_CONTROL, HeaderValue::from_static(ASSET_CACHE)),
        ],
        file.contents(),
    )
        .into_response()
}

/// `GET /` 与 `GET /login`：`index.html`。
///
/// SPA 的入口必须由服务端给出（history 模式下 `/login` 不是一个文件）。
pub async fn index() -> Response {
    serve_index()
}

/// 未匹配的非 API 路径 → `index.html`（SPA fallback）。
///
/// 只挂在 `.layer(require_auth)` 之后，且 `/api/*` 有自己的 404 兜底，
/// 所以它**吞不掉任何 API 404**。
pub async fn spa_index() -> Response {
    serve_index()
}

fn serve_index() -> Response {
    let Some(file) = WEB_DIST.get_file("index.html") else {
        // 构造上不可达：`build.rs` 在 dist 缺失时写的就是 index.html。
        // 真走到这里（有人手工改 AF_WEB_DIST）也不该 panic——那是一台
        // 活着但页面打不开的 Hub，比进程退出好排查。
        //
        // coverage: excluded — 上面三行已说明：这条分支结构上不可达。
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "web console is not built",
        )
            .into_response();
    };
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(INDEX_CACHE)),
        ],
        file.contents(),
    )
        .into_response()
}

/// 扩展名 → MIME。
///
/// 手写一张小表而不是引 `mime_guess`：构建产物只有 `js` / `css` / 图标这几种，
/// 而「猜测任意扩展名」会把一个下载响应变成 `text/html`——那是把静态目录
/// 变成 XSS 面的经典做法。**未知扩展名一律 `application/octet-stream`**。
fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e);
    match ext {
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构建产物里的两类文件必须拿到正确的类型：`text/javascript` 判错时
    /// 浏览器会拒绝执行（`nosniff` 下尤其），页面就是白的。
    #[test]
    fn known_extensions_map_to_their_types() {
        assert!(content_type("assets/index-abc.js").starts_with("text/javascript"));
        assert!(content_type("assets/index-abc.mjs").starts_with("text/javascript"));
        assert!(content_type("assets/index-abc.css").starts_with("text/css"));
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("favicon.ico"), "image/x-icon");
        assert_eq!(content_type("logo.svg"), "image/svg+xml");
        assert_eq!(content_type("app.woff2"), "font/woff2");
    }

    /// 未知（含无扩展名、大小写不同）一律二进制流：不让上传的文件被当 HTML 执行。
    #[test]
    fn unknown_extensions_are_octet_stream() {
        assert_eq!(content_type("secrets.env"), "application/octet-stream");
        assert_eq!(content_type("README"), "application/octet-stream");
        assert_eq!(content_type("APP.JS"), "application/octet-stream");
    }

    /// 嵌入目录里**一定**有 index.html——空目录会让 `/` 变成 500。
    #[test]
    fn the_embedded_dist_has_an_index() {
        assert!(
            WEB_DIST.get_file("index.html").is_some(),
            "AF_WEB_DIST 必须指向含 index.html 的目录（build.rs 的占位页也算）"
        );
    }

    /// `/assets/*` 真的能从嵌入目录里取到构建产物，且带上长期缓存头。
    ///
    /// 不手写文件名：哈希随每次 `vite build` 变，写死会让「跑过一次前端构建」
    /// 变成测试的隐含前提——而 CI 里 Rust job 未必有 dist。
    #[tokio::test]
    async fn a_hashed_asset_is_served_with_long_lived_cache_headers() {
        // 干净检出里 `packages/web/dist` 被 .gitignore 忽略，`build.rs` 只写一个
        // 占位 `index.html`——那时没有 `assets/` 可取。所以这里钉两条：
        // **一定有**的那条（index.html）与**构建产物在时**的那条（带哈希的
        // 文件名）。断言是同一条：「嵌进去的文件能被取到、并带长缓存头」。
        let index = WEB_DIST
            .get_file("index.html")
            .expect("build.rs always writes one");
        assert_served_with_long_cache("index.html", index.contents()).await;

        if let Some(file) = WEB_DIST
            .get_dir("assets")
            .and_then(|dir| dir.files().next())
        {
            let name = file
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .expect("asset file name");
            assert_served_with_long_cache(&format!("assets/{name}"), file.contents()).await;
        }
    }

    /// 「嵌进去的文件能被取到、带长缓存头、字节原样」——这个 handler 的全部
    /// 契约，三条断言合成一处，免得两个用例各写一遍。
    async fn assert_served_with_long_cache(path: &str, contents: &[u8]) {
        let resp = asset(Path(path.to_string())).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path} 必须取得到");
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(ASSET_CACHE),
            "带哈希的产物必须长缓存"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), contents, "字节必须原样");
    }

    /// 缺失的资产 → 404，**不回落到 index.html**。
    ///
    /// 回落会让浏览器把一段 HTML 当 JS 执行，报的是「Unexpected token '<'」——
    /// 症状与根因（构建没跑 / 资源被删）相距很远。
    #[tokio::test]
    async fn a_missing_asset_is_404_not_the_spa_shell() {
        let resp = asset(Path("assets/does-not-exist.js".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// `index` / `spa_index` 都返回 HTML 外壳，且**不缓存**。
    #[tokio::test]
    async fn both_index_handlers_serve_an_uncached_shell() {
        for resp in [index().await, spa_index().await] {
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("text/html; charset=utf-8")
            );
            assert_eq!(
                resp.headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some(INDEX_CACHE),
                "外壳一旦被缓存，发版后用户会一直拿到旧壳"
            );
        }
    }
}
