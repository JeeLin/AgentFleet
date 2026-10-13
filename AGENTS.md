# AGENTS.md

## 项目定位

AgentFleet 是**个人自托管的设备 / Agent 平台**：把自己拥有的设备（服务器、PC、手机、树莓派）
注册为节点，在一个 Web 页面里统一管理，并在指定节点上运行 AI agent 会话。

第一版**只做 self-host，单用户**。这是范围选择，不是能力妥协。
不要引入多用户、RBAC、团队协作、SaaS 计费等概念。

- 产品文档：`docs/PRODUCT.md`
- 开发文档：`docs/DEVELOPMENT.md`（索引）
- 缺陷池：`docs/BUGS.md`
- 里程碑文档：`.mdflow/milestones/`（本地产物，不入库）

新增功能前先确认产品文档中的功能边界，再把实现细节写入里程碑文档。

---

## 硬性约束

1. **节点主动外联，Hub 不需要任何到设备的入站访问。** 设备装一次、跑一条命令即接入，
   Hub 无入站端口是产品承诺，不是部署偏好。
2. **`agent` 一词只指 AI agent。** 绝不用来指设备进程。设备侧进程一律叫
   `node` / 节点进程 / connector。REX 的教训：进程叫 `rex-agent`、表叫 `agents`，
   AI agent 出现后内部命名直接冲突，本仓库从第一版起就钉死。
3. **没有 `environment` 层级。** 可达性只是设备当前网络的观测属性（标签），不是实体。
4. **没有多态 Resource 抽象。** 设备能力是独立的、有类型的 connector
   （terminal / files / exec），各自独立演进。
5. **通道键为 `(node_id, channel_id)`**，不是全局 channel_id；每通道独立流控/信用。
6. **认证用设备密钥对**（注册时生成，Hub 记指纹），不用静态共享 bearer token。
7. 依赖声明集中在根 `Cargo.toml` 的 `[workspace.dependencies]`，子 crate 用 `workspace = true`。
8. **不要用 Rust 写 LLM 推理内核**——没有 Rust 栈支撑分布式推理。
   推理交给 llama.cpp 等成熟实现，Rust 只做编排与网关。

---

## 技术栈

| 层 | 选型 | 理由 |
|---|---|---|
| Hub + 节点进程 | **Rust**（单二进制） | 节点要跑在树莓派 / 小内存机器上，无运行时、交叉编译 amd64+arm64 是硬需求 |
| Web | 待定（倾向 Vue 3 + Bun） | 独立选择，不构成阻塞 |
| 存储 | SQLite | 建库**必须**设 `journal_mode=WAL` 与 `busy_timeout` |
| 会话层 | ACP（Agent Client Protocol） | 不自建 agent 会话协议 |
| 推理 | llama.cpp 等既有开源实现 | 见下节 |

### SQLite 铁律

建库时必须 `PRAGMA journal_mode=WAL` + `PRAGMA busy_timeout`。
REX 实测：零 PRAGMA、默认回滚日志 + 每次心跳都写 → 多设备重连必 `SQLITE_BUSY`。

---

## 设计对标

交互布局对标成熟专业工具，复刻其**操作逻辑**而非外观：

- 终端 / 文件管理 → **Xshell** / **Xftp**
- 对话与动作流 → 现代 agent 客户端（逐条可查动作，不是只给一句摘要）
- 管理页面（设备 / agent / 设置） → 自有设计系统

**深色优先、等宽字体、高信息密度、键盘优先、一致的交互范式贯穿全站。**

---

## 代码审查维度（步骤5 每轮必读）

在契约「精简必查清单」之上，本项目追加：

1. `agent` / `environment` / `resource` 是否被误用于指设备或泛指资源
2. 是否出现全局跨设备的 channel_id
3. 是否引入需要 Hub 入站访问设备的设计
4. 是否出现静态共享 token
5. 是否新增多态资源分支（加一个协议要改 N 处即违规）
6. SQLite 是否漏设 WAL / busy_timeout

---

## 质量门禁

Rust：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Web（选定前端后补齐）：

```bash
bun run type-check
bun run lint
bun run build
```

**前端命令一律用 `bun`，禁止 `npm run`。**

测试与覆盖率项若确实无法本地自证，如实记录「不可执行 + 根因」，
不得记为通过；也不要把「环境因素导致失败」写成「环境限制」——
磁盘、工具链这类**可修复因素**应先排除再下结论。

本项目暂不设**阻塞性**覆盖率门禁；`cargo llvm-cov --workspace` 作为报告项。
`cargo llvm-cov` 未安装时本地不测覆盖率，如实记「未测量」。

---

## 仓库结构

```text
docs/
  PRODUCT.md              产品功能边界、架构决策、用户可见流程
  DEVELOPMENT.md          开发索引（技术栈、crate 结构、里程碑总览）
  BUGS.md                 缺陷池（待处理 bug 的汇总地）
  architecture/           架构文档（连接模型、会话、推理网关）
  reference/              参考文档（数据模型、协议、前端工程）
CHANGELOG.md               Keep a Changelog
.mdflow/                   dev-flow 产出物（本地产物，不入库）
  milestones/
    {version}-{name}.md    里程碑开发文档（完成后保留）
    {version}-reports/     里程碑报告（步骤 2/4/5/6/7）
README.md                 产品简介
AGENTS.md                 本文件
```
