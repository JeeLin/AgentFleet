import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import SessionPage from '@/pages/SessionPage.vue'
import { device, mountIn, session } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { cssRuleOf, readRepoFile } from '@/test/css'
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
 * 确保面板坞开着。**幂等**。
 *
 * v0.4.0 起进会话就是「坞已展开、缺省 git」（用户拍板：打开就是 git 界面），
 * 所以这里多数时候什么都不做——但保留「没开就点页面头那个按钮」的分支，
 * 因为 `closeDock()` 之后的用例仍要能从关着的状态打开。
 */
async function openDock(w: ReturnType<typeof mountIn>): Promise<void> {
  if (w.element.querySelector('.af-docktabs')) return
  await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
  await flush(w)
}

/**
 * 把文件面板拉进坞并显示它。**到此为止不碰面板正文**——通道因此还没建
 * （§5.5.1 的懒建立：点 / 聚焦坞标签都**不**算交互，只有碰正文才算）。
 *
 * v0.4.0 改走坞头的「＋」：进会话时坞里只有 git，而 `]`（cyclePanel）在
 * 单面板下是「拉**下一个**面板进来」——按 `PANEL_ORDER` 那是 terminal，
 * 不是 files。走「＋」挑名字既确定，又顺带覆盖了那个入口本身。
 */
async function openFiles(w: ReturnType<typeof mountIn>): Promise<void> {
  await openDock(w)
  const add = w.element.querySelector<HTMLElement>('[data-add="toggle"]')
  if (!add) throw new Error('the add button is missing')
  await add.click()
  await flush(w)
  const candidate = w.element.querySelector<HTMLElement>('[data-add="files"]')
  if (!candidate) throw new Error('files is not among the add candidates')
  await candidate.click()
  await flush(w)
  const tab = w.element.querySelector<HTMLElement>('[data-panel="files"]')
  if (!tab) throw new Error('files panel did not mount')
  await tab.click()
  await flush(w)
}

/**
 * 用坞头「＋」把终端拉进坞并选中它。
 *
 * v0.4.0 起缺省面板是 git，页面头那个「打开面板」按钮开的也是 git——所以
 * 「给我一个终端」这条路径现在只剩「＋」。这也正是它必须留在候选里的理由：
 * 它是终端唯一的入口。
 */
async function openTerminal(w: ReturnType<typeof mountIn>): Promise<void> {
  await openDock(w)
  const add = w.element.querySelector<HTMLElement>('[data-add="toggle"]')
  if (!add) throw new Error('the add button is missing')
  await add.click()
  await flush(w)
  const candidate = w.element.querySelector<HTMLElement>('[data-add="terminal"]')
  if (!candidate) throw new Error('terminal is not among the add candidates')
  await candidate.click()
  await flush(w)
}

/** 在 document 上按一个键（键位表监听在那里）。 */
function press(key: string): void {
  document.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }))
}

/** 取所有发往通道的 send body。 */
function sentBodies(fetchMock: ReturnType<typeof stubFetch>): Array<Record<string, unknown>> {
  return fetchMock.mock.calls
    .filter(([url]) => String(url).includes('/send'))
    .map(([, init]) => JSON.parse(String((init as RequestInit).body)) as Record<string, unknown>)
}

/**
 * 取某一种 op 的请求体。没有就抛——测试里这是前提不成立，继续往下断言
 * 只会拿到 `undefined` 上的属性。
 */
function reqOf(sent: Array<Record<string, unknown>>, op: string): Record<string, unknown> {
  const r = sent.find((b) => b.op === op)
  if (!r) throw new Error(`要发出 ${op} 请求，实发：${JSON.stringify(sent)}`)
  return r
}

/**
 * 用「＋」把一个具名面板拉进坞并选中它。
 *
 * 面板还没挂载时必须走「＋」——那是缺省状态下唯一的入口；已挂载的则直接点
 * 它的标签（「＋」的候选里本来就不含已挂载的）。
 */
