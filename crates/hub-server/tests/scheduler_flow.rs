//! 调度器接入（v0.8.2 子任务 3）：任务进入 `pending` 与设备上线都要触发一次路由。
//!
//! 六条用例逐条对应里程碑 §3 的六项测试标准，**一条用例钉一件事**：
//!
//! | 标准 | 用例 |
//! |---|---|
//! | ①合格设备在线 → 执行新增 + `queued` + 收到帧 | [`a_queued_task_is_dispatched_when_a_device_is_online`] |
//! | ②无合格设备 → 零执行 + 仍 `pending` + 无帧 + 归因 | [`no_eligible_device_leaves_the_task_pending_with_a_reason`] |
//! | ③重试不绕过边表 | [`a_dispatch_never_bypasses_the_edge_table`] |
//! | ④设备离线留 pending、上线后重试路由 | [`a_task_waits_for_an_offline_device_and_is_dispatched_when_it_comes_online`] |
//! | ⑤同绑定第二个任务不派（`BindingBusy`） | [`a_second_task_on_a_busy_binding_is_not_dispatched`] |
//! | ⑥`msg_type == types::TASK_DISPATCH` | [`the_dispatch_frame_carries_the_task_dispatch_msg_type`] |
//!
//! # 为什么这一组用例走「内存连接表」而不是真 WS
//!
//! 真 WS 上设备**能不能收到帧**取决于写协程何时把控制面队列里那一帧取出并写
//! 出去；而本子任务要证明的是**Hub 侧有没有产生这一帧、交给哪条连接**。用
//! `OutboundTable::register` 的收端直接读，判据一步到位，且不必和设备的读循环
//! 抢时序（设备桩也要靠 `wait_until` 才能看到帧，那层等待本身不是本任务的
//! 判据）。真 WS 上的端到端形状由子任务 5 的 ack 用例覆盖。
//!
//! `clippy.toml` 的 `allow-*-in-tests` 覆盖不到本文件顶层的 helper
//! （见 `AGENTS.md`），所以文件头自带豁免。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

mod common;

use std::time::Duration;

use hub_core::reports::Report;
use hub_server::scheduler::{
    RouteOutcome, TASK_DISPATCHED_KIND, TASK_REJECTED_KIND, TASK_TRAJECTORY_NAMESPACE,
};
use hub_storage::execution::ExecutionRepo;
use hub_storage::project::{NewProject, ProjectRepo, ProjectShape};
use hub_storage::task::{DoneWhen, NewTask, TaskState, TaskView};
use hub_storage::{BindingRepo, BindingSpec, CleanupPolicy, TaskRepo};
use protocol::frame::Frame;

use crate::common::Hub;

/// 绑定目录。**必须是绝对路径**：`hub_core::schedule` 的「拿得到代码」那一层
/// 用 `fixed_dir.starts_with('/')` 判定，相对路径一律 `no_code`。
const FIXED_DIR: &str = "/srv/af/worktrees";

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn task_repo(hub: &Hub) -> TaskRepo {
    TaskRepo::new(hub.pool())
}

fn execution_repo(hub: &Hub) -> ExecutionRepo {
    ExecutionRepo::new(hub.pool())
}

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

fn bind(hub: &Hub, project_id: &str, device_id: &str) -> String {
    BindingRepo::new(hub.pool())
        .upsert(
            project_id,
            device_id,
            BindingSpec {
                fixed_dir: FIXED_DIR.to_string(),
                // 不设配额：配额会把 ⑤ 的判据变成「配额够不够」，而本组用例
                // 要的是「有没有合格设备」这件事本身。
                disk_quota_bytes: None,
                cleanup_policy: CleanupPolicy::Balanced,
            },
            "unix:1700000000",
        )
        .unwrap()
        .id
}

