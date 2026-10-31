<script setup lang="ts">
/* ===========================================================================
   SplitHandle —— 对话区与面板坞之间的把手（规范 §3.15 / §7.1）
   ---------------------------------------------------------------------------
   它是 `role="separator"`，可聚焦，`←→` 调 4%、`Home` / `End` 到 30% / 70%，
   **双击恢复缺省比例**。
   能拖、能按、能用键盘——**没有键盘的把手等于鼠标专属**，上一版就是这样。

   ── 为什么双击恢复缺省是必要的（v0.5.0） ──────────────────────────
   右侧变成一格之后，用户会比以前更频繁地拖它：文件树要宽、终端也要宽、
   git diff 还要宽，三种需求差得远。于是「拖到极端之后怎么回去」从罕见
   变成常见——没有回头路的话，用户只能靠方向键一格一格挪回去，而他还得
   先记得缺省是 56。

   ── 为什么不是 <input type="range"> ────────────────────────────────
   原生 range 在垂直方向上是「一条带刻度的滑轨」，视觉上会打断两块面板的
   并置关系；而这里是「两块内容之间的一条缝」，缝的视觉重量必须接近 0。
   代价是键盘与 ARIA 要自己补齐——`role=separator` + `aria-valuenow/min/max`
   就是补的那一半，所以它**不是**一个 div 加 tabindex。

   ── 指针拖动为什么用 Pointer Events + setPointerCapture ────────────
   鼠标拖出把手、拖到 iframe/终端上时，`mousemove` 会丢；`setPointerCapture`
   把后续事件都锁到这个元素上。拖动期间把 `user-select` 关掉，
   否则整块界面会被刷成蓝色选中。
   =========================================================================== */

import { ref } from 'vue'

import { SPLIT_MAX, SPLIT_MIN, splitFromDrag, stepSplit } from '@/lib/split'

const props = defineProps<{
  /** 当前百分比（30–70）。 */
  modelValue: number
}>()

const emit = defineEmits<{
  (e: 'update:modelValue', value: number): void
  /**
   * 双击 = 恢复缺省比例。**由宿主决定缺省值是多少**，组件只知道「要恢复」。
   *
   * 组件内不知道 `SPLIT_DEFAULT` 的语义（它是「对话列占多少」，是页面的事），
   * 让把手自己写死一个数就把那个数变成了两处真相源。
   */
  (e: 'reset'): void
}>()

const dragging = ref(false)
/** 拖动起点：按下时的百分比 + 指针 x。两者一起算增量，避免累计误差。 */
let origin: { percent: number; x: number } | null = null

function trackWidth(el: HTMLElement): number {
  const parent = el.parentElement
  return parent ? parent.getBoundingClientRect().width : 0
}

function onPointerDown(e: PointerEvent): void {
  if (e.button !== 0) return
  const el = e.currentTarget as HTMLElement
  origin = { percent: props.modelValue, x: e.clientX }
  dragging.value = true
  el.setPointerCapture(e.pointerId)
  // 拖动期间不要选中文本：指针扫过对话流时，整段字会被刷蓝
  document.body.style.userSelect = 'none'
  e.preventDefault()
}

function onPointerMove(e: PointerEvent): void {
  if (!dragging.value || !origin) return
  const el = e.currentTarget as HTMLElement
  emit('update:modelValue', splitFromDrag(origin.percent, e.clientX - origin.x, trackWidth(el)))
}

function endDrag(e: PointerEvent): void {
  if (!dragging.value) return
  dragging.value = false
  origin = null
  document.body.style.userSelect = ''
  const el = e.currentTarget as HTMLElement
  if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId)
}

function onKeydown(e: KeyboardEvent): void {
  // 中文输入法选字期间的按键不是「用户想调宽度」
  if (e.isComposing) return
  let next: number | null = null
  switch (e.key) {
    case 'ArrowLeft':
      next = stepSplit(props.modelValue, 'left')
      break
    case 'ArrowRight':
      next = stepSplit(props.modelValue, 'right')
      break
    case 'Home':
      next = stepSplit(props.modelValue, 'home')
      break
    case 'End':
      next = stepSplit(props.modelValue, 'end')
      break
    default:
      return
  }
  // 只有真的移动了才吞键：到头再按 `→` 时把键还给浏览器，
  // 否则「按了没反应但键也没了」。
  if (next === props.modelValue) return
  e.preventDefault()
  emit('update:modelValue', next)
}
</script>

<template>
  <div
    class="af-gutter"
    role="separator"
    tabindex="0"
    aria-orientation="vertical"
    aria-label="调整对话与面板宽度"
    :aria-valuenow="Math.round(modelValue)"
    :aria-valuemin="SPLIT_MIN"
    :aria-valuemax="SPLIT_MAX"
    :aria-valuetext="`对话区占 ${Math.round(modelValue)}%`"
    :data-dragging="dragging ? 'true' : undefined"
    @pointerdown="onPointerDown"
    @pointermove="onPointerMove"
    @pointerup="endDrag"
    @pointercancel="endDrag"
    @lostpointercapture="endDrag"
    @keydown="onKeydown"
    @dblclick="emit('reset')"
  >
    <span class="af-track" aria-hidden="true"></span>
    <span class="af-gutter__label" aria-hidden="true">← → 调整</span>
  </div>
</template>
