/* ===========================================================================
   router.ts —— 路由表（设计规范 §5.2）
   ---------------------------------------------------------------------------
   这个项目**不引 vue-router**。前端只有 8 条路由、没有嵌套布局、没有路由级
   数据加载，引一个库换来的是一份要跟着升级的依赖和一层与规范术语不同的
   抽象（`RouterView` / `useRoute` 与 §5.2 的表格对不上号）。这里 60 行
   能做完的事，不该用 60 行配置去做。

   ── 路由表就是 §5.2 的表格 ───────────────────────────────────────────
   `/sessions/:id` 只表达「**激活**这个会话 tab」，不表达「第 3 个 tab」
   （§5.5.6：tab 集合本身不可深链接）。

   ── `/login` 与 `/setup` 是「认证流程」，不是导航项 ────────────────
   §5.2 的表格里这两行的「导航」列写的是「认证流程」而不是 ✅：
   它们出现在路由表里，但**不进 §5.1 的 5 项一级导航**，也不带外壳渲染
   （`AUTH_ROUTES`，见 `App.vue`）。缺了这两条路由的后果不是「少两个页面」，
   而是 401/403 的落点变成 404 空态——登录页本身打不开，等于没有登录流程。

   **没有工作区路由**：`/w/:nodeId/*path` 与 `/workspace` 在架构上不存在
   （§5.2 否决项、§7.0）。所以这张表里连一个能长出它们的占位都没有。

   ── 没有 `/tasks` / `/artifacts` / `/devices/:id` ──────────────────
   规范写的是「不留占位页」。留一个灰掉的导航项等于在导航里承诺一件本阶段
   做不到的事——用户点进去看到「敬请期待」，会以为是我们忘做了。

   ── 为什么用 History API 而不是 hash ──────────────────────────────
   Hub 自己托管 `dist/`（子任务 6），同源相对路径成立；hash 路由会把
   `/sessions/abc` 变成 `/sessions#/sessions/abc`，把「工作区没有地址」
   这条已经理清的事又搅回去。
   =========================================================================== */

import { shallowRef, type ShallowRef } from 'vue'

import { focusMain } from '@/shared/focus'

/** 路由名。与 §5.2 表格的「页面」一列一一对应。 */
export type RouteName =
  | 'sessions'
  | 'new-session'
  | 'session'
  | 'devices'
  | 'settings'
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
 * 路由表。**顺序有意义**：`/sessions/new` 必须排在 `/sessions/:id` 之前，
 * 否则「新建会话」会被匹配成 id 为 `new` 的会话。
 *
 * 两条认证路由排在最后：它们与其余路径没有前缀关系（`/login` / `/setup`
 * 都是单段字面量），顺序对匹配结果没有影响，放末尾是为了让
 * 「一级导航的六条」连成一片、一眼看全 §5.2 的两张名单。
 */
export const ROUTES: readonly RouteDef[] = [
  { name: 'sessions', pattern: '/sessions' },
  { name: 'new-session', pattern: '/sessions/new' },
  { name: 'session', pattern: '/sessions/:id' },
  { name: 'devices', pattern: '/devices' },
  { name: 'settings', pattern: '/settings' },
  { name: 'login', pattern: '/login' },
  { name: 'setup', pattern: '/setup' },
]

/**
 * 已删除路由 → 去处。
 *
 * `agent-catalog` 并进设备页后 `/agents` 不再是路由。**不能就这么让它变成
 * 404**：它是真存在过的地址（§5.2 曾把它列为一级导航），用户的书签、以及
 * 我们自己在文档里写过的链接都指向它。静默 404 的症状是「点了收藏夹里的
 * 链接，看到『这个地址没有页面』」——而内容其实就在隔壁。
 *
 * 判据放在 `matchRoute` 里（而不是外壳的渲染分支）：「旧地址去哪」是路由
 * 的知识，让外壳知道它就是把一张不该存在的表塞进视图层。
 */
export const LEGACY_PATHS: Readonly<Record<string, string>> = {
  '/agents': '/devices?view=agents',
}

/** 根路径落在会话列表：会话是入口（§7.0）。 */
export const INDEX_PATH = '/sessions'

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
  return LEGACY_PATHS[path] ?? null
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
