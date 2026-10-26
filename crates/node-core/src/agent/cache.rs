//! 探测结果的缓存与心跳上报形态（子任务 3）。
//!
//! # 为什么要有这一层
//!
//! 探测会起子进程并等它（最长 [`super::detect::DEFAULT_PROBE_TIMEOUT`]）。
//! **心跳路径上不能直接探测**：心跳 tick 臂在 worker 的 `select!` 里，任何
//! 阻塞都会让心跳迟到——M1 的 `exec` 在 `open()` 里同步跑命令正是这么翻的车。
//!
//! 所以这里是「一份可随时读的缓存」：
//!
//! - 心跳读缓存（O(1)，不碰进程）；
//! - 缓存由 [`Detected::refresh`] 刷新，**调用方在心跳循环之外**调它
//!   （worker 在发心跳之前刷新一次，或由一个低频任务刷新——两者都不在
//!   `select!` 的阻塞路径上，见 `worker.rs` 的调用点）。
//!
//! 缓存里放的是**已经探测完的事实**，不是「能不能探测」的承诺。

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::detect::{detect_all, path_lookup, AgentSpec, Probe, DEFAULT_PROBE_TIMEOUT};

/// 心跳 `capabilities.agents` 里的一项。
///
/// # 形态是对象，不是字符串——这会**打破**现有 Hub 侧解析
///
/// v2 的 `agents` 是 `["opencode", ...]`，而 `api.rs` 的 `agent_list()` 用
/// `filter_map(as_str)` 只认字符串，**非字符串一律跳过**。改成对象数组而不改
/// `agent_list` 的话，页面上 agent 会**静默消失**（不报错、不警告，只是列表空了）。
///
/// 之所以还是要改：字符串装不下「版本」「可用」「探测时间」，而子任务 5 的
/// 409 门禁判的是「已装**且**可用」——那三个字段缺一个都判不了。所以配套改
/// `agent_list`（见 `hub-server` 侧），并且**保留对旧字符串形态的兼容读取**，
/// 让升级窗口里的旧帧不至于把列表打空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    /// 稳定标识（`agent_runtime.agent_id`）。
    pub id: String,
    pub name: String,
    /// 版本；没探到就是 `None`。**不填 "unknown"**——那是编的。
    pub version: Option<String>,
    /// 现在能不能对话。三态里的 `Ready` 才为真。
    pub available: bool,
    /// 探测失败的原因（`failed` 时才有），给人看的一句话。
    pub error: Option<String>,
    /// 探测到的可执行文件路径（装过才有，含探测失败的情形）。
    ///
    /// 保留它有明确用途：**下次能直接执行它**（`PROBE::Ready` 的注释就是这么
    /// 说的，而 `PATH` 里可能有两份，探测到的那份才算数）。开一条 ACP 对话时
    /// `agent_config.command` 为空串就用它。
    ///
    /// 早先这一列在 `report_of` 里被丢掉，于是「配了空命令」根本无从回落到
    /// 探测到的那个可执行文件。
    pub path: Option<String>,
}

impl AgentReport {
    /// 转成心跳里的 JSON 形态。
    ///
    /// 字段名用蛇形，与 `agent_runtime` 表列名一致，减少一层心智映射。
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), serde_json::Value::String(self.id.clone()));
        m.insert("name".into(), serde_json::Value::String(self.name.clone()));
        m.insert(
            "version".into(),
            match &self.version {
                Some(v) => serde_json::Value::String(v.clone()),
                None => serde_json::Value::Null,
            },
        );
        m.insert("available".into(), serde_json::Value::Bool(self.available));
        m.insert(
            "error".into(),
            match &self.error {
                Some(e) => serde_json::Value::String(e.clone()),
                None => serde_json::Value::Null,
            },
        );
        serde_json::Value::Object(m)
    }

    /// 从心跳 JSON 形态解回来（Hub 侧与测试用）。
    ///
    /// **兼容旧的纯字符串形态**：`"opencode"` 解成
    /// `{id:"opencode", name:"opencode", version:None, available:false}`。
    /// `available=false` 是刻意的保守值——一个只说名字的旧帧不构成
    /// 「可用」的证据，而门禁宁严勿松。
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        if let Some(name) = v.as_str() {
            return Some(Self {
                id: name.to_string(),
                name: name.to_string(),
                version: None,
                available: false,
                error: None,
                // 旧的纯字符串形态只给了个名字，没有路径可说。
                path: None,
            });
        }
        let o = v.as_object()?;
        let id = o.get("id")?.as_str()?.to_string();
        Some(Self {
            name: o
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&id)
                .to_string(),
            version: o
                .get("version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            available: o
                .get("available")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            error: o
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            // **路径不过网**：`to_json` 不写它，所以这里也读不到。
            // 它是设备本地事实（哪一份可执行文件），Hub 不执行它、也不该
            // 在库里存设备上的绝对路径。
            path: None,
            id,
        })
    }
}

