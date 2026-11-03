# AGENTS.md

## 项目定位

AgentFleet 是**个人自托管的设备 / Agent 平台**：把自己拥有的设备（服务器、PC、手机、树莓派）
注册为节点，在一个 Web 页面里统一管理，在节点上运行 AI agent 会话，并按设备能力把任务分发到最合适的节点执行。

第一版**只做 self-host，单用户**。这是范围选择，不是能力妥协。
不要引入多用户、RBAC、团队协作、SaaS 计费等概念。

- 产品文档：`docs/PRODUCT.md`
- 开发文档：`docs/DEVELOPMENT.md`（索引）
- 决策记录：`docs/DECISIONS.md`（架构决策的**理由与否决的替代方案**）
- 架构文档：`docs/architecture/`
- 缺陷池：`docs/BUGS.md`
- 里程碑文档：`.mdflow/milestones/`（本地产物，不入库）

新增功能前先确认产品文档中的功能边界，再把实现细节写入里程碑文档。

---

## 硬性约束

1. **节点主动外联，Hub 不需要任何到设备的入站访问。** 设备装一次、跑一条命令即接入，
   Hub 无入站端口是产品承诺，不是部署偏好。
2. **`agent` 一词只指 AI agent。** 绝不用来指设备进程。设备侧进程一律叫
   `node` / 节点进程 / connector。**设备二进制的命令名固定为 `afcli`**
   （不叫 `node`——会与 Node.js 运行时撞名；**crate 名用 `node-*`，二进制名用 `afcli`**）。
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
8. **任务分发在 Hub 层完成，不编排 agent 运行时的父子关系**：
   Hub 新建一个任务 → 按能力/负载路由到某台 node → 在该 node 上由一个 agent 会话执行。
   借鉴 oh-my-opencode 的是「**预置一套可配置专家 agent 定义、节点只读生效**」，
   **不是**由我们决定 agent 何时 spawn 子 agent。agent 运行时（opencode 等）的父子 agent
   由其自身决定，Hub 只通过轨迹观测「它开了子会话」，**不介入编排**。
9. **任务分发按设备能力/性能自动调度**：路由时读取节点的心跳能力上报（CPU/内存/GPU/模型/负载）。
    **能力用于硬过滤（排除），负载用于有序打分（排序）**——不做加权综合评分
    （权重靠拍脑袋且不可解释）。**任务路由与推理路由是两套独立决策**，共享的只有节点能力数据。
    详见 `docs/architecture/scheduling.md`。
10. **不同步文件夹，同步内容。** **项目文件不在设备间传输**：产品只做**每台设备内自己的对话**，
    不做任务分发，因此没有「把项目送到另一台机器」的需求（2026-10-26 裁定，
    原「内容寻址仓库供三样东西：项目文件 / 技能与配置 / 产物归档」整体撤销，
    规格留在 `docs/architecture/workspace-and-sync.md` 作决策记录）。
    **仍然有效的是**：Hub 是唯一真相源，**不引入设备间 P2P 直传，不用 Syncthing 当主同步通道**，
    **不同步 `node_modules` 等依赖目录**，**节点密钥永不上传**。
    **内容寻址存储（CAS）现在一处都不剩**：agent 安装已于 2026-10-26 撤销（Hub 不再把
    agent 安装包送给设备），`blob` 表随之从 schema 里去掉——`schema_v3.sql` 建的五张表
    是 `workspace_v3` / `session_v3` / `agent_runtime` / `agent_config` / `trajectory_event`。
    决策记录留在 `docs/architecture/workspace-and-sync.md` §4.4 / §7。
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
14. **协议功能由能力协商决定，不靠版本号猜**（版本 ≠ 功能）：
    node 在 `hello` 与心跳里带它支持的 feature 列表，Hub 按能力下发，
    **绝不根据协议版本号 `v` 推断「这台设备支持什么」**。`v` 只在发生
    **不兼容变更**时 +1（如删了必需字段）；新增可选消息类型 / 字段不 bump 版本，
    老节点遇到未知 `t` 直接忽略、不崩。这保证「老 node + 新 Hub」能继续跑，
    而不是一升级 Hub 就把旧设备踢下线。

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

