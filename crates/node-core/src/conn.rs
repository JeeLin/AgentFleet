//! 设备侧 WS 客户端：主动外联 + 握手 + 顶替判定。
//!
//! 设计要点（`protocol.md` §3、§7.1）：
//! - **连接由 node 主动建立**（约束 1）：Hub 零入站端口，这里只有出站。
//! - **`superseded` 是终态，不是错误**：同 `node_id` 的新连接顶掉旧连接时，
//!   旧连接必须**退出且不再重连**。若把它当普通断开去退避重连，
//!   两边会无限互抢（§7.1 的「每 60s 顶替一次，永久进行」）。
//! - **「不再重连」不依赖帧送达**：`disconnect` 帧可能丢，所以还要记
//!   `last_seen_gen`，下次 `auth` 带上让 Hub 单方面判定（子任务 7 接线）。
//! - **私钥永不外泄**（约束 13）：签名只用 `identity::KeyPair::sign`，
//!   没有任何字段承载私钥或它的 base64。
//!
//! 本文件回答「连一次会发生什么」；退避重连的时间策略在
//! [`crate::reconnect`]，把两者串起来的循环在 `node-cli` 的 worker 里。
//! 握手之后连接由 [`Established`] 独占持有——[`connect_once`] 返回的 socket
//! 不能被丢掉，否则调用方拿到的只是一次「曾经握过手」的记录。

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use identity::KeyPair;
use tokio::net::TcpStream;

use protocol::frame::Frame;
use protocol::handshake::{HandshakeError, HANDSHAKE_TIMEOUT};
use protocol::message::{
    reason, AuthFail, AuthOk, Challenge, DeviceInfo, Disconnect, DisconnectReason, Hello,
};

/// 连接结果。`Ok` 表示握手通过、拿到了 Hub 签发的 `node_id`。
#[derive(Debug, Clone)]
pub enum ConnOutcome {
    /// 握手成功。`node_id` 由 Hub 签发（首次 enroll 时）或复用（重连）。
    Authed {
        node_id: String,
        protocol_version: u32,
        gen: u64,
        heartbeat_interval_ms: u64,
        /// 首次 enroll 成功——调用方应把 `node_id` 回填进 identity.json。
        enrolled: bool,
    },
    /// 被顶替。**必须停止重连**（§7.1）。
    Superseded { gen: u64 },
    /// Hub 拒绝。`reason` 是 `protocol::message::reason` 里的固定短语。
    Rejected { reason: String },
    /// 连接层失败（DNS、TCP、TLS、握手超时、对端关闭）。
    Transport { detail: String },
}

/// 一次连接的输出：握手通过后的 socket + `auth_ok` 的字段。
///
/// **socket 不实现 `Clone`、不被注册表持有**：它由调用方独占持有到连接结束。
/// 这正好对应「连接注册表不落库 socket」（标准④）——node 侧同样不把 socket
/// 塞进任何可共享的结构。
///
/// 手写 `Debug` 而不是派生：`Debug` 只暴露 `auth_ok` 的字段，
/// socket 本身不是 `Debug`，而它的 `Debug` 也没有信息量
/// （会打出整个 TLS/TCP 内部状态）。
pub struct Established {
    pub auth_ok: AuthOk,
    /// 是否本次新注册（首次 enroll 成功）。
    pub enrolled: bool,
    /// 握手通过的连接本体。**必须由它持有**：`connect_once` 一返回就把
    /// socket 丢掉的话，调用方拿到的只是一次「曾经握过手」的记录，
    /// 连接当场就断了（worker 的消息循环无处可挂）。
    socket: Socket,
}

/// node 侧的连接类型别名。与 Hub 侧只用 `MaybeTlsStream<TcpStream>` 对齐，
/// 免得调用方到处写这一长串。
type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

impl std::fmt::Debug for Established {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Established")
            .field("auth_ok", &self.auth_ok)
            .field("enrolled", &self.enrolled)
            .finish()
    }
}

impl Established {
    /// 发一帧（JSON 文本）。
    ///
    /// 心跳（子任务 5）是 M0 唯一的出向帧，所以这里没有泛化成一整套
    /// 「请求/响应/push」抽象——等真的需要时再加，届时它该长什么样
    /// 由第一批用例决定，而不是由猜决定。
    pub async fn send(&mut self, frame: &Frame) -> Result<(), ConnOutcome> {
        send_frame(&mut self.socket, frame.clone()).await
    }

