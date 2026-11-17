<script setup lang="ts">
/* ===========================================================================
   OverviewPage —— 总览（`prototype/index.html`，v0.7.0 子任务 8；v0.8.1 子任务 6 重排）
   ---------------------------------------------------------------------------
   这是**全站入口**（`router.ts` 的 `INDEX_PATH = '/'`）。v0.7.0 之前根路径落在
   会话列表，那时会话是顶层；项目主轴改版后顶层是项目，所以第一屏是项目。

   ── 骨架：页头是 `.af-body` 的兄弟 ──────────────────────────────────
   原型 `index.html:18` 的 `header.af-head` 排在 `:23` 的 `div.af-body` **之前**，
   8 个原型页无一例外。嵌进去的后果是 `.af-body` 的滚动把页头一起带走。所以
   这里 `<PageHead>` 在 `.af-body` 外面，两块都由外壳 `.af-main`
   （`display:flex; flex-direction:column`）竖排。
   页头与内容层之间**不再包 `main.af-main`**：外壳 `App.vue` 已经渲染了那个
   唯一的 `<main tabindex="-1">`（`focusMain()` 的落点），再包一层就是嵌套 `<main>`。

   ── 四行摘要：两行真数据 + 两行显式空态（用户裁定） ─────────────────
   原型每个项目块里有四行：上次做到哪 / 正在跑 / 等你决定 / 最近总结。
     · 「正在跑」「等你决定」读 `GET /api/projects/{id}/tasks`，是**真数据**；
     · 「上次做到哪」要执行记录的时间线、「最近总结」要管家的总结，两者分别
       归 v0.8.2（`POST /api/tasks/{id}/executions`）与 v0.9.0。这两行**写明确
       的空态并说明归属**，不画假句子——一个写着「2 小时前合入了…」的假摘要
       在验收时会被当成真数据。

   ── 「正在跑」只说条数，不说设备名 ─────────────────────────────────
   原型 `index.html:42` 是「3 个执行 · device-a / device-b / device-c」。实现
   拿不到设备名：`ExecutionView`（`crates/hub-server/src/routes/tasks.rs:144`）
   九个字段里没有 `device_id`，它只在库里有 `binding_id`。曾经前端 `Execution`
   类型里写了一个可选 `device_id`，单测喂夹具所以一直是绿的，对着活 Hub 才
   发现那个字段根本不发（2026-11-17）。**不编一个「device-a」充当它**——真名
   与假名在页面上长得一样，验收只会记住「这一行有设备名」。

   要把设备名补上，改的是后端契约（`ExecutionView` 带 `device_id`），
   已记入 `docs/BUGS.md`。

   ── 一处与原型的有意偏离（两个动作按钮） ────────────────────────────
   原型 `index.html:55-58` 是「看板 / 执行监控」。执行监控页
   （`ExecutionPage.vue`）现在是骨架页，且要看执行必须带 `execId`——从总览
   直接点过去只能是死链。所以第二个动作换成「设置」，两个都指向真实页面；
   等执行监控落地后换回来（记在里程碑 Bugs 表）。
   =========================================================================== */

import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import PageHead from '@/domains/shared/components/PageHead.vue'
import {
  listProjectBindings,
  listProjects,
  type Project,
} from '@/domains/project/api'
import { listProjectTasks, type Task } from '@/domains/project/taskApi'
import { describeReadFailure } from '@/shared/http'
import { handleLayerKeydown, pop, push } from '@/shared/focus'
import { SHAPE_LABELS } from '@/shared/projectShape'
import { navigate } from '@/router'

const projects = ref<Project[]>([])
const loading = ref(true)
const failure = ref('')

/** 形态的中文名。**与 `PRODUCT.md` §3.2 的表格用词一致**，不另造说法。 */
const SHAPE_LABEL = SHAPE_LABELS

const hasProjects = computed(() => projects.value.length > 0)

