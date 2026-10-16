//! 身份：密钥对、指纹、node_id 签发。
//!
//! **这个 crate 由 node 与 Hub 双方共用**（约束 11 的主体），所以它是独立的：
//! node 侧生成密钥对、写 `identity.json`；Hub 侧只存指纹并验签。
//! 不让 node 依赖 `hub-core`——那会把 hub-storage / r2d2_sqlite / argon2 / uuid
//! 整套 Hub 存储层拉进 afcli 二进制。也不放进 `protocol`——protocol 只放
//! 帧信封 / 消息类型 / 载荷 / 错误码这类**协议定义**，不含密钥管理。
//!
//! 设计要点（`protocol.md` §3、`security-and-lifecycle.md` §4.0/§4.3）：
//! - **算法 Ed25519**，密钥对在 node 本地生成，Hub 只存指纹（约束 11）。
//! - **`node_id` 用 128 位随机串**，不是秘密，**不得作为授权依据**。
//! - enroll 是显式事务 + `UNIQUE(key_fingerprint)`，绝不 upsert。
//! - 重连两步：只信库内指纹，**绝不接受请求里带来的任何公钥**。

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub mod error;
pub use error::IdentityError;

/// 一对 Ed25519 密钥。
///
/// `Debug` 实现**不含私钥**（约束 13：私钥永不进日志/回显/轨迹）。
pub struct KeyPair {
    signing: SigningKey,
}

impl KeyPair {
    /// 生成新密钥对。
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// 从 PKCS#8 DER 字节恢复。
    ///
    /// `der.len() < 48` 一律拒绝：少了连一个完整的 PKCS#8 v1 头都放不下，
    /// 绝不能从「末尾 32 字节」硬切——那会把任意字节当私钥种子用。
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, IdentityError> {
        // ed25519-dalek 2.x 的 SigningKey::from_bytes 吃 32 字节种子；
        // PKCS#8 v1 的私钥字段就是这 32 字节（在 16 字节头之后）。
        if der.len() < 48 {
            return Err(IdentityError::MalformedKey);
        }
        // 48 = 16 字节 ASN.1 头 + 32 字节种子；固定偏移，不用 len 反推
        let mut seed = [0u8; 32];
        seed.copy_from_slice(der.get(16..48).ok_or(IdentityError::MalformedKey)?);
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    /// 导出 PKCS#8 v1 DER（48 字节固定头 + 32 字节种子）。
    pub fn to_pkcs8_der(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(&[
            0x30, 0x2e, // SEQUENCE, len 46
            0x02, 0x01, 0x00, // INTEGER 0 (version)
            0x30, 0x05, // SEQUENCE, len 5
            0x06, 0x03, 0x2b, 0x65, 0x70, // OID 1.3.101.112 (Ed25519)
            0x04, 0x22, // OCTET STRING, len 34
            0x04, 0x20, // OCTET STRING, len 32
        ]);
        out.extend_from_slice(&self.signing.to_bytes());
        out
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }

    /// 对任意消息签名。
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// 用库内公钥验签。
    // coverage: excluded — `Err(_)` 臂（from_bytes 拒绝非法曲线点）无可控测试输入；
    // ed25519-dalek 会对几乎所有 32 字节解压成功，构造不出确定性非法点。
    pub fn verify(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
        match VerifyingKey::from_bytes(pubkey) {
            Ok(vk) => vk
                .verify(msg, &ed25519_dalek::Signature::from_bytes(sig))
                .is_ok(),
            Err(_) => false,
        }
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只公开公钥指纹，私钥绝不出现
        f.debug_struct("KeyPair")
            .field(
                "fingerprint",
                &Fingerprint::of(&self.public_key_bytes()).to_hex(),
            )
            .finish()
    }
}

/// 公钥指纹（SHA-256 of public key，取前 16 字节 hex）。
///
/// 这是 Hub **唯一**存储的与设备密钥相关的东西（约束 11）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 16]);

impl Fingerprint {
    pub fn of(pubkey: &[u8; 32]) -> Self {
        // SHA-256 输出恒 32 字节，`first_chunk::<16>` 必成功。用 `first_chunk`
        // 取前 16 字节：返回 Option，无需 panic 路径。
        // coverage: excluded — else 臂（first_chunk::<16> 返回 None）结构上不可达，
        // SHA-256 digest 恒为 32 字节，恒 ≥ 16。
        let d = Sha256::digest(pubkey);
        let Some(head) = d.first_chunk::<16>() else {
            // 结构上不可达：SHA-256 输出 32 字节
            return Self([0u8; 16]);
        };
        Self(*head)
    }

