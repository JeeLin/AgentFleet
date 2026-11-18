# Lane D — LANE-REPORT

Worktree: `/tmp/wt-r5-D`（未 push、未切分支）。

## 门禁结果（收尾时实测）

| 门禁 | 命令 | 结果 |
|---|---|---|
| 全量测试 | `npx vitest run`（`packages/web`） | **exit 0，60 files / 1284 tests 全绿**（基线 59/1271，净增 1 文件 13 条用例） |
| 类型 | `npx vue-tsc --noEmit` | **exit 0**（0 error） |
| Lint | `npx eslint src --ext .ts,.vue` | **exit 0**（0 error） |

`src/dbg.test.ts` 已确认不存在（早前已 `rm`）。

---

## 逐条记录

约定：每条 🟡/🔴 的「反向验证」都真实跑过（先备份 → 变异 → 看 red → `cp` 还原 → 看 green → `cmp` 确认与备份逐字节一致）。所有变异与还原都记录了命令与输出。

### G29 —— 路由遍历：每个非认证路由都拉左栏三项

**改了什么**
- `packages/web/src/App.shell-state.test.ts`：新增遍历用例「每个非认证路由都照常拉左栏的项目与计数」。`ROUTES.filter(r => !AUTH_ROUTES.has(r.name))` 逐条挂外壳，模式具名段替换成 `x_$1`；断言 `/api/projects`、`/api/devices`、`/api/agents` 三个端点都被打到，缺项聚合进 `missing` 数组、最后统一断言为空。
- 同文件：`anyApi()` 之外新增 `shapedApi()`——遍历用例不能全部回 `[]`，否则 `DeviceDetailPage.vue:311`（`detail.deletable.allowed`）与 `AgentDetailPage.vue:191`（`detail.devices.length`）在首屏渲染抛 `TypeError`，vitest 记为 unhandled rejection 且整个运行 exit 1（实测 exit=1）。给 `/api/devices/x_id/detail` 与 `/api/agents/x_name` 一个字段齐全的响应是**测试夹具修正**，不是产品问题（那两行代码是对的，响应体由后端保证）。

**反向验证（改坏 → 红）**
- 命令：把 `App.vue` 里左栏取数的守卫 `if (!isAuthPage.value && route.value.name !== 'devices')` 收窄成去掉 `route.value.name !== 'devices'` 条件：
  - red：`devices（/devices）缺 /api/projects / /api/agents；实发：/api/devices`
  - 还原后 exit 0，`/usr/bin/git diff HEAD` 与改动集一致。

### G33 —— 弱断言「`expect(fetchMock).toHaveBeenCalled()`」

**改了什么**
- `packages/web/src/domains/session/SessionPage.test.ts`：
  - 新增共享 helper `httpCallsOf()`（路径 + 真实 method + 解析过的 body）与 `callsTo()`，写在 `reqOf()` 旁边。
  - 「已结束的会话」用例（原 `:444` 附近）：弱断言换成逐端点断言（`/api/sessions` GET、`/api/devices` GET 各一次），并新增两条**负向**断言：已结束的会话不发 `POST /api/channels`、不发任何 `/conversation` 请求。页面头「打开面板」按钮的禁用判据也改为直接按 `[data-open-panel]` 取（原来取 `.af-wb__head button`，按钮换类名时这条会静默失真）。
  - git 面板 status 用例（原 `:1888` 附近）：弱断言换成「碰面板之后真的发了一帧 `{"op":"status"}`」——断言 `POST /api/channels` 恰好一次 + `reqOf(sentBodies, 'status')` 深比较为 `{op:'status'}`（git.rs 契约：status 不带别的字段）。

**反向验证（改坏 → 红，四组变异）**
1. `gitKick` 不再触发（`if (state==='ready' && panel==='git') gitKick(tab)` → `void tab`）：
   red `Error: 要发出 status 请求，实发：[]`（旧断言在这组变异下**依然绿**，正是 G33 要修的）。
2. `interact()` 的 `ended` 守卫删掉（已结束的会话也开通道）：旧断言绿；新断言 red `已结束的会话不该建任何通道……expected [ { url:'/api/channels', …(2) } ] to have a length of +0 but got 1`。
3. `[data-open-panel]` 的 `:disabled="ended"` 删掉：red `已结束的会话：入口必须禁用: expected false to be true`（旧选择器 `.af-wb__head button` 拿到的是同一个按钮，所以需要先修选择器才能让这条变异可见）。
4. `onMounted` 里给已结束的会话塞一次 `ensureChannel().open()`：red 同 2。
- 还原后 `cmp` 一致，`SessionPage.test.ts` 125 条全绿。

