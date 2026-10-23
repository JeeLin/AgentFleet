<script setup lang="ts">
/* ===========================================================================
   StateBadge —— 状态的四通道（规范 §3.4）
   ---------------------------------------------------------------------------
   颜色 + 字形 + 填充 + **必带文字**。文字不是可选装饰：色觉障碍、灰度打印、
   8px 缩略、reduce-motion 下，前三个通道任一单独都不足以识别。

   `derived` 是 §6.1 要求的那枚「推」徽标：`reconnecting` 只存在于 Hub 内存，
   用户需要知道**这个状态的来源**——它比状态本身更容易被误当成持久事实。
   =========================================================================== */

import { computed } from 'vue'

import { describeStatus } from '@/lib/state'

const props = withDefaults(
  defineProps<{
    state: string
    /** 状态是前端/Hub 内存推导值，不是落库事实。 */
    derived?: boolean
  }>(),
  { derived: false },
)

const spec = computed(() => describeStatus(props.state))
</script>

<template>
  <span class="af-state" :data-state="spec.state" :title="spec.label">
    <span class="af-state__glyph" :data-glyph="spec.fill" aria-hidden="true">{{ spec.glyph }}</span>
    <span>{{ spec.label }}</span>
    <span v-if="derived" class="af-badge af-badge--off af-state__derived" title="推导值：不落库，由心跳年龄 / 连接状态算出">推</span>
  </span>
</template>

<style scoped>
/* 「推」徽标压到 `text-2xs`（10px）并紧贴状态文字：它是注解，不是第二个状态。
   早先是裸 `font-size: 9px`——一个不在 design-system.md §2.3 九档里的字号，
   于是「什么时候该用哪一档」在它身上没有依据。 */
.af-state__derived {
  height: 13px;
  padding: 0 3px;
  font-size: var(--af-text-2xs);
  line-height: 1;
  font-weight: 600;
}
</style>
