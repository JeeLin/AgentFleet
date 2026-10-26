-- AgentFleet Hub 存储 schema v3（M2：ACP 对话）
--
-- 三件事：
--   1. 去掉全部外键（用户裁定 2026-10-26：「不应该用外链」）
--   2. 新增 agent_runtime / agent_template / blob / trajectory_event 四张表
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
-- agent_runtime：设备上装了哪些 agent（心跳上报，来自设备侧探测）
-- ---------------------------------------------------------------------------
--
-- PK 含 `install_source`，这一列不是冗余：
--
-- 同一个可执行名可能来自两个地方——用户在 PATH 里自己装的（`detected`），
-- 和 Hub 模板装到固定目录的（`template`）。若 PK 只有 (node_id, agent_id)，
-- 两者会撞成一行，后写的覆盖前写的：模板装完回报就绪时会**掩蔽**用户自带
-- 那份的版本事实，页面上看起来像「agent 被升级了」，而本里程碑明确不做升级。
--
-- `available` 只有两态不够表达「已装但这次探测超时」，所以另有 `install_state`。
-- `install_error` 承接「安装失败只上报原因」的那句「原因」——没有它，
-- 原因报给谁、存哪都没有答案。
--
-- **本表不是 append-only**：探测是 upsert，卸载是 DELETE。append-only 只约束
-- `trajectory_event`（v2 文档里「只追加」紧跟轨迹表，主语曾被误读为盖住本表）。
CREATE TABLE IF NOT EXISTS agent_runtime (
    node_id        TEXT NOT NULL,
    agent_id       TEXT NOT NULL,            -- 探测出的稳定标识（见 detect.rs）
    install_source TEXT NOT NULL DEFAULT 'detected'
                   CHECK (install_source IN ('detected', 'template')),
    name           TEXT NOT NULL,
    version        TEXT NOT NULL,
    available      INTEGER NOT NULL DEFAULT 0,  -- 现在能不能对话（探测 + ACP 可用）
    install_state  TEXT NOT NULL DEFAULT 'ready'
                   CHECK (install_state IN ('none', 'installing', 'ready', 'failed')),
    install_error  TEXT,                     -- failed 时的可读原因；其余为 NULL
    detected_at    TEXT NOT NULL,            -- 新鲜度：门禁用它判「这次探测是什么时候的」
    PRIMARY KEY (node_id, agent_id, install_source)
);

CREATE INDEX IF NOT EXISTS idx_agent_runtime_node ON agent_runtime(node_id);

-- ---------------------------------------------------------------------------
-- agent_template：Hub 侧登记的安装模板
-- ---------------------------------------------------------------------------
--
-- 「模板」= 装什么（kind / source）+ 怎么验（checksum）+ 需要什么运行时。
--
-- 产品边界写在这里是为了让人看得见：**安装永远由用户手动触发**，
-- Hub 不得因为「发现某设备缺某 agent」就自动下发（workspace-and-sync.md §4.5）。
--
-- `blob_hash` 可空：安装可以走 `install_source` 直连（npm / URL），
-- 安装包的 blob 托管是后置加固，不是安装的前置条件。
CREATE TABLE IF NOT EXISTS agent_template (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    install_kind   TEXT NOT NULL
                   CHECK (install_kind IN ('npm', 'url', 'builtin')),
    install_source TEXT NOT NULL,
    checksum       TEXT,                     -- 安装包的期望校验和；builtin 可空
    runtime_req    TEXT,                     -- JSON：需要的运行时（如 node >= 20）
    blob_hash      TEXT,                     -- 托管到 blob 后填，见子任务 10
    created_at     TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- blob：agent 安装包的内容寻址存储（**只此一用**）
-- ---------------------------------------------------------------------------
--
-- 原 M2 的 CAS 供「项目文件 / 配置 / 产物」三样东西，整体撤销（2026-10-26）。
-- 保留的只有 agent 安装包这一点，因为「Hub 下发安装」绕不开它：
-- 把二进制塞进 WS 帧会打断设备连接，直传又没有校验和防线。
--
-- 只做最小形态：put / get / 校验。**不做** snapshot、manifest 差分、
-- refcount GC、四档传输。
--
-- `refcount` 只是一列，没有后台 GC——加它是为了让「孤儿 blob」可被发现
-- （删模板后它的包还占着盘），不是为了现在就做回收。
-- `ref_path` 存物理路径：大二进制不进 DB，DB 只存索引。
CREATE TABLE IF NOT EXISTS blob (
    hash       TEXT PRIMARY KEY,             -- sha256 hex（64 字符小写）
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    ref_path   TEXT NOT NULL,                -- 相对数据目录，如 `cas/ab/<hash>`
    refcount   INTEGER NOT NULL DEFAULT 0 CHECK (refcount >= 0),
    created_at TEXT NOT NULL
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
