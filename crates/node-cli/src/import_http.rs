//! 设备签名 HTTP 导入客户端（v0.7.4）。
//!
//! 导入是设备主动发起的**一次性** POST，不走 WS：复用 `connect_once` 会与
//! 正在运行的 worker 形成第二条设备连接并被代次机制顶替（`conn.rs`），
//! 把导入塞进 `heartbeat` / `channel_data` 又会污染数据面。所以这里是
//! 一个短请求：签名 → 发送 → 读响应 → 退出，不常驻、不写身份文件。
//!
//! # 摘要与签名的边界
//!
//! 请求体里既有 `signature` 也有 `body_sha256`，而两者又都在 body 里，
//! 因此**不可能**对「最终发出的完整 body 字节」做自引用摘要。这里采用
//! 明确的规范：先把这两个字段置为空串，序列化出的字节就是被摘要的
//! **canonical body**；`body_sha256` 是它的 SHA-256，`signature` 再对
//! [`identity::import_signing_message`] 的规范串签名。Hub 侧必须用同一
//! DTO（字段顺序一致）把这两个字段清空后重算摘要，再用库内公钥验签。
//!
//! body **不含公钥、私钥、cookie**：Hub 只按 `X-AF-Node-Id` 查库内公钥。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use identity::{base64url, import_signing_message, KeyPair};
use node_core::import::ImportCandidate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 导入端点的固定路径。**不在 `/api/*` 下**：那是浏览器 session 认证的地盘。
pub const IMPORT_PATH: &str = "/device-api/session-imports";

/// 签名覆盖的 HTTP method。
pub const IMPORT_METHOD: &str = "POST";

/// 签名 / body 的协议版本。不兼容变更才 +1。
pub const IMPORT_VERSION: u32 = 1;

pub const NODE_ID_HEADER: &str = "X-AF-Node-Id";
pub const NONCE_HEADER: &str = "X-AF-Nonce";
pub const SIGNATURE_HEADER: &str = "X-AF-Signature";
pub const BODY_SHA256_HEADER: &str = "X-AF-Body-SHA256";

/// 一次性请求不该等太久：Hub 不在就尽快失败，用户还能看见原因。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求体。字段顺序参与 canonical body 的字节，改动顺序会改变摘要——
/// Hub 侧的 DTO 必须保持同一顺序。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRequest {
    pub node_id: String,
    pub provider: String,
    pub external_session_id: String,
    pub acp_session_id: String,
    pub cwd: String,
    pub title: String,
    pub v: u32,
    pub t: u64,
    pub nonce: String,
    pub signature: String,
    pub body_sha256: String,
}

/// 已签名的请求：最终 body 字节与要放进 header 的元数据。
#[derive(Debug, Clone)]
pub struct SignedImport {
    pub body: Vec<u8>,
    pub node_id: String,
    pub nonce: String,
    pub signature: String,
    pub body_sha256: String,
}

/// Hub 的成功响应：**两个字段都是契约**。`acp_session_id` 是 Hub 的回显
/// 确认（绑定的是哪条历史），CLI 必须校验它非空——Hub 单方面少回一个字段
/// 不能被静默忽略。
#[derive(Debug, Deserialize)]
pub struct ImportResponse {
    pub session_id: String,
    pub acp_session_id: String,
}

/// 解析并校验成功响应体；形状不对或任一字段为空都是 `BadResponse`。
fn parse_response(bytes: &[u8]) -> Result<ImportOutcome, ImportHttpError> {
    let parsed: ImportResponse =
        serde_json::from_slice(bytes).map_err(|_| ImportHttpError::BadResponse)?;
    if parsed.session_id.trim().is_empty() || parsed.acp_session_id.trim().is_empty() {
        return Err(ImportHttpError::BadResponse);
    }
    Ok(ImportOutcome {
        session_id: parsed.session_id,
    })
}

/// Hub 的固定错误体：`{"error":"<code>"}`。
#[derive(Debug, Deserialize)]
struct ImportErrorBody {
    error: String,
}

/// 导入 HTTP 客户端的分类失败。
#[derive(Debug)]
pub enum ImportHttpError {
    /// 连接 / 发送 / 读响应失败。detail 是 reqwest 的错误描述（不含请求体）。
    Network(String),
    /// 响应不是约定的 JSON 形状，或缺少 `session_id` / `acp_session_id`。
    BadResponse,
    /// Hub 回了固定 code 的错误体。
    Server { code: String },
    /// 本地请求构造失败（序列化 / 运行时创建）。
    Local(String),
}

/// 一次成功导入的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub session_id: String,
}

