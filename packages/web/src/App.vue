<script setup lang="ts">
/* ===========================================================================
   App.vue —— 应用外壳（规范 §5.1）
   ---------------------------------------------------------------------------
   布局：左栏 + 主区（`prototype/index.html:12` 的 `.af-shell`）。原型**没有全局
   顶栏**，也没有全局状态条——页头 `.af-head` 属于页面自己。左栏分四组：
   全站 / 项目 / 收件箱 / 管理面（与 `prototype/assets/shell.js:19-49` 的 `NAV` 同构）。

   ── 这个文件负责的三件事 ─────────────────────────────────────────
   ① 左栏的 chrome；② 键位帮助面板（`?` 由 `KEYS` 渲染，不手写）；
   ③ **跳转后的焦点落点**：`main` 每次换页都拿一次焦点（§4.3）。
   页面自己不管外壳，也不重新声明 `main`。

   ④ **session 过期时的重新登录框**（`LoginPrompt`）：401 由 `shared/http.ts`
   弹框、由这里挂载与「取消时跳 /login」。这是外壳而不是页面的事——跳转是
   一个策略（见 `onLoginCancel` 的注释）。

   ⑤ **整页跳转的唯一出口**（`fullPageNavigate`）：403 `hub_not_initialized`
   的跳转也住在这里（`setNotInitializedHandler`），所以 `src/shared/` 里没有第二个
   会整页跳转的地方。

   ── 收起态为什么不是「缩成图标条」 ───────────────────────────────
   `⌃B` 按**原型**的机制把左栏整体滑出屏幕（`html[data-rail="off"]`），
   导航仍在 DOM 里、可由品牌行那颗 `.af-navbtn` 唤回。原型
   `assets/app.css:1212-1227` 就是这么做的，并且写明了理由：13 个导航项里
   只有 3 个项目色点有形状，其余 9 项是抽象概念——48px 里只能写汉字首字，
   「设备」和「设置」都变成「设」，那个方案当场就不成立。收起只为一件事：
   读长内容时把注意力让出来。
   =========================================================================== */

import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import LoginPrompt from '@/domains/shared/components/LoginPrompt.vue'
import { listProjects, type Project } from '@/domains/project/api'
import { pop, push, topContainer, trapTab } from '@/shared/focus'
import { SCOPE_LABEL, KEYS, hintOf, missingActs, onHandlersChanged, PREFIX_LEAD } from '@/shared/keynav'
import { logout } from '@/shared/auth'
import { setNotInitializedHandler } from '@/shared/http'
import { loginPromptOpen, dismissLoginPrompt } from '@/shared/login-prompt'
import { pageTitle } from '@/shared/page-titles'
import { bindShell, installShellKeys } from '@/shared/shell-keys'
import { interceptLink, legacyTarget, navigate, route, startRouter, LOGIN_PATH } from '@/router'

import AgentCatalogPage from '@/domains/agent/AgentCatalogPage.vue'
import AgentDetailPage from '@/domains/agent/AgentDetailPage.vue'
import ArchivedPage from '@/domains/project/ArchivedPage.vue'
import BindPage from '@/domains/project/BindPage.vue'
import BoardPage from '@/domains/project/BoardPage.vue'
import ButlerPage from '@/domains/butler/ButlerPage.vue'
import DeviceDetailPage from '@/domains/device/DeviceDetailPage.vue'
import DevicesPage from '@/domains/device/DevicesPage.vue'
import ExecutionPage from '@/domains/project/ExecutionPage.vue'
import InboxPage from '@/domains/inbox/InboxPage.vue'
import ActivityPage from '@/domains/inbox/ActivityPage.vue'
import IntegrationsSettingsPage from '@/pages/IntegrationsSettingsPage.vue'
import LoginPage from '@/pages/LoginPage.vue'
import NewProjectPage from '@/domains/project/NewProjectPage.vue'
import NotificationsSettingsPage from '@/pages/NotificationsSettingsPage.vue'
import OverviewPage from '@/domains/project/OverviewPage.vue'
import ProjectSettingsPage from '@/domains/project/ProjectSettingsPage.vue'
import ProjectSchedulesPage from '@/domains/project/ProjectSchedulesPage.vue'
import ProjectTaskPage from '@/domains/project/ProjectTaskPage.vue'
import ReplayPage from '@/domains/project/ReplayPage.vue'
import SessionPage from '@/domains/session/SessionPage.vue'
import SettingsPage from '@/pages/SettingsPage.vue'
import SetupPage from '@/pages/SetupPage.vue'
import SkillsSettingsPage from '@/pages/SkillsSettingsPage.vue'
import TimelinePage from '@/domains/project/TimelinePage.vue'
import { AUTH_ROUTES } from '@/router'

