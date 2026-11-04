/* ===========================================================================
   format.ts —— 把 API 的原始值翻成人读的短串
   ---------------------------------------------------------------------------
   时间戳是 `unix:<秒>`（`hub-core/src/registry.rs` 的 `now_stamp`），**不是**
   ISO。相对时间必须前端按当前时刻算：服务端算好的「12s」在页面停留一分钟
   之后就是错的，而它没有任何机制能自己更新。

   这些都是**纯函数**，所以有单测：`last_seen` 的格式是本项目里唯一跨
   服务端/页面共享的时间约定，猜错一个前缀就会把每台设备都显示成「—」。
   =========================================================================== */

import type { DeviceSummary } from '@/domains/device/api'
import type { SessionSummary } from '@/domains/session/api'

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
 * 相对时间（长跨度）：档位比 `relativeTime` 更粗——**第一档就是 60 秒**，
 * 没有「N 秒前」，并补上「个月 / 年」。
 *
 * **不要把它当成「`relativeTime` 加参数」**：两处的档位宽度不同是刻意的。
 * 设备的心跳按秒上报，「12 秒前」是有信息量的；而 commit 的时间戳按分钟看
 * 才有意义，一条 30 秒前的提交说「刚刚」比说「30 秒前」更贴近用户感受。
 * 所以同一个时刻在两个面板上可能显示成不同说法（「29 秒前」vs「刚刚」），
 * 那不是 bug。
 *
 * **解析失败原样回显**，不写「刚刚」（与 `relativeTime` 回「—」不同）：这里的
 * 输入是 git 的 `%aI`，解析不动说明设备给了个怪值——把怪值原样显示出来，
 * 用户与日志对得上；编成「刚刚」会让人以为提交真的刚发生。
 *
 * 与 `relativeTime` 分成两个函数而不是加参数：调用方的**失败口径**不同
 * （「—」是「未上报」，「原样」是「读不懂」），塞进同一个签名就要在里面判。
 */
export function relativeTimeLong(stamp: string, now = Date.now()): string {
  // 主路径是 ISO-8601（git 的 `%aI`）。`unix:` 回落**不是**给生产调用方的：
  // 本函数唯一的调用方是 `GitLog.vue` 的 `relative(c.when)`，入参一定是 `%aI`。
  // 留这条回落是为了单测能直接喂 `unix:` 样本、以及万一将来换了时间来源。
  // 换句话说：这里「能读 `unix:`」是兜底能力，不代表两种格式会混着来。
  const iso = Date.parse(stamp)
  const epoch = Number.isNaN(iso) ? parseStamp(stamp) : Math.floor(iso / 1000)
  if (epoch === null) return stamp
  // **不要 `Math.max(0, …)`**：未来时间要能被下面那行 `diff < 0` 认出来并原样
  // 回显——先夹到 0 的话就变成了「刚刚」，把时钟不同步说成「刚提交」。
  const diff = Math.floor(now / 1000) - epoch
  // 未来时间（时钟不同步）也原样回显：算出「-3 分钟前」比不显示更坏。
  if (diff < 0) return stamp
  // 60 秒：见函数头「第一档就是 60 秒」，与 `relativeTime` 的 5 秒不同。
  if (diff < 60) return '刚刚'
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)} 天前`
  if (diff < 86400 * 365) return `${Math.floor(diff / (86400 * 30))} 个月前`
  return `${Math.floor(diff / (86400 * 365))} 年前`
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
 * `nodeId === null` = **这台设备已被删除**（删设备不删会话，`workspace.node_id`
 * 置 NULL）。这一态和「列表还没回来」必须分开显示：前者设备永远不会回来了，
 * 后者等一会儿就有名字。回落到空串或 `null` 会让页面显示成后者。
 *
 * 列表页与工作台各写一份时，改兜底策略要改两处——漏一处则同一台设备在
 * 两处显示不同的名字。所以这条判据只在这里写一次。
 */
export function deviceLabel(
  devices: ReadonlyMap<string, DeviceSummary>,
  nodeId: string | null,
  removedNodeId?: string,
): string {
  if (nodeId === null) {
    return removedNodeId ? `${removedNodeId}（已移除）` : '（设备已移除）'
  }
  return devices.get(nodeId)?.name ?? nodeId
}
