//! 心跳载荷的构造（子任务 5，`scheduling.md` §2）。
//!
//! **能力用于「排除」，负载用于「排序」**——混在一起就会写出「内存高的设备
//! 优先」这种错逻辑：一台 64G 满载的机器比一台 8G 空闲的差。所以这里分成
//! 两个独立入口，由调用方决定此刻报什么。
//!
//! # `resources`：M2 起真采集，采不到就留 `null`
//!
//! M0/M1 恒 `null`，理由是「编一个数字比不报坏得多——路由会照着它做硬过滤」。
//! **那条理由没有失效**，只是它约束的对象变了：不再是「没有采集」，而是
//! 「**有就报、采不到就留 `null`**」。Hub 侧对缺省字段的约定是「保持上一次
//! 的值」，所以 `null` 让 Hub 保留旧事实，而填一个猜的值会让它把猜测写进路由。
//!
//! 采的是 `/proc/loadavg` + `/proc/meminfo`（`scheduling.md` §2 就这么定的），
//! 不引第三方依赖：这两个文件是纯文本、读取是纳秒级，心跳路径上可以直接读
//! （对比：agent 探测**不能**在这里做，那要起子进程并等它，见
//! [`crate::agent::Detected`]）。非 Linux 上没有 `/proc`，自然落到 `null`。

use std::path::Path;

use protocol::message::Heartbeat;

use crate::connector;

/// M1 上报的能力：connectors 与 arch，以及（M2 起）探测到的 agent。
///
/// **`connectors` 由 [`connector::registered_names`] 导出，不手写数组**
/// （子任务 11）。M0 的心跳硬编码报着 `["terminal","files","git","exec"]`，
/// 而那四个当时**一个都没实现**——`git` 直到 v0.4.0 才真正落地
/// （`connector/git.rs`，只读的 status / diff / log）。
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
// ↑ **生产路径不经过这个包装**：设备的每一次心跳都带探测结果，`worker.rs`
// 直接调 `capabilities_with_agents`（见同处的 `heartbeat_with_agents`）。
//
// 它不是「给不带 agent 的设备留的」——那种设备从来不会出现：心跳要么带着
// 上次探到的清单，要么（探测还没跑完）不上报 `agents` 键，而缺键在 Hub 侧
// 的语义是「这次没报」，也不是「一个都没有」。
//
// 它真正的用途是**测试**里那条「不传探测结果时仍然是空数组」的判据
// （本文件 `agents_are_empty_only_when_nothing_was_detected` 与
// `tests/capabilities.rs`）：「不许编造」这条不变式需要一个零入参入口才测得动。

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

/// 组一份心跳，`resources` 由 [`collect_resources`] 现采。
///
/// 不带 agent。**生产路径不经过它**：设备上报永远带探测结果，`worker.rs`
/// 调的是 [`heartbeat_with_agents`]。它只给测试用（协议往返、可缺省字段清单
/// 都要一份「最简心跳」，而传空 agents 与传 agents 在这两件事上等价）。
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
        resources: collect_resources(),
        workspace_roots,
    }
}

/// `/proc/loadavg` 与 `/proc/meminfo` 的路径。抽成常量是为了让「非 Linux
/// 上没有 `/proc`」这条在测试里可注入，而不是靠 `#[cfg]` 跳过真实分支。
const LOADAVG_PATH: &str = "/proc/loadavg";
const MEMINFO_PATH: &str = "/proc/meminfo";

