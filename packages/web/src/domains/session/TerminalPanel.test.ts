/* ===========================================================================
   TerminalPanel 的测试（里程碑 13b）
   ---------------------------------------------------------------------------
   三条测试标准**全部前端可独立完成**（里程碑 13b 的依赖段明确写了「刻意不排
   后端 PTY 子任务」）：mock 字节流渲染、resize 一致性、断线三态切换。
   组件不发请求、不持通道是它的设计前提（见组件头注释），所以这里喂的是
   **与 `terminal.rs` 载荷同形状**的字节（`{"bytes": "..."}` 里的那段字符串），
   而不是另一套测试专用形状。

   ── 这里跑的是**真 xterm.js**，不是 mock 的 Terminal ──────────────────
   本子任务的存在理由就是「xterm 的真实 ANSI 行为没有验证过」（§8.2 第 1 条）。
   把 Terminal 换成一个记录调用的假对象，三条标准里最要紧的那一条
   （跨帧切分的控制序列、CJK 列宽）就变成「对自己的断言」——那种测试绿了
   也不说明任何事。所以本文件只在**环境层**补 happy-dom 缺的两样东西：

     · 字符度量：`offsetWidth/offsetHeight`（happy-dom 不做排版，恒 0，
       于是 FitAddon 量不出字符宽高）；
     · `.xterm` 的 padding：`getComputedStyle` 在 happy-dom 里取不到
       xterm.css 的值，FitAddon 会算出 NaN。

   补的都是「浏览器本来就该有的输入」；被测的解析行为完全是真的。

   ── 断言读的是 xterm 自己算出来的东西 ────────────────────────────────
   渲染结果在 `.xterm-rows` 里，列宽由 xterm 的**行填充**体现（它把每一行
   补空格补到 `cols`，所以「这一行用了几列」= `cols - 行尾空格数`）。
   读这些而不是读组件的内部字段，等于断言「画面上有什么」，
   而不是「我们打算画什么」。
   =========================================================================== */

import { Terminal } from '@xterm/xterm'
import { afterAll, describe, expect, it } from 'vitest'

import TerminalPanel, {
  type TerminalChannel,
  type TerminalConn,
} from '@/domains/session/TerminalPanel.vue'
import { cssRuleOf, readRepoFile } from '@/test/css'
import { mountIn } from '@/test/mount'

/* ---------------------------------------------------------------- 环境补齐 */

/** 字符的像素宽度：12px 等宽（与组件里 `.af-term` 的字号一致）。 */
const CHAR_W = 9
/**
 * 字符的像素高度：**14**，不是「12 × 1.35 = 16.2」取整得的 18。
 *
 * 这不是凑数，是把 happy-dom 缺的那一次**真实排版**还回去：xterm 量字符
 * 高度用的是 `fontBoundingBoxAscent + fontBoundingBoxDescent`（`measureText`
 * 的字体盒），在 12px 字号下真浏览器给出的就是 14px。xterm 随后自己再乘
 * `lineHeight` 得出**行高** `floor(14 × 1.35) = 18`。
 *
 * 也就是说「字高 14 / 行高 18」是**两级**，而 `18` 只出现在第二级。若这里
 * 直接把字高写成 18，行高就变成 `floor(18 × 1.35) = 24`，行数按 24 算——
 * 那是用我们自己造的第二级去喂第二级。
 */
const CHAR_H = 14
/** 行高：xterm 按 `floor(字高 × lineHeight)` 自己算出来的，与组件 `lineHeight: 1.35` 一致。 */
const CELL_H = Math.floor(CHAR_H * 1.35)
/** xterm 的量测元素是 32 个字符宽。 */
const CHAR_MEASURE_W = CHAR_W * 32
/** FitAddon 给滚动条留的宽度（`scrollback > 0` 时的缺省值）。 */
const RULER_W = 14

const originals: Array<PropertyDescriptor | undefined> = []

function swap(prop: 'offsetWidth' | 'offsetHeight', value: (el: HTMLElement) => number): void {
  originals.push(Object.getOwnPropertyDescriptor(HTMLElement.prototype, prop))
  Object.defineProperty(HTMLElement.prototype, prop, {
    configurable: true,
    get(this: HTMLElement): number {
      return value(this)
    },
  })
}

