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
    // 对话走专用端点（不是 /api/channels）：`acp` 不是 connector
    '/api/sessions/s_1/conversation': {
      status: 201,
      body: { channel_id: 'ch-acp', node_id: 'n_1', session_id: 's_1' },
    },
    // 停止一轮：Hub 回 204 **空体**（所以前端不能用 `postJson` 解它）。
    '/api/sessions/s_1/conversation/cancel': { empty: true },
    '/api/sessions/s_1/trajectory': {
      body: {
        events: [
          { id: 1, seq: 1, kind: 'agent_message_chunk', payload: '{"content":[{"type":"text","text":"hi"}]}', duration_ms: null, duration_source: 'none', created_at: 'unix:1' },
        ],
        gaps: [],
        incomplete: false,
      },
    },
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

  it('工作台头写明路径锁定，以及这个会话的 agent 现状', async () => {
    const { w } = await mountWorkbench()
    const text = w.element.textContent ?? ''
    expect(text).toContain('/srv/alpha')
    expect(text).toContain('路径锁定')
    // 这个夹具的两个会话都没选 agent：标记说的是**此刻的事实**（不能对话），
    // 不是一个里程碑承诺（旧文案写的是「M3 起可选」）。
    expect(text).toContain('未选 agent（不能对话）')
    expect(text).not.toContain('M3')
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

  it('会话结束时飞着的保存必须复位 saving，否则保存按钮永久禁用', async () => {
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
    // 弄出改动，让保存按钮可点（`:disabled="!dirty || saving || ended"`）
    const area = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')
    if (!area) throw new Error('no editor')
    area.value = 'abc'
    area.dispatchEvent(new Event('input'))
    await flush(w)

    const save = () => w.element.querySelector<HTMLButtonElement>('[data-save]')
    expect(save()?.disabled).toBe(false)
    save()?.click()
    await flush(w)
    // 保存在飞：`write` 回帧还没来，按钮此时确实该禁用
    expect(save()?.disabled).toBe(true)

    // 会话结束了（`ChannelHub.onSessionState` 把该会话的通道标成 closed）：
    // `write` 回帧永远不会来，但按钮必须重新可点。
    src()?.emit('session.state', { session_id: 's_1', state: 'ended' })
    await flush(w)

    expect(save()?.disabled).toBe(false)
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
  it('对话区挂着 ConvFlow，且没有 agent 时说清为什么不能对话', async () => {
    const { w } = await mountWorkbench()
    const conv = w.element.querySelector('.af-wb__conv')
    expect(conv?.querySelector('[data-conv-input]')).not.toBeNull()
    // M1 那句「M3 起有数据」已经不成立了：对话真的能跑。现在没 agent 的会话
    // 说的是「为什么不能对话」，不是一句没来由的「无数据」。
    expect(conv?.textContent).toContain('还不能对话')
    expect(conv?.textContent).toContain('没有选 agent')
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
    // **必须先选 exec**：默认目标是「对话」（这个组件叫对话流），而对话走的是
    // ACP 子进程，不是 exec 通道。不显式选就会走错那条路。
    const target = w.element.querySelector<HTMLSelectElement>('[data-conv-target]')
    if (!target) throw new Error('no conv target')
    target.value = 'exec'
    target.dispatchEvent(new Event('change'))
    await w.vm.$nextTick()

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

  /**
   * 「归属设备已被删除」（`workspace.node_id === null`，会话还在列表里）与
   * 「这个会话根本不知道是谁的」（id 不在列表里）**是两件事**。
   *
   * 它们曾经共用 `TabMeta.nodeId = null`，于是后者从 sessionStorage 读回来时
   * 被 `isTabMeta` 的 `typeof nodeId === 'string'` 拦掉：刷新一次，tab 就静默
   * 消失，「这个会话不存在」这句结论跟着一起没了。断言必须落到「刷新之后」
   * 才抓得到——首屏的 fallback 本来就不经过读回。
   */
  it('会话未知（不在列表里）时明说「归属未知」，而不说成「设备已被删除」', async () => {
    stubFetch({ '/api/sessions': { body: [] }, '/api/devices': { body: [] } })
    sessionStorage.removeItem(STORAGE_KEY)
    window.history.replaceState(null, '', '/sessions/s_gone')
    route.value = matchRoute('/sessions/s_gone')
    const w = mountIn(SessionPage)
    await flush(w)

    // 首屏：页面是「找不到」，原因写的是「不在列表里 / 归属设备未知」
    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('不在会话列表里')
    expect(empty?.textContent).toContain('不是「设备已被删除」')
    expect(w.element.querySelector('[data-device-removed]')).toBeNull()

    // 刷新一次（同一份 sessionStorage，页面重新挂载）：结论必须还在。
    // 这一半才是本条用例的判据——首屏的 fallback 不经过读回，落回 `null`
    // 也能过；只有刷新后 tab 被 `isTabMeta` 丢掉时它才红。
    w.unmount()
    const w2 = mountIn(SessionPage)
    await flush(w2)
    expect(w2.element.querySelector('.af-empty')?.textContent).toContain('不在会话列表里')
    expect(w2.element.querySelector('.af-empty')?.textContent).toContain('不是「设备已被删除」')
  })

  it('归属设备已被删除的会话：说「设备已删除」，不说成「会话未知」', async () => {
    const sessions = [
      session({
        id: 's_1',
        workspace: { node_id: null, removed_node_id: 'n_9', path: '/srv/alpha', path_locked: true },
      }),
    ]
    stubFetch({ '/api/sessions': { body: sessions }, '/api/devices': { body: [] } })
    sessionStorage.removeItem(STORAGE_KEY)
    window.history.replaceState(null, '', '/sessions/s_1')
    route.value = matchRoute('/sessions/s_1')
    const w = mountIn(SessionPage)
    await flush(w)

    expect(w.element.querySelector('[data-device-removed]')).not.toBeNull()
    expect(w.element.querySelector('[data-tab-unknown]')).toBeNull()

    // 刷新一次：设备已删这一态也要活得下来
    w.unmount()
    const w2 = mountIn(SessionPage)
    await flush(w2)
    expect(w2.element.querySelector('[data-device-removed]')).not.toBeNull()
    expect(w2.element.querySelector('[data-tab-unknown]')).toBeNull()
  })
})

/* ------------------------------------------------------------------ 对话端到端 */

/**
 * 这些用例走的是**真接线**：从输入区一路到 `POST .../conversation` 与
 * `GET .../trajectory`。组件自己的单测证明不了页面把 `@say` 接到了哪个端点上。
 */
/** 一个**选了 agent** 的会话：只有它才走得到「开对话通道」那一步。 */
const WITH_AGENT = [
  session({
    id: 's_1',
    agent_id: 'opencode',
    workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true },
  }),
]

async function mountWithAgent(sessions = WITH_AGENT): Promise<Ctx> {
  const fetchMock = stubFetch({
    '/api/sessions': { body: sessions },
    '/api/devices': { body: DEVICES },
    '/api/sessions/s_1/conversation': {
      status: 201,
      body: { channel_id: 'ch-acp', node_id: 'n_1', session_id: 's_1' },
    },
    '/api/channels/ch-acp': { empty: true },
    '/api/channels/ch-acp/send': { empty: true },
    '/api/sessions/s_1/trajectory': {
      body: {
        events: [
          { id: 1, seq: 1, kind: 'agent_message_chunk', payload: '{"content":[{"type":"text","text":"hi"}]}', duration_ms: null, duration_source: 'none', created_at: 'unix:1' },
        ],
        gaps: [],
        incomplete: false,
      },
    },
  })
  sessionStorage.setItem(STORAGE_KEY, JSON.stringify([TWO_TABS[0]]))
  window.history.replaceState(null, '', '/sessions/s_1')
  route.value = matchRoute('/sessions/s_1')
  const w = mountIn(SessionPage)
  await flush(w)
  return {
    w,
    fetchMock,
    posts: () => 0,
    deletes: () => [],
    src: () => FakeEventSource.instances[0],
  }
}

describe('对话：说一句话走专用端点（不是 /api/channels）', () => {
  it('选了 agent 的会话，「对话」目标可选', async () => {
    const { w } = await mountWithAgent()
    const opt = w.element.querySelector<HTMLOptionElement>('option[value="chat"]')
    expect(opt?.disabled, '选了 agent 就该能对话').toBe(false)
  })

  it('没有 agent 时「对话」目标不可选', async () => {
    const { w } = await mountWithAgent([
      session({ id: 's_1', agent_id: null, workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
    ])
    const opt = w.element.querySelector<HTMLOptionElement>('option[value="chat"]')
    expect(opt?.disabled).toBe(true)
  })

  it('发送 → POST /api/sessions/{id}/conversation，不是 /api/channels', async () => {
    const { w, fetchMock } = await mountWithAgent()
    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    input.value = '帮我看看测试为什么红'
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    w.element.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }))
    await flush(w)

    const urls = fetchMock.mock.calls.map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    // **不是 /api/channels**：`acp` 不是 connector，通用端点的能力门禁会 409
    expect(urls, `实际请求：${urls.join(', ')}`).toContain('/api/sessions/s_1/conversation')
  })

  it('没有 agent 时点发送不会开任何对话通道', async () => {
    const { w, fetchMock } = await mountWithAgent([
      session({ id: 's_1', agent_id: null, workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
    ])
    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    input.value = 'hi'
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    w.element.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }))
    await flush(w)

    const urls = fetchMock.mock.calls.map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    expect(urls, `实际请求：${urls.join(', ')}`).not.toContain('/api/sessions/s_1/conversation')
  })

  /*
    设备已被删除（`workspace.node_id === null`）但**选了 agent**：
    发送路必须关掉。不关的话输入框能打字、按钮能点，一点就是 `device_removed`
    409——而同一屏顶上的横幅正写着「设备已被删除」，两句话互相打架。

    判据是 `deviceRemoved`，**不是** `ended`：这个会话的 `state` 仍是 `running`，
    会话本身没结束（删设备不删会话）。

    反向验证：把 `SessionPage.vue` 里 `convSay` / `terminalInput` / `runCommand`
    的 `deviceRemoved` 判据，或 `ConvFlow` 的 `deviceGone` 门禁删掉，
    「发送按钮 / 输入框仍可用」与「真的发出了请求」两条都会红。
  */
  it('设备已被删除：输入与发送都关掉，横幅说清原因', async () => {
    const { w, fetchMock } = await mountWithAgent([
      session({
        id: 's_1',
        agent_id: 'opencode',
        state: 'running',
        workspace: { node_id: null, removed_node_id: 'n_9', path: '/srv/alpha', path_locked: true },
      }),
    ])

    // 横幅与输入区说的是同一件事：删设备不删会话，但消息发不出去
    const banner = w.element.querySelector<HTMLElement>('[data-device-removed]')
    expect(banner, '设备已删除的横幅必须在').not.toBeNull()
    expect(banner?.textContent).toContain('设备已被删除')
    expect(banner?.textContent).toContain('n_9')

    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    expect(input.disabled, '设备没了，输入框不该还能打字').toBe(true)
    // 真把文本塞进 value 再提交：`disabled` 只是界面，发送路本身也必须拦住
    input.value = '这条不该发出去'
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    w.element.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }))
    await flush(w)

    const runBtn = w.element.querySelector<HTMLButtonElement>('[data-conv-run]')
    expect(runBtn?.disabled, '发送按钮必须禁用').toBe(true)

    const urls = fetchMock.mock.calls.map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    expect(urls, `实际请求：${urls.join(', ')}`).not.toContain('/api/sessions/s_1/conversation')
    expect(urls, `实际请求：${urls.join(', ')}`).not.toContain('/api/channels')
  })

  it('设备已被删除：界面那一层被绕过时，发送路自己也不开通道', async () => {
    const { w, fetchMock } = await mountWithAgent([
      session({
        id: 's_1',
        agent_id: 'opencode',
        state: 'running',
        workspace: { node_id: null, removed_node_id: 'n_9', path: '/srv/alpha', path_locked: true },
      }),
    ])

    /*
      **不能靠抹 `disabled` 来测这一层**：提交按钮的 `:disabled` 绑在 computed 上，
      抹掉之后 Vue 下一次 patch 就又写回 `true`（实测被它挡在 `submit()` 之前）。
      所以直接调页面接在 `@say` / `@run` / `@send` 上的 handler——`disabled` 是
      界面，handler 里那道判据才是「发不发得出去」。
    */
    const flow = w.findComponent({ name: 'ConvFlow' })
    expect(flow.exists(), 'ConvFlow 必须挂着').toBe(true)
    flow.vm.$emit('say', '这条不该发出去')
    flow.vm.$emit('run', { command: 'ls', args: [], cwd: '' })
    flow.vm.$emit('send', '这条也不该发出去\n')
    await flush(w)

    const urls = fetchMock.mock.calls.map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    expect(urls, `实际请求：${urls.join(', ')}`).not.toContain('/api/sessions/s_1/conversation')
    expect(urls, `实际请求：${urls.join(', ')}`).not.toContain('/api/channels')
  })
})

