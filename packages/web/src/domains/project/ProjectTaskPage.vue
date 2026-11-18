<script setup lang="ts">
/* ===========================================================================
   ProjectTaskPage —— 任务详情（`prototype/task.html`，v0.8.0 子任务 8）
   ---------------------------------------------------------------------------
   这一页回答「这件事是怎么走到这一步的、现在该我做什么」。看板只给一张卡，
   卡上放不下的因果链、执行现场、处置动作都在这里。

   ── 这一页负责什么、不负责什么 ──────────────────────────────────
   负责：读任务（深链接刷新必须能单独工作）、把五段拼起来、把两个面板的
   事件接到端点上、读失败 / 写冲突的错误态。
   不负责：面板内部的一切（4 行数据、5 个动作、二次确认、note 必填）——
   那是子任务 4 的 `FailurePanel` / `AcceptancePanel`。本节只 import、传
   props、接 emit 后调 API。**两个面板的契约只写在组件里**：这里再写一份，
   两份必然漂，而漂的那一次会把「判定码未全绿要写理由」悄悄丢掉。

   ── 五段（照原型，只做本轮范围内的）──────────────────────────────
     1. 这个任务是怎么来的     `prototype/task.html:31-88`
     2. 执行记录               `:90-123`（`ExecutionRepo::list_for_task` 的每一行）
     3. 失败面板               `:126-154`（条件见 `showFailure`）
     4. 验收面板               `:157-181`（只在 `review`）
     5. 任务约束               `:186-220`
   右栏两段**不实现**，写成灰色占位并注明归属：`花的时间和钱` → v0.8.2，
   `管家的判断` → v0.9.0。占位是纯文本而不是 `<a href="#">`：后者会把
   「还没做」伪装成「点了没反应」。

   ── 产出只给「分支名 + 合入状态」────────────────────────────────
   diff 入口**不画**（画了就是死链接：`/execution/…/replay` 还是骨架页）。
   **不编分支名**：库里没有 `branch` 这一列（`branch` 归 v0.8.2 的执行间接力），
   所以这一格写「库里还没有记录分支（v0.8.2 上线）」，与 `FailurePanel` 的
   `outputText` 同一个口径。拼一个 `afleet/<id>/eN` 出去，那串东西拿去
   `git checkout` 必然失败，而「看起来像真 ref」比一个空格外危险。

   ── 状态胶囊与看板列头同一张表 ──────────────────────────────────
   文案一律读 `shared/state.ts` 的 `STATES[key].label`（`BoardPage.vue:108-109`
   就是这么做的）。规格原文希望从 `taskApi.ts` 导出 `STATE_LABELS`，但那份
   映射早就在 `state.ts`，`taskApi.ts` 再导出一份就是第二个真相源——看板写
   「待验收」而详情写「review」正是这条要防的事。

   ── 时钟只在页面层 ──────────────────────────────────────────────
   `now` 由这里一个 `setInterval(1000)` 更新、`onUnmounted` 清除，传进
   `FailurePanel`（它渲染相对时间）。面板自己是**不许**持定时器的
   （子任务 4 的约定，`FailurePanel.test.ts` 有源码级断言守着）。

   ── 写失败与读失败分开 ──────────────────────────────────────────
   读失败可以把整页换成错误态；**写失败不能**——任务还在，用户填的 note
   还在，换掉整页等于把这两样一起丢掉。
   =========================================================================== */

import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import PageHead from '@/domains/shared/components/PageHead.vue'
import StateBadge from '@/domains/shared/components/StateBadge.vue'
import AcceptancePanel from '@/domains/project/AcceptancePanel.vue'
import FailurePanel from '@/domains/project/FailurePanel.vue'
import { getTask, submitAcceptance, submitFailure } from '@/domains/project/taskApi'
import type { Execution, Task } from '@/domains/project/taskTypes'
import { parseStamp, relativeTime } from '@/shared/format'
import { ApiError, describeReadFailure } from '@/shared/http'
import { focusRouteTarget, hasRouteFocusTarget } from '@/shared/focus'
import { STATES } from '@/shared/state'
import { navigate, route } from '@/router'

/** 任务 id 从**地址**读：地址栏是真相源，深链接刷新要能单独工作。 */
const taskId = computed(() => String(route.value.params.taskId ?? ''))

const task = ref<Task | null>(null)
const executions = ref<Execution[]>([])
const loadState = ref<'loading' | 'ready' | 'error'>('loading')
const errorText = ref('')

