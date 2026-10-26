-- AgentFleet Hub 存储 schema v3（M2：ACP 对话）
--
-- 三件事：
--   1. 去掉全部外键（用户裁定 2026-10-26：「不应该用外链」）
--   2. 新增 agent_runtime / agent_config / trajectory_event 三张表
--   3. 给 session 加 acp_session_id（Hub 存它，重连后用 ACP 的 session/load 恢复）
--
-- 幂等要求：靠 `migrate.rs` 的 `PRAGMA user_version` 闸门，不靠 DDL 本身
-- （理由与 schema_v2.sql 头注释相同）。
--
-- ---------------------------------------------------------------------------
-- 为什么去掉外键（用户裁定，不是实现偏好）
-- ---------------------------------------------------------------------------
--
-- v2 的两条外键在「删设备」时互相打架：
--   workspace.node_id    → device(id)    ON DELETE CASCADE   （要删工作区）
--   session.workspace_id → workspace(id) ON DELETE RESTRICT  （不让删工作区）
-- 于是「删一台有会话的设备」在数据库层根本执行不了。改 SET NULL 能绕开，
-- 但那样等于把产品决策永远写死在 DDL 里：以后每改一次删除语义就要迁一次表。
--
-- 更根本的是：**外键在做业务层已经做过的事**。`workspace.rs` 的 `delete()`
-- 早就显式查了「有没有会话指着它」并返回 `WorkspaceInUse`，注释原文是
-- 「这条业务规则写在 schema 的外键动作里没人看得见，写在这里才看得见」。
-- 外键在这里是重复实现，且只会甩一句 `FOREIGN KEY constraint failed`，
-- 用户看不懂。归属关系改由业务层显式检查并给可读错误。
--
-- `PRAGMA foreign_keys=ON` 仍然保留（`db.rs` 强制设置）——去掉的是这两条
-- 外键，不是关掉这个开关。关掉它会让将来误加的 REFERENCES 静默失效。
--
-- ---------------------------------------------------------------------------
-- 删除语义三分离（配套上面）
-- ---------------------------------------------------------------------------
--
--   卸载 agent  → **不删会话与轨迹**（「后续可能还安装回来」）
--   删除设备    → **不删会话与轨迹**，原 node_id 记进 removed_node_id
--   删除工作区  → 有会话指着就拒（业务层 `WorkspaceInUse`，已有）
--
-- `removed_node_id` 为什么不设外键：它记录的是「这个会话曾属于哪台已被移除
-- 的机器」，是一个历史事实，不是一条要维护的关系。设备回来了也是新的一行，
-- 旧对话不该自动接到新设备上。

-- ---------------------------------------------------------------------------
-- workspace 重建：去掉 node_id 的外键与 NOT NULL，加 removed_node_id
-- ---------------------------------------------------------------------------
--
-- SQLite 不支持 `ALTER TABLE ... DROP CONSTRAINT`，只能建新表 → 搬数据 →
-- 删旧表 → 改名。四步都在 `migrate.rs` 的同一个事务里，失败整体回滚。
--
-- `node_id` 改为可空：删设备后工作区行保留，node_id 置 NULL、removed_node_id
-- 记下原值。UNIQUE (node_id, path) 退化为「同一活跃设备上同一路径只有一行」——
-- 多个 NULL 在 SQLite 的唯一约束里**互不冲突**（NULL != NULL），正是我们要的：
-- 已移除设备留下的工作区不会互相撞。
CREATE TABLE workspace_v3 (
    id              TEXT PRIMARY KEY,
    node_id         TEXT,                    -- 设备被删后置 NULL
    removed_node_id TEXT,                    -- 原 node_id，仅供展示「已移除的设备」
    path            TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (node_id, path)
);

INSERT INTO workspace_v3 (id, node_id, removed_node_id, path, created_at)
SELECT id, node_id, NULL, path, created_at FROM workspace;

DROP TABLE workspace;

ALTER TABLE workspace_v3 RENAME TO workspace;

CREATE INDEX IF NOT EXISTS idx_workspace_node    ON workspace(node_id);
CREATE INDEX IF NOT EXISTS idx_workspace_removed ON workspace(removed_node_id)
    WHERE removed_node_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- session 重建：去掉 workspace_id 的外键，加 acp_session_id / removed_node_id
