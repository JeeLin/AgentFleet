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
  hub-server/   Hub HTTP/WS 服务 + 前端托管（进程入口 `main.rs` 在此）
  hub-storage/  Hub 存储：SQLite 连接（WAL + busy_timeout）、schema.sql、版本化迁移、数据目录解析与权限
  node-core/    node 核心：连接、connector（terminal/files/git/exec，**仅面向页面用户**）、能力上报
  node-cli/     node 二进制（enroll / run / worker），**对外命令名 `afcli`**（避免与 Node.js 运行时 `node` 撞名）
  protocol/     共享：消息定义、(node_id, channel_id) 通道、事件类型
packages/
  web/          Vue 3 + Bun + Vite 前端；构建产物由 hub-server 托管
```

- 依赖方向：`node-*` → `protocol`；`hub-*` → `protocol`；`hub-core` → `hub-storage`。**不反向依赖**。
- `hub-storage` 是唯一直接摸 SQLite 的 crate——`hub-core` 与 `hub-server` 不写裸 SQL。
- connector 是有类型的 trait，各自独立实现，不共用多态 Resource 分支。
- 前端**只在 `packages/web` 内**依赖后端类型，不反向进 Rust 侧。

## 2.1 CI 与发布

### 门禁（已有）

`.github/workflows/ci.yml` 跑**门禁**，与本地 `mise run ci` 是同一串：
check → fmt → lint → test → **cov-gate**（覆盖率 100% 阻塞）→ **cov-anchors**（例外理由的锚点核对）
→ web-check / web-test / web-build。
工具链版本只写在 `mise.toml` 的 `[tools]` 一处，workflow 从那里读——
覆盖率数值跨工具链不可比，版本写两处迟早只改一处。

**门禁清单写两处（`mise.toml` 的 `[tasks.ci].depends` 与 `ci.yml` 的 steps），改一边必须改另一边**：
漏改的那份会让 CI 少跑一道门禁而没人发现——少跑的那一道不会红，它只是不存在。
新增门禁时先加进 `mise.toml`，再把同一条抄进 `ci.yml`。

**只在推 tag（`v*`）时触发**：全量插桩的覆盖率是最贵的一步，
每个 push / PR 都跑一遍等于把 runner 时间烧在没定稿的提交上。
CI 的定位是「发布前的最后一道闸」，每次提交的反馈由本地 `mise run ci` 承担。

### 发布（**未落地**，参考 REX 的形态）

`main` 合入打 tag（`vX.Y.Z`）触发。计划中的形态是**构建一次，三个消费者**：

```text
build-binaries（5 平台矩阵，原生编译）
    ├─→ docker-hub：Hub 镜像，内嵌全部 arch 的 afcli 到 /app/node-binaries/
    ├─→ docker-node：afcli 镜像（linux/amd64 + linux/arm64，按 TARGETARCH 选二进制）
    └─→ release：GitHub Release + SHA256SUMS + 从 CHANGELOG 抽 body
build-frontend（bun install --frozen-lockfile → type-check → lint → test → build）
    └─→ Rust 侧 include_dir! 编译期嵌入，运行时不需要 dist 目录
