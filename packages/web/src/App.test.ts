import { beforeEach, describe, expect, it, vi } from 'vitest'

import App from '@/App.vue'
import { KEYS, actCount } from '@/shared/keynav'
import { depth, isTabbable } from '@/shared/focus'
import { matchRoute, route } from '@/router'
import { device, mountIn, session } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { cssRuleOf, readRepoFile } from '@/test/css'

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
  // 一级导航按**原型** `shell.js:19-49` 的 `NAV` 分四组：全站 / 项目 / 收件箱 / 管理面。
  // 「项目」组里每项是一个项目（数据来自 `listProjects()`，与总览页同一个端点）。
  // 项目内页面（看板 / 任务 / 绑定 / 设置）**不进左栏**——它们从某个项目点进去。
  // `新建项目` 是**动作**而不是地方，挂在「项目」组标题右侧的 `.af-rail__add` 上。
  it('左栏是原型的四组（全站 / 项目 / 收件箱 / 管理面），组内项数对得上', async () => {
    stubFetch({ ...EMPTY_API, '/api/projects': { body: [{ id: 'p_1', name: '示例项目' }] } })
    const w = mountApp('/')
    await flush(w)

    const groups = Array.from(w.element.querySelectorAll<HTMLElement>('.af-rail__group'))
    expect(groups.map((g) => g.querySelector('span')?.textContent)).toEqual([
      '全站',
      '项目',
      '收件箱',
      '管理面',
    ])
    // 每组后面跟一个 `.af-rail__list`：全站 2 项 / 项目 1 项（来自 API）/
    // 收件箱 2 项 / 管理面 3 项。
    const lists = Array.from(w.element.querySelectorAll<HTMLElement>('.af-rail__list'))
    expect(lists.map((ul) => ul.querySelectorAll('a').length)).toEqual([2, 1, 2, 3])
  })

  /*
    「总览」在左栏里**只出现一次**：一次是导航项自己的文字，不是「标题 + 条目」。

    必须是「恰好一次」而不是 `toContain`：后者对多出来的那一份视而不见，
    重复正是这条缺陷本身。数的是左栏的**直接文字**——`title` 是同一句话的
    属性副本，不算第二次渲染。

    反向验证：把分组标题那个 `<span>` 改成显示 `item.label`（或把 `<h2>` 收回
    外壳）→ `count` 变 2 以上，红。
  */
  it('左栏里「总览」只出现一次（没有和标题重复）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const rail = w.element.querySelector<HTMLElement>('.af-rail')
    if (!rail) throw new Error('no rail')

    const text = Array.from(rail.querySelectorAll<HTMLElement>('*'))
      .filter((el) => el.children.length === 0)
      .map((el) => el.textContent ?? '')
    expect(text.filter((t) => t.includes('总览')).length).toBe(1)
  })

  it('当前页在导航上有 aria-current', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/devices')
    await flush(w)

    const current = Array.from(w.element.querySelectorAll('[aria-current="page"]')).map((el) =>
      el.textContent?.trim(),
    )
    expect(current).toEqual(['设备'])
  })

  it('导航项是真 <a href>：中键 / ⌘ 点击能走浏览器自己的路径', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    // 「项目」组那一列是数据驱动的（这里没有项目），所以取的是三条
    // 静态组里的链接：全站 2 + 收件箱 2 + 管理面 3。
    const hrefs = Array.from(w.element.querySelectorAll<HTMLAnchorElement>('.af-rail__list a')).map(
      (a) => a.getAttribute('href'),
    )
    expect(hrefs).toEqual([
      '/',
      '/projects/archived',
      '/inbox',
      '/inbox/activity',
      '/devices',
      '/agents',
      '/settings',
    ])
  })

  it('main 是 tabindex="-1"（可编程聚焦，但不由键盘穿过）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(w.element.querySelector('main')?.getAttribute('tabindex')).toBe('-1')
  })

  /* ===========================================================================
     外壳的结构与原型对齐（v0.8.1 子任务 2）
     ---------------------------------------------------------------------------
     原型 `prototype/index.html:12` 的外壳是 `.af-shell`（两个网格区：左栏 +
     主区），左栏是 `aside.af-rail`。实现此前自造了一套「应用外壳」四件套
     （根 / 侧栏 / 全局顶栏 / 全局状态条，原型命中 0），两套网格同时
     管一棵子树。这一组钉住「只剩原型那一套」。
     ======================================================================== */

  it('外壳根是 .af-shell，里面只有 aside.af-rail 与 main.af-main 两个网格区', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const shell = w.element.querySelector<HTMLElement>('.af-shell')
    expect(shell, '外壳根必须是原型的 .af-shell').not.toBeNull()
    // 网格区是**直接子节点**：少一层嵌套，收起态那条
    // `html[data-rail='off'] .af-shell { grid-template-columns: 0 1fr }` 才对得上。
    const aside = shell?.querySelector(':scope > aside.af-rail')
    expect(aside, '左栏必须是 .af-shell 的直接子节点 aside.af-rail').not.toBeNull()
    expect(shell?.querySelector(':scope > main.af-main')).not.toBeNull()
    // 自造层已删除：外壳里不许再出现那四个类名中的任何一个。
    // 判据串用 `\u002d` 拼（不是把字面量直接写出来）：全仓门禁扫的是源码
    // 文本，写出来会让本文件自己命中自己。
    const gone = ['af-app', 'af\u002dsidebar', 'af\u002dtopbar', 'af-statusbar']
    for (const name of gone) {
      expect(w.element.querySelector(`.${name}`), `.${name} 是自造层，必须已删除`).toBeNull()
    }
  })

  it('左栏恰好 4 个分组标题，且「新建项目」是动作挂在「项目」组标题上', async () => {
    stubFetch({ ...EMPTY_API, '/api/projects': { body: [{ id: 'p_1', name: '示例项目' }] } })
    const w = mountApp('/')
    await flush(w)

    expect(w.element.querySelectorAll('.af-rail__group').length).toBe(4)
    const add = w.element.querySelector<HTMLAnchorElement>('.af-rail__group .af-rail__add')
    expect(add?.getAttribute('href')).toBe('/projects/new')
  })

  it('左栏底部的状态那几行落在 .af-rail__foot 里，不是全局状态条', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const foot = w.element.querySelector<HTMLElement>('.af-rail__foot')
    expect(foot).not.toBeNull()
    expect(foot?.textContent).toContain('Hub 已连接')
    // 「单用户 · 自托管」曾是自造状态条里的字：它现在只能出现在左栏里。
    const aside = w.element.querySelector('.af-rail')
    expect(aside?.textContent).toContain('单用户 · 自托管')
    expect(w.element.querySelector('footer')).toBeNull()
  })

  /*
    页面标题在整页里**只出现一次**。

    原型没有全局顶栏（`prototype/index.html:15` 的页头属于页面自己），所以外壳
    再渲染一个 `<h2>{{ docTitle }}</h2>` 就是第二个「总览」。放宽成
    `toContain` 或 `>= 1` 都抓不到它——重复正是这条缺陷本身。

    反向验证：把 `<header><h2>{{ docTitle }}</h2></header>` 加回外壳模板 → 红
    （实测 `findAll('h1')` 仍为 1，所以要数的是 h1+h2 的全部标题）。
  */
  it('整页只有一个页面标题（外壳不再重复渲染 docTitle）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const headings = Array.from(w.element.querySelectorAll<HTMLElement>('h1, h2')).map(
      (h) => h.textContent?.trim() ?? '',
    )
    expect(headings.filter((t) => t.includes('总览')).length).toBe(1)
    // 而且那个标题在**主区**里（是页面自己的页头），不在左栏里。
    const main = w.element.querySelector('main.af-main')
    expect(main?.querySelectorAll('h1').length).toBe(1)
  })
})

