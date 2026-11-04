-- AgentFleet Hub 存储 schema v5（v0.7.0：项目与绑定）
--
-- 五件事：
--   1. 新增 project / binding / task / execution 四张表
--   2. 给 session 加 execution_id（会话从顶层降为 Execution 的子实体）
--   3. 建表顺序按依赖：project → binding → task → execution
--
-- 幂等要求：`CREATE TABLE` / `CREATE INDEX` 都写了 `IF NOT EXISTS`；
-- **`ALTER TABLE ADD COLUMN` 写不了 `IF NOT EXISTS`**，故本文件与 v2/v4 一样
-- 靠 `migrate.rs` 的 `PRAGMA user_version` 闸门保证只跑一次，不靠 DDL 本身。
-- 闸门失效时撞上的正是 `duplicate column name`，整条 v5 在事务里回滚。
--
-- ---------------------------------------------------------------------------
-- 为什么仍然不加外键（延续 v3 的裁定，不是遗漏）
-- ---------------------------------------------------------------------------
--
-- v3 已把外键全部去掉，理由是「外键在做业务层已经做过的事，且只甩一句
-- FOREIGN KEY constraint failed，用户看不懂」。四张新表沿用同一条口径：
--
--   删项目      → **不提供**（`PRODUCT.md` §3.2：可归档不可删除）
--   删设备      → 有历史执行记录就拒（业务层返回可读原因，不是 DB 报错）
--   删绑定      → 有执行指着就拒（同上）
--
-- 这些规则写在 `project.rs` / `binding.rs` 里而不是 DDL 里，与 v3 的理由
-- 完全一致：DDL 里的约束没人读得到，业务层给得出「这台设备有 12 条历史
-- 执行记录」这样的句子。
--
-- ---------------------------------------------------------------------------
-- 为什么 shape 用 TEXT + Rust 侧校验，不做成三张表 / 不做成 CHECK
-- ---------------------------------------------------------------------------
--
-- `PRODUCT.md` §3.2 原文：「**形态是项目的一个属性，不是三种产品**」。
-- 三张表会让 Task / Binding / 看板 / 管家各写三份；CHECK 也不是不行，但
-- `PATCH` 里 evolution 规则（只有 isolated → central 单向）本来就只能在
-- Rust 侧表达，把取值校验拆成两处只会让「哪个值合法」有两个答案。
-- 取值校验统一在 `project.rs` 的 `ProjectShape::from_str`。
CREATE TABLE IF NOT EXISTS project (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    -- 'central' | 'mirror' | 'isolated'。见 project.rs 的 ProjectShape。
    shape        TEXT NOT NULL,
    -- 主题色：'blue' | 'green' | 'purple' | 'orange' | 'gray' | NULL。
    -- 只做视觉区分，**不影响任何业务逻辑**（PRODUCT.md §3.2）。
    theme_color  TEXT,
    -- 非空即已归档。归档后不出现在列表与看板、任务停止派发，
    -- 但历史执行 / 管家总结 / 轨迹全部保留，且可随时取消归档。
    archived_at  TEXT,
    created_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_project_archived ON project(archived_at);

-- ---------------------------------------------------------------------------
-- binding：一个 (project, device) 钉死一个目录
-- ---------------------------------------------------------------------------
--
-- `UNIQUE (project_id, device_id)` 是这张表的**全部意义**：同一个项目在同一台
-- 设备上只有一个固定目录。不能为不同任务绑不同目录，否则清理与配额都失去
-- 意义（里程碑 §4）。
--
-- `fixed_dir` 是设备上的路径原文，与 `workspace.path` 同一口径：不展开 `~`，
-- Hub 没有那台设备的文件系统。
--
-- `cleanup_policy`：'conservative'（保守，留 3 个产物）| 'balanced'（平衡）
-- | 'aggressive'（激进，跑完即清）。三档语义见 `workspace-and-sync.md`。
CREATE TABLE IF NOT EXISTS binding (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL,
    device_id        TEXT NOT NULL,
    fixed_dir        TEXT NOT NULL,
    -- 配额上限。NULL = 不限额（新绑定默认不限额，用户显式设才拦）。
    disk_quota_bytes INTEGER,
    cleanup_policy   TEXT NOT NULL DEFAULT 'balanced',
    created_at       TEXT NOT NULL,
    -- 唯一约束落在**业务键**上，不是 id 上：同一 (project, device) 第二次写入
    -- 必须更新同一行（PUT 幂等 upsert），而不是插一行新的。
    UNIQUE (project_id, device_id)
);

CREATE INDEX IF NOT EXISTS idx_binding_project ON binding(project_id);
CREATE INDEX IF NOT EXISTS idx_binding_device  ON binding(device_id);

-- ---------------------------------------------------------------------------
-- task：看板上的一张卡片
-- ---------------------------------------------------------------------------
--
-- 六态见 `task-model.md` §2。`state` 存 TEXT、取值校验在 Rust 侧
-- （`task.rs` 的 `TaskState::from_str`），与 shape 同一个理由。
--
-- `done_when` 拆成两列而不是一个 JSON：`criteria` 是**判定码**
-- （`all_tests_pass` / `review_approved` / …），`description` 是给人看的一句话。
-- 合成 JSON 会让「按判定码统计哪种验收最常失败」变成一次 JSON 解析。
--
-- `base_commit` 是任务派发时的起点。与 `execution.base_commit` 的关系：
-- **execution 的那一份才是唯一真相源**，task 这一份是「默认起点」，
-- 给「还没派发时页面显示什么」用（v0.6.0 交接规则，见里程碑 Context）。
CREATE TABLE IF NOT EXISTS task (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL,
    title        TEXT NOT NULL,
    -- 'draft' | 'ready' | 'running' | 'blocked' | 'done' | 'failed'
    state        TEXT NOT NULL DEFAULT 'draft',
    done_when_criteria    TEXT,
    done_when_description TEXT,
    created_by   TEXT,
    base_commit  TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_task_project ON task(project_id);
CREATE INDEX IF NOT EXISTS idx_task_state   ON task(state);

-- ---------------------------------------------------------------------------
-- execution：一次「把这个任务放到某台设备上跑」
-- ---------------------------------------------------------------------------
--
-- `Task:Execution = 1:N`——同一个任务可以跑多次（失败重试、换设备重跑），
-- 每次执行各有自己的起点与 worktree。
--
-- `base_commit` 是**起点唯一真相源**（v0.6.0 交接规则）。`session.base_rev`
-- 降级为只读快照，不再由新代码写入作为权威。判据：任何一个 execution 的起点
-- 只从本列读，不从 session 读。
--
-- `worktree_path` 是设备上的绝对路径（`<fixed_dir>/<execution_id>` 一类的形态，
-- 具体拼法由设备侧决定），Hub 只存不解释。
--
-- 不设 `ended_at`：执行结束与否由 `state` 表达，时间戳按需从轨迹推。
-- 多存一个可能与 state 不一致的列，就是给「正在跑但 ended_at 非空」留口子。
CREATE TABLE IF NOT EXISTS execution (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL,
    binding_id    TEXT NOT NULL,
    base_commit   TEXT,
    worktree_path TEXT,
    -- 'pending' | 'running' | 'succeeded' | 'failed' | 'aborted'
    state         TEXT NOT NULL DEFAULT 'pending',
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_execution_task    ON execution(task_id);
CREATE INDEX IF NOT EXISTS idx_execution_binding ON execution(binding_id);
CREATE INDEX IF NOT EXISTS idx_execution_state   ON execution(state);

-- ---------------------------------------------------------------------------
-- session.execution_id：会话从顶层降为 Execution 的子实体
-- ---------------------------------------------------------------------------
--
-- **可空，且老会话保持 NULL，不回填。** 这是步骤2 审查后写死的决定
-- （里程碑子任务 1「老会话不回填」）：
--
--   v0.6.0 时代的会话没有 Task、没有 Binding、没有 worktree。硬造一条
--   execution 只能把 project_id / task_id 编出来，而编出来的外键会让
--   「失败归因」「成本统计」从第一天就带脏数据。
--
-- 可见性后果（文档写明，不是实现遗漏）：
--   老会话**仍出现在会话列表**（GET /api/sessions 不变），base_rev 仍可显示；
--   **不出现**在「项目 → 执行」视图与看板里；
--   打开时不报错，页面按「此会话早于项目主轴」提示一句。
--
-- 默认值必须是 NULL 而不是某个占位串：`ALTER TABLE ADD COLUMN` 会把默认值
-- 填给**全部存量行**，填任何非 NULL 值都等于宣称「这个老会话属于某次执行」。
ALTER TABLE session ADD COLUMN execution_id TEXT;

CREATE INDEX IF NOT EXISTS idx_session_execution ON session(execution_id)
    WHERE execution_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- session.base_rev 降级为只读快照（v0.6.0 交接规则）
-- ---------------------------------------------------------------------------
--
-- 这里**不改 DDL**：列留着，历史数据留着，读路径不受影响。
-- 「只读」是写路径的规则（`SessionRepo::set_base_rev` 的调用方），不是列约束
-- ——加 NOT NULL / CHECK 会去动存量行，而存量行恰恰是这次要保的那批。
--
-- 记在这里是为了让下一个读这份 schema 的人知道：看到 session.base_rev 非空，
-- 不代表它还是起点；起点去 execution.base_commit 找。
