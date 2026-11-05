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
  node-connector/ node 侧能力实现：connector（terminal/files/exec/git，**仅面向页面用户**）+ 真 PTY（`pty.rs`）
  node-core/    node 核心：连接、帧分派、能力上报、agent 探测与 ACP 客户端（**只经注册表接口用 connector**）
  node-cli/     node 二进制（enroll / run / worker），**对外命令名 `afcli`**（避免与 Node.js 运行时 `node` 撞名）
  protocol/     共享：消息定义、(node_id, channel_id) 通道、事件类型
packages/
  web/          Vue 3 + Bun + Vite 前端；构建产物由 hub-server 托管
```

- 依赖方向：`node-*` → `protocol`；`hub-*` → `protocol`；`hub-core` → `hub-storage`；
  `node-core` → `node-connector`。**不反向依赖**。
- `node-connector` 独立成 crate 的理由是 **v0.12.0 的按需加载**（crate 边界是它的前提），
  不是「代码多」。它**不依赖 `node-core`**：`pty.rs` 跟着搬过去正是为了避开
  `node-connector → node-core → node-connector` 的环（见 v0.7.0 里程碑 0.A）。
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

**`ci.yml` 与 `release.yml` 是同触发、两个 workflow**：`ci.yml` 是闸，`release.yml` 是产线。
已知取舍——两者并行，**门禁红时镜像仍可能被推上去**。所以 tag 只在
`mise run ci` 全绿之后才打，不靠 CI 互相阻塞。

### 发布（**v0.7.1 落地**）

打 tag（`vX.Y.Z`）触发 `.github/workflows/release.yml`，形态是**前端先编、二进制一次、
三个消费者**：

```text
build-frontend（bun install --frozen-lockfile → build，dist 作为 artifact 上传）
    ↓ needs（硬前置，不是优化）
build-binaries（5 平台矩阵，**原生编译**）
    ├─→ docker-hub：Hub 镜像（afhub + afcli，前端已编译期嵌入，镜像里不另放 dist）
    ├─→ docker-node：afcli 镜像（linux/amd64 + linux/arm64，按 TARGETARCH 选二进制）
    └─→ release：GitHub Release + SHA256SUMS + 从 CHANGELOG.md 抽 body