describe('App：跳转后焦点落在 main', () => {
  beforeEach(() => {
    document.body.innerHTML = ''
  })

  it('首屏就把焦点交给 main（换页的 watch 不会为初始值触发）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(document.activeElement).toBe(w.element.querySelector('main'))
  })

  it('换页后焦点也落在 main，并给 h1 补上 tabindex 让 ⇧Tab 能回到标题', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
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
    const w = mountApp('/')
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
    const w = mountApp('/')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    expect(helpEl()?.textContent).toContain('这张表就是键位实现本身')
  })

  it('点帮助面板里的关闭按钮能关掉它', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)
    await helpEl()?.querySelector<HTMLElement>('.af-help__head button')?.click()
    await flush(w)

    expect(helpEl()).toBeNull()
  })

  it('帮助开着时把外壳置 inert——背后不是「还能 Tab 过去」的', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)
    const sidebar = w.element.querySelector<HTMLElement>('.af-rail')

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
    return mountApp('/project/p_1/session/s_1')
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
    const w = mountApp('/')
    await flush(w)
    expect(document.title).toBe('总览 · AgentFleet')
  })

  /**
   * `/agents` 已删除，但**不能 404**：它曾是一级导航项，书签与文档链接
   * 都指向它。外壳在首屏与换页时把它重定向到设备页的目录分段。
   *
   * 反向验证：把 `App.vue` 里 `redirectLegacy()` 那两次调用删掉，这条会红在
   * 「地址没被换掉」。
   */
  /*
    `/agents` 在 v0.7.0 起是**真页面**（`prototype/agents.html`），不再是设备页的
    一个分段。

    v0.5.0 时它确实被重定向到 `/devices?view=agents`，那时侧栏只有三项、Agent
    目录并进设备页。新原型把它分回来：设备回答「这台机器在不在线」，Agent 回答
    「这台机器上装了什么、能不能跑」——同一批数据，运维视角与能力视角。
    所以这条用例现在钉的是**地址不动、页面自己渲染出来**。
  */
  it('/agents 是 Agent 页本身，不再重定向到设备分段', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/agents')
    await flush(w)

    // 地址**不动**：重定向会把它改成 /devices?view=agents，而那样用户点回来
    // 就找不到这一页了。
    expect(location.pathname).toBe('/agents')
    expect(document.title).toBe('Agent · AgentFleet')
    // 渲染的是页面本身，不是 404 空态。
    expect(w.element.querySelector('.af-empty')?.textContent ?? '').not.toContain(
      '这个地址没有页面',
    )
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

    const shell = w.element.querySelector('.af-shell')
    expect(fetchMock.mock.calls.map(([u]) => String(u))).toContain('/api/devices')

    const box = document.querySelector<HTMLElement>('[data-loginprompt]')
    expect(box).not.toBeNull()
    // 整页跳转会重建整个 JS 上下文；这里外壳还是原来那个 DOM 节点
    expect(w.element.querySelector('.af-shell')).toBe(shell)
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
    const before = w.element.querySelector('.af-shell')
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
    expect(w.element.querySelector('.af-shell')).toBe(before)
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
    expect(w.element.querySelector('.af-shell')).toBeNull()
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
    expect(w.element.querySelector('.af-rail')).toBeNull()
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })
})

