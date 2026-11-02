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

> **「会话是页面，不是面板」——本节那句话的边界（M1 裁决，2026-10-17）**
>
> 本节的原始表述「会话在 Workbench 里作为一个面板类型渲染，不建独立页面」写在 **ACP 章节**里，
> 主语是 **ACP agent 会话**（M2 起；原计划在 M3）：那个会话的对话内容在 Web 端以**面板**呈现，
> 不单独占一个页面。它**不是**在说 M1 的会话容器，也不否定会话页本身。
>
> 三分法（与 `PRODUCT.md` §6 术语表、`design-system.md` §7.0 同一口径）：
>
> | 概念 | 是什么 | 有路由吗 |
> |------|-------|---------|
> | **会话页** | `/sessions/:id`，M1 的承载页：对话流 + 面板坞 + 轨迹抽屉 | **有** |
> | **工作区** | 设备上的 `(node_id, path)`，会话的一个字段 | **没有** |
> | **Workbench** | 会话页的**布局外壳**（标题栏 + tab 条 + 面板坞） | **没有** |
>
> **会话页与 Workbench 的关系**：Workbench 不是页面，是**会话页内部的那层布局**。
> 一个 tab = 一个会话页实例，`/sessions/:id` 表达的只是「激活哪一个」——
> tab 集合本身不可深链接（`design-system.md` §5.5.6）。
> 面板（terminal / files / git）挂在 Workbench 的面板坞里，生命周期随会话页。
>
> **M1 的会话容器与 ACP 会话层是两件事**：M1 交付的是「一个能建、能开面板的页面」，
> 不接 ACP；ACP 会话层与 §3 的轨迹在 **M2** 接进来（原计划排在 M3，已整体提前，
> 见 `DEVELOPMENT.md` 的里程碑重排）。因此「ACP 会话作为面板呈现」这句话在 M1
> **没有实施对象**，也不该被读成「M1 不许建 `/sessions/:id`」。

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

### 3.2.1 完整性：不承诺，但必须可见（2026-10-26）

**这一节回答的是「一个会话的消息没回传完整怎么办」。答案是三层，而本设计
只做到第二层，不做第三层。**

| 层 | 能保证什么 | 靠什么 |
|---|---|---|
| 1. 不重 | 同一事件写两遍不产生两行 | `UNIQUE(session_id, node_id, seq)`，数据库层生效，不靠应用层先查再插（那是 TOCTOU） |
| 2. 不完整**可见** | 丢了能查出来、页面能画出标记 | ① 序号断裂检测 ② 每轮一条 `turn_end` |
| 3. 不丢 | ——**不做**，见下 | —— |

**为什么必须做第 2 层**：`UNIQUE` 只保证幂等，与完整无关。缺了第 7 条与
本来只有 6 条，在库里长得一模一样（`seq` 都到 6）。没有第 2 层，页面会把一段
缺了中间的对话渲染成看起来连续的一整段——**静默的数据损坏**。

**第 2 层的两个判据**：

- **段内丢** = `seq` 断裂。`seq` 是设备侧会话内只增序号，相邻两条差 > 1 即中间丢了。
  查询时算，不占列。
- **末尾丢** = 这一轮的 `turn_end` 不在。`session/prompt` 的 response 带 `stopReason`
  （实测 `end_turn`），设备侧据此写一条 `kind = 'turn_end'` 的事件。
  **这是「一轮走完了」的唯一凭据**——chunk 流自己不会说它结束了，
  而断裂检测看不见末尾。

页面必须把两者都画出来：断口处显示「此处缺失 N 条」，未收口的末轮标「本轮未完成」。

**为什么不做第 3 层（补齐）**：

ACP **没有「重发第 N 条通知」的机制**。`session/load` 的重放实测是**重建流**——
同进程实测 live 9 条 → replay 4 条，中间态丢失，且重放事件**没有 `seq`、
没有时间戳**。所以：

- 断线窗口内产生的事件**永久丢失**，这是协议能力边界，不是实现选择。
- 想真正补回来只能在**设备侧加持久队列**（未确认不删、重连重发）。那是一个
  独立的、更大的机制（本地 WAL + 确认 + 去重），不在本里程碑范围内。
- 用重放去「补齐」会得到两套数据源：轨迹表与重放流，它们在刷新前后给出不同的
  对话，而用户无从知道哪个是真的。

**已知取舍的窗口**：一个心跳周期（15 s）+ 探测超时。设备在这段时间内断开，
期间的 `session/update` 全部丢失。页面显示最后已知状态并**标注断开区间**。

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

