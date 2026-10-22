<script setup lang="ts">
/* ===========================================================================
   SessionTable —— 历史会话表（规范 §4.4 / §6.2）
   ---------------------------------------------------------------------------
   一行一个会话。路径是**普通文本**，不是可点容器——工作区没有页面
   （§7.0），把路径做成链接会立刻长出一个「工作区」目的地。

   表中没有 title 列：后端 `SessionSummary` 没有 title 字段，会话的名字就是
   工作区路径的末段（`sessionTitle`）。编一个「会话 3」会让人分不清这是
   哪个项目。
   =========================================================================== */

import { onBeforeUnmount, onMounted, ref } from 'vue'

import StateBadge from '@/components/StateBadge.vue'
import type { SessionSummary } from '@/lib/api'
import { pageSessionState } from '@/lib/api'
import { relativeTime, sessionTitle } from '@/lib/format'
import { attach, type ListboxHandle } from '@/lib/listbox'

const props = defineProps<{
  sessions: SessionSummary[]
  /** 设备名，node_id 查不到时原样显示 id。 */
  deviceName: (nodeId: string) => string
  /** 行 ⏎ / 点标题：进这个会话。 */
  onOpen: (session: SessionSummary) => void
}>()

const list = ref<HTMLElement | null>(null)
const selectedId = ref<string | null>(null)
let handle: ListboxHandle | null = null

onMounted(() => {
  if (!list.value) return
  handle = attach(list.value, {
    role: 'list',
    onEnter: (row) => {
      const id = row.dataset.sessionId
      const session = props.sessions.find((s) => s.id === id)
      if (session) props.onOpen(session)
      return true
    },
  })
})

onBeforeUnmount(() => {
  handle?.detach()
  handle = null
})
</script>

<template>
  <div class="af-tablewrap">
    <table class="af-table">
      <thead>
        <tr>
          <th>会话</th>
          <th style="width: 132px">设备</th>
          <th style="width: 96px">agent</th>
          <th style="width: 230px">路径</th>
          <th style="width: 100px">状态</th>
          <th style="width: 100px">开始</th>
          <th style="width: 96px"></th>
        </tr>
      </thead>
      <tbody
        ref="list"
        data-role="list"
        tabindex="0"
        role="listbox"
        aria-label="会话列表（↑↓ 移动，⏎ 进入）"
      >
        <tr
          v-for="s in sessions"
          :key="s.id"
          data-row
          tabindex="-1"
          role="option"
          :aria-selected="selectedId === s.id"
          :data-session-id="s.id"
          @focus="selectedId = s.id"
        >
          <td>
            <button class="af-link af-sess-title" type="button" @click.stop="onOpen(s)">
              {{ sessionTitle(s) }}
            </button>
            <span class="af-t-mute af-fp af-sess-id">{{ s.id }}</span>
          </td>
          <td>{{ deviceName(s.workspace.node_id) }}</td>
          <td>
            <span v-if="s.agent_id" class="af-badge af-badge--agent">{{ s.agent_id }}</span>
            <span v-else class="af-t-mute" title="M1 不接 agent 运行时（ACP 归 M3），会话不绑 agent">—</span>
          </td>
          <td class="af-fp af-truncate" :title="s.workspace.path">{{ s.workspace.path }}</td>
          <td><StateBadge :state="pageSessionState(s.state)" /></td>
          <td class="af-t-mute">{{ relativeTime(s.created_at) }}</td>
          <td class="af-acts">
            <button class="af-btn af-btn--sm" type="button" @click.stop="onOpen(s)">打开</button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.af-tablewrap {
  border: 1px solid var(--af-border-subtle);
  border-radius: var(--af-radius-md);
  overflow: auto;
}
/* 标题是真按钮：Enter 的语义属于行，但「点标题进会话」是用户最自然的手势，
   而且按钮天然键盘可达，不必再给行内造一条不可见的点击热区。

   「画成链接」的那份样式在 `src/page-chrome.css`（全局 `.af-link`）——
   认证页与这里此前各写一份，而且已经分叉成两种外观。这里只留**布局**上
   与表格单元格有关的那几条。 */
.af-sess-title {
  margin-right: var(--af-space-3);
  font-size: var(--af-text-sm);
  text-align: left;
}
.af-sess-id {
  font-size: var(--af-text-2xs);
}
.af-acts {
  text-align: right;
  white-space: nowrap;
}
</style>
