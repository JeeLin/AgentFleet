# 产品文档 / Product

> 本文档定义 AgentFleet 的产品定位、功能边界与用户可见流程。架构决策见 `docs/architecture/`。
> This document defines product positioning, feature boundaries, and user-visible flows.
> Architecture decisions live in `docs/architecture/`.

---

## 1. 产品定位 / Positioning

个人自托管的**设备 / Agent 平台**。把自己拥有的全部设备（服务器、PC、手机、树莓派）
注册为节点，在一个 Web 页面里统一管理，在节点上运行 AI agent 会话，并按设备能力把任务
分发到最合适的节点执行。

第一版只做 self-host、单用户。这是范围选择，不是能力妥协。

Self-hosted, single-user **device / agent platform**. Every device you own becomes a node
of one web console; run AI agent sessions on any node and route tasks to the best-fit node.

### 1.1 非目标（显式）/ Non-goals (explicit)

- 多用户 / RBAC / 团队 / SaaS 计费
- 通用文件同步引擎（跨设备自动同步目录）
- 带 DAG 的重型调度器、消息中间件
- 异构设备张量并行推理作为第一版卖点（本地推理后置，主线是 AI Gateway，见 §5.2 与 `docs/architecture/ai-gateway.md`）
- 多态 Resource 抽象（connector 是有类型的、独立演进的能力）

---

## 2. 用户画像与核心价值 / Users & value

- **你有一堆设备**：服务器跑重活、PC 随手用、手机在外看进度、树莓派常驻角落。
  希望不登录每个设备、不开一堆 SSH/客户端，就能在一个界面管理和让它们干活。
- **你用各种 agent 软件**：不同厂商、不同协议的 coding agent / 通用 agent，
  希望统一接入、统一对话、统一看它们的动作。
- **你不想把数据交给 SaaS**：自托管，数据在你手中。

---

## 3. 功能边界 / Feature boundaries

### 3.1 第一版（必做）/ First release (must)

> **本表只定范围。** 哪项能力由哪个里程碑承接，以 `docs/DEVELOPMENT.md` §4
> 的范围映射表为准（那张表是对齐的唯一真相源，改范围或排期都要走它）。
> This table defines scope only. Which milestone delivers which capability is
> defined by the mapping table in `docs/DEVELOPMENT.md` §4 — the single source of
> truth. Change scope or schedule through it.

| 能力 | 用户可见行为 |
|------|------------|
| 设备接入 | `afcli run <hub-url>` 一条命令，节点生成密钥对、主动建立 WebSocket 外联；**首次启动自动 enroll**，用户无需单独跑 enroll；无需 Hub 入站端口 |
| 节点身份与状态 | 注册时 Hub 签发 `node_id`；心跳即能力上报（os / arch / hostname / 版本 / 支持的 connector）；在线/离线状态 |
| AI agent 管理与对话 | 接入各类 agent 软件（不同厂商/协议），在 Web 一个界面里统一配置、启动、对话。**agent 运行时装在节点上、由用户自己安装**（Hub 不代装，2026-10-26 裁定）；Hub 负责**扫描**出节点上装了什么、并保存它们的**启动配置**，见 `docs/architecture/workspace-and-sync.md` §4.4。接入方式见 `docs/architecture/session-and-trajectory.md` |
| ~~Hub 一键安装 agent~~ **已撤销（2026-10-26）** | 原计划在 Hub 选设备 + 选 agent 模板 → 节点本地安装。**产品裁定：不代装**——让 Hub 往设备写可执行文件等于「一处配置错、全体设备执行未核对代码」，agent 由用户自己在设备上装，Hub 只做**扫描**（心跳上报这台机器上**存在**哪些 agent）与**启动配置**的读写，见上一行与 `architecture/workspace-and-sync.md` §4.4（原文保留作决策记录）。**afcli 自身的升级是另一条通路，不受此裁定影响**：页面显示各设备 afcli 版本号 → 可手动触发升级 → 另有「自动升级」开关；版本号由 Hub 下发，升级失败自动回滚并回报 |
| 对话与动作流 | 会话有 Trajectory 视图，逐条展示 User/Assistant/Tool/子agent/压缩记录；**随时可看**本次改动文件与 diff |
| 文件浏览 | 会话工作区内浏览/读写节点本地目录，写操作限制在工作区根内（**仅约束页面用户**；agent 用自己装的能力，不走这条路径） |
| Git 操作界面 | 会话工作区的 git 状态 / diff / 提交视图 |
| Bash / PTY | 会话工作区内的本地 shell；不装 openssh-server，Hub 不拨入 |
| ~~工作区供料~~ **已撤销（2026-10-26）** | 原计划 Hub 把项目文件喂进设备。**产品裁定只做每台设备内自己的对话，不做任务分发**，因此没有项目文件传输需求。规格留 `architecture/workspace-and-sync.md` 作决策记录 |
| 统一配置 agent | 在 Hub 统一管理各设备的 agent 配置（专家 agent 定义、技能）；托管文件在节点上只读，本地改动标记为「漂移」，可 push / pull |
| ~~产物归档~~ **已撤销（2026-10-26）** | 同属「东西从设备回 Hub」的传输面，随任务分发一起撤销 |
| 模型配置一处切换 | 在 Hub 一处配置模型与 API key，各节点上的 agent 复用同一套配置（一处改、处处生效，对标 ccswitch 的交互） |

