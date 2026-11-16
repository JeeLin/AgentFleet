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

   ── 六列的顺序照原型 ─────────────────────────────────────────────
   `pending(待办) / queued(排队中) / running(执行中) / review(待验收) /
   done(已完成) / failed(失败)`（`prototype/board.html:59-111`、
   `assets/board.js:176-179` 的 `STATE_NAMES`）。

   **「失败」在最后一列**，不是「完成」之前。旧版本这里写的是「失败排在
   完成前，因为失败需要处置、排最后会被滚出视野」——那是 v0.7.0 的猜测，
   原型不是这么排的，而且它混淆了两件事：

     状态机的列序（六态的自然生命周期）  ≠  需要处置的提示

   后者的正确位置是**页头的「等你决定」计数**（原型 `prototype/index.html:50`
   的项目卡上有这个数；`board.html` 本身**没有**它——看板页只有六列）。不是把失败列
   搬到中间。把失败插在完成之前反而会让「完成」这个终点看起来在板子中间。

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

import { computed, onMounted, onUnmounted, ref, watch } from 'vue'

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
import { navigate, route } from '@/router'

/** 当前项目 id，从地址读——地址栏是真相源。 */
const projectId = computed(() => route.value.params.id ?? '')

const project = ref<Project | null>(null)
const all = ref<Project[]>([])
const tasks = ref<Task[]>([])
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
  { key: 'done', hint: '已合入主干' },
  { key: 'failed', hint: '失败的任务停在这里，不会自动回待办' },
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
  return tasks.value.filter((t) => t.state === key)
}

/** 整板无任务。与「某一列为空」是两件事，空态文案也不同。 */
const boardEmpty = computed(() => tasks.value.length === 0)

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
  loading.value = true
  failure.value = ''
  actionFailure.value = ''
  try {
    // 三个请求并行：切换器要整份项目列表，主区要当前项目，看板要任务。
    const [p, list, ts] = await Promise.all([getProject(id), listProjects(), listProjectTasks(id)])
    project.value = p
    all.value = list
    tasks.value = ts
  } catch (e) {
    project.value = null
    tasks.value = []
    failure.value = describeReadFailure(e, '项目')
  } finally {
    loading.value = false
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
 */
function openTask(id: string): void {
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

function openDialog(): void {
  draftTitle.value = ''
  draftCriteria.value = ''
  draftDescription.value = ''
  createFailure.value = ''
  dialogOpen.value = true
}

function closeDialog(): void {
  dialogOpen.value = false
}

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
  <!-- 页面骨架照原型 `board.html:38-56`：
         `.af-body.af-body--pad` 包住整页内容，页头是 `.af-head` 那一行。
       上一版是 `.af-page`（`max-width:1400px; margin:0 auto` 的居中单栏），
       原型是「外壳里吃满宽度的主区」——两者的第一眼差别就在这里。 -->
  <div class="af-body af-body--pad">
    <header class="af-head af-head--page">
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

    <!-- 播报「已刷新」：按钮按下去要有一个能听见的回应，否则读屏用户不知道按到了没有。 -->
    <p v-if="refreshed" class="af-muted" role="status" aria-live="polite">已刷新</p>

    <p v-if="loading" class="af-muted">正在读取项目…</p>

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
      <h2 class="af-page__subtitle">{{ project?.name ?? '看板' }}</h2>

      <!-- 写失败（含「重读任务失败」）单独一条，看板留在下面：写失败时把整页
           换成错误态，用户会以为任务没了。 -->
      <p v-if="actionFailure" class="af-form__error" role="alert">{{ actionFailure }}</p>

      <!-- 整板空 ≠ 六列各空：**列照渲染**（`prototype/assets/board.js:347`：
           「空着的列本身是信息」——用户要能看见这块板子有六站），但列内不再
           各写一句「这一列还没有任务」。六条「空」并排会让人以为六个独立问题，
           而真相是「一块空板子」。所以整板空时只多一条横幅。 -->
      <EmptyState
        v-if="boardEmpty"
        glyph="▢"
        title="这个项目还没有任务"
        text="新建一个任务，它会出现在待办列；跑起来之后会自动往前流。"
      >
        <button type="button" class="af-btn af-btn--primary" @click="openDialog">新建任务</button>
      </EmptyState>

      <div class="af-board">
        <section
          v-for="col in COLUMNS"
          :key="col.key"
          class="af-board__col"
          :data-state="col.key"
        >
          <header class="af-board__colhead">
            <h3 class="af-board__coltitle">{{ labelOf(col.key) }}</h3>
            <!-- 计数 = 当前数组长度，不做「+3 更多」折叠（原型没有）。 -->
            <span class="af-board__count" :aria-label="`${byState(col.key).length} 张`">
              {{ byState(col.key).length }}
            </span>
          </header>
          <p class="af-board__hint">{{ col.hint }}</p>

          <!-- 列体：卡片都排在这一层里，内边距与卡片间距来自 `.af-board__colbody`
               （原型 `.af-col__body`：`padding:8px; gap:8px`）。上一版卡片直接
               排在列里，所以列头的左边与卡片左边差 16px。 -->
          <div class="af-board__colbody">
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
         这是一个真模态（提交期间不该点背后的看板），不是内联表单。 -->
    <div v-if="dialogOpen" class="af-scrim">
      <div class="af-modal" role="dialog" aria-modal="true" aria-label="新建任务">
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
  </div>
</template>
