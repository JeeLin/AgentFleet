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
- 异构设备张量并行推理作为第一版卖点
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
| 设备接入 | `afcli enroll <hub-url>` 一条命令，节点生成密钥对、主动建立 WebSocket 外联；无需 Hub 入站端口 |
| 节点身份与状态 | 注册时 Hub 签发 `node_id`；心跳即能力上报（os / arch / hostname / 版本 / 支持的 connector）；在线/离线状态 |
| AI agent 管理与对话 | 接入各类 agent 软件（不同厂商/协议），在 Web 一个界面里统一配置、启动、对话。**agent 运行时装在节点上**，接入方式与安装来源见 `docs/PRODUCT.md` §5 |
| Hub 一键安装 agent | 在 Hub 选设备 + 选 agent 模板 → 节点本地安装 → 自动套用托管配置 → 心跳回报就绪。**必须由用户在页面上手动选择后才执行，不自动安装**；agent 版本升级策略未决。节点进程 afcli 自身的升级是另一条通路：心跳报版本 → Hub 比对 → 自动静默升级（无需用户确认），两者触发方式对照见 `architecture/workspace-and-sync.md` §4.5 |
| 对话与动作流 | 会话有 Trajectory 视图，逐条展示 User/Assistant/Tool/子agent/压缩记录；**随时可看**本次改动文件与 diff |
| 文件浏览 | 会话工作区内浏览/读写节点本地目录，写操作限制在工作区根内（**仅约束页面用户**；agent 用自己装的能力，不走这条路径） |
| Git 操作界面 | 会话工作区的 git 状态 / diff / 提交视图 |
| Bash / PTY | 会话工作区内的本地 shell；不装 openssh-server，Hub 不拨入 |
| 工作区供料 | Hub 把项目文件喂进设备：git 项目只传 commit 引用（节点自己去 origin 拉），非 git 项目按 manifest 差分；不传依赖目录（见 `architecture/workspace-and-sync.md`） |
| 统一配置 agent | 在 Hub 统一管理各设备的 agent 配置（专家 agent 定义、技能）；托管文件在节点上只读，本地改动标记为「漂移」，可 push / pull |
| 产物归档 | 任务产出的制品归档到 Hub，设备关机后仍可下载 |
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
- **分布式推理**：把接入设备作为协同的大模型运行节点（见 §5 设计问题二 与 `docs/architecture/inference.md`）

### 3.2.1 原「推后」项，现纳入第一版 / Now in the first release

- **Hub 统一配置 agent**：在 Hub 统一管理所有设备上的 agent 配置（专家 agent 定义、技能、
  `opencode.json`）。实现为**托管覆盖层**：Hub 记录托管清单，文件在节点上只读，
  心跳顺带检测漂移并给出 push / pull。**节点密钥（`identity.json`）永不上传。**
  任务的跨设备分发（新建任务 → 路由到某台 node）是 Hub 层的事（M5），与 agent 运行时
  内部的父子 agent 无关。见 `docs/architecture/workspace-and-sync.md`。

### 3.3 用户可见流程（第一版）/ Key flows (first release)

1. **接入一台设备**：在设备上跑 `afcli enroll <hub-url>` → Hub 显示新节点（真实 os/arch/hostname）→ 在线。
2. **在某个设备上开 agent 对话**：选节点 → 选工作区路径（项目文件由 Hub 喂进去）→ 选/配 agent →
   会话建立，terminal / files 面板挂到这个工作区上（git 面板在 M4 加入）→
   在面板里对话 → Trajectory 逐条可查。
3. **看它改了什么**：会话进行中随时可看 → 改动文件列表 → 点开看 diff（基线在会话开始时建立，持续 diff）。
4. **操作文件 / git / bash**：在**该会话内**用对应面板，直接作用于该会话的工作区。
5. **统一配置 agent**：在 Hub 改一份 agent 配置（专家 agent 定义 / 技能）→ 下发到所有节点 → 节点上只读生效，
   被本地改过的文件标记为「漂移」，可 push 或 pull。
6. **取回产物**：任务结束 → 产物归档到 Hub → 设备关机后仍可下载。
7. **设备掉线**：WS 断开 → 节点自动指数退避重连（1s/2s/4s…上限 30s），UI 显示「重连中」与已重试时长；
   连续失败超过约 3 分钟转为「离线」；离线期间工作区与 agent 会话在设备上仍存活，
   面板转为只读并显示最后已知状态，禁止发起新操作；用户可手动点「立即重连」。
