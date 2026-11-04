/* ===========================================================================
   state.ts —— 设备 / 会话三态的渲染（规范 §5.1 / §7.3）
   ---------------------------------------------------------------------------
   设备只有三个态：在线 / 重连中 / 离线。会话多两个：运行中 / 断线 / 已结束。
   它们共用**一个决定**：颜色 + 字形 + 填充 + 文字，四个通道一起给。

   为什么不只给颜色：色觉障碍、灰度打印、8px 缩略、reduce-motion 下
   都仍要能分辨——任何一个通道单独承担识别都不行。所以 `glyph` 与
   `fill` 不是装饰，删一个就是在砍一个识别通道。

   ── 为什么是数据而不是 HTML 字符串 ──────────────────────────────────
   原型 `AF.stateHTML` 拼的是 innerHTML 字符串，因为它零依赖、零构建。
   实现侧是 Vue：状态要进 `:data-state`、`:class` 与 `{{ }}`，不是进
   `v-html`。所以这里导出**数据**，组件自己渲染——顺手也消掉了
   「服务端返回的 state 字符串被拼进 innerHTML」这条注入路径。
   =========================================================================== */

/** 设备态。`GET /api/devices` 的 `status` 字段取这四个值。 */
export type DeviceStatus = 'online' | 'reconnecting' | 'offline' | 'unknown'

/** 会话态。`GET /api/sessions` 的 `status` 字段取这三个值。 */
export type SessionStatus = 'active' | 'disconnected' | 'ended'

/** 渲染层认识的全部态。设备态与会话态都在其中。 */
export type AnyStatus = DeviceStatus | SessionStatus

/** 字形的填充通道。CSS 用 `[data-glyph]` 选，不看字形字符本身。 */
export type GlyphFill = 'filled' | 'partial' | 'hollow'

/** 一个态的完整描述——渲染所需的全部信息，一处决定。 */
export interface StateSpec {
  /** 原样写进 `data-state`，CSS 按它上色。 */
  state: AnyStatus
  /** 字形字符。 */
  glyph: string
  /** 填充通道。 */
  fill: GlyphFill
  /** 文字。**必带**：颜色与字形是备份通道，文字才是主通道。 */
  label: string
}

/**
 * 态表。
 *
 * 注意 `components.css` §5 的颜色只覆盖 `online / reconnecting / offline /
 * active / disconnected / ended` 六个值——本表与它一一对应，改一处必须改两处，
 * 而 `state.test.ts` 断言了这个对应关系（表里多一个态而 CSS 没有颜色，
 * 在深色底上就是「继承主文字色」，用户看到的是在线设备显示成灰色）。
 */
export const STATES: Readonly<Record<AnyStatus, StateSpec>> = {
  online: { state: 'online', glyph: '●', fill: 'filled', label: '在线' },
  reconnecting: { state: 'reconnecting', glyph: '◐', fill: 'partial', label: '重连中' },
  offline: { state: 'offline', glyph: '○', fill: 'hollow', label: '离线' },
  unknown: { state: 'unknown', glyph: '◌', fill: 'partial', label: '状态未知' },
  active: { state: 'active', glyph: '●', fill: 'filled', label: '运行中' },
  disconnected: { state: 'disconnected', glyph: '◐', fill: 'partial', label: '断线' },
  ended: { state: 'ended', glyph: '○', fill: 'hollow', label: '已结束' },
}

/**
 * 取一个态的描述。表外的值**原样回显**而不是抛错：
 *
 * 服务端加了一个新状态而前端还没跟上时，页面应该显示那个状态的原文，
 * 让「前端落后」在界面上可见；抛错会让整个列表渲染不出来，
 * 把一个前端的落后变成整个页面的空白。缺字形用 `◌`，它在自己的
 * 设计里就是「未知」。
 */
export function describeStatus(state: string): StateSpec {
  const known = STATES[state as AnyStatus]
  if (known) return known
  return { state: state as AnyStatus, glyph: '◌', fill: 'partial', label: state }
}

/** 设备态的取值。子任务 5 的契约测试与它对齐。 */
export const DEVICE_STATUSES: readonly DeviceStatus[] = [
  'online',
  'reconnecting',
  'offline',
  'unknown',
]