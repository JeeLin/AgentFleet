<script setup lang="ts">
/* ===========================================================================
   AgentDetailPage —— Agent 详情（`prototype/agents.html`，v0.7.0 子任务 7）
   ---------------------------------------------------------------------------
   设备页回答「这台机器怎么了」，这一页回答「**这个 agent 靠不靠得住**」。
   后者不挂在某台设备上：同一个 agent 可能装在五台机器上，用户想知道的是
   「让它干活，成不成」。

   ── 一句必须显示的话 ──────────────────────────────────────────────
   执行记录里**没有**「这条是哪个 agent 跑的」这一列。历史是按「执行所在的
   那台设备上装了这个 agent」反推的，所以同一台机器上装了三个 agent 时，
   它们会共享同一批执行。

   这不是实现偷懒，是数据里真的没有那条信息——补它要改 schema 与派活链路，
   而那是一个独立的设计动作。**在补上之前，这句说明必须显示在页面上**：
   用户看到「成功率 83%」时会当真，而这个数字的分母可能包含别的 agent
   干的活。只写在代码注释里等于没写。

   ── 0/0 不是 0% ───────────────────────────────────────────────────
   一次都没跑过时显示「—」。`0%` 会被读成「装了但一次都没成」，而事实是
   「还没开始」。这是 `percent(null)` 那条既有口径的延续。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import StateBadge from '@/domains/shared/components/StateBadge.vue'
import type { AgentDetail } from '@/domains/agent/api'
import { getAgentDetail } from '@/domains/agent/api'
import { settledCount } from '@/domains/device/api'
import { describeReadFailure } from '@/shared/http'
import { percent, relativeTime } from '@/shared/format'
import { navigate, route } from '@/router'

/**
 * agent 名从**地址**读，不走 props——与其余页面同一口径（见 `DeviceDetailPage`）。
 */
const agentName = computed(() => route.value.params.name ?? '')

const detail = ref<AgentDetail | null>(null)
const loading = ref(true)
const error = ref<string | null>(null)

/**
 * 这个 agent **确实哪台机器上都没装**（404）。
 *
 * 与 `error` 分开：读失败是「现在不知道」，404 是「知道了——没装」。混在
 * 一条红条里的话，用户看到的是 `读 Agent 详情失败（not_found）`，然后得到一个
 * 永远不会成功的「重试」按钮——而真正该说的是「去设备上装一个，Hub 不代装」。
 */
const notInstalled = ref(false)

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  notInstalled.value = false
  try {
    detail.value = await getAgentDetail(agentName.value)
  } catch (err) {
    detail.value = null
    if ((err as { code?: string } | null)?.code === 'not_found') {
      notInstalled.value = true
    } else {
      error.value = describeReadFailure(err, 'Agent 详情')
    }
  } finally {
    loading.value = false
  }
}

/** 执行状态的徽标口径：与 `shared/state` 的 `execution` 族一致。 */
const runs = computed(() => detail.value?.task_history ?? [])
const breakdown = computed(() => detail.value?.breakdown ?? null)

/**
 * 成功率的可读值。
 *
 * `success_rate === null` → 「—」。**不用 `?? 0`**：那会把「还没跑过」
 * 变成「一次都没成」，而后者是一个需要用户去修的问题。
 */
const rate = computed(() => percent(detail.value?.success_rate ?? null))

const settled = computed(() => settledCount(breakdown.value))

function openProject(projectId: string): void {
  navigate(`/project/${encodeURIComponent(projectId)}/board`)
}

function openDevice(nodeId: string): void {
  navigate(`/devices/${encodeURIComponent(nodeId)}`)
}
</script>

