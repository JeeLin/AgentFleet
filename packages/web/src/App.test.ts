import { beforeEach, describe, expect, it, vi } from 'vitest'

import App from '@/App.vue'
import { KEYS, actCount } from '@/lib/keynav'
import { depth, isTabbable } from '@/lib/focus'
import { matchRoute, route } from '@/router'
import { device, mountIn, session } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { readRepoFile } from '@/test/css'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 挂外壳。路由是模块级状态，所以每条用例先把它摆到该在的页面。 */
function mountApp(path: string) {
  window.history.replaceState(null, '', path)
  route.value = matchRoute(path)
  return mountIn(App)
}

const EMPTY_API = {
  '/api/sessions': { body: [] },
  '/api/devices': { body: [] },
  '/api/agents': { body: [] },
}

describe('App：外壳骨架', () => {
  // 用户裁定「只要区分会话页、设置页和其他页」，收敛成三项。
  // `Agent 目录` 并进设备页的 `?view=agents` 分段；`新建会话` 是**动作**
  // 而不是地方，路由还在（有上下文的两条路要走它），但不进侧栏。
  it('一级导航只有 3 项（会话 / 设备 / 设置）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    const items = Array.from(w.element.querySelectorAll<HTMLElement>('.af-sidebar__item'))
    expect(items.map((el) => el.getAttribute('aria-label'))).toEqual(['会话', '设备', '设置'])
    expect(items.map((el) => el.querySelector('.af-sidebar__hint')?.textContent)).toEqual([
      'g s',
      'g d',
      'g ,',
    ])
    const groups = Array.from(w.element.querySelectorAll('.af-navgroup__title')).map((el) => el.textContent)
    expect(groups).toEqual(['会话', '节点', '系统'])
  })

  it('当前页在导航上有 aria-current，会话工作台仍高亮「会话」', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/devices')
    await flush(w)

    const current = Array.from(w.element.querySelectorAll('[aria-current="page"]')).map((el) =>
      el.getAttribute('aria-label'),
    )
    expect(current).toEqual(['设备'])
  })

  it('导航项是真 <a href>：中键 / ⌘ 点击能走浏览器自己的路径', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    const hrefs = Array.from(w.element.querySelectorAll<HTMLAnchorElement>('.af-sidebar__item')).map(
      (a) => a.getAttribute('href'),
    )
    expect(hrefs).toEqual(['/sessions', '/devices', '/settings'])
  })

  it('main 是 tabindex="-1"（可编程聚焦，但不由键盘穿过）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    expect(w.element.querySelector('main')?.getAttribute('tabindex')).toBe('-1')
  })
})

describe('App：跳转后焦点落在 main', () => {
  beforeEach(() => {
    document.body.innerHTML = ''
  })

  it('首屏就把焦点交给 main（换页的 watch 不会为初始值触发）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    expect(document.activeElement).toBe(w.element.querySelector('main'))
  })

  it('换页后焦点也落在 main，并给 h1 补上 tabindex 让 ⇧Tab 能回到标题', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    const other = document.createElement('button')
    document.body.appendChild(other)
    other.focus()
    expect(document.activeElement).toBe(other)

    window.history.pushState(null, '', '/devices')
    window.dispatchEvent(new PopStateEvent('popstate'))
    await flush(w)

    const main = w.element.querySelector('main')
    expect(document.activeElement).toBe(main)
    expect(main?.querySelector('h1')?.getAttribute('tabindex')).toBe('-1')
  })
})

