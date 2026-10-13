# AgentFleet

个人自托管的设备 / Agent 平台 / Personal Self-Hosted Device & Agent Platform

把自己拥有的每一台设备——服务器、电脑、手机、树莓派——注册为节点；
在一个 Web 页面里管理全部节点，在节点上打开终端与文件，运行 AI agent 会话，
并把任务分发给最合适的设备执行。

> **单用户 · 自托管 · 数据自主掌控 · 零入站端口 · 深色优先**
> **Single-user · Self-hosted · Data sovereignty · Zero inbound ports · Dark-mode first**

---

## 名字含义 / Name Meaning

- **AgentFleet** = 一支由**你拥有的全部设备**组成的 **agent 舰队**。
  每一台设备是一个节点，每个节点是一个可执行任务的 agent 载体。
  A fleet of agents, formed by every device you own.
- **Hub** 是你自己跑的服务端（托管前端 + 聚合节点与 agent）；
  **node** 是跑在目标设备上的接入进程。两者都是单二进制。
  Hub is the server you host yourself; node is the enrollment process on each device.

---

## 核心承诺 / Core Promises

| 承诺 | 传统方式 | AgentFleet |
|------|---------|-----------|
| 所有设备一个平台管 | 逐个 SSH / 多个客户端 | **一个工作空间**管全部节点 |
| 公网 + 内网一个服务 | VPN / 端口转发 / 跳板机 | 节点反向隧道，**零端口暴露** |
| 数据自主掌控 | 在线 SaaS，数据在第三方 | 完全**自托管**，数据在你手中 |
| agent 散落在各处 | 逐个开终端 / 多个客户端 | 统一接入，**一个界面里管理与对话** |
| 任务只在发出请求的那台机器上跑 | 手动搬代码、同步环境 | 按**设备能力**分发到合适节点执行 |

---

## 第一版做什么 / First Release Scope

| 能力 | 说明 |
|------|------|
| 设备接入 | 一条 CLI 命令注册设备，单条 WebSocket 主动外联；心跳即能力上报（os / arch / hostname / 版本 / 支持的 connector） |
| 节点身份 | 注册时由 Hub 签发 `node_id`；设备密钥对认证，不用静态共享 token |
| AI agent 管理与对话 | 接入各类 agent 软件（不同厂商、不同协议），一个界面里统一管理与对话 |
| 对话与动作流 | 任务与子任务的对话记录和动作在 Web 里直接查看，逐条可查，不是只给一句摘要 |
| 文件浏览 | 浏览 / 读写节点本地目录，写操作限制在工作区根内 |
| Git 操作界面 | 面向节点工作区的 git 状态 / diff / 提交视图 |
| Bash / PTY | 节点进程直接提供本地 shell，不需要节点装 `openssh-server`，也不需要 Hub 拨入 |

> 首个演示是 **「我的树莓派在浏览器里有 shell 和文件浏览，且零入站端口」**——
> 这是产品本身；agent 面板是一项功能。

---

## 后续规划（非第一版）/ Later

- **任务系统**：完整任务的定义、状态、进度与续跑
- **自动化**：定时 / 事件触发的任务编排
- **子 agent 体系**：可配置、开箱即用的子 agent，把单个完整任务分发到设备上执行
- **社交软件接入**：各 IM 作为 agent 的输入输出通道
- **客户端**：原生客户端本身也是一个能操作本机 / 服务器的 agent，
  可包裹开源软件实现，不一定要自研 agent 逻辑
- **分布式推理**：把接入设备作为协同的大模型运行节点（评估见 `docs/architecture/inference.md`）
- **端口转发**：把节点上的服务经 Hub 暴露给自己访问
- 视情况接入 REX 使用过的协议族

---

## 名字由来与立场 / Positioning

- **不做**：多用户 / RBAC / 团队协作、通用文件同步引擎、会话 transcript 集中存储、
  带 DAG 的重型调度器、任何需要消息中间件的设计。
- **刻意不做**异构设备张量并行推理作为第一版卖点——结论与可行路径见架构文档。
- 对外表述是「**一个由你 N 台设备支撑的平台**」，
  不是「N 台设备合成一台大模型机」。

---

## 技术选型 / Stack

- **Rust**（Hub + 节点进程）：单二进制、无运行时、交叉编译 amd64 / arm64。
- **SQLite**：`journal_mode=WAL` + `busy_timeout` 必须设，否则多设备重连即 `SQLITE_BUSY`。
- **ACP（Agent Client Protocol）**：会话层不自建。
- Web：待定（倾向 Vue 3 + Bun，独立选择，可换）。

---

## 许可 / License

待定。
