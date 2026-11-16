<script setup lang="ts">
/* ===========================================================================
   OverviewPage —— 总览（`prototype/index.html`，v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   这是**全站入口**（`router.ts` 的 `INDEX_PATH = '/'`）。v0.7.0 之前根路径落在
   会话列表，那时会话是顶层；项目主轴改版后顶层是项目，所以第一屏是项目。

   ── 这一页现在有什么、没有什么 ────────────────────────────────────
   有：项目列表（真数据，读 `GET /api/projects`）、新建项目入口、已归档入口。
   没有：「待办摘要」那一块。它需要任务的聚合数字，而任务归 v0.8.0。
   所以那一块**显式写成空态并说明归属**，不是留白——留白会被当成加载失败。

   ── 为什么空态要说「还没有项目」而不是「暂无数据」 ────────────────
   「暂无数据」是加载失败时的措辞，用户分不清「真的没有」和「出错了」。
   新建状态下第一件事就是建一个项目，空态必须直接给出那条路（规范 §3.11）。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import { listProjects, type Project } from '@/domains/project/api'
import { describeReadFailure } from '@/shared/http'
import { SHAPE_LABELS } from '@/shared/projectShape'
import { navigate } from '@/router'

const projects = ref<Project[]>([])
const loading = ref(true)
const failure = ref('')

/** 形态的中文名。**与 `PRODUCT.md` §3.2 的表格用词一致**，不另造说法。 */
const SHAPE_LABEL = SHAPE_LABELS

/**
 * 形态的说明句。**必须给一句「代码从哪来、到哪去」**，不能只给名字
 * （`prototype/project-new.html` 的形态卡就是这么写的）：三个名字对第一次
 * 见到的人没有区别，而它们的差别恰恰是产品最容易讲不清的地方。
 */
const SHAPE_HINT: Readonly<Record<string, string>> = {
  central: '代码在 Hub 的仓库里',
  mirror: '镜像上游仓库到 Hub',
  isolated: '代码只在设备上，永不上传',
}

const hasProjects = computed(() => projects.value.length > 0)

/**
 * 管理面三行（原型 `prototype/index.html:137-151`）。
 *
 * **三行的「回答什么问题」照原型逐字搬**：那一列是「点进去会得到什么」的
 * 说明，措辞一变，用户对「设备页」与「Agent 页」的分工判断就变了。
 *
 * 路由写死在这里而不是给每行存一个 href：这三条是本页唯一的对外出口，
 * 集中一处才看得出「管理面一共只有三页」。
 */
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

async function load(): Promise<void> {
  loading.value = true
  failure.value = ''
  try {
    projects.value = await listProjects()
  } catch (e) {
    failure.value = describeReadFailure(e, '项目')
  } finally {
    loading.value = false
  }
}

onMounted(load)
</script>

