# Changelog / 变更日志

本文件遵循 [Keep a Changelog](https://keepachangelog.com/) 格式，条目为中英双语。
**每条只写用户能感知的功能变化**：中文一句 + ` / ` + 英文一句，不超过 2 行。
实现论证、实测数据、权衡过程归 `docs/DEVELOPMENT.md` 与里程碑报告，不写进本文件。
Entries follow Keep a Changelog and are bilingual (中文 / English). **One user-visible
change per entry**, one sentence per language, at most two lines. Rationale, measurements
and trade-offs belong in `docs/DEVELOPMENT.md` or milestone reports.

版本段按里程碑补写，日期取该里程碑收尾提交的作者日期；v0.1.0–v0.5.0 于 2026-11-05
按各里程碑文档的 `里程碑起始 ref` 回溯归位过一次。Sections were reconstructed per
milestone and dated by that milestone's closing commit; v0.1.0–v0.5.0 were reorganised
once on 2026-11-05.

## [Unreleased]

### Added

- **新增 `afcli` 安装脚本**，从 Release 校验并下载可直接执行的静态二进制。 / **Added an `afcli` installer** that checksums and installs the directly executable static binary from Releases.
- **CI 只构建 musl 二进制**，不再重复编译无消费方的 gnu 产物。 / **CI now builds only musl binaries**, dropping unused gnu builds that duplicated release work.

## [0.7.2] - 2026-11-06

> 里程碑：设备侧 `afcli` 移植到 Windows 与 macOS（`afhub` 仍 Linux-only）。
> Milestone: device-side `afcli` on Windows and macOS; `afhub` stays Linux-only.

### Added

- **设备侧 `afcli` 新增 Windows 与 macOS 支持**，发布产物为 `afcli-windows-amd64.exe` 与 `afcli-macos-arm64`，Hub 镜像内嵌两者供设备下载。 / **The device-side `afcli` now runs on Windows and macOS**, shipping `afcli-windows-amd64.exe` and `afcli-macos-arm64` embedded in the Hub image.
- **CI 新增 Windows / macOS 真机测试与桌面产物构建，发布流程复用已编译依赖，避免重复编译**。 / **CI gained native Windows/macOS test runners and desktop artifacts, reusing compiled dependencies to avoid repeated builds.**
- **`docs/DEVELOPMENT.md` 新增「已知行为差异」一节**，记录三平台刻意保留的行为差异。 / **`docs/DEVELOPMENT.md` gained a known-behaviour-differences section for the three platforms.**

### Changed

- **覆盖率门禁改为行覆盖率 ≥98%**（`cov-gate`），逐行例外登记机制移除。 / **The coverage gate is now ≥98% line coverage**, replacing per-line exception bookkeeping.

### Fixed

- **修复 `afcli` 在 macOS 上编不过**（`execvpe` / `pipe2` / `forkpty` 三处平台差异）。 / **Fixed macOS build failures** in `execvpe`, `pipe2` and `forkpty`.
- **修复 Windows 下空命令行参数丢失与引号未转义**。 / **Fixed dropped empty arguments and unescaped quotes in Windows command lines.**
- **补上 `supervisor` spawn 失败分支的确定性单测**。 / **Added a deterministic test for the supervisor's spawn-failure branch.**

## [0.7.1] - 2026-11-05

### Added

- **设备可以从 Hub 自己拉 `afcli`**（`GET /api/agent-binary` + 设备页下载按钮），内网部署与版本对齐不再依赖 GitHub Release。 / **Devices can now fetch `afcli` from the Hub itself** (`GET /api/agent-binary` plus a download button), so air-gapped deployments and version alignment no longer depend on GitHub Releases.
- **打通发布通路：打 tag 即产出二进制、镜像与 GitHub Release**。 / **Tagging now produces binaries, images and a GitHub Release.**
- **新增 `AFHUB_ALLOW_NON_LOOPBACK=1`**：容器唯一被允许的放开绑定地址方式，缺省仍只绑回环。 / **Added `AFHUB_ALLOW_NON_LOOPBACK=1`**, the only sanctioned way to open the bind address for containers; loopback remains the default.
- **新增 Hub / node 镜像与 compose 文件**：Hub 镜像不另放 dist，node 镜像按 `TARGETARCH` 选二进制。 / **Added Hub/node images and compose files**; the Hub carries no separate `dist` and the node image picks its binary by `TARGETARCH`.
- **Release 附件固定为 4 个 musl 静态二进制 + `SHA256SUMS`**，body 从本文件对应版本段抽取。 / **Release assets are four musl static binaries plus `SHA256SUMS`**, with the body extracted from this file's matching section.

### Changed

- **GitHub Actions 升级到 Node 24 起步的最低版本**（`checkout@v5` / `setup-node@v5` / `upload-artifact@v6` / `download-artifact@v6`）。 / **GitHub Actions upgraded to the first Node 24 runtime versions.**
- **门禁拆成 4 个并行 job**，产线墙钟 14m06s → 7m09s。 / **The gate was split into four parallel jobs**, cutting pipeline wall time from 14m06s to 7m09s.
- **删掉 `ci.yml`**，门禁清单收敛到 `mise.toml` 一处。 / **Removed `ci.yml`**; the gate list now has one source of truth in `mise.toml`.
- **前端三道门禁合并成一步 `web-ci`**。 / **The three frontend gates were merged into one `web-ci` step.**
- **版本号从 `0.5.0` 提到 `0.7.1`**（`Cargo.toml` 与 `package.json` 同步）。 / **Version bumped from `0.5.0` to `0.7.1`** in `Cargo.toml` and `package.json`.
- **`docs/DEVELOPMENT.md` 的 `v0.14.0` 行划掉并注明迁至 v0.7.1**。 / **The `v0.14.0` row in `docs/DEVELOPMENT.md` is struck through and marked as moved to v0.7.1.**

### Fixed

- **修复 Hub 镜像部署后前端白屏**（`/assets/*` 全 404，handler 少补一个目录段）。 / **Fixed a blank Hub frontend after deployment** (`/assets/*` all 404 due to a missing path prefix).
- **修复 Release 的 Latest 断言用了 gh CLI 里不存在的字段**，改为查 REST 的两个事实。 / **Fixed the Release "Latest" assertion**, which used a nonexistent `gh` field; it now checks two REST facts.
- **修复覆盖率门禁的 `(racy)` 豁免机制从未真正生效**。 / **Fixed the coverage gate's `(racy)` exemption**, which had never actually taken effect.
- **修复 CI 上 `fmt` 一步直接红而本地全绿**（minimal profile 未装 rustfmt / clippy）。 / **Fixed `fmt` failing on CI while passing locally** (missing rustfmt/clippy components).
- **产线自己等门禁**：下游 job 全部 `needs: gate`，门禁红时不再发镜像与 Release。 / **The pipeline now waits for its own gate**: every downstream job needs it, so a red gate ships nothing.
- **移除 `windows-gnu` 矩阵条目**：v0.7.1 不做 Windows 移植，排进 v0.7.2。 / **Dropped the `windows-gnu` target**; Windows porting was deferred to v0.7.2.

## [0.7.0] - 2026-11-05

> 里程碑：**项目与绑定**。产品主轴从「会话」改为「项目」，项目成为真实体。
> Milestone: **projects and bindings** — the product axis moves from sessions to projects.

### Added

- **项目三种形态：中心式 / 镜像式 / 隔离式**，`shape` 决定代码在哪，只允许 `isolated → central` 单向升级。 / **Three project shapes** (central / mirror / isolated) decide where code lives, with only `isolated → central` upgrades allowed.
- **项目归档与取消归档，且不提供删除**。 / **Project archive and unarchive, with no delete route.**
- **Binding：`(project, device) → 固定目录`**，带磁盘配额与清理策略。 / **Bindings** map `(project, device)` to a fixed directory with a quota and cleanup policy.
- **设备侧 `worktree` connector**：每个任务一棵工作树。 / **A device-side `worktree` connector**: one worktree per task.
- **Hub 侧裸仓库**：中心式与镜像式的代码由它保管。 / **A Hub-side bare repo** holds the code for central and mirror projects.
- **数据结构按项目主轴重排（schema v5）**：新增 `project` / `task` / `execution` / `binding` 四表，会话挂到 Execution 下。 / **Schema v5** adds the project/task/execution/binding tables and nests sessions under executions.
- **前端新增 `projects` / `project` 路由与导航骨架**，项目页默认落在看板。 / **Frontend `projects`/`project` routes and navigation**, with the project page landing on the board.
- **新增绑定设备页与项目设置页**；后端还没有的能力按钮真的 `disabled` 并写明原因。 / **Added the bind page and project settings page**; unsupported actions are genuinely disabled with a reason.
- **设备页与 agent 页按项目视角扩展**：绑定关系、磁盘用量、失败归因、配额与清理策略。 / **The device and agent pages gained the project perspective**: bindings, disk usage, failure attribution, quota and cleanup.

### Changed

- **crate 从 7 个拆成 8 个**：`node-core` 拆出 `node-connector`。 / **Seven crates became eight**: `node-connector` split out of `node-core`.
- **前端按领域优先重组**（`domains/<领域>/`），`pages/` 只留三个入口页。 / **The frontend is now organised domain-first**.
- **`SessionPage.vue` 不拆**：与项目主轴无关的重构不拌进这一版。 / **`SessionPage.vue` was deliberately not split** here.
- **原型重做**：`prototype/` 21 页按项目轴重建。 / **The prototype was rebuilt** on the project axis (21 pages).
- **看板项目切换器按原型落地**（项目名本身是按钮 + 弹出菜单）。 / **The board's project switcher follows the prototype** rather than a `<select>`.

### Fixed

- **修掉 `scripts/check.sh` 的 7 项既有失败**。 / **Fixed seven pre-existing `scripts/check.sh` failures.**
- **补齐 `.af-region` / `.af-callout` 的 12 条样式规则**，此前它们渲染成裸 `<section>`。 / **Added twelve CSS rules** for region/callout blocks that had been rendering as bare `<section>` elements.
- **修正一个从未存在的 `var(--af-*)` token**，现在全仓零未定义。 / **Fixed a token that never existed**; the repo now has zero undefined `var(--af-*)`.
- **动作失败不再把页面替成「读取失败」**，用户仍可重试。 / **An action failure no longer replaces the page with a read failure.**
- **修复窄屏抽屉无参 `pop()` 关掉别人的浮层并留下僵尸层、导致整页点不动**。 / **Fixed an argument-less `pop()`** that closed someone else's layer and made the page unclickable.

## [0.6.0] - 2026-11-03

> 里程碑：**任务基准**。用户裁定「视为结束，从 v0.7.0 开始」，本版收口为只修真 bug、不再扩展。
> Milestone: **task baseline**, closed early; this version fixes real bugs only and hands its starting-point concept to v0.7.0.

### Added

- **会话工作区的「起点」**：建会话时用 git `rev` 记下 `base_commit`，Hub 只存字符串。 / **A session's starting point**: `base_commit` is recorded via a git `rev` op and stored as a string.
- **面板显示「这次会话产出了什么」**：提交几次、几个文件未提交，相对 `base_commit` 算。 / **The panel shows what the session produced** relative to its base commit.
- **起点 rev 已不存在时的处置**：不崩溃，也不假装没有改动。 / **Handling a base revision that no longer exists**, without crashing or pretending no changes.

### Fixed

- **修复折叠态侧栏长出横向滚动条**。 / **Fixed a horizontal scrollbar in the collapsed sidebar.**

### Changed

- **文档锚点、覆盖率例外与 e2e 脚本跟随文件搬家同步更新**。 / **Documentation anchors, coverage exceptions and e2e scripts followed the files to their new homes.**

## [0.5.0] - 2026-10-31

### Added

- **会话页改为 opencode 那样的单层工作界面**：进会话收起全站侧栏，会话铺满页面。 / **The session page is now a single-layer workbench**: entering a session collapses the global sidebar.
- **顶部 tab 条置顶，会话身份收进一条窄状态行**。 / **The tab bar moved to the top and session identity into a narrow status line.**
- **右侧只显示一个工具，唯一入口是头部「＋」**（裁决 C）。 / **One tool on the right, whose only entry point is the header's 「＋」.**
- **左右分栏可拖拽，按 tab 各自保留比例**。 / **A draggable split whose ratio is remembered per tab.**
- **窄屏（≤900px）降级为覆盖式抽屉**。 / **Narrow screens (≤900px) fall back to an overlay drawer.**
- **新增真机端到端 `scripts/e2e-v0.5.0.sh`**（19 条断言），验「换工具 = 替换 + 释放通道」。 / **Added a real-device e2e script** (19 assertions) for tool replacement and channel release.

### Changed

- **键位文案如实反映「切走会杀 PTY」**。 / **Key hints now state that switching tools kills the PTY.**
- **`design-system.md` / `DECISIONS.md` 同步**：记录裁决 C 的落地形态与坞标签条删除。 / **`design-system.md` / `DECISIONS.md` updated** for the decision and the deleted dock tab strip.

### Fixed

- **修复窄屏抽屉无参 `pop()` 导致整页点不动**。 / **Fixed an argument-less `pop()`** that made the page unclickable.
- **修复覆盖率门禁两条 stale exception 背后的恒真断言**，让测试真的覆盖实时流。 / **Fixed a vacuous assertion behind two stale coverage exceptions**, so the test really covers the live stream.
- **「全仓 900px 断点分布」用例改为递归扫 `src/` 下全部 `.vue`**，能发现新增文件。 / **The 900px-breakpoint test now recursively scans all `.vue` files** so new files are caught.
- **修掉 12 条代码精简与测试有效性缺陷**（含补上 `cyclePanel` 缺失的方向断言）。 / **Fixed twelve simplification and test-effectiveness defects**, including a missing direction assertion.

## [0.4.0] - 2026-10-30

### Added

- **会话内新增只读 git 面板**：`status` / `diff` / `log` 三个子集，经 git 子进程读工作区。 / **A read-only git panel** with `status` / `diff` / `log` via a git subprocess.
- **新增 `git log` 视图与面板级真机 e2e**。 / **Added a `git log` view and a real-device e2e for the panel.**
- **明确不做 `commit` 及其它写操作**：本版目标只是「看它改了什么」。 / **No `commit` or any other write op**: the goal here was seeing what changed.

### Changed

- **面板间切换是视图切换**（DECISIONS D10），切 tab 不释放通道。 / **Switching tabs is a view switch** (DECISIONS D10) and does not release channels.

### Fixed

- **修掉第 5 轮精简与代码审查的发现**（含 `diff` 视图形状只声明一次）。 / **Round-5 simplify and review findings across the git panel.**
- **修掉一批只有整仓一起跑时才红的测试夹具与过期断言**。 / **Test fixtures and stale assertions that only failed when the whole workspace ran together.**

## [0.3.0] - 2026-10-28

### Added

- **`session/load`：对话关掉再回来，agent 还记得之前聊过什么**；load 失败不静默回落成 `new`。 / **`session/load` reopens a conversation with the agent's context intact**; a failed load errors instead of silently falling back to `new`.
- **删设备**：新增 `DELETE /api/devices/{id}` 与行内确认框，按「删除语义三分离」保留会话与轨迹。 / **Device deletion** via `DELETE /api/devices/{id}` with an inline confirm, keeping sessions and trajectories.
- **设备表 CPU / 内存两列真采集负载**，采不到留 `null` 不回落成 0。 / **Device CPU/memory columns now really collect load**, leaving `null` when unavailable instead of 0.
- **新增 `deploy/afcli.service`**，把 `afcli` 跑成 systemd 服务。 / **Added `deploy/afcli.service`** to run `afcli` as a systemd service.
- **新增登录页与首次 setup 页**。 / **Added the login and first-run setup pages.**
- **新增 `GET /api/setup` 探针**，把 `/setup` 让给页面。 / **Added a `GET /api/setup` probe** so `/setup` serves the page.
- **设备探测与 agent 启动配置**：只扫描已装的 agent，Hub 存配置不代装。 / **Agent detection and launch configuration**: the device scans what is installed; the Hub stores config and never installs.

### Fixed

- **真设备 E2E 抓到四处「看起来正常、其实丢了」的缺陷并全部修在源头**（`seq` 跨进程重数、重放事件重复落库、状态快照去重、上报未安装的 agent）。 / **A real-device E2E caught four silent data-loss defects**, all fixed at the source (`seq` restart, replay duplication, snapshot dedup, phantom agents).
- **修复 node 上行 `channel_data` 缺 `channel_id` 信封**，此前双向数据面从未通过。 / **Fixed the missing `channel_id` envelope in node uplink `channel_data`**, which had broken the data plane.
- **修复 `GET /login` 在初始化模式下被 403**。 / **Fixed `GET /login` returning 403 in init mode.**
- **文案不再引用不存在的 `afhub reset-setup`**，统一改为「重启 Hub」。 / **Copy no longer references a nonexistent `afhub reset-setup`**; it says "restart the Hub".
- **闭环 M1 全量缺陷 156 条**（22 🔴 / 101 🟡 / 33 🟢），逐条附文件与行号实证。 / **Closed all 156 M1 defects** (22 critical / 101 major / 33 minor), each evidenced by file and line.

### Changed

- **文档不再承诺「Hub 下发安装 agent」**（共 6 处，代码里从未实现过）。 / **The docs no longer promise a Hub-driven agent install** — six places, none ever implemented.
- **品牌区与侧栏合并成一列**，折叠态保留原图标轨形态。 / **The brand block and sidebar are now one column**, keeping the collapsed icon-rail look.
- **侧栏从 5 项收敛成 3 项**（会话 / 设备 / 设置），Agent 目录并进设备页。 / **The sidebar went from five items to three**, with the agent catalogue merged into the devices page.
- **`mise` 分出开发与生产两条路**（`dev` / `build` / `start`）。 / **`mise` now separates development from production** (`dev` / `build` / `start`).
- **数据目录默认改为项目级 `data/`**，不落 `~/.local/share/`。 / **The data directory now defaults to a project-local `data/`**.
- **CI 改为只在推 tag 时跑**。 / **CI now runs on tags only.**
- **覆盖率门禁不再吞自己的诊断**，并新增 `mise run cov-gate`。 / **The coverage gate no longer swallows its own diagnostics**, and `mise run cov-gate` was added.
- **核实而非照抄缺陷池**：M1 的 8 条 bug 逐条 grep 实证后才勾掉。 / **Bugs were verified, not rubber-stamped**: all eight M1 defects were grep-proven before closure.

## [0.2.0] - 2026-10-22

### Added

- **M1 会话容器与首个演示里程碑完成**：页面建会话 → 开终端敲 `echo` 看到输出 → resize → 文件面板 → exec 拿退出码，全程零入站端口。 / **M1 session-container milestone completed**: sessions, terminals, resize, files and exec verified end to end with zero inbound ports.
- **页面侧数据面**：`GET /api/stream`（SSE 下行）+ 四个上行端点，CSRF 只落在上行。 / **A page-side data plane**: an SSE downlink plus four uplink endpoints, with CSRF on the uplinks only.
- **工作区 `(node_id, path)` 落库**，会话创建即挂载；exec connector 只经会话通道触发。 / **Workspaces** are persisted and mounted on session creation; the exec connector is reachable only through a session channel.
- **connector 只面向 Web 页面用户，不向 agent 暴露**。 / **Connectors serve the web UI only, never agents.**

## [0.1.0] - 2026-10-17

### Added

- **M0 设备接入里程碑完成并通过验收**：真 `afhub` + 真 `afcli` 端到端 8 条产品承诺全数验证。 / **M0 device-enrollment milestone completed and accepted**: all eight product promises verified end to end.
- **设备接入与身份**：`afcli run` 一条命令、密钥对本地生成且 Hub 只存指纹、单条 WS 主动外联、`afcli log`。 / **Device enrollment and identity**: one `afcli run` command, a local key pair with only a fingerprint on the Hub, one outbound WebSocket, and `afcli log`.
- **supervisor/worker 双角色**：升级只换 worker、身份不随进程变，回滚由未被替换的 supervisor 执行。 / **Supervisor/worker roles**: upgrades replace the worker only, so identity survives the process and rollback runs from the supervisor.
- **心跳与能力上报**：缺省字段保留上一次的值，只报配置的工作区根。 / **Heartbeats**: absent fields keep their previous value, and only configured workspace roots are reported.
- **Hub 侧离线判定**：连续心跳丢失 > 45s 标 offline，断开时不清内存上报。 / **Hub-side offline detection** after 45s without heartbeats, keeping the in-memory report on disconnect.
- **防重复注册与连接顶替**：同指纹新连接顶掉旧的，旧连接收到 `superseded` 后不再重连。 / **Duplicate enrollment & connection supersession**: a new connection displaces the old one, which then stops reconnecting.
- **通道与流控**：`(node_id, channel_id)` 分桶复用、每通道独立信用，Hub 侧读侧按字节令牌桶限速。 / **Channels & flow control**: per-channel credit and a hub-side byte-metered read token bucket.
- **Web 控制台设计语言与原型**（原 M1 设计语言里程碑已放弃，产出并入实现）。 / **Web console design language and prototypes** (kept from the abandoned design milestone).
- **单用户登录**：setup token、Argon2id、会话 cookie。 / **Single-user login** with a setup token, Argon2id and a session cookie.

### Changed

- **通道帧类型列表收敛到 `channels.rs` 一处**。 / **The channel frame type list now lives in one place**, `channels.rs`.
- **通道错误帧统一走 `push` helper**。 / **Channel error frames go through one `push` helper.**
- **`error_frame` 的五个同值分支折成常量**。 / **Five identical `error_frame` arms collapsed into a constant.**
- **`now_rfc3339` 正名为 `now_stamp`**（输出一直是 `unix:<秒>`）。 / **Renamed `now_rfc3339` to `now_stamp`**, since the output was always `unix:<secs>`.
- **`mise` 分出开发与生产两条路**，数据目录默认改为项目级 `data/`。 / **`mise` separates development from production**, with a project-local `data/`.
- **架构决策：任务分发调度**——能力 / 负载 / 存量三分，硬过滤 → 有序打分 → 可钉死。 / **Architecture decision: task scheduling** — capability/load/inventory, hard filters then ordered ranking.
- **架构决策：agent 分发**——Hub 只下发配置，不向设备写可执行文件。 / **Architecture decision: agent distribution** — the Hub ships configuration only.
- **架构决策：安全与生命周期**——凭据三类分层、supervisor/worker 两层、掉线指数退避。 / **Architecture decision: security & lifecycle** — three credential classes, a two-tier device model, backoff reconnect.
- **架构决策：工作区供料与同步**——Hub 内容寻址仓库统一供料。 / **Architecture decision: workspace feeding & sync** — one Hub content-addressed store.
- **终端 / 文件 / git 改为会话内面板**，绑定会话工作区 `(node_id, path)`。 / **Terminal, files and git are now session-scoped panels** bound to the session workspace.
- **首个演示（M1）改为**「在树莓派上开会话，会话内 shell 与文件浏览可用」。 / **First demo (M1)** is now "open a session on a Raspberry Pi and use the shell and file browser".
- **前端定选 Vue 3 + Bun + Vite**（`packages/web`），补齐质量门禁。 / **Frontend settled on Vue 3 + Bun + Vite** with its quality gates.
- **修正进程模型图**：浏览器从不直连 node，与零入站承诺对齐。 / **Fixed the process-model diagram**: the browser never connects to a node directly.
- **补齐轨迹事件表列**（类型 / 输入 / 输出 / 耗时 / 归属节点），修复断链。 / **Added the trajectory event table columns** and fixed a broken cross-reference.
- **澄清「节点级可恢复」边界**：同节点可经 `session/load` 恢复，换设备标记需重建。 / **Clarified node-level recoverability** and added reconnect / channel-rebuild rules.
- **明确不做设备间 P2P 直传**，跨节点取文件一律经 Hub 内容寻址仓库。 / **Made it explicit**: no device-to-device P2P transfer.
- **统一「第一版」口径 = 里程碑 M0–M4**，补齐 PRODUCT §3.1 范围表。 / **Aligned the "first release" definition to M0–M4** and completed the scope table.
- **agent 权限约束改为两层**（Hub 全局规则 + 会话级加严），不自研权限引擎。 / **Agent permissions are now two layers** (Hub global rules plus per-session tightening).
- **子任务工作区的内容来源在分发时由用户确认**（`HEAD` / 未提交改动 / CAS `snapshot`）。 / **Sub-task workspace source is confirmed at dispatch time** (`HEAD` / uncommitted changes / CAS snapshot).
- **Hub 不做「一键安装 agent 到新机器」**：分发只覆盖配置。 / **No one-click agent install**: distribution covers configuration only.

### Fixed

- **心跳落库不再抹掉库内公钥**，避免下一次重连永久 `identity mismatch`。 / **Heartbeats no longer wipe the stored public key**, which used to break every reconnect.

### Documentation / 文档

- **产品与技术文档基线**：定位、硬性约束、架构决策、里程碑规划。 / **Initial product & architecture docs**: positioning, constraints, decisions, milestone plan.
- **许可定为 Apache-2.0**。 / **License settled as Apache-2.0.**
- **缺陷池录入三条 TODO**：secret 加密加固、分布式推理路线、工作区同步兜底。 / **Three TODOs entered in the defect pool**: encryption hardening, inference routing, sync fallbacks.
- **README 改为门面式**：细节链向 `docs/PRODUCT.md` 与 `docs/DEVELOPMENT.md`。 / **README is now a facade** linking to the product and development docs.
- **新增里程碑 M2 工作区供料与同步**，排在 ACP 与 Git 之前。 / **New milestone M2: workspace feeding & sync**, ahead of ACP and Git.
- **记入缺陷池**：设备公钥寄存在 `capabilities.public_key`，靠 `preserve_public_key` 在心跳合并时抢救。 / **Defect-pool entry**: the device public key rides in `capabilities.public_key` and is preserved on heartbeat merges.
- **清空仓库内容，从零重建文档**。 / **Cleared the repository and rebuilt the docs from scratch.**

<!--
  版本段归属的依据（写在这里，免得下一个人重新考古）：

  | 版本  | 范围（git）              | 依据                                              |
  |-------|--------------------------|---------------------------------------------------|
  | 0.1.0 | 起始 → a960510（M1 设计）| `M0-device-enrollment.md` 起始 ref 073d010        |
  | 0.2.0 | → 903436b（M2 起始）     | `M1-web-console.md` 起始 ref e79512f              |
  | 0.3.0 | → 0a1f016（v0.4.0 起始） | `M2-acp-conversation.md` 起始 ref 903436b         |
  | 0.4.0 | → 5043daf（v0.5.0 起始） | `v0.4.0-git-panel.md` 起始 ref 0a1f016            |
  | 0.5.0 | → b12e78a（发布提交）    | `v0.5.0-opencode-layout.md` 起始 ref 5043daf      |
  | 0.6.0 | b12e78a → d3d008d        | `v0.6.0-task-baseline.md` 起始 ref b12e78a        |
  | 0.7.0 | d3d008d → HEAD           | `v0.7.0-project-and-binding.md` 起始 ref d3d008d  |
  | 0.7.1 | 发布通路                 | `v0.7.1-ci-release-pipeline.md`                   |

  日期取该里程碑收尾提交的**作者日期**（`git log -1 --format=%ad`），不是「本段写完的
  那天」——否则同一段在重写一次之后日期就变了。

  0.1.0 段里仍混着跨里程碑的条目（M1 设计语言、"connector 只面向页面"之类），因为原
  流水账没有给它们分过版本，而硬拆成几段比留在 0.1.0 更误导。这一条是已知的不精确，
  不影响任何可证伪的断言。
-->
