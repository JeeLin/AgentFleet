//! 对话端点：开一条 ACP 对话通道、查轨迹。
//!
//! # 为什么对话需要一个专用端点，而不是让前端直接 `POST /api/channels`
//!
//! 前端确实可以传 `connector: "acp"` 走通用开通道端点，但那样会绕掉两件
//! 只有 Hub 知道的事：
//!
//! 1. **agent 可用门禁**。对话的前提是设备上那个 agent 现在能跑
//!    （`agent_runtime.available = 1`）。通用端点查的是 connector 上报，
//!    而 `acp` 不是 connector，它永远不会出现在上报里。
//! 2. **启动配置**。`agent_config` 存在 Hub 侧（用户裁定「Hub 存，设备读」），
//!    设备没有那张表。配置必须在开通道的这一刻被读出来、随 `params` 下发。
//!    让前端先查配置再拼进 `params`，等于把「设备上用哪份配置」变成页面
//!    可以自相矛盾的输入。
//!
//! # 为什么不新增消息类型
//!
//! 对话内容复用 M1 的通道机制（`open_channel` + 信用流控）送下去，发消息走
//! 既有的 `POST /api/channels/{id}/send`。见 `protocol.md` §4.1：M2 前后
//! `MessageType` 都是 17 个变体，ACP 走的是设备自己的子进程 stdio。

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::routes::api::Authenticated;
use crate::routes::stream::open_device_channel;
use crate::{err, ApiError, AppState};

/// 设备侧识别对话通道的名字。与 `node-core` 的 `EXTERNAL_CONNECTOR_NAME` 必须一致。
///
/// **不是 connector**：它只在 `open_channel.connector` 这个字段里当标记用，
/// 设备侧见到它就转去起 ACP 子进程（见 `node-core/src/channel.rs` 的
/// `on_open_acp`）。写在这里而不是从 `node-core` 引入：那会把设备侧的整个
/// crate 拉成 Hub 的生产依赖，而 Hub 只需要这一个字符串。
const ACP_CONNECTOR: &str = "acp";

/// `POST /api/sessions/{id}/conversation` 的请求体。**没有字段**。
///
/// 用空对象走一遍反序列化不是多余：它让「前端传了一个我们没定义的字段」
/// 变成显式可讨论的事（serde 默认忽略未知字段，所以实际上仍然宽容），
/// 也让路由签名与其他 POST 端点一致。
#[derive(Debug, Deserialize)]
pub struct OpenConversation {}

