/* ===========================================================================
   G10 / G12 的回归网（v0.8.1 第5轮）
   ---------------------------------------------------------------------------
   这两条 bug 原先**一条测试都没有**，所以「改回原样」是绿的：
     · G10 左栏收起态不落盘——换页 / 刷新就展开，用户每翻一页重收一次；
     · G12 登录页未登录就拉三个业务接口——必然 401，而认证页根本不渲染左栏。
   两个用例都按「改坏即红」设计，判红方式写在各自的注释里。

   脚手架（`flushShell` / `mountShell` / `EMPTY_SHELL_API` / 那颗开关）
   与 `App.test.ts` 共用 `@/test/app-shell` 一份实现。
   =========================================================================== */

import { beforeEach, describe, expect, it, vi } from 'vitest'

import { AUTH_ROUTES, ROUTES } from '@/router'
import { stubFetch } from '@/test/http'
import {
  EMPTY_SHELL_API,
  clickRailToggle,
  flushShell,
  goTo,
  mountShell,
  railOf,
  railToggleOf,
} from '@/test/app-shell'

/** 一个「什么都不管，一律 200 空数组」的 fetch 桩：用来数请求，不关心内容。 */
function anyApi(): ReturnType<typeof vi.fn> {
  const spy = vi.fn((url: string) => {
    void url
    return Promise.resolve(new Response('[]', { status: 200 }))
  })
  vi.stubGlobal('fetch', spy)
  return spy
}

/**
 * 遍历用例用的「按路径给形状」桩：左栏三项之外，把详情页会读的那几条也
 * 给成**字段齐全的 JSON**。
 *
 * 为什么不能全给 `[]`：`anyApi` 那个全 `[]` 的桩让 `DeviceDetailPage` 的
 * `detail.deletable.allowed`（`DeviceDetailPage.vue:311`）与
 * `AgentDetailPage` 的 `detail.devices.length`（`AgentDetailPage.vue:191`）
 * 在首屏渲染上抛 `TypeError`——异常出现在渲染队列里，测试的断言不受影响，
 * 但 vitest 会把它记成 unhandled rejection 并让整次运行 exit 1。给这两条
 * 路径一个形状正确的响应是**测试夹具的问题**，不是产品代码的问题；产品
 * 侧那两行是对的（响应体由后端保证）。
 */
function shapedApi(): ReturnType<typeof vi.fn> {
  const spy = vi.fn((url: string) => {
    const path = new URL(String(url), 'http://hub.local').pathname
    let body: unknown = []
    if (/^\/api\/devices\/x_id\/detail$/.test(path)) {
      body = {
        device: { id: 'x_id', name: 'x', status: 'online', last_seen: 'unix:0' },
        bindings: [],
        sessions: [],
        deletable: { allowed: true, reason: null },
      }
    } else if (/^\/api\/agents\/x_name$/.test(path)) {
      body = { name: 'x', devices: [], sessions: [] }
    }
    return Promise.resolve(new Response(JSON.stringify(body), { status: 200 }))
  })
  vi.stubGlobal('fetch', spy)
  return spy
}

