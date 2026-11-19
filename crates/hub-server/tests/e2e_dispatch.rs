//! 端到端派发闭环（v0.8.2 子任务 7）：**建任务 → 真 WS 收到 `task_dispatch`
//! → 真 WS 回 ack → 任务 `running` → 执行结束 `review`**。
//!
//! # 这一组用例补的是什么
//!
//! 前面的用例各自钉住了链路的一段：`scheduler_flow.rs` 证明「Hub 产生了
//! `task_dispatch` 帧并交给正确的连接」（读内存连接表的收端），
//! `dispatch_ack_flow.rs` 的 `an_ack_over_the_ws_reaches_the_state_machine`
//! 证明「一帧 ack 经 `/ws` 走进状态机」。**两段从未在同一条用例里首尾相接**：
//! 没有一条用例证明「设备真的收到了活、真的回了 ack、Hub 因此把任务推进」。
//!
//! 本文件就从那条 ack 用例往上游多走两步——先建任务、先在真 WS 上收到
//! `task_dispatch`、再回 ack——于是缺陷池那条 🔴「任务永远不执行」的关单判据
//! （「调度器落地后，建任务 → 自动进 `queued` → 被合格设备接单进 `running`，
//! 端到端可复现」）才有对应的可执行断言。
//!
//! # 为什么处处是「每个状态转换单独断言」
//!
//! 「最后是 `running`」可以由很多错实现凑出来（建任务时顺手写进 `running`、
//! 或者 ack 的处置直接把状态跳着改）。所以每一步落地**都先断言一次**：
//! 执行行数 +1、`task.state == queued`、帧真的到了设备手上、`task.state ==
//! running`、`execution.state == running`。第 ③ 步（进 `running`）之前那一帧
//! ack 是**设备在 WS 上发出来的**，Hub 除了它没有别的输入——所以那一步的因果
//! 关系是被断言住的，不是猜的。
//!
//! # 假设备用真客户端
//!
//! 握手与读帧都走 `node_core::conn` 的产品代码（`connect_once` / `Established`），
//! 与 `ws_flow.rs` / `heartbeat_flow.rs` 同一立场：裸手搓的客户端会漏掉
//! 「node 侧签的字段与 Hub 侧验的字段不一致」这类只有两端都用真代码才暴露的问题。
//! 读帧用 `wait_until` 轮询 + `tokio::time::timeout` 兜底，不写固定 `sleep`。
//!
//! `clippy.toml` 的 `allow-*-in-tests` 覆盖不到本文件顶层的 helper
//! （见 `AGENTS.md`），所以文件头自带这行豁免。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

mod common;

use std::time::Duration;

use hub_server::scheduler::{TASK_REJECTED_KIND, TASK_TRAJECTORY_NAMESPACE};
use hub_storage::execution::{ExecutionRepo, ExecutionState};
use hub_storage::project::{NewProject, ProjectRepo, ProjectShape};
use hub_storage::task::TaskState;
use hub_storage::{BindingRepo, BindingSpec, CleanupPolicy, TaskRepo};
use node_core::conn::Established;
use protocol::frame::Frame;
use protocol::message::{types, DispatchStage, TaskDispatch, TaskDispatchAck};

use crate::common::Hub;

/// 绑定目录。**必须是绝对路径**：`hub_core::schedule` 的「拿得到代码」那一层
/// （`gate_six_code`）用 `fixed_dir.starts_with('/')` 判定，相对路径一律
/// `no_code`。
const FIXED_DIR: &str = "/srv/af/worktrees";

/// 等一帧/一个状态落地的预算。CI 慢时事件仍会到，所以给得比本地观测到的宽。
const BUDGET: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn task_repo(hub: &Hub) -> TaskRepo {
    TaskRepo::new(hub.pool())
}

fn execution_repo(hub: &Hub) -> ExecutionRepo {
    ExecutionRepo::new(hub.pool())
}

