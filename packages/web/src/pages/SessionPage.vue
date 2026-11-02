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
   ① **懒建立**：第一次与面板正文交互，或**显式选中终端面板**时才
      `open_channel`。仅仅是「把坞展开」不建——那时还没有任何数据要流动，
      用户可能只是要点文件、或临时占一下版面。但「选中终端」是一个明确
      指向终端的意图，此时不建只会留下一块没有任何说明的空白终端，所以
      它算建立时机（`openPanel`）。files 面板不预先建：它的首个动作
      （列目录）自己会开通道。
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
import GitPanel, { type GitDiffView, type GitFile } from '@/components/GitPanel.vue'
import type { GitCommit } from '@/components/GitLog.vue'
import PanelDock, { type ChannelView } from '@/components/PanelDock.vue'
import { PANEL_ORDER, type PanelName } from '@/lib/panel'
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
  WorkspaceRef,
} from '@/lib/api'
import { cancelConversation, getTrajectory, listDevices, listSessions } from '@/lib/api'
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
import { clampSplit, SPLIT_DEFAULT } from '@/lib/split'
import { handleLayerKeydown, pop, push } from '@/lib/focus'
import { NARROW_QUERY, watchMediaQuery } from '@/lib/media'
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

/**
 * git 面板的一份视图状态。
 *
 * **三件事各自独立**，所以是三个字段而不是一个 `error: string | null`：
 * 「没问过」「不是仓库」「问失败了」在页面上是三句不同的话，而一个
 * 可空字符串会让「not a git repository」和「通道断了」长得一样。
 */
interface GitView {
  /** `status` 回帧里的分支名。`null` = 还不知道（没问过 / 空仓库无 HEAD）。 */
  branch: string | null
  /** 短 HEAD。detached 时也有值——那正是它唯一的信息来源。 */
  head: string | null
  /** detached HEAD。页面要写 `(detached at abc1234)` 而不是留空白。 */
  detached: boolean
  /** 工作区是否有未提交改动。`null` = 还没问过（与 `false` 是两件事）。 */
  clean: boolean | null
  /** 改动文件列表（`status` 的 `files`）。 */
  files: GitStatusFile[]
  /** 设备说「这里不是 git 仓库」。**正常状态**，页面上是一句中性说明。 */
  notARepo: boolean
  /** 当前展开的那个文件的 diff。 */
  diff: GitDiffView | null
  /** 正在取 diff 的路径（点开后才发，防重入）。 */
  reading: string | null
  /**
   * 历史提交。`null` = 还没问过（切到历史分段时才问）。
   *
   * **与 `files` 用 `null`/`[]` 区分「没问过」和「没有」是同一条纪律**
   * （§3.12）：空数组是「这个仓库还没有提交」，`null` 是「还没问」。
   */
  commits: GitCommit[] | null
  /** 还有更早的提交没列出来。 */
  logTruncated: boolean
  /** 正在读取哪条提交的 diff（完整 hash）。 */
  logReading: string | null
  /** 当前 diff 属于哪条提交。`null` = 展示的不是历史 diff。 */
  logSelected: string | null
  /** 问 status / diff 被拒的原因。与 `notARepo` 分开：那是事实，这是故障。 */
  error: string | null
}

/**
 * `status` 回帧里的一个文件条目。两列状态码原样保留。
 *
 * 直接复用面板导出的 `GitFile` 而不是这里再写一份 `interface`：两边各写一份
 * 的话，加字段时必然只改一处、另一处静默丢字段（`orig` 就是这么丢的）。
 */
type GitStatusFile = GitFile

/**
 * 会话起点 + 相对起点的产出概览。
 *
 * **为什么不塞进 `GitView`**：起点不属于 git 面板，它属于**会话身份**——
 * 与设备、路径同级，切到终端 / 文件树时也应该看得见。放进 `GitView` 的后果
 * 具体到模板：`GitPanel` 是 `#git` 插槽里的组件，换到别的面板它就卸载了，
 * 起点跟着消失，于是「这次会话从哪开始的」只在看 git 时才知道。
 */
interface BaseView {
  /** 起点完整 hash。`null` = 没有（原因看 `status`）。 */
  rev: string | null
  /** Hub 给的 `base_rev_status` 五态。**不含本地派生态**（见 `basePhase`）。 */
  status: 'ok' | 'not_a_repo' | 'no_head' | 'unavailable' | 'unknown'
  /**
   * 相对起点的概览。`null` = 还算不出来（起点没有 / 还没问过 / 起点已不在历史里）。
   *
   * **两个数分开报**：`commits` 是 `base_rev` 之后的提交数，`files` 是**当前工作区
   * 未提交**的改动文件数。加起来报一个总数会让用户以为「提交了 3 次、改了 7 个
   * 文件」里的 7 包含那 3 次的内容。
   */
  progress: { commits: number; files: number } | null
  /**
   * 概览里的提交数**超过了 `log` 的上限**（找不到起点且 `truncated`）。
   *
   * 单独一个字段而不是把 `commits` 写成 `Infinity` 或 `-1`：那两种哨兵值会让
   * 任何忘记判它的人算出无意义的数（`Infinity` 甚至能一路活到模板里）。
   */
  overLimit: boolean
  /** 起点已不在当前历史里（rebase / 清理过）。不报数，只如实说。 */
  stale: boolean
  /**
   * 会话所属的设备**已被删除**（`workspace.node_id === null`）。
   *
   * **与「设备离线」必须是两个字段**：它们是两次不同的处置（一个该等设备
   * 上线，一个该找管理员恢复设备）。判据见 `setBaseDevice`。
   */
  deviceRemoved: boolean
  /**
   * 设备还在，但现在开不了通道（离线 / 没上报 git connector / 通道被关）。
   *
   * 判据**不是**「概览算不出来」——起点本来就可能是 `not_a_repo`，
   * 那种情况下概览也不存在，但那不是设备不可达。
   */
  deviceOffline: boolean
  /** 概览那次 `log` 正在飞（防重入）。 */
  reading: boolean
}

interface TabWorkspace {
  /** 面板坞开着吗。 */
  dock: boolean
  /**
   * 对话列宽度（百分比，30–70）。**这是左右分栏唯一的数字。**
   *
   * ── 为什么钉在 workspace 上而不是页面级 ref ────────────────────
   * 它是**每个会话自己的布局**：一个会话在跑长命令要把终端拉宽，另一个在
   * 读 diff 要把对话拉宽，两者不该互相污染。挂在 workspace 上就自动跟着
   * tab 走（切走再切回原值还在），也不需要另写一份 localStorage——
   * 写全局存储会让新 tab 继承上一个会话的比例，而两个会话的工作内容未必同类。
   *
   * ── 为什么只有一个数，没有 splitV ──────────────────────────────
   * v0.5.0 起右侧只有一格（裁决 C），**没有纵向分栏**。拆成 `splitH` /
   * `splitV` 两个字段却没有第二处可拖，只会让「哪个是哪个」变成新的认知
   * 负担。将来真要做纵向时再加，那时才有真实的第二处可调。
   */
  split: number
  /** 当前面板。`null` = 坞开着但还没选（只在坞开着时出现）。 */
  panel: PanelName | null
  /**
   * 这个 tab 上挂着的面板。**v0.5.0 起恒为 0 或 1 个元素**（裁决 C：右侧一格）。
   *
   * 留着数组而不是收成单个值，是因为 `closeDock` 那段「逐个释放」的循环、
   * 以及若干测试的前提都是「一组面板」，改成标量会牵动它们却不换来任何
   * 表达力。不变量的执行点在 `openPanel`：它写 `ws.mounted = [panel]`
   * （整体替换，不是 push），`closeDock` 写 `[]`。**别在别处 push**——
   * 一 push 就破坏了这条不变量，而症状（两个通道同时活着）很容易被当成
   * 「本来就是多面板」而漏过去。
   */
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
  /** 最后一轮**既没有开始标记也没有结束标记**：结尾丢了（或是旧数据）。 */
  incomplete: boolean
  /**
   * 最后一轮**有 `turn_start`、没有 `turn_end`**：一轮开了还没关。
   *
   * 与 `incomplete` 互斥。**不等于「agent 还活着」**——开始标记落库后设备掉了
   * 也是这个状态（见 `lib/api.ts` 的同一字段）。
   */
  running: boolean
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
  /** git 面板的视图状态（分支 / 改动 / diff）。 */
  git: GitView
  /**
   * 会话起点与产出概览。
   *
   * **与 `git` 平级而不是它的子字段**：`git` 随 git 面板的生命周期起落
   * （面板卸载了它的取数也就停了），而起点是会话身份的一部分，必须独立
   * 存活——见 `BaseView` 的注释。
   */
  base: BaseView
}

