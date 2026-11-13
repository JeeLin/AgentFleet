<script lang="ts">
import type { FailureKind } from './taskTypes'

/**
 * 八种 `failure_kind` 的人话（`docs/architecture/task-model.md` §3 的表）。
 *
 * 写在组件内而不是 `taskTypes.ts`：它是**这个组件怎么讲这件事**的说法，
 * 不是线格式的一部分（失败面板另有自己的说法）。`export` 是为了让测试
 * 直接对着表断言「八条都有话」。
 */
export const FAILURE_TEXT: Readonly<Record<FailureKind, string>> = {
  baseline_mismatch: '起点不对：设备上的 commit 不是期望的那个',
  env_broken: '环境坏了：依赖或工具链装不上',
  disk_exhausted: '磁盘满了：写满或超预算被中止',
  agent_error: 'agent 出错：崩了、超时或上下文爆了',
  task_failed: '事情没做成：测试红了或构建失败',
  interrupted: '被打断：设备掉线、Hub 重启或被取消',
  rejected: '派不出去：没有合格设备或配额不足',
  superseded: '需求变了：原判定条件不再成立，已被新任务取代',
}

/**
 * 把毫秒翻成「3 分 12 秒」这样的人话。
 *
 * 与组件定义分开写在普通 `<script>` 里，是为了让测试**直接调它**
 * （`format_elapsed_handles_the_units`），而不是隔着模板摸文本。它同时是
 * 「绝不产出百分比」的守卫点：返回值只会是
 * 「N 天 [N 小时] / N 小时 [N 分] / N 分 [N 秒] / N 秒」这四种形态。
 *
 * 负数按 0 处理：设备时钟比 Hub 快时 `now - startedAt` 会是负的，
 * 显示「-3 秒」比显示「0 秒」更让人怀疑整页数据。
 */
export function formatElapsed(ms: number): string {
  if (!Number.isFinite(ms)) return '—'
  const total = Math.max(0, Math.floor(ms / 1000))
  if (total < 60) return `${total} 秒`
  const secs = total % 60
  const mins = Math.floor(total / 60) % 60
  if (total < 3600) return secs === 0 ? `${mins} 分` : `${mins} 分 ${secs} 秒`
  const hours = Math.floor(total / 3600) % 24
  const days = Math.floor(total / 86400)
  if (total < 86400) return mins === 0 ? `${hours} 小时` : `${hours} 小时 ${mins} 分`
  return hours === 0 ? `${days} 天` : `${days} 天 ${hours} 小时`
}
</script>

<script setup lang="ts">
/* ===========================================================================
   TaskCard —— 看板里的一张卡（一卡 = 一个 Task）
   ---------------------------------------------------------------------------
   **纯展示 + 一个事件（`open`）**：不发请求、不读 route、不持定时器、
   **不读 `Date.now()`**。数据由页面传来，时间由 `now` prop 注入。

   ── 为什么 `now` 是必填而不是可选 ──────────────────────────────────
   缺省值一旦写成 `now?: number` + `?? Date.now()`，组件就能「自己取时间」，
   而**测试照样绿**——于是在实现里塞一个 `setInterval` 也没人发现。把 `now`
   设成必填，这条约束就同时被类型系统（不传就编译失败）和 grep
   （源码里不该出现 `Date.now()`）两道门守住。时钟的唯一来源是页面。

   ── 每态渲染什么，是一张显式表（不是模板里的隐式 if） ──────────────
   | state                                  | 已跑时长 | 失败人话 | 第 N 次 | 需人工验收 |
   |----------------------------------------|----------|----------|---------|------------|
   | pending / queued / review / done       | ✗        | ✗        | ✗       | 按需       |
   | running                                | **✓**    | ✗        | ✗       | 按需       |
   | failed                                 | ✗        | **✓**    | ✓(superseded 除外) | 按需 |

   即：`now` 只被 `running` 消费，「第 N 次」只被 `failed` 消费。测试的前置
   必须照这张表写，否则测的是一条走不到的分支。

   **绝不显示百分比**（`PRODUCT.md:328-330`：agent 不上报进度，编一个百分比
   就是自欺）。卡上关于「跑到哪」的话只有一句：已跑多久。
   =========================================================================== */

import { computed } from 'vue'

import StateBadge from '@/domains/shared/components/StateBadge.vue'
import { parseStamp, relativeTime } from '@/shared/format'
import type { Execution, Task } from './taskTypes'

const props = defineProps<{
  task: Task
  /** 当前时刻（毫秒）。**必填、没有缺省值**——见文件头。 */
  now: number
  /** 该任务最近一次执行（页面传最近一条；只读 `started_at` 与 `failure_kind`）。 */
  execution: Execution | null
}>()

const emit = defineEmits<{ (e: 'open', id: string): void }>()

const state = computed(() => props.task.state)
const isRunning = computed(() => state.value === 'running')
const isFailed = computed(() => state.value === 'failed')

/** `running` 卡唯一显示的时间——从 `now` 与 `started_at` 算出，不取系统时钟。 */
const elapsedMs = computed(() => {
  const started = parseStamp(props.execution?.started_at ?? null)
  if (started === null) return null
  return props.now - started * 1000
})

/** `superseded` 不占重试次数（`task-model.md:138`），所以失败人话照给，次数不给。 */
const showAttempt = computed(
  () => isFailed.value && props.execution?.failure_kind !== 'superseded',
)

const failureKind = computed(() => props.execution?.failure_kind ?? null)

/** 第几次 = 该任务已有多少次执行（`failed` 卡才消费它）。 */
const attemptNo = computed(() => props.task.executions?.length ?? null)

/** 未知的 `failure_kind` 显示「未知原因」，**不原样吐英文**。 */
const failureText = computed(() => {
  const kind = failureKind.value
  if (!kind) return ''
  return (FAILURE_TEXT as Readonly<Record<string, string>>)[kind] ?? '未知原因'
})

const relative = computed(() => relativeTime(props.task.updated_at, props.now))
</script>

<template>
  <button
    class="af-card af-taskcard"
    type="button"
    :data-state="state"
    :title="task.title"
    @click="emit('open', task.id)"
  >
    <span class="af-taskcard__title">{{ task.title }}</span>

    <span class="af-taskcard__status">
      <StateBadge :state="state" />
      <!-- `running`：已跑时长。**唯一来源是 `now` prop**（见文件头）。 -->
      <span v-if="isRunning && elapsedMs !== null" class="af-taskcard__elapsed">
        已跑 {{ formatElapsed(elapsedMs) }}
      </span>
    </span>

    <!-- `failed`：失败人话，只在 failed 卡上出现 -->
    <span v-if="isFailed" class="af-taskcard__why">{{ failureText }}</span>
    <!-- 「第 N 次」只挂 failed 卡，且 `superseded` 不给（它不占重试） -->
    <span v-if="showAttempt && attemptNo !== null" class="af-taskcard__attempt">
      第 {{ attemptNo }} 次
    </span>

    <span class="af-taskcard__tags">
      <span
        v-if="task.needs_manual_acceptance"
        class="af-taskcard__tag"
        title="这个任务没有判定码，跑完不会自动判定，必须由人验收"
      >
        需人工验收
      </span>
    </span>

    <span class="af-taskcard__meta">{{ relative }}</span>
  </button>
</template>