/// 设备上线：往内存上报缓存里记一份「能跑 git + opencode、磁盘很宽裕」的心跳，
/// 并在连接注册表里占一条连接——路由要求两者都有（前者过能力层，后者才能发帧）。
fn device_online(hub: &Hub, node_id: &str) -> tokio::sync::mpsc::Receiver<protocol::Frame> {
    hub.state.reports().record(
        node_id,
        Report {
            node_version: "0.1.0".to_string(),
            capabilities: serde_json::json!({
                "connectors": ["git"],
                "agents": [{"id": "opencode", "name": "opencode", "version": "0.0.1", "available": true}],
            }),
            // 磁盘余量给得很宽：⑤ 的门槛是 64 MiB 基础开销 + 512 MiB 安全余量。
            resources: serde_json::json!({"disk": {"workspace_free_mb": 102_400_u64}}),
            workspace_roots: Vec::new(),
            at: std::time::Instant::now(),
        },
    );
    let conn_id = format!("conn-{node_id}");
    let (control_rx, _data_rx) = hub.state.outbound().register(&conn_id);
    hub.state.connections().insert(
        node_id.to_string(),
        hub_core::ConnHandle { conn_id, gen: 1 },
    );
    control_rx
}

/// 设备下线：只摘连接，**保留上报**。
///
/// 这与 `presence.rs` 的口径一致：「心跳新鲜但连接断了」是
/// `Presence::Reconnecting`，调度**仍可用**（`scheduling.md` §3）。所以「离线」
/// 在这一组用例里统一指「没有心跳上报」——那才是被路由排除的那一档。
fn device_offline(hub: &Hub, node_id: &str) {
    if let Some(h) = hub.state.connections().get(node_id) {
        hub.state
            .connections()
            .remove_if_current(node_id, &h.conn_id);
    }
}

/// 等到收端上出现一帧（`None` = 预算内一帧都没来）。
async fn next_frame_or_none(rx: &mut tokio::sync::mpsc::Receiver<Frame>) -> Option<Frame> {
    tokio::time::timeout(Duration::from_millis(300), rx.recv())
        .await
        .ok()
        .flatten()
}

/// 某任务的执行记录条数（「execution 零新增」的判据）。
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

impl Hub {
    /// 在 `project_id` 下建一个任务（状态恒为 `pending`）。
    fn create_task(&self, project_id: &str, title: &str) -> TaskView {
        task_repo(self)
            .create(
                NewTask::new(
                    project_id,
                    title,
                    DoneWhen {
                        criteria: None,
                        // 「至少要有描述」是 `DoneWhen` 自己的不变量（验收标准
                        // 不能两栏都空），本组用例与它无关，给一句话即可。
                        description: Some("跑通即可".to_string()),
                    },
                ),
                "unix:1700000000",
            )
            .unwrap()
    }
}

// ---------------------------------------------------------------------------
// ① 合格设备在线 → 执行新增一行 + task.state == queued + 收到 task_dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_queued_task_is_dispatched_when_a_device_is_online() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_online";
    bind(&hub, &project_id, node_id);
    let mut frames = device_online(&hub, node_id);

    let task = hub.create_task(&project_id, "跑一次测试");
    assert_eq!(
        task.state,
        TaskState::Pending,
        "建出来的任务起点必须是 pending"
    );

    let (outcome, push) = hub.state.scheduler().route_task(&task);

    // 1. 结论是「派出去了」，并且帧进了队列（`push` 是 `None`）。
    let RouteOutcome::Dispatched {
        node_id: chosen,
        execution_id,
    } = outcome.clone()
    else {
        panic!("合格设备在线时必须派出去，实际 {outcome:?}");
    };
    assert_eq!(chosen, node_id, "选中的必须是那台唯一合格的设备");
    assert_eq!(push, None, "帧必须真的进了控制面队列");

    // 2. execution 新增**恰好一行**，且指向选中的绑定。
    let execs = execution_repo(&hub).list_for_task(&task.id).unwrap();
    assert_eq!(execs.len(), 1, "派一次只该建一条执行");
    assert_eq!(execs[0].id, execution_id);
    assert_eq!(execs[0].binding_id, bind_id(&hub, &project_id, node_id));

    // 3. task.state == queued（经 `task_state.rs` 的边，不是裸 UPDATE）。
    let after = task_repo(&hub).get(&task.id).unwrap().unwrap();
    assert_eq!(after.state, TaskState::Queued);

    // 4. 连接上确实收到一帧 `task_dispatch`，且 payload 指向那条执行。
    let frame = next_frame_or_none(&mut frames)
        .await
        .expect("派发帧必须被投到该连接的控制面队列");
    assert_eq!(frame.msg_type, protocol::message::types::TASK_DISPATCH);
    let dispatch = frame
        .payload::<protocol::message::TaskDispatch>()
        .expect("task_dispatch 的载荷必须是 TaskDispatch");
    assert_eq!(dispatch.task_id, task.id);
    assert_eq!(dispatch.execution_id, execution_id);
    assert_eq!(dispatch.project_id, project_id);
    assert_eq!(
        dispatch.worktree_path,
        format!("{FIXED_DIR}/{}", task.id),
        "worktree 必须落在绑定的固定目录下"
    );
}

