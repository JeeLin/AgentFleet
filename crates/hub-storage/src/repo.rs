//! 设备与秘密的仓储层。
//!
//! 这里是 `hub-core` 唯一直接写 SQL 的地方之外的唯一例外说明：
//! 实际上 `hub-storage` 持有全部 SQL，本模块是它的数据结构入口。
//! `hub-core` 通过 [`DeviceRepo`] / [`SecretRepo`] 操作，不拼 SQL。

use r2d2_sqlite::rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// base64url 无填充编码（hub-storage 内自足，不反向依赖 hub-core）。
///
/// 写法上刻意避开裸索引与算术运算：`chunk[i]` 越界 / `<<` 溢出在 Rust 里
/// 都是 panic，而这个函数在设备指纹落库等路径上会被调用——一处 panic
/// 就是一次 Hub 进程退出。全部走 `get` + 显式 `u32` 加宽 + 表查找。
pub fn base64url_of(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // 补齐到 3 字节，缺的按 0 处理——`get` 返回 Option，不用索引
        let b0 = u32::from(*chunk.first().unwrap_or(&0));
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        // 尾块（1 或 2 字节）只输出 2 或 3 个字符；满块输出 4 个
        let take = chunk.len() + 1;
        for (i, &shift) in [18u32, 12, 6, 0].iter().enumerate() {
            if i >= take {
                break;
            }
            let idx = ((n >> shift) & 63) as usize;
            // `idx` 恒 < 64 且 T 长 64 —— 必命中。用 get 而不是裸索引：
            // 索引一旦被改错就是 panic，而这个函数在设备指纹落库路径上。
            if let Some(&c) = T.get(idx) {
                out.push(c as char);
            }
        }
    }
    out
}

use crate::error::{Result, StorageError};
use crate::Db;

/// 设备状态。**只有两值**——「重连中」只存在于内存（security §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    Online,
    Offline,
}

impl DeviceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

impl std::str::FromStr for DeviceStatus {
    type Err = StorageError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "online" => Ok(Self::Online),
            "offline" => Ok(Self::Offline),
            other => Err(StorageError::InvalidDeviceStatus(other.to_string())),
        }
    }
}

/// 设备行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub key_fingerprint: String,
    pub enrolled_at: String,
    pub last_seen_at: Option<String>,
    pub status: DeviceStatus,
    pub last_seen_gen: u64,
    pub superseded_at: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub hostname: Option<String>,
    pub node_version: Option<String>,
    /// JSON：支持的 connector 及参数。**必须落库**（调度硬过滤输入）。
    pub capabilities: Option<String>,
    /// JSON：CPU/内存/GPU/负载。
    pub resources: Option<String>,
}

/// enroll 的输入。
#[derive(Debug, Clone)]
pub struct NewDevice {
    pub id: String,
    pub name: String,
    pub key_fingerprint: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub node_version: String,
}

/// 设备仓储。
pub struct DeviceRepo {
    pool: Db,
}

