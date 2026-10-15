//! 签名 session cookie。
//!
//! 设计要点（`security-and-lifecycle.md` §2.4）：
//! - **M0 不建 `session` 表**：cookie 自包含，HMAC 签名即凭证。
//! - **密钥 = 32 字节随机值**，存 `secret(kind='hub',name='session_hmac')`。
//!   **不用明文密码**（改密码会废掉全部会话）、**不用设备指纹派生**
//!   （`device` 表为空时算不出来 → 首次部署死锁；且指纹是公开值）。
//! - **防篡改靠签名**，不是加密：payload 是明文可读的，改动即验签失败。

pub mod error;
pub use error::SessionError;

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// session 有效期（秒）。默认 30 天。
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// 时钟偏移容差（秒）：`exp` 早于此刻这么多仍算有效，容忍节点时钟偏差。
const CLOCK_SKEW_TOLERANCE_SECS: i64 = 60;

/// 一个已签发的 session。
#[derive(Debug, Clone)]
pub struct Session {
    /// 主体。M0 只有 `admin`（单用户），但字段留着便于 M1 引入多主体。
    pub subject: String,
    /// 过期时间（unix 秒）。
    pub exp: i64,
    /// 签发时间（unix秒）。仅用于审计，不参与验签。
    pub iat: i64,
}

/// session 的签发与验签。**无状态**——不落库。
pub struct SessionStore {
    key: [u8; 32],
    ttl_secs: i64,
}

impl SessionStore {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            ttl_secs: SESSION_TTL_SECS,
        }
    }

    /// 覆盖有效期（测试用）。
    pub fn with_ttl(key: [u8; 32], ttl_secs: i64) -> Self {
        Self { key, ttl_secs }
    }

    /// 签发一个 session，返回 cookie 值。
    ///
    /// 格式：`<payload_b64url>.<sig_b64url>`，payload 是 `sub|exp|iat`。
    pub fn issue(&self, subject: &str) -> String {
        let now = now_unix();
        let exp = now + self.ttl_secs;
        let payload = format!("{subject}|{exp}|{now}");
        let sig = self.sign(payload.as_bytes());
        format!("{}.{}", b64url(payload.as_bytes()), b64url(&sig))
    }

    /// 验签并解出 session。
    ///
    /// 失败原因细分：格式错 / 签名不符 / 已过期。
    ///
    /// **先解析结构、后验签**：格式错的 cookie 报 `Malformed` 而不是
    /// `BadSignature`——前者是「这东西根本不是 session」，后者是
    /// 「它伪装成 session 但签不对」，排查时是两条完全不同的路径。
    pub fn verify(&self, cookie: &str) -> Result<Session, SessionError> {
        let (p_b64, s_b64) = cookie.split_once('.').ok_or(SessionError::Malformed)?;

        // 1) 结构：payload 必须能解码成 `sub|exp|iat`
        let payload = b64url_decode(p_b64).map_err(|_| SessionError::Malformed)?;
        let sig = b64url_decode(s_b64).map_err(|_| SessionError::Malformed)?;
        let text = String::from_utf8(payload).map_err(|_| SessionError::Malformed)?;
        let mut parts = text.split('|');
        let subject = parts.next().ok_or(SessionError::Malformed)?.to_string();
        let exp: i64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or(SessionError::Malformed)?;
        let iat: i64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or(SessionError::Malformed)?;
        if parts.next().is_some() || subject.is_empty() {
            return Err(SessionError::Malformed);
        }

        // 2) 验签
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|e| SessionError::BadKey(e.to_string()))?;
        mac.update(text.as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| SessionError::BadSignature)?;

        // 3) 过期
        if now_unix() > exp + CLOCK_SKEW_TOLERANCE_SECS {
            return Err(SessionError::Expired);
        }
        Ok(Session { subject, exp, iat })
    }
}

