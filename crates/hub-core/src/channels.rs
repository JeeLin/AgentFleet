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
use std::time::Instant;

use protocol::channel::{Channel, ChannelRegistry, ChannelState};

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

    /// 只读查一条通道。
    pub fn get(&self, node_id: &str, channel_id: &str) -> Option<&Channel> {
        self.per_node
            .get(node_id)
            .and_then(|reg| reg.get(channel_id))
    }

    /// 查某条通道的状态。**未知 node_id 不是错误**——通道可能是刚被对端
    /// 弄脏的 id，报「节点不存在」会让调用方误以为节点下线了。
    pub fn state(&self, node_id: &str, channel_id: &str) -> Option<ChannelState> {
        self.per_node
            .get(node_id)
            .and_then(|reg| reg.state(channel_id))
    }

    /// 查某条通道的剩余信用。
    pub fn credit(&self, node_id: &str, channel_id: &str) -> Option<u32> {
        self.per_node
            .get(node_id)
            .and_then(|reg| reg.credit(channel_id))
    }

    /// 该节点是否已有通道表（用于区分「没开过通道」与「记录被摘掉了」）。
    pub fn has_node(&self, node_id: &str) -> bool {
        self.per_node.contains_key(node_id)
    }

    pub fn node_count(&self) -> usize {
        self.per_node.len()
    }

    /// 丢弃某节点的全部通道记录（连接断开/被顶替，§7.1）。
    ///
    /// 返回被清掉的记录条数。这是重连后信用从零重算的唯一入口。
    pub fn drop_node(&mut self, node_id: &str) -> usize {
        self.per_node.remove(node_id).map_or(0, |reg| reg.len())
    }
}

/// 批量打开的通道描述：一个 `open_channel` 要落下的东西。
///
/// 参数带 `credit_start` 而不是让它走 [`OpenChannel`] 的 serde 缺省，
/// 是因为**两处默认值会漂移**：这里漏填就变成 0（永远挂起），
/// 而协议缺省是 [`protocol::channel::CREDIT_START`]。
pub struct OpenSpec {
    pub channel_id: String,
    pub connector: String,
    pub credit_start: u32,
}

impl HubChannels {
    /// 开一条通道，等价于发 `open_channel` + 记账。
    pub fn open_channel(
        &mut self,
        node_id: &str,
        spec: &OpenSpec,
        now: Instant,
    ) -> Result<(), protocol::channel::ChannelError> {
        self.registry(node_id)
            .open(&spec.channel_id, &spec.connector, spec.credit_start, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::channel::{ChannelAction, ChannelState, CREDIT_START, READY_TIMEOUT};

    fn t0() -> Instant {
        Instant::now()
    }

    fn spec(id: &str, connector: &str) -> OpenSpec {
        OpenSpec {
            channel_id: id.to_string(),
            connector: connector.to_string(),
            credit_start: CREDIT_START,
        }
    }

    #[test]
    fn channel_ids_are_scoped_per_node() {
        // 约束 4：channel_id 只在设备内唯一，不是全局 id
        let t = t0();
        let mut h = HubChannels::new();
        h.open_channel("n_a", &spec("ch-1", "terminal"), t).unwrap();
        h.open_channel("n_b", &spec("ch-1", "files"), t).unwrap();

        assert_eq!(h.get("n_a", "ch-1").unwrap().connector(), "terminal");
        assert_eq!(h.get("n_b", "ch-1").unwrap().connector(), "files");
        assert!(h.get("n_c", "ch-1").is_none());
        assert_eq!(h.node_count(), 2);

        // 同一节点内的通道彼此独立：一个发完不动另一个
        h.registry("n_a").mark_ready("ch-1").unwrap();
        h.registry("n_b").mark_ready("ch-1").unwrap();
        h.registry("n_b").on_data("ch-1").unwrap();
        for _ in 0..CREDIT_START {
            h.registry("n_a").on_data("ch-1").unwrap();
        }
        assert_eq!(h.credit("n_a", "ch-1"), Some(0));
        assert!(matches!(
            h.registry("n_a").on_data("ch-1").unwrap(),
            ChannelAction::Suspend { .. }
        ));
        // 信用按通道独立，不跨通道共享（§6）
        assert_eq!(h.credit("n_b", "ch-1"), Some(CREDIT_START - 1));
    }

    #[test]
    fn drop_node_resets_credit_accounting_on_reconnect() {
        // §6 末条：重连后 Hub 必须重新 open_channel，否则按 channel_id 记账
        // 会累积出一个无限大的信用余额
        let t = t0();
        let mut h = HubChannels::new();
        h.open_channel("n_a", &spec("ch-1", "terminal"), t).unwrap();
        h.registry("n_a").mark_ready("ch-1").unwrap();
        h.registry("n_a").on_data("ch-1").unwrap();
        assert_eq!(h.credit("n_a", "ch-1"), Some(CREDIT_START - 1));

        // 断线：整节点记录丢弃
        assert_eq!(h.drop_node("n_a"), 1);
        assert!(!h.has_node("n_a"));
        assert_eq!(h.state("n_a", "ch-1"), None);
        assert_eq!(h.credit("n_a", "ch-1"), None);
        assert_eq!(h.node_count(), 0);

        // 重连：重新 open_channel 才是记账入口
        let later = t + READY_TIMEOUT * 2;
        h.open_channel("n_a", &spec("ch-1", "terminal"), later)
            .unwrap();
        assert_eq!(h.state("n_a", "ch-1"), Some(ChannelState::Opening));
        assert_eq!(h.credit("n_a", "ch-1"), Some(CREDIT_START));
        assert!(!h.registry("n_a").ready_timed_out("ch-1", later));
    }

    #[test]
    fn drop_node_is_idempotent_and_untouches_unknown() {
        let mut h = HubChannels::new();
        assert_eq!(h.drop_node("n_missing"), 0);
        h.open_channel("n_a", &spec("ch-1", "terminal"), t0())
            .unwrap();
        assert_eq!(h.drop_node("n_a"), 1);
        assert_eq!(h.drop_node("n_a"), 0);
    }

    #[test]
    fn open_channel_propagates_registry_errors() {
        let mut h = HubChannels::new();
        let bad = OpenSpec {
            channel_id: String::new(),
            connector: "terminal".to_string(),
            credit_start: 1,
        };
        assert!(h.open_channel("n_a", &bad, t0()).is_err());
        // 拒绝的 open 不得留下半条通道；节点表本身留着（无副作用的惰性建表）
        assert!(h.registry("n_a").is_empty());
        assert_eq!(h.node_count(), 1);
        // 空 channel_id 之后照常能用
        h.open_channel("n_a", &spec("ch-1", "terminal"), t0())
            .unwrap();
        assert_eq!(h.get("n_a", "ch-1").unwrap().connector(), "terminal");
    }

    #[test]
    fn unknown_node_is_not_an_error_for_queries() {
        let h = HubChannels::new();
        // 通道 id 可能被对端弄脏：查不到就答「没有」，不得报成节点不存在
        assert!(!h.has_node("n_a"));
        assert!(h.get("n_a", "ch-1").is_none());
        assert_eq!(h.state("n_a", "ch-1"), None);
        assert_eq!(h.credit("n_a", "ch-1"), None);
    }

    #[test]
    fn empty_registry_is_created_on_first_touch() {
        let mut h = HubChannels::new();
        assert!(h.registry("n_a").is_empty());
        assert!(h.has_node("n_a"), "只读式访问也要留下节点表");
    }
}
