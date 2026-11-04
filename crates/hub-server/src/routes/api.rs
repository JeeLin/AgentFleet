//! `/api/*`：页面侧第一批要读的 JSON（设计规范 §6）。
//!
//! **认证不在这一层**——它由 [`crate::middleware_auth`] 统一做，而那条中间件
//! 挂在 `build_app` 里 `.layer(...)` 的位置**只覆盖它之前添加的路由**
//! （见 `build_app` 的注释）。所以这些路由必须注册在 `.layer(...)` 之前；
//! `tests/api.rs` 的 `api_routes_are_covered_by_the_auth_layer` 盯住这条。
//!
//! 三条口径：
//!
//! - **错误体只有固定 code**（`{"error": "<code>"}`，约束 13）：不夹带库路径、
//!   SQL 报错串，更不夹带任何 secret。
//! - **`status` 三态**（`online` / `reconnecting` / `offline`）**只在这里派生**：
//!   `reconnecting` 是「WS 断了但心跳还新鲜」这一瞬时态，落库只有两值
//!   （schema 的 CHECK 约束），而页面要三态（设计规范 §6.1）。
//! - **工作区是会话的字段，不是可获取对象**（设计规范 §7.0）：会话在创建期
//!   选定路径后锁定，所以没有 `GET /api/workspaces`，也没有路径可改。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{err, ApiError, AppState};

/// 会话当前归属的登录身份（单用户；subject 恒为 `admin`）。
///
/// 这不是多用户设施：第一版是自托管单用户（约束 13）。它存在是为了让
/// 「这个操作是谁发起的」有一个落点——将来要加第二个人时，「不属于你 → 403」
/// 才有地方判断，而不必那时再回头翻每个 handler。
///
/// **验证已经做完了**：只有认证中间件验签通过后才会构造它
/// （`middleware_auth` 里紧跟着验签的那次 `insert`）。
#[derive(Debug, Clone)]
pub struct Authenticated {
    subject: String,
}

impl Authenticated {
    /// 发起者（`SessionStore` 签发时的 subject）。
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// 由认证中间件在验签通过后构造。
    pub fn new(subject: String) -> Self {
        Self { subject }
    }
}

/// 设备状态。**这三态是页面契约**（设计规范 §6.1）：
/// `online` / `reconnecting` / `offline`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    Online,
    /// WS 断了但心跳未超时（`hub_core::Presence::Reconnecting`）。
    Reconnecting,
    Offline,
}

impl DeviceStatus {
    /// 这台设备现在能不能接一个**新会话**。
    ///
    /// 接 `Online | Reconnecting`：`Reconnecting` 是 Hub 到设备的 WS 断了
    /// （`hub_core::Presence::Reconnecting`），不是设备进程死了——它的心跳还在
    /// 45 秒窗口里（`hub_core::OFFLINE_AFTER`），而工作区路径是 Hub 侧校验的。
    /// 所以放行的后果只是「会话行落库、等设备重连回来再打开」，而不是一次
    /// 静默失败。
    ///
    /// 更要紧的是**页面已经这么承诺了**：`api.ts::canCreateSession` 是
    /// `status !== 'offline'`，`NewSessionPage.vue` 的正文里写着「「重连中」仍可
    /// 建会话」。用户被告知「能建」，点下去却拿到 409 `device_offline`，两句
    /// 自相矛盾的话会同时出现在屏幕上。
    ///
    /// 只收这两态、**不收 `Offline`**：心跳超时 = 没有可信证据说设备还活着，
    /// 409 才能让页面显示「这台设备现在连不上」。
    ///
    /// 写成函数而不是内联 `matches!`：这条判据就是与页面契约对齐的那一处，
    /// 只应有一个定义点。
    fn accepts_new_session(self) -> bool {
        matches!(self, Self::Online | Self::Reconnecting)
    }
}

/// `GET /api/devices` 的一项（设计规范 §6.1）。
///
/// **字段名逐字取自 §6.1 的 mock**：页面 mock 形状就是将来 API 的形状。
/// `derived` 标 `true` 当且仅当 `status` 不是 `online`——而库里只有
/// online/offline 两值，所以「重连中」永远是 Hub 在内存里推出来的，
/// 页面上要给个「推」徽标（§6.1）。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceSummary {
    /// node_id。永不变（升级只换 worker，约束 12）。
    pub id: String,
    pub name: String,
    /// `os/arch` 拼成的短串（`darwin/arm64`）。上报前取空串。
    pub platform: String,
    /// afcli 的版本号（**不是** AI agent 的：约束 2 里 `agent` 一词只指 AI
    /// agent，设备侧进程一律叫 node / afcli）。字段名跟着协议走
    /// （`heartbeat.node_version`、`security-and-lifecycle.md` §7.1.4），
    /// 曾叫 `agent_version` ——那个名字违反约束 2，已改。
    pub node_version: String,
    pub status: DeviceStatus,
    /// 时间戳原文（`unix:<秒>`）。**不是人读格式**：§6.1 说 API 给可比值，
    /// 「12s」那种相对时间由前端按当前时刻渲染——服务端给的相对时间在
    /// 跨过零点时会自己过期。
    pub last_seen: Option<String>,
    /// 0–100，来自心跳能力上报；没报过就是 `None`（不是 0——0 会被页面
    /// 画成「空转」）。
    pub cpu: Option<u64>,
    pub mem: Option<u64>,
    pub gpu: bool,
    /// **已装**的 agent 名（含装了但跑不起来的）。目录页要能显示后者——
    /// 那正是用户需要看见并处理的状态。
    pub agents: Vec<String>,
    /// **已装且现在能对话**的 agent 名（建会话的可选项，子任务 5）。
    ///
    /// 与 [`Self::agents`] 分开是必须的：合成一个字段的话，页面要么列不出
    /// 「装了但坏了」（用户不知道为什么建会话时它不可选），要么把坏的那个
    /// 列进可选项（创建会话后永远无法对话）。
    ///
    /// 判据与 `POST /api/sessions` 的 409 门禁**同源**（都读 `agent_runtime`，
    /// 都只看 `available`），所以页面上选得到的一定建得成。
    ///
    /// 「同源」现在是可点开的：这一档由 `AgentRuntimeRepo::usable_list` 算（SQL 的
    /// `available = 1`），门禁是旁边的 `usable_for_session`（同一条谓词）。**别在
    /// `device_summary` 里再判一遍 `r.available`**——那等于把判据写成两份。
    pub usable_agents: Vec<String>,
    pub derived: bool,
    /// 设备密钥指纹，**已按「前 3 + `***` + 后 3」掩过**（约束 13）。
    pub key_fingerprint: String,
}

/// 心跳 `resources` 里 CPU 百分比的候选字段名。
const CPU_PERCENT_FIELDS: &[&str] = &["cpu", "cpu_percent", "load"];

/// 心跳 `resources` 里内存百分比的候选字段名。
const MEM_PERCENT_FIELDS: &[&str] = &["mem", "mem_percent", "memory"];

