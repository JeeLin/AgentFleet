import { beforeEach, describe, expect, it, vi } from 'vitest'

import { INDEX_PATH, ROUTES, AUTH_ROUTES, interceptLink, matchRoute, navigate, route, startRouter } from '@/router'

describe('路由表：就是 §5.2 的表格', () => {
  it('八条具名路由：六条导航页 + 两条认证流程，没有工作区路由', () => {
    // 顺序即匹配语义。`/login` 与 `/setup` 在 §5.2 的表格里「导航」一列
    // 写的是「认证流程」而不是 ✅——它们在这里，但不进 §5.1 的 5 项一级导航。
    // 这两条曾经缺失，后果是 401/403 的落点变成 404 空态（登录页打不开）。
    expect(ROUTES.map((r) => `${r.name} ${r.pattern}`)).toEqual([
      'sessions /sessions',
      'new-session /sessions/new',
      'session /sessions/:id',
      'devices /devices',
      'agents /agents',
      'settings /settings',
      'login /login',
      'setup /setup',
    ])
  })

  it('/login 与 /setup 匹配成认证路由，且被标进 AUTH_ROUTES', () => {
    expect(matchRoute('/login').name).toBe('login')
    expect(matchRoute('/setup').name).toBe('setup')
    expect(AUTH_ROUTES.has('login')).toBe(true)
    expect(AUTH_ROUTES.has('setup')).toBe(true)
    // 导航页一个都不在认证集合里：误加会让某个页面凭空失去外壳
    for (const name of ['sessions', 'new-session', 'session', 'devices', 'agents', 'settings'] as const) {
      expect(AUTH_ROUTES.has(name)).toBe(false)
    }
  })

  it('没有任何能长出 /w/:nodeId/*path 或 /workspace 的占位', () => {
    const patterns = ROUTES.map((r) => r.pattern)
    expect(patterns.some((p) => p.startsWith('/w') || p.includes('workspace'))).toBe(false)
  })

  it('根路径落在会话列表——会话是入口', () => {
    expect(INDEX_PATH).toBe('/sessions')
  })

  it('/sessions/new 排在 /sessions/:id 之前（否则新建会被当成 id 为 new 的会话）', () => {
    const names = ROUTES.map((r) => r.name)
    expect(names.indexOf('new-session')).toBeLessThan(names.indexOf('session'))
  })
})

describe('matchRoute', () => {
  it('字面段按字面比', () => {
    expect(matchRoute('/devices').name).toBe('devices')
    expect(matchRoute('/settings').name).toBe('settings')
  })

  it('/sessions 是列表，/sessions/new 是新建，/sessions/abc 是会话', () => {
    expect(matchRoute('/sessions').name).toBe('sessions')
    expect(matchRoute('/sessions/new').name).toBe('new-session')
    expect(matchRoute('/sessions/abc').name).toBe('session')
  })

  it('具名段进 params，并做百分号解码', () => {
    const r = matchRoute('/sessions/s%2F1')
    expect(r.params.id).toBe('s/1')
  })

  it('query 进 searchParams', () => {
    const r = matchRoute('/sessions/new?device=n_1')
    expect(r.query.get('device')).toBe('n_1')
  })

  it('段数不匹配就不中（/sessions/abc/def 不退化成会话页）', () => {
    expect(matchRoute('/sessions/abc/def').name).toBe('not-found')
  })

  it('不认识的路由是 not-found，不是默默落在首页', () => {
    expect(matchRoute('/nope').name).toBe('not-found')
  })

  it('尾部斜杠不影响匹配', () => {
    expect(matchRoute('/devices/').name).toBe('devices')
  })
})

