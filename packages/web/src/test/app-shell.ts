/* ===========================================================================
   外壳测试脚手架（`App.test.ts` / `App.shell-state.test.ts` 共用）
   ---------------------------------------------------------------------------
   这两条文件测的是**同一个组件**（`App.vue`），而它们的脚手架曾经是三份手写：

     · `flush`      —— `for i<8 { $nextTick; settle(1) }`，两份曾**逐字相同**，
       合并后只剩这一份（`test/app-shell.ts:44-48`）；
     · `mountApp`   —— `history.replaceState` + `route.value = matchRoute` +
       `mountIn(App)`，两份逐字相同；
     · `EMPTY_API`  —— 同一张空桩表，两份逐字相同；
     · 第三份**同构**实现：`App.test.ts` 的 `railOf()` / `clickTopbarRail()` /
       `go()` 与 `App.shell-state.test.ts` 的 `railToggle()` 做的是同一件事
       （读 / 写 `<html data-rail>`、点那颗 `aria-label="折叠 / 展开侧栏"` 的
       按钮、改路由）。

   三份同时漂移的代价不是行数，而是**判据不一致**：一份用
   `expect(...).not.toBeNull()` 取节点、另一份用 `if (!btn) throw`，于是
   「按钮不见了」在两个文件里是两种不同的失败形态。这里统一成一份。

   ── 为什么 `mountShell` 带一个可选参数，而不是将来再分叉 ──────────────
   两条文件当前都只用「挂载 + 路由」，所以默认不传任何挂载参数。一旦某一份
   需要（例如给 `App.vue` 传一个 prop），**加在这个可选参数上**，不要再复制
   一份实现——复制出来的第二份就是这轮要消灭的那个东西。
   =========================================================================== */

import type { ComponentMountingOptions } from '@vue/test-utils'

import App from '@/App.vue'
import { matchRoute, route } from '@/router'
import { settle } from '@/test/http'
import { mountIn, type MountedIn } from '@/test/mount'

/** 本文件里「一个挂在宿主上的外壳实例」。 */
export type ShellWrapper = MountedIn<typeof App>

/**
 * 推 8 轮「微任务 + Vue 渲染队列」。
 *
 * 8 这个数是**实测的收敛轮数**：外壳挂载期会连发三个请求
 * （`listProjects` / `listDevices` / `listAgents`），每一条 `await` 链回来都要
 * 再推进一次渲染。轮数少的时候左栏底部的计数行偶尔还没落 DOM（机器忙时），
 * 症状是「单独跑绿、全量跑红」。
 */
export async function flushShell(w: ShellWrapper): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/**
 * 挂外壳。**路由是模块级状态**，所以每条用例都得先把它摆到该在的页面。
 */
export function mountShell(
  path: string,
  options: ComponentMountingOptions<typeof App> = {},
): ShellWrapper {
  window.history.replaceState(null, '', path)
  route.value = matchRoute(path)
  return mountIn(App, options)
}

/**
 * 换页。路由是模块级 `shallowRef`，测试直接写它就是「导航」。
 *
 * 与 `mountShell` 分开：换页不该重建外壳（「换页后左栏偏好还在」那条用例
 * 守的正是这一点），所以这里只动路由，不碰挂载。
 */
export function goTo(path: string): void {
  window.history.replaceState(null, '', path)
  route.value = matchRoute(path)
}

/**
 * 左栏的收起态，归一成 `open` / `collapsed`。
 *
 * **必须读全局 `document.documentElement`，不能读 `w.element`**：v0.8.1 起
 * 收起态落在原型的位置 `<html data-rail="off">`（`prototype/assets/shell.js`
 * 的 `applyRail`、`app.css:1212-1227` 的选择器以 `html` 为根）。两个取值：
 * 展开 → **属性不存在**（原型 `delete dataset.rail`），收起 → `'off'`。
 */
export function railOf(): 'open' | 'collapsed' {
  return document.documentElement.dataset.rail === 'off' ? 'collapsed' : 'open'
}

/**
 * 左栏品牌行那颗「折叠 / 展开侧栏」按钮。**每次都重新查**，不复用上一次
 * 拿到的节点。
 */
export function railToggleOf(w: ShellWrapper): HTMLElement {
  const btn = w.element.querySelector<HTMLElement>('button[aria-label="折叠 / 展开侧栏"]')
  // 用 `throw` 而不是 `expect(...).not.toBeNull()`：这里取的是**后续断言的
  // 操作对象**，取不到说明前提不成立，继续往下只会拿到 `undefined` 上的
  // 属性（报错信息指不到真正的原因）。
  if (!btn) throw new Error('左栏里没有那颗 aria-label="折叠 / 展开侧栏" 的按钮')
  return btn
}

/** 点一下那颗开关，并等渲染走完。 */
export async function clickRailToggle(w: ShellWrapper): Promise<void> {
  railToggleOf(w).click()
  await flushShell(w)
}

/** 零数据的桩表：三个左栏端点各回一个空数组。 */
export const EMPTY_SHELL_API: Record<string, { body: unknown }> = {
  '/api/sessions': { body: [] },
  '/api/devices': { body: [] },
  '/api/agents': { body: [] },
}

/** 一个记录调用的 `fetch` 桩（`stubFetch` / `vi.fn` 的返回都满足）。
 *
 *  `calls` 写成 `unknown[][]` 而不是元组 `[unknown, unknown][]`：`vi.fn()` 的
 *  `mock.calls` 类型是 `any[][]`，元组类型会让 `calledUrls(fetchMock)` 在
 *  每个测试文件里都报 TS2345（G88 接入真实消费者时实测 vue-tsc 3 处报错）。
 *  这两个 helper 只按下标取 `[0]` / `[1]`，用不到元组的长度保证。 */
type FetchMock = { mock: { calls: unknown[][] } }

/** 这个桩收到过的**全部请求 URL**（字符串形态）。 */
export function calledUrls(fetchMock: FetchMock): string[] {
  return fetchMock.mock.calls.map(([u]) => String(u))
}

/**
 * 取这个桩收到的全部 JSON 请求体（GET / 空体返回 `null`，由调用方按 URL 过滤）。
 *
 * 留在这里的理由与 `flushShell` 相同：外壳那两条测试文件里「发了什么」的
 * 断言要能写成「发了哪个请求」，而不是「**发过**某个请求」（后者连 URL 都
 * 不判，换个端点照样绿）。
 */
export function sentBodiesOf(fetchMock: FetchMock): Array<Record<string, unknown> | null> {
  return fetchMock.mock.calls.map(([, init]) => {
    const body = (init as RequestInit | undefined)?.body
    if (typeof body !== 'string' || body === '') return null
    try {
      return JSON.parse(body) as Record<string, unknown>
    } catch {
      return null
    }
  })
}
