<script setup lang="ts">
/* ===========================================================================
   TrajDrawer —— 轨迹抽屉
   ---------------------------------------------------------------------------
   **轨迹是抽屉不是第三栏**：轨迹 = `trajectory_event` 的 append-only 流水，
   对话是它按 kind 折叠后的读法。做成两个并列视图会暗示「两套不同的数据」，
   于是用户去比「对话里有几条、轨迹里有几条」——而它们本来必须逐条对齐。
   第三栏又会把对话挤到 1/3 屏宽。所以它是**覆盖层**。

   ── 这一版接上真数据 ────────────────────────────────────────
   以前抽屉恒空（「M3 起有数据」），那是 M1 的事实。现在有真数据了，所以：
   列表按物理顺序画（`TrajectoryRepo::list` 的口径是 `id` 升序，不是 `seq`——
   `seq` 是设备数的、重连后会从 1 重数，它只在一次进程生命周期内单调）。

   ── 为什么分页 ────────────────────────────────────────────
   一轮长对话几百条事件，一次全画出来会让抽屉首屏卡住、也让「翻到底」变成
   一件没有尽头的事。所以按页取（每页 200 条）+ 「加载更多」按钮。
   **不做虚拟滚动**：抽屉只有 520px 宽、条目是纯文本，200 条一页的 DOM
   开销远小于一套虚拟滚动带来的位置计算与可访问性代价（读屏读不到没渲染的
   行，而轨迹恰恰是要被逐条读的）。

   ── 断口画在列表里 ──────────────────────────────────────────
   与对话区同一套判据、同一个理由：缺 N 条就写「缺失 N 条」，末轮没收口就写
   「本轮未完成」。**不补内容**——ACP 没有重发机制，`session/load` 是重建流
   不是原始事件流，补不回来。

   ── 三条关闭路径，一条实现 ────────────────────────────────────
   Esc / 点遮罩 / 点关闭按钮**都走 `close()`**，而 `close()` 里唯一的动作是
   `focus.pop()`。焦点归还是焦点栈的事，组件自己**不许** `opener.focus()`：
   三条路三条实现，只要有一条忘了还焦点，键盘用户就「丢了」。

   ── 为什么 Teleport 到 body ────────────────────────────────────
   `focus.push` 的 `inertOthers` 把**除容器所在的那棵 body 子树之外**的
   body 直接子节点设为 `inert`。抽屉如果留在会话工作台的 DOM 里，它所在的
   那棵子树就是整个 `#app`，于是「背后全部 inert」退化成「一个都没 inert」——
   遮罩盖住了视觉，背后的控件仍然可 Tab、可点、可被读屏读到。挂到 body 上
   才让「最内层容器之外一律 inert」这条不变量成立。

   ── 键位不在这里注册 ─────────────────────────────────────────
   `t` / `g g` → `traj:toggle` 是会话工作台的页面级键位，由页面注册
   （`keynav.register` 拒绝重复注册，两边都注册会直接抛）。本组件只认
   `open` 这个 prop：键位、按钮、程序化调用都是改变它的不同入口。

   ── 为什么 `opener` 是 prop 而不是读 `activeElement` ─────────────
   让抽屉自己去猜 `document.activeElement` 在 Safari 上会猜错：点一个
   `<button>` 不移动焦点，那时 `activeElement` 是 `body`。触发方（ConvFlow 的
   轨迹按钮、页面的键位 handler）知道是哪个元素，就由它交出来。
   =========================================================================== */

import { computed, onBeforeUnmount, ref, watch } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import type { TrajectoryEvent, TrajectoryGap } from '@/lib/api'
import { handleLayerKeydown, pop, push } from '@/lib/focus'

/** 一页取多少条。200 条纯文本行的 DOM 开销是可接受的（见模块注释）。 */
const PAGE_SIZE = 200

/**
 * 一段序号空洞：`[from, to)` 里的 seq 都不在库里。
 *
 * 与 `hub-storage/src/trajectory.rs` 的 `Gap` 同一套坐标，`missing` 由 Hub 算好
 * ——前端不重算，两处各算一份会分叉。
 */
/*
  断口形状直接用 `api.ts` 的 `TrajectoryGap`：原先自己声明同形的 `TrajGap`，
  而 `SessionPage` 传给本组件和 `ConvFlow` 的是同一个数组，本地那份不增加
  任何类型信息，只多一个会跟着漂的副本。
*/