describe('App：登出', () => {
  /** 左栏底部 `.af-rail__foot` 里那个「退出登录」按钮。 */
  function logoutButton(w: ReturnType<typeof mountIn>): HTMLButtonElement | null {
    const all = Array.from(w.element.querySelectorAll('button'))
    return (all.find((b) => b.getAttribute('aria-label') === '退出登录') as
      | HTMLButtonElement
      | undefined) ?? null
  }

  it('左栏有登出入口', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(logoutButton(w)).not.toBeNull()
  })

  it('点登出会先 POST /logout 再去登录页', async () => {
    const assign = vi.spyOn(location, 'assign').mockImplementation(() => {})
    const fetchMock = stubFetch(EMPTY_API)
    const w = mountApp('/')
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
    const w = mountApp('/')
    await flush(w)

    logoutButton(w)?.click()
    await flush(w)

    expect(location.pathname).toBe('/login')
  })
})

/* ===========================================================================
   左栏几何与收起态：**只有原型那一套**
   ---------------------------------------------------------------------------
   用户裁定（2026-11-16）「以原型为准」后，自造的 `.af-app` 网格
   （`44px 208px 1fr` + `[data-rail='collapsed'] .af-brand { flex-direction:
   column }` 的 52px 图标轨）与自造的侧栏一族整体删除。
   收起态改走原型的机制：`html[data-rail='off'] .af-shell
   { grid-template-columns: 0 1fr }` + `html[data-rail='off'] .af-rail
   { transform: translateX(-100%) }`（原型 `app.css:1212-1227`）。

   这一组守三件事，**都不是「样式表里有几行」那种恒真断言**：
   ① 品牌行是 `aside.af-rail` 的**第一个子节点**（与导航同属一列，一起滑走）；
   ② 网格模板只有两列且宽度取 `--af-rail-w`（不写死像素）；
   ③ 收起态**整列滑出**，而不是缩成图标轨——并且唤回按钮仍可 Tab 到。

   反向验证：把品牌行挪出 `aside` → ①红；把 `grid-template-columns` 写成
   `44px 208px 1fr` → ②红；把 `html[data-rail='off'] .af-rail` 那条规则删掉
   或换回 `.af-app[data-rail]` → ③红。
   =========================================================================== */
