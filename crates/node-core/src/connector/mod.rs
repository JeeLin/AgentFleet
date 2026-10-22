//! connector 注册表（约束 3：connector 是有类型的、可插件化的能力）。
//!
//! **按名静态分派，不是动态插件系统**：`match name { "terminal" => .. }`
//! 一个类型一个 `impl`，加一种协议就加一个 `mod` + 一个 `match` 臂。
//! 动态加载（扫目录、dlopen、`register(name, factory)`）推到 M7——
//! 现在做半套只会得到一个「看起来可扩展、实际只能加代码」的注册表，
//! 而两份会漂移的能力清单（注册表 vs 心跳上报）比动态性更值得先解决。
//!
//! **不做多态 Resource 抽象**：`Connector` 就是全部的抽象面。SSH / DB / S3
//! 作为插件加载时，是「新一个 mod + 一条 match 臂」，不是「新一个 trait 实现
//! + 所有调用点多一个分支」。
//!
//! connector **只面向页面用户与 Hub**：本地 agent 完全依赖设备上装好的
//! 工具，不经过这里（约束 3）。所以本模块不提供任何「让 agent 枚举 / 调用
//! connector」的入口。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub mod exec;
pub mod files;
pub mod terminal;

/// terminal connector 名。**注册表与心跳上报共用这批常量**（约束 14）。
pub mod names {
    pub const TERMINAL: &str = "terminal";
    pub const FILES: &str = "files";
    pub const EXEC: &str = "exec";
}

/// 一条通道的请求参数：`open_channel.params` 的 connector 私有部分。
///
/// **一个类型，所有 connector 共用**：每家的私有参数由各自的
/// [`crate::connector::Connector::open`] 自行反序列化。理由有两条：
/// - 一堆「字段按 connector 互斥」的可选字段只有一个能填对，而填错时
///   serde 不会告诉你「你把 terminal 的 cols 填给 files 了」；
/// - 通道帧反序列化是热路径上的一层间接，`params` 保持 `Value` 则这份
///   间接只出现在每家 open 的第一行。
///
/// 例外只有一个：`workspace_root` 是 files 与 exec **都**要的会话工作区根
/// （约束 3：它们绑定会话的工作区），所以它是结构体上的一等字段而不是
/// 各家 params 里的重复键。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenReq {
    /// 会话工作区根目录（Hub 从 `session.workspace_id` 查出 `(node_id, path)` 后塞进来）。
    /// 缺失按空串处理：**不猜**——一个缺根的文件面板只能看到当前目录，
    /// 而那看起来像「工作区是空的」。
    #[serde(default)]
    pub workspace_root: String,
    /// 其余 connector 私有参数，原样透传给 [`Connector::open`]。
    #[serde(flatten)]
    pub params: serde_json::Value,
}

impl OpenReq {
    pub fn new(workspace_root: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            params,
        }
    }

    /// 把私有参数解成具体类型。**出错时带上 connector 名**——同一个
    /// `params` 在不同 connector 下含义完全不同，而 serde 的错误信息
    /// 本身不含它是给谁的。
    pub fn parse<T: serde::de::DeserializeOwned>(&self, connector: &str) -> Result<T, String> {
        serde_json::from_value(self.params.clone())
            .map_err(|e| format!("bad {connector} params: {e}"))
    }

    /// 工作区根；缺失或空都算「没有工作区」。
    pub fn root(&self) -> Option<&std::path::Path> {
        if self.workspace_root.is_empty() {
            None
        } else {
            Some(std::path::Path::new(&self.workspace_root))
        }
    }
}

/// connector 回给通道的一帧。
///
/// **只有一个自由载荷字段**：schema 随 connector 走，通道层不解释
/// （`protocol.md` §4「`channel_data` 的 payload 本层只透传」）。M1 的三种
/// 分别是 terminal 的「一坨字节」、files 的「操作 + 结果」、exec 的
/// 「`exit_code` + `stdout_b64` / `stderr_b64`」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutFrame {
    pub payload: serde_json::Value,
}

