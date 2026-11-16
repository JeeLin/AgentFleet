-- AgentFleet Hub 存储 schema v7：任务态对齐规格六态 + 本里程碑所需列
--
-- 目标：`task.state` 从 v5 的六值 `draft/ready/running/blocked/done/failed`
-- （`schema_v5.sql:101-102`）改成规格六态
-- `pending/queued/running/review/done/failed`（`docs/architecture/task-model.md:35-40`）。
--
-- 存量行怎么迁 —— 三条旧态在规格六态里没有对应物，统一落 `pending`：
--   draft   = 「刚建、还没准备好派发」。那个态在 v5 的 `TaskState` 上已随本
--             迁移一并删除（`task.rs:69` 起说明了为什么 `FromStr` 不回落成
--             `pending`：迁移后库里再出现 `draft` 就是数据损坏，不是旧值）。
--             规格里 `pending` 的进入条件含「人建」，故 draft → pending。
--             **不是丢弃**：`butler.md:196` 说的「管家草稿箱」不产生 `Task`
--             记录，所以不存在需要丢弃的行。
--   ready   = 「可派发」。规格 `pending` 是「等被派活」，同义；而 `queued` 要求
--             已选中 `(device, agent)`，存量行没有这个选择，塞进 queued 是撒谎。
--   blocked = 「等人决策」。规格明确不单开第七态（`task-model.md:42-52`），
--             且 blocked 的行没有执行记录，不能迁到 `running` + `waiting_for_user`
--             （那是「有执行在跑、等人」）。落 pending 最保守。
--
-- `running` / `done` / `failed` 三态在两侧同义，原样保留，不改写。
--
-- 为什么一条 `UPDATE` 一句、不用 `CASE` 折叠：迁移是一次性读的代码，读的人要能
-- 一眼看出一行数据被改成了什么。`CASE` 省下的行数不值得。
--
-- 幂等：`UPDATE` 天然幂等（第二次跑时旧值已不存在，影响 0 行）；
-- `ALTER TABLE ADD COLUMN` **没有** `IF NOT EXISTS`，只靠 `migrate.rs` 的
-- `PRAGMA user_version` 闸门保证整条迁移跑一次。闸门说谎时撞上的正是
-- `duplicate column name`，整条 v7 在事务里回滚。

-- ---------------------------------------------------------------------------
-- 一、存量任务态归并（三条 UPDATE，理由见文件头）
-- ---------------------------------------------------------------------------

UPDATE task SET state = 'pending' WHERE state = 'draft';
UPDATE task SET state = 'pending' WHERE state = 'ready';
UPDATE task SET state = 'pending' WHERE state = 'blocked';

-- ---------------------------------------------------------------------------
-- 二、task 表补列（`task-model.md:363-383`）
-- ---------------------------------------------------------------------------
--
-- 三个约束列存**原文 JSON 字符串**，不在类型层解析：本里程碑没有消费者需要
-- 结构化，过早定型会把 v0.8.2 的调度字段钉死在今天。NULL = 不限。
ALTER TABLE task ADD COLUMN allowed_devices TEXT;
ALTER TABLE task ADD COLUMN allowed_agents TEXT;
ALTER TABLE task ADD COLUMN required_capabilities TEXT;

-- NULL = 用默认上限（2）。不给 DEFAULT，因为「没填」与「填了 2」在语义上不同：
-- 前者跟随默认值的未来调整，后者是用户显式选择。
ALTER TABLE task ADD COLUMN retry_limit INTEGER;

-- 乐观锁：`UPDATE task SET … WHERE id = ? AND state = <旧态> AND version = ?`
-- 做 CAS，影响 0 行即返回 409 `task_state_changed`。
--
-- **只对状态迁移成立**：`version` 的两条写路径（`transition_with` 与
-- `update_if_version`）连同 `TaskRepo::set_state` 这个转发入口都带 `version = ?`
-- 守卫。`TaskRepo` 之外直接摸 SQL 的地方（迁移脚本、测试夹具）不带——它们不是
-- 「两个请求抢一行」，没有可抢的对手。
--
-- 版本守卫**不能只在事后看涨了几**：改一个字段也可能涨 1，而插队 1–2 次的
-- 结果与「自己改的」不可区分。所以条件必须在 `WHERE` 里，影响 0 行就整体回滚，
-- 409 的语义才是「什么都没发生」。
ALTER TABLE task ADD COLUMN version INTEGER NOT NULL DEFAULT 1;

-- 被哪次执行取代（失败八形态里的 `superseded` 需要指回去）。
ALTER TABLE task ADD COLUMN superseded_by TEXT;

-- 任务从哪来（会话 / 收件箱条目），失败面板「怎么来的」那一段用。
ALTER TABLE task ADD COLUMN intent_ref TEXT;

-- ---------------------------------------------------------------------------
-- 三、execution 表补列（`task-model.md:385-396`）
-- ---------------------------------------------------------------------------
--
-- 全部可空：存量执行没有这些信息，无法反推。给 DEFAULT 等于凭空造事实。
ALTER TABLE execution ADD COLUMN agent_id TEXT;
ALTER TABLE execution ADD COLUMN failure_kind TEXT;
ALTER TABLE execution ADD COLUMN reason TEXT;
ALTER TABLE execution ADD COLUMN evidence TEXT;
-- `started_at` / `ended_at` 在 v0.8.0 **有写入函数但生产零调用**
-- （`ExecutionRepo::mark_started` / `mark_ended`）：没有派发器就没有「执行开始
-- 了」这个事件。列先加上是因为规格的时长统计要它们，且 `mark_started` 的
-- 「只填一次」口径已经写对并有单测——v0.8.2 接上执行端点即可用。
-- 这不是「有列没写者」：写者已经在，缺的只是调用点。
ALTER TABLE execution ADD COLUMN started_at TEXT;
ALTER TABLE execution ADD COLUMN ended_at TEXT;

-- **不加 `parent_execution_id`**：执行间接力归 v0.8.2，加空列只会让
-- 「有列没写者」变成下一轮的假线索。上面两列不同——它们的写者已经存在。