describe('App：左栏几何与收起态', () => {
  it('品牌行是 aside.af-rail 的第一个子节点，与导航同属一列', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const rail = w.element.querySelector<HTMLElement>('aside.af-rail')
    expect(rail, '左栏必须存在').toBeTruthy()
    const brand = rail?.querySelector<HTMLElement>(':scope > .af-rail__brand')
    expect(brand, '品牌行必须是 aside 的直接子节点').not.toBeNull()
    // 关键：它在 `aside` 里面。挪出去这条立刻红。
    expect(brand?.parentElement).toBe(rail)
  })

  it('网格模板只有两列，左栏宽取 --af-rail-w（不写死像素）', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const body = cssRuleOf(css, '.af-shell')

    expect(body, '左栏宽必须取 --af-rail-w，不能写死像素').toContain(
      'grid-template-columns: var(--af-rail-w) 1fr',
    )
    expect(body).not.toContain('44px 208px')
    // 自造的那套网格已经没了：外壳的几何**只能**有一条定义。
    // 剥注释再查——两边的注释里都写着 `.af-app`（解释它被删掉的理由）。
    const components = readRepoFile('packages/web/src/styles/components.css').replace(
      /\/\*[\s\S]*?\*\//g,
      '',
    )
    expect(components).not.toContain('.af-app')
  })

  it('收起态是整列滑出屏幕（原型的 html[data-rail] 机制），不是 52px 图标轨', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    // 剥注释再查「52px」：注释里写着「上一版是 52px 图标轨」这句账，不剥会误报。
    const decls = css.replace(/\/\*[\s\S]*?\*\//g, '')

    // 正面：内容区吃满 + 整列平移出去。这两条缺一条就是「点了没反应」。
    expect(cssRuleOf(css, "html[data-rail='off'] .af-shell")).toContain(
      'grid-template-columns: 0 1fr',
    )
    const railBlock = cssRuleOf(css, "html[data-rail='off'] .af-rail")
    expect(railBlock, '收起态左栏规则必须存在').not.toBe('')
    expect(railBlock).toContain('transform: translateX(-100%)')

    // 反向：不许再有「52px 图标轨」那一套（竖排名字、图标轨定宽）。
    expect(decls).not.toContain('writing-mode: vertical-rl')
    expect(decls).not.toContain('52px')
  })

  it('收起态：唤回按钮留在原地（悬浮形态），且仍可 Tab 到', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    // 左栏整列 `visibility: hidden` 之后，唤回按钮必须自己浮回来。
    const recall = cssRuleOf(css, "html[data-rail='off'] .af-rail__brand .af-navbtn")
    expect(recall, '收起态唤回按钮规则必须存在').not.toBe('')
    expect(recall).toContain('position: fixed')
    expect(recall).toContain('visibility: visible')

    // 「浮回来」不是只画出来的：按钮本身仍是品牌行里那颗可聚焦的 button。
    const btn = w.element.querySelector<HTMLElement>('.af-rail__brand .af-navbtn')
    expect(btn, '品牌行里必须有那颗开关').not.toBeNull()
    expect(isTabbable(btn as Element)).toBe(true)
  })

  it('收起态不该有可拖动的横向滚动条（用户反馈 2026-10-28）', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    // **必须取顶层那条规则**：`cssRuleOf` 会先命中以 `.af-rail {` 结尾的子串
    // （`html[data-rail='off'] .af-rail {` 里就有），于是读到的是覆盖规则、
    // 永远不含 `overflow-x`。实测踩过（见旧注释）。
    const at = css.indexOf('\n.af-rail {')
    expect(at, '顶层 .af-rail 规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(css.indexOf('{', at) + 1, css.indexOf('}', at))

    // 这条断言锁的是一件容易被当成「没事」的事：**只写 `overflow-y: auto`
    // 时 `overflow-x` 的计算值是 `auto`，不是 `visible`**。CSS Overflow
    // 规范规定一轴非 visible 会把另一轴的 visible 提升成 auto，于是左栏里
    // 任何横向溢出都长成一条可拖动的滚动条，而它拖出来是一片空白。
    //
    // **必须剥掉注释再断言**：直接 `toContain('overflow-x: hidden')` 时，
    // 把那条声明删掉用例照样绿——解释为什么需要它的散文里就有这串字。
    const decls = block.replace(/\/\*[\s\S]*?\*\//g, '')
    expect(decls, 'overflow-x 必须显式收掉').toContain('overflow-x: hidden')

    // 网格项默认 `min-width: auto`：内容比轨道宽时它会把左栏顶宽。
    expect(decls, '网格项必须允许收窄').toContain('min-width: 0')
  })
})

