/* ===========================================================================
   router.ts —— 路由表（设计规范 §5.2）
   ---------------------------------------------------------------------------
   这个项目**不引 vue-router**，现在是 23 条路由也仍然不引。理由不是「行数还
   不够多」，而是这个文件实际只有三件事：一张模式表、一个按段比对的匹配器、
   一个 `port` 与模式表的对应。`matchPattern` 从 v0.5.0 起就支持任意层具名段
   （`/project/:id/task/:taskId` 不需要新机制），23 条路由的匹配逻辑与 8 条
   时**一模一样**——涨的是表里的行数，不是逻辑。

   `vue-router` 换来的是 `RouterView` / `useRoute` 一层抽象，而 §5.2 的表格
   用的是「页面 / 地址 / 导航」三列。多一层对不上号的术语，在多一个人接手时
   就是多一次翻译。**判据是逻辑的重复度，不是表的长短。**

   ── 页面照 `prototype/`，不照旧会话轴 ──────────────────────────────
   路由结构以 `prototype/` 的 21 页为准（v0.7.0 子任务 8）：
   总览是入口，项目下挂看板 / 任务 / 执行 / 会话，会话不再是顶层。

   ── 骨架路由 ≠ 占位承诺 ─────────────────────────────────────────────
   v0.7.0 之前这里写着「没有 `/tasks` / `/artifacts`，规范写的是『不留占位页』，
   留一个灰掉的导航项等于在导航里承诺一件本阶段做不到的事」。
   那条理由**依然成立**，但它针对的是「**导航里灰掉的条目**」，不是「路由表里
   有这条路径」。两者不是一件事：

   - 灰条目 = 对用户承诺一个功能。用户点进去看到「敬请期待」，会以为是我们忘做了。
   - 骨架路由 = 给后续里程碑预留的挂载点。它只能从项目页内部进入，
     不进一级导航，且页面顶部**必须写明本页在哪个版本开放**。

   所以子任务 8 建了 13 个骨架页，**一级导航仍然只有四项**
   （总览 / 设备 / Agent / 设置）。这条边界改一次就够，别再来回改。

   ── `/login` 与 `/setup` 是「认证流程」，不是导航项 ────────────────
   它们出现在路由表里但不进一级导航，也不带外壳渲染（`AUTH_ROUTES`，见
   `App.vue`）。缺了这两条路由的后果不是「少两个页面」，而是 401/403 的落点
   变成 404 空态——登录页本身打不开，等于没有登录流程。

   **没有工作区路由**：`/w/:nodeId/*path` 与 `/workspace` 在架构上不存在
   （§5.2 否决项、§7.0）。所以这张表里连一个能长出它们的占位都没有。

   ── 为什么用 History API 而不是 hash ──────────────────────────────
   Hub 自己托管 `dist/`，同源相对路径成立；hash 路由会把 `/project/x/board`
   变成 `/project#/project/x/board`，把「工作区没有地址」这条已经理清的事
   又搅回去。
   =========================================================================== */

import { shallowRef, type ShallowRef } from 'vue'

import { focusMain } from '@/shared/focus'

/** 路由名。与 `prototype/` 的页面对照表一一对应（子任务 8）。 */
export type RouteName =
  // 一级导航
  | 'overview'
  | 'devices'
  | 'device-detail'
  | 'agents'
  | 'agent-detail'
  | 'settings'
  // 项目内
  | 'board'
  | 'project-task'
  | 'execution'
  | 'replay'
  | 'timeline'
  | 'session'
  | 'projects-new'
  | 'project-bind'
  | 'project-settings'
  | 'project-schedules'
  | 'projects-archived'
  // 收件箱与管家
  | 'inbox'
  | 'inbox-activity'
  | 'butler'
  // 设置子页
  | 'settings-skills'
  | 'settings-integrations'
  | 'settings-notifications'
  // 认证流程
  | 'login'
  | 'setup'
  | 'not-found'

/**
 * §5.2 里标「认证流程」的那两条路由。
 *
 * 它们**不在 §5.1 的 5 项一级导航里**，也不该在：登录前没有导航可言
 * （§5.1 最后一条）。外壳据此决定「不带 chrome 渲染」——判据是这个集合，
 * 不是在模板里写 `name === 'login' || name === 'setup'`：后者会在加第三个
 * 认证页时被漏掉一处。
 */
export const AUTH_ROUTES: ReadonlySet<RouteName> = new Set<RouteName>(['login', 'setup'])

/** 一次匹配的结果。`params` 只含这张表里的具名段（目前只有 `:id`）。 */
export interface Route {
  name: RouteName
  params: Record<string, string>
  path: string
  query: URLSearchParams
}

interface RouteDef {
  name: RouteName
  /** `:x` 是具名段。其余按字面比。 */
  pattern: string
}

