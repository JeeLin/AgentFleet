//! **能力上报与 connector 注册表必须永远一致**（子任务 11，约束 9 / 14）。
//!
//! 这不是「再测一遍注册表」——`connector/mod.rs` 里的单元测试已经验过
//! `create` 与 `registered_names` 互相印证。这里钉的是**跨模块那条链**：
//! `heartbeat::capabilities()` 导出的东西，是不是就是注册表里的东西。
//!
//! 链子断在哪一环，症状都很难查：
//! - **注册表加了、上报没跟上** → 用户看不到一个明明可用的能力；
//! - **上报了、注册表没加** → 约束 9 要求路由按上报能力做**硬过滤**，于是
//!   任务被派给一台根本做不到的机器，页面上只剩一句「没回音」。
//!
//! M0 的心跳正是在第二条上栽的：硬编码报着 `terminal/files/git/exec`，
//! 而四个当时一个都没实现。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use node_core::connector;
use node_core::heartbeat;

/// 心跳上报的 connector 名。
fn reported() -> Vec<String> {
    let caps = heartbeat::capabilities();
    caps["connectors"]
        .as_array()
        .expect("connectors 是数组")
        .iter()
        .map(|v| v.as_str().expect("connector 名是字符串").to_string())
        .collect()
}

/// **这条是主测试**：上报的集合**恒等于**注册表的集合。
///
/// 刻意不写死名字列表——写死的话，「注册表加了第四个 connector 而上报
/// 忘了」只要顺手改一行断言就又绿了，而那正是这条测试要抓的东西。
/// 集合相等是那条唯一的不变式。
#[test]
fn the_reported_capabilities_are_exactly_the_registry() {
    assert_eq!(
        reported(),
        connector::registered_names(),
        "上报的能力表与注册表漂移了：路由会按这份表做硬过滤"
    );
}

/// 反向：注册表里的每一个名字都**真造得出实例**。
///
/// 「上报了却建不出来」是能力表撒谎的另一种形状——Hub 把任务派过来，
/// `open_channel` 回 `channel_error{unsupported}`。
#[test]
fn every_reported_name_builds_a_real_connector() {
    for name in reported() {
        assert!(connector::supports(&name), "{name} 上报了却不在注册表");
        assert!(
            connector::create(&name).is_some(),
            "{name} 上报了却造不出实例"
        );
    }
}

/// **M0 栽过的那一跤必须不再发生**：`git` 一次也没实现过，
/// 所以它**永远不该出现在上报里**。
#[test]
fn a_connector_that_was_never_implemented_is_never_reported() {
    for phantom in ["git", "ssh", "db", "s3"] {
        assert!(
            !reported().iter().any(|n| n == phantom),
            "{phantom} 从来没实现过，上报它等于把任务派给做不到的机器"
        );
    }
}

/// 数量对得上：**注册表 → 上报这条路真的连着**，不是两边各写一份
/// 恰好数字相同的清单。
#[test]
fn the_reported_count_tracks_the_registry_count() {
    assert_eq!(
        reported().len(),
        connector::registered_count(),
        "「注册表 → 上报」这条路断了"
    );
}

/// 排序：同一台设备每次心跳的字段要能 diff（`scheduling.md` §2）。
#[test]
fn the_reported_list_is_sorted_so_heartbeats_diff_cleanly() {
    let got = reported();
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted, "上报顺序不稳定，联调时的 diff 读不懂");
}

/// **`agents` 保持空**：M1 没有任何 agent 运行时可探测（ACP 归 M3）。
///
/// 这条锁住的是「**未探测到就上报空，不得编造**」——不是「忘了填」。
/// 填一个「就绪」会让 Hub 把任务派给一台根本没装那个 agent 的机器。
#[test]
fn agents_are_reported_empty_when_nothing_is_detected() {
    let caps = heartbeat::capabilities();
    let agents = caps["agents"].as_array().expect("agents 是数组");
    assert!(
        agents.is_empty(),
        "M1 没有 agent 运行时；上报一个编出来的「就绪」正是子任务 11 要修的那类撒谎：{agents:?}"
    );
}

/// 组出来的心跳**带着这份能力表**——能力不是在某个旁路上算的，
/// 真实的心跳帧里就是它。
#[test]
fn a_built_heartbeat_carries_the_registry_derived_capabilities() {
    let hb = heartbeat::heartbeat(vec!["/srv/ws".to_string()]);
    assert_eq!(
        hb.capabilities["connectors"].as_array().map(Vec::len),
        Some(connector::registered_count()),
        "心跳帧里的能力表与注册表对不上"
    );
    // 必须能过协议层序列化——形状错了 Hub 会当成坏帧，而症状是
    // 「心跳一来就被断开」，与真相相距很远。
    let text = serde_json::to_string(&hb).expect("serialize");
    let back: protocol::message::Heartbeat = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back.capabilities, hb.capabilities);
}

/// **能力表是路由的硬过滤输入**，所以它必须能被 Hub 侧那套口径读懂：
/// 数组里的每一项都是非空字符串。这是「上报 = 一份可以被机器读的承诺」
/// 那一层的最后一道检查。
#[test]
fn every_reported_entry_is_a_non_empty_machine_readable_name() {
    for name in reported() {
        assert!(
            !name.is_empty(),
            "空的 connector 名会让 Hub 的过滤永远匹配不上"
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{name} 不该带空格或标点"
        );
    }
}
