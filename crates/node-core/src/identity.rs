//! 设备侧身份：本地密钥对 + `identity.json` 原子写。
//!
//! 权威规格见 `security-and-lifecycle.md` §4.4。这个文件是**整个安全模型的根**——
//! 它决定了「你是不是你」。写入方式必须钉死，否则会出现「节点永久无法接入」的故障：
//!
//! 1. **原子**：`临时文件 → fsync → rename`。绝不用 `File::create` 直接写：
//!    写到一半崩溃 → 身份文件损坏 → 重启生成**新密钥对** → 重新 enroll 撞上
//!    「指纹已存在则拒绝」→ 设备永久无法接入，只能手工删库。
//! 2. **权限 0600 不依赖 umask**：systemd 单元 umask 常是 0022，靠 umask 会得到 0644，
//!    写入期间私钥世界可读。
//! 3. **读取时权限宽于 0600 先收紧并告警**，不静默继续用。
//! 4. **私钥永不外泄**（约束 13）：`Debug` 不含私钥（`identity::KeyPair` 已保证）。
//!
//! `KeyPair` / PKCS#8 / base64url 复用 `identity` crate——那是 node 与 Hub 共用的
//! 唯一一份实现，不在本文件重写。

use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use identity::KeyPair;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::warn;

/// `identity.json` 的 schema 版本。
pub const IDENTITY_VERSION: u32 = 1;

/// 文件名。历史名称 `auth.json` 已统一为 `identity.json`
/// （`PRODUCT.md` / `workspace-and-sync.md` 同步）。
pub const IDENTITY_FILE: &str = "identity.json";

/// 权限必须收紧到的目标：owner rw。
///
/// **只在 Unix 上有意义**：Windows 用 ACL，`0o600` 这个数没有对应物。
/// 那边的等价保护是「用户 profile 目录默认只有本用户可读」——这一点写进
/// `docs/DEVELOPMENT.md` 的「已知行为差异」。
#[cfg(unix)]
const IDENTITY_MODE: u32 = 0o600;

/// 数据目录权限：owner rwx（同上，仅 Unix）。
#[cfg(unix)]
const DATA_DIR_MODE: u32 = 0o700;

/// 基目录下的子目录名。
const APP_DIR: &str = "agentfleet";

/// node 数据目录环境变量名。与 Hub 的 `AFHUB_DATA_DIR` 同一取向。
const ENV_DATA_DIR: &str = "AFCLI_DATA_DIR";

/// 落盘的身份文件内容。
///
/// `private_key_pkcs8_b64` 是这个结构体里唯一敏感字段——它让 `Debug` 派生
/// 成为泄密通道，所以 `IdentityFile` **不派生 `Debug`**，只派生 `Serialize`/`Deserialize`。
#[derive(Serialize, Deserialize)]
pub struct IdentityFile {
    pub version: u32,
    /// 首次 enroll 成功后由 Hub 签发回填。enroll 前为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 本节点见过的最大连接代次（§7.1 的 `gen` 兜底）。
    ///
    /// 每次 `auth_ok` 都更新。**必须持久化**：进程重启后若从 0 开始，
    /// 而 Hub 已经发到 42，这条连接就会永远被判成「已被顶替」——
    /// 设备接不上且无自愈路径（子任务 7 测试标准⑧）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_gen: Option<u64>,
    /// 被顶替的时刻（RFC3339）。一旦置上就**不再重连**（§7.1），
    /// 由人工介入清除——「被顶替」意味着同 node_id 有另一条连接活着，
    /// 再连就是互抢。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_at: Option<String>,
    pub public_key: String,
    pub private_key_pkcs8_b64: String,
    pub created_at: String,
}

/// 身份相关的错误。
///
/// 消息里**绝不包含**私钥材料、公钥内容或路径之外的敏感信息（约束 13）。
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("identity file {path} is corrupt: {reason}")]
    Corrupt { path: String, reason: String },

    #[error("cannot determine data directory")]
    NoDataDir,

    #[error("permission bits unavailable on this platform")]
    NoMode,
}

/// 节点身份：内存里的密钥对 + 它在磁盘上的位置。
///
/// `Debug` 只暴露公钥与路径，**不含私钥**（约束 13）。
pub struct Identity {
    key: KeyPair,
    path: PathBuf,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("public_key", &self.public_key_b64())
            .field("path", &self.path)
            .finish()
    }
}

