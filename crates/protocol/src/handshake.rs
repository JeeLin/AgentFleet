//! 握手状态机：`hello → challenge → auth → auth_ok / auth_fail`。
//!
//! 设计要点（`docs/architecture/protocol.md` §2–§3）：
//! - **不凭公钥放行**：一次性 challenge（32 字节 CSPRNG，base64url），
//!   `auth` 必须带上对该 nonce 的签名。
//! - **签名覆盖 `v + node_id + seq + t + nonce` 的规范编码**，不只是 nonce——
//!   只签 nonce 的话，一个被重放的 `auth` 签名能在任意新连接上复用。
//!   **规范编码与验签不在本 crate**：由 [`Authenticator`] 的实现方调用
//!   `identity::signature_message` + `KeyPair::verify` 完成，
//!   本 crate **不重复实现那份编码**（重复实现 = 两份会漂移的真相源）。
//! - **nonce 用完即弃**：取出来验签就不再还回去；验签失败直接进 `Rejected` 终态，
//!   不给第二次 challenge 的机会（challenge 每连接一次）。
//! - **enroll 与重连是两种类型**而不是运行时可选字段：重连的结构体里
//!   根本没有 `public_key`，「重连时用本次上传的公钥」在类型层面就写不出来。
//! - **本 crate 不含 IO**：超时判定是纯函数（给定状态 + 已流逝时长），
//!   计时与断连由 `hub-core` / `node-core` 做。

use std::time::Duration;

use base64ct::{Base64UrlUnpadded, Encoding};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::frame::{Frame, PROTOCOL_VERSION};
use crate::message::{types, Challenge, DeviceInfo, ErrorCode, Hello};

/// 握手超时：连接建立后 10s 未收到 `auth` 即关闭并
/// `err{code:"handshake_timeout"}`（§2、§3）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// nonce 长度：32 字节（§3）。
pub const NONCE_BYTES: usize = 32;

/// Ed25519 公钥长度（约束 11）。
pub const PUBLIC_KEY_BYTES: usize = 32;

/// Ed25519 签名长度。
pub const SIGNATURE_BYTES: usize = 64;

/// 连接握手状态（§3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeState {
    /// 连接已建立，等 `hello`。
    AwaitingHello,
    /// 已发 `challenge`，持有**尚未消费**的 nonce。
    Challenged,
    /// 验签通过，可以开始业务帧。
    Authed,
    /// 终态：握手已失败，连接待关闭。
    Rejected,
}

impl HandshakeState {
    /// 握手是否仍在进行中（既没 `Authed` 也没 `Rejected`）。
    ///
    /// 超时判定只看这个：**已 `Authed` 的连接不因为「握手慢」被关**。
    pub fn in_progress(self) -> bool {
        matches!(self, Self::AwaitingHello | Self::Challenged)
    }
}

/// 握手期内部状态。数据与状态放在一起，`nonce` 的存在性就是
/// 「还在 `Challenged`」的证明——不维护两个可能互相矛盾的变量。
#[derive(Debug)]
enum Phase {
    AwaitingHello,
    Challenged {
        /// 节点在 `hello` 里声明的 `v`——**签名覆盖的是它**（见 [`AuthAttempt`]）。
        node_v: u32,
        /// 协商结果（§2 双方各自声明、取较小者），用于 `auth_ok.protocol_version`。
        negotiated: u32,
        /// 一次性 nonce。验签时**取走即弃**。
        nonce: String,
    },
    Authed {
        negotiated: u32,
    },
    Rejected,
}

impl Phase {
    fn state(&self) -> HandshakeState {
        match self {
            Self::AwaitingHello => HandshakeState::AwaitingHello,
            Self::Challenged { .. } => HandshakeState::Challenged,
            Self::Authed { .. } => HandshakeState::Authed,
            Self::Rejected => HandshakeState::Rejected,
        }
    }
}

/// 已从状态机里取走的一次性 challenge（§3）。
///
/// 取出来之后它就不再属于这条连接了——`Phase` 里不会有第二份，
/// 所以「同一 nonce 消费两次」在结构上就不可能发生。
struct TakenChallenge {
    node_v: u32,
    negotiated: u32,
    nonce: String,
}

/// 握手错误。**非法转移一律报错，不静默接受**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    #[error("illegal handshake transition: {state:?} received {event}")]
    IllegalTransition {
        state: HandshakeState,
        event: &'static str,
    },

    #[error("invalid hello payload: {0} is empty")]
    InvalidHello(&'static str),

    #[error("invalid protocol version: {0} (protocol versions start at 1)")]
    InvalidVersion(u32),

    #[error("invalid auth payload: {0}")]
    MalformedAuth(&'static str),

    #[error("reconnect must not carry an uploaded public key (protocol.md 3)")]
    PublicKeyOnReconnect,

    #[error("bad signature")]
    BadSignature,
}

/// 版本协商（§2）：**双方各自声明 `v`，取较小者**。
///
/// `v == 0` 不合法——本协议从 1 起，`0` 只可能是没填字段的默认值。
/// 高于本端支持版本**不**在这里拒绝：§2 的规则就是取较小者，
/// 老 Hub 遇到新节点时按老版本说话，而不是把人踢下线。
pub fn negotiate_version(node_v: u32, hub_v: u32) -> Result<u32, HandshakeError> {
    if node_v == 0 {
        return Err(HandshakeError::InvalidVersion(node_v));
    }
    if hub_v == 0 {
        return Err(HandshakeError::InvalidVersion(hub_v));
    }
    Ok(node_v.min(hub_v))
}

/// 握手超时判定（§3）：**纯函数**，调用方自己计时——本 crate 不起定时器。
///
/// `state` 不在进行中（已 `Authed` / `Rejected`）时恒为 `false`：
/// 握手已经结束了，慢不慢与超时无关。
pub fn should_close_for_timeout(state: HandshakeState, elapsed: Duration) -> bool {
    state.in_progress() && elapsed >= HANDSHAKE_TIMEOUT
}

/// 验签用的公钥来源（§3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicKeySource<'a> {
    /// enroll：本次上传的公钥（尚未入库）。
    Submitted(&'a [u8; PUBLIC_KEY_BYTES]),
    /// 重连：库内指纹对应的公钥。**绝不接受本次上传的值**（§3）。
    Stored(&'a [u8; PUBLIC_KEY_BYTES]),
}

/// 一次 `auth` 验签的输入——**签名必须覆盖这五个字段的规范编码**（§3）。
///
/// `v` 取 **`hello` 帧里节点声明的那个**，不是协商结果：
/// `challenge` 只带 nonce、不带 Hub 的版本，节点签 `auth` 时还不知道协商值，
/// 只能签自己声明的 `v`；Hub 用握手期记下的同一个值验签，中途换 `v` 必然验不过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthAttempt<'a> {
    pub v: u32,
    /// enroll 时为空串（尚无身份），重连时为库内 `node_id`。
    pub node_id: &'a str,
    /// `auth` 帧自己的 `seq`（连接内从 0 起）。签名覆盖它，
    /// 所以旧连接上的 `auth` 挪到新连接（seq 归零）一定验不过。
    pub seq: u64,
    /// 恒为 `auth`。
    pub msg_type: &'static str,
    /// 本连接的一次性 nonce。
    pub nonce: &'a str,
    pub public_key: PublicKeySource<'a>,
    pub signature: &'a [u8; SIGNATURE_BYTES],
}

/// 验签能力由上层注入——本 crate **不含密码学依赖**。
///
/// 实现**必须**用 `identity::signature_message(v, node_id, seq, t, nonce)`
/// 构造待签字节、`KeyPair::verify` 验签。规范编码只有那一份，
/// 不在这里重写一份（两份编码迟早漂移，而漂移的后果是「双方都以为验过了」）。
pub trait Authenticator {
    /// 签名覆盖 [`AuthAttempt`] 的规范编码时返回 `true`。
    fn verify_auth(&self, attempt: &AuthAttempt<'_>) -> bool;
}

/// `auth` 载荷：按**有无 `node_id`** 分两形态（§3）。
///
/// 不派生 `PartialEq`：`DeviceInfo`（在 `message.rs` 里，本子任务不改那个文件）
/// 没派生它，而顺手给 `message.rs` 补派生属于越界改动。
#[derive(Debug, Clone)]
pub enum AuthPayload {
    Enroll(EnrollAuth),
    Reconnect(ReconnectAuth),
}

/// enroll 形态：`auth { public_key, signature, device_info }`（§3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollAuth {
    /// base64url 的 32 字节 Ed25519 公钥。
    pub public_key: String,
    /// base64url 的 64 字节签名。
    pub signature: String,
    pub device_info: DeviceInfo,
}

/// 重连形态：`auth { node_id, signature, last_seen_gen? }`（§3）。
///
/// **结构上就没有 `public_key`**：重连只传签名，公钥一律来自库内指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectAuth {
    pub node_id: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_gen: Option<u64>,
}

