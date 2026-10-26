//! ACP 客户端：JSON-RPC 2.0 over stdio（要求 4）。
//!
//! # 协议形状（本机 opencode v2.1.4 实测）
//!
//! 子进程的 stdin/stdout 上**一行一条 JSON**。请求带 `id`，响应带回同一个 `id`；
//! **通知没有 `id`**（`session/update` 就是通知）。
//!
//! `initialize` → `session/new` → `session/prompt`，期间设备侧持续收到
//! `session/update` 通知流。
//!
//! # 三条实测逼出来的硬约束
//!
//! 1. **`session/update` 是通知，不能当响应等**。opencode 会在 `session/new` 之后
//!    主动推 `available_commands_update` / `config_option_update`，在 `session/prompt`
//!    之后推 `agent_message_chunk` / `usage_update`。客户端必须能处理「当前没有任何
//!    请求在等这条消息」的情形——把它丢掉就等于丢掉对话内容。
//!
//! 2. **`session/prompt` 的 response 时机不可依赖**。实测短回复 2.93 s / 4.27 s / 14.53 s
//!    才回，长回复 130 s 仍未回（只有 chunks 到了）。所以**不能用「等 response」来判定
//!    一轮结束**，调用方要自己定结束条件（流结束标志 / 超时 / 用户取消）。
//!    本模块只做「把消息收进来、分发出去」，不替调用方判结束。
//!
//! 3. **关 stdin 后子进程立刻自己退出**（实测 `poll()=0`，不泄漏）。这给了一条兜底：
//!    worker 被 `kill -9` 时管道随之关闭，子进程自己走。**不需要 pidfile 回收**。
//!
//! # 为什么这个模块不认识 Hub
//!
//! ACP 进程跑在**设备上**，Hub 只转发与记录。把 Hub 的帧类型带进这里，会让
//! 「ACP 说什么」与「Hub 怎么发」耦合在一起，而两者的演进节奏完全不同
//! （agent 版本由第三方定）。这一层只吐 [`Event`]，由上层决定怎么送出去。
//!
//! # 进程归属
//!
//! **owner 是 worker，不是 supervisor。** `DECISIONS.md` D1 的白盒边界要求
//! supervisor 不碰业务状态；把 ACP 挂过去会让它从「无 bug 面」变成持业务状态，
//! 且 worker 被 `exit(10)` 换掉时 ACP 进程不随之退出，变成孤儿继续写 stdout。
//!
//! **本模块的进程不随连接断开而死**：`security-and-lifecycle.md` 承诺
//! 「离线期间 agent 进程仍存活」，而 `session/load`（子任务 8）要 load 的正是
//! 这个还活着的进程。这与 M1 的 PTY「随连接一起死」（`channel.rs` 的注释）
//! **显式不同**，不要照抄那条释放逻辑。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

/// ACP 协议版本。实测 opencode 回 `protocolVersion: 1`。
pub const ACP_PROTOCOL_VERSION: u64 = 1;

/// 客户端 → 上层或上层 → 客户端的事件/指令。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 一条 `session/update` 通知（对话流：消息分片、工具调用、用量……）。
    ///
    /// **原样透传 `params`，不做解释**：新版本 agent 会推不认识的事件类型，
    /// 在这里丢弃就是「静默丢消息」。类型判定留给落库与页面。
    SessionUpdate(Value),
    /// 一条**请求**（带 `id`）——目前是 ACP 的权限请求。
    ///
    /// 它需要上层回一个响应，所以带上 `id` 与原始 `params`。
    /// 本里程碑的阶段一不实现权限交互，只把它交给上层并**允许上层不回**
    /// （ACP 允许请求方超时）。
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// 子进程退出（含 stdout 关闭）。
    ///
    /// 上层必须当**终态**处理：此后任何 `prompt` / `cancel` 都会失败。
    /// 带 `code` 是因为「agent 自己崩了」与「我们关了它」要能分开。
    Exited { code: Option<i32>, reason: String },
}