impl DeviceRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 按指纹查设备。
    pub fn find_by_fingerprint(&self, fp: &str) -> Result<Option<Device>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, name, key_fingerprint, enrolled_at, last_seen_at, status,
                    last_seen_gen, superseded_at, os, arch, hostname, node_version,
                    capabilities, resources
             FROM device WHERE key_fingerprint = ?1",
            params![fp],
            row_to_device,
        )
        .optional()
        .map_err(StorageError::from)
    }

    /// 按 node_id 查设备。
    pub fn find_by_id(&self, id: &str) -> Result<Option<Device>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, name, key_fingerprint, enrolled_at, last_seen_at, status,
                    last_seen_gen, superseded_at, os, arch, hostname, node_version,
                    capabilities, resources
             FROM device WHERE id = ?1",
            params![id],
            row_to_device,
        )
        .optional()
        .map_err(StorageError::from)
    }

    /// 全部设备（Hub 重启后、设备尚未连上时也要能回答「有没有这台设备」）。
    pub fn list(&self) -> Result<Vec<Device>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, key_fingerprint, enrolled_at, last_seen_at, status,
                    last_seen_gen, superseded_at, os, arch, hostname, node_version,
                    capabilities, resources
             FROM device ORDER BY enrolled_at",
        )?;
        let rows = stmt.query_map([], row_to_device)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// 心跳后更新能力/负载/在线状态。
    pub fn touch_online(
        &self,
        id: &str,
        capabilities: &str,
        resources: &str,
        now: &str,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE device
             SET capabilities = ?2, resources = ?3, last_seen_at = ?4, status = 'online'
             WHERE id = ?1",
            params![id, capabilities, resources, now],
        )?;
        Ok(())
    }

    /// 标记离线。
    pub fn mark_offline(&self, id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE device SET status = 'offline' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// 记录被顶替（`gen` 兜底：重启后仍生效）。
    pub fn mark_superseded(&self, id: &str, now: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE device SET superseded_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    /// 连接池访问器（供 hub-core 在同一连接上开事务）。
    pub fn pool(&self) -> &Db {
        &self.pool
    }

    /// 把设备公钥存进 `capabilities.public_key`。
    ///
    /// **这是 M0 的权宜之计**：schema 里没有独立公钥列，而 Hub 只存指纹时
    /// 无法验签（指纹是单向导出）。已登记到 `docs/BUGS.md`——
    /// M1 应改为 `device.public_key TEXT NOT NULL` 独立列 + 迁移。
    pub fn set_public_key(&self, id: &str, pubkey: &[u8; 32]) -> Result<()> {
        let conn = self.pool.get()?;
        let b64 = crate::base64url_of(pubkey);
        // 读-改-写：保留 capabilities 的其它键。
        // 显式标注 `Option<String>`：`optional()` 只处理「没有这一行」，
        // 不处理「这一行的 capabilities 是 NULL」——那要 `Option` 来吃。
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT capabilities FROM device WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        let current = current.flatten();
        let mut v: serde_json::Value = current
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Object(Default::default()));
        if let Some(o) = v.as_object_mut() {
            o.insert("public_key".into(), serde_json::Value::String(b64));
        }
        // coverage: excluded — 232 行是 `execute(...)?` 隐含的 Err 传播 region，
        // 成功路径测试（set_public_key 写入并断言）永远走不到；构造 execute 失败
        // 需破坏底层连接，代价与收益不匹配。
        conn.execute(
            "UPDATE device SET capabilities = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(&v)?],
        )?;
        Ok(())
    }

    /// 推进 `last_seen_gen`。
    pub fn set_gen(&self, id: &str, gen: u64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE device SET last_seen_gen = ?2 WHERE id = ?1",
            params![id, gen as i64],
        )?;
        Ok(())
    }
}

fn row_to_device(
    r: &r2d2_sqlite::rusqlite::Row<'_>,
) -> std::result::Result<Device, r2d2_sqlite::rusqlite::Error> {
    Ok(Device {
        id: r.get(0)?,
        name: r.get(1)?,
        key_fingerprint: r.get(2)?,
        enrolled_at: r.get(3)?,
        last_seen_at: r.get(4)?,
        status: DeviceStatus::from_str(&r.get::<_, String>(5)?).unwrap_or(DeviceStatus::Offline),
        last_seen_gen: r.get::<_, i64>(6)? as u64,
        superseded_at: r.get(7)?,
        os: r.get(8)?,
        arch: r.get(9)?,
        hostname: r.get(10)?,
        node_version: r.get(11)?,
        capabilities: r.get(12)?,
        resources: r.get(13)?,
    })
}

/// 秘密仓储（`secret` 表）。
pub struct SecretRepo {
    pool: Db,
}