/// 每个用例各自建项目：用例之间不共享任何状态（各自起 Hub、各自建项目）。
fn build_project(hub: &Hub, name: &str) -> String {
    ProjectRepo::new(hub.pool())
        .create(
            NewProject {
                name: name.to_string(),
                shape: ProjectShape::Central,
                theme_color: None,
                upstream_url: None,
            },
            "unix:1700000000",
        )
        .unwrap()
        .id
}

/// 把项目绑定到设备上。`fixed_dir` 必须是绝对路径（见 [`FIXED_DIR`]）。
fn bind(hub: &Hub, project_id: &str, device_id: &str) {
    BindingRepo::new(hub.pool())
        .upsert(
            project_id,
            device_id,
            BindingSpec {
                fixed_dir: FIXED_DIR.to_string(),
                // 不设配额：⑤ 的判据要落在「设备本身余量够不够」，而不是
                // 「绑定给的配额够不够」。
                disk_quota_bytes: None,
                cleanup_policy: CleanupPolicy::Balanced,
            },
            "unix:1700000000",
        )
        .unwrap();
}

/// 假设备的心跳载荷。
///
/// `agents` 是设备**报上来的** agent 名单（`available: true` 才算可用，
/// 见 `hub_core::schedule` 的 `available_agents`）；`free_disk_mb` 是
/// `disk.workspace_free_mb`，⑤ 的硬门槛是 64 MiB 基础开销 + 512 MiB 安全余量。
fn heartbeat(agents: &[&str], free_disk_mb: u64) -> serde_json::Value {
    let agent_reports: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| serde_json::json!({"id": a, "name": a, "version": "0.0.1", "available": true}))
        .collect();
    serde_json::json!({
        "node_version": "0.1.0",
        "capabilities": {"connectors": ["git"], "agents": agent_reports},
        "resources": {
            "load": {"load_per_core": 0.25},
            "memory": {"total_mb": 8192, "available_mb": 6144},
            "disk": {"workspace_free_mb": free_disk_mb},
        },
        "workspace_roots": [],
    })
}

/// 发一次心跳并等它落进 Hub 的内存上报缓存（路由读内存不读库）。
async fn send_heartbeat_and_wait(hub: &Hub, est: &mut Established, payload: serde_json::Value) {
    est.send(&Frame::push(types::HEARTBEAT, 0, payload))
        .await
        .expect("heartbeat must send");
    let node_id = est.auth_ok.node_id.clone();
    common::wait_until(BUDGET, || hub.state.reports().get(&node_id).is_some()).await;
}