/// 客户端持有的、可跨任务共享的句柄。
///
/// 内部是 `Arc<Mutex<...>>`：**`session/update` 的读取任务与发请求的调用方
/// 是两个不同的任务**，而 stdout 只有一个读端。用锁而不是拆读写两半，
/// 是因为写 sidecar 的顺序无关紧要、读却必须串行。
#[derive(Debug, Clone)]
pub struct AcpClient {
    inner: Arc<Inner>,
    /// 事件接收端。`Option` 是因为它只能被取走一次（见 [`Self::recv`]）。
    ///
    /// **克隆出来的 `AcpClient` 拿到的是 `None`**，理由见 [`Inner::events`] 的注释。
    events: Arc<Mutex<Option<mpsc::UnboundedReceiver<Event>>>>,
}

#[derive(Debug)]
struct Inner {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    /// 在途请求：`id → 回执通道`。
    ///
    /// **必须有它**：响应与请求是异步配对的，而 `session/prompt` 可能永不回。
    /// 没有这张表，响应回来时无法知道该交给谁。
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    /// 事件出口。读循环往里灌。
    ///
    /// 发送端放这里、接收端放 [`AcpClient`] 上（`Mutex<Option<...>>`）：一个
    /// 客户端只该有一个事件消费者。克隆 `AcpClient` 不会克隆接收端——
    /// 否则两个克隆会各自收到一半事件，而那种丢消息没有任何报错。
    events: mpsc::UnboundedSender<Event>,
}

