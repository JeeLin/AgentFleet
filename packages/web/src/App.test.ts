import { beforeEach, describe, expect, it, vi } from 'vitest'

import App from '@/App.vue'
import { KEYS, actCount } from '@/lib/keynav'
import { matchRoute, route } from '@/router'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

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
  it('一级导航只有 5 项，分三组（会话 / 环境 / 系统）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    const items = Array.from(w.element.querySelectorAll<HTMLElement>('.af-sidebar__item'))
    expect(items.map((el) => el.getAttribute('aria-label'))).toEqual([
      '会话',
      '新建会话',
      '设备',
      'Agent 目录',
      '设置',
    ])
    expect(items.map((el) => el.querySelector('.af-sidebar__hint')?.textContent)).toEqual([
      'g s',
      'g n',
      'g d',
      'g a',
      'g ,',
    ])
    const groups = Array.from(w.element.querySelectorAll('.af-navgroup__title')).map((el) => el.textContent)
    expect(groups).toEqual(['会话', '环境', '系统'])
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
    expect(hrefs).toEqual(['/sessions', '/sessions/new', '/devices', '/agents', '/settings'])
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
  it('打开帮助后列出 KEYS 的全部组与条目（不手写第二份表）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    const help = w.element.querySelector('.af-help')
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

    expect(w.element.querySelector('.af-help')?.textContent).toContain('这张表就是键位实现本身')
  })

  it('点帮助面板里的关闭按钮能关掉它', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/sessions')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)
    await w.element.querySelector<HTMLElement>('.af-help__head button')?.click()
    await flush(w)

    expect(w.element.querySelector('.af-help')).toBeNull()
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
    const w = mountApp('/agents')
    await flush(w)
    expect(document.title).toBe('Agent 目录 · AgentFleet')
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

