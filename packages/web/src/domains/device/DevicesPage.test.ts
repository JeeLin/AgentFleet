import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { DOMWrapper } from '@vue/test-utils'

import DevicesPage from '@/domains/device/DevicesPage.vue'
import { detectDownloadPlatform, downloadAgentBinary } from '@/domains/device/api'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { legacyTarget, matchRoute, route } from '@/router'

/** 等 fetch 链 + Vue 渲染队列都走完。 */
async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

describe('DevicesPage：渲染', () => {
  it('列出设备，并把状态摘要分开数（在线 / 重连中 / 离线）', async () => {
    stubFetch({
      '/api/devices': {
        body: [
          device({ id: 'n_1', name: 'mbp.local', status: 'online' }),
          device({ id: 'n_2', name: 'pi.local', status: 'reconnecting', derived: true }),
          device({ id: 'n_3', name: 'old.local', status: 'offline' }),
        ],
      },
    })
    const w = mountIn(DevicesPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('mbp.local')
    expect(text).toContain('pi.local')
    expect(text).toContain('old.local')
    expect(text).toContain('在线 1')
    expect(text).toContain('重连中 1')
    expect(text).toContain('离线 1')
    expect(text).toContain('共 3 台')
  })

  it('这一页没有打开终端 / 文件的入口（它们是会话内的面板）', async () => {
    stubFetch({ '/api/devices': { body: [device()] } })
    const w = mountIn(DevicesPage)
    await flush(w)
    const text = w.element.textContent ?? ''
    expect(text).toContain('没有打开它们的入口')
  })

  it('⏎ 看详情：把详情显示成内联块，而不是弹一个锁焦点的模态', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_1', name: 'mbp.local' })] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    const row = w.element.querySelector<HTMLElement>('tbody [data-row]')
    expect(row).not.toBeNull()
    row?.focus()
    row?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))
    await w.vm.$nextTick()

    const inspect = w.element.querySelector('.af-inspect')
    expect(inspect?.textContent).toContain('n_1')
    expect(w.element.querySelector('[role="dialog"]')).toBeNull()
  })
})

/**
 * 身份锚在列表与详情里必须是**同一个值、同一段文字**。
 *
 * 这台设备的身份是 `node_id`（`api.ts`：「永不变」），不是 `key_fingerprint`
 * ——后者是 Hub 打码的（前 3 + `***` + 后 3），拿它跟 `afcli` 输出对不上。
 * 两处各显示一个的话，用户复制的那一串有一半场合是错的标识。
 */
describe('DevicesPage：身份锚在列表与详情里一致', () => {
  it('两处显示的都是 node_id，指纹只作凭证信息', async () => {
    stubFetch({
      '/api/devices': {
        body: [device({ id: 'n_7f3a9c', name: 'mbp.local', key_fingerprint: '9f3***21b' })],
      },
    })
    const w = mountIn(DevicesPage)
    await flush(w)

    const row = w.element.querySelector<HTMLElement>('tbody [data-row]')
    const idCell = row?.querySelector<HTMLElement>('.af-dev-id')
    expect(idCell?.textContent?.trim()).toBe('n_7f3a9c')
    // 指纹不在列表里当身份显示，但悬停仍能读到（凭证问题要看它）
    expect(idCell?.textContent).not.toContain('9f3***21b')
    expect(idCell?.getAttribute('title')).toContain('9f3***21b')

    row?.focus()
    row?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))
    await w.vm.$nextTick()

    const inspect = w.element.querySelector<HTMLElement>('.af-inspect')
    const kv = Array.from(inspect?.querySelectorAll<HTMLElement>('.af-kv__v') ?? []).map((v) =>
      v.textContent?.trim(),
    )
    // 第一项就是 node_id 那一格，与列表里画的是同一个值
    expect(kv[0]).toBe('n_7f3a9c')
    expect(kv[1]).toBe('9f3***21b')
  })
})

