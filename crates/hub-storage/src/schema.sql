-- AgentFleet Hub 存储 schema
--
-- 迁移按 `PRAGMA user_version` 驱动（见 `migrate.rs`），每个版本一个文件：
-- v1 = 本文件，v2 = `schema_v2.sql`。**不要把新表加进 v1**——已存在的库
-- 停在 `user_version = 1`，改 v1 不会作用到它们。
--
-- 表清单与列语义见 docs/architecture/README.md §3。

-- ---------------------------------------------------------------------------
-- v1: 初始 schema（M0：设备接入 + 登录态）
-- ---------------------------------------------------------------------------
--
-- v1 的 session 是**签名 cookie**（HMAC 密钥取自
-- secret(kind='hub',name='session_hmac')），所以 v1 没有服务端 session 表。
-- M1 的 `session` 表是**工作台会话**（一个设备上的一个工作区 + agent），
-- 两者不是一回事，见 schema_v2.sql。

CREATE TABLE IF NOT EXISTS device (
    id              TEXT PRIMARY KEY,   -- node_id，128 位随机 base64url（43 字符）
    name            TEXT NOT NULL,
    key_fingerprint TEXT NOT NULL UNIQUE, -- Ed25519 公钥指纹（hex）；UNIQUE 是防重复注册的硬约束
    enrolled_at     TEXT NOT NULL,
    last_seen_at    TEXT,
    status          TEXT NOT NULL DEFAULT 'offline'
                    CHECK (status IN ('online','offline')),
    last_seen_gen   INTEGER NOT NULL DEFAULT 0,  -- 最近一次连接的 gen，用于 auth_fail{superseded} 兜底
    superseded_at   TEXT,                        -- 被顶替的时间戳；重启后仍生效
    os              TEXT,
    arch            TEXT,
    hostname        TEXT,
    node_version    TEXT,
    capabilities    TEXT,             -- JSON：支持的 connector 及参数（心跳更新）
    resources       TEXT              -- JSON：CPU/内存/GPU/当前负载（调度用）
);

CREATE INDEX IF NOT EXISTS idx_device_fingerprint ON device(key_fingerprint);
CREATE INDEX IF NOT EXISTS idx_device_status      ON device(status);

-- Hub 自身的秘密：登录密码哈希、session HMAC 密钥、模型 key。
-- 明文存储是约束 13 的取舍，配套防护：DB 0600 / 数据目录 0700 / umask 077。
CREATE TABLE IF NOT EXISTS secret (
    kind        TEXT NOT NULL,        -- 'hub' / 'connector' / 'model'
    name        TEXT NOT NULL,        -- 如 'password' / 'session_hmac'
    value       TEXT NOT NULL,        -- 明文（或 argon2id 哈希，见 kind='hub' name='password'）
    created_at  TEXT NOT NULL,
    PRIMARY KEY (kind, name)
);

-- ---------------------------------------------------------------------------
-- v2: 工作台（M1）
-- ---------------------------------------------------------------------------
--
-- `device.public_key` 与 workspace / session 两张表的 DDL 在 `schema_v2.sql`。
-- 留在 v1 里改不动已存在的库——迁移按 `user_version` 只跑一遍。
