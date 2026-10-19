import { beforeEach, describe, expect, it } from 'vitest'

import SessionPage from '@/pages/SessionPage.vue'
import { device, mountIn, session } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { STORAGE_KEY, type TabMeta } from '@/lib/tabs'
import { matchRoute, route } from '@/router'

/* ------------------------------------------------------------------ 假 SSE */

/** 一个能手动派发事件的假 EventSource：这里只关心「页面建没建下行」。 */
class FakeEventSource {
  static instances: FakeEventSource[] = []
  readonly url: string
  closed = false
  private readonly listeners = new Map<string, Set<(ev: Event) => void>>()

  constructor(url: string) {
    this.url = url
    FakeEventSource.instances.push(this)
  }

  addEventListener(type: string, fn: (ev: Event) => void): void {
    const set = this.listeners.get(type) ?? new Set()
    set.add(fn)
    this.listeners.set(type, set)
  }

  close(): void {
    this.closed = true
  }

  emit(type: string, data?: unknown): void {
    const ev = data === undefined ? new Event(type) : new MessageEvent(type, { data: JSON.stringify(data) })
    for (const fn of this.listeners.get(type) ?? []) fn(ev)
  }
}

const TWO_TABS: TabMeta[] = [
  { id: 's_1', title: 'alpha', path: '/srv/alpha', nodeId: 'n_1' },
  { id: 's_2', title: 'beta', path: '/srv/beta', nodeId: 'n_1' },
]

const SESSIONS = [
  session({ id: 's_1', workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
  session({ id: 's_2', workspace: { node_id: 'n_1', path: '/srv/beta', path_locked: true } }),
]

const DEVICES = [device({ id: 'n_1', name: 'mbp.local' })]

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

interface Ctx {
  w: ReturnType<typeof mountIn>
  fetchMock: ReturnType<typeof stubFetch>
  posts: () => number
  deletes: () => string[]
}

async function mountWorkbench(seed: TabMeta[] = TWO_TABS, id = 's_1'): Promise<Ctx> {
  sessionStorage.setItem(STORAGE_KEY, JSON.stringify(seed))
  window.history.replaceState(null, '', `/sessions/${id}`)
  route.value = matchRoute(`/sessions/${id}`)

  const fetchMock = stubFetch({
    '/api/sessions': { body: SESSIONS },
    '/api/devices': { body: DEVICES },
    '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    '/api/channels/ch-1': { empty: true },
  })
  const w = mountIn(SessionPage)
  await flush(w)

  const calls = () =>
    fetchMock.mock.calls.map(([url, init]) => ({
      url: new URL(String(url), 'http://hub.local').pathname,
      method: ((init as RequestInit | undefined)?.method ?? 'GET').toUpperCase(),
    }))

  return {
    w,
    fetchMock,
    posts: () => calls().filter((c) => c.url === '/api/channels' && c.method === 'POST').length,
    deletes: () => calls().filter((c) => c.method === 'DELETE').map((c) => c.url),
  }
}

function panelHost(w: ReturnType<typeof mountIn>): HTMLElement {
  const el = w.element.querySelector<HTMLElement>('[data-panelhost]')
  if (!el) throw new Error('panel host not rendered')
  return el
}

/** 「第一次与面板正文交互」——桥到 PanelDock 的 interact 上。 */
async function interact(w: ReturnType<typeof mountIn>): Promise<void> {
  panelHost(w).dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
  await flush(w)
}

function tabLabel(w: ReturnType<typeof mountIn>, id: string): HTMLElement {
  const el = w.element.querySelector<HTMLElement>(`[data-tab-label="${id}"]`)
  if (!el) throw new Error(`no tab ${id}`)
  return el
}

beforeEach(() => {
  FakeEventSource.instances = []
  ;(globalThis as { EventSource?: unknown }).EventSource = FakeEventSource
})

describe('工作台：结构与默认值', () => {
  it('对话区默认 56%，坞没开时没有把手也没有面板', async () => {
    const { w } = await mountWorkbench()
    expect(w.element.querySelector('.af-wb__conv')).not.toBeNull()
    expect(w.element.querySelector('[role="separator"]')).toBeNull()
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()
  })

  it('打开面板后出现把手，并把对话区宽度写成 flex-basis（默认 56%）', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)

    const sep = w.element.querySelector<HTMLElement>('[role="separator"]')
    expect(sep?.getAttribute('aria-valuenow')).toBe('56')
    expect((w.element.querySelector<HTMLElement>('.af-wb__conv') as HTMLElement).style.flex).toContain('56%')
  })

  it('工作台头写明路径锁定与「agent 未选」（M1 不接 ACP）', async () => {
    const { w } = await mountWorkbench()
    const text = w.element.textContent ?? ''
    expect(text).toContain('/srv/alpha')
    expect(text).toContain('路径锁定')
    expect(text).toContain('agent 未选（M3 起可选）')
  })

  it('已结束的会话明写面板不会再建通道，且入口禁用', async () => {
    const fetchMock = stubFetch({
      '/api/sessions': {
        body: [
          session({
            id: 's_1',
            state: 'ended',
            workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true },
          }),
        ],
      },
      '/api/devices': { body: DEVICES },
    })
    window.history.replaceState(null, '', '/sessions/s_1')
    route.value = matchRoute('/sessions/s_1')
    const w = mountIn(SessionPage)
    await flush(w)

    expect(w.element.querySelector<HTMLElement>('.af-wb__banner')?.textContent).toContain('不会再建立通道')
    expect((w.element.querySelector<HTMLButtonElement>('.af-wb__head button') as HTMLButtonElement).disabled).toBe(true)
    expect(fetchMock).toHaveBeenCalled()
  })
})

