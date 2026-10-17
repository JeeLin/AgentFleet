//! Hub 内存里的「最近一次心跳上报」与在线状态判定（子任务 5、6）。
//!
//! **路由路径不读库**：`scheduling.md` §2.2 明确「Hub 内存里维护一份
//! `node_id → 上报`，只在心跳时更新，路由时读内存不读库」。多设备高频写
//! 落库会撞 `SQLITE_BUSY`，而路由不该等磁盘。
//!
//! 落库的只有 `last_seen_at`、一份能力/负载摘要与 `status`——**连接句柄永不落库**
//! （子任务 3 测试标准④）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use protocol::message::Heartbeat;

/// 连续心跳丢失超过这个时长 → 判离线（= 3 个 15s 周期）。
///
/// ⚠️ 三个实现必须用同一个常量：Hub 判离线、心跳间隔默认值、文档里「45s」。
/// 写成两处不同的数就会出现「刚判离线又收到心跳」的抖动。
pub const OFFLINE_AFTER: Duration = Duration::from_secs(45);

/// 一台设备当前能不能被路由到。
///
/// `Reconnecting` 是**只在内存的状态**（`README.md` §3）：WS 断了但心跳还没
/// 超时——PTY 还活着、工作区还活着、会话还活着，所以调度**硬过滤必须把它算可用**
/// （`scheduling.md` §3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// WS 连着，且心跳新鲜。
    Online,
    /// WS 断了，但心跳未超时：正在退避重连。
    Reconnecting,
    /// 心跳超时。
    Offline,
}

impl Presence {
    /// 调度硬过滤：只有 `Offline` 被排除。
    ///
    /// 「重连中的设备被过滤掉」与 `PRODUCT.md`「离线期间会话仍存活」直接冲突。
    pub fn schedulable(self) -> bool {
        !matches!(self, Self::Offline)
    }

    /// 落库的 `device.status` **只有 online / offline 两种取值**
    /// （schema 的 CHECK 约束），`Reconnecting` 落库时按 online 记。
    pub fn persisted(self) -> &'static str {
        match self {
            Self::Online | Self::Reconnecting => "online",
            Self::Offline => "offline",
        }
    }
}

/// 一次心跳上报的内容（`scheduling.md` §2）。
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub node_version: String,
    /// connector / agents / arch 等**能力**：任务路由的硬过滤输入。
    pub capabilities: serde_json::Value,
    /// cpu / memory / load 等**负载**：任务路由的有序打分输入。
    ///
    /// ⚠️ 能力用于「排除」、负载用于「排序」，混在一起就会写出
    /// 「内存高的优先」这种错逻辑（§2.1）。
    pub resources: serde_json::Value,
    /// 工作区根目录，**只报根**、绝不递归（§2 路径排除规则）。
    pub workspace_roots: Vec<String>,
    /// 收到这次上报的时刻。
    pub at: Instant,
}

impl Report {
    /// 心跳距今多久。判定离线只看这个，不看落库的 `last_seen_at`——
    /// 那是墙钟，重启 / 改时区会跳变。
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.at)
    }
}

/// `node_id → 最近一次上报`。`Arc` 以便与 `AppState` 共享。
#[derive(Debug, Clone, Default)]
pub struct Reports(Arc<RwLock<HashMap<String, Report>>>);

impl Reports {
    pub fn new() -> Self {
        Self::default()
    }

    /// 心跳到达时更新。**只在心跳时调用**——路由路径不写这里。
    pub fn record(&self, node_id: &str, report: Report) {
        if let Ok(mut m) = self.0.write() {
            m.insert(node_id.to_string(), report);
        }
        // coverage: excluded — 锁中毒意味着另一个持锁者 panic 了，
        // 而 workspace lint 把 panic 禁成 deny（生产代码无 panic 路径）；
        // 那种情况下进程的状态已不可信，静默丢弃这次上报不会更糟。
    }

    /// 读最近一次上报（路由用）。
    pub fn get(&self, node_id: &str) -> Option<Report> {
        match self.0.read() {
            Ok(m) => m.get(node_id).cloned(),
            // coverage: excluded — 同 record：锁中毒只在别的持锁者 panic 时出现。
            Err(_) => None,
        }
    }