/// 心跳 `resources` 里 GPU 标志的候选字段名。
const GPU_FIELDS: &[&str] = &["gpu", "has_gpu"];

/// 从心跳上报里取一个负载百分比。
///
/// **取不到就是 `None`，不回落成 0**：路由把「未上报」当成「占满了」，
/// 会把一台刚连上的设备判成该关机（M0 的 `resources` 恒 `null`，
/// 见 `node_core::heartbeat::heartbeat`）。
fn load_percent(resources: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    let obj = resources.as_object()?;
    for k in keys {
        if let Some(n) = obj.get(*k).and_then(serde_json::Value::as_u64) {
            return Some(n.min(100));
        }
    }
    None
}

/// GPU 标志：布尔 `true`，或一个非空字符串（`"nvidia"` 这种运行时名）。
fn has_gpu(resources: &serde_json::Value) -> bool {
    let Some(obj) = resources.as_object() else {
        return false;
    };
    GPU_FIELDS.iter().any(|k| match obj.get(*k) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        _ => false,
    })
}

/// `capabilities.agents` → 去重且排序的名字列表。
///
/// **去重 + 排序**：同一台设备多次心跳的同一批 agent 必须给出同一份输出，
/// 否则页面每刷新一次顺序就变一次（`BTreeSet` 同时管住了两件事）。
///
/// # 两种形态都要认（M2）
///
/// M1 的 `agents` 是字符串数组 `["opencode"]`；M2 起 node 上报的是对象
/// `[{"id","name","version","available","error"}]`（见
/// `node-core/src/agent/cache.rs` 的 `AgentReport`），因为字符串装不下
/// 「版本 / 可用 / 失败原因」，而子任务 5 的 409 门禁判的是「已装**且**可用」。
///
/// **这里必须两种都认**：只认字符串的话，新形态会让页面上 agent
/// **静默消失**——不报错、不警告，只是列表空了。反过来只认对象则会让
/// 升级窗口里还在发旧帧的设备同样空掉。两种都认，过渡期两边都在。
///
/// 认不出的成员仍然跳过：那是坏数据，宁可少列一个。
fn agent_list(caps: &serde_json::Value) -> Vec<String> {
    let Some(arr) = caps.get("agents").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            // 旧形态：裸字符串
            if let Some(name) = v.as_str() {
                return Some(name.to_string());
            }
            // 新形态：对象，取 `name`（缺省回落到 `id`）
            let o = v.as_object()?;
            o.get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| o.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `capabilities.agents` 里**现在可用**的 agent 名。
///
/// # 它不用于建会话的门禁（子任务 5）
///
/// 门禁读的是 `agent_runtime` 表，不是这里的心跳内存。原因：`Reports` 是
/// 「缺省即保留」且没有 `forget`，一次不带 `capabilities` 的心跳会让旧清单
/// 永久粘在那里，于是「设备卸载了 agent」传不到 Hub。判据在
/// `hub_storage::AgentRuntimeRepo::usable_for_session`，那条带 `detected_at`
/// 可判新鲜度。
///
/// 保留本函数是为了回答「**这一次心跳**说哪些 agent 可用」——它只描述那一帧，
/// 不是当前事实。页面目录用 [`agent_list`]（列全部已装，含跑不起来的）。
///
/// # 旧形态（裸字符串）不算可用
///
/// 与 `persist_agents` 的 `parse_agent_report` 同一口径：只说名字的帧不构成
/// 「可用」的证据，门禁宁严勿松。两处必须保持一致——否则目录页说它可用、
/// 点创建却 409，而 409 的错误码还被翻成「这台设备上跑不起来」。
fn available_agent_names(caps: &serde_json::Value) -> Vec<String> {
    let Some(arr) = caps.get("agents").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            let o = v.as_object()?;
            if o.get("available").and_then(serde_json::Value::as_bool) != Some(true) {
                return None;
            }
            o.get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| o.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 从心跳 `capabilities.agents` 里读出该帧「已装」与「当前可用」两份名单。
///
/// 库读失败时的两处回退点（`device_summary` 与 `list_agents`）共用这一份，
/// 避免把「读库失败→回退心跳」的同一段逻辑写两遍。`NotInstalled` 的 agent
/// 压根不出现在心跳里（只被探测到的才会上报），所以两份名单都不会把它
/// 列入：`agent_list` 只收上报的名字，`available_agent_names` 再按
/// `available == true` 过滤，二者在回退口径上保持一致。
fn agent_names_from_heartbeat(caps: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    (agent_list(caps), available_agent_names(caps))
}

/// `os/arch` 拼成的 `darwin/arm64` 短串。任一缺失就是空串——
/// 上报前填一个「未知/未知」比空串更难让人判断「是没上报还是没这字段」。
fn platform_of(os: Option<&str>, arch: Option<&str>) -> String {
    match (os, arch) {
        (Some(o), Some(a)) => format!("{o}/{a}"),
        _ => String::new(),
    }
}

/// 设备行 + 库里的 agent 事实 → 一项 `DeviceSummary`。
///
/// # `agents` 读 `agent_runtime` 表，不读心跳内存
///
/// 两个理由：
///
/// 1. **内存那份会结构性粘滞**：`hub_core::Reports` 的 `merge_report` 是
///    「缺省即保留」，且没有 `forget`。一次不带 `capabilities` 的心跳之后，
///    设备上已经卸载的 agent 仍会显示在页面上。
/// 2. **库里有 `detected_at`**：探测事实有新鲜度，页面才能区分「刚探的」
///    与「很久以前的事实」。内存 `Report` 的 `at` 是心跳时刻，不是探测时刻。
///
/// 读库失败时**退回心跳内存**而不是给一个空列表：空列表会被读成「这台设备
/// 一个 agent 都没有」，而那与「库读不出来」是两件事。留痕由调用方看不到，
/// 所以这里直接 `tracing::warn`。
pub(crate) fn device_summary(
    state: &AppState,
    device: &hub_storage::repo::Device,
) -> DeviceSummary {
    let now = Instant::now();
    let connected = state.connections().get(&device.id).is_some();
    let presence = hub_core::presence(state.reports(), &device.id, connected, now);
    let report = state.reports().get(&device.id);
    let status = match presence {
        hub_core::Presence::Online => DeviceStatus::Online,
        hub_core::Presence::Reconnecting => DeviceStatus::Reconnecting,
        hub_core::Presence::Offline => DeviceStatus::Offline,
    };
    // 库里的版本号优先（那是登记时的快照），库里没有才用心跳上报的。
    let node_version = match (device.node_version.as_deref(), report.as_ref()) {
        (Some(v), _) => v.to_string(),
        (None, Some(r)) => r.node_version.clone(),
        (None, None) => String::new(),
    };
    // 列全部已装的（含跑不起来的）：目录页要能显示「装了但坏了」，
    // 那正是用户需要看见的东西。门禁另用 `usable_agents`。
    //
    // 「可用」那一档**不在这里再判一遍 `available`**：两次查询读的是同一张表、
    // 同一个 `available = 1` 谓词，判据只能有一份——`AgentRuntimeRepo::usable_list`，
    // 与建会话 / 开对话用的 `usable_for_session` 并排放着。在 Rust 里重写一遍，
    // 就又把「页面上选得到、点创建 409」变成可能。
    let (agents, usable_agents) = match (
        state.agent_runtime().list_for_node(&device.id),
        state.agent_runtime().usable_list(&device.id),
    ) {
        (Ok(rows), Ok(usable_rows)) => (
            rows.iter().map(|r| r.name.clone()).collect(),
            usable_rows.iter().map(|r| r.name.clone()).collect(),
        ),
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(
                node_id = %device.id, error = %e,
                "cannot read agent_runtime; falling back to the heartbeat report"
            );
            let caps = report.as_ref().map(|r| r.capabilities.clone());
            match caps {
                Some(c) => agent_names_from_heartbeat(&c),
                None => (Vec::new(), Vec::new()),
            }
        }
    };
    DeviceSummary {
        id: device.id.clone(),
        name: device.name.clone(),
        platform: platform_of(device.os.as_deref(), device.arch.as_deref()),
        node_version,
        status,
        last_seen: device.last_seen_at.clone(),
        cpu: report
            .as_ref()
            .and_then(|r| load_percent(&r.resources, CPU_PERCENT_FIELDS)),
        mem: report
            .as_ref()
            .and_then(|r| load_percent(&r.resources, MEM_PERCENT_FIELDS)),
        gpu: report.as_ref().is_some_and(|r| has_gpu(&r.resources)),
        agents,
        usable_agents,
        derived: !matches!(presence, hub_core::Presence::Online),
        key_fingerprint: crate::routes::masked_fingerprint(&device.key_fingerprint),
    }
}

/// `GET /api/devices`。
pub async fn list_devices(
    State(state): State<AppState>,
) -> Result<Json<Vec<DeviceSummary>>, ApiError> {
    let devices = state
        .devices()
        .list()
        .map_err(|e| ApiError::internal_with("cannot list devices", e))?;
    Ok(Json(
        devices.iter().map(|d| device_summary(&state, d)).collect(),
    ))
}

/// `DELETE /api/devices/{id}`：移除一台设备。
///
/// # 它**不删**会话与轨迹
///
/// 用户 2026-10-26 裁定的「删除语义三分离」：卸载 agent 不删会话、删设备不删
/// 会话、有会话的工作区拒删。前两条的共同理由是**对话是用户的数据，设备只是
/// 一条通路**——而这个动作在页面上看起来只是「移除一台机器」。工作区与会话
/// 留一行 `removed_node_id`，页面据此显示「已移除的设备」而不是一片空白。
///
/// 见 `hub_storage::DeviceRepo::delete_and_mark_removed`。
///
/// # 在线设备也允许删
///
/// 不给这条加「先断开」的前置条件：约束里没有「必须先卸载」这一步，而要求用户
/// 先去别处断开再回来删，是把内部实现顺序暴露成操作流程。连接本身由 `ws` 层
/// 在设备重连时按「库里有这行吗」处理——那一步已有（`auth_fail`）。
///
/// # 404 还是 204
///
/// 不存在就 404，与其它端点同口径（`:not_found` 一个码走全路由）。设备**真的
/// 存在过**是这条请求唯一的前置事实，它不成立时「删掉了」这句话是假的。
pub async fn delete_device(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    // **有历史执行记录就不许删**（`PRODUCT.md` §6.1，v0.7.0 子任务 4）。
    //
    // 执行记录是历史，而设备是它的归属。删掉设备之后那些记录还在库里（存储层
    // 刻意只删 `device` 行、保留 `workspace` / `session` / `trajectory_event`），
    // 但它们会指向一台已不存在的机器——页面上既说不清这条记录跑在哪，也没有
    // 任何入口能把它转走。所以正确的下一步是「先归档项目或把项目转到别的设备」。
    //
    // **带条数**：只说「不能删」用户判断不了该做什么，说「有 12 条历史执行」
    // 才让人知道这不是一个待会儿再试的问题。
    let executions = hub_storage::binding::BindingRepo::new(state.pool().clone())
        .execution_count_for_device(&id)
        .map_err(|e| ApiError::internal_with("cannot count executions", e))?;
    if executions > 0 {
        return Err(ApiError::conflict_with(
            err::DEVICE_HAS_EXECUTIONS,
            || serde_json::json!({ "executions": executions }),
        ));
    }

    let n = state
        .devices()
        .delete_and_mark_removed(&id)
        .map_err(|e| ApiError::internal_with("cannot delete the device", e))?;
    if n == 0 {
        return Err(ApiError::not_found());
    }
    // 通道账本是**内存**的、断连即清（`hub_core::channels` 的 `drop_node`）。
    // 在线时删掉设备，那些记录还挂在一个已经不存在的 node_id 上——不释放的话
    // 它们要等到下一次断连才走，而那时进来的可能是**另一台复用同一 node_id
    // 的机器**，它会继承别人的通道信用。
    //
    // **两份账本都要清**，与 `ws.rs` 断开处理器同一张清单：漏掉 `bindings`
    // 那份的症状是「`channel_id` 仍指向一台已不存在的设备」，于是
    // `close_channel` 会对一条设备侧早已不存在的通道走完归属校验。
    state.channels().write().drop_node(&id);
    state.bindings().drop_node(&id);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/sessions` 的请求体。
///
/// `agent_id` **必填**（见 [`create_session`] 的门禁）；`Option` 只为让「键缺失」
/// 与「`null`」走到我们自己的 400（`err::AGENT_REQUIRED`），而不是 serde 的
/// 一句 `missing field` 加 422。
///
/// **没有 connector 字段**：connector 是开面板时才指定的（`POST /api/channels`
/// 的 `connector`），所以「设备能力是否覆盖所需 connector」这道门禁**只能**
/// 落在开通道那一刻——见 [`crate::routes::stream::open_channel`] 的错误口径。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSession {
    pub node_id: String,
    pub path: String,
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// 会话摘要。`GET /api/sessions` 的一项与 `POST` 的响应共用同一个形状——
/// 前端建完会话直接把它塞进列表，不必再 GET 一次。
///
/// `workspace` 是**会话的字段而不是可获取对象**（设计规范 §6.2 / §7.0）：
/// 它只在读会话时出现，没有单独的 URL、没有生命周期，路径创建后只读
/// （`path_locked` 恒 `true`——改路径就等于让一个会话变成多资源句柄，
/// 约束 3 排除的设备级管理台会从这里长出来）。
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub agent_id: Option<String>,
    pub state: hub_storage::SessionState,
    pub created_at: String,
    pub ended_at: Option<String>,
    /// 会话建立那一刻工作区的 HEAD（完整 40 位）。`null` = 没有起点，
    /// 原因看 `base_rev_status`。
    ///
    /// **明确不加 `skip_serializing_if`**：加了的话取不到起点时这个键会整个
    /// 消失，于是「老 Hub + 新页面」（键不在）与「新 Hub 正常返回无起点」
    /// （键在、值为 null）在 TypeScript 里长得一样。键恒在。
    pub base_rev: Option<String>,
    /// 起点为什么有／没有（五态），取值语义见
    /// `docs/architecture/session-and-trajectory.md` §6.1。
    ///
    /// 不是 `Option`，所以本来就恒在。
    pub base_rev_status: hub_storage::BaseRevStatus,
    /// 这个会话属于哪一次执行（`execution.id`）。
    ///
    /// `null` 有两个来源，页面必须能区分它们的**后果**而不是原因：
    /// 一是 v0.6.0 时代的老会话（永远不会有执行），二是还没有人把它挂上去。
    /// 两者的可见性后果相同——不进「项目 → 执行」视图、不显示执行面板，
    /// 页面提示「此会话早于项目主轴」。用 `Option` 而不是空串，因为空串会让
    /// 前端 `if (executionId)` 恰好也对，但它们表达的不是同一件事。
    ///
    /// **同样不加 `skip_serializing_if`**，理由与 `base_rev` 相同：键恒在。
    pub execution_id: Option<String>,
    pub workspace: WorkspaceRef,
}

/// 会话的工作区引用（设计规范 §6.2 的形状 + 归属节点）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceRef {
    /// 归属的 node_id。**通道键是 `(node_id, channel_id)`**（约束 4），
    /// 页面拿这个引用就够定位，不必自己去 join 设备表。
    ///
    /// `null` = 这台设备已被删除（v3 去掉外键后工作区行保留、`node_id` 置 NULL）。
    /// 此时页面**不能**再拿它去开通道，也不该把它当「离线」等——那是另一种状态
    /// （`err::DEVICE_REMOVED` 与 `err::OFFLINE` 是两个 code）。
    pub node_id: Option<String>,
    /// 设备被删前的原 node_id；设备还在时 `null`。仅供页面显示「已移除的设备」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_node_id: Option<String>,
    pub path: String,
    pub path_locked: bool,
}

impl WorkspaceRef {
    fn new(node_id: &Option<String>, removed_node_id: &Option<String>, path: &str) -> Self {
        Self {
            node_id: node_id.clone(),
            removed_node_id: removed_node_id.clone(),
            path: path.to_string(),
            path_locked: true,
        }
    }
}

/// `SessionListItem` → [`SessionSummary`]。
fn summary_of(item: &hub_storage::SessionListItem) -> SessionSummary {
    SessionSummary {
        id: item.session.id.clone(),
        agent_id: item.session.agent_id.clone(),
        state: item.session.state,
        created_at: item.session.created_at.clone(),
        ended_at: item.session.ended_at.clone(),
        base_rev: item.session.base_rev.clone(),
        base_rev_status: item.session.base_rev_status,
        execution_id: item.session.execution_id.clone(),
        workspace: WorkspaceRef::new(&item.node_id, &item.removed_node_id, &item.path),
    }
}

/// `GET /api/sessions`：全部工作台会话（新的在前）。
pub async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<Vec<SessionSummary>>, ApiError> {
    let items = hub_storage::SessionRepo::new(state.devices().pool().clone())
        .list()
        .map_err(|e| ApiError::internal_with("cannot list sessions", e))?;
    Ok(Json(items.iter().map(summary_of).collect()))
}

/// `POST /api/sessions`：建会话（设备 + 工作区路径 + agent）。
///
/// 后端流程：
/// 1. `agent_id` 必填——没有就 400。见下面那段「为什么必填」。
/// 2. 校验工作区路径（绝对或 `~` 开头）。**越界路径在此就拒**，
///    不给「先建会话、打开面板时报错」留余地（设计规范 §7.4 要求越界可见）。
/// 3. 设备是否**能接新会话**——只有 `offline` 回 **409 并带设备状态**，让页面能显示
///    「这台设备现在连不上」而不是一个没有主语的成功。
///    **`reconnecting` 放行**（`DeviceStatus::accepts_new_session`）：那是 Hub 到设备的
///    WS 断了，不是设备死了——心跳还在 45 秒窗口里，工作区路径是 Hub 侧校验的，
///    而页面（`api.ts::canCreateSession`）本来就把「重连中」算作可用、`NewSessionPage.vue`
///    的正文也这么对用户说。用户被告知能建，Hub 就必须真能建。
/// 4. `SessionRepo::create` 在**一个事务**里 upsert 工作区 + 插会话行
///    （工作区是主实体，多个会话引用同一行；见 M1 §5c）。
pub async fn create_session(
    State(state): State<AppState>,
    Extension(auth): Extension<Authenticated>,
    req: axum::extract::Request,
) -> Result<Response, ApiError> {
    // body 自己读而不是用 `Json` 提取器：axum 对形状不对的 JSON 回的是
    // **422** 与一条 `text/plain` 说明（裸的 `JsonRejection` 直接落进响应），
    // 而 `/api/*` 的错误体形状是 `{"error": "<code>"}`（前端 `http.ts` 按它
    // 做错误映射）。统一成自己解，形状才有一处定义。
    let body = crate::routes::read_body(req).await?;
    let body: CreateSession = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => return Err(ApiError::bad_request(err::BAD_BODY)),
    };

    // **agent 是会话的必要前提，所以缺了是 400。** 一个 `agent_id` 为 NULL 的
    // 会话永远开不了对话（`conversation::open` 直接 409，`ConvFlow` 的 `canChat`
    // 只看 `agent_id`），也就是说 201 给出去的会是一个看起来建成、实际永远哑的
    // 会话。空串与缺失同罪——空串不是 agent。
    //
    // 400 而不是 409：请求本身就不完整，与设备状态无关，改一个字就能过。
    //
    // 放在路径校验**之前**：这是请求形状的门禁，形状不对时路径对不对没有意义。
    let agent_id = match body.agent_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => return Err(ApiError::bad_request(err::AGENT_REQUIRED)),
    };
    let agent_id = agent_id.as_str();
    hub_storage::WorkspaceRepo::validate_path(&body.path)
        .map_err(|_| ApiError::bad_request(err::INVALID_WORKSPACE_PATH))?;

    let device = state
        .devices()
        .find_by_id(&body.node_id)
        .map_err(|e| ApiError::internal_with("cannot read the device", e))?;
    // 设备不存在 → 404：与「会话不存在」同一个 code（见模块注释的口径表）
    let Some(device) = device else {
        return Err(ApiError::not_found());
    };

    let status = device_summary(&state, &device).status;
    if !status.accepts_new_session() {
        return Err(ApiError::conflict_with(
            err::OFFLINE,
            || json!({"status": status}),
        ));
    }

    // agent 可用门禁（要求 3）。**读 `agent_runtime` 表，不读内存 `Reports`**：
    // `Reports` 是「缺省即保留」且没有 `forget`，一次不带 `capabilities` 的心跳
    // 会把旧清单永久粘在那里，于是设备卸载 agent 之后 Hub 仍然放行。
    //
    // **对「从没上报过」必须拒**——这与 M1 的 connector 门禁（`stream.rs` 的
    // `unreported_connector`）正好相反，两处不一样是对的：connector 是可选能力，
    // 没上报过不代表不支持；agent 是会话的**必要**前提，放行的后果是用户拿到
    // 一个 201 的会话，等到开 ACP 时才发现二进制不存在——一个永远无法对话的
    // 死会话，而它看起来像「建成功了」。
    //
    // `agent_id` 走到这里必然非空（上面已经 400 挡掉了缺失与空串），所以这道门禁
    // 无条件执行——不再有「不选 agent 就跳过」的分支。
    let usable = state
        .agent_runtime()
        .usable_for_session(&body.node_id, agent_id)
        .map_err(|e| ApiError::internal_with("cannot read agent_runtime", e))?;
    if usable.is_none() {
        return Err(ApiError::conflict_with(
            err::AGENT_UNAVAILABLE,
            || json!({"agent_id": agent_id}),
        ));
    }

    let session = hub_storage::SessionRepo::new(state.devices().pool().clone())
        .create(
            &body.node_id,
            &body.path,
            Some(agent_id),
            &hub_core::registry::now_stamp(),
            // 起点由紧随其后的探测回填（见下面的 `probe_base_rev`）。
            // 初值用 `Unknown` 而不是随便一个态——它是「我还不知道」。
            None,
            hub_storage::BaseRevStatus::Unknown,
        )
        .map_err(|e| ApiError::internal_with("cannot create the session", e))?;

    // **先落行、再探测、再回填。** 顺序不能反：探测要走 `open_device_channel`，
    // 而它第一件事就是 `SessionRepo::detail(session_id)` 并查
    // `state == Running`（`stream.rs::open_device_channel`）。「先探测再插入」
    // 要求给 `ChannelBinding.session_id` 填一个尚无会话的占位值，而那个字段是
    // `POST /api/channels/{id}/*` 归属校验的依据——占位值会让这条通道的归属
    // 变成谎话，比多一次 `UPDATE` 坏得多。
    //
    // 代价是**一个几十毫秒的窗口**：另一个标签页若恰好在这段时间里刷新了会话
    // 列表，会看到 `base_rev_status == "unknown"`（「起点未记录」），刷新一次
    // 即正确。**它不会显示成错误的起点**——`unknown` 不是 `ok`。不加锁、
    // 不加额外的进度状态。
    let (base_rev, base_rev_status) =
        super::base_rev::probe_base_rev(&state, &auth, &session.id, &body.node_id).await;

    // 写库失败是一件**真错**（不是「取不到起点」）：会话行已在，但它的起点
    // 状态会永远停在 `unknown`——与「用户看到一个看起来成功、实际起点永远未知
    // 的会话」是同一件事。静默吞掉会让它变成「起点偶尔是未知的」，无从排查。
    hub_storage::SessionRepo::new(state.devices().pool().clone())
        .set_base_rev(&session.id, base_rev.as_deref(), base_rev_status)
        .map_err(|e| ApiError::internal_with("cannot record the session's base revision", e))?;

    let detail = SessionSummary {
        id: session.id.clone(),
        agent_id: session.agent_id.clone(),
        state: session.state,
        created_at: session.created_at.clone(),
        ended_at: session.ended_at.clone(),
        base_rev,
        base_rev_status,
        // 新建的会话还没有所属执行：执行由任务派发时创建，两者先后不固定。
        // 挂载走 `SessionRepo::set_execution_id`，不在建会话时定。
        execution_id: None,
        // 刚建出来的会话，设备必然存在（上面刚验过 `device.id`），
        // 所以这里 node_id 一定有值、removed 一定为 None。
        workspace: WorkspaceRef::new(&Some(device.id.clone()), &None, &body.path),
    };
    // coverage: excluded — 多行宏的参数行被 llvm-cov 记为 miss（已知现象），
    // 宏本身在上面的用例里执行过。
    tracing::info!(
        session_id = %session.id,
        node_id = %body.node_id,
        subject = %auth.subject(),
        created = true,
        "session created"
    );
    // 页面侧（子任务 5a）：会话建成的瞬间推给所有订阅者，好让另一个标签页
    // 的工作台 tab 条上立刻多出这一条，而不必等它下次轮询。
    state
        .streams()
        .session_state(&session.id, session.state.as_str());
    Ok((StatusCode::CREATED, Json(detail)).into_response())
}

/// `GET /api/agents`：Agent 目录（只读聚合，设计规范 §7.6）。
///
/// 数据源是 `agent_runtime` 表（M2 起，见函数体第一段注释为什么不是心跳内存）；
/// 只有读库失败时才退回到**设备心跳上报的 `capabilities.agents`**。一台设备都
/// 没有上报过、库里也没有行时列表是空的，那是对的——编一个「装了什么」等于把
/// 任务派给没有它的机器。
pub async fn list_agents(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentSummary>>, ApiError> {
    let devices = state
        .devices()
        .list()
        .map_err(|e| ApiError::internal_with("cannot list devices", e))?;
    // 读 `agent_runtime` 表而不是心跳内存：理由见 `device_summary` 的注释
    // （内存那份缺省即保留、没有 `forget`，卸载的 agent 永远留在里面）。
    //
    // **带 `available` 一起返回**：只给一堆名字的话，页面无从区分「装了且
    // 能对话」与「装了但没有 ACP 入口」（`Probe::NotCapable`），而后者的
    // 存在意义正是让用户看见「为什么我的 agent 不能对话」——不给这个字段，
    // 那条四态设计在页面上等于没有。
    //
    // 同名 agent 在**多台设备**上可能一真一假（这台能对话、那台装坏了）。
    // 按「有一台可用即显示可用」归并，并把各自的 `error` 拼起来：目录页是
    // 「这个 agent 存在吗、现在能不能用」的总览，不是某一台设备的清单。
    // 建会话那一档的判据不在这里——它走 `device_summary` 的 `usable_agents`。
    let mut out: BTreeMap<String, AgentSummary> = BTreeMap::new();
    for d in &devices {
        match state.agent_runtime().list_for_node(&d.id) {
            Ok(rows) => {
                for r in rows {
                    merge_agent(&mut out, &r.name, r.available, r.probe_error.as_deref());
                }
            }
            Err(e) => {
                // 退回心跳内存，但**留痕**：静默回落会让「库坏了」看起来像
                // 「页面上少了几个 agent」。
                tracing::warn!(node_id = %d.id, error = %e, "cannot read agent_runtime; using the heartbeat report");
                if let Some(r) = state.reports().get(&d.id) {
                    // 心跳内存那份没有「为什么不可用」，如实留空而不是编一句
                    let (all, usable) = agent_names_from_heartbeat(&r.capabilities);
                    for name in &all {
                        merge_agent(&mut out, name, usable.contains(name), None);
                    }
                }
            }
        }
    }
    Ok(Json(out.into_values().collect::<Vec<_>>()))
}

/// 把一台设备上的一条 agent 记录并进目录。
///
/// `available` 取**或**：只要有一台设备上它能对话，目录就标可用。
/// `error` 取**并**：把所有说它不可用的原因都留着——用户要据此判断
/// 「是我这台机器的问题吗」。
fn merge_agent(
    out: &mut BTreeMap<String, AgentSummary>,
    name: &str,
    available: bool,
    error: Option<&str>,
) {
    let entry = out.entry(name.to_string()).or_insert(AgentSummary {
        name: name.to_string(),
        available: false,
        error: None,
    });
    if available {
        entry.available = true;
        return;
    }
    let Some(reason) = error else { return };
    if reason.is_empty() {
        return;
    }
    match entry.error.as_mut() {
        // 已经标了可用就没有必要再挂原因：那是另一台设备的事
        Some(_) if entry.available => {}
        Some(existing) => {
            if !existing.contains(reason) {
                existing.push('；');
                existing.push_str(reason);
            }
        }
        None => entry.error = Some(reason.to_string()),
    }
}

/// Agent 目录的一项。
#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub name: String,
    /// 现在能不能对话。**至少一台设备上可用即为真**。
    pub available: bool,
    /// 说它不可用的原因。设备没给原因时为 `null`——不编一句。
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::auth::{AuthParams, LoginRateLimiter};
    use std::time::Duration;

    fn state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let st = AppState::bootstrap_with(
            pool,
            dir.path(),
            AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        (st, dir)
    }

    /// `agent_list` 必须**同时认两种形态**（M2 把 `agents` 从字符串改成了对象）。
    ///
    /// 判据不是「能解析」，而是**不会静默消失**：只认字符串时，新形态让页面
    /// 上 agent 列表变空——不报错、没有日志，用户只看到「这台设备没有 agent」。
    /// 所以这里两种形态各断言一次，且断言它们的顺序稳定（可 diff）。
    #[test]
    fn agent_list_accepts_both_the_legacy_string_and_the_object_form() {
        // 旧形态（M1 设备还在发这种帧）
        let legacy = serde_json::json!({ "agents": ["opencode", "claude"] });
        assert_eq!(
            agent_list(&legacy),
            vec!["claude".to_string(), "opencode".to_string()],
            "旧形态必须继续列出来"
        );

        // 新形态（M2）
        let modern = serde_json::json!({
            "agents": [
                {"id": "opencode", "name": "opencode", "version": "2.1.4", "available": true, "error": null},
                {"id": "broken", "name": "broken", "version": null, "available": false, "error": "退出码 3"},
            ]
        });
        assert_eq!(
            agent_list(&modern),
            vec!["broken".to_string(), "opencode".to_string()],
            "新形态必须列出来（含装了但跑不起来的），否则页面会静默变空"
        );

        // 两种形态混在一帧里（升级窗口）
        let mixed = serde_json::json!({ "agents": ["a", {"id": "b", "available": true}] });
        assert_eq!(agent_list(&mixed), vec!["a".to_string(), "b".to_string()]);

        // 坏数据跳过，不 panic
        let garbage = serde_json::json!({ "agents": [42, null, {"no_id": 1}] });
        assert!(agent_list(&garbage).is_empty(), "认不出的成员一律跳过");
        // 缺键 / 非数组
        assert!(agent_list(&serde_json::json!({})).is_empty());
        assert!(agent_list(&serde_json::json!({ "agents": "opencode" })).is_empty());
    }

    /// **可用性判据**：门禁只认 `available: true`，旧形态一律不算。
    ///
    /// 与 `agent_list` 分开断言，是因为二者服务于不同目的：目录页要列出
    /// 「装了但跑不起来」的 agent，而建会话的门禁只认「已装且可用」。合成
    /// 一个函数要么让目录页少东西，要么让门禁放行一个其实起不来的 agent。
    #[test]
    fn only_agents_marked_available_pass_the_gate() {
        let caps = serde_json::json!({
            "agents": [
                {"id": "opencode", "name": "opencode", "available": true},
                {"id": "broken", "name": "broken", "available": false},
                {"id": "noflag", "name": "noflag"},
            ]
        });
        assert_eq!(available_agent_names(&caps), vec!["opencode".to_string()]);

        // 旧形态不携带可用性证据 → 宁严勿松
        let legacy = serde_json::json!({ "agents": ["opencode"] });
        assert!(
            available_agent_names(&legacy).is_empty(),
            "只说名字的旧帧不能过门禁"
        );
    }

    /// 走产品自己的 enroll 入口建一台设备（不碰真 WS）：
    /// 单测要的是「有一台设备行」，而 `Registry::enroll` 是唯一会写出合法
    /// `key_fingerprint` 的入口——手工 INSERT 一个假指纹造出的设备在重连
    /// 那一步会表现成另一回事。
    fn insert_device(st: &AppState, label: &str) -> String {
        let kp = identity::KeyPair::generate();
        let outcome = st
            .registry()
            .enroll(
                &kp.public_key_bytes(),
                hub_storage::repo::NewDevice {
                    id: String::new(),
                    name: label.to_string(),
                    key_fingerprint: String::new(),
                    os: "linux".into(),
                    arch: "aarch64".into(),
                    hostname: label.into(),
                    node_version: "0.1.0".into(),
                },
            )
            .expect("enroll");
        outcome.node_id().to_string()
    }

    fn report_with(st: &AppState, node: &str, caps: serde_json::Value, res: serde_json::Value) {
        st.reports().record(
            node,
            hub_core::Report {
                node_version: "0.1.0".into(),
                capabilities: caps,
                resources: res,
                workspace_roots: Vec::new(),
                at: Instant::now(),
            },
        );
    }

    fn summary_of_device(st: &AppState, id: &str) -> DeviceSummary {
        let d = st.devices().find_by_id(id).unwrap().expect("device row");
        device_summary(st, &d)
    }

    /// 三态的取值就是页面契约，一个字都不能改（§6.1）。
    #[test]
    fn device_status_serialises_to_the_three_page_values() {
        let s = serde_json::to_value([
            DeviceStatus::Online,
            DeviceStatus::Reconnecting,
            DeviceStatus::Offline,
        ])
        .unwrap();
        assert_eq!(s, json!(["online", "reconnecting", "offline"]));
    }

    /// 三态的**判据**：上报新鲜但没有活连接 = reconnecting；没有上报 = offline。
    /// 「online」需要一条真 WS 连接，由 `tests/api.rs` 的真握手覆盖。
    #[test]
    fn presence_decides_the_device_status() {
        let (st, _d) = state();
        let fresh = insert_device(&st, "fresh");
        let silent = insert_device(&st, "silent");
        report_with(&st, &fresh, json!({}), json!({}));

        assert_eq!(
            summary_of_device(&st, &fresh).status,
            DeviceStatus::Reconnecting,
            "心跳新鲜但没有活连接 = 重连中"
        );
        assert_eq!(
            summary_of_device(&st, &silent).status,
            DeviceStatus::Offline,
            "从没上报过心跳 = 离线"
        );
    }

    /// `reconnecting` 标 `derived`——它只存在于 Hub 内存，页面要标个「推」。
    /// 三态里只有 `online` 不是推出来的（库里也只有 online/offline 两值）。
    #[test]
    fn a_status_derived_from_memory_is_marked_as_such() {
        let (st, _d) = state();
        let fresh = insert_device(&st, "fresh");
        report_with(&st, &fresh, json!({}), json!({}));
        assert!(summary_of_device(&st, &fresh).derived);
    }

    /// `agents` 是「已装且就绪」：去重、排序、只取字符串成员。
    #[test]
    fn agents_are_deduplicated_sorted_and_never_objects() {
        let caps = json!({"agents": ["opencode", 42, "claude-code", "opencode"]});
        assert_eq!(agent_list(&caps), vec!["claude-code", "opencode"]);
        assert!(agent_list(&json!({})).is_empty());
        assert!(agent_list(&json!({"agents": "claude-code"})).is_empty());
        assert!(agent_list(&json!({"agents": null})).is_empty());
    }

    /// 负载取不到就是 `None`——M0 的 `resources` 恒 `null`，
    /// 回落成 0 会让页面把一台刚连上的设备画成「空转 0%」。
    #[test]
    fn an_unreported_load_is_unknown_not_zero() {
        assert_eq!(load_percent(&json!({}), CPU_PERCENT_FIELDS), None);
        assert_eq!(load_percent(&json!(null), CPU_PERCENT_FIELDS), None);
        assert_eq!(load_percent(&json!("x"), CPU_PERCENT_FIELDS), None);
        assert_eq!(
            load_percent(&json!({"cpu": 34}), CPU_PERCENT_FIELDS),
            Some(34)
        );
        assert_eq!(
            load_percent(&json!({"memory": 61}), MEM_PERCENT_FIELDS),
            Some(61)
        );
        // 越界的值收敛到 100，而不是让页面画出一个不存在的百分比
        assert_eq!(
            load_percent(&json!({"cpu": 480}), CPU_PERCENT_FIELDS),
            Some(100)
        );
        assert_eq!(load_percent(&json!({"disk": 3}), CPU_PERCENT_FIELDS), None);
    }

    #[test]
    fn gpu_is_true_for_a_flag_or_a_named_runtime() {
        assert!(has_gpu(&json!({"gpu": true})));
        assert!(has_gpu(&json!({"gpu": "nvidia"})));
        assert!(!has_gpu(&json!({"gpu": false})));
        assert!(!has_gpu(&json!({"gpu": ""})));
        assert!(!has_gpu(&json!({"gpu": 8})));
        assert!(!has_gpu(&json!({})));
        assert!(!has_gpu(&json!(null)));
    }

    /// 平台短串：`os/arch` 缺一不可，缺了就是空串而不是编一个「未知」。
    #[test]
    fn platform_needs_both_halves() {
        assert_eq!(platform_of(Some("darwin"), Some("arm64")), "darwin/arm64");
        assert_eq!(platform_of(Some("darwin"), None), "");
        assert_eq!(platform_of(None, Some("arm64")), "");
        assert_eq!(platform_of(None, None), "");
    }

    /// 摘要里带的是掩过的指纹，不是库里那一串（约束 13）。
    #[test]
    fn a_device_summary_carries_a_masked_fingerprint_only() {
        let (st, _d) = state();
        let id = insert_device(&st, "pi");
        let stored = st
            .devices()
            .find_by_id(&id)
            .unwrap()
            .expect("row")
            .key_fingerprint;
        let s = summary_of_device(&st, &id);
        assert!(stored.len() > 6, "真指纹足够长");
        assert_eq!(s.key_fingerprint.len(), 9, "前 3 + *** + 后 3");
        assert!(s.key_fingerprint.contains("***"));
        assert!(!s.key_fingerprint.contains(&stored));
        // 设备行里的平台与版本进了摘要
        assert_eq!(s.platform, "linux/aarch64");
        assert_eq!(s.node_version, "0.1.0");
    }

    /// 版本回退：设备行里没有 `node_version` 时取内存上报的，
    /// 再没有就是空串（不是 `null`——前端的 `??` 兜底要的是字符串）。
    #[test]
    fn the_node_version_falls_back_to_the_latest_heartbeat() {
        let (st, _d) = state();
        let with_row = insert_device(&st, "row");
        let from_hb = insert_device(&st, "hb");
        st.pool()
            .get()
            .unwrap()
            .execute("UPDATE device SET node_version = NULL", [])
            .unwrap();
        report_with(&st, &from_hb, json!({}), json!(null));
        assert_eq!(summary_of_device(&st, &from_hb).node_version, "0.1.0");
        assert_eq!(summary_of_device(&st, &with_row).node_version, "");
    }

    /// 往一台设备上放几条 `agent_runtime` 行。
    fn detect(st: &AppState, node: &str, rows: &[(&str, bool, Option<&str>)]) {
        let owned: Vec<hub_storage::agent::DetectedRow> = rows
            .iter()
            .map(|(id, available, err)| {
                (
                    (*id).to_string(),
                    (*id).to_string(),
                    Some("2.1.4".to_string()),
                    *available,
                    err.map(str::to_string),
                )
            })
            .collect();
        st.agent_runtime()
            .replace_detected(node, &owned, "unix:1700000000")
            .expect("replace_detected");
    }

    async fn agent_dir(st: &AppState) -> Vec<AgentSummary> {
        list_agents(State(st.clone())).await.expect("list_agents").0
    }

    /// **「装了但不能用」必须连原因一起列出来**，而不是被筛掉。
    ///
    /// 这是 `Probe::NotCapable` 存在的唯一理由：让用户看见「为什么我的 agent
    /// 不能对话」。只给一堆名字，那条四态设计在页面上等于没有。
    #[tokio::test]
    async fn the_agent_directory_reports_which_agents_can_converse_and_why_not() {
        let (st, _dir) = state();
        let n = insert_device(&st, "mbp");
        detect(
            &st,
            &n,
            &[
                ("opencode", true, None),
                ("claude", false, Some("没有 acp 子命令")),
            ],
        );

        let got = agent_dir(&st).await;
        let opencode = got.iter().find(|a| a.name == "opencode").expect("opencode");
        assert!(opencode.available, "能对话的必须标可用");
        assert_eq!(opencode.error, None);

        let claude = got.iter().find(|a| a.name == "claude").expect("claude");
        // 列出来（不被筛掉），但标不可用，并带出原因
        assert!(!claude.available, "不能对话的不能标可用");
        assert_eq!(claude.error.as_deref(), Some("没有 acp 子命令"));
    }

    /// 同名 agent 在多台设备上**一真一假**：目录标可用（有一台能对话即可），
    /// 但原因要留着——用户要据此判断「是不是我这台机器的问题」。
    #[tokio::test]
    async fn one_device_being_broken_does_not_hide_that_another_can_converse() {
        let (st, _dir) = state();
        let a = insert_device(&st, "dev-a");
        let b = insert_device(&st, "dev-b");
        detect(&st, &a, &[("opencode", true, None)]);
        detect(&st, &b, &[("opencode", false, Some("退出码 3"))]);

        let got = agent_dir(&st).await;
        let e = got.iter().find(|x| x.name == "opencode").expect("opencode");
        assert!(e.available, "有一台能对话，目录就该标可用");
        assert_eq!(e.error.as_deref(), Some("退出码 3"), "另一台的坏原因要留着");
    }

    /// **反向验证**：把 `merge_agent` 的 `available` 取「或」改坏（永远写最后一台
    /// 的结果），这条会红——它证明归并口径真的被测到了，而不是「恰好通过」。
    #[tokio::test]
    async fn availability_is_an_or_across_devices_not_last_write_wins() {
        let (st, _dir) = state();
        let a = insert_device(&st, "dev-a");
        let b = insert_device(&st, "dev-b");
        // 顺序上「坏的那台在后」：若实现是「最后写的覆盖」，可用会被抹掉
        detect(&st, &a, &[("opencode", true, None)]);
        detect(&st, &b, &[("opencode", false, Some("退出码 3"))]);

        let got = agent_dir(&st).await;
        assert!(
            got.iter().any(|x| x.name == "opencode" && x.available),
            "后一台说它不可用，也不能抹掉前一台的可用（判据是「或」）"
        );
    }

    /// `Last-Event-ID` 头：畸形 / 缺失 / 正常三种。
    #[test]
    fn last_event_id_is_parsed_or_ignored() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(crate::routes::last_event_id(&h), None);
        h.insert("last-event-id", "42".parse().unwrap());
        assert_eq!(crate::routes::last_event_id(&h), Some(42));
        h.insert("last-event-id", "not-a-number".parse().unwrap());
        assert_eq!(crate::routes::last_event_id(&h), None);
    }

    // ---- 心跳回退口径：装了 ≠ 能对话 ----

    /// `available_agent_names` 只收 `available == true` 的，且**旧形态一律不算**。
    ///
    /// 与 `parse_agent_report` 同一口径：一个只说名字的帧不构成「可用」的
    /// 证据。两处若漂移，目录页会说它可用，而用户点创建拿到 409「这台设备上
    /// 跑不起来」——同一件事两套说法。
    #[test]
    fn available_agent_names_requires_an_explicit_true() {
        let caps = serde_json::json!({
            "agents": [
                {"id": "opencode", "name": "opencode", "available": true},
                {"id": "claude", "name": "claude", "available": false},
                {"id": "nokey", "name": "nokey"},
                {"id": "str-avail", "name": "str-avail", "available": "yes"},
                {"id": "idonly", "available": true},
                "legacy-bare-string",
                42,
            ]
        });
        assert_eq!(
            available_agent_names(&caps),
            vec!["idonly".to_string(), "opencode".to_string()],
            "只有显式 true 的算可用；缺 name 时退回 id"
        );
    }

    /// 没有 `agents` 字段 / 不是数组 → 空，而不是 panic 或「全可用」。
    #[test]
    fn available_agent_names_is_empty_when_the_field_is_absent_or_wrong() {
        for caps in [
            serde_json::json!({}),
            serde_json::json!({"agents": "opencode"}),
            serde_json::json!({"agents": null}),
            serde_json::json!({"agents": []}),
        ] {
            assert!(
                available_agent_names(&caps).is_empty(),
                "{caps} 不该给出任何可用名字"
            );
        }
    }

    /// 回退口径的两份名单必须**自洽**：可用集合是已装集合的子集。
    ///
    /// 读库失败时页面用这份回退渲染，两份名单若不自洽，会出现「列出了
    /// 一个不在可用列表里的 agent，且没有任何原因说明」。
    #[test]
    fn the_heartbeat_fallback_keeps_usable_within_all() {
        let caps = serde_json::json!({
            "agents": [
                {"id": "opencode", "name": "opencode", "available": true},
                {"id": "claude", "name": "claude", "available": false},
            ]
        });
        let (all, usable) = agent_names_from_heartbeat(&caps);
        assert_eq!(all, vec!["claude".to_string(), "opencode".to_string()]);
        assert_eq!(usable, vec!["opencode".to_string()]);
        assert!(
            usable.iter().all(|n| all.contains(n)),
            "可用的必须是已装的子集：all={all:?} usable={usable:?}"
        );
    }

    /// `merge_agent`：`available` 取**或**、`error` 取**并**且去重。
    ///
    /// 这两条方向相反是有意的——「有一台能用」就是能用，而「为什么不能用」
    /// 要全留着，否则用户换一台设备测就看不到另一台上那条原因。
    #[test]
    fn merge_agent_unions_availability_and_collects_reasons() {
        let mut out = BTreeMap::new();
        merge_agent(&mut out, "opencode", false, Some("退出码 3"));
        merge_agent(&mut out, "opencode", false, Some("缺少 acp 入口"));
        let one = out.get("opencode").expect("一条");
        assert!(one.error.as_deref().unwrap_or("").contains("退出码 3"));
        assert!(
            one.error.as_deref().unwrap_or("").contains("缺少 acp 入口"),
            "两条原因都要留下：{:?}",
            one.error
        );

        // 另一台设备说它可用 → 整体可用，且重复原因不再重复追加。
        let before = out.get("opencode").and_then(|e| e.error.clone());
        merge_agent(&mut out, "opencode", true, Some("退出码 3"));
        let one = out.get("opencode").expect("一条");
        assert!(one.available, "有一台能用就是能用");
        assert_eq!(one.error, before, "标了可用之后不再追加原因");
    }

    /// `merge_agent` 的空原因与缺原因**都不该**制造一条错误说明。
    #[test]
    fn merge_agent_ignores_a_missing_or_empty_reason() {
        let mut out = BTreeMap::new();
        merge_agent(&mut out, "a", false, None);
        merge_agent(&mut out, "b", false, Some(""));
        assert_eq!(out.get("a").and_then(|e| e.error.clone()), None);
        assert_eq!(out.get("b").and_then(|e| e.error.clone()), None);
        assert!(!out.get("a").expect("a").available);
    }

    /// 已经标成可用之后，**不再往它身上挂原因**。
    ///
    /// 顺序在这里是有意义的：先有一台说「不可用 + 原因」，再来一台说「可用」
    /// 就整体可用了；此后又有一台报错，也不能把那条原因挂到一条已经可用的
    /// 记录上——那会让页面显示「可用，但：退出码 3」，用户无从判断该信哪个。
    #[test]
    fn merge_agent_stops_collecting_reasons_once_it_is_available() {
        let mut out = BTreeMap::new();
        merge_agent(&mut out, "opencode", false, Some("退出码 3"));
        merge_agent(&mut out, "opencode", true, None);
        let err_when_usable = out.get("opencode").and_then(|e| e.error.clone());
        assert!(out.get("opencode").expect("一条").available);
        assert_eq!(err_when_usable.as_deref(), Some("退出码 3"));

        // 第三台设备也报错：原因不该再追加。
        merge_agent(&mut out, "opencode", false, Some("缺少 acp 入口"));
        assert_eq!(
            out.get("opencode").and_then(|e| e.error.clone()),
            err_when_usable,
            "已经可用的记录不再追加原因"
        );
    }
}