-- ---------------------------------------------------------------------------
--
-- `acp_session_id`：ACP 的会话 id（opencode 形如 `ses_...`）。Hub **只存这一个
-- 不透明字符串，不自建 agent 会话协议**（`session-and-trajectory.md` §2）。
-- 重连后拿它走 `session/load` 恢复上下文，不重建。
-- 可空：M1 的会话没有 agent，M2 也允许建无 agent 的会话（不退化）。
-- 没有它 e2e 的「断开重连恢复同一会话」断言无处读取。
--
-- `removed_node_id`：与 workspace 同义。会话也存一份，因为页面按会话列表
-- 展示时不该为了判断「设备还在不在」去 join 工作区。
CREATE TABLE session_v3 (
    id              TEXT PRIMARY KEY,
    workspace_id    TEXT NOT NULL,           -- 无外键；归属由业务层检查
    agent_id        TEXT,                    -- M2 起填：设备上已装且可用的 agent
    acp_session_id  TEXT,                    -- ACP 会话 id；无 agent 的会话恒 NULL
    removed_node_id TEXT,                    -- 原 node_id，设备被删后填
    state           TEXT NOT NULL DEFAULT 'running'
                    CHECK (state IN ('running','ended')),
    created_at      TEXT NOT NULL,
    ended_at        TEXT                     -- running 时恒 NULL
);

INSERT INTO session_v3 (id, workspace_id, agent_id, acp_session_id, removed_node_id,
                        state, created_at, ended_at)
SELECT id, workspace_id, agent_id, NULL, NULL, state, created_at, ended_at FROM session;

DROP TABLE session;

ALTER TABLE session_v3 RENAME TO session;

CREATE INDEX IF NOT EXISTS idx_session_workspace ON session(workspace_id);
CREATE INDEX IF NOT EXISTS idx_session_state     ON session(state);
CREATE INDEX IF NOT EXISTS idx_session_agent     ON session(agent_id)
    WHERE agent_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- agent_runtime：设备上**扫描到**了哪些 agent（心跳上报）
-- ---------------------------------------------------------------------------
--
-- 「扫描」是这张表的唯一来源：设备探测本机 PATH 里有什么、版本多少、
-- 能不能跑，然后整体上报。**Hub 不往设备上装任何东西**——agent 由用户
-- 自己 `npm i -g` / 下二进制 / 用系统包管理器装。
--
-- 这一决定删掉了原先的两列，值得写清为什么：
--
-- - `install_source ∈ {detected, template}`：只为区分「用户自己装的」与
--   「Hub 模板装的」。不再有 Hub 装的那一种，这一列恒为同一个值，
--   留着只会让每个读者去猜另一种什么时候出现。
-- - `install_state ∈ {none, installing, ready, failed}`：表达「正在装”。
--   Hub 不发安装，就没有「正在装」这个中间态可表达。
--
-- `install_error` 保留下来改名 `probe_error`：它承接的事实**变了但没消失**
-- ——「扫描到了但跑不起来」仍然需要一个可读的原因（退出码、缺动态库），
-- 否则页面上只有一个 available=false，用户无从下手。
--
-- **本表不是 append-only**：一次扫描整体覆盖这台设备的全部行。
-- append-only 只约束 `trajectory_event`。
CREATE TABLE IF NOT EXISTS agent_runtime (
    node_id     TEXT NOT NULL,
    agent_id    TEXT NOT NULL,             -- 探测出的稳定标识（见 detect.rs）
    name        TEXT NOT NULL,
    version     TEXT NOT NULL,             -- 没探到写空串（列 NOT NULL）
    available   INTEGER NOT NULL DEFAULT 0, -- 现在能不能对话
    probe_error TEXT,                      -- 「装了但跑不起来」的可读原因；其余 NULL
    detected_at TEXT NOT NULL,             -- 新鲜度：门禁判「这次扫描是什么时候的」
    PRIMARY KEY (node_id, agent_id)
);

CREATE INDEX IF NOT EXISTS idx_agent_runtime_node ON agent_runtime(node_id);

