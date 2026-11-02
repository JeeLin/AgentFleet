//! 建会话时探一次工作区的起点（`v0.6.0`）。
//!
//! # 为什么在这里问，而不是让页面先问再带上
//!
//! 页面能问出 `rev` 与会话能问出 `rev` 之间隔着用户点「创建」的那一下，两个值
//! 可能不同（agent 自己 commit 过）。起点必须与会话**诞生在同一时刻**产生，
//! 否则「起点」在定义上就已经偏了。所以由建会话的那一侧问。
//!
//! # 为什么走通道而不是新增 HTTP 端点
//!
//! Hub 到设备只有一条下行路径——WebSocket 帧（`AGENTS.md` 约束：零入站端口、
//! 单条主动外联）。所有「问设备一件事」都已走通道，新增端点只能是在 Hub 进程内
//! 转发，绕不出这条约束，反而多一层要维护的路由。
//!
//! # 起点是增益，不是门禁
//!
//! 探测的任何失败都折成 [`BaseRevStatus::Unavailable`]，**建会话照常成功**。
//! 把它做成门禁的后果是「设备上装的是 hg / 目录不是仓库 / 设备刚好忙」时用户
//! 连会话都开不了——而 `v0.2.0` 起「非 git 工作区」一直是合法用法。这与
//! `agent_id` 必须是同一类东西吗？**不是**：没有 agent 的会话永远无法对话，
//! 而没有起点的会话只是少一条信息。

use serde_json::json;

use super::api::Authenticated;
use super::stream::{open_device_channel, send_channel_data, Envelope};
use crate::AppState;
use hub_storage::BaseRevStatus;

/// 探测自己的超时。
///
/// `rev` 是两次极短的子进程（`rev-parse`），正常在 20ms 内；3 秒是「设备忙到
/// 连两次 rev-parse 都跑不完」的判据。
///
/// **与 `protocol::channel::READY_TIMEOUT`（5 秒）的先后关系**：3 < 5，所以
/// **探针总是先到自己这个超时**，而不是等通道层那条 `ready` 定时器。反过来说，
/// 若将来把 `READY_TIMEOUT` 调到 3 秒以下，探针会先收到通道层推的那帧
/// `{"op":"error","code":"timeout"}`——下面第 4 步的 `op == "error"` 分支就是
/// 为那一天准备的终止信号，两条路都收敛到「立刻归 unavailable 并关通道」。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 探测结果：完整 hash（只有 `Ok` 才有）+ 五态之一。
pub(crate) type Probe = (Option<String>, BaseRevStatus);