/**
 * 页面组件表。**与 `router.ts` 的 `ROUTES` 一一对应**（v0.7.0 子任务 8）。
 *
 * 两张表分开是故意的：`ROUTES` 知道「什么地址是什么页」，这张表知道
 * 「那个页是哪个组件」。合一会让 `router.ts` 依赖每一个页面组件，
 * 而它现在是一个纯函数模块（好测、不拉组件树）。
 *
 * 代价是**必须同步**：加了路由不加这一行，`page` 会是 `undefined`，
 * 页面区一片空白。所以 `router.test.ts` 有一条用例逐条比对两张表的键集。
 */
const PAGES = {
  overview: OverviewPage,
  devices: DevicesPage,
  'device-detail': DeviceDetailPage,
  agents: AgentCatalogPage,
  'agent-detail': AgentDetailPage,
  settings: SettingsPage,
  'projects-new': NewProjectPage,
  'projects-archived': ArchivedPage,
  board: BoardPage,
  'project-task': ProjectTaskPage,
  execution: ExecutionPage,
  replay: ReplayPage,
  timeline: TimelinePage,
  session: SessionPage,
  'project-bind': BindPage,
  'project-settings': ProjectSettingsPage,
  'project-schedules': ProjectSchedulesPage,
  inbox: InboxPage,
  'inbox-activity': ActivityPage,
  butler: ButlerPage,
  'settings-skills': SkillsSettingsPage,
  'settings-integrations': IntegrationsSettingsPage,
  'settings-notifications': NotificationsSettingsPage,
  login: LoginPage,
  setup: SetupPage,
} as const

const page = computed(() => (route.value.name === 'not-found' ? null : PAGES[route.value.name]))

/**
 * 换页的 key。
 *
 * **要把所有具名段都算进去**，不只 `id`：看板与项目任务是不同的页但共享
 * `/project/:id` 前缀；`/project/:id/session/:sessionId` 换会话时也必须重建
 * 组件（否则上一个会话的通道句柄会留在内存里）。只拼 `id` 时，
 * 「同一个项目里换会话」不会触发重建——实测过，症状是新会话显示旧输出。
 */
const pageKey = computed(() => {
  const p = route.value.params
  const parts = Object.keys(p)
    .sort()
    .map((k) => `${k}=${p[k]}`)
    .join(',')
  return `${route.value.name}:${parts}`
})

/* ---------------------------------------------------------- 已删除的旧地址 */

/**
 * 撞上一条**已删除但有去处**的旧地址时，重定向过去（`/agents` →
 * `/devices?view=agents`，表在 `router.ts` 的 `LEGACY_PATHS`）。
 *
 * **用 `replace`**：用户从来没有「访问过」那个 404，让后退键回到一条已经
 * 被取代的地址只会再弹一次重定向。这与 `navigate` 里「新建会话成功后用
 * `replace`」是同一条理由。
 *
 * 判据放在这里而不是 `matchRoute` 里：解析是纯函数（好测、无副作用），
 * 改历史记录是策略。**`onMounted` 与换页 watch 都要调**——首屏刷新直接
 * 落在旧地址上时不会触发 watch（初始值不触发）。
 */
function redirectLegacy(): void {
  const target = legacyTarget(route.value.path)
  if (target) navigate(target, { replace: true })
}

/* ---------------------------------------------------------------- 认证页 */

/**
 * 当前是不是「认证流程」页（`/login` / `/setup`）。
 *
 * 判据是 `router.ts` 的 `AUTH_ROUTES`，不是在这里写一遍
 * `name === 'login' || name === 'setup'`：那样加第三个认证页时会漏改一处，
 * 症状是「登录页里出现了侧栏导航」——而那时用户还没登录。
 *
 * 认证页**不带外壳**（§5.1 最后一条「登录前没有导航可言」）：侧栏、顶栏、
 * 状态条全部不渲染。状态条尤其不能留——它写着「Hub 已连接」，
 * 而此刻 Hub 可能正回着 403 `hub_not_initialized`。（v0.8.1 起全局状态条与
 * 全局顶栏本身也已按原型删除，状态那几行落在左栏底部 `.af-rail__foot` 里，
 * 随左栏一起不渲染。）
 */
const isAuthPage = computed(() => AUTH_ROUTES.has(route.value.name))

/* ------------------------------------------------------------------ 左栏 */

