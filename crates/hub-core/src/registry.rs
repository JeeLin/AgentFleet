//! 注册表：enroll 事务、指纹查重、node_id 签发。
//!
//! 设计要点（`protocol.md` §3、`security-and-lifecycle.md` §4.3）：
//! - enroll 是 `BEGIN IMMEDIATE → SELECT → INSERT(UNIQUE) → 签发 → COMMIT`，
//!   **绝不 upsert**——UPSERT 会让并发 enroll 者覆盖别人的指纹绑定，
//!   导致已签发的 `node_id` 立即失效，而对方私钥已落盘，永远接不回来。
//! - **重连验签不在这里**：握手状态机在 `protocol::handshake`，验签用的是
//!   库内那一列公钥（`Device::public_key_bytes`）。本模块只管 enroll 这一侧
//!   的写——「只信库内指纹」是调用方的契约，不是这里的一个入口。

use crate::time;
use hub_storage::repo::{Device, DeviceRepo, NewDevice};

// `DeviceStatus` 只在测试断言里用；生产代码只写不读状态枚举。
#[cfg(test)]
use hub_storage::repo::DeviceStatus;
use hub_storage::Db;
use r2d2_sqlite::rusqlite::{params, Connection, OptionalExtension};

use crate::CoreError;
use identity::{self, Fingerprint};

pub mod error;
pub use error::RegistryError;

/// 注册表。
pub struct Registry {
    devices: DeviceRepo,
}

/// enroll 的结果。
#[derive(Debug, Clone)]
pub enum EnrollOutcome {
    /// 新设备注册成功。
    Enrolled { node_id: String },
    /// 该指纹已绑定到一个 node_id；调用方应走重连流程。
    AlreadyRegistered { node_id: String },
}

impl EnrollOutcome {
    /// 绑定的 `node_id`。
    ///
    /// 两个成功路径都带同一个 node_id——**指纹是唯一入口**，
    /// 所以「重复注册」不是新设备，而是「老设备又来了一次 enroll」，
    /// 客户端应改走 `auth` 重连流程（§4.3）。
    ///
    /// **总是有值**：enroll 只有「签发」与「已注册」两条出路，
    /// 没有第三种结果可表示「拿不到 node_id」。
    pub fn node_id(&self) -> &str {
        match self {
            Self::Enrolled { node_id } | Self::AlreadyRegistered { node_id } => node_id,
        }
    }
}

impl Registry {
    pub fn new(pool: Db) -> Self {
        Self {
            devices: DeviceRepo::new(pool),
        }
    }

    /// enroll：在**显式事务**内完成「查指纹 → 插入 → 签发 node_id」。
    ///
    /// `pubkey` 是本次上传的公钥，只在首次 enroll 被接受。
    pub fn enroll(&self, pubkey: &[u8; 32], info: NewDevice) -> Result<EnrollOutcome, CoreError> {
        let fp = Fingerprint::of(pubkey).to_hex();

        // 事务外先查一次（快路径，避免无谓开事务）
        if let Some(existing) = self.devices.find_by_fingerprint(&fp)? {
            return Ok(EnrollOutcome::AlreadyRegistered {
                node_id: existing.id,
            });
        }

        let node_id = identity::issue_node_id();
        let now = now_stamp();

        // 显式事务：BEGIN IMMEDIATE → INSERT(UNIQUE) → COMMIT
        let conn = self.devices.pool().get()?;
        conn.execute("BEGIN IMMEDIATE", [])?;
        let result = Self::enroll_in_tx(&conn, &fp, &node_id, &now, &info);
        Self::finish_enroll(&conn, result, node_id)
    }

