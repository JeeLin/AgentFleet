<script setup lang="ts">
/* ===========================================================================
   ReplayPage —— 执行复盘（replay.html，v0.8.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.8.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '只读轨迹回放',
  '该次执行的改动清单',
] as const
</script>

<template>
  <SkeletonPage
    title="执行复盘"
    prototype="replay.html"
    version="v0.8.0"
    :items="items"
  />
</template>