impl OutFrame {
    pub fn new(payload: serde_json::Value) -> Self {
        Self { payload }
    }

    /// 若干帧拼成一帧（PTY 读回来的字节按 credit 预算切）。
    ///
    /// 单帧受协议层 1 MiB 上限约束（`protocol.md` §2），所以「攒够再发」
    /// 与「读多少发多少」都必须在这里截到上限内，而不是等 Hub 回
    /// `err{frame_too_large}`——那会把一次正常输出变成一次可见故障。
    pub fn bytes(bytes: Vec<u8>) -> Result<Self, ConnectorError> {
        if bytes.len() > protocol::frame::MAX_FRAME_BYTES {
            return Err(ConnectorError::internal("channel payload exceeds 1 MiB"));
        }
        // PTY 是**字节流**：非法 UTF-8 用替换字符兜住，不能让一帧坏字节
        // 丢掉整块输出。丢字节与换字节之间，换是可恢复的——页面少一个字符，
        // 而丢是整段输出静默消失。
        let text: String = String::from_utf8_lossy(&bytes).into_owned();
        Ok(Self::new(serde_json::json!({ "bytes": text })))
    }
}

/// 一条通道上发生的、需要变成结构化错误的东西。
///
/// 刻意**不**是 `std::io::Error` 那一族：errno 对用户没有意义，
/// 「工作区根不在了」得写成一句人话，而 `design-system.md` §7.4 要求
/// 越界**可见**又**不泄漏布局**。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorError {
    /// 工作区根不存在 / 不可用。**不给绝对路径**（§7.4）。
    #[error("workspace root is not available: {detail}")]
    WorkspaceRoot { detail: String },
    /// 路径越界。`shown` 是**相对工作区根**的可读写法，可能是 `..`、
    /// 绝对路径、或一个指向界外的链接目标——正是用户要看的那一个。
    #[error("path outside the workspace: {shown}")]
    OutsideWorkspace { shown: String },
    /// 目标不存在 / 不是那一类东西。
    #[error("{op} failed: {detail}")]
    Failed { op: String, detail: String },
    /// 参数不合法。
    #[error("invalid request: {detail}")]
    Invalid { detail: String },
    /// 内部错误（不区分细节）：照样要变成一帧 `channel_error`。
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl ConnectorError {
    /// 一句不带用户语言错误的内部错误。
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    /// 该回哪个 `channel_error.code`（§4 的 code 清单只有一份）。
    ///
    /// 「越界」映射到 `internal` 而不是 `unsupported`：`unsupported` 是
    /// 「这台设备根本没有这个 connector / 通道用不了」，而越界是**通道能用、
    /// 这次请求被策略拒绝**——混成同一档会让人以为终端不支持。
    pub fn code(&self) -> protocol::channel::ChannelErrorCode {
        use protocol::channel::ChannelErrorCode;
        match self {
            Self::OutsideWorkspace { .. } | Self::WorkspaceRoot { .. } | Self::Invalid { .. } => {
                ChannelErrorCode::Internal
            }
            Self::Failed { .. } | Self::Internal { .. } => ChannelErrorCode::Internal,
        }
    }
}

