//! Hub 登录：Argon2id 密码哈希、setup token、session 签发。
//!
//! 设计要点（`security-and-lifecycle.md` §2.4）：
//! - **没有默认密码**；首次访问设密码，`POST /setup` 必须带 `X-Setup-Token`。
//! - **Argon2id**（不是 SHA-256：快哈希能被 GPU 暴力破解）。
//! - **Argon2id 必须走 `spawn_blocking`**——`m=65536,t=3,p=4` 单次 50–200ms，
//!   在 tokio worker 上同步执行会阻塞心跳与 `/ws` 读循环。
//! - **session HMAC 密钥 = Hub 首次初始化时的 32 字节随机值**，
//!   **不用明文密码**（改密码会使全部会话失效）、**不用设备指纹派生**（死锁 + 公开值）。

pub mod error;
pub use error::AuthError;

use argon2::password_hash::SaltString;
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier};
use hub_storage::SecretRepo;
use rand::RngCore;

use crate::identity::base64url;

/// Argon2id 参数。可按 `nproc` 下调（见 [`AuthParams::for_cpu_count`]）。
pub const DEFAULT_M_COST: u32 = 65536; // KiB
pub const DEFAULT_T_COST: u32 = 3;
pub const DEFAULT_P_COST: u32 = 4;

/// setup token 长度（字节）。
pub const SETUP_TOKEN_BYTES: usize = 16;

/// setup token 最大失败次数，超过即失效（防爆破）。
pub const SETUP_TOKEN_MAX_FAILURES: u32 = 10;

/// `/login` 全局并发信号量大小。
pub const LOGIN_SEMAPHORE_PERMITS: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct AuthParams {
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for AuthParams {
    fn default() -> Self {
        Self {
            m_cost: DEFAULT_M_COST,
            t_cost: DEFAULT_T_COST,
            p_cost: DEFAULT_P_COST,
        }
    }
}

impl AuthParams {
    /// 按 CPU 核数下调 m_cost，避免小机器上登录把整机拖死。
    ///
    /// 4 核以上用默认；2–3 核减半；1 核用 1/4。
    pub fn for_cpu_count(n: usize) -> Self {
        let m = match n {
            0..=1 => DEFAULT_M_COST / 4,
            2..=3 => DEFAULT_M_COST / 2,
            _ => DEFAULT_M_COST,
        };
        Self {
            m_cost: m.max(8192),
            ..Self::default()
        }
    }

    /// 构造 Argon2 实例。参数非法（`m_cost` 超上限等）返回
    /// [`AuthError::HashFailed`]——**不 panic**：`m_cost` 可由
    /// [`AuthParams::for_cpu_count`] 按 CPU 核数算出，将来若调整公式
    /// 出界，那应该是一次可报告的登录失败，不是整个 Hub 进程退出。
    fn argon2(&self) -> Result<Argon2<'static>, AuthError> {
        Ok(Argon2::new(
            Algorithm::Argon2id,
            argon2::Version::V0x13,
            Params::new(self.m_cost, self.t_cost, self.p_cost, None)
                .map_err(|_| AuthError::HashFailed)?,
        ))
    }
}

/// 密码哈希（存 `secret(kind='hub',name='password')`）。
///
/// 返回 PHC 字符串 `argon2id$m=...,t=...,p=...$<salt>$<hash>`。
/// **纯 CPU 密集，调用方必须放在 `spawn_blocking` 里。**
pub fn hash_password(password: &str, params: AuthParams) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut rand::thread_rng());
    let hash = params
        .argon2()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| AuthError::HashFailed)?;
    Ok(hash.to_string())
}

/// 校验密码。**同样必须放 `spawn_blocking`。**
pub fn verify_password(password: &str, stored: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(stored).map_err(|_| AuthError::MalformedHash)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::WrongPassword)
}

/// 首次访问的 setup token（16 字节，base64url）。
#[derive(Debug, Clone)]
pub struct SetupToken {
    token: String,
    failures: u32,
}

impl SetupToken {
    pub fn generate() -> Self {
        let mut b = [0u8; SETUP_TOKEN_BYTES];
        rand::thread_rng().fill_bytes(&mut b);
        Self {
            token: base64url(&b),
            failures: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.token
    }

    /// 已失败次数。
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// 是否已因失败次数过多而失效。
    pub fn is_locked(&self) -> bool {
        self.failures >= SETUP_TOKEN_MAX_FAILURES
    }

    /// 校验。恒定时间比较；失败计数 +1。
    pub fn check(&mut self, candidate: &str) -> Result<(), AuthError> {
        if self.is_locked() {
            return Err(AuthError::SetupTokenLocked);
        }
        if constant_time_eq(candidate.as_bytes(), self.token.as_bytes()) {
            Ok(())
        } else {
            self.failures += 1;
            Err(AuthError::SetupTokenMismatch)
        }
    }
}

/// 恒定时间比较。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Hub 认证状态。
pub struct HubAuth {
    repo: SecretRepo,
    params: AuthParams,
}

impl HubAuth {
    pub fn new(pool: hub_storage::Db, params: AuthParams) -> Self {
        Self {
            repo: SecretRepo::new(pool),
            params,
        }
    }