impl AuthPayload {
    /// 按有无 `node_id` 分派形态。
    ///
    /// - `node_id` 缺失 / `null` / 空串 → **尚无身份**，走 enroll（§2 的帧格式注释：
    ///   `node_id` 在 `hello`/`challenge`/`auth` 阶段可为空）。
    /// - `node_id` 是非空字符串 → 重连。
    /// - 重连形态**带 `public_key` 一律拒**（[`HandshakeError::PublicKeyOnReconnect`]）：
    ///   §3 规定公钥只在首次 enroll 上传，「把公钥带在重连里」正是想改绑
    ///   指纹的接管动作。显式失败而不是静默忽略——静默忽略会让运维看不出
    ///   有节点在试这条路。
    pub fn from_value(c: &serde_json::Value) -> Result<Self, HandshakeError> {
        let has_node_id = match c.get("node_id") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::String(s)) => !s.is_empty(),
            Some(_) => return Err(HandshakeError::MalformedAuth("node_id must be a string")),
        };
        if has_node_id {
            if c.get("public_key").is_some() {
                return Err(HandshakeError::PublicKeyOnReconnect);
            }
            return serde_json::from_value::<ReconnectAuth>(c.clone())
                .map(AuthPayload::Reconnect)
                .map_err(|_| HandshakeError::MalformedAuth("reconnect needs node_id + signature"));
        }
        serde_json::from_value::<EnrollAuth>(c.clone())
            .map(AuthPayload::Enroll)
            .map_err(|_| {
                HandshakeError::MalformedAuth("enroll needs public_key + signature + device_info")
            })
    }

    /// enroll 形态；重连载荷返回 `None`。
    pub fn as_enroll(&self) -> Option<&EnrollAuth> {
        match self {
            Self::Enroll(a) => Some(a),
            Self::Reconnect(_) => None,
        }
    }

    /// 重连形态；enroll 载荷返回 `None`。
    pub fn as_reconnect(&self) -> Option<&ReconnectAuth> {
        match self {
            Self::Reconnect(a) => Some(a),
            Self::Enroll(_) => None,
        }
    }
}

/// enroll 验签通过的结果。`node_id` 由注册表入库成功后签发，**本层不发**（§3）。
#[derive(Debug, Clone)]
pub struct EnrollAccepted {
    /// 验签通过的那把公钥——注册表用它算指纹、入库。
    pub public_key: [u8; PUBLIC_KEY_BYTES],
    pub device_info: DeviceInfo,
    pub protocol_version: u32,
}

/// 重连验签通过的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectAccepted {
    pub node_id: String,
    /// 节点记得的上一条连接代次（§7.1 顶替兜底）。
    pub last_seen_gen: Option<u64>,
    pub protocol_version: u32,
}

/// 单条连接的握手状态机（Hub 侧）。
#[derive(Debug)]
pub struct Handshake {
    phase: Phase,
    hello: Option<Hello>,
}

impl Default for Handshake {
    fn default() -> Self {
        Self::new()
    }
}

impl Handshake {
    /// 新连接的握手机：还没收到 `hello`（§3）。
    pub fn new() -> Self {
        Self {
            phase: Phase::AwaitingHello,
            hello: None,
        }
    }

    pub fn state(&self) -> HandshakeState {
        self.phase.state()
    }

    /// `hello` 载荷（arch/os/features 供 §7.1.4 选二进制、限流分类用）。
    pub fn hello(&self) -> Option<&Hello> {
        self.hello.as_ref()
    }

    /// 协商出的协议版本（§2）。`hello` 之前 / 被拒之后没有。
    pub fn protocol_version(&self) -> Option<u32> {
        match self.phase {
            Phase::Challenged { negotiated, .. } | Phase::Authed { negotiated } => Some(negotiated),
            Phase::AwaitingHello | Phase::Rejected => None,
        }
    }

    /// **尚未消费**的 nonce。没有 = 还没发 challenge，或已经用掉（用完即弃，§3）。
    pub fn pending_nonce(&self) -> Option<&str> {
        match &self.phase {
            Phase::Challenged { nonce, .. } => Some(nonce),
            Phase::AwaitingHello | Phase::Authed { .. } | Phase::Rejected => None,
        }
    }

    /// 收到 `hello`：校验载荷 → 协商版本 → 发出一次性 nonce。
    ///
    /// 一条连接只走一次：已 `Challenged` / `Authed` / `Rejected` 再来 `hello`
    /// 一律 [`HandshakeError::IllegalTransition`]——不给第二条挑战。
    pub fn on_hello(&mut self, frame_v: u32, hello: Hello) -> Result<Challenge, HandshakeError> {
        let state = self.phase.state();
        if state != HandshakeState::AwaitingHello {
            return Err(HandshakeError::IllegalTransition {
                state,
                event: "hello",
            });
        }
        for (field, value) in [
            ("client_version", &hello.client_version),
            ("arch", &hello.arch),
            ("os", &hello.os),
        ] {
            if value.is_empty() {
                return Err(HandshakeError::InvalidHello(field));
            }
        }
        let negotiated = negotiate_version(frame_v, PROTOCOL_VERSION)?;
        let nonce = issue_nonce();
        self.hello = Some(hello);
        self.phase = Phase::Challenged {
            node_v: frame_v,
            negotiated,
            nonce: nonce.clone(),
        };
        Ok(Challenge { nonce })
    }