### G35 —— keynav.test.ts 里的 CSS 变量计数搬家

**改了什么**
- 新建 `packages/web/src/styles/tokens.test.ts`：两条用例——① `design-system.md` 声明的变量数 == `tokens.css` 实测数（两边都 `> 50` 防「0 == 0」）；② 说明注释原位留在 `src/shared/keynav.test.ts`。
- 实测值：文档 64 == tokens.css 64（当前不红）。
- `keynav.test.ts` 里原 `:110` 用例已删、原位留注释指向新家。

**反向验证（变异）**
- 在 `tokens.css` 追加 `:root { --af-card-title-step: 17px; }`：`visual-contract.test.ts` 的「顶层 `:root` 段数」先红（它数的是段数而不是变量数）；**单跑 `src/styles/tokens.test.ts` 仍 2 passed**——因为该变异不改变 `--af-*` 计数之外的守恒式（新增的 `--af-card-title-step` 会同时出现在文档与实现两侧……实际上不会，所以如实记录：**这条用例拦不住「追加一个新变量」这类变异，它守的是「搬家后计数仍相等」这个回归**。要拦「私自加变量」需要文档与实现各自独立取数 + 逐键 diff，超出本条范围，见「未修」）。
- 还原后 `cmp` 一致。

**未修与原因**
- 「逐键 diff」级别的守卫没做：`design-system.md` 是 Lane A/文档侧的地盘（G35 明令不改它的数字），而逐键 diff 需要文档侧给出机器可读的变量清单（现在 markdown 里是手写段落）。已按派工口径只做「搬家 + 计数守恒」，不越权改文档。

### G36 —— BoardPage 对话框的测试补写（含两处必要的产品修复）

**先说清楚：BoardPage.vue 上动了两处（Lane C 的文件）**
触发原因是 G29 的遍历用例挂到 `/project/x_id/board` 时整页崩：
`TypeError: Cannot read properties of undefined (reading 'length') @ BoardPage.vue:117`。
根因是**真实产品缺陷**，不是测试造出来的：
1. `load()` 没有轮次保护：换项目时上一轮仍在飞的 `Promise.all` 回来，会把**旧项目**的数据写进新项目的页面（还可能在 `tasks` 尚为 `null` 时提前渲染）。
2. 模板不是互斥三分支：`loading` 是独立的 `<p>`，数据分支不判 `tasks !== null`，首屏两次渲染会把未初始化的 `tasks` 渲出来。

修复（最小）：
- `BoardPage.vue`：`load()` 加轮次令牌 `let loadToken = 0`，回来时对不上就整份丢掉（连 `loading` 也不许动）；`tasks` 改为 `ref<Task[] | null>(null)`，模板改成 `v-if="loading"` / `v-else-if="failure"` / `v-else-if="tasks !== null"` 三分支互斥；`byState` / `boardEmpty` 用 `tasks.value ?? []`。
- 改后 `src/App.shell-state.test.ts` + `src/domains/project` 共 12 files / 207 tests 全绿。

**测试补写（BoardPage.test.ts，+4 条用例）**
- `create_dialog_open_and_cancel`：打开是空表单（三处输入为空）、选「不设判定码」时当场显示「需人工验收」、取消后对话框消失。
- `create_dialog_submits_and_reloads`：POST 打到 `/api/projects/p_1/tasks`，请求体深比较 `{ title: <去空白>, done_when: { criteria, description } }`（按 `CreateTaskBody` 的形状）；成功后对话框关闭 + 任务列表被**重新 GET**（断言恰好 2 次 GET，防「本地插入一张卡」蒙混）；板上渲染的是后端回的那一份。
- `create_dialog_keeps_the_form_open_and_shows_the_code`：POST 400 `invalid_criteria` → 对话框留在原地、错误条（`.af-form__error`，role=alert）里带**后端错误码**、提交按钮回到可点（`creating` 在 `finally` 复位）。
- 原 `create_dialog_requires_criteria_or_description` 保留不动。

