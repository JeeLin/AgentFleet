import { beforeEach, describe, expect, it, vi } from 'vitest'

import { readRepoFile } from '@/test/css'

import {
  INDEX_PATH,
  ROUTES,
  AUTH_ROUTES,
  interceptLink,
  legacyTarget,
  matchRoute,
  navigate,
  route,
  startRouter,
} from '@/router'

describe('路由表：就是 `prototype/` 的 21 页', () => {
  it('路由表与 `prototype/` 的页面对照表逐条一致', () => {
    // 这张表是 v0.7.0 子任务 8 的落点清单。**顺序有意义**：`/projects/new`
    // 必须排在 `/projects/:id/...` 之前，否则新建会被当成 id 为 `new` 的项目。
    //
    // 认证路由排最后：它们与其余路径无前缀关系，放末尾是为了让页面路由连成一片。
    expect(ROUTES.map((r) => `${r.name} ${r.pattern}`)).toEqual([
      'overview /',
      'devices /devices',
      'agents /agents',
      'settings /settings',
      'projects-new /projects/new',
      'projects-archived /projects/archived',
      'board /project/:id/board',
      'project-task /project/:id/task/:taskId',
      'execution /project/:id/execution/:execId',
      'replay /project/:id/execution/:execId/replay',
      'timeline /project/:id/timeline',
      'session /project/:id/session/:sessionId',
      'project-bind /project/:id/bind',
      'project-settings /project/:id/settings',
      'project-schedules /project/:id/schedules',
      'inbox /inbox',
      'inbox-activity /inbox/activity',
      'butler /butler',
      'settings-skills /settings/skills',
      'settings-integrations /settings/integrations',
      'settings-notifications /settings/notifications',
      'login /login',
      'setup /setup',
    ])
  })

  /**
   * **每一条路由都必须真的可达**（子任务 8 的判据「照着原型点一遍，每一页都
   * 点得到」）。这条用例把路由表与「一个具体地址」绑在一起：只改表不改这里
   * 会失败，只改这里不改表也会失败。
   *
   * 与上面那条清单用例不重复：那条盯**表和顺序**，这条盯**匹配结果**。
   * 只写清单的话，`matchPattern` 写坏了（比如具名段只认一层）清单仍然绿，
   * 而页面上点进去全是 404。
   */
  it('每一条路由都用一个具体地址匹配得上（逐条可达）', () => {
    const samples: Record<string, string> = {
      overview: '/',
      devices: '/devices',
      agents: '/agents',
      settings: '/settings',
      'projects-new': '/projects/new',
      'projects-archived': '/projects/archived',
      board: '/project/p1/board',
      'project-task': '/project/p1/task/t1',
      execution: '/project/p1/execution/e1',
      replay: '/project/p1/execution/e1/replay',
      timeline: '/project/p1/timeline',
      session: '/project/p1/session/s1',
      'project-bind': '/project/p1/bind',
      'project-settings': '/project/p1/settings',
      'project-schedules': '/project/p1/schedules',
      inbox: '/inbox',
      'inbox-activity': '/inbox/activity',
      butler: '/butler',
      'settings-skills': '/settings/skills',
      'settings-integrations': '/settings/integrations',
      'settings-notifications': '/settings/notifications',
      login: '/login',
      setup: '/setup',
    }

    // 样本必须与路由表一一对应：漏一条或多一条都要红。
    expect(Object.keys(samples).sort()).toEqual(ROUTES.map((r) => r.name).sort())

    for (const [name, path] of Object.entries(samples)) {
      expect(matchRoute(path).name, `${path} 必须匹配成 ${name}`).toBe(name)
    }
  })

  /**
   * `/agents` 从 v0.7.0 起**重新成为一条路由**（新原型把它单独成一页）。
   *
   * v0.5.0 曾把它并进 `/devices?view=agents` 并靠 `LEGACY_PATHS` 接住；现在
   * 它是真路由，所以那条 `LEGACY_PATHS` 条目**仍然保留**但不再是唯一去处——
   * 保留它是因为老书签 `/agents` 现在直接命中路由表，`legacyTarget` 那条
   * 只对「还没解析过路由的裸调用」有意义。这里断言的是**它不再是 not-found**。
   */
  it('/agents 是路由，不是已删除的地址', () => {
    expect(matchRoute('/agents').name).toBe('agents')
  })

  /**
   * 会话迁到项目下之后，旧地址**不能静默 404**。
   *
   * 用户在 `/sessions/abc` 上有书签，我们自己的文档里也写着这个地址。
   * 静默 404 的症状是「点收藏夹，看到『这个地址没有页面』」。
   *
   * **但也不能精确还原**：新地址是 `/project/:id/session/:sessionId`，旧地址里
   * 没有项目 id，要还原得先查「这个会话属于哪个项目」——那需要 IO，而路由表
   * 是纯数据。所以统一落到总览，由页面明说「会话已迁到项目下」。
   */
  it('/sessions* 是已删除的地址，解析出去处而不是 404', () => {
    for (const old of ['/sessions', '/sessions/new', '/sessions/abc', '/tasks', '/artifacts']) {
      expect(matchRoute(old).name, `${old} 不该有路由`).toBe('not-found')
      expect(legacyTarget(old), `${old} 必须解析出去处`).not.toBeNull()
    }
    // 前缀表只认「前缀 + 一段」：`/sessionsfoo` 不是旧地址。
    expect(legacyTarget('/sessionsfoo')).toBeNull()
    // 对照组：真的不存在的路径没有去处。
    expect(legacyTarget('/nope')).toBeNull()
  })

  it('/login 与 /setup 匹配成认证路由，且被标进 AUTH_ROUTES', () => {
    expect(matchRoute('/login').name).toBe('login')
    expect(matchRoute('/setup').name).toBe('setup')
    expect(AUTH_ROUTES.has('login')).toBe(true)
    expect(AUTH_ROUTES.has('setup')).toBe(true)
    // 导航页一个都不在认证集合里：误加会让某个页面凭空失去外壳。
    // **逐个列全**而不是挑几个：新加页面时忘掉这一条比漏测更常见。
    for (const r of ROUTES) {
      if (r.name === 'login' || r.name === 'setup') continue
      expect(AUTH_ROUTES.has(r.name), `${r.name} 不该是认证页`).toBe(false)
    }
  })

  it('没有任何能长出 /w/:nodeId/*path 或 /workspace 的占位', () => {
    const patterns = ROUTES.map((r) => r.pattern)
    expect(patterns.some((p) => p.startsWith('/w') || p.includes('workspace'))).toBe(false)
  })

  it('根路径落在总览——项目是入口，会话不再是顶层', () => {
    expect(INDEX_PATH).toBe('/')
    expect(matchRoute('/').name).toBe('overview')
  })

  it('新建项目排在项目内路由之前（否则会被当成 id 为 new 的项目）', () => {
    const names = ROUTES.map((r) => r.name)
    expect(names.indexOf('projects-new')).toBeLessThan(names.indexOf('board'))
    // 段数不同本来就不会互吃，这里顺带钉住「`/projects/archived` 也是固定段」。
    expect(matchRoute('/projects/archived').name).toBe('projects-archived')
    expect(matchRoute('/projects/new').name).toBe('projects-new')
  })
})

