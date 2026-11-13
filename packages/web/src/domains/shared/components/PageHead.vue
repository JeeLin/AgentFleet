<script setup lang="ts">
/* ===========================================================================
   PageHead —— 页面头「主区 + 右侧动作」的共享骨架
   ---------------------------------------------------------------------------
   为什么值得抽成组件（而不是停在 `.af-page__head` 这一族 CSS 上）：
   `.af-page__head` 只给了 `flex` + 两端对齐 + 间距，**剩下的结构每个页面
   各写一遍**，而且已经写歪了两种：

     ① `AgentDetailPage` / `DeviceDetailPage` —— 主区包一层裸 `<div>`（要给
        标题 + 副标题一组），右侧靠 `<span class="af-spacer">` 把按钮顶到右边；
     ② `BindPage` / `ProjectSettingsPage` —— 徽标直接扔在页头这一行里，用
        一个**空 span** `<span class="af-page__actions"></span>` 当间隔柱。

   ②是纯粹的将就：空元素不承载语义，读屏会忽略它，而「按钮为什么在右边」在
   结构上完全看不出来。两种写法的可见结果一致，但后续每加一个动作按钮都要
   先猜这一页用的是哪一派。组件把这件事定成一条：**主区（slot 默认） + 动作
   （slot `actions`）**，右侧动作自己排，页头不再需要 spacer 柱。

   调用点（4 页，满足「≥2 页才建」的门槛）：
     AgentDetailPage / DeviceDetailPage / ProjectTaskPage / BindPage

   不吞掉 `.af-page__head` 这个类名：组件渲染的就是它，样式仍住在
   `styles/components.css` §2，组件只负责结构，不新增第二条样式真相源。
   =========================================================================== */
</script>

<template>
  <header class="af-page__head">
    <slot />
    <div class="af-page__actions">
      <slot name="actions" />
    </div>
  </header>
</template>