/* ===========================================================================
   每个项目那四行摘要的取数
   ---------------------------------------------------------------------------
   两个请求合成一次「这个项目的摘要」：任务列表（正在跑 / 等你决定）与绑定
   列表（几台已绑定）。**任一失败都不写 0**——0 是一个真实的数量（真的没有
   在跑的执行），把它当成失败的兜底会让「读不到」伪装成「没有」。
   =========================================================================== */

interface ProjectSummary {
  /** `running` 的任务数。 */
  running: number
  /** `failed` 的任务数——「失败待处置」。 */
  failed: number
  /** `review` 的任务数——「待验收」。 */
  review: number
  /** 绑了几台设备。`null` = 读不到（**不是 0**）。 */
  bindings: number | null
  /** 非空 = 这个项目的摘要读不到，值是能读的人话。 */
  failure: string
}

const summaries = ref<Record<string, ProjectSummary>>({})

function summaryOf(id: string): ProjectSummary | undefined {
  return summaries.value[id]
}

function summarize(tasks: Task[], bindings: number): ProjectSummary {
  const runningTasks = tasks.filter((t) => t.state === 'running')
  // 执行条数按**执行**数，不按任务数：一个任务重试过一次就有两条执行，
  // 说「1 个执行进行中」而实际是 2 条，会让「等你决定」的计数逻辑看起来自洽、
  // 实际对不上（失败重派是真会发生的路径）。
  const runningExecutions = runningTasks.reduce(
    (n, t) => n + (t.executions ?? []).filter((e) => e.state === 'running').length,
    0,
  )
  return {
    // 有执行记录时按执行数算；列表端点对没跑过的任务给空数组，那时退化成任务数。
    running: runningExecutions > 0 ? runningExecutions : runningTasks.length,
    failed: tasks.filter((t) => t.state === 'failed').length,
    review: tasks.filter((t) => t.state === 'review').length,
    bindings,
    failure: '',
  }
}

async function loadSummaries(list: Project[]): Promise<void> {
  const rows = await Promise.all(
    list.map(async (p): Promise<readonly [string, ProjectSummary]> => {
      try {
        const [tasks, bindings] = await Promise.all([
          listProjectTasks(p.id),
          listProjectBindings(p.id),
        ])
        return [p.id, summarize(tasks, bindings.length)] as const
      } catch (e) {
        return [
          p.id,
          {
            running: 0,
            failed: 0,
            review: 0,
            bindings: null,
            failure: describeReadFailure(e, '这个项目的任务与绑定'),
          },
        ] as const
      }
    }),
  )
  summaries.value = Object.fromEntries(rows)
}

/** 区域头右侧那句：绑定数是真数据；读不到就直说，不写「0 台已绑定」。 */
function boundNote(p: Project): string {
  const s = summaryOf(p.id)
  if (!s) return '正在读取绑定…'
  if (s.bindings === null) return '绑定数读不到'
  const tail = p.shape === 'isolated' ? ' · 代码不出设备' : ''
  return `${s.bindings} 台已绑定${tail}`
}

/**
 * 形态名旁边那句「代码从哪来」（悬停可见）。
 *
 * 原型 `index.html:36-40` 的区域头里只有形 `.af-tag`（「中心式」三个字），
 * 对第一次见到的人没有区别——但原型只给**真正需要**的那一款补一句说明：
 * `:83` 的隔离式项目在 `.af-region__note` 里写「1 台已绑定 · 代码不出设备」。
 * 这里照这个取舍走：隔离式在 note 里明说（`boundNote` 的 tail），另外两个
 * 形态挂 `title`，不占版面。
 */
const SHAPE_HINT: Readonly<Record<string, string>> = {
  central: '代码在 Hub 的仓库里',
  mirror: '镜像上游仓库到 Hub',
  isolated: '代码只在设备上，永不上传',
}

