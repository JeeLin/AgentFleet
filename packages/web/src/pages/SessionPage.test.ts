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

/** 一帧。happy-dom 的 rAF 真的排队，xterm 的渲染帧能被它带出来。 */
function nextFrame(): Promise<void> {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}

/** 一个宏任务。xterm 的 `WriteBuffer` 解析排在这上面。 */
function nextTask(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0))
}

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
    // 宏任务与帧也要泵：xterm 的字节解析排在宏任务上、渲染排在帧上，
    // 只泵微任务的话「帧到了但画面上还没有」——而那正是要抓的东西。
    await nextTask()
    await nextFrame()
  }
}

interface Ctx {
  w: ReturnType<typeof mountIn>
  fetchMock: ReturnType<typeof stubFetch>
  posts: () => number
  deletes: () => string[]
  /**
   * 页面建的那条下行。SSE 是**页面共用一条**（hub.connect），所以这里是
   * `instances[0]` 而不是「每个面板一条」——按「一个 tab 一条」去取会拿到
   * undefined，帧就发不出去，而症状看起来像「页面没解帧」。
   */
  src: () => FakeEventSource | undefined
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
    '/api/channels/ch-1/send': { empty: true },
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
    src: () => FakeEventSource.instances[0],
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

/**
 * 往终端里敲一个字。
 *
 * 走 xterm 自己的输入事件（`input` + `data`）而不是直接调组件方法：xterm
 * 就是这么把按键变成 `onData` 的，中间任何一层被改掉（换了事件源、换了
 * 编码）这条用例都会跟着红。
 */
function typeIntoTerminal(w: ReturnType<typeof mountIn>, data: string): void {
  const ta = w.element.querySelector<HTMLTextAreaElement>('.xterm textarea')
  if (!ta) throw new Error('terminal textarea is missing')
  ta.dispatchEvent(new InputEvent('input', { data, inputType: 'insertText', bubbles: true }))
}

/** 终端画面上的文字（`.xterm-rows` 拼起来）。 */
function terminalText(w: ReturnType<typeof mountIn>): string {
  return Array.from(w.element.querySelectorAll<HTMLElement>('.xterm-rows > div'))
    .map((el) => el.textContent ?? '')
    .join('\n')
}

function tabLabel(w: ReturnType<typeof mountIn>, id: string): HTMLElement {
  const el = w.element.querySelector<HTMLElement>(`[data-tab-label="${id}"]`)
  if (!el) throw new Error(`no tab ${id}`)
  return el
}

/**
 * 把文件面板拉进坞并显示它。**到此为止不碰面板正文**——通道因此还没建
 * （§5.5.1 的懒建立：点 / 聚焦坞标签都**不**算交互，只有碰正文才算）。
 *
 * 走 `]`（cyclePanel）而不是「点坞标签」：坞里只有一个面板时没有第二个
 * 标签可点，而 `[` / `]` 的语义正是「把另一个拉进来」（§5.4：坞能同时放
 * 终端与文件）。
 */
async function openFiles(w: ReturnType<typeof mountIn>): Promise<void> {
  await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
  await flush(w)
  document.dispatchEvent(new KeyboardEvent('keydown', { key: ']', bubbles: true, cancelable: true }))
  await flush(w)
  const tab = w.element.querySelector<HTMLElement>('[data-panel="files"]')
  if (!tab) throw new Error('files panel did not mount')
  await tab.click()
  await flush(w)
}

/** 碰一下面板正文：这是建通道的判据。返回之后通道应该在 `ready`。 */
async function touchFiles(w: ReturnType<typeof mountIn>): Promise<void> {
  const tree = w.element.querySelector<HTMLElement>('[data-role="tree"]')
  if (!tree) throw new Error('files panel body not rendered')
  tree.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
  await flush(w)
}

/**
 * 从假 SSE 上发一帧，并等到它被解完。
 *
 * `hub.connect()` 是**懒**的：第一次 `open()` 时才建 SSE。所以这条 helper
 * 必须在交互之后用，否则 `src()` 还是 undefined。
 */
async function frame(
  w: ReturnType<typeof mountIn>,
  src: FakeEventSource | undefined,
  payload: unknown,
): Promise<void> {
  if (!src) throw new Error('no EventSource: 还没有任何通道试图建立')
  src.emit('channel.frame', { node_id: 'n_1', channel_id: 'ch-1', payload })
  await flush(w)
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
    expect(posts()).toBe(1)

    // 把文件面板拉进坞：**拉进来本身不建通道**（懒建立）。
    await openFiles(w)
    expect(posts()).toBe(1)

    // 碰文件面板正文才建它那一条——终端与文件各一条。
    await touchFiles(w)
    expect(posts()).toBe(2)

    // 在两个面板之间来回切：不该再多建通道（切面板 ≠ 建通道）。
    await w.element.querySelector<HTMLElement>('[data-panel="terminal"]')?.click()
    await flush(w)
    await w.element.querySelector<HTMLElement>('[data-panel="files"]')?.click()
    await flush(w)
    expect(posts()).toBe(2)
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

    await frame(w, FakeEventSource.instances[0], { bytes: 'hello from pty' })
    expect(terminalText(w)).toContain('hello from pty')

    await tabLabel(w, 's_2').click()
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()

    await tabLabel(w, 's_1').click()
    await flush(w)

    // 缓冲挂在 tab 上：切回来时 xterm 重新挂载并回放同一份字节，
    // 所以「画面上还有那行输出」才是这条用例的承诺。
    expect(terminalText(w)).toContain('hello from pty')
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
    const { w, fetchMock } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)
    await interact(w)

