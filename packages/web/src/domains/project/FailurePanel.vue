<script lang="ts">
import type { FailureKind } from './taskTypes'

/**
 * 八种失败形态在**这个面板里**的说法。
 *
 * 与卡片上那句不同：面板是「诊断 + 处置」，所以每句都写清**该谁处置**
 * （`task-model.md` §3 表格的「该谁处置」列）。例如 `env_broken` 要人去修机器，
 * `task_failed` 要人换条件——把它们都说成「失败了，重试吧」是这一格最容易犯的错。
 *
 * `export` 是为了让测试直接对着表断言「八条都有话」。
 */
export const FAILURE_DIAGNOSIS: Readonly<Record<FailureKind, string>> = {
  baseline_mismatch: '起点不对：设备上的 commit 不是期望的那个。重新拉取，或换一台基线干净的执行',
  env_broken: '环境坏了：依赖或工具链装不上。重试没用，要修这台机器的环境或换架构',
  disk_exhausted: '磁盘满了：执行中写满或超预算被中止。换设备前先过磁盘硬门槛',
  agent_error: 'agent 出错：崩了、超时、协议错或上下文爆了。优先换 agent',
  task_failed: '事情没做成：agent 正常跑完了，但测试红了 / 构建失败。必须换条件',
  interrupted: '被打断：设备掉线、Hub 重启或任务被取消。先试续跑',
  rejected: '派不出去：没有合格设备、被隔离策略挡住或配额不足。要改条件',
  superseded: '需求变了：原判定条件不再成立，任务已被新任务取代。只留痕，不重试',
}
</script>

<script setup lang="ts">
/* ===========================================================================
   FailurePanel —— 失败面板：**全站唯一一套失败处置动作**
   ---------------------------------------------------------------------------
   `docs/architecture/task-model.md` §4：点开一张 `failed` 卡，看到的不是
   一堆散落按钮，而是「一个诊断 + 一组动作」。**不许在别处（设备页、收件箱、
   命令行）提供第二套**——两处状态会打架。

   ── 4 行数据（`task-model.md:229-232`）────────────────────────────
     失败原因 / 证据 / 第几次 / 产出

   ── 5 个动作（`task-model.md:234`）────────────────────────────
     换设备重派 → `retry`    （`failed → queued`，不挑设备；本轮没有「设备拒接」）
     改任务再跑 → `edit`     （task 版本 +1，旧值留痕）
     放宽约束   → `relax`    （只改 allowed_* / required_capabilities）
     看执行复盘 → **渲染但 disabled**（复盘页 v0.8.2 才上线，不做死链接）
     关掉       → `close`    （必须写明原因，否则「失败」会变成「消失的失败」）

   `failed → queued` 这条边**只有「换设备重派」一个入口**；少画一个动作就是
   少一条边，所以五个都在。

   **纯展示 + 事件**：不发请求、不读 route、不持定时器、不读 `Date.now()`。
   =========================================================================== */

import { computed, ref } from 'vue'

import { relativeTime } from '@/shared/format'
import type { Execution, Task } from './taskTypes'

const props = defineProps<{
  task: Task
  /** 最近一次执行（`null` = 还没跑过，例如 `rejected` 的「未派活」）。 */
  execution: Execution | null
  /**
   * 当前时刻（毫秒）。**必填、没有缺省值**——与 `TaskCard` 同一条纪律：
   * 组件不自己取时间，时间由页面注入。
   */
  now: number
}>()

const emit = defineEmits<{
  (e: 'retry'): void
  (e: 'edit', reason: string): void
  (e: 'relax'): void
  (e: 'close', reason: string): void
}>()

const kind = computed(() => props.execution?.failure_kind ?? null)

/** 失败原因的人话。未知值显示「未知原因」，**不原样吐英文**。 */
const diagnosis = computed(() => {
  const k = kind.value
  if (!k) return '没有找到执行记录：这次失败没有留下原因'
  return (FAILURE_DIAGNOSIS as Readonly<Record<string, string>>)[k] ?? '未知原因'
})

/** 原因一行：认得的给人话，不认得的**显示「未知原因」+ 原文**。
 * 原文留在括号里是因为它是事实（数据脏 / 前端落后都该被看见），
 * 但**主词必须是「未知原因」**——用户先要知道「这不是我认得的那八种」。 */
const reasonValue = computed(() => {
  const k = kind.value
  if (!k) return '未知原因'
  return (FAILURE_DIAGNOSIS as Readonly<Record<string, string>>)[k] ? k : `未知原因（${k}）`
})

/** `evidence` 为 `'none'` 时显示「无证据」，而不是字面 `none`。 */
const evidenceText = computed(() => {
  const ev = props.execution?.evidence ?? null
  if (!ev || ev === 'none') return '无证据'
  return ev
})