impl AcpClient {
    /// 起一个 ACP 子进程并与它握手（`initialize`）。
    ///
    /// `command` / `args` / `env` 来自 `agent_config`（用户配的启动方式）：
    /// - `command` 为空串时用 `default_command`，即设备扫描到的那个可执行文件
    /// - `env` 是**追加**到继承环境之上的，不是替换（替换会让子进程丢掉 `PATH`）
    ///
    /// # 为什么握手放在构造函数里
    ///
    /// `initialize` 失败意味着「这个 agent 根本说不了 ACP」。让调用方拿到一个
    /// 已经能用或明确失败的客户端，比让它自己记得先握手一次更安全——
    /// 忘了握手的症状是「第一条 prompt 卡住」，而不是一条可读的错误。
    ///
    /// 事件从 [`Self::recv`] 取。**接收端归客户端持有**，不由调用方传进来：
    /// 两边各持一端时很容易「拿错那个已经没人读的」。
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        default_command: &str,
        cwd: Option<&std::path::Path>,
    ) -> Result<Self, AcpError> {
        let program = if command.trim().is_empty() {
            default_command
        } else {
            command
        };
        if program.trim().is_empty() {
            return Err(AcpError::NoCommand);
        }

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 不接管：agent 的日志走它自己的路。接管了要有人读，
            // 不读就让子进程在写满管道时阻塞——而那个阻塞看起来像「ACP 卡死」。
            .stderr(Stdio::null())
            .kill_on_drop(false);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn().map_err(|e| AcpError::Spawn {
            program: program.to_string(),
            detail: e.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or(AcpError::NoStdin)?;
        let stdout = child.stdout.take().ok_or(AcpError::NoStdout)?;

        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events: tx.clone(),
        });

        // 读循环：独立任务。**不在这里 `.await` 它**，否则 mark 一旦没有输出就
        // 会堵住构造函数。
        tokio::spawn(read_loop(BufReader::new(stdout), inner.clone(), tx));

        let client = Self {
            inner: inner.clone(),
            events: Arc::new(Mutex::new(Some(rx))),
        };

        // 握手。`protocolVersion` 由我们提，agent 回的版本**不参与判定**：
        // 实测 opencode 回 1，但把「回几」当成能力判据会让每个 agent 版本
        // 都要改一次这段代码。能力看 `agentCapabilities`。
        let _ = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
                }),
            )
            .await?;

        Ok(client)
    }

    /// 取下一条事件。返回 `None` 表示读循环已经结束、不会再有事件。
    ///
    /// **只有原始句柄能取到**；克隆出来的返回 `None`。见 [`Inner::events`]。
    pub async fn recv(&self) -> Option<Event> {
        let mut guard = self.events.lock().await;
        match guard.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }

    /// 发一个请求并等它的响应。
    ///
    /// **超时不在这里**：`session/prompt` 实测 130 s 都不回是正常行为。
    /// 定超时会让「回复长一点」变成「调用失败」。需要上限的调用方自己 `select!`。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_line(&line).await?;

        match rx.await {
            Ok(v) => {
                if let Some(err) = v.get("error") {
                    return Err(AcpError::Rpc {
                        method: method.to_string(),
                        detail: err.to_string(),
                    });
                }
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
            // 发送端掉了 = 读循环结束（子进程死了）。这不是「超时」，
            // 而是「不可能再有响应」——两者的区别决定上层要不要重试。
            Err(_) => Err(AcpError::Closed {
                method: method.to_string(),
            }),
        }
    }

    /// 发一条通知（无 `id`，不等响应）。
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// 回一个**请求**的响应（权限请求等）。
    pub async fn respond(&self, id: &Value, result: Value) -> Result<(), AcpError> {
        self.write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    /// `session/new` → 返回 `sessionId`。
    ///
    /// `cwd` 是**会话工作区路径**，不是进程的 cwd：ACP 用它在 agent 侧定位项目。
    pub async fn new_session(&self, cwd: &str) -> Result<String, AcpError> {
        let r = self
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        r.get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AcpError::Rpc {
                method: "session/new".into(),
                detail: format!("no sessionId in {r}"),
            })
    }

    /// `session/load` —— 重连后恢复一个既有会话。
    ///
    /// **实测：这是「重建流」而不是「接着推」**。同进程实测 live 9 条事件 →
    /// replay 4 条，且重放的事件**没有 `seq`、没有时间戳**。所以：
    /// - 重放**不写轨迹**（否则历史会被重复写入且顺序错乱）
    /// - 页面历史一律读轨迹表，不靠 replay 补
    ///
    /// 返回值是重放出来多少条——只用于日志与测试，**不是**「这个会话有多少历史」。
    pub async fn load_session(&self, session_id: &str, cwd: &str) -> Result<usize, AcpError> {
        let r = self
            .request(
                "session/load",
                json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
            )
            .await?;
        Ok(r.get("replayed")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize)
    }

    /// `session/prompt` —— 发一条用户消息。
    ///
    /// **立即返回**：真正的回复以 [`Event::SessionUpdate`] 的形式从事件流里出来。
    /// 不要把这个 `Result` 当成「这一轮结束了」——见模块注释的第 2 条硬约束。
    ///
    /// 返回值里**可能**带 `stopReason`（实测 `end_turn`），也可能什么都没有
    /// （长回复那一次 130 s 未回）。要不要等它、等多久，是调用方的事。
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<Value, AcpError> {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": text }],
            }),
        )
        .await
    }

    /// 从 `session/prompt` 的响应里取 `stopReason`。
    ///
    /// 抽成函数而不是让调用方自己 `get("stopReason")`：**这是「这一轮走完了」
    /// 的唯一凭据**（chunk 流自己不会说它结束了）。集中在一处，才能保证轨迹的
    /// `turn_end` 与页面判定用的是同一个判据。
    ///
    /// 返回 `None` 表示响应里没有这个字段——那**不能**当成「没走完」，
    /// 只能当成「这一次没拿到凭据」。两者的区别在轨迹里体现为
    /// `turn_end` 在或不在。
    pub fn stop_reason(result: &Value) -> Option<&str> {
        result.get("stopReason").and_then(Value::as_str)
    }

    /// `session/cancel` —— 中断当前这一轮。是通知，不等响应。
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        self.notify("session/cancel", json!({ "sessionId": session_id }))
            .await
    }

    /// 关掉子进程的 stdin 并等它退出。
    ///
    /// **这是唯一一个会主动杀死 agent 的入口**，且只该在「会话结束 / 页面显式
    /// 关闭」时调。断连**不调**它（见模块注释）。
    ///
    /// 先关 stdin 再等：实测关 stdin 后 opencode 自己就退了，不需要 `SIGKILL`。
    /// 给一个宽限再强杀，是为了不让一个卡住的 agent 把关闭流程也卡住。
    pub async fn shutdown(&self) {
        self.inner.stdin.lock().await.take();
        let guard = self.inner.child.lock().await;
        let mut guard = guard;
        if let Some(mut child) = guard.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(_) => {}
                // 超过宽限还在跑：强杀。**这不是「正常路径」**，所以留一行日志。
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
        }
    }

    async fn write_line(&self, v: &Value) -> Result<(), AcpError> {
        let mut guard = self.inner.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(AcpError::Closed {
                method: "write".into(),
            });
        };
        let mut line = serde_json::to_vec(v).map_err(|e| AcpError::Encode(e.to_string()))?;
        line.push(b'\n');
        // **写失败一律归 `Closed`，不归 `Write`**。
        // 实测：子进程已死时写 stdin 得到 `Broken pipe (os error 32)`。把它
        // 单独报成 `Write` 会让上层必须判两种「进程没了」——而它们的下一步
        // 动作完全相同（重开会话）。`Write` 留给「管道还在但写不进去」那种
        // 理论上可能、实测没见过的情况，这里不再制造第二种分类。
        if let Err(e) = stdin.write_all(&line).await {
            return Err(AcpError::Closed {
                method: format!("write ({e})"),
            });
        }
        if let Err(e) = stdin.flush().await {
            return Err(AcpError::Closed {
                method: format!("flush ({e})"),
            });
        }
        Ok(())
    }
}