describe('工作台头：agent 标记说真话', () => {
  /*
    这两条按**值**断言，不按「有那个 class」断言：`af-t-mute` 在同一行里出现
    好几次（分隔点、开始时间），用选择器挑出来的可能根本不是这一格。
  */
  it('选了 agent：显示它的 id，与 ConvFlow 拿到的是同一个值', async () => {
    const { w } = await mountWithAgent()
    const badge = w.element.querySelector<HTMLElement>('[data-agent-badge]')
    expect(badge?.textContent?.trim()).toBe('opencode')
    expect(w.findComponent({ name: 'ConvFlow' }).props('agentId')).toBe('opencode')
  })

  it('没选 agent：说明这个会话不能对话（不写任何里程碑号）', async () => {
    const { w } = await mountWithAgent([
      session({ id: 's_1', agent_id: null, workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
    ])
    const badge = w.element.querySelector<HTMLElement>('[data-agent-badge]')
    expect(badge?.textContent).toContain('未选 agent')
    expect(badge?.textContent).toContain('不能对话')
    expect(badge?.textContent).not.toMatch(/M\d/)
    expect(w.findComponent({ name: 'ConvFlow' }).props('agentId')).toBeNull()
  })
})

describe('对话通道：关 tab 时释放，不只是一直往表里加', () => {
  /** 在这个会话里说一句话：这是**唯一**会建对话通道的路径（懒建立）。 */
  async function say(w: ReturnType<typeof mountIn>, text = '帮我看看测试为什么红'): Promise<void> {
    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    input.value = text
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    w.element.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }))
    await flush(w)
  }

  /*
    `chatHandles` 不进 `channels`（见页面里那张表的头注释），所以 `releaseTab`
    里那条按面板名遍历的释放路径**碰不到它**。不补这一步的话，每开一个会话说一句
    话，表里就多一条永不回收的记录，关 tab 也不减。

    **不能只断言 DELETE**：`close()` 会发 DELETE，而「删掉表项」不发任何请求，
    所以「关了但没删」与「关了且删了」在网络上完全一样——前者正是这个 bug，
    断言 `deleted` 时它会红，断言 DELETE 时它照样绿（实测：把 `chatHandles.delete`
    删掉，DELETE `/api/channels/ch-acp` 仍然发生）。所以这里读页面暴露的表长度。
  */
  it('关掉说过话的 tab → 对话句柄从表里释放（不只是 close() 了一下）', async () => {
    const { w, fetchMock } = await mountWithAgent()
    await say(w)

    const urls = () => fetchMock.mock.calls.map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    expect(urls(), '先说清楚这一句真的开了对话通道').toContain('/api/sessions/s_1/conversation')
    expect(urls()).not.toContain('/api/channels/ch-acp')

    const count = (): number =>
      (w.vm as unknown as { chatHandleCount: () => number }).chatHandleCount()
    expect(count(), '说了一句话，表里应当有且只有一条对话句柄').toBe(1)

    const closer = w.element.querySelector<HTMLElement>('[data-tab-close="s_1"]')
    expect(closer, '关 tab 的按钮必须在').not.toBeNull()
    await closer?.click()
    await flush(w)

    expect(count(), '关 tab 必须把对话句柄从表里删掉，否则这条记录永不回收').toBe(0)

    // 顺带锁住「关」这件事本身：表项没了，设备侧那条通道也必须被关掉
    const deletes = fetchMock.mock.calls
      .filter(([, init]) => (init as RequestInit | undefined)?.method === 'DELETE')
      .map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    expect(deletes).toContain('/api/channels/ch-acp')
  })
})