const props = withDefaults(
  defineProps<{
    /** 抽屉开吗。本组件是受控的：不自己记住「关了」。 */
    open: boolean
    /**
     * 打开它的元素。**关闭后焦点还给它**。
     * `null` 时焦点栈退到 `main[tabindex="-1"]`（那是「这里是一片内容的起点」）。
     */
    opener: HTMLElement | null
    /** 会话标题（抽屉头重复一次：两个会话并排时靠它分）。 */
    title: string
    /** 会话所属设备名。 */
    deviceName: string
    /** 会话工作区路径（锁定）。 */
    path: string
    /** 已取到的事件（按物理顺序）。 */
    events?: TrajectoryEvent[]
    /** 序号断口。Hub 算好给过来。 */
    gaps?: TrajectoryGap[]
    /** 最后一轮没有 `turn_end`：还在跑，或结尾丢了。 */
    incomplete?: boolean
    /** 还有更多没取回来吗（决定「加载更多」在不在）。 */
    hasMore?: boolean
    /** 正在取下一页。 */
    loading?: boolean
    /** 取失败的原因。不显示一个空抽屉，而是说清为什么空。 */
    error?: string | null
  }>(),
  {
    opener: null,
    title: '',
    deviceName: '',
    path: '',
    events: () => [],
    gaps: () => [],
    incomplete: false,
    hasMore: false,
    loading: false,
    error: null,
  },
)

const emit = defineEmits<{
  /** 请求关闭。父级把 `open` 置回 `false`；焦点已由 `pop()` 还回去了。 */
  (e: 'close'): void
  /** 取下一页。父级去拉，拉完把 `events` 换掉。 */
  (e: 'more'): void
}>()

const rootEl = ref<HTMLElement | null>(null)
/** 这一层是否已经入栈。防重复 `push`（`open` 为真时重渲染不该再入一次）。 */
let pushed = false

const EMPTY_TEXT =
  '轨迹是 trajectory_event 的 append-only 流水：还没有事件写进来。' +
  '对话是它按 kind 折叠后的读法，所以这里和对话区是同一份数据的两种看法。'

/**
 * 事件行，断口已插到它该在的位置。
 *
 * **断口按「它后面那条存在的事件」索引，也就是 `gap.to`**。`Gap` 是半开区间
 * `[from, to)`，`to` 是那个**存在**的 seq。按 `from` 索引是错的且错得很隐蔽：
 * 断口里的 seq 都不在库里，于是永远找不到那个锚点，标记一个也不画——
 * 页面看起来就是连续的。
 *
 * 在 script 里排好而不是在模板里 `v-if` 查表：模板表达式是 JS 不是 TS，
 * 那里没法写类型断言。
 */
type TrajRow =
  | { t: 'gap'; key: string; missing: number; after: number }
  | { t: 'row'; key: string; ev: TrajectoryEvent }

const rows = computed<TrajRow[]>(() => {
  const before = new Map<number, TrajectoryGap>()
  for (const g of props.gaps) before.set(g.to, g)

  const out: TrajRow[] = []
  for (const ev of props.events) {
    const gap = before.get(ev.seq)
    if (gap) {
      out.push({
        t: 'gap',
        key: `gap-${gap.from}`,
        missing: gap.missing,
        // `after` 是断口前最后一条**存在**的 seq，也就是 `from - 1`
        after: gap.from - 1,
      })
    }
    out.push({ t: 'row', key: `${ev.id}`, ev })
  }
  return out
})

const rowCount = computed(() => props.events.length)

function openLayer(): void {
  const el = rootEl.value
  if (!el || pushed) return
  // 开合只走焦点栈：`push` 负责背后 inert 与首焦点，`pop` 负责还焦点。
  push(el, { opener: props.opener })
  pushed = true
  document.addEventListener('keydown', onKeydown)
}

function closeLayer(): void {
  const el = rootEl.value
  if (!el || !pushed) return
  pushed = false
  document.removeEventListener('keydown', onKeydown)
  pop(el)
}

/**
 * 关闭。**三条路都走这里**，所以「焦点回到触发元素」没有第二条实现。
 *
 * 先 `pop()`（还焦点）再 `emit('close')`：反过来的话父级把 `open` 置 false
 * 会先摘掉 DOM，而还焦点需要 opener 还在文档里——顺序反了就是丢焦点。
 */
