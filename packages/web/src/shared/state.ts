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

/**
 * 任务态。`GET /api/projects/{id}/tasks` 的 `state` 字段取这六个值
 * （`docs/architecture/task-model.md:35-40`）。
 *
 * 这六个**字面值**与 Rust 侧 `TaskState` 的 `as_str` 逐字对齐——那份实现才是
 * 状态的真相源（含 10 条边的状态机）。前端只认字符串，不复制状态机：
 * 一次转移合不合法由服务端 `check_transition` 判，非法转移回 409。
 */
export type TaskStatus = 'pending' | 'queued' | 'running' | 'review' | 'done' | 'failed'

/** 渲染层认识的全部态。设备态、会话态、任务态都在其中。 */
export type AnyStatus = DeviceStatus | SessionStatus | TaskStatus

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
 * 态表（**十三个态**：设备四 + 会话三 + 任务六）。
 *
 * 注意 `components.css` §5 的颜色只覆盖本表里的值——本表与它一一对应，
 * 改一处必须改两处，而 `state.test.ts` 断言了这个对应关系（表里多一个态而
 * CSS 没有颜色，在深色底上就是「继承主文字色」，用户看到的是在线设备显示成灰色）。
 *
 * ── 任务六态的字形为什么这么选 ──────────────────────────────────────
 * `running` **复用** `●`：它与 `online` / `active` 同为「绿实心 = 活的」，
 * 规范 §3.4 已有的复用先例；而设备态与任务态不在同一张列表上出现
 * （设备页 / 会话页 / 看板是三个屏），所以不会有人同时看到两个 `●` 却要分辨。
 * 其余五个任务态一律用**从未出现过**的字形，保证灰度下也能与既有七态分开：
 *
 *   pending ◻ 空心   —— 空方块：还没开始（方块 = 事，方块空着 = 还没动）
 *   queued  ◔ 半轮   —— 缺右下的一轮：已排队、还没动
 *   running ● 实心   —— 复用，见上
 *   review  ◑ 半轮   —— 缺左的一轮
 *   done    ✓       —— 完成是一个勾，不是又一个圆
 *   failed  ✕       —— 失败是一个叉，与勾同族（同字号同样式），一眼对立
 *
 * **已知取舍：六个任务态里仍有两个与既有态同族**——`◻`（pending）形状同族于
 * `○`（offline / ended），`◔` / `◑` 同族于 `◐`（reconnecting / disconnected）。
 * 但**字符本身互不相同**（`state.test.ts` 逐个比的是字符，不是「族」），所以
 * 灰度下能分开；这与规范 §3.4 已允许 `◐` 由两个会话态共用是同一条取舍。
 * 为什么不再往「异族字形」上加：进一步异族化会落到 `⚠` / `◆` / `▲` 这类
 * **与告警 / 优先级语义撞车**的字符上——字形本身会被误读成第二个含义。
 */
export const STATES: Readonly<Record<AnyStatus, StateSpec>> = {
  online: { state: 'online', glyph: '●', fill: 'filled', label: '在线' },
  reconnecting: { state: 'reconnecting', glyph: '◐', fill: 'partial', label: '重连中' },
  offline: { state: 'offline', glyph: '○', fill: 'hollow', label: '离线' },
  unknown: { state: 'unknown', glyph: '◌', fill: 'partial', label: '状态未知' },
  active: { state: 'active', glyph: '●', fill: 'filled', label: '运行中' },
  disconnected: { state: 'disconnected', glyph: '◐', fill: 'partial', label: '断线' },
  ended: { state: 'ended', glyph: '○', fill: 'hollow', label: '已结束' },
  pending: { state: 'pending', glyph: '◻', fill: 'hollow', label: '待办' },
  queued: { state: 'queued', glyph: '◔', fill: 'partial', label: '排队中' },
  running: { state: 'running', glyph: '●', fill: 'filled', label: '执行中' },
  review: { state: 'review', glyph: '◑', fill: 'partial', label: '待验收' },
  done: { state: 'done', glyph: '✓', fill: 'filled', label: '已完成' },
  failed: { state: 'failed', glyph: '✕', fill: 'filled', label: '失败' },
}

/**
 * 字形复用白名单：**只有这几组**允许同形（`docs/design-system.md` §3.4）。
 *
 * 写成数据而不是散在组件断言里：`state.test.ts` 拿它当「允许的复用」的
 * 唯一依据，同时也反向校验它本身没写错（组内必须真的同形、组外没有态用这个字形）。
 */
export const GLYPH_SHARING_GROUPS: readonly (readonly AnyStatus[])[] = [
  ['online', 'active', 'running'],
  ['reconnecting', 'disconnected'],
  ['offline', 'ended'],
]

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

/**
 * 设备态的取值。**只有测试在用**（`state.test.ts` 拿它核对词表完整性）；
 * 生产页面都走 `describeStatus()` 或 `StateBadge`。它留在这里而不是删掉，
 * 是因为删了就没有一条断言守着「`STATES` 少了某个设备态」。
 */
export const DEVICE_STATUSES: readonly DeviceStatus[] = [
  'online',
  'reconnecting',
  'offline',
  'unknown',
]

/**
 * 任务态的取值。与 `TaskStatus` 同源，顺序照 `task-model.md:35-40` 的列序。
 *
 * 它存在的理由与 `DEVICE_STATUSES` 一样：**列出一份而不是在各处手抄**。
 * **但它不是看板的取数来源**——`BoardPage.vue` 的 `COLUMNS` 自己带
 * 每列一句 `hint`，那是列数据不是词表，仍读 `STATES[key].label` 取中文。
 * 生产代码零引用；用它的只有 `state.test.ts`（含与 `TASK_STATE_WIRE`
 * 的同源断言，见 `state.test.ts:237`）。
 */
export const TASK_STATUSES: readonly TaskStatus[] = [
  'pending',
  'queued',
  'running',
  'review',
  'done',
  'failed',
]