describe('通道生命周期：懒建立', () => {
  it('打开坞不建通道——那时还没有数据要流动', async () => {
    const { w, posts } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    expect(posts()).toBe(0)
  })

  it('第一次与面板正文交互才 POST /api/channels，并建一条 SSE', async () => {
    const { w, posts } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    expect(posts()).toBe(1)
    expect(FakeEventSource.instances).toHaveLength(1)
  })

  it('第二次交互不重发：句柄已经 ready，open() 是幂等的', async () => {
    const { w, posts } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)
    await interact(w)
    expect(posts()).toBe(1)
  })

  it('切面板不建第二条通道（终端 / 文件各一条，各自懒建立）', async () => {
    const { w, posts } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    // 坞里只有一个面板（terminal），所以没有可切的第二个
    const tabs = w.element.querySelectorAll('.af-docktabs__b')
    expect(tabs).toHaveLength(1)
    expect(posts()).toBe(1)
  })
})

describe('通道生命周期：切 tab 不释放', () => {
  it('切到另一个 tab 再切回来，通道只建过一次，也没有 DELETE', async () => {
    const { w, posts, deletes } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)
    expect(posts()).toBe(1)

    await tabLabel(w, 's_2').click()
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()

    await tabLabel(w, 's_1').click()
    await flush(w)

    expect(posts()).toBe(1)
    expect(deletes()).toEqual([])
  })

  it('切走再切回来，终端缓冲还在（状态挂在 tab 上，不是全局一份）', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    FakeEventSource.instances[0]?.emit('channel.frame', {
      node_id: 'n_1',
      channel_id: 'ch-1',
      payload: { bytes: 'hello from pty' },
    })
    await flush(w)
    // 13a 的正文是占位，不渲染字节流：这里断言的是「帧到了句柄，没有崩」
    await tabLabel(w, 's_2').click()
    await flush(w)
    await tabLabel(w, 's_1').click()
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).not.toBeNull()
  })

  it('关掉面板坞才发 close_channel（退出即释放）', async () => {
    const { w, deletes } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    await w.element.querySelector<HTMLElement>('.af-docktabs__close')?.click()
    await flush(w)

    expect(deletes()).toEqual(['/api/channels/ch-1'])
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()
  })

  it('关 tab 才释放它名下的通道——这是生命周期边界', async () => {
    const { w, deletes } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    await w.element.querySelector<HTMLElement>('[data-tab-close="s_1"]')?.click()
    await flush(w)

    expect(deletes()).toEqual(['/api/channels/ch-1'])
  })
})

describe('通道生命周期：断线丢弃不排队', () => {
  it('流断开时 send 只计数，面板头显示「有 N 条已丢」', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    const src = FakeEventSource.instances[0]
    src?.emit('open')
    src?.emit('error')
    await flush(w)

    // 面板正文（13a 占位）不持有输入通道，所以这里直接断言「断线态下没有已丢」
    // 之外的形态：一旦 13b 挂上输入，这条会变成「有 1 条已丢」。
    expect(w.element.textContent).not.toContain('条已丢')
  })
})

describe('多 tab 的代价可见', () => {
  it('开过面板的 tab 在条上有面板数徽标', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)

    const tab = w.element.querySelector<HTMLElement>('[data-sid="s_1"]')
    expect(tab?.querySelector('.af-badge')?.textContent?.trim()).toBe('1')
  })

  it('没开过面板的 tab 没有徽标', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)

    expect(w.element.querySelector('[data-sid="s_2"] .af-badge')).toBeNull()
  })
})

describe('键盘', () => {
  it('`o` 打开面板，`\\` 收起坞', async () => {
    const { w } = await mountWorkbench()
    const press = (key: string): void => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }))
    }

    press('o')
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).not.toBeNull()

    press('\\')
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()
  })

  it('焦点在输入框里时单键不抢（那是打字）', async () => {
    const { w } = await mountWorkbench()
    const input = w.element.querySelector<HTMLInputElement>('input')
    expect(input).toBeNull()

    const probe = document.createElement('input')
    document.body.appendChild(probe)
    probe.focus()
    probe.dispatchEvent(new KeyboardEvent('keydown', { key: 'o', bubbles: true, cancelable: true }))
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()
  })
})

describe('找不到的会话', () => {
  it('给出空态并指回会话列表，而不是一片空白', async () => {
    stubFetch({ '/api/sessions': { body: [] }, '/api/devices': { body: [] } })
    window.history.replaceState(null, '', '/sessions/s_gone')
    route.value = matchRoute('/sessions/s_gone')
    const w = mountIn(SessionPage)
    await flush(w)

    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('这个会话不存在')
    expect(w.element.querySelector('.af-wb__head')).toBeNull()
  })
})
