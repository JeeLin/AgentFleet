//! 设备签名 HTTP 导入的认证 layer（v0.7.4，方案 1：专用 `from_fn` layer）。
//!
//! # 为什么不是 `middleware_auth` 的一个分支
//!
//! 浏览器 session / CSRF 的每一处豁免都是攻击面：`AUTH_EXEMPT_PATHS` 里多一行
//! 就等于「这条路径不需要登录」，而设备端点根本没有 cookie 可验。把设备认证
//! 塞进 [`crate::middleware_auth`] 必须同时改三张表（session / CSRF / init
//! 模式），每张表都是一次静默放宽。这里改成一条独立的 layer，只挂在**一条**
//! 路由上（`POST /device-api/session-imports`），三张表原样不动。
//!
//! # 校验顺序（都是 fail-closed）
//!
//! 1. **Hub 已初始化**：未设密码时 403 `hub_not_initialized`，与
//!    [`crate::middleware_auth`] 同一措辞、同一个理由（此时不存在「用户」，
//!    401 会误导成「你没登录」）。
//! 2. **四个签名头齐全 + body 可解析成约定 DTO**：缺头 / 形状不对 → 401
//!    `device_signature_invalid`（body 解不开则是 400，与 `/api/*` 同口径）。
//! 3. **头与 body 一致**：`node_id` / `nonce` / `signature` / `body_sha256`
//!    四处都有两份（头一份、body 一份），对不上就拒——否则攻击者可以让签名
//!    覆盖 body 里的值，却让 Hub 用头里的值查设备。
//! 4. **body 摘要**：把 DTO 的两个签名字段清空后按**同一字段顺序**重新序列化，
//!    SHA-256 必须等于声明的摘要。字段顺序漂移会让这一步恒失败，见
//!    `tests/session_import.rs::the_cli_and_hub_share_one_canonical_body_digest`
//!    （它比 canonical body 的字符串字面量、摘要常量，以及从**原始字节**
//!    抽出的字段顺序）。
//! 5. **时间窗**：`|now - t| <= 300 s`。
//! 6. **设备存在且有库内公钥**：未知 node 与「没公钥」是**同一个** 401——
//!    对未认证的请求区分「设备不存在」与「签名不对」等于白送一张设备 id 字典。
//! 7. **Ed25519 验签**，规范串来自 `identity::import_signing_message`（唯一实现），
//!    第一参数取 body 的 `v` 而不是 Hub 常量，签名因此绑定声明的版本。
//! 8. **协议版本**：验签通过后才要求 `v == IMPORT_VERSION`，不匹配回 400
//!    `invalid_request_body`。顺序不能反：未验签的请求不该得知「协议版本不支持」；
//!    版本不匹配的请求也不该消费 nonce（它是请求形状错，不是一次业务尝试）。
//! 9. **nonce 一次性消费**：在这一步之后、handler 之前。顺序是安全语义——
//!    未验签的请求不能探「这个 nonce 用过没」，而业务失败也必须烧掉 nonce
//!    （否则同一个签名可以反复撞同一业务错误）。
//!
//! body 只在**这里**读一次，解析好的 DTO 塞进 request extensions，handler 消费
//! 它，不再从流里读第二次（流已经空了）。
//!
//! # 重启边界
//!
//! nonce 集合是**进程内存**的，重启即清空。所以签名还绑定 node_id、method、
//! path、body 摘要与 ±300 s 时间窗：即使重启后有人重放一条旧请求，也只有
//! 「时间窗内 + 签名有效」的那条能进来，而这条请求的效果是**建一个会话**，
//! 不暴露任何 secret。这条取舍写在 `security-and-lifecycle.md` §1.1。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use identity::import_protocol::ImportRequest;
// Hub 集成测试按 `hub_server::import_auth::IMPORT_*` 取常量：这里**重导出**
// 共享定义而不是另立一份。头名同理，`HeaderMap::get` 大小写不敏感。
pub use identity::import_protocol::{
    BODY_SHA256_HEADER, IMPORT_METHOD, IMPORT_PATH, IMPORT_VERSION, NODE_ID_HEADER, NONCE_HEADER,
    SIGNATURE_HEADER,
};

/// 测试接缝：Hub 验签用的就是这个函数，把它暴露出来让端点测试直接比字节，
/// 不另抄一份摘要算法（抄一份又成了「两侧各写一遍」）。
pub fn canonical_digest_for_test(parsed: &ImportRequest) -> Option<String> {
    canonical_digest(parsed)
}

use crate::routes::api::{Authenticated, DeviceImportAuth};
use crate::routes::session_import::ImportedBody;
use crate::{err, ApiError, AppState};

