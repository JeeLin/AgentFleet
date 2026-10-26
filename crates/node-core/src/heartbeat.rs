//! 心跳载荷的构造（子任务 5，`scheduling.md` §2）。
//!
//! **能力用于「排除」，负载用于「排序」**——混在一起就会写出「内存高的设备
//! 优先」这种错逻辑：一台 64G 满载的机器比一台 8G 空闲的差。所以这里分成
//! 两个独立入口，由调用方决定此刻报什么。
//!
//! M1 只报**真能取到**的量：`node_version` 与 `arch`/`os`。内存、CPU、磁盘
//! 余量需要读 `/proc` 或引依赖，M1 不做——报一个编出来的数字比不报坏得多，
//! 路由会照着它做硬过滤。

use std::path::Path;

use protocol::message::Heartbeat;

use crate::connector;

/// M1 上报的能力：connectors 与 arch，以及（M2 起）探测到的 agent。
///
/// **`connectors` 由 [`connector::registered_names`] 导出，不手写数组**
/// （子任务 11）。M0 的心跳硬编码报着 `["terminal","files","git","exec"]`，
/// 而那四个当时**一个都没实现**——`git` 至今没有实现。
///
/// 「上报了但没实现」比「没上报」危险得多：约束 9 要求路由按上报能力做
/// **硬过滤**，所以一份撒谎的能力表会把任务派给一台根本没装那个 connector
/// 的机器，而症状是「任务发出去就没回音」，与根因相距很远。
///
/// 导出而不是手写的另一个理由是**它让漂移变得可测**：注册表加了一个
/// connector 而上报没跟上时，`tests/capabilities.rs` 会红，而手写的数组
/// 要等到有人在真实路由里踩到才发现。
///
/// # `agents`（M2 起非空）
///
/// M1 时这里是硬编码的 `[]`，注释写着「M1 还没有 agent 运行时可探测」。
/// **那个前提已被推翻**：M2 起设备探测本机装了哪些 agent（`agent::detect`），
/// 结果由 [`capabilities_with_agents`] 填进来。
///
/// 但**「不编造」这条没变**，只是换了个方向：
/// - 探测不到就是空数组 / `available:false`，**绝不填一个看起来能用的假条目**；
/// - 探测**不在这个函数里做**（它会起子进程）。这里只消费已经探好的快照，
///   见 [`crate::agent::Detected`]——心跳路径上阻塞等于让心跳节奏取决于本机
///   装了什么 agent（M1 的 `exec` 在 `open()` 里同步跑命令就是这么翻的车）。
///
/// 兼容性：agent 项从字符串改成了对象，Hub 侧 `agent_list()` 已配套支持两种
/// 形态（`AgentReport::from_json` 也认旧的纯字符串）。改形态而不同步改 Hub
/// 解析，会让页面上 agent **静默消失**——这条在 `agent::cache` 的模块注释里。
pub fn capabilities() -> serde_json::Value {
    capabilities_with_agents(&[])
}

