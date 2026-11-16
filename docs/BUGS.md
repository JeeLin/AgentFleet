# 缺陷池 / Defect Pool

> 待处理 bug 的汇总地。`mdflow-planner` 在规划里程碑时把 🔴🟡🟢 条目原样搬入里程碑文档的
> `## Bugs` 表，**确认写入行数 == 本表条目数后**才从本表删行（写入与删行成对，缺一即停止）。
>
> Pool of pending bugs. The planner moves 🔴🟡🟢 entries into the milestone's Bugs table at
> planning time, and only deletes rows from here after confirming the move succeeded.

## 待处理 / Pending

| 提出版本 | 优先级 | 标题 | 来源 | 描述 |
|----------|--------|------|------|------|
| v0.8.1 | 🔴 | **新建任务并绑定设备后，任务永远不会开始执行**：没有组件把 `pending` 推进到 `queued`，调度器/派发器尚未实现 | 用户反馈 | **现象**（用户原话）：「新建了任务、绑定了设备，好像任务并没有开始执行」。**位置**：`crates/hub-server/src/routes/tasks.rs` 的 `create_task`（`POST /api/projects/{id}/tasks`）；状态机边 `crates/hub-storage/src/task_state.rs:45` 的 `Pending → Queued`（规格锚点：`docs/architecture/task-model.md` 的 `### 2.2 状态图的边` 段）。**证据**：① `CreateTaskBody`（`tasks.rs:184-191`）**无 `state` 字段**，`create_task` 走 `NewTask::new(…)`，落库状态恒为 `pending`；② 生产代码里能产出 `TaskState::Queued` 的地方**只有一处**——`tasks.rs:965` 的 `FailureAction::Retry => TaskState::Queued`（失败后重派），其余命中全是测试的 `force_state`/`set_state`；③ `grep -rln` 在 `crates/` 全部 `.rs` 里搜 `fn schedule`、`struct Scheduler`、`Dispatcher` 三个模式 → **零命中**，`ls crates/` 无调度相关 crate；④ `docs/DEVELOPMENT.md:457` 把「调度器与派发器（三层过滤与打分，`scheduling.md`）」排在 **v0.8.2**。**断言**：用户建完任务、绑好设备后，任务卡片一直停在「待办」列，**永远不进入**「排队中 / 执行中」，设备侧收不到任何派发——把 `pending`→`queued`→`running` 打通的组件在整个仓库里不存在。这不是界面 bug，是任务闭环缺了**派发**这一环：v0.8.0 交付的是状态机 + API + 看板，驱动状态机的执行侧在 v0.8.2。**关单判据**：调度器落地后，建任务 → 自动进 `queued` → 被合格设备接单进 `running`，端到端可复现；仅把新建任务的初始状态改成 `queued` **不算**关闭（那只是把「没派发」伪装成「排队中」）。 |
| — | — | 当前无其他待处理缺陷 | — | v0.8.0 子任务 11 的 3 条（🔴 Windows WS 200 / 🟡 CI node20 / 🟡 afcli 架构选择）已修完并从本表删行，逐行状态见 `.mdflow/milestones/v0.8.0-task-and-board.md` 的 `## Bugs` 表。 |
