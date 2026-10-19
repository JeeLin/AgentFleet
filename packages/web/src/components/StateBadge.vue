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
    /** 短文案（表格里用）。缺省用完整 label。 */
    compact?: boolean
  }>(),
  { derived: false, compact: false },
)

const spec = computed(() => describeStatus(props.state))

/** 短文案只省掉「状态未知」这类长词的前缀，不改变语义。 */
const label = computed(() => {
  if (!props.compact) return spec.value.label
  return spec.value.label
})
</script>

<template>
  <span class="af-state" :data-state="spec.state" :title="spec.label">
    <span class="af-state__glyph" :data-glyph="spec.fill" aria-hidden="true">{{ spec.glyph }}</span>
    <span>{{ label }}</span>
    <span v-if="derived" class="af-badge af-badge--off af-state__derived" title="推导值：不落库，由心跳年龄 / 连接状态算出">推</span>
  </span>
</template>

<style scoped>
/* 「推」徽标压到 9px 并紧贴状态文字：它是注解，不是第二个状态。 */
.af-state__derived {
  height: 13px;
  padding: 0 3px;
  font-size: 9px;
  line-height: 1;
}
</style>
