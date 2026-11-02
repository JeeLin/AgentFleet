-- AgentFleet Hub 存储 schema v4（v0.6.0：任务基准 / 会话起点）
--
-- 一件事：给 session 加两列，记下「这次会话基于哪个 rev 开始」以及
-- 「起点为什么没有」。
--
--   base_rev         TEXT             完整 40 位 hash；没有起点时为 NULL
--   base_rev_status  TEXT NOT NULL   五态之一，DEFAULT 'unknown'
--
-- 幂等要求：`ALTER TABLE ADD COLUMN` **没有** `IF NOT EXISTS` 形式，重复执行会
-- 报 `duplicate column name`。所以本文件与 schema_v2.sql 一样，靠 `migrate.rs`
-- 的 `PRAGMA user_version` 闸门保证只跑一次，不靠 DDL 本身。
--
-- ---------------------------------------------------------------------------
-- 为什么 base_rev_status 的默认值是 'unknown' 而不是 'ok'
-- ---------------------------------------------------------------------------
--
-- `ALTER TABLE ... ADD COLUMN ... DEFAULT` 会把**存量行**一起填上默认值。
-- 填 'ok' 的话，v0.6.0 之前建的每一个会话都会声称「我的起点是这个」，而
-- base_rev 是 NULL——一个看起来正常、实际毫无意义的状态。'unknown' 才是
-- 「我不知道」，与老会话的事实相符。
--
-- **'unknown' 与 'unavailable' 绝不合并**：前者是「那时我们还没有这个功能」，
-- 后者是「试过了、设备没给」。合并会把一个历史事实显示成一次故障，把用户
-- 引去排查一个并不存在的问题。
--
-- ---------------------------------------------------------------------------
-- 为什么约束写在 Rust 侧而不在库里加 CHECK
-- ---------------------------------------------------------------------------
--
-- SQLite 的 `ALTER TABLE ADD COLUMN` 可以带 CHECK，但存量行会拿 DEFAULT 去过
-- 检查。而这里真正需要的是「只在值非 unknown 时才校验 base_rev 非空」这种
-- 条件约束，ADD COLUMN 表达不了。取值合法性由 Rust 枚举的 FromStr 兜住
-- （解析失败报 StorageError，与 SessionState::from_str 同一条口径），
-- 不在库里再写第二份判据。

ALTER TABLE session ADD COLUMN base_rev TEXT;

ALTER TABLE session ADD COLUMN base_rev_status TEXT NOT NULL DEFAULT 'unknown';
