//! 设备与秘密的仓储层。
//!
//! `hub-storage` 持有全部 SQL；本模块是设备与 secret 的数据结构入口。
//! `hub-core` 通过 [`DeviceRepo`] / [`SecretRepo`] 操作，不拼 SQL。

use r2d2_sqlite::rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::Db;

/// base64url 无填充编码。与 [`identity::base64url`] 同一份实现。
///
/// 设备指纹、公钥落库都走这里，而 Hub 侧解析同一串用的是
/// `identity::base64url_decode`——两份编解码器只会在「哪个字符算非法」上分叉，
/// 届时存的公钥永远解不出来。
pub fn base64url_of(bytes: &[u8]) -> String {
    identity::base64url(bytes)
}

/// base64url 解码。**只认无填充的 URL-safe 变体**，长度不是 4 的倍数就拒
/// （解码失败返回 `None`）。
///
/// 公钥落库与出库都走它：库里的 `device.public_key` 可能来自 v2 迁移的
/// `json_extract` 回填，也可能来自设备 enroll 时上传的字符串，两条来路必须用
/// 同一个判据，否则「迁移回填进来的公钥验不过签」会表现成一个查不出源头的问题。
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    identity::base64url_decode(s).ok()
}

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
    /// 设备公钥（base64url 的 32 字节）。M1 起是**真列**——M0 没有这一列，
    /// 只能寄存在 `capabilities` 的 `public_key` 键里，而「心跳合并必须保住它」
    /// 成了一条只有注释守着的约定（见 `DeviceRepo::set_public_key`）。
    pub public_key: Option<String>,
}

