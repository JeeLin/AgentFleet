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

import { del, getJson, postJson, request, type RequestOptions } from './http'

/** 设备态。与 `hub-server/src/routes/api.rs` 的 `DeviceStatus` 逐字一致。 */
export type ApiDeviceStatus = 'online' | 'reconnecting' | 'offline'

/** `GET /api/devices` 的一项（§6.1）。 */
export interface DeviceSummary {
  /** node_id，永不变（升级只换 worker，约束 12）。 */
  id: string
  name: string
  /** `os/arch`，未上报时为空串。 */
  platform: string
  node_version: string
  status: ApiDeviceStatus
  /** `unix:<秒>` 原文；从没上报过是 `null`。**不是**人读的相对时间。 */
  last_seen: string | null
  /** 0–100；`null` = 未上报（不是 0）。 */
  cpu: number | null
  mem: number | null
  gpu: boolean
  /**
   * **已装**的 agent 名（含装了但跑不起来的）。
   *
   * 「已装」不是「能对话」：装了但没有 ACP 入口的（`Probe::NotCapable`）也在
   * 这里面，而它存在的意义就是让用户看见「为什么我的 agent 不能对话」——
   * 展示时必须按 `usable_agents` 分两档，混在一起等于没接这个字段。
   */
  agents: string[]
  /**
   * **已装且现在能对话**的 agent 名。建会话的可选项。
   *
   * 与后端 `POST /api/sessions` 的 409 门禁**同源**（都读 `agent_runtime`，
   * 都只看 `available`），所以页面在这里见到的名字一定建得成。
   */
  usable_agents: string[]
  /** `status` 是否为 Hub 内存推导值（§6.1 的「推」徽标）。 */
  derived: boolean
  /** 前 3 + `***` + 后 3，Hub 已打码。 */
  key_fingerprint: string
}

/** 会话的工作区引用。路径创建后只读（`path_locked` 恒 true）。 */
export interface WorkspaceRef {
  /**
   * `null` = 这台设备已被删除（删设备不删会话）。
   *
   * 和「离线」是两种状态：离线是设备还在、等等会回来；已移除是它不会再回来了。
   * 拿 `null` 去开通道没有意义，页面必须显式处理这一态。
   */
  node_id: string | null
  /** 设备被删前的原 node_id；设备还在时缺省。仅供显示「已移除的设备」。 */
  removed_node_id?: string
  path: string
  path_locked: boolean
}

/** `GET /api/sessions` 的一项；也是 `POST /api/sessions` 的响应。 */
export interface SessionSummary {
  id: string
  /**
   * 建会话时选定的 agent（`agent_id`）。M2 起建会话时**必填**，所以新会话里它不会是
   * `null`；老会话（M2 之前建的）仍是 `null`，那类会话不能对话。
   */
  agent_id: string | null
  state: 'running' | 'ended'
  created_at: string
  ended_at: string | null
  workspace: WorkspaceRef
}