/**
 * 路由表。**顺序有意义**：`/projects/new` 必须排在 `/projects/:id/...` 之前，
 * 否则「新建项目」会被匹配成 id 为 `new` 的项目。
 *
 * 长模式要排在它的前缀短模式之前吗？不需要——`matchPattern` 按**段数**先筛，
 * `/projects/new`（2 段）与 `/project/:id/board`（3 段）段数就不同，不会互相
 * 吃掉。真正需要小心的是**同段数**的：`/projects/new` vs `/projects/:id`
 * （后者不存在，归档页是固定段 `/projects/archived`，所以这条也安全）。
 *
 * 认证路由排在最后：它们与其余路径没有前缀关系，放末尾是为了让「页面路由」
 * 连成一片。
 */
export const ROUTES: readonly RouteDef[] = [
  // ── 一级导航（只有这四项进左栏，见 `App.vue` 的 `NAV`） ──────────────
  { name: 'overview', pattern: '/' },
  { name: 'devices', pattern: '/devices' },
  // 设备详情。**排在 `/devices` 之后、段数不同**，不会互相吃掉：`/devices`
  // 是 1 段、`/devices/:id` 是 2 段，`matchPattern` 按段数先筛。
  { name: 'device-detail', pattern: '/devices/:id' },
  { name: 'agents', pattern: '/agents' },
  { name: 'agent-detail', pattern: '/agents/:name' },
  { name: 'settings', pattern: '/settings' },
  // ── 项目内 ────────────────────────────────────────────────────────
  { name: 'projects-new', pattern: '/projects/new' },
  { name: 'projects-archived', pattern: '/projects/archived' },
  { name: 'board', pattern: '/project/:id/board' },
  { name: 'project-task', pattern: '/project/:id/task/:taskId' },
  { name: 'execution', pattern: '/project/:id/execution/:execId' },
  { name: 'replay', pattern: '/project/:id/execution/:execId/replay' },
  { name: 'timeline', pattern: '/project/:id/timeline' },
  { name: 'session', pattern: '/project/:id/session/:sessionId' },
  { name: 'project-bind', pattern: '/project/:id/bind' },
  { name: 'project-settings', pattern: '/project/:id/settings' },
  { name: 'project-schedules', pattern: '/project/:id/schedules' },
  // ── 收件箱与管家 ──────────────────────────────────────────────────
  { name: 'inbox', pattern: '/inbox' },
  { name: 'inbox-activity', pattern: '/inbox/activity' },
  { name: 'butler', pattern: '/butler' },
  // ── 设置子页 ──────────────────────────────────────────────────────
  { name: 'settings-skills', pattern: '/settings/skills' },
  { name: 'settings-integrations', pattern: '/settings/integrations' },
  { name: 'settings-notifications', pattern: '/settings/notifications' },
  // ── 认证流程（不带外壳，见 `AUTH_ROUTES`） ────────────────────────
  { name: 'login', pattern: '/login' },
  { name: 'setup', pattern: '/setup' },
]

/**
 * 已删除路由 → 去处。
 *
 * # `/sessions*`：会话迁到项目下之后
 *
 * 会话不再是顶层实体（`PRODUCT.md` §1：项目 → 任务 → 执行 → 会话）。旧地址
 * `/sessions`、`/sessions/:id`、`/sessions/new` 曾经是真存在过的地址，
 * 用户的书签与我们写过的文档链接都指向它们。**不能让它静默 404**。
 *
 * **但也不能精确还原**：新路由是 `/project/:id/session/:sessionId`，而旧地址
 * 里没有项目 id。要还原它得先查「这个会话属于哪个项目」——那是页面的活，
 * 不是路由表的活（路由表是纯数据、无 IO）。所以统一落到总览，并在页面上
 * **明说**「会话已迁到项目下」，让用户知道该往哪找，而不是看见一片空白。
 *
 * `:id` 段的旧地址要单独处理：`LEGACY_PATHS` 是精确匹配表，带参数的进不来。
 * 见 `legacyTarget` 的第二条判据。
 *
 * # `/agents` 曾经在这里，现在不在了
 *
 * v0.5.0 把 Agent 目录并进了设备页的 `?view=agents` 分段，所以那时这里有一行
 * `/agents → /devices?view=agents`。v0.7.0 起按新原型（`prototype/agents.html`）
 * Agent 重新成为**一级导航项**、`/agents` 重新是**真路由**，`matchRoute` 会先
 * 命中路由表——这行映射再也走不到，而它指向的分段视图在新设备页里已经没有了。
 * 一条永远不被调用、且指错目标的映射不是「保险」，是下一次改动的陷阱。
 */
export const LEGACY_PATHS: Readonly<Record<string, string>> = {
  '/sessions': '/',
  '/sessions/new': '/',
  '/tasks': '/',
  '/artifacts': '/',
}

/**
 * 带参数的旧地址前缀 → 去处。
 *
 * 与 `LEGACY_PATHS` 分开是因为判据不同：那张表是精确相等，这里按前缀匹配。
 * 前缀匹配会误伤（`/sessionsX` 也会命中），所以判据里要求前缀之后紧跟 `/`
 * 或就是结尾——见 `legacyTarget`。
 */
const LEGACY_PREFIXES: Readonly<Record<string, string>> = {
  '/sessions/': '/',
}

/**
 * 根路径：**总览**（`prototype/index.html`）。
 *
 * v0.7.0 之前是 `/sessions`——那时会话是顶层。现在顶层是项目，总览是入口。
 */
