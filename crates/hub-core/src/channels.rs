//! Hub 侧的 per-node 通道路由表：`node_id → 该设备的 [`ChannelRegistry`]`
//!（`protocol.md` §5）。
//!
//! 通道键是 `(node_id, channel_id)`（约束 4）——**`channel_id` 只在设备内唯一**，
//! 所以 Hub 必须按 `node_id` 分表，否则两台设备的 `ch-1` 会互相踩。
//! 状态机与信用账本本身在 `protocol::channel` 里（纯逻辑、两侧共用），这里
//! **只加 node_id 那一层**，不复制它的逻辑。
//!
//! **整节点记录是连接生命周期的一部分**：断开/被顶替时整表丢弃（§7.1）。
//! 这不是内存优化，而是信用重置的**唯一入口**——只要旧记录还在，
//! 重连后按 `channel_id` 记账就会累积出一个恒 > 0 的信用余额，等于没有流控（§6）。

use std::collections::HashMap;

use protocol::channel::ChannelRegistry;

/// 全部节点的通道路由表。纯内存，不落库（§5）。
#[derive(Debug, Default)]
pub struct HubChannels {
    per_node: HashMap<String, ChannelRegistry>,
}

impl HubChannels {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取某节点的通道表，**不存在时建空表**。唯一的可变入口。
    pub fn registry(&mut self, node_id: &str) -> &mut ChannelRegistry {
        self.per_node.entry(node_id.to_string()).or_default()
    }

    /// 查某条通道的剩余信用。
    ///
    /// **未知 node_id 不是错误**：通道 id 可能被对端弄脏，报「节点不存在」
    /// 会让调用方误以为节点下线了。
    pub fn credit(&self, node_id: &str, channel_id: &str) -> Option<u32> {
        self.per_node
            .get(node_id)
            .and_then(|reg| reg.credit(channel_id))
    }

    /// 丢弃某节点的全部通道记录（连接断开/被顶替，§7.1）。
    ///
    /// 返回被清掉的**通道**条数（不是「删了几个 `node_id` 键」：一个节点
    /// 的那张表里有几条，就报几条）。这是重连后信用从零重算的唯一入口。
    pub fn drop_node(&mut self, node_id: &str) -> usize {
        self.per_node.remove(node_id).map_or(0, |reg| reg.len())
    }
}

