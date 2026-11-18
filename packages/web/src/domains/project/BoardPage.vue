<script setup lang="ts">
/* ===========================================================================
   BoardPage —— 看板（`prototype/board.html`，v0.8.0 子任务 7）
   ---------------------------------------------------------------------------
   项目详情的**默认落点**（`PRODUCT.md` §3.3：看板优先）。没有看板的项目页
   只是一个仓库浏览器。

   ── 这一页负责什么、不负责什么 ──────────────────────────────────
   负责：把哪些任务放进哪一列、点开去哪、新建任务、刷新、错误态。
   不负责：卡片内部的一切（标题 / 状态徽标 / 已跑时长 / 失败人话 / 标签 /
   元信息）——那是子任务 4 的 `TaskCard`。本节只 import 与传 props。
   两份实现必然漂：卡片上多一个标签时，只有一份会被改。

   ── 切换器为什么必须真切换 ────────────────────────────────────────
   只换标题是最容易做出来、也最容易骗过审查的假实现：地址变了、页眉变了，
   而列还是同一批。所以切换器**读真项目**（`GET /api/projects`），切换后
   整页重新按新项目加载。

   ── 六列的顺序：**有意偏离原型**（用户裁定 2026-11-17） ─────────────
   实现：`pending(待办) / queued(排队中) / running(执行中) / review(待验收) /
   failed(失败) / done(已完成)`。

   原型是 `… review / done / failed`（`prototype/board.html:59-111`、
   `assets/board.js:176-179` 的 `STATE_NAMES`），**失败在最后一列**。
   用户 2026-11-17 裁定把「失败」挪到「完成」**前面**，原话：

     「失败是不是放在完成左边，完成的一般都不会再看」

   这个理由站得住：**完成列是终点**，人看一眼就走了、不会回头再翻；
   而**失败列需要处置**（重试 / 关掉 / 看原因），埋在横向滚动的最右边
   会被吃掉。所以这里是**故意不照原型**，不是搬漏了——别再改回去。

   曾经的反面论证（v0.7.0 写的「失败排在完成前是猜测」，以及紧随其后
   「把失败插在完成之前会让完成这个终点看起来在板子中间」）：前半句已被
   这次裁定推翻；后半句仍然成立，但用户认为「失败更容易被滚出视野」的代价
   更大，故取失败优先。**六列的相对顺序其余部分不变**，只有 failed 换了位置。

   两列被去掉：`todo` 从来不是任务态（旧代码里的占位键）；`cancelled` 也不
   是——v0.8.0 的任务六态里没有「已取消」，**主动放弃 = 失败处置的「关掉」**
   （`task-model.md:244`：`failed` 终态 + `closed_reason`），任务留在 `failed`
   并记下原因。所以「主动放弃」
   的任务显示在失败列，那不是遗漏，是语义。

   ── 列头文案从词表取，不在这里再写一遍 ────────────────────────────
   `COLUMNS` 里只有 `key`，`label` 一律读 `STATES[key].label`。写两遍就会
   有两份「待验收」/「待验受」漂移的风险，而 `state.ts` 已经是 13 态词表的
   唯一真相源（它又被徽标与 CSS 的颜色规则对着）。

   ── 时钟只在页面层 ────────────────────────────────────────────────
   `now` 由这里一个 `setInterval(1000)` 更新，`onUnmounted` 清除；20 张卡
   共用同一份 `now`。卡片自己持定时器的话，一屏 20 张卡就是 20 个定时器，
   而且卸载时漏清一个就是内存泄漏（子任务 4 的组件约定「组件不持 timer」，
   `TaskCard.test.ts` 有一条源码级断言守着）。
   =========================================================================== */

import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import StateBadge from '@/domains/shared/components/StateBadge.vue'
import TaskCard from '@/domains/project/TaskCard.vue'
import { getProject, listProjects, type Project } from '@/domains/project/api'
import { CRITERIA_CODES, createTask, listProjectTasks } from '@/domains/project/taskApi'
import type { DoneWhen, Task, TaskStateWire } from '@/domains/project/taskTypes'
import { STATES } from '@/shared/state'
import { describeShape } from '@/shared/projectShape'
import { parseStamp } from '@/shared/format'
import { ApiError, describeReadFailure } from '@/shared/http'
import {
  focusRouteTarget,
  handleLayerKeydown,
  lastRouteFocusId,
  pop,
  push,
  setRouteFocusTarget,
} from '@/shared/focus'
import { navigate, route } from '@/router'

/** 当前项目 id，从地址读——地址栏是真相源。 */
const projectId = computed(() => route.value.params.id ?? '')

const project = ref<Project | null>(null)
const all = ref<Project[]>([])
const tasks = ref<Task[] | null>(null)
const loading = ref(true)
/** 读失败（项目或任务）。整页进错误态——没有数据时让用户看到半份更糟。 */
const failure = ref('')
/** 写失败。**与 `failure` 分开**：写失败时看板还要留在页面上。 */
const actionFailure = ref('')

/**
 * 六列。`key` = 规格六态字符串（`TaskStateWire`），顺序 = 原型。
 *
 * `satisfies` 保证 `key` 落在六态上——写错一个字（例如 `queued` 拼成
 * `queue`）是编译错误，而不是一列永远空的板子。
 */