fn bind_id(hub: &Hub, project_id: &str, device_id: &str) -> String {
    BindingRepo::new(hub.pool())
        .get(project_id, device_id)
        .unwrap()
        .expect("binding row")
        .id
}

// ---------------------------------------------------------------------------
// ② 无合格设备 → execution 零新增 + 仍 pending + 无帧 + 轨迹有归因
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_eligible_device_leaves_the_task_pending_with_a_reason() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    // 有绑定但**设备没上报**——即「一台合格设备都不在线」。
    let node_id = "n_absent";
    bind(&hub, &project_id, node_id);

    let task = hub.create_task(&project_id, "没人能跑");
    let (outcome, push) = hub.state.scheduler().route_task(&task);

    // 1. 结论是「被拒」，且说清是哪一层（⓪：一台可用设备都没有）。
    assert_eq!(
        outcome,
        RouteOutcome::Rejected {
            rejection: hub_core::Rejection::NoAllowedDevice
        }
    );
    assert_eq!(push, None, "没选出设备就不该有帧要发");

    // 2. execution 零新增。
    assert_eq!(
        execution_count(&hub, &task.id),
        0,
        "不降级凑数：一行执行都不许建"
    );

    // 3. 任务仍在 pending。
    let after = task_repo(&hub).get(&task.id).unwrap().unwrap();
    assert_eq!(after.state, TaskState::Pending);

    // 4. 轨迹里有归因，且带着线上码（`task-model.md` §7.2 的 `rejected(...)`）。
    let facts = dispatch_facts(&hub, &task.id);
    assert_eq!(facts.len(), 1, "一次拒绝留一条事实：{facts:?}");
    assert_eq!(facts[0].0, TASK_REJECTED_KIND);
    let payload: serde_json::Value = serde_json::from_str(&facts[0].1).unwrap();
    assert_eq!(payload["code"], "no_allowed_device");
    assert_eq!(payload["layer"], "0");
}

// ---------------------------------------------------------------------------
// ③ 重试不绕过边表
// ---------------------------------------------------------------------------

/// 构造一条**不合法的边**，断言被 `task_state.rs` 拒掉。
///
/// 这一条盯的是「派发路径没有第二个写状态的地方」：任何一处裸
/// `UPDATE task SET state` / `force_state` 都能让 `done → queued` 成功，
/// 而本用例会因为 `set_state` 返回 `Err` 以外的结果而红。
#[tokio::test]
async fn a_dispatch_never_bypasses_the_edge_table() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_online";
    bind(&hub, &project_id, node_id);
    let mut frames = device_online(&hub, node_id);

    let task = hub.create_task(&project_id, "已经做完的任务");
    // 走到 `done`：pending → queued → running → review → done 全走合法边。
    let repo = task_repo(&hub);
    for to in [
        TaskState::Queued,
        TaskState::Running,
        TaskState::Review,
        TaskState::Done,
    ] {
        repo.set_state(&task.id, to, "unix:1700000001").unwrap();
    }
    let done = repo.get(&task.id).unwrap().unwrap();
    assert_eq!(done.state, TaskState::Done);

    // 1. 状态机本身拒绝 `done → queued`：这是边表的唯一真相源。
    let refused = repo.set_state(&task.id, TaskState::Queued, "unix:1700000002");
    assert!(
        refused.is_err(),
        "done → queued 不是合法边，必须被 task_state 拒掉，实际 {refused:?}"
    );

    // 2. 调度器看到 `done` 的任务**根本不去算路由**（`NotPending`），
    //    所以它也没有机会绕开边表去改状态。
    let (outcome, push) = hub.state.scheduler().route_task(&done);
    assert_eq!(
        outcome,
        RouteOutcome::NotPending {
            state: TaskState::Done
        }
    );
    assert_eq!(push, None);

    // 3. 库里的状态没被任何一条路径改动，也没有新执行、没有帧。
    let after = repo.get(&task.id).unwrap().unwrap();
    assert_eq!(after.state, TaskState::Done, "绕过边表的写法会在这里露出来");
    assert_eq!(execution_count(&hub, &task.id), 0);
    assert!(
        next_frame_or_none(&mut frames).await.is_none(),
        "不该发出任何 dispatch 帧"
    );
}

