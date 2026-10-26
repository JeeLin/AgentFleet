import { describe, expect, it, vi } from 'vitest'

import DevicesPage from '@/pages/DevicesPage.vue'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

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
    const btn = w.element.querySelector<HTMLButtonElement>('tbody button')
    expect(btn?.disabled).toBe(true)
    expect(btn?.getAttribute('title')).toContain('离线')
  })

  it('「用它开会话」跳到新建会话页并把设备带在 query 上', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_1' })] } })
    const w = mountIn(DevicesPage)
    await flush(w)

    await w.element.querySelector<HTMLButtonElement>('tbody button')?.click()
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