/**
 * 一级导航。**分组与原型 `prototype/assets/shell.js:19-49` 的 `NAV` 同构**：
 * 全站 / 项目 / 收件箱 / 管理面（+ 项目列表）。
 *
 * 标签与键位提示都从 `keynav.ts` / `page-titles.ts` 读：手抄一份的话，改了
 * 键位表而忘了改这里，侧栏就会显示一个按了没反应的键（症状与「改了 `PREFIX`
 * 而侧栏没跟着变」完全一样）。
 *
 * # 为什么分组是这四组
 *
 * 原型把「你在哪」与「你去哪」分成两层：组标题是层级语言，列表项是入口。
 * - **全站**：总览是「做事」的入口；已归档是「找得回来」的那个门。
 * - **项目**：每个项目一项，数据来自**既有**的 `listProjects`（与总览页同一个
 *   端点、同一套读法），不新增请求端点。项目是主轴，所以它在中间。
 * - **收件箱**：待决策与动态——「等你决定的事」不在项目里，它横跨项目。
 * - **管理面**：设备 / Agent / 设置。它们回答「机器怎么样」，不是「项目怎么样」
 *   （`AGENTS.md` §1「做事从项目进，管理从设备进」）。
 *
 * # 项目内页面不进左栏
 *
 * 看板 / 任务 / 执行 / 绑定 / 项目设置**不是「地方」，是「在某个项目里」**。
 * 它们从某个项目的条目点进去（条目高亮由 `isCurrentProject` 判断）。
 * `新建项目` 是**动作**，挂在「项目」组标题右侧的 `.af-rail__add` 上。
 */
const NAV_GROUPS = [
  {
    title: '全站',
    items: [
      { to: '/', label: pageTitle('overview'), act: 'nav:overview', match: ['overview'] },
      {
        to: '/projects/archived',
        label: pageTitle('projects-archived'),
        act: '',
        match: ['projects-archived'],
      },
    ],
  },
  {
    title: '收件箱',
    items: [
      { to: '/inbox', label: pageTitle('inbox'), act: '', match: ['inbox'] },
      { to: '/inbox/activity', label: pageTitle('inbox-activity'), act: '', match: ['inbox-activity'] },
    ],
  },
  {
    title: '管理面',
    items: [
      { to: '/devices', label: pageTitle('devices'), act: 'nav:devices', match: ['devices', 'device-detail'] },
      { to: '/agents', label: pageTitle('agents'), act: 'nav:agents', match: ['agents', 'agent-detail'] },
      {
        to: '/settings',
        label: pageTitle('settings'),
        act: 'nav:settings',
        match: ['settings', 'settings-skills', 'settings-integrations', 'settings-notifications'],
      },
    ],
  },
] as const

/** 左栏的全部分组标题（含「项目」）。断言「4 组」时用的是这一份。 */
const RAIL_GROUPS = ['全站', '项目', '收件箱', '管理面'] as const

/** 一个分组里的导航项。分组名不在这张表里时返回空数组（模板不自己判）。 */
function navItemsOf(title: string): readonly { to: string; label: string; act: string; match: readonly string[] }[] {
  return NAV_GROUPS.find((g) => g.title === title)?.items ?? []
}

/**
 * 左栏「项目」组里那一列。数据来自既有的 `listProjects`（`domains/project/api`），
 * 与总览页同一个端点，**没有新增请求**。
 *
 * 失败时**什么都不显示**：左栏是导航，不是状态面板——那里弹一条读取失败会把
 * 「今天做什么」的注意力拉走，而总览页本身会给出完整的失败态与重试。
 * 收起态下它读不到也不影响用户操作（`.af-rail` 收起后整体滑出屏幕）。
 */
const railProjects = ref<Project[]>([])

async function loadRailProjects(): Promise<void> {
  try {
    railProjects.value = await listProjects()
  } catch {
    // 左栏不是错误展示面（见上）。总览页的 `EmptyState` 才是。
    railProjects.value = []
  }
}

/**
 * 当前项目 id（路由里有 `:id` 时）。
 *
 * 判据取 `route.value.params.id`，**不另写一份路径匹配**：同一判据两处实现
 * 必然漂开。会话 / 看板 / 任务 / 绑定 / 设置页都带这个段。
 */
const currentProjectId = computed(() => route.value.params.id ?? '')

/* --------------------------------------------- 左栏：会话页自动收起 */

