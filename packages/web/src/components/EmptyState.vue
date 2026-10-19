<script setup lang="ts">
/* ===========================================================================
   EmptyState —— 空态（规范 §3.11）
   ---------------------------------------------------------------------------
   文案三段式：字形 + 标题 + **下一步**。空态必须给出路——只说「这里是空的」
   等于把「然后呢」留给用户自己想。

   这里把「下一步」做成具名 slot 而不是一段文本：有的空态要按钮，有的要
   一条命令，有的只要一句「M3 起会有」。塞进 prop 会逼每个调用方传 HTML。
   =========================================================================== */

withDefaults(
  defineProps<{
    glyph?: string
    title: string
    /** 为什么是空的、什么时候会有。这段是空态的核心，不是副标题。 */
    text?: string
  }>(),
  { glyph: '▢', text: '' },
)
</script>

<template>
  <div class="af-empty" role="status">
    <div class="af-empty__glyph" aria-hidden="true">{{ glyph }}</div>
    <div class="af-empty__title">{{ title }}</div>
    <p v-if="text" class="af-empty__text">{{ text }}</p>
    <div v-if="$slots.default" class="af-empty__actions">
      <slot />
    </div>
  </div>
</template>