> **终端 / 文件 / git 是会话内的面板**，绑定该会话的工作区（设备上的一个 `(node_id, path)`），
> 进入会话才建立、退出即释放。**不做设备级的多资源管理台。**
> Terminal / files / git are **panels inside a session**, bound to that session's workspace;
> they attach on entry and release on exit. No device-level multi-resource console.

### 3.2 后续（非第一版）/ Later (not first release)

> 「第一版」指**首个可用版本（里程碑 M0–M4）**。M5 起为第二阶段。
> "First release" = the first usable version (milestones M0–M4). M5 onward is the next stage.

- **任务系统**：完整任务的定义、状态、进度与续跑（见 §5 设计问题一）
- **自动化**：定时 / 事件触发的任务编排
- **connector 插件化**：把 SSH / SFTP / MySQL / PostgreSQL / Redis / SQLite / S3 等协议做成插件按需加载；额外提供**网络抓包插件**与**端口转发插件**
- **社交软件接入**：各 IM 作为 agent 的输入输出通道
- **原生客户端**：客户端本身也是一个操作本机/服务器的 agent，可包裹开源软件实现
- **AI Gateway**：把各平台（免费 + 付费）的模型 API 收敛成统一网关（见 §5.2 与 `docs/architecture/ai-gateway.md`）；本地模型推理**后置**，触发条件见该文 §4 第 6 条

### 3.2.1 原「推后」项，现纳入第一版 / Now in the first release

- **Hub 统一配置 agent**：在 Hub 统一管理所有设备上的 agent 配置（专家 agent 定义、技能、
  `opencode.json`）。实现为**托管覆盖层**：Hub 记录托管清单，文件在节点上只读，
  心跳顺带检测漂移并给出 push / pull。**节点密钥（`identity.json`）永不上传。**
  任务的跨设备分发（新建任务 → 路由到某台 node）是 Hub 层的事（M5），与 agent 运行时
  内部的父子 agent 无关。见 `docs/architecture/workspace-and-sync.md`。

### 3.3 用户可见流程（第一版）/ Key flows (first release)

1. **接入一台设备**：在设备上跑 `afcli run <hub-url>` → 首次自动 enroll、生成密钥对并外联 → Hub 显示新节点（真实 os/arch/hostname）→ 在线。之后排障用 `afcli log` 看最近日志。
2. **在某个设备上开 agent 对话**：选节点 → 选工作区路径（设备上已有的目录）→ 选该设备上已装且可用的 agent →
   会话建立，terminal / files 面板挂到这个工作区上（git 面板在 M3 加入）→
   在面板里对话 → Trajectory 逐条可查。
3. **看它改了什么**：会话进行中，ACP 的 `tool_call` 事件逐条展示工具名、状态与输出摘要（改动内容从这里看）。专门的 diff 视图留到 M3 的 git 面板。
4. **操作文件 / git / bash**：在**该会话内**用对应面板，直接作用于该会话的工作区。
5. ~~**统一配置 agent**~~：**暂缓**（规格在 `architecture/workspace-and-sync.md` §4.1–4.3，实现未排期）。
6. ~~**取回产物**~~：**已撤销（2026-10-26）**，随产物归档一起撤销。
7. **设备掉线**：WS 断开 → 节点自动指数退避重连（1s/2s/4s…上限 30s），UI 显示「重连中」与已重试时长；
   连续心跳丢失 > 45s（= 3 个心跳周期）转为「离线」；离线期间工作区与 agent 会话在设备上仍存活，
   面板转为只读并显示最后已知状态，禁止发起新操作；用户可手动点「立即重连」。