// ---------------------------------------------------------------------------
// ④ 设备离线 → 留 pending；上线后被重试路由（「晚到」）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_task_waits_for_an_offline_device_and_is_dispatched_when_it_comes_online() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_late";
    bind(&hub, &project_id, node_id);

    // 设备此刻不在线（没有心跳上报）。
    let task = hub.create_task(&project_id, "等设备上线");
    let (first, _) = hub.state.scheduler().route_task(&task);
    assert_eq!(
        first,
        RouteOutcome::Rejected {
            rejection: hub_core::Rejection::NoAllowedDevice
        }
    );
    assert_eq!(execution_count(&hub, &task.id), 0);
    assert_eq!(
        task_repo(&hub).get(&task.id).unwrap().unwrap().state,
        TaskState::Pending
    );

    // 设备上线：心跳落进内存上报缓存 + 连接入册，然后触发重试路由
    // （生产路径就是 `routes/ws.rs` 的 `retry_routes_for`，这里直接调它背后的
    // `Scheduler::route_binding_tasks`）。
    let mut frames = device_online(&hub, node_id);
    let results = hub.state.scheduler().route_binding_tasks(node_id);

    let dispatched: Vec<&(String, RouteOutcome)> =
        results.iter().filter(|(_, o)| o.is_dispatched()).collect();
    assert_eq!(
        dispatched.len(),
        1,
        "上线只该把这一条待派任务派出去：{results:?}"
    );
    assert_eq!(dispatched[0].0, task.id);

    // 落地三件事都在：执行、queued、帧。
    assert_eq!(execution_count(&hub, &task.id), 1);
    assert_eq!(
        task_repo(&hub).get(&task.id).unwrap().unwrap().state,
        TaskState::Queued
    );
    let frame = next_frame_or_none(&mut frames)
        .await
        .expect("上线后的重试必须把帧送出去");
    assert_eq!(frame.msg_type, protocol::message::types::TASK_DISPATCH);
}

// ---------------------------------------------------------------------------
// ⑤ 同绑定第二个任务在第一个未完成时不派（BindingBusy）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_task_on_a_busy_binding_is_not_dispatched() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_online";
    bind(&hub, &project_id, node_id);
    let mut frames = device_online(&hub, node_id);

    let first = hub.create_task(&project_id, "第一个");
    let (outcome, _) = hub.state.scheduler().route_task(&first);
    assert!(outcome.is_dispatched(), "第一个必须派出去：{outcome:?}");
    // 第一条执行还在 `pending`（没收尾）——那正是「绑定在忙」。

    let second = hub.create_task(&project_id, "第二个");
    let (outcome, push) = hub.state.scheduler().route_task(&second);

    assert_eq!(
        outcome,
        RouteOutcome::Rejected {
            rejection: hub_core::Rejection::BindingBusy
        },
        "同一绑定默认串行（scheduling.md §3.3）"
    );
    assert_eq!(push, None);
    assert_eq!(execution_count(&hub, &second.id), 0, "被拒的任务不建执行");
    assert_eq!(
        task_repo(&hub).get(&second.id).unwrap().unwrap().state,
        TaskState::Pending
    );

    // 归因说清了是 ⑦。
    let facts = dispatch_facts(&hub, &second.id);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].0, TASK_REJECTED_KIND);
    let payload: serde_json::Value = serde_json::from_str(&facts[0].1).unwrap();
    assert_eq!(payload["code"], "binding_busy");
    assert_eq!(payload["layer"], "7");

    // 连接上只有第一个任务的那一帧。
    let frame = next_frame_or_none(&mut frames).await.expect("第一帧");
    let dispatch = frame.payload::<protocol::message::TaskDispatch>().unwrap();
    assert_eq!(dispatch.task_id, first.id);
    assert!(
        next_frame_or_none(&mut frames).await.is_none(),
        "被 Busy 挡下的任务不该再发帧"
    );
}

// ---------------------------------------------------------------------------
// ⑥ msg_type 必须是 types::TASK_DISPATCH
// ---------------------------------------------------------------------------

