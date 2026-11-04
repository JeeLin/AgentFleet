<script setup lang="ts">
/* ===========================================================================
   BindPage —— 绑定设备（project-bind.html，v0.7.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.7.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '一台设备一行：固定目录、磁盘配额与已用',
  '增删绑定、改固定目录',
] as const
</script>

<template>
  <SkeletonPage
    title="绑定设备"
    prototype="project-bind.html"
    version="v0.7.0"
    :items="items"
  />
</template>
