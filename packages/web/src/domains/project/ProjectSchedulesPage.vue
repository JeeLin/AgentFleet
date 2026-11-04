<script setup lang="ts">
/* ===========================================================================
   ProjectSchedulesPage —— 定时任务（schedules.html，v0.11.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.11.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  'cron 式与「某任务完成后触发」两类来源',
  '错过窗口的补偿策略（跳过 / 补跑）',
] as const
</script>

<template>
  <SkeletonPage
    title="定时任务"
    prototype="schedules.html"
    version="v0.11.0"
    :items="items"
  />
</template>
