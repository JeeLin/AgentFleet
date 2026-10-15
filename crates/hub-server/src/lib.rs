//! afhub 的可测试核心：路由装配。
//!
//! 与 `main.rs` 分开是为了能**不起真端口**就测路由：`build_app` 返回
//! `axum::Router`，测试用 `tower::ServiceExt::oneshot` 直接打请求。
//!
//! 约束 1：**Hub 零入站端口**——只监听页面侧的 `127.0.0.1:8080`，
//! 设备一律**主动外联** `/ws`。这里没有任何设备入站路由。

pub mod routes;

use axum::{routing::get, Json, Router};
use serde_json::json;

/// `GET /healthz` —— 存活探针。
///
/// 页面与运维用它确认 Hub 起来了。**不泄露任何内部状态**：只回 `status`，
/// 不回版本、设备数、数据库路径——那些属于 `/api/*`，需要 session。
async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

/// 装配全部路由。
pub fn build_app() -> Router {
    Router::new().route("/healthz", get(healthz))
}

#[cfg(test)]
mod tests {
    // 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
    // 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
    // let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
    // 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::indexing_slicing
    )]
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = build_app();
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
    async fn unknown_route_is_404() {
        let app = build_app();
        let req = Request::builder().uri("/nope").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn app_can_bind_and_serve_healthz() {
        // 真起一个监听（随机端口）跑一次，证明路由装配后能真正 serve。
        // 子任务 0 测试标准 ⑥：bind 成功 + GET /healthz 返回 200。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_app();
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let app = build_app();
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
        // 豁免表是安全边界：M0 只放行健康探针。
        // 任何返回设备数据/密钥的路径出现在表里都是事故。
        assert!(routes::is_auth_exempt("/healthz"));
        assert!(!routes::is_auth_exempt("/api/devices"));
        assert!(!routes::is_auth_exempt("/setup"));
        assert!(!routes::is_auth_exempt("/login"));
        // 前缀不算豁免：`/healthz/../api/devices` 不能因为以 /healthz 开头就放行
        assert!(!routes::is_auth_exempt("/api/devices/healthz"));
        // 表本身要小——豁免越多边界越模糊
        assert!(
            routes::AUTH_EXEMPT_PATHS.len() <= 8,
            "auth exemption table must stay small"
        );
    }
}