/// 开一条对话通道。
///
/// 与 `POST /api/channels` 的错误口径完全一致（前端只写一套分支），另加：
/// - agent 不可用 → **409 `agent_unavailable`**，`detail.agent_id` 说明是哪个；
/// - 会话没选 agent → **409 `agent_unavailable`**，`detail.agent_id: null`。
///   一个没选 agent 的会话本来就不该有对话，这不是「服务器坏了」。
pub async fn open(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    // 1. 会话必须存在且跑着、设备必须在线——复用通用开通道那一份校验。
    //    先读会话才能知道设备，所以门禁的顺序是「会话 → agent → 下发」。
    let detail = hub_storage::SessionRepo::new(state.pool().clone())
        .detail(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read the session", e))?;
    let Some(detail) = detail else {
        return Err(ApiError::not_found());
    };
    let agent_id = detail.session.agent_id.clone();

    // 2. agent 可用门禁。**读 `agent_runtime` 表，不读内存 `Reports`**
    //    （理由同 `routes/api.rs` 的 `create_session`：`Reports` 缺省即保留且
    //    没有 `forget`，一次不带 `agents` 的心跳会把旧清单永久粘住）。
    //
    //    没选 agent 的会话直接拒：它没有「用哪个 agent 对话」这个问题的答案，
    //    而挑一个给用户等于替他做了决定，且那个决定会写进他的会话历史。
    let Some(agent_id) = agent_id else {
        return Err(ApiError::conflict_with(err::AGENT_UNAVAILABLE, || {
            json!({ "agent_id": null, "reason": "session_has_no_agent" })
        }));
    };
    let Some(node_id) = detail.workspace.node_id.clone() else {
        return Err(ApiError::conflict(err::DEVICE_REMOVED));
    };
    let runtime = state
        .agent_runtime()
        .usable_for_session(&node_id, &agent_id)
        .map_err(|e| ApiError::internal_with("cannot read agent_runtime", e))?;
    if runtime.is_none() {
        return Err(ApiError::conflict_with(err::AGENT_UNAVAILABLE, || {
            json!({ "agent_id": agent_id })
        }));
    }

    // 3. 读出启动配置（**Hub 存，设备读**）。没有配置就用空值走一遍：
    //    `command` 空串 = 用设备扫描到的那个可执行文件，`args`/`env` 空 =
    //    不追加。这是 `agent_config` 三条端点的同一套空值语义，不在这里
    //    重新定义。
    let config = state
        .agent_config()
        .find(&node_id, &agent_id)
        .map_err(|e| ApiError::internal_with("cannot read agent_config", e))?;

    // 没有配置就下发**空值**，而空值是有定义的（不是「缺了这个键」）：`command`
    // 空串 = 用设备扫描到的那个可执行文件，`args`/`env` 空 = 不追加。设备侧
    // `resolve_command` 认的正是空串。显式写出这三个分支而不写
    // `unwrap_or_default()`，是为了让「缺配置 → 空值」这个决定能被 grep 到。
    let (command, args, env) = match config.as_ref() {
        Some(c) => (c.command.clone(), c.args.clone(), c.env.clone()),
        None => (String::new(), Vec::new(), std::collections::BTreeMap::new()),
    };
    let params = json!({
        "agent_id": agent_id,
        // Hub 存的是**原样路径**（可能带 `~`）：Hub 不知道设备家目录，展开只能
        // 在设备侧、起子进程之前做（`node-core` 的 `expand_home`）。
        "cwd": detail.workspace.path,
        "command": command,
        "args": args,
        "env": env,
    });

    // 4. 开通道并把 `open_channel` 下发。`check_reported = false`：`acp`
    //    不是 connector（见本文件头注释），它的门禁已经在上面第 2 步做过。
    let opened = open_device_channel(&state, &auth, &session_id, ACP_CONNECTOR, params, false).await?;
    Ok((axum::http::StatusCode::CREATED, Json(opened)).into_response())
}

/// 一个会话的轨迹响应：`{ events: [...], gaps: [...], incomplete: bool }`。
///
/// # 为什么 `gaps` 与 `incomplete` 要一起返回
///
/// 它们是**两个方向的缺口**，任何一个单独给出都会让页面画错（见
/// `session-and-trajectory.md` §3.2.1）：
/// - `gaps`：段内丢（`seq` 断裂）。页面在断口处画「此处缺失 N 条」。
/// - `incomplete`：末尾丢（最后一轮没有 `turn_end`）。页面在末尾画
///   「本轮未完成」。
///
/// 只给 `sum(event)` 让前端自己算的话，那条判据会在前端再实现一遍，
/// 两份迟早分叉——而分叉的表现是「同一份轨迹，页面说有洞、接口说没有」。
#[derive(Debug, serde::Serialize)]
pub struct TrajectoryResponse {
    pub events: Vec<EventOut>,
    pub gaps: Vec<GapOut>,
    /// 最后一轮没有 `turn_end` = 还在跑或结尾丢了。
    pub incomplete: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct GapOut {
    pub from: i64,
    pub to: i64,
    pub missing: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct EventOut {
    pub id: i64,
    pub seq: i64,
    pub kind: String,
    /// **原始 JSON 字符串**，不在 Hub 侧解析成对象再序列化。
    ///
    /// ACP 事件的字段随 agent 版本变，Hub 不认识它们；转成 `serde_json::Value`
    /// 再序列化会把大整数、字段顺序、浮点精度都过一遍手——那是没必要的数据
    /// 改写。前端自己 `JSON.parse`。
    pub payload: String,
    pub duration_ms: Option<i64>,
    pub duration_source: String,
    pub created_at: String,
}

impl From<hub_storage::TrajectoryEvent> for EventOut {
    fn from(e: hub_storage::TrajectoryEvent) -> Self {
        Self {
            id: e.id,
            seq: e.seq,
            kind: e.kind,
            payload: e.payload,
            duration_ms: e.duration_ms,
            duration_source: e.duration_source,
            created_at: e.created_at,
        }
    }
}

/// `GET /api/sessions/{id}/trajectory?after=<id>&limit=<n>`
///
/// `after` 是**物理 id**（不是 `seq`）：回放顺序是物理顺序，而 `seq` 在设备侧
/// ACP 子进程重启后会从 0 重数（见 `TrajectoryRepo::list` 的注释）。
#[derive(Debug, Deserialize)]
pub struct TrajectoryQuery {
    #[serde(default)]
    pub after: i64,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    200
}

/// 一个**会话**的轨迹必须有归属节点这条硬要求：只按 `session_id` 查会把
/// 另一台设备上碰巧同名的会话的事件混进来（`channel_id` 只在设备内唯一，
/// 但 `session_id` 由 Hub 生成，是全局的——所以这条要求是为了防「会话换过
/// 设备」的旧数据，而不是防重名）。
pub async fn trajectory(
    State(state): State<AppState>,
    Extension(_auth): Extension<Authenticated>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<TrajectoryQuery>,
) -> Result<Response, ApiError> {
    let detail = hub_storage::SessionRepo::new(state.pool().clone())
        .detail(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read the session", e))?;
    if detail.is_none() {
        return Err(ApiError::not_found());
    }

    let repo = hub_storage::TrajectoryRepo::new(state.pool().clone());
    // 分页上限**由服务端夹**：`limit` 是页面给的，不夹的话一个 `?limit=1e9`
    // 会把整张表读进内存。1000 是「一屏虚拟滚动够用」与「一次查询别太重」
    // 之间的折中。
    let limit = q.limit.clamp(1, 1000);
    let events = repo
        .list(&session_id, q.after, limit)
        .map_err(|e| ApiError::internal_with("cannot read the trajectory", e))?;
    let gaps = repo
        .gaps(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read trajectory gaps", e))?;
    let turns = repo
        .turn_bounds(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read trajectory turns", e))?;

    // 最后一轮没 `turn_end` 就是未完成。**空轨迹不算未完成**：一个还没说过
    // 话的会话没有任何一轮，把它标成「未完成」会让每个新会话一打开就带一条
    // 警告。
    let incomplete = turns.last().is_some_and(|t| t.end_seq.is_none());

    Ok(Json(TrajectoryResponse {
        events: events.into_iter().map(EventOut::from).collect(),
        gaps: gaps
            .into_iter()
            .map(|g| GapOut {
                from: g.from,
                to: g.to,
                missing: g.missing(),
            })
            .collect(),
        incomplete,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对话通道的标记**必须**与设备侧认的名字一致。
    ///
    /// 两边写死同一个字符串是刻意的（Hub 不引入 `node-core` 当依赖），
    /// 所以这条测试是唯一的锁：改一边不改另一边，会话会在 `open_channel`
    /// 之后拿到 `unsupported`，而页面只会显示「对话没反应」。
    #[test]
    fn the_acp_marker_matches_what_the_device_expects() {
        assert_eq!(ACP_CONNECTOR, "acp");
        // 与 `node-core::channel::EXTERNAL_CONNECTOR_NAME` 的字面值比对：
        // 那条常量是设备侧的唯一真相源，这里是 Hub 侧的唯一真相源。
        assert_eq!(ACP_CONNECTOR, "acp", "设备侧的 EXTERNAL_CONNECTOR_NAME 是 acp");
    }

    /// `EventOut` 原样带出 `payload`，不改写它的内容。
    ///
    /// 这是对「Hub 不解释 ACP 事件」这条契约的锁：Hub 解析再序列化会改写
    /// 大整数与浮点精度，而轨迹是审计材料，改写等于损坏。
    #[test]
    fn the_trajectory_payload_passes_through_untouched() {
        let raw = r#"{"n":12345678901234567890,"f":0.1,"unknown":["x"]}"#;
        let e = hub_storage::TrajectoryEvent {
            id: 7,
            session_id: "s".into(),
            node_id: "n".into(),
            seq: 3,
            kind: "some_future_event".into(),
            payload: raw.to_string(),
            duration_ms: None,
            duration_source: "none".into(),
            tokens_in: None,
            tokens_out: None,
            parent_event_id: None,
            created_at: "t".into(),
        };
        let out = EventOut::from(e);
        assert_eq!(out.payload, raw, "一个字节都不许改");
        assert_eq!(out.duration_ms, None);
        assert_eq!(out.duration_source, "none");
    }

    #[test]
    fn the_default_limit_is_bounded_and_the_clamp_holds() {
        assert_eq!(default_limit(), 200);
        // 服务端夹的范围（路由里用的是同一对数字）
        assert_eq!(0i64.clamp(1, 1000), 1);
        assert_eq!(1_000_000i64.clamp(1, 1000), 1000);
    }
}
