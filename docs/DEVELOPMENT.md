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

门禁是**一回事**：本地 `mise run ci`、产线 `release.yml` 的 `gate` job 跑的是同一串：
check → fmt → lint → test → **cov-gate**（行覆盖率 ≥98%）→ web-ci（= web-check / web-test / web-build）。
工具链版本只写在 `mise.toml` 的 `[tools]` 一处，workflow 从那里读——
覆盖率数值跨工具链不可比，版本写两处迟早只改一处。

**覆盖率口径在 v0.7.2 改过一次，原因与代价都要记住**：原口径是「未覆盖行集合
== 例外登记集合」（`coverage-exceptions.txt` 676 条 + 就地 `// coverage: excluded`
标注 191 条，配 `cov-gate` / `cov-anchors` 两道门禁）。它的问题是**以行号索引源码**：
任何一次加行都要手工同步整张表。v0.7.2 内 4 个提交移动了行号而无人同步，锚点门禁
从 PASS 回归到 FAIL（12 条孤儿标注），而 `scripts/check.sh` 并不调用它——本地判
「全绿」是假绿。现在收敛成一个数字：`cargo llvm-cov … --fail-under-lines 98`，
`mise run cov-gate` 与 CI 的 `cov-gate` 步骤跑的是同一条命令。

⚠️ **这是削弱不是加强**：98% 只约束总量，不再要求每条未覆盖行给出理由，整块新增的
未测试代码可以靠既有总量稀释。原先那套虽然难维护，但确实能拦住「忘了测」。
v0.7.3 已按「补测试或删死代码」这两条真路把覆盖率推过门槛：**实跑 `TOTAL ... 98.02%`、
641 / 32348 行未覆盖，`--fail-under-lines 98` 退出码 0**；不许用排除文件 /
`#[allow]` / 标注伪造数字。源码里遗留的 `// coverage: excluded` 与 `(racy)` 标注
已不参与任何门禁计算，按注释读，别当豁免。覆盖率随每次改动变动，仍以
`cargo llvm-cov` 实跑为准，不把这里的数字当永久判据。

**CI 与本地现在是同一条硬判据**：v0.7.2 收尾时 `cov-gate` 曾带 step 级临时豁免，
v0.7.3 补完测试后已删除；覆盖率失败即 `gate` job failure，下游产物 / 镜像 /
Release 全部 skipped。命令文本 `--fail-under-lines 98` 与下游 `needs` 一律不动，
那两处是 `scripts/check.sh` C7/C8 的解析锚点；`scripts/check.sh` 另断言
`release.yml` 不再出现该豁免字段，防止非阻断逻辑回流。

**钉死与浮动的分档**：`rust` / `node` 钉死（覆盖率门禁是精确比对，换一次编译器整表漂移；
`clippy -D warnings` 也会因新版 lint 凭空变红，而没有人改过代码）。`bun` 跟 `latest`——
依赖由 `packages/web/bun.lock` 锁住（已入库），剩下的风险只有 bun 自己改了构建产物格式，
那会被 `web-ci` 当场发现。**不要**把 `rust` 改成 `stable`：`stable` 不是「更新的版本」，
只是「不固定的版本」；要升级就显式改那个数字。

**唯一权威是 `mise.toml` 的 `[tasks.ci].depends`**。新增门禁只加在那里一次，产线不必再抄。
`scripts/check.sh` 的 C7 会解析 `mise.toml` 与 `release.yml` 的 **4 个 gate job
（`gate-static` / `gate-test` / `gate-coverage` / `gate-web`）的步骤合集**，**双向比对**：
漏抄一条会红，多抄一条也会红；同一条门禁出现在两个 job 里但 `run` 文本不一致也红
（复制步骤后只改了一份）。这条门禁存在的理由是「少跑的那一道不会红，它只是不存在」。

**为什么门禁拆成 4 个 job（v0.7.1 收尾改造）**：同一个 job 内的步骤**永远串行**，
而 `test`（普通二进制）与 `cov-gate`（全量插桩二进制）是两套互不复用的产物，
`web-ci` 与 Rust 侧零交集——原本没有数据依赖，拆开后 wall clock 从「逐项相加」
变成「最长那条」：实测 **14m06s → 7m09s（省 49%）**。v0.7.2 收尾只为复用缓存与
dist 加了两条轻量 `needs`：`gate-test` 等 `gate-static`、`gate-coverage` 等 `gate-web`。
`Swatinem/rust-cache@v2` 只把插桩覆盖率单独留一份；普通 debug 依赖图由
`gate-static` 写、`gate-test` 只读 `shared-key: gate-linux`，发布矩阵按 target
用 `build-<target>` / `desktop-<target>`（对齐 REX）。**腾磁盘（62s）只在
`gate-coverage` 里做**——只有全量插桩 + 链接峰值需要 ≥6 GiB 空闲。

