-- AgentFleet Hub 存储 schema v2（M1：工作台会话 + 设备公钥真列）
--
-- 幂等要求：`CREATE TABLE` / `CREATE INDEX` 都写了 `IF NOT EXISTS`。
-- **`ALTER TABLE ADD COLUMN` 写不了 `IF NOT EXISTS`**（SQLite 的 DDL 里没有这个
-- 形式），所以本文件**不是**靠 DDL 本身幂等的，而是靠 `migrate.rs` 的
-- `PRAGMA user_version` 闸门：每条迁移只跑一遍，而闸门一旦失效
-- （`user_version` 被手工改过、库被别的版本的文件灌过）撞上的正是
-- `duplicate column name`，整条 v2 在事务里回滚。
-- 闸门失效时的正确反应是报错停下（`SchemaTooNew`），不是重试——
-- `migrate.rs` 的 `the_migration_gate_holds_and_its_lie_is_refused_before_touching_anything`
-- 锁的就是这条。

-- ---------------------------------------------------------------------------
-- device.public_key：把 M0 的权宜之计提成真列
-- ---------------------------------------------------------------------------
--
-- M0 没有这一列，Hub 只能把公钥寄存在 `capabilities` 的 `public_key` 键里，
-- 于是「心跳合并必须保住它」成了一条只有注释守着的约定
-- （docs/BUGS.md）。这里把它提成真列并回填（见下方 UPDATE）。
--
-- 保留 JSON 键是**有意的**，不是为了留后路：M2 的工作区 manifest、
-- 设备指纹校验都会直接读这一列，而旧二进制写不进来——降级会撞上
-- `SchemaTooNew` 拒绝开库。
ALTER TABLE device ADD COLUMN public_key TEXT;

-- 存量回填：v1 时代公钥只存在于 capabilities JSON 里。抽不出来就留 NULL
-- （列里没有公钥的设备会被拒绝重连，比拿一个错的公钥去验签安全）。
UPDATE device
   SET public_key = json_extract(capabilities, '$.public_key')
 WHERE public_key IS NULL
   AND capabilities IS NOT NULL
   AND json_valid(capabilities)
   AND json_type(capabilities, '$.public_key') = 'text';

-- ---------------------------------------------------------------------------
-- workspace：设备上的一个 `(node_id, path)`
-- ---------------------------------------------------------------------------
--
-- 工作区是**主实体**，会话引用它（`session.workspace_id`）：同一个
-- `(node_id, path)` 可以被多个会话同时引用，唯一约束保证它们指向同一行，
-- 而不是各建一份互相看不见的副本。裁决理由见 M1 里程碑 §5c。
--
-- `path` 存设备上的路径原文（`~` 开头或 `/` 开头），不做展开——Hub 没有那台
-- 设备的文件系统，展开只能靠猜（约束 1：Hub 不做设备入站访问）。
CREATE TABLE IF NOT EXISTS workspace (
    id          TEXT PRIMARY KEY,     -- uuid v4（无连字符，32 位小写 hex）
    node_id     TEXT NOT NULL REFERENCES device(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    UNIQUE (node_id, path)            -- 「同一个工作区只有一行」是并发建会话的前提
);

CREATE INDEX IF NOT EXISTS idx_workspace_node ON workspace(node_id);

-- ---------------------------------------------------------------------------
-- session：工作台会话（一个设备上的一个工作区 + 可选 agent）
-- ---------------------------------------------------------------------------
--
-- 与登录 cookie 无关——`secret` 表里的 session_hmac 管的是登录态，这里管的是
-- 「用户在某个设备的某个目录里开着的那个工作台」。工作区在创建时选定后锁定
-- （design-system.md §6.2 的 `path_locked`），所以这一行此后不再改 path。
--
-- `agent_id` 可空：M1 没有 agent 运行时（ACP 归 M3），建会话时选不选 agent
-- 都成立。`ON DELETE RESTRICT` 而非 CASCADE：删工作区要显式走仓储层的检查，
-- 「关一个面板」不该悄悄连带删掉别的会话正在用的工作区。
CREATE TABLE IF NOT EXISTS session (
    id            TEXT PRIMARY KEY,
    workspace_id  TEXT NOT NULL REFERENCES workspace(id) ON DELETE RESTRICT,
    agent_id      TEXT,                    -- M1 恒 NULL；M3 起填
    state         TEXT NOT NULL DEFAULT 'running'
                  CHECK (state IN ('running','ended')),
    created_at    TEXT NOT NULL,
    ended_at      TEXT                     -- running 时恒 NULL
);

CREATE INDEX IF NOT EXISTS idx_session_workspace ON session(workspace_id);
CREATE INDEX IF NOT EXISTS idx_session_state     ON session(state);