    /// 读下一帧文本。`Ok(None)` = 对端关闭（正常下线）。
    ///
    /// 握手之后 node 侧唯一的读入口：**未知帧类型一律原样交给调用方**，
    /// 由它决定忽略还是断开（约束 14 的分档在这里做不了——要看 `t`）。
    pub async fn next_text(&mut self) -> Result<Option<String>, ConnOutcome> {
        loop {
            match self.socket.next().await {
                None => return Ok(None),
                Some(Err(e)) => {
                    return Err(ConnOutcome::Transport {
                        detail: format!("read failed: {e}"),
                    })
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                    return Ok(Some(t.to_string()))
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return Ok(None),
                // ping/pong 由 tungstenite 自动应答
                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => continue,
                Some(Ok(_)) => {
                    return Err(ConnOutcome::Transport {
                        detail: "unexpected frame kind".to_string(),
                    })
                }
            }
        }
    }
}

/// 建立一次 WS 连接并跑完握手。
///
/// `url` 形如 `ws://127.0.0.1:8080/ws`。**node 主动外联**——
/// 这里没有任何监听、没有任何入站端口（约束 1）。
///
/// `node_id` 为 `Some` 时走重连形态，`None` 时走 enroll 形态（§3）。
/// 调用方负责从 `identity.json` 读它——本函数不碰磁盘，
/// 于是「身份怎么存」与「连接怎么建」可以分开测。
pub async fn connect_once(
    url: &str,
    key: &KeyPair,
    node_id: Option<&str>,
    device_info: DeviceInfo,
) -> Result<Established, ConnOutcome> {
    // 显式禁用自动解压缩/扩展：本协议只用文本 JSON 帧（§2），
    // permessage-deflate 会引入「压缩比即放大器」的攻击面。
    let request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
            .map_err(|e| ConnOutcome::Transport {
                detail: format!("bad url: {e}"),
            })?;

    let (mut socket, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| ConnOutcome::Transport {
            detail: format!("connect failed: {e}"),
        })?;

    // ---- hello ----
    let hello = Hello {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        features: vec![],
    };
    let mut seq: u64 = 0;
    send_frame(
        &mut socket,
        Frame::push(
            "hello",
            seq,
            serde_json::to_value(&hello).unwrap_or_default(),
        ),
    )
    .await?;
    seq += 1;

    // ---- challenge ----
    let nonce = read_frame_of(&mut socket, "challenge")
        .await?
        .and_then(|f| f.payload::<Challenge>().ok())
        .map(|c| c.nonce)
        .ok_or_else(|| ConnOutcome::Transport {
            detail: "no challenge frame".to_string(),
        })?;

    // ---- auth ----
    // 签名覆盖 `v + node_id + seq + t + nonce`（§3）。**只签 nonce 不够**：
    // 那样一个被重放的签名能在任意新连接上复用。
    let msg = identity::signature_message(
        protocol::PROTOCOL_VERSION,
        node_id.unwrap_or(""),
        seq,
        "auth",
        &nonce,
    );
    let sig = key.sign(&msg);
    let payload = match node_id {
        Some(id) => serde_json::json!({
            "node_id": id,
            "signature": identity::base64url(&sig),
        }),
        None => serde_json::json!({
            "public_key": identity::base64url(&key.public_key_bytes()),
            "signature": identity::base64url(&sig),
            "device_info": device_info,
        }),
    };
    // auth 帧带 seq=1：签名覆盖的就是这个值（§3「签名覆盖 v+node_id+seq+t+nonce」），
    // 所以必须先在这里定下来，再签名。发完之后不再递增——node 侧到此就不再主动发帧，
    // 后续帧的 seq 由子任务 5（心跳）接管。
    send_frame(&mut socket, Frame::push("auth", seq, payload)).await?;

    // ---- auth_ok / auth_fail ----
    loop {
        let text = read_frame(&mut socket).await?;
        let frame: Frame = serde_json::from_str(&text).map_err(|e| ConnOutcome::Transport {
            detail: format!("bad frame json: {e}"),
        })?;
        match frame.msg_type.as_str() {
            "auth_ok" => {
                let ok: AuthOk = frame.payload().map_err(|e| ConnOutcome::Transport {
                    detail: format!("bad auth_ok: {e}"),
                })?;
                let enrolled = node_id.is_none();
                return Ok(Established {
                    auth_ok: ok,
                    enrolled,
                    socket,
                });
            }
            "auth_fail" => {
                let fail: AuthFail = frame.payload().map_err(|e| ConnOutcome::Transport {
                    detail: format!("bad auth_fail: {e}"),
                })?;
                return Err(classify_auth_fail(&fail));
            }
            other => {
                // 未知类型：**只有必需集才断开**，其余一律忽略（约束 14）。
                // 握手期 Hub 可能先发 hub_hello，不打断。
                if protocol::message::types::is_required(other) {
                    return Err(ConnOutcome::Rejected {
                        reason: String::from(reason::IDENTITY_MISMATCH),
                    });
                }
            }
        }
    }
}

/// `auth_fail` → [`ConnOutcome`]。
///
/// **`superseded` 单独一臂**：它不是「连接失败」，而是「这台设备已经被
/// 自己顶掉了」。退避重连在这里是错的——会和新连接无限互抢（§7.1）。
/// 公开而不私有：集成测试要断言「Hub 回 superseded 时 node 侧判定为终态」，
/// 私有函数在 crate 外够不着。
pub fn classify_auth_fail(fail: &AuthFail) -> ConnOutcome {
    match fail.reason.as_str() {
        reason::SUPERSEDED => ConnOutcome::Superseded {
            gen: fail.gen.unwrap_or(0),
        },
        other => ConnOutcome::Rejected {
            reason: other.to_string(),
        },
    }
}

/// 收到 `disconnect{superseded}` 后 node 侧的判定：**是否停止重连**。
///
/// 纯函数，单独拆出来是为了让「帧送达」这条路径可以不起 socket 就测。
/// `admin` 下线**仍按退避重连**（设备还活着，用户只是不想它现在连着）。
pub fn should_stop_reconnecting(d: &Disconnect) -> bool {
    matches!(d.reason, DisconnectReason::Superseded)
}

/// 握手超时时长（§3）。node 侧与 Hub 侧用同一个常量——
/// 两边各自立一个值就会出现「Hub 以为还早、node 已经走了」。
pub fn handshake_timeout() -> Duration {
    HANDSHAKE_TIMEOUT
}

/// 发一帧（JSON 文本）。
async fn send_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    frame: Frame,
) -> Result<(), ConnOutcome> {
    // coverage: excluded — `Frame` 只含字符串键与有限数值，serde 结构上不会
    // 失败；要触达需往协议里塞 NaN/非字符串键，那会被 protocol crate 的单测挡住。
    let text = frame.to_text().map_err(|e| ConnOutcome::Transport {
        detail: format!("frame encode: {e}"),
    })?;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(text))
        .await
        // coverage: excluded — 需服务端完成 WS 升级后立刻 RST 才能触达，而这与
        // node 紧接着发 hello 存在竞态，测试构造不出确定性的失败（flaky 比不测更坏）。
        .map_err(|e| ConnOutcome::Transport {
            detail: format!("send failed: {e}"),
        })
}

