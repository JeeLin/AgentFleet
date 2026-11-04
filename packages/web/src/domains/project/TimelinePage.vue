<script setup lang="ts">
/* ===========================================================================
   TimelinePage —— 时间线（timeline.html，v0.8.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.8.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '看板的第二种投影（同一批事实按时间排）',
  '执行与任务的交错视图',
] as const
</script>

<template>
  <SkeletonPage
    title="时间线"
    prototype="timeline.html"
    version="v0.8.0"
    :items="items"
  />
</template>