    // **故意没有 `forget`**：内存上报必须活过连接断开，否则断线设备在
    // 心跳还没超时的窗口里会被判成「没有上报」，而那正是
    // `Presence::Reconnecting` 存在的理由（`ws.rs` 的断开处理器也不清它）。
    // 判离线只认心跳年龄（`OFFLINE_AFTER`），不需要断开时主动删。
}

/// 判在线状态。`connected` 是「WS 现在是否连着」，与心跳新鲜度**分开看**。
///
/// 为什么不合成一个字段：设备重连期间 WS 是断的，但会话还活着；
/// 只看心跳会在这段窗口里把它误判成离线（`PRODUCT.md`「离线期间会话仍存活」）。
pub fn presence(reports: &Reports, node_id: &str, connected: bool, now: Instant) -> Presence {
    let Some(report) = reports.get(node_id) else {
        // 从没上报过心跳 = 没有可信数据，按离线处理。
        // 刚 enroll 完还没发第一次心跳的窗口很短（子任务 5 要求 auth_ok 后立即发一次）。
        return Presence::Offline;
    };
    if report.age(now) >= OFFLINE_AFTER {
        return Presence::Offline;
    }
    if connected {
        Presence::Online
    } else {
        Presence::Reconnecting
    }
}

/// 心跳载荷 → 内存上报（`scheduling.md` §2）。
///
/// `capabilities` / `resources` / `workspace_roots` 三项允许缺省（约束 14），
/// 而**缺省不是「写 null」，而是「保留上一次的值」**（里程碑测试标准④）：
/// 一次不带负载的心跳不得把设备的能力清空。
pub fn report_from_heartbeat(hb: &Heartbeat, at: Instant) -> Report {
    Report {
        node_version: hb.node_version.clone(),
        capabilities: hb.capabilities.clone(),
        resources: hb.resources.clone(),
        workspace_roots: hb.workspace_roots.clone(),
        at,
    }
}

/// 把一次心跳合并进**已落库**的能力/负载里（标准④的落库侧）。
///
/// 三项都可缺省；缺省时用旧值，`null` 与「字段不存在」在这里**同义**：
/// 载荷里显式写 `null` 与不写这个字段，对 Hub 都是「这次没报」。
/// 只有真报了新值才会覆盖——所以「只含 node_version 的心跳」是一次
/// 纯粹的在线确认，不动库里任何东西。
///
/// ⚠️ `capabilities` 合并时**必须保住了 `public_key`**：M0 权宜把公钥存在
/// `capabilities.public_key`（见 `repo::set_public_key` 与 `BUGS.md`），
/// 而心跳报的 capabilities 里没有它。整对象覆盖会把它抹掉，于是下一次
/// 重连找不到库内公钥 → `identity mismatch`，设备永久接不上。
pub fn merge_into_stored(
    stored_caps: Option<&str>,
    stored_resources: Option<&str>,
    hb: &Heartbeat,
) -> (String, String) {
    let caps = match non_null(&hb.capabilities) {
        Some(v) => preserve_public_key(&v, stored_caps),
        None => stored_json(stored_caps),
    };
    let res = match non_null(&hb.resources) {
        Some(v) => v,
        None => stored_json(stored_resources),
    };
    // 序列化失败只有「Value 里有 NaN 之类」这一种可能，而那进不来
    // （载荷是 JSON 解出来的）。真失败了也别让心跳断——退回旧值。
    (
        serde_json::to_string(&caps).unwrap_or_else(|_| stored_json(stored_caps).to_string()),
        serde_json::to_string(&res).unwrap_or_else(|_| stored_json(stored_resources).to_string()),
    )
}

/// `null` 视作缺省。其余值（含 `{}`、`[]`、字符串）都算「这次报了」。
fn non_null(v: &serde_json::Value) -> Option<serde_json::Value> {
    if v.is_null() {
        None
    } else {
        Some(v.clone())
    }
}