async function openPanelNamed(w: ReturnType<typeof mountIn>, name: string): Promise<void> {
  await openDock(w)
  const existing = w.element.querySelector<HTMLElement>(`[data-panel="${name}"]`)
  if (existing) {
    await existing.click()
    await flush(w)
    return
  }
  const add = w.element.querySelector<HTMLElement>('[data-add="toggle"]')
  if (!add) throw new Error('the add button is missing')
  await add.click()
  await flush(w)
  const candidate = w.element.querySelector<HTMLElement>(`[data-add="${name}"]`)
  if (!candidate) throw new Error(`${name} is not among the add candidates`)
  await candidate.click()
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
  it('对话区默认 56%，且进会话就是 git 面板（v0.4.0 起坞默认展开）', async () => {
    // 这条原来断言「坞没开时没有把手也没有面板」。v0.4.0 推翻了那个缺省
    // （用户拍板：会话里最常看的是「这次改了什么」，git 比终端更值得常驻），
    // 于是断言翻过来——**并且翻的不只是"存在"**：缺省的必须是 git，
    // 否则「打开就是 git 界面」就不成立。
    const { w } = await mountWorkbench()
    expect(w.element.querySelector('.af-wb__conv')).not.toBeNull()
    expect(w.element.querySelector('[role="separator"]')).not.toBeNull()
    expect(w.element.querySelector('[data-panelhost]')).not.toBeNull()

    const tabs = Array.from(w.element.querySelectorAll<HTMLElement>('[data-panel]'))
    expect(tabs.map((t) => t.dataset.panel)).toEqual(['git'])
    expect(
      w.element.querySelector<HTMLElement>('[data-panel="git"]')?.getAttribute('aria-selected'),
    ).toBe('true')
    // 正文真的渲染了，不是空壳：占位说明在，且面板头写的是 Git。
    expect(w.element.textContent).toContain('Git')
    expect(w.element.querySelector('.af-panelhead')?.textContent).toContain('Git')
  })

  it('打开面板后出现把手，并把对话区宽度写成 flex-basis（默认 56%）', async () => {
    const { w } = await mountWorkbench()
    await w.element.querySelector<HTMLElement>('.af-wb__head button')?.click()
    await flush(w)

    const sep = w.element.querySelector<HTMLElement>('[role="separator"]')
    expect(sep?.getAttribute('aria-valuenow')).toBe('56')
    expect((w.element.querySelector<HTMLElement>('.af-wb__conv') as HTMLElement).style.flex).toContain('56%')
  })

  /**
   * 页面头「打开面板」按钮开的是**缺省面板**，不是某个写死的字面量。
   *
   * 这一条是给 `DEFAULT_PANEL` 兜底的：那个常量收敛了六个回落点，但页面头
   * 这个按钮曾是第七个——它写的是裸的 `'git'`，改缺省值时它不跟着变，于是
   * 「按 `o` 开 git、点头上的按钮开另一个」。那次是审查看出来的，不是测试
   * 抓出来的（**当时没有任何一条用例覆盖它**）。
   *
   * **判据为什么读源文件而不是断言渲染结果**：第一版这条用例断言「点了之后
   * 选中 `git` 标签」，而 `DEFAULT_PANEL` 当前值就是 `'git'`——「按钮用常量」与
   * 「按钮写死 `'git'`」在任何输入下产出同一个标签。实测把
   * `openPanel(DEFAULT_PANEL)` 改回 `openPanel('git')` 仍然绿，零检测力。
   * 它能守住的只能是**静态源文本**：那一行的实参是标识符而不是引号字面量。
   */
  it('页面头的「打开面板」按钮跟 DEFAULT_PANEL 走，不写死字面量', async () => {
    const src = readRepoFile('packages/web/src/pages/SessionPage.vue')

    const bind = /data-open-panel[\s\S]{0,400}?@click="openPanel\(([^)]*)\)"/.exec(src)
    if (!bind) {
      throw new Error('页面头的「打开面板」按钮不在，或它不再用 @click="openPanel(...)"：换锚点了吗？')
    }
    expect(bind[1]?.trim(), '页面头按钮的实参必须是 DEFAULT_PANEL，不能是引号字面量').toBe('DEFAULT_PANEL')

    // 反向兜底：整个文件里不该再有写死缺省面板的调用。
    expect(
      /openPanel\(['"]git['"]\)/.test(src),
      "不该出现 openPanel('git')——缺省值只有 DEFAULT_PANEL 一个真相源",
    ).toBe(false)

    // 行为侧仍验一次（按钮真能打开面板），但它不是这条用例的判据。
    const { w } = await mountWorkbench()
    const button = w.element.querySelector<HTMLElement>('[data-open-panel]')
    if (!button) throw new Error('页面头的「打开面板」按钮不在：它换锚点了吗？')
    await button.click()
    await flush(w)
    const selected = w.element.querySelector<HTMLElement>('[data-panel][aria-selected="true"]')
    expect(selected?.getAttribute('data-panel')).toBe('git')
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
  it('展开坞并选中终端面板即建通道——用户已经明确要一个终端', async () => {
    // 原来这条断言「打开坞不建通道」。那把它变成了隐性前置条件：用户看到的是
    // 一块没有任何说明的空白终端，唯一出路是「随便敲一个键」——而这个前条件
    // 没有任何地方写着。现在「选中终端面板」本身就算建立时机，只有「展开坞
    // 但选中 files / 还没选」才不建（见下一条）。
    const { w, posts } = await mountWorkbench()
    await openTerminal(w)
    expect(posts()).toBe(1)
  })

  it('把 files 拉进坞不建第二条通道：它的首个动作（列目录）自己会开', async () => {
    // files 面板**不**由 `openPanel` 预建通道——把 files 拉进来本身不该多一条。
    // v0.4.0 起进会话就有 git 那条（`mounted: ['git']`，不建通道），所以这里
    // 数的是「除了 git 之外没多出终端那条」：openFiles 之后仍是 0 条。
    const { w, posts } = await mountWorkbench()
    await openFiles(w)
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

  it('展开坞即连上终端：面板头显示「已连接」，不是「未建立」', async () => {
    // 用户原话「打开终端要点一下才会连接终端」：正确的读法是「打开终端就该
    // 连上」。判据取**面板头里那块状态徽标的文字**——它是用户能看见的唯一
    // 连接状态，也正是原来一直停在「未建立」的那个地方。
    const { w } = await mountWorkbench()
    await openTerminal(w)

    const head = w.element.querySelector<HTMLElement>('.af-panelhead')
    if (!head) throw new Error('dock header did not render')
    const stateText = head.textContent ?? ''
    expect(stateText).toContain('已连接')
    expect(stateText).not.toContain('未建立')
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
    await openTerminal(w)
    await interact(w)
    expect(posts()).toBe(1)

    // s_2 也有它自己的 git 面板（v0.4.0 起每个 tab 都这样），所以这里不能再
    // 断言「没有面板」——要断言的是**它的面板是 git 且没带走 s_1 的终端**。
    await tabLabel(w, 's_2').click()
    await flush(w)
    expect(w.element.querySelector<HTMLElement>('.af-panelhead')?.textContent).toContain('Git')
    expect(w.element.querySelector<HTMLElement>('.af-panelhead')?.textContent).not.toContain('终端')

    await tabLabel(w, 's_1').click()
    await flush(w)

    expect(posts()).toBe(1)
    expect(deletes()).toEqual([])
  })

  it('切走再切回来，终端缓冲还在（状态挂在 tab 上，不是全局一份）', async () => {
    const { w } = await mountWorkbench()
    await openTerminal(w)
    await interact(w)

    await frame(w, FakeEventSource.instances[0], { bytes: 'hello from pty' })
    expect(terminalText(w)).toContain('hello from pty')

    await tabLabel(w, 's_2').click()
    await flush(w)
    // s_2 显示的是它自己的 git 面板，s_1 的终端不在画面里。
    expect(w.element.querySelector('.xterm')).toBeNull()

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
    await openTerminal(w)
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
    await openTerminal(w)

    // v0.4.0 起每个 tab 进会话就带 git，所以这里开过的是终端那个——
    // 徽标数的是「这个 tab 挂了几块面板」，不是「用户手动开过几次」。
    const tab = w.element.querySelector<HTMLElement>('[data-sid="s_1"]')
    expect(tab?.querySelector('.af-badge')?.textContent?.trim()).toBe('2')
  })

  it('每个 tab 进会话就带一个面板徽标（v0.4.0：缺省 git）', async () => {
    // 这条原来是「没开过面板的 tab 没有徽标」。那个前提随「进会话即 git」
    // 一起作废了：s_2 没被点过，但它有自己的 git 面板，所以它有徽标。
    const { w } = await mountWorkbench()

    expect(
      w.element.querySelector('[data-sid="s_2"] .af-badge')?.textContent?.trim(),
    ).toBe('1')
  })
})

describe('键盘', () => {
  it('`o` 打开面板，`\\` 收起坞', async () => {
    const { w } = await mountWorkbench()
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
    // v0.4.0 起进会话就有面板（缺省 git），所以判据不能是「没有面板」。
    // 这条要钉的是「输入框里按 o 不抢键」：面板状态**一点没变**——仍是 git、
    // 仍只有一个标签、也没有因此建通道。
    const tabs = Array.from(w.element.querySelectorAll<HTMLElement>('[data-panel]'))
    expect(tabs.map((t) => t.dataset.panel)).toEqual(['git'])
    expect(
      w.element.querySelector<HTMLElement>('[data-panel="git"]')?.getAttribute('aria-selected'),
    ).toBe('true')
  })
})

describe('面板正文是 13b / 13c 的真组件，不是 13a 的占位', () => {
  it('终端位挂着真 xterm 面板（占位那句「13b 挂入」不再出现）', async () => {
    const { w } = await mountWorkbench()
    await openTerminal(w)

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

describe('关 tab 时把还挂着的东西一并撤掉', () => {
  /** 说一句话：建对话通道（懒建立，这是唯一入口）。 */
  async function say(w: ReturnType<typeof mountIn>): Promise<void> {
    const input = w.element.querySelector<HTMLTextAreaElement>('[data-conv-input]')
    if (!input) throw new Error('no conv input')
    input.value = '关 tab 之前说的话'
    input.dispatchEvent(new Event('input'))
    await w.vm.$nextTick()
    w.element.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }))
    await flush(w)
  }

  const subs = (w: ReturnType<typeof mountIn>): number =>
    (w.vm as unknown as { chatSubscriberCount: () => number }).chatSubscriberCount()

  it('关掉 tab 会退掉对话通道的订阅（`onState` / `onFrame` 两个回调）', async () => {
    // 守 `chatUnsubs`。判据是**页面自己记的订阅数**，不是网络请求：
    //
    // `refreshTrajectory` 首行 `workspaces.get(tabId)` 在 `workspaces.delete`
    // 之后直接 return，所以「订阅没退」在请求数上与「退了」完全一样——
    // 我最初就是按请求数断言的，实测把 `chatUnsubs` 的退订整段换成
    // `void chatUnsubs` 之后用例照样绿。要能证伪只能读订阅表本身。
    const { w } = await mountWithAgent()
    await say(w)
    expect(subs(w), '说了一句话就挂上了两个回调').toBe(1)

    const closer = w.element.querySelector<HTMLElement>('[data-tab-close="s_1"]')
    expect(closer, '关 tab 的按钮必须在').not.toBeNull()
    await closer?.click()
    await flush(w)

    expect(subs(w), '关 tab 必须退掉对话通道的订阅，否则回调挂在已丢弃的 ws 上').toBe(0)
  })

  it('关 tab 会撤掉挂在窗口里的轨迹重读定时器', async () => {
    // 守 `releaseTab` 里那段 `trajTimers` 清理。**这是第二道防线**，第一道是
    // `refreshTrajectory` 首行的 `workspaces.get(tabId)` 守卫（取不到就 return）。
    // 因为两道都在，删掉任何一道都观察不到行为差异——所以这条不假称「断言了
    // 行为」，它钉的是**清理本身做了**（定时器表在关 tab 后不应还留着这个 tab）。
    //
    // 时序必须是「帧刚把定时器挂上 → 立刻关 tab」，中间不能等过
    // `TRAJ_DEBOUNCE_MS`。
    const { w, src } = await mountWithAgent()
    await say(w)

    src()?.emit('channel.frame', {
      node_id: 'n_1',
      channel_id: 'ch-acp',
      payload: { kind: 'session/update', update: { sessionUpdate: 'agent_message_chunk' } },
    })
    await flush(w)

    const closer = w.element.querySelector<HTMLElement>('[data-tab-close="s_1"]')
    await closer?.click()
    await flush(w)

    // 订阅已退（上一条用例守住），定时器也已撤：再等过整个窗口，静默即可。
    // 这里只断言「没有把异常打到控制台」是弱判据，所以真正的判据放在
    // 下面——关 tab 之后再说话也不该重读（workspaces 里没有它了）。
    await new Promise((r) => setTimeout(r, 260))
    await flush(w)
    expect(subs(w), '关掉之后订阅表里不该还留着这个 tab').toBe(0)
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

/* ===========================================================================
   v0.4.0 子任务 4：面板坞接入第三个面板（git），并修「文件面板没有入口」
   ---------------------------------------------------------------------------
   这一组同时是 Bug 表里那条 🔴 的守门测试：加 git 之前 `mounted` 是空数组，
   而标签由 `mounted` 渲染、`mounted` 只由点标签填充——空数组下画不出标签，
   于是点不到标签、也就永远填不满。任何面板（包括 files 与 git 自己）都不可达。
   =========================================================================== */
describe('面板坞：三个面板与「＋」入口', () => {
  it('缺省进会话就是 git 面板（打开就是 git 界面）', async () => {
    const { w, posts } = await mountWorkbench()

    const tabs = Array.from(w.element.querySelectorAll<HTMLElement>('[data-panel]'))
    expect(tabs.map((t) => t.dataset.panel)).toEqual(['git'])
    expect(
      w.element.querySelector<HTMLElement>('[data-panel="git"]')?.getAttribute('aria-selected'),
    ).toBe('true')

    // 正文不是空壳：面板头写着 Git，正文里有 git 面板自己的那块。
    const head = w.element.querySelector<HTMLElement>('.af-panelhead')
    expect(head?.textContent).toContain('Git')
    // 真 git 面板已挂上（子任务 5 起，不再是我在子任务 4 留的占位）。
    expect(w.element.querySelector('[data-git-panel]')).not.toBeNull()

    // 面板挂着 ≠ 通道建了：git 也走懒建立（碰正文才算）。
    expect(posts()).toBe(0)
  })

  it('三个面板都有标签与图标，扩枚举时漏一项会编译不过', async () => {
    // `PANEL_LABEL` / `PANEL_ICON` 是 `Record<PanelName, string>`，类型层已保证
    // 三项齐全；这里再在运行时核一遍，防的是有人把它们改成 `Partial<...>`
    // 或 `Record<string, string>` —— 那时漏项会静默渲染成空字符串。
    const { w } = await mountWorkbench()
    for (const name of ['terminal', 'files', 'git']) {
      await openPanelNamed(w, name)
      const tab = w.element.querySelector<HTMLElement>(`[data-panel="${name}"]`)
      if (!tab) throw new Error(`${name} 面板没有标签`)
      const text = (tab.textContent ?? '').trim()
      expect(text, `${name} 的标签与图标都不能为空`).not.toBe('')
    }
  })

  it('「＋」在缺省状态下列出终端与文件（不含已挂载的 git）', async () => {
    const { w } = await mountWorkbench()

    const add = w.element.querySelector<HTMLElement>('[data-add="toggle"]')
    if (!add) throw new Error('坞头没有「＋」')
    await add.click()
    await flush(w)

    const names = Array.from(w.element.querySelectorAll<HTMLElement>('[data-add]'))
      .map((el) => el.dataset.add)
      .filter((n) => n !== 'toggle' && n !== 'cancel')
    expect(names).toEqual(['terminal', 'files'])
    // 已挂载的不出现在候选里：它已经在标签条上了。
    expect(names).not.toContain('git')
  })

  it('点「＋」里的文件 → files 挂上并成为当前面板（🔴 文件面板入口）', async () => {
    // 这条就是 Bug 表那条 🔴 的守门测试：把「＋」删掉它必须红。
    const { w } = await mountWorkbench()

    await openFiles(w)

    const tabs = Array.from(w.element.querySelectorAll<HTMLElement>('[data-panel]'))
    expect(tabs.map((t) => t.dataset.panel)).toEqual(['git', 'files'])
    expect(
      w.element.querySelector<HTMLElement>('[data-panel="files"]')?.getAttribute('aria-selected'),
    ).toBe('true')
    expect(w.element.querySelector('[data-role="tree"]')).not.toBeNull()
  })

  it('点「＋」里的终端 → 终端挂上并建通道（终端唯一的入口）', async () => {
    // 缺省面板改成 git 之后，「＋」是终端唯一的路。漏掉它等于把终端弄没了。
    const { w, posts } = await mountWorkbench()

    await openTerminal(w)

    expect(
      w.element.querySelector<HTMLElement>('[data-panel="terminal"]')?.getAttribute('aria-selected'),
    ).toBe('true')
    expect(w.element.querySelector('[data-terminal]')).not.toBeNull()
    expect(posts(), '选中终端面板就该建通道（v0.3.0 的口径）').toBe(1)
  })

  it('候选随挂载变化：挂上 files 之后只剩终端', async () => {
    const { w } = await mountWorkbench()
    await openFiles(w)

    const add = w.element.querySelector<HTMLElement>('[data-add="toggle"]')
    if (!add) throw new Error('坞头没有「＋」')
    await add.click()
    await flush(w)

    const names = Array.from(w.element.querySelectorAll<HTMLElement>('[data-add]'))
      .map((el) => el.dataset.add)
      .filter((n) => n !== 'toggle' && n !== 'cancel')
    expect(names).toEqual(['terminal'])
  })

  it('「收起」不改变挂载，只把候选收回去', async () => {
    const { w } = await mountWorkbench()
    const add = w.element.querySelector<HTMLElement>('[data-add="toggle"]')
    await add?.click()
    await flush(w)

    await w.element.querySelector<HTMLElement>('[data-add="cancel"]')?.click()
    await flush(w)

    expect(w.element.querySelector('[data-add="cancel"]')).toBeNull()
    const tabs = Array.from(w.element.querySelectorAll<HTMLElement>('[data-panel]'))
    expect(tabs.map((t) => t.dataset.panel)).toEqual(['git'])
  })

  it('`]` 轮转顺序是 terminal → files → git → terminal', async () => {
    // 三面板之后「另一个」不再唯一，`otherPanel` 改成按固定顺序轮转。
    // 这条锁的是那个行为改变——它同时是「＋」候选的排列顺序。
    const { w } = await mountWorkbench()
    await openTerminal(w)
    await openFiles(w)
    // 现在三个都挂上了，当前是 files。

    const selected = (): string | null | undefined =>
      w.element
        .querySelector<HTMLElement>('[data-panel][aria-selected="true"]')
        ?.getAttribute('data-panel')

    expect(selected()).toBe('files')
    press(']')
    await flush(w)
    expect(selected(), 'files 之后是 git').toBe('git')
    press(']')
    await flush(w)
    expect(selected(), 'git 之后回环到 terminal').toBe('terminal')
  })

  it('关掉坞之后 openDock 能从关着的状态再把坞打开（走回落分支）', async () => {
    // `openDock` 的注释说「保留回落分支，因为 `closeDock()` 之后的用例仍要能
    // 从关着的状态打开」——在此之前**没有任何用例**走那条分支（实测 31 次
    // 快路径 / 0 次回落）。这条把它钉住：先真的关掉坞，再走 `openDock`。
    const { w } = await mountWorkbench()
    expect(w.element.querySelector('.af-docktabs'), '前提：进会话时坞是开着的').not.toBeNull()

    await w.element.querySelector<HTMLElement>('.af-docktabs__close')?.click()
    await flush(w)
    expect(w.element.querySelector('.af-docktabs'), '前提：点关闭之后坞真的没了').toBeNull()

    // 这一步必须走 177-181 行的回落分支（快路径在 178 行就 return 了）。
    await openDock(w)
    expect(w.element.querySelector('.af-docktabs'), 'openDock 要从关着的状态把坞打开').not.toBeNull()
  })

  it('关掉坞之后按 `o` 回到只有 git 的干净状态', async () => {
    const { w, deletes } = await mountWorkbench()
    await openTerminal(w)
    await interact(w)
    expect(deletes()).toEqual([])

    // `\` 收起坞 = 释放它托管的面板。
    press('\\')
    await flush(w)
    expect(w.element.querySelector('[data-panelhost]')).toBeNull()
    expect(deletes()).toEqual(['/api/channels/ch-1'])

    press('o')
    await flush(w)
    const tabs = Array.from(w.element.querySelectorAll<HTMLElement>('[data-panel]'))
    expect(tabs.map((t) => t.dataset.panel), 'o 回到缺省的 git，不是原来的多面板').toEqual(['git'])
  })
})

describe('面板坞：窄列不溢出（412px）', () => {
  it('标签条会换行且允许收缩，三面板加候选不会横向溢出', () => {
    // 源文本断言：vitest 不处理 CSS，只能读源（`readRepoFile` 的先例）。
    // 这条锁的是 v0.3.0 刚修过的同一类问题（`.af-compose__target` 的
    // `min-width: 0`）：三面板 + 「＋」候选在 412px 窄列里必然超宽。
    const css = readRepoFile('packages/web/src/styles/components.css')
    const tabs = cssRuleOf(css, '.af-docktabs')
    expect(tabs, '标签条规则必须存在').not.toBe('')

    // 换行：超宽时往下排，而不是把横向撑开。
    expect(tabs, '标签条必须允许换行').toContain('flex-wrap: wrap')
    // 收缩：作为 flex 子项不被内容撑开。
    expect(tabs, '标签条必须能收缩到容器宽度').toContain('min-width: 0')

    // 「＋」与候选都是一行文字，不能自己折字（折字比换行难读）。
    const add = cssRuleOf(css, '.af-docktabs__add')
    expect(add, '「＋」规则必须存在').not.toBe('')
    expect(add).toContain('white-space: nowrap')
  })
})

/* ===========================================================================
   v0.4.0 子任务 5：git 面板的 status 与 diff 视图
   ---------------------------------------------------------------------------
   这一组锁的是「设备回的帧 → 用户看得懂的界面」这一段。帧形状以
   `crates/node-core/src/connector/git.rs` 的回帧为准（那边有对应的单元测试）。
   =========================================================================== */
describe('git 面板：status 与 diff', () => {
  it('status 回帧渲染出分支与文件列表，两列状态码原样显示', async () => {
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]

    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [
        { x: ' ', y: 'M', path: 'src/a.rs' },
        { x: 'A', y: ' ', path: 'src/b.rs' },
        { x: '?', y: '?', path: 'notes.md' },
      ],
    })

    expect(w.element.querySelector('[data-git-branch]')?.textContent).toContain('main')
    expect(w.element.querySelector('[data-git-branch]')?.textContent).toContain('a1b2c3d')

    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('[data-git-file]'))
    expect(rows.map((r) => r.dataset.gitFile)).toEqual(['src/a.rs', 'src/b.rs', 'notes.md'])
    // 状态码**不翻译**：`M` / `A` / `??` 是 git 用户的共同词汇。
    const codes = rows.map((r) => r.querySelector('.af-git__code')?.textContent)
    expect(codes).toEqual([' M', 'A ', '??'])
    expect(fetchMock).toHaveBeenCalled()
  })

  it('clean:true 显示「没有未提交的改动」，不是空列表', async () => {
    // §3.12：空列表读起来是「没读到」，而这是一条**结论**。
    const { w } = await mountWorkbench()
    await interact(w)
    await frame(w, FakeEventSource.instances[0], {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: true,
      files: [],
    })

    const clean = w.element.querySelector('[data-git-clean]')
    expect(clean, '干净时必须有一句明写的结论').not.toBeNull()
    expect(clean?.textContent).toContain('没有未提交的改动')
    expect(w.element.querySelector('[data-git-file]')).toBeNull()
  })

  it('有改动时头部徽标写「N 个改动」，与文件条数一致；干净时没有这个徽标', async () => {
    // 这个徽标（`data-git-dirty`）是全仓最后一个零断言的 git 锚点：把
    // `v-if="clean === false"` 改成恒 false、或把 `files.length` 写成常量，
    // 770 条测试曾经全绿。判据取「文案里的数字 == 文件条数」，不写死 3——
    // 写死数字的话，将来改样本仍绿而徽标可能已经算错。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    const files = [
      { x: ' ', y: 'M', path: 'src/a.rs' },
      { x: 'A', y: ' ', path: 'src/b.rs' },
      { x: '?', y: '?', path: 'notes.md' },
    ]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files,
    })

    const dirty = w.element.querySelector<HTMLElement>('[data-git-dirty]')
    expect(dirty, 'clean === false 时头部必须有改动数徽标').not.toBeNull()
    const rows = w.element.querySelectorAll('[data-git-file]').length
    expect(rows, '文件行数与样本条数一致（否则下面的数字比对没有意义）').toBe(files.length)
    expect(dirty?.textContent?.trim()).toBe(`${files.length} 个改动`)

    // 反方向：干净时这个徽标不该出现（`clean:true` 有自己的结论句）。
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: true,
      files: [],
    })
    expect(w.element.querySelector('[data-git-dirty]'), '干净时不该有改动数徽标').toBeNull()
  })

  it('不是 git 仓库时给中性说明，且不出现错误样式', async () => {
    // 「不是仓库」是**正常状态**，设备侧因此回一帧普通 `status` 带
    // `not_a_repo:true`，而不走错误路径（见 `connector/git.rs` 的
    // `status_outside_a_repo_is_a_normal_frame_saying_so`）。走错误路径的话
    // 通道层只把错误码传出来，页面分不清「不是仓库」和「git 挂了」。
    const { w } = await mountWorkbench()
    await interact(w)
    await frame(w, FakeEventSource.instances[0], {
      op: 'status',
      not_a_repo: true,
      branch: null,
      head: null,
      detached: false,
      clean: null,
      files: [],
    })

    const voidEl = w.element.querySelector<HTMLElement>('[data-git-not-a-repo]')
    expect(voidEl, '非仓库要说出来，不能画成空列表').not.toBeNull()
    expect(voidEl?.textContent).toContain('不是 git 仓库')
    // 不报红、不当错误：这里没有问题要修。
    expect(w.element.querySelector('[role="alert"]')).toBeNull()
    expect(w.element.querySelector('.af-danger')).toBeNull()
    // `clean:null` 也不能被渲染成「干净」或「加载中」——那两句在非仓库上是错的。
    expect(w.element.querySelector('[data-git-clean]')).toBeNull()
    expect(w.element.querySelector('[data-git-loading]')).toBeNull()
    // 分支行与刷新按钮照旧在（用户还能刷新一次，万一刚 `git init`）。
    expect(w.element.querySelector('[data-git-refresh]')).not.toBeNull()
  })

  it('重命名文件点开的 diff 请求要带上旧路径 orig', async () => {
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      // 重命名：`orig` 是旧路径，页面只**显示** `新 <- 旧`。
      files: [{ x: 'R', y: ' ', path: 'src/b.rs', orig: 'src/a.rs' }],
    })

    await w.element.querySelector<HTMLElement>('[data-git-file="src/b.rs"]')?.click()
    await flush(w)

    const sent = sentBodies(fetchMock)
    const diffReq = reqOf(sent, 'diff')
    // `path` 必须是那条真实路径——曾经的 bug 是把它拼成 `src/b.rs <- src/a.rs`
    // 原样发出，设备侧拿它去 `git diff` 得到空串，页面显示「没有可显示的改动」。
    expect(diffReq.path).toBe('src/b.rs')
    // `orig` 必须一起发：git 的 rename 检测要 pathspec 里同时有新旧两条路径，
    // 只给新路径时 git 判成「新建文件」，页面于是把改名显示成「新建了文件」。
    expect(diffReq.orig).toBe('src/a.rs')

    // 列表里给用户看的仍是「新 <- 旧」，请求标识不因此改变。
    const row = w.element.querySelector<HTMLElement>('[data-git-file="src/b.rs"]')
    expect(row?.textContent).toContain('src/a.rs')
  })

  it('点文件发 {"op":"diff","path":…} 并渲染 diff', async () => {
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ x: ' ', y: 'M', path: 'src/a.rs' }],
    })

    await w.element.querySelector<HTMLElement>('[data-git-file="src/a.rs"]')?.click()
    await flush(w)

    // 发出去的 body 必须带 `path`——不带的话设备回的是「工作区全部改动」，
    // 而用户点的是某一个文件。
    const sent = sentBodies(fetchMock)
    const diffReq = reqOf(sent, 'diff')
    expect(diffReq.path).toBe('src/a.rs')

    await frame(w, src, {
      op: 'diff',
      path: 'src/a.rs',
      unified: '--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n one\n+two\n',
      truncated: false,
    })

    const body = w.element.querySelector('[data-git-diff-body]')
    expect(body, 'diff 正文要渲染出来').not.toBeNull()
    expect(body?.textContent).toContain('+two')
    // 文件头不染成增删色：`+++` / `---` 是头，不是改动行。
    const kinds = Array.from(
      w.element.querySelectorAll<HTMLElement>('.af-gdiff__line'),
    ).map((el) => `${el.dataset.kind}:${el.textContent}`)
    expect(kinds[0]).toBe('meta:--- a/src/a.rs')
    expect(kinds[1]).toBe('meta:+++ b/src/a.rs')
    expect(kinds.some((k) => k.startsWith('add:'))).toBe(true)
  })

  it('truncated:true 时显式提示已截断，不静默', async () => {
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ x: ' ', y: 'M', path: 'big.txt' }],
    })
    await frame(w, src, {
      op: 'diff',
      path: 'big.txt',
      unified: '+line\n… (diff truncated by the device)\n',
      truncated: true,
    })

    const note = w.element.querySelector('[data-git-diff-truncated]')
    expect(note, '截断必须说出来').not.toBeNull()
    expect(note?.textContent).toContain('截断')
  })

  it('detached HEAD 写出来，不留空白', async () => {
    // 空白会被读成「还在加载」。
    const { w } = await mountWorkbench()
    await interact(w)
    await frame(w, FakeEventSource.instances[0], {
      op: 'status',
      branch: null,
      head: 'deadbee',
      detached: true,
      clean: true,
      files: [],
    })

    const branch = w.element.querySelector('[data-git-branch]')?.textContent ?? ''
    expect(branch).toContain('detached')
    expect(branch).toContain('deadbee')
  })

  it('通道 ready 后自动问一次 status（挂上就问，不是等用户点刷新）', async () => {
    // 与 filesKick 同一条：不问的话用户看到的是一块「还没问过」的空壳，
    // 而通道明明是好的——两种空被读反（§3.12）。
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    await flush(w)

    const sent = sentBodies(fetchMock)
    expect(
      sent.filter((b) => b.op === 'status').length,
      `面板一 ready 就该问 status，实发：${JSON.stringify(sent)}`,
    ).toBe(1)
  })
})