/// 在 WS 上等一帧指定 `msg_type` 的帧。
///
/// 心跳回执（`heartbeat_ack`）之类可能先到，所以**按类型过滤**而不是「读下一帧
/// 就断言」。用 `tokio::time::timeout` 兜住「帧永远不来」——那时是连接卡死，
/// 再等下去也没有意义；不用固定 `sleep`：要等的是「帧到达」这个事件本身。
async fn await_frame(est: &mut Established, msg_type: &str) -> Frame {
    tokio::time::timeout(BUDGET, async {
        loop {
            let Some(text) = est.next_text().await.expect("read must not fail") else {
                panic!("connection closed while waiting for {msg_type}");
            };
            let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
                // 解不开的帧与设备侧的处置一致（约束 14）：只丢这一帧。
                continue;
            };
            if frame.msg_type == msg_type {
                return frame;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("等 {msg_type} 帧超时（{BUDGET:?}）"))
}

/// 假设备回一条 ack。
async fn send_ack(est: &mut Established, execution_id: &str, stage: DispatchStage) {
    let ack = TaskDispatchAck {
        execution_id: execution_id.to_string(),
        stage,
        reason: None,
        evidence: None,
        failure_kind: None,
        items: None,
    };
    let payload = serde_json::to_value(&ack).expect("ack must serialize");
    est.send(&Frame::push(types::TASK_DISPATCH_ACK, 0, payload))
        .await
        .expect("ack must send");
}

/// 等任务落到指定状态（ack 是异步处理的，用轮询而不是固定 sleep）。
async fn wait_task_state(hub: &Hub, task_id: &str, want: TaskState) {
    common::wait_until(BUDGET, || {
        task_repo(hub).get(task_id).unwrap().map(|t| t.state) == Some(want)
    })
    .await;
    assert_eq!(
        task_repo(hub).get(task_id).unwrap().unwrap().state,
        want,
        "任务没有落到 {want:?}"
    );
}

/// 等执行落到指定状态。
async fn wait_execution_state(hub: &Hub, execution_id: &str, want: ExecutionState) {
    common::wait_until(BUDGET, || {
        execution_repo(hub)
            .get(execution_id)
            .unwrap()
            .map(|e| e.state)
            == Some(want)
    })
    .await;
    assert_eq!(
        execution_repo(hub)
            .get(execution_id)
            .unwrap()
            .expect("execution row")
            .state,
        want,
        "执行没有落到 {want:?}"
    );
}

/// 某任务的执行记录条数。
fn execution_count(hub: &Hub, task_id: &str) -> usize {
    execution_repo(hub).list_for_task(task_id).unwrap().len()
}

/// 某任务在派发轨迹里的 `(kind, payload)`，按物理顺序。
fn dispatch_facts(hub: &Hub, task_id: &str) -> Vec<(String, String)> {
    let session_id = format!("{TASK_TRAJECTORY_NAMESPACE}{task_id}");
    hub.state
        .trajectory()
        .list(&session_id, 0, 100)
        .unwrap()
        .into_iter()
        .map(|e| (e.kind, e.payload))
        .collect()
}

/// `POST /api/projects/{id}/tasks` 建任务，返回 `(状态码, 响应体 JSON)`。
///
/// 走真 HTTP 路由而不是直接调 `TaskRepo::create`：`create_task` 是「建完就
/// 触发路由」的接线点（它内部调 `Scheduler::route_task`），绕开它就测不到
/// 「用户点一下新建，任务就开始跑」这件事。
async fn post_task(
    hub: &Hub,
    project_id: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    let (session, csrf) = common::login(hub).await;
    let app = hub_server::build_app(hub.state.clone());
    let req = common::browser_request(
        "POST",
        &format!("/api/projects/{project_id}/tasks"),
        &session,
        &csrf,
        true,
        Some(body),
    );
    let (status, text) = common::send(&app, req).await;
    let json =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("未预期的响应体 {text}（{e}）"));
    (status, json)
}

/// 「建任务就路由」的最小请求体：标题 + 一条说明（`done_when` 两栏不能全空）。
fn task_body(title: &str) -> serde_json::Value {
    serde_json::json!({
        "title": title,
        "done_when": {"criteria": null, "description": "跑通即可"},
    })
}

// ---------------------------------------------------------------------------
// 主链路：建任务 → 收到 dispatch → 回 running → 回 succeeded
// ---------------------------------------------------------------------------

/// **完整的任务生命周期**，四步状态转换各断言一次。
///
/// 这条用例就是缺陷池 🔴 关单判据的可执行版本：
///
/// | 判据原文 | 本用例的断言 |
/// |---|---|
/// | 「建任务 → 自动进 `queued`」 | ①执行新增 + ②`queued`（见 [`assert_stage_two`]） |
/// | 「被合格设备**接单**进 `running`」 | ③`running` 由**设备在 WS 上发的 ack** 引起（见 ③ 段） |
/// | 「端到端可复现」 | ④执行 `running` + ⑤收尾 `review` / `succeeded` |
///
/// 「仅把新建任务的初始状态改成 `queued` 不算关闭」这一条由 ③ 段守住：进
/// `running` 之前有一条**只有设备能产生**的输入（WS 上的一帧 `task_dispatch_ack`），
/// 且该断言发生在「设备确实收到了 `task_dispatch`」之后。
#[tokio::test]
async fn a_task_created_over_http_runs_end_to_end_over_the_ws() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app-e2e");

    // 假设备连上：enroll → 拿到 node_id → 报心跳（能力与磁盘都够用）。
    let (mut est, node_id, _kp) = common::connect_enrolled(&hub).await;
    send_heartbeat_and_wait(&hub, &mut est, heartbeat(&["opencode"], 102_400)).await;
    bind(&hub, &project_id, &node_id);

    // ---- 建任务（真 HTTP：POST /api/projects/{id}/tasks）----
    let (status, body) = post_task(&hub, &project_id, task_body("端到端跑一次")).await;
    assert_eq!(
        status,
        axum::http::StatusCode::CREATED,
        "建任务必须 201：{body}"
    );
    let task_id = body["task"]["id"].as_str().expect("任务 id").to_string();

    // ---- 断言①（建立前提）：路由确实选中了这台设备 ----
    //
    // 先断「派给谁」再断「库里怎么写的」：若这里就不是 `n_online`，后面所有
    // 状态断言都会变成「在一个不是我们预期的系统上碰巧成立」。
    assert_eq!(
        body["dispatch"]["outcome"], "dispatched",
        "合格设备在线时必须派出去：{body}"
    );
    assert_eq!(
        body["dispatch"]["node_id"], node_id,
        "必须派给那台唯一合格的设备：{body}"
    );

    // ---- 断言②：execution 表 +1（且正好一条，指向这台设备的绑定）----
    let executions = execution_repo(&hub).list_for_task(&task_id).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "派一次只该建一条执行记录：{executions:?}"
    );
    let execution_id = executions[0].id.clone();
    assert_eq!(
        executions[0].binding_id,
        BindingRepo::new(hub.pool())
            .get(&project_id, &node_id)
            .unwrap()
            .expect("binding row")
            .id,
        "执行必须挂在这台设备的绑定上"
    );
    // 起点：新执行还没开跑，`started_at` 必须还空——「进 running」那一步
    // 只能是 ack 做的，不能是建任务顺手写进去的。
    assert_eq!(
        executions[0].state,
        ExecutionState::Pending,
        "刚派出去的执行应当还是 pending"
    );

    // ---- 断言③：task.state == queued（自动进 `queued`，不是 `pending`）----
    //
    // 先断「建任务时落的是 `pending`、路由之后才是 `queued`」这条因果：响应体里
    // 的 `state` 是重读后的值，而库里的值此刻也必须是 `queued`。两者都断，
    // 因为「只在响应体里改成 queued」同样是伪装。
    assert_eq!(
        body["task"]["state"], "queued",
        "建完就路由，回显必须是 queued：{body}"
    );
    assert_eq!(
        task_repo(&hub).get(&task_id).unwrap().unwrap().state,
        TaskState::Queued,
        "库里的状态也必须是 queued"
    );

    // ---- 断言④：假设备在**真 WS 上**收到 `task_dispatch` 帧 ----
    let frame = await_frame(&mut est, types::TASK_DISPATCH).await;
    // `msg_type` 逐字断言（`Frame.msg_type` 是 `String`，写错一个字母编译器不管，
    // 设备侧会把不认识的类型当可选帧忽略——症状是「帧发了、设备不动」）。
    assert_eq!(
        frame.msg_type, "task_dispatch",
        "线上字面量必须逐字是 task_dispatch"
    );
    let dispatch = frame
        .payload::<TaskDispatch>()
        .expect("task_dispatch 的载荷必须是 TaskDispatch");
    assert_eq!(dispatch.execution_id, execution_id, "帧指向的执行");
    assert_eq!(dispatch.task_id, task_id, "帧指向的任务");
    assert_eq!(dispatch.project_id, project_id, "帧指向的项目");
    assert_eq!(
        dispatch.worktree_path,
        format!("{FIXED_DIR}/{task_id}"),
        "worktree 必须落在绑定的固定目录下"
    );

    // ---- 断言⑤：设备回 ack{running} → task.state == running ----
    //
    // 这一步之前，设备侧能证明「我真的收到了活」（上面那条帧断言）；这一步
    // 之后，Hub 侧唯一的输入就是这帧 ack。所以「进 running」的因果成立。
    send_ack(&mut est, &execution_id, DispatchStage::Running).await;
    wait_task_state(&hub, &task_id, TaskState::Running).await;

    // ---- 断言⑥：execution.state == running（且 `started_at` 被填上）----
    wait_execution_state(&hub, &execution_id, ExecutionState::Running).await;
    let running = execution_repo(&hub)
        .get(&execution_id)
        .unwrap()
        .expect("execution row");
    assert!(
        running.started_at.is_some(),
        "开跑必须填 started_at：{running:?}"
    );
    assert_eq!(running.ended_at, None, "还没结束，ended_at 必须留空");

    // ---- 收尾：设备回 ack{succeeded} → 任务 `review`、执行 `succeeded` ----
    //
    // 任务的终点是 `review` 而**不是** `done`：跳过 review 就是跳过价值锚点
    // （`task-model.md` 的六态）。
    send_ack(&mut est, &execution_id, DispatchStage::Succeeded).await;
    wait_task_state(&hub, &task_id, TaskState::Review).await;
    wait_execution_state(&hub, &execution_id, ExecutionState::Succeeded).await;
    let done = execution_repo(&hub)
        .get(&execution_id)
        .unwrap()
        .expect("execution row");
    assert!(done.ended_at.is_some(), "收尾必须填 ended_at：{done:?}");
    assert_eq!(done.failure_kind, None, "成功不该有失败形态");

    // 全程只有一条执行：进 running / 进 review 都不该新建执行记录。
    assert_eq!(
        execution_count(&hub, &task_id),
        1,
        "生命周期内不该出现第二条执行"
    );
}

