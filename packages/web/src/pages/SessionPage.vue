<script setup lang="ts">
/* ===========================================================================
   SessionPage —— 会话工作台（骨架，规范 §5.4 / §5.5 / §7.1 / §7.2）
   ---------------------------------------------------------------------------
   13a 只做**结构与焦点**：对话列 + 可拖拽分栏 + 面板坞 + 顶部 tab 条。
   对话流（13d）、终端（13b）、文件（13c）各自挂进来；这里不画假数据。

   ── 每个 tab 持有自己的一份状态 ───────────────────────────────────
   `{ dock, panel, mounted }` + 终端缓冲 + 文件树 cwd 挂在 workspace 对象上，
   切 tab 时整体换掉。只有一个全局 state 的话，切到 B 会把 A 的面板与输出
   全冲掉——于是「支持多个」只剩一条 tab 条，是个假的（§5.5.2）。

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

import EmptyState from '@/components/EmptyState.vue'
import PanelDock, { type ChannelView, type PanelName } from '@/components/PanelDock.vue'
import SplitHandle from '@/components/SplitHandle.vue'
import StateBadge from '@/components/StateBadge.vue'
import TabBar, { type TabConn } from '@/components/TabBar.vue'
import type { DeviceSummary, SessionSummary } from '@/lib/api'
import { listDevices, listSessions } from '@/lib/api'
import { ChannelHub, type ChannelHandle, type ChannelState } from '@/lib/channel'
import { relativeTime, sessionTitle } from '@/lib/format'
import { ApiError, NetworkError } from '@/lib/http'
import { dispatch, isTyping, register, unregister } from '@/lib/keynav'
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

function deviceName(nodeId: string): string {
  return deviceOf(nodeId)?.name ?? nodeId
}

function connOf(nodeId: string): TabConn {
  return deviceOf(nodeId)?.status ?? 'offline'
}

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
  /** 终端缓冲：纯文本，切走再回来仍在。 */
  output: string
  /** 文件面板当前目录（相对工作区根）。 */
  cwd: string[]
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

function channelKey(tabId: string, panel: PanelName): string {
  return `${tabId}\u0000${panel}`
}

function ensureChannel(tab: TabMeta, panel: PanelName): ChannelHandle {
  const key = channelKey(tab.id, panel)
  const existing = channels.get(key)
  if (existing) return existing
  if (!hub) throw new Error('channel hub is not mounted')
  const handle = hub.handle(key, {
    sessionId: tab.id,
    connector: panel,
    // terminal 的 cwd 跟着会话走（约束 3）：Hub 只透传 params，node 不会替
    // 页面猜一个工作区根。
    params: panel === 'terminal' ? { cwd: tab.path, cols: 80, rows: 24 } : {},
  })
  channels.set(key, handle)
  const ws = workspaceOf(tab.id)
  const unsub = handle.onState((state: ChannelState) => {
    ws.views[panel].state = state
    ws.views[panel].error = handle.errorCode()
    ws.views[panel].dropped = handle.dropped()
  })
  channelUnsubs.set(key, unsub)
  return handle
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

const dockEl = ref<HTMLElement | null>(null)
const openButtonEl = ref<HTMLButtonElement | null>(null)

/* ---------------------------------------------------------------- 工作台头 */

const title = computed(() => (current.value ? sessionTitle(current.value) : '会话工作台'))
const deviceLabel = computed(() =>
  current.value ? deviceName(current.value.workspace.node_id) : '—',
)
const startedLabel = computed(() =>
  current.value ? `${relativeTime(current.value.created_at)}开始` : '',
)

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

const KEY_ACTS: Readonly<Record<string, string>> = {
  o: 'panel:open',
  '[': 'panel:prev',
  ']': 'panel:next',
  '\\': 'dock:toggle',
}
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
}

function onKeydown(e: KeyboardEvent): void {
  if (e.defaultPrevented || e.isComposing) return
  if (e.ctrlKey || e.metaKey || e.altKey) return
  // 单键在输入区内不抢（§4.2）：终端 / 对话输入框里按 o 是在打字
  if (isTyping(e.target)) return
  const act = KEY_ACTS[e.key]
  if (!act) return
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
    // 好让页面能给出「找不到 / 已结束」而不是一片空白。
    const id = route.value.params.id
    const known = list.find((s) => s.id === id)
    const fallback: TabMeta[] = known
      ? [metaOf(known)]
      : [{ id, title: id, path: '', nodeId: '' }]
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
    loadError.value = errorLabel(err)
  } finally {
    loading.value = false
  }
}

function errorLabel(err: unknown): string {
  if (err instanceof ApiError) {
    return err.code === 'session_expired'
      ? '登录已失效，请重新登录。'
      : `读会话失败（${err.code}）。`
  }
  if (err instanceof NetworkError) return 'Hub 连不上。检查 Hub 进程是否还在。'
  return '读会话失败。'
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
  for (const act of Object.keys(HANDLERS)) unregister(act)
  // 离开工作台 = 页面销毁：所有通道随之释放。切 tab 不走这里。
  hub?.dispose()
  hub = null
  channels.clear()
  channelUnsubs.clear()
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
      text="它可能已经被删掉，或者这条链接指向一个从未建成的会话。回到会话列表看看现在有哪些。"
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
          <span class="af-t-mute">agent 未选（M3 起可选）</span>
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
        :conn-of="connOf"
        @activate="activate"
        @close="closeOneTab"
      />

      <p v-if="ended" class="af-wb__banner" role="note">
        这个会话已结束：工作区不再可达，面板不会再建立通道。列表里仍然保留它，是为了能查到
        它曾在哪台设备的哪条路径上跑过。
      </p>

      <div class="af-wb__body">
        <!-- 对话列：13d 挂入真实对话流；这里只有它的位置与一句说明 -->
        <div
          ref="bodyEl"
          class="af-wb__conv"
          tabindex="-1"
          data-wb-body
          :style="activeWs?.dock ? { flex: `0 0 ${split}%` } : undefined"
        >
          <div class="af-wb__convvoid">
            <div class="af-flow__day">对话</div>
            <p class="af-wb__convtxt">
              M1 的会话是<b>页面级容器</b>：它有工作区与面板，但没有 ACP 会话与轨迹——
              对话流由 13d 挂入，数据在 M3 才存在。这里不画假消息。
            </p>
            <p class="af-wb__convtxt af-t-mute">
              面板在右边：第一次与它交互才会建立通道。同时开多个会话时，用上方 tab 条切换——
              切换不会释放通道，关掉 tab 才会。
            </p>
          </div>
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
          />
        </div>
      </div>
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
  font-size: var(--af-text-xs);
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
.af-wb__convvoid {
  flex: 1 1 auto;
  overflow: auto;
  padding: var(--af-space-5);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
.af-wb__convtxt {
  margin: 0;
  max-width: 68ch;
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
  color: var(--af-fg-secondary);
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
