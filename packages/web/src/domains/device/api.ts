/* ===========================================================================
   domains/device/api.ts —— `/api/devices` 的读取层（设计规范 §6 的数据契约）
   ---------------------------------------------------------------------------
   这一层只做两件事：① 把 JSON 的**形状**写成类型；② 给出设备列表的两个
   函数（读 / 删）。页面不自己拼 URL、不自己解响应、不自己判错误码——错误
   一律是 `shared/http.ts` 的 `ApiError`，页面只读 `code` 做分支。

   传输层（`fetch` / CSRF / 401 / 403 的处置）在 `shared/http.ts`，本文件
   只声明形状与端点。
   =========================================================================== */

import { del, getJson, type RequestOptions } from '@/shared/http'

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

/** 读设备列表。空库返回 `[]`——那是「没有设备」，不是「Hub 坏了」。 */
export function listDevices(options?: RequestOptions): Promise<DeviceSummary[]> {
  return getJson<DeviceSummary[]>('/api/devices', options)
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

/** 设备是否「现在能建会话」。只有 `offline` 不行——`reconnecting` 仍算可用（§7.3）。 */
export function canCreateSession(device: DeviceSummary): boolean {
  return device.status !== 'offline'
}