/** 「第几次」：执行条数 / 重试上限（上限空 = 默认 2，`schema_v7.sql` 的口径）。 */
const attemptText = computed(() => {
  const done = props.task.executions?.length ?? 0
  const limit = props.task.constraints?.retry_limit ?? 2
  return `第 ${Math.max(done, 1)} 次 · 上限 ${limit} 次`
})

/**
 * 「产出」：起点。**不编分支名**。
 *
 * `task-model.md` §8 明列 `branch` 尚未落库（v0.8.2 的执行间接力才写）。
 * 早先这里拼了一个 `afleet/<task>/e1` 出去——它与同一页 `ProjectTaskPage`
 * 的「库里还没有记录分支（v0.8.2 上线）」互相矛盾，而且那串东西拿去
 * `git checkout` 必然失败（`e1` 也恒为第 1 次，重试过就是错的）。
 * **一个看上去像真 ref 的字符串比一个空格外危险**。
 *
 * 没有执行记录时说「没有产出」——那是事实，不是编的。
 */
const outputText = computed(() => {
  const ex = props.execution
  if (!ex) return '没有产出（这条失败没有产生执行）'
  return ex.base_commit
    ? `分支：库里还没有记录（v0.8.2 上线） · 起点 ${ex.base_commit.slice(0, 7)}`
    : '分支：库里还没有记录（v0.8.2 上线）'
})

/** 关掉必须写明原因（`task-model.md:244`）：reason 空时按钮 disabled。 */
const closeReason = ref('')
const closeDisabled = computed(() => closeReason.value.trim().length === 0)

function onClose(): void {
  if (closeDisabled.value) return
  emit('close', closeReason.value.trim())
}

/** 「改任务再跑」也要留痕（改前改后要看得出来），所以它同样吃一个 reason。 */
const editReason = ref('')

function onEdit(): void {
  emit('edit', editReason.value.trim())
}

const executedAt = computed(() => {
  const at = props.execution?.ended_at ?? props.execution?.started_at ?? null
  return at ? relativeTime(at, props.now) : '—'
})
</script>

<template>
  <section class="af-region af-failurepanel" data-panel="failure" aria-labelledby="af-failurepanel-title">
    <div class="af-region__head">
      <h2 id="af-failurepanel-title" class="af-region__title">上次为什么失败</h2>
      <span class="af-region__note">{{ executedAt }}</span>
    </div>

    <div class="af-region__body">
      <div class="af-callout af-callout--failed">
        <span class="af-callout__k">失败原因 · {{ reasonValue }}</span>
        <span>{{ diagnosis }}</span>
        <span v-if="execution?.reason" class="af-fp">{{ execution.reason }}</span>
      </div>

      <!-- 4 行数据：失败原因 / 证据 / 第几次 / 产出（`task-model.md:229-232`） -->
      <dl class="af-failurepanel__kv">
        <dt class="af-kv__k">失败原因</dt>
        <dd class="af-kv__v" data-field="reason">{{ reasonValue }}</dd>
        <dt class="af-kv__k">证据</dt>
        <dd class="af-kv__v" data-field="evidence">{{ evidenceText }}</dd>
        <dt class="af-kv__k">第几次</dt>
        <dd class="af-kv__v" data-field="attempt">{{ attemptText }}</dd>
        <dt class="af-kv__k">产出</dt>
        <dd class="af-kv__v af-fp" data-field="output">{{ outputText }}</dd>
      </dl>

      <!-- 5 个动作，一个都不许静默裁剪 -->
      <div class="af-row af-failurepanel__actions">
        <button
          class="af-btn af-btn--primary"
          type="button"
          data-action="retry"
          title="同一个任务、同一个判定条件，换一台设备再跑一次（不改任务定义）"
          @click="emit('retry')"
        >
          换设备重派
        </button>

        <button class="af-btn" type="button" data-action="edit" @click="onEdit">改任务再跑</button>

        <button
          class="af-btn"
          type="button"
          data-action="relax"
          title="只改允许设备 / 允许 agent / 必须具备的能力"
          @click="emit('relax')"
        >
          放宽约束
        </button>

        <!-- 例外 1（里程碑 §4）：复盘页 v0.8.2 才上线 → 渲染但 disabled，不做死链接 -->
        <button class="af-btn" type="button" data-action="replay" disabled title="执行复盘 v0.8.2 上线">
          看执行复盘
        </button>
      </div>

      <div class="af-failurepanel__close">
        <input
          v-model="editReason"
          class="af-input af-failurepanel__input"
          type="text"
          placeholder="改了什么（可留空）"
          aria-label="改任务再跑的原因"
        />
        <input
          v-model="closeReason"
          class="af-input af-failurepanel__input"
          type="text"
          placeholder="为什么不做这个任务（必填）"
          aria-label="关掉这个任务的原因"
        />
        <button
          class="af-btn af-btn--ghost"
          type="button"
          data-action="close"
          :disabled="closeDisabled"
          title="没有原因的关闭会把失败变成「消失的失败」"
          @click="onClose"
        >
          关掉
        </button>
      </div>
    </div>
  </section>
</template>