// 路径 / method / 版本 / 头名 / DTO 全部来自 `identity::import_protocol`：
// CLI 与 Hub 只准读同一份定义，改一侧就是改协议本身。
/// 时间窗：`|now - t| <= 300 s`。同时是 nonce 表的 TTL。
///
/// `pub` 只为 `tests/session_import.rs` 推边界值（它按「窗外 = 窗口 + 1」算，
/// 不写死 300）。**不是**给生产调用方的 API：本 crate 之外没有 `src/**` 读者。
pub const TIME_WINDOW_SECS: u64 = 300;

/// nonce 表的上限。超过就清掉过期项；仍超上限则整体清空。
///
/// 清空是**安全的**：nonce 的防重放保护本来就只覆盖时间窗，而「清空后同一
/// nonce 还能用」的窗口仍然被签名 + 摘要 + 时间窗限制。取 4096 是因为正常
/// 导入频率远低于此（一台设备一次导入），上界只防内存被灌爆。
const NONCE_CAP: usize = 4096;

/// 进程内存的 nonce 一次性集合（值是「第一次见到的 Unix 秒」）。
///
/// 放 `AppState` 而不是全局静态：测试要能各自一份，而全局静态会让并行用例
/// 互相消费对方 nonce。
#[derive(Default)]
pub struct ImportNonces {
    seen: Mutex<HashMap<String, u64>>,
}

impl ImportNonces {
    pub fn new() -> Self {
        Self::default()
    }

    /// 消费一个 nonce。返回 `true` = 第一次见到（放行）。
    ///
    /// 锁中毒时**按「已见过」处理**而不是放行：这里是安全判据，宁可拒绝一次
    /// 合法导入（设备重试会换 nonce），也不能因为一次 panic 放开重放。
    pub fn consume(&self, nonce: &str, now: u64) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        // 顺带做过期清理：TTL 与时间窗同值，窗外的东西无论如何都过不了时间窗。
        seen.retain(|_, at| now.saturating_sub(*at) <= TIME_WINDOW_SECS);
        if seen.len() >= NONCE_CAP {
            // 仍然超上限（短时间内灌了上万个不同 nonce）：整体清空。
            // 上界优先于「永不重复」——不清就是内存放大攻击。
            seen.clear();
        }
        match seen.get(nonce) {
            Some(_) => false,
            None => {
                seen.insert(nonce.to_string(), now);
                true
            }
        }
    }
}