/// 在 `session_id` 上开一条 `git` 通道，问一次 `rev`，然后**必定**关掉它。
///
/// 调用方约定：`session_id` 对应的会话 **已经落库且 `state == running`**——这是
/// 能开通道的唯一前提（`stream.rs::open_device_channel` 第一件事就查它）。
/// 所以顺序只能是「先落行 → 探测 → 回填」，见 `api.rs::create_session`。
///
/// 任何失败路径都归 `Unavailable`，并且**不写 warn 日志**：`unavailable` 是
/// 预期路径之一（设备没装 git 的场景很正常），记 debug 就够。
pub(crate) async fn probe_base_rev(
    state: &AppState,
    auth: &Authenticated,
    session_id: &str,
    node_id: &str,
) -> Probe {
    // **第 1 步：先订阅。** 顺序不能反：上行 `channel_data` 到 Hub 后走
    // `on_channel_frame` → `state.streams().channel_frame(...)` → `publish` →
    // `tokio::sync::broadcast`，而 **broadcast 不重放**。先发后订阅的话，快设备
    // 回的帧在订阅之前就投递完了，于是必然等满 3 秒超时。这条坑在
    // `stream.rs` 的 SSE 用例注释里已经记过一次，本处是第二个调用点。
    //
    // `None` 是「不要回放」：`replay_since` 那条是给页面 `Last-Event-ID` 断线
    // 续收用的，要传一个序号，而探针不知道序号，也不需要历史。
    //
    // 用 `subscribe_raw` 而不是 `subscribe`：后者返回的是 `BoxStream<Event>`，
    // 而 `axum` 的 `Event` 是只写的，读不回事件名与载荷。
    let mut stream = state.streams().subscribe_raw(None);

    // 第 2 步：开通道。四道门禁的 `Err` 全部折成 `unavailable`——见下面注释。
    let opened = match open_device_channel(
        state,
        auth,
        session_id,
        "git",
        json!({}),
        // `true`：设备没上报 `git` connector 就不下发。设备上没装 git 是
        // **正常情形**，所以这里的 409 `not_reported` 同样是「取不到起点」，
        // 不是「建会话失败」。
        true,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            // 四道门禁（`session_ended` / `device_removed` / `device_offline` /
            // `not_reported`）的语义就是「探测不可用」，不是请求错误。这里是
            // 本里程碑唯一允许把 `ApiError` 丢掉的地方：`probe_base_rev` 的返回
            // 类型里没有 `ApiError`，而建会话不该因为这四件事之一而失败。
            //
            // 前两道在正常流程下不应触发（`create` 刚写 `running`，设备刚从
            // `find_by_id` 查出来），但**不能因此把它们当不可能**。
            tracing::debug!(
                session_id,
                node_id,
                code = e.code(),
                "cannot open a git channel to probe the base revision"
            );
            return (None, BaseRevStatus::Unavailable);
        }
    };

    // 第 3 步：下发 `rev`。这条失败只可能来自「设备不在」或「队列满」，
    // 都是「没拿到」，不是请求错误。
    let sent = send_channel_data(state, node_id, &opened.channel_id, json!({ "op": "rev" }));

    // 第 4 步：等一条属于本通道的 `rev` 帧。`None` = 没等到（超时 / 错误帧 /
    // 通道被关），一律归 `unavailable`。
    let frame = match sent {
        Ok(_) => tokio::time::timeout(PROBE_TIMEOUT, wait_for_rev(&mut stream, &opened.channel_id))
            .await
            .unwrap_or_default(),
        Err(e) => {
            tracing::debug!(
                session_id,
                node_id,
                code = e.code(),
                "cannot send the rev op to the device"
            );
            None
        }
    };

    // 第 5 步：**无论成功 / 失败 / 超时，都关通道。** 不关的后果是每次建会话都
    // 在设备上留一条没人关的通道，而设备侧通道槽位有限。
    //
    // 走与 `DELETE /api/channels/{id}` 同一份内部逻辑（`close_channel_inner`），
    // 不是自己写一遍 `unbind`：解除绑定 / 发 `close_channel` 帧 / 推
    // `channel.closed` 三件事的顺序在那边已经定过，抄一份只会迟早分叉。
    //
    // **关不掉不改变探测结果**：通道归属在 `open_device_channel` 里已经落账，
    // 这里拿的是同一个 subject，所以 `NotYours` 不可能发生；`BUSY`（设备读不过来）
    // 则会走 `close_channel_inner` 的「把归属放回去」分支，留给设备下次连上时
    // 由 `drop_node` 整表清理。把它当成建会话失败是错的——会话本身已经建好，
    // 起点也已探到（或已归因），用户没有理由因为收尾没做成而丢一次成功的创建。
    if let Err(e) = super::stream::close_channel_inner(
        state,
        auth,
        &opened.channel_id,
        "closed after probing the base revision",
    ) {
        tracing::debug!(
            session_id,
            node_id,
            code = e.code(),
            "cannot close the probe channel; the binding is released on disconnect"
        );
    }

    // 第 6 步：解读那一帧。
    match frame {
        // `not_a_repo: true` → 工作区不是 git 仓库。
        //
        // 判据看 `head` 是否为空串而不是 `is_null` 之外的东西：设备侧 `rev` 的
        // 三帧键集合逐字相同，靠的就是「非仓库时 head 为 null」。
        Some(f) if f.get("not_a_repo").and_then(serde_json::Value::as_bool) == Some(true) => {
            (None, BaseRevStatus::NotARepo)
        }
        // `head: null` 且不是「非仓库」→ 仓库还没有提交。
        Some(f) => match f.get("head").and_then(serde_json::Value::as_str) {
            Some(h) if !h.is_empty() => (Some(h.to_string()), BaseRevStatus::Ok),
            _ => (None, BaseRevStatus::NoHead),
        },
        None => (None, BaseRevStatus::Unavailable),
    }
}

/// 在事件流里等一条属于 `channel_id` 的 `rev` 结果帧。
///
/// 三种终止条件，语义各不相同，**都必须认**：
/// - 拿到 `payload.op == "rev"` 且 `channel_id` 对得上 → 交出去；
/// - 收到 `payload.op == "error"` → 通道层自己判了超时 / 设备拒开。**这是终止
///   信号**，不能只等 `op == "rev"`——否则 `READY_TIMEOUT` 若短于本探针的超时，
///   探针会拿着一个已知的失败继续干等到自己超时；
/// - 收到 `channel.closed` → 设备或别处把通道关了，不会有 `rev` 了。
///
/// 其余一切继续等：包括本通道以外的帧（同一会话可能开着别的面板）。
async fn wait_for_rev(stream: &mut (impl futures_util::Stream<Item = Envelope> + Unpin), channel_id: &str) -> Option<serde_json::Value> {
    use futures_util::StreamExt as _;

    while let Some(env) = stream.next().await {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&env.data) else {
            continue;
        };
        // 只认本通道的帧：同一会话可能同时开着别的面板（`files` / `terminal`），
        // 它们的事件都在同一条总线上。
        if v.get("channel_id").and_then(serde_json::Value::as_str) != Some(channel_id) {
            continue;
        }
        match env.name {
            "channel.frame" => {
                let payload = v.get("payload").cloned().unwrap_or_default();
                match payload.get("op").and_then(serde_json::Value::as_str) {
                    Some("rev") => return Some(payload),
                    // 通道层判的失败：立刻终止，不要等到自己的 3 秒。
                    Some("error") => return None,
                    // 别的 op（理论上不会有，探针只发过 `rev`）：不是我们的帧。
                    _ => continue,
                }
            }
            // 通道被关：不会有 `rev` 了。
            "channel.closed" => return None,
            _ => continue,
        }
    }
    None
}
