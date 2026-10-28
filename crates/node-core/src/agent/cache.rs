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
/// # 形态是对象，不是字符串——这会**打破**当时的 Hub 侧解析
///
/// v2 的 `agents` 是 `["opencode", ...]`。改成对象数组时若不同步改 Hub 侧
/// 解析，页面上 agent 会**静默消失**（不报错、不警告，只是列表空了）。
/// 当时 `agent_list()` 用 `filter_map(as_str)` 只认字符串，正是这个风险。
///
/// **现状（M2 之后）**：`agent_list()` 已经两种形态都认——先试 `as_str()`，
/// 再走 `as_object()` 取 `name`/`id`。所以升级窗口里的旧字符串帧不会把列表
/// 打空；这段告警保留是因为它解释了**为什么**那条兼容分支必须留着，而不是
/// 因为缺陷还在。
///
/// 之所以还是要改成对象：字符串装不下「版本」「可用」「探测时间」，而子任务 5
/// 的 409 门禁判的是「已装**且**可用」——那三个字段缺一个都判不了。
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
    /// # 它是设备本地事实，**不过网**
    ///
    /// [`AgentReport::to_json`] **不写**这个字段，[`AgentReport::from_json`] 也就
    /// 恒读回 `None`：Hub 不执行这个文件，库里也不该存设备上的绝对路径。
    /// 所以「保留它是为了下次能直接执行它」这句话**只在本进程内成立**——
    /// 进程重启之后它是空的，由重新探测填充，而不是从 Hub 带回来。
    ///
    /// 进程内它确实有用：`PATH` 里可能有两份同名可执行文件，探测到的那份
    /// 才算数；`agent_config.command` 为空串时就回落到它
    /// （`worker::detected_command` 读这里，再交给 `conversation::resolve_command`）。
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
///
/// # 没装的 agent **不进上报**
///
/// [`Probe::NotInstalled`] 不产生上报项（`report_of` 返回 `None`），而不是转成一条
/// `available: false` 的记录。这是 M2 收尾时实测抓到的真缺陷（子任务 13 的
/// `--uninstall-drill`）：把 opencode 从 PATH 移走后，它**仍然留在**
/// `/api/agents` 里，于是「这台机器上有什么」的目录对用户撒谎。
///
/// # 为什么不能靠 `available: false` 区分
///
/// 上报里 `available: false` 有三种来源，形状却一样：
/// - `NotInstalled`（从没装过）
/// - `NotCapable`（装了、能跑、但没有 ACP 入口——**必须列出来**，好让用户
///   看见「为什么我的 claude 不能对话」，这是四态设计的全部意义）
/// - `Failed`（装了但跑不起来）
///
/// 早先 `NotInstalled` 转成 `{available: false, error: null}`，与另外两种只差
/// 一个字段；Hub 侧拿到 `agent_runtime` 的一行、按 `available` 判，分不开
/// 「没装」与「装了但坏」。让它在**源头就不出现**，比在 Hub 侧猜字段可靠：
/// 心跳的语义是「这台机器上**存在**哪些 agent」，不是「我知道的 9 个 agent
/// 各自怎么样」。与 `heartbeat::capabilities_with_agents` 的注释一致——「探测
/// 不到就是空数组，绝不填一个看起来能用的假条目」。
pub fn probe_once(
    lookup: &dyn Fn(&str) -> Option<std::path::PathBuf>,
    timeout: Duration,
) -> Snapshot {
    let mut agents: Vec<AgentReport> = Vec::new();
    for (spec, probe) in detect_all(lookup, timeout) {
        // 没装的不上报，理由见本函数的文档。
        if let Some(report) = report_of(spec, &probe) {
            agents.push(report);
        }
    }
    Snapshot {
        agents,
        at: Some(Instant::now()),
    }
}

