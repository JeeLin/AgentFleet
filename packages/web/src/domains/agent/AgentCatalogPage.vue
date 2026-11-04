<script setup lang="ts">
/* ===========================================================================
   AgentCatalogPage —— Agent（`prototype/agents.html`，v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   Agent 从 v0.7.0 起重回**一级导航**（新原型把它单独成一页）。v0.5.0 曾把它
   并进设备页的 `?view=agents` 分段，那时侧栏只留三项；项目主轴改版后
   「从设备 / agent 进」是管理面的两个并列入口，分开才对。

   这一页是**真数据**：`AgentCatalog` 本来就自读自管（`GET /api/devices` 的
   agent 字段），不需要重写。子任务 7 会在这里加「该 agent 上跑过哪些任务 /
   成功率」——那要任务模型，归 v0.8.0 之后。

   **不把 `<AgentCatalog>` 的加载逻辑复制过来**：它有 `load` 与自身的空态，
   复制一份必然漂移（一份加了重试、另一份没加）。
   =========================================================================== */

import AgentCatalog from '@/domains/agent/AgentCatalog.vue'
</script>

<template>
  <section class="af-page">
    <header class="af-page__head">
      <h1 class="af-page__title">Agent</h1>
      <p class="af-muted">
        各设备上装了哪些 agent、哪些跑不起来。Hub 不代装，只扫描与读写启动配置。
      </p>
    </header>

    <AgentCatalog />

    <section class="af-page__section">
      <h2 class="af-page__subtitle">任务与成功率</h2>
      <p class="af-muted">
        「该 agent 上跑过哪些任务、成功率多少」要等任务模型落地（v0.8.0 之后）才有数据。
      </p>
    </section>
  </section>
</template>
