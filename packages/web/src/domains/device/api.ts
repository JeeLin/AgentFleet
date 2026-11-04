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

/* ===========================================================================
   设备详情（v0.7.0 子任务 7）——**从设备侧看项目**
   ---------------------------------------------------------------------------
   `PRODUCT.md` §3 的主轴是「做事从项目进，管理从设备进」。项目主轴改版之后
   设备页恰恰要**变多**：运维时的第一反应永远是「这台机器怎么了」，而那个
   问题项目页回答不了。

   三块新数据（绑定关系 / 磁盘 / 失败归因）全部是既有表的读，没有新表。
   =========================================================================== */

/** 一条绑定在设备视角下的样子（带项目名，省掉一次项目列表请求）。 */
export interface DeviceBinding {
  id: string
  project_id: string
  project_name: string
  /** 设备上的路径原文（不展开 `~`：Hub 没有那台设备的文件系统）。 */
  fixed_dir: string
  /** `null` = 不限额。**不是 0**：0 是一个会把一切都拦掉的真实配额。 */
  disk_quota_bytes: number | null
  cleanup_policy: 'conservative' | 'balanced' | 'aggressive'
  /**
   * 这个项目在这台设备上占了多少字节。
   *
   * **`null` 不是 0**：真实用量只有设备自己量得出来（它才有那个盘）。
   * `null` 说的是「设备还没上报」，`0` 说的是「一点不占」——两句话。
   */
  used_bytes: number | null
}

/**
 * 磁盘那一块的汇总。**没有绑定时整个 `disk` 是 `null`**。
 *
 * 回 `{used: 0}` 的话页面会渲染成「已用 0 B」，而用户真正需要知道的是
 * 「这台机器还没被任何项目用起来」。
 */
export interface DeviceDisk {
  quota_bytes: number | null
  used_bytes: number | null
  /** 0–100。`null` = 算不出来（没配额，或设备还没报用量）。**不是 0%**。 */
  percent: number | null
  /** 还能不能派活。配额为 0 时是 `false`（一个字节都放不下）。 */
  accepts_new_work: boolean
}

/**
 * 执行结果分布。
 *
 * **`aborted` 不是失败**：「我们主动停的」与「它自己挂的」在归因里是两个
 * 结论，混成一个会让一台从没出过问题的机器看起来有失败。
 */
export interface FailureBreakdown {
  succeeded: number
  failed: number
  aborted: number
  /** 还在跑或还没开工。**不进成功率的分母**。 */
  in_flight: number
}

/** 删除前置条件的结论，不只是布尔值。 */
export interface Deletable {
  allowed: boolean
  /** 拒绝的理由（人话）。`allowed === true` 时是 `null`。 */
  reason: string | null
  executions: number
}

/** `GET /api/devices/:id/detail` 的响应。 */
export interface DeviceDetail {
  /** 与列表里那一项**同一个形状**：设备页头部本来就用那套字段。 */
  summary: DeviceSummary
  /** 空数组 = 真的没绑（页面说「还没有绑定项目」）。 */
  bindings: DeviceBinding[]
  disk: DeviceDisk | null
  failure_breakdown: FailureBreakdown
  deletable: Deletable
}

/**
 * 读设备详情。
 *
 * 路径带 `/detail` 而不是复用 `/api/devices/{id}`：后者已经存在吗？没有——
 * 但 `DELETE /api/devices/{id}` 在，同一路径加一个 GET 会让「读详情」与
 * 「删设备」共用一条路径，而这两件事的失败后果差了一个数量级。分开更安全。
 */
export function getDeviceDetail(
  id: string,
  options?: RequestOptions,
): Promise<DeviceDetail> {
  return getJson<DeviceDetail>(`/api/devices/${encodeURIComponent(id)}/detail`, options)
}

/** 清理策略的中文说明。**三档的差别要写出来**，只给名字等于没接这个字段。 */
export function describeCleanupPolicy(policy: DeviceBinding['cleanup_policy']): string {
  switch (policy) {
    case 'conservative':
      return '保守：依赖缓存留 30 天，产物留最近 3 个做诊断'
    case 'balanced':
      return '平衡：依赖缓存留 7 天，产物跑完即清'
    case 'aggressive':
      return '激进：依赖缓存尽快清，产物跑完即清'
  }
}
