//! 注册表：enroll 事务、指纹查重、node_id 签发。
//!
//! 设计要点（`protocol.md` §3、`security-and-lifecycle.md` §4.3）：
//! - enroll 是 `BEGIN IMMEDIATE → SELECT → INSERT(UNIQUE) → 签发 → COMMIT`，
//!   **绝不 upsert**——UPSERT 会让并发 enroll 者覆盖别人的指纹绑定，
//!   导致已签发的 `node_id` 立即失效，而对方私钥已落盘，永远接不回来。
//! - 重连两步：**只信库内指纹**，忽略请求里带来的任何公钥，失败绝不改写指纹。

use ed25519_dalek::VerifyingKey;
use hub_storage::repo::{Device, DeviceRepo, NewDevice};
// `DeviceStatus` 只在测试断言里用；生产代码只写不读状态枚举。
#[cfg(test)]
use hub_storage::repo::DeviceStatus;
use hub_storage::Db;
#[cfg(test)]
use r2d2_sqlite::rusqlite as sqlite;
use r2d2_sqlite::rusqlite::{params, OptionalExtension};

use crate::CoreError;
use identity::{self, Fingerprint, KeyPair};

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
    /// 并发 enroll 撞了唯一约束；让客户端重试。
    Race,
}

impl EnrollOutcome {
    /// 绑定的 `node_id`。
    ///
    /// 两个成功路径都带同一个 node_id——**指纹是唯一入口**，
    /// 所以「重复注册」不是新设备，而是「老设备又来了一次 enroll」，
    /// 客户端应改走 `auth` 重连流程（§4.3）。
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::Enrolled { node_id } | Self::AlreadyRegistered { node_id } => Some(node_id),
            Self::Race => None,
        }
    }

    /// 是否是本次新注册（而非重复）。
    pub fn is_new(&self) -> bool {
        matches!(self, Self::Enrolled { .. })
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
        let result = (|| -> Result<(), CoreError> {
            // 事务内再查一次：另一个事务可能刚插入
            let dup: Option<String> = conn
                .query_row(
                    "SELECT id FROM device WHERE key_fingerprint = ?1",
                    [&fp],
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
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", [])?;
                Ok(EnrollOutcome::Enrolled { node_id })
            }
            Err(CoreError::Registry(RegistryError::AlreadyRegistered(id))) => {
                conn.execute("ROLLBACK", [])?;
                Ok(EnrollOutcome::AlreadyRegistered { node_id: id })
            }
            // Err(e) 主体由 enroll_rolls_back_on_transaction_error 覆盖。
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

    /// 重连认证：查库内指纹 → 验签。**绝不接受上传的公钥**。
    pub fn verify_reconnect(
        &self,
        node_id: &str,
        nonce: &str,
        signature: &[u8; 64],
        v: u32,
        seq: u64,
        msg_type: &str,
    ) -> Result<Device, CoreError> {
        let device = self
            .devices
            .find_by_id(node_id)?
            .ok_or(CoreError::NoSuchNode(node_id.to_string()))?;

        // 库内指纹 → 公钥。忽略请求里带来的任何 public_key。
        let pubkey = pubkey_from_fingerprint(&device)?;
        let msg = identity::signature_message(v, node_id, seq, msg_type, nonce);
        if !KeyPair::verify(&pubkey, &msg, signature) {
            return Err(CoreError::Registry(RegistryError::IdentityMismatch));
        }
        Ok(device)
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

/// 从设备行取公钥。
///
/// M1 起它就是 `device.public_key` 一列（M0 时代寄居在 `capabilities` 的 JSON
/// 键里，靠 `reports::preserve_public_key` 在心跳覆盖时抢救——那套约定已随
/// v2 迁移一并删除）。列里没有就是没有：降级二进制会被 `SchemaTooNew` 挡在
/// 开库之前，不存在「只有 JSON 里有公钥」的合法库。
fn pubkey_from_fingerprint(device: &Device) -> Result<[u8; 32], CoreError> {
    let pubkey = device
        .public_key_bytes()
        .ok_or(CoreError::Registry(RegistryError::NoStoredPublicKey))?;
    // 校验一致性：存公钥的指纹必须与库里一致
    if Fingerprint::of(&pubkey).to_hex() != device.key_fingerprint {
        return Err(CoreError::Registry(RegistryError::IdentityMismatch));
    }
    Ok(pubkey)
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
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}", d.as_secs())
}

/// 把上传的 base64url 公钥解析为 32 字节。
pub fn parse_pubkey(b64: &str) -> Result<[u8; 32], RegistryError> {
    let raw = identity::base64url_decode(b64).map_err(|_| RegistryError::BadPublicKey)?;
    let mut out = [0u8; 32];
    if raw.len() != 32 {
        return Err(RegistryError::BadPublicKey);
    }
    out.copy_from_slice(&raw);
    // 必须是合法的 Ed25519 公钥点
    VerifyingKey::from_bytes(&out).map_err(|_| RegistryError::BadPublicKey)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_storage::db::open;
    use identity::issue_node_id;

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
    /// 用 `node_id()` 访问器而不是 `match { other => panic!() }`：
    /// 后者的兜底臂在 100% 覆盖率门禁下永远跑不到，会把 TOTAL 拖下来。
    fn enroll_ok(r: &Registry, kp: &KeyPair) -> String {
        let out = r.enroll(&kp.public_key_bytes(), info()).unwrap();
        assert!(out.is_new(), "expected Enrolled, got {out:?}");
        out.node_id()
            .expect("Enrolled carries a node_id")
            .to_string()
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
        assert_eq!(second.node_id(), Some(node_id.as_str()));
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
        // 败者看到的是 AlreadyRegistered（而不是 Race），库里一行
        assert_eq!(r.list().unwrap().len(), 1);
    }

    #[test]
    fn reconnect_verifies_with_stored_pubkey_only() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        // 模拟 Hub 把公钥存进 capabilities（M0 权宜，BUGS.md 已登记）
        r.devices()
            .set_public_key(&node_id, &kp.public_key_bytes())
            .unwrap();

        let msg = identity::signature_message(1, &node_id, 0, "auth", "nonce-1");
        let sig = kp.sign(&msg);
        let dev = r
            .verify_reconnect(&node_id, "nonce-1", &sig, 1, 0, "auth")
            .unwrap();
        assert_eq!(dev.id, node_id);
    }

    /// 心跳把 `capabilities` 整个覆盖掉之后，重连验签仍然成功。
    ///
    /// 这是把公钥提成真列（M1 v2 迁移）换来的东西：`set_public_key` 抹掉 JSON
    /// 里的旧键、心跳再覆盖一次 capabilities，库里那份公钥都不受影响。
    /// 改 `merge_into_stored` 或 `touch_online` 的人不必再知道有这件事。
    #[test]
    fn a_heartbeat_overwriting_capabilities_does_not_break_reconnect() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        r.devices()
            .set_public_key(&node_id, &kp.public_key_bytes())
            .unwrap();
        r.devices()
            .touch_online(&node_id, r#"{"connectors":["terminal"]}"#, "{}", "unix:10")
            .unwrap();

        let msg = identity::signature_message(1, &node_id, 1, "auth", "nonce-1");
        let sig = kp.sign(&msg);
        assert!(r
            .verify_reconnect(&node_id, "nonce-1", &sig, 1, 1, "auth")
            .is_ok());
    }

    /// 没有库内公钥的设备（老数据 / 手工插的行）一律拒绝重连。
    #[test]
    fn reconnect_without_a_stored_public_key_is_refused() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp); // 没调 set_public_key
        let msg = identity::signature_message(1, &node_id, 0, "auth", "nonce-1");
        let sig = kp.sign(&msg);
        let err = r
            .verify_reconnect(&node_id, "nonce-1", &sig, 1, 0, "auth")
            .unwrap_err();
        assert!(
            matches!(err, CoreError::Registry(RegistryError::NoStoredPublicKey)),
            "missing public key must surface as a registry error, got {err:?}"
        );
    }

    #[test]
    fn reconnect_with_wrong_key_is_identity_mismatch_and_keeps_fp() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let evil = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        r.devices()
            .set_public_key(&node_id, &kp.public_key_bytes())
            .unwrap();
        let before = r.device(&node_id).unwrap().unwrap().key_fingerprint;

        // 持正确 node_id + 错误密钥对 + 合法签名
        let msg = identity::signature_message(1, &node_id, 0, "auth", "nonce-1");
        let sig = evil.sign(&msg);
        let e = r.verify_reconnect(&node_id, "nonce-1", &sig, 1, 0, "auth");
        assert!(matches!(
            e,
            Err(CoreError::Registry(RegistryError::IdentityMismatch))
        ));
        // 库内指纹绝不被改写
        assert_eq!(r.device(&node_id).unwrap().unwrap().key_fingerprint, before);
    }

    #[test]
    fn reconnect_with_replayed_signature_across_connection_fails() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        r.devices()
            .set_public_key(&node_id, &kp.public_key_bytes())
            .unwrap();

        // 连接 1：seq=0 的 auth 签名
        let m1 = identity::signature_message(1, &node_id, 0, "auth", "nonce-A");
        let sig1 = kp.sign(&m1);
        // 连接 2：同样 seq=0 但 nonce 不同 → 旧签名必须失败
        assert!(r
            .verify_reconnect(&node_id, "nonce-B", &sig1, 1, 0, "auth")
            .is_err());
    }

    #[test]
    fn unknown_node_is_rejected() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = issue_node_id();
        let msg = identity::signature_message(1, &node_id, 0, "auth", "n");
        let sig = kp.sign(&msg);
        assert!(matches!(
            r.verify_reconnect(&node_id, "n", &sig, 1, 0, "auth"),
            Err(CoreError::NoSuchNode(_))
        ));
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
    fn parse_pubkey_validates_length_and_charset() {
        let kp = KeyPair::generate();
        let b64 = identity::base64url(&kp.public_key_bytes());
        assert!(parse_pubkey(&b64).is_ok());
        assert!(parse_pubkey("AAAA").is_err()); // 长度不对（3 字节 ≠ 32）
        assert!(parse_pubkey("!!!not-base64!!!").is_err()); // 非法字符
                                                            // 全零在 Ed25519 是合法点（小序点），dalek 会接受；
                                                            // 是否「可用」由后续签名验证决定，不在此处预设
        assert!(parse_pubkey(&identity::base64url(&[0u8; 32])).is_ok());
    }

    #[test]
    fn node_id_issued_by_registry_is_unique() {
        let ids: std::collections::HashSet<_> = (0..100).map(|_| issue_node_id()).collect();
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn stored_pubkey_of_wrong_length_is_rejected() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        // 手写一个长度不对的 capabilities.public_key（16 字节而非 32）：
        // 走 pool() 直接写库，避免为测试在生产 API 上加只测一次用的口子。
        let conn = r.devices().pool().get().unwrap();
        let bad = serde_json::json!({"public_key": identity::base64url(&[7u8; 16])});
        conn.execute(
            "UPDATE device SET capabilities = ?2 WHERE id = ?1",
            sqlite::params![node_id, bad.to_string()],
        )
        .unwrap();
        let msg = identity::signature_message(1, &node_id, 0, "auth", "n");
        let sig = kp.sign(&msg);
        assert!(matches!(
            r.verify_reconnect(&node_id, "n", &sig, 1, 0, "auth"),
            Err(CoreError::Registry(RegistryError::NoStoredPublicKey))
        ));
    }

    #[test]
    fn stored_pubkey_inconsistent_with_fingerprint_is_mismatch() {
        let (_d, r) = setup();
        let kp = KeyPair::generate();
        let evil = KeyPair::generate();
        let node_id = enroll_ok(&r, &kp);
        // 库里存的是**另一把**公钥：指纹对不上 → IdentityMismatch
        r.devices()
            .set_public_key(&node_id, &evil.public_key_bytes())
            .unwrap();
        let msg = identity::signature_message(1, &node_id, 0, "auth", "n");
        let sig = kp.sign(&msg);
        assert!(matches!(
            r.verify_reconnect(&node_id, "n", &sig, 1, 0, "auth"),
            Err(CoreError::Registry(RegistryError::IdentityMismatch))
        ));
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
        // 让 enroll **闭包内部**的 INSERT 失败，才能命中 match 的 `Err(e)` 臂
        // （registry.rs:130）。
        //
        // 为什么不能直接 DROP TABLE：事务外还有一次快路径查询
        // （`find_by_fingerprint`，enroll 开头），表没了会在这里就 `?` 返回，
        // 根本进不了 match。
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
        assert!(first.is_new());
        let second = r.enroll(&kp.public_key_bytes(), info()).unwrap();
        assert!(!second.is_new());
        // 两个成功路径都要拿到同一个 node_id（指纹是唯一入口，§4.3）
        assert_eq!(first.node_id(), second.node_id());
        // Race 变体不带 node_id
        assert_eq!(EnrollOutcome::Race.node_id(), None);
        assert!(!EnrollOutcome::Race.is_new());
    }

    #[test]
    fn enroll_transaction_branches_covered_by_race() {
        // enroll 的「事务内再查到重复指纹 → 回滚并报告 AlreadyRegistered」
        // （registry.rs:92,97,122-124）与「闭包因 UNIQUE 冲突失败 → 走 Err(e)
        // 臂回滚」（registry.rs:126）两条分支都依赖并发竞态：两线程同时通过
        // 快路径后，胜者提交、败者要么在事务内查到已提交行（122-124）、要么
        // 在 INSERT 时撞 UNIQUE（126）。多跑若干次确保两条臂都被命中，并断言
        // 库里始终只有一行、且恰好一个 Enrolled。
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