-- ---------------------------------------------------------------------------
-- agent_config：每个 agent 的**配置**（Hub 存，设备读）
-- ---------------------------------------------------------------------------
--
-- 这是「取消安装」之后 Hub 侧真正要管的东西。用户在自己的机器上装好
-- agent，然后在页面上配它怎么被启动：命令、参数、环境变量。
--
-- 为什么配置在 Hub 而不是设备：换机器、重装 agent 之后配置还在；而且
-- 用户只需要在网页上改一处，不必 SSH 到每台设备上编辑文件。
--
-- **配置不是代码**：这几列全是字符串与字符串数组，Hub 下发的是「怎么启动
-- 一个用户已经装好的程序」，不是「一段要被执行的程序」。这正是取消安装
-- 后风险等级降下来的地方——没有可执行文件从 Hub 流向设备。
--
-- `agent_id` 是主键的一部分而不是单独的配置表：配置属于「这台设备上的
-- 这个 agent」。同一台设备上不同 agent 的参数不能互相污染，而同一个 agent
-- 在不同设备上可能需要不同参数（路径不同）。
--
-- 扫描不到的 agent 也可以有配置行：用户可能先配好、后装。所以本表与
-- `agent_runtime` 之间**没有外键**，两者各自独立。
CREATE TABLE IF NOT EXISTS agent_config (
    node_id     TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    -- 启动命令。空串 = 用扫描到的可执行文件（`agent_runtime` 里那个）。
    -- 允许覆盖是因为用户可能把它装在 PATH 之外。
    command     TEXT NOT NULL DEFAULT '',
    -- 启动参数，JSON 字符串数组（如 `["--acp"]`）。
    args_json   TEXT NOT NULL DEFAULT '[]',
    -- 环境变量，JSON 对象（如 `{"FOO":"bar"}`）。**值是明文**：
    -- 密钥类的不该放这里，本里程碑没有加密存储，也不能假装有。
    env_json    TEXT NOT NULL DEFAULT '{}',
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (node_id, agent_id)
);

-- ---------------------------------------------------------------------------
-- trajectory_event：ACP 的 session/update 事件流（append-only）
-- ---------------------------------------------------------------------------
--
-- 五个硬要求（`session-and-trajectory.md` §3.2）逐列落地：
-- 类型 / 输入 / 输出 / 耗时 / 归属节点。缺一列就「只能看不能审计」。
--
-- `duration_ms` **允许 NULL**，这是实测逼出来的：本机 opencode v2.1.4 的
-- `tool_call` / `tool_call_update` / `agent_message_chunk` / `usage_update`
-- payload 里**没有任何时间戳字段**（`tool_call` 只有 status/rawInput/rawOutput，
-- `usage_update` 只有 used/size/cost）。设备侧只能拿「收到事件的时刻」相邻差分，
-- 那衡量的是**事件间隔**而不是**工具执行耗时**——写进 duration_ms 就是假数据。
-- `duration_source` 标明这一列是怎么来的（`measured` / `none`），让审计看得懂。
--
-- `seq` 是**设备侧**生成的会话内序号：ACP 通知本身不带序号，而 ACP 子进程
-- 重启后是设备从 0 重数。所以必须配 UNIQUE 约束让重复写入被数据库拒掉，
-- 而不是靠应用层「先查再插」（并发/重连下那是 TOCTOU）。
--
-- 未知事件类型必须原样存：`kind` 不加 CHECK 白名单。新版本 agent 会推
-- 我们现在不认识的事件，丢弃等于让它们静默消失。
CREATE TABLE IF NOT EXISTS trajectory_event (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,  -- 回放顺序 = 物理顺序
    session_id      TEXT NOT NULL,
    node_id         TEXT NOT NULL,           -- 归属节点（硬要求）
    seq             INTEGER NOT NULL,        -- 设备侧会话内序号
    kind            TEXT NOT NULL,           -- available_commands_update / tool_call / ...
    payload         TEXT NOT NULL,           -- 原始 JSON（输入 + 输出）
    duration_ms     INTEGER,                 -- 耗时；结构性无耗时为 NULL
    duration_source TEXT NOT NULL DEFAULT 'none'
                    CHECK (duration_source IN ('measured', 'none')),
    tokens_in       INTEGER,
    tokens_out      INTEGER,
    parent_event_id INTEGER,                 -- 子 agent 事件挂父事件
    created_at      TEXT NOT NULL,
    UNIQUE (session_id, node_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_trajectory_session
    ON trajectory_event(session_id, id);
CREATE INDEX IF NOT EXISTS idx_trajectory_kind
    ON trajectory_event(session_id, kind);
