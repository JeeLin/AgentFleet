//! 连接注册表：`node_id → 连接句柄`，**仅内存，不持久化 socket**。
//!
//! 关键不变量（`protocol.md` §7.1）：
//! - 顶替时**先换表、后通知旧连接**——顺序反了会让消息路由到已关闭的句柄。
//! - 断开处理器**必须先校验 `conn == registry.get(node_id)` 再摘表**，
//!   否则已关闭的连接会把 status 改回 offline，覆盖新连接的 online。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// 一条连接的句柄。只暴露路由所需的最小信息——**不含 socket**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnHandle {
    /// 连接 id（进程内唯一，不是 node_id）。
    pub conn_id: String,
    /// 该连接所属的连接代次。单调递增。
    pub gen: u64,
}

/// 内存连接注册表。
#[derive(Debug, Default)]
pub struct ConnectionRegistry {
    inner: Arc<RwLock<HashMap<String, ConnHandle>>>,
    gen_counter: u64,
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 分配下一个 `gen`。
    pub fn next_gen(&mut self) -> u64 {
        self.gen_counter += 1;
        self.gen_counter
    }

    pub fn current_gen(&self) -> u64 {
        self.gen_counter
    }

    pub fn insert(&self, node_id: String, handle: ConnHandle) -> Option<ConnHandle> {
        self.inner.write().insert(node_id, handle)
    }

    pub fn get(&self, node_id: &str) -> Option<ConnHandle> {
        self.inner.read().get(node_id).cloned()
    }

    /// 仅当该连接仍是当前连接时才摘除。
    ///
    /// 返回 `true` 表示确实摘除了（调用方可以安全地把 node 标记为 offline）；
    /// 返回 `false` 表示该连接已被顶替，**不得**改状态。
    pub fn remove_if_current(&self, node_id: &str, conn_id: &str) -> bool {
        let mut g = self.inner.write();
        match g.get(node_id) {
            Some(h) if h.conn_id == conn_id => {
                g.remove(node_id);
                true
            }
            // None（已被摘）或 Some(其它 conn_id)（已被顶替）→ 不动
            _ => false,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// 全部 node_id（供 Hub 重启后的对账）。
    pub fn node_ids(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    // 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
    // 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
    // let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
    // 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::indexing_slicing
    )]
    use super::*;

    fn h(conn: &str, gen: u64) -> ConnHandle {
        ConnHandle {
            conn_id: conn.into(),
            gen,
        }
    }

    #[test]
    fn insert_and_get() {
        let r = ConnectionRegistry::new();
        assert!(r.get("n_a").is_none());
        r.insert("n_a".into(), h("c1", 1));
        assert_eq!(r.get("n_a").unwrap().conn_id, "c1");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn takeover_returns_old_handle() {
        let r = ConnectionRegistry::new();
        r.insert("n_a".into(), h("c1", 1));
        let old = r.insert("n_a".into(), h("c2", 2));
        assert_eq!(old.unwrap().conn_id, "c1");
        assert_eq!(r.get("n_a").unwrap().conn_id, "c2");
    }

    #[test]
    fn gen_is_monotonic() {
        let mut r = ConnectionRegistry::new();
        assert_eq!(r.next_gen(), 1);
        assert_eq!(r.next_gen(), 2);
        assert_eq!(r.current_gen(), 2);
    }

    #[test]
    fn remove_if_current_guards_against_stale_disconnect() {
        let r = ConnectionRegistry::new();
        r.insert("n_a".into(), h("c1", 1));
        // c2 顶替 c1
        r.insert("n_a".into(), h("c2", 2));
        // c1 的断开处理器后跑：不得摘表、不得改状态
        assert!(!r.remove_if_current("n_a", "c1"));
        assert_eq!(r.get("n_a").unwrap().conn_id, "c2");

        // c2 自己断开：可以摘
        assert!(r.remove_if_current("n_a", "c2"));
        assert!(r.is_empty());
    }

    #[test]
    fn remove_if_current_on_missing_is_false() {
        let r = ConnectionRegistry::new();
        assert!(!r.remove_if_current("n_missing", "c1"));
    }

    #[test]
    fn node_ids_listed() {
        let r = ConnectionRegistry::new();
        r.insert("n_a".into(), h("c1", 1));
        r.insert("n_b".into(), h("c2", 1));
        let mut ids = r.node_ids();
        ids.sort();
        assert_eq!(ids, vec!["n_a", "n_b"]);
    }
}