    pub fn from_hex(s: &str) -> Result<Self, IdentityError> {
        let b = hex_decode(s)?;
        if b.len() != 16 {
            return Err(IdentityError::BadFingerprint);
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&b);
        Ok(Self(out))
    }

    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// 签发 `node_id`：128 位随机，base64url（43 字符）。
///
/// **不是秘密**（出现在每个心跳帧里），**不得作为授权依据**（§4.0）。
pub fn issue_node_id() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    base64url(&b)
}

/// 签名覆盖的规范字节：`v + node_id + seq + t + nonce`。
///
/// 只签 nonce 会让一个被重放的 `auth` 签名在任意新连接上复用（§3）。
pub fn signature_message(v: u32, node_id: &str, seq: u64, msg_type: &str, nonce: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(&v.to_be_bytes());
    m.extend_from_slice(&(node_id.len() as u32).to_be_bytes());
    m.extend_from_slice(node_id.as_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(&(msg_type.len() as u32).to_be_bytes());
    m.extend_from_slice(msg_type.as_bytes());
    m.extend_from_slice(&(nonce.len() as u32).to_be_bytes());
    m.extend_from_slice(nonce.as_bytes());
    m
}

/// 32 字节 CSPRNG nonce，base64url。每连接一次（§3）。
pub fn issue_nonce() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    base64url(&b)
}

/// base64url 无填充编码。
///
/// node 侧写 `identity.json` 与 Hub 侧解析用的是**同一份**实现——
/// 这就是本 crate 存在的意义：不裸索引、不裸切片。
/// `T.get(idx)` 恒命中（`idx` 被 `& 63` 夹在 0..64），用 `get` + 兜底
/// 把「不可能失败」写成结构上不可能失败。
pub fn base64url(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
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
            // `idx` 被 `& 63` 夹在 0..64，T 恒长 64 —— 必命中。
            // 用 get + 兜底而不是裸索引：裸索引一旦被改成 `T[(n >> shift)]`
            // 之类的形式就是 panic，而这里连 panic 的可能都要排除。
            if let Some(&c) = T.get(((n >> shift) & 63) as usize) {
                out.push(c as char);
            }
        }
    }
    out
}