```

**`build-frontend` 是硬前置**：`include_dir!` 在编译期嵌入 dist，而 dist 缺失时
`crates/hub-server/build.rs` 不失败——它造一个占位页、打一条 `cargo:warning`，二进制照编。
后果是「Hub 装起来打开是 Not built 占位页」能不红任何一步地走到用户手里。所以矩阵里
**每个平台在编之前**都断言 `packages/web/dist/index.html` 存在，缺了就 fail。

| 文件 | 作用 | 状态 |
|---|---|---|
| `.github/workflows/ci.yml` | 门禁（= `mise run ci`），**仅 tag 触发** | **已有** |
| `.github/workflows/release.yml` | 发布全流程（前端 / 二进制矩阵 / 镜像 / Release） | **v0.7.1 落地** |
| `Dockerfile.hub` | Hub 镜像；`FROM scratch` + musl 静态二进制 | **v0.7.1 落地** |
| `Dockerfile.node` | afcli 镜像；`COPY dist/${TARGETARCH}/afcli-…` | **v0.7.1 落地** |
| `docker-compose.hub.yaml` / `docker-compose.agent.yaml` | 自托管部署 | **v0.7.1 落地** |
| Hub 侧「设备从 Hub 拉二进制」的下载路由 | `DEVELOPMENT.md` §2 蓝图里 `/app/node-binaries/` 的**真正用途** | **未落地（v0.8.0）**：二进制已放进镜像，路由还没有 |

- **原生编译优先于交叉编译**：GitHub 有 `ubuntu-24.04-arm` runner，aarch64 直接原生编
  （REX 的教训：交叉编 OpenSSL/zlib 会架构错配）。
- **「静态链 OpenSSL」在本仓不适用，但断言照抄**：`Cargo.toml` 的 `reqwest` 是
  `rustls-tls`（`default-features = false`），整棵树**不链系统 OpenSSL**。所以真正要
  静态的是 libc，用 **musl 目标**解决。REX 那条「产物不依赖 `libssl|libcrypto`」的
  自包含断言仍然照抄，只是判据按平台分开：musl 用 `ldd` 判「not a dynamic executable」，
  gnu 用 `ldd` 判不含 `libssl|libcrypto`，windows-gnu 用
  `x86_64-w64-mingw32-objdump -p` 判 DLL 列表里没有它们。**断言在 workflow 里，不在文档里**。
- **节点只从 Hub 拉二进制，不直连 GitHub**（REX 有 GitHub 回退，我们刻意不做，
  见 `architecture/security-and-lifecycle.md` §7.1.4）。
- 镜像推到 `ghcr.io/<owner>/agentfleet-hub` 与 `agentfleet-node`。只有正式 tag 才更新
  `latest`（tag 名里带 `-` 的预发版本不会顶掉它）。
- **Hub 容器必须显式放开回环限制**：`afhub` 缺省只绑回环（约束 1）。容器网络是隔离的，
  所以 `Dockerfile.hub` 设 `AFHUB_ALLOW_NON_LOOPBACK=1` + `AFHUB_ADDR=0.0.0.0:8080`。
  开关**只认确切的 `1`**（`0` / `true` / 空串一律当没设），理由见
  `crates/hub-server/src/main.rs` 的 `resolve_addr`。裸机部署不变：仍然只许回环。

---

## 3. 里程碑总览 / Milestones

状态词汇三态：`✅ 已完成` / `🔄 当前` / 无标记（未开始）。

| 版本 | 里程碑 | 目标 | 承接 `PRODUCT.md` §6.1 能力 |
|------|--------|------|--------------|
| v0.1.0 ✅ 已完成 | 设备接入 | `afcli run` 一条命令（首次自动 enroll）；**密钥对在 node 本地生成、Hub 只存指纹**；supervisor/worker 同二进制双角色（`run --worker`，回滚由未被替换的 supervisor 执行，见 `DECISIONS.md` D1/D2）；单条 WS 主动外联；心跳即能力上报；掉线指数退避重连；防重复注册（同一指纹不得新建设备）；`afcli log` 看最近日志；**WS 协议层与 enroll 时序一并设计**（见 `architecture/protocol.md`） | 设备接入、节点身份与状态、存储与启动基座 |
| v0.2.0 ✅ 已完成 | 会话容器与首个演示 | 工作区 `(node_id, path)` 落库；会话创建即挂载工作区；**在树莓派上开一个会话，会话内的 terminal / files / exec 面板可用，零入站端口、不装 openssh-server**；exec connector（一次性命令、无 PTY；**只经会话通道触发**，connector 跑在设备上，Hub 没有旁路入口）随此里程碑落地。**connector 只面向页面用户，不向 agent 暴露**。**「会话」指页面级容器，不是 ACP agent 会话**——ACP 归 v0.3.0 | Bash/PTY、文件浏览 |
| v0.3.0 ✅ 已完成 | ACP 对话 | 接入 ACP（`session-and-trajectory.md`）：设备探测并心跳上报 agent 运行时；**agent 由用户自己在设备上安装，Hub 只扫描 + 存启动配置**（2026-10-26 裁定，不代装）；建会话可选用该设备上已装且可用的 agent（未装 → 409）；ACP 客户端（`initialize` / `session/new` / `session/prompt` / `session/update` / `session/cancel` / `session/load`）；`session/update` 事件流落 append-only 轨迹表（类型/输入/输出/耗时/归属节点）；对话页面。**删除语义三分离**：卸载 agent 不删会话、删设备不删会话、删工作区有会话则拒。**不用外键**——归属关系由业务层显式检查并给可读错误 | AI agent 管理与对话、对话与动作流 |
| v0.4.0 ✅ 已完成 | Git 界面 | 会话工作区的 git 状态 / diff / 提交视图；**此时会话面板补齐第三个 git 面板**。设备侧补上 `git` connector（此前 `connector/` 里从未实现——v0.1.0 的心跳曾硬编码上报过它，`heartbeat.rs` 的注释与 `the_registered_set_is_exactly_the_three_m1_connectors` 都记着这件事）：**只读子集** `status` / `diff` / `log`，经 `git` 子进程读取工作区。**不做 `commit` 及其它写操作**：v0.4.0 的目标是「看它改了什么」，而写操作要连带解决提交者身份、钩子与失败回滚，属另一件事，有明确需求再排期 | Git 操作界面 |
| v0.5.0 ✅ 已完成 | 会话界面重排（对齐 opencode 形态） | 会话页从「全局侧栏 + 会话内一个坞」改为 opencode 形态的**单层工作界面**：进会话即收起全局侧栏、会话铺满页面；顶部一条 tab 条切换会话；**对话在左，右侧一格显示一个工具，工具的唯一入口是头部「＋」（列全部三个，选中即替换当前）**——三个工具（terminal / files / git）平级且**互斥**（2026-10-29 裁决 C；此前一度规划「右上 git/文件 + 右下终端常驻」的两格方案，已被用户推翻）；左右一条分栏可拖拽且按 tab 各自保留比例（**没有上下分栏**）；窄屏（< 900px）降级为一个覆盖式抽屉。**纯前端**：不改任何 connector、帧协议或设备侧代码；`v0.4.0` 的 D10（tab 是视图切换器、切 tab 不释放通道）原样保留。**已知代价**：换工具会释放被替换者的通道，终端则是杀掉 PTY（`node-core` 的 `terminal.close()` 走 `pty.close()`）| 会话界面形态（**新增能力**，无对应 §6.1 能力项） |
| v0.6.0 ✅ 已完成 | 任务基准（**重挂**，用户 2026-11-03 裁定「视为结束，从 v0.7.0 开始」） | 会话工作区的**起点 + 产出概览**：建会话时用 git connector 的第四个只读 op `rev` 记下 `base_commit`，面板上显示「提交几次 / 另有几个文件未提交」。**产品主轴改版后**：这套逻辑是对的，但它算的是 `base_commit..HEAD`——在项目主轴下这是 **Execution 的产出 diff**，最终该挂在 Execution 上，不是会话上。因此本里程碑**收口为「只修真 bug、不再扩展」**，其「起点」概念作为 v0.7.0 的输入移交 | Git 操作界面（起点部分） |
| v0.7.0 ✅ 已完成 | **项目与绑定（+ 设备/agent 管理面扩展）** — **含「项目结构按新需求调整」** | **先做两件结构活**：①**crate 拆分**——`node-core` 拆出 `node-connector`（7 → 8 个 crate，纯搬家、单独一个提交，理由只此一条：v0.12.0 的按需加载必须靠 crate 边界）；②按项目主轴重排数据模型与模块分层——`schema` 增加 `project` / `task` / `execution` / `binding` 表与迁移脚本，`crates/` 与 `packages/web/src` 按新归属切分（会话挂到 Execution 下，不再是顶层）。**这是本里程碑的第一件事**，否则后面的项目实体只能挂在旧结构上，等于给会话再加一层壳。然后再做业务：项目实体三形态（中心式 / 隔离式 / 镜像式）；Hub 侧裸仓库（git CLI，藏在接口后）；Binding = `(project, device) → 固定目录`；设备侧 worktree 分任务 + 四层磁盘处置（`architecture/project-and-snapshot.md`）；前端新增 `projects` / `project` 路由，**项目页默认落在看板**。**v0.6.0 的 `base_commit` 概念在此落到 Task 上**：交接规则写死——`execution.base_commit` 是**唯一真相源**，会话上的 `base_rev` 降级为「会话第一段执行的起点快照」，只读、不再参与任何「相对它产出了什么」的计算（那套展示整体移到 Execution 上）。**不保留两份可写的起点**：同一个事实有两个写入口，迟早会出现「会话说基于 A、执行说基于 B」。**同时扩展设备页 / agent 页（不砍反扩）**：绑定关系（这台设备绑了哪些项目、绑在哪个固定目录）、磁盘配额与已用、失败归因（哪类失败集中在这台机器）、改配额与清理策略；agent 页加「该 agent 上跑过哪些任务 / 成功率」 | 项目实体、绑定与固定目录、设备管理、agent 管理 |
| v0.7.1 🔄 当前（推送 tag 后由 CI 验证） | **CI 镜像构建（发布通路）** | 从 v0.14.0 提前而来（用户 2026-11-05 裁定）。把 §2 的「发布」那一整段真正建起来：`build-frontend`（bun `--frozen-lockfile` → build，dist 作为 artifact）**硬前置**于 `build-binaries`（5 平台**原生**矩阵：x86_64 / aarch64 × gnu / musl + windows-gnu），再分发到三个消费者：`docker-hub`（`afhub` + `afcli`，前端**已在编译期嵌入**，镜像里不另放 dist）、`docker-node`（`afcli`，按 `TARGETARCH` 选二进制）、`release`（GitHub Release + `SHA256SUMS` + 从 `CHANGELOG.md` 抽 body）。镜像推 `ghcr.io/<owner>/agentfleet-hub` / `agentfleet-node`。**`build-frontend` 之所以是硬前置**：dist 缺失时 `build.rs` 只造占位页并打 warning，二进制照编——「Hub 打开是占位页」能不红任何一步地发出去，所以每个平台编之前都断言 `dist/index.html` 存在。**安全边界变更**：`afhub` 缺省仍只绑回环，新增 `AFHUB_ALLOW_NON_LOOPBACK=1` 供容器显式放开，**只认确切的 `1`**。版本号 `0.5.0` → `0.7.1`（tag 与二进制自报版本必须一致） | —（`DEVELOPMENT.md` §2.1 发布段） |
| v0.8.0 | **任务与看板** | Task 六态 + Task:Execution = 1:N；`done_when` = `criteria` 判定码 + `description`（`DECISIONS.md` D23）；任务级约束与固定过滤顺序（① allowed_* ② 隔离策略 ③ 能力/负载）；失败八形态与重试矩阵（含 `superseded` / `disk_exhausted`）；**执行中改需求的三种处置**；看板六列 + 任务详情页；**看板过滤 / 搜索 / 批量操作**；**项目摘要四行（含管家总结落点）**；**失败面板与验收面板（唯一处置入口）**；**执行复盘（只读轨迹回放）**；**执行监控（`projects/:id/executions`，慢变摘要 + 按行订阅，不编百分比）**；**收件箱（待决策 / 动态两区 + 决策请求 TTL/降级/留痕）**；`disk_exhausted` 时**询问清理并说明清理什么（预演）**；**时间与成本统计（排队与执行分开记，token 拿不到标 unknown）**；执行间接力（缓存复用）；时间线作为第二视图。见 `architecture/task-model.md` | 任务看板、任务与执行、项目时间线、执行监控、执行复盘、收件箱、失败与验收 |
| v0.9.0 | **管家** | 跑在 Hub 上的项目级 agent：接意图 / 建派任务 / 督进度 / 写总结；**技能集（`project.status` / `task.create` / `task.clarify` / `memory.*`），不写死意图分类**（D22）；两处记忆（库 `project_memory` + 仓库 `.afleet/MEMORY.md`）；**建议派活草稿箱（点下发才进看板）**；与调度器的接口（只提交 task，不指定设备）；决策请求与收件箱的接入。调度器侧补三层过滤（`architecture/scheduling.md` 按此改写）。见 `architecture/butler.md` | 管家 |
| v0.10.0 | **外部 issue 单向导入** | 导入生成 Task + Hub 内收件箱通知 + 管家初筛 + 结论单向往回写。**不做双向同步**。见 `PRODUCT.md` §5.5 | 外部 issue 单向导入 |
| v0.11.0 | **定时任务（自动化）** | 定时与事件触发的调度：cron 式表达 + 「某任务完成后触发」两类来源，落到既有 Task（生成新 Task 而不是绕过六态）。**必须复用 v0.8.0 的任务模型**——定时任务不是另一套调度，触发只是「谁在什么时候创建了 Task」。要点：触发器本身是项目级配置；错过窗口（Hub 停机）后的补偿策略要显式定（跳过 / 补跑，不默认静默跳过）；触发产生的 Task 带 `created_by=schedule` 以便在看板与失败归因里区分。**⚠️ 原「推理网关」已降级为 TODO**（见 `PRODUCT.md` §6.2.1），本号改由定时任务占用 | —（`PRODUCT.md` §6.2 后续项） |
| v0.12.0 | connector 插件化 + 技能 / MCP | 把协议族（SSH / DB / Redis / S3 / SFTP / TCP 透传）做成插件按需加载（**实现参考 REX**）+ 抓包插件 + 端口转发插件；**同时提供「任务级授予的技能 / MCP」通路**（凭据不进 agent 上下文、每次连接进审计）。**不引入 Resource 抽象**（`DECISIONS.md` D21） | —（`PRODUCT.md` §6.2 后续项）、技能 / MCP |
| v0.13.0 | 任务连贯性（跨节点续跑） | 检查点 + 断点续跑 + 跨设备重建工作区（见 `architecture/task-continuity.md`）；此时 Task/Execution 已存在，只补「续跑」这一层 | —（`PRODUCT.md` §6.2 后续项） |
| ~~v0.14.0~~ | ~~CI 镜像构建（发布通路）~~ | **整行迁至 v0.7.1**（用户 2026-11-05 裁定提前）。原文里「必须等 v0.7.0–v0.13.0 对外契约定稿后再发镜像」这一条前提被推翻：用户明确要求现在就打通发布通路并推 tag 触发 CI。**代价如实记录**：v0.7.1 发的镜像是 0.7.1 版本的接口，v0.8.0–v0.13.0 仍会改公开 API，届时需要再发新版本并让自托管用户升级 | —（已合并进 v0.7.1） |
| 后续 | 后续能力 | 社交软件接入、原生客户端、分布式推理进阶；**暂缓项**（统一配置 agent / 模型配置一处切换） | —（`PRODUCT.md` §6.2 / §6.3） |

> **2026-11-01 里程碑重排的裁定（产品主轴改版）**
>
> 产品主轴从「设备 / agent」改为「项目」（`PRODUCT.md` §1/§3，`DECISIONS.md` D10），
> 因此里程碑顺序**整体重排**，旧版本号引用（v0.7.0 推理网关、v0.8.0 插件化、v0.9.0 任务系统）
> **全部失效**，一律以本表为准。重排的依据：
>
> - **项目要先于任务**：Task 属于某个 Project，Task 的 `base_commit` 来自 Project 的仓库，
>   Execution 的 worktree 来自 Binding 的固定目录。没有 v0.7.0 就没有 v0.8.0 的落脚点。
> - **看板要早**：看板是默认视图（用户明示「看板优先」）。没有看板的项目页只是仓库浏览器。
> - **管家要在任务之后**：管家的四项能力全部是「对 Task 做的事」，Task 没有稳定的六态之前
>   写不出能用的管家。
> - **issue 导入在管家之后**：导入的核心价值是「管家替你看第一遍」（`PRODUCT.md` §5.4），
>   没有管家的导入只是一个列表。
> - **推理网关与插件化后移**：它们与项目主轴正交，不阻塞主线；挂在主线中间只会拉长第一个可用版本。
>
> **「第一版」的边界**：原为 v0.1.0–v0.5.0（后顺延到 v0.6.0）。产品主轴改版后，
> **第一个可用版本 = v0.1.0–v0.8.0（会话 + 项目 + 任务看板）**——
> 「项目能派活、能看板、能验收」就是可用；管家是让它好用的下一层，不阻塞「可用」。

> 首个演示是 v0.2.0：在树莓派上开一个**会话**（页面级容器，不是 ACP agent 会话——ACP 归 v0.3.0），会话里的 shell 与文件浏览就能用，且零入站端口。
> 终端 / 文件 / git 都在会话内，因此演示必须经由会话，不能脱离会话直接开终端。
>
> **编号说明（2026-10-29）**：本里程碑表原用 `M0`–`M8` 编号，现改为版本号
> （`M0`→`v0.1.0`、`M1`→`v0.2.0`、`M2`→`v0.3.0`、`M3`→`v0.4.0`，此后依次顺延）。
> 下面几段**历史裁定**描述的是**当时的排期**，不是今天的排期；其中指向「当时叫 Mn 的那个里程碑」
> 一律保留 `Mn` 的引述写法而不换算成版本号——`M3` 当时是 ACP，今天的 `v0.4.0` 是 Git，
> 换算只会让读者以为它在说今天那个。

> **2026-10-26 范围裁定**：原「工作区供料与同步」里程碑整体撤销（**注：该撤销已于 2026-11-01 被
> `project-and-snapshot.md` 承接回来——「供料」这件事现在由 Project + Binding + git 承担，
> 不再叫「供料」；下面的描述是当时的排期记录，保留原文**）。当时产品只做每台设备内自己的对话，
> 不做任务分发，因此没有项目文件传输的需求（原 CAS / manifest 差分 / A-B-C-D 四档规格留在
> `architecture/workspace-and-sync.md` 作决策记录）。原 M3 的 ACP 对话与轨迹**整个提前**为现在的 v0.3.0；
> 原 M5 的「Hub 一键把 agent 装到设备」也并入 v0.3.0。
>
> **原 M2 与 Git 的先后顺序已不适用**：那条理由（「工作区必须先能被喂进去」）随传输面一起撤销。
> v0.4.0（Git 界面）现在只需 v0.2.0 的工作区绑定即可开工。

> **v0.4.0 的范围（2026-10-29 规划）**：**子任务预估 6 个**（git connector 的设备侧实现 / git 帧协议与注册表接线 /
Hub 侧透传与能力校验 / 会话页第三个面板的注册与布局 / 状态与 diff 视图 / log 视图与真机端到端）。
**依赖 v0.2.0**（工作区绑定、面板坞、`files` 面板的既有形态全部复用）。
**版本类型：minor**（新增能力，向后兼容）。**缺陷池 bug**：0 个。
> 只读是**范围决定**而非实现取舍：写操作（commit / checkout / stage）要连带决定提交者身份来源、
> 钩子执行与失败回滚，与 v0.4.0 的目标「看它改了什么」无关，因此不在本里程碑内。

> **v0.5.0 的范围（2026-10-29 规划）**：**子任务预估 6 个**（外壳收起全局侧栏 / 顶部重排 /
右侧改为一格＋「＋」互斥选择 / 左右一条分栏的拖拽与持久化 / 窄屏降级 / 键位与文档同步 + 真机端到端）。
**依赖 v0.4.0**（git 面板、面板坞、`v0.4.0` 锁定的「进会话即 git、＋开终端/文件」三条全部复用并重排）。
**版本类型：minor**（新增布局能力；**纯前端**，不改 connector / 帧协议 / 设备侧代码）。**缺陷池 bug**：0 个。
> 单开一个里程碑而不并进 v0.4.0 的理由：重排会同时改掉 v0.4.0 刚锁定的入口形态（进会话即展开坞、
缺省 git、「＋」开终端），并进去会让它的端到端断言与验收结论一起失效——那是在给正在审查的里程碑换地基。
> 这个里程碑**插在**原 v0.5.0 之前，后续里程碑顺延一位（原 v0.5.0 → v0.6.0，原 v0.6.0 → v0.7.0，
> 原 v0.7.0 → v0.8.0，原 v0.8.0 → v0.9.0）。**「第一版」的边界从 v0.5.0 顺延到 v0.6.0**：
> 首个可用版本要能舒服地用，而「三块同时在场」正是「舒服地用」的一部分。

> 第一版 = v0.1.0–v0.6.0（首个可用版本）。v0.7.0 起为第二阶段。
> The first usable version is v0.1.0–v0.6.0; v0.7.0 onward is the next stage.

---

## 4. 范围映射（两份文档的唯一对齐依据）

`PRODUCT.md` §6.1 定范围，本节定排期，**中间的这张表是唯一真相源**。
里程碑表最后一列「承接 §6.1 能力」即由此得出。

### 4.1 对齐契约（每次改动必查）

改动范围或排期时，**四件事必须同时成立**：

1. **每个 §6.1 能力都能找到承接它的里程碑**（下表「承接」列无遗漏）
2. **每个里程碑都有对应架构依据**，或明确标注「新增能力 / 无对应设计」
3. **第一版口径三处一致**：PRODUCT §6.2 的标注、DEVELOPMENT §3 的标注、README 的标注
4. **不产生「承诺了但没排期」或「排了期但没承诺」的项**
5. **每个里程碑必须有存储/进程入口基座**——子任务里若出现写 `device` 表、
   用 `secret` 表、起 `main.rs`，就必须有一个前置子任务产出 schema + 迁移 + 进程入口。
   v0.1.0 第 1 轮设计审查即漏此条（9 个子任务无人建表）。

> 这份文档已三次出现漂移（Xshell 对标基调、「Hub 一键配置 agent」推后与否、
> git 面板在哪个里程碑），全部源于两份文档各写各的、靠人工对齐。
> **步骤 2 设计核对时把本节当固定检查项，逐条走，不要凭印象判断。**

### 4.2 逐条承接

| PRODUCT §6.1 能力 | 承接里程碑 | 架构依据 |
|------------------|-----------|---------|
| 设备接入 | v0.1.0 ✅ | `security-and-lifecycle.md` §4（supervisor/worker、身份） |
| 节点身份与状态 | v0.1.0 ✅ | `security-and-lifecycle.md` §4.0/§5；`scheduling.md` §2 |
| 存储与启动基座 | v0.1.0 ✅（新增，非 `PRODUCT.md` §6.1 能力） | `architecture/README.md` §3/§8；`security-and-lifecycle.md` §2.1 |
| Bash / PTY | v0.2.0 ✅ | `architecture/README.md` §4.1（工作区绑定）；`design-system.md` §7.0 |
| 文件浏览 | v0.2.0 ✅ | 同上 |
| AI agent 管理与对话 | v0.3.0 ✅ | `session-and-trajectory.md`（ACP 会话） |
| 对话与动作流 | v0.3.0 ✅ | `session-and-trajectory.md` §3/§7（轨迹表） |
| Git 操作界面 | v0.4.0 ✅（只读 status/diff/log）+ v0.6.0 ✅（`rev` 起点） | `architecture/README.md` §4；面板绑会话工作区，**工作区不是路由** |
| 会话界面形态 | v0.5.0 ✅ | **新增能力，无对应架构章节**：形态对齐 opencode。`design-system.md` §5.4 / §7.1 是视觉落点 |
| **项目实体** | **v0.7.0** | `project-and-snapshot.md` §1/§2（三形态）、§3（为什么用真 git） |
| **绑定与固定目录** | **v0.7.0** | `project-and-snapshot.md` §5（固定目录 + worktree + 四层磁盘处置） |
| **设备管理（管理面，保留并扩展）** | **v0.7.0** | `PRODUCT.md` §3（设备是「管理」的对象）；`scheduling.md` §2（心跳与能力字段）；`project-and-snapshot.md` §5.3（配额与清理策略） |
| **agent 管理（管理面，保留并扩展）** | **v0.7.0** | `PRODUCT.md` §3；`session-and-trajectory.md`（agent 探测与启动配置）；**Hub 不代装** |
| **任务看板** | **v0.8.0** | `task-model.md` §2/§6（六态、两视图投影） |
| **任务与执行** | **v0.8.0** | `task-model.md` §1/§3/§4（1:N、失败八形态、约束与过滤顺序） |
| **管家** | **v0.9.0** | `butler.md` §2/§3/§4（四项能力、两处记忆、调度接口） |
| **项目时间线** | **v0.8.0** | `task-model.md` §6（同一批事实的两种投影） |
| **执行监控** | **v0.8.0** | `PRODUCT.md` §3.7（三层信息、按行订阅、不编百分比）；`task-model.md` §2 |
| **收件箱** | **v0.8.0** | `PRODUCT.md` §5.4；`butler.md` §2.2.2（决策请求形态、TTL、降级、留痕） |
| **连接分层** | **v0.12.0**（随技能 / MCP 的真实批量负载落地） | `protocol.md` §0.1（按用途分连接 + 三条可测断言）；`DECISIONS.md` D18。**2026-11-04 从 v0.7.0 后移**：三条性质（主动外联 / Hub 不拨入 / 可鉴权可审计）现有 WS 已满足，而「按用途分三类连接」是为**批量传输**服务的——v0.7.0 的同步量（worktree 指针 + 元数据）撑不起拆分，现在拆是提前优化；v0.12.0 的技能 / MCP 才有真批量（大文件、S3、DB 导出） |
| **技能 / MCP** | **v0.12.0** | `PRODUCT.md` §3.6；`DECISIONS.md` D21（凭据不进上下文、审计、不开放任意 connector） |
| **项目摘要** | **v0.8.0** | `PRODUCT.md` §3.7（四行摘要，管家总结的 UI 落点） |
| **看板过滤 / 搜索 / 批量** | **v0.8.0** | `PRODUCT.md` §3.3.1（批量重试仍逐张检查约束） |
| **时间与成本** | **v0.8.0** | `PRODUCT.md` §3.8（排队与执行分开记，token 拿不到标 unknown 不估算） |
| **失败与验收面板** | **v0.8.0** | `task-model.md` §4/§5（唯一处置入口）；`PRODUCT.md` §3.9 |
| **执行复盘** | **v0.8.0** | `PRODUCT.md` §3.11（只读，已有轨迹表的新投影）；`session-and-trajectory.md` |
| **执行间接力** | **v0.8.0** | `PRODUCT.md` §3.12；`task-model.md` §8（`parent_execution_id`） |
| **管家建议派活草稿箱** | **v0.9.0** | `butler.md` §2.2.2（草稿不进看板，信任模式默认关） |
| **磁盘清理预演** | **v0.8.0** | `PRODUCT.md` §5.4；`task-model.md` §3.1 |
| **绑定管理（双向可改）** | **v0.7.0** | `PRODUCT.md` §3.2（主题色、归档）；`DECISIONS.md` D24/D25 |
| **外部 issue 单向导入** | **v0.10.0** | `PRODUCT.md` §5.5；**无独立架构文档**，规格在 PRODUCT 内 |
| **任务模板** [TODO] | **未排期** | `PRODUCT.md` §3.13（项目内任务约束快照） |
| **全局任务流** [TODO] | **未排期** | `PRODUCT.md` §3.3.2（第一版不做） |
| **跨执行对比** [TODO] | **未排期** | `task-model.md` §8（Task:Execution 1:N 已留现场） |
| 统一配置 agent | **未排期（暂缓）** | `workspace-and-sync.md` §4.1–4.3（托管覆盖层 + 两层权限） |
| 模型配置一处切换 | **未排期（暂缓）** | `workspace-and-sync.md` §4.3；`security-and-lifecycle.md` §3 |
| ~~Hub 一键安装 agent~~ **已撤销（2026-10-26）** | —（不做代装） | agent 由用户自己安装，Hub 只扫描 + 存启动配置 |
| ~~工作区供料~~ **已于 2026-11-01 由「项目」承接** | v0.7.0（并入项目实体） | 供料这件事现在由 Project + Binding + git 承担，`workspace-and-sync.md` §3 保留原文作决策记录 |
| ~~产物归档~~ **已于 2026-11-01 由「任务分支」承接** | v0.8.0（并入任务与执行） | 「东西从设备回 Hub」由 `push` 任务分支承担，`workspace-and-sync.md` §5 保留原文作决策记录 |

> **2026-11-01 改版影响**：`PRODUCT.md` §3.1 已改为 §6.1（章节重排）。本表随之更新两列：
> 左列全部指向 §6.1，并新登记 v0.7.0–v0.10.0 的五项能力。
> 原「工作区供料」「产物归档」两条撤销项**由新能力承接**，不再是无主之地。

### 4.3 反向检查

| 来源 | 项 | 承接 |
|------|---|---|
| `PRODUCT.md` §6.2 后续 | connector 插件化 | v0.12.0 |
| `PRODUCT.md` §6.2 后续 | 任务连贯性（跨节点续跑） | v0.13.0 |
| `PRODUCT.md` §6.2 后续 | 分布式推理进阶 | **TODO，暂时不考虑**（用户 2026-11-03 裁定；本地推理同理） |
| `PRODUCT.md` §6.2 后续 | 定时任务（自动化） | **v0.11.0**（复用 v0.8.0 的任务模型，触发器只负责创建 Task） |
| `PRODUCT.md` §6.2 后续 | 社交软件 / 原生客户端 | **无排期**（开工前单独排期） |
| `PRODUCT.md` §6.3 暂缓 | 统一配置 agent / 模型配置一处切换 | **无排期**（规格在，实现未排） |
| `AGENTS.md` 约束 10 行内 | **afcli 自身的升级通路**（页面显示版本号 → 手动触发升级 → 自动升级开关） | **未排期**。v0.2.0 只交付了前半段：`/api/devices` 给出版本号（`node_version`） |

> 「后续」不是编号里程碑，需在开工前单独排期与编号。
> **2026-11-01 起：第二阶段从 v0.11.0（定时任务）开始**；v0.7.0–v0.10.0 是项目主轴的落地段。
> **2026-11-03 更新：v0.11.0 改由「定时任务（自动化）」占用**（用户确认要做）。
> 原「推理网关」降级为 TODO、未排期——本地推理与分布式推理都暂时不考虑（`PRODUCT.md` §6.2.1 / §7.2）。
> **v0.12.0 / v0.13.0 的编号保持不变**，从 v0.10.0 之后接 v0.11.0（定时任务）→ v0.12.0 → v0.13.0。
> 定时任务的既定前提：**它复用 v0.8.0 的任务模型，触发器只负责创建 Task**，不引入第二套调度。
> 「agent 安装」已于 2026-10-26 撤销，不属于任何阶段。
> "Later" is not a numbered milestone; schedule and number it separately before starting.

---

## 5. 架构文档索引

- `architecture/README.md` — 进程模型 / 连接与通道 / 数据模型 / connector / 会话
- **`architecture/project-and-snapshot.md`** — **产品主轴**：Project 三形态 / Binding 固定目录 / worktree / Snapshot 与「同一起点」/ 磁盘四层处置
- **`architecture/butler.md`** — 管家：四项能力 / 两处记忆 / 与调度器的接口
- **`architecture/task-model.md`** — 任务：Task:Execution = 1:N / 六态 / 失败八形态与重试矩阵 / 任务级约束与过滤顺序
- `architecture/session-and-trajectory.md` — ACP 会话恢复 + dsh 式轨迹 + 改动文件/diff
- `architecture/task-continuity.md` — 跨设备任务连贯性模型（检查点续跑）
- `architecture/workspace-and-sync.md` — 托管配置与同步（**项目文件供料 / 产物归档两节已由 project-and-snapshot 承接**，原文保留作决策记录）
- `architecture/scheduling.md` — 任务路由：心跳上报字段、硬过滤、有序打分、任务状态机（**§4.1 的三层过滤顺序以 `task-model.md` 为准**）
- `architecture/security-and-lifecycle.md` — 凭据模型、supervisor/worker、设备升级与掉线重连
- `architecture/protocol.md` — WS 帧格式、enroll/challenge 握手、通道复用与流控/信用、重连序列号
- `architecture/ai-gateway.md` — 推理网关需求（权威来源）
- `architecture/inference.md` — 分布式推理完整评估与分阶段路径（**调研存档，非主线**）

---

## 5. 开发流程 / Workflow

本项目使用 **mdflow** 里程碑流程：写文档 → 设计核对 → 开发 → 精简 → 审查 → 测试 → 验收 → 提交。
项目约定见 `AGENTS.md`；里程碑文档落在 `.mdflow/milestones/`（本地产物，不入库）。