/**
 * 补上 happy-dom 不做排版这件事。
 *
 * 只给一类元素真值：xterm 的 32 字符量测元素（决定 `CharSize`）。
 * 面板可视区的尺寸由每个用例自己写在元素上（`sized()`），因为「多宽」
 * 是被测的输入，不是环境常量。其余元素一律 0 —— 那是 happy-dom 的本来行为。
 */
swap('offsetWidth', (el) =>
  String(el.className ?? '').includes('xterm-char-measure-element') ? CHAR_MEASURE_W : 0,
)
swap('offsetHeight', (el) =>
  String(el.className ?? '').includes('xterm-char-measure-element') ? CHAR_H : 0,
)

/**
 * 组件建出来的 xterm 实例，按挂载实例记一份。
 *
 * 组件只暴露 `focus`/`fit`（它刻意不把 Terminal 交出去：字节进、字节出是它的
 * 契约），而 `usedColumns` 要读 xterm 自己的列账本。这里在**环境层**把实例
 * 截下来——`open()` 是 xterm 建完 DOM 的必经一步，拦它不吃掉任何被测行为：
 * 拦截点之后走的仍是 xterm 原本的实现（`return original.apply(...)`）。
 * 于是断言读到的仍是「xterm 自己算出来的东西」，只是拿到了读它的入口。
 */
const terminalRegistry = new WeakMap<object, Terminal>()

/** 当前正在挂载的组件（同步的 `mount()` 期间唯一，所以不需要栈）。 */
let mounting: object | null = null

const originalOpen = Terminal.prototype.open
Terminal.prototype.open = function open(this: Terminal, parent: HTMLElement): void {
  if (mounting) terminalRegistry.set(mounting, this)
  originalOpen.call(this, parent)
}

afterAll(() => {
  // 原型补丁是进程级的：不还原会把「happy-dom 不排版」这条事实留给同进程里
  // 后跑的文件，而那些文件可能在断言 `offsetWidth === 0` 的兜底行为。
  for (const prop of ['offsetWidth', 'offsetHeight'] as const) {
    const desc = originals.shift()
    if (desc) Object.defineProperty(HTMLElement.prototype, prop, desc)
  }
  Terminal.prototype.open = originalOpen
})

/* ---------------------------------------------------------------- 挂载 */

/** 组件源文件——样式规则的断言读的是它（vitest 不注入 SFC 的 `<style>`）。 */
const TERMINAL_PANEL_SOURCE = readRepoFile('packages/web/src/domains/session/TerminalPanel.vue')

const READY: TerminalChannel = { state: 'ready', error: null, dropped: 0 }

function mount(props: Partial<Record<string, unknown>> = {}) {
  // `mountIn` 内部同步跑 `onMounted`，所以这一段里 `mounting` 是唯一的。
  const previous = mounting
  const holder = {}
  mounting = holder
  try {
    const wrapper = mountIn(TerminalPanel, {
      props: {
        deviceName: 'mbp.local',
        path: '/srv/alpha',
        channel: READY,
        conn: 'online' as TerminalConn,
        output: '',
        ended: false,
        ...props,
      },
    })
    // 登记表按**组件实例**记，而不是按这个 holder：`usedColumns(w)` 拿得到的
    // 只有 `w`。
    const term = terminalRegistry.get(holder)
    if (term) terminalRegistry.set(wrapper, term)
    terminalRegistry.delete(holder)
    return wrapper
  } finally {
    mounting = previous
  }
}

type Panel = ReturnType<typeof mount>

/**
 * 让面板「有尺寸」，并让 `.xterm` 的 padding 可解析。
 *
 * 两步都是**浏览器本来就会做的事**：布局给出宽高，`xterm.css` 给出 padding。
 * 之后调 `fit()`（组件暴露了它）——`ResizeObserver` 在 happy-dom 里不触发，
 * 而真实环境里它是尺寸变化的来源。
 *
 * 它是 **async** 的，理由和 `flush()` 是同一条：`resize(cols, rows)` 之后
 * xterm 要跨几帧才把 `.xterm-rows` 的行元素补到新的列数（`_refreshRowElements`
 * 发生在渲染帧里），而 `[data-size]` 是 `size` 这个 ref 驱动的、也要等 Vue
 * 下一次 DOM 更新。断言 cols/rows 与画面之前必须等这一落。
 */