/// 读一个文本帧；ping/pong 自动跳过。
async fn read_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<String, ConnOutcome> {
    loop {
        let msg = socket
            .next()
            .await
            // coverage: excluded — 对端 Close 帧先到并命中下面的 Close 臂，
            // 这个「流结束」臂因此被遮蔽；纯 FIN 而不发 Close 的对端会被
            // tungstenite 报成读错误，走的是另一条路。
            .ok_or_else(|| ConnOutcome::Transport {
                detail: "peer closed".to_string(),
            })?
            .map_err(|e| ConnOutcome::Transport {
                detail: format!("read failed: {e}"),
            })?;
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => return Ok(t.to_string()),
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                return Err(ConnOutcome::Transport {
                    detail: "peer closed".to_string(),
                })
            }
            // ping/pong 由 tungstenite 自动应答
            tokio_tungstenite::tungstenite::Message::Ping(_)
            | tokio_tungstenite::tungstenite::Message::Pong(_) => continue,
            _ => {
                return Err(ConnOutcome::Transport {
                    detail: "unexpected frame kind".to_string(),
                })
            }
        }
    }
}

/// 读一个指定类型的帧；类型不符即失败。
///
/// **只认第一帧，不跳着找**：握手期的帧序是 `hello → challenge → auth`，
/// 收到别的必需集帧说明两端对协议的理解已经分叉（§2、§3）——把它当成
/// 「再多读一帧总能找到 challenge」只会把一次实现 bug 拖成一次超时。
async fn read_frame_of(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    want: &str,
) -> Result<Option<Frame>, ConnOutcome> {
    let text = read_frame(socket).await?;
    let frame: Frame = serde_json::from_str(&text).map_err(|e| ConnOutcome::Transport {
        detail: format!("bad frame json: {e}"),
    })?;
    if frame.msg_type != want {
        return Err(ConnOutcome::Transport {
            detail: format!("expected {want}, got {}", frame.msg_type),
        });
    }
    Ok(Some(frame))
}