/// 一个 connector 的行为契约。
///
/// `open` 必须在 [`READY_TIMEOUT`](protocol::channel::READY_TIMEOUT) 内返回
/// （协议 §5：超时 5s，Hub 回 `channel_error{reason:"unsupported"}`），
/// 所以它只能是「建起本地资源」这种微秒级的事，不能在里面等 IO。
///
/// **这份 trait 就是契约本身**：规格正文里抄的那份签名早就与代码不一致
/// （少了 `poll`、`on_data` 的参数类型也不同），照规格实现第三个 connector
/// 会编不过。加新 connector 时以这里的签名为准。
pub trait Connector: Send {
    /// 建起通道资源。**成功即回 `channel_ready`**。
    fn open(&mut self, req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError>;

    /// 通道内收到一帧数据 → 回零到若干帧。
    fn on_data(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError>;

    /// **主动**产出待发数据，与「有没有收到东西」无关。
    ///
    /// 不做这一步的话，PTY 的输出只在**下一帧下行数据到达时**才被泵出去：
    /// 敲一次 `ls` 然后干等，输出永远不来——终端看着像坏了。
    /// 空闲的 files/exec connector 返回空即可（默认实现）。
    fn poll(&mut self) -> Result<Vec<OutFrame>, ConnectorError> {
        Ok(Vec::new())
    }

    /// 释放资源。**必须能重复调用**：断连重连与 `close_channel` 可能各来一次。
    fn close(&mut self);

    /// 这个 connector 是否**已经跑完、且不会再产出任何东西**。
    ///
    /// 通道层据此收尾：返回 `true` 时它会发一帧 `close_channel` 并释放资源。
    /// 于是「一次性命令跑完就结束」这条规格（子任务 10）由 connector 自己
    /// 声明，而不是靠页面记得去关通道。
    ///
    /// **契约**：返回 `true` 之后 `poll` 必须返回空、`on_data` 不该再被调用。
    /// 默认 `false`：多数 connector 是长驻的（terminal / files）。
    fn finished(&self) -> bool {
        false
    }
}

/// 一个 connector **实例**。（协议文档里的泛称是 `ConnectorMsg`，
/// 泛型参数在 M1 只填 `serde_json::Value`。）
pub type BoxedConnector = Box<dyn Connector>;

/// 本进程**实际注册**的 connector 名。**唯一的能力真相源**。
///
/// 用 `BTreeSet` 而不是 `Vec`：心跳上报要求「按名排序、字段稳定可 diff」
/// （`scheduling.md` §2），集合天然给出这个顺序，漏了下标比较也不会被察觉。
/// 查表时也省掉一次线性扫描——通道表的查找是每帧一次。
fn registered() -> &'static BTreeSet<&'static str> {
    static NAMES: std::sync::OnceLock<BTreeSet<&'static str>> = std::sync::OnceLock::new();
    // 与 `create` 的臂**必须同一次改**：两处不一致时下面那两条测试会红。
    // 心跳上报只认这里（子任务 11），所以**这里是「这台设备有什么能力」
    // 的唯一真相源**，不是一份可以随手加的名字清单。
    NAMES.get_or_init(|| BTreeSet::from([names::TERMINAL, names::FILES, names::EXEC]))
}

/// 某个 connector 名在这台设备上**是否真的可用**（约束 9 的硬过滤口径）。
///
/// 未知名返回 `false` 而不报错：Hub 打开一个本页不提供的 connector 时，
/// 要的是一帧 `channel_error`，不是一个 panic。
pub fn supports(name: &str) -> bool {
    registered().contains(name)
}

/// 本设备上报的 connector 能力，**按名排序**。
///
/// 心跳的 `capabilities.connectors` 只能调它。手写一份数组的话，
/// 「注册了但没上报」与「上报了但没注册」这两种撒谎迟早发生一次，
/// 而它们的后果都是路由把任务派给一台做不到的机器（约束 9）。
pub fn registered_names() -> Vec<&'static str> {
    registered().iter().copied().collect()
}

/// 已注册的 connector 数量。集成测试用它证明「注册表 → 上报」这条路
/// 真的连着：改了一边而没改另一边，这里对不上，`tests/capabilities.rs`
/// 就会红。
pub fn registered_count() -> usize {
    registered().len()
}

/// 按名建一个 connector 实例。**静态分派**（约束 3）。
///
/// 加一种协议 = 加一个 `mod` + 这里一条臂。返回值是 `Option` 而不是
/// `Result`：这件事没有会失败的输入——名字要么在表里，要么不在，
/// 而「不在」由调用方回 `channel_error{reason:"unsupported"}`（§5）。
pub fn create(name: &str) -> Option<BoxedConnector> {
    match name {
        names::TERMINAL => Some(Box::new(crate::connector::terminal::Terminal::new())),
        names::FILES => Some(Box::new(crate::connector::files::Files::new())),
        names::EXEC => Some(Box::new(crate::connector::exec::Exec::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::channel::ChannelErrorCode;

    fn frame_payload(f: &OutFrame) -> serde_json::Value {
        f.payload.clone()
    }

    /// 注册表里有的名字必须能建出实例，而「注册表导出的名字」与
    /// 「`create` 认的名字」必须是同一批——否则上报在撒谎。
    #[test]
    fn every_registered_name_creates_a_connector() {
        for name in registered_names() {
            assert!(supports(name), "{name} 在注册表里");
            assert!(create(name).is_some(), "{name} 上报了却建不出来");
        }
        assert_eq!(registered_count(), registered_names().len());
    }

    /// 反过来：建得出实例的名字也必须在注册表里，否则它做的工作
    /// 永远不会被上报（用户看不到、路由也不会派活给它）。
    #[test]
    fn a_created_connector_is_always_reported() {
        for name in [names::TERMINAL, names::FILES, names::EXEC, "ssh", "db"] {
            if create(name).is_some() {
                assert!(supports(name), "{name} 能建却没上报");
            }
        }
    }

    /// **这条测试是「不许编造能力」的形状**：M0 的心跳硬编码报着四个
    /// connector，而其中四个当时**一个都没实现**——「上报了但没实现」
    /// 比「没上报」危险得多，它会让路由把任务派给一台做不到的机器
    /// （约束 9）。所以注册表与 `create` 必须互相印证，且**只有**它们
    /// 认的名字才允许出现在心跳里（`tests/capabilities.rs` 锁住导出那侧）。
    #[test]
    fn the_registered_set_is_exactly_the_three_m1_connectors() {
        assert_eq!(
            registered_names(),
            vec![names::EXEC, names::FILES, names::TERMINAL],
            "注册表就是能力表：加/减一种 connector 时这里必须一起改"
        );
    }

    /// 未知名 = 不支持，回 `false` 而不是崩（§5 的超时拒绝走的是这条路）。
    #[test]
    fn unknown_names_are_unsupported_not_fatal() {
        for n in ["", "git", "ssh", "TERMINAL", "termina"] {
            assert!(!supports(n), "{n} 不该被支持");
            assert!(create(n).is_none(), "{n} 不该建得出 connector");
        }
    }

    /// 上报必须**排序**：同一台设备每次心跳的字段要能 diff，
    /// 而 `HashMap` 的迭代序是随机的。
    #[test]
    fn the_reported_names_are_sorted() {
        let got = registered_names();
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(got, sorted, "上报顺序必须稳定可 diff: {got:?}");
    }

    /// `workspace_root` 是一等字段（files 与 exec 都要），其余 params 透传。
    #[test]
    fn open_req_separates_the_workspace_root_from_connector_params() {
        let req = OpenReq::new("/srv/ws", serde_json::json!({"cols": 80, "rows": 24}));
        assert_eq!(req.root(), Some(std::path::Path::new("/srv/ws")));
        let probe: TerminalParamsProbe = req.parse("terminal").unwrap();
        assert_eq!(probe.cols, 80);
        // 根不该混进 connector 的私有 params：那是 files/exec 的契约，不是它们的参数
        let round: OpenReq = serde_json::from_value(serde_json::json!({
            "workspace_root": "/srv/ws",
            "cols": 80,
        }))
        .unwrap();
        assert_eq!(round.workspace_root, "/srv/ws");
        assert_eq!(round.params["cols"], 80);
    }

    /// 缺根就是「没有工作区」，**不能**退化成当前目录——那看起来像
    /// 「工作区是空的」，用户会去删文件而不是去检查会话配置。
    #[test]
    fn a_missing_workspace_root_reads_as_absent_not_as_cwd() {
        let empty = OpenReq::default();
        assert_eq!(empty.root(), None);
        let explicit = OpenReq::new("/srv/ws", serde_json::json!({}));
        assert!(explicit.root().is_some());
    }

    /// 参数坏了要说清**是哪个 connector 的参数坏了**：同一份 `params`
    /// 在 terminal 与 files 下含义完全不同。
    #[test]
    fn bad_params_name_the_connector_they_belong_to() {
        let req = OpenReq::new("/w", serde_json::json!({"cols": "wide"}));
        let err = req
            .parse::<TerminalParamsProbe>("terminal")
            .expect_err("cols must be a number");
        assert!(err.starts_with("bad terminal params:"), "got {err}");
    }

    /// 测试用的探针：一个带必填数字字段的最小结构。
    #[derive(Debug, serde::Deserialize)]
    struct TerminalParamsProbe {
        cols: u16,
    }

    /// PTY 字节必须整体透传：非法 UTF-8 换字符而不是丢块。
    #[test]
    fn byte_frames_survive_invalid_utf8() {
        let f = OutFrame::bytes(vec![b'a', 0xff, b'b']).unwrap();
        assert_eq!(frame_payload(&f)["bytes"], "a\u{fffd}b");
    }

    /// 一帧不能超过协议上限：PTY 一次读回来的量由 credit 预算控制，
    /// 而 credit 预算本身可能大于 1 MiB（Hub 可以给很大的 `credit_start`）。
    #[test]
    fn an_oversized_byte_frame_is_refused_before_it_is_sent() {
        let big = vec![b'x'; protocol::frame::MAX_FRAME_BYTES + 1];
        assert!(OutFrame::bytes(big).is_err());
        let exact = vec![b'x'; protocol::frame::MAX_FRAME_BYTES];
        assert!(OutFrame::bytes(exact).is_ok());
    }

    /// 空闲 connector 的默认契约：`poll` 空、`finished` 永假。
    ///
    /// files 与 exec 都不覆写这两条（前者等请求，后者靠 `on_data` 推着跑），
    /// 所以走的是这里的默认实现。契约的要点是 `poll` **不报错**：通道层
    /// 每轮都会调它，一次 `Err` 会把整条通道拆掉，而「现在没东西要发」
    /// 不是错误。
    #[test]
    fn an_idle_connector_polls_empty_and_never_finishes() {
        let mut files = create(names::FILES).expect("files 建得出来");
        assert!(
            files
                .poll()
                .expect("空闲 connector 的 poll 不该报错")
                .is_empty(),
            "空闲的 files 没有待发帧"
        );
        assert!(!files.finished(), "files 是长驻的：不能自称跑完");

        let mut exec = create(names::EXEC).expect("exec 建得出来");
        assert!(
            exec.poll()
                .expect("空闲 connector 的 poll 不该报错")
                .is_empty(),
            "空闲的 exec 没有待发帧"
        );
        assert!(!exec.finished(), "exec 未开始时不能自称跑完");
    }

    /// 越界与「这台设备不支持」是两回事，混成同一档会让人以为终端坏了。
    #[test]
    fn boundary_violations_are_not_reported_as_unsupported() {
        let e = ConnectorError::OutsideWorkspace {
            shown: "../etc".into(),
        };
        assert_eq!(e.code(), ChannelErrorCode::Internal);
        assert!(e.to_string().contains("../etc"), "越界要看得见");
        for e in [
            ConnectorError::Invalid { detail: "x".into() },
            ConnectorError::Failed {
                op: "read".into(),
                detail: "x".into(),
            },
            ConnectorError::internal("x"),
        ] {
            assert_eq!(e.code(), ChannelErrorCode::Internal);
        }
    }
}
