<script setup lang="ts">
/* ===========================================================================
   SessionPage —— 会话工作台（规范 §5.4 / §5.5 / §7.1 / §7.2）
   ---------------------------------------------------------------------------
   13a 做了**结构与焦点**：对话列 + 可拖拽分栏 + 面板坞 + 顶部 tab 条。
   13b / 13c / 13d 的三个面板在这里挂进来：终端（TerminalPanel）、
   文件（FilesPanel）、对话流 + 轨迹抽屉（ConvFlow + TrajDrawer）。

   ── 宿主喂数据，面板只报意图 ──────────────────────────────────────
   三个面板**都不 import `channel.ts`、都不发请求**：通道的生命周期是页面
   这一层的事实（§5.5.1），面板若各持一条句柄就会出现「面板以为通道活着、
   页面已经关了它」的第二份真相。所以页面负责：喂输出 / 清单 / 预览，
   收 `input` / `list` / `read` / `save` / `run`，把它们翻译成上行。

   ── `channel.frame` 的解帧只有一处 ────────────────────────────────
   真 files connector 到位后，`onFileFrame` 是唯一要改的地方：现在它认
   `payload.op === 'list' | 'read' | 'write'` 的形状，而那个形状就是
   `node-core/src/connector/files.rs` 已经定好的回帧形状。终端那条
   （`payload.bytes` 累加进 `ws.output`）与 exec 那条（`exec_result`）
   同理，各在自己的段落里。

   ── 每个 tab 持有自己的一份状态 ───────────────────────────────────
   `{ dock, panel, mounted }` + 终端缓冲 + 文件树 cwd / 清单 / 预览 / 脏标记
   挂在 workspace 对象上，切 tab 时整体换掉。只有一个全局 state 的话，
   切到 B 会把 A 的面板与输出全冲掉——于是「支持多个」只剩一条 tab 条，
   是个假的（§5.5.2）。

   ── 通道的生命周期只有两条规则（§5.5.1 / §7.2） ─────────────────
   ① **懒建立**：第一次与面板正文交互才 `open_channel`。打开坞、切面板都
      不建——那时还没有任何数据要流动。
   ② **关面板才 `close_channel`**；**切 tab 不关**。边界是「关掉这个 tab」
      或「关掉面板坞」。切 tab 是视图切换（DECISIONS D10）。

   代价是 N 个 tab = N × 面板数条活跃通道，所以 tab 上显示面板数徽标。

   ── 断线期间的上行：丢弃 + 计数，不排队 ──────────────────────────
   `channel.send` 在流不在线时直接丢并计数，面板头显示「有 N 条已丢」。
   排队会让重连之后一整段字符乱序灌进 PTY，比丢几个字符难查得多。

   ── 为什么切 tab 用 replaceState 而不是 navigate ─────────────────
   `/sessions/:id` 只表达「激活这个 tab」（§5.5.6），切 tab 是视图切换：
   用 navigate 会进历史记录，于是「后退」变成在 tab 之间来回跳，
   而 tab 集合本身并不可深链接。replaceState 让地址栏跟着当前 tab 走
   （刷新后回到同一个），但不制造历史条目。
   =========================================================================== */

import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'

import ConvFlow, { type ConvExecRequest } from '@/components/ConvFlow.vue'
import EmptyState from '@/components/EmptyState.vue'
import FilesPanel, {
  type FileListing,
  type FilePreview,
  type FilesError,
} from '@/components/FilesPanel.vue'
import PanelDock, { type ChannelView, type PanelName } from '@/components/PanelDock.vue'
import SplitHandle from '@/components/SplitHandle.vue'
import StateBadge from '@/components/StateBadge.vue'
import TabBar, { type TabConn } from '@/components/TabBar.vue'
import TerminalPanel, { type TerminalSize } from '@/components/TerminalPanel.vue'
import TrajDrawer from '@/components/TrajDrawer.vue'
import type {
  DeviceSummary,
  SessionSummary,
  TrajectoryEvent,
  TrajectoryGap,
} from '@/lib/api'
import { getTrajectory, listDevices, listSessions } from '@/lib/api'
import {
  ChannelHub,
  DEFAULT_COLS,
  DEFAULT_ROWS,
  type ChannelFrame,
  type ChannelHandle,
  type ChannelState,
} from '@/lib/channel'
import { cwdPathOf, isOutsideSeg, splitPath } from '@/lib/files-path'
import { deviceLabel as deviceLabelOf, relativeTime, sessionTitle } from '@/lib/format'
import { ApiError, NetworkError, describeReadFailure, request } from '@/lib/http'
import { actForKey, bindingOf, dispatch, isTyping, register, unregister } from '@/lib/keynav'
import { SPLIT_DEFAULT } from '@/lib/split'
import {
  activateTab as activateTabState,
  closeTab,
  loadTabs,
  openTab,
  saveTabs,
  type TabMeta,
  type TabsState,
} from '@/lib/tabs'
import { navigate, route } from '@/router'

/* ---------------------------------------------------------------- 数据 */

const sessions = ref<SessionSummary[]>([])
const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const loadError = ref<string | null>(null)

const sessionsById = computed(() => new Map(sessions.value.map((s) => [s.id, s])))
const devicesById = computed(() => new Map(devices.value.map((d) => [d.id, d])))

const current = computed<SessionSummary | null>(
  () => sessionsById.value.get(route.value.params.id) ?? null,
)
const ended = computed(() => current.value?.state === 'ended')

function deviceOf(nodeId: string): DeviceSummary | null {
  return devicesById.value.get(nodeId) ?? null
}

/**
 * 设备连接态。`nodeId === null` = 设备已被删除，**与「离线」是两种态**：
 * 离线是「等它回来」，已移除是「它不会再回来了」。页面据此决定是显示
 * 重连动画还是让用户别再等——合成一个态用户会一直等一台删掉的机器。
 */
function connOf(nodeId: string | null): TabConn {
  if (nodeId === null) return 'offline'
  return deviceOf(nodeId)?.status ?? 'offline'
}

/**
 * 会话所属设备是否已被删除（`workspace.node_id` 为 NULL）。
 *
 * **与 `ended` 是两个事实，所以是两个谓词**：「会话结束了」是会话自己的状态，
 * 「设备被删了」是它落脚的那台机器没了——页面为两者写的是不同的解释（见
 * 模板里两条横幅），合成一个的话用户读到的原因是错的那一条。它拦的是
 * **对话 / 命令这条发送路**：设备没了，`workspace.node_id` 是 NULL，开通道只会
 * 换回 `device_removed` 409——拦不住的话，横幅说「设备已被删除」而输入框仍可
 * 打字、按钮仍可点，两句话互相打架。
 */
const deviceRemoved = computed(() => current.value?.workspace.node_id === null)

/**
 * 「设备已被删除」这句话本身。横幅与输入区（`ConvFlow` 的 `deviceGoneWhy`）
 * 读的是**同一份**：同一个事实在两处换个说法，用户会以为那是两件事。
 */
const deviceGoneWhy = computed(() => {
  const removed = current.value?.workspace.removed_node_id
  return `这个会话所属的设备已被删除（原设备 ${removed ?? '（未记录）'}）`
})

/* ---------------------------------------------------------------- tab 状态 */

const tabs = ref<TabsState>({ tabs: [], activeId: null })
const activeTab = computed<TabMeta | null>(
  () => tabs.value.tabs.find((t) => t.id === tabs.value.activeId) ?? null,
)

interface TabWorkspace {
  /** 面板坞开着吗。 */
  dock: boolean
  /** 当前面板。`null` = 坞开着但还没选（只在坞开着时出现）。 */
  panel: PanelName | null
  /** 这个 tab 已打开过的面板（决定坞标签与徽标数）。 */
  mounted: PanelName[]
  /**
   * 终端缓冲：纯文本（PTY 的原始字节），切走再回来仍在。
   *
   * 缓冲**必须**由宿主持有：xterm 实例随面板卸载（`v-if`），而 §5.5.2
   * 要求「切走再回来时滚动记录还在」。组件只写增量。
   */
  output: string
  /** 文件面板当前目录（相对工作区根）。每项一层。 */
  cwd: string[]
  /** 已列过的层：`相对路径 → 该层条目`。没列过的层没有键（不是「空目录」）。 */
  listings: Map<string, FileListing>
  /** 正在列的层（相对路径）。 */
  loading: Set<string>
  /** 当前打开的文件预览。没打开文件时 `null`。 */
  preview: FilePreview | null
  /** 正在读的文件（相对路径）。 */
  readingPath: string | null
  /** 导航越界 / 读失败（§7.4 ③）。 */
  filesError: FilesError | null
  /** 写越界 / 写失败（§7.4 ④）。**与 `filesError` 分开**：下一步不同。 */
  writeError: FilesError | null
  /** 写操作进行中。 */
  saving: boolean
  /**
   * 编辑区有未保存的改动。
   *
   * §5.5.2 承诺「关 tab 会丢未保存改动」——这个标记是那条承诺的兑现处：
   * 面板报 `dirty-change`，页面把它挂在 tab 上。
   */
  dirty: boolean
  /** 轨迹抽屉开着吗。**页面持有**：TrajDrawer 是受控组件，不自己记开关。 */
  traj: boolean
  /** 打开抽屉的那个元素——关闭后焦点还给它。 */
  trajOpener: HTMLElement | null
  /**
   * 已读到的轨迹事件。
   *
   * **来源是 `GET /api/sessions/{id}/trajectory`，不是 SSE 帧**：帧在断线期间
   * 会丢，而它是库里的行。两个来源混着用会让页面出现两套顺序——刷新前后
   * 看到的还不一样。
   */
  events: TrajectoryEvent[]
  /** 序号断口（Hub 算好的 `missing` 一并带来）。 */
  gaps: TrajectoryGap[]
  /** 最后一轮没有 `turn_end`：还在跑，或结尾丢了。 */
  incomplete: boolean
  /** 轨迹还有下一页吗。由「上一页取满没有」推出（Hub 不给总数）。 */
  hasMore: boolean
  /** 正在读轨迹。防重入：每来一帧都发一次请求会把 Hub 打满。 */
  trajLoading: boolean
  /** 读轨迹失败的原因。空态与「读不到」是两件不同的事，要分开说。 */
  trajError: string | null
  /**
   * 对话通道最近一次失败的错误码。
   *
   * 只用来**显示原因**——它不拦发送（判据只有 `agent_id`，见 `ConvFlow`）：
   * agent 不可用是设备侧当时的状态，过一会可能就好了，留着重试的路比替用户
   * 判定「这次不行」有用。
   */
  convError: string | null
  /** 每个面板的通道视图（状态 / 错误 / 丢弃数）。 */
  views: Record<PanelName, ChannelView>
}