describe('G10：左栏收起态落盘，换页与刷新都保持', () => {
  beforeEach(() => {
    localStorage.clear()
  })

  it('点收起 → 写入 localStorage；重新挂载（等价于刷新）仍是收起', async () => {
    // 判红方式：把 `watch(rail, …)` 那段删掉 → 第一条断言红（键是 null）；
    // 把 `ref('open')` 改回初值常量 → 第二条断言红（重新挂载后展开）。
    stubFetch(EMPTY_SHELL_API)
    const w = mountShell('/')
    await flushShell(w)

    expect(document.documentElement.dataset.rail, '缺省是展开，`data-rail` 应当不存在').toBeUndefined()

    await clickRailToggle(w)

    expect(document.documentElement.dataset.rail, '点一下之后应落到 <html data-rail="off">').toBe('off')
    expect(localStorage.getItem('af-rail'), '用户亲手拨的那一下要落盘').toBe('collapsed')

    // 换页：`rail` 是外壳自己的状态，换页不重建外壳，所以仍收起。
    w.unmount()
    const w2 = mountShell('/devices')
    await flushShell(w2)
    expect(document.documentElement.dataset.rail, '换页后仍收起').toBe('off')

    // 刷新：外壳重新挂载，初值从 localStorage 读回。
    w2.unmount()
    delete document.documentElement.dataset.rail
    const w3 = mountShell('/')
    await flushShell(w3)
    expect(document.documentElement.dataset.rail, '刷新后仍收起（初值从 localStorage 读）').toBe('off')

    w3.unmount()
  })

  it('会话页的自动收起不是用户意愿：不落盘，也不改掉已落盘的偏好', async () => {
    // 判红方式（实测过）：把初值改回 `ref('open')` → 本用例红。
    //
    // **不能用它判「watch(rail) 还是 watch(effectiveRail)」**：实测在演示库
    // 走通的路径上两种写法写出同一个值——`watch` 都默认不是 `immediate`，
    // 挂载在会话页时不触发；用户在会话页点一次开关时 `effectiveRail` 也变成
    // `open`，两者同值。所以「以可见态为准落盘」这个错**在这几条路径上观察
    // 不到**，本用例不对它负责（详见 App.vue 里那段 watch 注释的说明）。
    // 它守的是「用户亲手拨的那一下要落盘」与「自动收起本身不许写盘」。
    localStorage.setItem('af-rail', 'collapsed')
    stubFetch(EMPTY_SHELL_API)

    const w = mountShell('/project/p_1/session/s_1')
    await flushShell(w)
    expect(railOf(), '会话页自动收起').toBe('collapsed')
    expect(
      localStorage.getItem('af-rail'),
      '自动收起不许改写用户已落盘的偏好（它本来就该是 collapsed，这里守的是「不被动过」）',
    ).toBe('collapsed')

    // 会话页里点一次开关：可见态是 collapsed → 用户要展开 → `rail` 变 `open`，
    // 落盘必须是 `open`。
    await clickRailToggle(w)
    expect(railOf(), '会话页里点开关应展开').toBe('open')
    expect(localStorage.getItem('af-rail'), '用户亲手展开，落盘 open').toBe('open')

    // 离开会话页：自动收起撤掉，恢复的是**用户偏好**（刚才是 open）。
    w.unmount()
    const w2 = mountShell('/')
    await flushShell(w2)
    expect(railOf(), '离开会话页恢复用户偏好：展开').toBe('open')

    w2.unmount()
    localStorage.clear()
  })

  it('挂载在会话页时不会替用户写下一条偏好', async () => {
    // 判红方式：把 `readRailPref()` 的初值改成 `ref('collapsed')`（或任何
    // 在挂载期就写盘的实现）→ 本用例红。
    localStorage.clear()
    stubFetch(EMPTY_SHELL_API)

    // 先落在会话页（自动收起），再离开。用户的偏好从头到尾没被写过。
    const w = mountShell('/project/p_1/session/s_1')
    await flushShell(w)
    expect(localStorage.getItem('af-rail'), '自动收起不是用户意愿').toBeNull()

    w.unmount()
    const w2 = mountShell('/')
    await flushShell(w2)
    expect(railOf(), '离开会话页应展开（偏好缺省 open）').toBe('open')

    w2.unmount()
  })
})