describe('App：键位帮助由 KEYS 渲染', () => {
  /** 帮助面板 Teleport 到 body，所以只能在 document 上找（不在外壳子树里）。 */
  function helpEl(): HTMLElement | null {
    return document.querySelector<HTMLElement>('.af-help')
  }

  it('打开帮助后列出 KEYS 的全部组与条目（不手写第二份表）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    const help = helpEl()
    expect(help).not.toBeNull()
    // 一行一个键位条目；同一个 act 的 alias（`t` 与 `g g`）各占一行，
    // 所以行数是「条目数」而不是 act 数——表里有多少条，面板里就有多少条。
    const bindings = KEYS.reduce((n, g) => n + g.keys.length, 0)
    expect(help?.querySelectorAll('[data-unimplemented]').length).toBe(bindings)
    expect(actCount()).toBeLessThanOrEqual(bindings)

    for (const group of KEYS) {
      expect(help?.textContent).toContain(group.group)
    }
  })

  it('标题里说明这张表就是键位实现本身', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    expect(helpEl()?.textContent).toContain('这张表就是键位实现本身')
  })

  it('点帮助面板里的关闭按钮能关掉它', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)
    await helpEl()?.querySelector<HTMLElement>('.af-help__head button')?.click()
    await flush(w)

    expect(helpEl()).toBeNull()
  })

  it('帮助开着时把外壳置 inert——背后不是「还能 Tab 过去」的', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)
    const sidebar = w.element.querySelector<HTMLElement>('.af-sidebar')

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    // `inertOthers` 把「body 的直接子节点里、不含浮层容器的那几个」置 inert，
    // 所以**外壳的挂载根**（测试里的 host div，真浏览器里的 `#app`）才带上这个
    // 属性——`inert` 不继承，断言子树里某个元素自己是白费。
    // 帮助面板留在 `#app` 里时，`#app` 是 body 唯一的子节点且 `contains` 命中
    // 宿主 → 一个节点都不会被 inert。所以它必须 Teleport 到 body。
    const host = sidebar?.closest<HTMLElement>('body > *')
    expect(host?.inert, '背后那一棵必须整棵 inert').toBe(true)
    expect(host?.getAttribute('aria-hidden')).toBe('true')
    // 侧栏里的可聚焦项因此也不再可 Tab（`isTabbable` 读的正是 `inert`）
    expect(isTabbable(sidebar as Element)).toBe(false)
    expect(helpEl()).not.toBeNull()
  })
})

describe('App：帮助面板的 Esc / 焦点栈', () => {
  function helpEl(): HTMLElement | null {
    return document.querySelector<HTMLElement>('.af-help')
  }

  /** 会话工作台的路由：轨迹抽屉只在那页上注册 `traj:toggle`。 */
  function mountWorkbench(): ReturnType<typeof mountIn> {
    return mountApp('/sessions/s_1')
  }

  it('? → t → Esc：关掉的是栈顶那一层，另一层不动，栈也不残留', async () => {
    stubFetch({
      '/api/sessions': {
        body: [
          session({ id: 's_1', workspace: { node_id: 'n_1', path: '/srv/app', path_locked: true } }),
        ],
      },
      '/api/devices': { body: [device()] },
    })
    const w = mountWorkbench()
    await flush(w)

    // `?` 打开帮助（进焦点栈）
    document.dispatchEvent(new KeyboardEvent('keydown', { key: '?', bubbles: true, cancelable: true }))
    await flush(w)
    expect(helpEl()).not.toBeNull()

    // `t` 打开轨迹抽屉（第二层）——**它才是栈顶**
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 't', bubbles: true, cancelable: true }))
    await flush(w)
    expect(document.querySelector<HTMLElement>('[data-traj]')).not.toBeNull()

    // 第一次 Esc 关**栈顶**（抽屉），不是帮助：
    // `KEYS` 里 `layer:escape` 的 label 就是「关闭最上层浮层」，而
    // `TrajDrawer.vue` 与 `App.vue` 的 `onHelpKeydown` 都写着「只在本层是
    // 栈顶时生效」——两个 document 级监听靠这条互斥，否则一次 Esc 关两层。
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    await flush(w)

    expect(document.querySelector<HTMLElement>('[data-traj]')).toBeNull()
    expect(helpEl(), '帮助在栈底，不该被这一次 Esc 带走').not.toBeNull()

    // 第二次 Esc 关剩下的那一层
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    await flush(w)
    expect(helpEl()).toBeNull()
    // 栈真的空了：没有指向已移除元素的空壳层
    expect(depth()).toBe(0)
  })
})