const workspaces = reactive(new Map<string, TabWorkspace>())

function blankWorkspace(): TabWorkspace {
  return {
    dock: false,
    panel: null,
    mounted: [],
    output: '',
    cwd: [],
    listings: new Map(),
    loading: new Set(),
    preview: null,
    readingPath: null,
    filesError: null,
    writeError: null,
    saving: false,
    dirty: false,
    traj: false,
    trajOpener: null,
    events: [],
    gaps: [],
    incomplete: false,
    hasMore: false,
    trajLoading: false,
    trajError: null,
    convError: null,
    views: {
      terminal: { state: 'idle', error: null, dropped: 0 },
      files: { state: 'idle', error: null, dropped: 0 },
    },
  }
}

function workspaceOf(id: string): TabWorkspace {
  let ws = workspaces.get(id)
  if (!ws) {
    ws = blankWorkspace()
    workspaces.set(id, ws)
  }
  return ws
}

/** 补上缺失的 tab 状态。**不在 computed 里建**：那会在求值过程中写回依赖的 Map。 */
function ensureWorkspaces(list: TabMeta[]): void {
  for (const tab of list) {
    if (!workspaces.has(tab.id)) workspaces.set(tab.id, blankWorkspace())
  }
}

const activeWs = computed<TabWorkspace | null>(() =>
  tabs.value.activeId ? (workspaces.get(tabs.value.activeId) ?? null) : null,
)

const panelCounts = computed<Record<string, number>>(() => {
  const out: Record<string, number> = {}
  for (const tab of tabs.value.tabs) out[tab.id] = workspaces.get(tab.id)?.mounted.length ?? 0
  return out
})

/**
 * 每个 tab 的脏标记。
 *
 * §5.5.2 明写「关 tab 会丢未保存改动」：这句话只有在 tab 上**看得见**
 * 时才成立，否则它只是一句写在文档里的免责声明。徽标由 FilesPanel 的
 * `dirty-change` 喂进来，页面把它挂在 tab 上——面板自己不持久化（它在
 * 切 tab 时被 `v-if` 卸掉，留不住）。
 */
const dirtyTabs = computed<Record<string, boolean>>(() => {
  const out: Record<string, boolean> = {}
  for (const tab of tabs.value.tabs) out[tab.id] = workspaces.get(tab.id)?.dirty ?? false
  return out
})

const channelView = computed<ChannelView>(() => {
  const ws = activeWs.value
  const panel = ws?.panel
  if (!ws || !panel) return { state: 'idle', error: null, dropped: 0 }
  return ws.views[panel]
})

/* ---------------------------------------------------------------- 通道 */

let hub: ChannelHub | null = null
/** `tabId\\u0000panel` → 句柄。不放进 reactive：句柄内部状态由 views 镜像。 */
const channels = new Map<string, ChannelHandle>()
const channelUnsubs = new Map<string, () => void>()
/** `tabId\\u0000panel` → 帧订阅的退订函数。与状态订阅分开存，各自退订。 */
const frameUnsubs = new Map<string, () => void>()

function channelKey(tabId: string, panel: PanelName): string {
  return `${tabId}\u0000${panel}`
}

/**
 * exec 的序号：每条命令一条通道，所以键里要有序号。
 *
 * 带序号**不是**为了给同一条命令去重——同一条命令在不同时刻跑是不同的
 * 事实；是为了让两条同时发出的命令不撞进同一条通道（`exec.rs`：`spent`
 * 之后不接受第二帧，重用它必然报「这条通道是一次性的」）。
 */
let execSeq = 0

/**
 * 活着的 exec 句柄。
 *
 * 它们**不进 `channels`**：那条表的键是 `tabId␀panel`，而 exec 的键里有
 * 序号，一个 tab 名下可以同时有多条。混进去会让「这个 tab 有哪些面板」
 * 与「这个 tab 有哪些通道」变成两张对不上的账——面板数徽标正是按
 * `mounted.length` 算的。
 */
const execHandles = new Set<ChannelHandle>()

/**
 * 对话通道句柄（`tabId␀chat` → 句柄）。
 *
 * **不进 `channels`**：那条表的键是 `tabId␀panel`，而 `chat` 不是一个面板
 * （它挂在对话列、没有坞位）。混进去会让「这个 tab 有哪些面板」与「这个 tab
 * 有哪些通道」变成两张对不上的账——面板数徽标正是按 `mounted.length` 算的。
 */
const chatHandles = new Map<string, ChannelHandle>()

/**
 * 这张表还剩几条，**只给测试看**（`SessionPage.test.ts`）。
 *
 * 对话释放不能只靠 DELETE 断言：`close()` 发 DELETE，而「删掉表项」不发任何
 * 请求。于是「关了但没删」在网络上与「关了且删了」**一模一样**——只断言
 * DELETE 的用例在漏掉 `delete` 时照样绿（本里程碑实测过）。
 * 这里只读一个数，不暴露句柄，也不给测试改状态的入口。
 */
function chatHandleCount(): number {
  return chatHandles.size
}

defineExpose({ chatHandleCount })

function ensureChannel(tab: TabMeta, panel: PanelName): ChannelHandle {
  const key = channelKey(tab.id, panel)
  const existing = channels.get(key)
  if (existing) return existing
  if (!hub) throw new Error('channel hub is not mounted')
  const handle = hub.handle(key, {
    sessionId: tab.id,
    connector: panel,
    // terminal 的 cwd 跟着会话走（约束 3）：Hub 只透传 params，node 不会替
    // 页面猜一个工作区根。尺寸用 `lib/channel.ts` 的那一份常量——面板量不到
    // 尺寸时用它当 xterm 初值，这里用它当下发值，两处各写一份必然漂。
    params:
      panel === 'terminal' ? { cwd: tab.path, cols: DEFAULT_COLS, rows: DEFAULT_ROWS } : {},
  })
  channels.set(key, handle)
  const ws = workspaceOf(tab.id)
  const unsub = handle.onState((state: ChannelState) => {
    ws.views[panel].state = state
    ws.views[panel].error = handle.errorCode()
    ws.views[panel].dropped = handle.dropped()
    // 会话结束 / 断连把这条通道标成 `closed`（`ChannelHub.onSessionState`）。
    // 此时若还有一次保存在飞，它的 `write` 回帧永远不会来——通道都关了。
    // 不复位 `saving` 的话保存按钮永久禁用（`:disabled="dirty || saving || ended"`），
    // 而用户能看到的唯一线索是「⌃S 按了没反应」。静默的按钮比一条错更难查。
    if (state === 'closed') {
      ws.saving = false
      ws.loading.clear()
      ws.readingPath = null
    }
    // 通道刚建好：把想问而没问成的那一层补上（files 的取数不走 `send` 的
    // 丢弃语义，见 `pendingFiles` 头注释）。终端不需要这一步——它没有
    // 「打开时该问的第一件事」，输出是设备自己推的。
    if (state === 'ready' && panel === 'files') filesKick(tab)
  })
  channelUnsubs.set(key, unsub)
  const unframe = handle.onFrame((frame) => onPanelFrame(tab.id, panel, frame))
  frameUnsubs.set(key, unframe)
  return handle
}