/**
 * 左栏的用户意图。**只由用户写**（`⌃B` / 品牌行那颗 `.af-navbtn`）。
 *
 * ── 为什么「自动收起」不能只是另一个布尔 ──────────────────────────
 * 写成「在会话页里就强制收起」（`autoCollapsed ? 'collapsed' : rail`）时，
 * 会话内的手动展开会被同一个 computed 立刻盖回去，症状是「开关点了没反应」。
 *
 * ── 为什么这次是「以当前可见态为基准翻转」而不是「清标志 + 翻 rail」 ──
 * 后者看起来更干净，但它有一个坏交互：左栏被自动收起成 `collapsed`，用户点
 * 开关 → 清标志、`rail` 由 `open` 翻成 `collapsed` → 可见态**还是** `collapsed`。
 * 用户要的是「展开」，得到的是「没动」，得点第二下才展开。实测踩过。
 *
 * 所以 `toggleRail` 一律以**用户眼前看到的**为基准翻转（见其注释），
 * `rail` 因此始终等于「用户此刻要的样子」——离开会话页时直接恢复它就对了。
 *
 * 取值仍是 `open` / `collapsed`（测试锁着这个契约），但它不再写成节点的
 * `data-rail` 属性：原型把状态放在 **`<html data-rail="off">`** 上，
 * 由 `html[data-rail="off"] .af-shell` 把整列滑出去（见下面的 `watch`）。
 */
const rail = ref<'open' | 'collapsed'>('open')

/**
 * 会话页里左栏被自动收起。唯一写入者是下面那个路由 `watch`。
 *
 * 与 `rail` 分开是因为两者回答不同的问题：`rail` 是「用户要什么」，这里是
 * 「这一页要不要先替他收起来」。合成一个字段就无法回答「用户手动拨过之后，
 * 路由还该不该覆盖他」。
 */
const autoCollapsed = ref(false)

const effectiveRail = computed<'open' | 'collapsed'>(() =>
  autoCollapsed.value ? 'collapsed' : rail.value,
)

/**
 * 翻转侧栏。
 *
 * **基准是当前可见态 `effectiveRail`，不是 `rail`。** 两者在会话页里会分叉
 * （自动收起让可见态是 `collapsed`，而 `rail` 可能还是 `open`）：以 `rail`
 * 为基准，用户点「展开」实际翻到 `collapsed`，视觉上毫无反应。
 *
 * 翻转之后写回 `rail`：那就是用户此刻的意图，离开会话页时恢复它。
 */
function toggleRail(): void {
  const next = effectiveRail.value === 'open' ? 'collapsed' : 'open'
  autoCollapsed.value = false
  rail.value = next
}

/**
 * 进会话工作台自动收左栏，离开会话页时这条自动收起失效。
 *
 * 它**不写 `rail`**：离开会话页恢复的是用户自己的偏好，而偏好从没被这里改过。
 *
 * 判据取 `route.value.name`，**不另写一份路径匹配**：同一判据两处实现必然
 * 漂开（D10 的教训是 tab 的「视图切换器」语义被实现成了两个真相源）。
 *
 * `immediate` 是必须的：深链直接打开 `/sessions/<id>` 时也要收起，
 * 否则「从列表点进去」与「粘贴链接进来」得到两种外壳。
 */
watch(
  () => route.value.name === 'session',
  (inSession) => {
    autoCollapsed.value = inSession
  },
  { immediate: true },
)

/**
 * 收起态落到 **`<html data-rail="off">`**（原型的机制，`app.css:1212-1227`）。
 *
 * 为什么必须写在 `<html>` 上而不是外壳节点上：原型的选择器是
 * `html[data-rail="off"] .af-shell { grid-template-columns: 0 1fr; }` +
 * `html[data-rail="off"] .af-rail { transform: translateX(-100%); visibility: hidden; }`
 * ——收起时整列滑出屏幕、内容区吃满，不留图标条。选择器挂在 `html` 上，
 * 属性写在别的节点上就一条都不生效（症状是「点了没反应」）。
 *
 * **展开时删掉属性而不是写 `"on"`**：原型同样如此（`delete dataset.rail`），
 * 于是「展开」是缺省态，CSS 里不需要第二条 `[data-rail="on"]` 规则。
 */
watch(
  effectiveRail,
  (state) => {
    if (typeof document === 'undefined') return
    if (state === 'collapsed') document.documentElement.dataset.rail = 'off'
    else delete document.documentElement.dataset.rail
  },
  { immediate: true },
)

/** 一条导航项是否当前页。 */
function isCurrent(match: readonly string[]): boolean {
  return (match as readonly string[]).includes(route.value.name)
}