### 生产代码禁 panic 铁律

**`crates/**/src/**` 的非测试代码禁止 `unwrap()` / `expect()` / `panic!()` /
裸索引切片 / `let _ =` 丢错误。** 由 workspace lints 强制（见下）。

```rust
// ❌ 假设失败即进程退出：Hub 是常驻服务，node 是无人值守守护进程，
//    一处「不可能失败」的 unwrap 就能把整台设备带下去。
let seed = &der[der.len() - 32..];          // 裸切片：len < 32 时 panic
let mac = HmacSha256::new_from_slice(k).unwrap();
Params::new(m, t, p, None).expect("valid");

// ✅ 让失败变成可传播的错误，或写成结构上不可能失败
seed.copy_from_slice(der.get(16..48).ok_or(IdentityError::MalformedKey)?);
let idx = ((n >> shift) & 63) as usize;     // 恒 < 64
match T.get(idx) { Some(&c) => out.push(c as char), None => break }
```

`Cargo.toml` 的 `[workspace.lints.clippy]`：

| lint | 级别 | 管什么 |
|---|---|---|
| `unwrap_used` / `expect_used` / `panic` | deny | 进程级退出 |
| `indexing_slicing` | deny | 越界 panic |
| `arithmetic_side_effects` | **不开** | 连 `i += 1` / `len() * 2` 都命中，开只会得到满屏 allow；溢出靠 `overflow-checks` + 测试 |

`clippy --all-targets` 必须**零 warning**，所以上面每条都是 deny 而非 warn。

**`unwrap_or_default` 允许用**（这条规则已收窄）。

真正要禁的是**静默吞掉一个错误**——`let _ =`、`.ok()` 后不管、把 `Err` 分支
压成默认值继续跑。`unwrap_or_default()` 只在**默认值本身就是正确语义**时用，
比如「Hub 没下发某个可选的 JSON 字符串字段」= 空串、「可选的数组字段没给」=
空列表：那里 `None` 与默认值说的是同一件事，写成 `match { Some(v) => v, None
=> Default::default() }` 只是把同一件事多写四行。

反过来，**`None` / `Err` 各自有不同含义时不许压成默认值**，要写成显式分支并
在注释里说清每个分支的含义（`channel.rs` 的 `on_open_acp` 就是这种：空串 =
回落到扫描结果，空列表 = 不追加参数，与「字段形状错了」不是一回事）。

这条不在 `Cargo.toml` 的 lint 表里（clippy 没有对应规则），也不在
`scripts/check.sh` 里（原先的 C4 grep 已删除，见该处注释）。判断靠读代码。

**测试代码豁免由根目录 `clippy.toml` 的 `allow-*-in-tests` 承担，不要手写
`#[allow]` 豁免头**——测试里 `unwrap` 失败就该炸，panic 消息比 `match`
分支更能说明是哪一行、哪个值不对。手抄过 16 个 `mod tests`，抄漏一条那条
lint 就静默失效。

⚠️ 已知边界（rust-clippy#13981，Clippy 1.100 修复）：这四条只覆盖 `#[test]`
函数体与 `#[cfg(test)] mod` 内的代码。`crates/*/tests/*.rs` **顶层的 helper
函数**不在范围内，每个这样的文件需自己写一行 `#![allow(...)]`。

`let_underscore_must_use` 已从 lint 表移除：Clippy 的 8 个 `*-in-tests`
选项里没有它，保留它就得让每个测试文件各写一次 `#[allow]`。它在测试里
几乎没有价值（`let _ =` 正是「失败不影响状态」的常规写法），真正要拦的
是生产代码吞错，那部分由覆盖率门禁与上面四条兜住。

**真不可达的兜底**（如 `match T.get(idx) { None => break }`）要能跑到或标
`// coverage: excluded — <理由>`，与覆盖率门禁同规则。