/**
 * 区域头那个小色点：原型 `index.html:39/64/87` 是 `span.af-dot.dot-a / dot-b / dot-c`
 * ——三个色槽**轮着用**，不承载信息（`aria-hidden`，`:347-349` 各取一个固定色）。
 *
 * 为什么不改成「按项目主题色取色」（上一版的写法）：原型没有给五个色名各留
 * 一个色槽，硬折过去只能就近借 `--p-done` / `--p-review` 之类的语义色——那样
 * 一个**装饰**点会随语义色漂移，还会让人误以为「绿色点 = 这个项目成功」。
 * 项目主题色在项目设置页与归档页都有文字，不靠这个点表达。
 */
const DOT_CLASS = ['dot-a', 'dot-b', 'dot-c'] as const

function dotClass(index: number): string {
  return DOT_CLASS[index % DOT_CLASS.length]
}


/* ===========================================================================
   管理面三行（原型 `index.html:137-151`）
   ---------------------------------------------------------------------------
   三行的「回答什么问题」照原型逐字搬：那一列是「点进去会得到什么」的说明，
   措辞一变，用户对「设备页」与「Agent 页」的分工判断就变了。

   路由写死在这里而不是给每行存一个 href：这三条是本页唯一的对外出口，
   集中一处才看得出「管理面一共只有三页」。
   =========================================================================== */
const MANAGE_ENTRIES = [
  {
    name: '设备',
    question:
      '这台机器在线吗、装了什么 Agent、磁盘还剩多少、绑定了哪些项目、失败集中在哪一类',
    to: '/devices',
  },
  {
    name: 'Agent',
    question: '装了哪些、哪些不可用、启动配置是什么、有没有漂移、跑过哪些任务',
    to: '/agents',
  },
  {
    name: '设置',
    question: '项目形态与合并权、磁盘清理策略、通知出口',
    to: '/settings',
  },
] as const

/* ===========================================================================
   管家抽屉：只做壳
   ---------------------------------------------------------------------------
   原型右下角一个悬浮按钮 + 右侧滑出的 `.af-drawer`（`index.html:161-199`）。
   **这一版面板里没有任何业务逻辑**：不发请求、不建建议、不派任务，只有一句
   「归 v0.9.0」的说明。画一个「今天三件事」的假摘要成本最低、也最容易被当成
   真数据带进验收，所以不画。

   开合用组件里的布尔值而不是原型的 `:target`：`:target` 把状态塞进 URL 片段，
   刷新会误开、浏览器历史里会多出一堆条目。

   悬浮按钮**始终在**（原型用两个 `<a>` 互斥显示），靠 `aria-expanded` 与文字
   表达状态——藏掉再显示会让键盘焦点无处可回。
   =========================================================================== */
const butlerOpen = ref(false)
const drawerEl = ref<HTMLElement | null>(null)
const fabEl = ref<HTMLButtonElement | null>(null)
let pushed = false

/**
 * 开合走**全站的焦点栈**（`shared/focus.ts`），不是自己写一套。
 *
 * 曾经这里只有 `toggleButler` 里的一句 `closeEl.value?.focus()` 加模板上的
 * `@keydown.esc`。两个后果都是实测出来的（2026-11-17，playwright 对着活 Hub）：
 *
 *   ① **Esc 关不掉**——键位挂在 `<aside>` 上，而焦点在「关闭」按钮里，事件确实
 *      会冒到 aside……但只要焦点不在抽屉内（例如用户点了一下页面），一次 Esc
 *      什么也不发生。判据放错地方，只在「刚好 Tab 到里面」时有效。
 *   ② **背后没有 inert**——抽屉开着时 Tab 仍能走进整页，读屏也照样读得到
 *      后面那些区域，那不是一个覆盖层该有的行为。
 *
 * `push` 一次解决两件事：背后 inert + 首焦点；`pop` 负责还焦点给 fab。
 * `handleLayerKeydown` 只在**本层是栈顶**时才响应 Esc，所以将来在抽屉上再盖
 * 一层（确认框之类）时，一次 Esc 不会把两层一起关掉。
 */
