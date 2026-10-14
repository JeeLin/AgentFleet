# AgentFleet

个人自托管的设备 / Agent 平台 / Personal Self-Hosted Device & Agent Platform

把自己拥有的每一台设备——服务器、电脑、手机、树莓派——注册为节点；
在一个 Web 页面里统一管理，在节点上打开终端与文件、运行 AI agent 会话，
并按设备能力把任务分发到最合适的节点执行。

Turn every device you own—servers, PCs, phones, Raspberry Pis—into a node of one
self-hosted fleet: manage them all in one web console, open terminals and files on any
node, run AI agent sessions, and route tasks to the node best suited to run them.

> **单用户 · 自托管 · 数据自主掌控 · 零入站端口 · 深色优先**
> **Single-user · Self-hosted · Data sovereignty · Zero inbound ports · Dark-mode first**

---

## 名字含义 / Name Meaning

- **AgentFleet** = 一支由**你拥有的全部设备**组成的 **agent 舰队**。
  A fleet of agents, formed by every device you own.
- **Hub** 是你自己跑的服务端（托管前端 + 聚合节点与 agent）；
  **node** 是跑在目标设备上的接入进程。两者都是单二进制。
  Hub is the server you host yourself; node is the enrollment process on each device.

---

## 核心承诺 / Core Promises

| 承诺 / Promise | 传统方式 / Traditional Way | AgentFleet |
|----------------|---------------------------|------------|
| 所有设备一个平台管 / One console for every device | 逐个 SSH / 多个客户端 / One SSH or client per device | **一个工作空间**管全部节点 / One workspace for all nodes |
| 公网 + 内网一个服务 / One service, public + LAN | VPN / 端口转发 / 跳板机 / VPN / port-forward / jump host | 节点反向隧道，**零端口暴露** / Node reverse tunnel, **zero port exposure** |
| 数据自主掌控 / Data sovereignty | 在线 SaaS，数据在第三方 / SaaS, data with third party | 完全**自托管** / Fully **self-hosted** |
| agent 散落在各处 / Agents scattered | 逐个开终端 / 多客户端 / One terminal per agent | 统一接入，**一个界面里管理与对话** / One place to manage & chat |
| 任务按能力分发 / Tasks routed by capability | 手动搬代码、同步环境 / Hand-carry code & env | 按**设备能力**分发到合适节点 / Routed to the best-fit node |

---

## 第一版做什么 / First Release Scope

| 能力 / Capability | 说明 / Description |
|------------------|-------------------|
| 设备接入 / Device enrollment | 一条 CLI 命令注册，单条 WebSocket 主动外联；心跳即能力上报（os / arch / hostname / 版本 / 支持的 connector） |
| 节点身份 / Node identity | 注册时由 Hub 签发 `node_id`；设备密钥对认证，不用静态共享 token |
| AI agent 管理与对话 / Agent management & chat | 接入各类 agent 软件（不同厂商、不同协议），在 Web 一个界面里统一配置、启动、对话。**agent 运行时装在节点上**，接入方式与安装来源见 `docs/PRODUCT.md` §5 |
| 对话与动作流 / Transcript & action stream | 任务与子任务的对话记录和动作在 Web 里直接查看（dsh 式 Trajectory），逐条可查，并列出本次改动的**文件**与 diff |
| 文件浏览 / File browser | 会话工作区内浏览 / 读写节点本地目录，写操作限制在工作区根内 |
| Git 操作界面 / Git UI | 会话工作区的 git 状态 / diff / 提交视图 |
| Bash / PTY | 会话工作区内的本地 shell，不需要装 `openssh-server`，也不需要 Hub 拨入 |
| 工作区供料 / Workspace feeding | Hub 用内容寻址仓库把项目文件喂进设备：git 项目只传 commit 引用（节点自己去 origin 拉），非 git 项目按 manifest 差分；不传依赖目录 |
| 统一配置 agent / Unified agent config | 在 Hub 统一管理各设备的子 agent 定义与技能；托管文件在节点上只读，本地改动标记为「漂移」，可 push / pull。**密钥永不上传** |
| 产物归档 / Artifact archive | 任务产出的制品归档到 Hub，设备关机后仍可下载 |

> **不同步文件夹，同步内容。** Hub 是唯一真相源；不做设备间 P2P 直传，不用文件夹同步工具当主通道。
> We sync **content, not folders**. Hub is the single source of truth; no device-to-device P2P transfer.

> **终端 / 文件 / git 是会话内的面板**，绑定该会话的工作区（设备上的一个 `(node_id, path)`），
> 进入会话才建立、退出即释放。**不做设备级的多资源管理台。**
> Terminal / files / git are **panels inside a session**, bound to that session's workspace;
> they attach on entry and release on exit. No device-level multi-resource console.

> 首个演示是 **「我在树莓派上开一个 agent 会话，会话里的 shell 和文件浏览就能用，且零入站端口」**——
> 这是产品本身；agent 面板是一项功能。
> The first demo is "I open an agent session on my Raspberry Pi and get a shell plus file browser inside it, zero inbound ports"—that is the product; the agent panel is a feature.

---

## 后续规划（非第一版）/ Later (not first release)

> 「第一版」指**首个可用版本（里程碑 M0–M4）**。M5 起为第二阶段。
> "First release" = the first usable version (milestones M0–M4). M5 onward is the next stage.

- **任务系统 / Task system**：完整任务的定义、状态、进度与续跑
- **自动化 / Automation**：定时 / 事件触发的任务编排
- **connector 插件化 / Connector plugins**：SSH / SFTP / MySQL / PostgreSQL / Redis / SQLite / S3 做成插件按需加载；额外的**网络抓包插件**与**端口转发插件**
- **社交软件接入 / Social channels**：各 IM 作为 agent 的输入输出通道
- **客户端 / Native client**：原生客户端本身也是一个能操作本机 / 服务器的 agent，可包裹开源软件实现，不一定要自研 agent 逻辑
- **分布式推理 / Distributed inference**：把接入设备作为协同的大模型运行节点（评估见 `docs/architecture/inference.md`）

---

## 设计基调 / Design Tone

深色优先、等宽字体、高信息密度、键盘优先；终端 / 文件 / git 是**会话内面板**，易用即可，
**不刻意对标 Xshell / Xftp**；对话与动作流对标 dsh，强调「逐条可查、改动可见」。

Dark-mode first, monospace, high information density, keyboard-oriented; terminal / files / git
are **panels inside a session**—usable is enough, no deliberate benchmarking of Xshell / Xftp;
transcript & action stream borrows dsh's "every step inspectable, every file change visible" philosophy.

---

## 技术选型 / Stack

- **Rust**（Hub + 节点进程）：单二进制、无运行时、交叉编译 amd64 / arm64。
- **SQLite**：`journal_mode=WAL` + `busy_timeout` 必须设，否则多设备重连即 `SQLITE_BUSY`。
- **ACP（Agent Client Protocol）**：会话层不自建。
- **轨迹**：append-only 事件流，Web 逐条回放。
- **推理**：基于 llama.cpp / mesh-llm / prima.cpp 等现成开源实现，不自研内核。
- Web：**Vue 3 + Bun**（+ Vite）。前端命令一律 `bun run`，禁用 `npm run`。

---

## 许可 / License

待定 / TBD.
