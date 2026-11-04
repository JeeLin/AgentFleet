<script setup lang="ts">
/* ===========================================================================
   ProjectSettingsPage —— 项目设置（project-settings.html，v0.7.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.7.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '改名与主题色',
  '形态升级（隔离式 → 中心式）',
  '归档项目',
] as const
</script>

<template>
  <SkeletonPage
    title="项目设置"
    prototype="project-settings.html"
    version="v0.7.0"
    :items="items"
  />
</template>