describe('matchRoute', () => {
  it('字面段按字面比', () => {
    expect(matchRoute('/devices').name).toBe('devices')
    expect(matchRoute('/settings').name).toBe('settings')
  })

  it('多层具名段都进 params', () => {
    const task = matchRoute('/project/p1/task/t1')
    expect(task.name).toBe('project-task')
    expect(task.params).toEqual({ id: 'p1', taskId: 't1' })

    const exec = matchRoute('/project/p1/execution/e1')
    expect(exec.params).toEqual({ id: 'p1', execId: 'e1' })

    const replay = matchRoute('/project/p1/execution/e1/replay')
    expect(replay.name).toBe('replay')
    expect(replay.params).toEqual({ id: 'p1', execId: 'e1' })

    const session = matchRoute('/project/p1/session/s1')
    expect(session.params).toEqual({ id: 'p1', sessionId: 's1' })
  })

  it('具名段做百分号解码', () => {
    const r = matchRoute('/project/p%2F1/board')
    expect(r.params.id).toBe('p/1')
  })

  it('query 进 searchParams', () => {
    const r = matchRoute('/project/p1/board?tab=failed')
    expect(r.query.get('tab')).toBe('failed')
  })

  it('段数不匹配就不中（/project/p1/board/x 不退化成看板）', () => {
    expect(matchRoute('/project/p1/board/x').name).toBe('not-found')
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
    window.history.replaceState(null, '', '/')
    route.value = matchRoute('/')
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
})

describe('interceptLink', () => {
  beforeEach(() => {
    window.history.replaceState(null, '', '/')
    route.value = matchRoute('/')
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
    expect(location.pathname).toBe('/')
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
    history.replaceState(null, '', '/settings')
    window.dispatchEvent(new PopStateEvent('popstate'))
    expect(route.value.name).toBe('settings')
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

/**
 * `ROUTES` 与 `App.vue` 的 `PAGES` 是两张表，**必须同步**。
 *
 * 加了路由不加 `PAGES` 那一行，`page` 会是 `undefined`，页面区一片空白——
 * 而类型系统看不出来（`PAGES` 是 `as const`，索引用的是 `RouteName` 联合的
 * 一个子集）。所以这条用例在**运行时**比对键集。
 *
 * 读 `App.vue` 的源码而不是 import 它：`App.vue` 会拉起整棵组件树与
 * `startRouter` 的副作用，在单测里跑起来等于测了一个假应用。这里只提取
 * `PAGES` 那段字面量的键。
 */
describe('ROUTES 与 PAGES 的键集一致', () => {
  it('每一条路由（除 not-found）都在 PAGES 里有一行', () => {
    const src = readRepoFile('packages/web/src/App.vue')
    const block = src.match(/const PAGES = \{([\s\S]*?)\n\} as const/)
    expect(block, 'App.vue 里找不到 `const PAGES = { ... } as const`').not.toBeNull()

    const keys = [...(block as RegExpMatchArray)[1].matchAll(/^\s*(?:'([^']+)'|(\w+)):/gm)].map(
      (m) => m[1] ?? m[2],
    )
    const wanted = ROUTES.map((r) => r.name)
    expect(keys.slice().sort()).toEqual(wanted.slice().sort())
  })
})