const COLUMNS = [
  { key: 'pending', hint: '还没开始的任务' },
  { key: 'queued', hint: '已排队，等设备腾出来' },
  { key: 'running', hint: '正在设备上执行' },
  { key: 'review', hint: '跑完了，等你看过改动再定' },
  /* failed 在 done **前面**：用户 2026-11-17 裁定（「完成的一般都不会再看」，
     而失败的还要处置）。有意偏离原型，理由见文件头「六列的顺序」那一节。 */
  { key: 'failed', hint: '失败的任务停在这里，不会自动回待办' },
  { key: 'done', hint: '已合入主干' },
] as const satisfies readonly { key: TaskStateWire; hint: string }[]

/** 列头文案的唯一来源（见文件头最后一节）。 */
function labelOf(key: TaskStateWire): string {
  return STATES[key].label
}

/**
 * 某一列的任务。
 *
 * **用 `filter` 而不是「先建六个数组再 `push`」**：后者要求先按下标或按键
 * 建一个桶，桶里没有的态（存量数据里的 `cancelled`、上游新增的态）就得
 * 塞进某个桶或者抛错。`filter` 对未知值是天然的「不进任何列、也不报错」
 * ——脏数据不该让整个看板白屏（`describeStatus` 对未知态同一条口径）。
 */
function byState(key: TaskStateWire): Task[] {
  return (tasks.value ?? []).filter((t) => t.state === key)
}

/** 整板无任务。与「某一列为空」是两件事，空态文案也不同。 */
const boardEmpty = computed(() => (tasks.value ?? []).length === 0)

/**
 * `load()` 的轮次令牌：只用来判断「这次回来的结果还是不是当前的」。
 * 见 `load()` 里那段注释（换项目时上一轮仍在飞）。
 */
let loadToken = 0

/**
 * 卡片要的「最近一次执行」。
 *
 * 列表端点带 `executions`（`list_tasks` 里一条 SQL 批量取，没跑过的任务给
 * `[]`），但组件把它声明成可选——数组万一缺了，卡片仍要能渲染。
 *
 * **必须和 `ProjectTaskPage.latestExecution` 同口径**：后端 `list_for_task` 是
 * `created_at DESC`（最新在前），所以「数组最后一条」是**最旧**那条——
 * 直接取 `list[list.length - 1]` 会把上一轮的失败显示成这一轮的。这里按
 * `started_at` 升序排完再取最后一条，与详情页逐字同口径。
 */
function latestExecution(t: Task): NonNullable<Task['executions']>[number] | null {
  const list = t.executions
  if (!list || list.length === 0) return null
  const stamp = (e: NonNullable<Task['executions']>[number]): number =>
    parseStamp(e.started_at) ?? Number.MAX_SAFE_INTEGER
  const ordered = [...list].sort((a, b) => stamp(a) - stamp(b))
  return ordered[ordered.length - 1] ?? null
}

/* ---- 时钟 -------------------------------------------------------------- */

/**
 * 当前时刻。**这一页是全看板唯一的时钟**，每秒更新一次。
 *
 * 为什么是 1 秒而不是 1 分钟：`running` 卡的「已跑 3 分 12 秒」里有一档是
 * 「秒」，跳得比真实慢会让人以为卡住了。为什么不是 60 秒：那会让秒位停在
 * 某个值上不动，同样是假信息。
 */
const now = ref(Date.now())
let clock: ReturnType<typeof setInterval> | null = null

/* ---- 取数 -------------------------------------------------------------- */

/** 只重读任务（刷新按钮、建任务成功后都走它）。错误进 `actionFailure`。 */
async function reloadTasks(): Promise<void> {
  const id = projectId.value
  if (!id) return
  try {
    tasks.value = await listProjectTasks(id)
    actionFailure.value = ''
  } catch (e) {
    // **不把错误吞成空数组**：空数组会渲染成「这个项目还没有任务」，
    // 那是一句假话（板子不是空的，是没读到）。
    actionFailure.value = describeReadFailure(e, '任务')
  }
}

async function load(): Promise<void> {
  const id = projectId.value
  if (!id) {
    failure.value = '这个地址缺少项目 id。'
    loading.value = false
    return
  }
  // 换项目时 `watch` 会再跑一次 `load`，而上一轮那次可能还在飞：它回来时
  // `projectId` 已经换了，于是把**旧项目**的任务写进新项目的板子（症状是
  // 「切过去先闪一下上一个项目的卡」，也可能是「读到一半 `tasks` 还是 null
  // 就被渲染」）。所以每次进来领一个令牌，回来时对不上就整份丢掉——
  // 连 `loading` 也不许动，那会把新一轮的 loading 提前关掉。
  const token = ++loadToken
  loading.value = true
  failure.value = ''
  actionFailure.value = ''
  try {
    // 三个请求并行：切换器要整份项目列表，主区要当前项目，看板要任务。
    const [p, list, ts] = await Promise.all([getProject(id), listProjects(), listProjectTasks(id)])
    if (token !== loadToken) return
    project.value = p
    all.value = list
    tasks.value = ts
  } catch (e) {
    if (token !== loadToken) return
    project.value = null
    tasks.value = []
    failure.value = describeReadFailure(e, '项目')
  } finally {
    if (token === loadToken) {
      loading.value = false
      /* 数据到齐后补一次「返回来源卡片」的焦点（见 `restoreSourceFocus`）。
         只有真的有待找回的目标时才做，否则会抢当页自己的首焦点。 */
      void restoreSourceFocus()
    }
  }
}

  // 换项目时重载：`:id` 变了就是不同项目，不能沿用上一份数据。
  watch(projectId, load)
  onMounted(() => {
    clock = setInterval(() => {
      now.value = Date.now()
    }, 1000)
    void load()
  })