/**
 * 任务**确实不存在**（404）。
 *
 * 与 `errorText` 分开：读失败是「现在不知道」（值得给一个重试），404 是
 * 「知道了——没有这个任务」，重试一千次也还是 404。给一个永远不会成功的
 * 按钮比不给更坏。
 */
const gone = ref(false)

/** 路由参数缺失。**一个请求都不发**——空 id 会打到 `/api/tasks/`，那不是用户想看的东西。 */
const missingId = computed(() => taskId.value === '')

/* ---- 时钟 -------------------------------------------------------------- */

/**
 * 当前时刻（毫秒）。这一页唯一的时钟，每秒一次。
 *
 * 为什么不是 60 秒：`FailurePanel` 渲染的是「3 分钟前」这类相对时间，跳得
 * 比真实慢会让人以为这条记录卡住了。与 `BoardPage.vue` 同一条口径。
 */
const now = ref(Date.now())
let clock: ReturnType<typeof setInterval> | null = null

/* ---- 取数 -------------------------------------------------------------- */

async function load(): Promise<void> {
  const id = taskId.value
  if (!id) {
    // 守卫放在这里而不只是 `onMounted` 里：以后加重试入口时它也会走这条路。
    loadState.value = 'error'
    errorText.value = '这个地址缺少任务 id。'
    return
  }
  loadState.value = 'loading'
  errorText.value = ''
  gone.value = false
  try {
    const r = await getTask(id)
    task.value = r.task
    executions.value = r.executions
    loadState.value = 'ready'
  } catch (e) {
    task.value = null
    executions.value = []
    loadState.value = 'error'
    if (e instanceof ApiError && (e.code === 'task_not_found' || e.status === 404)) {
      gone.value = true
      errorText.value = '任务不存在或已删除。'
    } else {
      errorText.value = describeReadFailure(e, '任务')
    }
  }
}

onMounted(() => {
  clock = setInterval(() => {
    now.value = Date.now()
  }, 1000)
  if (missingId.value) {
    // 早退之前已经把时钟起好了，所以这里**不能**自己 return 掉定时器：
    // `onUnmounted` 那条清理由同一个组件实例负责，两边成对。
    loadState.value = 'error'
    errorText.value = '这个地址缺少任务 id。'
    return
  }
  void load()
})

/**
 * 换页后的焦点落点（v0.8.1 第5轮 G65）。
 *
 * 从看板点开一张卡进来时，**焦点要落在任务标题上**，而不是 `main`——后者会让
 * 键盘用户第一次 Tab 落到页头的「← 看板」，等于每次进出一个任务都要重新
 * Tab 一遍整页。
 *
 * 三个约束，缺一条就会静默退化：
 *   ① **必须等 `h1` 真的在 DOM 里**：`task` 是异步取到的，页头是
 *      `v-if="task"`。所以在 `task` 变化后 `await nextTick()` 再量，
 *      而不是在 `onMounted` 里（那时 `h1` 还不存在）。
 *   ② **只做一次**：`now` 每秒变一次，盯着 `task` 只触发一次，但仍用
 *      `focusedOnce` 闸住——将来若有人把依赖加宽，也不会每秒把焦点抢回标题。
 *   ③ **只在「有来源」时抢**：`hasRouteFocusTarget()` 为假的路径（直接输地址、
 *      Back 回看板）不该被本页抢走落点，那些交给 `focusRouteTarget()` 的
 *      第 ②/③ 档。
 */
let focusedOnce = false
watch(
  () => task.value,
  async () => {
    if (focusedOnce || !task.value) return
    focusedOnce = true
    // 等 `v-if="task"` 的页头渲染出来再量。`flush: 'post'` 只保证**本次**
    // 更新已应用，`PageHead` 的 `v-if` 是同一帧里插入的，所以这里再
    // `nextTick()` 一次最稳。
    await nextTick()
    const heading = taskHeading()
    if (!heading) return
    // `h1` 默认不可编程聚焦：不补 `tabindex="-1"` 的话 `focus()` 在部分
    // 浏览器里只移动「焦点锚点」而不改 `activeElement`，读屏也不会播报标题。
    // 与 `focusMain()` 给 `h1` 补的那一份是同一个理由（§4.3）。
    if (!heading.hasAttribute('tabindex')) heading.setAttribute('tabindex', '-1')
    heading.focus()
    // 消费掉「让出」标志：路由那次微任务可能已经先跑过，也可能还没跑
    // （取决于 `navigate()` 与本次 `load()` 谁先落地）。消费一次即可——
    // 留着的后果是下一次换页会**多让出一档**，焦点停在 `<body>` 上。
    if (hasRouteFocusTarget()) focusRouteTarget()
  },
  { flush: 'post' },
)