    /// 首次 enroll 的 `auth`（§3）：用**本次上传的公钥**验签。
    ///
    /// 签名里 `node_id` 取空串——此刻还没有 `node_id`，它由注册表签发。
    ///
    /// **收到 `auth` 就用掉了这条连接的 challenge**：过 → `Authed`，
    /// 不过（含载荷非法）→ `Rejected` 终态，绝不退回 `Challenged`
    /// 让同一个 nonce 再来一次（§3「nonce 用完即弃」）。
    pub fn on_enroll<A: Authenticator>(
        &mut self,
        seq: u64,
        auth: &EnrollAuth,
        authz: &A,
    ) -> Result<EnrollAccepted, HandshakeError> {
        // 先取 nonce：状态不对就在解载荷之前拒——非法转移比载荷错误更根本。
        let challenge = self.take_nonce()?;
        let result = decode_fixed::<PUBLIC_KEY_BYTES>(&auth.public_key, "public_key").and_then(
            |public_key| {
                decode_fixed::<SIGNATURE_BYTES>(&auth.signature, "signature").and_then(
                    |signature| {
                        self.verify_auth(
                            seq,
                            &challenge,
                            "",
                            PublicKeySource::Submitted(&public_key),
                            &signature,
                            authz,
                        )
                        .map(|protocol_version| EnrollAccepted {
                            public_key,
                            device_info: auth.device_info.clone(),
                            protocol_version,
                        })
                    },
                )
            },
        );
        if result.is_err() {
            self.phase = Phase::Rejected;
        }
        result
    }

    /// 重连的 `auth`（§3）：**用库内指纹对应的公钥**验签。
    ///
    /// `stored_public_key` 只能来自库内（注册表查指纹），调用方**没有别的路子**
    /// 拿到它——[`ReconnectAuth`] 里根本没有上传公钥的字段。
    pub fn on_reconnect<A: Authenticator>(
        &mut self,
        seq: u64,
        auth: &ReconnectAuth,
        stored_public_key: &[u8; PUBLIC_KEY_BYTES],
        authz: &A,
    ) -> Result<ReconnectAccepted, HandshakeError> {
        let challenge = self.take_nonce()?;
        let result =
            decode_fixed::<SIGNATURE_BYTES>(&auth.signature, "signature").and_then(|signature| {
                self.verify_auth(
                    seq,
                    &challenge,
                    &auth.node_id,
                    PublicKeySource::Stored(stored_public_key),
                    &signature,
                    authz,
                )
                .map(|protocol_version| ReconnectAccepted {
                    node_id: auth.node_id.clone(),
                    last_seen_gen: auth.last_seen_gen,
                    protocol_version,
                })
            });
        if result.is_err() {
            self.phase = Phase::Rejected;
        }
        result
    }

    /// 取走本连接的一次性 nonce（**用完即弃**，§3）。
    ///
    /// 取出来就不在状态里了——后面验签成败都不还回去。
    fn take_nonce(&mut self) -> Result<TakenChallenge, HandshakeError> {
        let phase = std::mem::replace(&mut self.phase, Phase::AwaitingHello);
        match phase {
            Phase::Challenged {
                node_v,
                negotiated,
                nonce,
            } => Ok(TakenChallenge {
                node_v,
                negotiated,
                nonce,
            }),
            other => {
                // 没 challenge / 已经验过：状态原样放回去，让调用方看清是哪一态拒的。
                self.phase = other;
                let state = self.phase.state();
                Err(HandshakeError::IllegalTransition {
                    state,
                    event: "auth",
                })
            }
        }
    }

    /// 按规范编码验签（签名必须覆盖 `v + node_id + seq + t + nonce`，§3）。
    fn verify_auth<A: Authenticator>(
        &mut self,
        seq: u64,
        challenge: &TakenChallenge,
        node_id: &str,
        public_key: PublicKeySource<'_>,
        signature: &[u8; SIGNATURE_BYTES],
        authz: &A,
    ) -> Result<u32, HandshakeError> {
        let attempt = AuthAttempt {
            v: challenge.node_v,
            node_id,
            seq,
            msg_type: types::AUTH,
            nonce: &challenge.nonce,
            public_key,
            signature,
        };
        if !authz.verify_auth(&attempt) {
            // 身份没被证明。challenge 每连接一次，所以不给第二次机会：
            // 直接进终态，调用方回 `auth_fail` 后关连接。
            self.phase = Phase::Rejected;
            return Err(HandshakeError::BadSignature);
        }
        self.phase = Phase::Authed {
            negotiated: challenge.negotiated,
        };
        Ok(challenge.negotiated)
    }

    /// 握手成功回 `auth_ok`（§4）。**必须已 `Authed`**。
    ///
    /// `protocol_version` 由本层取协商结果，不由调用方指定——
    /// 免得回一帧自称的版本号和实际说的版本对不上（约束 14 的老 Hub + 新节点）。
    pub fn auth_ok(
        &self,
        seq: u64,
        node_id: &str,
        gen: u64,
        heartbeat_interval_ms: u64,
    ) -> Result<Frame, HandshakeError> {
        let negotiated = match self.phase {
            Phase::Authed { negotiated } => negotiated,
            _ => {
                let state = self.phase.state();
                return Err(HandshakeError::IllegalTransition {
                    state,
                    event: "auth_ok",
                });
            }
        };
        // 四个字段逐个写出来（而不是 to_value(AuthOk)）：`to_value` 的 Err 分支
        // 结构上不可达，留着就凑不满 100% 覆盖率；往返测试保证它们与 AuthOk 一致。
        let payload = serde_json::json!({
            "node_id": node_id,
            "protocol_version": negotiated,
            "gen": gen,
            "heartbeat_interval_ms": heartbeat_interval_ms,
        });
        Ok(Frame::push(types::AUTH_OK, seq, payload).with_node_id(node_id))
    }

    /// 握手失败回 `auth_fail`（§3、§4）并置 `Rejected` 终态。
    ///
    /// `reason` 取 `message::reason` 的常量；`gen` 仅在被顶替时带（§7.1）。
    /// 幂等：已经是 `Rejected` 也能调（先验签失败、后补 `auth_fail` 是常规顺序）。
    pub fn auth_fail(&mut self, seq: u64, reason: &str, gen: Option<u64>) -> Frame {
        self.phase = Phase::Rejected;
        let mut payload = serde_json::Map::with_capacity(2);
        payload.insert(
            String::from("reason"),
            serde_json::Value::String(reason.to_owned()),
        );
        if let Some(g) = gen {
            payload.insert(String::from("gen"), serde_json::Value::from(g));
        }
        Frame::push(types::AUTH_FAIL, seq, serde_json::Value::Object(payload))
    }

    /// 不需要 `auth_fail` 的失败（帧超限、Hub 未初始化等）直接置终态。
    /// 返回置位前的状态，便于调用方记日志。
    pub fn reject(&mut self) -> HandshakeState {
        let previous = self.phase.state();
        self.phase = Phase::Rejected;
        previous
    }

