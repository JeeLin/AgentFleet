/* ===========================================================================
   domains/agent/api.ts —— `/api/agents` 的读取层（设计规范 §6 的数据契约）
   ---------------------------------------------------------------------------
   这一层只做两件事：① 把 JSON 的**形状**写成类型；② 给出 Agent 目录的
   读函数。错误一律是 `shared/http.ts` 的 `ApiError`，页面只读 `code` 做分支。

   传输层在 `shared/http.ts`，本文件只声明形状与端点。
   =========================================================================== */

import { getJson, type RequestOptions } from '@/shared/http'

/** `GET /api/agents` 的一项。 */
export interface AgentSummary {
  name: string
  /** 现在能不能对话。**至少有一台设备上可用即为真**。 */
  available: boolean
  /** 说它不可用的原因；设备没给原因时为 `null`（不编一句）。 */
  error: string | null
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

/* ===========================================================================
   Agent 详情（v0.7.0 子任务 7）——**从 agent 侧看任务**
   ---------------------------------------------------------------------------
   设备页回答「这台机器怎么了」，这里回答「**这个 agent 靠不靠得住**」。
   后者不挂在某台设备上：同一个 agent 可能装在五台机器上。
   =========================================================================== */

import type { FailureBreakdown } from '@/domains/device/api'

/** 一台设备上的这个 agent。 */
export interface AgentOnDevice {
  node_id: string
  device_name: string
  available: boolean
  probe_error: string | null
  /** 这台设备上（反推出来的）执行条数。 */
  executions: number
}

/** 一条「跑过的任务」。 */
export interface AgentTaskRun {
  execution_id: string
  task_id: string
  /** 任务行已不存在时是 `null`（v5 刻意不给 `execution.task_id` 加外键）。 */
  task_title: string | null
  project_id: string
  project_name: string
  device_id: string
  state: 'pending' | 'running' | 'succeeded' | 'failed' | 'aborted'
  created_at: string
}

/** `GET /api/agents/:name` 的响应。 */
export interface AgentDetail {
  name: string
  available: boolean
  error: string | null
  devices: AgentOnDevice[]
  task_history: AgentTaskRun[]
  breakdown: FailureBreakdown
  /**
   * 成功率（0–100）。
   *
   * **`null` 不是 0**：一次执行都没有时要显示「—」而不是「0%」。
   * 0/0 是「还没跑过」，0/10 是「一次都没成」——两句话完全不同。
   */
  success_rate: number | null
  /**
   * `task_history` 的口径说明。**页面必须显示这句话**。
   *
   * 执行记录里没有「这条是哪个 agent 跑的」这一列，所以历史是按「执行所在的
   * 那台设备上装了这个 agent」反推的。用户看到成功率时会当真，而它的分母
   * 可能包含别的 agent 干的活——那句话不能让用户自己去猜。
   */
  task_history_caveat: string
}

/** 读 agent 详情。没在任何设备上装过 → `404 not_found`。 */
export function getAgentDetail(
  name: string,
  options?: RequestOptions,
): Promise<AgentDetail> {
  return getJson<AgentDetail>(`/api/agents/${encodeURIComponent(name)}`, options)
}