function openButler(): void {
  butlerOpen.value = true
  void nextTick(() => {
    const el = drawerEl.value
    if (!el || pushed) return
    push(el, { opener: fabEl.value })
    pushed = true
    document.addEventListener('keydown', onKeydown)
  })
}

function closeButler(): void {
  if (!butlerOpen.value) return
  butlerOpen.value = false
  const el = drawerEl.value
  if (pushed && el) {
    pushed = false
    document.removeEventListener('keydown', onKeydown)
    pop(el)
  }
}

function toggleButler(): void {
  if (butlerOpen.value) closeButler()
  else openButler()
}

function onKeydown(e: KeyboardEvent): void {
  const el = drawerEl.value
  if (!el) return
  handleLayerKeydown(e, el, closeButler)
}

onBeforeUnmount(() => {
  // 组件卸载时监听得摘掉：留着会让下一个页面的 Esc 被这个已卸载的抽屉接走。
  document.removeEventListener('keydown', onKeydown)
  if (pushed && drawerEl.value) {
    pushed = false
    pop(drawerEl.value)
  }
})

async function load(): Promise<void> {
  loading.value = true
  failure.value = ''
  summaries.value = {}
  try {
    projects.value = await listProjects()
  } catch (e) {
    failure.value = describeReadFailure(e, '项目')
    loading.value = false
    return
  }
  loading.value = false
  // 摘要单独一趟：项目列表先出来，四行慢慢填。**不 `await`** 在这里，
  // 否则一页要等 2N 个请求才显示第一个字。
  void loadSummaries(projects.value)
}

onMounted(load)
</script>

