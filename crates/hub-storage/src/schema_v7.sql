-- AgentFleet Hub 存储 schema v7：任务态对齐规格六态 + 本里程碑所需列
--
-- 目标：`task.state` 从 v5 的六值 `draft/ready/running/blocked/done/failed`
-- （`schema_v5.sql:101-102`）改成规格六态
-- `pending/queued/running/review/done/failed`（`docs/architecture/task-model.md:35-40`）。
--
-- 存量行怎么迁 —— 三条旧态在规格六态里没有对应物，统一落 `pending`：
--   draft   = 「刚建、还没准备好派发」（`task.rs:26-27`）。规格里 `pending` 的
--             进入条件含「人建」，故 draft → pending。
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

-- 乐观锁：`UPDATE ... WHERE state = <旧态> AND version = ?` 做 CAS，
-- 影响 0 行即返回 409 `task_state_changed`。
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
ALTER TABLE execution ADD COLUMN started_at TEXT;
ALTER TABLE execution ADD COLUMN ended_at TEXT;

-- **不加 `parent_execution_id`**：执行间接力归 v0.8.2，加空列只会让
-- 「有列没写者」变成下一轮的假线索。