/// 从一张扁平账本里摘掉属于 `node_id` 的记录，返回摘掉条数。
///
/// **「按 node 清账」的第二张表必须走这里**（第一张是 [`HubChannels::drop_node`]，
/// 它按 `node_id` 分表所以不需要筛）。两处各写一遍 `retain` 时，「新增第三张
/// 按 node 索引的表」就会漏掉其中一张——漏的那张留着一台已下线设备的记录，
/// 于是 `close_channel` 会对一条设备侧早已不存在的通道走完归属校验。
///
/// **返回值口径**：这里是「几条记录」，而 `HubChannels::drop_node` 是「几条
/// 通道」。两者不必一致——各表的调用方只看自己那张表。
pub fn retain_other_nodes<K, V>(
    map: &mut HashMap<K, V>,
    belongs_to_node: impl Fn(&K, &V) -> bool,
) -> usize
where
    K: Eq + std::hash::Hash,
{
    let before = map.len();
    map.retain(|k, v| !belongs_to_node(k, v));
    before - map.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::channel::{ChannelAction, ChannelState, CREDIT_START, READY_TIMEOUT};
    use std::time::Instant;

    fn t0() -> Instant {
        Instant::now()
    }

    /// 开一条通道并置 Ready——`HubChannels` 只加 node_id 那一层，
    /// 记账与状态转移全在 `protocol::channel` 里。
    fn ready(h: &mut HubChannels, node_id: &str, connector: &str, now: Instant) {
        h.registry(node_id)
            .open("ch-1", connector, CREDIT_START, now)
            .unwrap();
        h.registry(node_id).mark_ready("ch-1").unwrap();
    }

    #[test]
    fn channel_ids_are_scoped_per_node() {
        // 约束 4：channel_id 只在设备内唯一，不是全局 id
        let t = t0();
        let mut h = HubChannels::new();
        ready(&mut h, "n_a", "terminal", t);
        ready(&mut h, "n_b", "files", t);
        assert_eq!(h.credit("n_a", "ch-1"), Some(CREDIT_START));
        assert_eq!(h.credit("n_b", "ch-1"), Some(CREDIT_START));
        assert_eq!(h.credit("n_c", "ch-1"), None);

        // 信用按通道独立，一个节点耗尽不动另一个
        for _ in 0..CREDIT_START {
            h.registry("n_a").on_data("ch-1").unwrap();
        }
        assert_eq!(h.credit("n_a", "ch-1"), Some(0));
        assert!(matches!(
            h.registry("n_a").on_data("ch-1").unwrap(),
            ChannelAction::Suspend { .. }
        ));
        assert_eq!(
            h.credit("n_b", "ch-1"),
            Some(CREDIT_START),
            "§6：信用不跨节点共享"
        );
    }

    /// 清账的判据只看 node，不看表的形状——这条锁的是那个共用实现。
    #[test]
    fn retain_other_nodes_drops_only_the_named_node_and_counts_what_it_dropped() {
        let mut flat: HashMap<&str, &str> = HashMap::from([("ch_a", "n_1"), ("ch_b", "n_2")]);
        assert_eq!(retain_other_nodes(&mut flat, |_, node| *node == "n_1"), 1);
        assert_eq!(flat.len(), 1);
        assert_eq!(retain_other_nodes(&mut flat, |_, node| *node == "n_1"), 0);
    }

    #[test]
    fn drop_node_resets_credit_accounting_on_reconnect() {
        // §6 末条：重连后 Hub 必须重新 open_channel，否则按 channel_id 记账
        // 会累积出一个无限大的信用余额
        let t = t0();
        let mut h = HubChannels::new();
        ready(&mut h, "n_a", "terminal", t);
        h.registry("n_a").on_data("ch-1").unwrap();
        assert_eq!(h.credit("n_a", "ch-1"), Some(CREDIT_START - 1));

        // 断线：整节点记录丢弃
        assert_eq!(h.drop_node("n_a"), 1);
        assert_eq!(h.credit("n_a", "ch-1"), None);

        // 重连：重新 open_channel 才是记账入口
        let later = t + READY_TIMEOUT * 2;
        ready(&mut h, "n_a", "terminal", later);
        assert_eq!(h.credit("n_a", "ch-1"), Some(CREDIT_START));
        assert_eq!(h.registry("n_a").state("ch-1"), Some(ChannelState::Ready));
    }

    #[test]
    fn drop_node_is_idempotent_and_untouches_unknown() {
        let mut h = HubChannels::new();
        assert_eq!(h.drop_node("n_missing"), 0);
        ready(&mut h, "n_a", "terminal", t0());
        assert_eq!(h.drop_node("n_a"), 1);
        assert_eq!(h.drop_node("n_a"), 0);
    }

    #[test]
    fn open_rejects_a_bad_channel_id_without_leaving_a_half_record() {
        let mut h = HubChannels::new();
        assert!(h.registry("n_a").open("", "terminal", 1, t0()).is_err());
        // 拒绝的 open 不得留下半条通道；节点表本身留着（无副作用的惰性建表）
        assert!(h.registry("n_a").is_empty());
        // 空 channel_id 之后照常能用
        ready(&mut h, "n_a", "terminal", t0());
        assert_eq!(h.credit("n_a", "ch-1"), Some(CREDIT_START));
    }

    #[test]
    fn unknown_node_is_not_an_error_for_queries() {
        let h = HubChannels::new();
        // 通道 id 可能被对端弄脏：查不到就答「没有」，不得报成节点不存在
        assert_eq!(h.credit("n_a", "ch-1"), None);
    }

    #[test]
    fn empty_registry_is_created_on_first_touch() {
        let mut h = HubChannels::new();
        assert!(h.registry("n_a").is_empty());
    }

    /// 同一台设备的两张通道表互不相干之外，**清一台不能动另一台**——
    /// `drop_node` 返回被清条数，这个数是顶替路径上「到底清了什么」的留痕。
    #[test]
    fn dropping_one_node_leaves_the_others_intact() {
        let t = t0();
        let mut h = HubChannels::new();
        h.registry("n_a").open("ch_a", "terminal", 2, t).unwrap();
        h.registry("n_a").open("ch_b", "files", 3, t).unwrap();
        ready(&mut h, "n_b", "terminal", t);

        assert_eq!(h.drop_node("n_a"), 2, "n_a 有两条记录要被清掉");
        assert_eq!(h.credit("n_a", "ch_a"), None);
        assert_eq!(
            h.credit("n_b", "ch-1"),
            Some(CREDIT_START),
            "n_b 不能被连坐"
        );
    }
}
