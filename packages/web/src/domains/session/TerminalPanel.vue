<script setup lang="ts">
/* ===========================================================================
   TerminalPanel —— 真终端面板（子任务 13b；规范 §5.4 / §7.3 / §4.5）
   ---------------------------------------------------------------------------
   这一块是**全规范唯一承认未验证过的组件**（§8.2 第 1 条）：原型用 HTML/CSS
   模拟终端，真实实现是 xterm.js。所以这里不做「把字节按行切开画 <div>」
   ——那是原型做得到而真终端做不到的事：ANSI 序列、光标寻址、CJK 宽字符、
   滚动区、选区，全都得由 xterm 自己解析。node 侧也明确不解释转义序列
   （`node-core/src/connector/terminal.rs` 的模块注释：「node 只搬字节，
   不做终端仿真」），所以**字节到画面的唯一解释器就是这里的 xterm**。

   ── 尺寸协商：cols/rows 是「协商」出来的，不是 CSS 算出来的 ──────────
   面板头（原型 `panels/terminal.html` 的尺寸协商段）要求显示**协商后的**
   `cols×rows`。这里的协商方是 xterm：它按自己的 `CharSize` 量出字符宽高，
   `FitAddon.proposeDimensions()` 给出「这块元素能放下几列几行」，
   我们把它 `term.resize(cols, rows)` 并报给宿主。

   **不新增消息类型**（约束 14）：resize 走现有的控制帧 —— 宿主收到
   `resize` 事件后调 `ChannelHandle.resize(cols, rows)`，由 `channel.ts`
   发 `POST /api/channels/{id}/resize`，Hub 转成 `channel_data` 的
   `{"resize":{...}}`（`hub-server/src/routes/stream.rs`）。协议版本不动。

   ── 这个组件不知道通道 ──────────────────────────────────────────────
   与 FilesPanel 同一条纪律：不 import 运行时的 `channel.ts`，不发任何请求。
   字节进（`output` prop）→ 字节出（`input` 事件）→ 尺寸出（`resize` 事件）。
   宿主现在喂 mock，将来喂 `channel.frame` 解出来的 `payload.bytes`，
   **本文件的形状一行都不用改**。

   ── 输出为什么是一个「累积字符串」而不是 `write()` 方法 ────────────
   因为切 tab 会让面板卸载（PanelDock 对面板用 `v-if`），而 §5.5.2 要求
   「切走再回来时滚动记录还在」。xterm 实例随组件销毁，所以**缓冲必须由
   宿主持有**（13a 的 `TabWorkspace.output` 就是它）。组件只写增量：
   `output` 是 append-only 的，已写长度记在 `written` 里，每次只
   `write(output.slice(written))`。写成 prop 而不是暴露 `write()`，
   是为了让「缓冲归谁」这个问题只有**一个**答案。

   ── 断线三态：只有 `offline` 冻结（§7.3） ───────────────────────────
   | 态 | 输入 | 这里做什么 |
   |---|---|---|
   | `online` | 可用 | 正常 |
   | `reconnecting`（< 45s） | **不禁用** | 琥珀说明条：设备上的 shell 仍然活着 |
   | `offline`（> 45s） | **冻结** | 斜纹只读遮罩，写清三件事 |

   45s 的判据**由宿主判**（`security-and-lifecycle.md` §5：判据是心跳年龄，
   不是连接状态；`design-system.md` §8.1 第 5 条已裁决按 45s 标 offline）。
   组件只吃 `conn` 这个结论——它拿不到心跳，也不该自己猜一个时钟。

   冻结用 `term.options.disableStdin = true`（xterm 把 textarea 置
   `readonly`）而不是把元素 `inert`：后者会让**选区也拿不到**，而 §7.3 第 2 条
   明确要求「保留最后已知内容且可全选复制」。遮罩是斜纹 + `pointer-events:none`
   （§3.16：遮到不透明等于清屏，清屏毁掉「显示最后已知状态」这条承诺）。

   只在 `offline` / `ended` 时冻结，是**刻意的**：`idle` / `opening` /
   `closed` 都还能建或重建通道（`channel.ts` 的 `interact` 在
   `idle|failed|closed` 下会重发 `open_channel`）。若在那些态冻结，
   xterm 不再产生 `onData`，于是「第一次敲键」永远不发生，通道永远建不起来——
   一个自锁的死锁。

   ── 空就是空 ────────────────────────────────────────────────────────
   不预置提示符、不预置欢迎行、不画假的 `$`。通道建好之前这一块是**真的空**，
   「空」本身就是正确的显示（§3.12：空态回答「这里本来该有东西但没有」）。

   ── 焦点 ────────────────────────────────────────────────────────────
   xterm 的输入落在它自己创建的 `<textarea>` 上（`tabindex=0`）。所以
   `data-panel-focus` 挂在**那个 textarea**上，而不是本组件的根元素上：
   宿主的 `focusPanelBody()` 找 `[data-panel-focus]` 并 `focus()` 它，
   拿到焦点的是 textarea（能打字），且 `document.activeElement` 就是宿主
   刚记下的那个元素——13a 用 `selfFocused` 抑制「打开坞就建通道」的那段逻辑
   因此**仍然成立**。若焦点先落在根 div 上再转发给 textarea，
   `activeElement` 就变成了另一个元素，那段抑制会失效。
   =========================================================================== */