/** 本页的标题元素：`.af-head__title`，带 `data-id`（返回看板时按它找回卡片）。 */
function taskHeading(): HTMLElement | null {
  return document.querySelector<HTMLElement>('h1.af-head__title[data-id]')
}

// 卸载必须清：不清就是每次进出任务页多一个每秒跑一次的定时器。
onUnmounted(() => {
  if (clock !== null) {
    clearInterval(clock)
    clock = null
  }
})

/* ---- 派生数据 ---------------------------------------------------------- */

/** 执行记录按 `started_at` 升序；「最近一条」永远是最后一个（`BoardPage.latestExecution` 同口径）。 */
const orderedExecutions = computed<Execution[]>(() => {
  const stamp = (e: Execution): number => parseStamp(e.started_at) ?? Number.MAX_SAFE_INTEGER
  return [...executions.value].sort((a, b) => stamp(a) - stamp(b))
})

const latestExecution = computed<Execution | null>(
  () => orderedExecutions.value[orderedExecutions.value.length - 1] ?? null,
)

/**
 * 失败面板出现条件：任务是 `failed`，**或**最近一条执行是 `failed`。
 *
 * 后半条覆盖「任务已回 `pending` 但上一次是失败」——那时任务态上看不出
 * 任何异常，而用户最需要的恰恰是「上次为什么挂」和那五个动作
 * （`task-model.md:229-232`）。
 */
const showFailure = computed(
  () => task.value?.state === 'failed' || latestExecution.value?.state === 'failed',
)

/**
 * 验收面板出现条件：**只有** `review`。
 *
 * 别的态一律不画（原型 `:163` 写的是「现在还没到这一步」）。画出来再
 * disabled 会让人以为「差一步就能点」，而 `review → done` 的那条边只对
 * `review` 开放，服务端会 409。
 */
const showAcceptance = computed(() => task.value?.state === 'review')

/** 页头状态胶囊的文案。表外的值原样回显（`describeStatus` 同一条口径）。 */
const stateLabel = computed(() => {
  const s = task.value?.state
  if (!s) return ''
  return (STATES as Readonly<Record<string, { label: string }>>)[s]?.label ?? s
})

/** 短 commit：7 位足够人眼比对，完整值放 `title`。 */
function shortCommit(c: string | null): string {
  return c ? c.slice(0, 7) : '—'
}

/** 执行序号（第几次）。列表按时间升序，所以下标 + 1 就是次数。 */
function attemptOf(e: Execution): number {
  const i = orderedExecutions.value.findIndex((x) => x.id === e.id)
  return i < 0 ? 0 : i + 1
}

/**
 * 产出段的起点：执行记录的 `base_commit` 优先，退回任务自己那一个。
 */
const outputBase = computed(
  () => latestExecution.value?.base_commit ?? task.value?.base_commit ?? null,
)

/** 执行记录里的一行时间：`started_at` ~ `ended_at`，缺哪端就写「—」/「进行中」。 */
function spanOf(e: Execution): string {
  const from = e.started_at ? relativeTime(e.started_at, now.value) : '—'
  const to = e.ended_at ? relativeTime(e.ended_at, now.value) : '进行中'
  return `${from} ~ ${to}`
}

/* ---- 写：动作状态 ------------------------------------------------------ */

/** 写失败的文案。**与读失败分开**（见文件头）。 */
const actionFailure = ref('')
/** 「任务刚被更新，已刷新，请重新提交」——409 `task_state_changed` 的专用提示。 */
const conflictNotice = ref('')
/** 正在提交（挡连点：连点两次 = 两个 `expected_version`，第二次必然 409）。 */
const submitting = ref(false)

/**
 * 「放宽约束」的回应。
 *
 * `FailurePanel` 有第 4 个 emit `relax`，而本轮 `/failure` 只有
 * retry / edit / close 三个动作——**没有对应端点**。所以这里不伪造一个
 * 不存在的调用，只写一条提示。
 */
const relaxNotice = ref('')

/* ---- 写：提交 ---------------------------------------------------------- */

/**
 * 任何写动作成功后的收口。
 *
 * 用响应里的 `task` 覆盖本地（不再多一次 GET 去拿任务本身），但
 * `executions` **重新拉取**：重试会让执行记录多一条，不重拉这一页就停在
 * 旧现场上。
 */
