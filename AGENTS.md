# AGENTS.md

## 项目定位

AgentFleet 是**个人自托管的设备 / Agent 平台**：把自己拥有的设备（服务器、PC、手机、树莓派）
注册为节点，在一个 Web 页面里统一管理，在节点上运行 AI agent 会话，并按设备能力把任务分发到最合适的节点执行。

第一版**只做 self-host，单用户**。这是范围选择，不是能力妥协。
不要引入多用户、RBAC、团队协作、SaaS 计费等概念。

- 产品文档：`docs/PRODUCT.md`
- 开发文档：`docs/DEVELOPMENT.md`（索引）
- 架构文档：`docs/architecture/`
- 缺陷池：`docs/BUGS.md`
- 里程碑文档：`.mdflow/milestones/`（本地产物，不入库）

新增功能前先确认产品文档中的功能边界，再把实现细节写入里程碑文档。

---

## 硬性约束

1. **节点主动外联，Hub 不需要任何到设备的入站访问。** 设备装一次、跑一条命令即接入，
   Hub 无入站端口是产品承诺，不是部署偏好。
2. **`agent` 一词只指 AI agent。** 绝不用来指设备进程。设备侧进程一律叫
   `node` / 节点进程 / connector。
3. **connector 是有类型的、可插件化的能力。** 第一版内置 terminal / files / git / exec；
   后续 SSH / DB / S3 等协议作为**插件**按需加载，额外有抓包插件与端口转发插件。
   **不引入多态 Resource 抽象**（加一个协议要改 N 处即违规）。
   **connector 只面向 Web 页面用户，不向 agent 暴露**——本地 agent 完全依赖设备上安装的
   agent 自带能力与它自己的权限系统。
   **terminal / files / git 绑定会话的工作区**，生命周期随会话（`node_id, path`），
   不做设备级的长驻多资源管理台。
4. **每设备一条 WebSocket**，承载控制 + 数据通道。通道键为 `(node_id, channel_id)`，
   不是全局 channel_id；每通道独立流控/信用。
5. **认证用设备密钥对**（注册时生成，Hub 记指纹），不用静态共享 token。
6. **会话层用 ACP**（Agent Client Protocol），不自建 agent 会话协议；
   轨迹（trajectory）用 dsh 式的 **append-only 事件流**，Web 端可逐条查看对话与动作。
7. **推理基于成熟开源项目**（llama.cpp / mesh-llm / prima.cpp 等），不自研推理内核，
   也不自研无法审核的分布式推理代码。Rust 只做编排与网关。
8. **子任务分发借鉴 oh-my-opencode 的子 agent 体系**：可配置、开箱即用的子 agent，
   主 agent 把单个完整任务分发给子 agent 在设备上执行。
9. **任务分发按设备能力/性能自动调度**：路由时读取节点的心跳能力上报（CPU/内存/GPU/模型/负载）。
    **能力用于硬过滤（排除），负载用于有序打分（排序）**——不做加权综合评分
    （权重靠拍脑袋且不可解释）。**任务路由与推理路由是两套独立决策**，共享的只有节点能力数据。
    详见 `docs/architecture/scheduling.md`。
10. **不同步文件夹，同步内容。** Hub 是唯一真相源，用一个内容寻址仓库供三样东西：
    项目文件（物化到 `(node_id, path)`）、技能与 agent 配置（托管覆盖层）、产物归档。
    大文件不进 WebSocket（git 项目只传 commit 引用，节点自己去 origin 拉）；
    **不引入设备间 P2P 直传，不用 Syncthing 当主同步通道，不同步 `node_modules` 等依赖**。
    节点密钥**永不上传**。详见 `docs/architecture/workspace-and-sync.md`。
11. **设备密钥对在 node 本地生成，Hub 只存指纹。** 若 Hub 生成设备私钥，
    则 Hub 等于持有全部设备私钥，密钥对的意义归零。
12. **node 分 supervisor / worker 两层，身份由 supervisor 持有。**
    supervisor 持设备私钥、维持 WebSocket、拉起与监控 worker；
    **升级只换 worker，身份不变**，因此不需要任何身份抢占逻辑。
13. **connector 凭据与模型 API key 由 Hub 明文存储，不做应用层加密**
    （单用户自托管的取舍）。**但明文 ≠ 可以放松其他防护**：
    SQLite 0600 / 数据目录 0700 / `umask 077`；secret 值**永不进日志、永不回显、
    永不进轨迹**；前端只拿 `fingerprint`（前 3 + `***` + 后 3）。
    详见 `docs/architecture/security-and-lifecycle.md`。

---

## 技术栈