/**
 * 面板通道的帧落到 tab 的那份状态上。
 *
 * 这是**唯一的解帧处**。三个 connector 各有自己的形状，混在一处解是为了
 * 让「这条数据最后落到哪儿」只有一张表：
 *
 * | connector | 帧形状 | 落点 |
 * |---|---|---|
 * | terminal | `{ bytes }`（可能是半条控制序列） | `ws.output`（append-only） |
 * | files | `{ op: 'list' \| 'read' \| 'write' \| 'error', … }` | `ws.listings` / `ws.preview` / `ws.writeError` |
 * | exec | `{ exit_code, stdout_b64, … }` | 执行结果（一次性，回完即关） |
 *
 * **真 files connector 到位后这里是唯一要改的地方**——形状已经按
 * `node-core/src/connector/files.rs` 的回帧写好了，验一次即可。
 */
function onPanelFrame(tabId: string, panel: PanelName, frame: ChannelFrame): void {
  const ws = workspaces.get(tabId)
  if (!ws) return
  if (panel === 'terminal') {
    const bytes = payloadString(frame.payload, 'bytes')
    if (bytes !== null) ws.output += bytes
    return
  }
  if (panel === 'files') {
    applyFileFrame(ws, frame.payload)
  }
}

function payloadString(payload: unknown, key: string): string | null {
  if (typeof payload !== 'object' || payload === null) return null
  const v = (payload as Record<string, unknown>)[key]
  return typeof v === 'string' ? v : null
}

/**
 * 第一次与面板正文交互：这时候才建通道。
 *
 * 失败后再点一次是**重试**：`open()` 在 `failed` 态下会重新发一次
 * `POST /api/channels`，而不是永远停在失败上（设备可能刚回来）。
 */
function interact(panel: PanelName): void {
  const tab = activeTab.value
  if (!tab || ended.value) return
  if (consumeSelfFocus()) return
  const handle = ensureChannel(tab, panel)
  const state = handle.state()
  if (state === 'idle' || state === 'failed' || state === 'closed') {
    void handle.open().catch(() => {
      // 状态已经由句柄写回 views（failed + 错误码），这里不再二次处理
    })
  }
}

/** 页面自己刚把焦点放进面板正文时，那一次 focusin 不是用户交互。 */
let selfFocused: HTMLElement | null = null

function consumeSelfFocus(): boolean {
  const ae = document.activeElement
  if (selfFocused && ae === selfFocused) {
    selfFocused = null
    return true
  }
  return false
}

/**
 * 释放一个面板的通道。**只在关面板 / 关 tab 时调**，切 tab 不调。
 */
async function releaseChannel(tabId: string, panel: PanelName): Promise<void> {
  const key = channelKey(tabId, panel)
  const handle = channels.get(key)
  if (!handle) return
  channels.delete(key)
  const unsub = channelUnsubs.get(key)
  if (unsub) {
    unsub()
    channelUnsubs.delete(key)
  }
  const unframe = frameUnsubs.get(key)
  if (unframe) {
    unframe()
    frameUnsubs.delete(key)
  }
  const ws = workspaces.get(tabId)
  if (ws) {
    ws.views[panel] = { state: 'closed', error: null, dropped: 0 }
    if (ws.panel === panel) ws.panel = null
  }
  await handle.close()
}

/** 释放一个 tab 的全部通道。**这是生命周期边界**（§5.5.1）。 */
async function releaseTab(tabId: string): Promise<void> {
  const ws = workspaces.get(tabId)
  const panels = ws ? [...ws.mounted] : (['terminal', 'files'] as PanelName[])
  for (const panel of panels) await releaseChannel(tabId, panel)
  // 对话通道不挂在 `ws.mounted` 上（它在对话列、没有坞位），所以必须在这里
  // 单独释放：`releaseChannel` 按面板名找键，永远找不到 `tabId␀chat`。
  // 顺序与 `releaseChannel` 一致——先删表再 `close()`：只关不删的话，键还在，
  // 关掉 tab 后重开同一个会话会拿回一个已经 `closed` 的句柄，`open()` 不再生效。
  const chatKey = `${tabId}\u0000chat`
  const chat = chatHandles.get(chatKey)
  if (chat) {
    chatHandles.delete(chatKey)
    await chat.close()
  }
  // 关 tab 时还挂在窗口里的轨迹重读也要撤掉：它读的是一个已经不在工作区里的
  // 会话，读回来的结果只会写进马上要丢掉的 `ws`。
  const trajPending = trajTimers.get(tabId)
  if (trajPending !== undefined) {
    clearTimeout(trajPending)
    trajTimers.delete(tabId)
  }
}

/* ---------------------------------------------------------------- 面板坞 */

const split = ref(SPLIT_DEFAULT)

function openPanel(panel: PanelName): void {
  const ws = activeWs.value
  const tab = activeTab.value
  if (!ws || !tab) return
  if (ended.value) return
  ws.dock = true
  ws.panel = panel
  if (!ws.mounted.includes(panel)) ws.mounted.push(panel)
  // 焦点进面板正文（用户按 `o` 的意图是去用那个面板，不是打开一个壳）
  void nextTick(() => focusPanelBody())
}

/**
 * 把焦点交给面板正文，并**让那次 focusin 不算交互**。
 *
 * 面板正文的「第一次交互」是建通道的判据，而「聚焦」也算交互
 * （键盘用户 Tab 进面板就是要用它，通道该在那时开始建，好赶在他敲下
 * 第一个字符之前就绪）。但这里是**页面自己**把焦点放进去的：用户按的是
 * `o`，他碰的是坞，不是面板内容。不做区分的话「打开坞」立刻变成
 * 「建一条 PTY」——懒建立就名存实亡了。
 */
function focusPanelBody(): void {
  const el = dockEl.value?.querySelector<HTMLElement>('[data-panel-focus]')
  if (!el) return
  selfFocused = el
  el.focus()
  // `focus()` 没真的移动焦点时（元素不可聚焦）标记会留下，于是下一次
  // 真实交互被它吞掉。下一帧清一次，把这条兜住。
  queueMicrotask(() => {
    if (selfFocused === el) selfFocused = null
  })
}

function otherPanel(name: PanelName): PanelName {
  return name === 'terminal' ? 'files' : 'terminal'
}

function cyclePanel(delta: number): void {
  const ws = activeWs.value
  if (!ws) return
  if (!ws.dock) {
    openPanel(ws.panel ?? 'terminal')
    return
  }
  const order = ws.mounted
  if (!order.length) {
    openPanel('terminal')
    return
  }
  if (order.length < 2) {
    // 只挂过一个面板时 `[` / `]` 是**开另一个**（原型 `cyclePanel` 的同一条）：
    // 在这两个键的语义里「切换」包括了「把另一个拉进坞」——否则坞里永远
    // 只有终端，而「坞能同时放两个」这条（§5.4）在键盘上就走不到。
    openPanel(otherPanel(ws.panel ?? order[0] ?? 'terminal'))
    return
  }
  const idx = ws.panel ? order.indexOf(ws.panel) : 0
  const next = order[(idx + delta + order.length) % order.length] as PanelName
  openPanel(next)
}

function toggleDock(): void {
  const ws = activeWs.value
  if (!ws) return
  if (ws.dock) closeDock()
  else openPanel(ws.panel ?? 'terminal')
}

/**
 * 关面板坞 = 释放它托管的面板（§7.2 的「退出即释放」）。
 * 焦点回到「打开面板」按钮——那是打开它的触发元素，也是用户接下来
 * 最可能再按一次的地方。
 */
function closeDock(): void {
  const ws = activeWs.value
  const tab = activeTab.value
  if (!ws || !tab) return
  const panels = [...ws.mounted]
  ws.dock = false
  ws.panel = null
  ws.mounted = []
  for (const panel of panels) void releaseChannel(tab.id, panel)
  void nextTick(() => openButtonEl.value?.focus())
}

/**
 * 终端输入 → PTY。
 *
 * `interact` 在 `onData` 里已经由组件报过一次（它挂在 `onData` 而不是
 * `focusin` 上，理由见组件头），这里再报一次是**幂等**的：`open()` 在
 * `ready` 下直接返回。
 *
 * `send()` 在通道没建好时**排队**而不是丢弃（`channel.ts` 的 `pendingText`），
 * 所以「点进终端就敲第一个键」这个最常见的第一个动作不丢字符：通道在那时
 * 开始建，字符等它建好后按原顺序补发。
 */
function terminalInput(text: string): void {
  const tab = activeTab.value
  if (!tab || ended.value || deviceRemoved.value) return
  const handle = ensureChannel(tab, 'terminal')
  if (handle.state() !== 'ready') interact('terminal')
  handle.send(text)
}

/** 尺寸协商的结果 → `TIOCSWINSZ`（走控制帧，不新增消息类型）。 */
function terminalResize(size: TerminalSize): void {
  const tab = activeTab.value
  if (!tab) return
  const handle = channels.get(channelKey(tab.id, 'terminal'))
  handle?.resize(size.cols, size.rows)
}

/** 对话流里选了「终端」这个目标：等于在终端里敲这一行。 */
function convSend(text: string): void {
  terminalInput(text)
}

/* ------------------------------------------------------------------ 对话（ACP） */