async function afterWrite(next: Task): Promise<void> {
  task.value = next
  actionFailure.value = ''
  conflictNotice.value = ''
  try {
    const r = await getTask(taskId.value)
    task.value = r.task
    executions.value = r.executions
  } catch {
    // 读回失败**不进错误态**：任务已经写成功了，把整页换成错误态会让用户
    // 以为刚才那一下没成。执行记录停在刚才那份，下次刷新会对上。
  }
}

/**
 * 写动作的统一错误处理。
 *
 * 409 `task_state_changed`（乐观锁冲突）：**保留用户填的 note**、只重取
 * `task`，并提示重新提交。清空表单的实现会让「用户刚打的一段理由」凭空
 * 消失——那是这条分支最容易犯的错，也是最难被原谅的。
 */
async function handleWriteError(e: unknown): Promise<void> {
  if (e instanceof ApiError && e.code === 'task_state_changed') {
    conflictNotice.value = '任务刚被更新，已刷新，请重新提交'
    actionFailure.value = ''
    // 重取详情一次拿回 `{task, executions}`：比只取 `task` 少一次请求，也不用
    // 新端点。`task` 是必须换的（`version` 变了，不换下一次写还会撞同一个 409）；
    // `executions` 顺手更新——冲突往往正来自「别人刚重派了一次」。
    //
    // **`note` 不受影响**：它存在 `AcceptancePanel` / `FailurePanel` 的内部
    // state 里，本页一个字段都没读也没清。这正是这条分支的要点：用户刚打的一段
    // 理由不能因为别人先动了一下而凭空消失。
    try {
      const fresh = await getTask(taskId.value)
      task.value = fresh.task
      executions.value = fresh.executions
    } catch {
      // 连重取都失败：提示已经写出来了，用户知道要继续得先刷新这一页。
    }
    return
  }
  conflictNotice.value = ''
  if (e instanceof ApiError) {
    if (e.code === 'acceptance_note_required') {
      actionFailure.value = '判定码没有全绿时必须写明理由（服务端拒绝了这次提交）。'
    } else if (e.code === 'illegal_task_transition') {
      actionFailure.value = '这个动作在当前状态下不允许（服务端拒绝了这次提交）。'
    } else {
      actionFailure.value = `提交失败（${e.code}）。`
    }
    return
  }
  actionFailure.value = '提交失败：Hub 连不上。'
}

/** 所有写动作共用的外壳：挡连点、清旧提示、成功收口、失败分支。 */
async function runWrite(call: () => Promise<Task>): Promise<void> {
  if (submitting.value) return
  submitting.value = true
  actionFailure.value = ''
  conflictNotice.value = ''
  relaxNotice.value = ''
  try {
    await afterWrite(await call())
  } catch (e) {
    await handleWriteError(e)
  } finally {
    submitting.value = false
  }
}

/** 乐观锁前置条件。`task` 为 null 时（理论上走不到）不带它。 */
function expectedVersion(): number | undefined {
  return task.value?.version
}

/* ---- 接线（规格 §8「接口设计」的接线表，逐条对）------------------------ */

/** `@retry` → `{action:'retry'}`（换设备重派：同一个任务、同一个判定条件）。 */
function onRetry(): void {
  void runWrite(() =>
    submitFailure(taskId.value, { action: 'retry', expected_version: expectedVersion() }),
  )
}

/** `@edit(reason)` → `{action:'edit', reason}`（改任务再跑，旧值留痕）。 */
function onEdit(reason: string): void {
  void runWrite(() =>
    submitFailure(taskId.value, {
      action: 'edit',
      reason,
      expected_version: expectedVersion(),
    }),
  )
}

/** `@close(reason)` → `{action:'close', reason}`（组件已保证 reason 非空）。 */
function onClose(reason: string): void {
  void runWrite(() =>
    submitFailure(taskId.value, {
      action: 'close',
      reason,
      expected_version: expectedVersion(),
    }),
  )
}

/**
 * `@relax` —— **本轮没有端点**。
 *
 * `FailurePanel` 的「放宽约束」发的是 `relax`，而 `/api/tasks/{id}/failure`
 * 只认 retry / edit / close。所以这里不调任何 API，只写一条提示：编一个
 * `{action:'relax'}` 发出去只会得到 400/409，用户看到的是一句错误码，
 * 还得自己去猜「是不是我点错了」。
 */
function onRelax(): void {
  relaxNotice.value = '放宽约束入口在 v0.8.2（本轮无对应端点）'
}