8. **在新设备上装 agent**：Hub 选设备 + 选 agent 模板 → 下发安装指令 → 节点本地安装 →
   自动套用托管配置 → 心跳回报就绪 → 即可开会话。

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
- **工作区/产物定位用内容寻址或稳定引用**：跨节点续跑时，需要的文件/产物一律经
  **Hub 的内容寻址仓库**按内容哈希取得，不依赖「上一台机器还在」，**不做设备间 P2P 直传**。
- **ACP 的 `session/load` 作为对话恢复机制**：agent 会话恢复走 ACP，而非自建。
- **轨迹 append-only**：所有动作写入 dsh 式事件流，既是 UI 回放源，也是断点续跑的审计源。

**对外口径**：第一版只做到「会话/节点级可恢复」，完整的跨节点任务连贯性在任务系统里程碑做。

### 5.2 二、把接入设备作为分布式大模型节点 / Distributed inference

> 多个设备协同运行一个大模型（模型切分 / 分布式推理 / 异构组网），可行性、约束与路径？

**调研结论（完整评估见 `docs/architecture/inference.md`）：**

- **异构消费级设备做真实张量并行不可行**：张量并行每层多次跨设备归约，受互联带宽/延迟限制；
  llama.cpp 的 RPC 后端只做「逐算子转发 + 层切分（流水线并行）」，没有集合通信原语，
  跨机 TP 不可用；MoE 走 expert 分片（零跨节点流量）才是异构友好路径。
- **必须基于现成开源，不自研内核**（你也明确无能力审核推理内核代码）：
  - 最稳健：每台设备跑自己的全量副本 + 一个 OpenAI 兼容网关按模型/状态路由
  - 进阶：llama.cpp RPC 层切分（2-3 台强设备跑装不下的模型）
  - MoE 异构最优：mesh-llm 的 expert 分片（零跨节点流量）
  - 实验：draft 分节点投机解码
- **约束前置决策**：llama.cpp RPC 无鉴权且需入站端口，与「Hub 无入站访问」冲突——
  采用 mesh-llm 的 QUIC/Iroh 出站隧道或 rileynz 的出站 TLS 隧道模式解决。
- **对外表述**：「一个由你 N 台设备支撑的 OpenAI 兼容端点」，不是「N 台设备合成一台大模型机」。
  正确预期是「能跑更大的模型」，不是「跑得更快」。

**结论：暂不确定，记为 TODO。**

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
| workspace | 工作区：设备上的一个 `(node_id, path)`，归属于某个 agent 会话，终端 / 文件 / git 面板绑在其上。Workspace: a `(node_id, path)` on a device, owned by one agent session; terminal / files / git panels bind to it |
| artifact | 产物：任务产出的最终制品，按 id 归档到 Hub，设备关机后仍可取回。Artifact: a task's final output, archived on the Hub by id and retrievable after the device powers off |
| channel_id | 通道号：一条设备 WebSocket 上的子通道，通道键为 `(node_id, channel_id)`，每通道独立流控。Channel id: a sub-channel on one node WebSocket, keyed `(node_id, channel_id)`, with per-channel flow control |
| baseline | 基线：会话开始时记录的工作区初始状态，用于随时计算改动文件与 diff。Baseline: the workspace state captured at session start, used to compute changed files and diffs on demand |
| CAS | 内容寻址仓库：Hub 侧以内容哈希存储 blob 的仓库，统一供料项目文件、配置与产物。CAS: the Hub's content-addressed blob store, feeding project files, config and artifacts alike |
| supervisor | 守护进程：持有设备身份与 WebSocket 的稳定进程，负责拉起与监控 worker。Supervisor: the stable process holding device identity and the WebSocket, responsible for starting and supervising workers |
| worker | 工作进程：实际执行 connector 与 agent 的可替换进程，升级时更换，身份不变。Worker: the replaceable process that actually runs connectors and agents; swapped on upgrade, identity unchanged |
| worktree | Git 工作树：同一仓库的多个独立工作目录，用于隔离并行任务，共享 object store。Worktree: independent working directories of one repository, isolating parallel tasks while sharing the object store |
| Workbench | 会话工作台：Web 端承载会话的界面外壳，终端 / 文件 / git / 轨迹都渲染在其面板里。Workbench: the web UI shell that hosts a session; terminal / files / git / trajectory all render as its panels |

---

## 7. 设计审查配置 / Review config

- 人工复核：可选（自动设计审查通过后，有争议时才进入人工复核）
