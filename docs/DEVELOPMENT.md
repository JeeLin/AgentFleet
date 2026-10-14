# 开发索引 / Development

> 技术栈、crate 结构、里程碑总览。架构决策见 `architecture/`。
> Stack, crate layout, milestone overview. Architecture decisions in `architecture/`.

---

## 1. 技术栈 / Stack

| 层 | 选型 | 理由 |
|----|------|------|
| Hub + node | **Rust**（单二进制） | 跑在树莓派 / 小内存机器，无运行时，交叉编译 amd64+arm64 |
| Web | **Vue 3 + Bun**（+ Vite） | 已定；Bun 跑构建与测试，Vue 3 生态成熟（xterm.js / diff 视图现成） |
| 存储 | SQLite（WAL + busy_timeout） | 单用户自托管，零运维 |
| 会话 | ACP | 不自建 agent 会话协议 |
| 轨迹 | append-only 事件流（dsh 式） | Web 逐条回放 + 续跑审计 |
| 推理 | llama.cpp / mesh-llm / prima.cpp 等开源 | 不自研内核 |

详见 `AGENTS.md`。

---

## 2. crate 结构（初版，Rust workspace）

依赖声明集中在根 `Cargo.toml` 的 `[workspace.dependencies]`，子 crate 用 `workspace = true`。

```text
crates/
  hub-core/     Hub 核心：注册表、连接注册表、会话/任务编排、推理网关
  hub-server/   Hub HTTP/WS 服务 + 前端托管
  node-core/    node 核心：连接、connector（terminal/files/git/exec，**仅面向页面用户**）、能力上报
  node-cli/     node 二进制（enroll / run）
  protocol/     共享：消息定义、(node_id, channel_id) 通道、事件类型
packages/
  web/          Vue 3 + Bun + Vite 前端；构建产物由 hub-server 托管
```

- 依赖方向：`node-*` → `protocol`；`hub-*` → `protocol`。**不反向依赖**。
- connector 是有类型的 trait，各自独立实现，不共用多态 Resource 分支。
- 前端**只在 `packages/web` 内**依赖后端类型，不反向进 Rust 侧。

---

## 3. 里程碑总览 / Milestones

状态词汇三态：`✅ 已完成` / `🔄 当前` / 无标记（未开始）。

| # | 里程碑 | 目标 |
|---|--------|------|
| M0 | 设备接入 | `node enroll` 一条命令；**密钥对在 node 本地生成、Hub 只存指纹**；supervisor/worker 两层（身份随 supervisor，升级只换 worker）；单条 WS 主动外联；心跳即能力上报；掉线指数退避重连；防重复注册（同一指纹不得新建设备） |
| M1 | 会话工作区与首个演示 | 工作区 `(node_id, path)` 落库；会话创建即挂载工作区；**在树莓派上开一个 agent 会话，会话内的 terminal / files 面板可用，零入站端口、不装 openssh-server**；exec connector（一次性命令、无 PTY，供页面与 Hub 侧系统调用）随此里程碑落地。**connector 只面向页面用户，不向 agent 暴露** |
| M2 | 工作区供料与同步 | Hub 内容寻址仓库（CAS）；manifest 差分物化到 `(node_id, path)`；排除依赖目录；技能/配置走托管覆盖层（只读 + 漂移检测 + **全局/会话两层权限规则**）；产物归档（见 `architecture/workspace-and-sync.md`） |
| M3 | ACP 会话与轨迹 | 接入 ACP；Trajectory 逐条可查（事件表含类型/输入/输出/耗时/归属节点）；会话开始建基线，**随时可看**改动文件与 diff |
| M4 | Git 界面 | 会话工作区的 git 状态 / diff / 提交视图；**此时会话面板补齐第三个 git 面板** |
| M5 | 子 agent、任务分发与 agent 安装 | oh-my-opencode 式子 agent 体系；**按节点能力/负载自动路由**（硬过滤 → 有序打分 → 可钉死，见 `architecture/scheduling.md`）；子任务工作区来源分发时确认；**Hub 一键把 agent 装到设备**并套用托管配置（升级策略未决，见 `architecture/workspace-and-sync.md` §4.4） |
| M6 | 推理网关 | 一个 OpenAI 兼容端点，背后是 N 台设备各自的全量副本（阶段 1，请求级路由） |
| M7 | connector 插件化 | 把协议族做成插件按需加载 + 抓包插件 + 端口转发插件 |
| M8 | 任务系统 | 完整任务定义 / 状态 / 进度 / 断点续跑（跨节点任务连贯性，见 `architecture/task-continuity.md`） |
| 后续 | 后续能力 | 自动化、社交软件接入、原生客户端、分布式推理进阶（RPC 层切分 / MoE 分片） |

> 首个演示是 M1：在树莓派上开一个会话，会话里的 shell 与文件浏览就能用，且零入站端口。
> 终端 / 文件 / git 都在会话内，因此演示必须经由会话，不能脱离 agent 直接开终端。

> M2 排在 ACP 与 Git 之前：会话要在目标设备上正确跑起来，工作区必须先能被喂进去。
> git 项目走「Hub 只发 commit 引用、节点自己去 origin 拉」的主路径；非 git 项目走 CAS 差分。

> 第一版 = M0–M4（首个可用版本）。M5 起为第二阶段。
> The first usable version is M0–M4; M5 onward is the next stage.

> 「后续」不是编号里程碑，不在 M0–M8 范围内，需在开工前单独排期与编号。
> 第二阶段从 M5（子 agent 与任务分发）开始。
> "Later" is not a numbered milestone; schedule and number it separately before starting.

---

## 4. 架构文档索引

- `architecture/README.md` — 进程模型 / 连接与通道 / 数据模型 / connector / 会话 / 子 agent
- `architecture/session-and-trajectory.md` — ACP 会话恢复 + dsh 式轨迹 + 改动文件/diff
- `architecture/task-continuity.md` — 跨设备任务连贯性模型
- `architecture/workspace-and-sync.md` — Hub 内容寻址仓库、项目文件供料、配置铺设、产物归档
- `architecture/scheduling.md` — 任务路由：心跳上报字段、硬过滤、有序打分、任务状态机
- `architecture/security-and-lifecycle.md` — 凭据模型（明文存储的取舍与强制防护）、supervisor/worker、设备升级与掉线重连
- `architecture/inference.md` — 分布式推理完整评估与分阶段路径

---

## 5. 开发流程 / Workflow

本项目使用 **mdflow** 里程碑流程：写文档 → 设计核对 → 开发 → 精简 → 审查 → 测试 → 验收 → 提交。
项目约定见 `AGENTS.md`；里程碑文档落在 `.mdflow/milestones/`（本地产物，不入库）。