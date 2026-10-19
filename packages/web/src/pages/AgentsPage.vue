<script setup lang="ts">
/* ===========================================================================
   AgentsPage —— Agent 目录（规范 §5.3 / §7.6）
   ---------------------------------------------------------------------------
   这个页面在 M1 **必然是空的**，所以它必须把「为什么空」写在页面上，
   而不是做出一个「做了但永远空」的页面。

   数据源是设备心跳上报的 `capabilities.agents`（「已装且就绪」，不是
   「支持」）。M1 的设备没有 agent 运行时可探测——`node-core` 的心跳
   故意留空并加了测试锁定「未探测到就上报空，不得编造」——所以这里恒空是
   正确行为，不是加载失败、也不是功能没做完。

   M3 接 ACP 之后，这里会填上按设备分组的就绪实例。
   =========================================================================== */

import { onMounted, ref } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import type { AgentSummary, DeviceSummary } from '@/lib/api'
import { listAgents, listDevices } from '@/lib/api'
import { ApiError, NetworkError } from '@/lib/http'
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
    if (err instanceof NetworkError) error.value = 'Hub 连不上。检查 Hub 进程是否还在。'
    else if (err instanceof ApiError) error.value = `读 Agent 目录失败（${err.code}）。`
    else error.value = '读 Agent 目录失败。'
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
          这里列出的是各设备<b>已装且就绪</b>的 agent，不是「支持哪些 agent」。
          数据来自设备心跳上报的 <span class="af-fp">capabilities.agents</span>。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">刷新</button>
    </div>

    <!-- 这条横幅是本页存在的前提：不写它，验收时会被当成「做了但永远空」 -->
    <div class="af-stage-note" role="note">
      <span class="af-badge af-badge--warn">M1 无数据</span>
      <div class="af-stage-note__body">
        <p>
          <b>M1 没有 agent 运行时可探测</b>，所以这一页在 M1 恒定为空：
          M1 的设备侧只实现了 terminal / files / exec 三个 connector，
          agent 运行时的 ACP 会话层归 M3。设备心跳不会编造一个 agent 清单，
          所以这里也不显示任何行。
        </p>
        <p class="af-t-mute">
          会话在这一页之外建：M1 的会话是<b>页面级容器</b>（工作区 + 面板），
          agent 由你在设备侧自行启动，Hub 不接管它的生命周期。
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
      title="M1 无数据，M3 起填充"
      :text="`${devices.length} 台设备里没有任何一台上报 agent——这是 M1 的预期结果。M3 接 ACP 之后，这里会出现按设备分组的就绪实例，以及它们各自的会话入口。`"
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
        定义来自各设备心跳的上报；M1 只读、不提供安装入口（装 agent 是 M5 的流程）。
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
  font-size: var(--af-text-sm);
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
  font-size: var(--af-text-xs);
}
.af-skel {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-skel .af-skeleton {
  height: var(--af-row-compact);
}
.af-inline-error {
  display: flex;
  align-items: center;
  gap: var(--af-space-4);
  padding: var(--af-space-3) var(--af-space-4);
  border: 1px solid var(--af-danger-line);
  background: var(--af-danger-bg);
  color: var(--af-danger-fg);
  border-radius: var(--af-radius-md);
  font-size: var(--af-text-sm);
}
</style>