### 依赖版本收敛铁律

**所有依赖版本号只出现在根 `Cargo.toml` 的 `[workspace.dependencies]` 与
`[workspace.dev-dependencies]`**，子 crate 一律写 `xxx.workspace = true`。

```toml
# ❌ 子 crate 里手写版本号
[dependencies]
serde = "1"              # 会与其它 crate 的 "1.0.210" 解析成不同版本

# ✅
[dependencies]
serde.workspace = true
```

理由：同一依赖出现多个版本 → **重复编译、二进制体积翻倍、`--locked` 无法复现**；
更麻烦的是 trait 实现在两个版本间不互通（`impl From<A::Error> for B::Error` 编不过），
报错信息指向的类型名一模一样，极难定位。

由 `scripts/check.sh` C1 以 grep 强制，新增依赖时同步补进根 `[workspace.dependencies]`。

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

## 注释规范

**代码能自解释的，就别写注释。** 一条注释如果在复述代码已经说清的事，它就是负债：
代码改了它不会跟着改，于是它开始撒谎。

只写这三类：

1. **为什么是这样**——读代码看不出来的动机、被否掉的替代方案、踩过的坑。
   例：「为什么必须有抖动：同机两个进程同时重连会稳定活锁」。
2. **对外契约**——调用方必须知道但签名表达不出的约束。
   例：「节点不得硬编码心跳周期」「`superseded` 是终态，不得重连」。
3. **反直觉的事实**——看起来像 bug 但其实是有意的。
   例：「`mut` 是故意的：这个字段只在 Drop 里被读」。

不写这些：

- 复述代码的（`// 自增 attempt`）
- 复述断言本身的测试注释（`// 断言 code 是 SUCCESS`）
- 大段论证与背景故事——那些属于文档（`DECISIONS.md` / 架构文档），
  写在代码里只会和文档漂移。代码注释里放一句「理由见 D2-⑥」就够。

测试命名本身已经是文档：`a_killed_worker_counts_as_a_crash` 比
「这个测试验证 kill -9」的注释信息量更大。**优先改名字，而不是加注释。**

### 测试代码不要手写豁免头

根 `clippy.toml` 的 `allow-*-in-tests` 已覆盖 `#[cfg(test)] mod tests` 内的代码
（含 `unwrap` / `expect` / `panic` / `indexing_slicing`）。

**`crates/*/src/*.rs` 里的内联测试模块不要再写 `#[allow(...)]`**——那是抄来的，
抄漏会静默失效，而且让人以为这里真的需要豁免。

唯一的合法情形：`crates/*/tests/*.rs` **顶层**的 helper 函数（rust-clippy#13981
的边界，mod 之外的代码不在覆盖范围内）。该文件自己写一行即可，别扩散到 src。

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
8. 是否引入了「项目文件在设备间传输」的路径（已撤销），或做了「整目录同步 / 设备间 P2P 直传」，或把依赖目录、密钥传上了线；**另**：新表是否用了外键（用户裁定不用外键，归属由业务层显式检查并给可读错误）
9. secret 值是否出现在日志 / API 响应 / 轨迹事件里（哪怕「只是本地自用」）
10. 是否出现了让 **Hub 持有设备私钥** 的路径，或让升级导致 `node_id` 变化的机制
11. 是否把「agent 运行时内部的父子 agent」误当成我们 Hub 要编排的对象（约束 8：Hub 只编排
    「任务 → node」，agent 内部分发由其自身决定，Hub 只观测轨迹）

### 代码事实维度（步骤4/5 逐条对照，来源见各里程碑 `## 沉淀候选` 台账）