const workspaces = reactive(new Map<string, TabWorkspace>())

function blankWorkspace(): TabWorkspace {
  return {
    // 进会话即展开坞、缺省显示 git（v0.4.0，用户拍板）：会话里最常要看的是
    // 「这次改了什么」。这一条推翻了 `design-system.md` 原「坞默认关着、
    // 不预开」——那条的理由是「对话列独占全宽」，而 git 面板给出的信息
    // （改动 + 历史）比一次终端占用更值。
    //
    // 顺带它是文件面板入口的**前提**：坞标签由 `mounted` 渲染，而 `mounted`
    // 唯一的写入点是 `openPanel`（只由点标签触发）——`mounted: []` 时一个标签
    // 都画不出来，于是点不到标签、也就永远填不满 `mounted`。加面板之前必须
    // 先有一条非空的初值，否则新面板和 files 一样不可达。
    dock: true,
    split: SPLIT_DEFAULT,
    panel: DEFAULT_PANEL,
    mounted: [DEFAULT_PANEL],
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
    running: false,
    hasMore: false,
    trajLoading: false,
    trajError: null,
    convError: null,
    views: {
      terminal: { state: 'idle', error: null, dropped: 0 },
      files: { state: 'idle', error: null, dropped: 0 },
      // 类型是 `Record<PanelName, ChannelView>`：扩 `PanelName` 时漏一项
      // 编译不过，而不是运行时读到 `undefined` 再把 `.state` 炸出来。
      git: { state: 'idle', error: null, dropped: 0 },
    },
    git: {
      branch: null,
      head: null,
      detached: false,
      // `null` 而不是 `true`：还没问过的时候页面不能声称「干净」。
      clean: null,
      files: [],
      notARepo: false,
      diff: null,
      reading: null,
      commits: null,
      logTruncated: false,
      logReading: null,
      logSelected: null,
      error: null,
    },
    base: {
      rev: null,
      // 初值 `unknown` 而不是 `ok`：与 Hub 侧迁移默认值同一条口径——
      // 「我还不知道」不能假装成「取到了」。
      status: 'unknown',
      progress: null,
      overLimit: false,
      stale: false,
      deviceRemoved: false,
      deviceOffline: false,
      reading: false,
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
    const ws = workspaces.get(tab.id)
    if (!ws) {
      const fresh = blankWorkspace()
      // 新 workspace 的起点从 tab 元信息里恢复：刷新页面时先有 `tab.baseRev`
      // （随 localStorage 来的），`GET /api/sessions` 是稍后的一次请求。
      // 不先恢复的话，「起点」那一行会在刷新后闪成「未记录」。
      seedBase(fresh, tab)
      workspaces.set(tab.id, fresh)
      continue
    }
    // 已存在的 workspace 也要跟着 tab 元信息走：会话列表刷新可能带来更新的
    // 起点（同一个 id 在另一台设备上刚被补记）。`seedBase` 不动 `progress`。
    seedBase(ws, tab)
  }
}

/**
 * 把**会话列表里那份 `workspace`** 与通道层的错误码喂给起点的设备两档。
 *
 * 单独一个函数而不是塞进 `seedBase`：`seedBase` 只有 `TabMeta`，而「设备已删」
 * 的判据要用 `WorkspaceRef` 的两个字段（`TabMeta` 只留了一个 `nodeId`，
 * 那一个字段**故意不够判别**——它把「已删」与「离线」压成同一个 `null`）。
 *
 * `offline` 从 `ws.views.git.error` 读：那是通道层把 Hub 的 `err::*` 码原样
 * 放进来的地方（见 `syncViews`）。只认 `OFFLINE` / `NOT_REPORTED` 两个码——
 * 其余错误（`channel_error`、`device_busy`）说明通道开起来了，不是不可达。
 */
function syncBaseDevice(list: TabMeta[]): void {
  for (const tab of list) {
    const ws = workspaces.get(tab.id)
    const session = sessionsById.value.get(tab.id)
    if (!ws || !session) continue
    const code = ws.views.git.error
    const offline = code === 'device_offline' || code === 'connector_not_reported'
    setBaseDevice(ws.base, session.workspace, offline)
  }
}

const activeWs = computed<TabWorkspace | null>(() =>
  tabs.value.activeId ? (workspaces.get(tabs.value.activeId) ?? null) : null,
)

/**
 * 每个 tab 当前开着的**工具**（`null` = 坞收着）。
 *
 * ── 为什么是工具而不是「面板数」 ──────────────────────────────────
 * v0.4.0 这里给的是 `mounted.length`，那个数让「N 个 tab = N × 面板数条
 * 活跃通道」这个代价可见。v0.5.0 右侧只有一格（裁决 C），`mounted` 恒为
 * 一个元素，长度恒为 1——一个永远是 1 的徽标不是信息，是噪音。
 *
 * 但「这个 tab 现在开着什么」仍是有用信息（切回去之前想知道会看到终端还是
 * Git），所以要留着，只换载体：图标比数字省地方，也不撒谎。
 *
 * 键用 `Record<string, PanelName | null>` 而不是 `Partial<...>`：
 * 缺键与「没有工具」是两件事，前者会让 `v-if` 静默成立不了。
 */
const panelIcons = computed<Record<string, PanelName | null>>(() => {
  const out: Record<string, PanelName | null> = {}
  for (const tab of tabs.value.tabs) {
    const ws = workspaces.get(tab.id)
    out[tab.id] = ws && ws.dock ? ws.panel : null
  }
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
 * 对话通道的两个订阅的退订函数，键与 `chatHandles` 相同。
 *
 * **必须有这张表**：`ensureChatChannel` 用 `handle.onState` / `handle.onFrame`
 * 挂了两个回调，而 `releaseTab` 里只 `chatHandles.delete` + `close()` 的话，
 * 回调仍挂在**句柄**上。`close()` 之后句柄确实不再产帧，但「靠在句柄会自我
 * 清理」是**对另一个模块行为的假设**，而不是这里给的保证——面板通道
 * （`channelUnsubs` / `frameUnsubs`）早就是显式持有的，对话这一侧没有理由
 * 例外。持有退订函数后，关掉 tab 这件事在本文件里就是完整的。
 */
const chatUnsubs = new Map<string, () => void>()

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

/**
 * 还挂着订阅的对话通道有几条，**只给测试看**。
 *
 * 与 `chatHandleCount` 同一理由：退订发不出任何请求，断言「关 tab 后不再有
 * 轨迹重读」不成立——`refreshTrajectory` 首行的 `workspaces.get(tabId)` 守卫
 * 会在 `workspaces.delete` 之后直接 return，那一道防线与退订无关。于是
 * 「订阅退没退」在网络上与「退了」一模一样。这里只读一个数。
 */
function chatSubscriberCount(): number {
  return chatUnsubs.size
}

defineExpose({ chatHandleCount, chatSubscriberCount })

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
    // git 与 files 同一条理由：面板挂上就问一次 status。不问的话用户看到的
    // 是一块「还没问过」的空壳，而通道明明是好的——两种空被读反（§3.12）。
    if (state === 'ready' && panel === 'git') gitKick(tab)
    // 起点那一行也要跟着通道层走：`views.git.error` 刚被写成 `device_offline`
    // 时，「设备当前不可达」这句话得立刻出现，而不是等下一次全量刷新。
    if (panel === 'git') syncBaseDevice(tabs.value.tabs)
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
 * | git | `{ op: 'status' \| 'diff' \| 'log' \| 'error', … }` | `ws.git`（分支 / 改动 / diff） |
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
    return
  }
  if (panel === 'git') {
    // 传 `tab` 而不是只用 `ws`：概览那次 `log` 要发请求，而请求要 tab 元信息
    // （`gitRequest` 的第一参）。tab 找不到就说明这个 tab 刚被关掉，此时
    // 什么也不做——帧还会来，但已经没有落点。
    const tab = tabs.value.tabs.find((t) => t.id === tabId)
    if (tab) applyGitFrame(ws, tab, frame.payload)
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
    // 先退订再关：反过来的话，`close()` 期间到达的帧仍会被回调收下，而
    // `scheduleTrajectory` 会对着一个即将消失的 ws 起定时器。
    chatUnsubs.get(chatKey)?.()
    chatUnsubs.delete(chatKey)
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

/**
 * 坞的缺省面板。**一处定义，所有回落点都用它**（不写「N 处使用」：那个数字
 * 会随着新增入口变旧，2026-10-29 就已经从「四处」变成六处而注释没跟上）。
 *
 * v0.4.0 起缺省是 git（用户拍板：打开就是 git 界面）。各写一份字面量的后果
 * 不是编译错，而是「关掉坞按 `o` 是 git、按 `\` 打开却是终端」这种半吊子一致
 * ——实测撞上过两处：`o` 键的回落，以及页面头「打开面板」按钮（第 1933 行
 * 曾是裸的 `'git'`，改缺省值时它不跟着变）。所以收敛到这里。
 */
const DEFAULT_PANEL: PanelName = 'git'

/**
 * 打开 / 换到某个面板。v0.5.0 起右侧只有**一格**，所以这里的语义是
 * 「**换掉**当前显示的那一个」，不是「再挂一个」（裁决 C）。
 *
 * ── 点当前这一个 = 无副作用 ──────────────────────────────────────────
 * 这是本函数最先做的一件事，也是最要紧的一条：`＋` 里列出了**全部三个**
 * 工具（含当前这个），用户点到它时不能走「先释放再重建」那条路——
 * 对终端来说那就是**把正在跑的 shell 杀掉**，而用户只是点了一下。
 * 所以先判等、直接返回，一个通道调用都不发。
 */
function openPanel(panel: PanelName): void {
  const ws = activeWs.value
  const tab = activeTab.value
  if (!ws || !tab) return
  if (ended.value) return

  const previous = ws.panel
  ws.dock = true

  if (previous === panel && ws.mounted.includes(panel)) {
    // 已经是这一个了：只保证坞是开的、把焦点送进正文，**不碰通道**。
    void nextTick(() => focusPanelBody())
    return
  }

  // 换掉旧的那一个 = 释放它的通道（§7.2「退出即释放」按工具算，不按坞算）。
  // **终端这一路要格外小心**：`crates/node-core/src/connector/terminal.rs:305`
  // 的 `close()` 走 `pty.close()`，释放通道 = **杀掉真 shell**。这是本里程碑
  // 相对 v0.4.0 的行为降级（那时可以在多个已挂载面板间来回切而不丢 shell），
  // 已知并接受，不在这里缓解——缓解属于新能力。键位文案与 e2e 如实反映它。
  if (previous && previous !== panel) void releaseChannel(tab.id, previous)

  ws.panel = panel
  ws.mounted = [panel]
  // 焦点进面板正文（用户按 `o` 的意图是去用那个面板，不是打开一个壳）
  void nextTick(() => focusPanelBody())
  // 显式选中终端面板 = 「给我一个终端」，这时就该建通道。
  //
  // 这与 `focusPanelBody` 里「打开坞不算建 PTY」**不矛盾**：那条说的是
  // 「把坞展开」这个动作本身，用户可能只是要看文件、临时占一下版面。而
  // 选中 terminal 面板是一个明确指向终端的意图，此时不建通道的结果就是一块
  // 空白终端、屏幕上没有任何字说它还没连——用户只会以为终端坏了，然后去
  // 找「连接」按钮（没有这个按钮）。原来的懒建立只挂在用户输入上，于是
  // 「必须先随便敲一个键才连上」成了隐性前置条件，这是本 bug 的来源。
  //
  // 仍然不在这里建的是 **files** 面板：它的首个动作（列目录）自己会开通道
  // （`fileList` → `ensureChannel`），不需要预先建一条空通道。
  if (panel === 'terminal') {
    // 先清掉「页面自己放焦点」的标记：这个调用是显式意图，不该被
    // `consumeSelfFocus` 当成自造事件吞掉。标记不在这里清的话，上一个面板
    // 遗留的 `selfFocused` 可能正好等于当前 activeElement，把这次开通道
    // 静默吃掉——表现与「终端永远连不上」一模一样，很难查。
    selfFocused = null
    interact('terminal')
  }
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

/**
 * `[` / `]`：换成上一个 / 下一个工具。
 *
 * ── 为什么在 `PANEL_ORDER` 上轮转，而不是在 `ws.mounted` 上 ──────────
 * v0.5.0 起右侧只有一格（裁决 C），而 `ws.mounted` 的**唯一**写入点是
 * `openPanel` 的 `ws.mounted = [panel]`（整体替换），`blankWorkspace` 也
 * 只放一个。所以它恒为长度 0 或 1——在它上面「轮转」永远原地不动，`[` 与
 * `]` 会变成同一个键（都只是「把当前那个重新打开一次」）。
 *
 * 于是轮转的基准换成 `PANEL_ORDER`（三个工具的固定顺序）：`]` 往后一个、
 * `[` 往前一个，回环。这才是「上一个 / 下一个工具」，也才让 `delta` 真的
 * 有两个方向。
 *
 * ── 代价（已知降级，不在此缓解）─────────────────────────────────────
 * 换到另一个工具 = 释放当前工具的通道。对终端就是**杀掉正在跑的 shell**
 * （`terminal.rs:305` 的 `close()` 走 `pty.close()`）。键位文案与 `?` 面板
 * 都如实写了这一句（见 `lib/keynav.ts` 的 `panel:prev` / `panel:next`）。
 */
function cyclePanel(delta: number): void {
  const ws = activeWs.value
  if (!ws) return
  // 坞收着时，`[` / `]` 等同于「打开当前那个」（没有「下一个」可言）
  if (!ws.dock || ws.panel === null) {
    openPanel(ws.panel ?? DEFAULT_PANEL)
    return
  }
  const i = PANEL_ORDER.indexOf(ws.panel)
  // `i < 0` 只可能来自 `panel` 里有 `PANEL_ORDER` 不认的值（类型层已排除）。
  // 兜到 0 而不是让 `NaN` 传播进取模。
  const from = i < 0 ? 0 : i
  const next = PANEL_ORDER[(from + delta + PANEL_ORDER.length) % PANEL_ORDER.length] as PanelName
  openPanel(next)
}

/**
 * 改当前 tab 的对话列宽度。
 *
 * 收口放在这里而不是信任子组件：`clampSplit` 在 `lib/split.ts` 里是唯一的
 * 口径（30–70），而 `SplitHandle` 的 v-model 只是一个数字通道——将来再接
 * 第二个拖动来源（触摸板手势之类）时，收口点不应该也跟着多一个。
 */
function setSplit(value: number): void {
  const ws = activeWs.value
  if (!ws) return
  ws.split = clampSplit(value)
}

/** 双击把手 = 恢复缺省比例。拖歪了的回头路（见 SplitHandle 的事件说明）。 */
function resetSplit(): void {
  setSplit(SPLIT_DEFAULT)
}

function toggleDock(): void {
  const ws = activeWs.value
  if (!ws) return
  if (ws.dock) closeDock()
  else openPanel(ws.panel ?? DEFAULT_PANEL)
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
  const offState = handle.onState(() => {
    ws.convError = handle.errorCode()
  })
  // 对话的帧只用来判「还在动」：内容一律读轨迹表（`GET .../trajectory`），
  // 因为帧在断线期间会丢，而轨迹是库里的。页面不靠帧补历史。
  // 读的动作走 `scheduleTrajectory`：一次回复的帧是成批到的，每帧一读会把
  // 请求数抬到帧数那么多，而新内容往往只有几个字。
  const offFrame = handle.onFrame(() => {
    scheduleTrajectory(tab.id)
  })
  chatUnsubs.set(key, () => {
    offState()
    offFrame()
  })
  return handle
}

/**
 * 停止正在跑的那一轮（`ConvFlow` 的停止按钮）。
 *
 * 这里**不关通道**：`session/cancel` 只掐一轮，会话还能继续聊。
 *
 * 失败要说出来，成功则**不假装**「已经停了」：Hub 回的 204 只代表帧发到了
 * 设备，它判不出设备上到底有没有一轮在飞（见 `cancelConversation` 的注释）。
 * 所以成功时什么横幅都不加——真停了，轨迹里会出现 `turn_end`；没停，
 * 页面本来也没承诺过。
 */
function convCancel(): void {
  const tab = activeTab.value
  const ws = activeWs.value
  if (!tab || !ws) return
  void cancelConversation(tab.id).catch((err: unknown) => {
    // 404 = 对话根本没开过 / 会话不在你名下；其余（含网络）一并如实说。
    ws.convError = err instanceof ApiError ? err.code : 'cancel_failed'
  })
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
    ws.running = view.running
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
    ws.running = view.running
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

/* ---------------------------------------------------------------- 窄屏抽屉 */

/**
 * 窄屏（< 900px，断点见 `lib/media.ts`）。
 *
 * ── 为什么这个判断在 JS 里，尽管样式已经被 CSS 处理了 ──────────────
 * 窄屏的右侧不是一个「更窄的列」，而是**一层浮层**：它盖在对话上、有遮罩、
 * Esc 能关、焦点被圈在里面、关掉之后回到打开它的按钮。这四件事样式做不到。
 *
 * 宽屏下 `dock` 只是「这一列显不显示」，焦点自由进出；窄屏下它是模态层。
 * 两套语义共用一个 `ws.dock` 状态（它确实是同一件事：用户要看到面板），
 * 但**只有窄屏才入焦点栈**——宽屏下把右侧设成 inert 会让对话区点不动，
 * 那是荒谬的：两边并列，凭什么右边是模态。
 */
/*
  订阅放在 setup 期而不是 `onMounted`：`matchMedia` 在 setup 期已经可用，
  而「第一帧就该知道自己是窄屏」——放到 mounted 之后，窄屏下第一帧会先渲染
  成宽屏（并列、有把手），再跳成抽屉。测试还依赖 setup 期就有正确值。
*/
const narrow = watchMediaQuery(NARROW_QUERY)
const isNarrow = narrow.matches

/**
 * 当前窄屏抽屉是否已经在焦点栈里。
 *
 * 它必须是一个显式的账本，不能靠 `depth()` 判断：轨迹抽屉可能开着、
 * 帮助面板可能开着，`depth() > 0` 说明不了这一层在不在。
 * 也不能靠 `ws.dock` 反推——宽屏下 `dock` 为真时它**不该**在栈里。
 */
let narrowPushed = false

/**
 * 抽屉入栈时那个容器的账本。
 *
 * **出栈时必须用它，不能现读 `dockEl.value`**：出栈的时机恰好是元素被 `v-if`
 * 卸载之后，那时 `dockEl` 已是 `null`，而 `pop(undefined)` 关的是栈顶（可能是
 * 别人的层）。详见 `popNarrowDrawer` 的注释。
 */
let narrowLayerEl: HTMLElement | null = null

/** 抽屉真正作为一层浮层挂着的条件。 */
const drawerIsLayer = computed(() => isNarrow.value && (activeWs.value?.dock ?? false))

function onNarrowKeydown(e: KeyboardEvent): void {
  const el = dockEl.value
  if (!el) return
  // 判据（是不是栈顶、输入法选字中吗）全在 `handleLayerKeydown` 里——
  // `TrajDrawer` / `LoginPrompt` 走的是同一个函数，三份实现迟早会漂成
  // 三种键位。
  handleLayerKeydown(e, el, closeDock)
}

function pushNarrowDrawer(): void {
  const el = dockEl.value
  if (!el || narrowPushed) return
  // `opener` 显式给「打开面板」按钮：它是用户刚才按的那个，
  // 关掉抽屉要把焦点还回去，而不是丢给 `main`。
  push(el, { opener: openButtonEl.value })
  narrowLayerEl = el
  narrowPushed = true
  document.addEventListener('keydown', onNarrowKeydown)
}

/**
 * 把抽屉移出焦点栈。
 *
 * ── 为什么必须记住 `el`，不能现读 `dockEl.value` ────────────────────
 * `dockEl` 绑在 `v-if="activeWs?.dock"` 的那个元素上，而本函数唯一的调用
 * 时机就是「抽屉不该再是浮层了」——那一刻 `dock` 已为 false（或视口已变宽），
 * 元素被 `v-if` 卸载，`dockEl.value` **必然**是 `null`。
 *
 * 于是 `pop(dockEl.value ?? undefined)` 会退化成**无参 `pop()`**，而它关的是
 * **栈顶**——栈顶可能是别人的层（窄屏下抽屉开着时按 `?` 打开帮助面板，栈顶
 * 就是帮助）。结果：帮助被无关关闭，抽屉的容器却还留在栈里，成了一个指向
 * 已卸载元素的僵尸层。此后 `depth()` 永久虚高，`inertOthers` 按僵尸层重算
 * 把 body 下所有内容标成 inert——**整页点不动，Esc 也救不回来**。
 * （`App.vue` 的帮助面板踩过同一个坑，那里留了警告注释。）
 *
 * 所以 `push` 时就把容器存进账本，出栈时用它定位自己那一层。
 */
function popNarrowDrawer(): void {
  if (!narrowPushed) return
  narrowPushed = false
  document.removeEventListener('keydown', onNarrowKeydown)
  const el = narrowLayerEl
  narrowLayerEl = null
  // 账本为空说明这一层从未真的入栈（`push` 被 `dockEl` 为 null 挡下了）。
  // 此时**什么都不做**：无参 `pop()` 会去关栈顶，那是别人的层。
  if (el) pop(el)
}

/**
 * 让「这一层在不在栈里」跟上「该不该在」。
 *
 * 三种触发都会走到这里：`dock` 变了、视口跨过断点、切了 tab。
 * **必须集中在一个函数**——分散写的话，最容易漏的是「宽屏拖窄时抽屉正开着」
 * 这一条（那时没有任何用户动作，只有一个 `matchMedia` 事件）。
 */
watch(
  drawerIsLayer,
  (want) => {
    if (want) void nextTick(() => pushNarrowDrawer())
    else popNarrowDrawer()
  },
  { immediate: true, flush: 'post' },
)

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
 * 往某个面板的通道上发一个请求帧。
 *
 * `files` 与 `git` 两条取数路径的骨架**逐行同构**（取 handle → 取 id → POST
 * /send → catch 里把错误写进面板状态），早先是两份各写一遍的；只把「失败时
 * 说什么」留在外面，是因为那是这两个面板唯一真正不同的部分。
 *
 * 失败必须**说话**：面板没有第二条反馈路径，静默失败就是一块永远不动的面板
 * ——用户看不出是网络断了、通道没了，还是这次操作本来就没结果。
 *
 * **必须发 `application/json`**：Hub 的 `/send` 按 Content-Type 分派
 * （`crates/hub-server/src/routes/stream.rs`），只有 JSON 会原样成为
 * `channel_data.payload`；终端的 `application/octet-stream` 会被包成
 * `{"bytes":…}`，两个 connector 都收不到 `op`。
 *
 * **未 `ready` 的通道直接返回，不排队也不补发**。补发是 `files` 自己的事
 * （`pendingFiles`，见 `fileRequest` 的注释），git 侧没有对应机制——把那条
 * 保证写在这个共用函数的头上会让人以为 git 也有。
 */
function sendPanelRequest(
  tab: TabMeta,
  panel: PanelName,
  op: string,
  payload: Record<string, unknown>,
  onError: (err: unknown) => void,
): boolean {
  const handle = channels.get(channelKey(tab.id, panel))
  if (!handle || handle.state() !== 'ready') return false
  const id = handle.id()
  if (!id) return false
  void request(`/api/channels/${encodeURIComponent(id)}/send`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ op, ...payload }),
  }).catch(onError)
  return true
}

function fileRequest(tab: TabMeta, op: 'list' | 'read' | 'write', payload: Record<string, unknown>): void {
  sendPanelRequest(tab, 'files', op, payload, (err: unknown) => {
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
 * git 通道 ready 之后问一次 `status`。
 *
 * 同 `filesKick` 的理由：面板挂上却什么都没问，用户看到的是一块「还没问过」
 * 的空壳，而通道明明是好的——两种空被读反（§3.12）。**问一次就够**：git 的
 * 状态变化是用户自己触发的（切回面板时重问见 `gitRefresh`），不像终端有设备
 * 主动推送。
 */
function gitKick(tab: TabMeta): void {
  void Promise.resolve().then(() => {
    const ws = workspaces.get(tab.id)
    if (!ws) return
    gitRequest(tab, 'status')
  })
}

/**
 * 概览那次 `log`：**与历史分段那次是不同的请求**（`limit` 500 vs 30）。
 *
 * 不共用 `ws.git.commits`：共用会让用户打开历史分段时看到 500 条，或让概览
 * 只看到 30 条。两次请求各自落各自的字段（`ws.git.commits` / `ws.base.*`）。
 *
 * 只在起点是 `ok` 时才问：没有起点就没有「相对起点」这回事，问了也白问。
 */
function baseKick(tab: TabMeta): void {
  const ws = workspaces.get(tab.id)
  if (!ws) return
  if (!ws.base.rev || ws.base.status !== 'ok') return
  if (ws.base.reading) return
  ws.base.reading = true
  // 问不到不是故障（设备忙 / 通道被关）：概览停在 `null`，起点那一行照常显示。
  if (!gitRequest(tab, 'log', { limit: BASE_LOG_LIMIT })) ws.base.reading = false
}

/** 用户点了刷新：重问一次 status（丢失 diff 是刻意的，见 `applyGitFrame`）。 */
function gitRefresh(tab: TabMeta): void {
  const ws = workspaces.get(tab.id)
  if (!ws) return
  gitRequest(tab, 'status')
}

/**
 * `orig` 是重命名 / 复制的旧路径。
 *
 * 它必须一起发给设备侧：git 的 rename 检测要 pathspec 里**同时**有新旧两条
 * 路径，只给新路径时 git 判成「新建文件」，页面于是把一次改名显示成
 * 「新建了这个文件」——用户看不到「这是同一个文件改名了」。
 */
function gitShowDiff(tab: TabMeta, path: string, orig?: string): void {
  const ws = workspaces.get(tab.id)
  if (!ws) return
  // 从改动分段取 diff 时清掉「这份 diff 属于哪条提交」——不清的话 diff 面板
  // 会把它当历史 diff 渲染，`stale` 判据也就跟着错（见 `GitPanel` 的 `logReading`）。
  ws.git.logSelected = null
  ws.git.logReading = null
  // `reading` 只有在请求**确实发出去之后**才置位：`sendPanelRequest` 在通道未
  // ready / 取不到 id 时直接 return 且不调 `onError`（git 侧没有 `files` 那样的
  // 补发机制），先置位就会让面板永久停在「正在读取 diff…」。
  if (gitRequest(tab, 'diff', orig ? { path, orig } : { path })) ws.git.reading = path
}

/**
 * 用户切回改动分段。
 *
 * 与 `gitShowLog` **对称的一半**：历史侧把 `ws.git.diff` 写成「某条提交的
 * diff」，这里必须把它撤掉。不撤的话，切回改动后左栏是工作区文件列表、一行
 * 都没高亮，右栏却仍是那条提交的改动——两个提示指向两个事实。
 *
 * 只清不请求：`status` 在进入面板时已经问过，用户切回来时那块数据还在
 * （`gitRefresh` 负责重问）。这里再发一次 `status` 就是替用户做他没要的请求
 * ——和「切到历史才问 log」是同一把尺子。
 */
function gitShowChanges(tab: TabMeta): void {
  const ws = workspaces.get(tab.id)
  if (!ws) return
  ws.git.diff = null
  ws.git.reading = null
  ws.git.logSelected = null
  ws.git.logReading = null
}

/**
 * 用户切到历史分段：问最近 30 条提交。
 *
 * `limit` 写死 30 而不是做成可调的「加载更多」：v0.4.0 只要「历史提交可查」，
 * 而一个没有总数（git 不给）的分页控件比一次给 30 条更难用也对不了账。
 * 超过时设备回 `truncated:true`，页面明说「更早的没有列出」。
 *
 * **必须清 `diff`**：「改动」与「历史」两个分段共用同一个 `ws.git.diff`
 * （右栏的判据是 `GitPanel.vue:289` 那个 `v-if="!diff"`）。不清的话，在改动
 * 分段点过文件后切过来，右栏会把**工作区**的 diff 当成「这条提交改了什么」
 * 渲染出来——而 `GitLog.vue` 的文件头正明说那是另一件事。
 *
 * **`logSelected` 也要清**：不清的话上一轮选中的提交仍在 `GitLog` 里高亮
 * （`aria-selected`），而 `diff` 已被清成 `null`、右栏写着「点左边任一提交」。
 * 同一屏里高亮说「你选了这条」、正文说「你还没选」，是两件事。与
 * `gitShowDiff` 对称：那边切到时也清 `logSelected`。
 *
 * **反方向由 `gitShowChanges` 负责**：本函数是「进入历史」的一半，离开历史
 * 走 `gitShowChanges`。只写这一半的后果是 diff 从历史串到改动分段——方向
 * 相反，但同样是「两个提示指向两个事实」。
 */
function gitShowLog(tab: TabMeta): void {
  const ws = workspaces.get(tab.id)
  if (!ws) return
  ws.git.diff = null
  ws.git.reading = null
  ws.git.logSelected = null
  // **已经有列表就不重发**：用户在同一对分段之间来回切（看一处改动、回到历史
  // 核对一下、再看下一处）时，每切一次都跑一遍 `git log` 是替用户做他没要的
  // 请求——而 `GitPanel` 那条懒建立的注释写的动机正是「不要替用户做他没要的
  // 请求」。要拿最新历史用刷新按钮（它重问 `status`；历史仍然点「历史」进来
  // 时若想强制重取，先把分段切走再切回来不是可靠手段，所以这里不提供隐式
  // 刷新——`git log` 的结果不会因为面板多开一会儿而变化，除非用户在别处提交，
  // 而那种情况他本来就该点刷新）。
  if (ws.git.commits === null) gitRequest(tab, 'log', { limit: 30 })
}

/**
 * 用户点了某条提交：问**该提交**的 diff。
 *
 * 发 `commit`（完整 hash），不是 `path`——`path` 回的是「工作区现在改了什么」，
 * 而用户点的是历史上的一条提交。设备侧对 `commit` 用
 * `git show --format= --patch`（`git diff <hash>` 比的是 hash 与工作区，是另一件事）。
 */
function gitOpenCommit(tab: TabMeta, hash: string): void {
  const ws = workspaces.get(tab.id)
  if (!ws) return
  // 与 `gitShowLog` 对称：切到「看某条提交」时把上一份 diff 撤掉，否则
  // 从改动分段带过来的工作区 diff 会先闪一下再被替换。
  ws.git.diff = null
  ws.git.logReading = hash
  ws.git.logSelected = hash
  gitRequest(tab, 'diff', { commit: hash })
}

/**
 * 往 git 通道发一次请求。
 *
 * 与 `fileRequest` 分开而不是共用一个函数：两者的失败落点不同（files 写进
 * `ws.filesError`，git 写进 `ws.git.error`），合并就要在里面判 `panel`，
 * 而那正是「一个函数知道太多」的样子。共同的形状只有五行 `fetch`。
 *
 * 返回**请求是否真的发出去了**：未 ready / 取不到 id 时 `sendPanelRequest`
 * 直接返回且不调 `onError`，调用方据此决定要不要置「正在读取」标记——
 * 先置位再发就会在早返路径上留下一个永远不复位的 reading。
 */
function gitRequest(
  tab: TabMeta,
  op: 'status' | 'diff' | 'log',
  payload: Record<string, unknown> = {},
): boolean {
  return sendPanelRequest(tab, 'git', op, payload, (err: unknown) => {
    const ws = workspaces.get(tab.id)
    if (!ws) return
    ws.git.reading = null
    ws.git.logReading = null
    // 静默失败就是一块永远不动的面板：git 没有第二条反馈路径。
    ws.git.error =
      err instanceof ApiError
        ? `Hub 拒绝了这次 git 请求（${err.code}）。`
        : 'git 请求没能送到 Hub，面板内容可能已经过期。'
  })
}

/**
 * git 通道的帧落到 `ws.git` 上。
 *
 * **`status` 与 `diff` 各自只动自己的那几格**：一帧 `status` 不该把已经取到的
 * diff 清掉（用户点开文件后再刷新一次状态，diff 还在才对）；反过来一帧 `diff`
 * 也不该动分支名。只有 `error` 是两边共享的。
 *
 * **设备侧的失败走 `channel_error`（不是 `op:"error"`）**——见
 * `crates/node-core/src/connector/git.rs` 的 `run_git`：非零退出返回
 * `ConnectorError::Failed`，由通道层变成 `channel_error`，于是它到不了这里，
 * 而是进 `views.git.error`（错误码）与 `ws.git.error` 无关。
 *
 * **一个已知的可读性缺口**：设备侧 `ConnectorError` 的 `to_string()` 是带
 * detail 的（`#[error("invalid request: {detail}")]`），而通道层把码与这句话
 * 一起放进了 `channel_error` 的 `reason`；但 `ChannelHandle` 只暴露
 * `errorCode()`、不暴露 `reason`，所以页面只能显示「Hub 拒绝了这次 git 请求
 * （internal）。」——用户看不到「有一条在飞」这类可读原因。要补它得让
 * `lib/channel.ts` 把 `reason` 也存下来，那是通道层的事，不在本里程碑的
 * 只读面板范围内。这里写下来是为了让下一个人知道「不是设备没说，是页面没读」。
 */
function applyGitFrame(ws: TabWorkspace, tab: TabMeta, payload: unknown): void {
  const frame = asRecord(payload)
  if (!frame) return
  switch (frame.op) {
    case 'status':
      applyGitStatus(ws.git, frame)
      // status 回来了，改动文件数才是真的——这时才问概览那次 `log`。
      // **必须在 `applyGitStatus` 之后**：`applyBaseLog` 要用 `git.files.length`。
      if (frame.not_a_repo !== true) baseKick(tab)
      return
    case 'diff':
      applyGitDiff(ws.git, frame)
      return
    case 'log':
      // 同一条通道上可能有**两种** log 请求（概览 500 条 / 历史 30 条）。
      // 两者各写自己那份字段，谁也不会覆盖谁——判据只是「这一条在不在当前
      // 起点的历史里」，与请求本身无关，所以两边都算一遍是安全的。
      applyGitLog(ws.git, frame)
      applyBaseLog(ws.base, ws.git, frame)
      return
    case 'error':
      // git connector 目前不产生 `op:"error"`（失败走 `channel_error`），
      // 但设备侧将来若统一成这一形状，这里要能接住而不是静默丢弃。
      ws.git.reading = null
      ws.git.error = typeof frame.reason === 'string' ? frame.reason : '设备没能完成这次 git 操作。'
      return
    default:
      // 不认识的 op 一律不动状态（与 `applyFileFrame` 同一条：约束 14 的
      // 前向兼容在页面这一侧的对应物）。
      return
  }
}

/**
 * `op:"status"`：分支、HEAD、改动列表。
 *
 * `not_a_repo` 是设备侧的一等字段，不是「形状异常」：非仓库时它回的是
 * **正常回帧**（见 `connector/git.rs`），`branch` / `head` / `clean` 全是
 * `null`、`files` 为空。不认这个字段的话，非仓库会落进「还没问过」那一档，
 * 页面上一直写「正在读取工作区状态…」——一句永远不成立的话。
 */
function applyGitStatus(git: GitView, frame: Record<string, unknown>): void {
  git.error = null
  git.notARepo = frame.not_a_repo === true
  if (git.notARepo) {
    // 非仓库时后面这些字段没有意义，清空而不是留上一次的值：用户可能刚在
    // 另一个 tab 是仓库、切过来这台不是，残留的旧分支名会被当成这台的分支。
    git.branch = null
    git.head = null
    git.detached = false
    git.clean = null
    git.files = []
    git.diff = null
    git.reading = null
    return
  }
  git.branch = typeof frame.branch === 'string' ? frame.branch : null
  git.head = typeof frame.head === 'string' ? frame.head : null
  git.detached = frame.detached === true
  git.clean = frame.clean === true
  const raw = Array.isArray(frame.files) ? frame.files : []
  git.files = raw.flatMap((entry) => {
    const f = asRecord(entry)
    if (!f || typeof f.path !== 'string') return []
    // `orig` 只在重命名 / 复制时出现，且**必须留着**：问 diff 时它要一起
    // 发给设备侧（否则 git 把改名判成新建）。早先这里逐字段重建对象，
    // 只写了 x/y/path，`orig` 就在这行里被静默丢掉了。
    const out: GitStatusFile = {
      x: typeof f.x === 'string' ? f.x : ' ',
      y: typeof f.y === 'string' ? f.y : ' ',
      path: f.path,
    }
    if (typeof f.orig === 'string') out.orig = f.orig
    return [out]
  })
}

// ---------------------------------------------------------------------------
// 会话起点与产出概览（v0.6.0）
// ---------------------------------------------------------------------------

/** 概览那一次 `log` 要多少条。`MAX_LOG_LIMIT`（见 `connector/git.rs`）。 */
const BASE_LOG_LIMIT = 500

/** 起点的短 hash（前 7 位）。与设备侧 `rev` 的 `short` 是同一个截法。 */
function shortRev(rev: string): string {
  return rev.slice(0, 7)
}

/**
 * 从 tab 元信息里恢复起点（页面刚打开、`log` 还没飞时的初值）。
 *
 * **不覆盖已经算出来的概览**：`applyGitStatus` 之后会再调一次，那时
 * `progress` 已被 `log` 填上，这里不能把它清掉。
 */
function seedBase(ws: TabWorkspace, tab: TabMeta): void {
  ws.base.rev = tab.baseRev
  ws.base.status = tab.baseRevStatus
}

/**
 * 页面侧的起点派生档。**后端都不产生，所以不进 API**。
 */
type BasePhase =
  | BaseView['status']
  | 'stale'
  | 'over_limit'
  | 'device_removed'
  | 'device_offline'

/**
 * 起点在页面上处于哪一档。
 *
 * **Hub 的五态之外，页面再派生四档**：
 * - `device_removed`：设备已被删除。起点**照常显示**（它是历史事实，不因设备
 *   消失而失效），但概览不再请求（开不了通道）；
 * - `device_offline`：设备还在但开不了通道（离线 / 没上报 git）。起点同样照常
 *   显示，概览同样不请求。**与上一档分开**：一个该等设备上线，一个该找管理员
 *   恢复设备，两句话指向两个不同的下一步；
 * - `stale`：Hub 说 `ok`，但 `log` 里找不到那个 hash（被 rebase / 清理过）；
 * - `over_limit`：同样找不到，但 `truncated` 说「还有更早的没列出来」——那
 *   说明起点不是「不在历史里」，而是「比第 500 条还早」。
 *
 * **顺序有语义**：设备两档排在前面。设备不可达时 `log` 根本没发出去，
 * `overLimit` / `stale` 都还是上一轮的残留值——先判 `ok` 再判它们，会把
 * 「设备离线」显示成「起点已不在历史里」，那是一句**关于仓库的错话**。
 *
 * `truncated` 必须能把后两档分开：`truncated` 的判据是「取到 `limit + 1`
 * 条」，所以「恰好 500 条且起点是第 501 条」与「恰好 500 条且起点不在历史里」
 * 是同一个帧的前半段，只有 `truncated` 不同。把它写成 `>=` 会让两者混淆。
 */
function basePhase(ws: TabWorkspace): BasePhase {
  if (ws.base.deviceRemoved) return 'device_removed'
  if (ws.base.status !== 'ok') return ws.base.status
  if (ws.base.deviceOffline) return 'device_offline'
  if (ws.base.overLimit) return 'over_limit'
  if (ws.base.stale) return 'stale'
  return 'ok'
}

/**
 * 按会话的 `workspace` 两个字段判定「设备已删」与「设备离线」。
 *
 * **判据必须是两个字段的组合**（见 `WorkspaceRef`）：
 *
 * ```ts
 * workspace.node_id          // 设备被删时为 null；设备还在时是字符串（含离线）
 * workspace.removed_node_id  // 设备被删时为原 node_id；设备还在时为 undefined
 * ```
 *
 * 只判 `node_id === null` 会把「设备离线」也显示成「设备已被移除」。
 * 这里还额外要求 `removed_node_id` 在场：`node_id` 为 null 而它不存在的
 * 形状在当前 Hub 上不产生，但一旦出现，说「设备已被移除」是编造——
 * 那时只能说设备不可达。
 *
 * `offline` 参数来自**通道层的事实**（开通道回来的 `err::OFFLINE` /
 * `err::NOT_REPORTED`），不是「概览算不出来」：起点本来就可能是
 * `not_a_repo`，那时概览也不存在，但那不是设备不可达。
 */
function setBaseDevice(base: BaseView, ws: WorkspaceRef, offline: boolean): void {
  base.deviceRemoved = ws.node_id === null && ws.removed_node_id !== undefined
  // 设备已删就不再谈离线：两档互斥，且「已被移除」是更强的那句话。
  //
  // `node_id === null` **但**没有 `removed_node_id` 时落到离线档：这种形状下
  // 我们没有归属设备可连，说「当前不可达」是能确证的那句话，说「已被移除」
  // 则是编造（见 `setBaseDevice` 的注释与本文件那三条用例）。
  const noDevice = ws.node_id === null
  base.deviceOffline = !base.deviceRemoved && (noDevice || offline)
}

/**
 * `log` 帧到达后算出「相对起点有多少产出」。
 *
 * **提交数与未提交文件数分开**：`commits` 是 `base_rev` 之后的提交数，
 * `files` 是当前工作区未提交的改动文件数。加起来会让用户以为后者包含前者。
 *
 * **起点本身不算产出**：所以 `commits` 取的是 `base_rev` 在列表里的**下标**，
 * 不是下标 +1。
 */
function applyBaseLog(base: BaseView, git: GitView, frame: Record<string, unknown>): void {
  base.reading = false
  const rev = base.rev
  if (!rev) return
  const raw = Array.isArray(frame.commits) ? frame.commits : []
  const hashes = raw.flatMap((entry) => {
    const c = asRecord(entry)
    return c && typeof c.hash === 'string' ? [c.hash] : []
  })
  const truncated = frame.truncated === true
  const index = hashes.indexOf(rev)

  if (index >= 0) {
    // 起点自身不算产出：取下标，不是下标 + 1。
    base.progress = { commits: index, files: git.files.length }
    base.overLimit = false
    base.stale = false
    return
  }
  // 找不到起点，两档必须靠 `truncated` 分开（见 `basePhase`）。
  base.progress = null
  base.overLimit = truncated
  base.stale = !truncated
}

/**
 * 一句话的产出概览。**不用图表、不显示「完成度」**：产出没有目标，
 * 编一个分母就是在假装有意义。
 *
 * 返回 `null` = 这一档没有概览可讲（起点本来就没有 / 还没算出来）。
 */
function progressLine(ws: TabWorkspace): string | null {
  const phase = basePhase(ws)
  if (phase === 'ok') {
    const p = ws.base.progress
    if (!p) return null
    const head = `本次会话：从 ${ws.base.rev ? shortRev(ws.base.rev) : '?'} 起`
    const commits = p.commits === 1 ? '提交 1 次' : `提交 ${p.commits} 次`
    if (p.files === 0) return `${head}，${commits}，工作区无未提交改动`
    return `${head}，${commits}，另有 ${p.files} 个文件未提交`
  }
  if (phase === 'over_limit') {
    // 只报下界，不报一个确切数字：`truncated` 意味着至少 `limit + 1` 条。
    return `本次会话：从 ${ws.base.rev ? shortRev(ws.base.rev) : '?'} 起，提交超过 ${BASE_LOG_LIMIT} 次`
  }
  if (phase === 'stale') {
    return `本次会话：起点 ${ws.base.rev ? shortRev(ws.base.rev) : '?'} 已不在当前历史里（可能被 rebase 或清理过）`
  }
  if (phase === 'device_removed') {
    return '本次会话：设备已被移除，无法统计产出'
  }
  // `device_offline` 与上一档**不是同一条处置**：这台设备还会回来。
  // 所以不说「无法统计产出」（听起来像永久），说「现在」。
  if (phase === 'device_offline') {
    return '本次会话：设备当前不可达，暂时统计不了产出'
  }
  return null
}

/**
 * 起点那一行（会话身份）。五态 + 设备已删，每一档一句**互不相同**的话。
 *
 * 与 `progressLine` 分开：起点是「有没有」，产出是「有多少」。合成一句会在
 * 没有起点时把「为什么没有」和「所以统计不了」挤成半句。
 */
function baseIdentityLine(ws: TabWorkspace): string {
  const phase = basePhase(ws)
  if (phase === 'ok' || phase === 'stale' || phase === 'over_limit') {
    return ws.base.rev ? shortRev(ws.base.rev) : '未知'
  }
  // 设备已删 / 设备离线：起点仍在——它是会话建立那一刻的历史事实，
  // 与设备现在能不能连通无关。**不能因为开不了通道就说「未记录」**，
  // 那是把一个已知的事实说成不知道。
  if (phase === 'device_removed' || phase === 'device_offline') {
    return ws.base.rev ? shortRev(ws.base.rev) : '未记录'
  }
  if (phase === 'not_a_repo') return '不是 git 仓库'
  if (phase === 'no_head') return '仓库还没有提交'
  if (phase === 'unavailable') return '未能读取'
  return '未记录'
}

/** 起点那一行的 `title`：完整 hash + 一句时间说明。 */
function baseIdentityTitle(ws: TabWorkspace): string {
  const phase = basePhase(ws)
  if (
    ws.base.rev &&
    (phase === 'ok' ||
      phase === 'stale' ||
      phase === 'over_limit' ||
      phase === 'device_removed' ||
      phase === 'device_offline')
  ) {
    return `起点 ${ws.base.rev}（这次会话建立时工作区的 HEAD）`
  }
  if (phase === 'not_a_repo') return '这个目录不是 git 仓库，所以这次会话没有起点'
  if (phase === 'no_head') return '这个仓库还没有任何提交，所以没有 HEAD 可作起点'
  if (phase === 'unavailable') return '建会话时没能从设备读到起点（设备离线 / 超时 / 没装 git）'
  return '起点未记录（v0.6.0 之前建立的会话）'
}

/** `op:"diff"`：一份 unified diff。 */
function applyGitDiff(git: GitView, frame: Record<string, unknown>): void {
  git.reading = null
  git.logReading = null
  // 这份 diff 是**成功**取到的，所以失败横幅必须撤掉：`GitPanel` 的错误横幅
  // 排在文件列表与 diff 之前（`v-if="error"` 在 `v-else-if="clean === null"` 上面），
  // 不清的话一次失败的刷新会让之后所有成功的 diff 都被那行红字盖住。
  git.error = null
  git.diff = {
    path: typeof frame.path === 'string' ? frame.path : null,
    unified: typeof frame.unified === 'string' ? frame.unified : '',
    truncated: frame.truncated === true,
  }
}

/**
 * `op:"log"`：历史提交列表。
 *
 * **`commits` 从 `null` 变数组是「问过了」的唯一标志**：空数组是「这个仓库
 * 还没有提交」（设备侧对无提交仓库回空列表，那不是错误，见 `git.rs` 的
 * `is_empty_repo`），`null` 是「还没问」。混成一档的话新建仓库上会一直写
 * 「正在读取提交历史…」。
 */
function applyGitLog(git: GitView, frame: Record<string, unknown>): void {
  const raw = Array.isArray(frame.commits) ? frame.commits : []
  git.commits = raw.flatMap((entry) => {
    const c = asRecord(entry)
    if (!c || typeof c.hash !== 'string' || !c.hash) return []
    return [
      {
        hash: c.hash,
        author: typeof c.author === 'string' ? c.author : '',
        when: typeof c.when === 'string' ? c.when : '',
        subject: typeof c.subject === 'string' ? c.subject : '',
      },
    ]
  })
  git.logTruncated = frame.truncated === true
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
    baseRev: session.base_rev,
    baseRevStatus: session.base_rev_status,
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
  // 起点同样是「未知」而不是「没有」：这条 id 连会话都没有，谈不上起点。
  // 用 `unknown` 而不是 `not_a_repo` 之类——后者是一个具体的论断。
  return { id, title: `会话 ${id}`, path: '', nodeId: '', baseRev: null, baseRevStatus: 'unknown' }
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
    // 回落值是 **git** 而不是 terminal（v0.4.0）：`closeDock()` 会把 `panel`
    // 清成 `null`，所以「关掉坞再按 `o`」走的就是这条回落。用户拍板的形状是
    // 「打开就是 git 界面」——关掉再打开不该变成终端。
    //
    // 里程碑文档原判断「这条不必改，因为它优先用 `activeWs.panel`」漏了
    // `closeDock()` 清空 `panel` 这一层，是被上面这条用例抓出来的。
    openPanel(activeWs.value?.panel ?? DEFAULT_PANEL)
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
    // 列表到手了，`workspace` 的两个字段才可读——设备两档在这里定下来。
    // 通道层的事实此刻还没有（git 通道还没建），所以先按「能连」喂。
    syncBaseDevice(restored.tabs)
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
  // 抽屉若还作为浮层挂着，先出栈：留下一个 inert 的 body 会让整页点不动，
  // 而这个页面已经卸载了，没人会再帮它 pop。
  popNarrowDrawer()
  narrow.stop()
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
      <!--
        顶部三层（v0.5.0，对齐 opencode）：tab 条 → 标题行 → 状态行。

        **tab 条在最上**：它是会话页唯一的顶部导航，把「我在哪几个会话里、
        现在在哪个」放在最靠边、最容易被扫到的位置。它原来夹在页头与正文
        之间，读起来像「页面的第二行说明」。

        标题行与状态行拆成两层而不是挤成一行：标题行只留会话名与「打开 / 收起
        面板」，状态行只留设备 / 路径 / agent / 开始时间（更小的字、更矮的高度）。
        挤成一行时，窄屏下路径会把设备名挤没，而两者说的不是同一件事。
      -->
      <TabBar
        ref="tabBarEl"
        :tabs="tabs.tabs"
        :active-id="tabs.activeId"
        :panel-icons="panelIcons"
        :dirty="dirtyTabs"
        :conn-of="connOf"
        @activate="activate"
        @close="closeOneTab"
      />

      <div class="af-wb__head">
        <h1 class="af-wb__title">
          {{ title }}
          <StateBadge :state="ended ? 'ended' : 'active'" />
        </h1>
        <span class="af-spacer"></span>
        <button
          ref="openButtonEl"
          class="af-btn af-btn--sm"
          type="button"
          :disabled="ended"
          :title="ended ? '会话已结束：面板不会再建立通道' : '打开 Git 面板（o）'"
          data-open-panel
          @click="openPanel(DEFAULT_PANEL)"
        >
          <span aria-hidden="true">▤</span> 打开面板
          <kbd>o</kbd>
        </button>
        <button
          class="af-btn af-btn--sm af-btn--ghost"
          type="button"
          :disabled="ended"
          :aria-pressed="activeWs?.dock ? 'true' : 'false'"
          :title="'打开 / 收起右侧那一格（\\）'"
          @click="toggleDock"
        >
          {{ activeWs?.dock ? '收起面板' : '面板坞' }}
        </button>
      </div>

      <!--
        状态行是**独立**的语义块，不复用 `.af-wb__meta`：它的内容更少、
        字更小、行更矮，后续还要往里放分支名。复用同一组 class 会让两个块
        的样式互相牵制（调状态行的行高就动了页头）。
      -->
      <div class="af-wb__statusline">
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
        <!--
          起点：会话身份的一部分（与设备、路径同级），所以在这里、不在 git 面板里。
          切到终端 / 文件树时它必须还在——那正是「换工具不该丢掉『这次从哪开始』」
          的全部理由。`title` 给完整 hash。
        -->
        <span v-if="activeWs" class="af-wb__baserev" data-base-rev>
          <span class="af-t-mute">起点</span>
          <code
            class="af-fp"
            :title="baseIdentityTitle(activeWs)"
            :data-base-status="basePhase(activeWs)"
          >{{ baseIdentityLine(activeWs) }}</code>
        </span>
        <span class="af-t-mute">·</span>
        <span>{{ startedLabel }}</span>
      </div>

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
          :style="activeWs?.dock ? { flex: `0 0 ${activeWs.split}%` } : undefined"
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
            :running="activeWs?.running ?? false"
            @run="runCommand"
            @send="convSend"
            @say="convSay"
            @cancel="convCancel"
            @open-traj="toggleTraj"
          />
          <!--
            键位提示条（v0.5.0）：文案与 `lib/keynav.ts` 的 label **必须说同一件事**。
            `?` 面板是用户查得到的承诺，这条提示是他没按 `?` 之前唯一看得见的
            那句——两处不一致时，用户信的是眼前这句，然后被 `]` 杀掉一个 shell。

            所以这里不写「切换面板」：v0.5.0 起 `[` / `]` 是**替换**当前工具，
            并释放被换掉那个的通道（对终端就是杀掉 shell）。这句话不好塞进
            一行提示，于是给出最短的诚实版本「换工具（终端会被杀掉）」。
          -->
          <div class="af-wb__convhint af-field__hint">
            <span><kbd>o</kbd> 聚焦右侧那一格</span>
            <!--
              窄屏下 `[` / `]` 仍然可用（换工具不因屏宽分叉），只是抽屉
              盖住了对话，这条提示看不见——所以窄屏改用 Esc 这一句。
            -->
            <span v-if="!isNarrow"><kbd>[</kbd><kbd>]</kbd> 换工具（终端会被杀掉）</span>
            <span v-else><kbd>Esc</kbd> 收起抽屉</span>
            <span><kbd>\</kbd> 收起右侧那一格</span>
            <span class="af-spacer"></span>
            <span><kbd>⇧Tab</kbd> 回 tab 条</span>
          </div>
        </div>

        <!--
          v-model 直连 workspace：`split` 是**每个会话自己的**布局（见 TabWorkspace）。
          绑页面的一个 ref 会让两个 tab 共用一个数——切 tab 时比例跳一下，
          用户会以为分栏坏了。
        -->
        <!--
          窄屏才有把手：< 900px 时右侧是覆盖式抽屉（对话全宽），比例不参与
          布局——留一个拖不动的把手在这里只会让人以为它坏了。
        -->
        <SplitHandle
          v-if="activeWs?.dock && !isNarrow"
          :model-value="activeWs.split"
          @update:model-value="setSplit"
          @reset="resetSplit"
        />

        <!--
          ── 为什么包一层 Teleport，且 `:disabled` 跟着 `isNarrow` ─────────
          宽屏：右侧是并列的一列，**必须留在原位**参与 flex 布局，所以禁用
          Teleport（`disabled` 时它原地渲染，与没有这层完全一样）。

          窄屏：它变成一层**模态浮层**，而 `focus.push` 的 `inertOthers` 只把
          「body 直接子节点里、不含浮层容器的那几个」置 inert。抽屉留在
          `.af-wb__body` 里时，它所在的那棵子树就是整个 `#app`——而 `#app`
          是 body 唯一的子节点，于是「背后全部 inert」退化成「一个都没
          inert」：遮罩盖住了视觉，背后的对话区仍然可 Tab、可点。
          （`TrajDrawer.vue` / `App.vue` 的帮助面板踩的是同一个坑，注释里
          写着同一件事。）

          遮罩跟着一起 Teleport：留在原位的话它会被 `#app` 的 inert 影响，
          且定位基准变成 `.af-wb__body` 而不是视口。
        -->
        <Teleport to="body" :disabled="!isNarrow">
          <!--
            窄屏的遮罩：抽屉是最上层的一层浮层，点外面 = 关。
            **只在窄屏存在**——宽屏下右侧是并列的一列，它旁边没有「外面」。
            用 `@click.self` 而不是给遮罩单独一个按钮：遮罩本身就是那块区域，
            点在抽屉里的事件会冒泡到遮罩，`self` 把它们挡掉。
          -->
          <div
            v-if="drawerIsLayer"
            class="af-wb__scrim"
            data-dock-scrim
            @click.self="closeDock"
          ></div>

          <div
            v-if="activeWs?.dock"
            ref="dockEl"
            class="af-wb__dockwrap"
            :data-drawer="isNarrow ? 'true' : undefined"
            :aria-label="isNarrow ? '面板' : undefined"
          >
          <PanelDock
            :panel="activeWs.panel"
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
            <!--
              git 面板：`status` + 逐文件 `diff`（v0.4.0 只读）。

              与终端 / 文件同一条纪律：**不持通道**——取数由本页翻译
              （`gitRequest` / `applyGitFrame`），面板只吃 props、只发 intent。
            -->
            <template #git>
              <!--
                本次会话产出概览。**放在 `GitPanel` 之上、由本页自己渲染**，
                而不是给 `GitPanel` 加一个 slot：面板只吃 props、只发 intent，
                而这份概览的数据源（`ws.base`）不属于 git 面板的视图状态。
                放这里也满足「优先不改 `GitPanel.vue`」——它的 12 个 props 一个没动。

                只在有话说的时候渲染：没有起点时这一行会是空白，白白吃掉一条高度。
              -->
              <p
                v-if="activeWs.panel === 'git' && progressLine(activeWs)"
                class="af-wb__baseov"
                data-base-overview
              >
                {{ progressLine(activeWs) }}
              </p>
              <GitPanel
                v-if="activeWs.panel === 'git'"
                :state="activeWs.views.git.state"
                :branch="activeWs.git.branch"
                :head="activeWs.git.head"
                :detached="activeWs.git.detached"
                :clean="activeWs.git.clean"
                :files="activeWs.git.files"
                :not-a-repo="activeWs.git.notARepo"
                :diff="activeWs.git.diff"
                :reading="activeWs.git.reading"
                :error="activeWs.git.error"
                :commits="activeWs.git.commits"
                :log-truncated="activeWs.git.logTruncated"
                :log-reading="activeWs.git.logReading"
                :log-selected="activeWs.git.logSelected"
                @refresh="activeTab && gitRefresh(activeTab)"
                @diff="(path: string, orig?: string) => activeTab && gitShowDiff(activeTab, path, orig)"
                @log="activeTab && gitShowLog(activeTab)"
                @changes="activeTab && gitShowChanges(activeTab)"
                @open-commit="(hash: string) => activeTab && gitOpenCommit(activeTab, hash)"
              />
            </template>
          </PanelDock>
          </div>
        </Teleport>
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
/* 遮罩（v0.5.0）：只在窄屏渲染（`v-if="drawerIsLayer"`），所以这里不需要
   媒体查询——它出现的时候必然已经是覆盖态。 */
.af-wb__scrim {
  position: absolute; inset: 0;
  background: var(--af-scrim);
  z-index: 1;
}

/*
  < 900px：右侧一格转**覆盖式抽屉**（§7.1 / D15）。

  断点这个数必须与 `lib/media.ts` 的 `NARROW_MAX_PX` 一致——CSS 的媒体查询
  JS 读不到，所以它天然有两份。`media.test.ts` 会把这两边钉在一起。
*/
@media (max-width: 900px) {
  /* 抽屉是 `position: absolute`，需要 `relative` 的锚点 */
  .af-wb__body {
    position: relative;
  }
  /* 对话全宽：抽屉盖在上面，不占布局宽度。`!important` 是因为宽屏的
     flex-basis 走内联样式（`activeWs.split`），内联的优先级高于类选择器。 */
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
    /* 抽屉比遮罩高一层：遮罩盖住对话，抽屉盖住遮罩 */
    z-index: 2;
    /* 缩到窗口宽度以内：`min()` 之上再兜一道，防止某个浏览器的 `min`
       在某些缩放下算大了导致横向溢出（412px 那条用例的判据）。 */
    max-width: 100vw;
    min-width: 0;
  }
}
</style>
