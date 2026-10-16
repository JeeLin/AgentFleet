//! 连接注册表：`node_id → 连接句柄`，**仅内存，不持久化 socket**。
//!
//! 关键不变量（`protocol.md` §7.1）：
//! - 顶替时**先换表、后通知旧连接**——顺序反了会让消息路由到已关闭的句柄。
//! - 断开处理器**必须先校验 `conn == registry.get(node_id)` 再摘表**，
//!   否则已关闭的连接会把 status 改回 offline，覆盖新连接的 online。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// 起点是 `boot_gen` 而非 0，所以 `fetch_add` 自带单调性，无须额外取最大值。
    gen_counter: Arc<AtomicU64>,
}

impl ConnectionRegistry {
    /// 全新进程：库是空的，gen 从 1 起。
    pub fn new() -> Self {
        Self::with_boot_gen(0)
    }

    /// 用「库里的最大 `last_seen_gen`」兜住计数器的起点。
    ///
    /// 没有这个下界，Hub 每次重启都会把 `gen` 归零，而设备记得上一进程的
    /// `last_seen_gen`（§7.1 的 gen 兜底要靠它）。设备带着 42 重连、Hub 只发出 1
    /// → 每次都被判成「已被顶替」→ **设备永远接不上**，且没有自愈路径。
    /// 这正是子任务 7 测试标准⑧的反面：进程重启后，兜底机制把设备锁在门外。
    ///
    /// 抬高起点而不是每次 `max` 是必需的：`max` 会让计数器停在 1、每次都返回 42，
    /// 两条连接共用同一个 `gen`，顶替判定随之失效。
    pub fn with_boot_gen(boot_gen: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            gen_counter: Arc::new(AtomicU64::new(boot_gen)),
        }
    }

    /// 分配下一个 `gen`。
    ///
    /// `&self` 而不是 `&mut self`：注册表由 `AppState` 以 `Arc` 共享，
    /// 而每条新连接都要取号。要求 `&mut` 会让共享注册表变成不可能
    /// （或者逼着调用方在 `RwLock` 外面再套一层 `Mutex`，多一把锁多一个死锁面）。
    pub fn next_gen(&self) -> u64 {
        self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current_gen(&self) -> u64 {
        self.gen_counter.load(Ordering::Relaxed)
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
        let r = ConnectionRegistry::new();
        assert_eq!(r.next_gen(), 1);
        assert_eq!(r.next_gen(), 2);
        assert_eq!(r.next_gen(), 3);
        assert_eq!(r.current_gen(), 3);
    }

    #[test]
    fn gen_is_unique_across_concurrent_takers() {
        // `next_gen` 走 `&self` 正是为了「注册表被 Arc 共享时仍能取号」。
        // 多个线程同时取号，拿到的值必须两两不同（fetch_add 的保证）。
        let r = Arc::new(ConnectionRegistry::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let r = Arc::clone(&r);
            handles.push(std::thread::spawn(move || r.next_gen()));
        }
        let mut got: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6, 7, 8]);
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

    /// Hub 重启后设备仍带着上一进程的 `last_seen_gen` 重连：新进程从 1 起号，
    /// 设备会被 `auth_fail{superseded}` 永久拒在门外。`boot_gen` 是唯一的解法。
    #[test]
    fn gen_never_falls_back_below_the_stored_max() {
        let r = ConnectionRegistry::with_boot_gen(42);
        assert_eq!(r.next_gen(), 43, "不能回到 1");
        assert_eq!(r.next_gen(), 44);
    }

    /// 并发取号在抬高起点之后仍必须两两不同——这正是不能用 `max()` 兜底的原因。
    #[test]
    fn boot_gen_does_not_break_gen_uniqueness_under_concurrency() {
        let r = Arc::new(ConnectionRegistry::with_boot_gen(42));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let r = Arc::clone(&r);
            handles.push(std::thread::spawn(move || r.next_gen()));
        }
        let mut got: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort_unstable();
        assert_eq!(got, vec![43, 44, 45, 46, 47, 48, 49, 50]);
    }

    #[test]
    fn boot_gen_of_zero_behaves_like_a_fresh_registry() {
        let r = ConnectionRegistry::with_boot_gen(0);
        assert_eq!(r.next_gen(), 1);
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