/// 构造并签名请求体。
///
/// `nonce` 每次调用重新生成，`t` 是当前 Unix 秒；两者都进入 canonical body
/// 与签名规范串，Hub 只接受 ±300 s 窗口。
pub fn build_signed_body(
    key: &KeyPair,
    node_id: &str,
    provider: &str,
    candidate: &ImportCandidate,
    title: &str,
) -> Result<SignedImport, ImportHttpError> {
    let nonce = identity::issue_nonce();
    let t = unix_seconds();
    let mut request = ImportRequest {
        node_id: node_id.to_string(),
        provider: provider.to_string(),
        // opencode 首版 external id 与 ACP id 相等；provider 契约保证
        // `acp_session_id` 就是原样的外部会话 id。
        external_session_id: candidate.acp_session_id.clone(),
        acp_session_id: candidate.acp_session_id.clone(),
        cwd: candidate.cwd.clone(),
        title: title.to_string(),
        v: IMPORT_VERSION,
        t,
        nonce: nonce.clone(),
        signature: String::new(),
        body_sha256: String::new(),
    };

    let canonical = serde_json::to_vec(&request)
        .map_err(|e| ImportHttpError::Local(format!("cannot encode request body: {e}")))?;
    let digest = Sha256::digest(&canonical);
    let body_sha256 = identity::hex_encode(digest.as_ref());

    let message = import_signing_message(
        IMPORT_VERSION,
        node_id,
        IMPORT_METHOD,
        IMPORT_PATH,
        &body_sha256,
        &nonce,
        t,
    );
    let signature = base64url(&key.sign(&message));

    request.body_sha256 = body_sha256.clone();
    request.signature = signature.clone();
    let body = serde_json::to_vec(&request)
        .map_err(|e| ImportHttpError::Local(format!("cannot encode signed body: {e}")))?;

    Ok(SignedImport {
        body,
        node_id: node_id.to_string(),
        nonce,
        signature,
        body_sha256,
    })
}

/// 把已签名的请求 POST 到 `{origin}/device-api/session-imports`。
///
/// 关闭重定向（3xx 当失败）、只接受 JSON、超时 10 s。错误响应体只解析
/// `{"error": code}`，**不把原始 HTML / 文本带回给调用方**。
pub fn post(origin: &str, signed: &SignedImport) -> Result<ImportOutcome, ImportHttpError> {
    let url = format!("{}{IMPORT_PATH}", origin.trim_end_matches('/'));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ImportHttpError::Local(format!("cannot start HTTP runtime: {e}")))?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ImportHttpError::Network(e.to_string()))?;
        let response = client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(NODE_ID_HEADER, &signed.node_id)
            .header(NONCE_HEADER, &signed.nonce)
            .header(SIGNATURE_HEADER, &signed.signature)
            .header(BODY_SHA256_HEADER, &signed.body_sha256)
            .body(signed.body.clone())
            .send()
            .await
            .map_err(|e| ImportHttpError::Network(e.to_string()))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ImportHttpError::Network(e.to_string()))?;

        if status.is_success() {
            return parse_response(&bytes);
        }

        let parsed: ImportErrorBody =
            serde_json::from_slice(&bytes).map_err(|_| ImportHttpError::BadResponse)?;
        Err(ImportHttpError::Server { code: parsed.error })
    })
}