    // 事务体：事务内再查一次重复指纹 → 插入设备行。
    //
    // 抽成独立函数是为了能**确定性地**测到「事务内查到重复指纹」那条臂：
    // 生产里它由并发 enroll 触发（败者在 `BEGIN IMMEDIATE` 上等胜者提交，
    // 进去就查到刚提交的那一行），竞态没法稳定复现，而这条臂恰恰是
    // 「绝不 upsert」的第二道闸——删掉它，并发同指纹就能插出两行。
    fn enroll_in_tx(
        conn: &Connection,
        fp: &str,
        node_id: &str,
        now: &str,
        info: &NewDevice,
    ) -> Result<(), CoreError> {
        // 事务内再查一次：另一个事务可能刚插入
        let dup: Option<String> = conn
            .query_row(
                "SELECT id FROM device WHERE key_fingerprint = ?1",
                [fp],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = dup {
            // 已存在：回滚并告知已注册（不是错误）
            return Err(CoreError::Registry(RegistryError::AlreadyRegistered(id)));
        }
        conn.execute(
            "INSERT INTO device
                (id, name, key_fingerprint, enrolled_at, status, os, arch, hostname, node_version)
             VALUES (?1, ?2, ?3, ?4, 'offline', ?5, ?6, ?7, ?8)",
            params![
                node_id,
                info.name,
                fp,
                now,
                info.os,
                info.arch,
                info.hostname,
                info.node_version
            ],
        )?;
        Ok(())
    }

    // 事务收尾：把事务体的结果翻译成 `EnrollOutcome`，并落定事务。
    //
    // 抽出来同样是为了能确定性地测到「事务内查到重复指纹」那条臂
    // （见 `Registry::enroll_in_tx`）：生产里它只由并发竞态触发。
    fn finish_enroll(
        conn: &Connection,
        result: Result<(), CoreError>,
        node_id: String,
    ) -> Result<EnrollOutcome, CoreError> {
        match result {
            Ok(()) => {
                conn.execute("COMMIT", [])?;
                Ok(EnrollOutcome::Enrolled { node_id })
            }
            Err(CoreError::Registry(RegistryError::AlreadyRegistered(id))) => {
                conn.execute("ROLLBACK", [])?;
                Ok(EnrollOutcome::AlreadyRegistered { node_id: id })
            }
            // Err(e) 主体由 enroll_inner_statement_error_rolls_back_and_propagates 覆盖。
            //
            // coverage: excluded — 下面的 `if let Err(rb)` 不可达：只要闭包返回
            // Err 事务仍活跃，SQLite 的 ROLLBACK 必然成功；该分支仅防御连接级
            // 故障，无可控测试手段。而「回滚失败」这条本身也不该被覆盖——
            // 原错误才是调用方要的，它只负责留痕。
            Err(e) => {
                if let Err(rb) = conn.execute("ROLLBACK", []) {
                    tracing::warn!(error = %rb, "rollback failed after enroll error");
                }
                Err(e)
            }
        }
    }

    pub fn device(&self, node_id: &str) -> Result<Option<Device>, CoreError> {
        Ok(self.devices.find_by_id(node_id)?)
    }

    pub fn list(&self) -> Result<Vec<Device>, CoreError> {
        Ok(self.devices.list()?)
    }

    pub fn devices(&self) -> &DeviceRepo {
        &self.devices
    }
}

/// 当前时刻的时间戳串，落库进 `enrolled_at` / `last_seen_at`。
///
/// **格式是 `unix:<秒>`，不是 RFC3339**（函数名与这个文档串都曾经说成
/// RFC3339，而代码从没做到过）。引 chrono 只为把它改成真的 RFC3339 一件
/// 纯装饰的事：本仓没有一处按时间**解析**这些列——判在线只看心跳年龄
/// （`reports::OFFLINE_AFTER`），落库的 `last_seen_at` 仅供人工阅读，
/// 所以「稳定可比」就够了，而这正是前缀要写清格式的原因。
pub fn now_stamp() -> String {
    // 不引 chrono：只需要一个稳定可比的时间戳
    format!("unix:{}", time::unix_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_storage::db::open;
    use identity::{issue_node_id, KeyPair};

    fn setup() -> (tempfile::TempDir, Registry) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        (d, Registry::new(pool))
    }

    fn info() -> NewDevice {
        NewDevice {
            id: String::new(),
            name: "pi".into(),
            key_fingerprint: String::new(),
            os: "linux".into(),
            arch: "aarch64".into(),
            hostname: "raspberrypi".into(),
            node_version: "0.1.0".into(),
        }
    }

    /// enroll 一次，断言成功并返回 node_id。
    ///
    /// 用 `matches!` 而不是 `match { other => panic!() }`：后者的兜底臂在
    /// 100% 覆盖率门禁下永远跑不到，会把 TOTAL 拖下来。
    fn enroll_ok(r: &Registry, kp: &KeyPair) -> String {
        let out = r.enroll(&kp.public_key_bytes(), info()).unwrap();
        assert!(
            matches!(out, EnrollOutcome::Enrolled { .. }),
            "expected Enrolled, got {out:?}"
        );
        out.node_id().to_string()
    }

    #[test]
    fn enroll_issues_node_id() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        assert_eq!(node_id.len(), 22);
        let dev = r.device(&node_id).unwrap().unwrap();
        assert_eq!(dev.status, DeviceStatus::Offline);
        assert_eq!(dev.arch.as_deref(), Some("aarch64"));
    }