function close(): void {
  closeLayer()
  emit('close')
}

/** 点遮罩：只有点在遮罩自己上才算（点在抽屉里不该关）。 */
function onScrim(e: MouseEvent): void {
  if (e.target === e.currentTarget) close()
}

/**
 * Esc 与 Tab。判据（本层是栈顶吗、输入法选字中吗）都在 `handleLayerKeydown`
 * 里——`LoginPrompt.vue` 有同一份需求，两份实现迟早会漂成两种键位。
 */
function onKeydown(e: KeyboardEvent): void {
  const root = rootEl.value
  if (!root) return
  handleLayerKeydown(e, root, close)
}

/**
 * payload 预览：截到 120 字符。
 *
 * **截断只影响显示，不影响数据**——详情在对话区里，这里只是「定位用」的一行。
 * 不截断的话一条 `rawOutput` 能把整屏占满。
 */
function preview(ev: TrajectoryEvent): string {
  const p = ev.payload
  return p.length > 120 ? `${p.slice(0, 120)}…` : p
}

watch(
  () => props.open,
  (open) => {
    if (open) {
      // 抽屉是 `v-if` 渲出来的：这一帧 `rootEl` 还是 null，所以入栈推到
      // 微任务里。用 `immediate` 而不是只在变化时开，是因为**首帧就开着**
      // 才是本组件的常态（父级按 `t` 键先切 `open` 再让它渲染）——只有
      // 「变化」那个触发点会让第一次挂载完全不入栈。
      void Promise.resolve().then(() => {
        if (props.open) openLayer()
      })
      return
    }
    // 「父级自己关掉了」这条路径也要出栈：否则焦点栈上留一个已经不存在的层，
    // 下一次 Esc 会关掉一个空层（§4.1）。
    closeLayer()
  },
  { immediate: true },
)

onBeforeUnmount(() => {
  // 开着就走一条完整的关闭路径：直接摘 DOM 会把焦点留在 body 上
  closeLayer()
})
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      ref="rootEl"
      class="af-scrim af-traj"
      role="dialog"
      aria-modal="true"
      aria-label="轨迹抽屉"
      tabindex="-1"
      data-traj
      @click="onScrim"
    >
      <div class="af-traj__panel">
        <div class="af-traj__head">
          <span class="af-panel__title">轨迹</span>
          <span class="af-t-mute af-traj__where">
            <span aria-hidden="true">▣</span> {{ deviceName }}
            <span class="af-t-mute">·</span>
            <span class="af-fp">{{ path }}</span>
          </span>
          <span class="af-spacer"></span>
          <span v-if="title" class="af-t-mute af-traj__title">{{ title }}</span>
          <button
            class="af-btn af-btn--ghost af-btn--sm af-btn--icon"
            type="button"
            aria-label="关闭轨迹抽屉（Esc）"
            title="关闭轨迹抽屉（Esc）"
            data-autofocus
            data-traj-close
            @click="close"
          >
            ✕
          </button>
        </div>

        <!-- 取失败：说清为什么空，而不是给一个空列表让用户以为「没有轨迹」 -->
        <div v-if="error" class="af-traj__error" role="alert">
          <span class="af-badge af-badge--warn">读不到轨迹</span>
          <span>{{ error }}</span>
        </div>

        <div class="af-traj__body" data-traj-body>
          <EmptyState
            v-if="!error && rowCount === 0"
            glyph="≡"
            title="还没有事件"
            :text="EMPTY_TEXT"
          />

            <template v-else-if="!error">
              <ol class="af-traj__list" data-traj-list>
                <template v-for="row in rows" :key="row.key">
                  <li v-if="row.t === 'gap'" class="af-traj__gap" data-traj-gap>
                    <span aria-hidden="true">⋯</span>
                    缺失 {{ row.missing }} 条（seq {{ row.after }} 之后断开）
                  </li>
                  <li v-else class="af-traj__row" data-traj-row>
                    <span class="af-traj__seq">{{ row.ev.seq }}</span>
                    <span class="af-traj__kind">{{ row.ev.kind }}</span>
                    <span class="af-traj__payload af-fp">{{ preview(row.ev) }}</span>
                  </li>
                </template>
              </ol>

              <!-- 末轮没收口：与对话区同一句判据，两个视图必须说同一件事 -->
              <p v-if="incomplete" class="af-traj__incomplete" data-traj-incomplete>
                <span aria-hidden="true">◌</span>
                本轮未完成：没有收到 <span class="af-fp">turn_end</span>，
                可能还在跑，也可能结尾丢了。
              </p>

              <div v-if="hasMore" class="af-traj__more">
                <button
                  class="af-btn af-btn--sm"
                  type="button"
                  :disabled="loading"
                  data-traj-more
                  @click="emit('more')"
                >
                  {{ loading ? '正在取…' : `加载更多（每页 ${PAGE_SIZE} 条）` }}
                </button>
              </div>
            </template>
        </div>

        <div class="af-traj__foot af-field__hint">
          <span>{{ rowCount }} 条</span>
          <span v-if="gaps.length" class="af-traj__gaps" data-traj-gaps>
            断口 {{ gaps.length }} 处
          </span>
          <span class="af-spacer"></span>
          <span><kbd>Esc</kbd> 关闭</span>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
