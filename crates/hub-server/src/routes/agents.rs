//! `/api/agents/config`：agent **启动配置**的读写（M2 子任务 4，要求 2）。
//!
//! # 这里为什么没有「安装」
//!
//! 原先这三条路由是 `/api/agents/install` 与 `/api/agents/uninstall`：Hub 挑一个
//! 模板、下发到设备、设备下载校验解包。**2026-10-26 用户裁定取消**——agent
//! 由用户自己在设备上装（`npm i -g`、下二进制、系统包管理器），Hub 只做两件事：
//!
//! 1. 设备**扫描**本机装了什么（`agent_runtime` 表，走心跳上报）；
//! 2. Hub 存这些 agent 的**启动配置**（`agent_config` 表，就是本模块）。
//!
//! 理由不是省事：让 Hub 往设备写可执行文件 = 「一处配置错、全体设备执行
//! 未核对代码」，换来的只是省掉用户一条命令。而配置全是字符串与字符串
//! 数组，不是代码——风险等级低一个量级。
//!
//! # 三条口径
//!
//! - **配置属于「这台设备上的这个 agent」**（PK `(node_id, agent_id)`）：同一台
//!   设备上不同 agent 的参数不能互相污染；同一个 agent 在不同设备上路径不同。
//! - **配置可以先于 agent 存在**：用户可能先配好、后装。所以 `upsert` 不检查
//!   `agent_runtime` 里有没有这一行——那会让「先配置后安装」这条路走不通。
//! - **三条都是状态变更端点**，必须落在 `build_app` 的 `.layer(...)` 之前：
//!   落到之后会绕过 session 认证与 CSRF，于是任意网页都能改用户的 agent
//!   启动参数（往 `args` 或 `env` 里塞东西，等于让设备下次启动 agent 时
//!   带上攻击者的载荷）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::{err, ApiError, AppState};

/// `GET /api/agents/config?node_id=...` 的查询参数。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub node_id: String,
}

/// 一份配置的对外形状。
///
/// 字段名与 `agent_config` 的列名**不完全一致**（`args` / `env` 是解析后的
/// 结构）：页面拿到的是可以直接喂给启动逻辑的东西，不是 JSON 字符串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDto {
    pub node_id: String,
    pub agent_id: String,
    /// 空串 = 用扫描到的那个可执行文件。
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub updated_at: String,
}

impl From<hub_storage::AgentConfig> for ConfigDto {
    fn from(c: hub_storage::AgentConfig) -> Self {
        Self {
            node_id: c.node_id,
            agent_id: c.agent_id,
            command: c.command,
            args: c.args,
            env: c.env,
            updated_at: c.updated_at,
        }
    }
}

/// `POST /api/agents/config` 的请求体。
///
/// `updated_at` **不在请求体里**：时间戳由 Hub 盖（`now_stamp`）。让客户端给
/// 时间戳等于让客户端决定「哪一次写入更新」，而那条信息只该有一个来源。
#[derive(Debug, Deserialize)]
pub struct UpsertBody {
    pub node_id: String,
    pub agent_id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

/// `DELETE /api/agents/config?node_id=...&agent_id=...` 的查询参数。
#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    pub node_id: String,
    pub agent_id: String,
}

/// `GET /api/agents/config` —— 这台设备上配过的全部 agent。
pub async fn list_config(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let rows = state
        .agent_config()
        .list_for_node(&q.node_id)
        .map_err(|e| ApiError::internal_with("cannot list agent configs", e))?;
    let items: Vec<ConfigDto> = rows.into_iter().map(ConfigDto::from).collect();
    Ok(Json(serde_json::json!({ "configs": items })).into_response())
}

/// `POST /api/agents/config` —— 写入（或覆盖）一份配置。
///
/// **不校验设备是否存在**，也不校验 agent 是否被扫描到：
///
/// - 设备校验会挡掉「设备还没连上就先配好」这个正常流程；
/// - agent 校验会挡掉「先配置、后安装」，而装之前配置恰恰最需要写下来。
///
/// 代价是能写出「指向不存在设备」的孤儿行。那比「配置写不进去」轻得多，
/// 且设备被删除时它们会被一并清掉：清理在 `DeviceRepo::delete_and_mark_removed`
/// 里，与「删设备行」在同一个事务（`DELETE FROM agent_config` 与
/// `DELETE FROM agent_runtime` 各一条）。它不是由 `AgentRuntimeRepo` 做的——
/// 那个 repo 只持有连接池，跟事务里那批语句不是同一条路径。
pub async fn upsert_config(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Result<Response, ApiError> {
    // 自己读 body：理由见 `api.rs::create_session`（axum 的 `Json` 拒绝体会回
    // 422 + text/plain，而 `/api/*` 的错误体只有 `{"error": "<code>"}` 一种形状）。
    let body = crate::routes::read_body(req).await?;
    let body: UpsertBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => return Err(ApiError::bad_request(err::BAD_BODY)),
    };
    if body.node_id.trim().is_empty() || body.agent_id.trim().is_empty() {
        return Err(ApiError::bad_request(err::BAD_BODY));
    }

    let cfg = hub_storage::AgentConfig {
        node_id: body.node_id,
        agent_id: body.agent_id,
        command: body.command,
        args: body.args,
        env: body.env,
        updated_at: hub_core::registry::now_stamp(),
    };
    state
        .agent_config()
        .upsert(&cfg)
        .map_err(|e| ApiError::internal_with("cannot write the agent config", e))?;
    Ok((StatusCode::OK, Json(ConfigDto::from(cfg))).into_response())
}

/// `DELETE /api/agents/config` —— 删掉一份配置，之后该 agent 走默认启动方式。
///
/// 删不存在的配置回 **404**：让「我以为删掉了」与「本来就没有」区分开。
pub async fn delete_config(
    State(state): State<AppState>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, ApiError> {
    let deleted = state
        .agent_config()
        .delete(&q.node_id, &q.agent_id)
        .map_err(|e| ApiError::internal_with("cannot delete the agent config", e))?;
    if !deleted {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