import { FitAddon } from '@xterm/addon-fit'
import { Terminal } from '@xterm/xterm'
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import type { ChannelState } from '@/shared/channel'
import { DEFAULT_COLS, DEFAULT_ROWS } from '@/shared/channel'

// xterm 的样式必须显式引入：它把 `.xterm` 的定位、光标层、滚动条都写在
// 自己的 CSS 里，漏掉这份 CSS 的表现是「终端有内容但全部叠在左上角」。
import '@xterm/xterm/css/xterm.css'

/* ---------------------------------------------------------------- 数据契约 */

/** 通道视图。形状与 `PanelDock` 的 `ChannelView` 一致——宿主可以把它那份直接传进来。 */
export interface TerminalChannel {
  state: ChannelState
  error: string | null
  dropped: number
}

/** 设备连接态。**45s 的判定在宿主**（见文件头），这里只吃结论。 */
export type TerminalConn = 'online' | 'reconnecting' | 'offline'

/** 尺寸。报给宿主 → `ChannelHandle.resize` → `TIOCSWINSZ`。 */
export interface TerminalSize {
  cols: number
  rows: number
}

const props = withDefaults(
  defineProps<{
    /** 会话所属设备名。无障碍名里重复一次：两个终端并排时靠它分。 */
    deviceName: string
    /** 会话工作区路径（锁定）。同样只在无障碍名里出现。 */
    path: string
    /** 当前面板的通道视图（状态 / 错误码 / 丢弃的输入条数）。 */
    channel: TerminalChannel
    /** 设备连接态。§7.3 的三态开关。 */
    conn: TerminalConn
    /** 累积的 PTY 字节。**宿主持有**，切 tab 不丢。组件只写增量。 */
    output?: string
    /** 会话已结束：不会再有通道，输入冻结。 */
    ended?: boolean
  }>(),
  { output: '', ended: false },
)

const emit = defineEmits<{
  /** 用户敲键：要送到设备上去的**原始文本**（含 `\r`、控制字符、粘贴块）。 */
  (e: 'input', text: string): void
  /** 协商后的尺寸变了。宿主调 `ChannelHandle.resize(cols, rows)`。 */
  (e: 'resize', size: TerminalSize): void
  /**
   * 与面板正文发生了交互：该建通道了。
   *
   * 对宿主是**幂等**的（`channel.ts` 的 `open()` 在 `ready` 下直接返回，
   * 失败态才重发）。挂在 `onData` 而不是 `focusin` 上：聚焦已经由
   * PanelDock 那一层的 `focusin` 报过了，而「敲了第一个键」是比「看了一眼」
   * 更硬的交互证据。
   */
  (e: 'interact'): void
}>()

