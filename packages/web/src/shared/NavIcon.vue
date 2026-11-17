<script setup lang="ts">
/**
 * 一个导航图标（收起态图标条用）。
 *
 * `aria-hidden` 固定为 true：收起态没有可见文字，无障碍名由**外层链接的
 * `aria-label`** 提供；展开态有 `.af-rail__label` 文字，图标纯装饰。
 * 所以这个组件不接收 `alt` 之类的参数——它能安全地只做装饰。
 */
import { NAV_ICON_PATHS, type NavIconKey } from '@/shared/navIcons'

const props = withDefaults(
  defineProps<{
    /** 形状键，见 `shared/navIcons.ts` 的形状选择依据 */
    name: NavIconKey
    /** 边长（px）。默认 18：56px 宽的图标条里，左右各留 19px 呼吸 */
    size?: number
  }>(),
  { size: 18 },
)
</script>

<template>
  <svg
    class="af-navicon"
    :width="props.size"
    :height="props.size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.6"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    focusable="false"
  >
    <path v-for="(d, i) in NAV_ICON_PATHS[props.name]" :key="i" :d="d" />
  </svg>
</template>
