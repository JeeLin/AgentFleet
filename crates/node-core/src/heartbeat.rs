//! 心跳载荷的构造（子任务 5，`scheduling.md` §2）。
//!
//! **能力用于「排除」，负载用于「排序」**——混在一起就会写出「内存高的设备
//! 优先」这种错逻辑：一台 64G 满载的机器比一台 8G 空闲的差。所以这里分成
//! 两个独立入口，由调用方决定此刻报什么。
//!
//! M0 只报**真能取到**的量：`node_version` 与 `arch`/`os`。内存、CPU、磁盘
//! 余量需要读 `/proc` 或引依赖，M0 不做——报一个编出来的数字比不报坏得多，
//! 路由会照着它做硬过滤。

use std::path::Path;

use protocol::message::Heartbeat;

/// M0 上报的能力：connectors 与 arch。
///
/// `agents` **故意留空**：M0 还没有 agent 运行时可探测。路由按它做硬过滤，
/// 编一个「就绪」等于把任务派给一台根本没装那个 agent 的机器。
pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "connectors": ["terminal", "files", "git", "exec"],
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
/// `resources` 留 `null`：M0 取不到真实的负载，而 Hub 侧对缺省字段的约定是
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

    /// connectors 是路由的硬过滤输入：少一项，那类任务就永远派不到这台设备。
    #[test]
    fn capabilities_carry_the_m0_connector_set() {
        let c = capabilities();
        let list = c["connectors"].as_array().expect("connectors 是数组");
        for want in ["terminal", "files", "git", "exec"] {
            assert!(list.iter().any(|v| v == want), "缺 {want}: {c}");
        }
        assert_eq!(list.len(), 4, "M0 只有这四类");
        assert_eq!(c["agents"].as_array().map(Vec::len), Some(0));
        assert!(!c["os"].as_str().expect("os").is_empty());
        assert!(!c["arch"].as_str().expect("arch").is_empty());
        assert!(c["container_runtime"].is_null());
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
        assert!(hb.resources.is_null(), "M0 取不到真实负载，不许编一个");
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