describe('G12：认证页不发业务请求', () => {
  it('挂载在 /login 时，一个 /api/ 请求都不发', async () => {
    // 判红方式：把 `if (!isAuthPage.value)` 那层去掉 → 三条 401 请求回来，本用例红。
    const fetchSpy = anyApi()
    try {
      const w = mountShell('/login')
      await flushShell(w)

      const urls = fetchSpy.mock.calls.map((c) => String(c[0]))
      const apiCalls = urls.filter((u) => u.includes('/api/'))
      expect(apiCalls, `登录页不该拉业务数据，实际发了：${apiCalls.join(', ')}`).toEqual([])

      w.unmount()
    } finally {
      vi.unstubAllGlobals()
    }
  })

  it('挂载在 / 时照常拉左栏的项目与计数（没把门禁开太大）', async () => {
    // 反向担保：上面那条不许靠「干脆全部不发请求」通过。
    const fetchSpy = anyApi()
    try {
      const w = mountShell('/')
      await flushShell(w)

      const urls = fetchSpy.mock.calls.map((c) => String(c[0]))
      expect(
        urls.some((u) => u.includes('/api/projects')),
        `非认证页必须照常拉项目列表，实际发了：${urls.join(', ')}`,
      ).toBe(true)
      expect(urls.some((u) => u.includes('/api/devices')), '设备计数也该照常拉').toBe(true)

      w.unmount()
    } finally {
      vi.unstubAllGlobals()
    }
  })

  /*
    **逐条遍历**，而不是只测 `/` 与 `/login` 两个点。

    G12 的门禁是一个谓词（`if (!isAuthPage.value)`），而「只测两个代表点」对
    **把谓词收窄**这类变异是瞎的：把 `!isAuthPage.value` 收窄成
    `!isAuthPage.value && route.value.name !== 'devices'`（设备页不再拉左栏
    数据）曾经整套用例全绿。上面的「挂在 / 时照常拉」抓不到它——它只钉了
    一个路由。

    判红方式（实测）：在 `App.vue` 的 `if (!isAuthPage.value)` 后面加上
    `&& route.value.name !== 'devices'` → 本用例红在「devices 没拉左栏数据」。

    认证路由自己**不进**这张表：它们由上面那条「一个 /api/ 都不发」守，
    两条一起才把谓词的两侧都钉住（非认证页全拉 / 认证页全不拉）。
  */
  it('每个非认证路由都照常拉左栏的项目与计数', async () => {
    const nonAuth = ROUTES.filter((r) => !AUTH_ROUTES.has(r.name))
    expect(nonAuth.length, '路由表里应当有多个非认证路由').toBeGreaterThan(5)

    // 每条路由各挂一次外壳，一次性收齐结果：断言放在循环**外面**。
    // 在循环里断言等于「diff 式跑测试」——红的时候得重新跑很多次才找得到
    // 是哪一个路由漏了。
    const missing: string[] = []
    for (const def of nonAuth) {
      // 把模式里的具名段换成实值，得到一条能匹配到同一个 `name` 的地址。
      const path = def.pattern.replace(/:([A-Za-z]+)/g, 'x_$1')
      const fetchSpy = shapedApi()
      const w = mountShell(path)
      await flushShell(w)
      const urls = fetchSpy.mock.calls.map((c) => String(c[0]))
      w.unmount()
      vi.unstubAllGlobals()

      const lacks = [
        ['/api/projects', urls.some((u) => u.includes('/api/projects'))],
        ['/api/devices', urls.some((u) => u.includes('/api/devices'))],
        ['/api/agents', urls.some((u) => u.includes('/api/agents'))],
      ]
        .filter(([, ok]) => !ok)
        .map(([u]) => u)
      if (lacks.length > 0) missing.push(`${def.name}（${path}）缺 ${lacks.join(' / ')}；实发：${urls.join(', ')}`)
    }

    expect(missing, '非认证路由都必须照常拉左栏那三项数据').toEqual([])
  })
})

/* 这一条不属于 G10/G12，但守的是同一类「外壳不许骗人」的事，且它需要
   `/devices` 这个**非认证**路由上的那颗开关还在原地——与上面那条遍历用例
   用的是同一份脚手架，放在这里少挂一次。 */
describe('外壳：收起态那颗开关始终在左栏里', () => {
  it('挂载在设备页时，折叠开关仍在左栏的品牌行里', async () => {
    stubFetch(EMPTY_SHELL_API)
    const w = mountShell('/devices')
    await flushShell(w)
    expect(railToggleOf(w).closest('.af-rail')).not.toBeNull()
    w.unmount()
  })
})

/* 让「换页」这条 helper 在本文件里也有一个使用者（它同时是 G10 的语义：
 * 换页不重建外壳，路由是模块级状态）。 */
describe('外壳：换页 helper 只动路由', () => {
  it('goTo 换页后外壳实例与收起态都不变', async () => {
    stubFetch(EMPTY_SHELL_API)
    const w = mountShell('/')
    await flushShell(w)
    const host = w.element
    goTo('/devices')
    await flushShell(w)
    expect(w.element, '换页不该重建外壳').toBe(host)
    expect(railOf()).toBe('open')
    w.unmount()
  })
})