/**
 * `@pass(note)` → `{verdict:'pass', note, override: note非空}`。
 *
 * `override` 只在**写了理由**时才为真：本轮没有验收执行器，判定码全是
 * 「未判定」，组件因此每次都要二次确认 + note；`override: true` 就是告诉
 * 后端「我知道判定没过，仍要合并」。
 */
function onPass(note: string): void {
  void runWrite(() =>
    submitAcceptance(taskId.value, {
      verdict: 'pass',
      note,
      override: note.trim().length > 0,
      expected_version: expectedVersion(),
    }),
  )
}

/** `@reject(note)` → `{verdict:'reject', note}`（打回 → 回 `pending`，不是失败）。 */
function onReject(note: string): void {
  void runWrite(() =>
    submitAcceptance(taskId.value, {
      verdict: 'reject',
      note,
      expected_version: expectedVersion(),
    }),
  )
}

/**
 * `@keep-branch(note)` → `{verdict:'reject', note: '只保留分支：' + note}`。
 *
 * **为什么映射成打回**（这条理由写在模板里那个 callout 上，不能只待在里程碑
 * 文档里）：本轮没有分支存储，也没有合入执行器（产出段只显示分支名）。做成
 * 「打回 + reason」是唯一不撒谎的实现——状态回 `pending`，用户能在失败 /
 * 约束区看到「只保留分支」的记录。真正的分支保留逻辑归 v0.8.2 的执行间接力。
 */
function onKeepBranch(note: string): void {
  const reason = note.trim().length > 0 ? `只保留分支：${note}` : '只保留分支'
  void runWrite(() =>
    submitAcceptance(taskId.value, {
      verdict: 'reject',
      note: reason,
      expected_version: expectedVersion(),
    }),
  )
}

/** 返回看板。必须 `encodeURIComponent`（理由见 `BoardPage.openTask`：id 含 `/` 会把路径切多一段）。 */
function backToBoard(): void {
  navigate(`/project/${encodeURIComponent(String(route.value.params.id ?? ''))}/board`)
}
</script>