describe('App：会话页自动收起侧栏（v0.5.0）', () => {
  const SESSIONS_API = {
    '/api/sessions': {
      body: [session({ id: 's_1', workspace: { node_id: 'n_1', path: '/srv/app', path_locked: true } })],
    },
    '/api/devices': { body: [device()] },
    '/api/agents': { body: [] },
  }

  /*
    读收起态**必须读全局 `document.documentElement`，不能读 `w.element`**。

    v0.8.1 起收起态落在原型的位置：`<html data-rail="off">`
    （`prototype/assets/shell.js` 的 `applyRail`，`app.css:1212-1227` 的选择器
    以 `html` 为根）。两个取值：
      · 展开 → **属性不存在**（原型 `delete dataset.rail`，展开是缺省态）
      · 收起 → `'off'`
    把它归一成 `open` / `collapsed` 再断言，用例读起来仍是「侧栏开没开」。
  */
  function railOf(): 'open' | 'collapsed' {
    return document.documentElement.dataset.rail === 'off' ? 'collapsed' : 'open'
  }

  /** 点左栏品牌行那颗开关。**每次都重新查**，不复用上一次拿到的节点。 */
  async function clickTopbarRail(w: ReturnType<typeof mountIn>): Promise<void> {
    const btn = w.element.querySelector<HTMLElement>('.af-rail button[aria-label="折叠 / 展开侧栏"]')
    if (!btn) throw new Error('no rail toggle in rail')
    btn.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await flush(w)
  }

  /** 换页。路由是模块级 `shallowRef`，测试直接写它就是「导航」。 */
  function go(path: string): void {
    window.history.replaceState(null, '', path)
    route.value = matchRoute(path)
  }

  it('进会话页收起侧栏，离开后回到进入前的值', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/')
    await flush(w)
    expect(railOf(), '列表页不该收侧栏').toBe('open')

    go('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf(), '会话页必须收起').toBe('collapsed')

    go('/devices')
    await flush(w)
    expect(railOf(), '离开会话页要还回去').toBe('open')
  })

  /*
    会话里每次点开关都**立刻生效**，且这一次拨动就是用户意图的更新。

    ── 这条锁的是「翻转基准」不是「rail」 ───────────────────────────
    进会话时 `rail` 还是列表页的 `open`，可见态被自动收起成 `collapsed`。
    若 `toggleRail` 以 `rail` 为基准翻，点一次会把 `rail` 翻成 `collapsed`，
    可见态**仍是** `collapsed`——用户点「展开」得到的是「没动」。
    正确的做法是以**可见态**为基准翻，于是这里第一次点击后必须是 `open`。

    反向验证（实测）：把 `toggleRail` 改成
    `autoCollapsed=false; rail=rail==='open'?'collapsed':'open'` →
    第③步实测红（期望 open，实际 collapsed）。

    ── 为什么离开会话后是 `collapsed`（最后一次拨动）而不是 `open` ────
    侧栏开关表达的是「我要不要看侧栏」，这是**用户偏好**，不是页面态。
    在会话里拨它，就是改了偏好；离开会话页没有任何理由把它改回去。
    「自动收起」才是页面态，它才是**不该**越界的那个（会话外永不生效）。
  */
  it('会话里每次点开关都立刻生效，且拨动即用户意图', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/')
    await flush(w)

    // ① 列表页、用户意图 open（初值）
    expect(railOf(), '列表页开着').toBe('open')

    // ② 进会话：自动收起
    go('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf(), '会话页必须收起').toBe('collapsed')

    // ③ 点一次开关：必须立刻展开（不是要按两下）
    await clickTopbarRail(w)
    expect(railOf(), '会话里点一次开关就要展开').toBe('open')

    // ④ 再点一次：收起
    await clickTopbarRail(w)
    expect(railOf(), '再点一次收起').toBe('collapsed')

    // ⑤ 离开会话：保持用户最后一次的选择（他刚在会话里收起了侧栏）
    go('/devices')
    await flush(w)
    expect(railOf(), '拨动即用户意图，离开会话不撤销它').toBe('collapsed')

    // ⑥ 回列表页再点一次：翻回 open——说明它确实是普通偏好，不是会话残留
    await clickTopbarRail(w)
    expect(railOf(), '会话外仍可正常翻转').toBe('open')
  })

  /*
    用户本来就收起了侧栏：进会话仍是 collapsed，会话里展开后离开仍是 open。

    它与上一条**必须同时存在**，因为两条把「离开时写什么」两侧都钉住了：
    无条件置 open 会让这里第②步（会话页收起）变成 open 而红；无条件置
    collapsed 会让这里最后一步红。
  */
  it('用户本来就收起了侧栏：会话里的拨动同样立刻生效并保持', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/devices')
    await flush(w)

    await clickTopbarRail(w)
    expect(railOf(), '用户自己收起').toBe('collapsed')

    go('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf(), '会话页收起').toBe('collapsed')

    // 会话里展开：立刻生效
    await clickTopbarRail(w)
    expect(railOf(), '会话里展开立刻生效').toBe('open')

    // 离开会话页：用户最后的选择是 open，就保持 open。
    // （这条与上一条合起来钉住「离开时不写缺省值」：无条件置 open 会让
    //  上一条的第⑥步之前的 collapsed 断言红；无条件置 collapsed 会让这里红。）
    go('/devices')
    await flush(w)
    expect(railOf(), '保持用户最后一次的选择').toBe('open')
  })

  it('深链直接进会话页也收起（不是只有「从列表点进来」才收）', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf()).toBe('collapsed')
  })

  it('会话页里侧栏开关仍在左栏的 Tab 序列里（手动展开可达）', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/project/p_1/session/s_1')
    await flush(w)
    const btn = w.element.querySelector<HTMLElement>('.af-rail button[aria-label="折叠 / 展开侧栏"]')
    if (!btn) throw new Error('no rail toggle')
    // 自动收起是**值**的变化，不是把按钮藏掉：撤掉它用户就再也展开不了侧栏，
    // 而「找设备 / 换会话」在会话页里就不可达了。
    expect(isTabbable(btn), '折叠态也必须能 Tab 到展开按钮').toBe(true)
  })
})