/* ---------------------------------------------------------------- 常量 */

/**
 * 建通道时的默认尺寸。
 *
 * 与 node 侧 `terminal.rs` 的 `DEFAULT_COLS/ROWS` 同一个值、同一个理由：
 * 80×24 是所有终端的出厂默认，也是「还没量到尺寸」时唯一站得住的值。
 * 它只是 `open_channel` 的 `params` 与 xterm 的初值——第一次 fit 成功后
 * 立刻被真值覆盖。
 *
 * **常量在 `lib/channel.ts`**：页面下发 `params` 时用的是同一个值，两处
 * 各写一份必然漂——漂出来的症状是「PTY 按 80×24 开、屏幕却按别的宽度折行」。
 */

/** 下限 1×1，与 node 侧 `MIN_DIM` 同源：0 会被 `TIOCSWINSZ` 当成「未设置」。 */
const MIN_COLS = 1
const MIN_ROWS = 1

/**
 * 字号 12px = `--af-text-sm`，与 `components.css` 里 `.af-term` 的取值一致。
 *
 * `--af-text-md`（13px）是**正文**基线（§6），`.af-term` 用的是 12px；
 * 终端按 `.af-term` 走，避免同一块界面里两种终端字号。
 */
const FONT_SIZE = 12

/** 行高 1.35 = `--af-leading-tight`，同样是 `.af-term` 的取值。 */
const LINE_HEIGHT = 1.35

/**
 * 字族必须是真字符串：xterm 要拿它去 `measureText` 量字符宽，
 * 而 `var(--af-font-mono)` 在它那儿是一个不认识的字族名。
 * 于是这里与 `tokens.css` 的 `--af-font-mono` 是同一串字（CJK 等宽在
 * 第三位）——**改 token 时这两处要一起改**，已在文件头注明。
 */
const FONT_FAMILY =
  "'JetBrains Mono', 'IBM Plex Mono', 'Noto Sans Mono CJK SC', ui-monospace, Menlo, Consolas, monospace"

/**
 * ANSI 主题走 `var(--af-term-*)`。
 *
 * 自定义属性**会继承**，而 xterm 把这些值写进 `.xterm-rows` 的内联样式，
 * 所以它们挂在 `:root` 上时照样能解析——比在 JS 里再抄一份色值强：
 * 抄一份就等于有了第二份真相源，而 `tokens.css` §12 的每个取值都是对着
 * `--af-inset` 量过对比度的。
 */
const THEME = {
  background: 'var(--af-term-bg)',
  foreground: 'var(--af-term-fg)',
  cursor: 'var(--af-accent)',
  cursorAccent: 'var(--af-fg-on-accent)',
  selectionBackground: 'var(--af-accent-bg)',
  black: 'var(--af-term-ansi-black)',
  red: 'var(--af-term-ansi-red)',
  green: 'var(--af-term-ansi-green)',
  yellow: 'var(--af-term-ansi-yellow)',
  blue: 'var(--af-term-ansi-blue)',
  magenta: 'var(--af-term-ansi-magenta)',
  cyan: 'var(--af-term-ansi-cyan)',
  white: 'var(--af-term-ansi-white)',
  brightBlack: 'var(--af-term-ansi-bright-black)',
  brightRed: 'var(--af-term-ansi-bright-red)',
  brightGreen: 'var(--af-term-ansi-bright-green)',
  brightYellow: 'var(--af-term-ansi-bright-yellow)',
  brightBlue: 'var(--af-term-ansi-bright-blue)',
  brightMagenta: 'var(--af-term-ansi-bright-magenta)',
  brightCyan: 'var(--af-term-ansi-bright-cyan)',
  brightWhite: 'var(--af-term-ansi-bright-white)',
}

/** 滚动缓冲行数。xterm 默认 1000，这里写出来是因为它对内存有直接影响。 */
const SCROLLBACK = 1000