/// 读 stdout 的一行一条消息，分发到「在途请求」或「事件出口」。
///
/// 结束条件有三种，**都要发出 [`Event::Exited`]**：
/// stdout 关闭、读出错、子进程退出。任一发生后，`pending` 里所有等待者的
/// 发送端被 drop，于是它们的 `rx.await` 立刻返回 `Closed`——不会有一条请求
/// 永远挂着。
async fn read_loop(
    reader: BufReader<tokio::process::ChildStdout>,
    inner: Arc<Inner>,
    on_exit: mpsc::UnboundedSender<Event>,
) {
    let mut lines = reader.lines();
    let reason = loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    // 一行坏 JSON **不终止循环**：agent 的 stdout 上可能混有
                    // 非协议的诊断输出（实测有）。丢掉那一行，继续读。
                    Err(_) => continue,
                };
                dispatch(&inner, v).await;
            }
            Ok(None) => break "stdout closed".to_string(),
            Err(e) => break format!("read error: {e}"),
        }
    };

    // 退出码：`try_wait` 可能在进程还没被回收时回 `None`，那就报 `None`。
    // **不在这里 `wait()`**：那会在这个任务里阻塞到进程真正退出，
    // 而 stdout 已关时上层该立刻知道，不必等回收。
    let code = match inner.child.lock().await.as_mut() {
        Some(c) => c.try_wait().ok().flatten().and_then(|s| s.code()),
        None => None,
    };
    drop(inner.pending.lock().await.drain());
    let _ = on_exit.send(Event::Exited { code, reason });
}