| 层 | 选型 | 理由 |
|---|---|---|
| Hub + 节点进程 | **Rust**（单二进制） | 节点要跑在树莓派 / 小内存机器上，无运行时、交叉编译 amd64+arm64 是硬需求 |
| Web | **Vue 3 + Bun** + Vite | 已定；Bun 跑构建与测试，Vue 3 生态成熟，xterm.js / diff 视图等组件现成 |
| 存储 | SQLite | 建库**必须**设 `journal_mode=WAL` 与 `busy_timeout` |
| 会话层 | ACP（Agent Client Protocol） | 不自建 agent 会话协议 |
| 轨迹 | append-only 事件流（dsh 式 Trajectory） | Web 逐条可查对话与动作 |
| 推理 | llama.cpp / mesh-llm / prima.cpp 等现成开源 | 不自研内核 |

### SQLite 铁律

建库时必须 `PRAGMA journal_mode=WAL` + `PRAGMA busy_timeout`。
多设备高频心跳/状态写入下，零 PRAGMA 必现 `SQLITE_BUSY`。

---

## 设计基调（UIUX）

- **深色优先、等宽字体、高信息密度、键盘优先**，贯穿全站的一致交互范式。
- **终端 / 文件 / git 是会话内的面板，不是设备级的管理面。** 它们绑定在某个会话的
  工作区上（设备上的一个 `(node_id, path)`），进入会话才建立、退出即释放。
  不做设备级的多资源管理台。**易用即可，不刻意对标 Xshell / Xftp。**
- **对话与动作流对标 dsh**：每个会话有 Trajectory 视图——逐条展示 User / Assistant / Tool /
  子 agent / 压缩记录，点开看 token、耗时、输入/输出、附件；
  会话结束后列出**本次改动的所有文件**，点开直接看 diff（借鉴 dsh 的改动视图）。
- 易用优先：设备接入一条命令、agent 即开即聊、任务进度一眼可见。

---

## 代码审查维度（步骤5 每轮必读）

在契约「精简必查清单」之上，本项目追加：

0. **`PRODUCT.md` §3.1 的每项能力是否都有里程碑承接**（反之亦然）——
   走 `DEVELOPMENT.md` §4 的范围映射表逐条核，**不凭印象**。
   本项目已三次因两份文档各写各的而漂移（Xshell 对标基调、「Hub 一键配置 agent」推后与否、
   git 面板所在里程碑），此为固定检查项。

1. `agent` / `environment` / `resource` 是否被误用于指设备或泛指资源
2. 是否出现全局跨设备的 channel_id
3. 是否引入需要 Hub 入站访问设备的设计
4. 是否出现静态共享 token
5. 是否新增多态资源分支（加一个协议要改 N 处即违规）
6. SQLite 是否漏设 WAL / busy_timeout
7. 轨迹事件是否丢失「可回放」所需的字段（类型 / 输入 / 输出 / 耗时 / 归属节点）
8. 是否绕开内容寻址仓库做了「整目录同步 / 设备间 P2P 直传」，或把依赖目录、密钥传上了行
9. secret 值是否出现在日志 / API 响应 / 轨迹事件里（哪怕「只是本地自用」）
10. 是否出现了让 **Hub 持有设备私钥** 的路径，或让升级导致 `node_id` 变化的机制

---

## 协作约定

- **Commit message 一律英文**，遵循 conventional commits（`feat:` / `fix:` / `docs:` / `chore:` / `refactor:`）。
- **README 与 CHANGELOG 中英双语**：每条内容同时给出中文与英文。其余文档（AGENTS / PRODUCT / DEVELOPMENT / 架构文档）以中文为主。
- **前端命令一律用 `bun`**，禁止 `npm run`。

---

## 质量门禁

Rust：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Web（`packages/web`，Vue 3 + Bun + Vite）：

```bash
bun install
bun run type-check     # vue-tsc --noEmit
bun run lint           # eslint
bun run test           # vitest
bun run build          # vite build
```

测试与覆盖率项若确实无法本地自证，如实记录「不可执行 + 根因」，
不得记为通过；也不要把「环境因素导致失败」写成「环境限制」——
磁盘、工具链这类**可修复因素**应先排除再下结论。

本项目暂不设**阻塞性**覆盖率门禁；`cargo llvm-cov --workspace` 作为报告项。

---

## 仓库结构

```text
docs/
  PRODUCT.md              产品功能边界、架构决策、用户可见流程
  DEVELOPMENT.md          开发索引（技术栈、crate 结构、里程碑总览）
  BUGS.md                 缺陷池（待处理 bug 的汇总地）
  architecture/           架构文档（连接模型、会话与轨迹、推理、插件、任务连贯性、工作区供料）
crates/                   Rust workspace（hub / node / protocol）
packages/web/             Vue 3 + Bun + Vite 前端
CHANGELOG.md               Keep a Changelog（中英双语）
.mdflow/                   dev-flow 产出物（本地产物，不入库）
  milestones/
    {version}-{name}.md    里程碑开发文档（完成后保留）
    {version}-reports/     里程碑报告（步骤 2/4/5/6/7）
README.md                 产品简介（中英双语）
AGENTS.md                 本文件
```
