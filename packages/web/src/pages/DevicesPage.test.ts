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