describe('对话帧驱动的轨迹重读：成批到达时合并', () => {
  /*
    真机一次长回复会把 `session/update` 聚成一批一批的帧推上来。每帧读一次
    整表，`GET .../trajectory` 的次数就等于帧数——而每帧带来的新内容往往只有
    几个字。这里连发 8 帧，断言读的次数**远小于**帧数（合并窗口内只读一次）。

    用真实时间等一个窗口：本套件的 `flush()` 靠 `requestAnimationFrame` 推进，
    混入 fake timers 会让它停摆，代价比多等 200ms 大。
  */
  it('连发 8 帧只读一次轨迹，而不是 8 次', async () => {
    const { w, fetchMock, src } = await mountWithAgent()

    // 先说话，把对话通道建起来——帧只在这个通道上送达。
    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    input.value = '帮我看看'
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    w.element.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }))
    await flush(w)

    const trajReads = (): number =>
      fetchMock.mock.calls.filter(
        ([u]) => new URL(String(u), 'http://hub.local').pathname === '/api/sessions/s_1/trajectory',
      ).length
    const before = trajReads()

    // 八帧几乎同时到达（同一轮同步发完，都在一个合并窗口里）。
    for (let i = 0; i < 8; i++) {
      src()?.emit('channel.frame', {
        node_id: 'n_1',
        channel_id: 'ch-acp',
        payload: { kind: 'session/update', update: { sessionUpdate: 'agent_message_chunk' } },
      })
      // 帧之间让上一次读跑完：真机上帧是隔几百毫秒到的，不是同一个 tick 里涌进来。
      // 若不等，`trajLoading` 会把整批合并成一次，测不到「每帧一读」的缺陷。
      await flush(w)
      await new Promise((r) => setTimeout(r, 10))
      await flush(w)
    }
    // 合并窗口过去之后应当**恰好**多读一次。
    await new Promise((r) => setTimeout(r, 260))
    await flush(w)

    const reads = trajReads() - before
    expect(reads, `八帧触发了 ${reads} 次整表重读；未合并时每一帧都会读一次（实测 8）`).toBeLessThanOrEqual(2)
    expect(reads, '合并不能把读全吞掉：突发结束后必须还读一次').toBeGreaterThanOrEqual(1)
  })
})