/**
 * 项目组里某一项是否当前项目。
 *
 * 判据是「路由的 `:id` 等于它」——不看 `route.name`：看板 / 任务 / 会话 / 绑定 /
 * 设置都发生在同一个项目里，左栏该一直指着那个项目，否则用户一进任务页就
 * 看不出自己在哪个项目里。
 */
function isCurrentProject(id: string): boolean {
  return currentProjectId.value === id
}

function onNavClick(e: MouseEvent, anchor: HTMLAnchorElement): void {
  interceptLink(e, anchor)
}

/* ---------------------------------------------------------------- 帮助面板 */

const helpOpen = ref(false)
const helpEl = ref<HTMLElement | null>(null)

/**
 * 「（未实现）」的 act 清单。
 *
 * **必须随 handler 表变化**：注册发生在组件挂载期——晚于首屏渲染。
 * 一个只读模块级 Map 的 `computed` 会在第一次求值后把结论冻住，于是
 * 后注册的 act 永远标着「（未实现）」，而用户按下去是有反应的。
 *
 * 所以这里用 `onHandlersChanged` 把「表变了」这件事接进响应式状态：
 * 表是模块级可变状态，它自己不会触发 Vue 的更新。
 */
const missing = ref<string[]>(missingActs())

/**
 * 订阅「handler 表变了」。
 *
 * **必须在这里订阅**（而不是 `onMounted`）：注册发生在子组件的挂载期，
 * 而子组件的 `onMounted` 先于父组件的 `onMounted` 跑——晚一步就漏掉了
 * 第一次注册，「（未实现）」的标记会停在首屏那一刻的结论上。
 */
const stopHandlers = onHandlersChanged(() => {
  missing.value = missingActs()
})

/**
 * Esc 与 Tab。**只在帮助是栈顶时生效**——`pop()` 按栈顶语义工作，
 * 而键盘事件是全站共用的：不判栈顶的话，帮助盖在轨迹抽屉上时一次 Esc
 * 会关掉两层。
 */
function onHelpKeydown(e: KeyboardEvent): void {
  if (e.isComposing) return
  const root = helpEl.value
  if (!root || topContainer() !== root) return
  if (e.key === 'Tab') trapTab(e, root)
}

/* ------------------------------------------------- session 过期重新登录 */

/**
 * 整页跳转的**唯一**出口。
 *
 * 整页跳转（而不是 `navigate()`）的语义是「这一页我不要了」——JS 上下文和
 * 内存里的东西全都清掉。`navigate` 只换 `route`（`history.pushState` +
 * 一个 ref），那些东西原样留在那儿。
 *
 * **为什么住在外壳**：整页跳转是一个**策略**，不是某个错误码的技术后果。
 * 401 的技术后果只是「接下来没有凭据可用」，那只需要一个框（`http.ts`
 * 弹框）；403 `hub_not_initialized` 的技术后果是「Hub 还没有用户」，那需要
 * 一整页 setup。两条路都归到同一个出口，于是「哪里会整页跳走」只有一处答案。
 */
function fullPageNavigate(path: string): void {
  location.assign(path)
}

/**
 * 403 `hub_not_initialized` 的跳转策略住在外壳。
 *
 * `http.ts` 只报事实（这是哪个错），「跳到哪」是策略——与 401 弹框同一个
 * 模式。它在 setup 体里注册（不是 `onMounted`）：首屏的请求就可能撞上
 * 403，而子组件的 `onMounted` 早于父组件的。
 */
const stopNotInitialized = setNotInitializedHandler(fullPageNavigate)

/**
 * 用户在重新登录框里点「取消」：**这一条**才真的去 `/login`。
 *
 * 取消的语义正是「这一页我不要了」——session 刚过期过，SPA 里那个
 * `LoginPage` 还会接着用它自己那份过期前的状态。一次真正的换页把这些
 * 一次清干净，`/login` 从零开始读 cookie。
 *
 * `replace` 不用：用户是**主动放弃**当前页面的，按后退该能回到它（回到一个
 * 又一次 401 的页面，比「后退键失灵」诚实）。
 */
function onLoginCancel(): void {
  fullPageNavigate(LOGIN_PATH)
}

/**
 * 主动登出：先让 Hub 清 cookie，再去 `/login`。
 *
 * **为什么必须整页跳转**：页面内存里还留着登出前拉到的数据，只换 `route`
 * 的话它们原样在那儿——`onLoginCancel` 那条注释里写过同样的理由。
 */
async function onLogout(): Promise<void> {
  try {
    await logout()
  } catch {
    // 清不掉也要走：留在页面上的人只会看到一堆 401 弹框，而 `/login`
    // 是唯一能重新拿凭据的地方。
  }
  fullPageNavigate(LOGIN_PATH)
}