    const src = FakeEventSource.instances[0]
    src?.emit('open')
    src?.emit('error')
    await flush(w)

    // 断线之后敲一个字：这一下**只计数**，不排队、也不发出去。
    typeIntoTerminal(w, 'x')
    await flush(w)

    expect(w.element.textContent).toContain('有 1 条已丢')
    const sent = fetchMock.mock.calls.filter(([url]) =>
      String(url).includes('/api/channels/ch-1/send'),
    )
    expect(sent).toHaveLength(0)
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

describe('面板正文是 13b / 13c 的真组件，不是 13a 的占位', () => {
  it('终端位挂着真 xterm 面板（占位那句「13b 挂入」不再出现）', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)

    expect(w.element.querySelector('[data-terminal]')).not.toBeNull()
    expect(w.element.textContent).not.toContain('终端内容由 13b 挂入')
  })

  it('`]` 把文件面板拉进坞：真树挂上，占位那句不再出现', async () => {
    const { w } = await mountWorkbench()
    await openFiles(w)
    expect(w.element.querySelector('[data-role="tree"]')).not.toBeNull()
    expect(w.element.textContent).not.toContain('文件树与预览由 13c 挂入')
  })

  it('碰正文之前树说「通道还没建立」——不是「这一层是空的」', async () => {
    const { w } = await mountWorkbench()
    await openFiles(w)
    // 传错 `channelReady`（恒 true）时，组件会把「还没问过设备」说成
    // 「目录是空的」——那正是 §3.12 要分开的两种空。
    expect(w.element.querySelector('[data-tree-empty]')?.textContent).toContain('通道还没建立')
  })