    /// Hub 是否已初始化（`secret` 有 `hub.password`）。
    pub fn is_initialized(&self) -> Result<bool, AuthError> {
        Ok(self.repo.exists("hub", "password")?)
    }

    /// 首次设置密码。已初始化则拒绝。
    pub fn setup_password(&self, password: &str) -> Result<(), AuthError> {
        if self.is_initialized()? {
            return Err(AuthError::AlreadyInitialized);
        }
        if password.len() < 8 {
            return Err(AuthError::PasswordTooShort);
        }
        let hash = hash_password(password, self.params)?;
        self.repo.set("hub", "password", &hash, &now())?;
        Ok(())
    }

    /// 登录。
    pub fn login(&self, password: &str) -> Result<(), AuthError> {
        let stored = self
            .repo
            .get("hub", "password")?
            .ok_or(AuthError::NotInitialized)?;
        verify_password(password, &stored)
    }

    /// 改密码。**不影响已签发 session**（签名密钥不是明文密码）。
    pub fn change_password(&self, old: &str, new: &str) -> Result<(), AuthError> {
        self.login(old)?;
        if new.len() < 8 {
            return Err(AuthError::PasswordTooShort);
        }
        let hash = hash_password(new, self.params)?;
        self.repo.set("hub", "password", &hash, &now())?;
        Ok(())
    }

    /// 取（或首次生成）session HMAC 密钥：**32 字节随机值**。
    ///
    /// **不用明文密码、不用设备指纹派生**——前者改密码即废全部会话，
    /// 后者在 `device` 表为空时算不出来（首次部署死锁）且指纹是公开值。
    pub fn session_hmac_key(&self) -> Result<[u8; 32], AuthError> {
        if let Some(b64) = self.repo.get("hub", "session_hmac")? {
            let raw =
                crate::identity::base64url_decode(&b64).map_err(|_| AuthError::MalformedHmacKey)?;
            let mut out = [0u8; 32];
            if raw.len() != 32 {
                return Err(AuthError::MalformedHmacKey);
            }
            out.copy_from_slice(&raw);
            return Ok(out);
        }
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        self.repo
            .set("hub", "session_hmac", &base64url(&b), &now())?;
        Ok(b)
    }

    /// 轮换 session HMAC 密钥——这才会让全部已签发 session 失效。
    pub fn rotate_session_hmac(&self) -> Result<[u8; 32], AuthError> {
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        self.repo
            .set("hub", "session_hmac", &base64url(&b), &now())?;
        Ok(b)
    }