// ---------------------------------------------------------------------------
// 反例：设备能力不足 → 不降级凑数
// ---------------------------------------------------------------------------

/// 设备**缺必需 agent** → 建任务：执行零新增、任务仍 `pending`、无帧、归因说清
/// 是第 ② 层（`no_agent`）。
///
/// 判红方式：把 `hub_core::schedule::filter` 的 ② 层（`has_required_agents`）
/// 去掉，或在 `Scheduler::route_task` 的拒绝分支里先建执行再判——前者这条会
/// 在「派出去」上红，后者会在「执行零新增」上红。
#[tokio::test]
async fn a_device_without_the_required_agent_leaves_the_task_pending() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app-no-agent");

    // 设备只装 `opencode`，而任务点名要 `codex`。
    let (mut est, node_id, _kp) = common::connect_enrolled(&hub).await;
    send_heartbeat_and_wait(&hub, &mut est, heartbeat(&["opencode"], 102_400)).await;
    bind(&hub, &project_id, &node_id);

    let (status, body) = post_task(
        &hub,
        &project_id,
        serde_json::json!({
            "title": "要 codex",
            "done_when": {"criteria": null, "description": "跑通即可"},
            "constraints": {"allowed_agents": "[\"codex\"]"},
        }),
    )
    .await;
    // 派不出去**不是建任务失败**：201 照旧，结论在 `dispatch` 字段里。
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    let task_id = body["task"]["id"].as_str().expect("任务 id").to_string();

    assert_eq!(
        body["dispatch"]["outcome"], "rejected",
        "能力不够就不许派：{body}"
    );
    assert_eq!(
        body["dispatch"]["code"], "no_agent",
        "归因必须说清是第 ② 层（缺 agent）：{body}"
    );

    // 1. 执行零新增（不降级凑数）。
    assert_eq!(
        execution_count(&hub, &task_id),
        0,
        "被拒的任务一行执行都不许建"
    );

    // 2. 任务仍停在 `pending`（不是 `queued`——那会把「没派发」伪装成「排队中」）。
    assert_eq!(
        task_repo(&hub).get(&task_id).unwrap().unwrap().state,
        TaskState::Pending
    );

    // 3. 设备侧一帧都没收到：给它一个短预算，等不到就是对的。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), est.next_text())
            .await
            .is_err(),
        "没选出设备就不该有帧发给设备"
    );

    // 4. 归因落进轨迹：哪一层（`layer`）+ 线上码（`code`）。
    let facts = dispatch_facts(&hub, &task_id);
    assert_eq!(facts.len(), 1, "一次拒绝留一条事实：{facts:?}");
    assert_eq!(facts[0].0, TASK_REJECTED_KIND, "归因的 kind 必须可查");
    let payload: serde_json::Value = serde_json::from_str(&facts[0].1).unwrap();
    assert_eq!(payload["layer"], "2", "第 ② 层（所需 agent）：{payload}");
    assert_eq!(payload["code"], "no_agent", "线上码：{payload}");
}

