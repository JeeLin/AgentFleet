<script lang="ts">
/**
 * 判定码的人话（四个闭集码）。**放在组件内**而不是 `taskTypes.ts`：
 * 它是「验收面板怎么讲这些码」的说法，属于组件的表达，不是线格式。
 *
 * 不在表里的码一律当未知码**原样显示**（`未知判定码：xxx`）——闭集是本次才
 * 引入的，库里可能已有旧值，猜一个意思就是在编事实。
 *
 * `export` 是为了让测试直接对着表断言「四个码都有话」。
 */
export const CRITERIA_TEXT: Readonly<Record<string, string>> = {
  all_tests_pass: '所有测试通过',
  build_passes: '构建通过',
  review_approved: '审查通过',
  command_exit_zero: '命令退出码 0',
}
</script>

<script setup lang="ts">
/* ===========================================================================
   AcceptancePanel —— 验收面板：跑完不等于做对，合并主干必须经过这里
   ---------------------------------------------------------------------------
   `docs/architecture/task-model.md` §5。四个动作，本轮实现三个：

     合入主干     → `pass`        （`review → done`；人；档 1/2 下管家也可）
     打回并写明原因 → `reject`      （`review → pending`，**打回不是失败**）
     只保留分支   → `keep-branch` （本轮 = 打回 + 记原因，真正保留分支归 v0.8.2）
     查看执行复盘 → 渲染但 disabled（复盘页在 v0.8.2 仍是骨架页，不做死链接）

   **没有「判失败」按钮**：`review → failed` 不是合法边，服务端回 409。
   一个按下去只会报错的按钮比没有按钮更坏——用户会以为是自己点错了。

   ── 两条硬规则 ────────────────────────────────────────────────────
   1. 判定码未全绿时「合入主干」**内联二次确认**，且必须写明理由
      （「允许人越过判定，但不许静默越过」）。`note` 空 → 确认键 disabled。
   2. 只有 `description` 没有 `criteria` 的任务必须标「需人工验收」。

   ── 判定码为什么一律显示「未判定」 ────────────────────────────────
   本轮没有验收执行器（里程碑 §3），判定码**没有被验证过**。所以这里既不给
   绿点也不给红点，只列出来 + 写「未判定」——编一个绿就是假话。
   判定码不在闭集里（存量数据）时 `criteria_known === false`，同样不给绿点，
   并把原文显示出来让「前端落后 / 数据脏」这件事可见。

   **纯展示 + 事件**：不发请求、不读 route、不持定时器、不读 `Date.now()`。
   =========================================================================== */

import { computed, ref } from 'vue'

import type { Task } from './taskTypes'

const props = defineProps<{
  task: Task
}>()

const emit = defineEmits<{
  (e: 'pass', note: string): void
  (e: 'reject', note: string): void
  (e: 'keep-branch', note: string): void
}>()

const criteria = computed(() => props.task.done_when?.criteria ?? null)

/** 判定码的人话。未知码原样显示，不猜。 */
const criteriaText = computed(() => {
  const code = criteria.value
  if (!code) return '本任务无判定码，需人工判定'
  return CRITERIA_TEXT[code] ?? `未知判定码：${code}`
})

/** `criteria_known === false` → 判定码区**不给绿点**，原文可见。 */
const criteriaKnown = computed(() => props.task.criteria_known && criteria.value !== null)

/** 判定是否「全绿」。本轮没有执行器，所以永远是 false（未判定）。 */
const allGreen = false

/** note 输入（打回原因 / 覆盖理由共用一条，两个动作不会同时进行）。 */
const note = ref('')

/** 「合入主干」的内联二次确认是否展开。 */
const confirming = ref(false)

/**
 * 判定码未全绿时，「合入主干」的第一下只是展开确认区；全绿时直接提交。
 * **note 空 → 确认键 disabled**（`pass_with_unmet_criteria_requires_a_note`）。
 */
function onPassClick(): void {
  if (!allGreen && !confirming.value) {
    confirming.value = true
    return
  }
  if (!allGreen && note.value.trim().length === 0) return
  emit('pass', note.value.trim())
}

const passConfirmDisabled = computed(() => !allGreen && note.value.trim().length === 0)

