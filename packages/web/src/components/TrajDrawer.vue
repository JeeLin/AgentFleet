<script setup lang="ts">
/* ===========================================================================
   TrajDrawer —— 轨迹抽屉（规范 §7.5 / §3.10 / §4.1 / §3.11）
   ---------------------------------------------------------------------------
   **轨迹是抽屉不是第三栏**（§5.4 / §7.5）：轨迹 = `trajectory_event` 的
   append-only 流水，对话是它按 kind 折叠后的读法。做成两个并列视图会暗示
   「两套不同的数据」，于是用户去比「对话里有几条、轨迹里有几条」——而它们
   本来必须逐条对齐。第三栏又会把对话挤到 1/3 屏宽。所以它是**覆盖层**。

   ── 13d 只做框架与空态 ────────────────────────────────────────
   抽屉的骨架（头 / 遮罩 / 三条关闭路径 / 焦点陷阱）是 M1 就要有的，
   抽屉的**内容**在 M1 恒空：M1 的会话容器不产生 `trajectory_event`
   （ACP 归 M3）。这里不画假行——空态明写「M3 起有数据」以及为什么。

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

import { onBeforeUnmount, ref, watch } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import { pop, push, topContainer, trapTab } from '@/lib/focus'

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
  }>(),
  { opener: null, title: '', deviceName: '', path: '' },
)

const emit = defineEmits<{
  /** 请求关闭。父级把 `open` 置回 `false`；焦点已由 `pop()` 还回去了。 */
  (e: 'close'): void
}>()

const rootEl = ref<HTMLElement | null>(null)
/** 这一层是否已经入栈。防重复 `push`（`open` 为真时重渲染不该再入一次）。 */
let pushed = false

const EMPTY_TEXT =
  'M3 起有数据。轨迹是 trajectory_event 的 append-only 流水，对话是它按 kind ' +
  '折叠后的读法；M1 的会话容器不产生这条流（ACP 归 M3）。这里不画假行。'

/** `design-system.md:736`：M3 起每行带齐这五项。本阶段只预告字段，不画行。 */
const FIELDS = ['seq', 'kind', '内容', '耗时', '归属']

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
 * Esc 与 Tab。
 *
 * Esc 只在**本层是栈顶**时才生效：`pop()` 按栈顶语义工作，而键盘事件是
 * 全站共用的——不判栈顶的话，帮助面板盖在抽屉上时一次 Esc 会关掉两层。
 */
function onKeydown(e: KeyboardEvent): void {
  if (e.isComposing) return
  const root = rootEl.value
  if (!root || topContainer() !== root) return
  if (e.key === 'Escape') {
    e.preventDefault()
    close()
    return
  }
  if (e.key === 'Tab') {
    // 焦点陷阱（§3.10）。`trapTab` 内部已经 `preventDefault` 了它接管的那些。
    trapTab(e, root)
  }
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

        <div class="af-traj__body">
          <EmptyState glyph="≡" title="M3 起有数据" :text="EMPTY_TEXT" />
        </div>

        <!-- 字段预告：说的是 M3 会带齐什么，不是一行假数据 -->
        <div class="af-traj__foot af-field__hint">
          <span class="af-badge">M3</span>
          <span>
            每行带齐 <span v-for="f in FIELDS" :key="f" class="af-fp">{{ f }}</span>
            五项（design-system §5.4）；本阶段不画这些行。
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