impl Device {
    /// 库内公钥（解码成 32 字节原始密钥）。
    ///
    /// **列里没有就是没有**：绝不退回 `capabilities` 的旧键去凑——v2 迁移已把
    /// 存量回填过，而降级二进制会被 `SchemaTooNew` 挡在开库之前，
    /// 不存在「只有 JSON 里有公钥」的合法库。解码失败与长度不对同样返回 `None`，
    /// 调用方据此拒绝重连——拿一个错的公钥去验签，得到的是 `identity mismatch`
    /// 之外的更难查的现象。
    pub fn public_key_bytes(&self) -> Option<[u8; 32]> {
        let raw = crate::base64url_decode(self.public_key.as_deref()?)?;
        <[u8; 32]>::try_from(raw).ok()
    }
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
///
/// `Clone` 只复制连接池句柄（`r2d2` 的 `Pool` 本身就是 `Arc`），
/// 不是复制连接——离线扫瞄器要拿一份进后台任务。
#[derive(Clone)]
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
        let row = conn
            .query_row(
                "SELECT id, name, key_fingerprint, enrolled_at, last_seen_at, status,
                        last_seen_gen, superseded_at, os, arch, hostname, node_version,
                        capabilities, resources, public_key
                 FROM device WHERE key_fingerprint = ?1",
                params![fp],
                row_to_device_row,
            )
            .optional()?;
        row.map(DeviceRow::into_device).transpose()
    }

    /// 按 node_id 查设备。
    pub fn find_by_id(&self, id: &str) -> Result<Option<Device>> {
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                "SELECT id, name, key_fingerprint, enrolled_at, last_seen_at, status,
                        last_seen_gen, superseded_at, os, arch, hostname, node_version,
                        capabilities, resources, public_key
                 FROM device WHERE id = ?1",
                params![id],
                row_to_device_row,
            )
            .optional()?;
        row.map(DeviceRow::into_device).transpose()
    }

    /// 全部设备（Hub 重启后、设备尚未连上时也要能回答「有没有这台设备」）。
    pub fn list(&self) -> Result<Vec<Device>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, key_fingerprint, enrolled_at, last_seen_at, status,
                    last_seen_gen, superseded_at, os, arch, hostname, node_version,
                    capabilities, resources, public_key
             FROM device ORDER BY enrolled_at",
        )?;
        let rows = stmt.query_map([], row_to_device_row)?;
        let raw = rows.collect::<std::result::Result<Vec<DeviceRow>, _>>()?;
        raw.into_iter().map(DeviceRow::into_device).collect()
    }

    /// 心跳后更新能力/负载/在线状态。
    ///
    /// **不含 `node_version`**：版本在 enroll 首帧的 `device_info` 里带上并
    /// 落库（§3），心跳不重复写它。重连帧结构上就没有 `device_info`，
    /// 所以库里那份版本停在 enroll 那一刻——M0 没有「按版本推升级」的
    /// 读者，等 M1 真要推 `update` 时再由那条需求决定版本从哪来。
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

    /// 把设备公钥写进 `device.public_key` 真列。
    ///
    /// M1 起不再寄生在 `capabilities` 的 JSON 键里（`docs/BUGS.md` 的彻底修法）。
    /// 连带消失的是 `hub-core::reports::preserve_public_key` 那套合并约定：
    /// 公钥不再是「心跳载荷里碰巧缺掉的一个键」，而是心跳压根不碰的一列。
    ///
    /// 同时把 JSON 里的旧键抹掉——留着它就得让 `merge_into_stored` 继续替它
    /// 保命，而两份公钥来源（列 + JSON）迟早会分叉：那才是真正难查的故障。
    pub fn set_public_key(&self, id: &str, pubkey: &[u8; 32]) -> Result<()> {
        let mut conn = self.pool.get()?;
        // 显式事务，**顺序要紧**：抹旧键在前，写新列在后，commit 收尾。
        // 反过来写（新列先、commit 后）是不行的——`UPDATE capabilities` 在
        // JSON 里没有 `public_key` 时影响 0 行（`json_remove` 改不动），
        // 那条「没事要做」的路上就没有 commit 可言，而显式 ROLLBACK 会把
        // 已经写好的新列一并撤销。
        //
        // `transaction()`（**不是** `unchecked_transaction()`）：库开着
        // `foreign_keys=ON`，嵌套事务在 SQLite 里是 deferred 的，内外两层
        // 各有一条隐式语句就可能撞 `cannot start a transaction within a
        // transaction`。`transaction()` 会先查 `Connection::is_autocommit()`，
        // 已在事务里就复用外层那层——错误路径上内层的 `Drop` 照样回滚。
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE device SET capabilities = json_remove(capabilities, '$.public_key')
             WHERE id = ?1 AND capabilities IS NOT NULL AND json_valid(capabilities)
               AND json_type(capabilities, '$.public_key') IS NOT NULL",
            params![id],
        )?;
        tx.execute(
            "UPDATE device SET public_key = ?2 WHERE id = ?1",
            params![id, base64url_of(pubkey)],
        )?;
        tx.commit()?;
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

    /// 库里所有设备见过的最大 `gen`，用作进程内计数器的起点（0 = 空库）。
    ///
    /// 必须查库而不是靠内存计数器：Hub 重启后内存归零，而设备重连时会带上
    /// 上一进程的 `last_seen_gen`。Hub 发出的 `gen` 比它小就会被判成
    /// 「已被顶替」（§7.1）→ 设备永久接不上。
    pub fn max_gen(&self) -> Result<u64> {
        let conn = self.pool.get()?;
        let max: Option<i64> =
            conn.query_row("SELECT MAX(last_seen_gen) FROM device", [], |r| r.get(0))?;
        Ok(max.unwrap_or(0).max(0) as u64)
    }
}

/// 一行设备的**原始形状**：`status` 保持库里的文本，解析推迟到
/// [`DeviceRow::into_device`]。
///
/// 分两段是为了让「状态非法」能变成 [`StorageError::InvalidDeviceStatus`]
/// 而不是被压成 `rusqlite::Error`——行映射函数的错误类型由 rusqlite 定死，
/// 装不下仓储层自己的错误类型。
struct DeviceRow {
    id: String,
    name: String,
    key_fingerprint: String,
    enrolled_at: String,
    last_seen_at: Option<String>,
    status: String,
    last_seen_gen: u64,
    superseded_at: Option<String>,
    os: Option<String>,
    arch: Option<String>,
    hostname: Option<String>,
    node_version: Option<String>,
    capabilities: Option<String>,
    resources: Option<String>,
    public_key: Option<String>,
}