function onReject(): void {
  emit('reject', note.value.trim())
}

function onKeepBranch(): void {
  emit('keep-branch', note.value.trim())
}
</script>

<template>
  <section class="af-region af-acceptancepanel" data-panel="acceptance" aria-labelledby="af-acceptancepanel-title">
    <div class="af-region__head">
      <h2 id="af-acceptancepanel-title" class="af-region__title">验收</h2>
      <span class="af-region__note">跑完不等于做对，合并主干必须经过这里</span>
    </div>

    <div class="af-region__body">
      <dl class="af-acceptancepanel__kv">
        <dt class="af-kv__k">完成判定</dt>
        <dd class="af-kv__v" data-field="criteria">
          {{ criteriaText }}
          <!-- 本轮没有验收执行器：判定码只有「未判定」，不编绿也不编红 -->
          <span v-if="criteria" class="af-badge af-badge--off" data-criteria-status="unknown">
            未判定
          </span>
          <!-- `criteria_known === false`（存量数据里的闭集外取值）：不给绿点，
               并把**存储里的原文**印出来——「前端落后 / 数据脏」这件事必须可见，
               否则用户会以为看到的是正确的人话。 -->
          <span v-if="!criteriaKnown && criteria" class="af-badge af-badge--warn">
            判定码不在已知集合内：<span class="af-fp" data-raw-criteria>{{ criteria }}</span>
          </span>
        </dd>
        <dt class="af-kv__k">总结</dt>
        <dd class="af-kv__v" data-field="summary">
          {{ task.done_when?.description ?? '没有写描述' }}
        </dd>
        <dt class="af-kv__k">需人工验收</dt>
        <dd class="af-kv__v" data-field="manual">
          {{ task.needs_manual_acceptance ? '是：这个任务没有判定码，跑完不会自动判定' : '否：有判定码' }}
        </dd>
      </dl>

      <!-- 内联二次确认：判定未全绿却要合并时，必须写明「判定未通过，仍要合并」 -->
      <div v-if="confirming" class="af-callout af-callout--warn" data-confirm="pass">
        <span class="af-callout__k">判定未通过，仍要合并</span>
        <span>判定码还没有全绿（本轮没有验收执行器，全部是「未判定」）。要合入主干，请写明理由。</span>
        <input
          v-model="note"
          class="af-input"
          type="text"
          placeholder="为什么仍然合并（必填）"
          aria-label="越过判定的理由"
        />
        <div class="af-row">
          <button
            class="af-btn af-btn--primary"
            type="button"
            data-action="pass-confirm"
            :disabled="passConfirmDisabled"
            @click="onPassClick"
          >
            确认合入主干
          </button>
          <button class="af-btn af-btn--ghost" type="button" data-action="pass-cancel" @click="confirming = false">
            取消
          </button>
        </div>
      </div>

      <!-- 三个动作。**没有「判失败」**：那條边服务端 409。 -->
      <div class="af-row af-acceptancepanel__actions">
        <button class="af-btn af-btn--primary" type="button" data-action="pass" @click="onPassClick">
          合入主干
        </button>
        <button class="af-btn" type="button" data-action="reject" @click="onReject">
          打回并写明原因
        </button>
        <button
          class="af-btn"
          type="button"
          data-action="keep-branch"
          title="本轮 = 打回 + 记原因，真正保留分支归 v0.8.2"
          @click="onKeepBranch"
        >
          只保留分支
        </button>
        <!--
          第 4 个动作：规格 `task-model.md:268` 的表格与原型
          `prototype/task.html:174-177` 都是四个动作，本轮只做三个。
          与失败面板的「看执行复盘」同一条处置：**渲染但 disabled**，
          不做死链接、也不静默裁掉——裁掉的话用户会以为这个功能不存在。
        -->
        <button
          class="af-btn"
          type="button"
          data-action="replay"
          disabled
          title="执行复盘 v0.8.2 上线"
        >
          查看执行复盘
        </button>
      </div>

      <input
        v-model="note"
        class="af-input af-acceptancepanel__note"
        type="text"
        placeholder="打回原因 / 越过判定的理由"
        aria-label="验收备注"
      />
    </div>
  </section>
</template>
