# 架构决策记录

> 记录"为什么这么定"，避免改着改着忘了当初的取舍。
> 每条含**决策 / 理由 / 被否决的替代方案**。未决项标注 ⬜ 待定。

---

## D1. 设备端进程模型：同二进制双角色（supervisor / worker）

**决策**

```
afcli run                 → supervisor（默认角色，父进程）
  └─ spawn: afcli run --worker    ← 同一个二进制，只多一个 flag
       └─ 持有 identity.json + WS 连接 + 心跳 + 重连
```

- **supervisor 不碰 WS、不碰业务**。唯一职责：保证有一个能用的 worker。
- **回滚由 supervisor 执行**，而 supervisor 是**没被替换过的旧代码**。
- Docker 模式：`afcli run --worker` 直接跑 worker，无 supervisor、无特判分支。

### 为什么是 `run --worker` 而不是顶层 `--worker` 或环境变量

| 形式 | 评价 |
| --- | --- |
| `afcli run --worker` ✅ | `run` 是"启动守护进程"这个动词，角色是它的修饰符。supervisor spawn 时**把自己的 argv 原样传下去再追加 `--worker`**——用户给的 `--data-dir` 之类参数自动继承，不用重新拼一遍 |
| `afcli --worker` | 没有动词，与 `afcli log` / `afcli status` 这类子命令不一致；看起来像全局开关，实际只影响 `run` |
| `AFCLI_WORKER=1` 环境变量 | ❌ **不采用**：(a) `ps` 里看不见角色，排查"这进程是 supervisor 还是 worker"得翻 `/proc/<pid>/environ`；(b) 环境变量会被孙进程继承，worker 再 spawn 任何东西都会误以为自己是 worker；(c) 与 argv flag 并存就是**两条代码路径**，而覆盖率门禁要求 100%，多一条路径就多一份要单独构造的分支 |

**只有一条进入 worker 角色的路径**：argv 里含 `--worker`。
（原设计同时保留 env var，现删除——两条路径不增加表达能力，只增加要测的分支。
REX 用 env var 是因为它 spawn 时只透传原始 argv；追加一个 flag 一样能做到，还顺带可见。）

**幂等**：若 supervisor 自己就是带 `--worker` 启动的（人手误敲、或 Docker 里直接指定），
它**不再追加**，直接以 worker 角色运行——不会出现 `run --worker --worker`。

**理由**

「执行回滚的代码不参与被回滚」——新二进制再坏也坏不到 supervisor 里，
坏 worker 只会 `exit(11)`，supervisor 照样能把它换回去。
supervisor 职责极小 ⇒ 没有 bug 面 ⇒ 恰恰是最需要永远可靠的那个进程。

**被否决的替代方案**

| 方案 | 否决原因 |
| --- | --- |
| A. supervisor 持 WS，IPC 转发给 worker | 要写一层帧转发 + req_id 映射表，M0 worker 是空壳，为不存在的转发需求付 100% 覆盖率的账 |
| B. worker 持 WS，supervisor 只拉起 | 方向对，但不如双角色彻底——仍需处理 WS 生命周期 |
| 单进程 + `execv` 原地换映像 | **根本性缺陷**：新二进制自己回滚自己。为"同 PID"这个假收益，放弃了唯一真正重要的属性 |