/// 一次探测的汇总结果。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub agents: Vec<AgentReport>,
    /// 这批结果是什么时候测的。心跳带上它，Hub 才能判「事实有多新鲜」。
    pub at: Option<Instant>,
}

impl Snapshot {
    /// 转成 `capabilities.agents` 的数组形态。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.agents.iter().map(AgentReport::to_json).collect())
    }

    /// 有没有任何一个 agent 现在可用（测试与门禁用）。
    pub fn any_available(&self) -> bool {
        self.agents.iter().any(|a| a.available)
    }
}

/// 探一次的**纯函数**：给定查找表与超时，返回汇总。
///
/// 抽出来是为了单测能不碰 `RwLock` 直接验形状。
pub fn probe_once(
    lookup: &dyn Fn(&str) -> Option<std::path::PathBuf>,
    timeout: Duration,
) -> Snapshot {
    let mut agents: Vec<AgentReport> = Vec::new();
    for (spec, probe) in detect_all(lookup, timeout) {
        agents.push(report_of(spec, &probe));
    }
    Snapshot {
        agents,
        at: Some(Instant::now()),
    }
}

fn report_of(spec: &AgentSpec, probe: &Probe) -> AgentReport {
    match probe {
        Probe::NotInstalled => AgentReport {
            id: spec.id.to_string(),
            name: spec.name.to_string(),
            version: None,
            available: false,
            error: None,
            path: None,
        },
        // **装了、能跑、但不能对话**。它必须出现在上报里（`available: false`
        // + 一句能照做的原因），而不是像早先那样整个消失：用户装了 `claude`，
        // 页面上一个字都不提，他只会以为系统没扫到。
        //
        // 原因里写**具体是什么不支持**，不写「不可用」——后者对用户是废话，
        // 既不能验证也不能照做。
        Probe::NotCapable { version, path } => AgentReport {
            id: spec.id.to_string(),
            name: spec.name.to_string(),
            version: Some(version.clone()),
            available: false,
            error: Some(format!(
                "{} has no ACP entry point, so it cannot be used for dialogue",
                spec.bin
            )),
            path: Some(path.to_string_lossy().into_owned()),
        },
        Probe::Ready { version, path } => AgentReport {
            id: spec.id.to_string(),
            name: spec.name.to_string(),
            version: Some(version.clone()),
            available: true,
            error: None,
            path: Some(path.to_string_lossy().into_owned()),
        },
        Probe::Failed { reason, path } => AgentReport {
            id: spec.id.to_string(),
            name: spec.name.to_string(),
            version: None,
            available: false,
            error: Some(reason.clone()),
            // 探测失败也留着路径：用户据此能自己去跑一下看看到底怎么了。
            path: Some(path.to_string_lossy().into_owned()),
        },
    }
}

/// 探测结果的共享缓存。`Clone` 只复制 `Arc`。
#[derive(Debug, Clone, Default)]
pub struct Detected(Arc<RwLock<Snapshot>>);

impl Detected {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用一份现成的快照初始化（测试与「启动时先探一次」用）。
    pub fn with_snapshot(snap: Snapshot) -> Self {
        Self(Arc::new(RwLock::new(snap)))
    }

    /// 读缓存。**心跳路径只用这一个**——它不碰子进程。
    pub fn snapshot(&self) -> Snapshot {
        match self.0.read() {
            Ok(g) => g.clone(),
            // coverage: excluded — 锁中毒只在别的持锁者 panic 时出现，
            // 而 workspace lint 把 panic 禁成 deny（生产代码无 panic 路径）。
            // 那种情况下进程状态已不可信，返回上一份空快照不会更糟。
            Err(_) => Snapshot::default(),
        }
    }

    /// 重新探测并覆盖缓存。走真实 `PATH`。
    ///
    /// **调用点必须在心跳 `select!` 之外**：这个函数会起子进程并最多等
    /// `timeout`。放进心跳臂就等于让心跳的节奏取决于本机装了什么 agent。
    pub fn refresh(&self) -> Snapshot {
        self.refresh_with(&path_lookup, DEFAULT_PROBE_TIMEOUT)
    }

