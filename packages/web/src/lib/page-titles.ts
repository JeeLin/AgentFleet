/* ===========================================================================
   page-titles.ts —— 页面中文名的唯一真相源
   ---------------------------------------------------------------------------
   「会话」「设备」「Agent 目录」这些页面名此前在**四处**各抄了一份：侧栏
   NAV、浏览器标题（`docTitle`）、`?` 帮助面板的 `nav:*` 条目、以及各页
   自己的 `<h1>`。改一处名字要改四处，漏一处的症状是「侧栏叫『Agent』、
   浏览器标题还叫『Agent 目录』」——同一屏里两种叫法。

   本模块不 import `router.ts`：`router.ts` → `lib/focus.ts` →
   `lib/keynav.ts` → 本模块，反向再 import `router.ts` 就成了环。所以这里的
   键是**普通字符串**，与 `RouteName` 的一一对应由 `App.vue` 保证
   （`route.value.name` 的类型就是 `RouteName`）。
   =========================================================================== */

/** 页面名 → 中文名。键与 `router.ts` 的 `RouteName` 一一对应。 */
export const PAGE_TITLE: Readonly<Record<string, string>> = {
  sessions: '会话',
  'new-session': '新建会话',
  session: '会话工作台',
  devices: '设备',
  settings: '设置',
  login: '登录',
  setup: '首次 setup',
  'not-found': '找不到页面',
}

/**
 * 一个页面的中文名。表里没有的名字**原样返回**——
 * 路由名是闭集（9 个），落到表外说明调用方传错了，而把错的路由名原样
 * 显示出来比编一个「找不到页面」更接近真相。
 */
export function pageTitle(name: string): string {
  return PAGE_TITLE[name] ?? name
}