/// 采一份当前负载，形状见模块头注释。
///
/// # 取不到就留 `null`，绝不回落成 0
///
/// `0` 会被路由读成「这台设备完全空闲」于是被选中，而真实情况是「不知道」。
/// **未知与空闲是两件事**，合成一个值就是把猜测写进路由。Hub 侧对 missing
/// 字段的约定是「保持上一次的值」，所以留空是安全的。**逐段独立**：内存读
/// 不出来不影响负载上报——两者用在不同决策上，一个坏了不该让另一个消失。
///
/// # 为什么可以在心跳路径上直接读
///
/// 这两个文件是内核维护的伪文件，读取是纳秒级、不阻塞。**对比 agent 探测
/// 绝不能在这里做**：那要起子进程并最多等 2 秒（M1 的 `exec` 在 `open()`
/// 里同步跑命令就是这么把心跳臂堵住的）。
pub fn collect_resources() -> serde_json::Value {
    let mut out = serde_json::Map::new();

    let load = read_loadavg(LOADAVG_PATH);
    if let Some(LoadAvg { load1, running }) = load {
        out.insert(
            "load".into(),
            serde_json::json!({ "load1": load1, "running": running }),
        );
        if let Some(cores) = core_count() {
            out.insert("cpu".into(), serde_json::json!({ "cores": cores }));
            // CPU 百分比 = 每核负载。**它是从真测的 load1 与真核数算出来的，
            // 但不是 CPU 利用率**：load 含不可中断睡眠（等 IO），所以一台 IO
            // 密集的机器会偏高于它的真实占用。仍然报它，是因为它足以支撑
            // 「谁相对更闲」的排序，而没有第二个不需要跨心跳存状态的算法。
            // 上限不封 100：Hub 侧已有 `min(100)`（`api.rs` 的 `load_percent`），
            // 在这里也封一次会让两层截断互相掩盖。
            let per_core = load1 / cores as f64;
            if per_core.is_finite() && per_core >= 0.0 {
                out.insert(
                    "cpu_percent".into(),
                    serde_json::json!((per_core * 100.0).round() as i64),
                );
            }
        }
    }

    if let Some(MemInfo {
        total_mb,
        available_mb,
    }) = read_meminfo(MEMINFO_PATH)
    {
        out.insert(
            "memory".into(),
            serde_json::json!({ "total_mb": total_mb, "available_mb": available_mb }),
        );
        if total_mb > 0 {
            let used = total_mb.saturating_sub(available_mb);
            let pct = (used as f64 / total_mb as f64) * 100.0;
            if pct.is_finite() {
                out.insert("mem_percent".into(), serde_json::json!(pct.round() as i64));
            }
        }
    }

    if out.is_empty() {
        // coverage: excluded — 只在**读不到 `/proc`** 的机器上才会走到（非 Linux、
        // 或被裁掉的容器）。本项目的目标平台是 Linux，`/proc/loadavg` 恒在，
        // 出参不可能为空；这是给「什么也没测到」留的诚实出口，不是业务分支。
        return serde_json::Value::Null;
    }
    serde_json::Value::Object(out)
}

/// `/proc/loadavg` 里的两个量。
///
/// 只取 1 分钟均值（`load1`）：心跳周期是 15 秒，15 分钟均值在这个采样
/// 尺度上几乎不动，报了也读不出变化。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAvg {
    pub load1: f64,
    /// 正在运行 / 不可中断睡眠的进程数。
    pub running: u64,
}

/// 内存：总量与可用量，单位 MB。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_mb: u64,
    pub available_mb: u64,
}

/// 可用核数。取不到（极端容器限制）就是 `None`——那时 `cpu_percent` 一并
/// 不上报，因为它的分母必须是真的核数。
fn core_count() -> Option<u64> {
    std::thread::available_parallelism()
        .ok()
        .and_then(|n| u64::try_from(n.get()).ok())
        .filter(|n| *n > 0)
}

/// 解析 `/proc/loadavg`：`"1.23 0.98 0.55 2/512 12345"`。
///
/// 返回 `None` 的情形一律当作「不知道」：文件不存在（非 Linux）、权限不足、
/// 格式与预期不符。**不猜、不补默认值**——缺一个字段就是不知道这一份。
pub fn parse_loadavg(text: &str) -> Option<LoadAvg> {
    let mut it = text.split_whitespace();
    let load1 = it.next()?.parse::<f64>().ok()?;
    // 第 4 个字段是 `<正在运行>/<总数>`，取分子。
    let running = {
        let pair = it.nth(2)?;
        let (head, _) = pair.split_once('/')?;
        head.parse::<u64>().ok()?
    };
    Some(LoadAvg { load1, running })
}

/// 解析 `/proc/meminfo`，取 `MemTotal` 与 `MemAvailable`（kB → MB）。
///
/// **两个都要有才返回**：只有总量没法判断「还有多少余量」，而那正是路由
/// 要问的；只有可用量则无法换算百分比。二者缺一就是不知道。
///
/// `MemAvailable` 而不是 `MemFree`：前者把可回收的 page cache 算进去，
/// 才是「还能不能用」的答案；`MemFree` 在一台跑久了的机器上恒接近 0，
/// 会把它读成「内存满了」——那是一份撒谎的事实。
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut total: Option<u64> = None;
    let mut available: Option<u64> = None;
    for line in text.lines() {
        let (key, rest) = match line.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        let kb = rest
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(kb) = kb {
            match key {
                "MemTotal" => total = Some(kb / 1024),
                "MemAvailable" => available = Some(kb / 1024),
                _ => {}
            }
        }
    }
    match (total, available) {
        (Some(total_mb), Some(available_mb)) => Some(MemInfo {
            total_mb,
            available_mb,
        }),
        _ => None,
    }
}

fn read_loadavg(path: &str) -> Option<LoadAvg> {
    parse_loadavg(&std::fs::read_to_string(path).ok()?)
}

