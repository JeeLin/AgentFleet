# 架构总览 / Architecture Overview

> 决策细节见：`ai-gateway.md`（**推理主线：AI Gateway**）、`session-and-trajectory.md`（会话与轨迹）、
> `task-continuity.md`（任务连贯性）、`workspace-and-sync.md`（工作区供料与同步）、
> `scheduling.md`（任务分发调度）、`security-and-lifecycle.md`（安全与生命周期）。
> Detail lives in: ai-gateway.md, session-and-trajectory.md, task-continuity.md,
> workspace-and-sync.md, scheduling.md, security-and-lifecycle.md.
> `inference.md` is archived local-inference research, not the primary direction.

---

## 1. 进程模型

两个二进制/角色，Hub 与 node 都是单二进制：

```text
node（设备 A）══主动外联══WSS══> Hub <══HTTPS/WSS══ 浏览器
node（设备 B）══主动外联══WSS══> Hub        （Hub 托管前端）
node（设备 C）══主动外联══WSS══> Hub
```

- **Hub**：注册表（持久）+ 连接注册表（内存）+ 会话/任务编排 + 推理网关（后续）。
- **node**：设备接入进程。每条 WebSocket **由 node 主动建立**，承载控制 + 数据通道。
- **只有一条对外链路**：浏览器 ↔ Hub、node → Hub。**浏览器从不直连 node**；
  Hub 不拨号到设备，设备也不监听任何对外端口。

## 2. 连接与通道

- 每设备一条 WebSocket，控制 + 数据复用。
- 通道键 `(node_id, channel_id)`；每通道独立流控/信用。一个跑飞的传输不拖死该设备全部通道。
- 心跳即能力上报：os / arch / hostname / node 版本 / 支持的 connector / agent / 资源余量（供调度）。**字段清单见 `scheduling.md` §2**。
- 认证：设备密钥对（注册时生成，Hub 记指纹）。不用静态共享 token。

## 3. 数据模型（初版）

```text
device(
  id              TEXT PK,   -- node_id，注册时 Hub 签发
  name            TEXT,
  key_fingerprint TEXT,      -- 设备公钥指纹
  enrolled_at, last_seen_at TEXT,
  status          TEXT,      -- online / offline（仅两值；「重连中」只在内存，见 security §5）
  last_seen_gen   INTEGER,   -- 最近一次连接的 gen，用于 auth_fail{superseded} 兜底
  superseded_at   TEXT,      -- 被顶替的时间戳；重启后仍生效
  os, arch, hostname, node_version,
  capabilities    TEXT,      -- JSON：支持的 connector 及参数（心跳更新，见 scheduling §2）
  resources       TEXT,      -- JSON：CPU/内存/GPU/当前负载（调度用）
  public_key      TEXT       -- 设备公钥 base64url(32)；M1 的 v2 迁移提的真列（见 BUGS.md）
)

secret(          -- M0 建：Hub 自身的秘密（登录密码哈希、session HMAC 密钥、模型 key）
  kind            TEXT,      -- 'hub' / 'connector' / 'model'
  name            TEXT,      -- 如 'password' / 'session_hmac' / 'sk-...'
  value           TEXT,      -- 明文（约束 13 的取舍，配套 0600/0700/umask 077）
  created_at      TEXT,
  UNIQUE(kind, name)
)
```

- **M0 只建 `device` + `secret` 两张表。**
  **不建 `session` 表**：M0 的 session 是**签名 cookie**（HMAC 密钥取自
  `secret(kind='hub',name='session_hmac')`，见 `security-and-lifecycle.md` §2.4.2），
  无服务端 session 存储需求。若要支持「服务端强制注销 / 踢下线 / 查看活跃会话」，
  随 M1 加表（`session(id, created_at, last_seen_at, user_agent)`），
  并把签发改为「库里存一条 + 签名 cookie 带 id」的混合形态。
- **M1 的 v2 迁移加了两张表 + 一列**（`crates/hub-storage/src/schema_v2.sql`）：
  ```text
  workspace(     -- 设备上的一个 (node_id, path)，工作区是主实体
    id            TEXT PK,
    node_id       TEXT NOT NULL REFERENCES device(id) ON DELETE CASCADE,
    path          TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    UNIQUE (node_id, path)      -- 「同一个工作区只有一行」是并发建会话的前提
  )

  session(       -- **工作台会话**，不是上面那个登录 cookie
    id            TEXT PK,
    workspace_id  TEXT NOT NULL REFERENCES workspace(id) ON DELETE RESTRICT,
    agent_id      TEXT,        -- M1 恒 NULL；M3 接 ACP 起填
    state         TEXT NOT NULL CHECK (state IN ('running','ended')),
    created_at    TEXT NOT NULL,
    ended_at      TEXT         -- running 时恒 NULL
  )
  ```
  加 `device.public_key TEXT`（见上），并把存量 `capabilities.public_key` 回填进去。
  两处易混：**登录 cookie 与工作台会话是两回事**（前者靠 `secret` 表的 HMAC 密钥
  自包含，后者是 Hub 库里的一行）；**工作区与 session 是主从**（见 §4.1）。
  迁移按 `PRAGMA user_version` 驱动（v1 `schema.sql` / v2 `schema_v2.sql`），
  幂等性靠「每条只跑一遍」而不是靠 DDL 本身。
- **没有** environment 表；**没有** resource 表（connector 是有类型的，不是多态实体）。
- **`capabilities` 必须落库**：它是路由的硬过滤输入，不能只在内存里——Hub 重启后
  设备还没连上时要能回答「有没有这台设备」，见 `scheduling.md` §2.1。
- SQLite：建库必须 `journal_mode=WAL` + `busy_timeout`。