describe('App：不知道的地址', () => {
  it('给一个空态而不是白屏，并指回会话列表', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/nope')
    await flush(w)

    expect(w.element.querySelector('.af-empty')?.textContent).toContain('这个地址没有页面')
    expect(document.title).toContain('找不到页面')
  })
})

describe('App：文档标题', () => {
  it('每个页面都有独立标题', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)
    expect(document.title).toBe('会话 · AgentFleet')
  })

  /**
   * `/agents` 已删除，但**不能 404**：它曾是一级导航项，书签与文档链接
   * 都指向它。外壳在首屏与换页时把它重定向到设备页的目录分段。
   *
   * 反向验证：把 `App.vue` 里 `redirectLegacy()` 那两次调用删掉，这条会红在
   * 「地址没被换掉」。
   */
  it('/agents 被重定向到设备页的目录分段，不是 404', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/agents')
    await flush(w)

    expect(location.pathname).toBe('/devices')
    expect(location.search).toBe('?view=agents')
    // 换成了目录分段：不再是「这个地址没有页面」
    expect(w.element.querySelector('.af-empty')?.textContent).not.toContain('这个地址没有页面')
  })
})

/* ===========================================================================
   session 过期 → 弹框，不是整页跳走
   ---------------------------------------------------------------------------
   这一组测的是**接在一起的两半**：`http.ts` 的 401 分支弹框（`promptLogin`），
   外壳把框挂出来、并在取消时跳 `/login`。两半各自有单测，但「401 之后用户
   眼前是什么」只有合起来才存在——所以这里从一次真实的 `request()` 出发。
   =========================================================================== */

describe('App：401 之后', () => {
  it('页面里的请求撞 401 → 弹出密码框，地址没动、页面没重建', async () => {
    const fetchMock = stubFetch({ ...EMPTY_API, '/api/devices': { status: 401, body: { error: 'session_expired' } } })
    const w = mountApp('/devices')
    await flush(w)

    const shell = w.element.querySelector('.af-app')
    expect(fetchMock.mock.calls.map(([u]) => String(u))).toContain('/api/devices')

    const box = document.querySelector<HTMLElement>('[data-loginprompt]')
    expect(box).not.toBeNull()
    // 整页跳转会重建整个 JS 上下文；这里外壳还是原来那个 DOM 节点
    expect(w.element.querySelector('.af-app')).toBe(shell)
    expect(location.pathname).toBe('/devices')
    // 401 仍然照常抛给页面（页面停在错误态，不继续渲染一屏读不到的列表）
    await flush(w)
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()
  })

  it('输对密码 → 关框并留在原页面（不导航、不刷新）', async () => {
    stubFetch({
      ...EMPTY_API,
      '/api/devices': { status: 401, body: { error: 'session_expired' } },
      '/login': { body: { ok: true } },
    })
    const w = mountApp('/devices')
    await flush(w)
    const before = w.element.querySelector('.af-app')
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()

    const input = document.querySelector<HTMLInputElement>('#login-prompt-password')
    expect(input).not.toBeNull()
    input!.value = 'correct horse battery'
    input!.dispatchEvent(new Event('input'))
    await flush(w)
    document.querySelector<HTMLFormElement>('#login-prompt-password')!.closest('form')!.dispatchEvent(
      new Event('submit', { bubbles: true, cancelable: true }),
    )
    await flush(w)

    expect(document.querySelector('[data-loginprompt]')).toBeNull()
    // 「不导航」不是只看地址：外壳 DOM 必须是同一个实例
    expect(location.pathname).toBe('/devices')
    expect(w.element.querySelector('.af-app')).toBe(before)
  })

  it('点「取消」→ 那一下才整页跳去 /login', async () => {
    const assign = vi.spyOn(location, 'assign').mockImplementation(() => {})
    stubFetch({ ...EMPTY_API, '/api/devices': { status: 401, body: { error: 'session_expired' } } })
    const w = mountApp('/devices')
    await flush(w)
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()

    Array.from(document.querySelectorAll<HTMLButtonElement>('[data-loginprompt] button'))
      .filter((b) => b.textContent?.includes('取消'))
      .forEach((b) => void b.click())
    await flush(w)

    expect(assign).toHaveBeenCalledWith('/login')
    // 关框与跳转都发生了：框不该留在屏幕上
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })

  it('/login 上不挂这个框（那一页自己的 401 是密码错，不是过期）', async () => {
    stubFetch({})
    const w = mountApp('/login')
    await flush(w)

    // `/login` 是认证页：不带外壳（§5.1），自然也没有这个框
    expect(w.element.querySelector('.af-app')).toBeNull()
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })

  it('硬开 /login 仍然渲染完整的登录页（不是只留一个弹框）', async () => {
    // 弹框是「 session 过期」的出路，不是它的**唯一**出路：用户主动退出、
    // 或直接敲地址进 /login 时，要看到的是一页完整的登录表单。
    // 少了这一条，401 处置就会变成「永远弹框、登录页再也打不开」。
    stubFetch({})
    const w = mountApp('/login')
    await flush(w)

    const page = w.element.querySelector('.af-auth')
    expect(page).not.toBeNull()
    expect(page?.querySelector('input[type="password"]')).not.toBeNull()
    expect(page?.querySelector('button[type="submit"]')?.textContent).toContain('进入控制台')
    // 仍然没有外壳 chrome：认证页不带导航（§5.1）
    expect(w.element.querySelector('.af-sidebar')).toBeNull()
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })
})