function openHelp(): void {
  // 不能在这里判 `helpEl`：面板是 `v-if`，第一次调用时它还没渲染，
  // 那样写会让 `?` 与点击都变成空操作（面板永远打不开）。
  if (helpOpen.value) return
  helpOpen.value = true
  void nextTick(() => {
    if (helpEl.value) push(helpEl.value)
  })
}

function closeHelp(): boolean {
  if (!helpOpen.value) return false
  const el = helpEl.value
  // **只在帮助是栈顶时才由它响应 Esc**：`layer:escape` 的契约是「关掉最上
  // 层，返回是否真的关了一层」，而键盘事件是全站共用的——不判栈顶的话，
  // `?` → `t` → `Esc` 会在这里关一层、又在轨迹抽屉自己的 handler 里关一层，
  // 一次按键关掉两个浮层。抽屉自己会接住属于它的那次 Esc。
  if (el && topContainer() !== el) return false
  helpOpen.value = false
  // 关闭只有一条路：`pop(el)` 负责还焦点。这里不许自己 `el.focus()`。
  //
  // **必须带上这一层**：无参 `pop()` 关的是**栈顶**，而栈顶可能不是帮助
  // （`?` → `t` → `Esc` 时栈顶是轨迹抽屉）——那会把别人的层弹出焦点栈，
  // 留下一层指向已移除元素的空壳，之后 Esc 再也关不掉任何浮层。
  if (el) pop(el)
  return true
}

function onHelpBackdrop(e: MouseEvent): void {
  if (e.target === e.currentTarget) closeHelp()
}

/* ---------------------------------------------------------------- 搜索槽 */

/**
 * `/` 聚焦当前页的搜索框。页面把搜索框标成 `data-role="search"`，
 * 外壳只按这个约定找——**不引入一份「哪个页面有搜索」的表**，
 * 那张表会在下一个页面出现时立刻过期。
 */
function focusSearch(): boolean {
  const el = document.querySelector<HTMLElement>('[data-role="search"]')
  if (!el) return false
  el.focus()
  if (el instanceof HTMLInputElement) el.select()
  return true
}

/* ---------------------------------------------------------------- 生命周期 */

let stopRouter: (() => void) | null = null
let stopKeys: (() => void) | null = null

onMounted(() => {
  installShellKeys()
  stopKeys = bindShell({
    navigate: (to) => navigate(to),
    toggleRail,
    openHelp,
    closeHelp,
    focusSearch,
  })
  stopRouter = startRouter()
  // 左栏「项目」组那一列。与总览页同一个端点（`listProjects`），只是这里
  // 不显示失败态（见 `loadRailProjects` 的注释）。
  void loadRailProjects()
  // 首屏落在一条已删除的旧地址上（书签、外部链接）：换成它的去处。
  // 必须在 `startRouter` 之后——`navigate` 会改历史记录，先起监听再改，
  // 顺序反了会让这次 `replaceState` 发生在监听装好之前（本身无害，但
  // 「先装监听再改状态」是更不容易出错的那个顺序）。
  redirectLegacy()
  // 首屏：换页的 watch 不会为初始值触发，所以这里显式落一次
  void nextTick(() => document.querySelector<HTMLElement>('main[tabindex="-1"]')?.focus())
})

onBeforeUnmount(() => {
  stopKeys?.()
  stopRouter?.()
  stopHandlers()
  stopNotInitialized()
  if (helpOpen.value) closeHelp()
})

/**
 * 换页后把焦点给 `main`（§4.3）。
 *
 * `navigate()` 里已经调用过一次 `focusMain()`，这里再跟一次是为了覆盖
 * `popstate` 与「同一路径不同参数」的情形：两种情况都不经过 `navigate` 的
 * 那次调用，而焦点必须落在新内容上。
 */
watch(
  () => route.value.path,
  () => {
    // 点了一个已删除旧地址的链接（`interceptLink` 走 `navigate`，不整页刷新）：
    // 换成去处。`replace` 的理由见 `redirectLegacy` 上方。
    redirectLegacy()
    void nextTick(() => {
      const main = document.querySelector<HTMLElement>('main.af-main[tabindex="-1"]')
      if (!main) return
      const h1 = main.querySelector('h1')
      if (h1 && !h1.hasAttribute('tabindex')) h1.setAttribute('tabindex', '-1')
      main.focus()
    })
  },
)

/* ---------------------------------------------------------------- 标题 */

