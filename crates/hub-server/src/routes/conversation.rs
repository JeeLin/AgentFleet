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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::routes::api::Authenticated;
use crate::routes::stream::{open_device_channel, send_channel_data};
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
        return Err(ApiError::conflict_with(
            err::AGENT_UNAVAILABLE,
            || json!({ "agent_id": null, "reason": "session_has_no_agent" }),
        ));
    };
    let Some(node_id) = detail.workspace.node_id.clone() else {
        return Err(ApiError::conflict(err::DEVICE_REMOVED));
    };
    let runtime = state
        .agent_runtime()
        .usable_for_session(&node_id, &agent_id)
        .map_err(|e| ApiError::internal_with("cannot read agent_runtime", e))?;
    if runtime.is_none() {
        return Err(ApiError::conflict_with(
            err::AGENT_UNAVAILABLE,
            || json!({ "agent_id": agent_id }),
        ));
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

    // 4. **把这条会话已经用到的最大 `seq` 下发**，让设备接着往下编号。
    //
    // # 为什么必须下发
    //
    // `seq` 是设备侧计数器，而它在每个 ACP 子进程里**从 1 重数**：关掉页面
    // 再打开、worker 换代、`session/load` 建了新进程——每一种都让计数器回到
    // 0。于是新事件带 `seq=1`，与库里已有的 `seq=1` 撞
    // `UNIQUE(session_id, node_id, seq)`，被 `append` 的 `INSERT OR IGNORE`
    // 静默吞掉。症状是「轨迹里少了一半对话」，而 `gaps` 不报警（seq 仍然连续）。
    //
    // 实测证据（M2 子任务 13）：同一会话第二次开对话走 `session/load`，
    // agent 重放的 `user_message_chunk` 带 `seq=1`，而库里 `seq=1` 已是
    // `available_commands_update`——那条用户消息就此消失。
    //
    // 所以「会话内只增」这条契约必须由**跨进程的真相源**兜住，而真相源是
    // `trajectory_event` 表，不是设备内存。下发它，设备侧 `Seq` 才能从历史
    // 续上，`UNIQUE` 才真正表达「同一事件不写两行」而不是「新事件被旧事件顶掉」。
    //
    // 读不到（新会话，一条事件都没有）就下发 `0`：`Seq` 的 `next()` 是
    // `fetch_add(1) + 1`，起点 0 正好让第一条是 1。**不要下发 `null`**——
    // 那会让设备侧多一个「没有起点」的分支，而 0 已经表达了同一件事。
    let last_seq = state
        .trajectory()
        .last_seq(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read the trajectory's last seq", e))?
        .unwrap_or(0);

    let params = json!({
        "agent_id": agent_id,
        // Hub 存的是**原样路径**（可能带 `~`）：Hub 不知道设备家目录，展开只能
        // 在设备侧、起子进程之前做（`node-core` 的 `expand_home`）。
        "cwd": detail.workspace.path,
        "command": command,
        "args": args,
        "env": env,
        // 已有 ACP 会话 id 时**下发它**，设备侧据此走 `session/load` 而不是
        // `session/new`（子任务 8）。这解决的是「Hub 重启 / worker 换代之后，
        // 同一会话再开会话时 agent 还记不记得之前聊过什么」——实测新进程
        // load 同一 id 之后确实记得（M2 里程碑文档的 V-2）。
        //
        // `null` 表示这条会话还没有 ACP id（第一次开），设备侧据此新建。
        // **不要下发空串**：空串与「没有」在设备侧是两回事，写空串会让设备
        // 去 load 一个叫 "" 的会话。
        "acp_session_id": detail.session.acp_session_id,
        // 这条会话已经落库的最大 `seq`；设备侧编号从这里续上。见上文的
        // 实测证据——不续号就会静默丢事件。
        "last_seq": last_seq,
    });

    // 5. 开通道并把 `open_channel` 下发。`check_reported = false`：`acp`
    //    不是 connector（见本文件头注释），它的门禁已经在上面第 2 步做过。
    let opened =
        open_device_channel(&state, &auth, &session_id, ACP_CONNECTOR, params, false).await?;
    Ok((axum::http::StatusCode::CREATED, Json(opened)).into_response())
}

/// `POST /api/sessions/{id}/conversation/cancel` —— 停止**正在跑的那一轮**，不关闭会话。
///
/// 与 `open` 复用同样的门禁：会话存在、设备未被删除、对话通道已开。下发一条
/// `channel_data { "cancel": true }` 到设备；设备侧的 `drain_inbound` 识别它并
/// 发 `session/cancel` 给 ACP 子进程，**不关会话**——页面停下之后还能继续聊。
///
/// # 可区分的结局（都不许静默 204）
///
/// | 事实 | 返回 |
/// |---|---|
/// | 会话不存在 | 404 `not_found` |
/// | 设备已被删除 | 409 `device_removed` |
/// | 对话通道从未打开（没有 channel 绑定） | 404 `not_found`（“没有这条对话”） |
/// | 设备离线 | 409 `device_offline`（由 `send_to_device` 回） |
/// | 下发成功 | 204 No Content |
///
/// 「没有这条对话」与「设备已被删除」是两个不同的用户动作：前者是说「还没
/// 开过对话，按停止也没意义」，后者是说「这台机器不会再回来了」。合成一个
/// 码会让页面给错下一步。
///
/// **不是「从不静默成功」的全部**：「有一轮在飞」 vs「本来就闲着」这两种
/// 情形，Hub 侧不持续追踪（`say`/`turn_end` 的上报在 WS 读循环里，不在这
/// 个端点能同步看到）。前端的停止按钮只在回复在飞时才可用，所以它不会
/// 在「闲着」时调这里；设备侧 `drain_inbound` 若收到 cancel 而没轮次在飞，
/// 只留痕、不关通道。所以这个端点**只说得出两件事**：404「没绑过通道」与
/// 204「帧已发出」——它判不出、也不承诺「设备上真有一轮被掐掉」。
pub async fn cancel(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    let detail = hub_storage::SessionRepo::new(state.pool().clone())
        .detail(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read the session", e))?;
    let Some(detail) = detail else {
        return Err(ApiError::not_found());
    };
    let Some(node_id) = detail.workspace.node_id.clone() else {
        return Err(ApiError::conflict(err::DEVICE_REMOVED));
    };
    // 找这条会话的 ACP 通道绑定：没有就说明对话从未打开过，不是「停了」，
    // 而是「还没开始」。同时校验归属，避免泄漏「会话存在但不在你名下」。
    let Some((channel_id, bound_node)) = state
        .bindings()
        .find_by_session(&session_id, auth.subject())
    else {
        return Err(ApiError::not_found());
    };
    // **绑定里的 node_id 必须与会话所属的一致**，不一致就当作「没有可用绑定」。
    //
    // `send_channel_data` 是**按 node_id 路由**的，所以这里要是照会话的
    // node_id 发、而绑定指向另一台设备，这一帧就发到了错误的设备上（那条
    // 通道 id 在它那儿多半不存在，于是帧无声消失，页面却收到了 204）。
    // 注释原先只写「应与之一致」——那是一句愿望，不是检查。设备重新注册
    // 或绑定没清干净时它不成立，而症状是「按了停止什么都没发生」。
    if bound_node != node_id {
        return Err(ApiError::not_found());
    }
    send_channel_data(&state, &node_id, &channel_id, json!({ "cancel": true }))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 一个会话的轨迹响应：`{ events: [...], gaps: [...], incomplete: bool, running: bool }`。
///
/// # 为什么 `gaps`、`incomplete`、`running` 要一起返回
///
/// 它们是三个方向的缺口，任何一个单独给出都会让页面画错（见
/// `session-and-trajectory.md` §3.2.1）：
/// - `gaps`：段内丢（`seq` 断裂）。页面在断口处画「此处缺失 N 条」。
/// - `incomplete`：末尾丢且**没有开始标记**（最后一轮既无 `turn_end` 也无
///   `turn_start`）。页面在末尾画「本轮未完成」。
/// - `running`：最后一轮**开过没关**（有 `turn_start`、无 `turn_end`）。
///
/// 只给 `sum(event)` 让前端自己算的话，那几条判据会在前端再实现一遍，
/// 两份迟早分叉——而分叉的表现是「同一份轨迹，页面说有洞、接口说没有」。
///
/// # 为什么 `running` 与 `incomplete` 是两个字段而不是一个三态
///
/// 它们回答的是**两个不同的问题**，而且都由同一个 `turns` 的最后一项算出：
/// - `running`：这一轮**开了**、并且还没关。一轮跑 130 秒是正常事，这段时间
///   里它必然是 `running`。
/// - `incomplete`：这一轮连开始标记都没有，结尾也没有。开始标记是这次改动
///   才有的，所以 `incomplete` 今天同时覆盖「改动之前写下的历史行」与
///   「开始标记与结束标记都丢了」这两种情形——本接口分不开它们，也不假装
///   分得开。
///
/// 合成一个枚举会把「历史行」和「开始标记丢了」塞进同一个值，而页面要画的
/// 是两句话。分开之后**两者互斥**：`running` 为真时 `incomplete` 必为假，
/// 反之亦然（见 `trajectory` 里的两行赋值）。
#[derive(Debug, serde::Serialize)]
pub struct TrajectoryResponse {
    pub events: Vec<EventOut>,
    pub gaps: Vec<GapOut>,
    /// 最后一轮既没有 `turn_start` 也没有 `turn_end`：结尾丢了，或者那是
    /// 开始标记上线之前写下的历史行。
    pub incomplete: bool,
    /// 最后一轮有 `turn_start`、没有 `turn_end`：**一轮开了还没关**。
    ///
    /// **这不等于「agent 还活着」**：开始标记落下之后设备掉线、进程被杀，
    /// 这一轮同样是「开了没关」，本接口看不出差别——它说的是轨迹里那个标记
    /// 的状态，不是设备的存活状态。
    pub running: bool,
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

/// 轨迹分页的两个边界。**生产与测试都引用这一对常量**：把 `1000` 抄进断言里，
/// 改生产那份时测试仍然绿，等于没锁。
///
/// 上限 1000 是「一屏虚拟滚动够用」与「一次查询别太重」之间的折中；下限 1 是因为
/// `?limit=0` 只会让页面拿到空数组，那不是分页而是故障。
const TRAJECTORY_LIMIT_MIN: i64 = 1;
const TRAJECTORY_LIMIT_MAX: i64 = 1000;

/// 把页面给的 `limit` 夹进 [`TRAJECTORY_LIMIT_MIN`]..=[`TRAJECTORY_LIMIT_MAX`]。
///
/// 抽成函数不是为了复用（只调用一次），而是为了让测试**测到生产那一行**：
/// 测试里自己再写一遍 `v.clamp(min, max)` 的话，生产那行没跟上常量也照样绿。
fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(TRAJECTORY_LIMIT_MIN, TRAJECTORY_LIMIT_MAX)
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
    // 会把整张表读进内存。边界是上面那对 `TRAJECTORY_LIMIT_*` 常量。
    let limit = clamp_limit(q.limit);
    let events = repo
        .list(&session_id, q.after, limit)
        .map_err(|e| ApiError::internal_with("cannot read the trajectory", e))?;
    let gaps = repo
        .gaps(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read trajectory gaps", e))?;
    let turns = repo
        .turn_bounds(&session_id)
        .map_err(|e| ApiError::internal_with("cannot read trajectory turns", e))?;

    // 最后一轮的状态，两个标志都由这一个 `turns.last()` 算出（互相矛盾的两份
    // 结论不可能同时出现）。**空轨迹两个都是 false**：一个还没说过话的会话
    // 没有任何一轮，把空轨迹标成「未完成」会让每个新会话一打开就带一条警告。
    //
    // `running` = 开过没关。`incomplete` = 连开始标记都没有、结尾也没有。
    // 后者必须**排除**有开始标记的情形，否则一轮正常跑了 130 秒的对话会被
    // 报成「本轮未完成」——那正是这次要修的那个谎。`start_seq` 为 `None` 的
    // 历史行仍然报 `incomplete`：那时设备还不会发开始标记，丢没丢分不开，
    // 不如按「可能丢了」说。
    let last = turns.last();
    let running = last.is_some_and(|t| t.start_seq.is_some() && t.end_seq.is_none());
    let incomplete = last.is_some_and(|t| t.start_seq.is_none() && t.end_seq.is_none());

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
        running,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对话通道的标记**必须**是 `"acp"`。
    ///
    /// 这条锁的是 **Hub 自己那份** `ACP_CONNECTOR`。它与设备侧的
    /// `node_core::channel::EXTERNAL_CONNECTOR_NAME` 是两个各自写死的字符串
    /// （`node-core` 只是 hub-server 的 dev-dependency，生产里没法引用它），
    /// 所以这里**比不出**「两边一致」——那需要设备侧那份也被断言住，而它由
    /// `tests/api.rs` 的 `the_device_acp_connector_name_is_still_acp` 锁。
    /// 两条用例合起来才是双向锁：改一边不改另一边，会有一条变红。
    #[test]
    fn the_acp_marker_matches_what_the_device_expects() {
        assert_eq!(
            ACP_CONNECTOR, "acp",
            "Hub 侧的 ACP_CONNECTOR 必须是 acp；设备侧那份由 tests/api.rs 锁"
        );
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

    /// 分页边界：默认值与夹逼范围都是**模块常量**，这里不重抄数字。
    ///
    /// 反向验证：把生产那行改成 `q.limit.clamp(TRAJECTORY_LIMIT_MIN, 2000)`（常量不动），
    /// 这条会红——它比的是 `clamp_limit` 的结果，不是常量等于它自己。
    #[test]
    fn the_default_limit_is_bounded_and_the_clamp_holds() {
        assert_eq!(default_limit(), 200);
        // **测的是生产那个函数**（路由里写的是 `clamp_limit(q.limit)`）。测试自己
        // 再写一遍 `v.clamp(A, B)` 的话，生产那行没跟上常量也照样绿——那样锁住的
        // 是常量与它自己，不是行为。
        assert_eq!(clamp_limit(0), TRAJECTORY_LIMIT_MIN, "低于下限夹到下限");
        assert_eq!(clamp_limit(-1), TRAJECTORY_LIMIT_MIN);
        // 这条是真正的目的：**超过上限的请求被夹到上限**。
        assert_eq!(clamp_limit(TRAJECTORY_LIMIT_MAX + 1), TRAJECTORY_LIMIT_MAX);
        assert_eq!(
            clamp_limit(i64::MAX),
            TRAJECTORY_LIMIT_MAX,
            "`?limit=1e9` 不许把整张表读进内存"
        );
        assert_eq!(
            clamp_limit(TRAJECTORY_LIMIT_MAX),
            TRAJECTORY_LIMIT_MAX,
            "上限本身是允许的"
        );
    }

    // ---- 轨迹端点的 `running` / `incomplete` ----
    //
    // 直接调 handler 而不是走 HTTP：`GET /api/sessions/{id}/trajectory` 的集成
    // 用例在 `tests/api.rs`（那条锁的是 JSON 形状），这里要锁的是**两个标志
    // 的判据**，而造出「有开始标记没结束标记」这种状态只需要往轨迹表里写行。

    /// 写几条事件进轨迹，返回一个可调 handler 的 state 与那条会话的 id。
    fn trajectory_fixture(
        events: &[hub_storage::NewEvent],
    ) -> (crate::AppState, tempfile::TempDir, String) {
        let d = tempfile::tempdir().expect("tmp");
        let pool = hub_storage::db::open_in(d.path()).expect("db").1;
        let state = crate::AppState::bootstrap_with(
            pool,
            d.path(),
            hub_core::auth::AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            std::time::Duration::from_millis(50),
            hub_core::auth::LoginRateLimiter::new(10, 60),
        )
        .expect("state");

        let enrolled = hub_core::registry::Registry::new(state.pool().clone())
            .enroll(
                &[9u8; 32],
                hub_storage::repo::NewDevice {
                    id: String::new(),
                    name: "dev".into(),
                    key_fingerprint: String::new(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    hostname: "h".into(),
                    node_version: "0.1.0".into(),
                },
            )
            .expect("enroll");
        let node_id = enrolled.node_id();

        let seat = hub_storage::SessionRepo::new(state.pool().clone())
            .create(node_id, "~/src/app", Some("opencode"), "t0")
            .expect("建会话");

        let repo = hub_storage::TrajectoryRepo::new(state.pool().clone());
        for e in events {
            repo.append(&seat.id, node_id, e, &hub_core::registry::now_stamp())
                .expect("写轨迹");
        }
        (state, d, seat.id)
    }

    /// 调 `GET /api/sessions/{id}/trajectory` 的那份 handler，把 JSON 解回来。
    async fn trajectory_json(state: &crate::AppState, session_id: &str) -> serde_json::Value {
        let r = super::trajectory(
            axum::extract::State(state.clone()),
            axum::Extension(Authenticated::new("admin".to_string())),
            axum::extract::Path(session_id.to_string()),
            axum::extract::Query(super::TrajectoryQuery {
                after: 0,
                limit: 100,
            }),
        )
        .await
        .expect("轨迹端点应成功");
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn chunk(seq: i64) -> hub_storage::NewEvent {
        hub_storage::NewEvent::new(seq, "agent_message_chunk", r#"{"k":1}"#)
    }

    /// **有开始标记、没有结束标记 ⇒ `running: true`、`incomplete: false`**。
    ///
    /// 这是一轮正常跑着的样子（实测一轮可以跑 130 秒以上）。从前它被报成
    /// 「本轮未完成」——那是在说一个没发生的事实。
    ///
    /// 两处反向验证（都实跑过）：把 handler 里 `running` 那行改成 `false`，
    /// 红在「running 该是 true」；把 `incomplete` 的判据改回只看
    /// `end_seq.is_none()`，红在「incomplete 该是 false」。
    #[tokio::test]
    async fn a_turn_opened_but_not_closed_is_running_not_incomplete() {
        let (state, _d, session_id) =
            trajectory_fixture(&[hub_storage::NewEvent::turn_start(1), chunk(2)]);
        let v = trajectory_json(&state, &session_id).await;
        assert_eq!(v["running"], json!(true), "开了没关就是 running：{v}");
        assert_eq!(
            v["incomplete"],
            json!(false),
            "有开始标记就不许说「未完成」——那是在说一个没发生的事实：{v}"
        );
    }

    /// **两个标记都没有 ⇒ `running: false`**。
    ///
    /// 历史行（开始标记上线之前写的）与「什么都没写」在库里长得一样，
    /// 这一个标志只回答「有没有开过」，两种都不是「开过」。
    ///
    /// 反向验证（实跑过）：把 `running` 的判据改成 `end_seq.is_none()`，
    /// 红在「running 该是 false」。
    #[tokio::test]
    async fn a_turn_with_neither_marker_is_not_running() {
        let (state, _d, session_id) = trajectory_fixture(&[chunk(1), chunk(2)]);
        let v = trajectory_json(&state, &session_id).await;
        assert_eq!(v["running"], json!(false), "没开过不算 running：{v}");
        assert_eq!(
            v["incomplete"],
            json!(true),
            "结尾也没有、开始也没有，只能按「可能丢了」报：{v}"
        );
    }

    /// **两个标记都有 ⇒ `running: false`、`incomplete: false`**：这一轮走完了。
    ///
    /// 反向验证（实跑过）：把 `running` 的判据里的 `end_seq.is_none()` 去掉，
    /// 红在「running 该是 false」。
    #[tokio::test]
    async fn a_closed_turn_is_neither_running_nor_incomplete() {
        let (state, _d, session_id) = trajectory_fixture(&[
            hub_storage::NewEvent::turn_start(1),
            chunk(2),
            hub_storage::NewEvent::turn_end(3, "end_turn"),
        ]);
        let v = trajectory_json(&state, &session_id).await;
        assert_eq!(v["running"], json!(false), "关了就不是 running：{v}");
        assert_eq!(v["incomplete"], json!(false), "走完了就不是未完成：{v}");
    }

    // ---- `cancel` 的两条 404 出路 ----
    //
    // 这两处都直接调 handler，而不是走 HTTP：要造出「绑定指向别的设备」这种
    // 不一致，只能碰绑定表，而它是 `pub(crate)` 的。走 HTTP 反而要为了这一
    // 个判据把内部结构开成公开 API。

    /// 会话属于 A，绑定却指向 B → 404，**不把停止发到 B 上**。
    ///
    /// `send_channel_data` 按 node_id 路由。照会话的 node 发、绑定却指向另一台
    /// 设备时，那一帧会落到错的设备上（通道 id 在它那儿多半不存在，于是无声
    /// 消失），页面却收到 204——症状是「按了停止什么都没发生」。所以宁可当
    /// 「没有可用绑定」。
    ///
    /// 生产里的来路是设备重新注册后旧绑定没被 `drop_node` 清干净；单测直接
    /// 造出这种局面。反向验证：去掉 `bound_node != node_id` 这个判断，本用例
    /// 会拿到 204。
    #[tokio::test]
    async fn cancelling_with_a_binding_for_another_node_is_404() {
        let d = tempfile::tempdir().expect("tmp");
        let pool = hub_storage::db::open_in(d.path()).expect("db").1;
        let state = crate::AppState::bootstrap_with(
            pool,
            d.path(),
            hub_core::auth::AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            std::time::Duration::from_millis(50),
            hub_core::auth::LoginRateLimiter::new(10, 60),
        )
        .expect("state");

        // 会话要挂在一台真设备上（`session.workspace` 有外键）。
        let enrolled = hub_core::registry::Registry::new(state.pool().clone())
            .enroll(
                &[7u8; 32],
                hub_storage::repo::NewDevice {
                    id: String::new(),
                    name: "dev".into(),
                    key_fingerprint: String::new(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    hostname: "h".into(),
                    node_version: "0.1.0".into(),
                },
            )
            .expect("enroll");
        let home = enrolled.node_id();

        let seat = hub_storage::SessionRepo::new(state.pool().clone())
            .create(home, "~/src/app", Some("opencode"), "t0")
            .expect("建会话");

        // 绑定的 node 与这条会话所属的不一致。
        state.bindings().bind(
            "ch_mismatched",
            super::super::stream::test_binding_for_session("n_elsewhere", &seat.id, "admin"),
        );

        let r = super::cancel(
            axum::extract::State(state.clone()),
            axum::Extension(Authenticated::new("admin".to_string())),
            axum::extract::Path(seat.id.clone()),
        )
        .await;
        let err = r.expect_err("绑定指向别的设备时必须拒绝，不许假装发出去");
        assert_eq!(
            err.into_response().status(),
            StatusCode::NOT_FOUND,
            "要回 404，而不是 204 或 500"
        );
    }

    /// 会话 id 不存在 → 404，且与「没绑定」同一个错误码（不泄漏存在性）。
    #[tokio::test]
    async fn cancelling_an_unknown_session_is_404() {
        let d = tempfile::tempdir().expect("tmp");
        let pool = hub_storage::db::open_in(d.path()).expect("db").1;
        let state = crate::AppState::bootstrap_with(
            pool,
            d.path(),
            hub_core::auth::AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            std::time::Duration::from_millis(50),
            hub_core::auth::LoginRateLimiter::new(10, 60),
        )
        .expect("state");

        let r = super::cancel(
            axum::extract::State(state),
            axum::Extension(Authenticated::new("admin".to_string())),
            axum::extract::Path("sess_never_existed".to_string()),
        )
        .await;
        let err = r.expect_err("不存在的会话必须 404");
        assert_eq!(err.into_response().status(), StatusCode::NOT_FOUND);
    }
}