function sized(w: Panel, width = 800, height = 600): Promise<void> {
  const host = hostOf(w)
  host.style.width = `${width}px`
  host.style.height = `${height}px`
  const term = w.element.querySelector<HTMLElement>('.xterm')
  if (term) term.style.padding = '0px'
  w.vm.fit()
  return flush()
}

function hostOf(w: Panel): HTMLElement {
  const el = w.element.querySelector<HTMLElement>('[data-terminal-host]')
  if (!el) throw new Error('terminal host is missing')
  return el
}

/** 可视区的行（逐行去掉 xterm 画在行尾的光标格）。 */
function rows(w: Panel): string[] {
  return Array.from(hostOf(w).querySelectorAll<HTMLElement>('.xterm-rows > div')).map((el) =>
    (el.textContent ?? '').trimEnd(),
  )
}

/** 可视区文本（去掉空行与行尾填充）。 */
function screenText(w: Panel): string {
  return rows(w)
    .join('\n')
    .replace(/^\n+|\n+$/g, '')
}

/** 面板里那个 xterm 实例的公开句柄（组件只暴露 `focus`/`fit`）。 */
function termOf(w: Panel): Terminal {
  const t = terminalRegistry.get(w)
  if (!t) throw new Error('terminal instance is not registered')
  return t
}

/**
 * 第一行**用了几列**。
 *
 * 问的是 xterm 自己：`ESC[6n`（DSR 光标位置报告）是终端协议的真查询，
 * xterm 回 `ESC[<行>;<列>R`，那个列就是**它自己的列账本**——按
 * `UnicodeService` 的 `wcwidth` 记，CJK 记 2 列、组合字符记 0 列。
 *
 * 这是「画面上有什么」里最硬的一种读法：它走的是真实终端会走的那条线
 * （`stty size`、`tput`、`printf` 光标寻址用的都是同一份账本），既不是
 * 数 DOM，也不是读组件字段。
 *
 * 为什么不数 DOM 里的字符：`.xterm-rows` 的一行**不会**被补到 `cols`
 * （实测：cols=20 写 `abc`，行文本是 `"abc "` 而不是补满 20 格），它只在
 * 内容末尾多画一个光标格。于是「字数」与「列数」是两件不同的事：
 * `中文ab` 在 DOM 里是 5 个字符、在账本里是 6 列。折行与光标寻址用的是列数。
 */
async function usedColumns(w: Panel): Promise<number> {
  const t = termOf(w)
  const seen: string[] = []
  const disposable = t.onData((data) => seen.push(data))
  try {
    t.write('\u001b[6n')
    await flush()
  } finally {
    disposable.dispose()
  }
  const reply = seen.join('').match(/\u001b\[\d+;(\d+)R/)
  if (!reply) throw new Error(`no cursor position report: ${JSON.stringify(seen)}`)
  // DSR 报的是**下一个**字符要落到的列（1 基），所以用了几列 = 报的列 − 1
  return Number(reply[1]) - 1
}

/**
 * xterm 从 `write()` 到 `.xterm-rows` 里真的有字，是**两拍一循环**：
 *
 *   宏任务（`WriteBuffer` 把 chunk 交给 `InputHandler.parse`）
 *     → 帧（`RenderDebouncer` 在 rAF 里把行内容排进 DOM）
 *
 * 所以「等一个宏任务」是不够的——那时一帧都还没跑；而**只**泵帧也不够：
 * 解析本身排在宏任务里，先泵帧就把帧用在了还没解析的数据上，那一帧白跑，
 * 之后再泵多少帧都不补这一次解析。实测（探针量过）：只泵帧时
 * 「跨帧切分的 `ESC[3`」这条用例时绿时红——buffer 里已经是 `ab`，
 * 画面上却还是 `a`，因为触发它渲染的那一帧在解析之前就跑掉了。
 *
 * 于是这里**成对**地泵：宏任务 → 帧 → 宏任务 → 帧 …… 泵 `FLUSH_ROUNDS` 对。
 * 每一对都完整覆盖一次「解析 + 渲染」，多出来的几对是给
 * `write` 内部按 `WRITE_TIMEOUT_MS` 切片续写的情形留的余量。
 *
 * 帧数与配对方式都是量出来的结论（`src/probe.test.ts` 量完即删），
 * 写成一个助手而不是在用例里复制计时代码：它只有**一个**答案。
 */
const FLUSH_ROUNDS = 3

/** 一帧。happy-dom 的 rAF 是真的排队执行，所以它能把 xterm 的渲染帧带出来。 */
function nextFrame(): Promise<void> {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}

/** 一个宏任务。`WriteBuffer` 的解析排在这上面。 */
function nextTask(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0))
}

