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

> **Hub 不会主动拨入你的任何设备**：一条命令接入，节点自己外联。
> **The Hub never dials into your devices**: enroll with one command, nodes dial out.

---

## 为什么 / Why

- **一个界面管全部设备**，不必逐台 SSH。**One console for every device**—no per-device SSH.
- **公网与内网同一个服务**，节点反向外联，**零端口暴露**。**One service** for public and LAN, nodes dial out, **zero port exposure**.
- **数据自主掌控**，完全**自托管**。**Data sovereignty**—fully **self-hosted**.
- **任务按设备能力分发**，交给最合适的节点。**Tasks routed by capability** to the best-fit node.

---

## 第一版范围 / First Release Scope

> 「第一版」指**首个可用版本（里程碑 M0–M4）**。M5 起为第二阶段。
> "First release" = the first usable version (M0–M4). M5 onward is the next stage.

- **设备接入**：一条 CLI 命令注册，单条 WebSocket 主动外联；心跳即能力上报。One CLI command, one outbound WebSocket, heartbeats carry capabilities.
- **节点身份与状态**：Hub 签发 `node_id`，设备密钥对认证，不用静态共享 token。Hub-issued `node_id`, key-pair auth, no static shared token.
- **AI agent 管理与对话**：各类 agent 统一配置、启动、对话。Configure, launch and chat with any agent in one place.
- **对话与动作流**：dsh 式 Trajectory 逐条可查，列出本次改动的**文件**与 diff。A dsh-style trajectory, step by step, plus changed files and diffs.
- **会话内面板**：终端 / 文件 / git 绑定会话工作区 `(node_id, path)`，进入才建立、退出即释放。Panels inside a session, bound to its workspace, attach on entry and release on exit.
- **工作区供料与统一配置**：内容寻址仓库喂项目文件（git 只传 commit 引用），统一管理子 agent 与技能，**密钥永不上传**。A content-addressed store feeds project files, sub-agents and skills are managed centrally, **node keys never leave the device**.
- **产物归档**：制品归档到 Hub，设备关机后仍可下载。Outputs archived on the Hub, downloadable after the device powers off.

> 完整范围与用户流程见 `docs/PRODUCT.md` §3。
> Full scope and user flows: `docs/PRODUCT.md` §3.

---

## 设计基调 / Design Tone

- **深色优先、等宽字体、高信息密度、键盘优先**。Dark-mode first, monospace, high density, keyboard-oriented.
- **终端 / 文件 / git 是会话内的面板**，易用即可。**Panels inside a session**—usable is enough.
- **对话与动作流对标 dsh**：逐条可查，改动可见。Borrow dsh: every step inspectable, every change visible.

---

## 技术选型 / Stack

Rust 单二进制（Hub + node）、SQLite（WAL）、ACP 会话层、append-only 轨迹、开源推理内核、Vue 3 + Bun。
Rust single binaries (Hub + node), SQLite (WAL), ACP sessions, append-only trajectories, open-source inference kernels, Vue 3 + Bun.
详见 `docs/DEVELOPMENT.md` §1 / See `docs/DEVELOPMENT.md` §1.

---

## 文档 / Documentation

- `docs/PRODUCT.md` — 产品功能边界、用户可见流程、术语表
- `docs/DEVELOPMENT.md` — 技术栈、crate 结构、里程碑总览
- `docs/architecture/` — 架构决策目录
- `AGENTS.md` — 项目约束与协作约定
- `CHANGELOG.md` — 变更日志
- `docs/BUGS.md` — 缺陷池

---

## 许可 / License

Apache-2.0 / Apache-2.0.

---

## 贡献 / Contributing

贡献前请阅读 `AGENTS.md` 的项目约束与协作约定。
Read the project constraints and collaboration conventions in `AGENTS.md` before contributing.