/**
 * 对话通道：`ensureChannel` 会按 `tabId␀panel` 记账，而 `acp` 不是面板
 * （它挂在对话列、没有坞位）。所以单独建，键固定带 `chat`。
 *
 * **懒建立**：说第一句话才开（`ensureChatChannel`）。打开会话页就开一条，
 * 会让「打开过一个会话」变成「在那台设备上起过一个 agent 子进程」——
 * 而用户可能只是路过。
 */
function ensureChatChannel(tab: TabMeta): ChannelHandle {
  const key = `${tab.id}\u0000chat`
  const existing = chatHandles.get(key)
  if (existing) return existing
  if (!hub) throw new Error('channel hub is not mounted')
  const handle = hub.handle(key, {
    sessionId: tab.id,
    connector: 'acp',
    openVia: 'conversation',
  })
  chatHandles.set(key, handle)
  const ws = workspaceOf(tab.id)
  // 失败的原因要看得见：输入框已经清空而没有回音时，用户唯一能读到的是这句。
  // 通道是在**发第一句话时**才开的，所以这里只显示结果，不参与能不能发的判据。
  handle.onState(() => {
    ws.convError = handle.errorCode()
  })
  // 对话的帧只用来判「还在动」：内容一律读轨迹表（`GET .../trajectory`），
  // 因为帧在断线期间会丢，而轨迹是库里的。页面不靠帧补历史。
  // 读的动作走 `scheduleTrajectory`：一次回复的帧是成批到的，每帧一读会把
  // 请求数抬到帧数那么多，而新内容往往只有几个字。
  handle.onFrame(() => {
    scheduleTrajectory(tab.id)
  })
  return handle
}

/** 说一句话：交给设备的 ACP 子进程。回复从事件流回来，不在这里等。 */
function convSay(text: string): void {
  const tab = activeTab.value
  if (!tab || ended.value || !hub) return
  // 设备已被删除：「说第一句话才开通道」这条懒建立会当场去开一条，
  // 而它唯一的结果是 `device_removed` 409。拦在这里，判据与横幅同一份。
  if (deviceRemoved.value) return
  const handle = ensureChatChannel(tab)
  void handle.open().catch(() => undefined)
  handle.send(text)
}

/**
 * 重读轨迹。
 *
 * **每次有帧到达就重读一次**，而不是把帧直接拼进列表：帧是「此刻流过的事」，
 * 断线期间会丢；轨迹是「库里的事实」，两者拼起来会有两套顺序。页面只信后者。
 *
 * `trajLoading` 只挡住**同时**发起的读。同一 tick 里涌进来的几帧确实会被它
 * 合成一次，但真机上帧是隔几百毫秒到的：上一读已完成，下一帧又是一次整表重读，
 * 长回复的请求数仍≈帧数。跨帧的合并要在 [`scheduleTrajectory`] 里做。
 */
async function refreshTrajectory(tabId: string): Promise<void> {
  const ws = workspaces.get(tabId)
  if (!ws) return
  if (ws.trajLoading) return
  ws.trajLoading = true
  try {
    const view = await getTrajectory(tabId)
    ws.events = view.events
    ws.gaps = view.gaps
    ws.incomplete = view.incomplete
    ws.hasMore = view.hasMore
    ws.trajError = null
  } catch (err) {
    // 说清为什么空：一个空抽屉会被读成「这个会话没有轨迹」，
    // 而「读不到」与「没有」是两件不同的事。
    ws.trajError = describeReadFailure(err, '轨迹')
  } finally {
    ws.trajLoading = false
  }
}

/**
 * 帧驱动的那次重读：**尾沿合并**，不是每帧一发。
 *
 * 为什么需要合并：agent 流式输出时设备把 `session/update` 聚成 `batch` 推上来，
 * 一次长回复会送上几十上百帧。每帧读一次整表，请求数就等于帧数——而每一帧
 * 带来的新内容往往只有几个字。读的次数降下来，页面看到的仍是「库里的事实」。
 *
 * 为什么是尾沿（reset 定时器）而不是节流：突发结束后**必定**还多读一次，
 * 最后一次写入不会因为落在窗口里被吞掉；节流会留下「最后一帧没读」的窗口。
 *
 * 定时器按 tab 分开记：两个会话并排时，一个的突发不该推迟另一个的可读时间。
 * 卸载时要清掉（见 `onBeforeUnmount`），否则定时器会打到已卸载的页面状态上。
 */
const TRAJ_DEBOUNCE_MS = 120
const trajTimers = new Map<string, ReturnType<typeof setTimeout>>()

function scheduleTrajectory(tabId: string): void {
  const pending = trajTimers.get(tabId)
  if (pending !== undefined) clearTimeout(pending)
  trajTimers.set(
    tabId,
    setTimeout(() => {
      trajTimers.delete(tabId)
      void refreshTrajectory(tabId)
    }, TRAJ_DEBOUNCE_MS),
  )
}

/**
 * 「加载更多」：接着最后一行的**物理 id** 往下取，拼在后面。
 *
 * 不用 `seq` 当游标（见 `getTrajectory` 的注释），也不用 `events.length` 当
 * `after`——那是「我已经有几条」，而游标要的是「库里读到第几个 id 了」；
 * 中途有新事件写进来的话这两个数不是一个意思。
 */
async function onTrajMore(): Promise<void> {
  const ws = activeWs.value
  const tab = activeTab.value
  if (!ws || !tab || ws.trajLoading) return
  const last = ws.events.at(-1)
  ws.trajLoading = true
  try {
    const view = await getTrajectory(tab.id, last ? last.id : 0)
    // 按 id 去重：Hub 可能把已读过的那一页又给一次（例如重连后重读），
    // 不去重就会把同一条事件画两遍。
    const seen = new Set(ws.events.map((e) => e.id))
    ws.events = [...ws.events, ...view.events.filter((e) => !seen.has(e.id))]
    ws.gaps = view.gaps
    ws.incomplete = view.incomplete
    ws.hasMore = view.hasMore
    ws.trajError = null
  } catch (err) {
    ws.trajError = describeReadFailure(err, '轨迹')
  } finally {
    ws.trajLoading = false
  }
}

/** 抽屉打开时读一次：不读的话用户看到的是「还没有事件」，而他明明说过话。 */
watch(
  () => activeWs.value?.traj ?? false,
  (open) => {
    const tab = activeTab.value
    if (open && tab) void refreshTrajectory(tab.id)
  },
)

const dockEl = ref<HTMLElement | null>(null)
const openButtonEl = ref<HTMLButtonElement | null>(null)

/* ---------------------------------------------------------------- 轨迹抽屉 */

/**
 * 开 / 关轨迹抽屉。
 *
 * 开关**由页面持有**（`ws.traj`）：TrajDrawer 是受控组件，不自己记状态，
 * 三条关闭路径（Esc / 遮罩 / 关闭按钮）都只 emit `close`。所以这里唯一要
 * 做的就是把 `open` 置回 `false`——焦点已由组件 `pop()` 还完了。
 *
 * `opener` 也由页面持有：抽屉要把它交还给焦点栈，而按 `t` 键时「触发元素」
 * 是键盘而不是某个按钮——那时传 `null`，焦点栈退到 `main[tabindex="-1"]`。
 */
function toggleTraj(opener: HTMLElement | null): void {
  const ws = activeWs.value
  if (!ws) return
  if (ws.traj) {
    ws.traj = false
    ws.trajOpener = null
    return
  }
  ws.traj = true
  ws.trajOpener = opener
}

function closeTraj(): void {
  const ws = activeWs.value
  if (!ws) return
  ws.traj = false
  ws.trajOpener = null
}

/* ---------------------------------------------------------------- 文件取数 */

/**
 * 文件面板要东西的入口。**面板不持通道**（组件头注释），所以「谁去取」这一步
 * 在页面：把意图包成通道数据帧送下去。
 *
 * 意图 → op 的对应关系与 node 侧 `files.rs` 的入帧（`op = "list" | "read" |
 * "write"`）逐字一致（`crates/node-core/src/connector/files.rs` 的
 * `on_data` 只认**顶层** `op`）。将来换成别的搬运方式只改这里，面板不知道。
 *
 * **必须发 `application/json`**：Hub 的 `/send` 按 Content-Type 分派
 * （`crates/hub-server/src/routes/stream.rs`），只有 JSON 会原样成为
 * `channel_data.payload`；终端的 `application/octet-stream` 会被包成
 * `{"bytes":…}`，files connector 收不到 `op`。
 *
 * 不走 `ChannelHandle.send()`：那条路的语义是「终端敲击」——断线时丢弃并
 * 计数。一次 `list` 被丢就再也没人回应它，而「有 N 条已丢」那个徽标在说
 * 取数这件事时用户根本读不懂（见 `pendingFiles`）；所以取数自己发请求，
 * 失败时把原因写进 `ws.filesError`（§7.4 第 ③ 路）。
 *
 * **只应对已 `ready` 的通道**：未 ready 的那一次由 `pendingFiles` 记下，
 * 等通道 ready 时补发。
 */