<template>
  <!-- 页头是 `.af-body` 的**兄弟**，不是它的子节点：原型 `task.html:20` 的
       `header.af-head` 排在 `:25` 的 `div.af-body` 之前，8 个原型页无一例外。
       嵌进去的后果是 `.af-body--pad` 的内边距把页头挤成内缩，且 `.af-body` 是
       `overflow:auto` 那一层，滚动时页头会跟着滚走。
       `af-taskd` 留在内容层上——它是这一页的样式命名空间，不是骨架。 -->
  <PageHead v-if="task">
    <div>
      <h1 class="af-head__title" :data-id="task.id">
        {{ task.title }}
        <StateBadge :state="task.state" />
        <span
          v-if="task.needs_manual_acceptance"
          class="af-taskd__tag"
          data-tag="manual"
          title="这个任务没有判定码，跑完不会自动判定，必须由人看"
        >
          需人工验收
        </span>
      </h1>
      <p class="af-head__sub">
        {{ stateLabel }} · 第 {{ task.version }} 版 · 更新于 {{ relativeTime(task.updated_at, now) }}
      </p>
    </div>
    <template #actions>
      <button type="button" class="af-btn af-btn--ghost" @click="backToBoard">← 看板</button>
    </template>
  </PageHead>

  <div class="af-body af-taskd">
    <!-- 参数缺失：直接说缺什么，**一个请求都不发**（发了会打到 `/api/tasks/`）。 -->
    <EmptyState
      v-if="missingId"
      glyph="⚠"
      title="参数错误"
      text="这个地址缺少任务 id，所以没有任何东西可以打开。"
    >
      <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
    </EmptyState>

    <!-- 404：「知道了，没有这个任务」。与「读失败」分开——重试一千次还是 404。 -->
    <EmptyState
      v-else-if="gone"
      glyph="▣"
      title="任务不存在或已删除"
      :text="errorText"
    >
      <button type="button" class="af-btn" @click="backToBoard">返回看板</button>
    </EmptyState>

    <!-- 读失败：给一条重试的路 + 一条回去的路。**不画半份数据**。 -->
    <EmptyState
      v-else-if="loadState === 'error'"
      glyph="◌"
      title="读不出这个任务"
      :text="errorText"
    >
      <button type="button" class="af-btn" @click="load">重新读取</button>
      <button type="button" class="af-btn" @click="backToBoard">返回看板</button>
    </EmptyState>

    <p v-else-if="loadState === 'loading'" class="af-muted">正在读取任务…</p>

    <template v-else-if="task">
      <!-- 写失败：**留在本页**（见文件头「写失败与读失败分开」）。 -->
      <p v-if="actionFailure" class="af-form__error" role="alert" data-alert="write">
        {{ actionFailure }}
      </p>
      <p v-if="conflictNotice" class="af-callout af-callout--warn" role="alert" data-alert="conflict">
        <span class="af-callout__k">任务已被别人改过</span>
        <span>{{ conflictNotice }}</span>
      </p>
      <p v-if="relaxNotice" class="af-muted" role="status" data-alert="relax">{{ relaxNotice }}</p>

      <div class="af-taskd__grid">
        <!-- 左主栏 -->
        <div class="af-taskd__col">
          <!-- ========== ① 这个任务是怎么来的（prototype/task.html:31-88） ========== -->
          <section class="af-region" data-section="origin" aria-labelledby="af-taskd-origin">
            <div class="af-region__head">
              <h2 id="af-taskd-origin" class="af-region__title">这个任务是怎么来的</h2>
              <span class="af-region__note">因果链：从一句话到一次执行</span>
            </div>
            <div class="af-region__body">
              <dl class="af-kv af-taskd__kv">
                <dt class="af-kv__k">谁建的</dt>
                <dd class="af-kv__v" data-field="created_by">
                  {{ task.created_by === 'user' ? '你（手动新建）' : task.created_by }}
                </dd>

                <dt class="af-kv__k">来源</dt>
                <!-- `intent_ref` 为 null → **明确写「人工创建」**，不编一个来源。
                     库里没有这条链的细节可讲，编一个 id 形状的假来源最坏。 -->
                <dd class="af-kv__v" data-field="intent_ref">
                  <template v-if="task.intent_ref">
                    <span class="af-fp">{{ task.intent_ref }}</span>
                    <span class="af-t-mute">（从这句话拆出来的）</span>
                  </template>
                  <span v-else data-intent="manual">人工创建：没有对应的意图记录，是有人在看板上直接建的</span>
                </dd>

                <dt class="af-kv__k">起点</dt>
                <dd class="af-kv__v" data-field="base_commit">
                  <span v-if="task.base_commit" class="af-fp" :title="task.base_commit">
                    {{ shortCommit(task.base_commit) }}
                  </span>
                  <span v-else class="af-t-mute">没有指定起点：执行时用设备上当前的分支头</span>
                </dd>

                <dt class="af-kv__k">建于</dt>
                <dd class="af-kv__v" data-field="created_at">{{ relativeTime(task.created_at, now) }}</dd>
              </dl>
            </div>
          </section>

          <!-- ========== ② 执行记录（prototype/task.html:90-123） ========== -->
          <section class="af-region" data-section="executions" aria-labelledby="af-taskd-executions">
            <div class="af-region__head">
              <h2 id="af-taskd-executions" class="af-region__title">执行记录</h2>
              <span class="af-region__note">
                同一个任务可以跑多次，每次都留现场，不覆盖上一次
              </span>
            </div>
            <div class="af-region__body">
              <!-- 空态是一句话，不是一张空表：空表头会让人以为「记录丢了」。 -->
              <EmptyState
                v-if="orderedExecutions.length === 0"
                glyph="▢"
                title="还没有执行过"
                text="这个任务还停在待办。派活之后每次执行都会在这里留一行。"
              />
              <table v-else class="af-table af-taskd__table">
                <thead>
                  <tr>
                    <th>次数</th>
                    <th>状态</th>
                    <th>起点</th>
                    <th>失败原因</th>
                    <th>起止</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="e in orderedExecutions" :key="e.id" :data-execution="e.id">
                    <td>第 {{ attemptOf(e) }} 次</td>
                    <td><code class="af-fp" data-field="execution-state">{{ e.state }}</code></td>
                    <td>
                      <span v-if="e.base_commit" class="af-fp" :title="e.base_commit">
                        {{ shortCommit(e.base_commit) }}
                      </span>
                      <span v-else class="af-t-mute">—</span>
                    </td>
                    <td>
                      <span v-if="e.failure_kind" class="af-t-mute" data-field="failure-kind">
                        {{ e.failure_kind }}
                      </span>
                      <span v-else class="af-t-mute">—</span>
                    </td>
                    <td class="af-t-mute">{{ spanOf(e) }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
          </section>

          <!-- ========== ③ 失败面板（prototype/task.html:126-154） ==========
               `failed` 任务，**或**最近一条执行失败时显示。组件契约与例外（含
               「看执行复盘」disabled）由子任务 4 定义，这里只接线。 -->
          <FailurePanel
            v-if="showFailure"
            :task="task"
            :execution="latestExecution"
            :now="now"
            @retry="onRetry"
            @edit="onEdit"
            @relax="onRelax"
            @close="onClose"
          />

          <!-- ========== ④ 验收面板（prototype/task.html:157-181，只在 `review`） ========== -->
          <AcceptancePanel
            v-if="showAcceptance"
            :task="task"
            @pass="onPass"
            @reject="onReject"
            @keep-branch="onKeepBranch"
          />

          <!-- 「只保留分支」为什么发的是打回：**写在这里**，不是只在里程碑文档里
               （规格 §8 `:1191-1194`）。用户不该为了知道这件事去 hover 一个按钮。 -->
          <div v-if="showAcceptance" class="af-callout af-callout--info" data-note="keep-branch">
            <span class="af-callout__k">「只保留分支」本轮 = 打回 + 记原因</span>
            <span class="af-taskd__xs">
              本轮没有分支存储，也没有合入执行器（产出段只显示预期的分支名），所以这个动作会
              以 <b>打回</b> 提交、note 前缀「只保留分支：」——状态回待办，记录留在执行现场。
              真正的分支保留逻辑归 <b>v0.8.2</b> 的执行间接力。
            </span>
          </div>

          <!-- ========== ⑤ 任务约束（prototype/task.html:186-220） ========== -->
          <section class="af-region" data-section="constraints" aria-labelledby="af-taskd-constraints">
            <div class="af-region__head">
              <h2 id="af-taskd-constraints" class="af-region__title">任务约束</h2>
              <span class="af-region__note">派活时定的，执行时必须满足</span>
            </div>
            <div class="af-region__body">
              <!-- 空值写「未限定…」而不是留白：留白会被读成「这一项读不出来」，
                   而真相是「这一项不限」。五句文案是规格定死的。 -->
              <dl class="af-kv af-taskd__kv">
                <dt class="af-kv__k">允许设备</dt>
                <dd class="af-kv__v" data-field="allowed_devices">
                  {{ task.constraints.allowed_devices || '未限定设备' }}
                </dd>
                <dt class="af-kv__k">允许 Agent</dt>
                <dd class="af-kv__v" data-field="allowed_agents">
                  {{ task.constraints.allowed_agents || '未限定 Agent' }}
                </dd>
                <dt class="af-kv__k">必须具备</dt>
                <dd class="af-kv__v" data-field="required_capabilities">
                  {{ task.constraints.required_capabilities || '未限定能力' }}
                </dd>
                <dt class="af-kv__k">重试上限</dt>
                <dd class="af-kv__v" data-field="retry_limit">
                  {{
                    task.constraints.retry_limit === null
                      ? '重试上限用默认（2 次）'
                      : `${task.constraints.retry_limit} 次`
                  }}
                </dd>
                <dt class="af-kv__k">版本</dt>
                <dd class="af-kv__v af-t-num" data-field="version">第 {{ task.version }} 版</dd>
              </dl>
              <p class="af-muted af-taskd__note">
                选机器的顺序：① 任务约束 → ② 项目隔离策略 → ③ 按能力和负载排序。
                任何一层筛空，任务就停在待办并说明原因，不会降级凑一台机器跑。
              </p>
            </div>
          </section>
        </div>

        <!-- 右栏：产出 + 两个占位段 -->
        <div class="af-taskd__col">
          <!-- ========== 产出（分支名 + 合入状态；diff 入口留 v0.8.2） ========== -->
          <section class="af-region" data-section="output" aria-labelledby="af-taskd-output">
            <div class="af-region__head">
              <h2 id="af-taskd-output" class="af-region__title">产出</h2>
              <span class="af-region__note">起点与合入状态；分支名与 diff 视图 v0.8.2 上线</span>
            </div>
            <div class="af-region__body">
              <dl class="af-kv af-taskd__kv">
                <dt class="af-kv__k">分支</dt>
                <dd class="af-kv__v">
                  <!-- **不编分支名**。`task-model.md` §8 明列 `branch` 尚未落库
                       （v0.8.2 的执行间接力才写），拼一个 `afleet/<id>/eN` 出去
                       会让人拿去 `git checkout` 然后失败——一个看上去像真 ref
                       的字符串比一个空格外危险。 -->
                  <span class="af-t-mute" data-field="branch">库里还没有记录分支（v0.8.2 上线）</span>
                </dd>
                <dt class="af-kv__k">起点</dt>
                <dd class="af-kv__v" data-field="output-base">
                  <span v-if="outputBase" class="af-fp" :title="outputBase">{{ shortCommit(outputBase) }}</span>
                  <span v-else class="af-t-mute">—</span>
                </dd>
                <dt class="af-kv__k">合入</dt>
                <dd class="af-kv__v" data-field="merge">未合入</dd>
                <!-- `superseded_by` 非空 = 这个任务已被另一个接续。多一行，
                     因为「这块产出还有人要吗」是用户接下来最该知道的事。 -->
                <template v-if="task.superseded_by">
                  <dt class="af-kv__k">接续</dt>
                  <dd class="af-kv__v" data-field="superseded">
                    已由 <span class="af-fp">{{ task.superseded_by }}</span> 接续
                  </dd>
                </template>
              </dl>
            </div>
          </section>

          <!-- 占位段（本轮不实现）。灰色不可点——**不是** `<a href="#">`：
               一个按下去没反应的链接会把「还没做」伪装成「坏了」。 -->
          <section class="af-region af-taskd__todo" data-section="cost" aria-labelledby="af-taskd-cost">
            <div class="af-region__head">
              <h2 id="af-taskd-cost" class="af-region__title">花的时间和钱</h2>
              <span class="af-region__note">v0.8.2 上线</span>
            </div>
            <div class="af-region__body">
              <p class="af-taskd__placeholder" data-placeholder="cost">
                花的时间和钱 —— v0.8.2 上线
              </p>
            </div>
          </section>

          <section class="af-region af-taskd__todo" data-section="butler" aria-labelledby="af-taskd-butler">
            <div class="af-region__head">
              <h2 id="af-taskd-butler" class="af-region__title">管家的判断</h2>
              <span class="af-region__note">v0.9.0 上线</span>
            </div>
            <div class="af-region__body">
              <p class="af-taskd__placeholder" data-placeholder="butler">
                管家的判断 —— v0.9.0 上线
              </p>
            </div>
          </section>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
/* 两栏布局。**本页不碰 `components.css`**（规格 §8 的自查修正：面板与共享
   样式归子任务 4），所以这里只做这一页的排法，命名带 `af-taskd__` 前缀、
   作用域限于本页（与 `ProjectSettingsPage` 的 `af-pset__*` 同一做法）。
   窄屏下塌成一栏：两栏在 1000px 以下都会窄到放不下一行 note。 */
.af-taskd__grid {
  display: grid;
  grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
  gap: var(--af-space-5);
  align-items: start;
}
@media (max-width: 1000px) {
  .af-taskd__grid {
    grid-template-columns: minmax(0, 1fr);
  }
}
.af-taskd__col {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-5);
  min-width: 0;
}

