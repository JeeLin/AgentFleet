<script setup lang="ts">
/* ===========================================================================
   EmptyState —— 空态（规范 §3.11）
   ---------------------------------------------------------------------------
   文案三段式：字形 + 标题 + **下一步**。空态必须给出路——只说「这里是空的」
   等于把「然后呢」留给用户自己想。

   这里把「下一步」做成具名 slot 而不是一段文本：有的空态要按钮，有的要
   一条命令，有的只要一句「M3 起会有」。塞进 prop 会逼每个调用方传 HTML。

   # `compact` —— 横排紧凑形态（v0.8.1 子任务 12）
   -------------------------------------------------------------------------
   **为什么加**（用户 2026-11-17：「现在的页面很差」，实测截图里空态是
   居中大虚线框 + 三段居中文字，空库时整页看起来像「加载失败」）：默认形态
   是「页面本身就只有这一件事」时才该用的（例如某个独立页整体为空）。而当
   空态出现在**内容区里的一块**（总览页的项目区、设备页的表格区），大块居中
   占位会把整页撑成「什么都没有」，还和错误态长得几乎一样。

   `compact` 形态是**横排**：字形在左、标题 + 说明在右、动作在末尾，高度
   约为一到两行，读起来是「这里现在是空的，点这里开始」，不是「出事了」。

   **向后兼容**：`compact` 默认 `false`，不传就是原来的居中形态；既有的
   `glyph` / `title` / `text` 三个 prop 与默认 slot **一个都没改**。
   =========================================================================== */

withDefaults(
  defineProps<{
    glyph?: string
    title: string
    /** 为什么是空的、什么时候会有。这段是空态的核心，不是副标题。 */
    text?: string
    /** 横排紧凑形态：字形在左、文字在右、动作在末尾。见上方说明。 */
    compact?: boolean
  }>(),
  { glyph: '▢', text: '', compact: false },
)
</script>

<template>
  <div class="af-empty" :class="{ 'af-empty--compact': compact }" role="status">
    <div class="af-empty__glyph" aria-hidden="true">{{ glyph }}</div>
    <div class="af-empty__main">
      <div class="af-empty__title">{{ title }}</div>
      <p v-if="text" class="af-empty__text">{{ text }}</p>
    </div>
    <div v-if="$slots.default" class="af-empty__actions">
      <slot />
    </div>
  </div>
</template>
