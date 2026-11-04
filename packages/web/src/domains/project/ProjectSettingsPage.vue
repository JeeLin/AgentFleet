<script setup lang="ts">
/* ===========================================================================
   ProjectSettingsPage —— 项目设置（`prototype/project-settings.html`，v0.7.0）
   ---------------------------------------------------------------------------
   这个页面回答三个问题，顺序就是原型四个区块的顺序：
   1. **代码在哪、谁能合**（合并权 + 隔离策略）
   2. **占多大、怎么收**（配额与清理）
   3. **怎么退场**（归档，可取消）

   ── 三个区块里，只有一个后端现在真的支持 ─────────────────────────────
   **必须如实标出「还没接后端」的部分**，否则用户点「升到这一档」看到
   「保存失败（unknown）」——那是比不给按钮更坏的体验，因为他会以为自己
   操作错了。原型上这些按钮是真的能点的，所以这里不能做成假的：
   每个未接线的地方给一条 `af-callout--info`，写明「界面上写的是设计，
   现在改不了」，并且**按钮真的 disabled**。

   现状（2026-11-05，对着 `routes/projects.rs` 与 `hub-storage/src/binding.rs` 核过）：
   - **合并权三档**：后端**没有**这个字段。`ProjectView` 只有
     `id/name/shape/theme_color/archived_at/created_at`。
   - **隔离策略**：形态读得到（`shape`）、能升（`POST /shape`），这块真接。
   - **配额与清理**：字段**在绑定上**（`binding.disk_quota_bytes` /
     `binding.cleanup_policy`），**不在项目上**。所以这里按项目改不了——
     要改得去 `BindPage`。这是数据模型的真实形状，不是漏做。
   - **主题色**：真接（`PATCH /api/projects/:id` 的 `theme_color`）。
   - **归档**：真接（`POST /archive` / `POST /unarchive`）。

   ── 形态只能单向升 ─────────────────────────────────────────────────
   `isolated → central` 允许，反过来不提供：把已经上传的代码真收回来，
   产品做不到。同形态提交会被后端拒（`shape_transition_refused`），
   所以前端在同形态上不显示按钮。

   ── 归档失败不能把页面变成「读取失败」 ───────────────────────────────
   与 `ArchivedPage` 同一条教训：写失败时列表和内容必须留在页面上，
   用独立的 `actionFailure`。混成一个 `failure` 的后果是用户点一次归档
   失败，整页变成「打不开这个项目」——他连再试一次的按钮都没有了。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import {
  archiveProject,
  changeProjectShape,
  getProject,
  unarchiveProject,
  patchProject,
  type Project,
} from '@/domains/project/api'
import { ApiError, describeReadFailure } from '@/shared/http'
import { navigate, route } from '@/router'

const projectId = computed(() => route.value.params.id ?? '')

const project = ref<Project | null>(null)
const loading = ref(true)
/** 读失败：整页进错误态。 */
const failure = ref('')
/** 写失败：**与 `failure` 分开**，内容留在页面上。 */
const actionFailure = ref('')
const actionNotice = ref('')
const saving = ref(false)

const archived = computed(() => project.value?.archived_at !== null && project.value !== null)

/**
 * 形态的中文名与「代码在哪」。
 *
 * 后端的 `shape` 是开放字符串（见 `api.ts` 顶部那段），所以这里必须有一个
 * `default` 分支：后端将来加第四种形态时，页面显示原文而不是空白。
 */
const SHAPES: Record<string, { label: string; where: string; stored: string; origin: string }> = {
  central: {
    label: '中心式',
    where: 'Hub 的裸仓库',
    stored: '全部代码 + 元数据',
    origin: 'Hub 上的 commit',
  },
  mirror: {
    label: '镜像式',
    where: '上游 + Hub 镜像',
    stored: '镜像的全部代码 + 元数据',
    origin: 'Hub 上镜像的 commit',
  },
  isolated: {
    label: '隔离式',
    where: '只在指定设备，永不出该设备',
    stored: '只有元数据',
    origin: '设备上的 commit hash，Hub 不验证内容',
  },
}

function describeShape(shape: string): string {
  return SHAPES[shape]?.label ?? shape
}

/**
 * 能不能升到中心式。
 *
 * 只有 `isolated → central` 这一条路。`central` 已是终点、`mirror` 不在
 * 允许表里（后端同样只认这一条）——三种情况都**不显示按钮**，因为一个
 * 点了必然被拒的按钮比没有按钮更糟。
 */
