# Changelog / 变更日志

本文件遵循 [Keep a Changelog](https://keepachangelog.com/) 格式，条目为中英双语。
本仓在 v0.7.1 之前没有严格的「一个 tag 一个版本段」纪律：版本段是按里程碑补写的，
日期取该里程碑收尾提交的日期。**v0.1.0–v0.5.0 的条目来自一次回溯整理**——
原始 `0.5.0` 段是一份把 M0–M2 混在一起的流水账，2026-11-05 按各里程碑文档的
`里程碑起始 ref` 重新归位，未改条目内容。
Entries follow Keep a Changelog; each entry is bilingual (中文 / English).
Before v0.7.1 this repo did not keep a strict one-tag-one-section discipline: sections
were reconstructed per milestone and dated by that milestone's closing commit.
**v0.1.0–v0.5.0 were reorganised once**, on 2026-11-05 — the original `0.5.0` section was
one undivided log covering M0 through M2, and it was split back into its milestones using
each milestone document's `里程碑起始 ref`. Entry text was not rewritten.

## [Unreleased]

## [0.7.1] - 2026-11-05

### Added

- **发布通路真的建起来了：打 tag 即出二进制、镜像与 Release**。此前 `docs/DEVELOPMENT.md`
  §2 的发布段整段标着「未落地」——`ci.yml` 只管门禁，推 tag 之后没有任何东西产出用户能拿
  的东西。现在 `release.yml` 与它并列同一触发（`push: tags: ['v*']`），跑
  `build-frontend` → `build-binaries`（5 平台**原生**矩阵）→ `docker-hub` / `docker-node` /
  `release` 三个消费者。**`build-frontend` 是 `build-binaries` 的硬前置**：`include_dir!`
  在编译期嵌入 dist，dist 缺失时 `build.rs` 只造一个占位页并打 `cargo:warning`——二进制照
  样编得出来、页面却是空的，这种静默降级一旦发出去就是「Hub 打开是占位页」。所以矩阵里
  每个平台在编之前都断言 `packages/web/dist/index.html` 存在，不存在就 fail / **The release
  pipeline now exists: tagging produces binaries, images, and a Release**. Until now the
  release section of `docs/DEVELOPMENT.md` §2 was marked "not implemented" — `ci.yml` only
  gated, and pushing a tag produced nothing a user could run. `release.yml` now sits beside
  it on the same trigger and runs `build-frontend` → `build-binaries` (a five-target **native**
  matrix) → the three consumers `docker-hub` / `docker-node` / `release`. **The frontend job is
  a hard prerequisite of the binaries**: `include_dir!` embeds `dist` at compile time, and when
  it is missing `build.rs` writes a placeholder page and only emits a `cargo:warning` — the
  binary builds fine and the page is blank, and shipping that silently means the Hub opens on
  a placeholder. Every matrix entry therefore asserts `packages/web/dist/index.html` exists
  before compiling and fails if it does not.
- **`AFHUB_ALLOW_NON_LOOPBACK=1`：容器唯一被允许的放开方式**。`afhub` 缺省只绑回环（约束 1），
  而 Hub 容器必须绑 `0.0.0.0` 才能从容器外访问——没有这个开关，`docker run` 就是启动即退出。
  **缺省行为一个字都没变**：不设变量仍然拒绝非回环地址。**只认确切的 `1`**：`0` / `false` /
  `true` / `yes` / 空串一律按「没设」处理——这是安全开关，宽松解析的代价是「明明写的是 `0`，
  却把端口放开了」，那种 bug 在代码里看不出来、只在被扫到的那天才暴露。拒绝信息里写明这个
  变量和它唯一的合法取值 / **`AFHUB_ALLOW_NON_LOOPBACK=1`: the only sanctioned way to open
  up for containers**. `afhub` binds loopback only by default (constraint 1), while a Hub
  container has to bind `0.0.0.0` to be reachable — without the switch `docker run` exits
  immediately. **The default behaviour is unchanged byte for byte**: leaving the variable
  unset still refuses non-loopback addresses. **Only the exact value `1` counts**: `0`,
  `false`, `true`, `yes` and the empty string are all treated as unset, because loose parsing
  on a security switch means "I wrote `0` and it opened the port", a bug invisible in the code
  until the day someone scans it. The refusal message names the variable and its one legal value.
- `Dockerfile.hub` / `Dockerfile.node` / `docker-compose.hub.yaml` / `docker-compose.agent.yaml`：
  Hub 镜像**不另放 dist**（它已在编译期嵌进二进制，另放一份反而让「单二进制」这条硬需求
  失去验证），node 镜像按 `TARGETARCH` 选二进制 / The Hub image deliberately **carries no
  separate `dist`** — it is already inside the binary, and shipping a second copy would void
  the evidence for the single-binary requirement — while the node image picks its binary by
  `TARGETARCH`.
- `SHA256SUMS` 与 GitHub Release：Release body 从本文件的对应版本段抽出，**抽不到就 fail**，
  不给一份空 body / `SHA256SUMS` plus a GitHub Release whose body is extracted from this
  file's matching version section; **extraction failure fails the job** rather than shipping
  an empty body.

### Changed

- **版本号从 `0.5.0` 提到 `0.7.1`**（`Cargo.toml` 的 `workspace.package` 与
  `packages/web/package.json` 同步）。此前二进制自报 0.5.0，而里程碑已经走到 v0.7.0——
  「发出去的二进制」与 tag 名失配正是发布通路要消灭的那类问题 / **Version bumped from
  `0.5.0` to `0.7.1`** in both `workspace.package` and `packages/web/package.json`. The
  binaries used to report 0.5.0 while the milestones had reached v0.7.0, and a shipped
  binary that disagrees with its tag is exactly what a release pipeline exists to prevent.
- `docs/DEVELOPMENT.md`：`~~v0.14.0~~` 那行划掉并注明整行迁至 v0.7.1，**代价如实记录**——
  v0.7.1 发出去的镜像是 0.7.1 的接口，而 v0.8.0–v0.13.0 仍会改公开 API，届时需要再发版本
  让自托管用户升级。「必须等对外契约定稿后再发镜像」这条前提被用户裁定推翻，不是悄悄失效的 /
  The `~~v0.14.0~~` row is struck through and marked as moved to v0.7.1, **with the cost
  written down**: the image shipped by 0.7.1 speaks the 0.7.1 API while v0.8.0–v0.13.0 will
  still change the public API, so self-hosted users will need to upgrade again. The
  "wait until the external contract settles" premise was overridden by an explicit decision,
  not quietly dropped.

## [0.7.0] - 2026-11-05

> 里程碑：**项目与绑定**（`DEVELOPMENT.md` v0.7.0 行）。产品主轴从「会话」改为「项目」，
> 这一版把轴换过去，并把「项目」变成真实体。
> Milestone: **projects and bindings**. The product axis moves from sessions to projects,
> and a project becomes a real entity rather than a label on sessions.

### Added

- **项目的三种形态：中心式 / 镜像式 / 隔离式**。`shape` 决定「代码在哪」——中心式由 Hub 的
  裸仓库保管全部代码，镜像式是上游加一份 Hub 镜像，隔离式的代码**永不出那台设备**、Hub 只
  存元数据。形态是**项目的属性，不是三种产品**：同一个页面、同一套任务流程，只有「起点来自
  拿」这一处分叉。`shape` 只允许 `isolated → central` **单向升级**（把已经上传的代码收回来，
  产品做不到）；同形态提交被拒并回 `shape_transition_refused` + `detail{from,to}`；形态变更
  是独立端点 `POST /api/projects/{id}/shape`，`PATCH` 不含 `shape`——写入口只有一个，避免
  「一次 PATCH 顺带把形态改了」这种看不见的连带 / **Three project shapes**: central
  (the Hub's bare repo holds everything), mirror (upstream plus a Hub copy), and isolated
  (the code **never leaves that device**; the Hub stores metadata only). The shape is a
  **property of the project, not three products** — same pages, same task flow, one branch
  point at "where does the starting point come from". Only `isolated → central` is permitted,
  because code that has been uploaded cannot honestly be recalled; same-shape requests are
  refused with `shape_transition_refused` plus `detail{from,to}`, and the change has its own
  endpoint so that a `PATCH` can never move the shape as a side effect.
- **项目生命周期：归档与取消归档，且没有「删除」**。`POST /archive` / `POST /unarchive`。
  归档**不打断**正在跑的执行（跑完才归档），停止派发排队任务，定时任务一并暂停，而取消归档
  **不会**自动恢复定时任务——那要人看一眼再开。执行记录、失败归因、总结、轨迹全部保留：
  项目是 Hub 的真相源，删掉会让已合入的历史一起消失，所以产品层面**不提供 DELETE 路由** /
  **Project lifecycle: archive and unarchive, with no delete**. Archiving does not interrupt
  a running execution, stops dispatching queued work, and pauses schedules; unarchiving does
  **not** silently resume them. Every execution record, failure attribution, summary and
  trajectory survives, and there is deliberately **no DELETE route**.
- **Binding：`(project, device) → 固定目录`**，加上磁盘配额与清理策略（保守 / 平衡 / 激进）。
  全仓**唯一**的 upsert 是 `PUT /api/projects/{id}/bindings/{device_id}`，项目页与设备页
  走的是同一条——同一个事实有两个写入口，迟早会出现「项目页说绑在 A、设备页说绑在 B」。
  已有执行记录时修改 `fixed_dir` 被拒（`fixed_dir_locked`）：路径一漂，依赖缓存全废，而这
  件事在页面上看不出来。**配额与清理挂在绑定上、不在项目上**——同一个项目在三台设备上可以
  各占 40 GB、各用不同策略，做成「项目级配额」才是错的 / **Bindings** map
  `(project, device)` to a fixed directory with a disk quota and a cleanup policy. There is
  exactly **one** upsert in the whole repo, reached from both the project page and the device
  page, because one fact with two write paths eventually reads differently in two places.
  Changing `fixed_dir` once executions exist is refused, and quota and cleanup live on the
  **binding**, not the project, which is the true shape of the data.
- **设备侧 worktree connector**（`node-connector` 的第 5 个 connector，注册名 `worktree`）：
  每个任务一棵工作树，**共享 git 历史、不共享依赖目录**。产物跑完即清，只有依赖缓存按
  lockfile 哈希受清理策略影响——缓存不是产物。依赖目录**永不跨设备搬运**：x86-64 与 aarch64
  装出来的不是同一份东西 / **A device-side worktree connector**: one worktree per task
  sharing git history but not dependency directories. Products are removed as soon as the
  task finishes; only the dependency cache is governed by the cleanup policy, and that cache
  is never shipped between devices.
- **Hub 侧裸仓库**（git CLI 藏在接口后，接口本身从不提「git」）：中心式与镜像式的代码都由
  它保管，将来换实现不影响调用方 / **A Hub-side bare repo** with the git CLI hidden behind
  an interface that never mentions git.
- **数据结构按项目主轴重排（schema v5）**：新增 `project` / `task` / `execution` /
  `binding` 四张表与会话仓库层，**会话挂到 Execution 下，不再是顶层**。`execution.base_commit`
  是起点的**唯一权威来源**；会话上的 `base_rev` 降级为「会话第一段执行的起点快照」，只读、
  不再参与任何「相对它产出了什么」的计算。**老会话不回填** `execution`——`execution_id`
  保持 `NULL`，宁可缺一条归属，也不要编一条假的 / **Schema v5**: project, task, execution
  and binding tables, with sessions nested under executions. `execution.base_commit` is the
  single authority for a starting point, and the old `base_rev` is demoted to a read-only
  snapshot. Old sessions are **not** backfilled — their `execution_id` stays `NULL`, because
  a missing attribution beats a fabricated one.
- **前端的 `projects` / `project` 路由与导航骨架**：项目页默认落在看板；`/project/:id` 裸路径
  不设重定向、落入 404 空态（一个说不清「你想去哪」的地址，重定向到某处只是猜）。一级导航
  按新原型**重新定为 4 项**（总览 / 设备 / Agent / 设置）——v0.5.0 曾把 Agent 并进设备页而
  收敛成 3 项，这里按项目轴把它再单独成页：设备回答「这台机器在不在线」，Agent 回答「装了
  什么、能不能跑」，是同一批数据的两个视角。旧 `/agents` 地址由 `LEGACY_PATHS` 接住。
  `router.ts` 仍手写、不引 vue-router / **Frontend routes and navigation skeleton** for
  `projects` / `project`, with the project page landing on the board. The first-level nav was
  **re-set to four items** after the prototype rebuild — v0.5.0 had merged agents into the
  devices page for three, and the project axis separates them again. `router.ts` stays
  hand-written, with no vue-router.
- **`BindPage`**（绑定设备）与 **`ProjectSettingsPage`**（合并权 / 隔离策略 / 配额与清理 /
  主题色 / 归档，5 区块）。设计上最要紧的一条：**后端还没有的能力，按钮真的 `disabled` 并
  写明原因**，不做一个点了必然报错的假按钮。`ProjectSettingsPage` 的三处「未接后端」
  （合并权三档、项目级配额、形态之外的重策略）都如实标注 / **The bind page and the project
  settings page**. The design rule that matters: **where the backend has no field, the
  button is genuinely disabled and says why** — a button that is certain to fail is worse
  than no button.
- **设备页与 agent 页按项目视角扩展（只增不减）**：绑定关系（这台设备绑了哪些项目、绑在哪个
  固定目录）、磁盘用量、失败归因（哪类失败集中在这台机器）、改配额与清理策略；agent 页加
  「跑过哪些任务 / 成功率」。**404 与读取失败分开呈现**——设备页存 `gone`、agent 页存
  `notInstalled`，各渲染专门空态。agent 页的 `task_history` 统计口径有限制，页面上必须显示
  `TASK_HISTORY_CAVEAT` 而不是让用户以为那是全量 / **The device and agent pages gained the
  project perspective, additively**. A 404 and a failed read are rendered differently
  (`gone` vs `notInstalled`), and the agent page must show its statistical caveat rather
  than let the user read a partial figure as a complete one.

### Changed

- **crate 从 7 个拆成 8 个**：`node-core` 拆出 `node-connector`（纯搬家，单独一个提交）。
  理由**只有一条**：v0.12.0 的 connector 按需加载必须靠 crate 边界。`bin` 不合并、crate
  前缀保留，只拆 `node-connector`；`pty` 随 connector 迁移，正是为了避开
  `node-connector → node-core → node-connector` 的环形依赖 / **Seven crates became eight**:
  `node-connector` split out of `node-core` in a single pure-move commit, for one reason —
  v0.12.0's on-demand connector loading needs a crate boundary. `pty` moved with it
  specifically to avoid a dependency cycle.
- **前端按领域优先重组**：`domains/<领域>/` + `domains/shared/components/` + `shared/`，
  `pages/` 只留 LoginPage / SetupPage / SettingsPage。按技术角色切的目录在实体变多之后会让
  「改一个功能要开三个文件夹」变成常态 / **The frontend is now organised domain-first**.
- **`SessionPage.vue` 不拆**。它已经很大，但拆它是一次与「项目主轴」无关的重构；把它拌进
  这一版会让「谁弄坏了什么」无法归因 / `SessionPage.vue` was deliberately **not** split here.
- **原型重做**：`prototype/` 21 页按项目轴重建，此后页面开发一律以新原型为准 / The
  prototype was rebuilt on the project axis (21 pages), and page work follows it.
- 导航与看板切换器按原型落地：`BoardPage` 用 `.af-switch`（项目名本身是按钮 + `role="menu"`
  弹出菜单），不是 `<select>`，也不是横向平铺按钮列表 / The board's project switcher follows
  the prototype rather than a `<select>`.

### Fixed

- **`scripts/check.sh` 的 7 项既有失败**在本里程碑一并修掉（此前一直红着，红着的门禁等于
  没有门禁） / The seven pre-existing `scripts/check.sh` failures were fixed here.
- **`.af-region` / `.af-callout` 从未从原型移植到生产样式表**，而设备页与绑定页都在用
  `.af-region`——那些块此前渲染成没有边框与内边距的裸 `<section>`。补齐 12 条规则 /
  The region and callout blocks were rendering as bare `<section>` elements because their
  CSS had never been ported from the prototype; twelve rules were added.
- **`--af-leading-normal` 在 `tokens.css` 里不存在**（`.af-git__void` 一直在静默回退），
  改为 `--af-leading-snug`。现在全仓 `var(--af-*)` **零未定义** / A token that never
  existed had been silently falling back; the repo now has zero undefined `var(--af-*)`.
- **归档页与项目设置页的动作失败不再把页面替成「读取失败」**：写失败与读失败必须是两条路。
  混成一条的后果是用户点一次归档失败，整页变成「打不开这个项目」——他连再试一次的按钮都
  没有了。这是 `ArchivedPage` 真实发生过的缺陷 / **An action failure no longer replaces the
  page with a read failure.** Merging the two meant one failed archive turned the whole page
  into "this project cannot be opened", leaving no button to retry — a defect `ArchivedPage`
  actually shipped.
- **`popNarrowDrawer` 用无参 `pop()` 会关掉别人的浮层并留下僵尸层，导致整页点不动**（🔴）：
  `dockEl` 绑在 `v-if` 元素上、调用时机是元素已卸载之后，于是它**就是 null**，
  `pop(undefined)` 退化成「弹栈顶」。实测后果是帮助面板被无关关闭、`depth()` 仍为 1、
  `body` 直接子节点全部 inert、**整页点不动且 `Esc` 也救不回来**。改为用闭包账本里存的
  那个 `el` / **An argument-less `pop()` closed someone else's layer and left a zombie
  entry behind, making the whole page unclickable** — the element was already unmounted, so
  the call degenerated into "pop the top of the stack".

## [0.6.0] - 2026-11-03

> 里程碑：**任务基准**。用户 2026-11-03 裁定「视为结束，从 v0.7.0 开始」——产品主轴改版后，
> 这套逻辑是对的，但它算的是 `base_commit..HEAD`，在项目主轴下那是 **Execution 的产出
> diff**，最终该挂在 Execution 上而不是会话上。所以本版**收口为「只修真 bug、不再扩展」**，
> 其「起点」概念作为 v0.7.0 的输入移交。
> Milestone: **task baseline**, closed early by decision. The logic was sound, but it
> computed `base_commit..HEAD`, which under the project axis is an **execution's output
> diff** and belongs on the execution rather than the session. The version therefore closed
> as "fix real bugs only, do not extend", handing its starting-point concept to v0.7.0.

### Added

- **会话工作区的「起点」：建会话时记下 `base_commit`**。`git` connector 增加第四个只读 op
  `rev`（`git rev-parse`），Hub 只存字符串，**零文件传输**——因此完全不触碰 2026-10-26
  撤销的「工作区供料 / 产物归档」传输面裁定，也不需要 Hub 侧建 mirror。`session` 表加
  `base_rev` 列（schema v4），`SessionSummary` 暴露它 / **A session's starting point**:
  the git connector gained a fourth read-only `rev` op, the Hub stores the string, and no
  file ever moves — which is why this does not touch the withdrawn transfer-surface decision.
- **面板显示「这次会话产出了什么」**：提交几次、另有几个文件未提交，相对 `base_commit` 算。
  在这之前，「这次改了 3 个文件」是一句无法证伪的话——它只对「现在」成立 / **The panel
  shows what the session produced** relative to its base commit, where "3 files changed"
  used to be unfalsifiable because it only described "now".
- **起点 rev 已不存在的处置**：仓库被 rebase 或 gc 掉之后，下一页不能崩溃，也不能装作
  「没有改动」——两者都会让用户得出错误结论 / **Handling a base revision that no longer
  exists**, rather than crashing or pretending there were no changes.

### Fixed

- **折叠态侧栏会长出横向滚动条**：折叠后图标轨的内容宽与定宽规则不匹配，多出几个像素就
  出现滚动条 / The collapsed sidebar grew a horizontal scrollbar.

### Changed

- **文档锚点、覆盖率例外与 e2e 脚本跟随文件搬家**：`node-connector` 拆分与前端重组之后，
  所有指向旧路径的 `文件:行号` 同步更新，否则门禁会在「文件不存在」上静默失明 /
  Documentation anchors, coverage exceptions and the e2e scripts followed the files to their
  new homes.

## [0.5.0] - 2026-10-31

### Added
- **会话页变成 opencode 那样的单层工作界面：进会话收起全站侧栏，会话铺满页面**——此前是相反的两层结构：`App.vue` 的全局侧栏（会话 / 设备 / 设置）与会话内容**并列常驻**，进会话后仍占一列，而会话自己的 tab 条排在页头下方，同一屏里有两层「导航」。现在 `rail`（用户偏好）与 `autoCollapsed`（页面态）分开，`effectiveRail = autoCollapsed ? 'collapsed' : rail`，进会话即收起、离开即恢复用户偏好。**`toggleRail` 以「用户眼前看到的」为基准翻转，不是以 `rail` 为基准**：会话页里两者会分叉，以 `rail` 翻会翻出「点了没反应」——这是本条最容易写错的地方 / **The session page is now a single-layer workbench like opencode: entering a session collapses the global sidebar and the session fills the page**. The previous structure was the opposite — the global sidebar sat permanently alongside session content while the session's own tab bar sat below the header, putting two layers of navigation on one screen. `rail` (the user's preference) and `autoCollapsed` (page state) are now separate, and `toggleRail` flips against **what the user actually sees**, because the two diverge inside a session and flipping against `rail` would produce a toggle that appears dead.
- **顶部 tab 条置顶，会话身份收进一条窄状态行**：`<TabBar>` 移到 `.af-wb__head` 之前；新增独立的 `.af-wb__statusline` 承载设备 / 路径 / 状态。**不复用 `.af-wb__meta`**：状态行字更小、行更矮、后续还要放分支名，复用同一组 class 会让两个块的样式互相牵制。
- **右侧一个工具，工具的唯一入口是头部「＋」（裁决 C）**：`PanelDock` 从「一个 `role="tablist"` 标签条 + 同一时刻只显示一个面板」改为**单选替换容器**——「＋」列出**全部三个**工具（terminal / files / git），选中即替换当前；坞标签条整个删掉（含 4 条 `.af-docktabs__b` 死样式）。这同时落地了 `design-system.md` §7.1 早已写下的「否决三个 tab 并排常驻」——**是沿用现行「一格」裁定，不是推翻它** / **One tool on the right, and its only entry point is the header's 「＋」** — the dock became a single-select replacement container listing all three tools, and the tab strip was deleted outright, which is what §7.1's long-standing rejection of a permanent three-tab row actually called for.
- **左右一条分栏可拖拽并按 tab 各自保留比例**：`split` 自始至终只有一个数（左 vs 右），**没有引入第二个维度**；`SPLIT_MIN=30 / MAX=70 / STEP=4 / DEFAULT=56` 是唯一一组常量，`clampSplit` 为纯函数。`SplitHandle` 双击 emit `reset` 回到 `SPLIT_DEFAULT`。
- **窄屏（≤900px）降级为覆盖式抽屉**：抽屉与遮罩**都 `Teleport` 到 body**——不这么做的话，`focus.push` 的 `inertOthers` 只覆盖 body 直接子节点，抽屉留在 `#app` 里会让「背后全部 inert」退化成「一个都没 inert」：遮罩盖住了视觉，背后的对话区仍然可 Tab、可点。`Esc` 与 `TrajDrawer` / `LoginPrompt` 共用 `handleLayerKeydown`。
- **真机端到端 `scripts/e2e-v0.5.0.sh`（19 条断言）**：验的是布局重排里**唯一有真实代价**的那条语义——「换工具 = 替换 + 释放被换掉那个的通道」。起真 `afhub`、真 `afcli`、真 PTY，断言旧通道 204 且**再也发不进去**（404）、点当前那个工具零副作用、替换只释放被换掉那一个、收起时全部释放且重复释放幂等。

### Changed
- **键位文案如实反映「切走会杀 PTY」**：`node-core` 的 `terminal.close()` 走 `pty.close()`，释放终端通道 = 杀掉用户的 shell。候选按钮的 `title` 写「换成…（会释放当前面板的通道）」，收起按钮 `aria-label` 写「收起右侧面板（释放它的通道）」。「换工具会释放旧通道」是**已知降级、本里程碑不缓解**，用代码就地注释 + 键位文案 + e2e 断言三处钉住，不藏。
- `docs/design-system.md` / `docs/DECISIONS.md` 同步：§7.1 记录裁决 C 的落地形态、坞标签条被删、窄屏抽屉的模态浮层口径。

### Fixed
- **`popNarrowDrawer` 用无参 `pop()` 会关掉别人的浮层并留下僵尸层，导致整页点不动**（🔴，代码审查发现）：`pop(undefined)` 走「弹栈顶」，而 `dockEl` 绑在 `v-if` 元素上、调用时机是元素已卸载之后 → `dockEl.value` **就是 null**。实测后果：窄屏抽屉开着时按 `?` 开帮助、再按 `\` 收起右侧 → 帮助被无关关闭、`depth()` 仍为 1、`body` 直接子节点全部 inert、**整页点不动且 `Esc` 也救不回来**。修法是 `pushNarrowDrawer` 时把 `el` 存入闭包账本，`popNarrowDrawer` 用 `pop(el)`，账本为空时只清账本不碰栈。
- **覆盖率门禁的两条 stale exception 背后是一个恒真断言**：`coverage-exceptions.txt` 曾把 `crates/hub-server/src/routes/stream.rs` 的某两行登记为「`panic!` 参数行」，实测那段代码是 `while let Ok(env) = rx.try_recv()` 的循环体（行号早已随文件改动漂移，这也是后来登记表改挂源码就地标注的原因）。追下去才发现根因——循环在第一次遇到 `Err(Lagged(_))` 时**立刻退出**，而后台发布线程可能已把 broadcast 环形缓冲（256 条）填满，于是 `live` **恒空**，「快照与实时流不相交」那条断言长期是「快照非空 ∧ live 为空」的**恒真形状，检测力为零**。修法不是「再补一条例外」而是让测试真的覆盖：`Lagged` 改为跳过被甩掉那段继续读（`try_recv` 的正确用法）+ 订阅后显式 publish 一条 `probe` 并断言它**必须**流到实时侧（新增真判据）。改动全在 `#[cfg(test)]` 内，产品代码零变化（`pub fn`/`struct`/`impl` 增删计数为 0）。
- **「全仓 900px 断点分布」用例名不符实**：实现只扫硬编码的三个文件，发现不了「第四个文件新增一处」——而那正是它要防的。第一次修法换成只扫 `pages/` **仍不达标**（`App.vue` 与 `components/*.vue` 同样是样式落点），第二次才改为递归扫 `src/` 下全部 `.vue`。两组反向验证：在 `App.vue` 加一处断点 → 新用例精确变红；同时用探针证明**旧口径看不见这个第四处**（若两口径等价，探针里那条 `new != old` 会红）。
- **12 条代码精简与测试有效性缺陷**：`cyclePanel` 重写为在 `PANEL_ORDER` 上按方向轮转（删掉不可达的 `mounted.length < 2` 分支与死代码 `otherPanel`，并**补上缺失的方向断言**——此前没有任何用例能区分 `[` 与 `]`）；`PanelDock` 的无读取点 `mounted` prop 删除；测试里 `realDock` 垃圾代码删除；`PANEL_LABELS` 注释改指 `@/lib/panel` 并写明「保留副本、不改成 import」的理由（**旧断言改写而非删除**，`px` 恒真断言改为读 `SPLIT_MIN`）；`e2e` 的 `$TMP` 加 `${TMP:-}` 兜底、断言 5 加 `READY_BEFORE > 0` 前置（否则基线为 0 时「增量相等」会退化成恒绿）；`design-system.md` 的 `v0.6.0` 误标改 `v0.5.0`。

## [0.4.0] - 2026-10-30

### Added
- **会话内的 git 面板（只读）**：`git` connector 落地 `status` / `diff` / `log` 三个只读子集，经 `git` 子进程读工作区；面板并进既有的面板坞，与 terminal / files 平级。此前 `connector/` 里**从未实现过 git**——v0.1.0 的心跳曾硬编码上报过它，`heartbeat.rs` 的注释与 `the_registered_set_is_exactly_the_three_m1_connectors` 都记着这件事 / **A read-only git panel inside the session**: the `git` connector implements `status`, `diff` and `log` by shelling out to git and reading the workspace. `connector/` had never actually implemented git, even though the v0.1.0 heartbeat had been hard-coding it as present.
- **`git log` 视图**，以及面板级的真机 e2e / **A `git log` view**, plus a real-device e2e for the panel.
- **不做 `commit` 及其它写操作**：v0.4.0 的目标是「看它改了什么」，而写操作要连带解决提交者身份、钩子执行与失败回滚，属另一件事，有明确需求再排期 / **No `commit` or any other write op**: the goal here was seeing what changed, and writes drag in committer identity, hooks and failure rollback.

### Changed
- 面板间切换是**视图切换**（DECISIONS D10）：切 tab 不释放通道 / Switching tabs is a view switch and does not release channels.

### Fixed
- **第 5 轮精简与代码审查的发现**（git 面板）：含 `diff` 视图形状在 `GitPanel` 里只声明一次 / Round-5 simplify and review findings across the git panel.
- **全量门禁跑出来的测试夹具与过期断言**：修掉一批只有整仓一起跑时才红的用例 / Test fixtures and stale assertions that only failed when the whole workspace ran together.

## [0.3.0] - 2026-10-28

### Added
- **`session/load`：对话关掉再回来，agent 还记得之前聊过什么**。`session.acp_session_id` 存 agent 给的会话 id，开对话时下发回去，设备侧据此走 `session/load` 而不是 `session/new`。**回填点在 `channel_data` 的 `ready` 载荷上，不在 `channel_ready`**：ACP 通道的 `channel_ready` 由设备侧 `open_external` 生成、**不含 `sessionId`**；agent 给的 id 是之后由 `conversation::open` 经 `push_external` 推上来的一条 `{"type":"ready","sessionId":…}`。选错位置的症状是静默的——`acp_session_id` 永远为 NULL，每次开对话都是全新会话，页面一切正常而上下文全丢。**load 失败不回落成 `new`**：报错并点名是哪个会话接不上；自动回落会让页面上一切正常、只是用户的上下文没了。重放事件**不落轨迹**（以轨迹表为唯一真相源，重放只喂 ACP 客户端内存态），所以「轨迹行数在 load 前后不变」是结构性成立的；V-2 实测也表明重放不带 `seq`，真写进去会撞 `UNIQUE`。新增 8 条测试（三条读路径都验、`null` 与「缺键」分开钉、假 agent 记录收到的 RPC），**反向验证 5 轮全部精确变红** / **`session/load`: reopen a conversation and the agent still remembers it**. `session.acp_session_id` stores the id the agent handed out and ships it back down when the conversation opens, so the device performs `session/load` instead of `session/new`. **The write-back hangs off the `ready` payload on `channel_data`, not `channel_ready`**: an ACP channel's `channel_ready` is generated device-side by `open_external` and **carries no `sessionId`**; the agent's id arrives afterwards as a `{"type":"ready","sessionId":...}` frame pushed by `conversation::open` through `push_external`. Picking the wrong site fails silently — `acp_session_id` stays NULL, every open starts a fresh agent session, and the page looks perfectly fine while the context is gone. **A failed load does not fall back to `new`**: it errors and names the session that could not be restored, because a silent fallback leaves everything looking right except the user's context. Replayed events **never reach the trajectory** (the trajectory table is the single source of truth; replay only feeds the ACP client's in-memory state), which makes "the row count is unchanged across a load" hold structurally; the V-2 measurement also shows replays carry no `seq`, so writing them would collide on the `UNIQUE` constraint. Eight new tests (all three read paths, `null` pinned separately from a missing key, a fake agent recording which RPCs it received), and **all five reverse checks went red**
- **删设备：`DELETE /api/devices/{id}` + 设备表行内确认框**。语义按用户的「删除语义三分离」裁定，**不是级联删**：`device` 行删；`workspace` / `session` / `trajectory_event` **全部保留**（对话是用户的数据，设备只是一条通路）；`agent_runtime` / `agent_config` 删（它们是「设备现状」，留着会让复用同一 `node_id` 的新设备继承上一台机器的 agent 清单——页面上一切都对，只是全都不属于这台机器）。归属改记到 `removed_node_id`、`node_id` 置 NULL，会话列表因此仍显示那几条并标明「已移除的设备」。**四步在一个事务里**：「设备行没了但工作区还挂着它的 node_id」是无法再被发现的中间态，而删除动作已经返回成功、没有重试入口。**两份内存账本都清**（`channels` + `bindings`，与 `ws.rs` 断开处理器同一张清单）。不存在回 404（`204` 说的是「删掉了」，那时它本来就不存在），成功回 204；前端把 `not_found` 当成「已经删掉了」不报错。**反向验证 5 轮均红**，其中三次是我自己的验证方法出错而非代码有问题：断言写在不存在的键上、反例没跑到被测路径、反例放错位置——详见里程碑文档 / **Device deletion: `DELETE /api/devices/{id}` plus an inline confirm in the device table**. The semantics follow the agreed "three separate deletion meanings" and are **not** a cascade: the `device` row goes; `workspace` / `session` / `trajectory_event` are **all kept** (a conversation is the user's data, the device is only a transport); `agent_runtime` / `agent_config` are deleted because they describe a device's present state, and keeping them would let a new device reusing the same `node_id` inherit the previous machine's agent list — everything looks right on the page while belonging to a machine that no longer exists. Ownership moves to `removed_node_id` with `node_id` set to NULL, so the session list still shows those rows marked as belonging to a removed device. **All four steps run in one transaction**, because "device row gone but its workspaces still point at its node_id" is an intermediate state nothing would ever rediscover — the delete has already reported success and there is no retry entry point. **Both in-memory ledgers are cleared** (`channels` and `bindings`, the same list the `ws.rs` disconnect path uses). Unknown ids return 404 (a 204 says "deleted", and it was already absent), success returns 204, and the front end treats `not_found` as "already gone" rather than an error. **All five reverse checks went red**, three of them because my verification method was wrong rather than the code — see the milestone document
- **设备表 CPU / 内存两列不再是 `—`：心跳真采集负载**——`heartbeat::collect_resources()` 读 `/proc/loadavg` 与 `/proc/meminfo`，**采不到就留 `null`，绝不回落成 0**（未知与空闲是两件事，0 会让路由把刚连上的设备当成最闲的那台）。**这条欠账的根因不只是「没采」**：`scheduling.md` 的目标形状把 `resources` 写成嵌套对象，而 Hub 的 `load_percent` 只按顶层键 `cpu` / `mem` 找——两边各自都没错、对不上，所以即便当初采了那两列也还是 `—`。现在**扁平百分比（`cpu_percent` / `mem_percent`）与明细（`load` / `memory`）都报**，并把分裂写进 `scheduling.md` 的差异列表，`:42` 那句「M0 的形状」改成「M2 的形状」。口径不包装：`cpu_percent` 是**每核负载不是 CPU 利用率**（load 含不可中断睡眠）；`mem_percent` 用 `MemAvailable` 不是 `MemFree`（后者在跑久了的机器上恒接近 0）。测试是**端到端**：用 node 侧真构造器发心跳再看 `/api/devices`（手搓 JSON 会跟着「我以为的形状」走，形状漂了也绿，正是这条欠账能空两三个里程碑的原因），内存与真读的 `/proc/meminfo` 对账、容差 ±3，CPU 只卡 ≤100 因为 Hub 侧有 `min(100)` / **Device CPU and memory columns are no longer `—`: the heartbeat now really collects load** — `heartbeat::collect_resources()` reads `/proc/loadavg` and `/proc/meminfo`, and **leaves `null` when it cannot read them, never falling back to 0** (unknown and idle are different things; 0 makes the router pick a freshly connected device as the idlest). **The root cause was not just "nothing collected"**: the target shape in `scheduling.md` nests `resources`, while the Hub's `load_percent` looks only at the top-level `cpu`/`mem` keys — each side was individually right and they did not line up, so even had collection existed, both columns would still read `—`. Flat percentages (`cpu_percent`/`mem_percent`) and detail (`load`/`memory`) are now both reported, and the split is recorded in `scheduling.md`. Stated without euphemism: `cpu_percent` is **per-core load, not CPU utilisation** (load includes uninterruptible sleep); `mem_percent` uses `MemAvailable`, not `MemFree` (the latter sits near 0 on any long-running machine). The test is end-to-end: send a heartbeat built by the real node-side constructor and read `/api/devices`, because a hand-written JSON payload follows "the shape I assumed" and stays green when the shape drifts — exactly how this debt survived two or three milestones
- **`deploy/afcli.service`：设备上把 afcli 跑成 systemd 服务**——`afcli run` 是**前台阻塞**的（supervisor 是一个无限 loop，仓库里有意的没有 setsid / nohup 路径，见 `DECISIONS.md` D1），而 `security-and-lifecycle.md` §4.1 早已假设「下次被 systemd 拉起」却从未给出 unit（`find . -name '*.service'` 零命中）。unit 是 `Type=simple` + `Restart=always`，配置走 `EnvironmentFile=/etc/agentfleet/afcli.env`（改 Hub 地址只要 restart，不必 daemon-reload），以非 root 用户跑（afcli 持有设备私钥 `node.key`）。**两层退避不重复**：systemd 管进程存活（supervisor 自己被 OOM kill），supervisor 内部的 `CrashStats` / `decide` 管 worker 生命周期（worker 崩了怎么退避、何时判坏版本）。`README.md` 生产段从裸 `afcli run` 改为指向它，并写明 `AFCLI_HUB_URL` 必须显式给（缺省是本机 Hub，真设备给它永远注册不上）、`AFCLI_DATA_DIR` 定了就别改（换了等于换一台设备）。`scripts/check.sh` 加 C6 门禁锁住 unit 与代码契约 / **`deploy/afcli.service`: run afcli as a systemd service on a device** — `afcli run` **blocks in the foreground** (the supervisor is an infinite loop; there is deliberately no setsid/nohup path, see `DECISIONS.md` D1), yet `security-and-lifecycle.md` §4.1 had long assumed "restarted by systemd" without ever shipping a unit (`find . -name '*.service'` matched nothing). The unit is `Type=simple` + `Restart=always`, configured through `EnvironmentFile` (changing the Hub URL needs a restart, not a daemon-reload), and runs as a non-root user (afcli holds the device private key). **The two retry layers are not redundant**: systemd owns process liveness (the supervisor itself getting OOM-killed), the supervisor's `CrashStats` / `decide` owns worker lifecycle. The README's production section now points here instead of a bare `afcli run`, and states that `AFCLI_HUB_URL` must be set explicitly and `AFCLI_DATA_DIR` must not change after the fact. A C6 gate in `scripts/check.sh` pins the unit to the code contract
- **登录页与首次 setup 页**：`design-system.md` §5.2 / §5.3 早已把这两页标为 M1、原型也在仓库里，但子任务清单漏了。缺它们不是「少两个页面」，而是**全新部署没有任何路径能走到 setup**（`/api/*` 全回 403 `hub_not_initialized`，而前端只认 401）。五种登录错误分开呈现；setup 的 `token_mismatch` 与 `token_locked` 分开呈现（后者出路是重启 Hub）/ **Login and first-run setup pages**: the design spec and prototypes had both marked M1 from the start. Without them a fresh deployment had **no path to setup at all**. Five login failures are presented distinctly, and setup distinguishes a mismatched token from a locked one
- **`GET /api/setup` 探针**：`/setup` 原本是 JSON 探针，于是硬加载（首次访问、刷新）拿到的是 `{"initialized":false}` 而不是页面。一条路径不能兼两职，而 M0 验收标准③ 锁的是探针**语义**（已初始化 → 409）不是路径，所以探针挪走、`/setup` 让给页面——与 `POST /login`（端点）/ `GET /login`（页面）同构 / **`GET /api/setup` probe**: `/setup` used to be the JSON probe, so a hard load (first visit, refresh) returned `{"initialized":false}` instead of a page. One path cannot serve both roles, and M0's acceptance criterion locks the probe's **semantics** rather than its path, so the probe moved and `/setup` became the page — mirroring the existing `POST /login` / `GET /login` split
- **设备探测与 agent 启动配置**：设备扫描**已装**的 agent 并上报，Hub 只存启动配置、**不代装**；建会话可选用该设备上已装且可用的 agent（未装 → 409）/ **Agent detection and launch configuration**: the device scans what is actually installed and reports it; the Hub stores launch configuration and **never installs agents**; creating a session with an uninstalled agent returns 409.

### Fixed
- **真设备 E2E（`scripts/e2e-m2.sh`）抓到四处会让对话「看起来正常、其实丢了」的缺陷，全部修在源头**。这四条都是**跑真 opencode 才暴露**的，单测两侧各自全绿。① **`seq` 跨 ACP 子进程从 1 重数，事件被静默吞掉**：`seq` 的契约是「会话内只增」（`gaps` 靠它判断裂），但计数器活在 ACP 子进程里——关页面再开、worker 换代、`session/load` 建新进程，每种都让它回到 0，于是新事件带 `seq=1` 撞库里已有的 `seq=1`，被 `UNIQUE` + `INSERT OR IGNORE` 吃掉，而 `gaps` 不报警（seq 仍然连续）。实测一条 `user_message_chunk` 整条消失。修法是让起点来自**跨进程的真相源**：Hub 在 `open_channel.params.last_seq` 下发 `trajectory_event` 的 `MAX(seq)`（没有就 `0`，`Seq::next()` 让第一条是 1），设备侧 `Seq::new(start)` 从它续号；缺字段 / ≤0 都当 0，旧 Hub 不下发时行为与从前一致。② **重放事件按 agent 自带的 `messageId` 去重**：①之后重放不再撞号，于是全部落库，轨迹里同一段回答出现两遍（实测行数 4 → 7）。实测重放与实时那条的 `messageId` **逐字相同**（`msg_17af87a19001xcrudnts0tSVl3`），这是 agent 对「同一条消息」的明确声明，比「payload 完全相等」可靠（重放的 `usage_update` 计数可能不同）。③ **没有 `messageId` 的状态快照按内容去重，但只在白名单上**：load 回执之后 opencode 又推一份 `available_commands_update`，与 load 之前那条**剔除 `seq` 后逐字相同**。判据 =「同会话、同 kind、剔掉 `seq` 后 payload 相同」，白名单目前只有 `available_commands_update`——`usage_update` **不在**里面（它带计数，按内容去重会让计数不变的时期看起来像「没有事件」），**也绝不能扩展到所有 kind**（同一句话被说两次是两次真实发言；这一层曾误伤 5 个 `gaps`/`turn_bounds`/`listing` 测试）。④ **设备曾上报未安装的 agent**：`probe_once` 把 `KNOWN_AGENTS` 全部 9 条都推上心跳，`Probe::NotInstalled` 转成 `{available:false, error:null}`，与「装了但不能对话」（`NotCapable`）在 Hub 侧**形状完全相同**，分不开——症状是把 opencode 从 PATH 移走后它仍留在 `/api/agents` 里，目录对用户撒谎。修法是 `report_of` 对 `NotInstalled` 返回 `None`、`probe_once` 跳过它：心跳的语义收紧为「这台机器上**存在**哪些 agent」。`NotCapable` / `Failed` 两条「必须列出来并带原因」的契约原样保留 / **The real-device E2E caught four defects that made conversations look fine while silently losing data; all four are fixed at the source.** Every one needed a real opencode run to surface — unit tests were green on both sides. (1) **`seq` restarted at 1 per ACP child process, so events were silently swallowed**: the contract is "monotonic within a session" (the `gaps` query relies on it), but the counter lived in the ACP subprocess, so closing the page and reopening, a worker generation change or a `session/load` spawned a new process and reset it to 0. New events then carried `seq=1`, collided with the existing `seq=1`, and were eaten by `UNIQUE` + `INSERT OR IGNORE` while `gaps` stayed quiet (the sequence was still contiguous). One `user_message_chunk` disappeared entirely. The starting point now comes from the **cross-process source of truth**: the Hub sends `MAX(seq)` of `trajectory_event` in `open_channel.params.last_seq` (`0` when there is none, and `Seq::next()` makes the first one 1), and the device continues with `Seq::new(start)`; a missing field or a value ≤ 0 counts as 0, so an older Hub behaves exactly as before. (2) **Replayed events are deduplicated by the agent's own `messageId`**: after (1) replays no longer collided and were all stored, so the same answer appeared twice (row count 4 → 7). The replay's `messageId` matches the live one **character for character**, which is the agent's explicit statement that these are the same message and more reliable than "identical payload" (a replayed `usage_update` may carry different counters). (3) **State snapshots without a `messageId` are content-deduplicated, but only against a whitelist**: after the load response opencode pushes another `available_commands_update` that is character-identical to the earlier one **once `seq` is removed**. The rule is same session, same kind, equal payload minus `seq`, and the whitelist holds only `available_commands_update` — `usage_update` is deliberately **not** in it (it carries counters, and content dedup would make a period of unchanged counters look like no events), and it **must not** be extended to every kind (an agent saying the same sentence twice is two real utterances; this layer once broke five `gaps`/`turn_bounds`/`listing` tests). (4) **The device used to report agents that were not installed**: `probe_once` pushed all nine `KNOWN_AGENTS` into the heartbeat, and `Probe::NotInstalled` became `{available:false, error:null}` — the exact same shape as "installed but cannot converse" (`NotCapable`), indistinguishable on the Hub. The symptom was opencode still listed in `/api/agents` after being moved off the PATH, so the catalogue lied to the user. `report_of` now returns `None` for `NotInstalled` and `probe_once` skips it, tightening the heartbeat to "which agents **exist** on this machine". The `NotCapable` / `Failed` contracts that require listing them with a reason are untouched
- **node 上行 `channel_data` 缺 `channel_id` 信封，双向数据面从来没通过**——只有真设备 E2E 才暴露。协议（`protocol.md` §4）把 `channel_data` 定为**双向**信封 `ChannelData{channel_id, payload}`：Hub 下发时包了，node 上行时把 connector 的裸载荷直接放进帧的 `c`。Hub 侧解析失败后把它当「无法解释的 JSON」静默丢掉，于是 `/send` 回 204、通道 `ready`、PTY 真的读到了字节、**页面上一片空白**，两侧日志各只有一行 DEBUG。单测两侧各自全绿，因为各自按自己的错误形状写测试（node 侧测试注释甚至写着「本层只透传，不需要再剥一层 payload」）。现在三处测试助手都改成**断言信封存在**，形状错误下次能被单测抓住 / **The node's uplink `channel_data` was missing the `channel_id` envelope, so the data plane had never worked in either direction** — only a real-device E2E run exposed it. The protocol defines `channel_data` as a bidirectional envelope; the Hub wrapped it, the node dropped the bare connector payload into `c`. The Hub failed to parse it, treated it as unexplainable JSON and dropped it silently: `/send` returned 204, the channel reported ready, the PTY really received the bytes, and **the page stayed blank** with one DEBUG line on each side. Both sides' unit tests were green because each asserted its own wrong shape. Three test helpers now assert the envelope exists
- **`GET /login` 在初始化模式下被 403**：`/login` 整条注册在 `.layer` 之前，而中间件对**所有**经过它的请求做初始化模式判断，于是首次部署时登录页自己打不开——而 `/sessions/abc` 因为走 SPA fallback 却能出外壳，症状看起来像「只有登录页坏了」。现在 `GET`（页面）与 `POST`（端点）拆开，只有端点受控 / **`GET /login` was 403 in init mode**: the whole path sat before the auth layer, and the middleware applies the init-mode rule to everything passing through it, so on a fresh deploy the login page could not open itself while `/sessions/abc` rendered fine through the SPA fallback. GET (page) and POST (endpoint) are now split
- **文案引用了不存在的 `afhub reset-setup`**：文档与页面都把它当作 `setup_token_locked` 的恢复路径，但这条命令从未实现过（`afcli setup --print-token` / `--reset-password` 同）。重启 Hub 本身就会重新生成 token 并清零失败计数，所以统一改成「重启 Hub」——**不加命令，只改文案** / **Copy referenced a nonexistent `afhub reset-setup`**: both docs and UI treated it as the recovery path for a locked setup token, but it never existed. Restarting the Hub already issues a fresh token and resets the attempt counter, so the copy now says that — no new command
- **M1 全量缺陷清扫（156 条）**：里程碑 `Bugs` 表最终 **156 条全部闭环**（22 🔴 / 101 🟡 / 33 🟢）——其中 96 条是步骤 3 开工时的待修集（11 🔴 / 56 🟡 / 29 🟢），另 60 条是三轮审查新登记的。逐条修复并**逐条实证**——每条都在表里附上「已落地」的文件与行号，没有一条靠「看起来改了」勾掉；第 2 轮把一条已被标 ✅ 的假闭环（`on_disconnect()` 生产零调用）**退回 ⬜** 重修。最后 6 条是第 3 轮审查发现的，其中两条 🔴 只可能由真机暴露，且**都做了反向验证**（把修复 `git stash` 掉后新回归用例必须红）：① **Hub 侧信用账本从未被生产路径写入**——账本的唯一建表入口挂在「node 发 `open_channel`」这条生产上不存在的路径上，于是每帧上行都被判未知通道、永不回 `channel_credit`，终端输出在 ~32 帧后**永久冻结**且两侧日志全静默（E2E 29/29 不是反证：它只见过 2/6/5 帧，全在额度内）；② **`exec` 在 node 的事件循环任务里同步阻塞最长 30 s**——`timeout_ms: 0` 时永久，期间心跳停发（> 45 s 判离线）、同设备所有通道冻结、全局通道锁被独占。其余用户可见的几条：`afcli run` 在 Hub 未初始化时不再以退出码 20 静默退出（持续退避 + 往 stdout 打一句 setup 指引）；读路径不再把「设备状态损坏」静默编成 `offline`；files 按**编码后**的载荷判一帧上限（一个 500 KiB 文本 → 1.17 MiB 帧会把整台设备的 WS 顶掉）；`?` 帮助面板改为 `Teleport` 到 `body` 并纳入焦点栈，`Esc` 按栈顶语义逐层关闭 / **Closed all 156 M1 defects** (22 critical / 101 major / 33 minor) — 96 of them were pending when step 3 started (11 critical / 56 major / 29 minor) and 60 more were registered by the three review rounds. Each was fixed **and evidenced**: every row in the milestone Bugs table carries the file and line that closes it, and none was checked off on appearance. Round 2 rolled one false closure back to open (`on_disconnect()` had no production caller) and had it fixed properly. The final six came from round 3, and two of them are critical defects that only a real device can expose — both **reverse-verified** (stashing the fix away must turn the new regression case red): (1) **the Hub's credit ledger was never written by any production path** — its only creation site hung off a node-originated `open_channel`, a path that does not exist, so every uplink frame was judged an unknown channel and never answered with `channel_credit`, freezing terminal output for good after ~32 frames while both sides stayed silent (the 29/29 E2E is not a counter-proof: it only ever saw 2/6/5 frames, all within budget); (2) **`exec` blocked synchronously inside the node's event-loop task for up to 30 s** — forever with `timeout_ms: 0` — during which heartbeats stopped (> 45 s means offline), every other channel on the device froze, and the global channel lock was held. Other user-visible ones: `afcli run` no longer exits silently with code 20 when the Hub is uninitialized (persistent backoff plus a one-line setup hint on stdout); the read path no longer disguises a corrupted device status as `offline`; the files connector now bounds a frame by the **encoded** payload (a 500 KiB text expands to a 1.17 MiB frame that used to drop the whole device's WebSocket); the `?` help panel is `Teleport`ed to `body` and joins the focus stack, and `Esc` closes one layer at a time following stack order

### Changed
- **文档不再承诺「Hub 下发安装 agent」——共 6 处，代码里从来没实现过**。用户 2026-10-26 已裁定「不要支持安装 agent，直接扫描就行，agent 让用户自己去下载安装，但是我们要支持对应的配置」，但这条裁定只落到了一部分文档上：`routes/agents.rs` 的三条路由确实是 `/api/agents/config`（注释明写「原先这三条路由是 `/api/agents/install` 与 `/api/agents/uninstall`」），`PRODUCT.md` 的「AI agent 管理与对话」行也已改成「Hub 负责扫描 + 保存启动配置」——**而另外 6 处仍在承诺安装**。其中两处最误导：① 本仓 `README.md` 第 47 行把「Hub 一键安装 agent」标成 `M2`，而本仓 README 的头标写着「M0、M1、M2 已完成」，读者会以为这个能力已经能用，去页面上找安装按钮；② `docs/PRODUCT.md` 第 103 行的 M0 验收标准第 8 条仍写着「下发安装指令 → 节点本地安装」的完整步骤，它是**当次验收要逐条走的清单**，不是历史引述。另外四处：`DEVELOPMENT.md` 的 M2 目标列「由用户手动触发下发安装」、§4.2 映射表「Hub 一键安装 agent → M2」、`docs/PRODUCT.md` §3.1 能力表同名行、以及 `DEVELOPMENT.md` §4.3 反向检查表里把 **afcli 自身升级**也挂在「与 agent 托管配置同一节落地」上的一行。**改法统一为「划掉 + 写清现状 + 指向决策记录」**，不删原文：产品承诺文档里「我们曾承诺过这个、现在决定不做」必须留下痕迹，否则下一个人会以为这条能力是自己漏做了。**afcli 自身的升级通路被明确区分出来**——它是另一条通路，不随 agent 代装一起撤销，但落地位置随之失去依附，故改为「未排期」而不装作 M2 交付了。顺带把里程碑表与本仓 README 的**进度标记对齐**:`DEVELOPMENT.md` 的 M2 改成 `✅ 已完成`、M3 改成 `🔄 当前`，`README.md` 第 37 行那句「M2 把 ACP 接进来：…Hub 下发安装…」改成「扫描 + 配置」。**验证**：`grep -rn` 全仓扫「下发安装 / 一键安装 / 节点本地安装 / 代装」与英文 `one-click install`，非 `.mdflow/` 结果**只剩撤销说明自身的引述**；新写的「9 个已知 agent、只上报存在的」与代码对账——`KNOWN_AGENTS` 实测 `id:` 恰好 9 条，`report_of` 实测对 `Probe::NotInstalled` 返回 `None` / **The docs no longer promise a Hub-driven agent install — six places, none of which the code ever implemented**. On 2026-10-26 the decision was made to drop installs in favour of scanning, but that decision only reached part of the documentation: `routes/agents.rs` really does expose `/api/agents/config` (its comment says "these three routes used to be `/api/agents/install` and `/api/agents/uninstall`"), and the docs/PRODUCT.md capability row already says the Hub scans and stores launch configuration — **while six other places still promised installs**. Two are actively misleading: README.md line 47 tagged "Hub one-click agent install" as `M2` while the header proclaims "M0, M1 and M2 are done", so a reader concludes the feature is usable and goes looking for the button; and docs/PRODUCT.md line 103, in the M0 acceptance criteria, still spells out the step "issue an install instruction -> the node installs it locally". That is a checklist to be walked through, not a historical quote. The other four: the M2 goal cell in DEVELOPMENT.md, the §4.2 mapping row "Hub one-click agent install -> M2", the identically named row in docs/PRODUCT.md §3.1, and a row in DEVELOPMENT.md §4.3 that hitched **the afcli self-upgrade** to the same "lands with agent managed config" story. **Every fix is strike-through + state the present + point at the decision record**; nothing is deleted, because a product promise that was made and then withdrawn has to leave a trace, or the next person assumes they simply missed implementing it. **The afcli self-upgrade path is called out separately** — it is a different mechanism, it did not go away with agent installs, but its landing site lost its anchor, so it is now "unscheduled" rather than dressed up as M2 work. Progress markers were aligned while there: DEVELOPMENT.md now shows M2 as done and M3 as current, and README.md line 37 no longer says the Hub pushes installs. **Verified** by grepping the whole repo for the Chinese and English phrasings: outside `.mdflow/`, only the withdrawal notes themselves still contain them. The new claims about nine known agents and reporting only what is present were checked against the code — `KNOWN_AGENTS` has exactly nine `id:` entries and `report_of` really does return `None` for `Probe::NotInstalled`
- **品牌区与侧栏合并成一列，折叠态保留原来的图标轨形态**。用户问「侧边栏是否和 af-brand 合并」，追问后裁定合并。**改之前**：`.af-app` 的 grid 是 `44px 208px 1fr`，`.af-brand`（22px 方块 + 竖排 `AGENTFLEET`）和 `.af-sidebar` 是**两块独立的网格区域**，中间靠 `border-right` 划开。但它们本来就同高、同背景色（都是 `--af-surface-1`）、同受 `⌃B` 折叠，分开只多一条分隔线；而竖排 `AGENTFLEET` 挤在 44px 里几乎读不出来。**改之后**：grid 是 `208px 1fr`，`grid-template-areas` 只剩 `sidebar` / `topbar` / `main` / `status`，`.af-brand` 成为 `.af-sidebar`（`nav`）的**子节点**，横排显示（方块 + 名字同一行），与导航同属一个 flex 容器，折叠时由同一个容器一起管。侧栏总宽从 `44+208` 降到 `208`，省下的 44px 还给主区。**折叠态按用户要求保留原样**：`.af-brand` 改回竖列（方块在上、名字在下）、`.af-brand__name` 恢复 `writing-mode: vertical-rl`。这不是审美取舍——折叠后只剩 52px，横排名字配 `white-space: nowrap` 会溢出被裁，竖排是唯一能读完的排法；折叠是「收起」不是「藏功能」，品牌标识得留下。**顺带修掉一个既存的对齐缺陷**：折叠态 `.af-sidebar` 是 `align-items: center`，子项不再被 `stretch` 拉满，于是 `.af-sidebar__item` 的宽度缩成内容宽 28px（16px 图标 + 左右各 6px 内边距），而品牌方块居中在 40px 的内容区里——图标贴左侧 6px、方块在正中，两处基准不同。这条在合并前就存在，但那时品牌区是自己的独立列、看得不明显；合并后两者同列，错位就露出来了。修法是给折叠态导航项定宽 40px + `justify-content: center` + `padding: 0`。**没有浏览器可用，视觉是靠算的**：`--af-space-3` = 6px，折叠态可用内容宽 `52 - 12 = 40px`，22px 方块放得进；展开态横排所需约 98px（方块 22 + 间距 6 + 名字约 62 + 字距），内容宽 196px，余量充足。**测试守结构契约，不守外观**：`App.test.ts` 新增 4 条——① 品牌区父节点必须是 `nav`；② grid 里不再有 `brand` 列名；③ 折叠态名字竖排且**不许** `display: none`；④ 折叠态导航项定宽居中。**四条反向验证全部精确变红**（挪回 `nav` 外 → `expected 'DIV' to be 'NAV'`；grid 改回三列 → `expected '.af-app {…' to contain 'grid-template-columns: 208px 1fr'`；竖排改回隐藏 → `expected … to contain 'writing-mode: vertical-rl'`；删掉定宽规则 → `折叠态导航项规则必须存在: expected -1 to be greater than -1`），恢复后 684 项全绿、四道门禁通过。规格同步 `design-system.md` §5.1（骨架图去掉图标轨列、写明同列理由与折叠态排法） / **The brand block and the sidebar are now one column, and the collapsed state keeps its original icon-rail look**. The user asked whether the sidebar should merge with `af-brand` and, after a follow-up, decided it should. **Before**: the app grid was `44px 208px 1fr` and `.af-brand` (a 22px square plus a vertical `AGENTFLEET`) and `.af-sidebar` were **two separate grid areas**, divided by a `border-right`. Yet they were always the same height, the same background and collapsed together on `⌃B` — splitting them bought nothing but a divider, while the vertical `AGENTFLEET` squeezed into 44px was nearly unreadable. **After**: the grid is `208px 1fr`, `grid-template-areas` holds only `sidebar` / `topbar` / `main` / `status`, and `.af-brand` is a **child of `.af-sidebar`** (the `nav`), laid out horizontally so the square and the name sit on one line; both share one flex container, so collapsing manages them together. The sidebar drops from `44+208` to `208`, returning 44px to the main area. **The collapsed state is untouched, as the user asked**: `.af-brand` goes back to a vertical stack (square above, name below) and `.af-brand__name` regains `writing-mode: vertical-rl`. This is not an aesthetic choice — at 52px a horizontal name with `white-space: nowrap` overflows and gets clipped, so vertical is the only readable arrangement, and collapsing means "tucked away", not "hide the feature": the brand mark stays. **An existing alignment defect fell out along the way**: in the collapsed state `.af-sidebar` uses `align-items: center`, so children are no longer stretched and `.af-sidebar__item` shrinks to its content width of 28px (a 16px icon plus 6px padding each side), while the brand square is centred in the 40px content box — the icon pinned 6px from the left, the square dead centre, two different baselines. The defect predates this merge, but it was hard to see when the brand block had a column of its own; sharing a column exposes it. The fix gives collapsed nav items a fixed 40px width with `justify-content: center` and `padding: 0`. **No browser was available, so the visuals were computed, not eyeballed**: with `--af-space-3` at 6px the collapsed content box is `52 - 12 = 40px`, which fits the 22px square, and the expanded horizontal row needs about 98px (22 square + 6 gap + ~62 name + tracking) inside a 196px box. **The tests guard structural contracts, not appearance**: four new cases in `App.test.ts` — the brand block's parent must be the `nav`; the grid must no longer carry a `brand` column; the collapsed name must be vertical and must **not** be `display: none`; and collapsed nav items must be fixed-width and centred. **All four reverse checks went precisely red** (moved outside the `nav` -> `expected 'DIV' to be 'NAV'`; grid reverted to three columns -> `expected '.af-app {…' to contain 'grid-template-columns: 208px 1fr'`; vertical reverted to hidden -> `expected … to contain 'writing-mode: vertical-rl'`; fixed-width rule deleted -> `expected -1 to be greater than -1`), and with everything restored 684 tests and all four gates pass. `design-system.md` §5.1 is updated to match (the skeleton no longer has an icon-rail column and now states both the merge rationale and the collapsed arrangement)
- **侧栏从 5 项收敛成 3 项：会话 / 设备 / 设置**。用户反馈「新建一个会话需要跳转好几个页面」，进一步裁定「只要区分会话页、设置页和其他页」。三处实质改动：① **`Agent 目录` 并进设备页**的 `?view=agents` 分段——两者看的是**同一个对象**（「这台机器上有什么」）：设备说它在不在线，目录说它上面装了什么，分两条路由时用户要在两个地址之间跳，而两者拉的是同一批设备、同一份心跳事实。`AgentsPage.vue` 降级为 `components/AgentCatalog.vue`（组件不再知道路由，去哪由设备页决定，于是两处入口能共用同一个组件）；② **`新建会话` 不再是导航项**——导航项承诺「这里有一块内容」，而新建会话要么从会话页、要么从设备行「用它开会话」发起，**两条路都带着上下文**（要建在哪台机器上），导航项带不了上下文，把它留在侧栏等于承诺一个「先选设备」的空表单；③ **`/agents` 路由删除**，旧地址由 `LEGACY_PATHS` 重定向到 `/devices?view=agents`，用 `replace` 不留历史——它真是一级导航里存在过的地址，书签与文档链接都指向它，静默 404 的症状是「点收藏夹看到『这个地址没有页面』」而内容就在隔壁。分段切换是**视图切换**（DECISIONS D10）所以用查询参数 + `replaceState` 而非第二条路由，否则「后退」变成在分段之间来回跳。规格同步：`design-system.md` §5.1（导航 5 → 3 项及理由）、§5.2（路由表与「已删除但有去处的地址」）、§4.2（键位表 22 → 20 个 act，删 `nav:new-session` / `nav:agents`）。**会话多开 tab 无需新做**：`lib/tabs.ts` + `TabBar.vue` + `SessionPage.vue` 的 `workspaces` Map 在 M1 已落地，本轮只验证它仍全绿（重做一件做过的事比漏做更贵：同时引入新缺陷和「不知道哪一版才对」的历史）。新增 3 条分段测试，**三条反向验证全部精确变红**（拆掉重定向 → `expected '/agents' to be '/devices'`；删掉映射 → `expected null to be '/devices?view=agents'`；`replace` 改回 `push` → `expected 3 to be 2`）；前端四道门禁全绿（680 项测试） / **The sidebar went from five items to three: sessions / devices / settings.** The user reported that creating a session took several page transitions and then ruled "only distinguish the session page, the settings page, and everything else". Three substantive changes: (1) **the agent catalogue is merged into the devices page** as a `?view=agents` segment — both look at the **same object** ("what is on this machine"): devices say whether it is online, the catalogue says what is installed on it, and splitting them across two routes made the user hop between two addresses that load the same devices and the same heartbeat facts. `AgentsPage.vue` is demoted to `components/AgentCatalog.vue`, which no longer knows about routing — where to go is the device page's decision, which is what lets two entry points share one component. (2) **"New session" is no longer a navigation item** — a nav item promises "there is content here", while creating a session starts either from the sessions page or from a device row's "use it for a session", and **both paths carry context** (which machine to create it on). A nav item cannot carry that context, so leaving it in the sidebar promised an empty "pick a device first" form. (3) **the `/agents` route is deleted**, with the old address redirected by `LEGACY_PATHS` to `/devices?view=agents` using `replace` so no history entry is left behind: it really was a first-class navigation address and both bookmarks and our own documentation links point at it, and a silent 404 reads as "I clicked my bookmark and got 'this address has no page'" while the content sits next door. Segment switching is a **view switch** (DECISIONS D10), hence a query parameter plus `replaceState` rather than a second route — otherwise "back" turns into ping-ponging between segments. Spec updated in three places: `design-system.md` §5.1 (five items to three, with the reasoning), §5.2 (routing table plus a "deleted but redirected" section), and §4.2 (key table from 22 to 20 acts, dropping `nav:new-session` / `nav:agents`). **Multi-session tabs needed no new work**: `lib/tabs.ts`, `TabBar.vue` and `SessionPage.vue`'s `workspaces` map already landed in M1, and this round only verified they are still green — redoing finished work is more expensive than skipping it, because it adds both new defects and a history where nobody knows which version is right. Three segment tests added, and **all three reverse checks went precisely red** (dropping the redirect -> `expected '/agents' to be '/devices'`; deleting the mapping -> `expected null to be '/devices?view=agents'`; `replace` reverted to `push` -> `expected 3 to be 2`); all four front-end gates pass (680 tests)
- **`mise` 分出开发与生产两条路**：`mise run dev` 同时起 Hub（8080 / 仅回环 / cargo-watch）与 Vite（3002 / 热更新 / 代理 `/api` 与 `/ws`）；`mise run build` 先编译前端再由 `include_dir!` 嵌进二进制，`mise run start` 单进程单端口。开发那条**必须用 `depends` 而不是 `run = [...]`**——后者是顺序执行，而 `dev-hub` 是长驻进程永不返回，前端永远轮不到启动 / **`mise` now separates development from production**: `mise run dev` starts the Hub (8080, loopback only, cargo-watch) alongside Vite (3002, hot reload, proxying `/api` and `/ws`); `mise run build` compiles the frontend first and embeds it in the binary, and `mise run start` runs one process on one port. The dev task **must use `depends`, not `run = [...]`** — the latter is sequential, and `dev-hub` never returns, so the frontend would never start
- 数据目录默认改为项目级 `data/`（`mise.toml` 的 `AFHUB_DATA_DIR`），不落 `~/.local/share/` / The data directory now defaults to a project-local `data/` instead of `~/.local/share/`
- **CI 只在推 tag 时跑**：全量门禁（编译 + clippy + fmt + 逐 crate 测试 + 前端四道 + 覆盖率 + 真机 E2E）在这台机器上要近一小时，每次 push 都跑等于没有 CI。`.github/workflows/ci.yml` 因此只挂在 `push: tags` 上，与 `DEVELOPMENT.md` / `mise.toml` 的说法对齐 / **CI now runs on tags only**: the full gate (build + clippy + fmt + per-crate tests + the four frontend checks + coverage + real-device E2E) takes close to an hour on this machine, so running it on every push meant no CI at all. `.github/workflows/ci.yml` is therefore triggered only by `push: tags`, matching what `DEVELOPMENT.md` and `mise.toml` claim
- **覆盖率门禁不再吞自己的诊断**：`check-coverage.sh` 把 `cargo llvm-cov` 的 stderr 收进临时文件，失败时原样转发——此前 `2>/dev/null` 让「缺 llvm-tools」「编译失败」只剩一句 `FATAL: cargo llvm-cov failed`。同时新增 `mise run cov-gate`（`ci` 依赖它），例外清单改为**以源码里的就地 `// coverage: excluded` 注释为事实源**，`fork` 后子进程、`pipe2` / `forkpty` 的 fd 耗尽、`Option` 的不可达 None 臂这类**结构上不可达**的行按行号逐条登记，登记了却已覆盖的行会被反向判为过期 / **The coverage gate no longer swallows its own diagnostics**: `check-coverage.sh` now collects `cargo llvm-cov`'s stderr into a temp file and forwards it verbatim on failure, where `2>/dev/null` had reduced "missing llvm-tools" and "build failed" to one useless `FATAL` line. `mise run cov-gate` was added and `ci` depends on it; the exception list is now sourced from in-place `// coverage: excluded` comments, and structurally unreachable lines (post-`fork` children, `pipe2` / `forkpty` fd exhaustion, unreachable `Option` `None` arms) are registered line by line — a registered line that becomes covered is reported as stale
- **核实而非照抄缺陷池**：M1 的 8 条 bug 逐条 grep 实证后才勾掉，每条附「已落地」的文件与行号。其中一条只有后端半边（前端没有 login / setup 页），于是补立项 14a 而不是标 ✅ / **Bugs were verified, not rubber-stamped**: all eight M1 defects were grep-proven before being closed, each with the file and line that fixes it. One had only its backend half, so it became subtask 14a instead of a checkmark

## [0.2.0] - 2026-10-22

### Added
- **M1 会话容器与首个演示里程碑完成**：真 `afhub` + 真 `afcli` 上跑通——页面建会话 → 开终端 → **从页面敲 `echo` 看到输出** → `resize` 后 `stty size` 回 `40 120` → 文件面板列目录读文件 → exec 跑一次性命令拿到退出码。全程**零入站端口**（`ss -tlnp` 核对）。交付物：Web 控制台（设备 / 会话 / 新会话 / 设置 + 会话工作台）、会话级三个 connector（PTY 终端含 resize、files 含越界强制、一次性 exec）、Hub 静态资产托管（前端产物嵌进单个二进制）/ **M1 session-container milestone completed**: verified against real `afhub` and `afcli` — create a session in the browser, open the terminal, **type `echo` from the page and see the output**, `stty size` returns `40 120` after a resize, the file panel lists and reads, and exec returns an exit code. **Zero inbound ports throughout** (checked with `ss -tlnp`). Ships: the web console (devices / sessions / new session / settings plus a session workbench), three session-scoped connectors (PTY terminal with resize, files with escape enforcement, one-shot exec), and Hub-side static hosting with the built console embedded in a single binary
- **页面侧数据面**：`GET /api/stream`（SSE 下行）+ 四个上行端点（开通道 / send / resize / close）。CSRF 双重提交只落在三个上行端点——下行是 `EventSource`，它发不了自定义请求头，**这是选择 SSE 而非 WebSocket 的直接代价**，写在 `tests/stream.rs` 的 `the_downlink_needs_no_csrf_header` 里 / **Page-side data plane**: an SSE downlink plus four uplink endpoints. CSRF double-submit covers only the uplinks, because `EventSource` cannot send custom headers — the direct cost of choosing SSE over WebSocket, pinned by a test
- **工作区 `(node_id, path)` 落库**，会话创建即挂载工作区；exec connector（一次性命令、无 PTY）**只经会话通道触发**，connector 跑在设备上，Hub 没有旁路入口 / **Workspaces** are persisted as `(node_id, path)` and mounted when a session is created; the exec connector is reachable only through the session channel — there is no Hub-side bypass.
- **connector 只面向 Web 页面用户，不向 agent 暴露**——本地 agent 完全依赖设备上安装的 agent 自带能力与它自己的权限系统；「工作区根内」只是页面侧的约束 / **Connectors serve the web UI only, never agents.**

## [0.1.0] - 2026-10-17

### Added
- **M0 设备接入里程碑完成并通过验收**：真 `afhub` + 真 `afcli` 端到端 8 条产品承诺全数验证——Hub 启动即 WAL、数据目录 700 / DB 600、未 setup 前设备一律接不上、一次性 setup token、设备一条命令自己外联且 Hub 零入站、密钥对本地 0600 且 Hub 只存指纹、心跳落库且公钥跨合并存活、断开重连同身份且 Hub 重启后 `gen` 仍单调。**M0 device-enrollment milestone completed and accepted**: all eight product promises verified end to end against real `afhub` and `afcli` processes — WAL on boot, 700 dir / 600 db, devices refused before setup, one-shot setup token, one command outbound with zero Hub inbound ports, a local 0600 key pair with only the fingerprint stored, heartbeats persisted with the public key surviving merges, and identity stable across reconnects and Hub restarts with `gen` still monotonic
- **设备接入与身份**：`afcli run` 一条命令（首次自动 enroll）；**密钥对在 node 本地生成、Hub 只存指纹**；supervisor/worker 同二进制双角色（`run --worker`，回滚由未被替换的 supervisor 执行，见 `DECISIONS.md` D1/D2）；单条 WS 主动外联；防重复注册（同一指纹不得新建设备）；`afcli log` 看最近日志 / **Device enrollment and identity**: one `afcli run` command (auto-enrolling on first run); the key pair is generated on the node and the Hub stores only a fingerprint; supervisor and worker are two roles of one binary; a single outbound WebSocket; duplicate-enrollment protection; and `afcli log`.
- **子任务 4 supervisor/worker 双角色**：`afcli run` 是 supervisor，spawn `afcli run --worker`（同一二进制，**角色只由 argv 决定**）；worker 持密钥与 WS，supervisor 只做「拉起 / 看退出码 / 替换 / 回滚」。升级只换 worker，**身份不随进程变**——kill 掉 worker 后 Hub 侧回来的是同一个 `node_id`（进程级测试验证）。回滚由未被替换的 supervisor 执行，所以新版本再坏也坏不到执行回滚的代码里。退出码协议 0/10/11/12，`update-state.json` 有 phase 状态机与 reconcile / **M0 subtask 4 — supervisor/worker roles**: `afcli run` is the supervisor and spawns `afcli run --worker` (same binary, **role decided by argv alone**); the worker holds the key and the WS while the supervisor only spawns, watches exit codes, swaps and rolls back. Upgrading replaces the worker only, so identity survives the process — killing the worker brings back the same `node_id` (verified by a process-level test). Rollback runs from the supervisor that was not replaced, so a bad new build cannot break the code performing the rollback. Exit codes 0/10/11/12 with a `update-state.json` phase machine and reconciliation
- **子任务 5 心跳与能力上报**：node 侧 `auth_ok` 后立即发一次、之后按 Hub 下发的 `heartbeat_interval_ms` 周期发送；Hub 侧更新内存上报（路由读内存不读库）并落库 `capabilities` / `resources` / `last_seen_at` / `status`。**缺省字段保留上一次的值**（一份只含 `node_version` 的心跳不清空能力）。心跳只报「配置的工作区根」，绝不递归、不带任何路径细节 / **M0 subtask 5 — heartbeats**: the node sends one immediately after `auth_ok` and then on the interval the Hub issued in `auth_ok`; the Hub updates its in-memory report (routing reads memory, never the DB) and persists `capabilities` / `resources` / `last_seen_at` / `status`. **Absent fields keep their previous value** — a heartbeat carrying only `node_version` does not clear capabilities. Only configured workspace roots are reported, never recursively, never with path detail
- **子任务 6 Hub 侧离线判定**：扫瞄器每 15s 把「连续心跳丢失 > 45s」的设备标 offline；判据**只看心跳**，不看重连次数。断开时**不清内存上报**——那正是「WS 断了但心跳新鲜 → 重连中，调度仍算可用」这一档存在的前提 / **M0 subtask 6 — Hub-side offline detection**: a scanner marks devices offline after 45s without heartbeats, keying on heartbeats alone and never on reconnect count. Disconnects no longer clear the in-memory report, which is what makes the "socket down but heartbeat fresh → reconnecting, still schedulable" state reachable at all
- **子任务 7 防重复注册与连接顶替**：同 `node_id` + 同指纹的新连接顶掉旧的（先换表、后通知），旧连接收 `disconnect{reason:"superseded", gen}` 并**不再重连**；「不再重连」有 `gen` 兜底——帧丢了也靠 `auth{last_seen_gen}` 让 Hub 回 `auth_fail{superseded}`。兜底判据是「落后于当前**活着**的那条连接」，不是「落后于累计发出过的 gen」（后者会让每一次正常重连都被判成已被顶替）/ **M0 subtask 7 — duplicate enrollment & connection supersession**: a new connection with the same `node_id` and fingerprint displaces the old one (swap the table first, notify after); the old one receives `disconnect{reason:"superseded", gen}` and stops reconnecting. That stop is backed by a `gen` fallback, so a lost frame still ends in `auth_fail{superseded}` via `auth{last_seen_gen}`. The fallback compares against the connection that is **currently alive**, not against the highest gen ever issued — the latter would reject every ordinary reconnect
- **子任务 8 通道与流控**：`(node_id, channel_id)` 分桶复用，每通道独立信用；Hub 是被动方（`open_channel` 到即回 `channel_ready`），信用耗尽由 Hub 补 `channel_credit`。Hub 侧读侧每连接一个按字节计的令牌桶（**先慢读，持续 30s 才断开**——立刻断会误杀 `cat` 大文件这类合法洪峰）；**写侧限流是待办**（M1 接上真实数据面前没有能压满它的流量）。断开时整节点通道记录全部丢弃，这是信用重置的唯一入口 / **M0 subtask 8 — channels & flow control**: `(node_id, channel_id)` multiplexing with per-channel credit; the Hub is the passive side and answers `channel_ready` on arrival, and tops credit back up when it runs out. One hub-side cap that needs no cooperation from the peer: a per-connection read token bucket, metered in bytes, that **slow-reads first and only drops after 30s** (dropping immediately would kill legitimate bursts like `cat`-ing a large file). **Write-side backpressure is tracked as a follow-up** — before the real data plane lands there is no traffic that could fill such a queue. All channel records for a node are dropped on disconnect, which is the only reset for credit accounting
- **Web 控制台设计语言与原型**（原 M1 设计语言里程碑；`M1-web-console-design.md` 标为已放弃，但其产出**没有作废**，已并入实现） / **Web console design language and prototypes** (originally its own milestone, marked abandoned, but its output was kept and consumed by the implementation).
- **单用户登录**：setup token、Argon2id、会话 cookie / **Single-user login** with a setup token, Argon2id and a session cookie.

### Changed
- **通道帧类型列表收敛到一处**：`ws.rs` 的 `is_channel_type` 与 `on_channel_frame` 的 match 把同样六个类型写了两遍，加第七种要改两处而漏改的那处只会静默忽略；现在由 `channels.rs` 单点持有，两侧都读它 / **The channel frame type list now lives in one place**: `ws.rs` and the dispatcher used to list the same six types twice, so adding a seventh meant editing both and the missed one would silently ignore the new frame; `channels.rs` now owns it and both callers read from there
- **通道错误帧统一走 `push` helper**：四处 `to_value(..).unwrap_or_else(|_| json!({}))` 收成一个 helper，附带「为什么兜底成空帧而不是 panic」的理由 / **Channel error frames go through one `push` helper**: four copies of the `to_value` fallback collected, with the reasoning for why an empty frame beats a panic attached
- **`error_frame` 的五个同值分支折成常量**：`ErrorCode` 没有「未知通道」档，而 id 不认识 / 通道没有 / 已关闭对 node 是同一件事；需要区分的是单帧超限与洪峰，那留在读侧限流路径上 / **Five identical match arms in `error_frame` collapsed to a constant**: ErrorCode has no "unknown channel" variant and unknown-id / missing / closed are one thing to the node; the distinction that does matter, oversized frame vs flood, stays on the read-limit path
- **`now_rfc3339` 正名为 `now_stamp`**：输出从来是 `unix:<秒>`，函数名与注释却写着 RFC3339。引 chrono 只为把它改成真的 RFC3339 这件纯装饰的事——本仓没有一处按时间解析这些列，判在线只看心跳年龄 / **`now_rfc3339` renamed to `now_stamp`**: the output has always been `unix:<secs>` while the name and the doc comment said RFC3339. Pulling in chrono purely to make the name true would be decoration — nothing in this repo parses those columns as time; online detection keys on heartbeat age alone
- **`mise` 分出开发与生产两条路**，数据目录默认改为项目级 `data/` / **`mise` separates development from production**, and the data directory defaults to a project-local `data/`.
- **架构决策：任务分发调度**——心跳上报字段（能力/负载/存量三分）、硬过滤→有序打分、可钉死；明确**任务路由与推理路由是两套独立决策** / Architecture decision: task scheduling — heartbeats split into capability / load / inventory, hard filters then an ordered ranking with pinning; task routing and inference routing are explicitly **two independent decisions**
- **架构决策：agent 分发**——**Hub 只下发配置，不向设备写可执行文件**。原先规划的「安装清单复用 CAS」已撤销：让 Hub 往设备写可执行文件等于「一处配置错、全体设备执行未核对代码」，换来的只是省掉用户一条命令。agent 由用户自己装，设备**扫描**出装了什么并上报；Hub 管的是这些 agent 的**启动配置**，设备读 / Architecture decision: agent distribution — **the Hub ships configuration only, never executable files to devices**. The originally planned CAS-backed install manifest was withdrawn: having the Hub write executables onto devices means "one misconfiguration, every device runs unreviewed code", and all it buys is one saved command per user. Users install agents themselves; the device **scans** what is installed and reports it, while the Hub owns each agent's **launch configuration**, read back by the device
- **架构决策：安全与生命周期**——凭据三类分层（设备密钥对 node 本地生成 / connector 凭据与模型 key 由 Hub 明文存储）、supervisor/worker 两层设备模型、掉线指数退避重连 / Architecture decision: security & lifecycle — three credential classes, a two-tier supervisor/worker device model, and exponential-backoff reconnect
- **架构决策：工作区供料与同步**——Hub 内容寻址仓库统一供料项目文件 / 技能配置 / 产物；git 项目只传 commit 引用、非 git 走 manifest 差分；配置走托管覆盖层（只读 + 漂移检测） / Architecture decision: workspace feeding & sync — one Hub content-addressed store serves project files, skill/agent config, and artifacts; git projects transfer only a commit ref while non-git trees sync by manifest diff; config lands as a managed read-only overlay with drift detection
- **终端 / 文件 / git 改为会话内面板**，绑定会话工作区 `(node_id, path)`，生命周期随会话；去掉「对标 Xshell / Xftp」的设计基调 / Terminal, files, and git are now **session-scoped panels** bound to the session workspace `(node_id, path)` with session lifetime; dropped the "benchmark Xshell / Xftp" design stance
- **首个演示（M1）改为**「在树莓派上开会话，会话内 shell 与文件浏览可用」 / First demo (M1) is now "open a session on the Raspberry Pi and use the shell and file browser inside it"
- **前端定选 Vue 3 + Bun + Vite**（`packages/web`），补齐 `bun run` 质量门禁 / Frontend settled on **Vue 3 + Bun + Vite** (`packages/web`) with `bun run` quality gates
- **修正进程模型图**：浏览器从不直连 node，只有「浏览器 ↔ Hub」与「node → Hub」两条链路；此前图示画成浏览器直连节点，与零入站承诺冲突 / **Fixed the process-model diagram**: the browser never connects to a node directly; only "browser ↔ Hub" and "node → Hub" exist. The previous diagram implied browser→node connections, contradicting the zero-inbound promise
- 补齐轨迹事件表（类型 / 输入 / 输出 / 耗时 / 归属节点落成列），修复指向任务连贯性文档的断链 / Added the trajectory event table (type, input, output, duration, owning node as real columns); fixed the broken cross-reference
- 澄清「节点级可恢复」边界：同节点可经 ACP `session/load` 恢复，换设备则标记需重建；补断线重连与通道重建规则 / Clarified the "node-level recoverable" boundary and added reconnect / channel-rebuild rules
- 明确**不做设备间 P2P 直传**，跨节点取文件一律经 Hub 内容寻址仓库 / Made it explicit: no device-to-device P2P transfer; cross-node file access always goes through the Hub CAS
- 统一「第一版」口径 = 里程碑 M0–M4（子 agent 从「后续」移入第一版）；补齐 PRODUCT §3.1 的工作区供料 / 统一配置 / 产物归档三行 / Aligned the "first release" definition to M0–M4 (sub-agent system moved into the first release); completed the PRODUCT §3.1 scope table
- agent 的权限约束改为**两层**（Hub 全局规则 + 会话级加严，后者不可放松全局 deny），由托管覆盖层统一下发，不自研权限引擎 / Agent permissions are now two layers (Hub global rules plus per-session tightening that cannot relax a global deny), delivered through the managed overlay; no permission engine of our own
- 子任务工作区的内容来源在**分发时由用户确认**：`HEAD`（默认）/ `HEAD` + 工作区未提交改动 / CAS `snapshot` / Sub-task workspace source is confirmed at dispatch time: `HEAD` (default), `HEAD` plus uncommitted changes, or a CAS `snapshot`
- **Hub 不做「一键安装 agent 到新机器」**：分发只覆盖配置，不覆盖 agent 软件本体 / **No one-click agent install**: distribution covers configuration only, not the agent binary itself

### Fixed
- **心跳落库不再抹掉库内公钥**：M0 权宜把设备公钥存在 `capabilities.public_key`，而心跳上报的 `capabilities` 结构上没有它——整对象覆盖会让下一次重连找不到库内公钥，设备永久 `identity mismatch`。现在合并时无条件保留该键 / **Heartbeats no longer wipe the stored public key**: the M0 stopgap keeps a device's public key in `capabilities.public_key`, which heartbeats do not carry. Overwriting the whole object used to break every subsequent reconnect with `identity mismatch`; the merge now always preserves that key

### Documentation / 文档
- **产品与技术文档基线**：定位、硬性约束、架构决策（会话与轨迹、任务连贯性、分布式推理）、里程碑规划 / Initial product & architecture docs: positioning, hard constraints, architecture decisions (session & trajectory, task continuity, distributed inference), milestone plan
- **许可定为 Apache-2.0** / License settled as **Apache-2.0**
- **缺陷池录入三条 TODO**：`secret` 表明文存储的加固项、分布式推理路线选型（结论未定）、工作区同步 B/D 档兜底 / Three TODOs entered in the defect pool: encryption hardening, distributed-inference route, sync fallbacks
- **README 改为门面式**：范围/技术选型细节链向 `docs/PRODUCT.md` 与 `docs/DEVELOPMENT.md`，新增文档索引 / README is now a facade: scope and stack link out to the product and development docs, plus a documentation index
- **新增里程碑 M2 工作区供料与同步**，排在 ACP 与 Git 之前 / New milestone **M2: workspace feeding & sync**, placed before ACP and Git
- **记入缺陷池**：设备公钥寄存在 `capabilities.public_key`（无 schema 列，靠 `preserve_public_key` 在每次心跳合并时抢救）——改 `merge_into_stored` 前必须先读该函数，否则所有设备重连一律 `identity mismatch` / Entered in the defect pool: the device public key rides inside `capabilities.public_key` with no schema column, rescued on every heartbeat merge by `preserve_public_key` — read that function before touching `merge_into_stored`, or every reconnect fails `identity mismatch` with a symptom that points nowhere near the cause
- **清空仓库内容，从零重建文档** / Cleared the repository and rebuilt the docs from scratch

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
