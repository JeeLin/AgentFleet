<script setup lang="ts">
/* ===========================================================================
   ButlerPage —— 管家（butler-im.html，v0.9.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.9.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '在 IM 里跟管家说话',
  '审查管家写的内容',
  '不可逆操作一律回 Hub 确认',
] as const
</script>

<template>
  <SkeletonPage
    title="管家"
    prototype="butler-im.html"
    version="v0.9.0"
    :items="items"
  />
</template>
