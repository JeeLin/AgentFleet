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