/**
 * 等渲染落地再断言。
 *
 * 收尾再补一个宏任务：Vue 的 DOM 更新是微任务，而 `resize` 事件里
 * 组件会写 `size` ref，那一笔要等下一次 DOM 更新才出现在 `[data-size]` 上。
 */
async function flush(): Promise<void> {
  for (let i = 0; i < FLUSH_ROUNDS; i += 1) {
    await nextTask()
    await nextFrame()
  }
  await nextTask()
}

/** 敲键：走 xterm 自己的 textarea，与真用户输入同一条路径。 */
function type(w: Panel, data: string): void {
  inputEl(w).dispatchEvent(
    new InputEvent('input', { data, inputType: 'insertText', bubbles: true }),
  )
}

function inputEl(w: Panel): HTMLTextAreaElement {
  const el = w.element.querySelector<HTMLElement>('[data-panel-focus]')
  if (!(el instanceof HTMLTextAreaElement)) throw new Error('terminal textarea is missing')
  return el
}

/** 最近一次报给宿主的尺寸。 */
function lastSize(w: Panel): { cols: number; rows: number } | undefined {
  return w.emitted('resize')?.at(-1)?.[0] as { cols: number; rows: number } | undefined
}

/* ==========================================================================
   一、mock 字节流能渲染
   --------------------------------------------------------------------------
   这些字节一律写成 `\r\n`，因为**真实 PTY 就是这么发出来的**：termios 的
   `ONLCR` 把 `\n` 翻译成 `\r\n` 是 PTY 的职责，不是渲染层的——xterm 的
   `convertEol` 文档自己写着「Normally the settings of the underlying PTY
   (termios) deal with the translation of `\n` to `\r\n` and this setting
   should not be used」，组件也据此把它关掉了。

   所以喂 `\n` 不是「同一种内容的简写」，而是喂了**另一种字节**：只有 LF 时
   xterm 只做 line feed，光标留在原列，下一行的内容会从那一列开始——画面上
   是 `one` 下面一个缩进三格的 `two`。那不是 xterm 的 bug，是波 terminal line
   discipline 没干活的输入。要验「画面上有什么」，输入就得先是对的。
   ========================================================================== */