**C8 盯另一件事：gate job 装的 rust 组件够不够跑那些门禁**（它看的是 4 个 job 的合集：
命令在 `gate-test` 里跑、组件在 `gate-coverage` 里装是合法分工，反过来才是错）。`dtolnay/rust-toolchain` 带
`--profile minimal`，只装 `components:` 里**显式列出**的东西。首次真跑 CI 时那里只写了
`llvm-tools-preview`，于是 `cargo fmt --check` 报 `'cargo-fmt' is not installed`——而**本地是绿的**，
因为 `cargo fmt` 找不到自带的 `cargo-fmt` 时会从 `$PATH` 捞到 rustup 的 shim、退回 `stable` 的
rustfmt 并成功返回（`cargo fmt --version` 报 `rustfmt 1.10.0-stable`，即用错工具链还能过）。
所以本地 `mise run ci` 绿**不能证明 CI 绿**，这一层必须由能读 workflow 的门禁来钉。
C8 有**两个方向**：① 正向——门禁用到的命令 → 需要的组件必须在 `components:` 里；
② 反向——跑了覆盖率门禁的 job 必须有 `taiki-e/install-action@cargo-llvm-cov` 安装步
（v0.7.2 起覆盖率步骤直接把 `cargo llvm-cov …` 写在 `run:` 里、不再经脚本包一层，
所以这条按**命令文本**断言；旧写法只认 `check-coverage.sh` 文件名，那个脚本删掉后
这条断言会恒假通过，正是它要防的假绿）。

> **`ci.yml` 已删除（v0.7.1）**。它曾经是与 `release.yml` 并列的第二个 workflow，两份门禁
> 清单各写一遍、靠 C7 比对防漂移。删掉的理由：`gate` job 已经把全部门禁跑了一遍，`ci.yml`
> 是纯重复；而两个 workflow 同触发并行会让页面上出现两个 run（见下面 `gate` 那段的历史）。
> 代价如实记录：**门禁红时不再有独立的 `CI` run 告诉你哪条挂了**——只能看 `release` run 里
> `gate` job 的步骤名（`cov-gate（mise run cov-gate）` 之类），信息其实没丢，只是换了位置。

**只在推 tag（`v*`）时触发**：全量插桩的覆盖率是最贵的一步，
每个 push / PR 都跑一遍等于把 runner 时间烧在没定稿的提交上。
产线的定位是「发布前的最后一道闸」，每次提交的反馈由本地 `mise run ci` 承担。

### 发布（**v0.7.1 落地**）

打 tag（`vX.Y.Z`）触发 `.github/workflows/release.yml`，形态是**先过闸、前端只编一次、二进制按 target 缓存、三个消费者**：

```text
gate（= mise.toml [tasks.ci] 的全部门禁，产线自己等它；红则下面全部不跑——**唯一例外是覆盖率，见下**）
    ├─ gate-web：唯一一次 bun install + bun run build，上传 web-dist
    └─ gate-coverage：下载 web-dist；Rust 插桩产物独立 cache
    ↓ needs
build-frontend（只校验/承接 web-dist artifact）
    ↓ needs（硬前置，不是优化）
build-binaries（2 平台 musl 矩阵，**原生编译**，按 target 复用 Rust cache）
    ├─→ docker-hub：Hub 镜像（afhub + afcli，前端已编译期嵌入，镜像里不另放 dist）
    ├─→ docker-node：afcli 镜像（linux/amd64 + linux/arm64，按 TARGETARCH 选二进制）
    └─→ release：GitHub Release + SHA256SUMS + install.sh + 从 CHANGELOG.md 抽 body
```

**Release 附件只发 musl 静态产物，且用不带后缀的干净名**（`afhub-linux-amd64` 而非
`afhub-linux-amd64-musl`）。三条理由：

1. **与容器取同一份**。`Dockerfile.hub` / `Dockerfile.node` 都是 `FROM scratch`，里面没有
   glibc，三个 `COPY` 全指向 `*-musl`。用户下载物与容器原料是同一个文件，才不会出现
   「镜像能跑、下载的跑不了」这种两份真相源。
2. **下载即可直接执行**。musl 目标静态链 libc，装在裸 Linux / macOS 设备上不缺
   `ld-linux` / `libc.so.6`；`install.sh` 按 `os/arch` 选文件、校验 `SHA256SUMS`、
   `chmod +x` 后真跑一次 `afcli --version`，把「可执行文件」当场验掉。
3. **避免制造错误选择**。gnu 产物在 scratch 里起不来；去掉 `-musl` 后缀是不让人
   先懂 glibc 与 musl 的区别才能选对。

**v0.7.3 起 `build-binaries` 只编 musl 两条目标**：先前 gnu 产物既不进 Release、
也不进镜像，零消费方却每次发布多编两套完整依赖图（用户 2026-11-07 裁定「浪费资源」）。
`scripts/check.sh` C10 断言矩阵里不再出现 `*-gnu`。附件固定 8 个：
`afhub-linux-{amd64,arm64}` / `afcli-linux-{amd64,arm64}` + 2 个桌面产物 +
`install.sh` + `SHA256SUMS`；`release` job 写死白名单并断言「Linux 二进制恰为 4 个且名字不含 musl」，
防止产物命名将来变化时 glob 空转、建出一个没有二进制的空 Release。