    pub fn params(&self) -> AuthParams {
        self.params
    }
}

pub(crate) fn now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}", d.as_secs())
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
    use hub_storage::db::open;

    fn auth() -> (tempfile::TempDir, HubAuth) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("t.db");
        // 测试用小参数，跑得快
        let params = AuthParams {
            m_cost: 8192,
            t_cost: 1,
            p_cost: 1,
        };
        (d, HubAuth::new(open(&path).unwrap(), params))
    }

    #[test]
    fn first_setup_password_then_login() {
        let (_d, a) = auth();
        assert!(!a.is_initialized().unwrap());
        a.setup_password("correct horse battery").unwrap();
        assert!(a.is_initialized().unwrap());
        a.login("correct horse battery").unwrap();
    }

    #[test]
    fn wrong_password_rejected() {
        let (_d, a) = auth();
        a.setup_password("correct horse battery").unwrap();
        assert!(matches!(
            a.login("wrong password entirely"),
            Err(AuthError::WrongPassword)
        ));
    }

    #[test]
    fn setup_twice_is_rejected() {
        let (_d, a) = auth();
        a.setup_password("correct horse battery").unwrap();
        assert!(matches!(
            a.setup_password("another password"),
            Err(AuthError::AlreadyInitialized)
        ));
    }

    #[test]
    fn short_password_rejected() {
        let (_d, a) = auth();
        assert!(matches!(
            a.setup_password("short"),
            Err(AuthError::PasswordTooShort)
        ));
        assert!(!a.is_initialized().unwrap());
    }

    #[test]
    fn hash_is_argon2id_and_not_plaintext() {
        let (_d, a) = auth();
        a.setup_password("correct horse battery").unwrap();
        let stored = a.repo.get("hub", "password").unwrap().unwrap();
        assert!(stored.starts_with("$argon2id$"), "not argon2id: {stored}");
        assert!(!stored.contains("correct horse battery"));
    }

    #[test]
    fn salt_is_random_per_hash() {
        let p = AuthParams {
            m_cost: 8192,
            t_cost: 1,
            p_cost: 1,
        };
        let h1 = hash_password("same password", p).unwrap();
        let h2 = hash_password("same password", p).unwrap();
        assert_ne!(h1, h2, "salt must be random per hash");
        assert!(verify_password("same password", &h1).is_ok());
        assert!(verify_password("same password", &h2).is_ok());
    }

    #[test]
    fn change_password_keeps_existing_sessions() {
        let (_d, a) = auth();
        a.setup_password("correct horse battery").unwrap();
        let key_before = a.session_hmac_key().unwrap();
        // 签发一个 session
        let sess = crate::session::SessionStore::new(a.session_hmac_key().unwrap());
        let token = sess.issue("admin");
        // 改密码
        a.change_password("correct horse battery", "new password here")
            .unwrap();
        // 旧 session 仍有效（密钥不是明文密码）
        let store_after = crate::session::SessionStore::new(key_before);
        assert_eq!(store_after.verify(&token).unwrap().subject, "admin");
        assert_eq!(a.session_hmac_key().unwrap(), key_before);
    }

    #[test]
    fn rotating_hmac_invalidates_sessions() {
        let (_d, a) = auth();
        a.setup_password("correct horse battery").unwrap();
        let k1 = a.session_hmac_key().unwrap();
        let store = crate::session::SessionStore::new(k1);
        let token = store.issue("admin");
        let k2 = a.rotate_session_hmac().unwrap();
        assert_ne!(k1, k2);
        let store2 = crate::session::SessionStore::new(k2);
        assert!(store2.verify(&token).is_err());
    }

    #[test]
    fn session_key_is_random_not_derived_from_device() {
        // 首次部署：device 表为空也必须能拿到 session 密钥
        let (_d, a) = auth();
        let k = a.session_hmac_key().unwrap();
        assert_eq!(k.len(), 32);
        assert!(k.iter().any(|&b| b != 0));
        // 换一个全新库得到不同密钥（不是从某个公开值派生的）
        let d2 = tempfile::tempdir().unwrap();
        let a2 = HubAuth::new(
            open(&d2.path().join("t.db")).unwrap(),
            AuthParams::default(),
        );
        assert_ne!(a2.session_hmac_key().unwrap(), k);
    }

    #[test]
    fn change_password_rejects_short_new_password() {
        let (_d, a) = auth();
        a.setup_password("correct horse battery").unwrap();
        // 新密码太短：必须拒，且**旧密码仍然有效**（不能被半途改掉）
        assert!(matches!(
            a.change_password("correct horse battery", "short"),
            Err(AuthError::PasswordTooShort)
        ));
        a.login("correct horse battery").unwrap();
    }

    #[test]
    fn malformed_stored_hmac_key_is_reported_not_panicked() {
        let (_d, a) = auth();
        // 直接往 secret 表写一个长度不对的值（模拟损坏/被外部改过）
        a.repo
            .set("hub", "session_hmac", &base64url(&[1u8; 16]), &now())
            .unwrap();
        // 必须是可报告的 MalformedHmacKey，不是 panic 或静默接受
        assert!(matches!(
            a.session_hmac_key(),
            Err(AuthError::MalformedHmacKey)
        ));
    }

    #[test]
    fn params_getter_roundtrips() {
        let (_d, a) = auth();
        let p = a.params();
        assert_eq!(p.m_cost, 8192);
        assert_eq!(p.t_cost, 1);
        assert_eq!(p.p_cost, 1);
    }

    #[test]
    fn session_key_survives_restart() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("t.db");
        let k1 = {
            let a = HubAuth::new(open(&path).unwrap(), AuthParams::default());
            a.session_hmac_key().unwrap()
        };
        let a2 = HubAuth::new(open(&path).unwrap(), AuthParams::default());
        assert_eq!(a2.session_hmac_key().unwrap(), k1);
    }

    #[test]
    fn setup_token_checks_and_locks() {
        let mut t = SetupToken::generate();
        assert_eq!(t.as_str().len(), 22);
        assert!(!t.is_locked());
        assert!(t.check(t.as_str().to_string().as_str()).is_ok());
        // 错误候选
        for _ in 0..(SETUP_TOKEN_MAX_FAILURES - 1) {
            assert!(t.check("wrong").is_err());
        }
        assert_eq!(t.failures(), SETUP_TOKEN_MAX_FAILURES - 1);
        // 再错一次即锁
        assert!(t.check("wrong").is_err());
        assert!(t.is_locked());
        // 锁后连正确 token 也拒（防爆破）
        let good = t.as_str().to_string();
        assert!(matches!(t.check(&good), Err(AuthError::SetupTokenLocked)));
    }

    #[test]
    fn params_scale_down_on_small_cpu() {
        let big = AuthParams::for_cpu_count(8);
        let small = AuthParams::for_cpu_count(1);
        assert!(big.m_cost > small.m_cost);
        assert!(small.m_cost >= 8192);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn malformed_stored_hash_is_reported() {
        assert!(matches!(
            verify_password("x", "not-a-hash"),
            Err(AuthError::MalformedHash)
        ));
    }

    #[test]
    fn login_before_setup_is_not_initialized() {
        let (_d, a) = auth();
        assert!(matches!(
            a.login("anything at all"),
            Err(AuthError::NotInitialized)
        ));
    }
}
