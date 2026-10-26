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

use std::collections::BTreeSet;
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
    /// **已装且就绪**，不是「支持」（§6.1 最后一段）。
    pub agents: Vec<String>,
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

/// `capabilities.agents` → 去重且排序的字符串列表。
///
/// **去重 + 排序**：同一台设备多次心跳的同一批 agent 必须给出同一份输出，
/// 否则页面每刷新一次顺序就变一次（`BTreeSet` 同时管住了两件事）。
/// 非字符串成员一律跳过：那是坏数据，宁可少列一个也不把一个对象
/// 当成 agent 名显示出去。
fn agent_list(caps: &serde_json::Value) -> Vec<String> {
    let Some(arr) = caps.get("agents").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `os/arch` 拼成的 `darwin/arm64` 短串。任一缺失就是空串——
/// 上报前填一个「未知/未知」比空串更难让人判断「是没上报还是没这字段」。
fn platform_of(os: Option<&str>, arch: Option<&str>) -> String {
    match (os, arch) {
        (Some(o), Some(a)) => format!("{o}/{a}"),
        _ => String::new(),
    }
}

/// 设备行 + 内存上报 → 一项 `DeviceSummary`。
fn device_summary(state: &AppState, device: &hub_storage::repo::Device) -> DeviceSummary {
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
        agents: report
            .as_ref()
            .map_or_else(Vec::new, |r| agent_list(&r.capabilities)),
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

/// `POST /api/sessions` 的请求体。
///
/// `agent_id` 本阶段恒不填（M3 起才接 ACP，`session.agent_id` 允许 NULL），
/// 但字段先收下：前端「新建会话」三步里本来就有选 agent 一步，
/// 现在不选、以后要选时不用再改一次契约。
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

/// `POST /api/sessions`：建会话（设备 + 工作区路径 + （M3 起）agent）。
///
/// 后端流程：
/// 1. 校验工作区路径（绝对或 `~` 开头）。**越界路径在此就拒**，
///    不给「先建会话、打开面板时报错」留余地（设计规范 §7.4 要求越界可见）。
/// 2. 设备在线否——离线建会话回 **409 并带设备状态**，让页面能显示
///    「这台设备现在连不上」而不是一个没有主语的成功。
/// 3. `SessionRepo::create` 在**一个事务**里 upsert 工作区 + 插会话行
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
    if !matches!(status, DeviceStatus::Online) {
        return Err(ApiError::conflict_with(
            err::OFFLINE,
            || json!({"status": status}),
        ));
    }

    let session = hub_storage::SessionRepo::new(state.devices().pool().clone())
        .create(
            &body.node_id,
            &body.path,
            body.agent_id.as_deref(),
            &hub_core::registry::now_stamp(),
        )
        .map_err(|e| ApiError::internal_with("cannot create the session", e))?;

    let detail = SessionSummary {
        id: session.id.clone(),
        agent_id: session.agent_id.clone(),
        state: session.state,
        created_at: session.created_at.clone(),
        ended_at: session.ended_at.clone(),
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
/// 数据源是**设备心跳上报的 `capabilities.agents`**（§6.1：「已装且就绪」
/// 不是「支持」），**不读库**：上报在 `Reports` 里，而它才是路由该读的
/// 那一份（`scheduling.md` §2.2）。一台设备都没上报过时列表是空的，
/// 那是对的——编一个「装了什么」等于把任务派给没有它的机器。
pub async fn list_agents(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentSummary>>, ApiError> {
    let devices = state
        .devices()
        .list()
        .map_err(|e| ApiError::internal_with("cannot list devices", e))?;
    let mut out: BTreeSet<String> = BTreeSet::new();
    for d in &devices {
        if let Some(r) = state.reports().get(&d.id) {
            out.extend(agent_list(&r.capabilities));
        }
    }
    Ok(Json(
        out.into_iter()
            .map(|name| AgentSummary { name })
            .collect::<Vec<_>>(),
    ))
}

/// Agent 目录的一项。
#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub name: String,
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
}