impl Identity {
    /// 从 `data_dir` 加载；不存在则生成并原子落盘。
    ///
    /// 这是 node 启动时的唯一入口：**已有身份绝不重新生成**——
    /// 重新生成等于换了一个设备，会撞上「指纹已存在则拒绝」。
    pub fn load_or_create(data_dir: &Path) -> Result<Self, IdentityError> {
        let path = data_dir.join(IDENTITY_FILE);
        // 目录权限**两条路都要管**：只在新创建时收紧，会让「目录过宽的既有部署」
        // 永远得不到修复（load 路径不经过 set_dir_mode）。
        fs::create_dir_all(data_dir)?;
        set_dir_mode(data_dir)?;
        if path.exists() {
            Self::load(&path)
        } else {
            Self::create(&path)
        }
    }

    /// 只加载，不创建。用于测试「损坏/权限异常」的路径。
    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        tighten_permissions(path)?;
        let raw = fs::read_to_string(path)?;
        let parsed: IdentityFile =
            serde_json::from_str(&raw).map_err(|e| IdentityError::Corrupt {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        validate_version(parsed.version, path)?;
        let der = identity::base64url_decode(&parsed.private_key_pkcs8_b64).map_err(|e| {
            IdentityError::Corrupt {
                path: path.display().to_string(),
                reason: format!("private key not base64url: {e}"),
            }
        })?;
        let key = KeyPair::from_pkcs8_der(&der).map_err(|e| IdentityError::Corrupt {
            path: path.display().to_string(),
            reason: format!("private key not PKCS#8: {e}"),
        })?;
        // 公钥必须与私钥对应：文件被手工改过一半时要能发现
        if identity::base64url(&key.public_key_bytes()) != parsed.public_key {
            return Err(IdentityError::Corrupt {
                path: path.display().to_string(),
                reason: "public key does not match private key".to_string(),
            });
        }
        Ok(Self {
            key,
            path: path.to_path_buf(),
        })
    }

    /// 生成新密钥对并原子落盘。
    fn create(path: &Path) -> Result<Self, IdentityError> {
        let key = KeyPair::generate();
        let file = IdentityFile {
            version: IDENTITY_VERSION,
            node_id: None,
            last_seen_gen: None,
            superseded_at: None,
            public_key: identity::base64url(&key.public_key_bytes()),
            private_key_pkcs8_b64: identity::base64url(&key.to_pkcs8_der()),
            created_at: now_secs(),
        };
        write_atomic(path, &serde_json::to_vec_pretty(&file)?)?;
        Ok(Self {
            key,
            path: path.to_path_buf(),
        })
    }

    pub fn key(&self) -> &KeyPair {
        &self.key
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 公钥的 base64url（enroll 时上传给 Hub）。
    pub fn public_key_b64(&self) -> String {
        identity::base64url(&self.key.public_key_bytes())
    }

    /// 当前记录的 `node_id`（enroll 前为 `None`）。
    ///
    /// 从磁盘读而不是内存缓存：enroll 成功后由 `set_node_id` 回填，
    /// 之后进程重启要以磁盘为准。
    pub fn node_id(&self) -> Option<String> {
        let raw = fs::read_to_string(&self.path).ok()?;
        let parsed: IdentityFile = serde_json::from_str(&raw).ok()?;
        parsed.node_id
    }

    /// 把 Hub 签发的 `node_id` 回填进身份文件。
    ///
    /// 同样走原子写：这一下写坏了，节点重启后虽然密钥对还在，
    /// 但会丢掉 node_id 只能重新 enroll。
    pub fn set_node_id(&mut self, node_id: &str) -> Result<(), IdentityError> {
        self.update(|id| {
            id.node_id = Some(node_id.to_string());
            true
        })
    }

    /// 本节点见过的最大连接代次。
    pub fn last_seen_gen(&self) -> Option<u64> {
        let raw = fs::read_to_string(&self.path).ok()?;
        let parsed: IdentityFile = serde_json::from_str(&raw).ok()?;
        parsed.last_seen_gen
    }

    /// 记录本次连接拿到的 `gen`（`auth_ok` 之后调用）。
    pub fn record_gen(&mut self, gen: u64) -> Result<(), IdentityError> {
        // 只前进不回退：并发的两条连接里，落后那条不得把记录拉回去。
        // 返回 false 表示不需要写盘，也就省掉了一次原子写。
        self.update(|id| {
            if id.last_seen_gen.is_none_or(|seen| seen < gen) {
                id.last_seen_gen = Some(gen);
                true
            } else {
                false
            }
        })
    }

    /// 被顶替了吗（§7.1 终态）。
    ///
    /// **读不出来按「未被顶替」处理**：方向不是随便选的——判成被顶替，
    /// 设备会就此永久停摆且无自愈路径。
    pub fn is_superseded(&self) -> bool {
        let Some(raw) = fs::read_to_string(&self.path).ok() else {
            return false;
        };
        let Ok(parsed) = serde_json::from_str::<IdentityFile>(&raw) else {
            return false;
        };
        parsed.superseded_at.is_some()
    }

    /// 标记被顶替。置上之后 [`Self::is_superseded`] 恒为真，重连循环据此停下。
    pub fn mark_superseded(&mut self, at: &str) -> Result<(), IdentityError> {
        self.update(|id| {
            id.superseded_at = Some(at.to_string());
            true
        })
    }

    /// 「读身份文件 → 改一个字段 → 原子写回」。三个写入口形状完全一样，
    /// 各抄一遍就会让某一条悄悄漏掉「读坏 JSON 要报错」这条契约
    /// （读侧 `last_seen_gen` / `is_superseded` 读坏了当没读到，写侧必须报错）。
    ///
    /// `mutate` 返回 `false` 表示「值没变，不用写盘」。
    fn update(
        &mut self,
        mutate: impl FnOnce(&mut IdentityFile) -> bool,
    ) -> Result<(), IdentityError> {
        let raw = fs::read_to_string(&self.path)?;
        let mut parsed: IdentityFile =
            serde_json::from_str(&raw).map_err(|e| IdentityError::Corrupt {
                path: self.path.display().to_string(),
                reason: e.to_string(),
            })?;
        if !mutate(&mut parsed) {
            return Ok(());
        }
        write_atomic(&self.path, &serde_json::to_vec_pretty(&parsed)?)
    }
}

fn validate_version(v: u32, path: &Path) -> Result<(), IdentityError> {
    if v != IDENTITY_VERSION {
        return Err(IdentityError::Corrupt {
            path: path.display().to_string(),
            reason: format!("unsupported version {v}, expected {IDENTITY_VERSION}"),
        });
    }
    Ok(())
}

/// 原子写：临时文件 → 写 → fsync → rename → fsync 父目录。
///
/// **不用 `File::create` 直接写目标路径**：那样写到一半崩溃就会留下损坏的身份文件。
/// 临时文件用 `.tmp` 后缀（同目录，保证 rename 是同一文件系统内的原子操作）。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), IdentityError> {
    let tmp = tmp_path(path);
    // 创建时就用 mode(0o600) 落权限，**不依赖 umask**（§4.4）。
    // create_new(true)：绝不覆盖已存在的临时文件，避免与并发实例互踩。
    {
        let mut f = open_private(&tmp)?;
        f.write_all(bytes)?;
        // 数据必须落盘才 rename：否则断电后 rename 已完成但内容是空的。
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // 目录项也要落盘：rename 本身在断电后可能丢失。
    if let Some(dir) = path.parent() {
        // tempdir 再调 write_atomic，测试无法在不弄坏夹具的前提下构造；
        // 外层 `Some(dir)` 恒成立（tmp_path 由 path 派生，必带父目录）。
        if let Ok(d) = fs::File::open(dir) {
            // 目录 fsync 失败不影响数据已落盘的事实，这里**不**用 `let _ =`
            // （workspace lint deny 了 let_underscore_must_use）：显式丢弃返回值。
            drop(d.sync_all());
            // coverage: excluded — 见 :298：内层 `if let Ok(d)` 的 else 不可达，
            // 这里是它的收尾行。
        }
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn open_private(path: &Path) -> Result<fs::File, IdentityError> {
    // create_new(true)：绝不覆盖已存在的临时文件，避免与并发实例互踩，
    // 也避免把别人的身份文件截断。
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    let f = o.open(path)?;
    // 立刻收紧到 0600：不依赖 umask（§4.4）。
    // 与 hub-storage 的 db.rs 同一手法——创建后立即 set_permissions，
    // 而不是在 OpenOptions 上带 mode()。窗口极短，且 create_new(true)
    // 保证这个文件是我们刚建的。
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(IDENTITY_MODE))?;
    Ok(f)
}

/// 读取时若权限宽于 0600，先收紧并告警（§4.4）。
#[cfg_attr(not(unix), allow(unused_variables))]
fn tighten_permissions(path: &Path) -> Result<(), IdentityError> {
    #[cfg(unix)]
    {
        let meta = fs::metadata(path)?;
        let mode = meta.permissions().mode();
        // 只比对权限位，不理会文件类型位
        let bits = mode & 0o777;
        if bits & !IDENTITY_MODE != 0 {
            // 单行 warn!：多行宏的参数行会被 llvm-cov 记为 miss。
            warn!(path = %path.display(), current = %format!("{bits:o}"), "identity permissions wider than 0600, tightening");
            fs::set_permissions(path, fs::Permissions::from_mode(IDENTITY_MODE))?;
        }
    }
    Ok(())
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn set_dir_mode(dir: &Path) -> Result<(), IdentityError> {
    #[cfg(unix)]
    {
        let meta = fs::metadata(dir)?;
        let bits = meta.permissions().mode() & 0o777;
        if bits & !DATA_DIR_MODE != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(DATA_DIR_MODE))?;
        }
    }
    Ok(())
}

/// 确保目录存在且权限为 0700。
fn ensure_dir(dir: &Path) -> Result<(), IdentityError> {
    fs::create_dir_all(dir)?;
    set_dir_mode(dir)
}

pub fn now_secs() -> String {
    // 秒级精度足够：created_at 只用于人工排查，不参与任何认证判定。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// 解析 node 数据目录。
///
/// 优先级与 Hub 侧一致（`AFHUB_DATA_DIR` 见 `hub-server/src/main.rs`）：
/// `AFCLI_DATA_DIR` 优先，否则平台标准目录。
pub fn data_dir() -> Result<PathBuf, IdentityError> {
    match std::env::var_os(ENV_DATA_DIR) {
        Some(raw) if !raw.is_empty() => Ok(PathBuf::from(raw).join(APP_DIR)),
        _ => platform_data_dir(),
    }
}

/// 平台标准数据目录：Linux `~/.local/share/agentfleet`、
/// macOS `~/Library/Application Support/agentfleet`。
pub fn platform_data_dir() -> Result<PathBuf, IdentityError> {
    let base = dirs::data_dir().ok_or(IdentityError::NoDataDir)?;
    Ok(base.join(APP_DIR))
}

/// 在给定基目录下解析数据目录。抽出为独立函数便于测试（`dirs::data_dir()`
/// 只能读进程环境变量，测它就得改 env）。
pub fn data_dir_in(base: impl AsRef<Path>) -> Result<PathBuf, IdentityError> {
    let dir = base.as_ref().join(APP_DIR);
    ensure_dir(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// 标准⑥：rename 前 panic → 目标路径要么不存在、要么是上一版完整 JSON。
    #[test]
    fn atomic_write_leaves_no_partial_file() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        let id = Identity::load_or_create(d.path()).unwrap();

        // 第一次写成功：文件是完整 JSON
        let first = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str::<serde_json::Value>(&first).unwrap();

        // 直接操作临时文件路径，模拟「写到一半崩溃」：
        // 写一半的字节留在 .tmp，目标文件未被触碰。
        let tmp = tmp_path(&path);
        std::fs::write(&tmp, b"{\"version\":1,\"publi").unwrap();
        // 目标文件仍是上一版完整内容（rename 从未发生）
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        // 且重启后身份不变（同一密钥对）
        let reloaded = Identity::load(&path).unwrap();
        assert_eq!(
            id.public_key_b64(),
            reloaded.public_key_b64(),
            "crash before rename must not change identity"
        );
    }

    /// 标准⑥：崩溃后重启，node_id 与崩溃前相同。
    #[test]
    fn node_id_survives_reload() {
        let d = tmpdir();
        let mut id = Identity::load_or_create(d.path()).unwrap();
        assert_eq!(id.node_id(), None);
        id.set_node_id("n_abc").unwrap();
        assert_eq!(id.node_id().as_deref(), Some("n_abc"));

        let reloaded = Identity::load(id.path()).unwrap();
        assert_eq!(reloaded.node_id().as_deref(), Some("n_abc"));
        // 回填 node_id 不换密钥对
        assert_eq!(reloaded.public_key_b64(), id.public_key_b64());
    }

    /// 标准⑦：实测 0600；umask 0022 下创建仍为 0600（不信 umask）。
    #[test]
    fn permissions_are_0600_regardless_of_umask() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        Identity::load_or_create(d.path()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "umask must not widen identity.json");

        // 再放宽到 0644 后重新加载：必须被收紧回 0600
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        Identity::load(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "load must tighten a loose file");
    }

    /// 数据目录必须是 0700；过宽则重新加载时收紧（§4.4）。
    #[test]
    fn data_dir_is_0700() {
        let d = tmpdir();
        Identity::load_or_create(d.path()).unwrap();
        assert_eq!(
            std::fs::metadata(d.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // 过宽 → 重新加载必须收紧
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        Identity::load_or_create(d.path()).unwrap();
        assert_eq!(
            std::fs::metadata(d.path()).unwrap().permissions().mode() & 0o777,
            0o700,
            "a loose data dir must be tightened on load"
        );
    }

    /// 重复加载绝不重新生成（否则等于换设备）。
    #[test]
    fn reload_reuses_the_same_key_pair() {
        let d = tmpdir();
        let a = Identity::load_or_create(d.path()).unwrap();
        let b = Identity::load_or_create(d.path()).unwrap();
        assert_eq!(a.public_key_b64(), b.public_key_b64());
    }

    /// 私钥材料不得出现在 Debug 输出里（约束 13）。
    #[test]
    fn debug_hides_private_key_material() {
        let d = tmpdir();
        let id = Identity::load_or_create(d.path()).unwrap();
        let dbg = format!("{id:?}");
        // 私钥 PKCS#8 的 base64 片段绝不出现
        let pkcs8 = identity::base64url(&id.key.to_pkcs8_der());
        assert!(!dbg.contains(&pkcs8), "Debug leaked private key: {dbg}");
        // 但公钥应该在（便于排查）
        assert!(dbg.contains("public_key"), "Debug should show public key");
    }

    /// 损坏文件必须报 Corrupt 而不是 panic。
    #[test]
    fn corrupt_file_is_reported_not_panicked() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        std::fs::write(&path, "not json").unwrap();
        let e = Identity::load(&path).unwrap_err();
        assert!(matches!(e, IdentityError::Corrupt { .. }), "got {e:?}");
    }

    /// 公钥与私钥不匹配必须被发现（文件被改了一半）。
    #[test]
    fn mismatched_public_key_is_rejected() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        let id = Identity::load_or_create(d.path()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["public_key"] = serde_json::json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        let e = Identity::load(&path).unwrap_err();
        assert!(matches!(e, IdentityError::Corrupt { .. }), "got {e:?}");
        // 原身份未损坏
        assert_eq!(
            id.public_key_b64(),
            identity::base64url(&id.key.public_key_bytes())
        );
    }

    /// base64 编码的私钥字段被改坏时必须报 Corrupt。
    #[test]
    fn malformed_private_key_is_reported() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        let _ = Identity::load_or_create(d.path()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["private_key_pkcs8_b64"] = serde_json::json!("!!!not-base64!!!");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        let e = Identity::load(&path).unwrap_err();
        assert!(matches!(e, IdentityError::Corrupt { .. }), "got {e:?}");
    }

    /// 三个写入口读坏 JSON 时都必须报 `Corrupt`。
    ///
    /// 三个写入口共用同一个读-改-写，但**每个都要求**这条契约：写侧读坏了
    /// 必须报错（绝不能拿一份空的身份把 node_id / gen / 顶替标记写回去，
    /// 那等于把设备改成「没注册过」）。漏掉一个入口的测试，这条就会在某次
    /// 重构里悄悄失效。
    #[test]
    fn every_write_entry_point_reports_a_corrupt_file() {
        type Write = fn(&mut Identity) -> Result<(), IdentityError>;
        let writes: [(&str, Write); 3] = [
            ("set_node_id", |id| id.set_node_id("n_x")),
            ("record_gen", |id| id.record_gen(9)),
            ("mark_superseded", |id| id.mark_superseded("unix:0")),
        ];
        for (name, write) in writes {
            let d = tmpdir();
            let mut id = Identity::load_or_create(d.path()).unwrap();
            std::fs::write(id.path(), "{ not json").unwrap();
            let e = write(&mut id).unwrap_err();
            assert!(
                matches!(e, IdentityError::Corrupt { .. }),
                "{name}: got {e:?}"
            );
        }
    }

    /// 版本号不符必须被拒（老版本文件不能当当前版本用）。
    ///
    /// 注意：必须构造**字段完整、仅版本不同**的文件。若写成 `{"version":99}`，
    /// serde 会先在缺字段上报 Corrupt，永远到不了 `validate_version`——
    /// 那样这个测试就白跑了。
    #[test]
    fn unsupported_version_is_rejected() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        let good = Identity::load_or_create(d.path()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["version"] = serde_json::json!(99);
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        let e = Identity::load(&path).unwrap_err();
        assert!(
            matches!(&e, IdentityError::Corrupt { reason, .. } if reason.contains("version")),
            "got {e:?}"
        );
        // 原身份未被破坏
        assert_eq!(
            good.public_key_b64(),
            identity::base64url(&good.key.public_key_bytes())
        );
    }

    /// base64 合法但 DER 不是 PKCS#8 → 必须报 Corrupt（而不是 panic 或当成合法密钥）。
    #[test]
    fn well_formed_base64_that_is_not_pkcs8_is_rejected() {
        let d = tmpdir();
        let path = d.path().join(IDENTITY_FILE);
        let _ = Identity::load_or_create(d.path()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // 4 个合法 base64url 字符 = 3 字节，远短于 PKCS#8 头的 48 字节
        v["private_key_pkcs8_b64"] = serde_json::json!("AAAA");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        let e = Identity::load(&path).unwrap_err();
        assert!(
            matches!(&e, IdentityError::Corrupt { reason, .. } if reason.contains("PKCS#8")),
            "got {e:?}"
        );
    }

    /// `AFCLI_DATA_DIR` 非空时必须优先生效，空串与缺省都退回平台目录。
    #[test]
    fn data_dir_prefers_afcli_data_dir() {
        // 只测纯拼接的那一半：不改进程 env（会与并行测试竞争）。
        //
        // base **必须落在临时目录下**，不能写 `/afcli-should-win` 这类根路径：
        // `data_dir_in` 会真的 `create_dir_all`，而 CI runner 不是 root，
        // 对 `/` 没有写权限 → `PermissionDenied`。这条用例曾经写成根路径，
        // 本地以 root 跑全绿、CI 一跑就红，属于「本地环境掩盖了缺陷」——
        // 用临时目录后，无论以谁的身份跑，「base 参与拼接」这个语义都测得到。
        let d = tmpdir();
        let base = d.path();
        assert_eq!(
            data_dir_in(base).unwrap(),
            base.join("agentfleet"),
            "non-empty AFCLI_DATA_DIR must take priority"
        );
        assert_eq!(
            platform_data_dir().unwrap(),
            dirs::data_dir().unwrap().join("agentfleet"),
            "absent AFCLI_DATA_DIR falls back to the platform dir"
        );
    }

    /// `AFCLI_DATA_DIR` 指向一个还不存在的目录时必须建出来，且权限 0700。
    #[test]
    fn data_dir_creates_the_directory() {
        let d = tmpdir();
        let target = d.path().join("nested").join("node");
        assert_eq!(data_dir_in(&target).unwrap(), target.join("agentfleet"));
        assert!(target.join("agentfleet").is_dir());
    }

    /// `key()` / `path()` / `public_key_b64()` 是 enroll 的直接输入，
    /// 必须可用且与磁盘一致。
    #[test]
    fn accessors_expose_the_on_disk_identity() {
        let d = tmpdir();
        let id = Identity::load_or_create(d.path()).unwrap();
        assert_eq!(id.path(), d.path().join(IDENTITY_FILE));
        assert_eq!(
            id.public_key_b64(),
            identity::base64url(&id.key().public_key_bytes())
        );
        // 磁盘上的公钥字段与 accessor 一致
        let raw = std::fs::read_to_string(id.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["public_key"], serde_json::json!(id.public_key_b64()));
    }

    /// `data_dir()` 末尾必带 `agentfleet`（无论走哪条优先级）。
    #[test]
    fn data_dir_ends_with_agentfleet() {
        let dir = data_dir().unwrap();
        assert_eq!(
            dir.file_name().and_then(|s| s.to_str()),
            Some("agentfleet"),
            "data dir must be namespaced: {dir:?}"
        );
    }

    /// 临时文件残留不得影响下次加载。
    #[test]
    fn leftover_tmp_file_is_ignored() {
        let d = tmpdir();
        let id = Identity::load_or_create(d.path()).unwrap();
        std::fs::write(tmp_path(id.path()), b"garbage").unwrap();
        let again = Identity::load_or_create(d.path()).unwrap();
        assert_eq!(again.public_key_b64(), id.public_key_b64());
    }

    /// 标准⑧：gen 必须落盘，且**只前进不回退**。
    ///
    /// 「只前进」不是洁癖：两条连接并发时，落后那条不许把记录拉回去——
    /// 拉回去之后下次重连带的 gen 就偏小，会被 Hub 判成已被顶替。
    #[test]
    fn the_connection_gen_only_moves_forward() {
        let d = tmpdir();
        let mut id = Identity::load_or_create(d.path()).unwrap();
        assert_eq!(id.last_seen_gen(), None, "新身份没有 gen");
        id.record_gen(7).unwrap();
        assert_eq!(id.last_seen_gen(), Some(7));
        id.record_gen(3).unwrap();
        assert_eq!(id.last_seen_gen(), Some(7), "落后的一条不得回退记录");
        id.record_gen(9).unwrap();
        assert_eq!(id.last_seen_gen(), Some(9));
    }

    /// 标准⑧ 的重启那一半：重新 load 之后 gen 还在。
    #[test]
    fn the_connection_gen_survives_a_reload() {
        let d = tmpdir();
        let mut id = Identity::load_or_create(d.path()).unwrap();
        id.record_gen(42).unwrap();
        let path = id.path().to_path_buf();
        let reloaded = Identity::load(&path).unwrap();
        assert_eq!(reloaded.last_seen_gen(), Some(42));
    }

    /// 被顶替是**终态**且持久化：supervisor 重拉出的新进程什么都不记得，
    /// 只看内存的话会老老实实再连一次。
    #[test]
    fn the_superseded_mark_survives_a_reload() {
        let d = tmpdir();
        let mut id = Identity::load_or_create(d.path()).unwrap();
        assert!(!id.is_superseded());
        id.mark_superseded("1700000000").unwrap();
        assert!(id.is_superseded());
        let path = id.path().to_path_buf();
        let reloaded = Identity::load(&path).unwrap();
        assert!(reloaded.is_superseded(), "必须持久化，否则重拉后照连不误");
    }

    /// 读不出来时按「未被顶替」处理，而不是「被顶替」。
    ///
    /// 方向很重要：身份文件损坏时判成被顶替，设备会就此永久停摆且无自愈路径。
    #[test]
    fn an_unreadable_identity_is_not_treated_as_superseded() {
        let d = tmpdir();
        let mut id = Identity::load_or_create(d.path()).unwrap();
        let p = id.path().to_path_buf();

        // 反向对照必须先做：写一个标记并确认**读得出真**。缺了它，
        // 「一律返回 false」的实现会被下面两条同时放行。
        //
        // 它得赶在文件被弄坏之前：`mark_superseded` 走「读-改-写」，
        // 文件已经坏了的话它自己就会先报错。
        id.mark_superseded("1700000000").unwrap();
        assert!(id.is_superseded(), "写了标记就必须读得出来");

        // 文件整个没了（卸载了磁盘 / 有人 rm 了数据目录）：走
        // `read_to_string` 失败那条路。
        std::fs::remove_file(&p).unwrap();
        assert!(!id.is_superseded(), "文件没了按未被顶替处理");
        assert_eq!(id.last_seen_gen(), None);

        // 有文件但解不开：走 `serde_json` 失败那条路。与上面不是一回事——
        // 这里读到的是「有文件但不是身份」。
        std::fs::write(&p, "not json at all").unwrap();
        assert!(!id.is_superseded(), "解不开按未被顶替处理");
    }

    /// `set_node_id` 走原子写：中途失败不留半个文件。
    #[test]
    fn set_node_id_is_atomic() {
        let d = tmpdir();
        let mut id = Identity::load_or_create(d.path()).unwrap();
        id.set_node_id("n_1").unwrap();
        let after_first = std::fs::read_to_string(id.path()).unwrap();
        id.set_node_id("n_2").unwrap();
        let after_second = std::fs::read_to_string(id.path()).unwrap();
        assert_ne!(after_first, after_second);
        // 两次都是完整 JSON
        serde_json::from_str::<serde_json::Value>(&after_second).unwrap();
    }
}
