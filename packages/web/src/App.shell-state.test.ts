/* ===========================================================================
   G10 / G12 的回归网（v0.8.1 第5轮）
   ---------------------------------------------------------------------------
   这两条 bug 原先**一条测试都没有**，所以「改回原样」是绿的：
     · G10 左栏收起态不落盘——换页 / 刷新就展开，用户每翻一页重收一次；
     · G12 登录页未登录就拉三个业务接口——必然 401，而认证页根本不渲染左栏。
   两个用例都按「改坏即红」设计，判红方式写在各自的注释里。
   =========================================================================== */

import { beforeEach, describe, expect, it, vi } from 'vitest'

import App from '@/App.vue'
import { matchRoute, route } from '@/router'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

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

/** 左栏那颗「折叠 / 展开侧栏」按钮。 */
function railToggle(w: ReturnType<typeof mountIn>): HTMLElement {
  const btn = w.element.querySelector<HTMLElement>('button[aria-label="折叠 / 展开侧栏"]')
  expect(btn, '左栏必须有一颗折叠按钮').not.toBeNull()
  return btn!
}

describe('G10：左栏收起态落盘，换页与刷新都保持', () => {
  beforeEach(() => {
    localStorage.clear()
  })

  it('点收起 → 写入 localStorage；重新挂载（等价于刷新）仍是收起', async () => {
    // 判红方式：把 `watch(rail, …)` 那段删掉 → 第一条断言红（键是 null）；
    // 把 `ref('open')` 改回初值常量 → 第二条断言红（重新挂载后展开）。
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(document.documentElement.dataset.rail, '缺省是展开，`data-rail` 应当不存在').toBeUndefined()

    railToggle(w).click()
    await flush(w)

    expect(document.documentElement.dataset.rail, '点一下之后应落到 <html data-rail="off">').toBe('off')
    expect(localStorage.getItem('af-rail'), '用户亲手拨的那一下要落盘').toBe('collapsed')

    // 换页：`rail` 是外壳自己的状态，换页不重建外壳，所以仍收起。
    w.unmount()
    const w2 = mountApp('/devices')
    await flush(w2)
    expect(document.documentElement.dataset.rail, '换页后仍收起').toBe('off')

    // 刷新：外壳重新挂载，初值从 localStorage 读回。
    w2.unmount()
    delete document.documentElement.dataset.rail
    const w3 = mountApp('/')
    await flush(w3)
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
    stubFetch(EMPTY_API)

    const w = mountApp('/project/p_1/session/s_1')
    await flush(w)
    expect(document.documentElement.dataset.rail, '会话页自动收起').toBe('off')
    expect(
      localStorage.getItem('af-rail'),
      '自动收起不许改写用户已落盘的偏好（它本来就该是 collapsed，这里守的是「不被动过」）',
    ).toBe('collapsed')

    // 会话页里点一次开关：可见态是 collapsed → 用户要展开 → `rail` 变 `open`，
    // 落盘必须是 `open`。
    railToggle(w).click()
    await flush(w)
    expect(document.documentElement.dataset.rail, '会话页里点开关应展开').toBeUndefined()
    expect(localStorage.getItem('af-rail'), '用户亲手展开，落盘 open').toBe('open')

    // 离开会话页：自动收起撤掉，恢复的是**用户偏好**（刚才是 open）。
    w.unmount()
    const w2 = mountApp('/')
    await flush(w2)
    expect(document.documentElement.dataset.rail, '离开会话页恢复用户偏好：展开').toBeUndefined()

    w2.unmount()
    localStorage.clear()
  })

  it('挂载在会话页时不会替用户写下一条偏好', async () => {
    // 判红方式：把 `readRailPref()` 的初值改成 `ref('collapsed')`（或任何
    // 在挂载期就写盘的实现）→ 本用例红。
    localStorage.clear()
    stubFetch(EMPTY_API)

    // 先落在会话页（自动收起），再离开。用户的偏好从头到尾没被写过。
    const w = mountApp('/project/p_1/session/s_1')
    await flush(w)
    expect(localStorage.getItem('af-rail'), '自动收起不是用户意愿').toBeNull()

    w.unmount()
    const w2 = mountApp('/')
    await flush(w2)
    expect(document.documentElement.dataset.rail, '离开会话页应展开（偏好缺省 open）').toBeUndefined()

    w2.unmount()
  })
})

describe('G12：认证页不发业务请求', () => {
  it('挂载在 /login 时，一个 /api/ 请求都不发', async () => {
    // 判红方式：把 `if (!isAuthPage.value)` 那层去掉 → 三条 401 请求回来，本用例红。
    const fetchSpy = vi.fn((url: string) => {
      void url
      return Promise.resolve(new Response('[]', { status: 200 }))
    })
    vi.stubGlobal('fetch', fetchSpy)
    try {
      const w = mountApp('/login')
      await flush(w)

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
    const fetchSpy = vi.fn((url: string) => {
      void url
      return Promise.resolve(new Response('[]', { status: 200 }))
    })
    vi.stubGlobal('fetch', fetchSpy)
    try {
      const w = mountApp('/')
      await flush(w)

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
})