    /// 同 [`Detected::refresh`]，但注入查找表与超时（单测用）。
    pub fn refresh_with(
        &self,
        lookup: &dyn Fn(&str) -> Option<std::path::PathBuf>,
        timeout: Duration,
    ) -> Snapshot {
        let snap = probe_once(lookup, timeout);
        if let Ok(mut w) = self.0.write() {
            *w = snap.clone();
        }
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn empty_lookup() -> impl Fn(&str) -> Option<PathBuf> {
        |_: &str| None
    }

    #[test]
    fn nothing_installed_reports_every_agent_as_unavailable_not_omitted() {
        // 「没装」也要上报：页面上要能显示「这台设备一个 agent 都没有」，
        // 而不是一个空列表让人分不清「没探」还是「没装」。
        let snap = probe_once(&empty_lookup(), Duration::from_millis(50));
        assert!(!snap.agents.is_empty(), "已知 agent 都要出现");
        assert!(snap.agents.iter().all(|a| !a.available));
        assert!(snap.agents.iter().all(|a| a.version.is_none()));
        assert!(!snap.any_available());
    }

    #[test]
    fn a_ready_agent_reports_version_available_and_no_error() {
        let d = std::env::temp_dir().join(format!("af-cache-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("临时目录");
        let bin = d.join("opencode");
        std::fs::write(&bin, "#!/bin/sh\necho 2.1.4\n").expect("写");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let m: HashMap<String, PathBuf> = [("opencode".to_string(), bin)].into();
        let lookup = move |n: &str| m.get(n).cloned();
        let snap = probe_once(&lookup, Duration::from_millis(500));
        let oc = snap
            .agents
            .iter()
            .find(|a| a.id == "opencode")
            .expect("opencode 在清单里");
        assert!(oc.available);
        assert_eq!(oc.version.as_deref(), Some("2.1.4"));
        assert_eq!(oc.error, None);
        assert!(snap.any_available());
    }

    #[test]
    fn a_failed_probe_carries_the_reason_and_is_not_available() {
        let d = std::env::temp_dir().join(format!("af-cache-f-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("临时目录");
        let bin = d.join("opencode");
        std::fs::write(&bin, "#!/bin/sh\nexit 7\n").expect("写");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let m: HashMap<String, PathBuf> = [("opencode".to_string(), bin)].into();
        let lookup = move |n: &str| m.get(n).cloned();
        let snap = probe_once(&lookup, Duration::from_millis(500));
        let oc = &snap.agents[0];
        assert!(!oc.available, "跑不起来不能算可用");
        assert!(oc.error.is_some(), "失败必须带原因：{oc:?}");
    }

    #[test]
    fn the_cache_serves_the_last_refresh_without_touching_processes() {
        let det = Detected::new();
        assert!(det.snapshot().agents.is_empty(), "初始缓存是空的");

        let snap = det.refresh_with(&empty_lookup(), Duration::from_millis(50));
        assert!(!snap.agents.is_empty());
        // 再读一次：拿到的是上次刷新的事实，不再探测
        let again = det.snapshot();
        assert_eq!(again.agents.len(), snap.agents.len());
    }

    #[test]
    fn agent_reports_round_trip_through_json() {
        let a = AgentReport {
            id: "opencode".into(),
            name: "opencode".into(),
            version: Some("2.1.4".into()),
            available: true,
            error: None,
            path: None,
        };
        let back = AgentReport::from_json(&a.to_json()).expect("解回来");
        assert_eq!(back, a);
    }

    #[test]
    fn a_versionless_report_keeps_null_rather_than_inventing_a_string() {
        let a = AgentReport {
            id: "x".into(),
            name: "x".into(),
            version: None,
            available: false,
            error: None,
            path: None,
        };
        let j = a.to_json();
        assert!(j["version"].is_null(), "没探到版本就是 null，不填 unknown");
        assert_eq!(AgentReport::from_json(&j).expect("解回来"), a);
    }

    /// **旧的纯字符串形态必须还能读**，否则升级窗口里页面 agent 列表会突然空掉。
    #[test]
    fn the_legacy_string_form_still_parses_but_is_never_available() {
        let v = serde_json::json!("opencode");
        let a = AgentReport::from_json(&v).expect("旧形态要能读");
        assert_eq!(a.id, "opencode");
        assert_eq!(a.name, "opencode");
        assert_eq!(a.version, None);
        assert!(
            !a.available,
            "只说名字的旧帧不构成「可用」的证据，门禁宁严勿松"
        );
    }

    #[test]
    fn garbage_is_skipped_rather_than_panicking() {
        assert!(AgentReport::from_json(&serde_json::json!(42)).is_none());
        assert!(AgentReport::from_json(&serde_json::json!({"no_id": 1})).is_none());
        assert!(AgentReport::from_json(&serde_json::json!({"id": 5})).is_none());
    }

    #[test]
    fn the_snapshot_json_shape_is_an_array_of_objects() {
        let snap = probe_once(&empty_lookup(), Duration::from_millis(50));
        let j = snap.to_json();
        let arr = j.as_array().expect("agents 是数组");
        assert!(!arr.is_empty());
        for item in arr {
            assert!(item.is_object(), "每一项都是对象：{item}");
            assert!(item["id"].is_string());
            assert!(item["available"].is_boolean());
        }
    }
}
