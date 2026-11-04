/* ===========================================================================
   domains/project/api.ts —— `/api/projects` 的读取层（设计规范 §6 的数据契约）
   ---------------------------------------------------------------------------
   与 `domains/device/api.ts` 同一套口径：这一层只做两件事——把 JSON 的**形状**
   写成类型，给出端点的函数。页面不自己拼 URL、不自己解响应、不自己判错误码；
   错误一律是 `shared/http.ts` 的 `ApiError`，页面只读 `code` 做分支。

   传输层（`fetch` / CSRF / 401 / 403 的处置）在 `shared/http.ts`。

   ── 形态与颜色为什么不写成前端枚举 ────────────────────────────────
   后端 `shape` 只有三个值、`theme_color` 只有五个值（都是封闭集合），但前端
   仍然按 `string` 收：多写一份枚举等于多一处需要跟后端同步的地方，而**漂移
   时前端会静默渲染出一个说不清的标签**。真正的封闭性由后端的 `FromStr` 保证
   （非法值 400），前端只负责把拿到的字符串显示出来。
   =========================================================================== */

import { getJson, patchJson, postJson, type RequestOptions } from '@/shared/http'

/** `GET /api/projects` 的一项。与 `routes/projects.rs` 的 `ProjectView` 逐字一致。 */
export interface Project {
  id: string
  name: string
  /** `central` / `mirror` / `isolated`。 */
  shape: string
  /** 没选主题色时为 `null`（不要当成 `'gray'`）。 */
  theme_color: string | null
  /** 非空即已归档，值是 `unix:<秒>`；页面要显示「什么时候归档的」。 */
  archived_at: string | null
  created_at: string
}

/**
 * 读项目列表。
 *
 * `includeArchived` 为真时连已归档的一起读（项目设置页与归档页要用）。
 * 默认不列归档：列表第一屏就是「我现在在做的项目」。
 */
export function listProjects(includeArchived = false, options?: RequestOptions): Promise<Project[]> {
  const q = includeArchived ? '?include_archived=1' : ''
  return getJson<Project[]>(`/api/projects${q}`, options)
}

export function getProject(id: string, options?: RequestOptions): Promise<Project> {
  return getJson<Project>(`/api/projects/${encodeURIComponent(id)}`, options)
}

/** 建项目。`shape` 缺省由后端定成 `isolated`（能反悔的那一侧）。 */
export function createProject(
  body: { name: string; shape?: string; theme_color?: string },
  options?: RequestOptions,
): Promise<Project> {
  return postJson<Project>('/api/projects', body, options)
}

/**
 * 改名称与主题色。
 *
 * `theme_color` 的三层语义与后端一致：不传 = 别动；传 `null` = 清除；传值 = 设它。
 * 所以参数类型是 `string | null | undefined`，**不能收窄成 `string?`**——
 * 「清除颜色」是用户真会做的操作。
 */
export function patchProject(
  id: string,
  body: { name?: string; theme_color?: string | null },
  options?: RequestOptions,
): Promise<Project> {
  return patchJson<Project>(`/api/projects/${encodeURIComponent(id)}`, body, options)
}

/**
 * 改形态（**只允许 `isolated → central`**）。
 *
 * 被拒时后端回 400 + `shape_transition_refused`，并在 `detail` 里带 `{from, to}`。
 * 单独一个端点而不是混进 `patchProject`：形态变更不是改字段，它要求真实建仓
 * （见 `routes/projects.rs` 的同名注释）。
 */
export function changeProjectShape(
  id: string,
  shape: string,
  options?: RequestOptions,
): Promise<Project> {
  return postJson<Project>(`/api/projects/${encodeURIComponent(id)}/shape`, { shape }, options)
}

export function archiveProject(id: string, options?: RequestOptions): Promise<Project> {
  return postJson<Project>(`/api/projects/${encodeURIComponent(id)}/archive`, {}, options)
}

export function unarchiveProject(id: string, options?: RequestOptions): Promise<Project> {
  return postJson<Project>(`/api/projects/${encodeURIComponent(id)}/unarchive`, {}, options)
}