/// 同 [`capabilities`]，但带上探测到的 agent。
///
/// `agents` 是 [`crate::agent::AgentReport`] 的切片，按 id 排序后上报
/// （稳定顺序才 diff 得动，`scheduling.md` §2）。
pub fn capabilities_with_agents(agents: &[crate::agent::AgentReport]) -> serde_json::Value {
    let mut sorted: Vec<&crate::agent::AgentReport> = agents.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    serde_json::json!({
        "connectors": connector::registered_names(),
        "agents": sorted
            .into_iter()
            .map(crate::agent::AgentReport::to_json)
            .collect::<Vec<_>>(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "container_runtime": null,
    })
}

/// 工作区根目录清单。**只报根，绝不递归**（§2）。
///
/// 路径是情报：报出子目录等于告诉 Hub 用户在哪有私钥、哪个项目用了什么
/// secret。所以这里的输入就是一串**调用方给的根**，本模块不遍历任何目录——
/// 测试⑥ 用一棵含 `.git/`、`.env`、`~/.ssh/` 的树验证这条。
pub fn workspace_roots(roots: &[&Path]) -> Vec<String> {
    roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// 组一份心跳。
///
/// `resources` 留 `null`：M1 取不到真实的负载，而 Hub 侧对缺省字段的约定是
/// 「保持上一次的值」——留空让 Hub 保留旧事实，填假值会让它把猜测写进路由。
///
/// 不带 agent：需要上报探测结果时用 [`heartbeat_with_agents`]。
pub fn heartbeat(workspace_roots: Vec<String>) -> Heartbeat {
    heartbeat_with_agents(workspace_roots, &[])
}

/// 组一份带 agent 探测结果的心跳。
///
/// `agents` 是**已经探好的快照**，本函数不做探测（理由见 [`capabilities`]）。
pub fn heartbeat_with_agents(
    workspace_roots: Vec<String>,
    agents: &[crate::agent::AgentReport],
) -> Heartbeat {
    Heartbeat {
        node_version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: capabilities_with_agents(agents),
        resources: serde_json::Value::Null,
        workspace_roots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`connectors` 就是注册表，一个不多一个不少。**
    ///
    /// 这条测试的形状是「不许编造能力」：M0 的心跳硬编码报着四个 connector，
    /// 而那四个当时一个都没实现。判据不写死具体名字——写死的话
    /// 「注册表加了、上报忘了」会被下一次修改顺手改掉，而**锁死在这里的
    /// 集合恒等于注册表**才是那条不变式。
    #[test]
    fn the_reported_connectors_are_exactly_the_registry() {
        let c = capabilities();
        let list = c["connectors"].as_array().expect("connectors 是数组");
        let reported: Vec<&str> = list.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            reported,
            connector::registered_names(),
            "上报的能力表必须与注册表一字不差"
        );
        assert!(!reported.contains(&"git"), "git 从来没实现过，不许上报");
    }

    /// **上报的每个名字都必须真造得出实例。**
    ///
    /// 「上报了但建不出来」是能力表撒谎的**另一种形状**：路由把任务派过来，
    /// `open_channel` 回 `channel_error{unsupported}`。而反向的「建得出却
    /// 没上报」让用户看不到一个明明可用的能力。两条方向都在这里钉住。
    #[test]
    fn every_reported_connector_can_actually_be_created() {
        let c = capabilities();
        for name in c["connectors"].as_array().expect("connectors 是数组") {
            let name = name.as_str().expect("connector 名是字符串");
            assert!(connector::supports(name), "{name} 上报了却不在注册表");
            assert!(connector::create(name).is_some(), "{name} 上报了却建不出来");
        }
    }

    /// **`agents` 从 M1 的恒空改成了真实上报**——前提被推翻，不是回归。
    ///
    /// M1 里这条测试正向断言 `agents` 恒为空，理由是「M1 没有 agent 运行时可探测」。
    /// M2 让设备探测本机 agent（要求 1），那个前提没了，所以判据换成：
    ///
    /// 1. **不传探测结果时仍然是空数组**——「不编造」这条没变。
    /// 2. **传了结果就原样上报**，且 `available` 由探测结论决定，不由这里猜。
    ///
    /// 第 1 条保留原测试的价值（防止有人为了让目录页有东西而填假名字），
    /// 第 2 条是新增的正向能力。两条一起才既守住「不编造」又证明「真能上报」。
    #[test]
    fn agents_are_empty_only_when_nothing_was_detected() {
        // ① 没探测结果 → 空数组（原判据，保留）
        let c = capabilities();
        let agents = c["agents"].as_array().expect("agents 是数组");
        assert!(
            agents.is_empty(),
            "没有探测结果时必须报空，不许编一个「就绪」的假条目：{agents:?}"
        );

        // ② 有探测结果 → 原样上报
        let with = capabilities_with_agents(&[
            crate::agent::AgentReport {
                id: "opencode".into(),
                name: "opencode".into(),
                version: Some("2.1.4".into()),
                available: true,
                error: None,
            path: None,
        },
            crate::agent::AgentReport {
                id: "broken".into(),
                name: "broken".into(),
                version: None,
                available: false,
                error: Some("退出码 3".into()),
            path: None,
        },
        ]);
        let list = with["agents"].as_array().expect("agents 是数组");
        assert_eq!(list.len(), 2, "探到几个就报几个");
        let ids: Vec<&str> = list.iter().filter_map(|v| v["id"].as_str()).collect();
        assert_eq!(ids, vec!["broken", "opencode"], "按 id 排序，便于 diff");
        // 可用与否是探测的结论，这里只搬运
        assert_eq!(list[1]["available"], serde_json::json!(true));
        assert_eq!(list[1]["version"], serde_json::json!("2.1.4"));
        assert_eq!(list[0]["available"], serde_json::json!(false));
        assert_eq!(list[0]["error"], serde_json::json!("退出码 3"));
    }

    /// 上报的 agent 项能被 Hub 侧的解码器原样读回（形状对得上）。
    ///
    /// 这条守的是「改形态而不同步改解析」：`agents` 从字符串变成对象后，
    /// 只要有一侧不同步，页面上 agent 就会**静默消失**（不报错，只是列表空）。
    #[test]
    fn the_reported_agent_items_round_trip_through_the_hub_decoder() {
        let source = vec![
            crate::agent::AgentReport {
                id: "opencode".into(),
                name: "opencode".into(),
                version: Some("2.1.4".into()),
                available: true,
                error: None,
            path: None,
        },
            crate::agent::AgentReport {
                id: "nocreds".into(),
                name: "nocreds".into(),
                version: Some("1.0.0".into()),
                available: false,
                error: Some("缺凭据".into()),
            path: None,
        },
        ];
        let caps = capabilities_with_agents(&source);
        let decoded: Vec<crate::agent::AgentReport> = caps["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .filter_map(crate::agent::AgentReport::from_json)
            .collect();
        let mut expect = source.clone();
        expect.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            decoded, expect,
            "node 上报的形状必须能被 Hub 的解码器读懂"
        );
    }

    /// 上报必须**排序**：同一台设备每次心跳的字段要能 diff，而
    /// `HashMap` 的迭代序是随机的（`scheduling.md` §2）。
    #[test]
    fn the_reported_connector_list_is_sorted() {
        let c = capabilities();
        let reported: Vec<String> = c["connectors"]
            .as_array()
            .expect("connectors")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let mut sorted = reported.clone();
        sorted.sort();
        assert_eq!(reported, sorted, "上报顺序必须稳定可 diff：{reported:?}");
    }

    #[test]
    fn capabilities_carry_the_platform_facts_and_nothing_invented() {
        let c = capabilities();
        assert!(!c["os"].as_str().expect("os").is_empty());
        assert!(!c["arch"].as_str().expect("arch").is_empty());
        // 取不到的真实值一律 null，不编一个 0 —— Hub 会照它做硬过滤。
        assert!(c["container_runtime"].is_null());
        // `resources` **不在能力表里**：它属于心跳的另一个字段。
        // `c["resources"]` 对缺键返回 `Null`，那条断言因此恒真——写它等于
        // 没写。判据要看键在不在。
        assert!(
            c.get("resources").is_none(),
            "能力表里不该有 resources 这个键：{c}"
        );
    }

    /// 路径只到根一级：给几个根报几个根，本模块不碰文件系统。
    #[test]
    fn workspace_roots_reports_only_what_the_caller_gave() {
        let roots = [Path::new("/w"), Path::new("/srv/agentfleet")];
        let got = workspace_roots(&roots);
        assert_eq!(got.len(), 2, "给了 2 个根就该报 2 个");
        assert!(got.iter().any(|s| s == "/w"));
    }

    /// 空根清单 = 「没有工作区」，不是「遍历整个磁盘」。
    #[test]
    fn an_empty_root_list_stays_empty() {
        assert!(workspace_roots(&[]).is_empty());
    }

    /// 组出来的心跳必须能被协议层解回去——序列化形状错了 Hub 会当成坏帧。
    #[test]
    fn a_built_heartbeat_round_trips_through_the_protocol() {
        let hb = heartbeat(vec!["/w".to_string()]);
        assert!(!hb.node_version.is_empty());
        assert!(!hb.capabilities.is_null(), "缺省会静默丢掉能力");
        assert!(hb.resources.is_null(), "M1 取不到真实负载，不许编一个");
        assert_eq!(hb.workspace_roots, vec!["/w".to_string()]);

        let text = serde_json::to_string(&hb).expect("serialize");
        let back: Heartbeat = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back.node_version, hb.node_version);
        assert_eq!(back.workspace_roots, hb.workspace_roots);
    }

    /// **可缺省字段必须真的可缺省**：这份清单从协议结构上**探**出来，
    /// 不是手抄的字面量。
    ///
    /// 做法：把 `Heartbeat` 序列化成一个对象，逐个字段**删掉**再解一次——
    /// 还能解开 = 该字段可缺省（`#[serde(default)]`），解不开 = 必填。
    /// 手抄的清单与协议各改各的时，这条测试依旧绿，而真正的漂移信号
    /// （node 以为可省、Hub 以为必填）要等真机踩到。
    #[test]
    fn the_optional_field_list_is_derived_from_the_protocol_struct() {
        let full = serde_json::to_value(heartbeat(vec!["/w".to_string()])).expect("serialize");
        let keys: Vec<String> = full
            .as_object()
            .expect("心跳载荷是对象")
            .keys()
            .cloned()
            .collect();
        let mut optional: Vec<String> = Vec::new();
        for key in keys {
            let mut probe = full.clone();
            if let Some(obj) = probe.as_object_mut() {
                obj.remove(&key);
            }
            if serde_json::from_value::<Heartbeat>(probe).is_ok() {
                optional.push(key);
            }
        }
        optional.sort();
        assert_eq!(
            optional,
            vec![
                "capabilities".to_string(),
                "resources".to_string(),
                "workspace_roots".to_string()
            ],
            "可缺省字段必须与协议结构一致（探出来的，不是抄的）"
        );
    }
}