**反向验证（三组变异，全部先 `cp` 备份）**
1. 成功分支删掉 `await reloadTasks()`（只关对话框、不重拉）：red `建成功后要重拉任务列表（不使用本地插入的卡）: expected 1 to be 2`。
2. 失败文案不再带错误码（`建任务失败（${e.code}）` → `建任务失败`）：red `…to contain 'invalid_criteria'`。
3. `openDialog` 不再清空表单（塞进一个「上一次留下的标题」）：red `expected '上一次留下的标题' to be ''`。
- 还原后 `cmp` 一致，26 passed。

**G36 的另一半（SessionPage 的两个未测区）**
- `SessionPage.vue:1162-1184`（`onTrajMore`）：+2 条用例（见下）。
- `SessionPage.vue:2114-2154`（`filesGoto` / `filesOpen` / `filesRevert`）：+3 条用例（见下）。
- 测试基建：`SessionPage.test.ts` 新增 `enterRow()` helper——**必须走真 ⏎ 键**。树行的激活不是 `@click`（`FilesPanel` 的键盘处理器统一分派，`row.click()` 静默无效果），写错入口会让「页面回调没接线」和「测试按错了地方」不可区分。

**onTrajMore 用例与反向验证**
- 用例 1：第一页给满 200 条（`getTrajectory` 按 `events.length >= limit` 判 `hasMore`），点「加载更多」后断言第二条请求是 `after=1199`（最后一条的**物理 id**，不是 `events.length`=200）；第二页回 3 条（含重复的 1199）→ 列表恰 202 行、末尾是 1201、无重复 seq。
  - 变异 (a) 游标改成 `ws.events.length`：red `游标必须是最后一条事件的物理 id: expected '…after=200…' to contain 'after=1199'`。
  - 变异 (b) 去掉按 id 去重：red `重复的 id 不能画两遍: expected 203 to be 202`。
- 用例 2：第二页 500 → 抽屉里出现「读不到轨迹」（role=alert），「加载更多」回到可点（`trajLoading` 在 `finally` 复位）。
  - 变异 (c) `onTrajMore` 的 `catch` 段吞掉错误：red `取失败必须在抽屉里说出来: expected '…' to contain '读不到轨迹'`。

**files 区用例与反向验证**
- `filesGoto`：从根走到 `src`（借一个 `kind:'other'` 行触发 `filesOpen`——⏎ 在目录行上是「展开/收起」，不是进入；能改 cwd 的只有 link/other/unknown/越界四类行），再往里一层，然后点第 1 层面包屑 → 面包屑层数 3→2、点根 → 1。**判据只能是面包屑**：树的 `rows` 跟折叠状态走、不跟 cwd 走，「树上有哪些行」证明不了 cwd 换没换。
  - 变异：删掉 `ws.cwd = ws.cwd.slice(0, depth)` → red `点第 1 层要退回 src，不能停在 src/lib: expected 3 to be 2`。
- `filesOpen`：树上回一帧带 `..` 与 `/etc`（都标 `data-outside="true"`）；⏎ 点 `..` → 出现 `[data-nav-error]`（含「跨出了工作区」），`sent` 里没有新的 list 请求、面包屑仍 1 层。
  - 变异：删掉 `isOutsideSeg(last)` 分支 → red `越界节点点了要有一条解释: expected null not to be null`。
- `filesRevert`：打开 a.txt、改一笔、保存 → 设备回 `ok:false` → `[data-write-error]` 出现；点「撤销」（`[data-revert]`）→ 红字消失、tab 脏标消失。这条守的是**页面模板里 `@revert="filesRevert"` 真的接线**。
  - 变异：`filesRevert` 函数体清空 → red `撤销之后那块红字要消失: expected <div…> to be null`。

**未修与原因（如实记录）**
- `filesRevert` 的「只清**这一个文件**」这一半守不住：`writeError` 是单例（`FilesError | null`），一次只有一个文件有红字，「无条件清」与「按路径清」在现有数据形状下**不可区分**（实测把判据放宽成 `if (ws) ws.writeError = null` 之后该用例仍绿）。要让它可判需要产品侧先支持多个文件的写错态。用例名与注释已如实改为「撤销把写错态清掉（`@revert` 真的接线了）」。
- `onTrajMore` 失败后按钮可点的断言写成两支：错误态下抽屉若不再渲染这个按钮（`v-if="!error"` 分支）也算合法收尾，只禁「按钮还在但 disabled」这种最糟组合。

### G50 —— 注释与实现不符（App.vue 的 NAV_GROUPS 说明）