describe('TerminalPanel：字节流渲染（真 xterm.js 解析）', () => {
  it('空输出就是空——不画假提示符，也不画占位行', async () => {
    const w = mount()
    await flush()
    expect(screenText(w)).toBe('')
  })

  it('一段 PTY 字节写进去，画面上出现同一段文字', async () => {
    const w = mount({ output: 'hello\r\n' })
    await flush()
    expect(screenText(w)).toBe('hello')
  })

  it('输出是 append-only：只把增量写进 xterm，不重放整段', async () => {
    const w = mount({ output: 'one\r\n' })
    await flush()
    await w.setProps({ output: 'one\r\ntwo\r\n' })
    await flush()
    expect(screenText(w)).toBe('one\ntwo')
  })

  it('缓冲被清空（重开 PTY）时画面跟着清，不留上一块 PTY 的内容', async () => {
    const w = mount({ output: 'old\r\n' })
    await flush()
    await w.setProps({ output: '' })
    await flush()
    expect(screenText(w)).toBe('')
  })

  it('ANSI 颜色序列不进文本：画面上只有字符', async () => {
    const w = mount({ output: '\u001b[31mred\u001b[0m\r\n' })
    await flush()
    expect(screenText(w)).toBe('red')
  })

  it('控制序列**跨帧切分**仍然解析（§8.2 第 1 条点名要验的那一条）', async () => {
    // `terminal.rs` 的 `poll` 按 credit 预算切帧，一帧里出现半条 `ESC[3` 是
    // 常态而不是异常：xterm 必须自己把它缓存到下一帧。
    const w = mount({ output: 'a\u001b[3' })
    await flush()
    await w.setProps({ output: 'a\u001b[31mb\u001b[0m' })
    await flush()
    // 解析成功的判据是「序列没被当成字面量」：`b` 落在第 1 列，
    // 且画面上没有 `3`、`[`、`ESC` 这些字符。
    expect(screenText(w)).toBe('ab')
    expect(await usedColumns(w)).toBe(2)
  })

  it('CJK 按两列渲染（§8.2 的「CJK 宽度折叠」）', async () => {
    const w = mount({ output: '中文ab' })
    await flush()
    // 2 个汉字 × 2 列 + 2 个字母 = 6 列。列宽算错的话 `stty size` 之后
    // 的折行会整屏错位，而错位的排查成本远高于在这里验一次。
    expect(await usedColumns(w)).toBe(6)
  })

  it('ASCII 一个字符一列（与上一条互为对照）', async () => {
    const w = mount({ output: 'abc' })
    await flush()
    expect(await usedColumns(w)).toBe(3)
  })
})

/* ==========================================================================
   二、resize：xterm 的 cols/rows 与下发给 PTY 的一致
   ========================================================================== */

describe('TerminalPanel：尺寸协商（cols/rows 是量出来的，不是 CSS 算的）', async () => {
  it('fit 后把协商结果报给宿主——宿主据此调 channel.resize', async () => {
    const w = mount()
    await sized(w, 800, 600)
    const size = lastSize(w)
    expect(size).toBeDefined()
    // (800 − 滚动条 14) / 字宽 9 = 87 列；(600 − padding 0) / 行高 18 = 33 行
    expect(size?.cols).toBe(Math.floor((800 - RULER_W) / CHAR_W))
    expect(size?.rows).toBe(Math.floor(600 / CELL_H))
  })

  it('报出的 cols 与 xterm 实际渲染的列宽是同一个数（不是两份）', async () => {
    const w = mount()
    await sized(w, 800, 600)
    // 判据是「下发给 PTY 的列数 === xterm 现在真正按它折行的列数」：
    // 这两份一旦不同，屏幕上的折行位置与 shell 认为的折行位置就分叉。
    // 读 xterm 自己的 `cols`（它就是折行的依据）而不是数 DOM 里的字符——
    // `.xterm-rows` 的一行**不会**被补到 `cols`，数它得到的是字数。
    const size = lastSize(w)
    expect(size).toBeDefined()
    expect(termOf(w).cols).toBe(size?.cols)
    expect(termOf(w).rows).toBe(size?.rows)
    // 画面上真的放得下这么多列：写满一行不折行，多写一个才折。
    termOf(w).reset()
    termOf(w).write('x'.repeat(size!.cols))
    await flush()
    expect(rows(w)[0]).toBe('x'.repeat(size!.cols))
    expect(rows(w)[1]).toBe('')
  })

  it('尺寸读数显示的是协商后的 cols×rows', async () => {
    const w = mount()
    await sized(w, 800, 600)
    expect(w.element.querySelector('[data-size]')?.textContent).toMatch(/^\d+×\d+$/)
  })

  it('量不到字符尺寸时**不发** resize——不编一个尺寸下发给 PTY', async () => {
    // 面板还没布局（没有宽高）：FitAddon 量不到，`proposeDimensions` 返回
    // undefined。编一个 80×24 发下去会让 PTY 按错的宽度折行，而那个宽度
    // 没人知道是假的。
    const w = mount()
    w.vm.fit()
    expect(w.emitted('resize')).toBeUndefined()
    expect(w.element.querySelector('[data-size]')).toBeNull()
  })

  it('尺寸没变就不重发（拖把手时每一像素都来一次，不能每像素发一帧）', async () => {
    const w = mount()
    await sized(w, 800, 600)
    const n = w.emitted('resize')?.length ?? 0
    expect(n).toBeGreaterThan(0)
    w.vm.fit()
    w.vm.fit()
    expect(w.emitted('resize')?.length).toBe(n)
  })

  it('面板变窄后重发，且新尺寸更小', async () => {
    const w = mount()
    await sized(w, 800, 600)
    const wide = lastSize(w)
    await sized(w, 400, 600)
    const narrow = lastSize(w)
    expect(narrow?.cols).toBe(Math.floor((400 - RULER_W) / CHAR_W))
    expect(narrow!.cols).toBeLessThan(wide!.cols)
  })

  it('面板被拖到极窄时给 1×1 而不是 0×0（0 会被内核当成「未设置」）', async () => {
    const w = mount()
    await sized(w, 800, 600)
    await sized(w, 4, 4)
    const size = lastSize(w)
    expect(size?.cols).toBeGreaterThanOrEqual(1)
    expect(size?.rows).toBeGreaterThanOrEqual(1)
  })
})

