/* ===========================================================================
   media.ts —— 视口断点（规范 §7.1 的 < 900px 覆盖式降级）
   ---------------------------------------------------------------------------
   只有一个用途：告诉 JS「现在是窄屏还是宽屏」。CSS 那边已经有同一道断点
   （`SessionPage.vue` 的 `@media (max-width: 900px)`），这里**必须读同一个数**。

   ── 这两份数怎么不漂开 ────────────────────────────────────────────
   CSS 的媒体查询不能被 JS 读，所以这个数天然有两份：一份在这里，一份在
   `SessionPage.vue` 的 `@media (max-width: 900px)` 里。与**本模块有关**的
   那一份由 `media.test.ts` 钉住（它扫 `SessionPage.vue` 的媒体查询并与
   `NARROW_MAX_PX` 比对），漂开的后果很难看：JS 认为窄屏、CSS 认为宽屏，
   于是抽屉叠在对话上却按并列算宽度。

   **本文件不是全仓断点的唯一源**：`SettingsPage.vue` / `NewSessionPage.vue`
   也各有一处 `@media (max-width: 900px)`，它们与抽屉语义无关、不在本模块的
   契约范围内，`media.test.ts` 不覆盖它们。别把「窄屏抽屉的断点收在一处」
   读成「全仓断点已统一」。

   ── 为什么窄屏态需要 JS，不能只靠 CSS ─────────────────────────────
   窄屏的抽屉是**一层浮层**：它要有遮罩、Esc 能关、焦点被圈住、关闭后回到
   触发元素。这四件事都不是样式能做的。所以「是不是窄屏」必须有一个 JS
   可读的真相——否则浮层在宽屏下也会生效（宽屏是并列，不该抢焦点）。
   =========================================================================== */

import { ref, type Ref } from 'vue'

/**
 * 窄屏断点（px，含）。**与 `SessionPage.vue` 的 `@media (max-width: 900px)` 同一个数。**
 *
 * 900 而不是 320：这个控制台是「笔记本 / 大屏」优先的，树莓派本身可能没有
 * 浏览器（§7.1 的既有裁定，不因本文件而改）。
 *
 * 除 `SessionPage.vue` 之外，`SettingsPage.vue` / `NewSessionPage.vue` 也有
 * 900px 的媒体查询——那些与抽屉语义无关，未被 `media.test.ts` 覆盖，见文件头。
 */
export const NARROW_MAX_PX = 900

/** 媒体查询串。由 `NARROW_MAX_PX` 拼出来，不另写一个 900。 */
export const NARROW_QUERY = `(max-width: ${NARROW_MAX_PX}px)`

/** 取消订阅。调用方在卸载时调它。 */
export type Unsubscribe = () => void

/**
 * 订阅一个媒体查询的当前结果。
 *
 * 没有 `matchMedia`（happy-dom 的部分版本、SSR）时**恒定返回 `false`**，
 * 而不是抛错：这个值只决定「要不要走覆盖式降级」，拿不到就按宽屏走——
 * 宽屏路径是功能更全的那一条，退到它不会丢任何能力。
 *
 * 清理函数由**调用方**在 `onBeforeUnmount` 里调，而不是这里自己挂生命周期：
 * 这个模块是纯函数库，同一个 ref 也可能被非组件代码持有（例如测试）。
 * 自己挂 `onBeforeUnmount` 会在组件外调用时静默失效并打一条警告。
 */
export function watchMediaQuery(query: string): { matches: Ref<boolean>; stop: Unsubscribe } {
  const matches = ref(false)
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
    return { matches, stop: () => {} }
  }

  const mql = window.matchMedia(query)
  matches.value = mql.matches

  const onChange = (e: MediaQueryListEvent): void => {
    matches.value = e.matches
  }
  // 不写 `addListener` 兼容分支：目标浏览器由 Vite 的 build target 决定，
  // 本仓没有需要旧 Safari（< 14）的部署场景——为不存在的浏览器写分支
  // 等于凭空维护它。
  mql.addEventListener('change', onChange)
  return {
    matches,
    stop: () => {
      mql.removeEventListener('change', onChange)
    },
  }
}