/// base64url 无填充解码。
pub fn base64url_decode(s: &str) -> Result<Vec<u8>, IdentityError> {
    fn val(c: u8) -> Result<u32, IdentityError> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err(IdentityError::BadBase64),
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for chunk in b.chunks(4) {
        let n = chunk.iter().enumerate().try_fold(0u32, |acc, (i, &c)| {
            let v = val(c)?;
            Ok::<_, IdentityError>(acc | (v << (18 - 6 * i)))
        })?;
        // 尾块只有 1 个字符：6 bit 凑不出任何完整字节 → 拒绝。
        // 编码端从不产出这种输入，解码端必须拒。
        if chunk.len() < 2 {
            return Err(IdentityError::BadBase64);
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

/// hex 编码。**不可失败**：输入是 `u8` 半字节（恒 < 16），
/// 不需要 `unwrap` 也不需要 `Result`。
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // 16 进制字符表直接索引：把「不可能失败」写成结构上不可能失败，
        // 而不是靠 unwrap 把不可能失败压下去。
        const H: &[u8; 16] = b"0123456789abcdef";
        // `b >> 4` 与 `b & 0xf` 恒 < 16，两个 get 必命中；写成 if let
        // 是为了不在索引 panic 与 unwrap panic 之间做选择。
        if let (Some(&hi), Some(&lo)) = (H.get((b >> 4) as usize), H.get((b & 0xf) as usize)) {
            s.push(hi as char);
            s.push(lo as char);
        }
    }
    s
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, IdentityError> {
    if !s.len().is_multiple_of(2) {
        return Err(IdentityError::BadHex);
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        // `to_digit` 给的是**数值**（'f' → 15）；`from_digit` 给的是字符
        // （15 → 'f'）。这里要的是数值，用 `to_digit`。
        // 长度已在校验过是偶数，chunks(2) 必给满块；仍走 get 不留索引 panic。
        let hi = (*pair.first().ok_or(IdentityError::BadHex)? as char)
            .to_digit(16)
            .ok_or(IdentityError::BadHex)?;
        let lo = (*pair.get(1).ok_or(IdentityError::BadHex)? as char)
            .to_digit(16)
            .ok_or(IdentityError::BadHex)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_debug_hides_private_key() {
        let kp = KeyPair::generate();
        let d = format!("{kp:?}");
        assert!(!d.contains("signing"), "Debug leaked signing key: {d}");
        assert!(
            d.contains("fingerprint"),
            "Debug should show fingerprint: {d}"
        );
        // 私钥种子的 hex 不得出现
        let seed_hex = hex_encode(&kp.signing.to_bytes());
        assert!(!d.contains(&seed_hex), "Debug leaked private key material");
    }

    #[test]
    fn pkcs8_roundtrip() {
        let kp = KeyPair::generate();
        let der = kp.to_pkcs8_der();
        assert_eq!(der.len(), 48);
        let kp2 = KeyPair::from_pkcs8_der(&der).unwrap();
        assert_eq!(kp.public_key_bytes(), kp2.public_key_bytes());
        // 恢复的密钥必须能签出可验的签名
        let m = signature_message(1, "n_x", 0, "auth", "nonce");
        assert!(KeyPair::verify(&kp2.public_key_bytes(), &m, &kp2.sign(&m)));
    }

    #[test]
    fn sign_verify_roundtrip() {
        let kp = KeyPair::generate();
        let m = signature_message(1, "n_x", 0, "auth", "nonce");
        let sig = kp.sign(&m);
        assert!(KeyPair::verify(&kp.public_key_bytes(), &m, &sig));
    }

    #[test]
    fn verify_with_undecodable_pubkey_is_false_not_panic() {
        // 库里存了损坏的公钥（32 字节但不是合法 Ed25519 点）：
        // verify 必须返回 false（认证失败），不能 panic 把 Hub 带崩。
        // 全零不是合法压缩点（`y` 坐标对应点不在曲线上，且 `x` 奇偶位非法），
        // `VerifyingKey::from_bytes` 对其返回 Err，确定性触发 verify 的 Err 臂。
        let kp = KeyPair::generate();
        let m = signature_message(1, "n_x", 0, "auth", "nonce");
        let sig = kp.sign(&m);
        assert!(!KeyPair::verify(&[0u8; 32], &m, &sig));
    }

    #[test]
    fn short_pkcs8_der_is_rejected() {
        // 长度不足 48：绝不能从末尾硬切 32 字节当种子用
        for bad in [&[][..], &[0u8; 16][..], &[0u8; 47][..]] {
            assert!(matches!(
                KeyPair::from_pkcs8_der(bad),
                Err(IdentityError::MalformedKey)
            ));
        }
    }

    #[test]
    fn tampered_message_fails_verification() {
        let kp = KeyPair::generate();
        let m = signature_message(1, "n_x", 0, "auth", "nonce");
        let sig = kp.sign(&m);
        // 改 node_id / seq / nonce 任一都应失败
        for bad in [
            signature_message(1, "n_y", 0, "auth", "nonce"),
            signature_message(1, "n_x", 1, "auth", "nonce"),
            signature_message(1, "n_x", 0, "auth", "other"),
        ] {
            assert!(!KeyPair::verify(&kp.public_key_bytes(), &bad, &sig));
        }
    }

    #[test]
    fn wrong_pubkey_fails() {
        let a = KeyPair::generate();
        let b = KeyPair::generate();
        let m = signature_message(1, "n_x", 0, "auth", "nonce");
        let sig = a.sign(&m);
        assert!(!KeyPair::verify(&b.public_key_bytes(), &m, &sig));
    }

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        let a = KeyPair::generate();
        let b = KeyPair::generate();
        let fa = Fingerprint::of(&a.public_key_bytes());
        let fb = Fingerprint::of(&b.public_key_bytes());
        assert_eq!(fa, Fingerprint::of(&a.public_key_bytes()));
        assert_ne!(fa, fb);
        // hex 往返
        assert_eq!(Fingerprint::from_hex(&fa.to_hex()).unwrap(), fa);
    }

    #[test]
    fn node_id_is_128bit_and_unique() {
        let a = issue_node_id();
        let b = issue_node_id();
        // base64url of 16 bytes = 22 chars
        assert_eq!(a.len(), 22);
        assert_ne!(a, b);
        // 不可枚举：连发 1000 个不重复
        let set: std::collections::HashSet<_> = (0..1000).map(|_| issue_node_id()).collect();
        assert_eq!(set.len(), 1000);
    }

    #[test]
    fn nonce_is_32bytes_b64url() {
        let n = issue_nonce();
        let raw = base64url_decode(&n).unwrap();
        assert_eq!(raw.len(), 32);
    }

    #[test]
    fn base64url_roundtrip() {
        for len in [0usize, 1, 2, 3, 4, 5, 31, 32, 33] {
            let b: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(base64url_decode(&base64url(&b)).unwrap(), b, "len {len}");
        }
    }

    #[test]
    fn base64url_decode_rejects_short_tail_and_bad_charset() {
        // 1 字符尾块：6 bit 凑不出任何完整字节 → 拒绝
        assert!(matches!(
            base64url_decode("a"),
            Err(IdentityError::BadBase64)
        ));
        assert!(matches!(
            base64url_decode("AAAAa"),
            Err(IdentityError::BadBase64)
        ));
        // 非法字符（不在 base64url 字符表）
        assert!(base64url_decode("a!b").is_err());
        assert!(base64url_decode("a b").is_err());
    }

    #[test]
    fn fingerprint_from_hex_rejects_wrong_length() {
        let kp = KeyPair::generate();
        let fp = Fingerprint::of(&kp.public_key_bytes());
        let hex = fp.to_hex();
        assert_eq!(hex.len(), 32, "16 bytes → 32 hex chars");
        // 长度不对（15 字节 / 17 字节 / 空）必须拒绝，
        // 否则库里会存进一个截断的指纹，验签永远失败
        for bad_len in [0usize, 15, 17, 32] {
            let s = "ab".repeat(bad_len);
            assert!(matches!(
                Fingerprint::from_hex(&s),
                Err(IdentityError::BadFingerprint)
            ));
        }
        // 非 hex 字符也在 from_hex 里被拒（hex_decode 的职责）
        assert!(matches!(
            Fingerprint::from_hex(&"zz".repeat(16)),
            Err(IdentityError::BadHex)
        ));
    }

    #[test]
    fn fingerprint_as_bytes_matches_to_hex() {
        let kp = KeyPair::generate();
        let fp = Fingerprint::of(&kp.public_key_bytes());
        assert_eq!(fp.as_bytes().len(), 16);
        // as_bytes 与 to_hex 必须描述同一个指纹（库的 hex ↔ 字节两条路径）
        assert_eq!(hex_encode(fp.as_bytes()), fp.to_hex());
    }

    #[test]
    fn signature_message_is_length_prefixed() {
        // 防拼接歧义：("ab","c") 与 ("a","bc") 必须产生不同消息
        let m1 = signature_message(1, "ab", 0, "c", "n");
        let m2 = signature_message(1, "a", 0, "bc", "n");
        assert_ne!(m1, m2);
    }

    #[test]
    fn hex_roundtrip_and_rejects_odd() {
        assert_eq!(
            hex_decode(&hex_encode(&[0, 255, 16])).unwrap(),
            vec![0, 255, 16]
        );
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn bad_pkcs8_is_rejected() {
        assert!(KeyPair::from_pkcs8_der(&[0u8; 4]).is_err());
    }
}