/// 把 `public_key` 从旧 capabilities 搬进新的那份。
///
/// 心跳载荷结构上没有这个键（它不属于能力上报），而库内那一份是 enroll 时
/// 写下的、重连验签的唯一来源。所以它**不是心跳字段，无条件保留**——
/// 与「缺省保留」是两回事：那条规则说的是「这次没报就不动」，而这里连报的
/// 时候也不许动。
///
/// **改 `merge_into_stored` 之前先读这里**：这个键是 M0 没有 schema 列、
/// 寄存在 `capabilities` 里的权宜之计（`docs/BUGS.md` 已登记）。合并规则
/// 每加一条，都要保证 `public_key` 仍然活着——它一丢，所有设备重连一律
/// `identity mismatch`，而根因在一条看似无关的合并逻辑里。
///
/// 两边都取不到就什么都不加：那台设备本来就没存过公钥（老数据），
/// 编一个空串进去只会让重连走进同一条 `identity mismatch`，还更难查。
fn preserve_public_key(incoming: &serde_json::Value, stored: Option<&str>) -> serde_json::Value {
    let Some(key) = stored
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("public_key").cloned())
    else {
        return incoming.clone();
    };
    let mut out = incoming.clone();
    if let Some(o) = out.as_object_mut() {
        o.insert("public_key".to_string(), key);
    }
    out
}

/// 库里读出来的 JSON 串。**解不动就丢**：旧值是坏的，而 Hub 没有更好的
/// 来源可用（「猜一个」比「留着坏的」更危险，路由会照着假的做硬过滤）。
fn stored_json(s: Option<&str>) -> serde_json::Value {
    s.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(serde_json::Value::Object(Default::default()))
}

