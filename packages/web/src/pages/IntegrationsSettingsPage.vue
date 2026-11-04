<script setup lang="ts">
/* ===========================================================================
   IntegrationsSettingsPage —— 外部 issue 导入（integrations.html，v0.10.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.10.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  'issue 单向导入为任务',
  '结论单向往回写（不做双向同步）',
] as const
</script>

<template>
  <SkeletonPage
    title="外部 issue 导入"
    prototype="integrations.html"
    version="v0.10.0"
    :items="items"
  />
</template>