```

| 文件 | 作用 | 状态 |
|---|---|---|
| `.github/workflows/ci.yml` | 门禁（= `mise run ci`），**仅 tag 触发** | **已有** |
| 发布 workflow（多平台二进制 / 镜像 / Release） | 上述发布全流程 | 未落地（M1 不需要） |
| `Dockerfile.hub` | Hub 镜像；`COPY dist/nodes/ /app/node-binaries/` | 未落地 |
| `Dockerfile.node` | afcli 镜像；`COPY dist/afcli-${TARGETARCH}` | 未落地 |
| `docker-compose.hub.yaml` / `.agent.yaml` | 自托管部署 | 未落地 |

- **原生编译优先于交叉编译**：GitHub 有 `ubuntu-24.04-arm` runner，aarch64 直接原生编
  （REX 的教训：交叉编 OpenSSL/zlib 会架构错配）。
- **必须静态链 OpenSSL + zlib**，产出自包含二进制；REX 加了 `dumpbin /dependents`
  断言 exe 不依赖 `libssl|libcrypto` DLL，**这个断言照抄**。
- **节点只从 Hub 拉二进制，不直连 GitHub**（REX 有 GitHub 回退，我们刻意不做，
  见 `architecture/security-and-lifecycle.md` §7.1.4）。
- 镜像推到 `ghcr.io/<owner>/agentfleet-hub` 与 `agentfleet-node`。

---

## 3. 里程碑总览 / Milestones

状态词汇三态：`✅ 已完成` / `🔄 当前` / 无标记（未开始）。

| # | 里程碑 | 目标 | 承接 §3.1 能力 |
|---|--------|------|--------------|
| M0 ✅ 已完成 | 设备接入 | `afcli run` 一条命令（首次自动 enroll）；**密钥对在 node 本地生成、Hub 只存指纹**；supervisor/worker 同二进制双角色（`run --worker`，回滚由未被替换的 supervisor 执行，见 `DECISIONS.md` D1/D2）；单条 WS 主动外联；心跳即能力上报；掉线指数退避重连；防重复注册（同一指纹不得新建设备）；`afcli log` 看最近日志；**WS 协议层与 enroll 时序一并设计**（见 `architecture/protocol.md`） | 设备接入、节点身份与状态、存储与启动基座 |
| M1 🔄 当前 | 会话容器与首个演示 | 工作区 `(node_id, path)` 落库；会话创建即挂载工作区；**在树莓派上开一个会话，会话内的 terminal / files / exec 面板可用，零入站端口、不装 openssh-server**；exec connector（一次性命令、无 PTY；**只经会话通道触发**，connector 跑在设备上，Hub 没有旁路入口）随此里程碑落地。**connector 只面向页面用户，不向 agent 暴露**。**「会话」指页面级容器，不是 ACP agent 会话**——ACP 归 M3 | Bash/PTY、文件浏览 |
| M2 | 工作区供料与同步 | Hub 内容寻址仓库（CAS）；manifest 差分物化到 `(node_id, path)`；排除依赖目录；技能/配置走托管覆盖层（只读 + 漂移检测 + **全局/会话两层权限规则**）；产物归档（见 `architecture/workspace-and-sync.md`） | 工作区供料、统一配置 agent、产物归档、模型配置一处切换 |
| M3 | ACP 会话与轨迹 | 接入 ACP；Trajectory 逐条可查（事件表含类型/输入/输出/耗时/归属节点）；会话开始建基线，**随时可看**改动文件与 diff | 对话与动作流、AI agent 管理与对话 |
| M4 | Git 界面 | 会话工作区的 git 状态 / diff / 提交视图；**此时会话面板补齐第三个 git 面板** | Git 操作界面 |
| M5 | 任务分发与 agent 安装 | **Hub 新建任务 → 仅节点内编排**：Hub 建任务，**node 决定开几个 sub-task**（按 `DECISIONS.md` D9 收窄为节点内编排，跨设备分发留 M8——其三支柱 CAS 工作区物化 / checkpoint 续跑 / 环境声明分别在 M2 / M8，M5 做必残缺）；任务工作区来源分发时确认；预置的专家 agent 定义走托管覆盖层（约束 8）；**Hub 一键把 agent 装到设备**并套用托管配置（升级策略未决，见 `architecture/workspace-and-sync.md` §4.4） | Hub 一键安装 agent |
| M6 | 推理网关 | 一个 OpenAI 兼容端点，背后是 N 台设备各自的全量副本（阶段 1，请求级路由） | —（新增能力） |
| M7 | connector 插件化 | 把协议族做成插件按需加载 + 抓包插件 + 端口转发插件 | —（`PRODUCT.md` §3.2 后续项） |
| M8 | 任务系统 | 完整任务定义 / 状态 / 进度 / 断点续跑（跨节点任务连贯性，见 `architecture/task-continuity.md`）。**前置条件：M2 的 CAS 与 checkpoint 重建**——跨设备续跑必须能从 CAS 取到所需文件与产物，否则「换一台机器接着跑」没有数据基座 | —（`PRODUCT.md` §3.2 后续项） |
| 后续 | 后续能力 | 自动化、社交软件接入、原生客户端、分布式推理进阶（RPC 层切分 / MoE 分片） | —（`PRODUCT.md` §3.2 后续项） |

> 首个演示是 M1：在树莓派上开一个**会话**（页面级容器，不是 ACP agent 会话——ACP 归 M3），会话里的 shell 与文件浏览就能用，且零入站端口。
> 终端 / 文件 / git 都在会话内，因此演示必须经由会话，不能脱离会话直接开终端。

> M2 排在 ACP 与 Git 之前：会话要在目标设备上正确跑起来，工作区必须先能被喂进去。
> git 项目走「Hub 只发 commit 引用、节点自己去 origin 拉」的主路径；非 git 项目走 CAS 差分。

> 第一版 = M0–M4（首个可用版本）。M5 起为第二阶段。
> The first usable version is M0–M4; M5 onward is the next stage.

---

## 4. 范围映射（两份文档的唯一对齐依据）

`PRODUCT.md` §3.1 定范围，本节定排期，**中间的这张表是唯一真相源**。
里程碑表最后一列「承接 §3.1 能力」即由此得出。

### 4.1 对齐契约（每次改动必查）

改动范围或排期时，**四件事必须同时成立**：

1. **每个 §3.1 能力都能找到承接它的里程碑**（下表「承接」列无遗漏）
2. **每个里程碑都有对应架构依据**，或明确标注「新增能力 / 无对应设计」
3. **第一版口径三处一致**：PRODUCT §3.2 的标注、DEVELOPMENT §3 的标注、README 的标注
4. **不产生「承诺了但没排期」或「排了期但没承诺」的项**
5. **每个里程碑必须有存储/进程入口基座**——子任务里若出现写 `device` 表、
   用 `secret` 表、起 `main.rs`，就必须有一个前置子任务产出 schema + 迁移 + 进程入口。
   M0 第 1 轮设计审查即漏此条（9 个子任务无人建表）。

> 这份文档已三次出现漂移（Xshell 对标基调、「Hub 一键配置 agent」推后与否、
> git 面板在哪个里程碑），全部源于两份文档各写各的、靠人工对齐。
> **步骤 2 设计核对时把本节当固定检查项，逐条走，不要凭印象判断。**

### 4.2 逐条承接

| PRODUCT §3.1 能力 | 承接里程碑 | 架构依据 |
|------------------|-----------|---------|
| 设备接入 | M0 | `security-and-lifecycle.md` §4（supervisor/worker、身份） |
| 节点身份与状态 | M0 | `security-and-lifecycle.md` §4.0/§5；`scheduling.md` §2 |
| 存储与启动基座 | M0（新增，非 §3.1 能力） | `architecture/README.md` §3/§8；`security-and-lifecycle.md` §2.1（强制防护） |
| Bash / PTY | M1 | `architecture/README.md` §4.1（工作区绑定）；`design-system.md` §7.0（入口是会话） |
| 文件浏览 | M1 | 同上 |
| AI agent 管理与对话 | M3 | `session-and-trajectory.md`（ACP 会话）。**M1 的会话容器不是 agent 会话**，见 §3 M1 行 |
| 对话与动作流 | M3 | `session-and-trajectory.md` §3/§4/§7（轨迹表） |
| Git 操作界面 | M4 | `architecture/README.md` §4 |
| 工作区供料 | M2 | `workspace-and-sync.md` §1/§3 |
| 统一配置 agent | M2 | `workspace-and-sync.md` §4（托管覆盖层 + 两层权限） |
| 模型配置一处切换 | M2 | `workspace-and-sync.md` §4.3；`security-and-lifecycle.md` §3 |
| 产物归档 | M2 | `workspace-and-sync.md` §5 |
| Hub 一键安装 agent | M5 | `workspace-and-sync.md` §4.4；`security-and-lifecycle.md` §7 |

### 4.3 反向检查

| 来源 | 项 | 承接 |
|------|---|---|
| `PRODUCT.md` §3.2 后续 | connector 插件化 | M7 |
| `PRODUCT.md` §3.2 后续 | 任务系统 | M8 |
| `PRODUCT.md` §3.2 后续 | 分布式推理进阶 | M6（阶段 1）+ 后续 |
| `PRODUCT.md` §3.2 后续 | 自动化 / 社交软件 / 原生客户端 | **无排期**（开工前单独排期，见 §3 下方说明） |
| `PRODUCT.md` §3.1「Hub 一键安装 agent」行内 | **afcli 自身的升级通路**（页面显示各设备 afcli 版本号 → 手动触发升级 → 「自动升级」开关；版本号由 Hub 下发，失败回滚并回报） | M2（与 agent 托管配置同一节落地，依据 `workspace-and-sync.md` §4.5）。**M1 只交付前半段**：`/api/devices` 给出版本号（`node_version`，即改名前 `agent_version`），触发与回滚不在 M1 |

> 「后续」不是编号里程碑，不在 M0–M8 范围内，需在开工前单独排期与编号。
> 第二阶段从 M5（任务分发与 agent 安装）开始。
> "Later" is not a numbered milestone; schedule and number it separately before starting.

---

## 5. 架构文档索引

- `architecture/README.md` — 进程模型 / 连接与通道 / 数据模型 / connector / 会话 / 任务分发
- `architecture/session-and-trajectory.md` — ACP 会话恢复 + dsh 式轨迹 + 改动文件/diff
- `architecture/task-continuity.md` — 跨设备任务连贯性模型
- `architecture/workspace-and-sync.md` — Hub 内容寻址仓库、项目文件供料、配置铺设、产物归档
- `architecture/scheduling.md` — 任务路由：心跳上报字段、硬过滤、有序打分、任务状态机
- `architecture/security-and-lifecycle.md` — 凭据模型（明文存储的取舍与强制防护）、supervisor/worker、设备升级与掉线重连
- `architecture/protocol.md` — WS 帧格式、enroll/challenge 握手、通道复用与流控/信用、重连序列号
- `architecture/inference.md` — 分布式推理完整评估与分阶段路径

---

## 5. 开发流程 / Workflow

本项目使用 **mdflow** 里程碑流程：写文档 → 设计核对 → 开发 → 精简 → 审查 → 测试 → 验收 → 提交。
项目约定见 `AGENTS.md`；里程碑文档落在 `.mdflow/milestones/`（本地产物，不入库）。