/// 一条探测结果转成上报项；`None` = 这台机器上根本没有它。
///
/// 返回 `Option` 而不是在 `report_of` 里静默填一条假的：调用方必须**显式**
/// 处理「没有」这一态，否则以后加一个消费者（比如写设备本地缓存）时，
/// 它会默认收到一条不存在的 agent。
fn report_of(spec: &AgentSpec, probe: &Probe) -> Option<AgentReport> {
    let report = match probe {
        // 从没装过 → 不进上报。判据是探测**没找到可执行文件**，不是
        // 「跑不起来」——后者是 `Failed`，要列出来并带原因。
        Probe::NotInstalled => return None,
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
    };
    Some(report)
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

    /// **没装的 agent 不进上报**——空查找表就是空列表。
    ///
    /// 这条替换了旧版 `nothing_installed_reports_every_agent_as_unavailable_not_omitted`，
    /// 旧版断言「已知 agent 都要出现」，理由是「页面上要能显示『这台设备一个
    /// agent 都没有』，而不是空列表让人分不清『没探』和『没装』」。
    ///
    /// **那条理由被实测推翻了**（M2 子任务 13 的 `--uninstall-drill`）：把
    /// opencode 从 PATH 移走后，它仍然留在 `/api/agents` 里——因为设备上报了
    /// 一条 `{available:false, error:null}`，而 Hub 侧那一行的形状与「装了但
    /// 不能对话」(`NotCapable`) 完全相同，分不开。目录对用户撒谎比空列表糟得多。
    ///
    /// 「没探」与「没装」的分不清问题，改由心跳的**别处**承担：设备侧探测
    /// 快照有 `at` 时间戳，Hub 侧看 `agent_runtime.detected_at` 就知道这份事实
    /// 有多新。不需要为此往上报里塞 9 条不存在的 agent。
    ///
    /// 反向验证：把 `report_of` 里 `Probe::NotInstalled => return None` 改回
    /// 构造一条假记录，这条会红在「空查找表必须上报空列表」。
    #[test]
    fn nothing_installed_reports_nothing_rather_than_nine_absent_agents() {
        let snap = probe_once(&empty_lookup(), Duration::from_millis(50));
        assert!(
            snap.agents.is_empty(),
            "一个都没装就该报空列表，不许把 9 个「没装」也推上去：{:?}",
            snap.agents.iter().map(|a| &a.id).collect::<Vec<_>>()
        );
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
        let oc = snap
            .agents
            .iter()
            .find(|a| a.id == "opencode")
            .expect("这个查找表里有 opencode，它必须出现在上报里");
        assert!(!oc.available, "跑不起来不能算可用");
        assert!(oc.error.is_some(), "失败必须带原因：{oc:?}");
    }

    /// **缓存返回上一次刷新的事实，不重新探测。**
    ///
    /// 用一个**真的能探到**的假 agent：空查找表现在回空列表（见
    /// `nothing_installed_reports_nothing_rather_than_nine_absent_agents`），
    /// 空对空比不出「缓存有没有起作用」。这里要验的是「第二次读拿到的是第一次
    /// 的结果、而不是又跑了一遍子进程」。
    #[test]
    fn the_cache_serves_the_last_refresh_without_touching_processes() {
        let d = std::env::temp_dir().join(format!("af-cache-c-{}", std::process::id()));
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

        let det = Detected::new();
        assert!(det.snapshot().agents.is_empty(), "初始缓存是空的");

        let snap = det.refresh_with(&lookup, Duration::from_millis(500));
        assert_eq!(snap.agents.len(), 1, "只装了 opencode 就只报它一条");
        assert_eq!(snap.agents.first().map(|a| a.id.as_str()), Some("opencode"));
        // 再读一次：拿到的是上次刷新的事实，不再探测
        let again = det.snapshot();
        assert_eq!(again.agents, snap.agents, "缓存要给回同一份事实");
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

    /// `capabilities.agents` 是**对象数组**，每一项都有 `id` 与 `available`。
    ///
    /// 用直接构造的 `Snapshot` 而不是 `probe_once(&empty_lookup)`：后者现在回
    /// 空列表（没装的不上报），空数组验不了「每一项的形状」——`for item in arr`
    /// 会一次都不执行，测试变成永远绿的空壳。
    #[test]
    fn the_snapshot_json_shape_is_an_array_of_objects() {
        let snap = Snapshot {
            agents: vec![
                AgentReport {
                    id: "opencode".into(),
                    name: "opencode".into(),
                    version: Some("2.1.4".into()),
                    available: true,
                    error: None,
                    path: None,
                },
                AgentReport {
                    id: "claude".into(),
                    name: "claude".into(),
                    version: None,
                    available: false,
                    error: Some("没有 ACP 入口".into()),
                    path: None,
                },
            ],
            at: None,
        };
        let j = snap.to_json();
        let arr = j.as_array().expect("agents 是数组");
        assert_eq!(arr.len(), 2, "两条都要在：{j}");
        for item in arr {
            assert!(item.is_object(), "每一项都是对象：{item}");
            assert!(item["id"].is_string());
            assert!(item["available"].is_boolean());
        }
    }
}