/* ===========================================================================
   v0.4.0 子任务 6：历史提交（log）分段
   ---------------------------------------------------------------------------
   帧形状以 `crates/node-core/src/connector/git.rs` 的 `log` 回帧为准：
   `{ op:'log', commits:[{hash,author,when,subject}], truncated }`。
   =========================================================================== */
describe('git 面板：历史分段', () => {
  /**
   * 钉住「现在」：相对时间那一列的输出取决于当前时钟，不钉死的话
   * 「3 小时前」会随时间漂成「4 小时前」，用例会在某天突然红。
   */
  const NOW_MS = Date.parse('2026-10-29T13:00:00+00:00')
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    vi.setSystemTime(NOW_MS)
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  /** 切到「历史」分段。 */
  async function toLog(w: ReturnType<typeof mountIn>): Promise<void> {
    await w.element.querySelector<HTMLElement>('[data-git-seg="log"]')?.click()
    await flush(w)
  }

  const COMMIT_A = {
    hash: 'a'.repeat(40),
    author: 'Ann',
    when: '2026-10-29T10:00:00+00:00',
    subject: 'feat: 加 A',
  }
  const COMMIT_B = {
    hash: 'b'.repeat(40),
    author: 'Bob',
    when: '2026-10-28T10:00:00+00:00',
    subject: 'fix: 改 B',
  }

  it('切到历史发 {"op":"log","limit":30} 并渲染提交列表', async () => {
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    await toLog(w)

    const logReq = sentBodies(fetchMock).find((b) => b.op === 'log')
    expect(logReq, `切历史时要发 log 请求，实发：${JSON.stringify(sentBodies(fetchMock))}`).toBeTruthy()
    // `limit` 必须真的发出去：不发的话设备用它的缺省值，而页面写着「只显示最近的」——
    // 两处各有一份上限，迟早对不上。
    expect(logReq?.limit).toBe(30)

    await frame(w, FakeEventSource.instances[0], {
      op: 'log',
      commits: [COMMIT_A, COMMIT_B],
      truncated: false,
    })

    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('[data-git-commit]'))
    // 顺序原样保留：git 给的就是新→旧，重排会让「最近改了什么」变成要算的问题。
    expect(rows.map((r) => r.dataset.gitCommit)).toEqual([COMMIT_A.hash, COMMIT_B.hash])
    expect(rows[0]?.textContent).toContain('aaaaaaa')
    expect(rows[0]?.textContent).toContain('Ann')
    expect(rows[0]?.textContent).toContain('feat: 加 A')

    // 第 4 列是**相对时间**，早先这条用例只断言了前三列，于是 `relative()`
    // 整个函数（六个档位）在全仓 0 断言覆盖：把 `'刚刚'` 改成别的东西都不红。
    // 用 `vi.setSystemTime` 钉住「现在」，输出才是确定的。
    const when = rows[0]?.querySelector<HTMLElement>('.af-glog__when')
    expect(when, '提交行要有相对时间那一列').not.toBeNull()
    expect(when?.textContent?.trim(), 'COMMIT_A 是 3 小时前').toBe('3 小时前')
  })

  it('取某条提交的 diff 时那一行带「读取中」，别的行不带', async () => {
    // `af-glog__busy` 是用户点完到回帧之间**唯一**的反馈：列表不动、什么都没有
    // 的话，用户会以为点击没生效从而再点一次。早先它在全仓 0 断言。
    const { w } = await mountWorkbench()
    await interact(w)
    await toLog(w)
    await frame(w, FakeEventSource.instances[0], {
      op: 'log',
      commits: [COMMIT_A, COMMIT_B],
      truncated: false,
    })

    await w.element
      .querySelector<HTMLElement>(`[data-git-commit="${COMMIT_A.hash}"]`)
      ?.click()
    await flush(w)

    const busy = Array.from(w.element.querySelectorAll<HTMLElement>('[data-git-commit]'))
      .filter((r) => r.querySelector('.af-glog__busy') !== null)
      .map((r) => r.dataset.gitCommit)
    // 只标被点的那一条：两条都标的话用户分不出在等哪一份。
    expect(busy).toEqual([COMMIT_A.hash])
  })

  it('切到历史分段会清掉上一轮选中的提交，不留「高亮了但正文说没选」的矛盾', async () => {
    // 这条守的是 `gitShowLog` 里的 `ws.git.logSelected = null`。
    //
    // 不清的后果是**同一屏自相矛盾**：`GitLog` 按 `aria-selected` 高亮上一轮
    // 选中的那行（说「你选了这条」），而 `diff` 已被清成 `null`、右栏写「点
    // 左边任一提交看它改了什么」（说「你还没选」）。两个提示指向两个事实。
    //
    // 判据用 `aria-selected` 而不是「有没有 .af-glog__busy」：后者是「请求在飞」
    // 的瞬时态，与「这份 diff 属于哪条提交」不是一回事。
    const { w } = await mountWorkbench()
    await interact(w)
    await toLog(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, { op: 'log', commits: [COMMIT_A, COMMIT_B], truncated: false })

    await w.element.querySelector<HTMLElement>(`[data-git-commit="${COMMIT_A.hash}"]`)?.click()
    await flush(w)
    // 先确认「选中」确实发生了，否则下面那句「清空了」在任何输入下都成立。
    const selectedBefore = Array.from(
      w.element.querySelectorAll<HTMLElement>('[data-git-commit][aria-selected="true"]'),
    ).map((r) => r.dataset.gitCommit)
    expect(selectedBefore, '点了提交之后那一行必须是选中态（否则后面的断言恒真）').toEqual([
      COMMIT_A.hash,
    ])

    // 走真实路径：先切到「改动」分段，再切回「历史」。
    //
    // **不能在同一个分段上再点一次「历史」**：`GitPanel.selectSegment` 在
    // `segment === next` 时提前 return、不 emit，所以那样点不会调 `gitShowLog`。
    // 真机上这条路径也确实存在（看改动 → 回历史，列表还在，只是 diff 没了）。
    await w.element.querySelector<HTMLElement>('[data-git-seg="changes"]')?.click()
    await flush(w)
    await toLog(w)

    const selectedAfter = Array.from(
      w.element.querySelectorAll<HTMLElement>('[data-git-commit][aria-selected="true"]'),
    ).map((r) => r.dataset.gitCommit)
    expect(selectedAfter, '切到历史分段后不该还有行是高亮的').toEqual([])
  })

  it('看过提交的 diff 后切回改动分段：右栏不得残留那条提交的改动', async () => {
    // 这条守的是 `gitShowChanges`（`GitPanel` 的 `changes` 一侧 emit）。
    //
    // 两个分段共用页面上的同一份 `ws.git.diff`。只清「进入历史」那一侧的话，
    // 切回改动后左栏是**工作区**文件列表、一行都没高亮，右栏却还是**上一条
    // 提交**的 diff——同一屏两个提示指向两个事实。
    //
    // **必须先铺一帧 status**：不铺的话改动分段是「还没问过」的空态，右栏被
    // 空态整块遮住，泄漏看不出来（这正是它逃过前几轮的原因）。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ path: 'worktree.txt', x: 'M', y: ' ' }],
      not_a_repo: false,
    })
    await toLog(w)
    await frame(w, src, { op: 'log', commits: [COMMIT_A, COMMIT_B], truncated: false })

    await w.element
      .querySelector<HTMLElement>(`[data-git-commit="${COMMIT_A.hash}"]`)
      ?.click()
    await flush(w)
    await frame(w, src, {
      op: 'diff',
      path: 'first.txt',
      unified: '--- a/first.txt\n+++ b/first.txt\n@@ -1 +1,2 @@\n one\n+COMMIT_ONLY\n',
      truncated: false,
    })
    // 先确认这条提交的改动**确实渲染出来了**，否则下面那句「没有残留」恒真。
    const before = w.element.querySelector('[data-git-diff-body]')?.textContent ?? ''
    expect(before, '点提交后右栏必须显示该提交的 diff（否则后面的断言恒真）').toContain(
      'COMMIT_ONLY',
    )

    // 走真实路径：点「改动」分段。
    await w.element.querySelector<HTMLElement>('[data-git-seg="changes"]')?.click()
    await flush(w)

    const after = w.element.querySelector('[data-git-diff-body]')?.textContent ?? ''
    expect(after, '切回改动分段后右栏不得残留提交的 diff').not.toContain('COMMIT_ONLY')
    // 顺带钉住「改动分段确实是改动分段」：左栏是工作区那一行。
    expect(w.element.querySelector('[data-git-file]')?.getAttribute('data-git-file')).toBe(
      'worktree.txt',
    )
  })

  it('分段来回切不会重复问 log：已经拿到列表就复用（问一次就够）', async () => {
    // 语义未定就写不出用例，于是 `gitShowLog` 一直是「每切一次发一次 log」而
    // 没人钉住它。这里把语义定下来：**列表已经拿到就不重发**。
    //
    // 理由与 `GitPanel` 那条懒建立的注释同源——「不要替用户做他没要的请求」；
    // 而「改动 ↔ 历史」来回切在一次排查里会点很多次。
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ path: 'a.txt', x: 'M', y: ' ' }],
      not_a_repo: false,
    })
    const logCount = (): number => sentBodies(fetchMock).filter((b) => b.op === 'log').length

    await toLog(w)
    await frame(w, src, { op: 'log', commits: [COMMIT_A, COMMIT_B], truncated: false })
    expect(logCount(), '第一次进历史要问一次').toBe(1)

    // 回改动，再进历史：列表已在手，不该再问。
    await w.element.querySelector<HTMLElement>('[data-git-seg="changes"]')?.click()
    await flush(w)
    await toLog(w)
    expect(logCount(), '列表已拿到时再进历史不该重发 log').toBe(1)
  })

  it('有 head 但没有分支名时，头一行要同时写出「分支未知」与那个 head', async () => {
    // 守 `branchLine` 的最后一档。旧写法是 `return props.head ?? '（分支未知）'`：
    // head 非空时它**只回 head**，于是头一行变成孤零零一个提交号，看不出
    // 「这台机器报不出分支名」——用户会以为那就是分支本身。
    //
    // 判据必须是**两样都在**：只断言「含 deadbee」的话旧写法照样过（旧写法
    // 在 head 非空时正是回 head，实测把新写法改回旧的仍绿）。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: null,
      head: 'deadbee',
      detached: false,
      clean: true,
      files: [],
      not_a_repo: false,
    })
    const line = w.element.querySelector('[data-git-branch]')?.textContent ?? ''
    expect(line, '已知的 head 要写出来').toContain('deadbee')
    expect(line, '也要说明「分支名未知」，否则那个 head 会被当成分支名').toContain('分支未知')
  })

  it('点第二个文件时，右栏给的是「正在读取这一份 diff」而不是把上一份当结论', async () => {
    // 守两处 `:stale`（`GitPanel.vue` 的 `reading !== null` /
    // `logReading !== null`）。它是「这份 diff 是上一份、新请求还在飞」的
    // 唯一显式提示——此前 `grep stale` 在测试里 0 命中，改成 `false` 全量
    // 772 条照样绿。
    //
    // 路径要真的**换一个文件**：同一个文件再点一次会被 `reading` 判据挡在
    // 外面（页面自己不清），测不到这份提示。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [
        { path: 'a.txt', x: 'M', y: ' ' },
        { path: 'b.txt', x: 'M', y: ' ' },
      ],
      not_a_repo: false,
    })

    // 先看 a.txt 的 diff（让它成为「上一份」）
    w.element.querySelector<HTMLElement>('[data-git-file="a.txt"]')?.click()
    await flush(w)
    await frame(w, src, {
      op: 'diff',
      path: 'a.txt',
      unified: '--- a/a.txt\n+++ b/a.txt\n@@ -1 +1,2 @@\n one\n+FROM_A\n',
      truncated: false,
    })
    const pane = (): string =>
      w.element.querySelector('.af-git__diffpane')?.textContent ?? ''
    expect(pane(), '先得有 a.txt 的 diff 当「上一份」').toContain('FROM_A')

    // 点 b.txt：新请求在飞，右栏必须明说「这一份是旧的」
    w.element.querySelector<HTMLElement>('[data-git-file="b.txt"]')?.click()
    await flush(w)
    expect(pane(), '新请求在飞时要提示「正在读取这一份 diff」').toContain('正在读取这一份 diff')
    // 而且不能把 a.txt 的结论当 b.txt 的结论展示（`stale` 时 `lines` 为空）
    expect(pane(), 'stale 时不该还在展示上一份的正文').not.toContain('FROM_A')
  })

  it('历史分段点下一条提交时，右栏立刻切到「正在读取这条提交的改动…」', async () => {
    // 历史侧**没有** `stale` 那一档，因为 `gitOpenCommit` 发请求前就把
    // `ws.git.diff` 清成 `null`——`<GitDiff>` 只在已经拿到某条提交的 diff
    // 之后才渲染，那时不存在「上一份」可言。（早先那里挂着
    // `:stale="logReading !== null"`，是走不到的死判据，已删。）
    //
    // 所以要守的是**清空之后**那句提示：右栏不能还挂着上一条提交的正文，
    // 也不能退回「点左边任一提交看它改了什么」（那说的是「你还没选」，而
    // 用户刚点了）。
    const { w } = await mountWorkbench()
    await interact(w)
    await toLog(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, { op: 'log', commits: [COMMIT_A, COMMIT_B], truncated: false })

    await w.element
      .querySelector<HTMLElement>(`[data-git-commit="${COMMIT_A.hash}"]`)
      ?.click()
    await flush(w)
    await frame(w, src, {
      op: 'diff',
      path: 'first.txt',
      unified: '--- a/first.txt\n+++ b/first.txt\n@@ -1 +1,2 @@\n one\n+FROM_COMMIT_A\n',
      truncated: false,
    })
    const pane = (): string => w.element.querySelector('.af-git__diffpane')?.textContent ?? ''
    expect(pane(), '先得有 A 的 diff 当「上一份」').toContain('FROM_COMMIT_A')

    // 点 B：请求在飞，右栏要明说正在读**这一条**。
    await w.element
      .querySelector<HTMLElement>(`[data-git-commit="${COMMIT_B.hash}"]`)
      ?.click()
    await flush(w)
    expect(pane(), '上一条提交的正文不得残留在 B 的请求期间').not.toContain('FROM_COMMIT_A')
    expect(pane(), '点了提交之后不能退回「你还没选」那句').not.toContain('点左边任一提交')
    expect(pane(), '要明说正在读这一条提交的改动').toContain('正在读取这条提交的改动')
  })

  it('刷新按钮再问一次 status（那条 0 覆盖的 emit 通路）', async () => {
    // 守 `GitPanel.vue` 的 `@click="emit('refresh')"`。改坏它（改成 `undefined`）
    // 时全量 772 条照样绿——说明此前没人在看这条通路。
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: true,
      files: [],
      not_a_repo: false,
    })
    const before = sentBodies(fetchMock).filter((b) => b.op === 'status').length

    const btn = w.element.querySelector<HTMLElement>('[data-git-refresh]')
    expect(btn, '头一行必须常驻刷新按钮（非仓库时也一样）').toBeTruthy()
    btn?.click()
    await flush(w)

    const after = sentBodies(fetchMock).filter((b) => b.op === 'status').length
    expect(after, '点刷新必须再发一次 {"op":"status"}').toBe(before + 1)
  })

  it('分段条上按左右方向键能换分段（`onSegKey` 的通路）', async () => {
    // 守 `onSegKey`：改坏它（首行插 `return`）时全量 772 条照样绿。
    // `role="tablist"` 写了就得让方向键能用，否则屏幕阅读器报「无反应」。
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: true,
      files: [],
      not_a_repo: false,
    })
    const bar = w.element.querySelector<HTMLElement>('[data-git-panel] [role="tablist"]')
    expect(bar, '分段条要有 role="tablist"').toBeTruthy()

    bar?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }))
    await flush(w)
    const logs = sentBodies(fetchMock).filter((b) => b.op === 'log').length
    expect(logs, 'ArrowRight 要切到历史并发一次 log').toBe(1)

    bar?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }))
    await flush(w)
    // 回到改动分段：右栏空态，历史列表消失。
    expect(
      w.element.querySelector('[data-git-commit]'),
      'ArrowLeft 要切回改动分段（历史列表不再渲染）',
    ).toBeNull()
  })

  it('「还没问过」与「正在读取 diff」两句空态文案都要真的出现', async () => {
    // 两处都是「空态」，而空态写错/写没正是最容易被读成「还在加载」的地方。
    // 前者判据 `clean === null`、后者判据 `!diff && reading`。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]

    // ① 一帧 status 都还没回：`clean === null`。
    expect(
      w.element.querySelector('[data-git-loading]')?.textContent ?? '',
      '没问过 status 时要说「正在读取工作区状态」',
    ).toContain('正在读取工作区状态')

    // ② 有一份改动列表，点一个文件但 diff 还没回来：`!diff && reading`。
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ path: 'a.txt', x: 'M', y: ' ' }],
      not_a_repo: false,
    })
    w.element.querySelector<HTMLElement>('[data-git-file="a.txt"]')?.click()
    await flush(w)
    expect(
      w.element.querySelector('[data-git-diffpane], .af-git__diffpane')?.textContent ?? '',
      '点了文件但 diff 没回来时要说「正在读取 diff…」',
    ).toContain('正在读取 diff')
    // 反向：这一刻不该出现「点左边任一文件」那句（那说的是「你还没选」）。
    expect(w.element.querySelector('[data-git-no-diff]'), '读取中不能同时说「你还没选」').toBeNull()
  })

  it('git 请求失败时错误横幅要显示出原因（`gitRequest` 的 onError）', async () => {
    // `data-git-error` 是面板唯一的错误出口。静默失败 = 一块永远不动的面板。
    //
    // 用通道帧 `{"op":"error","code":…,"reason":…}` 走真实路径：Hub 把
    // `channel_error` 重发成这一帧（见 `ws.rs` 的 `publish_channel_error`）。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ path: 'a.txt', x: 'M', y: ' ' }],
      not_a_repo: false,
    })

    // 触发一次会失败的请求：发 diff 后回一条错误帧。
    w.element.querySelector<HTMLElement>('[data-git-file="a.txt"]')?.click()
    await flush(w)
    await frame(w, src, { op: 'error', code: 'internal', reason: 'git exploded' })
    await flush(w)

    const banner = w.element.querySelector('[data-git-error]')
    expect(banner, '必须有一个错误横幅（静默失败就是一块不动的面板）').toBeTruthy()
    expect(banner?.getAttribute('role'), '错误横幅要 role="alert"').toBe('alert')
    // 优先显示 `reason`（设备说的人话），没有才退回通用句——所以这里断言
    // reason 原样出现。只断言「横幅非空」的话，写死一句「出错了」也能过。
    expect(banner?.textContent ?? '', '要把设备给的 reason 原样显示出来').toContain(
      'git exploded',
    )
    // 读取中的标记必须被清掉：不清的话「正在读取 diff…」与错误横幅会同时挂着。
    expect(
      w.element.querySelector('.af-git__diffpane')?.textContent ?? '',
      '出错误之后不能再挂着「正在读取 diff…」',
    ).not.toContain('正在读取 diff')
  })

  it('「不是 git 仓库」时上一台的字段必须被清空，不能在切回仓库时冒出来', async () => {
    // 用户可能刚在另一个 tab 是仓库、切过来这台不是——残留的旧分支名会被
    // 当成这台的分支。`applyGitStatus` 的 `notARepo` 分支清了 7 个字段。
    //
    // **这条用例守不住那 7 行清理，如实记在这里**：非仓库时模板里那 7 个
    // 字段一个都渲染不出来——`GitPanel` 的「不是 git 仓库」块是
    // `v-if="notARepo"`，内容区整块在 `v-else` 里。实测把 7 行清理整段删掉，
    // 本文件 784 条用例全绿（含本用例）。所以下面那两条「非仓库下不该有
    // diff 正文 / 文件行」**在删掉清理后同样成立**——它们查不到东西是因为
    // 内容区被 `v-if` 换掉了，不是因为清理生效了，对清理本身零检测力。
    //
    // 那 7 行仍然保留：它是切 tab 时防止旧值残留的正确性保障，只是**只能靠
    // 源码契约**（读者对着 `applyGitStatus` 核对），没有测试出口。删掉这条
    // 用例里的假断言，留下真正有效的那部分：一帧 `not_a_repo` 必须让面板
    // 走到「不是 git 仓库」这一支，而不是继续显示上一台的仓库内容。
    const { w } = await mountWorkbench()
    await interact(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, {
      op: 'status',
      branch: 'main',
      head: 'a1b2c3d',
      detached: false,
      clean: false,
      files: [{ path: 'a.txt', x: 'M', y: ' ' }],
      not_a_repo: false,
    })
    expect(w.element.querySelector('[data-git-file="a.txt"]'), '先得有文件行').toBeTruthy()

    await frame(w, src, {
      op: 'status',
      branch: null,
      head: null,
      detached: false,
      clean: null,
      files: [],
      not_a_repo: true,
    })
    await flush(w)
    expect(
      w.element.querySelector('[data-git-not-a-repo]'),
      '非仓库要走「不是 git 仓库」那一支',
    ).toBeTruthy()
    expect(w.element.querySelector('[data-git-branch]')?.textContent ?? '').toContain(
      '不是 git 仓库',
    )
    // 非仓库帧之后，上一台的改动列表必须不再出现在面板上（走到 `v-if` 那一支
    // 就必然满足；这条守的是「非仓库帧真的被处理了」，不是那 7 行清理）。
    expect(w.element.querySelector('[data-git-file]'), '非仓库下不该有文件行').toBeNull()
  })

  it('点某条提交发 {"op":"diff","commit":"<完整 hash>"} 并渲染该提交的改动', async () => {
    // 这条要能抓到「取错基线」：只断言「渲染非空」的话，发 `path` 拿到工作区
    // 改动也会通过。所以断言**内容只含该提交改过的文件**。
    const { w, fetchMock } = await mountWorkbench()
    await interact(w)
    await toLog(w)
    const src = FakeEventSource.instances[0]
    await frame(w, src, { op: 'log', commits: [COMMIT_A, COMMIT_B], truncated: false })

    await w.element
      .querySelector<HTMLElement>(`[data-git-commit="${COMMIT_A.hash}"]`)
      ?.click()
    await flush(w)

    const diffReq = sentBodies(fetchMock).find((b) => b.op === 'diff')
    expect(diffReq, '点提交要发 diff 请求').toBeTruthy()
    // **必须是完整 hash**：短 hash 在歧义时会指向另一条提交，而 `git show`
    // 接受短 hash——静默取错比报错难查得多。
    expect(diffReq?.commit).toBe(COMMIT_A.hash)
    // 不能带 `path`：那是「工作区未提交改动」的取法，与点的这条提交无关。
    expect(diffReq?.path).toBeUndefined()

    await frame(w, src, {
      op: 'diff',
      path: 'src/a.rs',
      unified: '--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n one\n+A_TWO\n',
      truncated: false,
    })

    const body = w.element.querySelector('[data-git-diff-body]')
    expect(body, '该提交的 diff 要渲染出来').not.toBeNull()
    expect(body?.textContent).toContain('A_TWO')
    // 另一条提交改过的文件不该出现在这里——出现就说明取错了基线。
    expect(body?.textContent).not.toContain('B_ONLY')
  })

  it('log 的 truncated:true 时明说更早的没有列出', async () => {
    const { w } = await mountWorkbench()
    await interact(w)
    await toLog(w)
    await frame(w, FakeEventSource.instances[0], {
      op: 'log',
      commits: [COMMIT_A],
      truncated: true,
    })

    const note = w.element.querySelector('[data-git-log-truncated]')
    expect(note, '只给了一部分就必须说出来').not.toBeNull()
    expect(note?.textContent).toContain('更早')
  })

  it('空仓库（commits 为空）写「还没有提交」，与「还没问」分开', async () => {
    // §3.12：空列表是结论，`null` 才是「还没问」。混成一档的话新建仓库上
    // 会一直写「正在读取提交历史…」。
    const { w } = await mountWorkbench()
    await interact(w)
    await toLog(w)

    // 还没回帧：是「正在读取」。
    expect(w.element.querySelector('[data-git-log-loading]')).not.toBeNull()

    await frame(w, FakeEventSource.instances[0], { op: 'log', commits: [], truncated: false })
    expect(w.element.querySelector('[data-git-log-loading]')).toBeNull()
    const empty = w.element.querySelector('[data-git-log-empty]')
    expect(empty, '空仓库要有明确结论').not.toBeNull()
    expect(empty?.textContent).toContain('还没有提交')
  })
})
