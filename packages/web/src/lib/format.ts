/* ===========================================================================
   format.ts —— 把 API 的原始值翻成人读的短串
   ---------------------------------------------------------------------------
   时间戳是 `unix:<秒>`（`hub-core/src/registry.rs` 的 `now_stamp`），**不是**
   ISO。相对时间必须前端按当前时刻算：服务端算好的「12s」在页面停留一分钟
   之后就是错的，而它没有任何机制能自己更新。

   这些都是**纯函数**，所以有单测：`last_seen` 的格式是本项目里唯一跨
   服务端/页面共享的时间约定，猜错一个前缀就会把每台设备都显示成「—」。
   =========================================================================== */

import type { DeviceSummary, SessionSummary } from './api'

/** 解析 `unix:<秒>`。认不出的格式返回 `null`。 */
export function parseStamp(stamp: string | null | undefined): number | null {
  if (!stamp) return null
  const m = /^unix:(\d+)$/.exec(stamp)
  if (!m) return null
  const secs = Number(m[1])
  return Number.isFinite(secs) ? secs : null
}

/**
 * 相对时间。
 *
 * `null` 一律是「—」：**「未上报」与「0 秒前」不是一回事**，编一个时间比
 * 不显示坏得多（`api.rs` 对 `cpu` / `mem` 是同一条口径）。
 */
export function relativeTime(stamp: string | null | undefined, now = Date.now()): string {
  const secs = parseStamp(stamp)
  if (secs === null) return '—'
  const diff = Math.max(0, Math.floor(now / 1000) - secs)
  if (diff <= 5) return '刚刚'
  if (diff < 60) return `${diff} 秒前`
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`
  return `${Math.floor(diff / 86400)} 天前`
}

/**
 * 会话在 tab 条 / 列表里的标题。
 *
 * 后端 `SessionSummary` **没有 title 字段**（`session` 表里也没有）：
 * 会话的名字就是它的工作区路径的最后一段。编一个「会话 3」会让人分不清
 * 这是哪个项目——路径才是用户认得出的那一半。
 */
export function workspaceName(path: string): string {
  const trimmed = path.replace(/\/+$/, '')
  const parts = trimmed.split('/')
  const last = parts[parts.length - 1] ?? ''
  return last || trimmed || path || '（无路径）'
}

/** 会话的短标题。 */
export function sessionTitle(session: SessionSummary): string {
  return workspaceName(session.workspace.path)
}

/** 百分比：未上报是「—」，不是 0%。 */
export function percent(value: number | null): string {
  return value === null ? '—' : `${value}%`
}

/** 取不到的数字画成 `—`，与 `percent(null)` 同一口径（不拿 0 顶上）。 */
export function orDash(value: number | null): string {
  return value === null ? '—' : String(value)
}

/**
 * 设备名。
 *
 * 取不到时**回显 `node_id`**：设备列表还没回来时显示「设备」会让人以为
 * 列表里没有这台设备，而 `n_xxx` 至少还能拿去搜。
 *
 * 列表页与工作台各写一份时，改兜底策略要改两处——漏一处则同一台设备在
 * 两处显示不同的名字。
 */
export function deviceLabel(devices: ReadonlyMap<string, DeviceSummary>, nodeId: string): string {
  return devices.get(nodeId)?.name ?? nodeId
}
