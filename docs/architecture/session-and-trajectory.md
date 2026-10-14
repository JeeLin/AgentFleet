# 会话与轨迹 / Session & Trajectory

> 会话层用 ACP，轨迹用 dsh 式 append-only 事件流。Web 端逐条可查、改动可见。

---

## 1. 职责分层

| 层 | 职责 | 是否自建 |
|----|------|---------|
| **会话协议** | agent 客户端协议、请求/响应 | 用 **ACP**，不自建 |
| **轨迹记录** | 把会话动作变成可回放事件流 | 自建（dsh 式 append-only） |
| **轨迹呈现** | Trajectory 视图、改动文件/diff | 自建 Web 组件 |

---

## 2. 会话恢复走 ACP

- Hub **不**自建 agent 会话协议；接入 **ACP**（Agent Client Protocol）。
- Hub 只存 `acp_session_id` + 归属 `node_id`；**恢复走 ACP 的 `session/load`**，不重建上下文。
- 会话在 Workbench 里作为一个面板类型渲染，不建独立页面。

---

## 3. 轨迹（Trajectory）：append-only 事件流

### 3.1 模型

轨迹是**只追加（append-only）的事件流**，每个事件是一条 typed record。
借鉴 dsh：把「模型可见的数据」当作持久 session 数据，session 是一条 append-only 的 typed event 序列。

记录分组（对应 dsh 的 User / Assistant / Tool / Subtool / Compaction）：

| 组 | 事件类型 | 关键字段 |
|----|---------|---------|
| User | user message | 文本、附件、图片摘要 |
| Assistant | assistant turn | 文本、token 用量、耗时 |
| Tool | tool call / result | 工具名、输入、输出、耗时、状态 |
| 子 agent | subtool | 子 agent 名、归属、递归嵌套 |
| 压缩 | compaction | 压缩前后 token 计数 |

### 3.2 「可回放」的硬要求

事件必须自带回放所需字段（这是审查维度第 7 条）：
**类型 / 输入 / 输出 / 耗时 / 归属节点**。缺任一项则轨迹只能看不能回放/审计。

### 3.3 Trajectory 视图（Web）

借鉴 dsh 的 `ui-trajectory`：turn-aware ledger + interactive timing overview。

- 逐条展示 User / Assistant / Tool / 子 agent / 压缩记录，标记 turn 与 step 边界。
- 记录检查器（record inspector）：点开一条看 token 用量、耗时、输入、输出、时序、图片与附件摘要。
- 读取一律按前缀 / 类型聚合，不因分片差异漏读。

---

## 4. 改动文件与 diff（借鉴 dsh 的改动视图）

**需求**：一次对话结束后，Web 直接列出「本次改动的所有文件」，点开即可查看修改内容。

实现要点：
- 会话开始时记录工作区基线（参考 opencode 的 snapshot 机制，用内部 git 仓库跟踪变更）。
- 会话进行中/结束时，对基线做 diff，得到**改动文件列表**（新增/修改/删除 + 行数变化）。
- 每个文件点开看**逐行 diff**（added / removed / context）。
- 大改动与大文件截断保护；二进制文件只标「已变更」不给 diff。

---

## 5. 子 agent 与嵌套会话

- 子 agent 借鉴 oh-my-opencode / OpenCode：primary agent + subagent；subagent 由 primary
  通过 task 调用或用户 `@` 提及；支持嵌套深度控制。
- 嵌套子 agent 的事件在轨迹里以「子 agent」分组呈现，可下钻到子 agent 自己的会话。

---

## 6. 存储与保留

- Hub 存 device / 会话元数据 / 任务状态 / **轨迹事件**（事件本身，不是引用）。
- 轨迹事件的表结构见 §7；工作区产物与 blob 的存储位置与保留策略见
  `workspace-and-sync.md`。

---

## 7. 轨迹事件表（初版）

§3.2 的五个硬要求（类型 / 输入 / 输出 / 耗时 / 归属节点）必须落成列，否则无法审查：

```sql
trajectory_event(
  id              INTEGER PK,   -- 单调递增，回放顺序 = 物理顺序
  session_id      TEXT,
  node_id         TEXT,          -- 归属节点（硬要求：回放要能定位到哪台设备）
  seq             INTEGER,       -- 会话内序号
  kind            TEXT,          -- user / assistant / tool / subagent / compaction
  payload         TEXT,          -- JSON：输入 + 输出（含附件/图片摘要）
  duration_ms     INTEGER,       -- 耗时（硬要求）
  tokens_in, tokens_out INTEGER, -- Assistant / 压缩记录用
  parent_event_id INTEGER,       -- 子 agent 事件挂到父事件下（嵌套）
  created_at      TEXT
)
```

- SQLite：建库必须 `journal_mode=WAL` + `busy_timeout`。
- **只追加**：不 UPDATE、不 DELETE；修正以新事件表达。
- 保留策略与产物同档（临时 / 归档 / 永久），见 `workspace-and-sync.md`。