/* 从任务详情**返回**看板：把焦点还给刚才那张卡（v0.8.1 第5轮 G65）。

   **必须在数据到齐之后补一次**（第5轮 4b 实测）。原先只在 `onMounted` 里
   调一次，而那时 `load()` 还没发出去——卡片一张都没渲染，`focusRouteTarget`
   找不到目标就退到 `main`（它的注释当时把这写成「等价于旧行为，不会更坏」，
   实际上它**静默地**什么都没做）。实测鼠标路径：点卡 → 详情 → 点「← 看板」
   → 焦点落在 `main`，`dataId: null`；键盘路径碰巧对，因为那时详情页的 `h1`
   带着 `data-id` 且渲染更早。这条 bug 因此藏了三轮。

   判据用 `lastRouteFocusId()`：只有「上一次落地带着 `data-id`」（即用户是从
   某张卡/某一行进来的）才补这一次。换项目、直接打开看板时它是 `null`，
   不抢当页自己的首焦点。`await nextTick()` 让 `v-for` 把卡片真的挂上去。 */
async function restoreSourceFocus(): Promise<void> {
  if (lastRouteFocusId() === null) return
  await nextTick()
  // 再等一帧：`focusRouteTarget` 按 `data-id` 找，卡片此时必须在 DOM 里。
  requestAnimationFrame(() => focusRouteTarget())
}
// 卸载必须清：不清就是每次进出看板多一个每秒跑一次的定时器。
onUnmounted(() => {
  if (clock !== null) {
    clearInterval(clock)
    clock = null
  }
})

/**
 * 刷新按钮。
 *
 * 不做自动轮询：本里程碑不推 WebSocket 事件（后端「全程不写
 * `protocol::MessageType`」，见里程碑子任务 6 的后端流程 8），轮询一份
 * 不变的列表只是白烧请求。页头给一个按钮 + `aria-live` 播报「已刷新」，
 * 用户按一下就知道按到了。
 */
const refreshed = ref(false)

async function refresh(): Promise<void> {
  await reloadTasks()
  refreshed.value = true
}

/* ---- 切换器 ------------------------------------------------------------ */

/**
 * 切换器菜单开着没有。
 *
 * **项目名本身就是那个按钮**（见 `components.css` 的 `.af-switch`）：左栏收起
 * 之后这是唯一的换项目入口，所以它必须一直可见，不能藏进二级菜单。
 */
const menuOpen = ref(false)

function switchTo(id: string): void {
  // 先关菜单再跳转：不关的话，同一页换了项目之后菜单还开着，
  // 用户会以为「点了没反应」（其实已经跳了，菜单挡住了新内容）。
  menuOpen.value = false
  if (id === projectId.value) return
  // **必须编码**：`matchPattern` 对具名段做 `decodeURIComponent`，id 里含
  // `/` 或 `%` 时不编码会把路径切成多一段，落进 `not-found`。
  navigate(`/project/${encodeURIComponent(id)}/board`)
}

/* ---- 列顶的六态形状 ---------------------------------------------------- */

/**
 * 状态 → 原型的形状类（`prototype/assets/app.css:235-260`）。
 *
 * **形状是颜色之外的第二重编码**：排队色与失败色的亮度很接近（原型在同一段
 * 注释里写了这一点），只靠色带分不出来。原型的列头结构是
 * `.af-shape.af-shape--<state>` + `.af-col__name` + `.af-col__count`
 * （`prototype/board.html:61-63`），这里照抄。
 *
 * 用 `satisfies Record<TaskStateWire, string>` 而不是拼字符串：键漏一个就是
 * 编译错误，而不是「某一列少了形状、要肉眼比六张截图才发现」。
 * 这三个类都已在 `prototype-shell.css` §7（`.af-shape`）有定义，不新增样式。
 */
const SHAPE_CLASS = {
  pending: 'af-shape--pending',
  queued: 'af-shape--queued',
  running: 'af-shape--running',
  review: 'af-shape--review',
  done: 'af-shape--done',
  failed: 'af-shape--failed',
} as const satisfies Record<TaskStateWire, string>

/* ---- 点卡片 ------------------------------------------------------------ */