function fileRequest(tab: TabMeta, op: 'list' | 'read' | 'write', payload: Record<string, unknown>): void {
  const handle = channels.get(channelKey(tab.id, 'files'))
  if (!handle || handle.state() !== 'ready') return
  const id = handle.id()
  if (!id) return
  void request(`/api/channels/${encodeURIComponent(id)}/send`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ op, ...payload }),
  }).catch((err: unknown) => {
    // 取数被 Hub 拒（404 通道没了 / 400 载荷不合法 / 403 CSRF）时页面必须
    // 说话：文件面板没有第二条反馈路径，静默失败就是一棵永远不动的空树。
    const ws = workspaces.get(tab.id)
    if (!ws) return
    ws.loading.clear()
    ws.readingPath = null
    ws.saving = false
    ws.filesError = {
      path: typeof payload.path === 'string' ? payload.path : '',
      reason:
        err instanceof ApiError
          ? `Hub 拒绝了这次文件请求（${err.code}）。`
          : '文件请求没能送到 Hub，面板内容可能已经过期。',
      code: err instanceof ApiError ? err.code : err instanceof NetworkError ? 'network' : null,
    }
  })
}

/**
 * 通道还没 `ready` 时想做而没做成的那一次取数（`tabId␀files` → 意图）。
 *
 * 只留最后一个：files 的 `list` 是一次一层，而用户连着展开两层时，列旧的那
 * 层没有意义——屏幕上已经不在那儿了。`read` 同理。
 *
 * **为什么必须存在**：`send()` 在未 ready 时静默丢弃并计数（`channel.ts`）。
 * 那是终端敲击的正确语义（丢几个字符比乱序灌进 PTY 好查），不是取数的——
 * 一次 `list` 被丢就再也没人回应它，而「有 N 条已丢」那个徽标在说取数这件
 * 事时用户根本读不懂。所以取数改成「记下来，等 ready 时补发」。
 */
const pendingFiles = new Map<string, { op: 'list' | 'read'; path: string }>()

/**
 * 把一次取数送出去；通道还没 `ready` 就先记下并去建通道。
 *
 * 交互（第一次与面板正文打交道）与取数在这里是同一次——用户按下第一个键
 * 的意思就是「把这一层给我看」。
 */
function fileAsk(op: 'list' | 'read', path: string): void {
  const tab = activeTab.value
  if (!tab || ended.value) return
  const handle = ensureChannel(tab, 'files')
  if (handle.state() === 'ready') {
    fileRequest(tab, op, { path })
    return
  }
  pendingFiles.set(channelKey(tab.id, 'files'), { op, path })
  interact('files')
}

/**
 * 一次性命令走的是另一条路：它没有「补发」这回事。
 *
 * `exec` 是一次性的，而一条命令**只想要一次**（用户在对话里提交一次命令，
 * 不该因为通道还没建好而在建好后自动再跑一遍）。所以未 ready 时它直接
 * 返回，由用户再按一次——而不是被 `pendingFiles` 悄悄记住。
 */
function fileList(path: string): void {
  const ws = activeWs.value
  if (!ws || ended.value) return
  ws.loading.add(path)
  fileAsk('list', path)
}

function fileRead(path: string): void {
  const ws = activeWs.value
  if (!ws || ended.value) return
  ws.readingPath = path
  ws.preview = null
  ws.writeError = null
  fileAsk('read', path)
}

function fileSave(payload: { path: string; text: string }): void {
  const ws = activeWs.value
  const tab = activeTab.value
  if (!ws || !tab || ended.value) return
  ws.saving = true
  ws.writeError = null
  fileRequest(tab, 'write', payload)
}

/**
 * files 通道刚 `ready`：把攒下的那一次取数补发；没有攒下的就问当前这一层。
 *
 * ### 为什么它必须是异步的
 *
 * 状态回调是从 `channel.ts` 的 `set('ready')` 里**同步**调进来的
 * （`notifyState`）。在这里直接 `send()` 会撞上同一条递归：
 * `send` → `hub.canSend()` 为假 → 计数 → `notifyState` → 本回调 → `send` …，
 * 栈溢出之后那次 `open()` 以 `RangeError` 收尾，通道于是停在 `failed`——
 * 症状是「POST 成功了，面板却写着建立失败（unknown）」。把取数推到微任务
 * 里，让 `set('ready')` 先返回，这条环就断了。
 *
 * ### 为什么没有攒下的也要问一层
 *
 * 挂上文件面板后用户看到的是一棵空树；不主动列，那棵树会一直空着，而它的
 * 空态写着「通道还没建立」——于是**一个已经建好的通道看起来像没建**（§3.12
 * 的两种空被读反）。所以通道一 ready 就把当前层问一遍；已经列过的层不问
 * （面板折叠再展开时数据还在，重问一次只是多一轮往返）。
 */
function filesKick(tab: TabMeta): void {
  void Promise.resolve().then(() => {
    const ws = workspaces.get(tab.id)
    if (!ws) return
    const pending = pendingFiles.get(channelKey(tab.id, 'files'))
    if (pending) {
      pendingFiles.delete(channelKey(tab.id, 'files'))
      fileRequest(tab, pending.op, { path: pending.path })
      return
    }
    const path = cwdPathOf(ws.cwd)
    if (ws.listings.has(path)) return
    ws.loading.add(path)
    fileRequest(tab, 'list', { path })
  })
}

/**
 * files 通道的帧落到 tab 的那份状态上。
 *
 * **真 files connector 到位后这里是唯一要改的地方**：形状已经按
 * `node-core/src/connector/files.rs` 的回帧写好了。
 *
 * 分派按 `op` **逐个 case**，不用一串 `if`：串起来写的时候「顺序即语义」
 * （谁先判谁先赢），而 `op` 是**互斥的键**——一帧只有一个 op，顺序不该
 * 承载任何含义。改成一个 `switch` 之后，加一档 op 是加一个 case，
 * 不会因为在中间插一个分支而悄悄改变另一个 op 的行为。
 */
function applyFileFrame(ws: TabWorkspace, payload: unknown): void {
  const frame = asRecord(payload)
  if (!frame) return
  switch (frame.op) {
    case 'list':
      applyListing(ws, frame)
      return
    case 'read':
      applyPreview(ws, frame)
      return
    case 'write':
      ws.saving = false
      if (frame.ok === false) ws.writeError = errorOf(frame, pathOf(frame) ?? '')
      return
    case 'error':
      ws.loading.clear()
      ws.readingPath = null
      // 保存中出的错归④（写失败），其余归③（导航 / 读失败）：下一步不同
      if (ws.saving) {
        ws.saving = false
        ws.writeError = errorOf(frame, pathOf(frame) ?? '')
        return
      }
      ws.filesError = errorOf(frame, pathOf(frame) ?? '')
      return
    default:
      // 不认识的 op 一律不动状态：node 侧新增 op 时旧页面不该把它解成
      // 某个已存在的 op（约束 14 的前向兼容在页面这一侧的对应物）。
      return
  }
}

/**
 * 一帧里的路径。
 *
 * **`''` 是工作区根目录**（`cwdPathOf([])` 就是它），而**缺 `path` 是坏帧**——
 * 两者不是一回事。缺 `path` 的帧必须丢掉：当成根目录的话，一帧空清单就会
 * 覆盖掉根目录的真清单，用户看到根目录「空了」而没有任何错误提示。
 */
function pathOf(frame: Record<string, unknown>): string | null {
  const p = frame.path
  return typeof p === 'string' ? p : null
}

/** `op:"list"`：一层清单进表，并清掉「正在列这一层」。 */
function applyListing(ws: TabWorkspace, frame: Record<string, unknown>): void {
  const path = pathOf(frame)
  // 坏帧（缺 path）：丢掉。见 `pathOf` 的注释——把它当成根目录会覆盖真清单。
  if (path === null) return
  const listing = frame as Partial<FileListing>
  ws.loading.delete(path)
  ws.listings.set(path, {
    path,
    entries: Array.isArray(listing.entries) ? (listing.entries as FileListing['entries']) : [],
    total: numOrNull(listing.total),
    truncated: listing.truncated === true,
  })
  ws.filesError = null
}

/** `op:"read"`：预览结论进状态，并清掉「正在读这个文件」。 */
function applyPreview(ws: TabWorkspace, frame: Record<string, unknown>): void {
  ws.readingPath = null
  const path = pathOf(frame)
  // 坏帧（缺 path）：丢掉，理由同 `applyListing`。
  if (path === null) return
  const preview = frame as Partial<FilePreview>
  ws.preview = {
    path,
    size: numOrNull(preview.size),
    kind: preview.kind === 'binary' || preview.kind === 'truncated' ? preview.kind : 'text',
    truncated: preview.truncated === true,
    text: typeof preview.text === 'string' ? preview.text : undefined,
    content_b64: typeof preview.content_b64 === 'string' ? preview.content_b64 : undefined,
  }
}

function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === 'object' && v !== null ? (v as Record<string, unknown>) : null
}

/**
 * 取不到就是 `null`，**不拿 0 顶上**。
 *
 * 大小缺省成 0 会编出一个事实：0 字节与「不知道多大」是两件事，而页面对
 * 后者有 `—` 这个说法（`format.ts` 的 `percent(null)` 同口径）。编出来的
 * 0 会被读成「这个空文件是空的」——而真相是「没人告诉我们」。
 */
