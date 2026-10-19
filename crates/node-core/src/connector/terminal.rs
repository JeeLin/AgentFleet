//! terminal connector：真 PTY（子任务 8）。
//!
//! 第 7 子任务只建**骨架**：真正的实现在子任务 8。
//! 在那之前 `open` 一律返回一句人话（而不是假装成功）——「打开失败并说清
//! 为什么」与「打开成功但什么都不做」必须选前者，否则用户看到的是一块
//! 黑屏，而不是一句「这个 connector 还没做」。

use super::{Connector, ConnectorError, OpenReq, OutFrame};

/// Terminal connector。M0 阶段没有会话工作区，所以没有 root。
#[derive(Debug, Default)]
pub struct Terminal;

impl Terminal {
    pub fn new() -> Self {
        Self
    }
}

impl Connector for Terminal {
    fn open(&mut self, _req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError> {
        Err(ConnectorError::Invalid {
            detail: "Terminal connector is not implemented in this build".to_string(),
        })
    }

    fn on_data(&mut self, _data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        Err(ConnectorError::Invalid {
            detail: "Terminal channel is not open".to_string(),
        })
    }

    fn close(&mut self) {}
}