    /// 把 `challenge` 包成帧（§3）。
    pub fn challenge_frame(seq: u64, challenge: &Challenge) -> Frame {
        Frame::push(
            types::CHALLENGE,
            seq,
            serde_json::json!({ "nonce": challenge.nonce }),
        )
    }

    /// 超时断开前的末帧：`err{code:"handshake_timeout"}`（§2、§3）。
    pub fn timeout_error_frame(seq: u64) -> Frame {
        Frame::push(
            types::ERR,
            seq,
            serde_json::json!({
                "code": ErrorCode::HandshakeTimeout,
                "message": "no auth frame within handshake timeout",
            }),
        )
    }
}

/// 32 字节 CSPRNG nonce，base64url 无填充（§3）。
///
/// 直接从 `OsRng` 读 OS CSPRNG，不用 `thread_rng`：一次性挑战的价值全在
/// 「预测不到」，没有理由为它多引入一层线程状态。
fn issue_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut bytes);
    Base64UrlUnpadded::encode_string(&bytes)
}

/// 解 base64url 到定长数组。长度不符与字符集非法都归为「载荷非法」。
fn decode_fixed<const N: usize>(s: &str, field: &'static str) -> Result<[u8; N], HandshakeError> {
    Base64UrlUnpadded::decode_vec(s)
        .ok()
        .and_then(|raw| <[u8; N]>::try_from(raw).ok())
        .ok_or(HandshakeError::MalformedAuth(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{reason, AuthFail, AuthOk, ErrPayload};
    use identity::{signature_message, KeyPair};
    use serde_json::json;

    /// **真**验签器：用 identity crate 的规范编码 + Ed25519 验签。
    ///
    /// 这里绝不用「恒返回 true」的假验签器：重放类测试只有在真验签下
    /// 才说明得了问题——假验签器能让重放测试「通过」，而线上照样被重放。
    struct HubIdentity;

    impl Authenticator for HubIdentity {
        fn verify_auth(&self, attempt: &AuthAttempt<'_>) -> bool {
            let key = match attempt.public_key {
                PublicKeySource::Submitted(k) | PublicKeySource::Stored(k) => k,
            };
            let msg = signature_message(
                attempt.v,
                attempt.node_id,
                attempt.seq,
                attempt.msg_type,
                attempt.nonce,
            );
            KeyPair::verify(key, &msg, attempt.signature)
        }
    }

    /// 记录每次验签输入的验签器，用于断言「交给验签器的是哪把钥匙、哪个 nonce」。
    struct Recorder {
        seen: std::cell::RefCell<Vec<AuthAttemptOwned>>,
        accept: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct AuthAttemptOwned {
        v: u32,
        node_id: String,
        seq: u64,
        msg_type: String,
        nonce: String,
        source: &'static str,
    }

    impl Recorder {
        fn new(accept: bool) -> Self {
            Self {
                seen: std::cell::RefCell::new(Vec::new()),
                accept,
            }
        }
    }

    impl Authenticator for Recorder {
        fn verify_auth(&self, attempt: &AuthAttempt<'_>) -> bool {
            let source = match attempt.public_key {
                PublicKeySource::Submitted(_) => "submitted",
                PublicKeySource::Stored(_) => "stored",
            };
            self.seen.borrow_mut().push(AuthAttemptOwned {
                v: attempt.v,
                node_id: String::from(attempt.node_id),
                seq: attempt.seq,
                msg_type: String::from(attempt.msg_type),
                nonce: String::from(attempt.nonce),
                source,
            });
            self.accept
        }
    }

    fn hello() -> Hello {
        Hello {
            client_version: String::from("0.1.0"),
            arch: String::from("aarch64"),
            os: String::from("linux"),
            features: vec![String::from("terminal"), String::from("files")],
        }
    }

    fn device_info() -> DeviceInfo {
        DeviceInfo {
            name: String::from("pi"),
            os: String::from("linux"),
            arch: String::from("aarch64"),
            hostname: String::from("raspberrypi"),
            node_version: String::from("0.1.0"),
        }
    }

    /// 已发过 `challenge` 的握手（状态 `Challenged`）。
    fn challenged() -> (Handshake, Challenge) {
        let mut hs = Handshake::new();
        let ch = hs.on_hello(PROTOCOL_VERSION, hello()).unwrap();
        (hs, ch)
    }

    /// 按给定 nonce 签一份 enroll 的 `auth`（node 侧该做的事）。
    fn signed_enroll(kp: &KeyPair, v: u32, seq: u64, nonce: &str) -> EnrollAuth {
        let msg = signature_message(v, "", seq, types::AUTH, nonce);
        EnrollAuth {
            public_key: Base64UrlUnpadded::encode_string(&kp.public_key_bytes()),
            signature: Base64UrlUnpadded::encode_string(&kp.sign(&msg)),
            device_info: device_info(),
        }
    }

    /// 按给定 nonce 签一份重连的 `auth`。
    fn signed_reconnect(
        kp: &KeyPair,
        v: u32,
        seq: u64,
        nonce: &str,
        node_id: &str,
    ) -> ReconnectAuth {
        let msg = signature_message(v, node_id, seq, types::AUTH, nonce);
        ReconnectAuth {
            node_id: String::from(node_id),
            signature: Base64UrlUnpadded::encode_string(&kp.sign(&msg)),
            last_seen_gen: None,
        }
    }

    // ---------- 测试标准 ①：握手完成后 Authed，auth_ok 带 4 个字段 ----------

    #[test]
    fn handshake_reaches_authed_and_auth_ok_carries_four_fields() {
        let kp = KeyPair::generate();
        let (mut hs, ch) = challenged();
        assert_eq!(hs.state(), HandshakeState::Challenged);

        let auth = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch.nonce);
        let accepted = hs.on_enroll(0, &auth, &HubIdentity).unwrap();
        assert_eq!(hs.state(), HandshakeState::Authed);
        assert_eq!(accepted.public_key, kp.public_key_bytes());
        assert_eq!(accepted.protocol_version, PROTOCOL_VERSION);
        // DeviceInfo 没有 PartialEq（message.rs 未派生），逐字段断言
        assert_eq!(accepted.device_info.name, "pi");
        assert_eq!(accepted.device_info.arch, "aarch64");
        assert_eq!(accepted.device_info.hostname, "raspberrypi");
        assert_eq!(accepted.device_info.node_version, "0.1.0");
        assert_eq!(accepted.device_info.os, "linux");

        let f = hs.auth_ok(7, "n_pi01", 3, 15_000).unwrap();
        assert_eq!(f.msg_type, types::AUTH_OK);
        assert_eq!(f.node_id.as_deref(), Some("n_pi01"));
        // 4 个字段一个都不能少（§4）
        let ok: AuthOk = f.payload().unwrap();
        assert_eq!(ok.node_id, "n_pi01");
        assert_eq!(ok.protocol_version, PROTOCOL_VERSION);
        assert_eq!(ok.gen, 3);
        assert_eq!(ok.heartbeat_interval_ms, 15_000);
        let text = f.to_text().unwrap();
        for field in [
            "\"node_id\"",
            "\"protocol_version\"",
            "\"gen\"",
            "\"heartbeat_interval_ms\"",
        ] {
            assert!(text.contains(field), "auth_ok missing {field}: {text}");
        }
    }

    #[test]
    fn auth_ok_is_refused_before_auth() {
        // 没验签就发 auth_ok = 放行了一个没证明身份的连接
        let hs = Handshake::new();
        assert!(matches!(
            hs.auth_ok(0, "n_x", 1, 15_000),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::AwaitingHello,
                event: "auth_ok"
            })
        ));

        let (hs, _) = challenged();
        assert!(matches!(
            hs.auth_ok(0, "n_x", 1, 15_000),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::Challenged,
                event: "auth_ok"
            })
        ));

        let mut hs2 = Handshake::new();
        assert_eq!(hs2.reject(), HandshakeState::AwaitingHello);
        assert!(matches!(
            hs2.auth_ok(0, "n_x", 1, 15_000),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::Rejected,
                event: "auth_ok"
            })
        ));
    }

    // ---------- 测试标准 ②：用连接 1 的 nonce 签的名应答连接 2 的 nonce ----------

    #[test]
    fn signature_over_connection_one_nonce_fails_on_connection_two() {
        let kp = KeyPair::generate();
        let (_hs1, ch1) = challenged();
        // 连接 1 的合法签名
        let auth_from_conn1 = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch1.nonce);

        // 连接 2：新的 nonce
        let (mut hs2, ch2) = challenged();
        assert_ne!(ch1.nonce, ch2.nonce, "every connection gets a fresh nonce");

        let err = hs2
            .on_enroll(0, &auth_from_conn1, &HubIdentity)
            .unwrap_err();
        assert_eq!(err, HandshakeError::BadSignature);
        // 验签失败 → 终态，不再有 nonce 可用，也换不出 auth_ok
        assert_eq!(hs2.state(), HandshakeState::Rejected);
        assert_eq!(hs2.pending_nonce(), None);
        assert!(hs2.auth_ok(0, "n_x", 1, 15_000).is_err());

        // 调用方回 auth_fail（§3）
        let fail = hs2.auth_fail(1, reason::IDENTITY_MISMATCH, None);
        let p: AuthFail = fail.payload().unwrap();
        assert_eq!(p.reason, "identity mismatch");
        assert_eq!(p.gen, None);
    }

    #[test]
    fn signature_over_a_foreign_nonce_is_also_rejected() {
        // 同一把钥匙、另一个 nonce：验签同样必须失败（nonce 是签进去的）
        let kp = KeyPair::generate();
        let (_hs1, ch1) = challenged();
        let (mut hs2, ch2) = challenged();
        let auth = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch1.nonce);
        assert_eq!(
            hs2.on_enroll(0, &auth, &HubIdentity).unwrap_err(),
            HandshakeError::BadSignature
        );
        assert_ne!(ch2.nonce, ch1.nonce);
    }

    // ---------- 测试标准 ③：旧 auth 签名跨连接重放被拒（签名覆盖 seq） ----------

    #[test]
    fn old_auth_payload_replayed_on_a_new_connection_is_rejected() {
        let kp = KeyPair::generate();
        // 连接 1 完成握手，auth 帧 seq = 0
        let (mut hs1, ch1) = challenged();
        let auth1 = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch1.nonce);
        hs1.on_enroll(0, &auth1, &HubIdentity).unwrap();
        let payload = serde_json::to_value(&auth1).unwrap();

        // 连接 2：seq 同样从 0 起，但 nonce 是新的
        let (mut hs2, ch2) = challenged();
        let replayed = AuthPayload::from_value(&payload).unwrap();
        // 重放的 enroll 载荷不会「变成」重连：没有 node_id 就是 enroll
        let replayed_enroll = replayed.as_enroll().expect("replayed enroll stays enroll");
        assert!(replayed.as_reconnect().is_none());
        assert_eq!(
            hs2.on_enroll(0, replayed_enroll, &HubIdentity).unwrap_err(),
            HandshakeError::BadSignature
        );
        assert_ne!(ch2.nonce, ch1.nonce);

        // 同一连接上按新 nonce 重签一份 → 必须能过（否则上面只是「全拒」）
        let (mut hs3, ch3) = challenged();
        let fresh = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch3.nonce);
        assert!(hs3.on_enroll(0, &fresh, &HubIdentity).is_ok());
        assert_ne!(ch3.nonce, ch2.nonce);
    }

    #[test]
    fn signature_is_bound_to_the_auth_frame_seq() {
        // 签名覆盖 seq：同一连接上把 auth 挪到别的 seq 也验不过（§3）
        let kp = KeyPair::generate();
        let (mut hs, ch) = challenged();
        let auth = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch.nonce);
        assert_eq!(
            hs.on_enroll(5, &auth, &HubIdentity).unwrap_err(),
            HandshakeError::BadSignature
        );
        // seq 对得上就过
        let (mut hs2, ch2) = challenged();
        let auth2 = signed_enroll(&kp, PROTOCOL_VERSION, 7, &ch2.nonce);
        assert!(hs2.on_enroll(7, &auth2, &HubIdentity).is_ok());
    }

    #[test]
    fn reconnect_signature_replay_is_rejected_too() {
        let kp = KeyPair::generate();
        let (_hs1, ch1) = challenged();
        let auth1 = signed_reconnect(&kp, PROTOCOL_VERSION, 0, &ch1.nonce, "n_pi01");

        let (mut hs2, _ch2) = challenged();
        assert_eq!(
            hs2.on_reconnect(0, &auth1, &kp.public_key_bytes(), &HubIdentity)
                .unwrap_err(),
            HandshakeError::BadSignature
        );
        assert_eq!(hs2.state(), HandshakeState::Rejected);
    }

    // ---------- 测试标准 ④：版本协商取较小者 ----------

    #[test]
    fn version_negotiation_takes_the_smaller() {
        assert_eq!(negotiate_version(1, 1).unwrap(), 1);
        assert_eq!(negotiate_version(1, 2).unwrap(), 1);
        assert_eq!(negotiate_version(2, 1).unwrap(), 1);
        // 远高于本端也按规则取小者，不把人踢下线（§2）
        assert_eq!(negotiate_version(99, 1).unwrap(), 1);
    }

    #[test]
    fn version_negotiation_rejects_zero() {
        assert_eq!(
            negotiate_version(0, 1).unwrap_err(),
            HandshakeError::InvalidVersion(0)
        );
        assert_eq!(
            negotiate_version(1, 0).unwrap_err(),
            HandshakeError::InvalidVersion(0)
        );
    }

    #[test]
    fn hello_negotiates_down_and_auth_ok_reports_negotiated_version() {
        // 节点声明 v=2，Hub 是 v=1 → 协商结果 1，auth_ok 必须回 1
        let kp = KeyPair::generate();
        let mut hs = Handshake::new();
        assert_eq!(hs.protocol_version(), None);
        let ch = hs.on_hello(2, hello()).unwrap();
        assert_eq!(hs.protocol_version(), Some(1));
        assert_eq!(hs.hello().unwrap().arch, "aarch64");

        // 签名覆盖的是节点声明的 v（challenge 不带 Hub 的版本，节点无从得知协商值）
        let auth = signed_enroll(&kp, 2, 0, &ch.nonce);
        assert!(hs.on_enroll(0, &auth, &HubIdentity).is_ok());
        let f = hs.auth_ok(0, "n_pi01", 1, 15_000).unwrap();
        let ok: AuthOk = f.payload().unwrap();
        assert_eq!(ok.protocol_version, 1);

        // 声明 v=1 的老节点：同样能握手（老 node + 新 Hub 必须能继续跑）
        let mut old = Handshake::new();
        assert_eq!(old.on_hello(1, hello()).unwrap().nonce.len(), 43);
        assert_eq!(old.protocol_version(), Some(1));
    }

    #[test]
    fn hello_with_zero_version_is_rejected() {
        let mut hs = Handshake::new();
        assert_eq!(
            hs.on_hello(0, hello()).unwrap_err(),
            HandshakeError::InvalidVersion(0)
        );
        // 没进 Challenged：连 challenge 都不该发出去
        assert_eq!(hs.state(), HandshakeState::AwaitingHello);
    }

    // ---------- 测试标准 ⑤：nonce 用完即弃 ----------

    #[test]
    fn nonce_is_consumed_once() {
        let kp = KeyPair::generate();
        let (mut hs, ch) = challenged();
        assert_eq!(hs.pending_nonce(), Some(ch.nonce.as_str()));

        let auth = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch.nonce);
        assert!(hs.on_enroll(0, &auth, &HubIdentity).is_ok());
        // 验签通过后 nonce 也不在手上
        assert_eq!(hs.pending_nonce(), None);
        assert!(hs.protocol_version().is_some());

        // 同一 nonce 第二次消费 → 直接被状态机拒（没有 nonce 可取）
        let again = signed_enroll(&kp, PROTOCOL_VERSION, 1, &ch.nonce);
        assert!(matches!(
            hs.on_enroll(1, &again, &HubIdentity),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::Authed,
                event: "auth"
            })
        ));
    }

    #[test]
    fn failed_auth_also_burns_the_nonce() {
        // 验签失败也不还 nonce：challenge 每连接一次，不给猜第二次的机会
        let (mut hs, _ch) = challenged();
        let bad = EnrollAuth {
            public_key: Base64UrlUnpadded::encode_string(&[1u8; 32]),
            signature: Base64UrlUnpadded::encode_string(&[2u8; 64]),
            device_info: device_info(),
        };
        assert_eq!(
            hs.on_enroll(0, &bad, &HubIdentity).unwrap_err(),
            HandshakeError::BadSignature
        );
        assert_eq!(hs.pending_nonce(), None);
        assert_eq!(hs.protocol_version(), None);
        assert_eq!(
            hs.reject(),
            HandshakeState::Rejected,
            "already rejected by the failed auth"
        );
    }

    #[test]
    fn a_connection_gets_only_one_challenge() {
        let (_hs, ch) = challenged();
        let mut hs = Handshake::new();
        let ch2 = hs.on_hello(PROTOCOL_VERSION, hello()).unwrap();
        assert_ne!(ch2.nonce, ch.nonce, "each connection issues a fresh nonce");
        // 同一连接再要一个 challenge → 非法转移
        assert!(matches!(
            hs.on_hello(PROTOCOL_VERSION, hello()),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::Challenged,
                event: "hello"
            })
        ));
        assert_eq!(hs.pending_nonce(), Some(ch2.nonce.as_str()));
    }

    // ---------- 测试标准 ⑥：握手超时判定（10s 边界前后） ----------

    #[test]
    fn timeout_boundary_at_ten_seconds() {
        let before = HANDSHAKE_TIMEOUT - Duration::from_millis(1);
        assert!(!should_close_for_timeout(
            HandshakeState::Challenged,
            before
        ));
        // 正好 10s：没收到 auth 就该关了（§3）
        assert!(should_close_for_timeout(
            HandshakeState::Challenged,
            HANDSHAKE_TIMEOUT
        ));
        assert!(should_close_for_timeout(
            HandshakeState::Challenged,
            HANDSHAKE_TIMEOUT + Duration::from_millis(1)
        ));
        // 连 hello 都没来也是超时
        assert!(!should_close_for_timeout(
            HandshakeState::AwaitingHello,
            before
        ));
        assert!(should_close_for_timeout(
            HandshakeState::AwaitingHello,
            HANDSHAKE_TIMEOUT
        ));
    }

    #[test]
    fn timeout_does_not_apply_after_handshake_ends() {
        // 已鉴权的连接不因「握手慢」被关：慢与它无关了
        assert!(!should_close_for_timeout(
            HandshakeState::Authed,
            HANDSHAKE_TIMEOUT * 100
        ));
        assert!(!should_close_for_timeout(
            HandshakeState::Rejected,
            HANDSHAKE_TIMEOUT * 100
        ));
        assert!(!HandshakeState::Authed.in_progress());
        assert!(!HandshakeState::Rejected.in_progress());
        assert!(HandshakeState::AwaitingHello.in_progress());
        assert!(HandshakeState::Challenged.in_progress());
    }

    #[test]
    fn timeout_error_frame_carries_handshake_timeout_code() {
        let f = Handshake::timeout_error_frame(9);
        assert_eq!(f.msg_type, types::ERR);
        let p: ErrPayload = f.payload().unwrap();
        assert_eq!(p.code, ErrorCode::HandshakeTimeout);
        assert!(
            f.to_text().unwrap().contains("\"handshake_timeout\""),
            "wire code must be snake_case"
        );
    }

    // ---------- 非法转移与载荷校验 ----------

    #[test]
    fn auth_before_hello_is_rejected() {
        let kp = KeyPair::generate();
        let mut hs = Handshake::new();
        let auth = signed_enroll(&kp, PROTOCOL_VERSION, 0, "whatever");
        assert!(matches!(
            hs.on_enroll(0, &auth, &HubIdentity),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::AwaitingHello,
                event: "auth"
            })
        ));
        assert_eq!(hs.state(), HandshakeState::AwaitingHello);
        assert!(matches!(
            hs.on_reconnect(
                0,
                &ReconnectAuth {
                    node_id: String::from("n_pi01"),
                    signature: String::from("x"),
                    last_seen_gen: None,
                },
                &kp.public_key_bytes(),
                &HubIdentity
            ),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::AwaitingHello,
                event: "auth"
            })
        ));
    }

    #[test]
    fn hello_after_rejection_is_rejected() {
        let mut hs = Handshake::new();
        assert_eq!(hs.reject(), HandshakeState::AwaitingHello);
        assert!(matches!(
            hs.on_hello(PROTOCOL_VERSION, hello()),
            Err(HandshakeError::IllegalTransition {
                state: HandshakeState::Rejected,
                event: "hello"
            })
        ));
        assert!(hs.hello().is_none());
    }

    #[test]
    fn hello_rejects_empty_capability_fields() {
        for field in ["client_version", "arch", "os"] {
            let mut hs = Handshake::new();
            let mut h = hello();
            match field {
                "client_version" => h.client_version.clear(),
                "arch" => h.arch.clear(),
                _ => h.os.clear(),
            }
            assert_eq!(
                hs.on_hello(PROTOCOL_VERSION, h).unwrap_err(),
                HandshakeError::InvalidHello(field),
                "field {field}"
            );
            assert_eq!(hs.state(), HandshakeState::AwaitingHello);
        }
    }

    #[test]
    fn auth_payload_dispatches_on_node_id() {
        // 无 node_id → enroll
        let enroll = json!({
            "public_key": "pk",
            "signature": "sig",
            "device_info": {
                "name": "pi", "os": "linux", "arch": "aarch64",
                "hostname": "pi", "node_version": "0.1.0"
            }
        });
        assert!(matches!(
            AuthPayload::from_value(&enroll).unwrap(),
            AuthPayload::Enroll(_)
        ));
        // node_id 为 null / 空串 → 仍是 enroll（尚无身份）
        let mut null_id = enroll.clone();
        null_id["node_id"] = serde_json::Value::Null;
        assert!(matches!(
            AuthPayload::from_value(&null_id).unwrap(),
            AuthPayload::Enroll(_)
        ));
        let mut empty_id = enroll.clone();
        empty_id["node_id"] = serde_json::Value::String(String::new());
        assert!(matches!(
            AuthPayload::from_value(&empty_id).unwrap(),
            AuthPayload::Enroll(_)
        ));
        // 带 node_id → 重连
        let reconnect = json!({"node_id": "n_pi01", "signature": "sig"});
        let got = AuthPayload::from_value(&reconnect).unwrap();
        assert_eq!(
            got.as_reconnect().expect("node_id present → reconnect"),
            &ReconnectAuth {
                node_id: String::from("n_pi01"),
                signature: String::from("sig"),
                last_seen_gen: None,
            }
        );
        // 两种形态互斥：重连载荷取不到 enroll 载荷
        assert!(got.as_enroll().is_none());
        // 带 last_seen_gen
        let with_gen = json!({"node_id": "n_pi01", "signature": "sig", "last_seen_gen": 4});
        let with_gen = AuthPayload::from_value(&with_gen).unwrap();
        assert_eq!(
            with_gen
                .as_reconnect()
                .expect("node_id present → reconnect")
                .last_seen_gen,
            Some(4)
        );
    }

    #[test]
    fn auth_payload_rejects_bad_shapes() {
        // node_id 类型不对
        let bad_id = json!({"node_id": 42, "signature": "sig"});
        assert_eq!(
            AuthPayload::from_value(&bad_id).unwrap_err(),
            HandshakeError::MalformedAuth("node_id must be a string")
        );
        // enroll 缺字段
        let no_sig = json!({"public_key": "pk"});
        assert!(matches!(
            AuthPayload::from_value(&no_sig).unwrap_err(),
            HandshakeError::MalformedAuth(_)
        ));
        // 重连缺签名
        let no_sig2 = json!({"node_id": "n_pi01"});
        assert!(matches!(
            AuthPayload::from_value(&no_sig2).unwrap_err(),
            HandshakeError::MalformedAuth(_)
        ));
        // 完全不是 auth
        assert!(AuthPayload::from_value(&json!({})).is_err());
    }

    #[test]
    fn reconnect_payload_with_public_key_is_refused() {
        // §3：公钥只在首次 enroll 上传。重连带公钥 = 想改绑指纹的接管动作
        let c = json!({"node_id": "n_pi01", "signature": "sig", "public_key": "pk"});
        assert_eq!(
            AuthPayload::from_value(&c).unwrap_err(),
            HandshakeError::PublicKeyOnReconnect
        );
    }

    #[test]
    fn malformed_encoded_key_or_signature_is_refused() {
        let kp = KeyPair::generate();
        let (mut hs, ch) = challenged();
        let good = signed_enroll(&kp, PROTOCOL_VERSION, 0, &ch.nonce);

        // 非法 base64url 字符集
        let bad_charset = EnrollAuth {
            public_key: String::from("not base64!"),
            signature: good.signature.clone(),
            device_info: device_info(),
        };
        assert_eq!(
            hs.on_enroll(0, &bad_charset, &HubIdentity).unwrap_err(),
            HandshakeError::MalformedAuth("public_key")
        );
        // 收到 auth 就烧掉了 challenge：载荷非法也不给第二次机会
        assert_eq!(hs.state(), HandshakeState::Rejected);
        assert_eq!(hs.pending_nonce(), None);

        // 长度不对（32 字节的槽塞 31 字节）
        let (mut hs, _) = challenged();
        let bad_len = EnrollAuth {
            public_key: Base64UrlUnpadded::encode_string(&[0u8; 31]),
            signature: good.signature.clone(),
            device_info: device_info(),
        };
        assert_eq!(
            hs.on_enroll(0, &bad_len, &HubIdentity).unwrap_err(),
            HandshakeError::MalformedAuth("public_key")
        );

        // 签名槽塞公钥
        let (mut hs, _) = challenged();
        let bad_sig = EnrollAuth {
            public_key: good.public_key.clone(),
            signature: Base64UrlUnpadded::encode_string(&kp.public_key_bytes()),
            device_info: device_info(),
        };
        assert_eq!(
            hs.on_enroll(0, &bad_sig, &HubIdentity).unwrap_err(),
            HandshakeError::MalformedAuth("signature")
        );

        // 重连侧同样
        let (mut hs, _) = challenged();
        let bad_reconnect = ReconnectAuth {
            node_id: String::from("n_pi01"),
            signature: String::from("***"),
            last_seen_gen: None,
        };
        assert_eq!(
            hs.on_reconnect(0, &bad_reconnect, &kp.public_key_bytes(), &HubIdentity)
                .unwrap_err(),
            HandshakeError::MalformedAuth("signature")
        );
        assert_eq!(hs.state(), HandshakeState::Rejected);

        // 载荷合法的重连不受上面影响（每条用例都是独立连接）
        let (mut hs, ch2) = challenged();
        let ok_reconnect = signed_reconnect(&kp, PROTOCOL_VERSION, 0, &ch2.nonce, "n_pi01");
        assert!(hs
            .on_reconnect(0, &ok_reconnect, &kp.public_key_bytes(), &HubIdentity)
            .is_ok());
        assert_eq!(hs.state(), HandshakeState::Authed);
        assert_ne!(ch2.nonce, ch.nonce);
    }

    // ---------- 交给验签器的是什么 ----------

    #[test]
    fn enroll_hands_over_the_uploaded_key_and_reconnect_the_stored_one() {
        let kp = KeyPair::generate();
        let other = KeyPair::generate();

        // enroll：Submitted（本次上传的公钥），node_id 为空串
        let (mut hs, ch) = challenged();
        let rec = Recorder::new(true);
        let auth = signed_enroll(&kp, PROTOCOL_VERSION, 4, &ch.nonce);
        assert!(hs.on_enroll(4, &auth, &rec).is_ok());
        assert_eq!(
            rec.seen.borrow().as_slice(),
            &[AuthAttemptOwned {
                v: PROTOCOL_VERSION,
                node_id: String::new(),
                seq: 4,
                msg_type: String::from("auth"),
                nonce: ch.nonce.clone(),
                source: "submitted",
            }]
        );

        // 重连：Stored（库内指纹对应的公钥）——即使调用方递来另一把钥匙，
        // 载荷里也没有上传公钥的字段，验签器拿到的只会是 Stored
        let (mut hs2, ch2) = challenged();
        let rec2 = Recorder::new(true);
        let rc = signed_reconnect(&kp, PROTOCOL_VERSION, 1, &ch2.nonce, "n_pi01");
        let accepted = hs2
            .on_reconnect(1, &rc, &other.public_key_bytes(), &rec2)
            .unwrap();
        assert_eq!(accepted.node_id, "n_pi01");
        assert_eq!(accepted.last_seen_gen, None);
        assert_eq!(accepted.protocol_version, PROTOCOL_VERSION);
        assert_eq!(rec2.seen.borrow()[0].source, "stored");
        assert_eq!(rec2.seen.borrow()[0].node_id, "n_pi01");

        // 真验签：拿别人的私钥签（Stored 是库内那把）→ 失败
        let (mut hs3, ch3) = challenged();
        let forged = signed_reconnect(&other, PROTOCOL_VERSION, 0, &ch3.nonce, "n_pi01");
        assert_eq!(
            hs3.on_reconnect(0, &forged, &kp.public_key_bytes(), &HubIdentity)
                .unwrap_err(),
            HandshakeError::BadSignature
        );
    }

    #[test]
    fn reconnect_reports_last_seen_gen_for_supersede_check() {
        // §7.1：last_seen_gen 由调用方与当前 connection_gen 比对
        let kp = KeyPair::generate();
        let (mut hs, ch) = challenged();
        let mut rc = signed_reconnect(&kp, PROTOCOL_VERSION, 0, &ch.nonce, "n_pi01");
        rc.last_seen_gen = Some(4);
        let accepted = hs
            .on_reconnect(0, &rc, &kp.public_key_bytes(), &HubIdentity)
            .unwrap();
        assert_eq!(accepted.last_seen_gen, Some(4));
        assert_eq!(hs.state(), HandshakeState::Authed);
    }

    // ---------- 帧构造与 nonce 本身 ----------

    #[test]
    fn challenge_is_32_bytes_of_base64url() {
        let mut hs = Handshake::new();
        let ch = hs.on_hello(PROTOCOL_VERSION, hello()).unwrap();
        let raw = Base64UrlUnpadded::decode_vec(&ch.nonce).unwrap();
        assert_eq!(raw.len(), NONCE_BYTES);
        assert_eq!(ch.nonce.len(), 43, "32 bytes → 43 unpadded base64url chars");
        // 每连接一次：两条连接两个 nonce
        let mut hs2 = Handshake::new();
        let ch2 = hs2.on_hello(PROTOCOL_VERSION, hello()).unwrap();
        assert_ne!(ch.nonce, ch2.nonce);

        let f = Handshake::challenge_frame(0, &ch);
        assert_eq!(f.msg_type, types::CHALLENGE);
        let back: Challenge = f.payload().unwrap();
        assert_eq!(back.nonce, ch.nonce);
    }

    #[test]
    fn auth_fail_carries_gen_only_when_superseded() {
        let mut hs = Handshake::new();
        let f = hs.auth_fail(0, reason::SUPERSEDED, Some(9));
        assert_eq!(f.msg_type, types::AUTH_FAIL);
        let p: AuthFail = f.payload().unwrap();
        assert_eq!(p.reason, reason::SUPERSEDED);
        assert_eq!(p.gen, Some(9));
        assert_eq!(hs.state(), HandshakeState::Rejected);
        assert_eq!(hs.protocol_version(), None);

        // 没有 gen 时字段整个不出现（§4 的 `gen?`）
        let f2 = hs.auth_fail(1, reason::HUB_NOT_INITIALIZED, None);
        assert!(
            !f2.to_text().unwrap().contains("gen"),
            "gen must be omitted, not null"
        );
        assert_eq!(f2.payload::<AuthFail>().unwrap().gen, None);
    }

    #[test]
    fn payload_types_roundtrip_through_the_wire_format() {
        let enroll = EnrollAuth {
            public_key: Base64UrlUnpadded::encode_string(&[7u8; 32]),
            signature: Base64UrlUnpadded::encode_string(&[9u8; 64]),
            device_info: device_info(),
        };
        let c = serde_json::to_value(&enroll).unwrap();
        let parsed = AuthPayload::from_value(&c).unwrap();
        let got = parsed.as_enroll().expect("enroll without node_id");
        assert_eq!(got.public_key, enroll.public_key);
        assert_eq!(got.signature, enroll.signature);
        assert_eq!(got.device_info.hostname, "raspberrypi");

        let reconnect = ReconnectAuth {
            node_id: String::from("n_pi01"),
            signature: Base64UrlUnpadded::encode_string(&[9u8; 64]),
            last_seen_gen: Some(2),
        };
        let c = serde_json::to_value(&reconnect).unwrap();
        assert_eq!(
            AuthPayload::from_value(&c)
                .unwrap()
                .as_reconnect()
                .expect("reconnect with node_id"),
            &reconnect
        );
        // last_seen_gen 省略时也能解出来（§3 的 `last_seen_gen?`）
        let mut without = serde_json::to_value(&reconnect).unwrap();
        without.as_object_mut().unwrap().remove("last_seen_gen");
        assert_eq!(
            AuthPayload::from_value(&without)
                .unwrap()
                .as_reconnect()
                .expect("reconnect with node_id")
                .last_seen_gen,
            None
        );
    }

    #[test]
    fn default_is_a_fresh_handshake() {
        let hs = Handshake::default();
        assert_eq!(hs.state(), HandshakeState::AwaitingHello);
        assert!(hs.hello().is_none());
        assert_eq!(hs.protocol_version(), None);
        assert_eq!(hs.pending_nonce(), None);
        // `Debug` 要能看到状态名：握手日志直接打它
        let d = format!("{hs:?}");
        assert!(d.contains("AwaitingHello"), "got {d}");
        assert_eq!(format!("{}", HandshakeError::BadSignature), "bad signature");
    }

    #[test]
    fn error_messages_name_the_cause() {
        // 握手错误会直接进日志与用户可见的错误提示，文本不能是 `variant name`
        let cases = [
            (
                HandshakeError::IllegalTransition {
                    state: HandshakeState::Challenged,
                    event: "auth",
                },
                "illegal handshake transition: Challenged received auth",
            ),
            (
                HandshakeError::InvalidHello("arch"),
                "invalid hello payload: arch is empty",
            ),
            (
                HandshakeError::InvalidVersion(0),
                "invalid protocol version: 0 (protocol versions start at 1)",
            ),
            (
                HandshakeError::MalformedAuth("public_key"),
                "invalid auth payload: public_key",
            ),
            (
                HandshakeError::PublicKeyOnReconnect,
                "reconnect must not carry an uploaded public key (protocol.md 3)",
            ),
            (HandshakeError::BadSignature, "bad signature"),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_string(), want, "{err:?}");
            // 错误里不得出现 nonce / 公钥这类秘密材料
            assert!(!err.to_string().contains("nonce"), "{err:?}");
        }
    }
}