12. **断言有效性**：期望值不得由被测实现自己再算一次（`f(x) == f(x)`、`assert_eq!(f(), g())` 而 `f` 内部就调 `g`、`coll.len() == coll.len()`、测试内对字面量做算术再断言）；「夹」「不夹」结果相同的断言（阈值写成上界而非逐字值、数据全落在界内、缺 `> 0` 前置、集合恒空使 `!contains` 恒真）一律判假绿；只观测规模（条数 / 长度）不断言内容、只断言中间产物形状不断言端到端可用，同样判不足；用例名声称的命题（全仓 / 全部 / 要么 A 要么 B / 输出形状）必须被函数体断言覆盖；自陈「观测不到 / 渲染不出来」后补的 `expect` 是假可观测量。
13. **反向验证**：断言有效性只能靠「改坏被测代码看是否变红」发现，「读用例形状」会被同义复述误导；`[x]` 类修复的验证须能区分「删对了」与「删多了」；`(racy)` 标注会让功能层同时失去检测力（删掉断言用例仍绿），标了就必须另找确定性判据；用例名与实现不符、prop 改名零断言这类只能靠变异暴露。
14. **端到端可达**：上行路径必须在真实链路上被验证——「前端 mock、后端直发帧，双方各测各的」等于中间路径无人覆盖；e2e 须断言「输入生效」而非仅有输出；展示用复合字段（`"b.txt <- a.txt"`、`new <- old`）不得原样回传当参数；被 `v-if` 遮蔽的分支内状态清理天然无测试出口，须如实登记为「只能靠源码契约」而不是补假断言。
15. **注释与文档事实核对**：注释 / 文档里的 `文件:行号`、符号名、计数（「N 处」「三件事」「第 N 行」）、版本号、「由 X 自动生成」的 X，须逐条实测——位置会随后续编辑漂移，漂移后理由仍指着旧内容即为缺陷；注释为已删除的实现辩护、以现存语气描述已删除的行为、写「防重入」「页面不会并发问两次」而代码无对应守卫，均判违规；同一节改 A 处漏 B 处、三处一致契约只改两处、文档计数与代码实况不符，均判违规。
16. **单一来源**：同一常量 / 错误码 / 文案 / 键位 / DTO / 协议形状 / 夹具常量在第二处出现即抽公共件；「收成常量」之后新增的测试若拿常量当前值当期望值，等于恒真；同一陷阱在 A 文件已修并写了注释警告，B 文件随后逐字复现，判为本维度缺失。
17. **死代码与残留**：删功能后须清 CSS 类 / prop / 不可达分支 / 只被它引用的函数 / i18n 死键 / 文档段落；`pub` 成员零生产调用、或仅被测试调用、或注释称有消费者而实际没有，均等价于死代码；「留着以防将来」的死字段、恒真条件残骸变量、插槽兜底永不渲染，判违规；但「0 引用 ≠ 死代码」——须先判是否经转发 shim / 全局表复用。
18. **错误与状态口径**：同一函数内 `?` 抛错与 `.join().ok().unwrap_or_default()` 吞错并存、守卫被下移但仍吞错、拒绝类错误被 `code()` 压成 `internal`（页面拿不到可读理由）、错误枚举所有变体折成同一码，均判违规；`null` 只许有一个含义；「先写 loading 标记再发可能早返的请求」的早返路径必须复位标记；「进入某状态」与「离开该状态」、分段切换的「去」与「回」两侧、对称的两处状态清理，必须同时处理。
19. **安全与归属**：新增安全守卫须有负样本用例（只测正路径不算）；子进程调用除 `current_dir` 外须清 `GIT_*`/`SSH_*` 等父进程环境；中间件豁免表不得连带走私横切检查（CSRF）；「先删 / 先 unbind 再校验归属」的 TOCTOU 写法禁止；字段名不得与本文档术语规范冲突（`agent_*` 指设备进程）。
20. **契约与门禁一致性**：门禁「自认覆盖却一条不查」是独立缺陷类——受管文件名单、`OWNED` 名单、分派表、校验段各写一份时，漏项只会让计数变小而不报错；覆盖率例外清单须走 `--rebuild --uncovered` 维护，`(racy)` 行进表会造成结构性永久红，理由按行号继承会跟错行；门禁分数须落「阈值 / 实际数值」两列，禁用「组合命令绿了」代替逐项数据；以集合相等为口径的门禁须先证明输入集合确定性，历史 PASS 不能继承。
21. **测试与夹具卫生**：夹具不得重写被测系统的输入常量（密码、库文件名、端口区间）；夹具注释声明的等待条件必须与代码实际等待的对象一致；夹具须有生产类型标注（后端加字段时编译器要叫住）；`setUp` 不得重复注册同一钩子把已有保护抵消；禁止用改全局环境（`PATH` / `set_var`）注入测试替身；契约变更后旧夹具须同步。
22. **流程与台账**：产品文档每项能力须有映射承接（反之亦然）；`Bugs` 表条目随修订同步状态，「已修」证据须能区分「删对了」与「删多了」；验收阶段把「待修发现」按「是否影响需求满足」分流，不得一律判 ✗；标 ✅ 的子任务其声称范围须全部落地；里程碑 `起始 ref` 取上一里程碑**结束**；磁盘满属可修复因素，须先排除再下结论。

