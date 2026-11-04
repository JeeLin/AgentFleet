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
import { navigate } from '@/router'

const projects = ref<Project[]>([])
const loading = ref(true)
const failure = ref('')

/** 形态的中文名。**与 `PRODUCT.md` §3.2 的表格用词一致**，不另造说法。 */
const SHAPE_LABEL: Readonly<Record<string, string>> = {
  central: '中心式',
  mirror: '镜像式',
  isolated: '隔离式',
}

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
          @click="navigate(`/project/${p.id}/board`)"
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

    <section class="af-page__section">
      <h2 class="af-page__subtitle">待办</h2>
      <EmptyState
        glyph="▢"
        title="待办摘要要等任务模型落地"
        text="这一块汇总「今天要做哪些任务」。任务归 v0.8.0，本里程碑只把项目与绑定做起来。"
      />
    </section>
  </section>
</template>