/** `GET /api/agents` 的一项。 */
export interface AgentSummary {
  name: string
  /** 现在能不能对话。**至少有一台设备上可用即为真**。 */
  available: boolean
  /** 说它不可用的原因；设备没给原因时为 `null`（不编一句）。 */
  error: string | null
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

/**
 * 移除一台设备。
 *
 * # 它**不删**这台设备上的会话与轨迹
 *
 * 会话是用户的数据，设备只是一条通路。删完之后那些会话仍在 `/api/sessions`
 * 里，`workspace.node_id` 变成 `null`、`workspace.removed_node_id` 记下原 id——
 * 所以调用方**不需要**在成功之后去清会话列表或本地缓存。
 *
 * # 404 是「本来就不存在」，不是「删失败」
 *
 * 抛出的 `ApiError.code` 是 `not_found`。重复点击、或另一处已经删过，都会走到
 * 这一条；调用方应当把它当成「目标状态已达成」，而不是显示一个错误。
 */
export function deleteDevice(id: string, options?: RequestOptions): Promise<void> {
  return del(`/api/devices/${encodeURIComponent(id)}`, options)
}

/**
 * `GET /api/sessions/{id}/trajectory` 的一项。是 `trajectory_event` 的一行，
 * 与 Hub 侧 `hub-server/src/routes/conversation.rs` 的 `EventOut` 逐字段同名。
 *
 * # 这是这个行形状的**唯一**声明处
 *
 * 组件要用就 `import type` 这一个，不要在组件里再写一份「同形」的接口：
 * 抄出去的副本缺字段时编译器不会说话（少字段的接口照样接得住多字段的
 * 对象），于是「后端给了、前端没有」这件事没有任何一处会红——轨迹抽屉
 * 因此至今画不出 `duration_ms`。`api.test.ts` 把这份声明的字段集与后端
 * `EventOut` 逐字段对齐，并数着全 src 里还有几处在自己声明这个形状。
 */
export interface TrajectoryEvent {
  id: number
  /** 设备侧编的序号。**重连后会从 1 重数**，所以只在一次进程生命周期内单调。 */
  seq: number
  kind: string
  /** **原始 JSON 字符串**：Hub 不解析成对象再序列化，页面自己解。 */
  payload: string
  /**
   * 耗时毫秒。**结构性无耗时时为 `null`**——ACP 事件全都不带时间戳，拿
   * 「收到时刻」相邻差分得到的是事件间隔而不是执行耗时，填 0 是谎报。
   */
  duration_ms: number | null
  /** `duration_ms` 的来源：`measured` / `none`。看这一列才知道上面那个数能不能信。 */
  duration_source: string
  created_at: string
}

/**
 * 一段序号空洞：`[from, to)` 里的 seq 都不在库里。
 *
 * `to` 是那个**存在**的 seq、充当排他上界，所以「缺几条」是 `to - from`。
 * 这个减法是 Hub 算好放进 `missing` 的，**前端不重算**——两处各算一份会分叉。
 */
export interface TrajectoryGap {
  from: number
  to: number
  missing: number
}

/** `GET /api/sessions/{id}/trajectory` 的响应。 */
export interface TrajectoryView {
  events: TrajectoryEvent[]
  gaps: TrajectoryGap[]
  /** 最后一轮没有 `turn_end`：还在跑，或结尾丢了。 */
  incomplete: boolean
}

/** `OpenedChannel`：开通道的响应。字段与 Hub 侧同名同义。 */
export interface OpenedChannel {
  channel_id: string
  /** 通道所在设备。页面据此把帧对上设备，不必再查一次会话。 */
  node_id: string
  session_id: string
}

/**
 * 读一页轨迹。**会话不存在时 404。**
 *
 * `after` 是**物理 id 不是 `seq`**：回放顺序是物理顺序，而 `seq` 在设备侧 ACP
 * 子进程重启后会从 1 重数，所以它只在一次进程生命周期内单调。按 `seq` 翻页
 * 会在重连后翻到已经读过的那一页。
 *
 * `hasMore` 由「这一页取满了没有」推出：`limit` 被服务端夹到 [1, 1000]，
 * 取满就说明后面可能还有。这是前端能拿到的唯一信号——Hub 不返回总数。
 */
export async function getTrajectory(
  sessionId: string,
  after: number = 0,
  limit: number = 200,
  options?: RequestOptions,
): Promise<TrajectoryView & { hasMore: boolean }> {
  const qs = `?after=${after}&limit=${limit}`
  const view = await getJson<TrajectoryView>(
    `/api/sessions/${sessionId}/trajectory${qs}`,
    options,
  )
  return { ...view, hasMore: view.events.length >= limit }
}

/**
 * 读 Agent 目录。
 *
 * **不再恒空**：M2 起设备会探测本机装了哪些 agent 并上报，所以这个列表可以
 * 真的有内容。每项带 `available` / `error`——「装了但没有 ACP 入口」与「装了
 * 且能对话」是两种事实，页面必须分得开。
 */
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
    case 'agent_unavailable':
      return '这台设备上这个 agent 现在跑不起来（装了但没有可用的 ACP 入口）。换一个 agent，或先在这台设备上装好它。'
    case 'agent_required':
      return '建会话要先选一个 agent：没有 agent 的会话打不开对话。'
    default:
      return `建会话没有成功（${code}）。`
  }
}