describe('DevicesPage：空态', () => {
  it('一台设备都没有时给出接入命令与 + M3 的差别', async () => {
    stubFetch({ '/api/devices': { body: [] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有设备接入')
    expect(text).toContain('afcli run')
    expect(w.element.querySelector('tbody')).toBeNull()
  })

  it('空态与加载态不混用（加载中是骨架，不是「设备没了」）', async () => {
    // 直接持有 resolver，而不是 `let release: (() => void) | null = null`
    // 再在 Promise 执行器里赋值：后者的赋值发生在回调里，TS 的控制流分析
    // 不跟踪它，到了 `release?.()` 处仍认定 `release` 是 `null`（TS2349）。
    let resolveDevices: (r: Response) => void = () => undefined
    const pending = new Promise<Response>((resolve) => {
      resolveDevices = resolve
    })
    vi.stubGlobal('fetch', vi.fn(() => pending))
    const w = mountIn(DevicesPage)
    await w.vm.$nextTick()

    expect(w.element.querySelector('[aria-busy="true"]')).not.toBeNull()
    expect(w.element.textContent).not.toContain('还没有设备接入')

    resolveDevices(
      new Response(JSON.stringify([]), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    )
    await flush(w)
    expect(w.element.textContent).toContain('还没有设备接入')
  })
})

describe('DevicesPage：读失败', () => {
  it('Hub 返回错误码时显示可读理由与重试', async () => {
    stubFetch({ '/api/devices': { status: 500, body: { error: 'boom' } } })
    const w = mountIn(DevicesPage)
    await flush(w)

    const alert = w.element.querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('boom')
    expect(alert?.textContent).toContain('重试')
  })

  it('Hub 连不上跟「读失败」不是同一种文案', async () => {
    vi.stubGlobal('fetch', vi.fn(() => Promise.reject(new Error('down'))))
    const w = mountIn(DevicesPage)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('Hub 连不上')
  })

  it('重试按钮真的再打一次端点', async () => {
    const fn = stubFetch({ '/api/devices': { status: 500, body: { error: 'boom' } } })
    const w = mountIn(DevicesPage)
    await flush(w)
    const before = fn.mock.calls.length
    await w.element.querySelector<HTMLElement>('[role="alert"] button')?.click()
    await flush(w)
    expect(fn.mock.calls.length).toBeGreaterThan(before)
  })
})

describe('DevicesPage：离线设备', () => {
  it('离线设备仍列出来，只有「用它开会话」被禁用且写明理由', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_off', status: 'offline' })] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    expect(w.element.querySelector('tbody [data-row]')).not.toBeNull()
    // 用 `data-act="create"` 而不是「tbody 里第一个 button」：行内现在还有
    // 设备名那个**进详情页**的按钮，靠顺序取会取错。
    const btn = w.element.querySelector<HTMLButtonElement>('tbody [data-act="create"]')
    expect(btn?.disabled).toBe(true)
    expect(btn?.getAttribute('title')).toContain('离线')
  })

  it('「用它开会话」恒禁用，且写明「没有可达入口」的理由', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_1' })] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    const btn = w.element.querySelector<HTMLButtonElement>('tbody [data-act="create"]')
    // **在线设备也禁用**：这个入口在 v0.8.0 没有任何可达目的地。
    expect(btn?.disabled, '在线设备也必须禁用').toBe(true)
    expect(btn?.getAttribute('aria-disabled')).toBe('true')
    expect(btn?.getAttribute('title')).toContain('v0.8.2')
    // 理由要**看得见**，不能只挂在 `title` 上（触屏与键盘用户看不到 tooltip）。
    const why = btn?.parentElement?.textContent ?? ''
    expect(why).toContain('v0.8.2')

    // 点它不跳转：地址栏原地不动（以前会变成 `/sessions/new?device=…` 再被
    // `LEGACY_PATHS` replace 回 `/`，用户看到的是「点了没反应还回首页」）。
    // 与点之前的快照比：用例之间共享同一个 jsdom，地址栏不保证从 `/` 开始。
    const before = location.pathname + location.search
    btn?.click()
    await flush(w)
    expect(location.pathname + location.search, '点禁用按钮不该改地址栏').toBe(before)
  })

  it('`/sessions/new` 本来就不是真路由——这正是上面禁用它的理由', async () => {
    // 这条**不挂载组件**，只钉路由事实：如果哪天 `/sessions/new` 变成了真路由，
    // 上面那条用例的「禁用」就失去依据，两处必须一起改。
    // 判红方式：给 `router.ts` 加 `{ name: 'session-new', pattern: '/sessions/new' }`
    // → 第一条断言红（那时应连同上面那条用例一起改写，而不是悄悄删掉）。
    expect(matchRoute('/sessions/new').name, '本里程碑 `/sessions/new` 不是真路由').toBe(
      'not-found',
    )
    expect(legacyTarget('/sessions/new'), '旧地址的去处是总览，不是新建页').toBe('/')
  })
})

describe('DevicesPage：移除设备', () => {
  /** 点「移除」→「确认移除」。返回 fetch 的 mock 以检查发了什么请求。 */
  async function removeDevice(w: ReturnType<typeof mountIn>, id = 'n_1'): Promise<void> {
    w.element.querySelector<HTMLButtonElement>(`[data-remove="${id}"]`)?.click()
    await w.vm.$nextTick()
    w.element.querySelector<HTMLButtonElement>(`[data-confirm="${id}"]`)?.click()
    await flush(w)
  }

  it('确认后发 DELETE /api/devices/{id}，并重新拉列表把它去掉', async () => {
    let devices = [device({ id: 'n_1', name: 'pi.local' })]
    // `stubFetch` 只按路径分派，不看方法；而这一页对同一个路径既有 GET
    // （列表）又有 DELETE（移除）。所以这里自己装一个按方法分派的 fetch。
    const calls: Array<{ url: string; method: string }> = []
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      const method = (init?.method ?? 'GET').toUpperCase()
      calls.push({ url, method })
      if (method === 'DELETE') {
        devices = []
        return new Response(null, { status: 204 })
      }
      return new Response(JSON.stringify(devices), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    })
    vi.stubGlobal('fetch', fetchMock)

    const w = mountIn(DevicesPage)
    await flush(w)
    await removeDevice(w)

    expect(calls.filter((c) => c.method === 'DELETE').map((c) => c.url)).toEqual([
      expect.stringContaining('/api/devices/n_1'),
    ])
    expect(w.element.textContent).not.toContain('pi.local')
    expect(w.element.textContent).toContain('还没有设备接入')
  })

  it('**不碰会话列表**：删设备不级联删会话，所以这一页不该去动它', async () => {
    // 删那条路径必须返回成功。我第一版漏了它 → stubFetch 给未声明路径回 500
    // → `remove()` 走失败分支直接 return，`listSessions()` 那段根本没执行，
    // 于是把页面改成「删完顺手拉会话列表」这个反例跑下来仍然是绿的。
    const fetchMock = stubFetch({
      '/api/devices': { body: [device({ id: 'n_1' })] },
      // DELETE 的路径要单独声明（stubFetch 只按路径分派，不看方法）
      '/api/devices/n_1': { empty: true, status: 204 },
    })
    const w = mountIn(DevicesPage)
    await flush(w)
    await removeDevice(w)

    const all = fetchMock.mock.calls.map(
      (c) => `${(c[1] as RequestInit | undefined)?.method ?? 'GET'} ${String(c[0])}`,
    )
    // 先钉住「删除真的发生了」，否则这条断言又退化成「什么都没发生所以没请求」
    expect(all, `实际请求：${all.join(', ')}`).toContain('DELETE /api/devices/n_1')
    expect(all.some((x) => x.includes('/api/sessions')), `实际请求：${all.join(', ')}`).toBe(false)
  })

  it('404 当成「已经删掉了」：不弹错误', async () => {
    // 第一次 GET 必须有这台设备，否则「移除」按钮根本不渲染，这个用例会
    // 变成空转（点不到按钮 = 没发 DELETE = 没报错，一路绿）。这正是我第一版
    // 写成「列表恒空」时踩的坑：反例（404 也报错）跑下来仍然是绿的。
    let listed: unknown[] = [device({ id: 'n_1', name: 'pi.local' })]
    vi.stubGlobal(
      'fetch',
      vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
        const method = (init?.method ?? 'GET').toUpperCase()
        if (method === 'DELETE') {
          // 另一个标签页先删了：目标状态已达成，只是这次没删到行
          listed = []
          return new Response(JSON.stringify({ error: 'not_found' }), {
            status: 404,
            headers: { 'Content-Type': 'application/json' },
          })
        }
        return new Response(JSON.stringify(listed), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)
    expect(w.element.querySelector('[data-remove="n_1"]')).not.toBeNull()
    await removeDevice(w)
    expect(w.element.querySelector('[role="alert"]')).toBeNull()
    // 判据的另一半：它真的把那一行去掉了，而不是「什么都没发生所以没报错」
    expect(w.element.textContent).toContain('还没有设备接入')
  })

  it('真失败时报出理由，而不是静默地把行留在那儿', async () => {
    const devices = [device({ id: 'n_1', name: 'pi.local' })]
    vi.stubGlobal(
      'fetch',
      vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
        const method = (init?.method ?? 'GET').toUpperCase()
        if (method === 'DELETE') {
          return new Response(JSON.stringify({ error: 'internal_error' }), {
            status: 500,
            headers: { 'Content-Type': 'application/json' },
          })
        }
        return new Response(JSON.stringify(devices), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)
    await removeDevice(w)
    // 行还在（Hub 上没删掉），同时要有一条可读的理由
    expect(w.element.textContent).toContain('pi.local')
    const alert = w.element.querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('没能移除')
  })
})

/* ===========================================================================
   分段：设备 / Agent 目录
   ---------------------------------------------------------------------------
   用户裁定侧栏只留会话 / 设备 / 设置，`/agents` 路由删除、目录并进本页。
   这一组守三件**会静默坏掉**的事：
   ① 默认分段是设备（地址不带 `view` 时）；
   ② `?view=agents` 真的渲染目录，而不是设备表；
   ③ 分段切换用 `replace`（不进历史栈），否则「后退」变成在分段之间来回跳。
   =========================================================================== */
describe('DevicesPage：设备 / Agent 目录分段', () => {
  /** 把路由状态摆到某个地址。**必须显式设**：`route` 是模块级 ref，
   *  上一条测试留下的值会让「默认分段」那条悄悄通过。 */
  function atPath(path: string): void {
    window.history.replaceState(null, '', path)
    route.value = matchRoute(path)
  }

  const SEG_API = {
    '/api/devices': { body: [device({ id: 'n_1', name: 'mbp.local' })] },
    '/api/agents': { body: [] },
  }

  it('地址不带 view 时默认是设备分段', async () => {
    atPath('/devices')
    stubFetch(SEG_API)
    const w = mountIn(DevicesPage)
    await flush(w)

    expect(w.element.textContent).toContain('mbp.local')
    // 目录的横幅不在：说明渲染的确实是设备表
    expect(w.element.querySelector('[role="note"]')).toBeNull()
  })

  it('?view=agents 渲染 Agent 目录，而不是设备表', async () => {
    atPath('/devices?view=agents')
    stubFetch(SEG_API)
    const w = mountIn(DevicesPage)
    await flush(w)

    // 目录的常驻横幅在（`role="note"`），设备表不在
    expect(w.element.querySelector('[role="note"]')).not.toBeNull()
    expect(w.element.textContent).not.toContain('在线 1')
  })

  /**
   * 分段切换是**视图切换**（DECISIONS D10），用 `replaceState`：不进历史栈。
   *
   * 反向验证：把 `selectSegment` 里的 `{ replace: true }` 去掉，这条会红在
   * 「历史长度变了」——而那时的症状是「点两下分段，后退键要按两下才离开本页」。
   */
  it('切分段不进历史栈（replace 而不是 push）', async () => {
    atPath('/devices')
    stubFetch(SEG_API)
    const w = mountIn(DevicesPage)
    await flush(w)

    const before = history.length
    const tabs = Array.from(w.element.querySelectorAll<HTMLButtonElement>('[role="tab"]'))
    const agentsTab = tabs.find((t) => t.textContent?.includes('Agent 目录'))
    expect(agentsTab, '分段条上要有「Agent 目录」').toBeTruthy()
    agentsTab?.click()
    await flush(w)

    expect(location.search).toBe('?view=agents')
    expect(history.length).toBe(before)
    // 点完之后目录真的出来了
    expect(w.element.querySelector('[role="note"]')).not.toBeNull()
  })
})

/**
 * 平台推断与 URL 拼装是**纯逻辑**，不该靠渲染整页去验：拼错了的症状是
 * 「Windows 用户拿到 Linux 二进制」，而组件用例看不出 URL 上的 query。
 */
describe('afcli 下载：浏览器平台推断', () => {
  it('常见 UA / platform 推断出后端认识的 os/arch', () => {
    expect(
      detectDownloadPlatform(
        'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36',
        'Win32',
      ),
    ).toEqual({ os: 'windows', arch: 'amd64' })
    // Apple Silicon 的 Safari/Chrome 出于兼容仍报 `Intel Mac OS X` / `MacIntel`；
    // 镜像里只有 `afcli-macos-arm64`，所以这种 UA 必须归 arm64，否则点下载就 404。
    expect(
      detectDownloadPlatform(
        'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 Version/17.5 Safari/605.1.15',
        'MacIntel',
      ),
    ).toEqual({ os: 'mac', arch: 'arm64' })
    expect(
      detectDownloadPlatform('Mozilla/5.0 (Macintosh; ARM64 Mac OS X 15_0)', 'MacARM64'),
    ).toEqual({ os: 'mac', arch: 'arm64' })
    // 只有明确写了 x86_64 架构字样的才报 amd64（Hub 没有该产物，如实 404）。
    expect(
      detectDownloadPlatform('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15)', 'MacIntel x86_64'),
    ).toEqual({ os: 'mac', arch: 'amd64' })
    expect(
      detectDownloadPlatform('Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36', 'Linux x86_64'),
    ).toEqual({ os: 'linux', arch: 'amd64' })
    expect(
      detectDownloadPlatform('Mozilla/5.0 (X11; Linux aarch64)', 'Linux aarch64'),
    ).toEqual({ os: 'linux', arch: 'arm64' })
  })

  it('UA 认不出来时按 Linux/amd64 兜底，而不是抛错', () => {
    expect(detectDownloadPlatform('', '')).toEqual({ os: 'linux', arch: 'amd64' })
  })

  it('downloadAgentBinary 发出的 URL 带 os/arch query', async () => {
    const seen: string[] = []
    const fn = vi.fn(async (input: RequestInfo | URL) => {
      seen.push(String(input))
      return new Response(new Blob(['\x7fELF-fake']), { status: 200 })
    })
    vi.stubGlobal('fetch', fn)

    const got = await downloadAgentBinary({ os: 'windows', arch: 'amd64' })

    expect(seen).toHaveLength(1)
    const url = new URL(seen[0], 'http://hub.local')
    expect(url.pathname).toBe('/api/agent-binary')
    expect(url.searchParams.get('os')).toBe('windows')
    expect(url.searchParams.get('arch')).toBe('amd64')
    // 附件名从响应头读的契约不变：没给头就回落 afcli。
    expect(got.filename).toBe('afcli')
  })
})

describe('DevicesPage：空态里的 afcli 下载', () => {
  /**
   * **必须显式把路由摆回设备分段。** `route` 是模块级 `ref`，同一个文件里
   * 先跑的分段用例会把地址设成 `?view=agents`，而那个值会留到后面的用例里
   * ——症状是「单独跑绿、全量跑红」，且失败信息说「找不到下载按钮」，让人
   * 去怀疑模板而不是怀疑路由状态。（本文件的分段 describe 里也有同样一段
   * 注释，这里踩的就是它说的那个坑。）
   */
  beforeEach(() => {
    window.history.replaceState(null, '', '/devices')
    route.value = matchRoute('/devices')
  })

  /**
   * 空态那句「在设备上跑 afcli run」有个前提——那台设备上**已经有** afcli。
   * 离线/内网环境里够不到 GitHub，这句话就成了一句空话。所以这里给它一条
   * 实际的路：从**这一台** Hub 直接下发。
   *
   * `stubFetch` 只能造 JSON 响应，触发不了 `blob()`，所以这里手搓 fetch。
   * `URL.createObjectURL` 在 jsdom 里不存在，必须打桩——不打桩时用例会以
   * 「URL.createObjectURL is not a function」失败，而那不是被测代码的错。
   */
  function stubBlobFetch(onRequest: (path: string) => Response): {
    revoked: string[]
    clicked: string[]
    /** 每次 fetch 的完整 URL（含 query），供「平台参数真的带上了」断言用。 */
    seen: string[]
  } {
    // **只 `spyOn` 那两个方法，绝不 `vi.stubGlobal('URL', {...URL})`**：
    // 把 `URL` 换成普通对象会让页面里 `new URL(...)` 一类的构造直接不可用
    // （曾这么写，症状是按钮点了但 fetch 从未发出，断言却报「找不到按钮」，
    // 排障方向完全被带偏）。
    const revoked: string[] = []
    let seq = 0
    vi.spyOn(URL, 'createObjectURL').mockImplementation(() => {
      seq += 1
      return `blob:fake-${seq}`
    })
    vi.spyOn(URL, 'revokeObjectURL').mockImplementation((u: string) => {
      revoked.push(u)
    })
    // 隐藏 `<a>` 的 click 在 jsdom 下会真的尝试导航；这里只记录它被点过。
    const clicked: string[] = []
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (
      this: HTMLAnchorElement,
    ) {
      clicked.push(this.download)
    })
    const seen: string[] = []
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
        seen.push(raw)
        return onRequest(new URL(raw, 'http://hub.local').pathname)
      }),
    )
    return { revoked, clicked, seen }
  }

  it('空态给出下载入口，点了就按浏览器平台从这一台 Hub 取二进制', async () => {
    // 把浏览器伪装成 Windows：Linux Hub 上缺省回落 `std::env::consts::OS`，
    // 这一条正是「Windows 用户点下载拿到 Linux 文件」那条 bug 的回归断言。
    vi.stubGlobal('navigator', {
      userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)',
      platform: 'Win32',
    })
    const { revoked, clicked, seen } = stubBlobFetch((path) =>
      path === '/api/agent-binary'
        ? new Response(new Blob(['\x7fELF-fake']), {
            status: 200,
            headers: {
              'Content-Disposition': 'attachment; filename="afcli-windows-amd64.exe"',
            },
          })
        : new Response(JSON.stringify([]), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)

    const btn = [...w.element.querySelectorAll('button')].find((b) =>
      (b.textContent ?? '').includes('下载 afcli'),
    )
    expect(btn, '空态里必须有下载 afcli 的入口').toBeTruthy()
    btn!.click()
    await flush(w)

    // 下载真的被触发，且**用的是响应头里的文件名**（带平台）。
    // 前端若写死 `afcli`，同时有 amd64 与 arm64 两台机器时两个文件同名，分不清。
    expect(clicked).toEqual(['afcli-windows-amd64.exe'])
    // 不带 query 时后端会回落到 Hub 自己的 Linux 平台——这条断言会红。
    const binaryUrl = seen
      .map((s) => new URL(s, 'http://hub.local'))
      .find((u) => u.pathname === '/api/agent-binary')
    expect(binaryUrl, '必须真的请求了 /api/agent-binary').toBeTruthy()
    expect(binaryUrl?.searchParams.get('os')).toBe('windows')
    expect(binaryUrl?.searchParams.get('arch')).toBe('amd64')
    // revoke 必须发生：不释放的话页面存活期间一直占着这份 blob。
    expect(revoked).toEqual(['blob:fake-1'])
  })

  it('响应头没给文件名时回落 afcli（拿不到头不是下载失败的理由）', async () => {
    // `Content-Disposition` 是可选头。服务端没给（或中间层剥了）时，
    // 下载仍必须成功——只是名字退化成 `afcli`。若这里抛错，一个纯粹的
    // 命名问题会变成「下载功能坏了」。
    const { clicked } = stubBlobFetch((path) =>
      path === '/api/agent-binary'
        ? new Response(new Blob(['\x7fELF-fake']), { status: 200 })
        : new Response(JSON.stringify([]), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)
    const btn = [...w.element.querySelectorAll('button')].find((b) =>
      (b.textContent ?? '').includes('下载 afcli'),
    )
    btn!.click()
    await flush(w)

    expect(clicked).toEqual(['afcli'])
    expect(w.element.textContent ?? '').not.toMatch(/下载.*失败/)
  })

  it('Hub 上没有 afcli 时提示「这台 Hub 没配」，而不是「下载失败」', async () => {
    stubBlobFetch((path) =>
      path === '/api/agent-binary'
        ? new Response(JSON.stringify({ error: 'agent_binary_unavailable' }), {
            status: 404,
            headers: { 'Content-Type': 'application/json' },
          })
        : new Response(JSON.stringify([]), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)
    const btn = [...w.element.querySelectorAll('button')].find((b) =>
      (b.textContent ?? '').includes('下载 afcli'),
    )
    btn!.click()
    await flush(w)

    const text = w.element.textContent ?? ''
    // 裸机跑 afhub（不经容器）时磁盘上本来就没有 afcli —— 这是**正常部署
    // 形态**，不是故障。提示要指向「去配 AFCLI_BINARY_PATH」，而不是让人
    // 以为 Hub 坏了跑去翻日志。
    expect(text).toContain('这台 Hub 没有可下发的 afcli')
    expect(text).toContain('AFCLI_BINARY_PATH')
  })

  it('其它错误走通用失败文案（不能被当成「没配」）', async () => {
    stubBlobFetch((path) =>
      path === '/api/agent-binary'
        ? new Response(JSON.stringify({ error: 'boom' }), {
            status: 500,
            headers: { 'Content-Type': 'application/json' },
          })
        : new Response(JSON.stringify([]), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)
    const btn = [...w.element.querySelectorAll('button')].find((b) =>
      (b.textContent ?? '').includes('下载 afcli'),
    )
    btn!.click()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).not.toContain('这台 Hub 没有可下发的 afcli')
    expect(text).toContain('afcli')
  })
})

/* ===========================================================================
   缺陷池 🟡「afcli 下载无法选择其他架构」（v0.8.0 子任务 11）
   ---------------------------------------------------------------------------
   用户的诉求是「我想下载其他架构的怎么办」。后端 `?os=&arch=` 早就是闭集
   （`routes/agent_binary.rs:154`），缺的只是前端的入口。这一组守四件事：

   ① 真的把**用户选的**平台发出去，而不是浏览器推断的那一个；
   ② 没选平台时下载按钮必须是 `disabled`——不带参数请求会让后端回落到
      Hub 自己的平台（Linux），那正是「Windows 用户拿到 Linux 文件」那条 bug；
   ③ 参数值全部来自闭集，前端不可能造出一个后端要 400 的组合；
   ④ 下载失败是**行内红字**「下载失败，请重试」，不弹 alert。

   **判红方式**：把 `downloadCli` 改回硬编码 `{ os: 'linux', arch: 'amd64' }`
   （或改回 `detectDownloadPlatform(navigator.userAgent, …)`），第一条红——
   那时选 `windows` + `arm64` 发出的仍是 `os=linux&arch=amd64`。
   =========================================================================== */
describe('DevicesPage：afcli 下载的平台选择（缺陷池 🟡）', () => {
  /** 复用本文件既有的姿势：`route` 是模块级 ref，上一条用例会留值。 */
  beforeEach(() => {
    window.history.replaceState(null, '', '/devices')
    route.value = matchRoute('/devices')
  })

  /**
   * 把浏览器伪装成某个平台。
   *
   * **必须显式设**：`navigator` 在不同用例里会被 `vi.stubGlobal` 换成不同值，
   * 而 `afterEach` 的 `unstubAllGlobals` 会把它还原成 happy-dom 的默认（一个
   * 认不出的空 UA）。默认值会决定下拉框的初始选中项，不显式设就等于让「默认
   * 猜哪个平台」这条断言依赖上一个用例——「单独跑绿、全量跑红」的经典成因。
   */
  function stubBrowser(userAgent: string, platform: string): void {
    vi.stubGlobal('navigator', { userAgent, platform })
  }

  /** 装好 blob 下载所需的三处打桩（理由同上面那个 `stubBlobFetch`）。 */
  function stubBlob(onRequest: (url: URL) => Response): { seen: URL[]; clicked: string[] } {
    const clicked: string[] = []
    vi.spyOn(URL, 'createObjectURL').mockImplementation(() => 'blob:fake-platform')
    vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => undefined)
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (
      this: HTMLAnchorElement,
    ) {
      clicked.push(this.download)
    })
    const seen: URL[] = []
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
        const url = new URL(raw, 'http://hub.local')
        if (url.pathname === '/api/agent-binary') {
          seen.push(url)
          return onRequest(url)
        }
        return new Response(JSON.stringify([]), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      }),
    )
    return { seen, clicked }
  }

  /** 下载按钮**按可访问名取**：页面上还有「刷新」「设备」「Agent 目录」等按钮，
   *  按顺序取第一个会取到它们，症状是「点了没反应」。 */
  function downloadButton(w: ReturnType<typeof mountIn>): HTMLButtonElement {
    const btn = [...w.element.querySelectorAll('button')].find((b) =>
      (b.textContent ?? '').includes('下载 afcli'),
    )
    expect(btn, '空态里必须有「下载 afcli」按钮').toBeTruthy()
    return btn as HTMLButtonElement
  }

  function platformSelect(w: ReturnType<typeof mountIn>): DOMWrapper<HTMLSelectElement> {
    const sel = w.find<HTMLSelectElement>('#af-cli-platform')
    expect(sel.exists(), '下载入口必须有一个平台下拉框').toBe(true)
    return sel
  }

  it('选 windows + arm64 → 请求 URL 里是 os=windows&arch=arm64', async () => {
    // 先把浏览器伪装成 **Linux**：这样「发出去的 windows/arm64」只可能来自
    // 用户的选择，不可能是 UA 推断的残留。用一个和期望值冲突的 UA 才能让
    // 「仍然按浏览器推断」的实现变红——两边相同的话那条断言恒真。
    stubBrowser('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64')
    const { seen } = stubBlob(
      () => new Response(new Blob(['\x7fELF']), { status: 200 }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)

    await platformSelect(w).setValue('windows/arm64')
    await w.vm.$nextTick()
    downloadButton(w).click()
    await flush(w)

    expect(seen, '必须真的请求了 /api/agent-binary').toHaveLength(1)
    const q = seen[0].searchParams
    expect(q.get('os')).toBe('windows')
    expect(q.get('arch')).toBe('arm64')
    // 顺序也钉住：规格的判据原文就是 `os=windows&arch=arm64`。
    expect(seen[0].search.slice(1)).toBe('os=windows&arch=arm64')
  })

  it('macOS 的两个选项都能被选中并发出去（Intel / Apple Silicon）', async () => {
    stubBrowser('Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5)', 'MacIntel')
    const { seen } = stubBlob(() => new Response(new Blob(['x']), { status: 200 }))
    const w = mountIn(DevicesPage)
    await flush(w)

    await platformSelect(w).setValue('mac/amd64')
    await w.vm.$nextTick()
    downloadButton(w).click()
    await flush(w)

    await platformSelect(w).setValue('mac/arm64')
    await w.vm.$nextTick()
    downloadButton(w).click()
    await flush(w)

    expect(seen.map((u) => `${u.searchParams.get('os')}/${u.searchParams.get('arch')}`)).toEqual([
      'mac/amd64',
      'mac/arm64',
    ])
  })

  it('猜不到平台 → 下拉框空着、下载按钮 disabled，且提示「请选择平台」', async () => {
    // 空 UA = 认不出操作系统。这时**不许**猜成 Linux：猜错的话 Windows 用户
    // 会以为自己选对了，拿到一个跑不起来的 Linux 文件，而界面上没有任何提示。
    stubBrowser('', '')
    const { seen } = stubBlob(() => new Response(new Blob(['x']), { status: 200 }))
    const w = mountIn(DevicesPage)
    await flush(w)

    expect(platformSelect(w).element.value).toBe('')
    const btn = downloadButton(w)
    expect(btn.disabled, '没选平台时必须禁用下载按钮').toBe(true)

    // 禁用之外，还要有一句看得见的提示——只说「禁用」会让用户找不到原因。
    expect(w.element.textContent ?? '').toContain('请选择平台')

    // **断言的另一半**：按钮禁用了→点了不能发请求。只断言 `disabled` 时，
    // 一个「按钮禁用但 `@click` 照样发请求」的实现仍然全绿，而它真正的问题
    // 是「没选平台就按后端自己的平台回落」——那正是这条缺陷要消灭的行为。
    btn.click()
    await flush(w)
    expect(seen, '没选平台时一个 /api/agent-binary 请求都不该发').toHaveLength(0)
  })

  it('能认出来的浏览器有默认值（猜一个，用户仍能改）', async () => {
    stubBrowser('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Win32')
    stubBlob(() => new Response(new Blob(['x']), { status: 200 }))
    const w = mountIn(DevicesPage)
    await flush(w)

    expect(platformSelect(w).element.value).toBe('windows/amd64')
    expect(downloadButton(w).disabled).toBe(false)
  })

  it('下拉框的选项恰好是后端闭集投影出的六种组合', async () => {
    stubBrowser('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64')
    stubBlob(() => new Response(new Blob(['x']), { status: 200 }))
    const w = mountIn(DevicesPage)
    await flush(w)

    const labels = [...platformSelect(w).element.querySelectorAll('option')]
      .map((o) => o.textContent?.trim() ?? '')
      // 第一项是「请选择平台」（空值占位），不是平台。
      .filter((t) => t !== '请选择平台')
    expect(labels).toEqual([
      'Windows x64',
      'Windows ARM64',
      'macOS Intel',
      'macOS Apple Silicon',
      'Linux x64',
      'Linux ARM64',
    ])
  })

  it('构造出的 URL 参数值全部来自闭集（六种组合逐个走一遍）', async () => {
    stubBrowser('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64')
    const { seen } = stubBlob(() => new Response(new Blob(['x']), { status: 200 }))
    const w = mountIn(DevicesPage)
    await flush(w)

    const combos = [
      ['windows/amd64', 'windows', 'amd64'],
      ['windows/arm64', 'windows', 'arm64'],
      ['mac/amd64', 'mac', 'amd64'],
      ['mac/arm64', 'mac', 'arm64'],
      ['linux/amd64', 'linux', 'amd64'],
      ['linux/arm64', 'linux', 'arm64'],
    ] as const
    for (const [key] of combos) {
      await platformSelect(w).setValue(key)
      await w.vm.$nextTick()
      downloadButton(w).click()
      await flush(w)
    }

    expect(seen).toHaveLength(combos.length)
    // 后端闭集（`agent_binary.rs:154` 的 `normalize_query_platform`）：
    // os ∈ {windows, mac, macos, darwin, linux}，arch ∈ {amd64, x86_64, arm64, aarch64}。
    // 前端只该用**短名**那一支，且每一个值都必须在这个集合里——
    // 一个 `window`（少了 s）或 `x64` 会让后端回 400，而用户只看到「下载失败」。
    const OS = new Set(['windows', 'mac', 'linux'])
    const ARCH = new Set(['amd64', 'arm64'])
    for (const [i, u] of seen.entries()) {
      const [wantKey, wantOs, wantArch] = combos[i]
      expect(wantKey).toBe(`${wantOs}/${wantArch}`)
      expect(OS.has(u.searchParams.get('os') ?? ''), `os 越界：${u.search}`).toBe(true)
      expect(ARCH.has(u.searchParams.get('arch') ?? ''), `arch 越界：${u.search}`).toBe(true)
      expect(u.searchParams.get('os')).toBe(wantOs)
      expect(u.searchParams.get('arch')).toBe(wantArch)
    }
  })

  it('下载失败（网络错误）→ 行内红字「下载失败，请重试」，不弹 alert', async () => {
    stubBrowser('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64')
    const alertSpy = vi.spyOn(window, 'alert').mockImplementation(() => undefined)
    vi.spyOn(URL, 'createObjectURL').mockImplementation(() => 'blob:x')
    vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => undefined)
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = new URL(String(input), 'http://hub.local')
        if (url.pathname === '/api/agent-binary') throw new Error('network down')
        return new Response(JSON.stringify([]), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)

    downloadButton(w).click()
    await flush(w)

    const err = w.element.querySelector('.af-field__error')
    expect(err?.textContent?.trim()).toBe('下载失败，请重试')
    // 不弹 alert：它会阻断整页，而且只能显示一句没法指到控件的话。
    expect(alertSpy).not.toHaveBeenCalled()
  })

  it('下载失败（404）同样是「下载失败，请重试」', async () => {
    stubBrowser('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64')
    stubBlob(
      () =>
        new Response(JSON.stringify({ error: 'not_found' }), {
          status: 404,
          headers: { 'Content-Type': 'application/json' },
        }),
    )
    const w = mountIn(DevicesPage)
    await flush(w)

    downloadButton(w).click()
    await flush(w)

    expect(w.element.querySelector('.af-field__error')?.textContent?.trim()).toBe(
      '下载失败，请重试',
    )
  })
})

