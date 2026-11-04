/* ===========================================================================
   domains/session/api.ts —— `/api/sessions` 与轨迹的读取层（设计规范 §6 的数据契约）
   ---------------------------------------------------------------------------
   这一层只做两件事：① 把 JSON 的**形状**写成类型；② 给出会话列表 / 建会话 /
   轨迹 / 对话控制的函数。页面不自己拼 URL、不自己解响应、不自己判错误码——
   错误一律是 `shared/http.ts` 的 `ApiError`，页面只读 `code` 做分支。

   传输层（`fetch` / CSRF / 401 / 403 的处置）在 `shared/http.ts`，本文件只
   声明形状与端点。

   ── 为什么 `state` 需要一层映射 ──────────────────────────────────
   库里只有 `running|ended` 两值（`hub-storage/src/session.rs` 的
   `SessionState`），而规范 §6.1 的页面态是 `active|disconnected|ended`。
   `disconnected` 是「设备断了但会话还活着」的**推导态**，M1 没有推导它的
   数据源（会话不绑 agent 运行时，也没有 per-session 连接），所以现在只可能
   出现 `active` / `ended`。映射集中在此处，页面不各自写一遍。
   =========================================================================== */

import { getJson, postJson, request, type RequestOptions } from '@/shared/http'

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
  /**
   * 会话建立那一刻工作区的 HEAD，完整 40 位。`null` = 没有起点，原因看
   * `base_rev_status`。
   *
   * **键恒在**（Hub 侧明确不加 `skip_serializing_if`），所以 `null` 与「键不存在」
   * 不是同一件事：后者只可能来自更老的 Hub。
   */
  base_rev: string | null
  /** 起点为什么有／没有。五态的取值语义见 `docs/architecture/session-and-trajectory.md` §6.1。 */
  base_rev_status: 'ok' | 'not_a_repo' | 'no_head' | 'unavailable' | 'unknown'
  workspace: WorkspaceRef
}

/** `POST /api/sessions` 的请求体。 */
export interface CreateSessionInput {
  node_id: string
  path: string
  agent_id?: string | null
}

/** 读会话列表（新的在前）。 */
export function listSessions(options?: RequestOptions): Promise<SessionSummary[]> {
  return getJson<SessionSummary[]>('/api/sessions', options)
}

/** 建会话。失败时抛 `ApiError`：`device_offline` / `invalid_workspace_path` / `not_found`。 */
export function createSession(input: CreateSessionInput): Promise<SessionSummary> {
  return postJson<SessionSummary>('/api/sessions', input)
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
  /** 最后一轮**既没有开始标记也没有结束标记**：结尾丢了（或是旧数据）。 */
  incomplete: boolean
  /**
   * 最后一轮**有 `turn_start`、没有 `turn_end`**：一轮开了还没关。
   *
   * **这不等于「agent 还活着」**：开始标记落库之后设备掉了，同样是这个状态。
   * 所以它只说明「这一轮没有走完」，不足以让页面说「正在回复」。
   */
  running: boolean
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
 * 停止**正在跑的那一轮**，不关闭会话。
 *
 * 204 只说明「这一帧已经发到设备」，**不说明设备上真有一轮被掐掉了**：
 * Hub 判不出「轮次在不在飞」（设备侧的 `turn_in_flight` 没有 Hub 调用方），
 * 它只校验这条会话有没有通道绑定（没有 → 404）。所以设备侧收到 cancel 而
 * 轮次本就闲着时，它会只留痕、不回话，页面看起来毫无反应——那是协议边界，
 * 不是请求失败。404 则是「对话根本没开过 / 会话不在你名下」。
 *
 * **不能用 `postJson`**：它无条件 `r.json()`，而这条端点回 **204 空体**。
 * 按 `Response.json()` 的规范，空体是 reject（`SyntaxError`）——那样「成功
 * 停止」会被当成错误报给用户。注意 jsdom 对空体宽容（返回 `null`），所以
 * 这条**在单测里看不出区别**，是照着规范与同仓 `del()` 的先例（它明写
 * 「服务端可能回 204，所以不解析响应体」）写的，不是被某条用例证出来的。
 * 走 `request` 只为发出请求（与 `logout` 同一形态）。
 */
export function cancelConversation(sessionId: string): Promise<void> {
  return request(`/api/sessions/${sessionId}/conversation/cancel`, { method: 'POST' }).then(
    () => undefined,
  )
}

/** API 的会话态 → 页面态。`running` 在页面上是「运行中」（`active`）。 */
export function pageSessionState(state: SessionSummary['state']): 'active' | 'ended' {
  return state === 'running' ? 'active' : 'ended'
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