<template>
  <PageHead>
    <div>
      <h1 class="af-head__title">总览</h1>
      <span class="af-head__sub">今天要处理的事，按项目分</span>
    </div>
    <template #actions>
      <button type="button" class="af-btn af-btn--ghost" @click="navigate('/projects/archived')">
        已归档
      </button>
      <button
        type="button"
        class="af-btn af-btn--primary"
        @click="navigate('/projects/new')"
      >
        新建项目
      </button>
    </template>
  </PageHead>

  <div class="af-body">
    <div class="af-ov__wrap af-stack">
      <!-- 原型 `:26-32` 的提示条。**这一句解释的是页面结构本身**：为什么第一屏
           是项目而不是设备清单。没有它的话，「管理面」被挪到页面最下面这件事
           看起来像是排错了顺序。 -->
      <div class="af-callout af-callout--info">
        <span class="af-callout__k">这一页为什么是「按项目分」</span>
        <span>
          AgentFleet 的主轴是项目。设备是拿来干活的，不是拿来日常浏览的。
          所以这里先给你「哪个项目卡住了」，设备健康退到管理面里去。
        </span>
      </div>

      <p v-if="loading" class="af-muted">正在读取项目…</p>

      <EmptyState
        v-else-if="failure"
        glyph="⚠"
        title="读取项目失败"
        :text="failure"
      >
        <button type="button" class="af-btn" @click="load">重试</button>
      </EmptyState>

      <EmptyState
        v-else-if="!hasProjects"
        glyph="◈"
        title="还没有项目"
        text="项目是这里的起点：每个项目有自己的仓库、自己的看板，任务挂在项目下面。先建一个。"
      >
        <button
          type="button"
          class="af-btn af-btn--primary"
          @click="navigate('/projects/new')"
        >
          新建项目
        </button>
      </EmptyState>

      <!-- 原型 `:34-108`：每个项目一块 `.af-region`，块内四行摘要 + 两个动作。
           网格是 `app.css:363` 的 `.af-groups`（320px 下限自适应列宽）。 -->
      <div v-else class="af-groups">
        <section
          v-for="(p, i) in projects"
          :key="p.id"
          class="af-region"
          :data-project="p.id"
        >
          <div class="af-region__head">
            <span class="af-dot" :class="dotClass(i)" aria-hidden="true"></span>
            <h2 class="af-region__title">
              <a
                :href="`/project/${encodeURIComponent(p.id)}/board`"
                @click.prevent="navigate(`/project/${encodeURIComponent(p.id)}/board`)"
              >{{ p.name }}</a>
            </h2>
            <span class="af-tag">{{ SHAPE_LABEL[p.shape] ?? p.shape }}</span>
            <!-- 「代码从哪来」为什么必须写在明处而不是放进 `title`：形态名三个
                 字对第一次见到的人没有区别，而「代码在 Hub」与「代码只在设备上」
                 是这个产品最要紧的一条边界（PRODUCT §3.2）。悬停才看得见等于
                 只有鼠标用户看得见。原型只给隔离式补了这句（`:83` 的 note），
                 实现把三种都写上——它们各占一行字宽，头部放得下。 -->
            <span class="af-2xs af-mute">{{ SHAPE_HINT[p.shape] ?? '' }}</span>
            <span class="af-spacer"></span>
            <span class="af-region__note">{{ boundNote(p) }}</span>
          </div>

          <div class="af-region__body af-stack">
            <!-- ① 上次做到哪：要执行记录的时间线，归 v0.8.2。 -->
            <div class="af-row">
              <span class="af-2xs af-mute af-ov__k">上次做到哪</span>
              <span class="af-xs af-dim">执行记录归 v0.8.2，这里还没有时间线可读</span>
            </div>

            <!-- ② 正在跑：真数据。读不到时说读不到，不写「没有在跑的执行」。 -->
            <div class="af-row" data-row="running">
              <span class="af-2xs af-mute af-ov__k">正在跑</span>
              <span v-if="!summaryOf(p.id)" class="af-xs af-mute">正在读取执行状态…</span>
              <span v-else-if="summaryOf(p.id)?.failure" class="af-xs af-dim">
                {{ summaryOf(p.id)?.failure }}
              </span>
              <span
                v-else-if="(summaryOf(p.id)?.running ?? 0) > 0"
                class="af-xs"
              >
                {{ summaryOf(p.id)?.running }} 个执行进行中
              </span>
              <span v-else class="af-xs af-dim">没有在跑的执行</span>
            </div>

            <!-- ③ 等你决定：真数据。失败待处置与待验收是两件事，分开数。 -->
            <div class="af-row" data-row="decide">
              <span class="af-2xs af-mute af-ov__k">等你决定</span>
              <span v-if="!summaryOf(p.id)" class="af-xs af-mute">正在读取执行状态…</span>
              <span v-else-if="summaryOf(p.id)?.failure" class="af-xs af-dim">
                读不到，先不猜
              </span>
              <span
                v-else-if="
                  (summaryOf(p.id)?.failed ?? 0) > 0 || (summaryOf(p.id)?.review ?? 0) > 0
                "
                class="af-xs af-ov__chips"
              >
                <span
                  v-if="(summaryOf(p.id)?.failed ?? 0) > 0"
                  class="af-tag af-tag--failed"
                >{{ summaryOf(p.id)?.failed }} 个失败待处置</span>
                <span
                  v-if="(summaryOf(p.id)?.review ?? 0) > 0"
                  class="af-tag af-tag--review"
                >{{ summaryOf(p.id)?.review }} 张待验收</span>
              </span>
              <span v-else class="af-xs af-dim">没有等你的事</span>
            </div>

            <!-- ④ 最近总结：管家总结归 v0.9.0。 -->
            <div class="af-row">
              <span class="af-2xs af-mute af-ov__k">最近总结</span>
              <span class="af-xs af-dim">管家总结归 v0.9.0，这一行现在还空着</span>
            </div>

            <!-- 第二个动作是「设置」而不是原型的「执行监控」：后者要 execId，
                 且执行监控页还是骨架页，点过去只能是死链（见文件头）。 -->
            <div class="af-row af-row--tight">
              <a
                class="af-btn af-btn--sm"
                :href="`/project/${encodeURIComponent(p.id)}/board`"
                @click.prevent="navigate(`/project/${encodeURIComponent(p.id)}/board`)"
              >看板</a>
              <a
                class="af-btn af-btn--sm"
                :href="`/project/${encodeURIComponent(p.id)}/settings`"
                @click.prevent="navigate(`/project/${encodeURIComponent(p.id)}/settings`)"
              >设置</a>
            </div>
          </div>
        </section>
      </div>

      <!-- 原型 `:112-123`：已归档默认折叠。**「归档不是删除、随时能取消」这句话
           必须在这一页出现**——它出现在详情页太晚：用户在第一屏做「要不要归档」
           的判断时就需要知道它可逆。 -->
      <details class="af-fold">
        <summary class="af-fold__sum">
          已归档的项目
          <span class="af-2xs af-mute">归档不是删除，历史都留着，随时能取消归档</span>
        </summary>
        <div class="af-fold__body">
          <div class="af-row">
            <button
              type="button"
              class="af-btn af-btn--sm"
              @click="navigate('/projects/archived')"
            >
              打开归档列表
            </button>
            <span class="af-2xs af-mute">
              归档后不出现在列表里、不派新任务；执行记录、失败原因、管家总结全保留
            </span>
          </div>
        </div>
      </details>

      <!-- 原型 `:126-155`：管理面三行表。**放在项目列表之后而不是之前**：
           这一页回答「今天做什么」，管理面回答「机器怎么样」——把后者放前面
           会让第一屏变成一份设备清单，正是提示条里说的那种误读。 -->
      <section class="af-region" aria-label="管理面">
        <div class="af-region__head">
          <h2 class="af-region__title">管理面</h2>
          <span class="af-region__note">这些页面回答的是「机器怎么样」，不是「项目怎么样」</span>
        </div>
        <div class="af-region__body af-region__body--flush">
          <table class="af-table">
            <thead>
              <tr>
                <th>页面</th>
                <th>回答什么问题</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="row in MANAGE_ENTRIES" :key="row.to">
                <td><strong>{{ row.name }}</strong></td>
                <td class="af-ov__manage-q">{{ row.question }}</td>
                <td>
                  <button type="button" class="af-ov__link" @click="navigate(row.to)">
                    进入
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>
    </div>
  </div>

  <!-- 管家抽屉壳 + 悬浮按钮（`index.html:161-199`）。`aria-hidden` 跟着状态走：
       面板收起时它在 DOM 里但不可见，读屏与 Tab 都不该进去。

       **必须 Teleport 到 body，且与 fab 同装在一个容器里**，两条理由都是实测的
       （2026-11-17 对着活 Hub）：

       ① `focus.push` 的 `inertOthers` 只把「body 的直接子节点里、不含浮层容器的
          那几个」置 inert。抽屉留在页面模板里时，它所在的子树就是整页——于是
          「背后全部 inert」退化成「一个都没 inert」，抽屉开着照样能 Tab 进背后的
          项目卡片。`App.vue` 的键位帮助面板为同一条理由 Teleport。
       ② fab 单留在页面里时，抽屉一开它就跟着那一整棵子树被 inert 掉：原型里
          这个按钮此刻写着「收起」，而用户点不动它。所以 fab 与抽屉同属一个浮层
          容器，入栈的是这个容器本身。

       这个包装元素**故意不带类名**：它只为了给 `push()` 一个共同的宿主，
       而 `.af-drawer` / `.af-fab` 都是 `position: fixed`（原型 `app.css:532` /
       `:560`，无 transform 祖先），所以这层 div 不参与布局、不改几何。
       给它起个 `af-butler` 之类的名字等于凭空再加一层自造类名——那正是
       v0.8.1 在删的东西（`class-usage` 门禁会当场判红）。 -->
  <Teleport to="body">
    <div>
      <aside
        ref="drawerEl"
        class="af-drawer"
        :class="{ 'af-drawer--open': butlerOpen }"
        :aria-hidden="butlerOpen ? 'false' : 'true'"
        aria-label="管家"
        tabindex="-1"
      >
    <div class="af-drawer__head">
      <div>
        <h2 class="af-drawer__title">管家</h2>
        <div class="af-drawer__sub">总览页的管家只看全局；要对某个项目说话，进那个项目</div>
      </div>
      <button
        type="button"
        class="af-btn af-btn--ghost af-drawer__close"
        aria-label="关闭管家面板"
        @click="closeButler"
      >
        关闭
      </button>
    </div>
    <div class="af-drawer__log">
      <div class="af-msg">
        <span class="af-msg__who">管家</span>
        <span class="af-msg__body">
          管家要读全部项目的执行记录与总结，才说得出「今天先处理什么」。它归
          <b>v0.9.0</b> —— 这一版只有这个抽屉壳：没有建议、没有数字，也不会替你派任务。
        </span>
      </div>
      <div class="af-callout">
        <span class="af-callout__k">管家是绑定到项目的</span>
        <span class="af-2xs af-dim">
          每个项目有自己的管家和自己的记忆。这一页只是汇总，在这里让它干活它会先问你「哪个项目」。
        </span>
      </div>
    </div>
    <div class="af-drawer__compose">
      <input
        class="af-input"
        type="text"
        placeholder="v0.9.0 起能在这里问管家"
        aria-label="对管家说话"
        disabled
      />
      <button class="af-btn af-btn--primary" type="button" disabled>发送</button>
    </div>
      </aside>

      <!-- fab 与抽屉同属这个浮层容器（理由见上面那段的 ②）。它**始终在**
           （原型用两个 `<a>` 互斥显示 `af-fab--off` / `af-fab--on`），靠
           `aria-expanded` 与文字表达状态——藏掉再显示会让键盘焦点无处可回。 -->
      <button
        ref="fabEl"
        class="af-fab"
        type="button"
        aria-haspopup="true"
        :aria-expanded="butlerOpen ? 'true' : 'false'"
        @click="toggleButler"
      >
        <span class="af-fab__mark" aria-hidden="true">管</span>
        <span>{{ butlerOpen ? '收起' : '管家' }}</span>
      </button>
    </div>
  </Teleport>