const canPromote = computed(() => project.value?.shape === 'isolated' && !archived.value)

/** 主题色。`null` = 没设，不是一个颜色。 */
const COLORS = [
  { key: 'blue', label: '蓝' },
  { key: 'green', label: '绿' },
  { key: 'purple', label: '紫' },
  { key: 'orange', label: '橙' },
  { key: 'gray', label: '灰' },
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
    project.value = await getProject(id)
  } catch (e) {
    project.value = null
    failure.value = describeReadFailure(e, '项目')
  } finally {
    loading.value = false
  }
}

/**
 * 写操作的统一出口。
 *
 * 三个动作（升形态 / 归档 / 改色）走同一条 wrap：清掉上一条提示、置
 * `saving`、成功重读。**重读而不是本地改状态**：形态变更会连带建仓，
 * 归档会连带停派发，前端自己拼的「应该是新的」迟早和库里对不上。
 */
async function runWrite(
  action: () => Promise<unknown>,
  okText: string,
  failPrefix: string,
): Promise<void> {
  actionFailure.value = ''
  actionNotice.value = ''
  saving.value = true
  try {
    await action()
    await load()
    actionNotice.value = okText
  } catch (e) {
    actionFailure.value = describeWriteFailure(e, failPrefix)
  } finally {
    saving.value = false
  }
}

/**
 * 写失败的措辞。
 *
 * `shape_transition_refused` 要把 `detail` 里的 `{from, to}` 说出来：
 * 只回「操作失败」的话，用户猜不到是「方向反了」还是「同形态重复提交」。
 */
function describeWriteFailure(e: unknown, prefix: string): string {
  if (e instanceof ApiError) {
    if (e.code === 'shape_transition_refused') {
      return `${prefix}：形态只能从隔离式单向升到中心式，这个方向不被允许。`
    }
    return `${prefix}（${e.code}）。`
  }
  return `${prefix}：Hub 连不上。`
}

function promote(): void {
  // 按钮在 `canPromote` 为假时根本不渲染，这里是第二道闸——
  // 键盘回车 / 自动化点击仍可能走到，不能靠「看不着」当保护。
  if (!canPromote.value) return
  void runWrite(
    () => changeProjectShape(projectId.value, 'central'),
    '已升级为中心式：代码现在由 Hub 保管。',
    '升级失败',
  )
}

function setColor(color: string | null): void {
  void runWrite(
    () => patchProject(projectId.value, { theme_color: color }),
    color === null ? '已清除主题色。' : `主题色已改为「${COLORS.find((c) => c.key === color)?.label ?? color}」。`,
    '改主题色失败',
  )
}

function toggleArchive(): void {
  if (!project.value) return
  if (archived.value) {
    void runWrite(
      () => unarchiveProject(projectId.value),
      '已取消归档。定时任务**不会**自动恢复，需要你自己去看一眼再开。',
      '取消归档失败',
    )
  } else {
    void runWrite(
      () => archiveProject(projectId.value),
      '已归档。正在跑的执行不会被打断，跑完才归档。',
      '归档失败',
    )
  }
}

onMounted(load)
</script>