/* ==========================================================================
   三、断线三态（§7.3）
   ========================================================================== */

describe('TerminalPanel：断线三态——只有 offline 冻结', () => {
  it('reconnecting 不冻结输入：设备上的 shell 还活着', async () => {
    const w = mount({ conn: 'reconnecting' })
    await flush()
    expect(inputEl(w).readOnly).toBe(false)
    expect(w.element.querySelector('[data-frozen]')).toBeNull()
    // 说明条要把「为什么还能敲」说出来（§7.3）
    const note = w.element.querySelector('[data-reconnecting]')?.textContent ?? ''
    expect(note).toContain('仍然活着')
    expect(note).toContain('不禁用输入')
  })

  it('reconnecting 期间敲键照常上行（断线 45s 内不阻断）', async () => {
    const w = mount({ conn: 'reconnecting' })
    await flush()
    type(w, 'x')
    expect(w.emitted('input')).toEqual([['x']])
  })

  it('offline 冻结：xterm 的输入框变只读，并给斜纹遮罩', async () => {
    const w = mount({ conn: 'offline' })
    await flush()
    expect(inputEl(w).readOnly).toBe(true)
    expect(w.element.querySelector('[data-frozen]')).not.toBeNull()
  })

  it('offline 的遮罩写清三件事：状态、事实、边界（§7.3 第 1 条）', async () => {
    const w = mount({ conn: 'offline' })
    await flush()
    const text = w.element.querySelector('[data-frozen]')?.textContent?.replace(/\s+/g, ' ') ?? ''
    expect(text).toContain('离线') // 状态
    expect(text).toContain('还活着') // 事实：设备上的 PTY 仍然活着
    expect(text).toContain('禁止新操作') // 边界
  })

  it('冻结期间内容仍可选（遮罩不吃指针事件，§7.3 第 2 条）', async () => {
    const w = mount({ conn: 'offline' })
    await flush()
    const overlay = w.element.querySelector<HTMLElement>('[data-frozen]')
    // happy-dom 不做排版、vitest 默认不注入 SFC 的 `<style>`，所以
    // `getComputedStyle` 读不到这条规则——那就直接读**样式源**：
    // 断言「遮罩不吃指针事件」这件事本身，而不是「样式表有若干条规则」。
    const rule = cssRuleOf(TERMINAL_PANEL_SOURCE, '.af-termx__frozen')
    expect(rule).toMatch(/pointer-events:\s*none/)
    expect(overlay?.className ?? '').toContain('af-termx__frozen')
    // 内容与遮罩同在：遮罩是兄弟节点，不是把它盖住的那块内容替换掉
    expect(hostOf(w).querySelector('.xterm-rows')).not.toBeNull()
  })

  it('offline 期间敲键不上行，也不触发建通道（冻结就是冻结）', async () => {
    const w = mount({ conn: 'offline' })
    await flush()
    type(w, 'x')
    expect(w.emitted('input')).toBeUndefined()
    expect(w.emitted('interact')).toBeUndefined()
  })

  it('三态切换：offline → online 之后能重新敲（冻结可逆）', async () => {
    const w = mount({ conn: 'offline' })
    await flush()
    expect(inputEl(w).readOnly).toBe(true)
    await w.setProps({ conn: 'online' })
    await flush()
    expect(inputEl(w).readOnly).toBe(false)
    expect(w.element.querySelector('[data-frozen]')).toBeNull()
    type(w, 'y')
    expect(w.emitted('input')).toEqual([['y']])
  })

  it('会话已结束也冻结（工作区不再可达）', async () => {
    const w = mount({ ended: true })
    await flush()
    expect(inputEl(w).readOnly).toBe(true)
    expect(w.element.textContent).toContain('不会再建立通道')
  })

  it('未建立 / 建立中 / 失败 / 已释放**都不冻结**——冻在那儿会自锁', async () => {
    // 冻结的依据只有「离线」与「已结束」。若在 `idle` 也冻结，xterm 不再产生
    // `onData`，于是「第一次敲键」永不发生、通道永不建立——一个自锁的死锁。
    for (const state of ['idle', 'opening', 'failed', 'closed'] as const) {
      const w = mount({ channel: { state, error: null, dropped: 0 } })
      await flush()
      expect(inputEl(w).readOnly, state).toBe(false)
      expect(w.element.querySelector('[data-frozen]'), state).toBeNull()
    }
  })
})