/* 键值区：键列固定窄宽，值列吃掉剩下的。字号与面板内那套同档，所以直接
   借用共享的 `.af-kv__k` / `.af-kv__v`（颜色与字号都在那边定义）。 */
.af-taskd__kv {
  display: grid;
  grid-template-columns: 84px 1fr;
  gap: var(--af-space-2) var(--af-space-4);
  margin: 0;
  font-size: var(--af-text-sm);
}

/* 页头的「需人工验收」标签。与卡片上那枚同一个语义，所以同一套颜色
   （警告档）——这一页没有 `.af-taskcard__tag` 可用，但含义必须一致。 */
.af-taskd__tag {
  display: inline-flex;
  align-items: center;
  vertical-align: middle;
  font-size: var(--af-text-2xs);
  line-height: var(--af-leading-snug);
  padding: 0 var(--af-space-2);
  border-radius: var(--af-radius-full);
  border: 1px solid var(--af-warn-line);
  color: var(--af-warn-fg);
  background: var(--af-warn-bg);
  white-space: nowrap;
}

.af-taskd__table {
  font-size: var(--af-text-sm);
}
.af-taskd__note {
  margin: var(--af-space-3) 0 0;
}
/* 说明小字：「只保留分支」那条 callout 与执行记录的空态说明。 */
.af-taskd__xs {
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-base);
  color: var(--af-fg-secondary);
}

/* 占位段：整块压暗，让它看起来就是「还没做」而不是「读不出来」。
   不给 hover / cursor，因为它不可点。 */
.af-taskd__todo {
  opacity: 0.6;
}
.af-taskd__placeholder {
  margin: 0;
  font-size: var(--af-text-sm);
  color: var(--af-fg-disabled);
}
</style>