/// 当前 Unix 秒。时钟早于 epoch 时返回 0（Hub 会按时间窗拒绝，不会当成有效）。
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> ImportCandidate {
        ImportCandidate {
            provider: "opencode",
            acp_session_id: "ses_abc".to_string(),
            cwd: "/tmp/work".to_string(),
        }
    }

    /// body 里绝不能出现公钥或私钥材料：Hub 用 `X-AF-Node-Id` 查库内公钥，
    /// 请求里带公钥等于给「伪造 node_id」留门（约束 11 / 15）。
    #[test]
    fn the_body_carries_no_public_or_private_key_material() {
        let kp = KeyPair::generate();
        let signed =
            build_signed_body(&kp, "n_test", "opencode", &candidate(), "demo").expect("sign");
        let text = String::from_utf8(signed.body.clone()).expect("utf8");
        let public = base64url(&kp.public_key_bytes());
        let private = base64url(&kp.to_pkcs8_der());
        assert!(!text.contains(&public), "body leaked the public key");
        assert!(!text.contains(&private), "body leaked the private key");
        assert!(
            !text.contains("public_key"),
            "body should not name key fields"
        );
        assert!(
            !text.contains("private_key"),
            "body should not name key fields"
        );
    }

    /// 摘要覆盖 canonical body（两个 auth 字段清空），而不是最终 body：
    /// 后者含摘要本身，自引用不可能成立。Hub 必须按同一规则重算。
    #[test]
    fn the_body_digest_covers_the_payload_with_blank_auth_fields() {
        let kp = KeyPair::generate();
        let signed =
            build_signed_body(&kp, "n_test", "opencode", &candidate(), "demo").expect("sign");
        let mut parsed: ImportRequest = serde_json::from_slice(&signed.body).expect("request json");
        assert_eq!(parsed.body_sha256, signed.body_sha256);
        assert_eq!(parsed.signature, signed.signature);
        parsed.body_sha256.clear();
        parsed.signature.clear();
        let canonical = serde_json::to_vec(&parsed).expect("canonical");
        let expected = identity::hex_encode(Sha256::digest(&canonical).as_ref());
        assert_eq!(
            signed.body_sha256, expected,
            "digest must be over the payload with auth fields blanked"
        );
        assert_eq!(signed.body_sha256.len(), 64, "hex sha256");
    }

    /// 改 canonical body 的任一字段都会改摘要；签名再改一个字节就验不过。
    #[test]
    fn changing_the_payload_changes_the_digest_and_the_signature_fails() {
        let kp = KeyPair::generate();
        let signed =
            build_signed_body(&kp, "n_test", "opencode", &candidate(), "demo").expect("sign");
        let parsed: ImportRequest = serde_json::from_slice(&signed.body).expect("request json");
        let message = import_signing_message(
            parsed.v,
            &parsed.node_id,
            IMPORT_METHOD,
            IMPORT_PATH,
            &parsed.body_sha256,
            &parsed.nonce,
            parsed.t,
        );
        let sig: [u8; 64] = identity::base64url_decode(&parsed.signature)
            .expect("signature b64")
            .try_into()
            .expect("64-byte signature");
        assert!(KeyPair::verify(&kp.public_key_bytes(), &message, &sig));

        let mut changed = candidate();
        changed.cwd = "/tmp/other".to_string();
        let other = build_signed_body(&kp, "n_test", "opencode", &changed, "demo").expect("sign");
        let other: ImportRequest = serde_json::from_slice(&other.body).expect("json");
        assert_ne!(parsed.body_sha256, other.body_sha256);
        // 旧签名不能盖住新 body。
        assert!(!KeyPair::verify(
            &kp.public_key_bytes(),
            &import_signing_message(
                other.v,
                &other.node_id,
                IMPORT_METHOD,
                IMPORT_PATH,
                &other.body_sha256,
                &other.nonce,
                other.t,
            ),
            &sig
        ));
    }

    /// body 必须带齐 Hub 需要的全部字段，且 opencode 首版两个会话 id 相等。
    #[test]
    fn the_body_has_the_agreed_shape() {
        let kp = KeyPair::generate();
        let signed =
            build_signed_body(&kp, "n_test", "opencode", &candidate(), "demo").expect("sign");
        let v: serde_json::Value = serde_json::from_slice(&signed.body).expect("json");
        for key in [
            "node_id",
            "provider",
            "external_session_id",
            "acp_session_id",
            "cwd",
            "title",
            "v",
            "t",
            "nonce",
            "signature",
            "body_sha256",
        ] {
            assert!(v.get(key).is_some(), "body missing {key}: {v}");
        }
        assert_eq!(v["node_id"], "n_test");
        assert_eq!(v["provider"], "opencode");
        assert_eq!(v["external_session_id"], "ses_abc");
        assert_eq!(v["acp_session_id"], "ses_abc");
        assert_eq!(v["cwd"], "/tmp/work");
        assert_eq!(v["title"], "demo");
        assert_eq!(v["v"], IMPORT_VERSION);
    }

    /// Hub 的 201 体少回 / 空回 `acp_session_id` 都必须是 `BadResponse`：
    /// 两字段回显契约在 CLI 侧有一条可执行约束。
    #[test]
    fn a_success_response_without_a_valid_acp_session_id_is_rejected() {
        for body in [
            br#"{"session_id":"s1"}"#.as_slice(),
            br#"{"session_id":"s1","acp_session_id":""}"#.as_slice(),
            br#"{"session_id":"s1","acp_session_id":"   "}"#.as_slice(),
            br#"{"session_id":"","acp_session_id":"ses_1"}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_response(body),
                Err(ImportHttpError::BadResponse)
            ));
        }
        let ok = parse_response(br#"{"session_id":"s1","acp_session_id":"ses_1"}"#)
            .expect("two-field response");
        assert_eq!(ok.session_id, "s1");
    }
}