describe('App：登出', () => {
  /** 顶栏上那个「退出登录」按钮。 */
  function logoutButton(w: ReturnType<typeof mountIn>): HTMLButtonElement | null {
    const all = Array.from(w.element.querySelectorAll('button'))
    return (all.find((b) => b.getAttribute('aria-label') === '退出登录') as
      | HTMLButtonElement
      | undefined) ?? null
  }

  it('顶栏有登出入口', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    expect(logoutButton(w)).not.toBeNull()
  })

  it('点登出会先 POST /logout 再去登录页', async () => {
    const assign = vi.spyOn(location, 'assign').mockImplementation(() => {})
    const fetchMock = stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    logoutButton(w)?.click()
    await flush(w)

    const hit = fetchMock.mock.calls.find((c) => String(c[0]).includes('/logout'))
    expect(hit, '登出前必须先通知 Hub 清 cookie').toBeTruthy()
    expect((hit?.[1] as RequestInit | undefined)?.method).toBe('POST')
    // 「再去登录页」是这条用例名里的承诺：只通知了 Hub 而不跳转的话，
    // 人留在原地，接下来每一个请求都是 401。
    expect(assign).toHaveBeenCalledWith('/login')
    // 还原：下一条用例断言的是真实跳转，留着这个 spy 会让它恒真
    assign.mockRestore()
  })

  it('Hub 清 cookie 失败也要去登录页（否则人被困在一堆 401 弹框里）', async () => {
    stubFetch({ ...EMPTY_API, '/logout': { status: 500 } })
    const w = mountApp('/sessions')
    await flush(w)

    logoutButton(w)?.click()
    await flush(w)

    expect(location.pathname).toBe('/login')
  })
})

/* ===========================================================================
   品牌区与一级导航**同一列**（用户裁定，2026-10-27）
   ---------------------------------------------------------------------------
   改之前 `.af-app` 的 grid 是 `44px 208px 1fr`，`brand` 与 `sidebar` 是两块
   独立区域，中间靠 `border-right` 划开。它们本来就同高、同背景色、同受
   `⌃B` 折叠，分开只多一条分隔线，而竖排 `AGENTFLEET` 挤在 44px 里读不出来。

   这一组守三件事，**都不是「样式表里有几行」那种恒真断言**：
   ① 品牌区在 DOM 里是 `nav` 的**子节点**（同属一个 flex 容器，折叠时一起管）；
   ② grid 模板里**不再有** `brand` 这个列名（44px 独立列真的没了）；
   ③ 折叠态品牌区**退回原来的图标轨形态**（竖排名字留着，不是藏掉）。

   反向验证：把 `.af-brand` 挪回 `nav` 外面 → ①红；把 grid 改回
   `44px 208px 1fr` → ②红；把折叠态的 `writing-mode: vertical-rl` 删掉或
   加回 `.af-brand__name { display: none }` → ③红。
   =========================================================================== */