describe('navigate', () => {
  beforeEach(() => {
    window.history.replaceState(null, '', '/sessions')
    route.value = matchRoute('/sessions')
  })

  it('改地址、改当前路由，并留下历史记录', () => {
    const before = history.length
    navigate('/devices')
    expect(location.pathname).toBe('/devices')
    expect(route.value.name).toBe('devices')
    expect(history.length).toBeGreaterThan(before)
  })

  it('replace 不留历史记录（提交过的表单不该在后退路径上）', () => {
    navigate('/devices')
    const after = history.length
    navigate('/settings', { replace: true })
    expect(location.pathname).toBe('/settings')
    expect(history.length).toBe(after)
  })

  it('跳转后的焦点落点是 main（§4.3）', async () => {
    document.body.innerHTML = '<main class="af-main" tabindex="-1"><h1>设备</h1></main>'
    const main = document.querySelector<HTMLElement>('main') as HTMLElement
    main.focus()

    navigate('/devices')
    await new Promise((r) => queueMicrotask(() => r(null)))

    expect(document.activeElement).toBe(main)
    expect(main.querySelector('h1')?.getAttribute('tabindex')).toBe('-1')
  })

  it('keepFocus 不抢焦点（切 tab 是视图切换，不是跳页）', async () => {
    document.body.innerHTML = '<button id="x">x</button><main class="af-main" tabindex="-1"></main>'
    const btn = document.querySelector<HTMLButtonElement>('#x') as HTMLButtonElement
    btn.focus()

    navigate('/sessions/s_1', { keepFocus: true })
    await new Promise((r) => queueMicrotask(() => r(null)))

    expect(document.activeElement).toBe(btn)
  })
})

describe('interceptLink', () => {
  beforeEach(() => {
    window.history.replaceState(null, '', '/sessions')
    route.value = matchRoute('/sessions')
    document.body.innerHTML = '<a id="a" href="/devices">设备</a>'
  })

  function anchor(): HTMLAnchorElement {
    return document.querySelector('#a') as HTMLAnchorElement
  }

  it('接管同源左键无修饰键的点击，翻译成路由跳转', () => {
    const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })
    expect(interceptLink(e, anchor())).toBe(true)
    expect(e.defaultPrevented).toBe(true)
    expect(location.pathname).toBe('/devices')
  })

  it('带 ⌘ / ⌃ / ⇧ 的点击不接管（那是「在新标签页打开」）', () => {
    for (const mod of [{ metaKey: true }, { ctrlKey: true }, { shiftKey: true }]) {
      const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0, ...mod })
      expect(interceptLink(e, anchor())).toBe(false)
      expect(e.defaultPrevented).toBe(false)
    }
    expect(location.pathname).toBe('/sessions')
  })

  it('中键不接管', () => {
    const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 1 })
    expect(interceptLink(e, anchor())).toBe(false)
  })

  it('外域链接不接管', () => {
    const outside = document.createElement('a')
    outside.href = 'https://example.com/x'
    const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })
    expect(interceptLink(e, outside)).toBe(false)
  })

  it('target 不是 _self 时不接管', () => {
    const a = anchor()
    a.target = '_blank'
    const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })
    expect(interceptLink(e, a)).toBe(false)
  })

  it('download 链接是「存文件」，不接管', () => {
    const a = anchor()
    a.setAttribute('download', '')
    const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })
    expect(interceptLink(e, a)).toBe(false)
  })

  it('已经被别人 preventDefault 的点击不重复处理', () => {
    const e = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })
    e.preventDefault()
    expect(interceptLink(e, anchor())).toBe(false)
  })
})

describe('startRouter', () => {
  it('popstate 时重算路由并把焦点交回 main', async () => {
    document.body.innerHTML = '<main class="af-main" tabindex="-1"></main>'
    const stop = startRouter()
    history.replaceState(null, '', '/agents')
    window.dispatchEvent(new PopStateEvent('popstate'))
    expect(route.value.name).toBe('agents')
    await new Promise((r) => queueMicrotask(() => r(null)))
    expect(document.activeElement).toBe(document.querySelector('main'))
    stop()
  })

  it('重复调用只装一次监听（返回的卸载函数不会把别人的监听拆掉）', () => {
    const removeSpy = vi.spyOn(window, 'removeEventListener')
    const a = startRouter()
    const b = startRouter()
    b()
    expect(removeSpy).not.toHaveBeenCalledWith('popstate', expect.anything())
    a()
    removeSpy.mockRestore()
  })
})
