<script setup lang="ts">
/* ===========================================================================
   DeviceTable —— 设备列表（规范 §4.4 / §6.1）
   ---------------------------------------------------------------------------
   表是**列表**，不是一堆可点的格子：`tbody` 是唯一的 Tab 落点，行是
   `tabindex="-1"`，↑↓ / Home / End 在行间走，⏎ 进选中行。

   ── 两个动作，不是一个 ────────────────────────────────────────────
   「看设备」与「用它开会话」是两件事。行上的 ⏎ 是**看**（选中一行的语义是
   「我在看这一台」），建会话是行内那个显式按钮。把行点击做成跳走会让
   「扫一眼」变得昂贵，而设备页的绝大多数访问就是扫一眼。

   ── 离线行为什么仍可选中 ──────────────────────────────────────────
   `data-inert` 是**降亮但不禁用**（§3.7）：离线设备的 `node_id` 仍然要被
   复制出来排障。真正被禁用的只有「用它开会话」，且按钮旁边写清为什么。
   =========================================================================== */

import { onBeforeUnmount, onMounted, ref } from 'vue'

import StateBadge from '@/components/StateBadge.vue'
import type { DeviceSummary } from '@/lib/api'
import { canCreateSession } from '@/lib/api'
import { attach, type ListboxHandle } from '@/lib/listbox'
import { percent, relativeTime } from '@/lib/format'

const props = defineProps<{
  devices: DeviceSummary[]
  /** 行 ⏎：看这一台的详情。 */
  onInspect: (device: DeviceSummary) => void
  /** 用它开会话。 */
  onCreate: (device: DeviceSummary) => void
}>()

const list = ref<HTMLElement | null>(null)
const selectedId = ref<string | null>(null)
let handle: ListboxHandle | null = null

onMounted(() => {
  if (!list.value) return
  handle = attach(list.value, {
    role: 'list',
    rowEnter: true,
    onEnter: (row) => {
      const id = row.dataset.deviceId
      const device = props.devices.find((d) => d.id === id)
      if (device) props.onInspect(device)
      return true
    },
  })
})

onBeforeUnmount(() => {
  handle?.detach()
  handle = null
})

/** 行获得焦点即记下 id，侧栏/详情用得上。 */
function onRowFocus(id: string): void {
  selectedId.value = id
}

function offlineReason(device: DeviceSummary): string {
  return device.status === 'offline'
    ? '设备已判离线：心跳超过 45 秒没到，Hub 连不上它，会话建不起来。等它重新上线。'
    : ''
}
</script>

<template>
  <div class="af-tablewrap">
    <table class="af-table">
      <thead>
        <tr>
          <th style="width: 116px">状态</th>
          <th>名称</th>
          <th style="width: 128px">平台</th>
          <th style="width: 92px" data-num>afcli</th>
          <th style="width: 190px">已装 agent</th>
          <th style="width: 72px" data-num>CPU</th>
          <th style="width: 72px" data-num>内存</th>
          <th style="width: 92px">最近心跳</th>
          <th style="width: 160px"></th>
        </tr>
      </thead>
      <tbody
        ref="list"
        data-role="list"
        tabindex="0"
        role="listbox"
        aria-label="设备列表（↑↓ 移动，⏎ 看详情）"
      >
        <tr
          v-for="d in devices"
          :key="d.id"
          data-row
          tabindex="-1"
          role="option"
          :aria-selected="selectedId === d.id"
          :data-device-id="d.id"
          :data-inert="d.status === 'offline' ? 'true' : undefined"
          @focus="onRowFocus(d.id)"
        >
          <td><StateBadge :state="d.status" :derived="d.derived" /></td>
          <td class="af-dev-name">
            <span class="af-truncate af-dev-name__name">{{ d.name }}</span>
            <span class="af-fp af-t-mute af-dev-id" :title="d.id">{{ d.key_fingerprint }}</span>
          </td>
          <td class="af-t-mute">{{ d.platform || '—' }}</td>
          <td data-num class="af-t-mute">{{ d.node_version || '—' }}</td>
          <td>
            <!--
              两档而不是一档。**装了但跑不起来的必须留着**，只是不带
              `--agent` 的强调色：删掉它就回到「用户以为系统没扫到」。

              判据取 `usable_agents` 而不是 `agents`：前者与建会话的 409 门禁
              同源（都读 `agent_runtime` 的 `available`），所以这里看着「能对话」
              的，点进新建页一定选得到。
            -->
            <span v-if="d.agents.length" class="af-caps">
              <span
                v-for="a in d.agents"
                :key="a"
                class="af-badge"
                :class="d.usable_agents.includes(a) ? 'af-badge--agent' : 'af-badge--off'"
                :data-usable="d.usable_agents.includes(a) ? 'true' : 'false'"
                :title="
                  d.usable_agents.includes(a) ? '现在能对话' : '装了，但现在不能对话（没有可用的 ACP 入口）'
                "
                >{{ a }}</span
              >
            </span>
            <span v-else class="af-badge af-badge--off">未上报</span>
          </td>
          <td data-num :class="{ 'af-t-mute': d.cpu === null }">{{ percent(d.cpu) }}</td>
          <td data-num :class="{ 'af-t-mute': d.mem === null }">{{ percent(d.mem) }}</td>
          <td class="af-t-mute">
            <span :title="d.last_seen ?? '从未上报'">{{ relativeTime(d.last_seen) }}</span>
          </td>
          <td class="af-dev-acts">
            <button
              class="af-btn af-btn--sm af-btn--primary"
              type="button"
              :disabled="!canCreateSession(d)"
              :aria-disabled="!canCreateSession(d)"
              :title="offlineReason(d) || `在这台设备上开一个会话`"
              @click.stop="onCreate(d)"
            >
              用它开会话
            </button>
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
/* 竖向居中而不是基线对齐：两个 span 不同字号（名字 13px / 指纹 10px），
   baseline 对齐时基线与视觉中心不重合，指纹会偏离格子中心。 */
.af-dev-name {
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  color: var(--af-fg-primary);
  min-width: 0;
}
.af-dev-id {
  font-size: var(--af-text-2xs);
  letter-spacing: 0.01em;
}
/* flex 子项默认 min-width:auto，长名字不会收缩、会把整列撑宽去挤别的列。
   置 0 才让 .af-truncate 的省略号真正起作用。 */
.af-dev-name__name {
  min-width: 0;
}
.af-caps {
  display: flex;
  flex-wrap: wrap;
  gap: var(--af-space-1);
}
.af-dev-acts {
  text-align: right;
  white-space: nowrap;
}
</style>