---

## 协作约定

- **Commit message 一律英文**，遵循 conventional commits（`feat:` / `fix:` / `docs:` / `chore:` / `refactor:`）。
- **README 与 CHANGELOG 中英双语**：每条内容同时给出中文与英文。其余文档（AGENTS / PRODUCT / DEVELOPMENT / 架构文档）以中文为主。
- **前端命令一律用 `bun`**，禁止 `npm run`。

---

## 质量门禁

Rust：

```bash
scripts/check.sh                                  # grep 门禁（依赖收敛/命名/锚点）
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
scripts/check-coverage.sh                         # 覆盖率门禁（100% 阻塞 + 例外双向核对），见下
```

Web（`packages/web`，Vue 3 + Bun + Vite）：

```bash
bun install --frozen-lockfile
bun run type-check     # vue-tsc --noEmit
bun run lint           # eslint
bun run test           # vitest
bun run build          # vite build
```

工具链版本钉在 `mise.toml` 的 `[tools]`：**node 24 是硬需求**，不是建议——
Vite 7 的 `engines` 是 `^20.19.0 || >=22.12.0`，且 `@vitejs/plugin-vue` 用
`node:crypto` 的 `crypto.hash`（Node 20.12+ 才有）。系统 node 18 上
`bun run build` 报的是 `crypto.hash is not a function`，看起来像代码 bug，
实则是工具链版本问题。

四道门禁的 mise 入口是 `mise run web-ci`（`web-check` + `web-test` +
`web-build`）。**本地全量门禁是 `mise run ci`**（check + fmt + lint + test +
`cov-gate` + `cov-anchors` + web-ci），CI 跑的是同一串（见
`.github/workflows/ci.yml`）。`cov-anchors` 是「每条覆盖率例外的理由都能在源码里
找到出处」的核对——少了它，例外清单会攒下一批无法复核的理由。

**前端不设行覆盖率阈值**——这是**度量工具不适用**，不是放水：Rust 侧 100% 依赖
「行覆盖率」这个度量，而 Vue SFC 模板 / 脚本 / 样式混排无法按行表达执行与否，
xterm.js 这类第三方渲染库的内部逻辑也不归我们测。前端替代口径是
**关键逻辑必须有单测覆盖**（焦点栈、键位表、listbox、tab 状态机、`http.ts` 的
错误映射与 CSRF 回填），由 `bun run test` 阻塞。改动若让某个关键模块的测试为零，
即视为门禁失败。

测试与覆盖率项若确实无法本地自证，如实记录「不可执行 + 根因」，
不得记为通过；也不要把「环境因素导致失败」写成「环境限制」——
磁盘、工具链这类**可修复因素**应先排除再下结论。

### 覆盖率门禁：零未登记未覆盖行（阻塞）

**「每一行没测到的代码都有名字」是硬性门禁**，不是报告项：

```bash
cargo llvm-cov --workspace --all-targets --summary-only   # 汇总
cargo llvm-cov --workspace --all-targets                  # 明细（找未覆盖行）
```