/// 断言帧的 `msg_type` **逐字**等于 `types::TASK_DISPATCH`。
///
/// 不靠类型推断是因为 `Frame.msg_type` 是 `String`：写错一个字母编译器不管，
/// 而设备侧会把不认识的名字当可选帧**忽略**——症状是「帧发了、设备不动」，
/// 没有任何一处报错。第二条断言把线上字面量也写死，防止常量本身被改。
#[tokio::test]
async fn the_dispatch_frame_carries_the_task_dispatch_msg_type() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_online";
    bind(&hub, &project_id, node_id);
    let mut frames = device_online(&hub, node_id);

    let task = hub.create_task(&project_id, "检查帧类型");
    let (outcome, _) = hub.state.scheduler().route_task(&task);
    assert!(outcome.is_dispatched(), "{outcome:?}");

    let frame = next_frame_or_none(&mut frames).await.expect("派发帧");
    assert_eq!(
        frame.msg_type,
        protocol::message::types::TASK_DISPATCH,
        "msg_type 必须是 task_dispatch"
    );
    // 线上字面量：常量若被改名而设备侧没跟上，这一条会红。
    assert_eq!(frame.msg_type, "task_dispatch");

    // 成功派发也在轨迹里留一条可查的事实。
    let facts = dispatch_facts(&hub, &task.id);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].0, TASK_DISPATCHED_KIND);
    let payload: serde_json::Value = serde_json::from_str(&facts[0].1).unwrap();
    assert_eq!(payload["node_id"], node_id);
    assert_eq!(payload["push_failure"], serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// 帧没送到：不是拒绝，但必须留痕（`push_failure`）
// ---------------------------------------------------------------------------

/// 选出了设备、执行也建了，但连接表里没有它 → 帧没送出去。
///
/// 这条不在六条标准里，但它钉住的是「`Dispatched` 与 `push_failure` 是两件事」
/// 这条口径：结论仍是 `Dispatched`（执行与状态都已落库），而「没送到」写在
/// 轨迹的 `push_failure` 上，让运维查得到。
#[tokio::test]
async fn a_dispatch_whose_frame_cannot_be_sent_is_still_recorded() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_no_conn";
    bind(&hub, &project_id, node_id);
    // 心跳新鲜（能过能力层），但**没有连接**（正在退避重连）。
    device_online(&hub, node_id);
    device_offline(&hub, node_id);

    let task = hub.create_task(&project_id, "连不上");
    let (outcome, push) = hub.state.scheduler().route_task(&task);

    assert!(outcome.is_dispatched(), "{outcome:?}");
    assert_eq!(push, Some(hub_server::scheduler::PushFailure::NotConnected));
    assert_eq!(execution_count(&hub, &task.id), 1);
    let facts = dispatch_facts(&hub, &task.id);
    let payload: serde_json::Value = serde_json::from_str(&facts[0].1).unwrap();
    assert_eq!(payload["push_failure"], "not_connected");
}

// ---------------------------------------------------------------------------
// 「建任务就触发路由」的 HTTP 入口
// ---------------------------------------------------------------------------

/// `POST /api/projects/{id}/tasks` 建完就路由：合格设备在线时，响应里的任务
/// 已经是 `queued`，并且 `dispatch.outcome == "dispatched"`。
///
/// 这条覆盖 `routes/tasks.rs` 的 `create_task` 那段接线：把
/// `state.scheduler().route_task(&task)` 删掉，任务会停在 `pending` 而红。
#[tokio::test]
async fn creating_a_task_over_http_routes_it_immediately() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    let node_id = "n_online";
    bind(&hub, &project_id, node_id);
    let mut frames = device_online(&hub, node_id);

    let (session, csrf) = common::login(&hub).await;
    let app = hub_server::build_app(hub.state.clone());
    let req = common::browser_request(
        "POST",
        &format!("/api/projects/{project_id}/tasks"),
        &session,
        &csrf,
        true,
        Some(serde_json::json!({
            "title": "HTTP 建的任务",
            "done_when": {"criteria": null, "description": "跑通即可"},
        })),
    );
    let (status, body) = common::send(&app, req).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["task"]["state"], "queued", "建完就该是 queued：{v}");
    assert_eq!(v["dispatch"]["outcome"], "dispatched");
    assert_eq!(v["dispatch"]["node_id"], node_id);

    let frame = next_frame_or_none(&mut frames).await.expect("派发帧");
    assert_eq!(frame.msg_type, protocol::message::types::TASK_DISPATCH);
}