**`gate` 为什么会存在**：`release.yml` 曾经与门禁只共享触发、互不阻塞，门禁红时
镜像照样推上 GHCR、Release 照样建，`latest` 一旦被污染所有 `docker pull :latest` 的用户
直接升到坏版本。当时的防线是「tag 只在本地 `mise run ci` 全绿后才打」，而纪律挡不住手工
打 tag 与 `workflow_dispatch` 补发。所以门禁进产线：`release.yml` 的 `gate` job 跑
`mise.toml [tasks.ci]` 的**全部**门禁，所有产线 job 都 `needs` 它。清单只有一处（`mise.toml`），
C7 双向比对产线的步骤名——不再存在「两份清单互相漂移」这件事。

> **没有例外**：`cov-gate` 与其余门禁一样硬阻断。v0.7.3 删除了旧 step 级临时
> 豁免，补测试到实跑 98.02%；覆盖率红时 `gate` job 判 failure，下面全部不跑。
> `scripts/check.sh` 的 C11 护栏同时禁止该豁免字段回流。

**`build-frontend` 是硬前置，不再重复构建**：dist 由 `gate-web` 的 `web-ci` 唯一一次产出并上传为
`web-dist`；`gate-coverage`、`build-frontend`、`build-binaries` 都下载同一份 artifact。
`include_dir!` 在编译期嵌入 dist，而 dist 缺失时 `crates/hub-server/build.rs` 不失败——它造一个
占位页、打一条 `cargo:warning`，二进制照编。后果是「Hub 装起来打开是 Not built 占位页」能不红
任何一步地走到用户手里。所以矩阵里**每个平台在编之前**都断言 `packages/web/dist/index.html`
存在，缺了就 fail。`scripts/check.sh` 的 C10 钉死「全 workflow 仅一处 `bun run build` / 一处
`web-dist` 上传 / 无 `macos-14`」。

**GitHub Actions 的版本钉在「Node 24 起步的最低版本」**：GitHub 已弃用 Node.js 20
运行时，CI 日志里每一步都会打 `Node.js 20 is deprecated ... forced to run on
Node.js 24`。本 workflow 用的是 `actions/checkout@v5` / `actions/setup-node@v5` /
`actions/upload-artifact@v6` / `actions/download-artifact@v6`——恰好是**各自改用
Node 24 runtime 的第一个版本**。为什么不直接上最新的 v7/v8：那两个大版本改的是
**模块形态**（ESM）与**校验强度**（download-artifact v8 起下载哈希不匹配默认报错），
属于行为变更而非运行时升级，与「消掉弃用警告」这件事无关，混在一起做会让出问题时
分不清是哪个变更带来的。`setup-node` v5 起在 `package.json` 有 `packageManager`
字段时会**自动开 npm 缓存**，这里显式 `package-manager-cache: false`——本仓的包管理器
是 bun，npm 的缓存条目对 bun 无效，让它去猜只会写出无效缓存。

| 文件 | 作用 | 状态 |
|---|---|---|
| `.github/workflows/release.yml` | 门禁 `gate` + 发布全流程（前端 / 二进制矩阵 / 镜像 / Release）。**唯一 workflow** | **v0.7.1 落地** |
| `Dockerfile.hub` | Hub 镜像；`FROM scratch` + musl 静态二进制 | **v0.7.1 落地** |
| `Dockerfile.node` | afcli 镜像；`COPY dist/${TARGETARCH}/afcli-…` | **v0.7.1 落地** |
| `docker-compose.hub.yaml` / `docker-compose.agent.yaml` | 自托管部署 | **v0.7.1 落地** |
| `GET /api/agent-binary` + 设备页下载按钮 | 设备从 Hub 拉 `afcli`（见下） | **v0.7.1 落地** |

#### 设备从 Hub 拉 `afcli`（`GET /api/agent-binary`）

接入引导一直是「在设备上跑 `afcli run <hub-url>`」，可那台设备上**首先得有** `afcli`。
GitHub Release 能用，但两个场景会卡死：**内网/离线部署**（Hub 自己就在隔离网里，设备够不到
GitHub）、**版本对齐**（Release 页上有多个版本，选错就连不上，协议可能已变）。所以多一条路：
从**这一台** Hub 直接下发。

- **读哪个文件：按候选列表探测**（查找顺序照 REX 的 `AgentBinaries::find`）。单一固定路径
  的问题不是「不够灵活」，而是**部署形态一多就必然漏**：镜像里在 `/usr/local/bin`、裸机可能
  在 `/usr/local/lib/agentfleet`、本地调试在 `~/.agentfleet`。所以依次试：

  1. `AFCLI_BINARY_PATH`（显式配置，**独占**——运维写明路径还去猜别的，等于悄悄忽略配置）；
  2. `/usr/local/bin/{os}/{arch}/afcli`、`…/{os}-{arch}/afcli`、`…/afcli-{os}-{arch}`（分架构摆放）；
  3. `/usr/local/bin/afcli`（= `Dockerfile.hub` 的 `COPY` 目标，镜像里唯一的形态）；
  4. `/usr/local/lib/agentfleet/…`、`~/.agentfleet/afcli`（裸机与本地调试）。

  第一个**读得出来且非空**的胜出。「读得出来」而不只是 `exists()`：`exists()` 到 `read()`
  之间文件可能被删或权限不足，那会把 500 漏出去。
