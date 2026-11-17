<script setup lang="ts">
/* ===========================================================================
   SkeletonPage —— 尚未实现页面的统一外壳（v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   为什么需要它：子任务 8 要按 `prototype/` 一次铺齐 21 条路由，但其中 19 页的
   业务逻辑归后续里程碑（v0.8.0 看板数据、v0.9.0 管家对话、v0.10.0 issue 导入、
   v0.11.0 定时、v0.12.0 技能）。这些路由要真实存在（否则页内链接全是死链），
   但不能假装做完了。

   **页面顶部必须写明本页在哪个版本开放**——这是子任务 8 与「不留占位页」那条
   老约束之间的界线的具体落地（见 `router.ts` 头注释）：那个约束反对的是
   **导航里灰掉的条目**，不是「路由表里有这条路径」。写清版本号，用户就知道
   这是「还没做」而不是「做坏了」。

   一个组件而不是 19 份拷贝：那段「为什么是空的 + 什么时候有」的文案有固定
   形状，抄 19 遍必然漂移（有一页忘了写版本号，用户就会以为它坏了）。
   =========================================================================== */

import PageHead from '@/domains/shared/components/PageHead.vue'

withDefaults(
  defineProps<{
    /** 页面标题，与 `prototype/` 对应页一致。 */
    title: string
    /** 本页在原型的哪个文件，便于实现者对照（也便于审查时核对）。 */
    prototype: string
    /** 计划开放的版本，如 `v0.8.0`。 */
    version: string
    /** 这页将来会有什么。**写具体的**，不写「敬请期待」。 */
    items?: readonly string[]
  }>(),
  { items: () => [] },
)
</script>

<template>
  <!-- 骨架页也要用原型的页面骨架（`header.af-head` + `div.af-body` 两块并排），
       否则它渲染出来的页头没有几何：上一版用的是自造的 `.af-page` /
       `.af-page__head` / `.af-page__title`，而那一族规则已随 v0.8.1 子任务 2
       的类名删除，于是 **10 个占位页**（执行监控 / 回放 / 时间线 / 定时 / 收件箱 /
       动态 / 管家 / 三个设置子页）一直是一行没有样式的裸 `<h1>`。
       它们不在「10 个已交付页面」的清单里，门禁扫不到；2026-11-17 子任务 10
       逐页扫 `af-page` 残留时抓到。

       两段根节点：与另外 12 个页面同一形状（`PageHead` 与 `.af-body` 是兄弟）。
       骨架页不接收透传 attrs，多根不会触发 fallthrough 警告。 -->
  <PageHead>
    <div>
      <h1 class="af-head__title">{{ title }}</h1>
      <p class="af-skeletonpage__note">
        本页在 <strong>{{ version }}</strong> 开放。设计稿见
        <code>{{ prototype }}</code>。
      </p>
    </div>
  </PageHead>

  <div class="af-body">
    <div class="af-skeletonpage__pad">
      <div class="af-skeletonpage__body">
        <slot>
          <ul v-if="items.length" class="af-skeletonpage__list">
            <li v-for="item in items" :key="item">{{ item }}</li>
          </ul>
          <p v-else class="af-skeletonpage__empty">这一页还没有可显示的内容。</p>
        </slot>
      </div>
    </div>
  </div>
</template>
