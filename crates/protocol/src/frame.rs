//! 帧信封。
//!
//! 设计要点（`docs/architecture/protocol.md` §2）：
//! - `seq` **不用于去重**：Hub 不做跨连接去重。跨连接重传由 `id` + ACK 驱动。
//! - `id` 是客户端生成的请求 id，仅请求帧有，响应原样回填；推送帧不带。
//! - `node_id` 握手成功后必带；`hello`/`challenge`/`auth` 阶段可为空。
//! - 签名必须覆盖 `v + node_id + seq + t + nonce` 的规范编码（§3）。

use serde::{Deserialize, Serialize};

/// 当前协议版本。只有**破坏性变更**才 +1（删必需字段 / 改语义）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 帧大小上限 1 MiB。超出即断开并 `err{code:"frame_too_large"}`（§2）。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// 协议版本。
    pub v: u32,
    /// 连接内单调递增。仅用于日志关联与调试，**协议语义不依赖它**。
    pub seq: u64,
    /// 消息类型。
    #[serde(rename = "t")]
    pub msg_type: String,
    /// 客户端生成的请求 id；仅请求帧有，响应原样回填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 节点 id。握手成功后必带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 载荷，按 `t` 决定 schema。
    #[serde(default)]
    pub c: serde_json::Value,
}

impl Frame {
    /// 构造一个请求帧（带 `id`）。
    pub fn request(
        msg_type: impl Into<String>,
        seq: u64,
        id: impl Into<String>,
        c: serde_json::Value,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            seq,
            msg_type: msg_type.into(),
            id: Some(id.into()),
            node_id: None,
            c,
        }
    }

    /// 构造一个推送帧（不带 `id`）。
    pub fn push(msg_type: impl Into<String>, seq: u64, c: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            seq,
            msg_type: msg_type.into(),
            id: None,
            node_id: None,
            c,
        }
    }

    pub fn with_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }

    /// 解出载荷为具体类型。
    pub fn payload<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.c.clone())
    }

    /// 序列化为文本帧。调用方负责检查 `MAX_FRAME_BYTES`。
    pub fn to_text(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// 帧是否超限。
pub fn exceeds_limit(bytes: usize) -> bool {
    bytes > MAX_FRAME_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn push_frame_omits_id_and_node_id() {
        let f = Frame::push("heartbeat", 0, json!({"node_version":"0.1.0"}));
        let text = f.to_text().unwrap();
        assert!(
            !text.contains("\"id\""),
            "push frame must not carry id: {text}"
        );
        assert!(
            !text.contains("\"node_id\""),
            "handshake frame must not carry node_id"
        );
        assert!(text.contains("\"seq\":0"));
    }

    #[test]
    fn request_frame_roundtrips_id() {
        let f = Frame::request("auth", 0, "c-7f3a", json!({"public_key":"..."}));
        let back: Frame = serde_json::from_str(&f.to_text().unwrap()).unwrap();
        assert_eq!(back.id.as_deref(), Some("c-7f3a"));
        assert_eq!(back.msg_type, "auth");
    }

    #[test]
    fn version_defaults_to_protocol_version() {
        let f = Frame::push("hello", 0, json!({}));
        assert_eq!(f.v, PROTOCOL_VERSION);
    }

    #[test]
    fn payload_deserializes() {
        use crate::message::Hello;
        let f = Frame::push(
            "hello",
            0,
            json!({"client_version":"0.1.0","arch":"aarch64","os":"linux","features":[]}),
        );
        let h: Hello = f.payload().unwrap();
        assert_eq!(h.arch, "aarch64");
    }

    #[test]
    fn frame_size_limit() {
        assert!(!exceeds_limit(MAX_FRAME_BYTES));
        assert!(exceeds_limit(MAX_FRAME_BYTES + 1));
    }

    #[test]
    fn oversized_frame_is_rejected_before_parse() {
        // 1 MiB + 1 字节的载荷必须被拒，而不是喂给 serde
        let big = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(exceeds_limit(big.len()));
    }

    #[test]
    fn with_node_id_stamps_every_frame() {
        // 鉴权后 Hub 下发的每一帧都必须带 node_id（§6：通道键是
        // (node_id, channel_id)，不是全局 channel_id）
        let f = Frame::push("heartbeat_ack", 7, json!({})).with_node_id("n_pi01");
        assert_eq!(f.node_id.as_deref(), Some("n_pi01"));
        // 序列化后仍带：只改内存字段不进 JSON 是不够的
        let text = f.to_text().unwrap();
        assert!(text.contains("\"node_id\":\"n_pi01\""), "got {text}");
        let back: Frame = serde_json::from_str(&text).unwrap();
        assert_eq!(back.node_id.as_deref(), Some("n_pi01"));
    }

    #[test]
    fn node_id_accepts_borrowed_and_owned_str() {
        // `impl Into<String>`：&str 与 String 都应可用（调用方多是 &str）
        assert_eq!(
            Frame::push("hello", 0, json!({}))
                .with_node_id("n_x")
                .node_id
                .as_deref(),
            Some("n_x")
        );
        let owned = String::from("n_y");
        assert_eq!(
            Frame::push("hello", 0, json!({}))
                .with_node_id(owned)
                .node_id
                .as_deref(),
            Some("n_y")
        );
    }
}