- **`?os=` / `?arch=` 只用于定位，不虚构**。REX 的镜像内嵌**所有**架构的 agent 二进制，
  所以它对任意 `os/arch` 都能供货；本仓 `Dockerfile.hub` 只 `COPY` 同架构那一份
  （`${TARGETARCH}`），于是参数只把「你要的平台」拼进候选路径，要的平台这台 Hub 没有就如实
  404。**绝不拿本机那份冒充别的架构**——那会让设备下到一个能 `chmod +x` 但运行即
  `Exec format error` 的文件，而报错发生在设备上。`mac` / `macos` / `darwin` 三个写法等价
  （`std::env::consts::OS` 在 macOS 上给 `macos`，而产物名与文档用 `mac`）。
- **下发文件名带平台**（`afcli-linux-aarch64`）。前端**从 `Content-Disposition` 读**而不是
  另写一个：写死 `afcli` 会让同时有 amd64 与 arm64 两台机器的用户存下两个同名文件，
  靠肉眼分不出哪个能跑；两处各写一个名字还必然漂移。拿不到头时回落 `afcli`——它是可选头，
  解析失败不该让下载失败。
- **不嵌进 Hub 二进制**：`afhub` 与 `afcli` 在镜像里是两个共用同一份静态二进制的进程，
  把 `afcli` 再 `include_dir!` 进 `afhub` 会让镜像多约 20MB 重复。「单二进制」那条硬需求讲的
  是**前端资产**，不是把所有可执行文件塞进一个。
- **找不到、或候选全是 0 字节 → `404 agent_binary_unavailable`，不是 500**。裸机跑 `afhub`
  （不经容器）时磁盘上本来就没有 `afcli`，这是**正常部署形态**，500 会让运维去翻一个不存在的
  bug；0 字节则会让设备 `chmod +x` 后立刻 `Exec format error`，报错发生在设备上。0 字节的候选
  **跳过继续试下一个**（分架构那份坏了，平铺那份可能还好）。错误体只有固定 code，**不含磁盘
  路径**（约束 13）。
- **要 session**：它不下发密钥，但一个能匿名取走约 20MB 文件的端点就是白送的放大攻击面。
  路由注册在 `.layer(require_auth)` 之前，`tests/api.rs` 的认证覆盖表已收它。
- **入口在设备页空态**：「还没有设备接入」那块下面有「下载 afcli」按钮。三种结果分别处置
  （成功 / `agent_binary_unavailable` 提示「这台 Hub 没配」/ 其它错误走通用文案）——
  合并成一句「下载失败」会把「没配」误导成「坏了」。
- **空串的 `AFCLI_BINARY_PATH` 按「没配」处理**：`AFCLI_BINARY_PATH=` 是「我不想配」的常见
  写法，当成路径会让候选表只剩一个空路径、永远 404 且日志看不出「你其实配错了」。

- **原生编译优先于交叉编译**：GitHub 有 `ubuntu-24.04-arm` runner，aarch64 直接原生编
  （REX 的教训：交叉编 OpenSSL/zlib 会架构错配）。
- **v0.7.1 不提供 Windows 目标——是「本里程碑不做」，不是「永远不支持」**：曾经有一条
  `x86_64-pc-windows-gnu`，v0.7.1 首次真跑 CI 时 `exit code 101`。`afcli` 依赖
  `node-connector`，后者有 **81 处 `libc::` 调用**（`pty.rs` 27、`exec.rs` 45、
  `files.rs` 4、`terminal.rs` 5），且障碍不只是 API 名字：`exec.rs` 的
  「`fork` → 子进程改 fd → `execvpe`」模型在 Windows 上要整体换成 `CreateProcess`
  + 句柄重定向，因为 **Windows 没有 `fork`**；会话终端则要从 `forkpty`/`ioctl`
  换成 ConPTY（`CreatePseudoConsole` / `ResizePseudoConsole`）。
  **注意别拿 `REX` 类推**：REX 能在 `windows-latest` 上编，是因为它 16 个 crate 全是
  纯 `std` 的客户端/协议/命令行工具，整个仓库**没有一处 `forkpty`/`openpty`/
  `TIOCSWINSZ`**，跑子进程只用跨平台的 `std::process::Command`——它从未面对 PTY 移植。
  所以 v0.7.1 的矩阵是 4 个 Linux 目标（x86_64 / aarch64 × gnu / musl）。