/* ===========================================================================
   v0.8.1 子任务 7：页面骨架与原型逐层对齐（`prototype/devices.html`）
   ---------------------------------------------------------------------------
   只量**结构**，不量外观。守的是「根是不是原型那两层的容器」：上一版根是
   自造的 `.af-page`，那一族类在子任务 2 已随类名删除，于是样式表里没有任何
   规则命中它——模板「看起来是页面」，实际是一张没有滚动语义的裸页面。
   外观（内边距、摘要条、表格）在 `.af-devpage__body` 的 scoped 规则里。

   为什么用 `querySelector` 而不是 `w.element.classList`：`mountIn` 走
   `attachTo`，`w.element` 是**挂载点**那个 host `div`（`class=""`），页面自己
   的根在它里面。拿挂载点问类名永远是 false，这条会以「骨架没改」误红。
   =========================================================================== */

describe('DevicesPage（v0.8.1）：骨架与原型 devices.html 对齐', () => {
  it('page_root_uses_af_body：整页一个 .af-body，页头是共享 PageHead 的 .af-head', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_1', name: 'mbp.local' })] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    // 原型 `devices.html:15-23`：`header.af-head` 与 `.af-body` 是两个兄弟。
    // 判据取「整页只有一个」——将来谁在 `.af-body` 里再包一层它，这一条会红在
    // 「多个」而不是静默取第一个通过。
    const bodies = Array.from(w.element.querySelectorAll('.af-body'))
    expect(bodies, '整页只该有一个 .af-body（页面内容区）').toHaveLength(1)
    // 页头若退回自造的类名，`.af-head` 的几何与内边距一条都不生效。
    expect(w.element.querySelectorAll('.af-head'), '页头必须是原型的 .af-head').toHaveLength(1)
    expect(bodies[0]?.classList.contains('af-head'), '页头不能塞在 .af-body 里面').toBe(false)
  })
})