fn read_meminfo(path: &str) -> Option<MemInfo> {
    parse_meminfo(&std::fs::read_to_string(path).ok()?)
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
        // 这一句在 M0/M1 一直是 `!contains("git")`，钉的是「git 从没实现过、
        // 硬编码上报它是撒谎」。v0.4.0 让 git 真的落地了，那条事实随之作废：
        // 现在反过来钉——实现了就必须上报，免得日后有人顺手把它摘掉。
        assert!(
            reported.contains(&"git"),
            "git 已在 v0.4.0 实现，必须出现在上报的能力表里: {reported:?}"
        );
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
        assert_eq!(decoded, expect, "node 上报的形状必须能被 Hub 的解码器读懂");
    }

    // ---- `resources`：M2 起真采集（文档漂移那条欠账）----

    /// 解析 `/proc/loadavg`：取 1 分钟均值与「正在运行」的进程数。
    #[test]
    fn loadavg_yields_the_one_minute_average_and_the_running_count() {
        let got = parse_loadavg("1.23 0.98 0.55 2/512 12345\n").expect("解析");
        assert_eq!(got.load1, 1.23);
        assert_eq!(got.running, 2);
    }

    /// 格式不认识就是「不知道」，**不猜**：猜出来的负载会进路由。
    #[test]
    fn a_loadavg_that_does_not_parse_is_unknown_not_zero() {
        // 空、截断、非数字、缺 `/`
        for bad in ["", "1.0", "abc 0.9 0.5 1/2 3", "1.0 0.9 0.5 2 3"] {
            assert_eq!(parse_loadavg(bad), None, "{bad:?} 不该被猜成一个负载");
        }
    }

    /// `MemAvailable` 而不是 `MemFree`：后者在跑久了的机器上恒接近 0，
    /// 会把它读成「内存满了」——那是一份撒谎的事实。
    #[test]
    fn meminfo_reads_available_not_free() {
        let text = "MemTotal:        8000000 kB\nMemFree:          100000 kB\nMemAvailable:    4000000 kB\n";
        let got = parse_meminfo(text).expect("解析");
        assert_eq!(got.total_mb, 7812, "kB → MB");
        assert_eq!(got.available_mb, 3906, "取 MemAvailable，不是 MemFree");
    }

    /// 只有一半信息就是不知道：**有总量没余量**算不出百分比，反之亦然。
    #[test]
    fn meminfo_needs_both_sides_or_it_is_unknown() {
        assert_eq!(parse_meminfo("MemTotal: 100 kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable: 100 kB\n"), None);
        assert_eq!(parse_meminfo(""), None);
        assert_eq!(parse_meminfo("MemTotal:  kB\nMemAvailable: 100 kB\n"), None);
    }

    /// 没有冒号的行**跳过**，不是让整份解析失败。
    ///
    /// `/proc/meminfo` 将来加一行注释或 `HugePages_Total: 0` 这种没单位的行，
    /// 都不该让设备从此报「内存未知」——那是把一个格式小变化放大成一条
    /// 消失的事实。
    #[test]
    fn meminfo_skips_lines_that_are_not_key_value_pairs() {
        let text =
            "HugePages without a colon\nMemTotal:        1000 kB\nMemAvailable:     400 kB\n";
        let got = parse_meminfo(text).expect("无冒号的行不该让整份解析失败");
        assert_eq!(got.total_mb, 0, "1000 kB 不足 1 MB，整除后是 0");
        assert_eq!(got.available_mb, 0);
    }

    /// 上报形状：`cpu_percent` / `mem_percent` 是**扁平数字**，因为 Hub 侧的
    /// `load_percent` 只读顶层字段（嵌套对象会让它恒返回 `None`，设备表那两列
    /// 就继续是 `—`——正是这条欠账要修的东西）；明细另放在 `load` / `memory` 里。
    #[test]
    fn the_reported_percentages_are_flat_numbers_the_hub_can_read() {
        let res = collect_resources();
        if Path::new(MEMINFO_PATH).exists() {
            let cpu = res.get("cpu_percent");
            let mem = res.get("mem_percent");
            for (k, v) in [("cpu_percent", cpu), ("mem_percent", mem)] {
                let v = v.unwrap_or_else(|| panic!("{k} 要上报：{res}"));
                assert!(v.is_i64(), "{k} 必须是扁平整数，Hub 才读得到：{v}");
                let n = v.as_i64().expect("i64");
                assert!((0..=10_000).contains(&n), "{k} 越界：{n}");
            }
            // 明细仍在，且不被百分比取代
            assert!(res["load"]["load1"].is_f64(), "明细要留着：{res}");
            assert!(res["memory"]["total_mb"].is_u64(), "明细要留着：{res}");
        } else {
            assert!(res.is_null(), "没有 /proc 就不许编：{res}");
        }
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
        // M2 起 `resources` 是真采集的：Linux 上有 `/proc` 就不为空。
        // 保留断言的形状但改成条件式——**目的是守住「不编造」**，而不是
        // 「必须为空」；后者在采集落地之后会变成一条阻止真事实的断言。
        if Path::new(MEMINFO_PATH).exists() {
            assert!(
                hb.resources.is_object(),
                "有 /proc 就该采到负载：{:?}",
                hb.resources
            );
        } else {
            assert!(hb.resources.is_null(), "采不到不许编一个");
        }
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
