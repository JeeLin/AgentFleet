# 开发索引 / Development

> 技术栈、crate 结构、里程碑总览。架构决策见 `architecture/`。
> Stack, crate layout, milestone overview. Architecture decisions in `architecture/`.

---

## 1. 技术栈 / Stack

| 层 | 选型 | 理由 |
|----|------|------|
| Hub + node | **Rust**（单二进制） | 跑在树莓派 / 小内存机器，无运行时，交叉编译 amd64+arm64 |
| Web | 待定（倾向 Vue 3 + Bun） | 独立选择，不构成阻塞 |
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
  node-core/    node 核心：连接、connector（terminal/files/git/exec）、能力上报
  node-cli/     node 二进制（enroll / run）
  protocol/     共享：消息定义、(node_id, channel_id) 通道、事件类型
```

- 依赖方向：`node-*` → `protocol`；`hub-*` → `protocol`。**不反向依赖**。
- connector 是有类型的 trait，各自独立实现，不共用多态 Resource 分支。

---

## 3. 里程碑总览 / Milestones

状态词汇三态：`✅ 已完成` / `🔄 当前` / 无标记（未开始）。

| # | 里程碑 | 目标 |
|---|--------|------|
| M0 | 设备接入 | `node enroll` 一条命令；密钥对认证；单条 WS 主动外联；心跳即能力上报；三台设备同时在线且各自身份不串（防第二台抢走第一台身份） |
| M1 | 首个演示 | 本地 PTY + 本地文件浏览：从浏览器在树莓派本机开终端并浏览 `~/`，零入站端口、不装 openssh-server |
| M2 | Git 界面 | 面向节点工作区的 git 状态 / diff / 提交视图 |
| M3 | ACP 会话 | 接入 ACP；在某台**指定**设备上开一个 agent 会话并对话，Trajectory 逐条可查 + 改动文件/diff 视图 |
| M4 | 子 agent 与任务分发 | oh-my-opencode 式子 agent 体系；按设备能力/性能自动调度，把完整任务分发到节点执行 |
| M5 | 推理网关 | 一个 OpenAI 兼容端点，背后是 N 台设备各自的全量副本（阶段 1，请求级路由） |
| M6 | connector 插件化 | 把协议族做成插件按需加载 + 抓包插件 + 端口转发插件 |
| M7 | 任务系统 | 完整任务定义 / 状态 / 进度 / 断点续跑（跨节点任务连贯性，见 `architecture/task-continuity.md`） |
| 后续 | 后续能力 | 自动化、社交软件接入、原生客户端、分布式推理进阶（RPC 层切分 / MoE 分片） |

> 首个演示是 M1，不是 M3。「树莓派在浏览器里有 shell 和文件浏览、零入站端口」是产品本身。

---

## 4. 架构文档索引

- `architecture/README.md` — 进程模型 / 连接与通道 / 数据模型 / connector / 会话 / 子 agent
- `architecture/session-and-trajectory.md` — ACP 会话恢复 + dsh 式轨迹 + 改动文件/diff
- `architecture/task-continuity.md` — 跨设备任务连贯性模型
- `architecture/inference.md` — 分布式推理完整评估与分阶段路径

---

## 5. 开发流程 / Workflow

本项目使用 **mdflow** 里程碑流程：写文档 → 设计核对 → 开发 → 精简 → 审查 → 测试 → 验收 → 提交。
项目约定见 `AGENTS.md`；里程碑文档落在 `.mdflow/milestones/`（本地产物，不入库）。