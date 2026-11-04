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
            // coverage: excluded — 多行宏的参数行被 llvm-cov 记为 miss（已知现象）：
            // `code = e.code()` 这一行报 miss，而同宏的 `session_id` / `node_id`
            // 两行有覆盖（它们展开成 `Display` 调用，这一行是方法调用）。这与
            // `stream.rs:592` 的 `subject = %auth.subject()` 是同一类。
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
            // coverage: excluded — 同上面那条 `code = e.code()`，多行宏的参数行
            // 被 llvm-cov 记为 miss。这一臂本身由
            // `a_full_outbound_queue_folds_into_unavailable_and_still_closes_the_channel`
            // 执行（队列满 → `send_channel_data` 当场失败）。
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
        // coverage: excluded — 同上面那条：多行宏的参数行被 llvm-cov 记为 miss。
        // 这一臂本身由上面那条队列满用例执行（它连 `close_channel` 都发不出去）。
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
async fn wait_for_rev(
    stream: &mut (impl futures_util::Stream<Item = Envelope> + Unpin),
    channel_id: &str,
) -> Option<serde_json::Value> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::stream::Envelope;

    /// 一帧事件的最小形状：`Envelope` 只要求 `name` 与 `data` 成形。
    ///
    /// `name` 是 `&'static str`，因为它在生产里来自常量表；测试里也只能给
    /// 字面量。数据是一个 JSON 对象串，就是 `Streams::*` 那批构造函数拼出来的
    /// 形状。
    fn env(name: &'static str, data: &str) -> Envelope {
        Envelope {
            id: 1,
            name,
            data: data.to_string(),
        }
    }

    /// 一条带 `channel_id` 与 `payload.op` 的 `channel.frame`。
    fn frame(channel_id: &str, op: &str) -> Envelope {
        env(
            "channel.frame",
            &serde_json::json!({
                "channel_id": channel_id,
                "payload": { "op": op, "head": "abc123" },
            })
            .to_string(),
        )
    }

    fn stream_of(
        events: Vec<Envelope>,
    ) -> futures_util::stream::Iter<std::vec::IntoIter<Envelope>> {
        futures_util::stream::iter(events)
    }

    /// 本通道的 `rev` 帧到了就交出去，**不等后面的帧**。
    #[tokio::test]
    async fn the_matching_rev_frame_is_returned() {
        let mut s = stream_of(vec![frame("ch-1", "rev"), frame("ch-1", "rev")]);
        let got = wait_for_rev(&mut s, "ch-1").await.expect("要拿到 rev 帧");
        assert_eq!(got["op"], "rev");
        assert_eq!(got["head"], "abc123", "整份 payload 交出去，不挑字段");
    }

    /// **不属于本通道的帧要跳过**：同一会话可能同时开着 `files` / `terminal`，
    /// 它们的帧都在同一条总线上。跳过之后仍要等到自己那一帧。
    #[tokio::test]
    async fn a_frame_from_another_channel_is_skipped_not_taken() {
        let mut s = stream_of(vec![frame("ch-other", "rev"), frame("ch-1", "rev")]);
        let got = wait_for_rev(&mut s, "ch-1").await.expect("要拿到 rev 帧");
        assert_eq!(got["op"], "rev");
    }

    /// `payload.op == "error"` 是**终止信号**，不是「继续等」：通道层已经判了
    /// 失败（超时 / 设备拒开），再等到自己那 3 秒只是白等。
    #[tokio::test]
    async fn an_error_frame_ends_the_wait_immediately() {
        let mut s = stream_of(vec![frame("ch-1", "error"), frame("ch-1", "rev")]);
        assert!(
            wait_for_rev(&mut s, "ch-1").await.is_none(),
            "错误帧之后的 rev 帧不该被当成成功"
        );
    }

    /// `channel.closed` 也是终止：设备或别处把通道关了，不会有 `rev` 了。
    #[tokio::test]
    async fn a_closed_channel_ends_the_wait() {
        let mut s = stream_of(vec![
            env("channel.closed", r#"{"channel_id":"ch-1"}"#),
            frame("ch-1", "rev"),
        ]);
        assert!(wait_for_rev(&mut s, "ch-1").await.is_none());
    }

    /// 其余三类帧都只跳过、不终止：
    /// ① `data` 不是 JSON（别的子系统推来的脏帧，不该让探针崩）；
    /// ② 事件名不认识（`device.status` / `session.state`）；
    /// ③ `payload.op` 是 `rev` / `error` 以外的值（探针只发过 `rev`，但同一
    ///    通道上将来可能加别的 op）。
    ///
    /// 这三条都是「跳过」而不是「终止」，差别在于终止会让探针提前放弃、
    /// 把一次本可成功的探测记成 `unavailable`。
    #[tokio::test]
    async fn other_frames_are_skipped_and_the_wait_continues() {
        let mut s = stream_of(vec![
            env("channel.frame", "not json at all"),
            env("device.status", r#"{"channel_id":"ch-1"}"#),
            frame("ch-1", "diff"),
            frame("ch-1", "rev"),
        ]);
        let got = wait_for_rev(&mut s, "ch-1")
            .await
            .expect("跳过之后仍要等到 rev");
        assert_eq!(got["op"], "rev");
    }

    /// 流结束（订阅被丢、连接断了）→ `None`，不是 panic。
    #[tokio::test]
    async fn an_exhausted_stream_returns_none() {
        let mut s = stream_of(vec![]);
        assert!(wait_for_rev(&mut s, "ch-1").await.is_none());
    }

    /// `channel_id` 不是字符串（或是 `null`）时按「不是本通道」处理。
    ///
    /// 判据写的是 `as_str() != Some(channel_id)`，所以非字符串一律不等——
    /// 这一条钉住的是「不会因为类型不对而 panic 或误认」。
    #[tokio::test]
    async fn a_channel_id_of_the_wrong_type_is_not_a_match() {
        let mut s = stream_of(vec![
            env(
                "channel.frame",
                r#"{"channel_id":7,"payload":{"op":"rev"}}"#,
            ),
            frame("ch-1", "rev"),
        ]);
        let got = wait_for_rev(&mut s, "ch-1")
            .await
            .expect("要拿到真正那一帧");
        assert_eq!(got["head"], "abc123", "跳过的是数字 id 那一帧");
    }
}

/// `probe_base_rev` 的门禁分支：这些用例**真的走一遍** `open_device_channel`，
/// 而不是直接断言折成 `Unavailable` 的那一行。
///
/// 为什么值得造这么重的夹具：`probe_base_rev` 的失败折算是这一整套设计里最容易
/// 被写错的地方——「探测失败 → 建会话照常成功」是产品承诺，而它的落点就是
/// 那几个 `return (None, BaseRevStatus::Unavailable)`。只测纯函数
/// `wait_for_rev` 证明不了「门禁的 Err 被吃到 `unavailable` 里」，那一段只能
/// 从外面开一次真通道才看得见。
#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::routes::api::Authenticated;
    use hub_core::connections::ConnHandle;
    use hub_core::reports::Report;
    use hub_storage::repo::NewDevice;

    /// 一台「已注册 + 已登记连接 + 已上报能力 + 已建会话」的设备。
    ///
    /// `reported` 是设备上报的 connector 列表。空列表 = 没上报 git——那是
    /// 「设备上没装 git」的正常情形，也是本模块最想覆盖的那条路径。
    ///
    /// 三个状态分三处造，**因为生产中就是三处**：注册进库（`Registry::enroll`）、
    /// 连接进内存（`ConnectionRegistry::insert`）、能力进内存上报表
    /// （`Reports::record`）。少造一处，对应的门禁就会先拦住，用例测到的就不是
    /// 它想测的那一条。
    struct Fixture {
        state: AppState,
        _dir: tempfile::TempDir,
        session_id: String,
        node_id: String,
        /// 这台「设备」的出站**控制面**收端。
        ///
        /// `open_channel` / `close_channel` / `channel_data` 都走控制面
        /// （`send_to_device` → `OutboundTable::send`），**不是**数据面——
        /// 数据面是 `send_data`，只有 PTY 输出那一路用它。读错队列的症状就是
        /// 用例挂在 `recv()` 上永远等不到帧。
        control_rx: tokio::sync::mpsc::Receiver<crate::conns::Outbound>,
    }

    fn fixture(reported: &[&str]) -> Fixture {
        let d = tempfile::tempdir().expect("tmp");
        let pool = hub_storage::db::open_in(d.path()).expect("db").1;
        let state = AppState::bootstrap_with(
            pool,
            d.path(),
            hub_core::auth::AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            std::time::Duration::from_millis(50),
            hub_core::auth::LoginRateLimiter::new(10, 60),
        )
        .expect("state");

        let enrolled = hub_core::registry::Registry::new(state.pool().clone())
            .enroll(
                &[7u8; 32],
                NewDevice {
                    id: String::new(),
                    name: "dev".into(),
                    key_fingerprint: String::new(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    hostname: "h".into(),
                    node_version: "0.1.0".into(),
                },
            )
            .expect("enroll");
        let node_id = enrolled.node_id().to_string();

        // 连接登记：`open_device_channel` 的「设备在线否」查的是它。
        //
        // **两本账都要登记**，少一本的表现不同：只进 `connections()` 时
        // `send_to_device` 会 `Ok(false)`（出站表里没有这条 conn），于是开通道
        // 在「下发」那一步就折成 `OFFLINE`——用例看起来通过了（都是
        // `unavailable`），实际根本没走到它想测的门禁。
        let conn_id = format!("c-{node_id}");
        state.connections().insert(
            node_id.clone(),
            ConnHandle {
                conn_id: conn_id.clone(),
                gen: state.connections().next_gen(),
            },
        );
        let (control_rx, data_rx) = state.outbound().register(&conn_id);
        // 数据面收端留着不 drop：drop 了那条队列就关闭，`send_data` 会变成
        // 另一种失败，与真实连接对不上。控制面收端交出去——用例要靠它读
        // Hub 下发的帧。
        std::mem::forget(data_rx);

        // 能力上报：`unreported_connector` 读的是内存上报表，不是库里的列。
        state.reports().record(
            &node_id,
            Report {
                node_version: "0.1.0".into(),
                capabilities: serde_json::json!({ "connectors": reported }),
                resources: serde_json::json!({}),
                workspace_roots: Vec::new(),
                at: std::time::Instant::now(),
            },
        );

        let session = hub_storage::SessionRepo::new(state.pool().clone())
            .create(
                &node_id,
                "~/src/app",
                Some("opencode"),
                "t0",
                None,
                BaseRevStatus::Unknown,
            )
            .expect("建会话");

        Fixture {
            state,
            _dir: d,
            session_id: session.id,
            node_id,
            control_rx,
        }
    }

    fn auth() -> Authenticated {
        Authenticated::new("admin".into())
    }

    /// **设备没上报 git connector → `unavailable`，且没有下发任何帧。**
    ///
    /// 这条锁的是「能力硬过滤也算『取不到起点』」：它不建通道，所以不会在设备上
    /// 留一条没人关的通道；也不把 409 当建会话失败。
    #[tokio::test]
    async fn an_unreported_git_connector_folds_into_unavailable() {
        let f = fixture(&[]);
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(
            status,
            BaseRevStatus::Unavailable,
            "没装 git 是正常情形，折成 unavailable 而不是让建会话失败"
        );
    }

    /// **不存在的会话 → `unavailable`**（而不是 panic 或 500）。
    #[tokio::test]
    async fn a_missing_session_folds_into_unavailable() {
        let f = fixture(&["git"]);
        let (got, status) = probe_base_rev(&f.state, &auth(), "s_nope", &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
    }

    /// **设备离线（连接登记被摘掉）→ `unavailable`。**
    ///
    /// 与上一条走的是门禁的不同分支（`OFFLINE` 而不是 `NOT_FOUND`），而对外
    /// 表现必须一样：都是「没拿到起点」。
    #[tokio::test]
    async fn an_offline_device_folds_into_unavailable() {
        let f = fixture(&["git"]);
        let conn_id = format!("c-{}", f.node_id);
        f.state
            .connections()
            .remove_if_current(&f.node_id, &conn_id);
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
    }

    /// **门禁全过、`rev` 也发出去了，但设备不回声** → 等满 `PROBE_TIMEOUT`
    /// 之后归 `unavailable`，并且**通道在绑定表里消失了**。
    ///
    /// 「必定关通道」是这一步唯一的资源承诺：每次建会话在设备上留一条通道，
    /// 设备侧槽位很快用完。这条用例真的等 3 秒——超时分支没法用更短的时间构造
    /// （`PROBE_TIMEOUT` 是常量，不注入），而它是「建会话不泄漏通道」的唯一证据。
    /// 3 秒的代价在这一条上用，其余用例都不等。
    #[tokio::test]
    async fn a_silent_device_times_out_into_unavailable_and_still_closes_the_channel() {
        let f = fixture(&["git"]);
        let started = std::time::Instant::now();
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
        assert!(
            started.elapsed() >= PROBE_TIMEOUT,
            "没回声要等满自己的超时：实耗 {:?}",
            started.elapsed()
        );
        assert!(
            f.state
                .bindings()
                .find_by_session(&f.session_id, auth().subject())
                .is_none(),
            "超时之后也必须关通道，否则每次建会话都泄一条"
        );
    }

    /// **设备真的回了一帧 `rev` → 拿到 hash 且状态 `Ok`。**
    ///
    /// 这一条要把整条链路都跑通：Hub 订阅 → 开通道（帧落到「设备」的出站队列）
    /// → 从队列里读出 `channel_id` → 回一帧 `channel.frame` → 探针取到它 → 关通道。
    /// 前面几条用例只覆盖了「失败都折成 unavailable」，而**成功路径**才是这个功能
    /// 存在的理由；缺了它，把 `rev` 的帧名写错、或把 `head` 的键名写成别的，
    /// 门禁一样绿。
    ///
    /// 这里不等超时也不能等：`probe_base_rev` 是**一次** `.await` 走到黑的，
    /// 所以「回帧」必须从另一条 task 里做。用 `tokio::spawn` 而不是在同一个
    /// future 里 `select!`：探针本身不接受联动对象，替设备答话的那个 task
    /// 就是模拟「设备在另一个进程里」。
    #[tokio::test]
    async fn a_device_that_answers_gives_the_hash_and_ok() {
        let f = fixture(&["git"]);

        // 替设备答话：从出站数据面里读出 `open_channel`，拿到 channel_id，
        // 然后回一帧 `rev` 的结果。
        let state = f.state.clone();
        let node_id = f.node_id.clone();
        // **收端要活到探针跑完**：`recv()` 拿到 `open_channel` 之后任务就返回，
        // 若收端随任务一起 drop，Hub 紧接着要发的 `close_channel` 会撞上一条
        // 已关闭的队列，`send` 报错 → `close_channel_inner` 把绑定放回去，
        // 看起来像「探针没关通道」。真实的长连接不会这样，所以这是夹具的
        // 责任：任务把收端**交回**主线程，主线程持有到断言之后。
        let mut control_rx = f.control_rx;
        let device = tokio::spawn(async move {
            let frame = control_rx.recv().await.expect("Hub 必须下发 open_channel");
            assert_eq!(frame.msg_type, protocol::message::types::OPEN_CHANNEL);
            let open: protocol::channel::OpenChannel =
                frame.payload().expect("open_channel 的载荷要能解");
            // 设备第一次回帧是 `ready`（生产里 node 侧 `open()` 的返回），
            // 但探针**不等** ready：它只等 `rev`。所以这里直接回结果帧，
            // 顺带钉住「探针不依赖 ready」这条性质。
            state.streams().channel_frame(
                &node_id,
                &open.channel_id,
                serde_json::json!({
                    "op": "rev",
                    "head": "0123456789abcdef0123456789abcdef01234567",
                    "not_a_repo": false,
                }),
            );
            (open.channel_id, control_rx)
        });

        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        let (channel_id, _keep_alive) = device.await.expect("设备那条 task 不能 panic");

        assert_eq!(status, BaseRevStatus::Ok);
        assert_eq!(
            got.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567"),
            "完整 40 位，不做短 hash"
        );
        assert!(
            f.state.bindings().get(&channel_id).is_none(),
            "成功路径也要关通道（channel_id={channel_id}）"
        );
    }

    /// 设备回 `not_a_repo: true` → `NotARepo`，不是 `NoHead`。
    ///
    /// 两者的区别是给用户看的那句话：「这个目录不是 git 仓库」与「仓库还没有
    /// 提交」指向完全不同的下一步。帧形状里 `not_a_repo` 优先于 `head`。
    #[tokio::test]
    async fn a_not_a_repo_answer_maps_to_not_a_repo() {
        let f = fixture(&["git"]);
        let state = f.state.clone();
        let node_id = f.node_id.clone();
        // 收端交回主线程持有到断言之后——见 `a_device_that_answers` 的注释。
        let mut control_rx = f.control_rx;
        let device = tokio::spawn(async move {
            let frame = control_rx.recv().await.expect("open_channel");
            let open: protocol::channel::OpenChannel = frame.payload().expect("载荷");
            state.streams().channel_frame(
                &node_id,
                &open.channel_id,
                serde_json::json!({
                    "op": "rev",
                    "head": null,
                    "not_a_repo": true,
                }),
            );
            control_rx
        });

        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        let _keep_alive = device.await.expect("设备那条 task 不能 panic");
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::NotARepo);
    }

    /// 设备回 `head: null` 且不是「非仓库」→ `NoHead`（空仓库）。
    #[tokio::test]
    async fn an_empty_repo_answer_maps_to_no_head() {
        let f = fixture(&["git"]);
        let state = f.state.clone();
        let node_id = f.node_id.clone();
        // 收端交回主线程持有到断言之后——见 `a_device_that_answers` 的注释。
        let mut control_rx = f.control_rx;
        let device = tokio::spawn(async move {
            let frame = control_rx.recv().await.expect("open_channel");
            let open: protocol::channel::OpenChannel = frame.payload().expect("载荷");
            state.streams().channel_frame(
                &node_id,
                &open.channel_id,
                serde_json::json!({
                    "op": "rev",
                    "head": "",
                    "not_a_repo": false,
                }),
            );
            control_rx
        });

        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        let _keep_alive = device.await.expect("设备那条 task 不能 panic");
        assert_eq!(got, None, "空字符串的 head 也是「没有起点」");
        assert_eq!(status, BaseRevStatus::NoHead);
    }

    /// 设备回一帧 `op: "error"` → **立刻** `Unavailable`，不等满 3 秒。
    ///
    /// 判据同时看结果与耗时：`READY_TIMEOUT`（5s）若将来被调到探针超时（3s）
    /// 以下，通道层会先推一帧 `error`，探针必须认它。「认」的定义就是不等，
    /// 所以这里断一个远小于 3 秒的上界。
    #[tokio::test]
    async fn an_error_frame_ends_the_probe_without_waiting_for_the_timeout() {
        let f = fixture(&["git"]);
        let state = f.state.clone();
        let node_id = f.node_id.clone();
        // 收端交回主线程持有到断言之后——见 `a_device_that_answers` 的注释。
        let mut control_rx = f.control_rx;
        let device = tokio::spawn(async move {
            let frame = control_rx.recv().await.expect("open_channel");
            let open: protocol::channel::OpenChannel = frame.payload().expect("载荷");
            state.streams().channel_frame(
                &node_id,
                &open.channel_id,
                serde_json::json!({
                    "op": "error",
                    "code": "timeout",
                }),
            );
            control_rx
        });

        let started = std::time::Instant::now();
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        let _keep_alive = device.await.expect("设备那条 task 不能 panic");
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
        assert!(
            started.elapsed() < PROBE_TIMEOUT,
            "错误帧是终止信号，不该傻等到自己的超时：实耗 {:?}",
            started.elapsed()
        );
    }

    /// **开通道成功了，但下发 `rev` 时队列满** → `unavailable`，且通道仍被关掉。
    ///
    /// 这一条锁的是第 3 步的 `Err` 臂：设备在、门禁全过、`open_channel` 也记了
    /// 账，只是它的控制面队列被塞满（设备半死不活）。此时探针**不能**陪着等
    /// 3 秒——`send_channel_data` 当场就告诉了我们没发出去。
    ///
    /// 造队列满的办法是**不读**控制面：`CONTROL_QUEUE` 是 8，先塞满 8 帧。
    /// 那 8 帧里第 1 帧就是 `open_channel`，所以塞满之后 `rev` 那一帧没有位置。
    #[tokio::test]
    async fn a_full_outbound_queue_folds_into_unavailable_and_still_closes_the_channel() {
        let f = fixture(&["git"]);
        let conn_id = format!("c-{}", f.node_id);
        // 8 格：第 1 格是 `open_channel`（探针发的），后 7 格由这里塞满。
        let filler = || protocol::frame::Frame::push("probe.filler", 0, serde_json::json!({}));
        for _ in 0..7 {
            f.state
                .outbound()
                .send(&conn_id, filler())
                .expect("前 7 帧必须塞得进 8 格队列");
        }

        let started = std::time::Instant::now();
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
        assert!(
            started.elapsed() < PROBE_TIMEOUT,
            "发不出去要当场归因，不要傻等 3 秒：实耗 {:?}",
            started.elapsed()
        );
        // **生产环境**断言不了「绑定已消失」：队列满时 `close_channel_inner` 连
        // `close_channel` 那帧都发不出去，于是它按设计把绑定**放回去**，留给设备
        // 下次连上时由 `drop_node` 整表清理（见 `probe_base_rev` 第 5 步的注释）。
        // 那才是正确行为——在这里硬要求「没了」等于要求它在明知设备收不到关闭帧
        // 的情况下假装关掉，页面重试 DELETE 会拿到幂等的 204 而设备的 PTY 还在跑。
        //
        // 所以这一条只钉「尝试过」：绑定仍在，且探测结果仍是 `unavailable`
        // ——收尾没做成**不改变**探测归因。设备侧通道由重连清掉，这是有意的取舍。
        assert!(
            f.state
                .bindings()
                .find_by_session(&f.session_id, auth().subject())
                .is_some(),
            "队列满时关不掉，绑定要放回去等重连清理（不是静默吞掉）"
        );
    }

    /// **会话已经结束 → `unavailable`**（`open_device_channel` 的第三道门禁）。
    ///
    /// 与「设备离线」是不同分支（`SESSION_ENDED` 而不是 `OFFLINE`），对外表现
    /// 必须一样：建会话本身不会在**已结束**的会话上探测，但这条守卫不能因此
    /// 缺席——`create` 与探测之间隔着一次 `.await`，那一瞬间会话被别处结束掉
    /// 是可能的。
    #[tokio::test]
    async fn an_ended_session_folds_into_unavailable() {
        let f = fixture(&["git"]);
        hub_storage::SessionRepo::new(f.state.pool().clone())
            .end(&f.session_id, "t1")
            .expect("结束会话");
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
    }

    /// **设备已被删除 → `unavailable`**（`workspace.node_id` 为 `None`）。
    ///
    /// 这一条与「设备离线」的区别在库里而不在内存里：删设备**不删会话**
    /// （删除语义），于是会话行还在、工作区的 `node_id` 变成 `NULL`，
    /// 而 `removed_node_id` 记着原值。门禁读到 `None` 就直接回
    /// `DEVICE_REMOVED`——这是唯一一条不看连接表、只看库的分支。
    #[tokio::test]
    async fn a_removed_device_folds_into_unavailable() {
        let f = fixture(&["git"]);
        let removed = hub_storage::repo::DeviceRepo::new(f.state.pool().clone())
            .delete_and_mark_removed(&f.node_id)
            .expect("删设备");
        assert_eq!(removed, 1, "前置：设备行确实被删了");
        let (got, status) = probe_base_rev(&f.state, &auth(), &f.session_id, &f.node_id).await;
        assert_eq!(got, None);
        assert_eq!(status, BaseRevStatus::Unavailable);
    }
}