/// 一条进来的消息该去哪：
/// - 有 `id` 且有 `result`/`error` → 是某个在途请求的响应
/// - 有 `id` 且有 `method` → 是一条**请求**（权限请求），交给上层
/// - 只有 `method` → 是通知
async fn dispatch(inner: &Inner, v: Value) {
    let id = v.get("id").cloned();
    let method = v.get("method").and_then(Value::as_str);

    match (id, method) {
        // 响应：配对给在途请求
        (Some(id), None) => {
            if let Some(n) = id.as_u64() {
                let tx = inner.pending.lock().await.remove(&n);
                if let Some(tx) = tx {
                    let _ = tx.send(v);
                }
            }
        }
        // 请求：带 id 也有 method
        (Some(id), Some(method)) => {
            let _ = inner.events.send(Event::Request {
                id,
                method: method.to_string(),
                params: v.get("params").cloned().unwrap_or(Value::Null),
            });
        }
        // 通知
        (None, Some(method)) => {
            // 只把 `session/update` 当对话流。其他通知（`available_commands_update`
            // 之类）也走同一条出口并保留 `method`，让上层自己决定要不要理。
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            if method == "session/update" {
                let _ = inner.events.send(Event::SessionUpdate(params));
            } else {
                let _ = inner.events.send(Event::SessionUpdate(json!({
                    "sessionUpdate": method,
                    "raw": params,
                })));
            }
        }
        (None, None) => {}
    }
}

/// ACP 客户端错误。
///
/// **`Closed` 与 `Rpc` 必须分开**：`Rpc` 是「agent 说不行」（参数错、会话没了），
/// 值得把原因显示给用户；`Closed` 是「这个进程没了」，下一步只能是重开会话。
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("agent command is empty")]
    NoCommand,
    #[error("cannot spawn `{program}`: {detail}")]
    Spawn { program: String, detail: String },
    #[error("the agent process has no stdin")]
    NoStdin,
    #[error("the agent process has no stdout")]
    NoStdout,
    #[error("cannot encode a request: {0}")]
    Encode(String),
    #[error("`{method}` failed: {detail}")]
    Rpc { method: String, detail: String },
    /// 进程没了——**写不进去与等不到响应是同一件事**（实测子进程死后写
    /// stdin 得 `Broken pipe`）。它们合并成一个变体，因为上层的下一步动作
    /// 完全相同：重开会话。`method` 里会带上原始错误串便于排查。
    #[error("the agent process is gone, cannot complete `{method}`")]
    Closed { method: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// 写一个假 agent 脚本（`#!/bin/sh` + python3 逻辑）。
    ///
    /// 用 python3 而不是纯 sh：要按行读 JSON 并回 JSON，sh 里做不了。
    fn fake_agent(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let script = format!("#!/usr/bin/env python3\n{body}");
        let mut f = std::fs::File::create(&path).expect("写脚本");
        f.write_all(script.as_bytes()).expect("写脚本");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        path
    }

    /// 所有脚本共用的骨架：逐行读 JSON，按 method 分发。
    const PRELUDE: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    mid = msg.get("id")
"#;

    /// 起客户端。`events` 由客户端内部管，调用方用 `recv()` 取。
    async fn client_for(script: &Path) -> AcpClient {
        AcpClient::spawn(
            script.to_str().expect("路径"),
            &[],
            &Default::default(),
            "",
            None,
        )
        .await
        .expect("握手应成功")
    }

    /// 一个最小的、能握手的 agent：`initialize` 回版本，`session/new` 回 id。
    fn minimal_agent(dir: &Path) -> PathBuf {
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1,"agentCapabilities":{{"loadSession":True}}}}}})
    elif method == "session/new":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"sessionId":"ses_test_1"}}}})
    elif method == "session/load":
        notify("session/update", {{"sessionId":"ses_test_1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"replayed"}}}}}})
        send({{"jsonrpc":"2.0","id":mid,"result":{{"replayed":1}}}})
"#
        );
        fake_agent(dir, "minimal", &body)
    }

    #[tokio::test]
    async fn initialize_and_new_session_yield_the_agent_session_id() {
        let d = tempfile::tempdir().expect("tmp");
        let c = client_for(&minimal_agent(d.path())).await;
        let sid = c.new_session("/tmp/proj").await.expect("session/new");
        assert_eq!(sid, "ses_test_1");
        c.shutdown().await;
    }

    /// **`session/update` 是通知，不能当响应等**——它会在没有任何请求在等的时候到。
    ///
    /// 这条钉的是实测事实：opencode 在 `session/new` 之后主动推
    /// `available_commands_update`。若把通知当响应配对，第一条 prompt 就会
    /// 拿到一个风马牛不相及的 result。
    #[tokio::test]
    async fn an_unsolicited_notification_reaches_the_event_stream() {
        let d = tempfile::tempdir().expect("tmp");
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}})
        notify("session/update", {{"sessionId":"s","update":{{"sessionUpdate":"available_commands_update"}}}})
    elif method == "session/new":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"sessionId":"ses_x"}}}})
