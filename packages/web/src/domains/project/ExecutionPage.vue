<script setup lang="ts">
/* ===========================================================================
   ExecutionPage —— 执行监控（execution.html，v0.8.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.8.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '慢变摘要（时长 + 最后一次动作 + 是否卡住）',
  '按行订阅的执行输出',
  '不编进度百分比',
] as const
</script>

<template>
  <SkeletonPage
    title="执行监控"
    prototype="execution.html"
    version="v0.8.0"
    :items="items"
  />
</template>