impl DeviceRow {
    /// 解析状态并组装成 [`Device`]。
    ///
    /// **状态非法就报错，不静默回落 offline**——回落会把数据损坏伪装成
    /// 「设备离线」，页面显示正常而库里那行是垃圾值，日志里一个字都没有。
    fn into_device(self) -> Result<Device> {
        let status = DeviceStatus::from_str(&self.status)?;
        Ok(Device {
            id: self.id,
            name: self.name,
            key_fingerprint: self.key_fingerprint,
            enrolled_at: self.enrolled_at,
            last_seen_at: self.last_seen_at,
            status,
            last_seen_gen: self.last_seen_gen,
            superseded_at: self.superseded_at,
            os: self.os,
            arch: self.arch,
            hostname: self.hostname,
            node_version: self.node_version,
            capabilities: self.capabilities,
            resources: self.resources,
            public_key: self.public_key,
        })
    }
}

fn row_to_device_row(
    r: &r2d2_sqlite::rusqlite::Row<'_>,
) -> std::result::Result<DeviceRow, r2d2_sqlite::rusqlite::Error> {
    Ok(DeviceRow {
        id: r.get(0)?,
        name: r.get(1)?,
        key_fingerprint: r.get(2)?,
        enrolled_at: r.get(3)?,
        last_seen_at: r.get(4)?,
        status: r.get(5)?,
        last_seen_gen: r.get::<_, i64>(6)? as u64,
        superseded_at: r.get(7)?,
        os: r.get(8)?,
        arch: r.get(9)?,
        hostname: r.get(10)?,
        node_version: r.get(11)?,
        capabilities: r.get(12)?,
        resources: r.get(13)?,
        public_key: r.get(14)?,
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

    /// 空库时起点必须是 0，否则第一台设备会拿到 `gen = 0`——而 `0` 正是
    /// 「还没连过」的哨兵值，`last_seen_gen` 兜底会把两者混同。
    #[test]
    fn max_gen_on_an_empty_library_is_zero() {
        let (_d, dr, _s) = repo();
        assert_eq!(dr.max_gen().unwrap(), 0);
    }

    #[test]
    fn max_gen_returns_the_largest_gen_not_the_latest_row() {
        // 目标设备先连（gen 低），另一台后连（gen 高）：查询不能依赖行序。
        let (_d, dr, _s) = repo();
        insert_device(&dr, "n_a", "fp_a");
        insert_device(&dr, "n_b", "fp_b");
        dr.set_gen("n_a", 3).unwrap();
        dr.set_gen("n_b", 41).unwrap();
        assert_eq!(dr.max_gen().unwrap(), 41);
    }

    /// gen 只会单调推进，设备撤销后残留的行也不能把起点拉回去。
    #[test]
    fn max_gen_ignores_devices_that_never_set_a_gen() {
        let (_d, dr, _s) = repo();
        insert_device(&dr, "n_a", "fp_a");
        dr.set_gen("n_a", 7).unwrap();
        insert_device(&dr, "n_b", "fp_b");
        assert_eq!(dr.max_gen().unwrap(), 7);
    }

    #[test]
    fn device_status_roundtrips_and_rejects_unknown() {
        // DB 里存的是文本，读回来必须还能解析回去——非法值在**读路径**上报错，
        // 不静默变成 Offline（见下面那条用例）。
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

    /// 库里那行 `status` 是垃圾值时，读路径必须**报错**，不能静默回落 offline。
    ///
    /// 回落会把数据损坏伪装成「设备离线」：页面显示正常，日志里一个字都没有，
    /// 而库里那行是垃圾值。schema 上的 `CHECK` 拦得住正常途径写入，这种值只能
    /// 来自外部改库——正是这种情况最需要有人被告知。
    #[test]
    fn a_corrupted_status_is_reported_instead_of_being_read_as_offline() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        {
            let conn = dr.pool.get().unwrap();
            // CHECK 约束拦得住正常途径写入；这一次绕开它。
            conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
                .unwrap();
            conn.execute(
                "UPDATE device SET status = 'reconnecting' WHERE id = ?1",
                params!["n_a"],
            )
            .unwrap();
        }
        // 三条读路径都必须报同一个错
        let e = dr.find_by_id("n_a").unwrap_err();
        assert!(
            matches!(&e, StorageError::InvalidDeviceStatus(s) if s == "reconnecting"),
            "got {e:?}"
        );
        assert!(matches!(
            dr.find_by_fingerprint("fp_a").unwrap_err(),
            StorageError::InvalidDeviceStatus(_)
        ));
        assert!(matches!(
            dr.list().unwrap_err(),
            StorageError::InvalidDeviceStatus(_)
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
        // 锁住契约：① 只输出 base64url 字符集 ② 长度随输入非递减
        // （这是它作为指纹/公钥编码被下游消费的前提，不变量而非具体公式）。
        // 编解码本身在 identity 里，这两条断言守的是「落库的那份与解析的那份
        // 用的是同一套字母表」——两份编解码器只会在非法字符的分歧上裂开。
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

    /// `base64url_of` 是转发，不是另一份实现——落库的那份编码与 identity
    /// 解析的那份必须逐字节相同，否则「库里读出来的公钥解不开」。
    #[test]
    fn base64url_of_is_the_same_encoder_as_identity() {
        for bytes in [Vec::new(), vec![7u8; 32], b"n_a|unix:0".to_vec()] {
            assert_eq!(base64url_of(&bytes), identity::base64url(&bytes));
        }
        // 解码回同一串：这一侧才是被 `Registry` 直接消费的方向
        let kp = identity::KeyPair::generate();
        let encoded = base64url_of(&kp.public_key_bytes());
        assert_eq!(
            identity::base64url_decode(&encoded).expect("must decode"),
            kp.public_key_bytes().to_vec()
        );
    }

    #[test]
    fn set_public_key_writes_the_column_and_keeps_other_capability_keys() {
        // M1：公钥进 `device.public_key` 真列，capabilities 的其它键一个不动。
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        dr.touch_online(
            "n_a",
            r#"{"connectors":["a"]}"#,
            "{}",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let pk = [7u8; 32];
        dr.set_public_key("n_a", &pk).unwrap();

        let d = dr.find_by_id("n_a").unwrap().unwrap();
        assert_eq!(d.public_key.as_deref(), Some(base64url_of(&pk).as_str()));
        assert_eq!(d.public_key_bytes(), Some(pk));
        assert_eq!(
            d.capabilities.as_deref(),
            Some(r#"{"connectors":["a"]}"#),
            "别的键必须原样保留"
        );
    }

    /// 覆盖写：设备换密钥（重装 / 重新 enroll）后旧值不得残留。
    #[test]
    fn set_public_key_overwrites_the_previous_one() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        dr.set_public_key("n_a", &[1u8; 32]).unwrap();
        dr.set_public_key("n_a", &[2u8; 32]).unwrap();
        assert_eq!(
            dr.find_by_id("n_a").unwrap().unwrap().public_key_bytes(),
            Some([2u8; 32])
        );
    }

    /// 解码走的是仓储层那份，与 `base64url_of` 同一个判据——
    /// 两处各写一份编解码器迟早在「哪个字符算非法」上分叉。
    #[test]
    fn the_storage_codec_is_the_inverse_of_the_identity_one() {
        let kp = identity::KeyPair::generate();
        let raw = kp.public_key_bytes();
        let b64 = base64url_of(&raw);
        assert_eq!(crate::base64url_decode(&b64), Some(raw.to_vec()));
        assert_eq!(
            base64url_of(raw.as_slice()),
            b64,
            "同一份实现，无第二套编码"
        );
    }

    /// M0 时代公钥寄居在 `capabilities.public_key`；写进新列时顺手抹掉旧键。
    ///
    /// 留着它就要让心跳合并继续替它保命，而两份公钥来源迟早分叉。
    #[test]
    fn set_public_key_strips_the_legacy_json_key() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        dr.touch_online(
            "n_a",
            r#"{"connectors":["a"],"public_key":"AAAA"}"#,
            "{}",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        dr.set_public_key("n_a", &[3u8; 32]).unwrap();
        let caps = dr.find_by_id("n_a").unwrap().unwrap().capabilities;
        let v: serde_json::Value = serde_json::from_str(&caps.unwrap()).unwrap();
        assert_eq!(v["connectors"], serde_json::json!(["a"]));
        assert!(
            v.get("public_key").is_none(),
            "JSON 键必须被抹掉，公钥只有列这一个来源"
        );
    }

    /// 抹键的三条「原样退回」路径：capabilities 是 NULL、是坏 JSON、
    /// 顶层不是对象。它们都是**库完好**的正常状态，不是错误。
    #[test]
    fn a_capabilities_that_is_not_a_json_object_is_left_alone() {
        for caps in [
            None,
            Some("not json"),
            Some(""),
            Some(r#"["git"]"#),
            Some(r#""git""#),
            Some(r#"{"connectors":["a"]}"#), // 压根没有 public_key
        ] {
            let (_d, dr, _sr) = repo();
            insert_device(&dr, "n_a", "fp_a");
            if let Some(c) = caps {
                dr.touch_online("n_a", c, "{}", "2026-01-01T00:00:00Z")
                    .unwrap();
            }
            dr.set_public_key("n_a", &[4u8; 32]).unwrap();
            let d = dr.find_by_id("n_a").unwrap().unwrap();
            assert_eq!(
                d.capabilities.as_deref(),
                caps,
                "{caps:?} must be untouched"
            );
            assert_eq!(d.public_key_bytes(), Some([4u8; 32]));
        }
    }

    /// 库里根本没有这一行时两条 UPDATE 都影响 0 行——不是错误：
    /// 真正要报的是「设备不存在」，那由调用方决定。
    #[test]
    fn set_public_key_on_an_unknown_device_is_a_no_op() {
        let (_d, dr, _sr) = repo();
        dr.set_public_key("n_ghost", &[5u8; 32]).unwrap();
        assert!(dr.find_by_id("n_ghost").unwrap().is_none());
    }

    /// 读出来的公钥各种坏值都必须报「没有」而不是崩：非 base64、长度不对、
    /// 合法 base64 但只有 31 字节、以及 NULL。重连验签据此拒绝，绝不拿一个
    /// 错的公钥去试。
    ///
    /// 库完好时写不进这种值（`set_public_key` 只接受 `&[u8; 32]`），它来自
    /// 外部改库或迁移回填了脏数据。
    #[test]
    fn a_broken_stored_public_key_reads_as_absent() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        let mut d = dr.find_by_id("n_a").unwrap().unwrap();
        assert_eq!(d.public_key, None);
        assert_eq!(d.public_key_bytes(), None);

        // 31 字节那条只在合法 base64、长度不足时命中 `<[u8; 32]>::try_from`，
        // 与「压根不是 base64」是两条不同的拒绝路径。
        let short = base64url_of(&[9u8; 31]);
        for bad in [
            Some("!!!not base64!!!"),
            Some("AAAA"),
            Some(""),
            Some(short.as_str()),
        ] {
            let conn = dr.pool().get().unwrap();
            conn.execute(
                "UPDATE device SET public_key = ?1 WHERE id = 'n_a'",
                params![bad],
            )
            .unwrap();
            d = dr.find_by_id("n_a").unwrap().unwrap();
            assert_eq!(d.public_key.as_deref(), bad, "{bad:?} 原样读回");
            assert_eq!(
                d.public_key_bytes(),
                None,
                "{bad:?} must not decode, and must not fall back to capabilities"
            );
        }
    }

    /// `pool()` 交出的必须是**同一个库**，不是一份快照、也不是另一条独立连接。
    ///
    /// 断言方式是真写一次再经 `find_by_id` 读回来：指针相等由类型系统保证，
    /// 那种断言在任何实现下都绿，等于没测。
    #[test]
    fn pool_accessor_exposes_the_same_database() {
        let (_d, dr, _sr) = repo();
        insert_device(&dr, "n_a", "fp_a");
        // 经 `pool()` 直接写：`find_by_id` 走的是仓储自己的 pool
        {
            let mut conn = dr.pool().get().unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "UPDATE device SET name = 'written-through-pool' WHERE id = ?1",
                params!["n_a"],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            dr.find_by_id("n_a").unwrap().unwrap().name,
            "written-through-pool"
        );

        // 回滚的事务对仓储不可见：同一个库，不是「一份读、一份写」
        {
            let mut conn = dr.pool().get().unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "UPDATE device SET name = 'rolled-back' WHERE id = ?1",
                params!["n_a"],
            )
            .unwrap();
            drop(tx);
        }
        assert_eq!(
            dr.find_by_id("n_a").unwrap().unwrap().name,
            "written-through-pool"
        );
    }
}
