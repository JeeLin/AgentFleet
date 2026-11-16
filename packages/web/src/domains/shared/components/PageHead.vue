<script setup lang="ts">
/* ===========================================================================
   PageHead —— 原型页面头 `.af-head` 的共享骨架
   ---------------------------------------------------------------------------
   渲染的就是原型每一页自己画的那一行页头（`prototype/index.html:15-23`）：

     <header class="af-head">
       <h1 class="af-head__title">总览</h1>
       <span class="af-head__sub">今天要处理的事，按项目分</span>
       <span class="af-head__spacer"></span>
       <a class="af-btn af-btn--primary">新建项目</a>
     </header>

   两段槽位与原型一一对应：
     - 默认槽（主区）：页头这一行里的标题 / 副标题 / 徽标，排进去就是左对齐
       的一串。调用方自己写 `h1.af-head__title` 与 `.af-head__sub`。
     - `actions` 槽：右侧动作。它们前面有一个 `.af-head__spacer`
       （`prototype/assets/app.css:130`：`margin-left: auto`），把动作顶到
       右边——原型就是这么做的，**不是**再套一层「动作 div」。
       spacer 在组件内**恒渲染**：页头形状不随内容变，没有动作时它也不承载
       任何东西。

   抽组件之前页头有三种写法，都写歪了：
     ① 主区包一层裸 `<div>`，右侧靠 `<span class="af-spacer">` 顶住
        （`AgentDetailPage` / `DeviceDetailPage`）；
     ② 徽标直接扔在页头这一行里，用一个**空 span** 当间隔柱
        （`ProjectSettingsPage.vue:230`，**没迁**，归子任务 9；那里原来写的是
        自造的 `af-page__actions`，已换成原型的 `af-head__spacer`——
        `project-settings.html:18` 就是同一个位置）；
     ③ 自造的 `.af-page__head` + `.af-page__actions` 两层包裹（这一族规则在
        v0.8.1 子任务 2 已随类名一起删除：它们在原型 `app.css` 里命中 0）。

   调用点（4 页，满足「≥2 页才建」的门槛）：
     AgentDetailPage / DeviceDetailPage / ProjectTaskPage / BindPage

   为什么默认槽里允许一个包住标题 + 副标题的 `<div>`：原型 21 页的页头里
   `.af-head__title` 与 `.af-head__sub` 一律是**兄弟节点**（例如
   `devices.html:16-17`），副标题是标题右边的一列小字。实现里
   `AgentDetailPage` / `DeviceDetailPage` / `ProjectTaskPage` 的副标题是一整句
   说明（20-40 字），排在标题右边会把标题挤到换行——所以那里保留一个包住
   「标题 + 副标题」的 `<div>`，副标题落在标题下面。**槽位结构与类名不变**
   （`af-head__title` / `af-head__sub` 照原型写），差别只在排版由调用方的
   那几个节点决定。`BindPage` 没有副标题，默认槽里是原型那样的平铺兄弟节点。

   样式不在这里：`.af-head` / `.af-head__title` / `.af-head__sub` /
   `.af-head__spacer` 的规则住在 `styles/prototype-shell.css` §1
   （`:222-249`，取值对齐 `prototype/assets/app.css:118-130`）。组件只负责
   结构，不新增第二条样式真相源。
   =========================================================================== */
</script>

<template>
  <header class="af-head">
    <slot />
    <span class="af-head__spacer"></span>
    <slot name="actions" />
  </header>
</template>