**改了什么**
- `App.vue` 文件头注释与 `NAV_GROUPS` 上方注释改为实话：
  - 原型 `shell.js:17-46` 是 **5 组**（`:17` 全站 / `:23` 项目 / `:26` 收件箱 / `:30` 管理面 / `:42` 通知），实现是 **3 个常量分组 +「项目」组由 `railProjects` 渲染**，附逐组差异表（管理面去掉的三项是项目内路由；通知合并进管理面的 `/settings/notifications` 与 `/butler`）。
  - 项目内路由的枚举补全为 `router.ts:129-140` 的 **7 条**（看板 / 任务 / 执行 / 复盘 / 时间线 / 会话 / 项目设置），并说明 `新建项目`（组标题右侧的 `+` 动作）与 `绑定设备` / `定时任务`（项目页头 / 项目设置里的项目内导航）这三个非列表入口。
- `App.test.ts:19` 的引言与用例名同步（原文写着「四组」）。

**反向验证**
- 纯注释 + 文案用例名改动，无行为面；验证方式是全量三件门禁仍绿 + `App.test.ts` 里那条「引言」用例仍通过（它断言的是注释与用例名同步）。无「改坏→红」可言（注释不能变异出红），如实记录为 **N/A**。

### G55 —— 两份逐字重复的 App 测试脚手架

**改了什么**
- 新建 `packages/web/src/test/app-shell.ts`，导出：`flushShell` / `mountShell(path, options?)` / `EMPTY_SHELL_API` / `railOf` / `railToggleOf` / `clickRailToggle` / `goTo` / `calledUrls` / `sentBodiesOf` / 类型 `ShellWrapper`。
- `App.test.ts` 与 `App.shell-state.test.ts` 改 import（各自用别名 `flush` / `mountApp` / `EMPTY_API` 保持用例体不动）；删除两份逐字重复的 `flush` / `mountApp` / `EMPTY_API` / `railOf` / `clickTopbarRail` / `go`。
- `App.test.ts` 原来内联的「剥 CSS 注释」正则改用已有的 `stripCssComments`。

**反向验证**
- 重构等价性验证：全量 vitest 两次全绿（重构前后各一次，计数一致）；`git diff` 只显示 import 与删除块。行为面用例（G10 左栏收起、G12 认证页不发请求）本身就有变异守卫，不重复跑。

### G63 —— `NAV_ICON` 恒等表 + `?? 'overview'` 兜底

**改了什么**
- `App.vue`：删掉 `NAV_ICON` 表（7 条全是 `键: 键` 恒等映射）与模板里的 `NAV_ICON[item.match[0]] ?? 'overview'`。
- 新增 `interface NavItem { readonly icon: NavIconKey }` / `interface NavGroup`；`NAV_GROUPS` 用 `as const satisfies readonly NavGroup[]`，每个 item 挂 `icon`；模板改 `<NavIcon :name="item.icon" />`；`navItemsOf` 返回 `readonly NavItem[]`。
- 漏写 `icon` 现在是**编译错误**而不是「静默画成总览的图标」。

**反向验证（两组变异）**
- (a) 把 devices 的 `icon` 改成 `'overview'`：red `导航项「设备」画成了别的图标（期望 devices 的形状）`。
- (b) 还原旧实现（`NAV_ICON[item.match[0]] ?? 'overview'` + 表里漏 `settings`）：red `导航项「设置」画成了别的图标`——这正是旧实现放走的那类静默缺陷，现在测试能拦。
- 还原后 44 passed（`App.test.ts`）。

**附加守卫**
- `App.test.ts` 新增「项目内页面不进左栏」用例：挂在看板页时，左栏 7 个非项目列表项没有任何一个 `aria-current="page"`（防「深链也高亮侧栏」回归）。
- 「图标条」用例加了逐项形状断言（引入 `NAV_ICON_PATHS`），补上「所有图标都画成 overview 也全绿」这一空白。

### G66 —— 骨架页页头几何（10 个 skeleton 页）

**改了什么**
- `packages/web/src/domains/shared/components/SkeletonPage.vue`：默认槽里**不再包那层 `<div>`**——`h1` + 说明句现在是 `.af-head`（`display:flex; align-items:center; min-height:56px`）的直接 flex 子项。
- `packages/web/src/styles/components.css`（本 lane 权限内）：`.af-skeletonpage__note` 从「块级、`margin-top`、`--af-text-sm`」改为「单行 + 省略号 + 0 顶距 + `--p-t-xs`」——与 `.af-head__sub` 同一几何档，不再把单行页头撑成两行。