- **v0.7.2 已把桌面平台做完**：平台抽象层（`pty/mod.rs` + `pty/unix.rs` +
  `pty/windows.rs`）→ ConPTY 与 `CreateProcess` 后端 → Windows / macOS runner 上
  真跑测试。发布矩阵仍是 **Linux 4 个目标 + 桌面 2 个目标分两条 job**：
  `build-binaries`（`--workspace`，`afhub` 仍 Linux-only）与 `build-desktop`
  （只 `-p node-cli`）。**不要把桌面条目塞进 `build-binaries` 的矩阵**——那条
  job 的自包含断言用 `readelf` / `ldd` / `file`，桌面平台上都没有。
  桌面产物的目标：`x86_64-pc-windows-msvc`（`windows-latest` 自带 MSVC 工具链）
  与 `aarch64-apple-darwin`（对齐 REX 用 `macos-latest`；固定 `macos-14` 曾让
  runner 配置直接被拒，Intel runner 也在退役时间表上，**所以 macOS 目前只发 arm64**）。
  `build-desktop` 先产出 release 依赖图与二进制，`test-desktop` 等它、
  复用同 target 的 `shared-key: desktop-<target>` 且同跑 `--release`——
  桌面真机测试保留，但不再另编一套 debug 依赖图。
- **`test-desktop` 是本里程碑唯一的真实验收手段**：`cargo check --target
  x86_64-pc-windows-msvc` 只证明编得过，证明不了读得到输出。ConPTY /
  `CreateProcessW` / 句柄继承 / 读流时序只能在真 Windows 上跑；macOS 的
  `pipe_nonblocking_cloexec` / `macos_aware_execvpe` / `platform_forkpty`
  三处分支在 Linux 上走不到。
- **「静态链 OpenSSL」在本仓不适用，但断言照抄**：`Cargo.toml` 的 `reqwest` 是
  `rustls-tls`（`default-features = false`），整棵树**不链系统 OpenSSL**。所以真正要
  静态的是 libc，用 **musl 目标**解决。REX 那条「产物不依赖 `libssl|libcrypto`」的
  自包含断言仍然照抄，判据按平台分开：musl 用 `ldd` 判「not a dynamic executable」
  且**查 `readelf` 无 `interpreter`、`file` 不报 `pie executable`**（PIE 产物 `ldd` 也
  报静态，却运行即段错误）；v0.7.3 起矩阵只有 musl 两条，非 musl 目标在断言里直接 fail。
  **断言在 workflow 里，不在文档里**。
- **节点只从 Hub 拉二进制，不直连 GitHub**（REX 有 GitHub 回退，我们刻意不做，
  见 `architecture/security-and-lifecycle.md` §7.1.4）。
- 镜像推到 `ghcr.io/<owner>/agentfleet-hub` 与 `agentfleet-node`。只有正式 tag 才更新
  `latest`（tag 名里带 `-` 的预发版本不会顶掉它）。
- **Hub 容器必须显式放开回环限制**：`afhub` 缺省只绑回环（约束 1）。容器网络是隔离的，
  所以 `Dockerfile.hub` 设 `AFHUB_ALLOW_NON_LOOPBACK=1` + `AFHUB_ADDR=0.0.0.0:8080`。
  开关**只认确切的 `1`**（`0` / `true` / 空串一律当没设），理由见
  `crates/hub-server/src/main.rs` 的 `resolve_addr`。裸机部署不变：仍然只许回环。

---

## 2.2 已知行为差异（v0.7.2：设备侧 `afcli` 的三平台）

> 这一节记的是**刻意的**差异，不是待修的缺陷。判据是「平台本身没有对应物」或
> 「对应物的语义对不上」；任何一处若只是「还没做」，不许写在这里，要进 Bugs 表。

#### macOS（Unix，但有三处与 Linux 不同）

| 差异 | 为什么必须分开 | 代码位置 |
|------|---------------|----------|
| `execvpe` 不存在 | glibc 有 `execvpe`，macOS 只有 `execvp` + 手动拼 `PATH`。直接用 `execvpe` 链接期就找不到符号。 | `pty/unix.rs` 的 `macos_aware_execvpe` |
| `pipe2(O_CLOEXEC)` 不存在 | macOS 没有 `pipe2`，要 `pipe` 之后 `fcntl(F_SETFD, FD_CLOEXEC)`。中间那一刻 fd 是**可继承**的，而 fork 就发生在旁边——所以两步之间不能有 fork。 | `pty/unix.rs` 的 `pipe_nonblocking_cloexec` |
| `forkpty` 在 `util.h` 里，不在 `libutil` 的常规导出处 | Linux 直接链 `libutil` 就有；macOS 上要按它的声明方式取。 | `pty/unix.rs` 的 `platform_forkpty` |

macOS 上 **POSIX 权限位照常生效**（`0o600` / `0o700` 与 Linux 同义），
所以 `identity.rs` 的权限加固在 macOS 上不需要分支。

#### Windows（不是 Unix，差异是结构性的）