/// 心跳超时 → 需要落库成 offline 的 node。
///
/// 返回 `node_id` 列表而不是直接改库：hub-core **不写 SQL**（`lib.rs` 模块注释），
/// 调用方拿列表去 `repo.mark_offline`。
pub fn timed_out(reports: &Reports, now: Instant) -> Vec<String> {
    let mut ids: Vec<String> = match reports.0.read() {
        Ok(m) => m
            .iter()
            .filter(|(_, r)| r.age(now) >= OFFLINE_AFTER)
            .map(|(id, _)| id.clone())
            .collect(),
        // coverage: excluded — 同 record：锁中毒只在别的持锁者 panic 时出现。
        Err(_) => Vec::new(),
    };
    // 排序让调用方落库顺序稳定：同一批超时的设备每次以同样顺序写，
    // 测试断言与日志比对才可复现。
    //
    // 与 `presence` 不同的是**排序不依赖内容**——调用方（`sweep`）拿到的
    // 就是落库顺序，同一批设备两次扫描的 SQL 序列必须逐字相同。
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(now: Instant) -> Report {
        Report {
            node_version: "0.1.0".into(),
            capabilities: serde_json::json!({"connectors": ["terminal"]}),
            resources: serde_json::json!({"load": {"load1": 1.8}}),
            workspace_roots: vec!["/w".into()],
            at: now,
        }
    }

    fn at(now: Instant, secs: u64) -> Report {
        let mut r = report(now);
        r.at = now - Duration::from_secs(secs);
        r
    }

    #[test]
    fn record_then_get_round_trips() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", report(now));
        let got = r.get("n_1").expect("must be present");
        assert_eq!(got.node_version, "0.1.0");
        assert_eq!(got.workspace_roots, vec!["/w".to_string()]);
    }

    #[test]
    fn get_on_unknown_node_is_none() {
        let r = Reports::new();
        assert!(r.get("n_missing").is_none());
    }

    #[test]
    fn record_replaces_the_previous_report() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", at(now, 30));
        r.record("n_1", report(now));
        let got = r.get("n_1").expect("must be present");
        assert_eq!(got.age(now), Duration::ZERO, "second report must win");
    }

    /// 上报**活过连接断开**：断开那一刻不清它，重连中的设备才拿得到
    /// `Presence::Reconnecting` 而不是「没有上报 → Offline」。
    #[test]
    fn a_report_survives_the_absence_of_a_live_socket() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", report(now));
        // 没有任何 forget：presence 的入参 `connected` 由调用方给
        assert_eq!(presence(&r, "n_1", false, now), Presence::Reconnecting);
    }

    #[test]
    fn fresh_heartbeat_and_open_socket_is_online() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", report(now));
        assert_eq!(presence(&r, "n_1", true, now), Presence::Online);
    }

    /// 重连窗口：WS 断了但心跳没超时 → `Reconnecting`，且**调度仍可用**。
    #[test]
    fn disconnected_but_fresh_is_reconnecting_and_still_schedulable() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", at(now, 10));
        let p = presence(&r, "n_1", false, now);
        assert_eq!(p, Presence::Reconnecting);
        assert!(p.schedulable(), "重连中的设备必须可被路由");
        assert_eq!(p.persisted(), "online", "库里只有 online/offline 两值");
    }

    #[test]
    fn stale_heartbeat_is_offline_even_while_connected() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", at(now, 46));
        let p = presence(&r, "n_1", true, now);
        assert_eq!(p, Presence::Offline);
        assert!(!p.schedulable());
        assert_eq!(p.persisted(), "offline");
    }

    /// 44s 与 46s 卡在阈值两侧——阈值本身是唯一判据，不能含糊。
    #[test]
    fn the_offline_threshold_is_exactly_45s() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", at(now, 44));
        assert_eq!(presence(&r, "n_1", true, now), Presence::Online);
        r.record("n_1", at(now, 46));
        assert_eq!(presence(&r, "n_1", true, now), Presence::Offline);
    }

    #[test]
    fn never_reported_is_offline_not_a_panic() {
        let r = Reports::new();
        assert_eq!(
            presence(&r, "n_ghost", true, Instant::now()),
            Presence::Offline
        );
    }

    #[test]
    fn timed_out_lists_only_stale_nodes() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_fresh", at(now, 5));
        r.record("n_stale", at(now, 100));
        r.record("n_edge", at(now, 45));
        let mut got = timed_out(&r, now);
        got.sort();
        assert_eq!(got, vec!["n_edge".to_string(), "n_stale".to_string()]);
    }

    #[test]
    fn timed_out_is_empty_when_everything_is_fresh() {
        let r = Reports::new();
        let now = Instant::now();
        r.record("n_1", at(now, 1));
        assert!(timed_out(&r, now).is_empty());
    }

    #[test]
    fn timed_out_on_empty_cache_is_empty() {
        assert!(timed_out(&Reports::new(), Instant::now()).is_empty());
    }

    /// 能力与负载是**两个独立字段**，路由一个用于硬过滤、一个用于有序打分。
    #[test]
    fn capabilities_and_load_stay_separate_fields() {
        let r = report(Instant::now());
        let caps = r.capabilities.get("connectors").expect("capabilities");
        let load = r.resources.get("load").expect("resources");
        assert!(caps.is_array(), "capabilities 是硬过滤输入");
        assert!(load.is_object(), "load 是打分输入");
    }

    fn heartbeat(caps: serde_json::Value, res: serde_json::Value) -> Heartbeat {
        Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: caps,
            resources: res,
            workspace_roots: vec!["/w".into()],
        }
    }

    #[test]
    fn report_from_heartbeat_carries_every_field() {
        let hb = heartbeat(
            serde_json::json!({"connectors": ["git"]}),
            serde_json::json!({"load": {"load1": 1.0}}),
        );
        let now = Instant::now();
        let r = report_from_heartbeat(&hb, now);
        assert_eq!(r.node_version, "0.1.0");
        assert_eq!(r.capabilities["connectors"][0], "git");
        assert_eq!(r.resources["load"]["load1"], 1.0);
        assert_eq!(r.workspace_roots, vec!["/w".to_string()]);
        assert_eq!(r.age(now), Duration::ZERO);
    }

    /// 标准④：**只含 `node_version`** 的心跳不得清空库里已有的能力/负载。
    ///
    /// 这是「缺省即保留」的落地处：null 与字段不在载荷里同义，
    /// 两种都走「用旧值」。
    #[test]
    fn a_minimal_heartbeat_keeps_the_stored_capabilities_and_resources() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::Value::Null,
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, res) = merge_into_stored(
            Some(r#"{"connectors":["terminal"]}"#),
            Some(r#"{"load":{"load1":2.5}}"#),
            &hb,
        );
        let caps: serde_json::Value = serde_json::from_str(&caps).expect("caps json");
        let res: serde_json::Value = serde_json::from_str(&res).expect("res json");
        assert_eq!(caps["connectors"][0], "terminal", "能力必须保留");
        assert_eq!(res["load"]["load1"], 2.5, "负载必须保留");
    }

    /// 真报了新值才覆盖：显式写 null 与不写字段都是「这次没报」。
    #[test]
    fn a_real_report_overrides_the_stored_values() {
        let hb = heartbeat(
            serde_json::json!({"connectors": ["files", "git"]}),
            serde_json::json!({"load": {"load1": 0.1}}),
        );
        let (caps, res) = merge_into_stored(
            Some(r#"{"connectors":["terminal"]}"#),
            Some(r#"{"load":{"load1":9.9}}"#),
            &hb,
        );
        assert!(caps.contains("files"), "{caps}");
        assert!(!caps.contains("terminal"), "旧能力应被替换: {caps}");
        assert!(res.contains("0.1"), "{res}");
    }

    /// 只报一半：能力更新、负载保留。两项缺省互不影响。
    #[test]
    fn each_optional_field_merges_independently() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::json!({"connectors": ["git"]}),
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, res) = merge_into_stored(
            Some(r#"{"connectors":["terminal"]}"#),
            Some(r#"{"load":{"load1":7.0}}"#),
            &hb,
        );
        assert!(caps.contains("git"), "{caps}");
        assert!(res.contains("7.0"), "没报负载就得留旧的: {res}");
    }

    /// 心跳覆盖 capabilities 时**必须保住 `public_key`**。
    ///
    /// M0 权宜把公钥存在 `capabilities.public_key`（`BUGS.md`），而心跳报的
    /// 能力里结构上没有它。整对象覆盖会把它抹掉 → 下次重连找不到库内公钥
    /// → `identity mismatch`，设备永久接不上。这条曾是真 bug。
    #[test]
    fn merging_capabilities_never_drops_the_stored_public_key() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::json!({"connectors": ["git"]}),
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, _) = merge_into_stored(
            Some(r#"{"connectors":["terminal"],"public_key":"AAAAB3NzaC1yc2E"}"#),
            None,
            &hb,
        );
        let v: serde_json::Value = serde_json::from_str(&caps).unwrap();
        assert_eq!(v["public_key"], "AAAAB3NzaC1yc2E", "公钥必须活过心跳覆盖");
        assert_eq!(v["connectors"][0], "git", "能力仍然要更新");
    }

    /// 库里没有公钥（老数据）→ 不编一个假的进去。
    #[test]
    fn no_stored_public_key_means_none_is_invented() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::json!({"connectors": ["git"]}),
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, _) = merge_into_stored(Some(r#"{"connectors":["terminal"]}"#), None, &hb);
        let v: serde_json::Value = serde_json::from_str(&caps).unwrap();
        assert!(v.get("public_key").is_none(), "{caps}");
    }

    /// 库里存的是坏 JSON 时也照样保住新报的能力（旧值读不出来就只当没有）。
    #[test]
    fn an_unparsable_stored_capabilities_still_keeps_the_new_ones() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::json!({"connectors": ["git"]}),
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, _) = merge_into_stored(Some("not json"), None, &hb);
        assert!(caps.contains("git"), "{caps}");
    }

    /// 库里是 NULL（从没上报过）且这次也没报 → 空对象，不是 null、不是字符串 "null"。
    #[test]
    fn nothing_stored_and_nothing_reported_yields_empty_objects() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::Value::Null,
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, res) = merge_into_stored(None, None, &hb);
        assert_eq!(caps, "{}");
        assert_eq!(res, "{}");
    }

    /// 库里存的是坏 JSON（历史数据 / 手工改过）→ 丢掉而不是 panic，
    /// 更不会把坏串原样写回去。
    #[test]
    fn unparsable_stored_values_are_dropped_not_propagated() {
        let hb = Heartbeat {
            node_version: "0.1.0".into(),
            capabilities: serde_json::Value::Null,
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
        };
        let (caps, res) = merge_into_stored(Some("not json"), Some(""), &hb);
        assert_eq!(caps, "{}");
        assert_eq!(res, "{}");
    }
}