describe('轨迹抽屉：打开即读，读到就画', () => {
  it('按 t 打开抽屉 → GET .../trajectory，事件画进列表', async () => {
    const { w, fetchMock } = await mountWithAgent()

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 't', bubbles: true, cancelable: true }))
    await flush(w)

    const urls = fetchMock.mock.calls.map(([u]) => new URL(String(u), 'http://hub.local').pathname)
    expect(urls, `实际请求：${urls.join(', ')}`).toContain('/api/sessions/s_1/trajectory')

    const rows = document.querySelectorAll('[data-traj-row]')
    expect(rows.length, '轨迹行要画出来').toBe(1)
    expect(rows[0]?.textContent).toContain('agent_message_chunk')
  })
})

describe('停止按钮：真的有前端链路（不是只画了个按钮）', () => {
  /**
   * 判据必须是**发出去的请求**，不是「按钮存在」或「点了没抛错」。
   *
   * 修前这条路径整条是空的：`ConvFlow` `emit('cancel')`，而 `SessionPage`
   * 的 `<ConvFlow>` 只有 `@run/@send/@say/@open-traj` 四个监听器，没有
   * `@cancel`；`api.ts` 里也没有任何 cancel 函数。于是按钮可点、点了什么
   * 都不发生，而当时的测试**没有任何一条**碰过 `[data-conv-stop]`。
   *
   * 反向验证：删掉 `SessionPage.vue` 模板里那行 `@cancel="convCancel"`
   * → 这里收不到 POST，红。
   */
  it('点停止 → POST /api/sessions/{id}/conversation/cancel', async () => {
    const fetchMock = stubFetch({
      '/api/sessions': { body: WITH_AGENT },
      '/api/devices': { body: DEVICES },
      '/api/sessions/s_1/conversation': {
        status: 201,
        body: { channel_id: 'ch-acp', node_id: 'n_1', session_id: 's_1' },
      },
      '/api/channels/ch-acp': { empty: true },
      '/api/channels/ch-acp/send': { empty: true },
      // 有一点还没收口 → 停止按钮可点。`incomplete` 就是它的唯一判据。
      '/api/sessions/s_1/trajectory': {
        body: {
          events: [
            { id: 1, seq: 1, kind: 'agent_message_chunk', payload: '{"content":[{"type":"text","text":"hi"}]}', duration_ms: null, duration_source: 'none', created_at: 'unix:1' },
          ],
          gaps: [],
          incomplete: true,
        },
      },
      '/api/sessions/s_1/conversation/cancel': { empty: true },
    })
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify([TWO_TABS[0]]))
    window.history.replaceState(null, '', '/sessions/s_1')
    route.value = matchRoute('/sessions/s_1')
    const w = mountIn(SessionPage)
    await flush(w)

    // 先把轨迹读进来：`incomplete` 由它决定，按钮是否可点取决于它。
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 't', bubbles: true, cancelable: true }))
    await flush(w)
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 't', bubbles: true, cancelable: true }))
    await flush(w)

    const stop = document.querySelector<HTMLElement>('[data-conv-stop]')
    expect(stop, '停止按钮要渲染出来').toBeTruthy()
    expect(stop!.hasAttribute('disabled'), '有未收口的一轮时停止按钮必须可点').toBe(false)

    stop!.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await flush(w)

    const calls = fetchMock.mock.calls.map(([url, init]) => ({
      url: new URL(String(url), 'http://hub.local').pathname,
      method: ((init as RequestInit | undefined)?.method ?? 'GET').toUpperCase(),
    }))
    const cancel = calls.filter((c) => c.url === '/api/sessions/s_1/conversation/cancel')
    expect(
      cancel.length,
      `点停止必须发出 cancel 请求，实收：${JSON.stringify(calls)}`,
    ).toBe(1)
    expect(cancel[0]!.method, 'cancel 是 POST').toBe('POST')
  })
})
