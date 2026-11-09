//! node-connector：设备侧能力实现（connector + PTY）。
//!
//! 拆成独立 crate 的理由（不是「代码多」）：
//!
//! 1. **v0.12.0 要按需加载 connector**（`PRODUCT.md` §3.6、`DECISIONS.md` D21）。
//!    按需加载**必须**靠 crate 边界——静态链接的模块做不到「用到才加载」。
//! 2. **职责本来就不同**：`channel.rs` 分派帧、`heartbeat.rs` 上报能力，
//!    它们只需要一张**注册表接口**，不需要任何具体 connector 的实现。
//!    拆开后这两处只依赖本 crate 的 `connector::{supports, registered_names, create}`。
//!
//! ── 边界 ────────────────────────────────────────────────────────────
//! 本 crate **不依赖 `node-core`**，方向是单向的：`node-core` → `node-connector`。
//! 它只依赖 `protocol`（帧类型）与标准库。
//!
//! `pty` 跟着搬过来而不是留在 `node-core`：`terminal` connector 生产使用它，
//! 而 `git` connector 的测试要用它的 `env_lock`（两者都改进程环境，
//! 各持一份 `static` 时两把锁互不互斥）。留在 `node-core` 会造成
//! `node-connector → node-core → node-connector` 的环。

pub mod connector;
pub mod pty;

/// Windows 命令行拼装（引号 / 环境块 / UTF-16）的**纯逻辑**。
///
/// 放在 crate 根而不是 `connector` 里，是因为 `pty/windows.rs` 也要用它：
/// 若留在 `connector` 下，`pty` 就得反向依赖 `connector`，而依赖方向本来
/// 是 `connector → pty`（`connector/terminal.rs` 调用 PTY）。两边都向这个
/// 中立模块依赖，方向才不打架。
///
/// `any(windows, test)`：生产上只有 Windows 编译它，但测试要在 Linux 上跑
/// ——这正是把它抽出来的全部意义（引号规则错了不该只能靠 Windows CI 发现）。
#[cfg(any(windows, test))]
pub(crate) mod win_command;

/// Windows 匿名管道可读性探测（`PeekNamedPipe`）的唯一实现。
///
/// `exec_windows` 与 `pty/windows` 两侧共用；整块只在 Windows 编译，因为它是
/// 真 FFI + `unsafe`，与 `win_command` 那段「平台无关、零 unsafe、Linux 上跑
/// 单测」的纯逻辑不同，不能并到同一个模块里。
#[cfg(windows)]
pub(crate) mod win_pipe;

pub use connector::{
    create, registered_count, registered_names, supports, BoxedConnector, Connector,
    ConnectorError, OpenReq, OutFrame,
};