**几何实测（playwright，脚本 `/tmp/pw/geom2.mjs` + `/tmp/pw/geom.mjs`，vite dev @ 3002，chromium）**
- 10 个骨架路由（execution / replay / timeline / schedules / inbox / inbox-activity / butler / settings×3）全部：`headH = 56`、`h1H = 22`、`h1top = 1.4`、`h1clipped = false`、说明句与标题同列起点（`itemLeft == h1Left == 248`）、无一处越出页头。
- 对照组：`/devices`（普通单行页头）`headH = 56`、`h1top = 16.5`；`/agents`、`/settings`（带包裹列的双档页头）`headH = 70.8`、`h1top = 0`。
- 改前（`/usr/bin/git show HEAD:…SkeletonPage.vue` 还原后实测 `/tmp/d-geom-old.json`）：结构是「h1+note wrapped in div」（`.af-head > div` 的 column 布局），说明句竖排在标题下面、字号 13.5px（≠ `.af-head__sub` 的 12.5px）；改后结构是「h1+note are direct flex children」，说明句横排、字号 12.5px，页头仍是单行 56px。
- 结论：改后骨架页页头与普通页**同档**，标题不再被「包裹列 + align-items:center」的旧结构挤压。**这不是伪装成单测的浏览器断言**——几何数值来自真实浏览器读数，读数原文在 `/tmp/d-geom-new.json`（改后）与 `/tmp/d-geom-old.json`（改前），脚本在 `/tmp/pw/`。

**反向验证**
- 结构变异（把包裹 `<div>` 加回去）：`skeleton-pages.test.ts` 不红（它只守「version 渲染出来」「文案不是 TODO」这类源码级判据，不守 DOM 嵌套）——这正是为什么 G66 需要浏览器实测而不是只加单测。变异后用 `geom2.mjs` 实测 `structure` 字段从 `direct flex children` 退回 `wrapped in div`、说明句字号回到 13.5px，确认读数脚本确实在测这件事；随后还原。

### G68 —— `.af-body` 与 `.af-fold__sum` 的 `:focus-visible`（Lane A 权限，只出补丁）

**为什么不能自己落地**：两个选择器都定义在 `packages/web/src/styles/prototype-shell.css`（Lane A 红线），本 lane 不动它。以下是精确补丁与证据。

**证据（playwright 实测，脚本 `/tmp/pw/focus-final.mjs`，读数 `/tmp/d-focus-readings.json`）**
- 键盘 Tab 真实到达 `.af-fold__sum`（SUMMARY 原生 tabbable）时：
  - warm：`outline: auto 1px rgb(16,16,16)`，offset 0
  - neutral：`outline: auto 1px rgb(16,16,16)`，offset 0
  - 即**浏览器默认环**，没有吃到设计系统的 `--af-focus-ring`。原因：`tokens.css:361` 的全局规则是 `:where(a,button,input,select,textarea,[tabindex])`，`summary` 与没有 `tabindex` 属性的 `.af-body` 都不在列表里（实测 `matches(':where(...)') === false`）。
- 对照组（`.af-btn`，BUTTON 在列表里）：`outline: solid 2px rgb(194,164,105)`（warm `--p-acc #c2a469`）/ `rgb(138,176,234)`（neutral `--p-acc #8ab0ea`），offset 2px —— 全局环工作正常。
- 对比度（WCAG 相对亮度计算，见会话记录里的脚本）：
  - 默认 `#101010` on warm fold 面 `#242320`：**1.21:1**；on neutral `#202529`：**1.23:1**（远低于 3:1 非文本对比度线）。
  - 补丁后 `--p-acc` on `--p-surf-1`：warm `#c2a469`/`#242320` = **6.59:1**；neutral `#8ab0ea`/`#202529` = **6.98:1**。✅ 均 ≥ 3:1。
  - 补丁后 on 页面底色：warm 7.15:1、neutral 7.65:1。
- 附带发现（不属 G68、未动手）：`OverviewPage` 的管家抽屉关着时其 Teleport 容器保持 `inert`（只有帮助面板 `?`/Esc 或抽屉开合的 pop 路径清过），初次加载后 `.af-fold__sum` 因此不可 Tab 达；关掉常驻的 `LoginPrompt`（测试环境里 session 弹窗开着）之后可达。这条涉及 `shared/focus.ts`（Lane C）与 `OverviewPage.vue`（Lane C），留给编排者。