</template>

<style scoped>
/* 内容层的内边距。原型 `index.html:24` 是内联 `padding:20px 20px 64px`——
   模板里的静态内联样式被门禁禁掉了（prototype-parity 的 style=" 判据），
   所以落成这条 scoped 规则。64px 没有对应 token，取最近的一档 `--p-s8`（56px）。 */
.af-ov__wrap {
  padding: var(--p-s5) var(--p-s5) var(--p-s8);
}

/* 四行摘要左边那个固定 76px 的标签列（原型是内联 `width:76px`）。
   `flex: 0 0 auto` 保证长标签（「上次做到哪」五个字）不被压窄。 */
.af-ov__k {
  width: 76px;
  flex: 0 0 auto;
}

/* 「等你决定」两个胶囊之间留一点缝：`.af-tag` 是 `inline-flex`，
   紧挨着写会粘成一个长条。 */
.af-ov__chips {
  display: inline-flex;
  align-items: center;
  gap: var(--p-s2);
  flex-wrap: wrap;
}

/* 管理面表格 */
.af-ov__manage-q {
  font-size: var(--af-text-sm);
}
/* 表内「进入」是一个动作，不是文档跳转——用 button + accent 下划线，
   与 `.af-agent__link` 同一种「行内可点文字」的观感。 */
.af-ov__link {
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: var(--af-accent);
  cursor: pointer;
  text-decoration: underline;
  text-underline-offset: 2px;
}
</style>