<template>
  <section class="af-page">
    <header class="af-page__head">
      <h1 class="af-page__title">总览</h1>
      <p class="af-page__sub">今天要处理的事，按项目分</p>
      <div class="af-page__actions">
        <button type="button" class="af-btn" @click="navigate('/projects/archived')">
          已归档
        </button>
        <button
          type="button"
          class="af-btn af-btn--primary"
          @click="navigate('/projects/new')"
        >
          新建项目
        </button>
      </div>
    </header>

    <!-- 原型 `:26-32` 的提示条。**这一句解释的是页面结构本身**：为什么第一屏
         是项目而不是设备清单。没有它的话，「管理面」被挪到页面最下面这件事
         看起来像是排错了顺序。 -->
    <div class="af-callout af-callout--info af-ov__lead">
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

    <ul v-else class="af-projectlist">
      <li v-for="p in projects" :key="p.id" class="af-projectlist__row">
        <button
          type="button"
          class="af-projectlist__link"
          @click="navigate(`/project/${encodeURIComponent(p.id)}/board`)"
        >
          <span class="af-projectlist__name">{{ p.name }}</span>
          <span class="af-projectlist__hint">{{ SHAPE_HINT[p.shape] ?? '' }}</span>
        </button>
        <span class="af-projectlist__shape">{{ SHAPE_LABEL[p.shape] ?? p.shape }}</span>
        <span
          v-if="p.theme_color"
          class="af-projectlist__dot"
          :data-color="p.theme_color"
          :title="p.theme_color"
        >
          {{ p.theme_color }}
        </span>
      </li>
    </ul>

    <!-- 原型 `:34-108`：每个项目一块 `.af-region`，块内是四行摘要（上次做到哪 /
         正在跑 / 等你决定 / 最近总结）。**这四行现在取不到数据**：它们分别要
         任务历史、进行中的执行、待处置数量、管家总结，而任务模型归
         v0.8.0 之后。所以这一块保留「说明归属」的形态，不画四个假数字——
         一个写着「2 小时前合入了…」的假摘要在验收时会被当成真数据。 -->
    <section class="af-page__section af-ov__todo">
      <h2 class="af-page__subtitle">待办</h2>
      <EmptyState
        glyph="▢"
        title="待办摘要要等任务模型落地"
        text="每一个项目的「上次做到哪 / 正在跑 / 等你决定 / 最近总结」四行摘要，要读任务历史与执行状态。任务模型归 v0.8.0，本里程碑只把项目与绑定做起来。"
      />
    </section>

    <!-- 原型 `:112-123`：已归档默认折叠。**「归档不是删除、随时能取消」这句话
         必须在这一页出现**——它出现在详情页太晚：用户在第一屏做「要不要归档」
         的判断时就需要知道它可逆。 -->
    <details class="af-fold af-ov__fold">
      <summary class="af-fold__sum">
        已归档的项目
        <span class="af-t-mute af-ov__fold-note">
          归档不是删除，历史都留着，随时能取消归档
        </span>
      </summary>
      <div class="af-fold__body">
        <button type="button" class="af-btn af-btn--sm" @click="navigate('/projects/archived')">
          打开归档列表
        </button>
        <span class="af-t-mute af-ov__fold-hint">
          归档后不出现在列表里、不派新任务；执行记录、失败原因、管家总结全保留
        </span>
      </div>
    </details>

    <!-- 原型 `:126-155`：管理面三行表。**放在项目列表之后而不是之前**：
         这一页回答「今天做什么」，管理面回答「机器怎么样」——把后者放前面
         会让第一屏变成一份设备清单，正是提示条里说的那种误读。 -->
    <section class="af-region af-ov__manage" aria-label="管理面">
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
  </section>
</template>

<style scoped>
/* 提示条与下方列表的间隔。只服务本页的阅读节奏，不进 `components.css`。 */
.af-ov__lead {
  margin-bottom: var(--af-space-6);
}

/* 已归档折叠块（原型 `.af-fold`，生产样式表里没有这个类）。
   **为什么用 `<details>` 而不是自己做一个开合状态**：浏览器原生支持键盘
   `Enter` / 空格开合、自带展开语义，而自己写的版本要再实现一遍焦点与
   `aria-expanded`——那正是「看起来能跑、实际不可用」的一类缺陷。 */
.af-ov__fold {
  margin-top: var(--af-space-6);
  border: 1px solid var(--af-border-default);
  border-radius: var(--af-radius-md);
  background: var(--af-surface-1);
}
.af-ov__fold > summary {
  cursor: pointer;
  padding: var(--af-space-3) var(--af-space-4);
  font-size: var(--af-text-sm);
  font-weight: var(--af-weight-semibold);
  color: var(--af-fg-secondary);
}
.af-ov__fold-note {
  font-weight: var(--af-weight-regular);
  margin-left: var(--af-space-2);
}
.af-ov__fold-body,
.af-fold__body {
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  flex-wrap: wrap;
  padding: 0 var(--af-space-4) var(--af-space-3);
}
.af-ov__fold-hint {
  font-size: var(--af-text-xs);
}

/* 管理面表格 */
.af-ov__manage {
  margin-top: var(--af-space-6);
}
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