/**
 * 浏览器标题。**页面名从 `page-titles.ts` 读**——侧栏、帮助面板、浏览器
 * 标题说的是同一批页面名，各抄一份时改一处要改三处，漏一处的症状是
 * 「侧栏叫『Agent』、浏览器标题还叫『Agent 目录』」。
 */
const docTitle = computed(() => pageTitle(route.value.name))

watch(
  docTitle,
  (t) => {
    if (typeof document !== 'undefined') document.title = `${t} · AgentFleet`
  },
  { immediate: true },
)
</script>

<template>
  <!-- 认证流程页：只有页面本身，没有任何外壳 chrome。`main` 由页面自己给
       （`.af-auth` 就是页面根节点），所以这里不套一层 main——两层 main
       会让 `focusMain()` 的 `main[tabindex="-1"]` 选择器命中外层那个空壳。 -->
  <component :is="page" v-if="isAuthPage && page" :key="pageKey" />

  <!-- 外壳：原型 `prototype/index.html:12` 的 `.af-shell`（两个网格区：左栏 + 主区）。
       原型**没有全局顶栏**——页头 `.af-head` 属于页面自己（`prototype/index.html:15`）。
       收起态由 `<html data-rail="off">` 驱动（见脚本里的那个 watch）。 -->
  <div v-else class="af-shell">
    <aside class="af-rail" aria-label="主导航">
      <!-- 品牌行（原型 `app.css:61-75`）：与页头同高，所以两条水平线对得上。
           收起按钮在原型里就长在这一行末尾（`.af-rail__brand .af-navbtn`，
           `app.css:1140`）。左栏整体滑出屏幕之后，它是唤回左栏的唯一入口，
           所以收起时它必须留在原地——由 `html[data-rail="off"]` 把它切成
           悬浮形态（见 `prototype-shell.css` §1 末尾那条规则）。 -->
      <div class="af-rail__brand">
        <span class="af-rail__mark" aria-hidden="true">AF</span>
        <span>
          <span class="af-rail__name">AgentFleet</span><br />
          <span class="af-rail__sub">self-hosted</span>
        </span>
        <button
          class="af-navbtn"
          type="button"
          aria-label="折叠 / 展开侧栏"
          :title="`折叠侧栏（${hintOf('rail:toggle')}）`"
          :aria-expanded="effectiveRail === 'open'"
          @click="toggleRail"
        >
          <span class="af-navbtn__bars" aria-hidden="true"><span></span><span></span><span></span></span>
        </button>
      </div>

      <!-- 分组：全站 / 项目 / 收件箱 / 管理面（与原型 `shell.js:19-49` 的 `NAV` 同构）。
           `RAIL_GROUPS` 决定顺序，不是模板顺序——「项目」组在中间：项目是主轴，
           而它的数据是异步拿到的。 -->
      <template v-for="group in RAIL_GROUPS" :key="group">
        <div class="af-rail__group">
          <span>{{ group }}</span>
          <!-- 「项目」组标题挂一个 +（原型 `shell.js:100-104`）：建项目是高频动作，
               不该逼人先回总览页再找按钮。 -->
          <a
            v-if="group === '项目'"
            class="af-rail__add"
            href="/projects/new"
            title="新建项目"
            aria-label="新建项目"
            @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)"
            >+</a
          >
        </div>
        <ul v-if="group === '项目'" class="af-rail__list">
          <li v-for="p in railProjects" :key="p.id">
            <a
              :href="`/project/${encodeURIComponent(p.id)}/board`"
              :aria-current="isCurrentProject(p.id) ? 'page' : undefined"
              @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)"
            >
              <span class="af-rail__label">{{ p.name }}</span>
            </a>
          </li>
        </ul>
        <ul v-else class="af-rail__list">
          <li v-for="item in navItemsOf(group)" :key="item.to">
            <a
              :href="item.to"
              :aria-current="isCurrent(item.match) ? 'page' : undefined"
              :title="item.act ? `${item.label}（${hintOf(item.act)}）` : item.label"
              @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)"
            >
              <span class="af-rail__label">{{ item.label }}</span>
            </a>
          </li>
        </ul>
      </template>

      <!-- 底部：状态 + 两个整站动作（原型 `.af-rail__foot`，`app.css:102-106`）。
           原型的底部只有状态那几行；帮助与退出登录这两个入口在原型里不属于
           左栏，但实现侧**必须有**（`?` 与登出不能被删掉），而原型没有全局顶栏
           可以挂，所以它们落在这里——这是「搬位置」，不是新增能力。 -->
      <div class="af-rail__foot">
        <div class="af-rail__status">
          <span><span class="af-dot" aria-hidden="true"></span>Hub 已连接</span>
          <span>单用户 · 自托管</span>
          <span class="af-rail__keys">
            <kbd>{{ hintOf('help:toggle') }}</kbd> 键位
            <kbd>{{ PREFIX_LEAD }}</kbd> 跳转
            <kbd>{{ hintOf('rail:toggle') }}</kbd> 侧栏
          </span>
        </div>
        <div class="af-rail__actions">
          <button
            class="af-btn af-btn--ghost af-btn--sm"
            type="button"
            aria-label="键位帮助"
            title="键位帮助（?）"
            @click="openHelp"
          >
            <span aria-hidden="true">?</span> 键位
          </button>
          <button
            class="af-btn af-btn--ghost af-btn--sm"
            type="button"
            aria-label="退出登录"
            title="退出登录"
            @click="onLogout"
          >
            <span aria-hidden="true">⏻</span> 退出
          </button>
        </div>
      </div>
    </aside>

    <main class="af-main" tabindex="-1">
      <component :is="page" v-if="page" :key="pageKey" />
      <EmptyState
        v-else
        glyph="◌"
        title="这个地址没有页面"
        text="检查一下链接。项目页是 /project/&lt;项目 id&gt;/board，设备列表是 /devices，总览是 /。"
      >
        <a class="af-btn af-btn--primary" href="/" @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)">
          回到总览
        </a>
      </EmptyState>
    </main>

    <!-- session 过期时弹出的密码框（§4.7「需要别的动作」那一列）。401 的一条
         出路是「填个密码继续」，不是「整页跳走」。它由 `shared/http.ts` 开，
         由这里挂载；取消时跳 /login 是外壳的策略（见 `onLoginCancel`）。

         它待在外壳里面而不是与它平级：外壳那个 `v-else` 的配对是 `<component
         :is="page" v-if="isAuthPage && page">`，在中间插一个 `v-if` 会抢走
         配对、整个外壳就再也不渲染了。而它自己 Teleport 到 body（焦点栈的
         「背后 inert」才成立），所以放这儿与放在外面在 DOM 上完全一样。 -->
    <LoginPrompt :open="loginPromptOpen" @close="dismissLoginPrompt" @go-login="onLoginCancel" />
  </div>

  <!-- 键位帮助：最高层级，它解释其他所有。由 KEYS 渲染，不手写第二份。

       **必须 Teleport 到 body**：`focus.push` 的 `inertOthers` 只把「body 的
       直接子节点里、不含浮层容器的那几个」置 inert。帮助面板留在 `#app` 里
       时，它所在的那棵子树就是整个 `#app`——而 `#app` 是 body 唯一的子节点，
       于是「背后全部 inert」退化成「一个都没 inert」：遮罩盖住了视觉，背后的
       侧栏项仍然可 Tab、可点（`aria-modal` 与「焦点归最内层容器」都不成立）。

       它放在外壳之外，所以 `v-else` 的配对不受影响。 -->
  <Teleport to="body">
    <div
      v-if="helpOpen"
      ref="helpEl"
      class="af-help"
      role="dialog"
      aria-modal="true"
      aria-label="键位帮助"
      tabindex="-1"
      @click="onHelpBackdrop"
      @keydown="onHelpKeydown"
    >
      <div class="af-help__box">
        <div class="af-help__head">
          <span class="af-panel__title">键位</span>
          <span class="af-t-mute" style="font-size: var(--af-text-sm)">
            这张表就是键位实现本身——表里有的键才有 handler
          </span>
          <span class="af-spacer"></span>
          <button class="af-btn af-btn--ghost af-btn--sm af-btn--icon" type="button" aria-label="关闭" @click="closeHelp">
            ✕
          </button>
        </div>
        <div class="af-help__body">
          <section v-for="g in KEYS" :key="g.group">
            <div class="help__gh">
              <span>{{ g.group }}</span>
              <span class="af-t-mute">{{ SCOPE_LABEL[g.scope] }}</span>
            </div>
            <table class="help__t">
              <tbody>
                <tr v-for="k in g.keys" :key="`${k.act}:${k.keys.join('')}`" :data-unimplemented="missing.includes(k.act)">
                  <td class="kbd-cell">
                    <kbd v-for="key in k.keys" :key="key">{{ key }}</kbd>
                  </td>
                  <td>{{ k.label }}</td>
                </tr>
              </tbody>
            </table>
          </section>
        </div>
      </div>
    </div>
  </Teleport>
</template>