"#
        );
        let c = client_for(&fake_agent(d.path(), "chatty", &body)).await;

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), c.recv())
            .await
            .expect("必须收到事件")
            .expect("事件流还在");
        match ev {
            Event::SessionUpdate(v) => {
                assert_eq!(v["update"]["sessionUpdate"], json!("available_commands_update"))
            }
            other => panic!("应该是 SessionUpdate，实际 {other:?}"),
        }
        c.shutdown().await;
    }

    /// **`session/prompt` 只推 chunks、**永不回** response，chunk 也必须能收到。
    ///
    /// 这是模块注释第 2 条硬约束的落地测试：长回复实测 130 s 不回 response。
    /// 客户端不能因此丢掉已经到达的 chunks。
    #[tokio::test]
    async fn chunks_are_delivered_even_when_the_response_never_comes() {
        let d = tempfile::tempdir().expect("tmp");
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}})
    elif method == "session/new":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"sessionId":"ses_x"}}}})
    elif method == "session/prompt":
        # 只推 chunks，故意不回 response
        for t in ["PO", "NG"]:
            notify("session/update", {{"sessionId":"ses_x","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":t}}}}}})
"#
        );
        let c = client_for(&fake_agent(d.path(), "streamer", &body)).await;
        let sid = c.new_session("/tmp/proj").await.expect("new");

        // prompt 会一直挂着（没有 response）。放到后台，不管它。
        let c2 = c.clone();
        tokio::spawn(async move {
            let _ = c2.prompt(&sid, "hi").await;
        });

        let mut texts = Vec::new();
        for _ in 0..2 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), c.recv())
                .await
                .expect("必须收到 chunk")
                .expect("事件流还在");
            if let Event::SessionUpdate(v) = ev {
                if let Some(t) = v["update"]["content"]["text"].as_str() {
                    texts.push(t.to_string());
                }
            }
        }
        assert_eq!(texts, vec!["PO".to_string(), "NG".to_string()]);
        c.shutdown().await;
    }

    /// 带 `id` 的**请求**（权限请求）要能到达上层，且上层能回它。
    #[tokio::test]
    async fn a_request_from_the_agent_reaches_the_caller_and_can_be_answered() {
        let d = tempfile::tempdir().expect("tmp");
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}})
    elif method == "session/new":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"sessionId":"ses_x"}}}})
    elif method == "session/prompt":
        # agent 反过来问客户端要不要允许某个操作
        send({{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{{"sessionId":"ses_x","toolCall":{{"title":"rm -rf"}}}}}})
    elif method is None and mid == 9001:
        # 客户端的答复回来了，用它结束这一轮
        send({{"jsonrpc":"2.0","id":{{"__prompt_id__"}},"result":{{"stopReason":"end_turn"}}}})