  it('`list` 回帧进 listings → 树上有行（页面负责解帧，面板只吃 props）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), {
      op: 'list',
      path: '',
      entries: [{ name: 'src', kind: 'dir', size: 0, mtime_ms: null }],
      total: 1,
      truncated: false,
    })

    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('[data-row]'))
    expect(rows).toHaveLength(1)
    expect(rows[0]?.dataset.path).toBe('src')
    expect(rows[0]?.getAttribute('aria-expanded')).toBe('false')
  })

  it('通道建好之后空树说的是「这一层是空的」（不再是「没问过设备」）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), { op: 'list', path: '', entries: [], total: 0, truncated: false })

    expect(w.element.querySelector('[data-tree-empty]')?.textContent).toContain('这一层是空的')
  })

  it('帧里没给 size / total 时页面画「—」，不编一个 0（0 与「不知道」是两件事）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), {
      op: 'list',
      path: '',
      entries: [{ name: 'src', kind: 'dir', size: 0, mtime_ms: null }],
      truncated: true,
    })
    expect(w.element.textContent).toContain('共 — 项')

    await frame(w, src(), { op: 'read', path: 'README.md', kind: 'text', truncated: false, text: 'hi' })
    expect(w.element.querySelector('.af-files__pvhead .af-badge')?.textContent?.trim()).toBe('—')
  })

  it('`read` 回帧进 preview → 预览区给出结论（§3.14）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), {
      op: 'read',
      path: 'README.md',
      size: 12,
      kind: 'text',
      truncated: false,
      text: 'hello\nworld',
    })

    expect(w.element.querySelector('[data-kind-text]')).not.toBeNull()
    expect(w.element.querySelector('.af-files__pvname')?.textContent).toContain('README.md')
  })

  it('编辑文件和设备上的内容不一致 → tab 上出现脏标记（§5.5.2）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), {
      op: 'read',
      path: 'a.txt',
      size: 2,
      kind: 'text',
      truncated: false,
      text: 'ab',
    })

    const tab = () => w.element.querySelector<HTMLElement>('[data-sid="s_1"]')
    expect(tab()?.querySelector('[data-tab-dirty]')).toBeNull()

    const area = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')
    if (!area) throw new Error('no editor')
    area.value = 'abc'
    area.dispatchEvent(new Event('input'))
    await flush(w)

    expect(tab()?.querySelector('[data-tab-dirty]')?.textContent).toContain('未保存')
  })

  it('没在保存时的 error 只进 filesError（③），不动 writeError（④）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), { op: 'error', path: '../x', reason: '越界', code: 'path_outside_root' })

    expect(w.element.querySelector('[data-nav-error]')).not.toBeNull()
    expect(w.element.querySelector('[data-write-error]')).toBeNull()
  })

  it('`write` 回帧只有失败那一次进④：ok 为 true 或缺失都不过错（清错归发送方）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)

    // ok 缺失也算成功：`files.rs` 的写回帧只在出错时带 `ok:false`
    await frame(w, src(), { op: 'write', path: 'a.txt' })
    expect(w.element.querySelector('[data-write-error]')).toBeNull()
    await frame(w, src(), { op: 'write', path: 'a.txt', ok: true })
    expect(w.element.querySelector('[data-write-error]')).toBeNull()

    await frame(w, src(), { op: 'write', path: 'a.txt', ok: false, reason: '磁盘满了' })
    expect(w.element.querySelector('[data-write-error]')?.textContent).toContain('磁盘满了')
  })

  it('不认识的 op 不动状态（node 侧加 op 时旧页面不该把它解成已存在的某一档）', async () => {
    const { w, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), {
      op: 'list',
      path: '',
      entries: [{ name: 'src', kind: 'dir', size: 0, mtime_ms: null }],
      total: 1,
      truncated: false,
    })
    const before = Array.from(w.element.querySelectorAll('[data-row]')).map((r) =>
      r.getAttribute('data-path'),
    )

    for (const op of ['stat', 'LIST', '', 'delete']) {
      await frame(w, src(), { op, path: 'src' })
    }
    expect(Array.from(w.element.querySelectorAll('[data-row]')).map((r) => r.getAttribute('data-path')))
      .toEqual(before)
    expect(w.element.querySelector('[data-nav-error]')).toBeNull()
    expect(w.element.querySelector('[data-write-error]')).toBeNull()
  })

  it('取数按 files 的契约发 application/json，op/字段与 node 侧 files.rs 逐字一致', async () => {
    const { w, fetchMock, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)

    const sends = () =>
      fetchMock.mock.calls
        .map(([url, init]) => ({ path: new URL(String(url), 'http://hub.local').pathname, init }))
        .filter((c) => c.path === '/api/channels/ch-1/send')
        .map((c) => ({
          type: new Headers(c.init?.headers).get('Content-Type'),
          body: JSON.parse(String(c.init?.body)),
        }))

    // 第一次交互问的是当前这一层（根）
    expect(sends()[0]).toEqual({ type: 'application/json', body: { op: 'list', path: '' } })

    // 设备回了根这一层之后，进入 src → 再发一次 list（路径相对工作区根）
    await frame(w, src(), {
      op: 'list',
      path: '',
      entries: [{ name: 'src', kind: 'dir', size: 0, mtime_ms: null }],
      total: 1,
      truncated: false,
    })
    const row = w.element.querySelector<HTMLElement>('[data-row][data-path="src"]')
    if (!row) throw new Error('no src row')
    row.focus()
    row.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))
    await flush(w)

    expect(sends().at(-1)).toEqual({ type: 'application/json', body: { op: 'list', path: 'src' } })
  })

  it('取数被 Hub 拒（400/404）时页面说话，而不是一棵永远不动的空树', async () => {
    const { w, fetchMock, src } = await mountWorkbench()
    await openFiles(w)
    await touchFiles(w)
    await frame(w, src(), {
      op: 'list',
      path: '',
      entries: [{ name: 'src', kind: 'dir', size: 0, mtime_ms: null }],
      total: 1,
      truncated: false,
    })

    const before = fetchMock.mock.calls.length
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const path = new URL(String(input), 'http://hub.local').pathname
      const body =
        path === '/api/channels/ch-1/send'
          ? { error: 'invalid_request_body' }
          : { error: 'unexpected_request' }
      return new Response(JSON.stringify(body), {
        status: path === '/api/channels/ch-1/send' ? 400 : 500,
        headers: { 'Content-Type': 'application/json' },
      })
    })

    const row = w.element.querySelector<HTMLElement>('[data-row][data-path="src"]')
    if (!row) throw new Error('no src row')
    row.focus()
    row.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))
    await flush(w)

    expect(fetchMock.mock.calls.length).toBeGreaterThan(before)
    expect(w.element.querySelector('[data-nav-error]')?.textContent).toContain('invalid_request_body')
  })

  it('已结束的会话：面板只读的文本在场且「打开面板」入口禁用', async () => {
    const fetchMock = stubFetch({
      '/api/sessions': {
        body: [
          session({ id: 's_1', state: 'ended', workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
        ],
      },
      '/api/devices': { body: DEVICES },
    })
    window.history.replaceState(null, '', '/sessions/s_1')
    route.value = matchRoute('/sessions/s_1')
    void fetchMock
    const w = mountIn(SessionPage)
    await flush(w)

    expect(w.element.querySelector('[data-conv-input]')).not.toBeNull()
    expect((w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]') as HTMLTextAreaElement).disabled).toBe(true)
    expect(w.element.textContent).toContain('这个会话已结束')
  })
})

describe('对话列：ConvFlow 与轨迹抽屉', () => {
  it('对话区挂着 ConvFlow（M1 恒空，明写「M3 起有数据」）', async () => {
    const { w } = await mountWorkbench()
    const conv = w.element.querySelector('.af-wb__conv')
    expect(conv?.querySelector('[data-conv-input]')).not.toBeNull()
    expect(conv?.textContent).toContain('M3 起有数据')
    expect(conv?.textContent).toContain('输入区是唯一有数据源的部分')
  })

  it('`t` 打开轨迹抽屉，再按一次关掉（键位由页面注册）', async () => {
    const { w } = await mountWorkbench()
    const press = (key: string): void => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }))
    }

    press('t')
    await flush(w)
    expect(document.querySelector('[data-traj]')).not.toBeNull()

    press('t')
    await flush(w)
    expect(document.querySelector('[data-traj]')).toBeNull()
  })

  it('抽屉 Teleport 到 body（焦点栈要按 body 子树判 inert）', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('[data-conv-traj]')?.click()
    await flush(w)

    const drawer = document.querySelector('[data-traj]')
    expect(drawer).not.toBeNull()
    // 在工作台的 DOM 之外：留在里面时 `inertOthers` 会退化成「一个都没 inert」
    expect(w.element.contains(drawer)).toBe(false)
  })

  it('抽屉的三条关闭路径都只 emit close：页面把 open 置回 false', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('[data-conv-traj]')?.click()
    await flush(w)

    await document.querySelector<HTMLElement>('[data-traj-close]')?.click()
    await flush(w)
    expect(document.querySelector('[data-traj]')).toBeNull()
  })

  it('抽屉开着时「轨迹」按钮是 aria-expanded=true（受控组件不自己记开关）', async () => {
    const { w } = await mountWorkbench()
    const btn = w.element.querySelector<HTMLElement>('[data-conv-traj]')
    expect(btn?.getAttribute('aria-expanded')).toBe('false')

    await btn?.click()
    await flush(w)
    expect(btn?.getAttribute('aria-expanded')).toBe('true')
  })
})

describe('一次性命令走 exec 通道', () => {
  it('提交一条命令 = 开一条 exec 通道，参数带 command / args / cwd', async () => {
    const { w, fetchMock } = await mountWorkbench()
    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    input.value = 'ls -la'
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    await w.element.querySelector<HTMLFormElement>('.af-compose')?.dispatchEvent(
      new Event('submit', { bubbles: true, cancelable: true }),
    )
    await flush(w)

    const opens = fetchMock.mock.calls
      .map(([u, i]) => ({ url: String(u), init: i as RequestInit | undefined }))
      .filter((c) => c.url === '/api/channels' && c.init?.method === 'POST')
    const exec = opens.find((c) => String(c.init?.body ?? '').includes('"exec"'))
    expect(exec).toBeDefined()
    const body = JSON.parse(String(exec?.init?.body ?? '{}')) as {
      connector: string
      params: { command: string; args: string[]; cwd: string }
    }
    expect(body.connector).toBe('exec')
    expect(body.params.command).toBe('ls')
    expect(body.params.args).toEqual(['-la'])
    expect(body.params.cwd).toBe('')
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
