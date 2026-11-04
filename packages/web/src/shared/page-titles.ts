/* ===========================================================================
   page-titles.ts —— 页面中文名的唯一真相源
   ---------------------------------------------------------------------------
   「会话」「设备」「Agent 目录」这些页面名此前在**四处**各抄了一份：侧栏
   NAV、浏览器标题（`docTitle`）、`?` 帮助面板的 `nav:*` 条目、以及各页
   自己的 `<h1>`。改一处名字要改四处，漏一处的症状是「侧栏叫『Agent』、
   浏览器标题还叫『Agent 目录』」——同一屏里两种叫法。

   本模块不 import `router.ts`：`router.ts` → `shared/focus.ts` →
   `shared/keynav.ts` → 本模块，反向再 import `router.ts` 就成了环。所以这里的
   键是**普通字符串**，与 `RouteName` 的一一对应由 `App.vue` 保证
   （`route.value.name` 的类型就是 `RouteName`）。
   =========================================================================== */

/** 页面名 → 中文名。键与 `router.ts` 的 `RouteName` 一一对应。 */
export const PAGE_TITLE: Readonly<Record<string, string>> = {
  // ── 一级导航 ──────────────────────────────────────────────────────
  overview: '总览',
  devices: '设备',
  agents: 'Agent',
  settings: '设置',
  // ── 项目内 ────────────────────────────────────────────────────────
  board: '看板',
  'projects-new': '新建项目',
  'projects-archived': '已归档的项目',
  'project-task': '任务',
  execution: '执行监控',
  replay: '执行复盘',
  timeline: '时间线',
  session: '会话',
  'project-bind': '绑定设备',
  'project-settings': '项目设置',
  'project-schedules': '定时任务',
  // ── 收件箱与管家 ──────────────────────────────────────────────────
  inbox: '收件箱',
  'inbox-activity': '动态',
  butler: '管家',
  // ── 设置子页 ──────────────────────────────────────────────────────
  'settings-skills': '技能与 MCP',
  'settings-integrations': '外部 issue 导入',
  'settings-notifications': '通知出口',
  // ── 认证流程 ──────────────────────────────────────────────────────
  login: '登录',
  setup: '首次 setup',
  'not-found': '找不到页面',
}

/**
 * 一个页面的中文名。表里没有的名字**原样返回**——
 * 路由名是闭集（`ROUTES` 的 23 条 + `not-found`），落到表外说明调用方
 * 传错了，而把错的路由名原样显示出来比编一个「找不到页面」更接近真相。
 */
export function pageTitle(name: string): string {
  return PAGE_TITLE[name] ?? name
}