/// 签名（原始字节）。
///
/// `key` 恒为 `[u8; 32]`，而 HMAC 接受任意长度 key——所以这里
/// **结构上不可能失败**。用 `new_from_slice` 的 `Result` 再 `unwrap` 只会
/// 制造一个永不触达的 panic 分支；HMAC-SHA256 的 block size 是 64，
/// 32 字节远小于上限，直接走无返回错误的内核初始化路径。
fn sign_bytes(key: &[u8; 32], msg: &[u8]) -> Vec<u8> {
    // hmac 0.12 起 `new_from_slice` 对任意长度都成功；这里不做冗余校验。
    let mut mac = HmacSha256::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC accepts any key length"));
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

impl SessionStore {
    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        sign_bytes(&self.key, msg)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// base64url 无填充编码。
///
/// 直接复用 [`identity::base64url`]：这里曾经有一份逐字重复的
/// 实现，两份代码会各自漂移，且都带裸索引。留一层转发只为让本模块的
/// 调用点保持简短。
pub fn b64url(bytes: &[u8]) -> String {
    identity::base64url(bytes)
}

/// base64url 无填充解码。
pub fn b64url_decode(s: &str) -> Result<Vec<u8>, SessionError> {
    fn val(c: u8) -> Result<u32, SessionError> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err(SessionError::Malformed),
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for chunk in b.chunks(4) {
        let n = chunk.iter().enumerate().try_fold(0u32, |acc, (i, &c)| {
            let v = val(c)?;
            Ok::<_, SessionError>(acc | (v << (18 - 6 * i)))
        })?;
        // 尾块只有 1 个字符时，6 bit 凑不出任何完整字节 → 拒绝。
        // （编码端从不产出这种输入，但解码端必须拒：否则攻击者可构造
        //   `AAAA` 之外的畸形 cookie 试探解析路径。）
        if chunk.len() < 2 {
            return Err(SessionError::Malformed);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
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

    fn store() -> SessionStore {
        SessionStore::new([7u8; 32])
    }

    #[test]
    fn issue_and_verify_roundtrip() {
        let s = store();
        let cookie = s.issue("admin");
        let sess = s.verify(&cookie).unwrap();
        assert_eq!(sess.subject, "admin");
        assert!(sess.exp > sess.iat);
    }

    #[test]
    fn tampered_payload_fails_signature() {
        let s = store();
        let cookie = s.issue("admin");
        // 改 subject：admin → root
        let forged_payload = b64url(b"root|9999999999|1");
        let forged = format!("{forged_payload}.{}", cookie.split_once('.').unwrap().1);
        assert!(matches!(s.verify(&forged), Err(SessionError::BadSignature)));
    }

    #[test]
    fn tampered_expiry_fails_signature() {
        let s = store();
        let cookie = s.issue("admin");
        let (p, sig) = cookie.split_once('.').unwrap();
        let payload = b64url_decode(p).unwrap();
        let text = String::from_utf8(payload).unwrap();
        let mut it = text.split('|');
        let _sub = it.next().unwrap();
        let _exp = it.next().unwrap();
        let iat = it.next().unwrap();
        // 把 exp 推到 100 年后
        let far = b64url(format!("admin|99999999999|{iat}").as_bytes());
        let forged = format!("{far}.{sig}");
        assert!(matches!(s.verify(&forged), Err(SessionError::BadSignature)));
    }

    #[test]
    fn wrong_key_rejects() {
        let a = SessionStore::new([1u8; 32]);
        let b = SessionStore::new([2u8; 32]);
        let cookie = a.issue("admin");
        assert!(matches!(b.verify(&cookie), Err(SessionError::BadSignature)));
    }

    #[test]
    fn expired_session_is_rejected() {
        // TTL = -120s：iat 在过去，exp 已过且超出容差
        let s = SessionStore::with_ttl([7u8; 32], -120);
        let cookie = s.issue("admin");
        assert!(matches!(s.verify(&cookie), Err(SessionError::Expired)));
    }

    #[test]
    fn expiry_within_clock_skew_is_accepted() {
        // 刚过期 30s，在 60s 容差内 → 仍有效（容忍时钟偏差）
        let s = SessionStore::with_ttl([7u8; 32], -30);
        let cookie = s.issue("admin");
        assert!(s.verify(&cookie).is_ok());
    }

    #[test]
    fn malformed_cookie_is_rejected() {
        let s = store();
        for bad in [
            "",
            "no-dot",
            "onlyone.",
            ".onlysig",
            "aaa.bbb",
            "AAAA.AAAA",
            // payload 里有非法 base64url 字符：走字符表 miss 分支，
            // 与「合法字符但长度不对」是两条不同的拒绝路径
            "!!!.AAA",
            "admin|1|1.AAA",
        ] {
            assert!(
                matches!(s.verify(bad), Err(SessionError::Malformed)),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn extra_payload_field_is_rejected() {
        // `sub|exp|iat|extra`：多一段 → 拒绝。
        // 不拒的话，将来给 payload 加字段时会静默改变解析语义。
        let s = store();
        let forged = b64url(b"admin|9999999999|1|extra");
        let cookie = format!("{forged}.{}", b64url(&[0u8; 64]));
        assert!(matches!(s.verify(&cookie), Err(SessionError::Malformed)));
    }

    #[test]
    fn empty_subject_is_rejected() {
        // `|exp|iat`：subject 为空 → 拒绝。
        // 空 subject 意味着「没有主体的会话」，放过去等于匿名访问。
        let s = store();
        let forged = b64url(b"|9999999999|1");
        let cookie = format!("{forged}.{}", b64url(&[0u8; 64]));
        assert!(matches!(s.verify(&cookie), Err(SessionError::Malformed)));
    }

    #[test]
    fn non_integer_exp_is_malformed() {
        let s = store();
        let forged = b64url(b"admin|not-a-number|1");
        let cookie = format!("{forged}.{}", b64url(&[0u8; 64]));
        assert!(matches!(s.verify(&cookie), Err(SessionError::Malformed)));
    }

    #[test]
    fn b64url_decode_rejects_bad_charset_and_short_tail() {
        // 非法字符（不在 base64url 字符表里）
        assert!(b64url_decode("a!b_").is_err());
        assert!(b64url_decode("a b c").is_err());
        // 尾块 1 字符：6 bit 凑不出任何完整字节 → 拒绝（不是静默推 1 字节）
        assert!(b64url_decode("a").is_err());
        assert!(b64url_decode("AAAAa").is_err());
        // 2 / 3 字符尾块合法
        assert_eq!(b64url_decode("AA").unwrap(), vec![0u8]);
        assert_eq!(b64url_decode("AAA").unwrap(), vec![0u8, 0]);
    }

    #[test]
    fn payload_is_readable_but_not_tamperable() {
        // 签名 cookie：payload 明文可读（便于审计），但改动即失败
        let s = store();
        let cookie = s.issue("admin");
        let p = cookie.split_once('.').unwrap().0;
        let raw = String::from_utf8(b64url_decode(p).unwrap()).unwrap();
        assert!(
            raw.starts_with("admin|"),
            "payload should be readable: {raw}"
        );
    }

    #[test]
    fn b64url_roundtrip() {
        for len in [0usize, 1, 2, 3, 4, 5, 31, 32, 33, 64] {
            let b: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(b64url_decode(&b64url(&b)).unwrap(), b, "len {len}");
        }
    }

    #[test]
    fn ttl_default_is_30_days() {
        assert_eq!(SESSION_TTL_SECS, 30 * 24 * 60 * 60);
    }
}