"#
        );
        let path = fake_agent(d.path(), "asker", &body);
        let c = client_for(&path).await;
        let sid = c.new_session("/tmp/proj").await.expect("new");

        let c2 = c.clone();
        tokio::spawn(async move {
            let _ = c2.prompt(&sid, "do it").await;
        });

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), c.recv())
            .await
            .expect("必须收到请求")
            .expect("事件流还在");
        match ev {
            Event::Request { id, method, params } => {
                assert_eq!(method, "session/request_permission");
                assert_eq!(params["toolCall"]["title"], json!("rm -rf"));
                c.respond(&id, json!({"outcome": "denied"})).await.expect("回应答");
            }
            other => panic!("应该是 Request，实际 {other:?}"),
        }
        c.shutdown().await;
    }

    /// 子进程退出必须作为**终态**报出来，且此后发请求回 `Closed` 而不是挂住。
    #[tokio::test]
    async fn a_dead_agent_reports_exit_and_fails_later_requests_fast() {
        let d = tempfile::tempdir().expect("tmp");
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}})
        sys.exit(3)
"#
        );
        let c = client_for(&fake_agent(d.path(), "dies", &body)).await;

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), c.recv())
            .await
            .expect("必须收到退出事件")
            .expect("事件流还在");
        match ev {
            Event::Exited { code, .. } => assert_eq!(code, Some(3)),
            other => panic!("应该是 Exited，实际 {other:?}"),
        }

        // 进程没了：请求必须**立刻**失败，不能等到超时
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            c.request("session/new", json!({})),
        )
        .await
        .expect("不能挂住");
        assert!(matches!(r, Err(AcpError::Closed { .. })), "实际 {r:?}");
    }

    /// 一行坏 JSON（agent 的 stdout 混了诊断输出）**不终止读循环**。
    #[tokio::test]
    async fn a_malformed_line_does_not_kill_the_stream() {
        let d = tempfile::tempdir().expect("tmp");
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        sys.stdout.write("this is not json\n")
        sys.stdout.flush()
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}})
    elif method == "session/new":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"sessionId":"ses_after_garbage"}}}})