<template>
  <div class="af-page">
    <div class="af-page__head">
      <button
        type="button"
        class="af-btn af-btn--ghost"
        @click="navigate(`/project/${encodeURIComponent(projectId)}/board`)"
      >
        ← 回看板
      </button>
      <h1 class="af-page__title">项目设置</h1>
      <span v-if="project" class="af-badge">{{ project.name }}</span>
      <span v-if="project" class="af-badge">{{ describeShape(project.shape) }}</span>
      <span v-if="archived" class="af-badge af-badge--warn">已归档</span>
      <span class="af-page__actions"></span>
      <button
        type="button"
        class="af-btn af-btn--ghost"
        @click="navigate(`/project/${encodeURIComponent(projectId)}/bind`)"
      >
        绑定与配额
      </button>
    </div>

    <p v-if="loading" class="af-muted">正在读取项目…</p>

    <EmptyState v-else-if="failure" glyph="⚠" title="打不开这个项目" :text="failure">
      <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
    </EmptyState>

    <template v-else-if="project">
      <p v-if="actionFailure" class="af-form__error" role="alert">{{ actionFailure }}</p>
      <p v-if="actionNotice" class="af-muted" role="status">{{ actionNotice }}</p>

      <!-- ── ① 合并权 ─────────────────────────────────────────────── -->
      <section class="af-region" aria-label="这个项目的合并权">
        <div class="af-region__head">
          <h2 class="af-region__title">这个项目的合并权</h2>
          <span class="af-region__note">档位决定管家能不能自己把分支合到主干</span>
        </div>
        <div class="af-region__body af-pset__stack">
          <p class="af-pset__lead">
            这是项目级设置——个人小工具和主仓库不该是同一个答案。
          </p>

          <div class="af-callout af-callout--info">
            <span class="af-callout__k">这一块界面是设计定稿，后端还没接</span>
            <span>
              Hub 目前没有「合并权档位」这个字段，所以下面的按钮是灰的、点不动。
              写在页面上而不是藏起来，是因为你需要在排期前就知道它长什么样、
              以及它会带来什么约束。**这不是一个假按钮**：它明确告诉你现在还改不了。
            </span>
          </div>

          <table class="af-table">
            <thead>
              <tr><th>档</th><th>管家能做什么</th><th>适合</th><th></th></tr>
            </thead>
            <tbody>
              <tr>
                <td><strong>档 0</strong></td>
                <td class="af-pset__cell">
                  只能你或你预设的规则合并。管家能给判断、能推你一把，合主干要你点头
                </td>
                <td class="af-pset__cell af-muted">还没建立信任</td>
                <td>
                  <button type="button" class="af-btn af-btn--sm" disabled>未接后端</button>
                </td>
              </tr>
              <tr>
                <td><strong>档 1</strong></td>
                <td class="af-pset__cell">管家可合并，<b>每次合并写进总结并通知你</b></td>
                <td class="af-pset__cell af-muted">用过一阵，想省操作</td>
                <td>
                  <button type="button" class="af-btn af-btn--sm" disabled>未接后端</button>
                </td>
              </tr>
              <tr>
                <td><strong>档 2</strong></td>
                <td class="af-pset__cell">管家可合并，<b>仅通知</b>，不写总结</td>
                <td class="af-pset__cell af-muted">已经信任</td>
                <td>
                  <button type="button" class="af-btn af-btn--sm" disabled>未接后端</button>
                </td>
              </tr>
            </tbody>
          </table>

          <div class="af-callout af-callout--warn">
            <span class="af-callout__k">升档只能由人操作，管家不能自己申请</span>
            <span>
              管家可以说「这次我判得对不对」，但不会说「给我升档吧」。
              自我扩权是这条设计里唯一不能开的口子。
            </span>
          </div>
        </div>
      </section>

      <!-- ── ② 隔离策略 ───────────────────────────────────────────── -->
      <section class="af-region" aria-label="隔离策略">
        <div class="af-region__head">
          <h2 class="af-region__title">隔离策略</h2>
          <span class="af-region__note">形态决定谁有资格跑，不是偏好是约束</span>
        </div>
        <div class="af-region__body af-pset__stack">
          <div class="af-field">
            <span class="af-field__label">项目形态</span>
            <div class="af-row">
              <span class="af-badge">{{ describeShape(project.shape) }}</span>
              <span class="af-muted">
                当前形态的代码位置见下表
              </span>
            </div>
            <span class="af-field__hint">
              形态是项目的属性，不是三种产品。可以把隔离式<b>转成</b>中心式（推上去即可），
              反过来不提供 —— 把已经上传的代码真收回来，产品做不到。
            </span>
          </div>

          <table class="af-table">
            <thead>
              <tr><th>形态</th><th>代码在哪</th><th>Hub 存什么</th><th>「起点」是什么</th></tr>
            </thead>
            <tbody>
              <tr v-for="(info, key) in SHAPES" :key="key">
                <td class="af-pset__cell">
                  <b>{{ info.label }}</b>
                  <span v-if="key === project.shape" class="af-badge af-badge--ok">当前</span>
                </td>
                <td class="af-pset__cell">{{ info.where }}</td>
                <td class="af-pset__cell">{{ info.stored }}</td>
                <td class="af-pset__cell">{{ info.origin }}</td>
              </tr>
            </tbody>
          </table>

          <div v-if="canPromote" class="af-row">
            <button
              type="button"
              class="af-btn af-btn--primary"
              :disabled="saving"
              @click="promote"
            >
              {{ saving ? '正在升级…' : '升级为中心式' }}
            </button>
            <span class="af-muted">
              升级后 Hub 会真实建仓并接管代码，这一步不可撤销
            </span>
          </div>
          <p v-else-if="project.shape === 'central'" class="af-pset__xs" style="margin: 0">
            已经是中心式，没有可升的下一档。
          </p>
          <p v-else class="af-pset__xs" style="margin: 0">
            镜像式不支持变更形态：它的代码来源是上游，换形态要先决定上游还跟不跟。
          </p>

          <div class="af-callout af-callout--warn">
            <span class="af-callout__k">隔离式项目的代价，现在就说清</span>
            <span>
              改成隔离式之后，<b>Hub 上没有代码</b>，Hub 也不核对设备报上来的 commit 内容。
              你能看到 diff（每次向设备拉取），但看不到「Hub 上的那份」——因为不存在那一份。
              这是隔离式的定义，不是缺陷。
            </span>
          </div>
        </div>
      </section>

      <!-- ── ③ 磁盘配额与清理 ─────────────────────────────────────── -->
      <section class="af-region" aria-label="磁盘配额与清理">
        <div class="af-region__head">
          <h2 class="af-region__title">磁盘配额与清理</h2>
          <span class="af-region__note">三件事分开：允许多大 / 跑不跑得动 / 跑完怎么收回</span>
        </div>
        <div class="af-region__body af-pset__stack">
          <div class="af-callout af-callout--info">
            <span class="af-callout__k">配额与清理挂在「项目 × 设备」的绑定上，不是项目上</span>
            <span>
              同一个项目在三台设备上可以各占 40 GB，也可以各用不同策略——所以这里没有
              一个「项目级」的值可改。要改请进
              <button
                type="button"
                class="af-btn af-btn--sm af-btn--ghost"
                @click="navigate(`/project/${encodeURIComponent(projectId)}/bind`)"
              >
                绑定与配额
              </button>
              。这是数据模型的真实形状，不是这一页漏做了。
            </span>
          </div>

          <table class="af-table">
            <thead>
              <tr><th>策略</th><th>依赖缓存</th><th>任务产物</th><th>什么时候该用</th></tr>
            </thead>
            <tbody>
              <tr>
                <td class="af-pset__cell"><b>保守</b></td>
                <td class="af-pset__cell">留 30 天</td>
                <td class="af-pset__cell">留最近 3 个做诊断</td>
                <td class="af-pset__cell af-muted">还在调这个项目，回头要看产物</td>
              </tr>
              <tr>
                <td class="af-pset__cell"><b>平衡</b></td>
                <td class="af-pset__cell">留 7 天</td>
                <td class="af-pset__cell">跑完即清</td>
                <td class="af-pset__cell af-muted">日常</td>
              </tr>
              <tr>
                <td class="af-pset__cell"><b>激进</b></td>
                <td class="af-pset__cell">尽快清</td>
                <td class="af-pset__cell">跑完即清</td>
                <td class="af-pset__cell af-muted">设备紧张，或者项目已经稳了</td>
              </tr>
            </tbody>
          </table>

          <div class="af-callout">
            <span class="af-callout__k">产物必清，缓存才可以留</span>
            <span>
              依赖目录按 lockfile 哈希复用 —— 但 <b>缓存不是产物</b>。
              任务产物永远跑完就清，只有缓存受上面三档影响。
              依赖目录也永远不在设备间搬运：x86-64 和 aarch64 装出来的不是同一份东西。
            </span>
          </div>
          <div class="af-callout af-callout--warn">
            <span class="af-callout__k">磁盘超 90% 就不再往这台派任务</span>
            <span class="af-muted">
              这是全局水位，不按项目改。满了不是性能问题，是任务会静默失败的问题。
              真满了会变成一条待决策进收件箱，不会自己删东西。
            </span>
          </div>
        </div>
      </section>

      <!-- ── ④ 主题色 ─────────────────────────────────────────────── -->
      <section class="af-region" aria-label="项目主题色">
        <div class="af-region__head">
          <h2 class="af-region__title">项目主题色</h2>
          <span class="af-region__note">只用来区分，不影响任何行为</span>
        </div>
        <div class="af-region__body af-pset__stack">
          <div class="af-field" role="radiogroup" aria-label="项目主题色">
            <span class="af-field__label">用在哪</span>
            <div class="af-row">
              <button
                v-for="c in COLORS"
                :key="c.key"
                type="button"
                role="radio"
                class="af-btn af-btn--sm"
                :class="{ 'af-btn--ghost': project.theme_color !== c.key }"
                :aria-checked="project.theme_color === c.key"
                :disabled="saving"
                @click="setColor(c.key)"
              >
                {{ c.label }}{{ project.theme_color === c.key ? '（当前）' : '' }}
              </button>
              <button
                v-if="project.theme_color !== null"
                type="button"
                class="af-btn af-btn--sm af-btn--ghost"
                :disabled="saving"
                @click="setColor(null)"
              >
                清除
              </button>
            </div>
            <span class="af-field__hint">
              用在看板卡片左侧色条、时间线和全局任务流里的项目标签、项目页页眉。
              <b>纯视觉</b>：改它不会改变任何任务、权限或调度。
            </span>
          </div>
          <div class="af-callout af-callout--info">
            <span class="af-callout__k">别和配色方案搞混</span>
            <span class="af-muted">
              这里是<b>项目</b>的颜色，账号级共享，团队里看到的是同一个。
              你自己屏幕的暗/亮和冷暖在全局设置 · 外观，那只存在你这台设备上，
              改不动别人的屏幕。
            </span>
          </div>
        </div>
      </section>

      <!-- ── ⑤ 归档 ───────────────────────────────────────────────── -->
      <section class="af-region" aria-label="归档这个项目">
        <div class="af-region__head">
          <h2 class="af-region__title">{{ archived ? '取消归档' : '归档这个项目' }}</h2>
          <span class="af-region__note">可取消，不是删除</span>
        </div>
        <div class="af-region__body af-pset__stack">
          <table class="af-table">
            <tbody>
              <tr>
                <td class="af-pset__cell" style="width: 38%">正在跑的执行</td>
                <td class="af-pset__cell"><b>不打断</b>，跑完才归档，结果照常进待验收</td>
              </tr>
              <tr>
                <td class="af-pset__cell">待办 / 排队的任务</td>
                <td class="af-pset__cell">停止派发。仍在这个项目里，取消归档后接着走</td>
              </tr>
              <tr>
                <td class="af-pset__cell">定时任务</td>
                <td class="af-pset__cell">一并暂停。<b>取消归档时不会自动恢复</b> —— 得你看一眼再开</td>
              </tr>
              <tr>
                <td class="af-pset__cell">设备上的工作目录</td>
                <td class="af-pset__cell">按绑定上的清理策略回收。历史记录不受影响</td>
              </tr>
              <tr>
                <td class="af-pset__cell">执行记录、失败归因、总结、轨迹</td>
                <td class="af-pset__cell"><b>全留着</b>。归档不是删除</td>
              </tr>
            </tbody>
          </table>

          <div class="af-row">
            <button
              type="button"
              class="af-btn"
              :class="{ 'af-btn--danger': !archived }"
              :disabled="saving"
              @click="toggleArchive"
            >
              {{ archived ? '取消归档' : '归档这个项目' }}
            </button>
            <button
              type="button"
              class="af-btn af-btn--ghost"
              @click="navigate('/projects/archived')"
            >
              看已归档的项目
            </button>
          </div>
          <p class="af-pset__xs" style="margin: 0">
            没有「删除」。项目是 Hub 的真相源，删掉会让已合入的历史、失败归因、成本记录一起消失。
            失败归因和排障都要靠它 —— 一个跑了十次失败的项目，比一个干净的空项目有价值。
          </p>
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
/* ---------------------------------------------------------------------------
   本页的局部样式。

   **为什么需要它们**：`af-xs` / `af-stack` 是原型（`prototype/`）里的
   简写工具类，生产 CSS 里从来没有这两条——`components.css` 只收真正的
   组件。所以这里按本页需要落成 `af-pset__*` 命名，作用域是这一页，
   不进共享表：它们是「这一页怎么排」，不是「全站怎么排」。
   （与 `BindPage` 的 `af-bind__*` 同一做法。）
   --------------------------------------------------------------------------- */

/* 区块主体纵向堆叠。`gap` 用 space-4：区块内部是「相关的一组」，比区块之间
   的间距紧一档，否则一页看下来全是等距的，分不出哪里是一组。 */
.af-pset__stack {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-4);
}

/* 小字。字号取自 token，不写字面值——暗/亮与四套主题下都要跟得上。 */
.af-pset__xs {
  font-size: var(--af-text-xs);
  color: var(--af-fg-secondary);
  line-height: var(--af-leading-base);
}

/* 表格单元格里的小字：同一档字号，但颜色跟正文（表格本身已有层级）。 */
.af-pset__cell {
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-snug);
}

/* 引导段落（区块内第一段说明）。 */
.af-pset__lead {
  margin: 0;
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
}
</style>
