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

/// 日志里一条 `title` 最多保留的字符数。
///
/// 历史标题是人给对话起的名字，正常只有几十个字。上限存在的唯一理由是
/// `title` 只受 1 MiB body 上限约束——没有上限时单条日志行就能有同一个量级。
const LOG_TITLE_MAX_CHARS: usize = 120;

/// 把设备库来的 `title` 净化成**单行、有界**的日志字段。
///
/// 不是审美问题，两条都可执行地成立：
/// 1. `tracing` 的 `%` 走 `Display`，**不转义**换行。标题里塞
///    `"\n2026-11-11T00:00:00Z ERROR hub_server: ..."` 就会在 stderr 里伪造出
///    一条与真实 `ERROR` 无法区分的记录。标题来自用户本机的外部 agent 库，
///    不是可信输入。
/// 2. 换行换成空格、其余控制字符（含 ESC，防终端转义注入）同样换成空格，
///    可打印字符原样保留：中文 / 引号 / emoji 都还在，排障时标题仍可读。
fn log_safe_title(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(LOG_TITLE_MAX_CHARS)
        .collect()
}

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
    // 进日志前必须 [`log_safe_title`]：它来自设备侧的外部 agent 库，不是可信输入。
    tracing::info!(
        session_id = %session.id,
        node_id = %identity.node_id,
        provider = %req.provider,
        title = %log_safe_title(&req.title),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 伪造日志行与超长标题都必须被压成**单行、有界**；正常标题原样保留。
    ///
    /// 断言不是「净化过」，而是三件可观测的事：
    /// 1. 换行 / CR / ESC 全变成空格 —— 日志行数不会因为标题而变多，终端也不会
    ///    执行注入序列。
    /// 2. 字符数被 `LOG_TITLE_MAX_CHARS` 截断 —— 900 KB 的标题不会变成 900 KB
    ///    的日志行。
    /// 3. 中文与普通标点原样保留 —— 净化不是把可读信息一起删掉。
    #[test]
    fn a_hostile_title_is_folded_into_one_bounded_log_field() {
        let forged = "\n2026-11-11T00:00:00Z ERROR hub_server: fake critical line";
        let safe = log_safe_title(forged);
        assert!(!safe.contains('\n'), "no line breaks: {safe:?}");
        assert!(
            safe.starts_with(' '),
            "the leading newline becomes a space: {safe:?}"
        );

        let with_escape = "a\u{1b}[2J b\r\nc";
        let safe = log_safe_title(with_escape);
        assert!(
            !safe.chars().any(char::is_control),
            "no control chars: {safe:?}"
        );

        let long = "标".repeat(900 * 1024);
        let safe = log_safe_title(&long);
        assert_eq!(safe.chars().count(), LOG_TITLE_MAX_CHARS);
        assert!(
            safe.chars().all(|c| c == '标'),
            "truncation keeps whole chars"
        );

        let normal = "固定 session_v2 的迁移窗口 — ok";
        assert_eq!(log_safe_title(normal), normal);
    }
}