/**
 * 打开一张卡。
 *
 * 走既有的 `navigate()`，并**必须 `encodeURIComponent`**（与本文件的
 * `switchTo`（`:226`）同口径）：任务 id 里含 `/` 或
 * `%` 时不编码会把路径切成多一段。
 *
 * 卡片上**没有其他可点区域**：本轮不做拖拽换列——拖拽要 6×6 的合法边判断，
 * 做错就是把非法转移画成合法。
 *
 * ── 为什么先登记 `setRouteFocusTarget`（v0.8.1 第5轮 G65） ──────────
 * 不登记的话，换页后焦点由 `focusRouteTarget()` 落到 `main[tabindex="-1"]`，
 * 键盘用户得重新 Tab 一遍整页才够得着「← 看板」。登记之后下一次落焦点让给
 * 「页面自己落标题」这一档——**焦点交给任务详情的 `h1`（任务标题）**，
 * 那才是用户刚点开的东西。清理由 `focusRouteTarget()` 负责，这里只管登记。
 */
function openTask(id: string, event?: Event): void {
  /* 记下发起导航的那张卡：返回看板时 `focusRouteTarget()` 按 `data-id` 找回
     同一张卡。

     **必须取 `event.currentTarget`，不能用 `document.activeElement`**
     （v0.8.1 第5轮 4b 实测）。Chrome 里鼠标点 `<button>` **不会**把它设为
     `activeElement`——那是 Safari/Firefox 的行为。所以鼠标用户点卡进详情、
     再点「← 看板」回来时，`activeElement` 是 `<body>`，登记到的是 `null`，
     焦点落到 `main` 而不是原卡（实测 `afterBack: {tag:"MAIN", dataId:null}`）。
     键盘用户（Tab 到卡上按 Enter）碰巧是对的，因为那时按钮确实有焦点——
     这正是这条 bug 藏了三轮的原因：`BoardPage.test.ts:906` 的用例手动
     `card!.focus()` 之后才点，把鼠标路径盖住了。
     `currentTarget` 在两种路径下都是那张卡本身。 */
  const source = (event?.currentTarget ?? document.activeElement) as HTMLElement | null
  setRouteFocusTarget(source instanceof HTMLElement ? source : null)
  navigate(`/project/${encodeURIComponent(projectId.value)}/task/${encodeURIComponent(id)}`)
}

/* ---- 新建任务对话框 ---------------------------------------------------- */

/**
 * 标题 + 判定码两列，取自 `task-model.md` 的 `criteria` / `description`。
 *
 * ⚠️ 原型里**没有**这个表单：`prototype/board.html:42` 只有一行入口链接
 * `<a class="af-btn af-btn--primary" href="task.html">新建任务</a>`，全文零
 * `<form>`、零「标题 / 判定码 / 描述」字段。所以字段组成不是「照原型抄」，
 * 而是按任务模型（`criteria` + `description`）定的；原型的 `task.html` 里才有
 * 任务详情的字段排布。
 *
 * 「不设判定码」不是一个缺失值，而是一个**有意义的选择**：它让任务变成
 * 「跑完要人看过才算完」（`needs_manual_acceptance`）。所以选它的时候当场
 * 显示「此任务需人工验收」，用户在下拉里就已经知道自己选了什么。
 */
const dialogOpen = ref(false)
/** 对话框根（`af-scrim`）。焦点栈要它：`push` 在它内部找第一个可聚焦元素。 */
const modalEl = ref<HTMLElement | null>(null)
/** 打开对话框的那颗按钮。关框后焦点还给它——这是 `push({opener})` 的入参。 */
let lastOpener: HTMLElement | null = null
const draftTitle = ref('')
/** `''` = 不设判定码（后端收 `criteria: null`）。 */
const draftCriteria = ref('')
const draftDescription = ref('')
const creating = ref(false)
const createFailure = ref('')

const trimmedTitle = computed(() => draftTitle.value.trim())
/** 判定码与描述至少一个非空——两者都空的 `done_when` 后端会 400（`invalid_criteria`）。 */
const doneWhenEmpty = computed(
  () => draftCriteria.value === '' && draftDescription.value.trim() === '',
)
/** 选「不设判定码」→ 由人验收。与后端的派生规则同一条（`criteria` 空且描述非空）。 */
const needsManualAcceptance = computed(() => draftCriteria.value === '')

/**
 * 提交按钮能不能点。
 *
 * 判据是「三处都齐」：标题非空、判定码或描述至少一个、没有正在提交。
 * 空 `done_when` 后端会 400，在这里先挡住能给出更具体的提示（与
 * `BindPage` 的空目录校验同一条理由）。
 */
const canSubmit = computed(
  () => trimmedTitle.value.length > 0 && !doneWhenEmpty.value && !creating.value,
)

/* ── 对话框的浮层语义（焦点栈 / Esc / Tab / 点遮罩）──────────────────
   **这一节是 v0.8.1 第5轮 4d 验收补上的**，原先这个对话框只有
   `v-if="dialogOpen"` + `role="dialog" aria-modal="true"`——三件「看起来
   像模态」的装饰，但没有一样真的生效。实测（playwright / 键盘）：
     · Esc 不关框：本文件根本没绑 `keydown`；
     · Tab 能一路逃出框，落到 `<body>`，框背后的看板还在 tab 序列里；
     · 点遮罩不关框：`af-scrim` 上没有点击处理；
     · 点「取消」后焦点回到 `<body>`，键盘用户失去位置。
   `aria-modal="true"` 在背后没设 `inert` 时是**假的**——读屏照样能念到
   看板内容。而 `focus.ts` 早就把这几件事都实现了（`push` 负责背后 inert
   与首焦点、`pop` 负责还焦点、`handleLayerKeydown` 管 Esc/Tab），
   全仓另外 5 个浮层（`LoginPrompt` / `TrajDrawer` / `OverviewPage` 抽屉 /
   `SessionPage` dock）都在用，只有这里漏了。这就是「同一范式漏一处」，
   不是新设计——所以照 `LoginPrompt.vue` 的接线抄，不另造一套。 */