### 6.1 会话起点（`session.base_rev` / `base_rev_status`，v0.6.0 / schema v4）

`session` 表加两列，回答「这次会话基于哪个 rev 开始」：

| 列 | 类型 | 语义 |
|----|------|------|
| `base_rev` | `TEXT` | 会话建立那一刻工作区的 HEAD，**完整 40 位**；没有起点时 `NULL` |
| `base_rev_status` | `TEXT NOT NULL DEFAULT 'unknown'` | 起点为什么有／没有，五态之一 |

`base_rev_status` 的**五态**：

| 值 | 含义 | `base_rev` | 谁产生 |
|----|------|-----------|--------|
| `ok` | 取到了 | 40 位十六进制 | 建会话时探测成功 |
| `not_a_repo` | 工作区不是 git 仓库 | `NULL` | 建会话时探测 |
| `no_head` | 仓库还没有提交 | `NULL` | 建会话时探测 |
| `unavailable` | 问了但没拿到（设备离线 / 超时 / 通道打不开 / connector 没上报） | `NULL` | 建会话时探测 |
| `unknown` | 迁移前的老会话，从没记过 | `NULL` | **只有迁移默认值** |

三条设计约束：

1. **默认值是 `unknown` 不是 `ok`**。`ALTER TABLE ADD COLUMN` 会把默认值填给
   **存量行**；填 `ok` 会让所有老会话声称「我的起点是 NULL」，一个看起来正常、
   实际无意义的谎话。
2. **`unknown` 与 `unavailable` 绝不合并**。前者是历史事实（那时还没有这个
   功能），后者是「试过了、设备没给」。合并会把历史事实显示成一次故障。
3. **不加 `CHECK` 约束**。`ADD COLUMN` 的 `CHECK` 会让存量行拿 `DEFAULT` 去过检查，
   而这里需要的是「值非 `unknown` 时才校验 `base_rev` 非空」这种条件约束，
   表达不了。合法性由 Rust 枚举 `BaseRevStatus` 的 `FromStr` 兜住，
   解析失败报 `InvalidBaseRevStatus`（**不回落成 `Unknown`**，与 `SessionState`
   对第三态的处理同一条口径）。

起点**不是门禁**：取不到就如实记原因，建会话照常成功。理由见
`docs/PRODUCT.md` §3.1「Git 操作界面」——非 git 工作区从 v0.2.0 起一直是合法用法。

#### 建会话时怎么钉住起点

`POST /api/sessions` 的顺序是**先落行、再探测、再回填**：

1. 门禁全部通过后 `create(..., base_rev = NULL, base_rev_status = "unknown")`
   落一行（`state = 'running'`）；
2. 用这个**真实会话 id** 开一条 `git` 通道，下发 `{"op":"rev"}`，等一条
   `op == "rev"` 的帧，然后**无论成败都关掉通道**；
3. `set_base_rev(id, rev, status)` 回填。

**为什么不能「先探测再插入」**：开通道的既有入口 `open_device_channel` 第一件事
就是按 `session_id` 查会话并要求 `state == Running`——会话行不存在时它无路可走。
另一条路是给 `ChannelBinding.session_id` 填一个占位 id，但那个字段正是
`POST /api/channels/{id}/*` 归属校验的依据，占位值会让这条通道的归属变成谎话，
比多一次 `UPDATE` 坏得多。

**代价与它为什么可以接受**：两次写库之间有一个几十毫秒的窗口，窗口内另一个
标签页刷新会话列表会看到 `base_rev_status == "unknown"`（「起点未记录」），
刷新一次即正确。关键在于**它不会显示成错误的起点**——`unknown` 不是 `ok`。
因此不加锁、不加临时的「探测中」状态。

**钉起点是一次 best-effort 探测，失败不阻塞建会话**：设备没上报 `git`、通道开
不起来、等帧超时（探针自带 3 秒上限）、设备回 `not_a_repo` / `no_head`——这五类
结局都只落成对应的 `base_rev_status`，`POST /api/sessions` 照常 201。会话本身
已经建好且可用，没有理由因为「少一条元信息」让用户丢一次成功的创建。

**通道一定会被关掉**：探测走的是与 `DELETE /api/channels/{id}` 同一份内部逻辑
（解除绑定 → 发 `close_channel` 帧 → 推 `channel.closed`），收尾失败不改变探测
结果，也不改变建会话的成败。

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
- ~~保留策略与产物同档（临时 / 归档 / 永久）~~：**产物归档已撤销（2026-10-26）**，
  本条失去参照对象。轨迹事件的保留策略**待定**（当前不设自动清理）。