import { beforeEach, describe, expect, it, vi } from 'vitest'

import DevicesPage from '@/domains/device/DevicesPage.vue'
import { detectDownloadPlatform, downloadAgentBinary } from '@/domains/device/api'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { matchRoute, route } from '@/router'

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

  it('「用它开会话」跳到新建会话页并把设备带在 query 上', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_1' })] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    await w.element.querySelector<HTMLButtonElement>('tbody [data-act="create"]')?.click()
    expect(location.pathname).toBe('/sessions/new')
    expect(location.search).toBe('?device=n_1')
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