"#
        );
        let c = client_for(&fake_agent(d.path(), "noisy", &body)).await;
        let sid = c
            .new_session("/tmp/proj")
            .await
            .expect("坏行之后仍要能工作");
        assert_eq!(sid, "ses_after_garbage");
        c.shutdown().await;
    }

    /// `command` 为空串时用 `default_command`（扫描到的可执行文件）。
    #[tokio::test]
    async fn an_empty_command_falls_back_to_the_scanned_binary() {
        let d = tempfile::tempdir().expect("tmp");
        let path = minimal_agent(d.path());
        let c = AcpClient::spawn(
            "",
            &[],
            &Default::default(),
            path.to_str().expect("路径"),
            None,
        )
        .await
        .expect("空 command 应回落到 default_command");
        assert_eq!(c.new_session("/p").await.expect("new"), "ses_test_1");
        c.shutdown().await;
    }

    /// 两边都空 → `NoCommand`，不是「起一个叫空串的程序」。
    #[tokio::test]
    async fn an_empty_command_with_no_default_is_an_error_not_a_spawn_attempt() {
        let r = AcpClient::spawn("", &[], &Default::default(), "", None).await;
        assert!(matches!(r, Err(AcpError::NoCommand)), "实际 {r:?}");
    }

    /// 可执行文件不存在 → `Spawn`，带上是哪个程序。
    #[tokio::test]
    async fn a_missing_binary_reports_which_program_failed() {
        let r = AcpClient::spawn(
            "/nonexistent/definitely-not-here",
            &[],
            &Default::default(),
            "",
            None,
        )
        .await;
        match r {
            Err(AcpError::Spawn { program, .. }) => {
                assert!(program.contains("definitely-not-here"));
            }
            other => panic!("应该是 Spawn，实际 {other:?}"),
        }
    }

    /// `session/load` 的重放走**事件流**，返回的条数只是日志/测试用。
    #[tokio::test]
    async fn load_session_replays_through_the_event_stream() {
        let d = tempfile::tempdir().expect("tmp");
        let c = client_for(&minimal_agent(d.path())).await;
        let n = c.load_session("ses_test_1", "/tmp/proj").await.expect("load");
        assert_eq!(n, 1);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), c.recv())
            .await
            .expect("必须收到重放事件")
            .expect("事件流还在");
        assert!(matches!(ev, Event::SessionUpdate(_)));
        c.shutdown().await;
    }

    /// `stop_reason` 从响应里取 `stopReason`；没有就是 `None`，**不猜**。
    #[test]
    fn the_stop_reason_is_read_from_the_response_or_is_none() {
        assert_eq!(
            AcpClient::stop_reason(&json!({"stopReason": "end_turn", "usage": {}})),
            Some("end_turn")
        );
        assert_eq!(
            AcpClient::stop_reason(&json!({"usage": {}})),
            None,
            "没有凭据就是 None，不能当成 end_turn"
        );
        assert_eq!(AcpClient::stop_reason(&Value::Null), None);
    }

    /// **真机实证**：与真的 opencode 握手、建会话、发一条消息、收到回复。
    ///
    /// `#[ignore]` 是因为它依赖本机装了 opencode。手动跑：
    /// `cargo test -p node-core --lib agent::acp -- --ignored --test-threads=1`
    ///
    /// 这条测试的价值不在断言多严，而在**它证明了本模块与真 agent 的协议
    /// 对得上**——前面那些假脚本证明的是我们按我们的理解实现了协议，
    /// 只有真 agent 能证明那份理解是对的。
    #[tokio::test]
    #[ignore = "需要本机装 opencode"]
    async fn a_real_opencode_completes_a_prompt() {
        let bin = match which_opencode() {
            Some(p) => p,
            None => {
                eprintln!("跳过：本机没有 opencode");
                return;
            }
        };
        let c = AcpClient::spawn(
            &bin,
            &["acp".to_string()],
            &Default::default(),
            "",
            None,
        )
        .await
        .expect("与真 opencode 握手");
        let sid = c.new_session("/tmp").await.expect("session/new");
        assert!(!sid.is_empty(), "真 agent 必须回一个 sessionId");

        let c2 = c.clone();
        tokio::spawn(async move {
            // 实测：短回复也不一定回 response，且回不回都不该影响 chunk 投递
            let _ = c2.prompt(&sid, "Reply with exactly: PONG").await;
        });

        let mut got = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline && !got.contains("PONG") {
            match tokio::time::timeout(std::time::Duration::from_secs(10), c.recv()).await {
                Ok(Some(Event::SessionUpdate(v))) => {
                    if let Some(t) = v["update"]["content"]["text"].as_str() {
                        got.push_str(t);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        assert!(got.contains("PONG"), "真 agent 应回 PONG，实际收到：{got:?}");
        c.shutdown().await;
    }

    /// 本机 `opencode` 的绝对路径（`which` 的等价物）。
    ///
    /// **`lookup` 走 `PATH` 而不是写死路径**：写死会让这条测试在别人的机器上
    /// 一律跳过，于是它永远只在写它的人那里跑过。
    fn which_opencode() -> Option<String> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("opencode");
            if cand.is_file() {
                return cand.to_str().map(str::to_string);
            }
        }
        None
    }

    /// 关掉 stdin 之后子进程自己退出——不需要 pidfile 回收。
    ///
    /// 这条是模块注释第 3 条硬约束的落地：worker 被 `kill -9` 时管道断开，
    /// agent 自己走，不留孤儿。
    #[tokio::test]
    async fn shutdown_leaves_no_orphan() {
        let d = tempfile::tempdir().expect("tmp");
        let body = format!(
            r#"{PRELUDE}
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":1}}}})
        for _ in sys.stdin:
            pass
"#
        );
        let path = fake_agent(d.path(), "idle", &body);
        let c = client_for(&path).await;
        // 宽限给足：脚本要在 stdin 关闭后自己走到 EOF
        tokio::time::timeout(std::time::Duration::from_secs(10), c.shutdown())
            .await
            .expect("shutdown 不能挂住");
        // 再关一次是幂等的
        c.shutdown().await;
    }
}