8. ~~**在新设备上装 agent**~~：**已撤销（2026-10-26）**。用户自己在设备上装 agent，Hub 不代装。
   现在这条走「**设备启动即上报它上面存在哪些 agent** → 页面显示 → 用户在该设备上开会话」，
   配置由用户在页面上保存到 Hub、设备读取。

---

## 4. 设计对标 / Benchmark

| 模块 | 对标 |
|------|------|
| 终端 / 文件 / git | **不对标**——会话内面板，易用即可 |
| 对话与动作流 | **dsh**：Trajectory（逐条事件）+ 会话改动文件/diff 视图 |
| 专家 agent 模板 | **oh-my-opencode**：可配置、开箱即用的专家 agent 定义库（Hub 预置、节点只读生效）；agent 运行时内部的父子关系不由我们编排 |
| 管理页（设备/agent/设置） | 自有设计系统 |

**基调**：深色优先、等宽字体、高信息密度、键盘优先、一致交互范式贯穿全站。易用优先。

---

## 5. 两个关键设计问题（结论）/ Two key design questions

### 5.1 一、任务的连贯性 / Task continuity

> 同一个任务在多设备、多节点之间流转时，状态、上下文、执行进度如何保持一致、不断裂、可续跑？

**结论（第一版不实现任务系统，但先钉死模型，避免返工）：**

- **状态与上下文外置到 Hub（single source of truth）**：任务的 transcript、工具调用轨迹、
  工作区快照引用都存 Hub，节点只持有「当前执行所需的上下文切片」。节点掉线，任务状态不丢。
- **断点可恢复**：任务 = 持久化的状态机 + 事件日志；节点执行带「检查点（checkpoint）」，
  重连后从最后一个成功检查点续跑，不重做。
- ~~**工作区/产物定位用内容寻址或稳定引用**~~：跨节点续跑属任务系统里程碑，**暂缓**。
  仍然有效的是：**不做设备间 P2P 直传**；**内容寻址存储（CAS）一处不剩**——agent 安装已于
  2026-10-26 撤销，`blob` 表随之从 schema 去掉，没有别的用途。
- **ACP 的 `session/load` 作为对话恢复机制**：agent 会话恢复走 ACP，而非自建。
- **轨迹 append-only**：所有动作写入 dsh 式事件流，既是 UI 回放源，也是断点续跑的审计源。

**对外口径**：第一版只做到「会话/节点级可恢复」，完整的跨节点任务连贯性在任务系统里程碑做。

### 5.2 二、推理：AI Gateway 优先，本地推理后置 / AI gateway first, local inference deferred

> 多个设备协同运行一个大模型（模型切分 / 分布式推理 / 异构组网），可行性、约束与路径？

**调研结论（完整评估见 `docs/architecture/inference.md`）：**

- **异构消费级设备做真实张量并行不可行**：张量并行每层多次跨设备归约，受互联带宽/延迟限制；
  llama.cpp 的 RPC 后端只做「逐算子转发 + 层切分（流水线并行）」，没有集合通信原语，
  跨机 TP 不可用；MoE 走 expert 分片（零跨节点流量）才是异构友好路径。
- **原「阶段 1–4」路径与 `AGENTS.md` 约束 7 冲突**：管 `--rpc` 列表、算 `--tensor-split`、
  强制全网同 build、MoE expert 分片属于「自研分布式推理编排」，正是约束 7 禁止的。
- **必须基于现成开源，不自研内核**（你也明确无能力审核推理内核代码）。

**结论（2026-10-15 定案）：先做 AI Gateway，本地推理后置。**

- **主线改为 AI Gateway**：把各平台（免费 + 付费）的模型 API 收敛成一个统一网关——
  统一端点 + 密钥托管 + 按 model/价格/延迟/可用性路由 + 故障转移 + 额度统计。
  上游先用现成 API；`Provider` trait 为将来的本地推理留口。
- **本地推理不实现**，但保留为「触发条件满足时再做」的待办（见 `ai-gateway.md` §4 第 6 条）。
- **理由**：Gateway 不需要 GPU、不需要等设备上线，部署当天可用；而分布式推理需要多台强设备
  在线且同 build。且 Gateway 路线下 Hub 只做「选上游并转发」，不再触碰约束 7 的红线。

**需求详情与待决问题见 `docs/architecture/ai-gateway.md`（需求权威来源）。**
`inference.md` 降级为本地推理的可行性调研存档，非主线方向。

上面是**调研结论**，不是已定决策：已评估的几条可行路径都不直接匹配本项目的
「自托管 + 个人设备」场景（详见 `docs/architecture/inference.md`）。本期不做决定，
需要时重新评估再定。