/** 这一层是否已经入栈。防重复 `push`（重渲染不该再入一次栈）。 */
let pushed = false

function openLayer(): void {
  const el = modalEl.value
  if (!el || pushed) return
  // 传 `opener`：这个框**是**用户点「新建任务」点出来的，关掉后焦点该回到
  // 那颗按钮上（`LoginPrompt` 不传是因为 401 没有触发元素）。
  push(el, { opener: lastOpener })
  pushed = true
  document.addEventListener('keydown', onLayerKeydown)
}

function closeLayer(): void {
  const el = modalEl.value
  if (!el || !pushed) return
  pushed = false
  document.removeEventListener('keydown', onLayerKeydown)
  pop(el)
}

/** Esc / Tab。判据（本层是栈顶吗、输入法选字中吗）都在 `handleLayerKeydown` 里。 */
function onLayerKeydown(e: KeyboardEvent): void {
  const el = modalEl.value
  if (!el) return
  handleLayerKeydown(e, el, closeDialog)
}

/** 点遮罩：只有点在遮罩自己上才算（点在框里不该关）。 */
function onScrim(e: MouseEvent): void {
  if (e.target === e.currentTarget) closeDialog()
}

function openDialog(): void {
  draftTitle.value = ''
  draftCriteria.value = ''
  draftDescription.value = ''
  createFailure.value = ''
  // 记下触发元素再开框：`push` 自己也读 `activeElement`，但这里显式传更稳
  // （点击事件与 Vue 的 patch 之间有异步空隙）。
  lastOpener = document.activeElement instanceof HTMLElement ? document.activeElement : null
  dialogOpen.value = true
}

/** 关框。**四条路都走这里**（取消 / Esc / 点遮罩 / 提交成功后），
    所以「出栈 + 还焦点」只有一条实现。 */
function closeDialog(): void {
  closeLayer()
  dialogOpen.value = false
}

/* `dialogOpen` 一变就入栈/出栈。用 `watch` 而不是在 `openDialog` 里直接
   `openLayer()`：`modalEl` 是 `v-if` 渲染出来的，`openDialog` 返回时 DOM
   还不存在，`push` 会拿到 `null`。`flush: 'post'` 保证在 DOM 更新之后跑，
   `modalEl.value` 那时才有值。 */
watch(dialogOpen, (open) => {
  if (open) openLayer()
  else closeLayer()
}, { flush: 'post' })

/* 组件卸载时若还开着，必须摘监听并出栈——否则 `document` 上留一个指向已
   卸载组件的闭包，下一次 Esc 会调到一个不存在的框上。 */
onUnmounted(() => {
  if (pushed) closeLayer()
})

async function submitCreate(): Promise<void> {
  if (!canSubmit.value) return
  creating.value = true
  createFailure.value = ''
  const doneWhen: DoneWhen = {
    criteria: draftCriteria.value === '' ? null : draftCriteria.value,
    description: draftDescription.value.trim() === '' ? null : draftDescription.value.trim(),
  }
  try {
    await createTask(projectId.value, { title: trimmedTitle.value, done_when: doneWhen })
    // **关闭对话框并重新拉列表**，不本地插入：本地插入的卡缺后端派生字段
    // （`needs_manual_acceptance` / `criteria_known` / `version`），会显示错值。
    dialogOpen.value = false
    await reloadTasks()
  } catch (e) {
    createFailure.value =
      e instanceof ApiError ? `建任务失败（${e.code}）。` : '建任务失败：Hub 连不上。'
  } finally {
    creating.value = false
  }
}
</script>

