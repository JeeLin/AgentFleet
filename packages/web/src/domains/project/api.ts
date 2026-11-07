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

import { del, getJson, patchJson, postJson, putJson, type RequestOptions } from '@/shared/http'

/**
 * `GET /api/projects` 的一项。字段与 `routes/projects.rs` 的 `ProjectView`
 * 对齐；`upstream_url` 标成可选是为了兼容 v5 及更早的响应（那时没有这个键），
 * 后端 v6 起总会带上它（`null` 或字符串）。
 */
export interface Project {
  id: string
  name: string
  /** `central` / `mirror` / `isolated`。 */
  shape: string
  /** 没选主题色时为 `null`（不要当成 `'gray'`）。 */
  theme_color: string | null
  /** 镜像式项目的上游 git 远端；其余形态 / 老项目为 `null`。 */
  upstream_url?: string | null
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

/**
 * 建项目。`shape` 缺省由后端定成 `isolated`（能反悔的那一侧）。
 *
 * `upstream_url` 只在 `shape === 'mirror'` 时有意义：那时后端要求非空且像
 * git 远端（`http(s)://` / `ssh://` / `git@host:path`），缺了回 400。
 * 前端 `NewProjectPage.vue` 先做同一套校验，挡住「没填就发请求」。
 */
export function createProject(
  body: { name: string; shape?: string; theme_color?: string; upstream_url?: string },
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

/* ===========================================================================
   绑定（v0.7.0 子任务 4 的后端 + 子任务 8 之后的 BindPage）
   ---------------------------------------------------------------------------
   读写的是同一个 `(project, device)` 组合，**后端只有一条 upsert**。前端因此
   不需要区分「新建」与「修改」：PUT 一次，201 是新建、200 是更新（状态码由
   后端区分，页面对两者做同一件事——写成功就重读）。
   =========================================================================== */

/**
 * 一条项目绑定的读法。与 `routes/bindings.rs` 的 `BindingView` 逐字一致。
 *
 * 与 `DeviceBinding`（设备视角）是**同一行的两种视图**：设备侧多带了
 * `project_name`（省一次项目列表请求），项目侧多带了 `device_id`。
 */
export interface ProjectBinding {
  id: string
  project_id: string
  device_id: string
  /** 设备上的路径原文。Hub 不展开 `~`，它没有那台设备的文件系统。 */
  fixed_dir: string
  /** `null` = 不限额。**不是 0**：0 是一个会把一切都拦掉的真实配额。 */
  disk_quota_bytes: number | null
  cleanup_policy: string
  /**
   * 这个项目在这台设备上占了多少字节。
   *
   * **`null` 不是 0**：真实用量只有设备自己量得出来。`null` 是「还没上报」，
   * `0` 是「一点不占」。
   */
  used_bytes: number | null
}

/** `GET /api/projects/{id}/bindings`：一个项目的全部绑定。 */
export function listProjectBindings(
  projectId: string,
  options?: RequestOptions,
): Promise<ProjectBinding[]> {
  return getJson<ProjectBinding[]>(
    `/api/projects/${encodeURIComponent(projectId)}/bindings`,
    options,
  )
}

/**
 * 写一行绑定（**upsert**）。
 *
 * `cleanup_policy` 不传时由存储层给默认值（`balanced`），前端**不自己编一个**：
 * 编的话，「用户没选」和「用户选了 balanced」在库里就成了同一个值。
 *
 * 有执行记录时改 `fixed_dir` 会被拒（409 + `detail.executions`）。那不是
 * 校验错误，是「这个目录上已经有历史了」——页面要把它连同条数一起显示，
 * 只说「不能改」的话用户分不清是「等一下」还是「永远不行」。
 */
export function putProjectBinding(
  projectId: string,
  deviceId: string,
  body: { fixed_dir: string; disk_quota_bytes?: number | null; cleanup_policy?: string },
  options?: RequestOptions,
): Promise<ProjectBinding> {
  return putJson<ProjectBinding>(
    `/api/projects/${encodeURIComponent(projectId)}/bindings/${encodeURIComponent(deviceId)}`,
    body,
    options,
  )
}

/** 解除绑定。历史执行记录保留；这台设备上该项目的在跑任务会先被取消。 */
export function deleteProjectBinding(
  projectId: string,
  deviceId: string,
  options?: RequestOptions,
): Promise<void> {
  return del(
    `/api/projects/${encodeURIComponent(projectId)}/bindings/${encodeURIComponent(deviceId)}`,
    options,
  )
}