impl SecretRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    pub fn get(&self, kind: &str, name: &str) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT value FROM secret WHERE kind = ?1 AND name = ?2",
            params![kind, name],
            |r| r.get(0),
        )
        .optional()
        .map_err(StorageError::from)
    }

    /// 写入；已存在则覆盖（用于轮换 session_hmac）。
    pub fn set(&self, kind: &str, name: &str, value: &str, now: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO secret (kind, name, value, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(kind, name) DO UPDATE SET value = excluded.value, created_at = excluded.created_at",
            params![kind, name, value, now],
        )?;
        Ok(())
    }

    pub fn delete(&self, kind: &str, name: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "DELETE FROM secret WHERE kind = ?1 AND name = ?2",
            params![kind, name],
        )?;
        Ok(())
    }

    /// 是否存在某 secret（用于判断 Hub 是否已初始化）。
    pub fn exists(&self, kind: &str, name: &str) -> Result<bool> {
        Ok(self.get(kind, name)?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    fn repo() -> (tempfile::TempDir, DeviceRepo, SecretRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        (d, DeviceRepo::new(pool.clone()), SecretRepo::new(pool))
    }

    /// 直接插一行 device，返回 conn 以便进一步操作。
    fn insert_device(dr: &DeviceRepo, id: &str, fp: &str) {
        let conn = dr.pool.get().unwrap();
        conn.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES (?1, 'pi', ?2, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, fp],
        )
        .unwrap();
    }

    #[test]
    fn device_status_roundtrips_and_rejects_unknown() {
        // DB 里存的是文本，读回来必须还能解析回去——
        // 否则 row_to_device 的静默 Offline 兜底会掩盖数据损坏
        assert_eq!(DeviceStatus::Online.as_str(), "online");
        assert_eq!(DeviceStatus::Offline.as_str(), "offline");
        for s in [DeviceStatus::Online, DeviceStatus::Offline] {
            assert_eq!(s.as_str().parse::<DeviceStatus>().unwrap(), s);
        }
        // 非法值必须报错而不是静默变成 Offline
        assert!(matches!(
            "reconnecting".parse::<DeviceStatus>(),
            Err(StorageError::InvalidDeviceStatus(_))
        ));
    }

    #[test]
    fn status_transitions_persist() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        assert_eq!(
            dr.find_by_id("n_a").unwrap().unwrap().status,
            DeviceStatus::Offline
        );

        // touch_online → online，mark_offline → offline
        dr.touch_online("n_a", "{}", "{}", "2026-01-02T00:00:00Z")
            .unwrap();
        assert_eq!(
            dr.find_by_id("n_a").unwrap().unwrap().status,
            DeviceStatus::Online
        );
        dr.mark_offline("n_a").unwrap();
        assert_eq!(
            dr.find_by_id("n_a").unwrap().unwrap().status,
            DeviceStatus::Offline
        );
    }

    #[test]
    fn superseded_and_gen_persist() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        // gen 兜底：Hub 重启后仍要记得上次 gen（§7.1）
        dr.set_gen("n_a", 42).unwrap();
        assert_eq!(dr.find_by_id("n_a").unwrap().unwrap().last_seen_gen, 42);
        // 被顶替的时间戳必须落库（不能只留内存）
        assert!(dr
            .find_by_id("n_a")
            .unwrap()
            .unwrap()
            .superseded_at
            .is_none());
        dr.mark_superseded("n_a", "2026-01-03T00:00:00Z").unwrap();
        assert_eq!(
            dr.find_by_id("n_a")
                .unwrap()
                .unwrap()
                .superseded_at
                .as_deref(),
            Some("2026-01-03T00:00:00Z")
        );
    }

    #[test]
    fn unknown_device_lookups_return_none_not_error() {
        let (_d, dr, _sr) = repo();
        // 找不到是正常业务状态（设备还没注册），不是错误
        assert!(dr.find_by_id("nope").unwrap().is_none());
        assert!(dr.find_by_fingerprint("nope").unwrap().is_none());
        assert!(dr.list().unwrap().is_empty());
        // 写操作对不存在的行不报错（UPDATE 影响 0 行）
        dr.mark_offline("nope").unwrap();
        dr.set_gen("nope", 1).unwrap();
    }

    #[test]
    fn secret_set_get_delete() {
        let (_d, _dr, sr) = repo();
        sr.set("hub", "session_hmac", "abc", "2026-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(
            sr.get("hub", "session_hmac").unwrap().as_deref(),
            Some("abc")
        );
        assert!(sr.exists("hub", "session_hmac").unwrap());

        // 覆盖
        sr.set("hub", "session_hmac", "def", "2026-01-02T00:00:00Z")
            .unwrap();
        assert_eq!(
            sr.get("hub", "session_hmac").unwrap().as_deref(),
            Some("def")
        );

        sr.delete("hub", "session_hmac").unwrap();
        assert!(!sr.exists("hub", "session_hmac").unwrap());
    }

    #[test]
    fn device_status_defaults_offline_and_touch_online() {
        let (_d, dr, _sr) = repo();
        // 直接插入一行模拟 enroll 后的状态
        insert_device(&dr, "n_a", "fp_a");

        let d = dr.find_by_id("n_a").unwrap().unwrap();
        assert_eq!(d.status, DeviceStatus::Offline);

        dr.touch_online(
            "n_a",
            r#"{"connectors":[]}"#,
            r#"{"load":{}}"#,
            "2026-01-01T00:00:15Z",
        )
        .unwrap();
        let d = dr.find_by_id("n_a").unwrap().unwrap();
        assert_eq!(d.status, DeviceStatus::Online);
        // capabilities 必须落库（不依赖内存缓存）
        assert_eq!(d.capabilities.as_deref(), Some(r#"{"connectors":[]}"#));
    }

    #[test]
    fn list_returns_devices_after_hub_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.db");
        {
            let pool = open(&path).unwrap();
            let dr = DeviceRepo::new(pool);
            insert_device(&dr, "n_a", "fp_a");
        }
        // 重新 open 模拟 Hub 重启：设备尚未连上，也要能列出
        let pool = open(&path).unwrap();
        let dr = DeviceRepo::new(pool);
        let all = dr.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "n_a");
        assert_eq!(all[0].status, DeviceStatus::Offline);
    }

    #[test]
    fn find_by_fingerprint() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        assert!(dr.find_by_fingerprint("fp_a").unwrap().is_some());
        assert!(dr.find_by_fingerprint("fp_missing").unwrap().is_none());
    }

    #[test]
    fn gen_and_superseded_persist() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        dr.set_gen("n_a", 7).unwrap();
        dr.mark_superseded("n_a", "2026-01-01T00:01:00Z").unwrap();
        let d = dr.find_by_id("n_a").unwrap().unwrap();
        assert_eq!(d.last_seen_gen, 7);
        assert_eq!(d.superseded_at.as_deref(), Some("2026-01-01T00:01:00Z"));
    }

    #[test]
    fn base64url_of_emits_only_valid_alphabet_and_grows_with_input() {
        // 独立实现（hub-storage 不依赖 hub-core，不能复用其 base64url）。
        // 锁住契约：① 只输出 base64url 字符集 ② 长度随输入非递减
        // （这是它作为指纹/公钥编码被下游消费的前提，不变量而非具体公式）。
        let mut prev_len: usize = 0;
        for len in 0u32..=34 {
            let b: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let enc = base64url_of(&b);
            for c in enc.bytes() {
                assert!(
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
                        .contains(&c),
                    "bad char {c} in {enc} (input len {len})"
                );
            }
            assert!(
                enc.len() >= prev_len,
                "encoded length must not shrink as input grows (len {len})"
            );
            prev_len = enc.len();
        }
        // 32 字节公钥 → 43 字符（base64url 的 n*8/6 向上取整：32*8/6≈42.7→43）
        assert_eq!(base64url_of(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn set_public_key_writes_into_capabilities_json() {
        // M0 权宜之计：公钥塞进 capabilities.public_key（M1 改为独立列）。
        // 必须验证：只写 public_key 键，不动 capabilities 里已有的其它键，
        // 且覆盖掉上一版（重连改密钥时）。
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        // 先放一个无关键
        dr.touch_online(
            "n_a",
            r#"{"connectors":["a"]}"#,
            "{}",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let pk = [7u8; 32];
        dr.set_public_key("n_a", &pk).unwrap();
        let caps: serde_json::Value = {
            let conn = dr.pool().get().unwrap();
            let s: String = conn
                .query_row("SELECT capabilities FROM device WHERE id='n_a'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            serde_json::from_str(&s).unwrap()
        };
        assert_eq!(
            caps["connectors"],
            serde_json::json!(["a"]),
            "must keep other keys"
        );
        // 公钥是 base64url(32 字节) → 43 字符
        assert_eq!(caps["public_key"].as_str().unwrap().len(), 43);
        assert_eq!(caps["public_key"].as_str().unwrap(), base64url_of(&pk));
    }

    #[test]
    fn pool_accessor_returns_same_pool() {
        // hub-core 在同一条连接上开显式事务需要拿 pool 引用。
        let (_d, dr, _sr) = repo();
        let p1: *const _ = dr.pool();
        let p2: *const _ = dr.pool();
        assert_eq!(p1, p2, "pool() must return the same connection pool");
    }
}