const DROP_TITLE =
  '断线期间发出的输入不排队：排队会在重连后把字符乱序灌进 PTY。' +
  '这里显示的是被丢掉的输入条数，不是字节数。'

/* ---------------------------------------------------------------- 状态 */

const hostEl = ref<HTMLElement | null>(null)

let term: Terminal | null = null
let fitAddon: FitAddon | null = null
let observer: ResizeObserver | null = null
let frame = 0

/** 已写进 xterm 的字节数。`output` 是 append-only，所以只写 `slice(written)`。 */
let written = 0
/** 最近一次报给宿主的尺寸。**没量到过就是 `null`**，那时不画尺寸读数。 */
const size = ref<TerminalSize | null>(null)
let reported: TerminalSize | null = null

/** 冻结 = 判离线，或会话已结束。别的态都不冻（理由见文件头的死锁论证）。 */
const frozen = computed<boolean>(() => props.ended || props.conn === 'offline')

const ariaLabel = computed<string>(
  () =>
    `终端输出：设备 ${props.deviceName}，工作区 ${props.path}。` +
    '敲键会送到设备上的 shell；这里的画面由 xterm.js 解析 ANSI 序列渲染。',
)

/* ---------------------------------------------------------------- 生命周期 */

onMounted(() => {
  const el = hostEl.value
  if (!el) return

  term = new Terminal({
    cols: DEFAULT_COLS,
    rows: DEFAULT_ROWS,
    // **不**开 convertEol：PTY 自己会发 `\r\n`，替它补一个 `\n` 会让
    // 每一行多一个空行（那是 node 侧 termios 的活，不是渲染层的）。
    convertEol: false,
    cursorBlink: false,
    fontFamily: FONT_FAMILY,
    fontSize: FONT_SIZE,
    lineHeight: LINE_HEIGHT,
    scrollback: SCROLLBACK,
    theme: THEME,
    // 冻结由 `disableStdin` 在运行时切（见 `applyFrozen`），初值按当前态给
    disableStdin: frozen.value,
    // 不碰 proposed API：开了就等于把「实验接口」写进生产面板
    allowProposedApi: false,
  })
  fitAddon = new FitAddon()
  term.loadAddon(fitAddon)
  term.onData(onData)
  term.open(el)

  // 焦点落点挂到 xterm 的 textarea 上（理由见文件头「焦点」段）。
  const ta = term.textarea
  if (ta) {
    ta.setAttribute('data-panel-focus', '')
    ta.setAttribute('aria-label', `终端输入：敲键送到 ${props.deviceName}`)
  }

  // 回放宿主已经攒下的缓冲（切回来时它还在）
  writeDelta(props.output)
  syncFit()

  // 面板宽度可被把手拖动（§3.15），所以尺寸要跟着元素走。
  // `ResizeObserver` 而不是 window.resize：把手拖动不改窗口尺寸，
  // 而那正是终端尺寸最常变的时刻。
  if (typeof ResizeObserver !== 'undefined') {
    observer = new ResizeObserver(scheduleFit)
    observer.observe(el)
  }
})

onBeforeUnmount(() => {
  if (frame && typeof cancelAnimationFrame !== 'undefined') cancelAnimationFrame(frame)
  frame = 0
  observer?.disconnect()
  observer = null
  term?.dispose()
  term = null
  fitAddon = null
})

/* ---------------------------------------------------------------- 尺寸 */

/** 尺寸变化攒到下一帧：拖动把手时 ResizeObserver 每一像素都来一次。 */
function scheduleFit(): void {
  if (frame) return
  frame = requestAnimationFrame(() => {
    frame = 0
    syncFit()
  })
}

/**
 * 量一次并同步给 xterm 与宿主。
 *
 * `proposeDimensions()` 在**量不到字符尺寸时返回 `undefined`**
 * （happy-dom / 还没布局就挂载 / 元素被折叠）——那时什么都不做：
 * 编一个尺寸发下去比不发更糟，PTY 会按错误的宽度折行，而那个宽度
 * 没人知道是假的。此时通道沿用 `open_channel` 时的 80×24。
 */