<template>
  <div class="af-page af-agd">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">
          {{ agentName }}
          <span
            v-if="detail"
            class="af-badge"
            :class="detail.available ? 'af-badge--ok' : 'af-badge--off'"
          >
            {{ detail.available ? '现在能对话' : '装了但跑不起来' }}
          </span>
        </h1>
        <p class="af-page__sub">
          这个 agent 装在哪几台机器上、跑过哪些任务、成功率多少。
          <b>Agent 视角</b>与设备视角是同一批数据的两个问法。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">
        刷新
      </button>
    </div>

    <div v-if="error" class="af-inline-error" role="alert">
      {{ error }}
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
    </div>

    <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载 Agent 详情">
      <div v-for="i in 3" :key="i" class="af-skeleton af-skeleton--row"></div>
    </div>

    <EmptyState
      v-else-if="notInstalled"
      glyph="◇"
      title="这台机器上没装它"
      text="Agent 由用户自己在设备上安装，Hub 不代装。装好之后下一次心跳就会出现在这里。"
    />

    <!-- 读失败是「现在不知道」，与上面那个「知道了，没装」分开。 -->
    <EmptyState
      v-else-if="!detail"
      glyph="◌"
      title="读不出这个 agent"
      text="Hub 这次没答上来。agent 本身可能没问题——重试一次往往就好了。"
    />

    <template v-else>
      <div v-if="detail.error" class="af-inline-error" role="alert">
        全都不行：{{ detail.error }}
      </div>

      <!-- ① 成功率：最先要看的那个数字 -->
      <section class="af-region" aria-label="成功率">
        <div class="af-region__head">
          <h2 class="af-region__title">成功率</h2>
          <span class="af-spacer"></span>
          <span class="af-region__note">
            只算已结束的执行；进行中的不进分母
          </span>
        </div>
        <div class="af-region__body">
          <div class="af-agd__rate">
            <span class="af-agd__rate-value af-num">{{ rate }}</span>
            <span class="af-agd__rate-detail af-t-mute">
              <template v-if="settled === 0">
                还没跑过任何任务——所以是「—」，不是 0%（0/0 不是一个比例）
              </template>
              <template v-else>
                已结束 {{ settled }} 次：成功 {{ breakdown?.succeeded }} ·
                失败 {{ breakdown?.failed }} · 中止 {{ breakdown?.aborted }}
              </template>
            </span>
          </div>
          <p class="af-t-mute af-agd__note">{{ detail.task_history_caveat }}</p>
        </div>
      </section>

      <!-- ② 装在哪几台机器上 -->
      <section class="af-region" aria-label="安装位置">
        <div class="af-region__head">
          <h2 class="af-region__title">装在哪几台机器上</h2>
          <span class="af-spacer"></span>
          <span class="af-region__note">{{ detail.devices.length }} 台</span>
        </div>
        <div class="af-region__body">
          <ul class="af-agd__devs">
            <li v-for="d in detail.devices" :key="d.node_id" class="af-agd__dev">
              <button class="af-agd__dev-name" type="button" @click="openDevice(d.node_id)">
                {{ d.device_name }}
              </button>
              <span class="af-badge" :class="d.available ? 'af-badge--ok' : 'af-badge--off'">
                {{ d.available ? '能对话' : '跑不起来' }}
              </span>
              <span class="af-t-mute af-agd__dev-runs af-num">{{ d.executions }} 次执行</span>
              <span v-if="d.probe_error" class="af-t-mute af-agd__dev-err">
                {{ d.probe_error }}
              </span>
            </li>
          </ul>
        </div>
      </section>

      <!-- ③ 跑过的任务 -->
      <section class="af-region" aria-label="跑过的任务">
        <div class="af-region__head">
          <h2 class="af-region__title">跑过的任务</h2>
          <span class="af-spacer"></span>
          <span class="af-region__note">最近的在前</span>
        </div>
        <div class="af-region__body">
          <EmptyState
            v-if="runs.length === 0"
            glyph="⌗"
            title="还没有跑过任务"
            text="这个 agent 所在设备上还没有任何执行记录。派一个任务过去，这里就会有历史。"
          />
          <table v-else class="af-table af-agd__table">
            <thead>
              <tr>
                <th>任务</th>
                <th>项目</th>
                <th>设备</th>
                <th>状态</th>
                <th>时间</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="run in runs" :key="run.execution_id">
                <td>
                  <button
                    v-if="run.task_title"
                    class="af-agd__link"
                    type="button"
                    @click="openProject(run.project_id)"
                  >
                    {{ run.task_title }}
                  </button>
                  <span v-else class="af-t-mute" title="任务行已不存在（执行记录不给 task_id 加外键）">
                    任务已不在（{{ run.task_id }}）
                  </span>
                </td>
                <td>{{ run.project_name }}</td>
                <td class="af-fp">{{ run.device_id }}</td>
                <td><StateBadge :state="run.state" /></td>
                <td class="af-t-mute">{{ relativeTime(run.created_at) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
.af-agd__rate {
  display: flex;
  align-items: baseline;
  gap: var(--af-space-4);
  flex-wrap: wrap;
}
.af-agd__rate-value {
  font-size: var(--af-text-2xl);
  font-weight: 600;
  color: var(--af-fg-primary);
  min-width: 3ch;
}
.af-agd__rate-detail {
  font-size: var(--af-text-sm);
  line-height: 1.6;
}
.af-agd__note {
  margin: var(--af-space-3) 0 0;
  font-size: var(--af-text-sm);
  line-height: 1.6;
}
.af-agd__devs {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-agd__dev {
  display: flex;
  align-items: baseline;
  gap: var(--af-space-3);
  font-size: var(--af-text-sm);
  flex-wrap: wrap;
}
.af-agd__dev-name {
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: var(--af-accent);
  cursor: pointer;
  text-decoration: underline;
  text-underline-offset: 2px;
}
.af-agd__dev-runs {
  font-size: var(--af-text-sm);
}
.af-agd__dev-err {
  font-size: var(--af-text-sm);
  overflow-wrap: anywhere;
}
.af-agd__table {
  width: 100%;
  font-size: var(--af-text-sm);
}
.af-agd__link {
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: var(--af-accent);
  cursor: pointer;
  text-align: left;
  text-decoration: underline;
  text-underline-offset: 2px;
}
</style>
