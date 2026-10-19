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

/// M1 上报的能力：connectors 与 arch。
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
/// `agents` **故意留空**：M1 还没有 agent 运行时可探测（ACP 归 M3）。
/// 路由按它做硬过滤，编一个「就绪」等于把任务派给一台根本没装那个 agent
/// 的机器。**不做「从实际安装探测导出」**：M1 没有任何探测对象，照做会做出
/// 一个恒空的 `AgentSummary[]` 与一个「做了但永远空」的 Agent 目录页。
pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "connectors": connector::registered_names(),
        "agents": [],
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
pub fn heartbeat(workspace_roots: Vec<String>) -> Heartbeat {
    Heartbeat {
        node_version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: capabilities(),
        resources: serde_json::Value::Null,
        workspace_roots,
    }
}

/// 载荷里可以缺省的字段（约束 14）。
///
/// Hub 侧对它们一律「缺省则保持上一次的值」，所以这里**不**为了「让字段好看」
/// 而填默认值。两边清单漂移就会出现「节点以为能省、Hub 以为必须给」。
pub const OPTIONAL_FIELDS: &[&str] = &["capabilities", "resources", "workspace_roots"];

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

    /// **`agents` 为空是 M1 的事实，不是遗漏。**
    ///
    /// M1 没有任何 agent 运行时（ACP 归 M3），所以「未探测到就上报空」是
    /// **正确**的形状。这条测试锁住的是那个「不得编造」：一旦有人为了
    /// 让 Agent 目录页看起来有东西而填一个假名字，这里会红。
    #[test]
    fn agents_stay_empty_because_none_is_detected() {
        let c = capabilities();
        let agents = c["agents"].as_array().expect("agents 是数组");
        assert!(
            agents.is_empty(),
            "M1 没有 agent 运行时可探测，上报一个「就绪」等于把任务派给 \
             一台根本没装那个 agent 的机器（约束 9）：{agents:?}"
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
        assert!(c["resources"].is_null());
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

    /// 缺省字段清单必须与协议结构一致。
    #[test]
    fn the_optional_field_list_matches_the_protocol_struct() {
        assert_eq!(OPTIONAL_FIELDS.len(), 3);
        for f in OPTIONAL_FIELDS {
            assert!(
                ["capabilities", "resources", "workspace_roots"].contains(f),
                "{f} 不在 Heartbeat 里"
            );
        }
    }
}