| 差异 | Unix 行为 | Windows 行为 | 影响与处置 |
|------|-----------|--------------|-----------|
| **进程模型** | `fork` + 子进程里改 fd + `exec` | **没有 `fork`**。`CreateProcessW` 一次成型，管道用 `SECURITY_ATTRIBUTES` 声明可继承 | `connector/exec_windows.rs`。`run()` 只有一条路径，平台细节在 `Backend` 里 |
| **PTY** | `forkpty` + `ioctl(TIOCSWINSZ)` | ConPTY：`CreatePseudoConsole` / `ResizePseudoConsole` | `pty/windows.rs`。尺寸语义一致（都是「告诉终端多大」） |
| **关面板 = 关 shell** | master 关掉 → 子进程收 `SIGHUP` | `ClosePseudoConsole` → 子进程收 **`CTRL_CLOSE_EVENT`** | 效果对齐（进程都会退出），但**信号不同**：依赖 `SIGHUP` 做清理的脚本在 Windows 上不会收到它 |
| **`Ctrl-C`** | `SIGINT` | 控制台事件 **`CTRL_C_EVENT`**；若走 `GenerateConsoleCtrlEvent` 发给进程组则需要进程组 | 见下一行 |
| **进程组** | `setsid` / `setpgid` / `kill(-pgid)` | **没有进程组**。只能按 PID 或按「同控制台」投递事件 | `Pty::pid()` 返回的仍是直接子进程 PID，语义与 `forkpty` 返回值相同；但「杀整个作业树」没有等价物。**这是能力缺失，不是 bug** |
| **文件权限** | `0o600` / `0o700` | **没有 POSIX 权限位**，走 ACL 继承 | `identity.rs` 的 `IDENTITY_MODE` / `DATA_DIR_MODE` 按 `#[cfg(unix)]` 门禁。Windows 上靠 `%LOCALAPPDATA%` 的默认 ACL（只当前用户与 SYSTEM 可读）。**不提供等价的显式收紧**——这是已知差异，不要假装有 |
| **路径语义**（用户 2026-11-06 裁定：**Windows 区分大小写**） | `/` 分隔；`Path` 逐码元比较 | `\` / `/` 都收；盘符 / UNC 归 `Component::Prefix`，在 `resolve_within` 里**一律拒**（工作区路径只认相对写法）；路径按**区分大小写**处理 | `connector/files.rs:103-144` 把段名逐段存成 `OsString`，`real.starts_with(root)` 也按 `OsString` 前缀比，路径判据里没有大小写折叠（grep 实测 `eq_ignore_ascii_case` / `to_lowercase` / `to_uppercase` 为空；`git.rs:717,767` 的 `to_lowercase()` 只用于匹配 git 报错文本，与路径无关）——`C:\Work` 与 `c:\work` 不归一，用户写哪种就按哪种走。本仓**不折叠大小写、也不依赖文件系统缺省行为做边界判定**。`MAX_PATH` / 长路径前缀 / 保留设备名（`CON`、`NUL`）一律按普通段名处理，不 special-case |
| **argv 拼装** | 直接传数组 | 一条**字符串**命令行，由子进程按 MSVC 规则自行解析 | `win_command.rs`（crate 根，非 `connector/` 子模块），规则与 Rust `Quote::Auto` 逐字节一致 |
| **符号链接** | `symlink` 任意可建 | 需开发者模式或管理员权限 | 生产代码跨平台（走 `symlink_metadata`），但相关**测试**限 Unix——造不出前提 |
| **FIFO / Unix 域套接字** | 有 | 没有 FIFO；`AF_UNIX` 语义与 `stat`/`open` 的交互不同 | `files.rs` 对应测试限 Unix；生产代码不依赖这两种类型 |
| **纪元前时间戳** | 可表示（负 `tv_sec`） | NTFS 用 1601 起的计数，**没有「纪元前」这个状态** | 该分支在 Windows 上结构上不可达 |
| **管道可读性判定** | 读端带 `O_NONBLOCK`，`read` 没数据立刻 `EAGAIN` | **没有非阻塞模式，且匿名管道句柄不是可等待对象**。`ReadFile` 没数据就阻塞，所以读之前必须用 `PeekNamedPipe` 查 `lpTotalBytesAvail`，有字节才读 | `exec_windows.rs` 的 `wait_handle_readable` + `read_into`、`pty/windows.rs` 的 `wait_readable`。v0.7.2 曾误用 `WaitForSingleObject` 判管道：`WAIT_FAILED` 在 exec 侧恒判「不可读」（6 条测试输出全空）、在 pty 侧恒判「可读」（`ReadFile` 永久阻塞，`test-desktop` 挂 53m51s），v0.7.3 修掉 |
| **杀进程树** | `kill(-pid, SIGKILL)` 带走整个进程组 | `TerminateProcess` **只杀直接子进程**，孙进程（`cmd /c start …`）继续活着并持有管道写端 | 因此 `run()` 的收尾 drain **必须**有 `DRAIN_GRACE` 上限（两平台共用）。捞不到的残留输出会少一截，并留一条 `warn!` 日志——这是「超时后放弃 drain」的唯一线索 |
| **PTY 输出端的 EOF 错误码** | `EIO` 归到「读完了」 | `ERROR_BROKEN_PIPE`；**部分情形下是 `ERROR_PIPE_NOT_CONNECTED`**（ConPTY 被 `ClosePseudoConsole` 带走时） | `pty/windows.rs` 的 `read_handle` 把两者都归到 EOF。判错的后果是「用户每敲一次 `exit` 都看到一句 IO 错误」 |
| **PTY 读线程的唤醒** | `poll` 同时监视 fd 的 `POLLIN`/`POLLHUP` | 用 `PeekNamedPipe` 探测输出读端（结构体不持有进程句柄，也不等它） | shell 退出但输出管道未 EOF 时，靠 100 ms 轮询回到循环头检查 `stop` 标志收场；写端已关时 `PeekNamedPipe` 自己就返回 FALSE + `BROKEN_PIPE` / `PIPE_NOT_CONNECTED`，`wait_readable` 必须把这两个错误码判成**可读**（而不是「暂时没字节」），否则 `ReadFile` 永远等不到 EOF、`any_open()` 不收敛。收场条件与 Unix 一致，唤醒机制不同 |
| **子进程的标准句柄** | `forkpty` 直接把 0/1/2 接到 slave 上 | ConPTY 只把 `HPCON` 挂进属性表；`STARTUPINFO` 必须带 **`STARTF_USESTDHANDLES`** 且三个 `hStd*` 保持 NULL | 不带这个标志时 `CreateProcessW` 会把父进程的 stdout/stderr 复制给子进程：`cmd /c echo` 的输出直接进了 runner 日志（v0.7.2 的 `CONPTY-MARKER` 就出现在 libtest 输出行尾），ConPTY 的 `output.read` 读不到任何字节。见 `pty/windows.rs` 的 `spawn` |
| **测试覆盖不对等** | Unix 侧 15 条 PTY 用例 + 29 条 exec 用例，断言细粒度语义 | Windows 侧另有 6 条 PTY + 7 条 exec 用例，覆盖「能起 ConPTY 并读到输出 / close 后拒绝再用 / resize / 超时 137 / 程序不存在报文件找不到 / stdin 是 EOF / 带空格参数」，但**没有**逐条对齐 Unix 的语义断言 | `pty/windows.rs` / `exec_windows.rs` 的 `mod tests`。差距如实记在里程碑报告的「遗留风险」里，不假装等价 |

**Windows 上目前不做的事**（都是能力缺失，不是缺陷）：

1. **没有进程组**，所以没有「一次杀掉整个作业树」。设备侧只用它管自己拉起的
   worker 与终端面板，按 PID 杀够用。
2. **不提供 `0o600` 的等价物**：文件权限交给 Windows 的 ACL 继承。
3. **`afhub` 仍 Linux-only**：v0.7.2 只移植设备侧 `afcli`。Hub 的部署形态是
   Linux 容器，没有桌面需求。

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
| v0.7.1 ✅ 已完成 | **CI 镜像构建（发布通路）** | 从 v0.14.0 提前而来（用户 2026-11-05 裁定）。本地 `mise run ci` 全绿后打 tag `v0.7.1` 触发 CI，**CI 首验暴露出两个真缺陷**：门禁红时镜像照样发出去（两个 workflow 只共享触发、互不阻塞）、`windows-gnu` 条目编不出来（`node-connector` 的 `fork`/`exec`/PTY 模型是 Unix-only 实现，移植需先抽平台抽象层）。**前者最终以「删掉 `ci.yml`、门禁并入产线」处置**：`release.yml` 的 `gate` job 跑齐 `mise.toml [tasks.ci]` 的全部 7 项，所有产线 job 都 `needs: gate`，清单只剩一个真相源（C7 双向比对）；**后者按「本里程碑不做移植」移除目标**，排进 v0.7.2。4 个 Linux 目标均已真编通过（`aarch64-*` 由 CI 的 arm runner 原生真编，11 步全 success）。把 §2 的「发布」那一整段真正建起来：`gate` → `build-frontend`（bun `--frozen-lockfile` → build，dist 作为 artifact）**硬前置**于 `build-binaries`（4 目标**原生**矩阵：x86_64 / aarch64 × gnu / musl；windows-gnu 已移除，**v0.7.1 不做移植，不是永远不支持**——见同节「v0.7.1 不提供 Windows 目标」），再分发到三个消费者：`docker-hub`（`afhub` + `afcli`，前端**已在编译期嵌入**，镜像里不另放 dist）、`docker-node`（`afcli`，按 `TARGETARCH` 选二进制）、`release`（GitHub Release + `SHA256SUMS` + `CHANGELOG.md` + 从 CHANGELOG 抽 body，**附件只发 musl 干净名**且稳定版带 `--latest`）。镜像推 `ghcr.io/<owner>/agentfleet-hub` / `agentfleet-node`。**`build-frontend` 之所以是硬前置**：dist 缺失时 `build.rs` 只造占位页并打 warning，二进制照编——「Hub 打开是占位页」能不红任何一步地发出去，所以每个平台编之前都断言 `dist/index.html` 存在。**安全边界变更**：`afhub` 缺省仍只绑回环，新增 `AFHUB_ALLOW_NON_LOOPBACK=1` 供容器显式放开，**只认确切的 `1`**。设备侧安装多了从 Hub 自己下载 `afcli` 的入口（`GET /api/agent-binary` + 设备页按钮），内网/离线部署不必够 GitHub。版本号 `0.5.0` → `0.7.1`（tag 与二进制自报版本必须一致） | —（`DEVELOPMENT.md` §2.1 发布段） |
| v0.7.2 ✅ 已完成 | **桌面平台支持（设备侧 afcli：Windows + macOS）** | **用户 2026-11-05 裁定**：插一个 v0.7.2 专做设备侧桌面平台移植（Windows + macOS，mac 为用户 2026-11-05 追加裁定），`v0.8.0` 骨架不动、顺延一位。**只做设备侧 `afcli`**，`afhub` 仍只发 Linux（自托管 Hub 一般跑在服务器/容器，Windows 不是主场景）。**改动面已核对**：`afcli` 的生产依赖链是 `node-cli` → `node-core` → `node-connector`，**不经过 `hub-storage`**；`libc` 附着点是 `node-connector` 的 `pty.rs`(27) / `exec.rs`(45) / `terminal.rs`(5) / `files.rs`(4) 与 `node-core/worker.rs`(5，疑为测试夹具)。**核心难点**：`exec.rs` 的「`fork` → 改 fd → `execvpe`」在 Windows 上没有对应物（**Windows 没有 `fork`**），要整体换成 `CreateProcess` + 句柄重定向；会话终端从 `forkpty`/`ioctl(TIOCSWINSZ)` 换成 ConPTY（`CreatePseudoConsole` / `ResizePseudoConsole`）。**不是「加个矩阵条目」的量级**——先抽平台无关的 PTY/进程后端，再写两个实现，且 Windows 分支在 Linux 上测不了，得要 Windows runner 真跑。CI 目标：`x86_64-pc-windows-msvc`（msvc 工具链）与 `aarch64-apple-darwin`（对齐 REX 用 `macos-latest`，Apple Silicon；**Intel runner 在退役时间表上，故 macOS 只发 arm64**）。macOS 是 Unix 系、移植量小，安排在 Windows 之前先做以验证抽象层。见 `.mdflow/milestones/v0.7.2-desktop-platforms.md`。**缺陷池 bug：2 个**（🟡 secret 加固、🟢 工作区同步兜底，planner 2026-11-05 原样搬入 Bugs 表；二者均为**未来补强项**、本期不做，已在 Bugs 表如实保留） | —（承接 v0.7.1 推迟的 `windows-gnu`） |
| v0.7.3 🔄 当前 | **覆盖率补强 + 恢复覆盖率硬门禁（+ 修 Windows 真机测试挂死）** | **版本类型 `patch`**：不加功能，把 v0.7.2 留下的两笔债还掉。①**补测试到 98%**：全量 `cov-gate` 实跑 `TOTAL ... 98.02%`（641 / 32348 行未覆盖），退出码 0；②**恢复硬门禁**：删除 `release.yml` 的 `cov-gate` step 级临时豁免并同步 `AGENTS.md` / `DEVELOPMENT.md` / `CHANGELOG.md`，`scripts/check.sh` 加 C11 护栏禁止该豁免字段回流；门槛、命令文本、下游 `needs` 一律不动；③**修 Windows 真机测试挂死**：`exec_windows` / `pty/windows` 改用 `PeekNamedPipe` 判匿名管道可读性，ConPTY 子进程保持标准句柄接法，给 `test-desktop` 加 `timeout-minutes` 兜底并收口 4 条 Unix-only 假 git 用例；最终判据仍是远端 `test-desktop (x86_64-pc-windows-msvc)` 的 `a_conpty_runs_a_command_and_we_read_its_output` 为 `ok`。**发布方式**：用户 2026-11-07 裁定发 `v0.7.3`，不 force-update 已发布的 `v0.7.2` tag；期间 v0.7.2 的 Release 为空、镜像已推但无 Release 附件 | —（工程补强，无新增 `PRODUCT.md` §6.1 能力项） |
| v0.7.4 | **会话导入（外部 agent 历史会话 → 设备侧 CLI 导入 → 可继续对话）** | **用户 2026-11-05 裁定立项，2026-11-06 顺延**（原 `v0.7.3`，因插入覆盖率补强里程碑而后移一位）。方案已调研（2026-10-27 实测）：导入=**绑定非搬运**——Hub 建 session 预填 `acp_session_id`，设备走 `session/load` 让 agent 自己找回历史（实测跨进程记得）。L0+L1 只支持 opencode（唯一有 ACP 入口的 agent，`KNOWN_AGENTS` 表中唯一有 `acp_args`）；opencode 会话存于 `~/.local/share/opencode/opencode.db` 的 `session_v2` 表。改动面：1 个 Hub 端点 + 1 个 CLI 子命令 + 只读探测 DB；与 v0.7.2 无依赖。里程碑文档开工时再派生 | —（用户新需求） |
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