> The analysis above is research output, not a decision. None of the evaluated paths maps
> directly onto this project's self-hosted personal-device scenario. **Undecided — tracked as
> TODO**; no call is made in this release, revisit when it actually matters.

---

## 6. 术语 / Glossary

| 词 | 含义 |
|----|------|
| device / node | 一台注册的设备及其上运行的节点进程 |
| agent | **仅指 AI agent**；绝不指设备进程 |
| Hub | 自托管的服务端，用户自己跑 |
| connector | 节点上提供的一类**有类型**的能力（terminal / files / git / exec / ssh / db / …），可插件化 |
| task | 一个完整任务；sub-task 是任务下可独立执行/续跑的单元，在某台 node 上由 agent 会话执行 |
| trajectory | dsh 式的会话动作事件流，Web 逐条回放 |
| 会话 / 会话页 | 会话容器：一个页面（`/sessions/:id`），持有对话流与面板坞。**M1 的会话不含 ACP；M2 起会话可选挂一个 agent**，拥有 `acp_session_id`，对话走 ACP。挂 agent 时 `session.agent_id` 非 NULL，且建会话当时该 agent 在目标节点上必须**可用**（否则 409 `agent_unavailable`）；不挂时仍为 NULL，M1 的行为不变。Session page: the container page `/sessions/:id`; M1 sessions carried no ACP, M2 sessions may bind an agent and carry an `acp_session_id` |
| workspace | 工作区：设备上的一个 `(node_id, path)`，**会话的一个字段**，不是路由、不是页面；可被多个会话引用（同一条路径可被两个会话同时打开，各自持有自己的通道）。Workspace: a `(node_id, path)` on a device, carried as a field on a session — not a route and not a page |
| ~~artifact~~ | **已撤销（2026-10-26）**：原指任务产出的最终制品归档到 Hub。随产物归档一起撤销 |
| channel_id | 通道号：一条设备 WebSocket 上的子通道，通道键为 `(node_id, channel_id)`，每通道独立流控。Channel id: a sub-channel on one node WebSocket, keyed `(node_id, channel_id)`, with per-channel flow control |
| ~~baseline~~ | **暂缓**：原指会话开始时记录的工作区初始状态，用于算改动文件与 diff。专门的 diff 视图留到 M3 git 面板 |
| CAS | 内容寻址存储：Hub 侧以内容哈希存 blob。**现已完全撤销**（2026-10-26）：agent 安装取消后 `blob` 表也从 schema 去掉，项目文件 / 配置 / 产物早就不再供料。CAS: content-addressed blob storage — fully withdrawn; with agent install cancelled the `blob` table was removed from the schema |
| supervisor | 守护进程的父进程：不碰业务与 WebSocket，只负责拉起 / 监控 / 替换 / 回滚 worker——**执行回滚的代码不参与被回滚**。Supervisor: the parent process that touches neither business logic nor the WebSocket, only starting, monitoring, swapping and rolling back the worker — the code performing a rollback is never itself replaced |
| worker | 工作进程：持有设备身份与 WebSocket，实际执行 connector 与 agent；升级时更换，`node_id` 不变。Worker: the process holding device identity and the WebSocket that actually runs connectors and agents; swapped on upgrade, `node_id` unchanged |
| worktree | Git 工作树：同一仓库的多个独立工作目录，用于隔离并行任务，共享 object store。Worktree: independent working directories of one repository, isolating parallel tasks while sharing the object store |
| Workbench | 会话工作台：**会话页的布局外壳**（标题栏 + 会话 tab 条 + 面板坞 + 轨迹抽屉），不是一个有地址的容器——**它自己没有路由**。Workbench: the layout shell of a session page (title bar + session tab strip + panel dock + trajectory drawer); it has no route of its own |

> **三分法，别混**（本术语表与 `design-system.md` §7.0 的唯一口径）：
> **会话页** = `/sessions/:id`，有路由；**工作区** = `(node_id, path)` 字段，**没有路由**；
> **Workbench** = 会话页的布局外壳，**没有路由**。
> 「工作区不是路由」是硬约束：工作区一旦有地址，它就成了可枚举、可跳转、可长期持有的句柄，
> 于是「设备级多资源长驻管理台」在结构上成为可能——而那正是 §3.1 排除的形态。
> 同理「会话页 ≠ 工作区」：会话页是承载页，工作区只是它身上的一个字段。

---

## 7. 设计审查配置 / Review config

- 人工复核：可选（自动设计审查通过后，有争议时才进入人工复核）