describe('App：品牌区与导航合并成一列', () => {
  it('品牌区是 nav 的子节点，不是并列的另一块', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    const brand = w.element.querySelector('.af-brand')
    expect(brand, '品牌区必须存在').toBeTruthy()
    // 关键：它的父节点就是 nav。挪回 nav 外面这条立刻红。
    expect(brand?.parentElement?.tagName).toBe('NAV')
    expect(brand?.parentElement?.classList.contains('af-sidebar')).toBe(true)
  })

  it('grid 模板里没有 brand 列：44px 独立列已取消', () => {
    const css = readRepoFile('packages/web/src/styles/components.css')
    // 取 `.af-app` 的声明体
    const at = css.indexOf('.af-app {')
    const close = css.indexOf('}', at)
    const body = css.slice(at, close)

    // 正面：列名只剩 sidebar / topbar / main / status
    expect(body).toContain('grid-template-columns: 208px 1fr')
    expect(body).not.toContain('44px 208px')
    // `brand` 不再是 grid 区域名。注意 `af-brand` 作为**类名**仍会出现在
    // 别处，所以这里只查 `grid-template-areas` 那一行里的裸词 `brand`。
    const areasLine = body
      .split('\n')
      .find((l) => l.includes('grid-template-areas'))
    expect(areasLine, 'grid-template-areas 必须存在').toBeTruthy()
    expect(areasLine).not.toMatch(/'brand\b/)
  })

  it('折叠态退回原来的图标轨：竖排名字留着，不是藏掉', () => {
    const css = readRepoFile('packages/web/src/styles/components.css')
    const at = css.indexOf("[data-rail='collapsed'] .af-brand {")
    expect(at, '折叠态品牌区规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(at, css.indexOf('}', at))

    // 折叠态是竖列：方块在上、名字在下。横排名字在 52px 里会被裁掉一半。
    expect(block).toContain('flex-direction: column')

    // 名字**必须留着并竖排**——这是「折叠不是藏起来」的具体落点。
    const nameAt = css.indexOf("[data-rail='collapsed'] .af-brand__name {")
    expect(nameAt, '折叠态名字竖排规则必须存在').toBeGreaterThan(-1)
    const nameBlock = css.slice(nameAt, css.indexOf('}', nameAt))
    expect(nameBlock).toContain('writing-mode: vertical-rl')

    // 反向：折叠态不许把整个品牌区或它的名字 `display: none` 掉。
    // 只查折叠态那几条规则，别误伤展开态的样式。
    const collapsedRules = css
      .split('\n')
      .filter((l) => l.includes("[data-rail='collapsed']"))
      .join('\n')
    expect(collapsedRules).not.toContain('.af-brand { display: none')
    expect(collapsedRules).not.toContain('.af-brand__name { display: none')
  })

  it('折叠态：导航图标与品牌方块同轴居中', () => {
    const css = readRepoFile('packages/web/src/styles/components.css')
    const at = css.indexOf("[data-rail='collapsed'] .af-sidebar__item {")
    expect(at, '折叠态导航项规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(at, css.indexOf('}', at))

    // 定宽 + 居中：不加这两条，导航项缩成内容宽 28px，图标贴左侧 6px，
    // 与居中的品牌方块不同轴。
    expect(block).toContain('width: 40px')
    expect(block).toContain('justify-content: center')
    // 内边距必须归零，否则 40px 里再减 12px，图标又偏左
    expect(block).toContain('padding: 0')
  })
})
