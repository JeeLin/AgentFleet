//! 设备签名 HTTP 导入端点：`POST /device-api/session-imports`（v0.7.4）。
//!
//! # 它做什么
//!
//! 设备侧 `afcli import bind` 把用户选中的一条**外部历史会话**绑定成 Hub 会话：
//! 建一行 `session`，把 `acp_session_id` 预填成用户选中的那个 id。之后从 Hub
//! 打开对话时，设备拿这个 id 走 ACP `session/load`，由 agent 自己找回历史
//! （实测跨进程记得，见 `session-and-trajectory.md` §2.1）。
//!
//! **绑定，不是搬运**：这里不读、不写任何历史消息，也不生成轨迹。
//!
//! # 认证
//!
//! 不在 `/api/*` 下，也不经过浏览器 session / CSRF。签名、body 摘要、时间窗、
//! nonce 全部由 [`crate::import_auth`] 的专用 layer 完成；本文件只做**业务**
//! 校验（provider allowlist、会话 id 形状、去重、复用建会话门禁）。
//!
//! # 错误码
//!
//! 固定 code（`{"error":"<code>"}`），不夹带 SQL / 路径 / 签名：
//! - 401 `device_signature_invalid`：签名 / 摘要 / 时间窗 / 设备公钥任一不过。
//! - 409 `import_replayed`：同一 nonce 在时间窗内第二次出现。
//! - 409 `unsupported_import_provider`：provider 不在首版 allowlist。
//! - 409 `import_already_exists`：同一设备 + agent + `acp_session_id` 已导入。
//! - 400 `invalid_request_body` / `invalid_workspace_path` / 409 `device_offline` /
//!   409 `agent_unavailable` / 404：与 `POST /api/sessions` 同一串门禁。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::Serialize;

use crate::routes::api::{create_session_guarded, DeviceImportAuth, NewSession};
use crate::{err, ApiError, AppState};

/// 首版允许导入的 provider 闭集。
///
/// 只放**经过 ACP 真机验证**的软件（当前只有 `opencode`，`acp_args: &["acp"]`）。
/// 其余 8 个 `KNOWN_AGENTS` 条目 `acp_args: &[]`，导入进来也开不了对话，所以
/// 明确 409 而不是让用户拿到一个永远哑的会话。Hub 不读 `node-core` 的 provider
/// 注册表（依赖方向：Hub 不反向依赖设备侧进程库），这份 allowlist 是**服务端
/// 边界**，与设备侧注册表必须同时更新。
const IMPORT_PROVIDERS: &[&str] = &["opencode"];

// 请求体是 `identity::import_protocol::ImportRequest`：CLI 与 Hub 序列化
// 的是**同一个类型**，字段顺序漂移不再只是注释里的约定。
pub use identity::import_protocol::ImportRequest;

/// 认证 layer 解析好的请求体。handler 消费它，**不重新解析**。
#[derive(Debug, Clone)]
pub struct ImportedBody(pub ImportRequest);

/// 成功响应。**恰好两个字段**：CLI 打印 `session_id`，`acp_session_id` 是
/// 回显给调用方做「绑定的是哪条历史」的确认。
#[derive(Debug, Clone, Serialize)]
pub struct ImportResponse {
    pub session_id: String,
    pub acp_session_id: String,
}

/// `POST /device-api/session-imports`。
///
/// 顺序固定（任一步失败都不落会话行）：
/// provider allowlist → 会话 id 形状 → 去重 → 共享建会话门禁 → 探测起点 →
/// 回填起点 → 推 `session_state` → 201。
pub async fn import_session(
    State(state): State<AppState>,
    Extension(imported): Extension<ImportedBody>,
    Extension(identity): Extension<DeviceImportAuth>,
) -> Result<Response, ApiError> {
    let req = imported.0;

    // 1. provider allowlist。放在最前面：不支持的软件不该有机会碰到任何库操作。
    if !IMPORT_PROVIDERS.contains(&req.provider.as_str()) {
        return Err(ApiError::conflict(err::UNSUPPORTED_IMPORT_PROVIDER));
    }

    // 2. 会话 id 形状。`acp_session_id` 会原样下发给设备走 `session/load`，
    //    空串不是「没有」而是「去 load 一个叫 "" 的会话」——`conversation.rs`
    //    把空串与 `NULL` 分开处理，这里也不允许写空串。
    //
    //    首版 opencode 的 provider 契约保证 `external_session_id` 就是原样的
    //    ACP 会话 id（`crates/node-cli/src/import_http.rs` 的构造），两者不等
    //    说明调用方不是本仓的 CLI，拒。
    if req.acp_session_id.trim().is_empty() || req.external_session_id != req.acp_session_id {
        return Err(ApiError::bad_request(err::BAD_BODY));
    }

    // 3. 去重。**检查与 INSERT 合成一条原子 SQL**（`ImportedIfAbsent`）：
    //    两个不同 nonce 的并发请求通过签名层后，`INSERT ... WHERE NOT EXISTS`
    //    只会插入一行，另一个拿到 409。先 `find_imported` 再 `create_imported`
    //    的窗口（中间隔着 probe_base_rev）能让两者都回 201，所以不再分两步。
    //
    //    `provider` 就是 `agent_id`：v1 两者 1:1（`opencode` 同名），`session`
    //    表没有 provider 列，不为此加列（见 `SessionRepo::find_imported`）。
    let repo = hub_storage::SessionRepo::new(state.pool().clone());

    // 4. 复用 `POST /api/sessions` 的同一串门禁（路径 / 设备 / agent / INSERT）。
    let auth = identity.auth;
    let session = create_session_guarded(
        &state,
        &auth,
        &identity.node_id,
        &req.cwd,
        &req.provider,
        NewSession::ImportedIfAbsent(&req.acp_session_id),
    )
    .await?;

    // 5. 起点探测与普通建会话完全相同：先落行、再探测、再回填。导入不改这条
    //    链路——会话仍是 `Running`，`open_device_channel` 的前提不变。
    let (base_rev, base_rev_status) =
        super::base_rev::probe_base_rev(&state, &auth, &session.id, &identity.node_id).await;
    repo.set_base_rev(&session.id, base_rev.as_deref(), base_rev_status)
        .map_err(|e| ApiError::internal_with("cannot record the session's base revision", e))?;

    // 6. 让已打开的页面立刻看到新会话，不必等下一次轮询。
    state
        .streams()
        .session_state(&session.id, session.state.as_str());

    // 标题只进日志，不进库：`session` 表没有 title 列，历史标题归 agent 自己管。
    // 不落库也不丢弃——它是排障时唯一能把日志行对回用户所选历史会话的线索。
    tracing::info!(
        session_id = %session.id,
        node_id = %identity.node_id,
        provider = %req.provider,
        title = %req.title,
        imported = true,
        "session imported"
    );

    Ok((
        StatusCode::CREATED,
        axum::Json(ImportResponse {
            session_id: session.id,
            acp_session_id: req.acp_session_id,
        }),
    )
        .into_response())
}