## 4. connector：有类型、可插件化、绑定会话、**面向用户**

> **connector 是给 Web 页面用的，不是给 agent 用的。**
> 本地 agent 完全依赖设备上安装的 agent 自带能力（MCP / 内建工具 / 它自己的权限系统），
> 我们**不向 agent 暴露 connector**。

- 第一版内置：**terminal**（本地 PTY）、**files**（工作区根内读写）、**git**（状态/diff/提交）、**exec**（一次性命令，无 PTY；供页面与 Hub 侧系统调用）。
- 后续插件：SSH / SFTP / MySQL / PostgreSQL / Redis / SQLite / S3。
- 额外插件：**网络抓包**（pcap）、**端口转发**。
- 插件是独立、有类型的能力，各自独立演进；**不引入多态 Resource 抽象**。

### 4.1 工作区（workspace）是一等概念

- 工作区 = 设备上的一个 `(node_id, path)`，**可被多个会话引用**（同一条路径可被两个会话同时打开，各自持有自己的通道）。
  **落库形态（M1，v2 迁移）**：`workspace` 表是**主实体**（`UNIQUE (node_id, path)`），
  `session.workspace_id` 是指向它的外键——同一个 `(node_id, path)` 永远只有一行，
  多个会话引用的是同一行而不是各建一份互相看不见的副本。
  理由：**M2 的 manifest 差分按 `(node_id, path)` 定位**，工作区必须独立于会话存在
  （`workspace-and-sync.md` §3.1）；且「用户在会话里选路径」这条交互说的是**会话引用一个
  已经在那儿的工作区**，不是会话生出一个工作区。
  `ON DELETE RESTRICT` 是刻意的：删工作区前必须显式检查有没有会话指着它，
  「关掉一个面板」不该悄悄连带删掉别的会话正在用的工作区。
  反过来，**删会话不删工作区**——工作区独立于会话存在。
  **「归属于某个 agent 会话」是 M3 的措辞**（那时才接 ACP），M1 的会话容器不是 agent 会话。
- terminal / files / git 三个面板都绑在会话的工作区上：**进入会话才建立通道，退出即释放**。
- 不做设备级的长驻多资源管理台（不是「一个设备下面挂 N 个资源 tab」那种形态）。
  这不是交互偏好，是结构要求：**工作区不是路由**（`PRODUCT.md` §6 术语表、`design-system.md` §7.0）。
- 由此，「写操作限制在工作区根内」对**页面用户**是天然约束，不是额外加的沙箱补丁。

### 4.2 agent 的文件访问与我们的约束无关

agent 在节点上用自己的工具读写文件，**不经过我们的 files connector**，
因此页面侧的「工作区根内」限制**约束不到 agent**。对 agent 的约束只有两层：

1. **它自己的权限机制**（我们通过 M2 的托管覆盖层统一下发配置，见 `workspace-and-sync.md` §4）
2. **它自己装在什么目录**——我们控制工作区目录与设备上的执行权限，不做额外沙箱

> **不要**因为「connector 限制在工作区内」就认为 agent 也被限制了。这是两套独立的路径。

### 4.2 生命周期

```text
会话创建 → 选定 (node_id, path) 建工作区 → 建立 terminal/files/git 通道
        → agent 与用户在会话内交互
会话结束 → 关闭通道、释放工作区句柄 → 轨迹与改动文件清单留存
```

断线重连时按 `(node_id, path)` 重新建立通道，工作区不随连接丢失而失效（见 `task-continuity.md`）。

### 4.3 工作区怎么被喂进去

**不同步文件夹，同步内容。** Hub 是唯一真相源，一个内容寻址仓库（CAS）供三样东西：
项目文件（按 `(node_id, path)` 物化）、技能/配置（按 `(kind, version)` 铺成托管覆盖层）、
产物（按 `artifact_id` 归档）。

git 项目走主路径：Hub 只发 commit 引用，节点自己去 origin 拉，**大文件不进 WebSocket**。
非 git 项目走 manifest 差分 + 内容哈希跳过。详见 `workspace-and-sync.md`。

## 5. 会话与轨迹

- 会话层用 **ACP**（Agent Client Protocol），不自建。恢复走 ACP `session/load`。
- 轨迹用 **dsh 式 append-only 事件流**：User / Assistant / Tool / 子agent / 压缩 逐条事件，
  Web 端 Trajectory 视图可回放；会话结束列出本次改动文件 + diff。

## 6. 子 agent 与任务分发

- 子 agent 体系借鉴 **oh-my-opencode**：可配置、开箱即用的专家 agent（主 agent / 规划 / 审查 / 检索…）。
- 主 agent 把单个完整任务分发给子 agent，在**设备上**执行。
- 调度按节点能力/性能自动路由：读心跳的 `resources` 上报，
  **硬过滤（能力）→ 有序打分（负载/亲和）**，可由用户钉死。详见 `scheduling.md`。

## 7. 推理

主线是 **AI Gateway**：把各平台免费/付费的模型 API 收敛成统一端点（密钥托管、
按 model/价格/延迟/可用性路由、故障转移、额度统计），上游先用现成 API。
本地模型推理**后置**。需求见 `ai-gateway.md`；`inference.md` 是本地推理的可行性存档。
Gateway 复用约束 9 的调度范式（能力硬过滤 + 有序打分），且模型路由与任务路由是两套独立决策。

## 8. 存储

SQLite（WAL + busy_timeout）存 device / 会话元数据 / 任务状态 / 轨迹事件引用。
轨迹事件与工作区产物的存储位置与保留策略见 `session-and-trajectory.md`。
内容寻址仓库（blob / snapshot / artifact / managed_file）见 `workspace-and-sync.md`。