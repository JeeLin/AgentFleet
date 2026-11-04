<script setup lang="ts">
/* ===========================================================================
   BoardPage —— 看板（`prototype/board.html`，v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   项目详情的**默认落点**（`PRODUCT.md` §3.3：看板优先）。没有看板的项目页
   只是一个仓库浏览器。

   ── 本里程碑的边界 ────────────────────────────────────────────────
   六列与卡片的**真实数据**归 v0.8.0（任务模型还没落地）。本里程碑要做到的是
   「路由与布局要真」：六列在、列名对、每列都有空态、项目切换器**真切换**。

   ── 切换器为什么必须真切换 ────────────────────────────────────────
   只换标题是最容易做出来、也最容易骗过审查的假实现：地址变了、页眉变了，
   而列还是同一批。所以切换器**读真项目**（`GET /api/projects`），切换后
   整页重新按新项目加载。

   ── 六列的顺序不是随便排的 ────────────────────────────────────────
   照 `task-model.md` 的六态：待办 → 进行中 → 待验收 → 失败 → 完成 → 已取消。
   「失败」在「完成」之前是因为它是**需要处置**的一列——排到最后会被滚出视野，
   而失败不处置就永远停在那里（约束 16：停在失败列不自动回待办）。
   =========================================================================== */

import { computed, onMounted, ref, watch } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import { getProject, listProjects, type Project } from '@/domains/project/api'
import { describeReadFailure } from '@/shared/http'
import { navigate, route } from '@/router'

/** 当前项目 id，从地址读——地址栏是真相源。 */
const projectId = computed(() => route.value.params.id ?? '')

const project = ref<Project | null>(null)
const all = ref<Project[]>([])
const loading = ref(true)
const failure = ref('')

/** 六列。`key` 只用于渲染 key，与后端状态名的对应关系在 v0.8.0 落地。 */
const COLUMNS = [
  { key: 'todo', title: '待办', hint: '还没开始的任务' },
  { key: 'running', title: '进行中', hint: '正在设备上执行' },
  { key: 'review', title: '待验收', hint: '等你看过改动再定' },
  { key: 'failed', title: '失败', hint: '失败的任务停在这里，不会自动回待办' },
  { key: 'done', title: '完成', hint: '已合入主干' },
  { key: 'cancelled', title: '已取消', hint: '主动放弃的任务' },
] as const

async function load(): Promise<void> {
  const id = projectId.value
  if (!id) {
    failure.value = '这个地址缺少项目 id。'
    loading.value = false
    return
  }
  loading.value = true
  failure.value = ''
  try {
    // 两个请求并行：切换器要整份列表，主区要当前项目。
    const [p, list] = await Promise.all([getProject(id), listProjects()])
    project.value = p
    all.value = list
  } catch (e) {
    project.value = null
    failure.value = describeReadFailure(e, '项目')
  } finally {
    loading.value = false
  }
}

// 换项目时重载：`:id` 变了就是不同项目，不能沿用上一份数据。
watch(projectId, load)
onMounted(load)

/**
 * 切换器菜单开着没有。
 *
 * **项目名本身就是那个按钮**（见 `components.css` 的 `.af-switch`）：左栏收起
 * 之后这是唯一的换项目入口，所以它必须一直可见，不能藏进二级菜单。
 */
const menuOpen = ref(false)

/** 形态的三个人话说法。菜单里每个项目后面带一个，否则同名的两个项目无从区分。 */
function describeShape(shape: string): string {
  if (shape === 'central') return '中心式'
  if (shape === 'mirror') return '镜像式'
  if (shape === 'isolated') return '隔离式'
  return shape
}

function switchTo(id: string): void {
  // 先关菜单再跳转：不关的话，同一页换了项目之后菜单还开着，
  // 用户会以为「点了没反应」（其实已经跳了，菜单挡住了新内容）。
  menuOpen.value = false
  if (id === projectId.value) return
  // **必须编码**：`matchPattern` 对具名段做 `decodeURIComponent`，id 里含
  // `/` 或 `%` 时不编码会把路径切成多一段，落进 `not-found`。
  navigate(`/project/${encodeURIComponent(id)}/board`)
}
</script>

<template>
  <section class="af-page">
    <header class="af-page__head">
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
      <nav class="af-projectnav" aria-label="项目内导航">
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
    </header>

    <p v-if="loading" class="af-muted">正在读取项目…</p>

    <EmptyState
      v-else-if="failure"
      glyph="⚠"
      title="打不开这个项目"
      :text="failure"
    >
      <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
    </EmptyState>

    <template v-else>
      <h2 class="af-page__subtitle">{{ project?.name ?? '看板' }}</h2>
      <div class="af-board">
        <section v-for="col in COLUMNS" :key="col.key" class="af-board__col">
          <header class="af-board__colhead">
            <h3 class="af-board__coltitle">{{ col.title }}</h3>
          </header>
          <p class="af-board__hint">{{ col.hint }}</p>
          <EmptyState
            glyph="▢"
            :title="`${col.title}里还没有任务`"
            text="任务模型在 v0.8.0 落地，届时这里会出现卡片的真实数据。"
          />
        </section>
      </div>
    </template>
  </section>
</template>
