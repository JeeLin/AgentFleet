/* ===========================================================================
   api.ts —— `/api/*` 的读取层（设计规范 §6 的数据契约）
   ---------------------------------------------------------------------------
   这一层只做两件事：① 把 JSON 的**形状**写成类型；② 给出四个读函数与一个
   建会话函数。页面不自己拼 URL、不自己解响应、不自己判错误码——错误一律
   `http.ts` 的 `ApiError`，页面只读 `code` 做分支。

   ── 为什么 `state` 需要一层映射 ──────────────────────────────────
   库里只有 `running|ended` 两值（`hub-storage/src/session.rs` 的
   `SessionState`），而规范 §6.1 的页面态是 `active|disconnected|ended`。
   `disconnected` 是「设备断了但会话还活着」的**推导态**，M1 没有推导它的
   数据源（会话不绑 agent 运行时，也没有 per-session 连接），所以现在只可能
   出现 `active` / `ended`。映射集中在此处，页面不各自写一遍。
   =========================================================================== */

import { getJson, postJson, request, type RequestOptions } from './http'

/** 设备态。与 `hub-server/src/routes/api.rs` 的 `DeviceStatus` 逐字一致。 */
export type ApiDeviceStatus = 'online' | 'reconnecting' | 'offline'

/** `GET /api/devices` 的一项（§6.1）。 */
export interface DeviceSummary {
  /** node_id，永不变（升级只换 worker，约束 12）。 */
  id: string
  name: string
  /** `os/arch`，未上报时为空串。 */
  platform: string
  agent_version: string
  status: ApiDeviceStatus
  /** `unix:<秒>` 原文；从没上报过是 `null`。**不是**人读的相对时间。 */
  last_seen: string | null
  /** 0–100；`null` = 未上报（不是 0）。 */
  cpu: number | null
  mem: number | null
  gpu: boolean
  /** 已装且就绪，不是「支持」。 */
  agents: string[]
  /** `status` 是否为 Hub 内存推导值（§6.1 的「推」徽标）。 */
  derived: boolean
  /** 前 3 + `***` + 后 3，Hub 已打码。 */
  key_fingerprint: string
}

/** 会话的工作区引用。路径创建后只读（`path_locked` 恒 true）。 */
export interface WorkspaceRef {
  node_id: string
  path: string
  path_locked: boolean
}

/** `GET /api/sessions` 的一项；也是 `POST /api/sessions` 的响应。 */
export interface SessionSummary {
  id: string
  /** M1 恒 `null`（ACP 归 M3）。 */
  agent_id: string | null
  state: 'running' | 'ended'
  created_at: string
  ended_at: string | null
  workspace: WorkspaceRef
}

/** `GET /api/agents` 的一项。数据源是设备心跳上报的 `capabilities.agents`。 */
export interface AgentSummary {
  name: string
}

/** `POST /api/sessions` 的请求体。 */
export interface CreateSessionInput {
  node_id: string
  path: string
  agent_id?: string | null
}

/** 读设备列表。空库返回 `[]`——那是「没有设备」，不是「Hub 坏了」。 */
export function listDevices(options?: RequestOptions): Promise<DeviceSummary[]> {
  return getJson<DeviceSummary[]>('/api/devices', options)
}

/** 读会话列表（新的在前）。 */
export function listSessions(options?: RequestOptions): Promise<SessionSummary[]> {
  return getJson<SessionSummary[]>('/api/sessions', options)
}

/** 读 Agent 目录。M1 恒空（没有 agent 运行时可探测）。 */
export function listAgents(options?: RequestOptions): Promise<AgentSummary[]> {
  return getJson<AgentSummary[]>('/api/agents', options)
}

/** 建会话。失败时抛 `ApiError`：`device_offline` / `invalid_workspace_path` / `not_found`。 */
export function createSession(input: CreateSessionInput): Promise<SessionSummary> {
  return postJson<SessionSummary>('/api/sessions', input)
}

/**
 * 登出：让 Hub 把两条 cookie 清成立即过期。
 *
 * **这不是「服务端拉黑票据」**——session 是自证的签名串，Hub 没有会话表可删；
 * 清 cookie 只作用于会照规矩更新 cookie jar 的浏览器。
 */
export function logout(options?: RequestOptions): Promise<Response> {
  return request('/logout', { ...options, method: 'POST' })
}

/** API 的会话态 → 页面态。`running` 在页面上是「运行中」（`active`）。 */
export function pageSessionState(state: SessionSummary['state']): 'active' | 'ended' {
  return state === 'running' ? 'active' : 'ended'
}

/** 设备是否「现在能建会话」。只有 `offline` 不行——`reconnecting` 仍算可用（§7.3）。 */
export function canCreateSession(device: DeviceSummary): boolean {
  return device.status !== 'offline'
}

/**
 * 建会话失败的可读理由。
 *
 * 文案只描述**事实与下一步**，不写「操作失败，请重试」这类把判断还给用户的句子。
 * 未知 code 原样带出：前端落后于 Hub 时，露出那个 code 比编一句好话更有用。
 */
export function createSessionErrorLabel(code: string): string {
  switch (code) {
    case 'device_offline':
      return '这台设备现在离线，Hub 连不上它。等它重新上线后再建会话。'
    case 'invalid_workspace_path':
      return '路径要写成绝对路径：以 / 或 ~ 开头。'
    case 'not_found':
      return '这台设备不存在了。刷新设备列表后再试。'
    case 'session_expired':
      return '登录已失效，请重新登录。'
    case 'invalid_request_body':
      return '提交的内容不完整：设备、路径都要填。'
    default:
      return `建会话没有成功（${code}）。`
  }
}