    #[test]
    fn same_fingerprint_enroll_twice_reports_already_registered() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        let second = r.enroll(&kp.public_key_bytes(), info()).unwrap();
        assert!(
            matches!(second, EnrollOutcome::AlreadyRegistered { .. }),
            "second enroll must report AlreadyRegistered"
        );
        // 库里仍然只有一行（绝不 upsert / 绝不插第二行）
        assert_eq!(r.list().unwrap().len(), 1);
        assert_eq!(second.node_id(), node_id.as_str());
    }

    #[test]
    fn different_fingerprints_enroll_separately() {
        let (_d, r) = setup();
        let a = KeyPair::generate();
        let b = KeyPair::generate();
        assert!(matches!(
            r.enroll(&a.public_key_bytes(), info()).unwrap(),
            EnrollOutcome::Enrolled { .. }
        ));
        assert!(matches!(
            r.enroll(&b.public_key_bytes(), info()).unwrap(),
            EnrollOutcome::Enrolled { .. }
        ));
        assert_eq!(r.list().unwrap().len(), 2);
    }

    #[test]
    fn concurrent_enroll_one_wins_without_upsert() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let pk = kp.public_key_bytes();
        let pool = r.devices().pool().clone();

        // 两个 Registry 实例模拟两个并发 enroll
        let r2 = Registry::new(pool);
        let pk2 = pk;
        let t = std::thread::spawn(move || r2.enroll(&pk2, info()));

        let a = r.enroll(&pk, info()).unwrap();
        let b = t.join().unwrap().unwrap();

        let enrolled = [&a, &b]
            .iter()
            .filter(|o| matches!(o, EnrollOutcome::Enrolled { .. }))
            .count();
        assert_eq!(enrolled, 1, "exactly one enroll may win");
        // 败者拿到的是同一个 node_id（已注册），库里始终一行
        assert_eq!(a.node_id(), b.node_id());
        assert_eq!(r.list().unwrap().len(), 1);
    }

    /// 时间戳格式是 `unix:<秒>`——落库的两列（`enrolled_at` / `last_seen_at`）
    /// 都靠它**排序可比**。函数名曾经说成 `now_rfc3339`、文档也写着 RFC3339，
    /// 而代码从来没做到过；这条把真格式钉住，免得又有人按 RFC3339 去解析。
    #[test]
    fn the_timestamp_is_unix_seconds_not_rfc3339() {
        let stamp = now_stamp();
        let secs = stamp
            .strip_prefix("unix:")
            .unwrap_or_else(|| panic!("prefix must be `unix:`, got {stamp}"))
            .parse::<u64>()
            .unwrap_or_else(|e| panic!("payload must be plain seconds, got {stamp} ({e})"));
        // 量级检查而不是具体值：写死某个时刻会随时间变假
        assert!(secs > 1_577_836_800, "stamp {stamp} is too old");
        assert!(secs < 100_000_000_000, "stamp {stamp} looks like millis");
    }

    /// 两次取戳必须可比且非递减：落库的「谁先谁后」判据就是字符串里的数字。
    #[test]
    fn timestamps_are_comparable_and_never_go_backwards() {
        let first: u64 = now_stamp().strip_prefix("unix:").unwrap().parse().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second: u64 = now_stamp().strip_prefix("unix:").unwrap().parse().unwrap();
        assert!(second >= first, "{second} must not precede {first}");
    }

    #[test]
    fn node_id_issued_by_registry_is_unique() {
        let ids: std::collections::HashSet<_> = (0..100).map(|_| issue_node_id()).collect();
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn enroll_rolls_back_on_transaction_error() {
        // 预先在同一条连接上开一个事务，enroll 里的 `BEGIN IMMEDIATE`
        // 就会失败（SQLite 不允许嵌套事务）→ 走「非 AlreadyRegistered 的
        // 错误」路径：必须回滚并把错误往上抛，而不是把事务悬挂在那里。
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let conn = r.devices().pool().get().unwrap();
        conn.execute("BEGIN IMMEDIATE", []).unwrap();
        assert!(r.enroll(&kp.public_key_bytes(), info()).is_err());
        // 外层事务仍在：enroll 不能把它提交掉
        conn.execute("ROLLBACK", []).unwrap();

        // 库里不能有半条记录
        assert_eq!(r.list().unwrap().len(), 0);
    }

    #[test]
    fn enroll_inner_statement_error_rolls_back_and_propagates() {
        // 让 enroll **事务体内**的 INSERT 失败，才能命中 `finish_enroll` 的
        // `Err(e)` 臂。
        //
        // 为什么不能直接 DROP TABLE：事务外还有一次快路径查询
        // （`find_by_fingerprint`，enroll 开头），表没了会在这里就 `?` 返回，
        // 根本进不了事务体。
        //
        // 手段：建一个 `BEFORE INSERT` 触发器主动 RAISE(ABORT)。表结构完好，
        // 快路径查询正常返回 None，只有 INSERT 会失败——确定性的，不依赖竞态。
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        {
            let conn = r.devices().pool().get().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON device
                 BEGIN SELECT RAISE(ABORT, 'enroll blocked'); END;",
            )
            .unwrap();
        }
        let e = r.enroll(&kp.public_key_bytes(), info()).unwrap_err();
        // 必须是 SQLite 层的约束错误（ABORT），不是「已注册」
        assert!(
            matches!(e, CoreError::Sqlite(_)),
            "expected an inner-statement storage error, got {e:?}"
        );
        // 事务已回滚：失败的 INSERT 没留下半条记录
        assert!(
            r.list().unwrap().is_empty(),
            "rolled-back enroll must leave no device row"
        );
    }

    #[test]
    fn outcome_accessors_describe_each_variant() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let first = r.enroll(&kp.public_key_bytes(), info()).unwrap();
        let second = r.enroll(&kp.public_key_bytes(), info()).unwrap();
        // 两个成功路径都要拿到同一个 node_id（指纹是唯一入口，§4.3）
        assert_eq!(first.node_id(), second.node_id());
        // `node_id()` 永远有值：没有第三种「拿不到 node_id」的结果可表示
        assert!(!first.node_id().is_empty());
    }

    /// 事务体查到重复指纹 → 拒绝插入，报「已注册」。
    ///
    /// 生产触发路径是并发 enroll（败者在 `BEGIN IMMEDIATE` 上等胜者提交，
    /// 进事务就查到刚提交的那一行），竞态没法稳定复现，所以这里直接驱动
    /// 事务体：库里已经有同指纹的行时，事务体必须拒绝插入——**绝不 upsert**
    /// 的第二道闸就靠它。
    #[test]
    fn a_duplicate_seen_inside_the_transaction_is_refused() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let fp = Fingerprint::of(&kp.public_key_bytes()).to_hex();

        // 直接插一行同指纹的设备（模拟「另一个事务刚提交」）
        {
            let conn = r.devices().pool().get().unwrap();
            conn.execute(
                "INSERT INTO device
                    (id, name, key_fingerprint, enrolled_at, status, os, arch, hostname, node_version)
                 VALUES ('n_dup', 'pi', ?1, 'unix:0', 'offline', 'linux', 'aarch64', 'h', 'v')",
                [&fp],
            )
            .unwrap();
        }

        let conn = r.devices().pool().get().unwrap();
        conn.execute("BEGIN IMMEDIATE", []).unwrap();
        let err = Registry::enroll_in_tx(&conn, &fp, "n_new", "unix:0", &info()).unwrap_err();
        assert!(
            matches!(&err, CoreError::Registry(RegistryError::AlreadyRegistered(id)) if id == "n_dup"),
            "expected AlreadyRegistered(n_dup), got {err:?}"
        );
        conn.execute("ROLLBACK", []).unwrap();
        // 库里仍然只有那一行：重复指纹没有插出第二行
        assert_eq!(r.list().unwrap().len(), 1);
    }

    /// 事务体报「已注册」→ 回滚，把已绑定的 node_id 交回去。
    ///
    /// 与上一个测试配套：那条臂的另一半是「回滚并如实交出已绑定的
    /// node_id」，生产里同样只由并发竞态触发，所以在这里直接驱动收尾逻辑。
    #[test]
    fn the_duplicate_outcome_rolls_back_and_keeps_the_existing_node_id() {
        let (_d, r) = setup();
        let conn = r.devices().pool().get().unwrap();
        conn.execute("BEGIN IMMEDIATE", []).unwrap();
        let outcome = Registry::finish_enroll(
            &conn,
            Err(CoreError::Registry(RegistryError::AlreadyRegistered(
                "n_dup".into(),
            ))),
            "n_new".into(),
        )
        .unwrap();
        assert!(
            matches!(&outcome, EnrollOutcome::AlreadyRegistered { node_id } if node_id == "n_dup"),
            "expected AlreadyRegistered(n_dup), got {outcome:?}"
        );
        // 事务已回滚：连接上没有悬挂的事务
        conn.execute("BEGIN IMMEDIATE", []).unwrap();
        conn.execute("ROLLBACK", []).unwrap();
    }

    /// 并发 enroll 同一指纹：恰好一个成功，库里恰好一行，绝不 upsert。
    ///
    /// 败者走哪条臂取决于竞态（快路径命中已提交的行，还是事务内查到
    /// 刚提交的行），本测试**不区分**——它守的是不变量：一行、一个
    /// Enrolled、两边拿到同一个 node_id。两条臂各自的分支由上面两个
    /// 直接驱动事务体与收尾逻辑的测试覆盖。
    #[test]
    fn concurrent_enroll_of_one_fingerprint_registers_exactly_one_device() {
        for _ in 0..60 {
            let (_d, r) = setup();
            let kp = KeyPair::generate();
            let pk = kp.public_key_bytes();
            let pool = r.devices().pool().clone();
            let r2 = Registry::new(pool);
            let pk2 = pk;
            let t = std::thread::spawn(move || r2.enroll(&pk2, info()));
            let a = r.enroll(&pk, info()).unwrap();
            let b = t.join().unwrap().unwrap();
            assert_eq!(r.list().unwrap().len(), 1, "exactly one device row");
            let enrolled = [&a, &b]
                .iter()
                .filter(|o| matches!(o, EnrollOutcome::Enrolled { .. }))
                .count();
            assert_eq!(enrolled, 1, "exactly one enroll may win");
        }
    }
}