/// 设备**磁盘不够** → 建任务：执行零新增、任务仍 `pending`、无帧、归因说清
/// 是第 ⑤ 层（`disk_insufficient`）。
///
/// 与上一条分开成两个用例，而不是合成「能力不足」一条：`scheduling.md` §3.2
/// 把磁盘硬门槛单列，因为「派过去只是把失败推后 20 分钟，而失败形态会变成
/// `env_broken` 归因到设备」——「是第几层」正是这张表要回答的问题，两个层的
/// 归因必须各自可观测。
///
/// 判红方式：把 ⑤（`filter` 里的 `free_disk_bytes` / `disk_floor_bytes` 判定）
/// 去掉，任务会被派出去，这条在「`execution` 零新增」上红。
#[tokio::test]
async fn a_device_without_enough_disk_leaves_the_task_pending() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app-no-disk");

    // 100 MiB：远低于 ⑤ 的门槛（64 MiB 基础开销 + 512 MiB 安全余量 = 576 MiB）。
    let (mut est, node_id, _kp) = common::connect_enrolled(&hub).await;
    send_heartbeat_and_wait(&hub, &mut est, heartbeat(&["opencode"], 100)).await;
    bind(&hub, &project_id, &node_id);

    let (status, body) = post_task(&hub, &project_id, task_body("磁盘不够")).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    let task_id = body["task"]["id"].as_str().expect("任务 id").to_string();

    assert_eq!(
        body["dispatch"]["outcome"], "rejected",
        "磁盘不够就不许派：{body}"
    );
    assert_eq!(
        body["dispatch"]["code"], "disk_insufficient",
        "归因必须说清是第 ⑤ 层（磁盘硬门槛）：{body}"
    );

    // 1. 执行零新增。
    assert_eq!(
        execution_count(&hub, &task_id),
        0,
        "磁盘门槛挡下时不许建执行"
    );

    // 2. 任务仍停在 `pending`。
    assert_eq!(
        task_repo(&hub).get(&task_id).unwrap().unwrap().state,
        TaskState::Pending
    );

    // 3. 设备侧没有帧。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), est.next_text())
            .await
            .is_err(),
        "没选出设备就不该有帧发给设备"
    );

    // 4. 轨迹里的 `layer` / `code` 逐字断言。
    let facts = dispatch_facts(&hub, &task_id);
    assert_eq!(facts.len(), 1, "一次拒绝留一条事实：{facts:?}");
    assert_eq!(facts[0].0, TASK_REJECTED_KIND);
    let payload: serde_json::Value = serde_json::from_str(&facts[0].1).unwrap();
    assert_eq!(payload["layer"], "5", "第 ⑤ 层（磁盘）：{payload}");
    assert_eq!(payload["code"], "disk_insufficient", "线上码：{payload}");
}