- **判定口径是集合相等，不是百分比**：门禁取
  `cargo llvm-cov --show-missing-lines` 的 **Uncovered 段**
  （整行一次都没执行到的行），与 `coverage-exceptions.txt` 的登记行集合
  **逐行双向比对**：
  - 未覆盖但没登记 → **失败**（有一行代码默默没被测到）；
  - 登记了但已被覆盖 → **失败**（例外清单退化成永久豁免）。
- 未覆盖的代码只有三条合法出路：**补测试**、**删掉**（死代码不该存在）、
  **显式登记例外并给出理由**。
  **禁止**用 `#[cfg(test)]` 之外的 `#[allow]`、`--ignore-tests`、
  排除文件等手段"做"出 100%——那是伪造。
- **例外必须就地标注并说明理由**：确实无法触达的分支
  （如 `unreachable!()` 兜底、平台专属路径）用
  `// coverage: excluded — <理由>` 注明，并在 step6 报告中逐条列出。
  例外是少数，不是常规手段。
- **例外登记在 `coverage-exceptions.txt`，由 `scripts/check-coverage.sh` 强制核对。**
  该文件管的是**行集合**（未覆盖行必须登记、登记必须仍有效）；
  每条登记的**理由**锚在源码里就近的 `// coverage: excluded — …` 就地标注上，
  由 `scripts/coverage-exceptions.sh --check` 核对（不跑 cargo，秒级）。
  清单里的理由可以是人工精修过的更具体版本，`--check` 会把它们列出来。

  > 为什么不直接让工具跳过：`cargo-llvm-cov` 没有行级排除能力
  > （`#[coverage(off)]` 在 rustc 1.99 仍是实验特性），而唯一可用的
  > `--ignore-filename-regex` 是整文件排除，正是上面禁止的伪造手段。
  > 所以例外只能**显式登记 + 脚本强制比对**，而不是让工具静默跳过。

  ```bash
  scripts/check-coverage.sh          # 覆盖率门禁：例外行集合双向核对
  scripts/coverage-exceptions.sh --check   # 例外理由的锚点核对（秒级，可进 CI）
  scripts/coverage-exceptions.sh --rebuild # 从就地标注重建整张表（需一次 llvm-cov）
  ```

  **别拿汇总表的 `Lines` 百分比当门禁口径**：`TOTAL` 的 `Missed Lines`
  把「行执行了、但行内某个区域没走到」也算进去，所以它永远大于 Uncovered 段。
  当前实测：`TOTAL` 的 `Lines` 覆盖 **97.89%**（16437 行 / 347 miss），
  其中 Uncovered 段 **248 行**——差的 99 行是行内部分区域未命中
  （`?` 的 Err 路径、宏的多行参数行这类分支变体），不是整行没测。
  门禁只管后者。
- 当前 248 条例外的构成：**生产 220 + 测试 28**（测试段内 `expect` / `assert!`
  的消息行，绿的时候永远不执行）。生产的 220 条理由分属几个族：
  **结构性不可达**（ROLLBACK 必然成功、闭锁信号量的 `AcquireError`、
  serde 结构上不会失败的字段）、**资源耗尽型兜底**
  （`pipe2` / `fcntl(F_DUPFD)` / `fork` 需 fd 表或进程表耗尽）、
  **工具插桩限制**（`fork` 后的子进程没有 llvm-cov 计数器，
  以及多行 `tracing!` / `format!` 的参数行、`%method()` 字段行的已知漏记）、
  **依赖外部条件无法构造**（对端在收帧前消失）。
  **不含任何业务逻辑。**
- **每轮审查都重跑**：步骤 5（代码审查）后新写的代码同样要满足这个口径。

---

## 仓库结构

```text
docs/
  PRODUCT.md              产品功能边界、架构决策、用户可见流程
  DEVELOPMENT.md          开发索引（技术栈、crate 结构、里程碑总览）
  DECISIONS.md            架构决策记录（为什么这么定 / 否决了什么方案）
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