function numOrNull(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

function errorOf(frame: Record<string, unknown>, path: string): FilesError {
  return {
    path,
    reason: typeof frame.reason === 'string' ? frame.reason : '设备没能完成这次操作。',
    code: typeof frame.code === 'string' ? frame.code : null,
  }
}

/* ---------------------------------------------------------------- 文件意图 */

/** 面包屑：回到第 `depth` 层（`0` = 工作区根）。 */
function filesGoto(depth: number): void {
  const ws = activeWs.value
  if (!ws) return
  ws.cwd = ws.cwd.slice(0, depth)
  ws.filesError = null
}

/**
 * 打开一个节点。
 *
 * 目录 = 进入它（列它的那一层）；越界节点也会走到这里——那就直接把解释
 * 摆出来（§7.4 ①：隐藏它比显示它更让人不安），而真正的判定在设备侧。
 */
function filesOpen(path: string): void {
  const ws = activeWs.value
  if (!ws) return
  const seg = splitPath(path)
  const last = seg[seg.length - 1] ?? ''
  if (isOutsideSeg(last)) {
    ws.filesError = {
      path,
      reason: '这一步跨出了工作区：会话的工作区在建会话时确定，之后不可改。',
      code: 'path_outside_root',
    }
    return
  }
  ws.cwd = seg
  ws.filesError = null
  fileList(path)
}

/**
 * 撤销编辑。
 *
 * 编辑区的内容由 FilesPanel 自己回滚（`draft = original`），这里只清
 * **这一个文件**的写错态：撤销之后那块红字还在的话，会被读成「撤销失败」。
 */
function filesRevert(path: string): void {
  const ws = activeWs.value
  if (ws && ws.writeError?.path === path) ws.writeError = null
}

/** 脏标记 → tab（§5.5.2 的「关 tab 会丢未保存改动」靠它兑现）。 */
function filesDirty(dirty: boolean): void {
  const ws = activeWs.value
  if (ws) ws.dirty = dirty
}

/* ---------------------------------------------------------------- 一次性命令 */

/**
 * 对话流提交了一条命令：这是 exec 通道——不是 PTY、是一次性的。
 *
 * ### 为什么每条命令一条新通道（exec 是一次性的）
 *
 * `exec.rs` 的 `open` **就是执行**：`params` 里带 `command` / `args`，
 * `poll_all` 之外没有第二条产出路径；第二次 `on_data` 明确回「这条通道
 * 是一次性的」。所以「再跑一次」是再 `open_channel`，不是往旧通道再发一帧。
 *
 * ### 结果在哪
 *
 * 第一帧就是全部：`{"exit_code","stdout_b64","stderr_b64","truncated",
 * "duration_ms","timed_out"}`，不存在「还要等地二帧」。这也是它能带上与
 * 那条 §3.14 的结论的原因：一顿极小的一次性 输出是一次回完的。
 */
function runCommand(req: ConvExecRequest): void {
  const tab = activeTab.value
  if (!tab || ended.value || !hub) return
  // exec 与对话同一条发送路：设备没了，`open()` 就是一次 `device_removed`。
  if (deviceRemoved.value) return
  const key = `${tab.id}\u0000exec\u0000${(execSeq += 1)}`
  const handle = hub.handle(key, {
    sessionId: tab.id,
    connector: 'exec',
    params: { command: req.command, args: req.args, cwd: req.cwd },
  })
  execHandles.add(handle)
  // 第一帧就是全部（exec 一次性、没有「等第二帧」这回事）：收完即关。
  //
  // **关通道是幂等的**：`ChannelHandle.close()` 自己吞掉 DELETE 的失败
  // （404 = 通道已经不在了，409 = 设备那边先关的），而且第二次 `close()`
  // 因为 `channelId` 已经清空而不会再发 DELETE。所以 node 侧「跑完就关」
  // 与页面这里「收到结果就关」撞在一起时，谁先谁后都不会报错、也不会双关。
  handle.onFrame(() => {
    execHandles.delete(handle)
    void handle.close()
  })
  void handle.open().catch(() => {
    execHandles.delete(handle)
  })
}

/* ---------------------------------------------------------------- 工作台头 */

const title = computed(() => (current.value ? sessionTitle(current.value) : '会话工作台'))
/**
 * 设备名。**共用 `lib/format.ts` 的那一份**：本页与会话列表页此前各写一遍
 * `devicesById.get(id)?.name ?? id`，改兜底策略要改两处，漏一处则同一个
 * 设备在两页显示不同的名字。
 */
const deviceLabel = computed(() =>
  current.value
    ? deviceLabelOf(
        devicesById.value,
        current.value.workspace.node_id,
        current.value.workspace.removed_node_id,
      )
    : '—',
)
const startedLabel = computed(() =>
  current.value ? `${relativeTime(current.value.created_at)}开始` : '',
)

/**
 * 工作台头的 agent 标记。
 *
 * `SessionSummary` 只带 `agent_id`（没有 agent 的显示名），所以选过 agent 时
 * 显示的就是那个 id——与 `SessionTable` 的 agent 列同一份数据、同一个口径。
 * 没有 agent 时说的是**此刻的事实**：这个会话没有 agent，因而不能对话
 * （判据与 `ConvFlow` 的 `:agent-id` 同一个值）。
 */
const agentLabel = computed(() => {
  const id = current.value?.agent_id
  return id ? id : '未选 agent（不能对话）'
})

/**
 * 会话工作区路径：面板 / 抽屉的「工作区」「根」都用它。
 *
 * 路径是**会话的字段**（§7.0：工作区不是路由），没有第二个根，所以它是
 * 一个值而不是一个选择器。
 */
const rootPath = computed(() => current.value?.workspace.path ?? '')

/**
 * 设备连接态。**45s 的判定在 Hub**（`security-and-lifecycle.md` §5），
 * 页面只吃结论：终端按它决定冻不冻结（§7.3 的三态表）。
 */
const conn = computed<'online' | 'reconnecting' | 'offline'>(() =>
  current.value ? connOf(current.value.workspace.node_id) : 'offline',
)

/** 终端面板要的那份通道视图（形状与 `PanelDock` 的 `ChannelView` 一致）。 */
const terminalView = computed(() => activeWs.value?.views.terminal ?? channelView.value)

/** 文件面板的通道建好了吗。没建好时不能把空树说成「目录是空的」。 */
const filesReady = computed(() => activeWs.value?.views.files.state === 'ready')

/**
 * 终端通道可用吗。ConvFlow 的「终端」目标能不能选，判据就是它。
 *
 * 看的是**通道状态**而不是「坞开没开」：坞开着但通道还没建（懒建立）时
 * 送过去是一次 `send()` 丢弃，而那丢得无声无息。
 */
const terminalReady = computed(() => activeWs.value?.views.terminal.state === 'ready')

/* ---------------------------------------------------------------- tab 操作 */

const tabBarEl = ref<InstanceType<typeof TabBar> | null>(null)
const bodyEl = ref<HTMLElement | null>(null)

function metaOf(session: SessionSummary): TabMeta {
  return {
    id: session.id,
    title: sessionTitle(session),
    path: session.workspace.path,
    nodeId: session.workspace.node_id,
  }
}

/**
 * 打开页时路由指向一个**会话列表里没有的 id**（深链接打错 / 会话已被删）。
 *
 * 它也要有一条 tab 元信息，页面才能说「这个会话不存在」而不是一片空白。
 * 但它的 `nodeId` **不能写 `null`**：`TabMeta.nodeId` 的 `null` 已经被
 * 「这台设备已被删除」占用（见 `TabMeta` 与本文件 `connOf`），而这条 id
 * 根本还没有会话、更谈不上归属设备。两个事实共用一个 `null` 的后果是
 * 具体的：`loadTabs` 的校验据 `typeof nodeId === 'string'` 拦掉它，于是
 * 这条 tab 从 sessionStorage 读回来时被静默丢掉——刷新一次「找不到」的
 * 结论就变成「没有打开任何东西」。
 *
 * `path` 空串是「未知」（这条 id 没有工作区），`nodeId` 空串是同一个意思：
 * 空串不指向任何设备，`deviceOf('')` 得 `null` → `connOf('')` 退到
 * `offline`，与「设备已删除」那一态在类型上仍然分得开。
 */
function unknownTabMeta(id: string): TabMeta {
  return { id, title: `会话 ${id}`, path: '', nodeId: '' }
}

/** 一个 tab 的设备归属是否已知。判据只看 `nodeId`，不认识 `path` / `title`。 */
function isKnownTab(tab: TabMeta): boolean {
  return tab.nodeId !== ''
}

/**
 * 「这个会话不存在」这句话为什么。
 *
 * 归属未知（`isKnownTab` 为假）时说得出**是哪一种**不知道：这条 id 不在会话
 * 列表里，所以它也可能是「那一行存在但我们没读到」。两种原因都指向同一个
 * 下一步（回列表），但原因写错会让人去查错的东西——「设备被删」与「链接打错」
 * 是两个人做两件事。
 */
const unknownWhy = computed(() => {
  const tab = activeTab.value
  if (!tab || isKnownTab(tab)) {
    return '它可能已经被删掉，或者这条链接指向一个从未建成的会话。回到会话列表看看现在有哪些。'
  }
  return `会话 ${tab.id} 不在会话列表里，所以它的归属设备也不得而知——这不是「设备已被删除」。这条链接可能打错了，或者它指向的会话已被删掉。回到会话列表看看现在有哪些。`
})

/** 地址栏跟随当前 tab，但**不制造历史条目**（见文件头）。 */
function syncUrl(id: string): void {
  if (typeof history === 'undefined') return
  history.replaceState(null, '', `/sessions/${encodeURIComponent(id)}`)
}

/**
 * 切 tab：换掉整份会话状态，然后把焦点交给工作台正文。
 *
 * 不碰通道——`releaseChannel` 只出现在 `closeDock` / `closeOneTab` 里。
 */
function activate(id: string): void {
  const tab = tabs.value.tabs.find((t) => t.id === id)
  if (!tab) return
  tabs.value = activateTabState(tabs.value, id)
  ensureWorkspaces(tabs.value.tabs)
  saveTabs(storage(), tabs.value)
  syncUrl(id)
  void nextTick(() => bodyEl.value?.focus())
}

function closeOneTab(id: string): void {
  const wasActive = tabs.value.activeId === id
  const result = closeTab(tabs.value, id)
  void releaseTab(id)
  workspaces.delete(id)
  tabs.value = result.state
  saveTabs(storage(), tabs.value)

  if (!result.nextActive) {
    navigate('/sessions')
    return
  }
  if (wasActive) {
    syncUrl(result.nextActive)
    void nextTick(() => {
      bodyEl.value?.focus()
      // 落点 tab 的名字按钮也要能被读到：焦点必须在能解释「发生了什么」的地方
      tabBarEl.value?.focusTab(result.nextActive as string)
    })
  } else {
    // 关的不是当前 tab：焦点留在原处，但按钮可能已经不在 DOM 里了
    void nextTick(() => {
      if (!document.contains(document.activeElement)) {
        tabBarEl.value?.focusTab(tabs.value.activeId ?? result.nextActive ?? '')
      }
    })
  }
}

function storage(): Storage | null {
  try {
    return typeof sessionStorage === 'undefined' ? null : sessionStorage
  } catch {
    return null
  }
}

/* ---------------------------------------------------------------- 键盘 */

const HANDLERS: Readonly<Record<string, () => boolean>> = {
  'panel:open': () => {
    if (!activeTab.value) return false
    openPanel(activeWs.value?.panel ?? 'terminal')
    return true
  },
  'panel:prev': () => {
    if (!activeTab.value) return false
    cyclePanel(-1)
    return true
  },
  'panel:next': () => {
    if (!activeTab.value) return false
    cyclePanel(1)
    return true
  },
  'dock:toggle': () => {
    if (!activeTab.value) return false
    toggleDock()
    return true
  },
  /**
   * 轨迹抽屉的开关。
   *
   * 键位**必须在页面注册**：`keynav.register` 拒绝重复注册，而页面可能被
   * 挂载多次（测试、重挂载），所以 `TrajDrawer` 自己不注册（见它文件头
   * 「键位不在这里注册」）。`t` 与 `g g` 是同一个 act 的两个键位，
   * `g g` 由 `shell-keys.ts` 的 `g` 前缀表派发到同一个 act——所以注册
   * 一次就两个键位都活。
   */
  'traj:toggle': () => {
    if (!activeTab.value) return false
    toggleTraj(null)
    return true
  },
}

function onKeydown(e: KeyboardEvent): void {
  if (e.defaultPrevented || e.isComposing) return
  if (e.ctrlKey || e.metaKey || e.altKey) return
  // 单键在输入区内不抢（§4.2）：终端 / 对话输入框里按 o 是在打字
  if (isTyping(e.target)) return
  // 键位 → act **从 `keynav.ts` 反查**，不自己抄一份：抄的那份与 `KEYS`
  // 漂了时，`?` 面板写着一个键、页面认着另一个键（按面板上的提示没反应）。
  const act = actForKey(e.key)
  if (!act) return
  // 只有会话工作台域的键归本页管：`list:*` / `focus:*` / `file:*` 由各自的
  // 模块处理，全局键由外壳处理。判据是表里的 scope，不是一份另抄的清单。
  if (bindingOf(act)?.scope !== 'session') return
  dispatch(act, e)
}

/* ---------------------------------------------------------------- 生命周期 */

async function load(): Promise<void> {
  loading.value = true
  loadError.value = null
  try {
    const [list, devs] = await Promise.all([listSessions(), listDevices()])
    sessions.value = list
    devices.value = devs
    // 路由里的 id 是「激活这个会话」，即使它不在列表里也要建一份 tab 元信息，
    // 好让页面能给出「找不到 / 已结束」而不是一片空白。这份元信息的形状见
    // `unknownTabMeta`——它的 `nodeId` 是 `''` 而不是 `null`，不会被读回时丢掉。
    const id = route.value.params.id
    const known = list.find((s) => s.id === id)
    const fallback: TabMeta[] = known ? [metaOf(known)] : [unknownTabMeta(id)]
    const restored = loadTabs(storage(), fallback)
    tabs.value = restored
    ensureWorkspaces(restored.tabs)
    const target = restored.tabs.find((t) => t.id === id)
    if (target) {
      tabs.value = activateTabState(restored, target.id)
      syncUrl(target.id)
    }
    saveTabs(storage(), tabs.value)
  } catch (err) {
    loadError.value = describeReadFailure(err, '会话')
  } finally {
    loading.value = false
  }
}

/**
 * 路由参数变化（同一页面内点了另一个会话链接 / 前进后退）：
 * 把那个会话开成 tab 并激活。**不释放任何已有 tab 的通道**。
 */
watch(
  () => route.value.params.id,
  (id) => {
    if (!id) return
    const known = sessionsById.value.get(id)
    if (!known) return
    // 已经在当前 tab 上：不重开、不重发任何请求（点当前 tab 的链接很常见）
    if (tabs.value.activeId === id) return
    tabs.value = openTab(tabs.value, metaOf(known))
    ensureWorkspaces(tabs.value.tabs)
    saveTabs(storage(), tabs.value)
  },
)

onMounted(() => {
  hub = new ChannelHub()
  for (const act of Object.keys(HANDLERS)) register(act, HANDLERS[act] as () => boolean)
  document.addEventListener('keydown', onKeydown)
  void load()
})

onBeforeUnmount(() => {
  document.removeEventListener('keydown', onKeydown)
  // 页面级键位随页面失效：离开工作台后按 `o` / `t` 不该再有作用
  // （`register` 拒绝重复注册，所以没有这一步时第二次进工作台会直接抛）。
  for (const act of Object.keys(HANDLERS)) unregister(act)
  // 离开工作台 = 页面销毁：所有通道随之释放。切 tab 不走这里。
  hub?.dispose()
  hub = null
  channels.clear()
  channelUnsubs.clear()
  frameUnsubs.clear()
  // exec 是一次性的：句柄不进 `channels`（它们随命令生灭），但页面卸载时
  // 仍要跟着走一条完整的关闭路径，否则设备上会留着跑完没回收的通道。
  for (const handle of execHandles) void handle.close()
  execHandles.clear()
  // 对话句柄同理，但不是一次性的：它是这张表里唯一会跨「说好几句话」活着的条目，
  // 所以也要在这里清掉，否则卸载后表里留着一个已关的句柄（键 `tabId␀chat`）。
  // `hub.dispose()` 已经先 `close()` 过它一次，这里只清账。
  chatHandles.clear()
  // 还在窗口里的轨迹重读：定时器不会自己消失，留着会在页面卸载后调
  // `refreshTrajectory`，写进已经没人看的 `workspaces`。
  for (const t of trajTimers.values()) clearTimeout(t)
  trajTimers.clear()
})
</script>

<template>
  <div class="af-page--flush af-wb">
    <!-- 加载 / 读失败：不画一个空工作台，空工作台看起来像「这个会话没有内容」 -->
    <div v-if="loading" class="af-wb__placeholder">
      <div class="af-skeleton af-skeleton--line"></div>
      <div class="af-skeleton af-skeleton--row"></div>
      <div class="af-skeleton af-skeleton--row"></div>
    </div>

    <EmptyState
      v-else-if="loadError"
      glyph="◌"
      title="读不到会话"
      :text="loadError"
    >
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
      <button class="af-btn af-btn--sm" type="button" @click="navigate('/sessions')">回到会话列表</button>
    </EmptyState>

    <EmptyState
      v-else-if="!current"
      glyph="◈"
      title="这个会话不存在"
      :text="unknownWhy"
    >
      <button class="af-btn af-btn--primary af-btn--sm" type="button" @click="navigate('/sessions')">
        回到会话列表
      </button>
    </EmptyState>

    <template v-else>
      <!-- 工作台头：标题 · 状态 · 设备 · 路径（锁定）· 打开面板 -->
      <div class="af-wb__head">
        <h1 class="af-wb__title">
          {{ title }}
          <StateBadge :state="ended ? 'ended' : 'active'" />
        </h1>
        <span class="af-wb__meta">
          <span class="af-wb__target">
            <span aria-hidden="true">▣</span>
            <b>{{ deviceLabel }}</b>
            <span class="af-t-mute">·</span>
            <span class="af-fp">{{ current.workspace.path }}</span>
            <span class="af-badge" title="建会话时确定，之后不可改">路径锁定</span>
          </span>
          <span class="af-t-mute">·</span>
          <span class="af-t-mute" data-agent-badge>{{ agentLabel }}</span>
          <span class="af-t-mute">·</span>
          <span>{{ startedLabel }}</span>
        </span>
        <span class="af-spacer"></span>
        <button
          ref="openButtonEl"
          class="af-btn af-btn--sm"
          type="button"
          :disabled="ended"
          :title="ended ? '会话已结束：面板不会再建立通道' : '打开终端面板（o）'"
          @click="openPanel('terminal')"
        >
          <span aria-hidden="true">▤</span> 打开面板
          <kbd>o</kbd>
        </button>
        <button
          class="af-btn af-btn--sm af-btn--ghost"
          type="button"
          :disabled="ended"
          :aria-pressed="activeWs?.dock ? 'true' : 'false'"
          :title="'打开 / 收起面板坞（\\）'"
          @click="toggleDock"
        >
          {{ activeWs?.dock ? '收起面板' : '面板坞' }}
        </button>
      </div>

      <TabBar
        ref="tabBarEl"
        :tabs="tabs.tabs"
        :active-id="tabs.activeId"
        :panel-counts="panelCounts"
        :dirty="dirtyTabs"
        :conn-of="connOf"
        @activate="activate"
        @close="closeOneTab"
      />

      <!--
        这条 tab 的归属未知（见 `unknownTabMeta`）。**与下面「设备已被删除」
        那条横幅是两个事实**：那个会话存在、只是它的设备没了；这个 id 连
        会话都没有。不分开写的话，刷新之后用户读到的是「设备没了」，而真正
        的原因是他手上那条链接指向一个不存在的会话。
      -->
      <p
        v-if="activeTab && !isKnownTab(activeTab)"
        class="af-wb__banner"
        role="note"
        data-tab-unknown
      >
        会话 {{ activeTab.id }} 不在会话列表里：这条链接可能打错了，或者它指向的会话已被删掉。
        归属设备也因此未知——这不是「设备已被删除」。
      </p>

      <p v-if="ended" class="af-wb__banner" role="note">
        这个会话已结束：工作区不再可达，面板不会再建立通道。列表里仍然保留它，是为了能查到
        它曾在哪台设备的哪条路径上跑过。
      </p>

      <!--
        设备已被删除：与会话「已结束」是两回事，所以是另一条横幅。
        会话本身还在（删设备不删会话），但它的设备不会再回来了——不写这句
        用户会一直等一台已经删掉的机器重连。
      -->
      <p v-if="deviceRemoved" class="af-wb__banner" role="note" data-device-removed>
        {{ deviceGoneWhy }}：对话与轨迹都保留着，但它的面板无法再建立通道，
        消息也发不出去了。
      </p>

      <div class="af-wb__body">
        <!-- 对话列：ConvFlow 挂在这里（13d）。M1 恒空，空态由它自己说清为什么 -->
        <div
          ref="bodyEl"
          class="af-wb__conv"
          tabindex="-1"
          data-wb-body
          :style="activeWs?.dock ? { flex: `0 0 ${split}%` } : undefined"
        >
          <ConvFlow
            :device-name="deviceLabel"
            :path="rootPath"
            :ended="ended"
            :device-gone="deviceRemoved"
            :device-gone-why="deviceGoneWhy"
            :terminal-ready="terminalReady"
            :traj-open="activeWs?.traj ?? false"
            :agent-id="current?.agent_id ?? null"
            :conv-error="activeWs?.convError ?? null"
            :events="activeWs?.events ?? []"
            :gaps="activeWs?.gaps ?? []"
            :incomplete="activeWs?.incomplete ?? false"
            @run="runCommand"
            @send="convSend"
            @say="convSay"
            @open-traj="toggleTraj"
          />
          <div class="af-wb__convhint af-field__hint">
            <span><kbd>o</kbd> 打开面板</span>
            <span><kbd>[</kbd><kbd>]</kbd> 切换面板</span>
            <span><kbd>\</kbd> 收起面板坞</span>
            <span class="af-spacer"></span>
            <span><kbd>⇧Tab</kbd> 回 tab 条</span>
          </div>
        </div>

        <SplitHandle v-if="activeWs?.dock" v-model="split" />

        <div v-if="activeWs?.dock" ref="dockEl" class="af-wb__dockwrap">
          <PanelDock
            :panel="activeWs.panel"
            :mounted="activeWs.mounted"
            :device-name="deviceLabel"
            :path="current.workspace.path"
            :channel="channelView"
            :output="activeWs.output"
            :cwd="activeWs.cwd"
            @open="openPanel"
            @close="closeDock"
            @interact="interact"
          >
            <!--
              面板正文由 13b / 13c 挂进来（坞的两个具名插槽）。

              两者的共同纪律：**都不持通道**——取数写数全由本页翻译（见文件头）。
              所以用插槽而不是让 PanelDock 直接 import 它们：坞只管容器
              （标签 / 状态 / 释放入口），它不知道里面是终端还是文件树。

              两个插槽各自带 `v-if`：坞里同时只显示一个面板，但两个组件都挂着的
              话，切走的那一个会带着自己的 xterm / 树状态在后台活着——终端的
              `ResizeObserver` 会在一个 0 高度的元素上一路量尺寸。
            -->
            <template #terminal>
              <TerminalPanel
                v-if="activeWs.panel === 'terminal'"
                :device-name="deviceLabel"
                :path="rootPath"
                :channel="terminalView"
                :conn="conn"
                :output="activeWs.output"
                :ended="ended"
                @input="terminalInput"
                @resize="terminalResize"
                @interact="interact('terminal')"
              />
            </template>
            <template #files>
              <FilesPanel
                v-if="activeWs.panel === 'files'"
                :root="rootPath"
                :cwd="activeWs.cwd"
                :listings="activeWs.listings"
                :loading="activeWs.loading"
                :preview="activeWs.preview"
                :reading-path="activeWs.readingPath"
                :error="activeWs.filesError"
                :write-error="activeWs.writeError"
                :saving="activeWs.saving"
                :ended="ended"
                :channel-ready="filesReady"
                @list="fileList"
                @read="fileRead"
                @save="fileSave"
                @goto="filesGoto"
                @open="filesOpen"
                @revert="filesRevert"
                @dirty-change="filesDirty"
                @dismiss-error="activeWs.filesError = null"
                @dismiss-write-error="activeWs.writeError = null"
              />
            </template>
          </PanelDock>
        </div>
      </div>

      <TrajDrawer
        :open="activeWs?.traj ?? false"
        :opener="activeWs?.trajOpener ?? null"
        :title="title"
        :device-name="deviceLabel"
        :path="rootPath"
        :events="activeWs?.events ?? []"
        :gaps="activeWs?.gaps ?? []"
        :incomplete="activeWs?.incomplete ?? false"
        :has-more="activeWs?.hasMore ?? false"
        :loading="activeWs?.trajLoading ?? false"
        :error="activeWs?.trajError ?? null"
        @close="closeTraj"
        @more="onTrajMore"
      />
    </template>
  </div>
</template>

<style scoped>
.af-wb__placeholder {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
  padding: var(--af-space-8) var(--af-space-9);
}
.af-wb__placeholder .af-skeleton--line {
  width: 240px;
}
.af-wb__banner {
  flex: 0 0 auto;
  margin: 0;
  padding: var(--af-space-2) var(--af-space-5);
  background: var(--af-surface-1);
  border-bottom: 1px solid var(--af-border-subtle);
  color: var(--af-fg-tertiary);
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-snug);
}
/* 对话列获得焦点时不要画一个「整块被选中」的环：它是容器，不是控件 */
.af-wb__conv:focus {
  outline: none;
}
.af-wb__conv:focus-visible {
  outline: 2px solid var(--af-accent);
  outline-offset: -2px;
}
.af-wb__convhint {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--af-space-4);
  padding: var(--af-space-2) var(--af-space-4);
  border-top: 1px solid var(--af-border-subtle);
  background: var(--af-surface-1);
}
.af-wb__dockwrap {
  display: flex;
  flex: 1 1 auto;
  min-width: 0;
}
.af-wb__dockwrap > * {
  flex: 1 1 auto;
  min-width: 0;
}
@media (max-width: 900px) {
  /* < 900px 面板坞覆盖式展开（§7.1）：对话全宽，坞叠在上面 */
  .af-wb__body {
    position: relative;
  }
  .af-wb__conv {
    flex: 1 1 auto !important;
  }
  .af-wb__dockwrap {
    position: absolute;
    inset: 0 0 0 auto;
    width: min(560px, 92%);
    background: var(--af-surface-0);
    border-left: 1px solid var(--af-border-strong);
    box-shadow: -16px 0 32px rgb(0 0 0 / 45%);
  }
}
</style>