**需要 Lane A 落地的补丁（精确 oldText → newText）**

`packages/web/src/styles/prototype-shell.css`，`.af-body` 三态那一行（当前约 `:356`）：

```css
/* oldText */
.af-body { flex: 1; min-height: 0; overflow: auto; }

/* newText */
.af-body { flex: 1; min-height: 0; overflow: auto; }
/* G68：`.af-body` 不在 tokens.css 的全局焦点环列表里（没有 tabindex 属性、
   不是表单元素），键盘到不了环。它本身不进 Tab 序（tabindex=-1 的语义留给
   `main[tabindex="-1"]`），但如果哪一页把它做成可滚动焦点宿主（程序化 focus），
   环要跟设计系统一致，而不是浏览器默认的 1px 黑。 */
.af-body:focus-visible { outline: var(--af-focus-ring); outline-offset: var(--af-focus-offset); }
```

`packages/web/src/styles/prototype-shell.css`，`.af-fold__sum`（当前约 `:601`）：

```css
/* oldText */
.af-fold__sum:hover { color: var(--p-fg-strong); }

/* newText */
.af-fold__sum:hover { color: var(--p-fg-strong); }
/* G68：`summary` 不在 tokens.css 的 :where(a,button,input,select,textarea,[tabindex])
   列表里，全局焦点环到不了它——实测键盘聚焦时只有浏览器默认的 `outline:auto 1px`
   （对 warm `#242320` / neutral `#202529` 的对比度是 1.21:1 / 1.23:1，远低于 3:1）。
   这里补上与全局同一条环：warm `#c2a469` 6.59:1、neutral `#8ab0ea` 6.98:1。 */
.af-fold__sum:focus-visible { outline: var(--af-focus-ring); outline-offset: var(--af-focus-offset); border-radius: var(--af-radius-sm); }
```

（`--af-focus-ring` / `--af-focus-offset` 已在 `tokens.css:243-244` 定义，无需新增变量。）

---

## 需要其他 lane 落地的补丁汇总

### 需要 Lane A 落地
1. **G68 的两段 CSS**（上文精确 oldText→newText）。不改 `tokens.css`（变量已齐）。
2. （可选，非本 lane 职责）`design-system.md` 的变量清单若要支撑「逐键 diff」级别的守卫，需要给出机器可读格式——见 G35「未修」。

### 需要 Lane C 落地
- 无（BoardPage.vue 的两处修复已在本 lane 以「最小必要」名义落地并在上文说明理由；Lane C 若对 `load()` 的轮次令牌或三分支模板有自己的方案，可等价替换，门禁三件与 G29 遍历用例会立刻给出红/绿反馈）。
- G65 补丁待编排者合并：`/tmp/wt-r5-C/LANE-REPORT.md` 在本 lane 收尾时**不存在**（多次 `ls` 确认，最后一次在提交前）。按约定如实记录，不虚构补丁内容。

---

## 未修与原因（逐条汇总）

| 条目 | 未修的部分 | 原因 |
|---|---|---|
| G35 | 逐键 diff 级守卫 | 需要文档侧机器可读清单，`design-system.md` 不在本 lane 红线内可改的范围 |
| G36 | `filesRevert`「只清这一个文件」不可判 | `writeError` 是单例，数据形状决定两条实现不可区分（已实测）；用例名与注释已如实改 |
| G36 | `onTrajMore` 失败后「按钮可点」断言是两支 | 错误态下不渲染按钮也是合法分支，只禁「在但 disabled」 |
| G33 | 无 | 两条弱断言都已换成具体断言且有变异证据 |
| G66 | 几何门禁未做成单测 | 断言几何只能在真实浏览器量（scoped CSS + 布局引擎），已交 playwright 脚本与读数，未伪装成单测 |
| G68 | 两段 CSS 未落地 | `prototype-shell.css` 是 Lane A 红线；补丁与对比度读数齐备 |
| （附带）| OverviewPage 管家抽屉关着时容器保持 `inert`，影响 Tab 可达性 | 涉及 `shared/focus.ts` / `OverviewPage.vue`（Lane C），本 lane 未动；证据见 G68 一节 |

## 提交

`git add -A` + `/usr/bin/git -c user.email=dev@local -c user.name=dev commit`（不含 push；两个 `node_modules` 符号链接在提交前已从 index 排除——它们是绝对路径软链，进库只会给别的 worktree 埋雷）。
