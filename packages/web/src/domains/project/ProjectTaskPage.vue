<script setup lang="ts">
/* ===========================================================================
   ProjectTaskPage —— 任务（task.html，v0.8.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.8.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '任务详情与六态流转',
  '验收判定（done_when 判定码 + 人话）',
  '失败面板与重试',
] as const
</script>

<template>
  <SkeletonPage
    title="任务"
    prototype="task.html"
    version="v0.8.0"
    :items="items"
  />
</template>