<template>
  <!-- 页面骨架照原型 `board.html:15-114`，只有两层：页头 `.af-head` +
        内容区 `.af-body.af-body--fill`。
        上一版是 `.af-page`（`max-width:1400px; margin:0 auto` 的居中单栏），
        原型是「外壳里吃满宽度的主区」——两者的第一眼差别就在这里。
        内容区用 `--fill` 而不是 `--pad`：原型的页头**不在**内容区里，两块是
        `.af-main` 下的兄弟；而且 `--fill`（`display:flex; flex-direction:column;
        overflow:hidden`，`prototype/assets/app.css:134`）才是让 `.af-board`
        吃满剩余高度、六列各自内部滚动的那一档。

        **页头必须是 `.af-body` 的兄弟，不能嵌在里面**（`prototype/board.html:29`
        的 `header.af-head` 在 `:57` 的 `div.af-body` 之前，8 个原型页无一例外）。
        嵌进去的后果：`.af-body--pad` 的 `padding: var(--s6)` 会把页头挤成内缩，
        而 `.af-body` 又是 `overflow:auto` 的那一层——滚动时页头跟着滚走。
        所以这里是两个兄弟节点，靠外层 `.af-main`（`display:flex;
        flex-direction:column`，`app.css:113`）竖排。 -->
  <header class="af-head">
      <div class="af-switch" :data-open="menuOpen ? 'true' : 'false'">
        <button
          type="button"
          class="af-switch__btn"
          aria-haspopup="true"
          :aria-expanded="menuOpen ? 'true' : 'false'"
          aria-label="切换项目"
          @click="menuOpen = !menuOpen"
        >
          <h1 class="af-head__title af-switch__title">{{ project?.name ?? '看板' }}</h1>
          <span class="af-switch__chev" aria-hidden="true"></span>
        </button>
        <div v-if="menuOpen" class="af-switch__menu" role="menu" aria-label="项目列表">
          <button
            v-for="p in all"
            :key="p.id"
            type="button"
            role="menuitem"
            class="af-switch__item"
            :aria-current="p.id === projectId ? 'true' : 'false'"
            @click="switchTo(p.id)"
          >
            <span class="af-switch__label">{{ p.name }}</span>
            <span class="af-switch__shape">{{ describeShape(p.shape) }}</span>
          </button>
          <button type="button" role="menuitem" class="af-switch__item" @click="navigate('/')">
            <span class="af-switch__label">全部项目</span>
          </button>
        </div>
      </div>
      <!-- 项目内导航：原型 `board.html` 页头的第二段。`af-head__spacer` 把它与
           右侧动作分开（原型 `.af-head__spacer { margin-left: auto }`）。 -->
      <nav class="af-projectnav af-head__spacer" aria-label="项目内导航">
        <button type="button" class="af-projectnav__item" @click="navigate(`/project/${encodeURIComponent(projectId)}/bind`)">
          绑定设备
        </button>
        <button
          type="button"
          class="af-projectnav__item"
          @click="navigate(`/project/${encodeURIComponent(projectId)}/timeline`)"
        >
          时间线
        </button>
        <button
          type="button"
          class="af-projectnav__item"
          @click="navigate(`/project/${encodeURIComponent(projectId)}/settings`)"
        >
          项目设置
        </button>
      </nav>
      <!-- 刷新与新建在右上（原型 `:42` 的「新建任务」就是页头主按钮）：
           不做自动轮询（见脚本里的理由），所以要有一个手动的口子。 -->
      <button type="button" class="af-btn" @click="refresh">刷新</button>
      <button type="button" class="af-btn af-btn--primary" @click="openDialog">新建任务</button>
  </header>

  <div class="af-body af-body--fill">
    <!-- 播报「已刷新」：按钮按下去要有一个能听见的回应，否则读屏用户不知道按到了没有。 -->
    <p v-if="refreshed" class="af-muted" role="status" aria-live="polite">已刷新</p>

    <!-- 三个分支**互斥**：`loading` 一句 / 读失败 / 读到数据。写成「一句 loading
         加一块内容」会让两块同时在树上（`loading` 那句与 44 行板子并存）。

         第二条判据是 `tasks !== null`：`tasks` 在首屏那次请求回来之前是 `null`
         （不是 `[]`），板子只在**真的读到**之后才渲染——「还没读到」与「读到了
         空的」是两件事，只有后者才该说「这个项目还没有任务」。 -->
    <template v-if="loading">
      <p class="af-muted">正在读取项目…</p>
    </template>

    <!-- 读失败 → 整页错误态 + 重试。**不画空板子**：空板子会说「这个项目还没有
         任务」，而真相是没读到（规格 §7 后端流程 2：「不把错误吞成空数组」）。 -->
    <EmptyState
      v-else-if="failure"
      glyph="⚠"
      title="打不开这个项目"
      :text="failure"
    >
      <button type="button" class="af-btn" @click="load">重新读取</button>
      <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
    </EmptyState>

    <template v-else>
      <!-- 分区标题（v0.8.1 第5轮 G69）：**不再是 `h2.af-head__sub`**。

           历史：`af-page__subtitle` 是实现自造的那一族（`af-page*`），原型
           `app.css` 里命中 0 条规则；v0.8.1 子任务 2 删自造类名时，为了不让
           这个 `h2` 退回浏览器默认尺寸，顺手挂了 `.af-head__sub`。副作用是
           **同一页出现两个 `.af-head__sub`**：页头那个（`.af-head` 里的副标题）
           与这一个，而后者还是 `h2`——无障碍的标题结构检查因此报
           「H1 + H2（文本与 H1 重复『…』）+ 6 个 H3 列标题」，且页头副标题的
           20.63px 排版被这里重复消费一次。

           现在的做法：降级为 `<p>` 并改挂**正文层**的 `.af-muted`
           （`components.css:1291`，现役的「次要文字」档），与页头
           `.af-head__sub` 彻底分开——页头的小号标题只属于页头，正文里不再造
           第二个标题层级。文案与可见位置都不变；`.af-muted` 自带 12.5px 档，
           视觉上仍是「比正文小一号的说明」。`aria-hidden="true"` 不挂：
           项目名对读屏用户也有意义，`<p>` 不是标题，重复播报的隐患随之消失。 -->
      <p class="af-muted af-board__sub">{{ project?.name ?? '看板' }}</p>

      <!-- 写失败（含「重读任务失败」）单独一条，看板留在下面：写失败时把整页
           换成错误态，用户会以为任务没了。 -->
      <p v-if="actionFailure" class="af-form__error" role="alert">{{ actionFailure }}</p>

      <!-- 整板空 ≠ 六列各空：**列照渲染**（`prototype/assets/board.js:347`：
           「空着的列本身是信息」——用户要能看见这块板子有六站），但列内不再
           各写一句「这一列还没有任务」。六条「空」并排会让人以为六个独立问题，
           而真相是「一块空板子」。所以整板空时只多一条横幅。

           **v0.8.1 子任务 13：这块横幅从 254px 的居中大框改成一行**
           （实测原来是 1216×254，把 844px 高的看板挤掉三成）。它只是板上方
           的一句提示 + 一个动作，不该比板子本身更抢眼。`compact` 就是横排一行。 -->
      <EmptyState
        v-if="boardEmpty"
        compact
        glyph="▢"
        title="这个项目还没有任务"
        text="新建一个任务，它会出现在待办列；跑起来之后会自动往前流。"
      >
        <button type="button" class="af-btn af-btn--primary af-btn--sm" @click="openDialog">新建任务</button>
      </EmptyState>

      <!-- 看板：结构与原型 `board.html:58-113` 逐层对应——
           `.af-board` 是六列的网格，每列 `section.af-col[data-state]`，列头
           `.af-col__head`（形状 + 列名 + 计数），列体 `.af-col__body`。
           这几个类只在 `prototype-shell.css` §5（`.af-board` / `.af-col*`）里
           有规则，那是唯一真相源；`components.css` 不再定义任何 `.af-board*`
           （v0.8.1 删掉了自造层）。本页不再造第二套 `af-board__col*` 类名。
           **注意这里写的是 `.af-board`，不是 `.af-board--proto`**：后者是上一版
           那个「模板零引用」的变体类，已随自造层一起删除（见
           `prototype-shell.css` §5 的改名说明）。 -->
      <div class="af-board">
        <section v-for="col in COLUMNS" :key="col.key" class="af-col" :data-state="col.key">
          <div class="af-col__head">
            <!-- 形状先于列名：扫视时先靠形状分出六态（见脚本里 `SHAPE_CLASS`
                 的理由）。`aria-hidden` 照原型——列名本身已经把状态说清了。 -->
            <span class="af-shape" :class="SHAPE_CLASS[col.key]" aria-hidden="true"></span>
            <h3 class="af-col__name">{{ labelOf(col.key) }}</h3>
            <!-- 计数 = 当前数组长度，不做「+3 更多」折叠（原型没有）。 -->
            <span class="af-col__count" :aria-label="`${byState(col.key).length} 张`">
              {{ byState(col.key).length }}
            </span>
          </div>

          <!-- 列的说明句（「失败的任务停在这里，不会自动回待办」）是实现多出来的
               一段文案，原型列头只有上面三样。它排在 `.af-col__head` 与
               `.af-col__body` **之间**，是 `.af-col`（flex column）的直接子元素：
               不进 `.af-col__head`——否则会挤在列名与计数之间、把
               `margin-left:auto` 的计数推歪；也不进 `.af-col__body`——那是
               `padding:8px; gap:8px` 的卡片容器，混进一段说明会让首个卡片的
               上边距与其余不一致。 -->
          <p class="af-col__note">{{ col.hint }}</p>

          <!-- 列体：卡片都排在这一层里，内边距与卡片间距来自 `.af-col__body`
               （原型 `app.css:409-413`：`padding:8px; gap:8px`）。上一版卡片
               直接排在列里，所以列头的左边与卡片左边差 16px。 -->
          <div class="af-col__body">
            <TaskCard
              v-for="t in byState(col.key)"
              :key="t.id"
              :task="t"
              :now="now"
              :execution="latestExecution(t)"
              @open="openTask"
            />

            <!-- 整板空时不再逐列重复空态（见上）。 -->
            <EmptyState
              v-if="!boardEmpty && byState(col.key).length === 0"
              glyph="▢"
              title="这一列还没有任务"
            />
          </div>
        </section>
      </div>
    </template>

    <!-- 新建任务对话框。
         **沿用真卡片的选择器**（`.af-card` / `.af-taskcard__*`，子任务 4 的样式，
         已在 `components.css` §22）：本子任务不许新增 `--af-*` token，也不该
         为了一个对话框再造一套卡面排版。卡片此时尚未变「可点」，所以这里用
         `<div>` 而不是 `<button>`（`TaskCard` 的根才是按钮）。
         这是一个真模态（提交期间不该点背后的看板），不是内联表单。

         **必须 `Teleport to="body"`**（v0.8.1 第5轮 4d 实测）。理由与
         `OverviewPage.vue:521-527` 那条一字不差：`focus.push` 的 `inertOthers`
         只把「body 直接子节点里、不含浮层容器的那几个」置 inert。`#app` 是
         body 的**唯一**子节点，对话框留在里面时容器就包含在 `#app` 里，于是
         「背后全部 inert」退化成「一个都没 inert」——实测开框后 `#app.inert`
         仍是 `false`，脚本能直接把焦点抢到左栏链接上（`behindStealsFocus:
         true`）。Teleport 到 body 后浮层自成第三个子节点，`#app` 才会真的
         被 inert（管家抽屉正是这么做的，实测 `appInert: true` /
         抢焦点失败）。
         `.af-scrim` 是 `position: fixed; inset: 0`（`components.css:518`），
         所以挂到 body 下视觉零变化。 -->
    <Teleport to="body">
      <div v-if="dialogOpen" ref="modalEl" class="af-scrim" @click="onScrim">
        <div
          class="af-modal"
          role="dialog"
          aria-modal="true"
          aria-label="新建任务"
          tabindex="-1"
        >
          <div class="af-modal__head">
            <h2 class="af-head__title">新建任务</h2>
          </div>
          <div class="af-modal__body">
            <div class="af-form">
              <label class="af-field">
                <span class="af-field__label">标题</span>
                <input
                  id="af-newtask-title"
                  v-model="draftTitle"
                  class="af-input"
                  type="text"
                  placeholder="一句话说清要做什么"
                />
              </label>

              <div class="af-field">
                <label class="af-field__label" for="af-newtask-criteria">判定码</label>
                <select id="af-newtask-criteria" v-model="draftCriteria" class="af-select">
                  <option value="">不设判定码</option>
                  <option v-for="c in CRITERIA_CODES" :key="c.code" :value="c.code">
                    {{ c.label }}
                  </option>
                </select>
                <!-- 选「不设判定码」= 跑完不会自动判定 = 必须由人验收。当场说清，
                     别等跑完了才发现要自己盯着。 -->
                <span v-if="needsManualAcceptance" class="af-field__hint">
                  不设判定码 → 此任务需人工验收。
                </span>
              </div>

              <div class="af-field">
                <label class="af-field__label" for="af-newtask-desc">描述</label>
                <textarea
                  id="af-newtask-desc"
                  v-model="draftDescription"
                  class="af-textarea"
                  rows="3"
                  placeholder="怎么才算跑通？"
                ></textarea>
              </div>

              <p v-if="doneWhenEmpty" class="af-field__hint">
                至少要有判定码或描述——两者都空的任务建出来只会空转。
              </p>
              <p v-if="createFailure" class="af-form__error" role="alert">{{ createFailure }}</p>
            </div>

            <!-- 预览：用户在下拉里选的那几条，最后会长成看板上的什么样。
                 预览的文本一律来自输入框（不含任何我们替他编的值）。 -->
            <div class="af-card af-taskcard" aria-label="预览">
              <span class="af-taskcard__title">
                {{ trimmedTitle === '' ? '（还没写标题）' : trimmedTitle }}
              </span>
              <span class="af-taskcard__status">
                <StateBadge state="pending" />
              </span>
              <span class="af-taskcard__why">
                {{ draftCriteria === '' ? '判定：无判定码，跑完由人验收' : `判定：${draftCriteria}` }}
              </span>
            </div>
          </div>
          <div class="af-modal__foot">
            <button type="button" class="af-btn af-btn--ghost" @click="closeDialog">取消</button>
            <button
              type="button"
              class="af-btn af-btn--primary"
              :disabled="!canSubmit"
              @click="submitCreate"
            >
              {{ creating ? '正在创建…' : '创建' }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
/* 列头下面那句列说明（`col.hint`）。
   ---------------------------------------------------------------------------
   原型 `board.html:60-64` 的列头只有「形状 / 列名 / 计数」三样，没有这句说明，
   所以它没有原型类名可挂——原来的 `.af-board__hint` 是随自造层一起删掉的
   （v0.8.1：自造类不留别名）。这句文案本身是**列语义的一部分**（六态的名字
   对第一次见到的人不足以说明「为什么失败要单独一列」），不是装饰，所以保留
   文案，把它的样式收进本页的 scoped 样式：只有一个使用者，放进全局表会让它
   变成第二个看板壳层。

   两侧内边距取 `--p-s3`（12px）与 `.af-col__head` 的左右内边距对齐（原型
   `app.css:389`），上面不加顶距：`.af-col__head` 自带底边线，再垫一段空白会
   让列头和它的说明分成两块。 */
/* 看板正文里的项目名那一行（v0.8.1 第5轮 G69）。
   ---------------------------------------------------------------------------
   它原来是 `<h2 class="af-head__sub">`——借页头的副标题类来压字号。现在降级为
   `<p class="af-muted af-board__sub">`：标题层级归页头独占（一页一个
   `.af-head__sub`），正文这行只保留「比正文小一号的说明」这个视觉意图。
   `.af-muted` 给颜色与字号（`components.css:1291`），这里只补行高与
   「别贴着上边」——`.af-body--fill` 是 `overflow:hidden` 的列容器，没有默认
   段距。 */
.af-board__sub {
  margin: 0 0 var(--p-s2);
  line-height: var(--af-leading-snug);
}

.af-col__note {
  margin: 0;
  padding: var(--p-s2) var(--p-s3) 0;
  font-size: var(--p-t-2xs);
  color: var(--p-fg-3);
  line-height: var(--af-leading-snug);
}
</style>
