<script setup lang="ts">
/* ===========================================================================
   SkillsSettingsPage —— 技能与 MCP（skills.html，v0.12.0 开放）
   ---------------------------------------------------------------------------
   子任务 8 的骨架页：**路由与层级是真的，业务数据归 v0.12.0**。
   为什么现在就要建：否则指向它的页内链接全是死链，而用户看到的是「点了没反应」而不是「还没做」。
   =========================================================================== */

import SkeletonPage from '@/domains/shared/components/SkeletonPage.vue'

const items = [
  '任务级授予的技能 / MCP',
  '凭据不进 agent 上下文',
  '每次连接进审计轨迹',
] as const
</script>

<template>
  <SkeletonPage
    title="技能与 MCP"
    prototype="skills.html"
    version="v0.12.0"
    :items="items"
  />
</template>