/* 遮罩即容器：它是 `role="dialog"` 的那层，点它自己 = 关闭（§3.10） */
.af-traj {
  position: fixed;
  inset: 0;
  z-index: var(--af-z-drawer);
  display: flex;
  justify-content: flex-end;
  background: var(--af-scrim);
}
.af-traj__panel {
  display: flex;
  flex-direction: column;
  width: min(520px, 92%);
  height: 100%;
  background: var(--af-surface-1);
  border-left: 1px solid var(--af-border-strong);
  box-shadow: var(--af-shadow-3);
}
.af-traj__head {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  padding: var(--af-space-4) var(--af-space-5);
  border-bottom: 1px solid var(--af-border-subtle);
  background: var(--af-surface-2);
}
.af-traj__where,
.af-traj__title {
  display: inline-flex;
  align-items: center;
  gap: var(--af-space-2);
  min-width: 0;
  font-size: var(--af-text-xs);
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.af-traj__body {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  display: flex;
  flex-direction: column;
  justify-content: center;
}
/* 有内容时顶对齐：垂直居中会让第一条「跳」到屏幕中间 */
.af-traj__body:has(.af-traj__list) {
  justify-content: flex-start;
}
.af-traj__list {
  margin: 0;
  padding: 0;
  list-style: none;
}
.af-traj__row {
  display: grid;
  grid-template-columns: 4ch auto 1fr;
  gap: var(--af-space-2) var(--af-space-3);
  align-items: baseline;
  padding: var(--af-space-2) var(--af-space-5);
  border-bottom: 1px solid var(--af-border-subtle);
  font-size: var(--af-text-xs);
}
.af-traj__seq {
  color: var(--af-fg-tertiary);
  font-variant-numeric: tabular-nums;
  text-align: right;
}
.af-traj__kind {
  color: var(--af-fg-secondary);
  white-space: nowrap;
}
.af-traj__payload {
  min-width: 0;
  color: var(--af-fg-muted);
  overflow-wrap: anywhere;
}
.af-traj__gap {
  grid-column: 1 / -1;
  margin: 0;
  padding: var(--af-space-1) var(--af-space-2);
  color: var(--af-fg-tertiary);
  border: 1px dashed var(--af-border-default);
  border-radius: var(--af-radius-sm);
}
.af-traj__incomplete {
  margin: 0;
  padding: var(--af-space-2) var(--af-space-5);
  font-size: var(--af-text-xs);
  color: var(--af-warn);
}
.af-traj__more {
  display: flex;
  justify-content: center;
  padding: var(--af-space-4);
}
.af-traj__error {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  padding: var(--af-space-3) var(--af-space-5);
  font-size: var(--af-text-xs);
  color: var(--af-fg-secondary);
  background: var(--af-surface-2);
  border-bottom: 1px solid var(--af-warn);
}
.af-traj__foot {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  padding: var(--af-space-3) var(--af-space-5);
  border-top: 1px solid var(--af-border-subtle);
  background: var(--af-surface-2);
}
/* 遮罩是 dialog 容器而不是控件：它可编程聚焦，但不该画成一个「整块被选中」 */
.af-traj:focus,
.af-traj:focus-visible {
  outline: none;
}
</style>