function syncFit(): void {
  const t = term
  const fit = fitAddon
  if (!t || !fit) return
  const dims = fit.proposeDimensions()
  if (!dims || !Number.isFinite(dims.cols) || !Number.isFinite(dims.rows)) return

  const cols = Math.max(MIN_COLS, Math.floor(dims.cols))
  const rows = Math.max(MIN_ROWS, Math.floor(dims.rows))
  if (reported && reported.cols === cols && reported.rows === rows) return

  // 先让 xterm 自己变，再报给宿主：屏幕上的列数与 PTY 的列数必须是同一个数
  if (t.cols !== cols || t.rows !== rows) t.resize(cols, rows)
  reported = { cols, rows }
  size.value = { cols, rows }
  emit('resize', { cols, rows })
}

/**
 * 显式重量一次。
 *
 * **只服务测试**：宿主从不调它（宿主的尺寸入口是 `ResizeObserver` 与
 * `emit('resize')`）。留着是因为测试要断言「协商出的尺寸是多少」，而
 * happy-dom 量不到字符尺寸——测试要的是一次确定的测量。
 *
 * 注释里**不写**「比如面板从折叠里展开时」：没有这样的调用方，而照着
 * 注释去加一次宿主 `ref.fit()` 会与 `ResizeObserver` 的 `scheduleFit`
 * 叠加成两次测量 / 两条 `resize`，设备侧 PTY 会连着收两条。
 */
function fit(): void {
  syncFit()
}

/* ---------------------------------------------------------------- 输出 */

/**
 * 只写增量。
 *
 * `output` 变短只有一种合理解释：宿主把缓冲清了（重建会话 / 重开 PTY）。
 * 那时 xterm 也必须清——不清就会留下上一块 PTY 的画面，而那块画面
 * 看起来像「这次命令的输出」。
 */
function writeDelta(text: string): void {
  const t = term
  if (!t) return
  if (text.length < written) {
    t.clear()
    written = 0
  }
  if (text.length === written) return
  const delta = text.slice(written)
  written = text.length
  if (delta) t.write(delta)
}

watch(
  () => props.output,
  (text) => writeDelta(text),
)

/* ---------------------------------------------------------------- 输入 */

function onData(data: string): void {
  // 敲键是「该建通道了」最硬的证据
  emit('interact')
  if (frozen.value) return
  emit('input', data)
}

/**
 * 冻结 / 解冻。
 *
 * 用 `disableStdin` 而不是 `inert`：后者会把选区一起拿走，而 §7.3 第 2 条
 * 要求离线时「保留最后已知内容且可全选复制」。
 */
function applyFrozen(value: boolean): void {
  if (!term) return
  term.options.disableStdin = value
}

watch(frozen, applyFrozen)

/* ---------------------------------------------------------------- 暴露 */

/**
 * 只暴露 `fit`。
 *
 * **不暴露 `focus`**：宿主的 `focusPanelBody()` 走的是 `[data-panel-focus]`
 * 选择器（xterm 自己创建的 textarea），不碰 `defineExpose`——多一个
 * 出口就多一条「谁负责还焦点」的路径。
 */
defineExpose({ fit })
</script>

