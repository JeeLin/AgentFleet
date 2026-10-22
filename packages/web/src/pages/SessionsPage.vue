<script setup lang="ts">
/* ===========================================================================
   SessionsPage —— 会话列表（规范 §5.3 / §5.5）
   ---------------------------------------------------------------------------
   **这一页没有多会话 tab 条。** tab 是工作台**内部**的切换器（§5.5），
   不是列表页的一种列。原型 `sessions.html:59` 那一行 tab 条是原型与规范
   不一致的一处，规范是终稿——以规范为准。

   分区：进行中（`running`）与历史（`ended`）。断线**不**从进行区挪走：
   它在设备上还活着，只是 Hub 暂时够不着，进去是只读的（§7.3）。
   本 M1 的会话不绑 agent 运行时，`disconnected` 没有数据源，所以只会有
   `active` / `ended` 两种。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import SessionTable from '@/components/SessionTable.vue'
import type { DeviceSummary, SessionSummary } from '@/lib/api'
import { listDevices, listSessions } from '@/lib/api'
import { sessionTitle } from '@/lib/format'
import { describeReadFailure } from '@/lib/http'
import { navigate } from '@/router'

const sessions = ref<SessionSummary[]>([])
const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const error = ref<string | null>(null)
const query = ref('')

const deviceNames = computed(() => new Map(devices.value.map((d) => [d.id, d.name])))

function deviceName(nodeId: string): string {
  return deviceNames.value.get(nodeId) ?? nodeId
}

const filtered = computed(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return sessions.value
  return sessions.value.filter((s) =>
    [sessionTitle(s), deviceName(s.workspace.node_id), s.workspace.path, s.agent_id ?? '', s.id]
      .join(' ')
      .toLowerCase()
      .includes(q),
  )
})

const active = computed(() => filtered.value.filter((s) => s.state === 'running'))
const ended = computed(() => filtered.value.filter((s) => s.state === 'ended'))
const hasAny = computed(() => sessions.value.length > 0)

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    // 两个端点并发：设备名是列表里的一列，等它回来才画会让整张表闪一下
    const [list, devs] = await Promise.all([listSessions(), listDevices()])
    sessions.value = list
    devices.value = devs
  } catch (err) {
    error.value = describeReadFailure(err, '会话列表')
  } finally {
    loading.value = false
  }
}

function open(session: SessionSummary): void {
  navigate(`/sessions/${encodeURIComponent(session.id)}`)
}

function clearQuery(): void {
  query.value = ''
  document.querySelector<HTMLInputElement>('[data-role="search"]')?.focus()
}
</script>

<template>
  <div class="af-page">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">会话</h1>
        <p class="af-page__sub">
          一个会话 = 一台设备上的一条工作区路径。路径是会话的属性，不是一个能单独
          打开的地方——所以这里没有「工作区」这一栏，也没有第二条绕开会话的入口。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">刷新</button>
    </div>

    <div class="af-listbar">
      <a
        class="af-btn af-btn--primary af-btn--lg"
        href="/sessions/new"
        @click.prevent="navigate('/sessions/new')"
      >
        <span aria-hidden="true">✚</span> 新建会话
      </a>
      <input
        v-model="query"
        class="af-input af-search"
        type="search"
        placeholder="搜标题 / 设备 / 路径"
        aria-label="搜索会话"
        data-role="search"
        autocomplete="off"
        spellcheck="false"
      />
      <span class="af-spacer"></span>
      <span class="af-t-mute af-keyhint">
        <kbd>↑</kbd><kbd>↓</kbd> 在列表里走 · <kbd>⏎</kbd> 进入会话
      </span>
    </div>

    <div v-if="error" class="af-inline-error" role="alert">
      {{ error }}
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
    </div>

    <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载会话">
      <div v-for="i in 5" :key="i" class="af-skeleton af-skeleton--row"></div>
    </div>

    <!-- 一个会话都没有：空态要说清「为什么空、什么时候会有」，并给出路 -->
    <EmptyState
      v-else-if="!hasAny"
      glyph="◈"
      title="还没有会话"
      text="会话在一台在线设备上创建，绑定它的工作区路径。先在设备页确认有一台设备在线，然后新建会话。"
    >
      <a class="af-btn af-btn--primary" href="/sessions/new" @click.prevent="navigate('/sessions/new')">
        新建会话
      </a>
      <a class="af-btn" href="/devices" @click.prevent="navigate('/devices')">看设备</a>
    </EmptyState>

    <!-- 有会话但搜索没匹配 -->
    <EmptyState
      v-else-if="filtered.length === 0"
      glyph="◌"
      title="没有匹配的会话"
      text="标题、设备名、路径和会话 id 都能匹配。换个词试试。"
    >
      <button class="af-btn af-btn--sm" type="button" @click="clearQuery">清空搜索</button>
    </EmptyState>

    <template v-else>
      <section v-if="active.length" aria-labelledby="h-active">
        <div class="af-sectionhead">
          <h2 id="h-active" class="af-t-label">进行中</h2>
          <span class="af-badge af-badge--ok">{{ active.length }}</span>
          <span class="af-spacer"></span>
          <span class="af-t-mute af-hint">同时开多个会话后，工作台里用 tab 条切换——切 tab 不释放通道</span>
        </div>
        <SessionTable :sessions="active" :device-name="deviceName" :on-open="open" />
      </section>

      <section v-if="ended.length" aria-labelledby="h-ended" :class="{ 'af-gap': active.length > 0 }">
        <div class="af-sectionhead">
          <h2 id="h-ended" class="af-t-label">历史</h2>
          <span class="af-badge">{{ ended.length }}</span>
          <span class="af-spacer"></span>
          <span class="af-t-mute af-hint">
            已结束的会话留档；工作区不再可达，面板不会再建立通道
          </span>
        </div>
        <SessionTable :sessions="ended" :device-name="deviceName" :on-open="open" />
      </section>
    </template>
  </div>
</template>

<style scoped>
.af-listbar {
  display: flex;
  align-items: center;
  gap: var(--af-space-4);
  margin-bottom: var(--af-space-8);
}
.af-search {
  width: 260px;
}
.af-keyhint {
  font-size: var(--af-text-2xs);
}
.af-sectionhead {
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  margin-bottom: var(--af-space-3);
}
.af-hint {
  font-size: var(--af-text-2xs);
}
.af-gap {
  margin-top: var(--af-space-11);
}
</style>
