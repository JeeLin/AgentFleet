<script setup lang="ts">
/* ===========================================================================
   AgentsPage —— Agent 目录（规范 §5.3 / §7.6）
   ---------------------------------------------------------------------------
   数据源是设备心跳上报的 `capabilities.agents`。

   **M2 起这个页面不再必然为空**：设备会探测本机装了哪些 agent 并上报
   （要求 1）。上一条注释（「M1 的设备没有 agent 运行时可探测，所以这里恒空」）
   描述的前提已被推翻。M1 时 `node-core` 的心跳故意留空并加了测试锁定
   「未探测到就上报空，不得编造」——**「不编造」这条仍然有效**，只是现在
   真的有东西可报了：探测不到就仍是空，探测到就列出来。

   列表含义是「这台设备上装了哪些 agent」，**不是**「哪些现在能用」：
   `/api/agents` 用 `agent_list()` 归并了所有设备的上报，装了但跑不起来的
   也在其中（页面要让人看见自己装坏的那个）。真正的可用性门禁在建会话那一步
   （未装或不可用 → 409）。
   =========================================================================== */

import { onMounted, ref } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import type { AgentSummary, DeviceSummary } from '@/lib/api'
import { listAgents, listDevices } from '@/lib/api'
import { describeReadFailure } from '@/lib/http'
import { navigate } from '@/router'

const agents = ref<AgentSummary[]>([])
const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const error = ref<string | null>(null)

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    const [ags, devs] = await Promise.all([listAgents(), listDevices()])
    agents.value = ags
    devices.value = devs
  } catch (err) {
    error.value = describeReadFailure(err, 'Agent 目录')
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div class="af-page">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">Agent 目录</h1>
        <p class="af-page__sub">
          这里列出各设备上<b>已装</b>的 agent，不是「支持哪些 agent」。已装但跑不起来的
          也会列出（你自己装坏的那个要能看见）；能否用它开会话，在建会话时判定。
          数据来自设备心跳上报的 <span class="af-fp">capabilities.agents</span>。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">刷新</button>
    </div>

    <!-- 空态也要说清「为什么空」，否则验收时会被当成「做了但永远空」 -->
    <div class="af-stage-note" role="note">
      <span class="af-badge af-badge--warn">取决于设备装了什么</span>
      <div class="af-stage-note__body">
        <p>
          M2 起设备会探测本机装了哪些 agent 并随心跳上报，所以这一页<b>不再必然为空</b>：
          设备上装了 opencode，这里就会多一行。
          <b>探测不到就是空</b>——设备不会编造一个清单，Hub 也不会。
        </p>
        <p class="af-t-mute">
          安装入口 <b>不在这里</b>：从 Hub 下发安装需要「选设备 + 选模板」，
          那是单独的一步，且**永远由用户手动触发**，不会因为「发现某台设备缺某个 agent」
          就自动装。
        </p>
      </div>
    </div>

    <div v-if="error" class="af-inline-error" role="alert">
      {{ error }}
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
    </div>

    <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载 Agent 目录">
      <div v-for="i in 3" :key="i" class="af-skeleton af-skeleton--row"></div>
    </div>

    <EmptyState
      v-else-if="agents.length === 0"
      glyph="☰"
      title="还没有设备上报 agent"
      :text="`${devices.length} 台设备里没有任何一台上报 agent。设备上装一个（例如 opencode）后，它会在下次心跳里出现。探测不到就是空，设备和 Hub 都不会编造。`"
    >
      <button class="af-btn" type="button" @click="navigate('/sessions/new')">新建会话</button>
    </EmptyState>

    <template v-else>
      <div class="af-roster">
        <div v-for="a in agents" :key="a.name" class="af-agent">
          <span class="af-badge af-badge--agent">{{ a.name }}</span>
          <span class="af-spacer"></span>
          <button class="af-btn af-btn--sm" type="button" @click="navigate('/sessions/new')">
            用它开会话
          </button>
        </div>
      </div>
      <p class="af-t-mute af-foot">
        定义来自各设备心跳的上报；本页只读，安装入口不在这一页。
      </p>
    </template>
  </div>
</template>

<style scoped>
.af-stage-note {
  display: flex;
  align-items: flex-start;
  gap: var(--af-space-4);
  padding: var(--af-space-4) var(--af-space-5);
  margin-bottom: var(--af-space-8);
  background: var(--af-surface-1);
  border: 1px solid var(--af-border-default);
  border-left: 2px solid var(--af-warn);
  border-radius: var(--af-radius-md);
}
.af-stage-note__body {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  font-size: var(--af-text-md);
  line-height: var(--af-leading-base);
  color: var(--af-fg-secondary);
}
.af-stage-note__body p {
  margin: 0;
}
.af-roster {
  display: flex;
  flex-direction: column;
  gap: 1px;
  border: 1px solid var(--af-border-subtle);
  border-radius: var(--af-radius-md);
  overflow: hidden;
}
.af-agent {
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  padding: var(--af-space-2) var(--af-space-4);
  background: var(--af-surface-1);
  height: var(--af-row-regular);
  border-bottom: 1px solid var(--af-border-subtle);
}
.af-agent:last-child {
  border-bottom: 0;
}
.af-foot {
  margin-top: var(--af-space-4);
  font-size: var(--af-text-sm);
}
</style>