/// 没有合格设备时 HTTP 仍然是 201，`dispatch.outcome == "rejected"` 并带上线上码。
///
/// 判红方式：把 `create_task` 里的拒绝分支改成 5xx（或删掉 `dispatch` 字段）。
#[tokio::test]
async fn creating_a_task_without_a_device_reports_the_rejection() {
    let hub = Hub::boot().await;
    let project_id = build_project(&hub, "app");
    // 没有任何绑定 / 设备。

    let (session, csrf) = common::login(&hub).await;
    let app = hub_server::build_app(hub.state.clone());
    let req = common::browser_request(
        "POST",
        &format!("/api/projects/{project_id}/tasks"),
        &session,
        &csrf,
        true,
        Some(serde_json::json!({
            "title": "没人能跑",
            "done_when": {"criteria": null, "description": "跑通即可"},
        })),
    );
    let (status, body) = common::send(&app, req).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["task"]["state"], "pending");
    assert_eq!(v["dispatch"]["outcome"], "rejected");
    assert_eq!(v["dispatch"]["code"], "no_allowed_device");
}

// ---------------------------------------------------------------------------
// 设备上线的**生产路径**：`/ws` 心跳 → `on_heartbeat` → 重试路由
// ---------------------------------------------------------------------------

/// 前面几条用例直接调 `Scheduler`，这一条走真 WS：设备连上来发一次心跳，
/// Hub 侧的 `on_heartbeat` 必须把「晚到」的任务派出去。
///
/// 判红方式：删掉 `on_heartbeat` 末尾的 `retry_routes_for(...)` → 任务停在
/// `pending`、设备收不到帧，这条会红。
#[tokio::test]
async fn a_heartbeat_on_the_ws_dispatches_the_waiting_task() {
    let hub = crate::common::Hub::boot().await;
    let project_id = build_project(&hub, "app");

    // 先连一台设备、拿到它的 node_id，紧接着断开——此时它是「离线」。
    let (est, node_id, kp) = crate::common::connect_enrolled(&hub).await;
    drop(est);
    crate::common::wait_disconnected(&hub, &node_id, Duration::from_secs(5)).await;

    bind(&hub, &project_id, &node_id);
    let task = hub.create_task(&project_id, "等设备上线（WS）");
    let (outcome, _) = hub.state.scheduler().route_task(&task);
    assert!(!outcome.is_dispatched(), "设备不在线时不该派：{outcome:?}");
    assert_eq!(execution_count(&hub, &task.id), 0);

    // 设备重连（同一把钥匙），发一次带能力的心跳。
    let mut est = crate::common::connect_reconnect(&hub, &kp, &node_id).await;
    est.send(&Frame::push(
        protocol::message::types::HEARTBEAT,
        0,
        serde_json::json!({
            "node_version": "0.1.0",
            "capabilities": {
                "connectors": ["git"],
                "agents": [{"id": "opencode", "name": "opencode", "version": "0.0.1", "available": true}],
            },
            // 磁盘余量要过 ⑤ 的门槛（64 MiB + 512 MiB）。
            "resources": {"disk": {"workspace_free_mb": 102_400_u64}},
            "workspace_roots": [],
        }),
    ))
    .await
    .expect("heartbeat must send");

    // 等到库里出现那条派发事实（心跳处理是异步的，用轮询而不是固定 sleep）。
    crate::common::wait_until(Duration::from_secs(5), || {
        task_repo(&hub).get(&task.id).unwrap().map(|t| t.state) == Some(TaskState::Queued)
    })
    .await;

    assert_eq!(execution_count(&hub, &task.id), 1, "上线后必须建一条执行");

    // 设备侧收到的下一帧里要有 `task_dispatch`（`heartbeat_ack` 可能先到）。
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Some(text) = est.next_text().await.unwrap_or_default() else {
                panic!("connection closed before the dispatch frame");
            };
            let Ok(f) = serde_json::from_str::<Frame>(&text) else {
                continue;
            };
            if f.msg_type == protocol::message::types::TASK_DISPATCH {
                return f;
            }
        }
    })
    .await
    .expect("must receive a task_dispatch frame");

    let dispatch = frame.payload::<protocol::message::TaskDispatch>().unwrap();
    assert_eq!(dispatch.task_id, task.id);
}