<template>
  <div class="af-termx" data-terminal>
    <div class="af-termx__stage">
      <!-- xterm 自己往这里挂 `.xterm`（含它创建的 textarea）。
           本组件不往里塞任何子节点——塞了就会被 xterm 的渲染层盖掉。 -->
      <div
        ref="hostEl"
        class="af-termx__screen"
        role="log"
        :aria-label="ariaLabel"
        data-terminal-host
      ></div>

      <!-- offline：冻结。斜纹 + pointer-events:none：遮住的是「输入」，不是「内容」 -->
      <div v-if="frozen" class="af-termx__frozen" data-frozen aria-hidden="false">
        <div class="af-termx__card" role="status">
          <div class="af-termx__cardline">
            <span aria-hidden="true">○</span>
            <span class="af-badge af-badge--danger">离线</span>
            <b>输入已冻结</b>
          </div>
          <p class="af-termx__cardline">
            设备上的进程<b>还活着</b>——Hub 只是暂时够不着它。
          </p>
          <p class="af-termx__cardline af-t-mute">
            冻结期间禁止新操作；最后已知的内容还在上面，可以全选复制。恢复后重新敲一次命令即可。
          </p>
        </div>
      </div>
    </div>

    <!-- reconnecting：按 §7.3 **不禁用输入**，但要说清为什么还能敲 -->
    <p v-if="conn === 'reconnecting'" class="af-termx__note" role="status" data-reconnecting>
      <span aria-hidden="true">◐</span>
      设备上的 shell <b>仍然活着</b>，所以这里<b>不禁用输入</b>；这段时间里的输出可能收不全。
    </p>

    <div class="af-termx__foot">
      <span
        v-if="channel.dropped > 0"
        class="af-badge af-badge--warn"
        :title="DROP_TITLE"
        data-dropped
      >
        有 {{ channel.dropped }} 条输入已丢
      </span>
      <!-- 尺寸读数是**协商后**的值（原型 §尺寸协商）：没量到过就不画 -->
      <span v-if="size" class="af-termx__size" data-size>{{ size.cols }}×{{ size.rows }}</span>
      <span class="af-spacer"></span>
      <span v-if="ended" class="af-t-mute">会话已结束：不会再建立通道。</span>
    </div>
  </div>
</template>

<style scoped>
.af-termx {
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
  background: var(--af-term-bg);
}
/* 舞台只是给冻结遮罩一个定位上下文：遮罩要盖住终端，但不盖住下面的说明条 */
.af-termx__stage {
  position: relative;
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
}
.af-termx__screen {
  flex: 1 1 auto;
  min-height: 0;
  /* xterm 的 `.xterm` 按父元素的高度量能放下几行，所以这里必须**有**高度：
     `min-height: 0` 让它在 flex 里可压缩，而不是塌成 0（塌成 0 时
     FitAddon 量不到，于是尺寸永远停在 80×24）。 */
  padding: var(--af-space-2) var(--af-space-3);
  overflow: hidden;
}
/* 冻结遮罩：斜纹而不是纯暗底（§3.16），且不吃指针事件——内容仍可全选复制 */
.af-termx__frozen {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: var(--af-space-4);
  pointer-events: none;
  background: repeating-linear-gradient(
    135deg,
    rgb(8 10 13 / 62%) 0 6px,
    rgb(8 10 13 / 38%) 6px 12px
  );
}
.af-termx__card {
  max-width: 46ch;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  padding: var(--af-space-4) var(--af-space-5);
  background: var(--af-surface-2);
  border: 1px solid var(--af-danger-line);
  border-radius: var(--af-radius-sm);
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-snug);
  color: var(--af-fg-secondary);
}
.af-termx__cardline {
  margin: 0;
  display: flex;
  align-items: center;
  gap: var(--af-space-2);
}
/* reconnecting 是琥珀：它是「还能动，但你该知道」，不是「不能动」（§4） */
.af-termx__note {
  flex: 0 0 auto;
  margin: 0;
  padding: var(--af-space-2) var(--af-space-4);
  border-top: 1px solid var(--af-warn-line);
  background: var(--af-warn-bg);
  color: var(--af-fg-secondary);
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-snug);
}
.af-termx__foot {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  padding: var(--af-space-2) var(--af-space-4);
  border-top: 1px solid var(--af-border-subtle);
  background: var(--af-surface-1);
  font-size: var(--af-text-2xs);
  color: var(--af-fg-tertiary);
}
.af-termx__size {
  font-variant-numeric: tabular-nums;
}
</style>