export const INDEX_PATH = '/'

/** 登录页路径。`http.ts` 的 401 与外壳的退出登录都指向它（`App.vue` 的 `onLogout`）。 */
export const LOGIN_PATH = '/login'

/** 首次 setup 的路径。`http.ts` 的 403 `hub_not_initialized` 指向它。 */
export const SETUP_PATH = '/setup'

/** 一条路径按表匹配。都不中就是 `not-found`。 */
export function matchRoute(input: string): Route {
  const url = new URL(input, 'http://hub.local')
  const path = url.pathname
  for (const def of ROUTES) {
    const params = matchPattern(def.pattern, path)
    if (params) return { name: def.name, params, path, query: url.searchParams }
  }
  // 已删除的地址：解析成它的去处（见 `LEGACY_PATHS`）。**不在这里做跳转**——
  // 解析是纯函数，跳转要改历史记录；由 `App.vue` 在挂载/换页时统一处理。
  return { name: 'not-found', params: {}, path, query: url.searchParams }
}

/** 这条路径是不是一个已删除、但有去处的旧地址。 */
export function legacyTarget(path: string): string | null {
  const exact = LEGACY_PATHS[path]
  if (exact !== undefined) return exact
  for (const [prefix, target] of Object.entries(LEGACY_PREFIXES)) {
    // 只认「前缀 + 一段」：`/sessions/abc` 命中，`/sessionsfoo` 不命中。
    if (path.startsWith(prefix) && path.length > prefix.length) return target
  }
  return null
}

/** 单条模式匹配。命中返回具名段，不中返回 `null`。 */
function matchPattern(pattern: string, path: string): Record<string, string> | null {
  const p = pattern.split('/').filter(Boolean)
  const a = path.split('/').filter(Boolean)
  if (p.length !== a.length) return null
  const params: Record<string, string> = {}
  for (let i = 0; i < p.length; i++) {
    const seg = p[i] as string
    const got = a[i] as string
    if (seg.startsWith(':')) {
      params[seg.slice(1)] = decodeURIComponent(got)
    } else if (seg !== got) {
      return null
    }
  }
  return params
}

/**
 * 当前路由。`shallowRef` 而不是 `ref`：整条路由是一个不可变值，
 * 换一次是换一个对象——`ref` 的深代理只会让每次比较都变成代理之间的比较。
 */
export const route: ShallowRef<Route> = shallowRef(matchRoute(currentPath()))

function currentPath(): string {
  if (typeof location === 'undefined') return INDEX_PATH
  return `${location.pathname}${location.search}`
}

let started = false

export interface NavigateOptions {
  /** 不留下历史记录。 */
  replace?: boolean
}

/**
 * 跳转。
 *
 * `push` 与 `replace` 的区别只有一条：`replace` 不留下历史记录。新建会话
 * 成功后的跳转用 `replace`——用户按「后退」不该回到一张已经提交过的表单。
 *
 * 跳转后的焦点落点是 `main`（§4.3），**没有开关**：会话工作台切 tab 是
 * **视图切换**（DECISIONS D10），它走 `history.replaceState`（见
 * `SessionPage.vue` 的 `syncUrl`），不经过这里——`route` 不变，焦点由页面
 * 自己落。
 */
export function navigate(to: string, opts: NavigateOptions = {}): void {
  if (typeof history === 'undefined') return
  const method = opts.replace ? 'replaceState' : 'pushState'
  history[method](null, '', to)
  route.value = matchRoute(to)
  // 跳转后的焦点落点是 `main`（§4.3）。放在下一次微任务之后：Vue 还没把
  // 新页面渲染出来时 `main` 里是旧内容，聚焦本身没问题，但屏幕阅读器
  // 播报的标题会是上一页的。
  queueMicrotask(() => focusMain())
}

/** 启动 popstate 监听。返回卸载函数（测试用）。 */
export function startRouter(): () => void {
  if (started || typeof window === 'undefined') return () => undefined
  started = true
  const onPop = (): void => {
    route.value = matchRoute(currentPath())
    queueMicrotask(() => focusMain())
  }
  window.addEventListener('popstate', onPop)
  return () => {
    window.removeEventListener('popstate', onPop)
    started = false
  }
}

/**
 * 把一次点击翻译成路由跳转。
 *
 * 只接管**同源、无修饰键、左键**的 `<a>`：带 `⌘` / `⌃` / `⇧` 的点击是
 * 「在新标签页打开」，那是浏览器的语义，抢过来就是把一个用户已经学会的
 * 手势弄坏。调用方传 `event` 与目标元素。
 *
 * 返回 `true` 表示已接管（调用方不要再做别的）。
 */
export function interceptLink(e: MouseEvent, anchor: HTMLAnchorElement): boolean {
  if (e.defaultPrevented || e.button !== 0) return false
  if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return false
  if (anchor.target && anchor.target !== '_self') return false
  if (anchor.hasAttribute('download')) return false
  const url = new URL(anchor.href, location.href)
  if (url.origin !== location.origin) return false
  e.preventDefault()
  navigate(`${url.pathname}${url.search}${url.hash}`)
  return true
}