**参考实现**：REX（`rex-common/src/supervisor.rs`）。本方案在其基础上修了 6 个缺陷，
见 [D2](#d2-升级与回滚时序)。

---

## D2. 升级与回滚时序

**决策**：版本号**由 Hub 通过 WS 下发**，不自己探测 GitHub Releases。

```
Hub ──heartbeat 应答带 update{version,url,sha256}──► worker
                                                      │ 下载 → SHA256 校验
                                                      │ 写 update-state.json (phase=Requested)
                                                      │ exit(10)
                                                      ▼
supervisor: 备份 exe → .bak  →  rename(staging → exe)  →  phase=StartingNew
                                                      │
                                             启动新版 worker
                                                      │
                              健康检查：30s 内完成握手且 node_version == 目标版本
                                                      │
                                    失败 → exit(11) → supervisor rollback()
```

- **worker 立即生效新代码**；**supervisor 懒生效**——文件已换成新的，进程继续跑旧映像，
  下次被 systemd/docker/用户拉起才更新。
  - 前提（**显式承诺**）：supervisor 的 ABI 是极小的稳定接口 = `spawn` + 退出码协议 + `update-state.json`，必须向后兼容。
  - 附带解决 Windows：没有 systemd 兜底，懒生效让 supervisor 永远不必为了自己的新代码而退出。

### 对 REX 参考实现的 6 处修正

| # | REX 的问题 | 本项目的修法 |
| --- | --- | --- |
| ① | `fast_crash` 判定在读 update-state **之前** → 坏版本把 supervisor 直接拖死，**永不回滚**（砖设备） | 判定移到读 state **之后**；本轮有 pending update → 走 rollback 而非 exit |
| ② | 退出码 0 不算 fast_crash → 「启动即干净退出」的 worker 以 1s 间隔无限重生，**永不回滚**，Hub 看到心跳风暴 | 改为**时间窗内连续退出计数**，与退出码无关（10s 内 3 次 → 不可恢复 → rollback） |
| ③ | 备份失败后仍 rollback → 用**上一轮**遗留的 `.bak`，**降级到几个月前的陈旧版本**且用户不知情 | **备份先行且必须成功**才进下一阶段；state 记 `generation`，rollback 只认本轮备份，对不上 → `phase=Failed` 大声报错 |
| ④ | 错误全线静默（`let _ = rename`、三层 `if let Ok`、`.ok()`）→ 磁盘满时后续阶段全错，日志一行没有 | 所有 I/O 失败必须 `tracing::error!`；**state 写失败 = 放弃本次更新**（宁可不升级，不可进到"说一套做一套"的阶段）。本项目 `let_underscore_must_use` / `unwrap_used` 是 deny，这类代码本来就编不过 |
| ⑤ | 回滚防死循环是 rename 消耗掉 `.bak` **偶然形成**的，无文档、无计数 | 显式 `rollback_count ≥ 1` → `phase=Failed` + 大声报错，不依赖 rename 副作用 |
| ⑥ | 崩溃在 `rename(exe)` 与 `write StartingNew` 之间 → 重启后 state=Requested 但 staged 已挪走 → **误回滚一次已成功的更新** | supervisor 启动时 **reconcile**：staged 不存在 && 二进制已是目标版本 → `phase=StartingNew` 继续；否则 `phase=Failed` |

另：`std::process::exit()` 不跑 drop，`tracing` 最后几行日志会丢 → 改为从 `main` 返回。

### 决策逻辑必须纯函数化

```rust
fn decide(exit: ExitInfo, state: Option<UpdateState>) -> Decision   // 纯函数，全分支单测
enum Decision { ReplaceAndSpawn, Rollback, Spawn, BackoffSleep, Stop }
// executor 只执行 Decision —— 薄到不需要单测，靠集成测试覆盖
```

**这不是洁癖，是被 100% 覆盖率门禁逼出来的**：REX 把决策内联在阻塞循环里，
里面真的 spawn 子进程、真的 rename 文件——在 100% 行覆盖下几乎没法测（测了就真删二进制）。

---

## D3. 升级触发方式与可见性

**决策**

- Hub 页面**显示每个设备的 CLI 版本号**
- 页面可**手动触发升级**
- 页面提供**自动升级开关**（开 / 关）

**理由**：设备状态必须在 Web 上"一眼可见"（产品基调）。升级失败必须在页面上看到
「设备 pi-1 升级失败，已回滚」，而不是只能 ssh 上去看日志。

**被否决 / 明确不做**

- ❌ 最短发布年龄（24h）—— 升级跟 Hub 走，Hub 自己决定何时推
- ❌ 不降级检查 —— Hub 不会下发比自己旧的版本
- ❌ 包管理器检测 —— 不进包安装体系（暂不考虑进包安装）

**协议影响**：需新增 `update_available{version,url,sha256}` 消息类型
（`protocol::message::types`，非必需集，老节点忽略）。同步更新 `docs/architecture/protocol.md` §2。

**验签分层**（此处修正此前的高估）：
- **M0：SHA256**（Hub 下发，防下载文件被污染 —— URL 走 CDN，这是真实威胁）
- ❌ ed25519 全量验签随 release 流水线一起立，**不阻塞 M0**
- 理由：信任根就是 Hub，Hub 本就能下发任意命令，签名防不住 Hub 被攻破，边际价值小

---

## D4. 日志：`afcli log`

**决策**

- 日志落盘：`<data-dir>/logs/afcli.log`，按大小 / 天数轮转，权限 **0600**
- supervisor 与 worker **写同一个文件**（追加，整行原子写）
- `afcli log` 默认 tail 最近 **200 行**；`afcli log -n <N>` 指定行数
- 所有 I/O 失败必须留痕（同 D2-④）

**理由**：`afcli` 是守护进程，**没有日志等于黑盒**——升级失败、回滚、重连退避这些
只在设备端发生的事件，用户 ssh 上去也看不到。这条把"日志落地"从 M1 拉进了 M0。

---

## D5. Hub 存储后端：sqlite 默认 / postgres 可选

**决策**（新立里程碑，**不并入 M0**）

- Hub 自身存储的**完整后端**：`sqlite://` 默认、`postgres://` 可选
- 导出为**一次性快照**（不是持续同步）
- 不引 Redis；不做多节点 Hub（多用户需求以后再说）

---

## D6. Windows 支持范围 ⬜ 待定

- **node 侧必须支持 Windows**（cli 要在 Windows 上，后续还要能操作电脑）
- **Hub 是否也要跑 Windows**：⬜ 未决。**建议 B：Hub 仍 Linux**（单用户 self-host，
  部署在 NAS/小服务器，无 Windows 诉求）

**Windows 改造面**（现有硬 Unix 依赖，供估算）：

| 依赖 | 位置 |
| --- | --- |
| `libc::umask(077)` | hub-storage/db.rs、main.rs |
| `PermissionsExt::mode(0o600)` | db.rs、identity.rs、setup.rs |
| `libc::kill(SIGTERM)` | afhub_process.rs |
| `libc::execv` | 自更新 |
| `/proc/net/tcp` + `/proc/<pid>/fd` | ws_flow.rs |

⚠️ **约束 13 的「0600/0700/umask 077」在 Windows 上无语义等价物，需按 NTFS ACL 重定义。**

⚠️ **`self_replace` 在 Windows 的已知坑**（Mise #12062）：先移走 exe 再从 TEMP 启 helper，
路径超 `MAX_PATH`(202) 时 `CreateProcess` 失败 → **安装目录变空、设备变砖**。上 Windows 必须预检路径长度。

---

## D7. Hub 自身不参与自更新

- **Hub 不实现自更新**：`afhub` 没有 `update` 子命令，也没有 `self_replace` 路径。
  M0 的升级机制（D2 的 supervisor/worker + 退出码协议 + 回滚）**只服务 node 侧**。
- **为什么**：supervisor 那套机制的可靠全押在「执行回滚的进程未被替换」上。Hub 若是
  单进程（Docker / systemd 托管），替换正在运行的二进制仍需一次重启，而那正是 D2
  想避开的东西；把 supervisor 再套一层到 Hub 上，收益为零而复杂度实打实。
- **Hub 怎么升级**：换镜像 / 换二进制后由外部进程管理器重启，与 D3 一致——
  **Hub 升级是部署动作，不是产品功能**。
- **与 D6 的关系**：Windows 改造面里 `libc::execv` 那一行**只属于 node 侧的自更新**，
  Hub 不上 Windows 不影响它；反过来 Hub 若真要上 Windows，也不必因此获得自更新能力。
  两项互不牵连，可分别裁决。

---

## D8. Origin 检查

**决策**：保留，定位为**纵深防御，不是安全边界**。
（曾提议改用自定义 User-Agent —— ⬜ 未落地，未被否决）

---

## D9. 环境一致：绑定声明，不绑定产物

**决策**：环境一致性靠**声明的可哈希指纹**（`env_signature`）保证，
不靠把已构建产物搬过去。sub-task 的**跨设备分发推迟到 M8**，
M5 只做**同一设备内**的 sub-task 编排。

### 为什么「搬产物」这条路是错的

「sub-task 换机器时用一样的环境」这个诉求，默认解法是同步整份依赖目录。
这条路有三重硬阻，任意一条都足以否掉它：

1. **依赖目录按架构分化**。`node_modules` 里是平台相关的二进制，
   x86-64 与 aarch64 装出来的不是同一份东西——**根本不存在「同一份依赖」可搬**。
2. **体积**。`workspace-and-sync.md:81` 的永不传输清单里 `node_modules/`、
   `.venv/`、`target/` 都在列，理由是「体积爆炸、平台相关、可能含绝对路径」。
3. **绝对路径**。虚拟环境与部分构建产物内嵌构建机路径，搬过去即失效。

### 为什么 mise 是答案的一半，不是全部

`mise.toml` 的 `[tools]` 已经钉死 `rust` / `bun` 版本，其注释里的理由
（重编译全树、覆盖率跨工具链不可比）与 sub-task 换机器是同一个理由。
但它**只管工具链版本**，管不到：

| 层 | 内容 | 载体 | 谁负责 |
|---|---|---|---|
| 工具链 | rust / bun / node / python 版本 | `mise.toml` | mise |
| 项目依赖 | `Cargo.lock` / `bun.lock` / `uv.lock` | 各语言 lockfile | 包管理器 |
| 系统能力 | openssl / cmake / pkg-config 版本 | **无载体，需新增声明** | 无人（缺口） |

第三层是最容易漏的一层：`rust = "1.99.0"` 装得上，但 `openssl-sys`
要的 `libssl-dev` 是宿主 OS 的东西，树莓派上是 OpenSSL 3.0、开发机上是 1.1。
同一个 rust、同一个 `Cargo.lock`，编出来的东西**不一样**。

### 三段式 env_signature

```toml
# 1. 工具链：mise.toml 内容哈希
[tools]
rust = "1.99.0"
bun  = "1.4.3"

# 2. 依赖：各语言 lockfile 内容哈希
#    rust: Cargo.lock / node: bun.lock / python: uv.lock

# 3. 系统能力：新增声明，随心跳上报
[env]
AF_SYS_REQUIRES = "openssl>=3.0,cmake>=3.22"
```

**定位：观测项，不是路由依据的独占来源。** 第一版路由仍按能力/负载打分
（`scheduling.md`），`env_signature` 只负责让页面能**说清「这台为什么跑不了」**——
差在哪一段（工具链不同 / lockfile 不同 / 系统库不满足），
而不是等 sub-task 跑崩了再猜。

**增量演进**：第一版只有 `os` / `arch`（M0 已在上报）；
第二版加 lockfile 哈希；第三版加系统库版本。每加一段是一次 capability 增量，
M7 的插件化来了正好接上。

### 为什么 M5 只做单设备

跨设备 sub-task 分发的三支柱里，两个在 M2（CAS + 工作区物化）、
一个在 M8（checkpoint 续跑），而真正回答本决策的那一层横跨 M2 与 M8。
在 M5 实现必然是残的，返工成本高于延后。

**M5 缩小后的样子**：任务记录 + 状态机 + 一个跑任务的方法。
注意语义变化——M5 不再是「Hub 决定派给哪台 node」（跨设备分发），
而是「Hub 建任务，**node 决定开几个 sub-task**」（node 侧编排）。
后者正是约束 8 说 Hub 不该做的事，所以 M5 砍掉跨设备分发**不损失能力**，
只是移走了原本就不属于 Hub 的部分。

### 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 同步整份依赖目录到目标设备 | 架构上不可能「一样」（平台相关），且违反永不传输清单 |
| 设备间 P2P 直传 | 违反约束 10：Hub 必须保持唯一真相源 |
| 只钉工具链版本（当前 mise.toml） | 系统能力层无载体，`openssl` 差异会静默改变产物 |
| 现在就做跨设备分发 | 三支柱缺两个，返工成本高于延后 |

---

## D10. 多会话 tab：视图切换器，不是生命周期边界

**决策**：会话工作台顶部 tab 条支持同时打开多个会话。**切 tab 不释放通道**，
关掉 tab 才释放。

### 为什么切 tab 不关面板

边界是「关掉这个 tab」或「离开会话列表」。若切 tab 也关面板，
则来回切两次就重建 PTY，用户刚跑出来的终端输出全丢——
**「支持多个会话」会退化成一条假的 tab 条**（切过去永远是空的）。

代价是实的：N 个 tab = N × 面板数条活跃通道。所以每个 tab 显示自己的**面板数徽标**——
**让代价可见**，而不是让用户猜。

**没有硬上限是有意的**：设上限就要在超限时做某种拒绝，
而拒绝谁（最早的？最重的？）是产品决策，不是设计决策。
实现阶段先加观测项，看真实使用里 N 实际到多大。

### 焦点落点

切 tab 后焦点进**工作台正文**，不回 tab 条——用户意图是「去那儿干活」，
不是「继续看标签栏」。`⇧Tab` 一步回到 tab 条（它是紧邻的上一个 tab stop）。

tab 条是一个 tab stop，`←→` 移动**并激活**（像浏览器 tab 条）：
切一个 tab 便宜且可逆，没有理由让用户多按一次 `⏎`；
真正不可逆的动作（关 tab）才需要停顿。

### 关闭按钮必须是真 button

曾设计为 `<span role="button">` 塞在 tab 按钮里。**否决**：
那样它既不在 Tab 序列里（父按钮把它从顺序中拿走），又让父元素的
`:focus-within` 永远不成立——**「键盘用户能关掉一个会话」在结构上做不出来**。
关闭会释放通道、丢未保存改动，是不可逆动作，不许是鼠标专属。

**没有全局关闭快捷键**：`⌃W` 是浏览器的，拦不住。
把一个冲突的键绑到不可逆动作上，比不给键更危险。

### 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 切 tab 即关闭面板 | 来回切两次就丢终端输出，「支持多个」是假的 |
| tab 条可深链接（`?tab=3`） | tab 是瞬态的，不该能链到「第 3 个标签页」 |
| 硬上限 N 个 tab | 超限拒绝谁（最早/最重）是产品决策，不该由设计替他决定 |
| `⌃W` 关闭 tab | 浏览器占用，拦不住；冲突键绑不可逆动作更危险 |

---

## D11. 设备重注册后的身份接续：只换设备行的密钥材料，不动历史数据

**场景**：设备先注册，Hub 里是记录 A。用户清理了设备本地数据（`identity.json` 连同私钥一起没了），
重新注册后 Hub 里多出记录 B。想把 B「并进」A。

**决策**：提供手工操作，**只把 device 行的身份材料改成 B 的**——
`UPDATE device SET key_fingerprint=<B 的指纹>, public_key=<B 的公钥> WHERE id=<A>`，
然后删掉 B。**A 名下的历史数据（工作区、会话、轨迹）原地不动。**

### 为什么 node_id 会变，以及它不是 bug

`node_id` 是 16 字节 CSPRNG 随机串（`crates/identity/src/lib.rs` 的 `random_id()`），
与密钥对**没有任何密码学派生关系**。指纹是 `SHA-256(公钥)`，新密钥对必然是新指纹。
enroll 载荷结构上**不带** `node_id`，Hub 侧 `issue_node_id()` 的结果调用方也无法指定。

所以「清空本地数据 → 重注册」= 新指纹 + 新随机 id = 第二条记录。
**这不是「同一台设备又来了一次 enroll」**——后者会被指纹命中并复用原 node_id
（`Registry::enroll` 的快路径，`AlreadyRegistered` 分支不是错误）。

设备侧私钥与 node_id 同存 `identity.json`，`load_or_create` 在文件缺失时生成全新密钥对，
两者必然一起丢。**这不是设计缺陷，是「密钥丢了就得换身份」的直接后果**：
签名锚点是密钥对，密钥没了就没有任何东西能把「新来的这台」与「原来那台」绑在一起。

### 为什么不做「搬数据」

把 B 的历史改挂到 A 名下（改 `workspace.node_id`、逐行改轨迹事件的冗余 `node_id` 列）
技术上做得到，但**方向是反的**：那些数据本来就是 A 名下设备产生的，
它们描述的是「设备 A 在某段时间做了什么」，不是「新设备 B 做过什么」。
把 B 的数据搬到 A，等于往设备的历史里塞进从未发生过的事。

反过来的方向（把 A 的历史搬到 B）才说得通，但那需要先回答「旧工作区在新设备上还在不在」
——而工作区是**设备本地路径**，设备都重装了，那个路径大概率已经没有意义了。
所以两种搬法都不做，只保留 A 的历史、承认设备换了。

### 为什么不做「真合并」（A 和 B 的数据并集）

要先定义「同一 `(node_id, path)` 下两个工作区怎么处理」——冲突、并存还是报错？
这是产品决策不是技术决策。而且一轮合并后「身份唯一性锚点」会从密钥对退化成
「人工维护的合并历史」，将来出问题时无法判断某条记录是谁接上来的。

### 与硬约束的关系

约束 12（升级只换 worker，身份不变，**因此不需要身份抢占逻辑**）是**设计理由**不是禁止条款，
它的射程限定在**升级路径**。手工接续是用户显式声明「这台机器就是原来那台」，
不违反「升级导致 node_id 变化」这条红线（审查维度 10）。

约束 11（设备私钥永不上传）约束的是**密钥流向**：接续后 Hub 存的是 B 的**公钥**，
私钥始终在设备上。**「存了公钥」这件事本身不违反约束 11**——指纹不可逆，
验签必须要公钥，`DeviceRepo::set_public_key` 与 `pubkey_from_fingerprint`
就是为此存在。约束 11 的字面表述（「Hub 只存指纹」）应按「只存公钥与指纹，
绝不存私钥」理解。

### 必须满足的不变量

1. **任一时刻，一个指纹只对应一个 node_id**（`device.key_fingerprint` 的 UNIQUE 约束）
2. **任一时刻，一个 node_id 只对应一个指纹**
3. 接续后设备能正常重连：新私钥对上新公钥，验签通过
4. 接续**不产生任何身份抢占窗口**——操作期间 B 与 A 同时在线会被明确拒绝，
   不允许「两个私钥轮流用同一个 node_id」

### 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 把 A 的历史改挂到 B | 设备都重装了，旧工作区路径在新机器上大概率无意义；搬过去等于制造假历史 |
| A/B 数据并集 | 「同一路径两个工作区怎么处理」是产品决策；合并后身份锚点从密钥对退化成人工历史 |
| 让用户指定 node_id | 破坏「指纹是唯一入口」，等于引入可枚举的弱身份；也违背约束 5 |
| 合并时保留旧指纹不换 | A 的公钥已随设备丢失，Hub 留着旧指纹会让人以为那台设备还在 |
| 升级路径复用此机制 | 升级只换 worker、身份不变（约束 12），与「设备被清空」是两件事，不该共用一条路径 |

### 未决（排期时必须先定）

- **合并操作的入口**：Web 设备页的按钮，还是 Hub CLI 子命令？前者要多一条带二次确认的 API。
- **是否要求 B 先离线**：倾向要求（不变量 4），但「B 在线」在 Hub 侧怎么判——心跳新鲜度阈值是多少。
- **是否留合并记录**：审计需要「谁在什么时候把哪台接到了哪台」，但这是新表，
  与「仅改一行」的轻量目标有张力。

---

## 已删除 / 边界裁定

- 删死代码 `open_default`
- 「发现设备缺 agent 就自动装」= **产品边界违规**（`afcli` 是接入通道，不是软件分发器）
- Gateway 配置同步是**追加**不是**替换**
- `docs/BUGS.md`「分布式推理路线选型」条目已被 ai-gateway 决策取代，属失效条目待清理