/* ==========================================================================
   四、输入上行
   ========================================================================== */

describe('TerminalPanel：输入走 channel.send，意图走 interact', () => {
  it('敲一个字符：报 input（原始文本）+ interact（该建通道了）', async () => {
    const w = mount()
    await flush()
    type(w, 'a')
    expect(w.emitted('input')).toEqual([['a']])
    expect(w.emitted('interact')).toHaveLength(1)
  })

  it('换行符原样上行——PTY 要的就是 `\r`，不替它格式化', async () => {
    const w = mount()
    await flush()
    type(w, '\r')
    expect(w.emitted('input')).toEqual([['\r']])
  })
})

/* ==========================================================================
   五、状态可见 / 无障碍 / 不画假数据
   ========================================================================== */

describe('TerminalPanel：丢弃的输入要说出来', () => {
  it('有丢弃时给徽标，并说明为什么不排队', () => {
    const w = mount({ channel: { state: 'ready', error: null, dropped: 3 } })
    const badge = w.element.querySelector('[data-dropped]')
    expect(badge?.textContent?.replace(/\s+/g, '')).toContain('有3条输入已丢')
    expect(badge?.getAttribute('title')).toContain('不排队')
  })

  it('没丢过就不显示（0 不是要展示的信息）', () => {
    const w = mount({ channel: READY })
    expect(w.element.querySelector('[data-dropped]')).toBeNull()
  })
})

describe('TerminalPanel：无障碍', () => {
  it('终端是 role="log"，aria-label 说明它是谁的终端', () => {
    const w = mount()
    const host = hostOf(w)
    expect(host.getAttribute('role')).toBe('log')
    const label = host.getAttribute('aria-label') ?? ''
    expect(label).toContain('终端输出')
    expect(label).toContain('mbp.local')
    expect(label).toContain('/srv/alpha')
  })

  it('焦点落点就是 xterm 的输入框（敲键即打字，不用再 Tab 一次）', () => {
    const w = mount()
    const ta = inputEl(w)
    expect(ta.getAttribute('data-panel-focus')).not.toBeNull()
    expect(ta.getAttribute('aria-label')).toContain('mbp.local')
    expect(ta.tabIndex).toBe(0)
  })
})

describe('TerminalPanel：不画假数据', () => {
  it('没有输出时不预置提示符、不预置欢迎行', async () => {
    const w = mount()
    await flush()
    expect(screenText(w)).toBe('')
  })

  it('面板里没有「连接」按钮——通道由交互触发建立', () => {
    const w = mount()
    const labels = Array.from(w.element.querySelectorAll('button')).map((b) => b.textContent ?? '')
    expect(labels.some((l) => l.includes('连接'))).toBe(false)
  })
})