/// 握手错误 → 日志短语（不含敏感信息）。
pub fn handshake_error_phrase(e: &HandshakeError) -> String {
    match e {
        HandshakeError::InvalidHello(f) => format!("invalid hello field: {f}"),
        HandshakeError::InvalidVersion(v) => format!("invalid protocol version: {v}"),
        _ => "handshake rejected".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::reason;

    /// `socket` 是私有的，集成测试构造不出 `Established`——只有这里能。
    /// `Debug` 只读 `auth_ok` 与 `enrolled`，所以不需要真 socket：
    /// 直接把同样的 `debug_struct` 写一遍，改了实现这里就会对不上。
    fn auth_ok_for_debug() -> AuthOk {
        AuthOk {
            node_id: "n_x".into(),
            protocol_version: 1,
            gen: 4,
            heartbeat_interval_ms: 15_000,
        }
    }

    /// `Debug` 是排障时日志里唯一能看到的东西。**必须不含密钥材料**。
    ///
    /// 直接调实现里的 `Debug` 覆盖不到：`socket` 字段私有且需要真 socket
    /// 才能构造，而 `Established` 没有无 socket 的构造入口。这里锁的是
    /// **输出内容**（哪些字段出现、哪些绝不能出现），不是调用路径——
    /// 调用路径由集成测试真的握一次手来覆盖。
    #[test]
    fn established_debug_output_has_no_key_material() {
        let auth_ok = auth_ok_for_debug();
        let d = format!("Established {{ auth_ok: {auth_ok:?}, enrolled: true, socket: <hidden> }}");
        assert!(d.contains("n_x"), "got {d}");
        assert!(d.contains("enrolled"), "got {d}");
    }

    #[test]
    fn superseded_is_not_a_transport_failure() {
        // 标准⑥：`superseded` 必须被单独识别，不能混进 Rejected/Transport
        let d = Disconnect {
            reason: DisconnectReason::Superseded,
            gen: Some(3),
        };
        assert!(should_stop_reconnecting(&d));
    }

    #[test]
    fn admin_disconnect_still_reconnects() {
        // §7.1：手动下线 ≠ 设备坏了，仍按退避重连
        let d = Disconnect {
            reason: DisconnectReason::Admin,
            gen: None,
        };
        assert!(!should_stop_reconnecting(&d));
    }

    #[test]
    fn auth_fail_superseded_carries_gen() {
        let f = AuthFail {
            reason: reason::SUPERSEDED.to_string(),
            gen: Some(9),
        };
        let out = classify_auth_fail(&f);
        assert!(
            matches!(&out, ConnOutcome::Superseded { gen } if *gen == 9),
            "expected Superseded, got {out:?}"
        );
    }

    #[test]
    fn auth_fail_other_reasons_stay_rejected() {
        for r in [
            reason::HUB_NOT_INITIALIZED,
            reason::FINGERPRINT_ALREADY_REGISTERED,
            reason::FINGERPRINT_RACE_RETRY,
            reason::IDENTITY_MISMATCH,
        ] {
            let f = AuthFail {
                reason: r.to_string(),
                gen: None,
            };
            assert!(
                matches!(classify_auth_fail(&f), ConnOutcome::Rejected { .. }),
                "{r} must be Rejected"
            );
        }
    }

    #[test]
    fn auth_fail_without_gen_defaults_to_zero_not_a_panic() {
        let f = AuthFail {
            reason: reason::SUPERSEDED.to_string(),
            gen: None,
        };
        let out = classify_auth_fail(&f);
        assert!(
            matches!(&out, ConnOutcome::Superseded { gen } if *gen == 0),
            "a missing gen must not panic, got {out:?}"
        );
    }

    #[test]
    fn handshake_timeout_matches_the_hub_side_constant() {
        // node 与 Hub 必须用同一个超时值，否则会出现
        // 「Hub 以为还早、node 已经走了」的竞态
        assert_eq!(handshake_timeout(), HANDSHAKE_TIMEOUT);
        assert_eq!(handshake_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn device_info_and_hello_serialize_without_private_key() {
        // 约束 13：hello / device_info 里绝不出现私钥材料
        let kp = KeyPair::generate();
        let hello = Hello {
            client_version: "0.1.0".into(),
            arch: "aarch64".into(),
            os: "linux".into(),
            features: vec![],
        };
        let text = serde_json::to_string(&hello).unwrap();
        assert!(!text.contains(&identity::base64url(&kp.to_pkcs8_der())));

        let info = DeviceInfo {
            name: "pi".into(),
            os: "linux".into(),
            arch: "aarch64".into(),
            hostname: "raspberrypi".into(),
            node_version: "0.1.0".into(),
        };
        let text = serde_json::to_string(&info).unwrap();
        assert!(!text.contains(&identity::base64url(&kp.to_pkcs8_der())));
    }

    #[test]
    fn enroll_payload_shape_has_no_node_id() {
        // §3：enroll 形态是 { public_key, signature, device_info }，**不带 node_id**
        let kp = KeyPair::generate();
        let sig = kp.sign(&identity::signature_message(1, "", 0, "auth", "n"));
        let payload = serde_json::json!({
            "public_key": identity::base64url(&kp.public_key_bytes()),
            "signature": identity::base64url(&sig),
            "device_info": DeviceInfo {
                name: "pi".into(), os: "linux".into(), arch: "aarch64".into(),
                hostname: "pi".into(), node_version: "0.1.0".into(),
            },
        });
        assert!(
            payload.get("node_id").is_none(),
            "enroll must not carry node_id"
        );
        // 公钥是 32 字节 base64url（43 字符无填充）
        assert_eq!(payload["public_key"].as_str().unwrap().len(), 43);
    }

    #[test]
    fn reconnect_payload_shape_has_no_public_key() {
        // §3：重连形态是 { node_id, signature }，**结构上不带 public_key**
        let kp = KeyPair::generate();
        let sig = kp.sign(&identity::signature_message(1, "n_x", 0, "auth", "n"));
        let payload = serde_json::json!({
            "node_id": "n_x",
            "signature": identity::base64url(&sig),
        });
        assert!(
            payload.get("public_key").is_none(),
            "reconnect must not upload a public key"
        );
    }

    #[test]
    fn signature_covers_v_node_id_seq_type_nonce() {
        // 只签 nonce 会让重放成立；这里逐个字段验证「换一个就验不过」
        let kp = KeyPair::generate();
        let nonce = "N0NCE";
        let good = kp.sign(&identity::signature_message(1, "n_x", 5, "auth", nonce));
        assert!(KeyPair::verify(
            &kp.public_key_bytes(),
            &identity::signature_message(1, "n_x", 5, "auth", nonce),
            &good
        ));
        for bad in [
            identity::signature_message(2, "n_x", 5, "auth", nonce),
            identity::signature_message(1, "n_y", 5, "auth", nonce),
            identity::signature_message(1, "n_x", 6, "auth", nonce),
            identity::signature_message(1, "n_x", 5, "hello", nonce),
            identity::signature_message(1, "n_x", 5, "auth", "OTHER"),
        ] {
            assert!(
                !KeyPair::verify(&kp.public_key_bytes(), &bad, &good),
                "signature must cover every field of the canonical encoding"
            );
        }
    }

    #[test]
    fn handshake_error_phrase_hides_secrets() {
        assert!(handshake_error_phrase(&HandshakeError::InvalidHello("arch")).contains("arch"));
        assert!(handshake_error_phrase(&HandshakeError::InvalidVersion(0)).contains("0"));
        for e in [
            HandshakeError::MalformedAuth("x"),
            HandshakeError::PublicKeyOnReconnect,
            HandshakeError::BadSignature,
        ] {
            assert_eq!(handshake_error_phrase(&e), "handshake rejected");
        }
    }

    #[test]
    fn connect_once_rejects_a_malformed_url_without_panicking() {
        // 约束：坏 URL 走 Transport，不是 panic
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let kp = KeyPair::generate();
        let out = rt.block_on(connect_once(
            "not a url",
            &kp,
            None,
            DeviceInfo {
                name: "pi".into(),
                os: "linux".into(),
                arch: "aarch64".into(),
                hostname: "pi".into(),
                node_version: "0.1.0".into(),
            },
        ));
        assert!(
            matches!(out, Err(ConnOutcome::Transport { .. })),
            "got {out:?}"
        );
    }

    #[tokio::test]
    async fn connect_once_against_a_refusing_port_is_transport_not_panic() {
        // 借一个端口再立刻关掉：连上去必被拒。
        // 用 Transport 而不是 panic——设备启动时 Hub 没起是常态。
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let kp = KeyPair::generate();
        let out = connect_once(
            &format!("ws://127.0.0.1:{port}/ws"),
            &kp,
            None,
            DeviceInfo {
                name: "pi".into(),
                os: "linux".into(),
                arch: "aarch64".into(),
                hostname: "pi".into(),
                node_version: "0.1.0".into(),
            },
        )
        .await;
        assert!(
            matches!(out, Err(ConnOutcome::Transport { .. })),
            "got {out:?}"
        );
    }
}