/// 设备签名认证 layer。挂在**一条** `MethodRouter` 上（见 `lib.rs::build_app`）。
pub async fn require_device_signature(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    // 第 1 步：Hub 已初始化。与 `middleware_auth` 逐句同措辞——同一条规则在
    // 两个 layer 里说法不同，运维会以为它们是两件事。
    match state.is_initialized() {
        Ok(true) => {}
        Ok(false) => return ApiError::forbidden(err::HUB_NOT_INITIALIZED).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "cannot read hub init state");
            return ApiError::internal().into_response();
        }
    }

    // 先拆成头与 body：body 只读一次，之后重建请求时不再有 body 可读。
    let (mut parts, body) = req.into_parts();

    // 第 2 步：头齐全。缺任何一个都归 401：请求没提供可验证的身份。
    let Some(headers) = HeaderSnapshot::take(&parts.headers) else {
        return signature_invalid();
    };

    // body 只读一次。上限与 `/api/*` 的 `read_body` 同值（WS 帧上限）。
    let body = match axum::body::to_bytes(body, crate::WS_MAX_MESSAGE_BYTES).await {
        Ok(b) => b.to_vec(),
        Err(_) => return ApiError::bad_request(err::BAD_BODY).into_response(),
    };
    let Ok(parsed) = serde_json::from_slice::<ImportRequest>(&body) else {
        return ApiError::bad_request(err::BAD_BODY).into_response();
    };

    // 第 3 步：头与 body 的同一事实必须逐字相等。
    if parsed.node_id != headers.node_id
        || parsed.nonce != headers.nonce
        || parsed.signature != headers.signature
        || parsed.body_sha256 != headers.body_sha256
    {
        return signature_invalid();
    }

    // 第 4 步：canonical body 摘要。字段顺序参与字节，所以必须**用 DTO 重新
    // 序列化**而不是对收到的原始 body 求摘要（后者含两个签名字段，自引用）。
    let Some(expected) = canonical_digest(&parsed) else {
        return ApiError::bad_request(err::BAD_BODY).into_response();
    };
    if expected != headers.body_sha256 {
        return signature_invalid();
    }

    // 第 5 步：时间窗。`abs_diff` 让「未来」同样越窗。
    let now = unix_seconds();
    if now.abs_diff(parsed.t) > TIME_WINDOW_SECS {
        return signature_invalid();
    }

    // 第 6 步：库内公钥。**绝不接受请求里带来的公钥**（约束 11）。
    let device = match state.devices().find_by_id(&headers.node_id) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the device for the import request");
            return ApiError::internal().into_response();
        }
    };
    // 未知设备、无公钥、公钥坏掉，对外是**同一个** 401。
    let Some(public_key) = device.and_then(|d| d.public_key_bytes()) else {
        return signature_invalid();
    };

    // 第 7 步：Ed25519 验签，规范串唯一实现。第一参数取 **body 自带的 `v`**，
    // 不是 Hub 常量 `IMPORT_VERSION`：后者只用来判「当前支持哪个版本」。若拿常量
    // 顶掉被签名字节，body 声明的版本就没进规范串，签名与版本会各说各话。
    let message = identity::import_signing_message(
        parsed.v,
        &headers.node_id,
        IMPORT_METHOD,
        IMPORT_PATH,
        &headers.body_sha256,
        &headers.nonce,
        parsed.t,
    );
    let Some(signature) = decode_signature(&headers.signature) else {
        return signature_invalid();
    };
    if !identity::KeyPair::verify(&public_key, &message, &signature) {
        return signature_invalid();
    }

    // 第 8 步：协议版本。验过签名之后才判——未验签的请求不许借这条探「Hub
    // 支持哪些版本」。版本不匹配是请求形状错，在**消费 nonce 之前**拒（否则
    // 一次错版本探测会平白烧掉设备本来要用的 nonce）。这里既不建会话，也不烧 nonce。
    if parsed.v != IMPORT_VERSION {
        return ApiError::bad_request(err::BAD_BODY).into_response();
    }

    // 第 9 步：nonce 一次性。**在验签之后**：未验签的请求不能拿本表探重复，
    // 而业务失败（409 等）照样烧掉它——同一个签名不该能重试同一个动作。
    if !state.import_nonces().consume(&headers.nonce, now) {
        return ApiError::conflict(err::IMPORT_REPLAYED).into_response();
    }

    // 认证结果与解析好的 DTO 一起进扩展：handler 不再读流（已经空了），
    // 也不再解析一遍（解析口径只在这里有一份）。
    parts.extensions.insert(DeviceImportAuth {
        node_id: headers.node_id.clone(),
        // subject 只用于本次请求内的 `probe_base_rev` 开通道，不跨请求复用。
        auth: Authenticated::for_device_import(&headers.node_id),
    });
    parts.extensions.insert(ImportedBody(parsed));
    next.run(Request::from_parts(parts, axum::body::Body::empty()))
        .await
}

/// 重算 canonical body 的 SHA-256（小写 hex）。
///
/// canonical body 的字节由 [`ImportRequest::canonical_body`] 唯一定义——CLI 侧
/// `build_signed_body` 算摘要时调的是同一个函数。这里只保留一层 `Option`
/// 包装，好让「序列化失败」在调用点仍是一次 400 而不是 panic。
fn canonical_digest(parsed: &ImportRequest) -> Option<String> {
    parsed.body_digest_hex().ok()
}

/// 头的本地快照。一次取出、一次校验，后面不再回头读 `HeaderMap`。
struct HeaderSnapshot {
    node_id: String,
    nonce: String,
    signature: String,
    body_sha256: String,
}

impl HeaderSnapshot {
    fn take(headers: &HeaderMap) -> Option<Self> {
        Some(Self {
            node_id: header(headers, NODE_ID_HEADER)?,
            nonce: header(headers, NONCE_HEADER)?,
            signature: header(headers, SIGNATURE_HEADER)?,
            body_sha256: header(headers, BODY_SHA256_HEADER)?,
        })
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    let v = headers.get(name)?.to_str().ok()?.trim();
    if v.is_empty() {
        return None;
    }
    Some(v.to_string())
}

/// 64 字节 Ed25519 签名的 base64url 解码。
fn decode_signature(s: &str) -> Option<[u8; 64]> {
    identity::base64url_decode(s).ok()?.try_into().ok()
}

fn signature_invalid() -> Response {
    // 401 而不是 403：请求没有提供可验证的身份，与「有身份但无权」是两回事。
    ApiError::unauthorized(err::DEVICE_SIGNATURE_INVALID).into_response()
}

/// 当前 Unix 秒。时钟早于 epoch 时返回 0（会被时间窗拒掉，不会当成有效）。
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
