# 架构总览 / Architecture Overview

> 决策细节见：`inference.md`（分布式推理）、`session-and-trajectory.md`（会话与轨迹）、
> `task-continuity.md`（任务连贯性）。
> Detail lives in: inference.md, session-and-trajectory.md, task-continuity.md.

---

## 1. 进程模型

三个二进制/角色，Hub 与 node 都是单二进制：

```text
浏览器 ──HTTPS/WSS──> Hub（自托管，托管前端 + 聚合节点与 agent）
   浏览器 <─────WSS─────> node（设备 A）  ← node 主动外联
   浏览器 <─────WSS─────> node（设备 B）
   浏览器 <─────WSS─────> node（设备 C）
```

- **Hub**：注册表（持久）+ 连接注册表（内存）+ 会话/任务编排 + 推理网关（后续）。
- **node**：设备接入进程。每条 WebSocket 由 node 主动建立，承载控制 + 数据通道。

## 2. 连接与通道

- 每设备一条 WebSocket，控制 + 数据复用。
- 通道键 `(node_id, channel_id)`；每通道独立流控/信用。一个跑飞的传输不拖死该设备全部通道。
- 心跳即能力上报：os / arch / hostname / node 版本 / 支持的 connector / 资源余量（供调度）。
- 认证：设备密钥对（注册时生成，Hub 记指纹）。不用静态共享 token。

## 3. 数据模型（初版）

```text
device(
  id              TEXT PK,   -- node_id，注册时 Hub 签发
  name            TEXT,
  key_fingerprint TEXT,      -- 设备公钥指纹
  enrolled_at, last_seen_at TEXT,
  status          TEXT,      -- online / offline
  os, arch, hostname, node_version,
  capabilities    TEXT,      -- JSON：支持的 connector 及参数
  resources       TEXT       -- JSON：CPU/内存/GPU/当前负载（调度用）
)
```

- **没有** environment 表；**没有** resource 表（connector 是有类型的，不是多态实体）。
- SQLite：建库必须 `journal_mode=WAL` + `busy_timeout`。

## 4. connector：有类型、可插件化、绑定会话

- 第一版内置：**terminal**（本地 PTY）、**files**（工作区根内读写）、**git**（状态/diff/提交）、**exec**。
- 后续插件：SSH / SFTP / MySQL / PostgreSQL / Redis / SQLite / S3。
- 额外插件：**网络抓包**（pcap）、**端口转发**。
- 插件是独立、有类型的能力，各自独立演进；**不引入多态 Resource 抽象**。

### 4.1 工作区（workspace）是一等概念

- 工作区 = 设备上的一个 `(node_id, path)`，**归属于某个 agent 会话**。
- terminal / files / git 三个面板都绑在会话的工作区上：**进入会话才建立通道，退出即释放**。
- 不做设备级的长驻多资源管理台（不是「一个设备下面挂 N 个资源 tab」那种形态）。
- 由此，「写操作限制在工作区根内」是天然约束，不是额外加的沙箱补丁。

### 4.2 生命周期

```text
会话创建 → 选定 (node_id, path) 建工作区 → 建立 terminal/files/git 通道
        → agent 与用户在会话内交互
会话结束 → 关闭通道、释放工作区句柄 → 轨迹与改动文件清单留存
```

断线重连时按 `(node_id, path)` 重新建立通道，工作区不随连接丢失而失效（见 `task-continuity.md`）。

## 5. 会话与轨迹

- 会话层用 **ACP**（Agent Client Protocol），不自建。恢复走 ACP `session/load`。
- 轨迹用 **dsh 式 append-only 事件流**：User / Assistant / Tool / 子agent / 压缩 逐条事件，
  Web 端 Trajectory 视图可回放；会话结束列出本次改动文件 + diff。

## 6. 子 agent 与任务分发

- 子 agent 体系借鉴 **oh-my-opencode**：可配置、开箱即用的专家 agent（主 agent / 规划 / 审查 / 检索…）。
- 主 agent 把单个完整任务分发给子 agent，在**设备上**执行。
- 调度按节点能力/性能自动路由（读心跳的 resources 上报）。

## 7. 推理

见 `inference.md`。核心约束：Hub 无入站访问 vs RPC 需入站 → 出站隧道/QUIC 方案前置决策。

## 8. 存储

SQLite（WAL + busy_timeout）存 device / 会话元数据 / 任务状态 / 轨迹事件引用。
轨迹事件与工作区产物的存储位置与保留策略见 `session-and-trajectory.md`。