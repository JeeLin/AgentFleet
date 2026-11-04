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

   ── 身份锚是 node_id，不是指纹 ────────────────────────────────────
   这一行画的身份锚**必须与详情块第一行是同一个值**（`DevicesPage.vue` 的
   `af-inspect`）。以前这里画的是 `key_fingerprint`、`title` 却是 `d.id`：
   悬停与眼睛看到的是两个标识，用户拿其中一个去对 `afcli` 输出或报障时
   有一半对不上。设备的身份是 `node_id`（`api.ts`：「永不变」），而指纹是
   Hub 打码的（前 3 + `***` + 后 3）。指纹不丢，搬到 `title` 里——它是
   凭证问题要看的那串，不是身份。
   =========================================================================== */

import { onBeforeUnmount, onMounted, ref } from 'vue'

import StateBadge from '@/domains/shared/components/StateBadge.vue'
import type { DeviceSummary } from '@/domains/device/api'
import { canCreateSession } from '@/domains/device/api'
import { attach, type ListboxHandle } from '@/shared/listbox'
import { percent, relativeTime } from '@/shared/format'

const props = defineProps<{
  devices: DeviceSummary[]
  /** 行 ⏎：看这一台的详情。 */
  onInspect: (device: DeviceSummary) => void
  /** 用它开会话。 */
  onCreate: (device: DeviceSummary) => void
  /** 确认移除这一台。 */
  onRemove: (device: DeviceSummary) => void
}>()

/**
 * 正在等待二次确认的设备 id。
 *
 * 行内确认而不是模态：删设备**不级联删会话**（用户裁定的「删除语义三分离」），
 * 所以它不是一个把数据带走的高危动作——但它也不可撤销（设备要重新 enroll 才会
 * 回到列表里）。行内一档就够，不需要一个锁住整页焦点的对话框。
 *
 * `null` = 没有正在确认的行。确认态只存 id 不存整行：刷新列表之后那一行可能
 * 已经不在，此时留着旧对象会让「确认」按钮指向一台不存在的设备。
 */
const confirming = ref<string | null>(null)

function askRemove(id: string): void {
  confirming.value = id
}

function cancelRemove(): void {
  confirming.value = null
}

function confirmRemove(device: DeviceSummary): void {
  confirming.value = null
  props.onRemove(device)
}

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

/**
 * 这一行的身份锚。**与详情块第一行同一个值**（`DevicesPage.vue` 的 `af-inspect`）。
 * 理由见文件头「身份锚是 node_id，不是指纹」。
 */
function anchor(device: DeviceSummary): string {
  return device.id
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
            <!-- 设备名本身是进详情页的入口（v0.7.0 子任务 7）：详情页有绑定关系 /
                 磁盘 / 失败归因，那些是**行里放不下**的内容。用按钮而不是整行可点，
                 因为整行已经有「⏎ 看详情」的内联展开，两种展开方式撞在一起
                 会让用户不知道自己刚才点到了哪个。 -->
            <button
              class="af-dev-name__link af-truncate"
              type="button"
              :title="`看这台设备的绑定、磁盘与失败归因`"
              :data-detail="d.id"
              @click.stop="onInspect(d)"
            >
              {{ d.name }}
            </button>
            <span
              class="af-fp af-t-mute af-dev-id"
              :title="`密钥指纹：${d.key_fingerprint}`"
              >{{ anchor(d) }}</span
            >
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
            <!-- 确认态：一句话说清「删什么、不删什么」，因为「删除」这个词会让
                 人以为会话也没了。两个按钮都是 `type="button"`，在表格里默认会
                 变成 submit。 -->
            <template v-if="confirming === d.id">
              <span class="af-dev-confirm af-t-mute">移除设备行，会话保留</span>
              <button
                class="af-btn af-btn--sm af-btn--danger"
                type="button"
                :data-confirm="d.id"
                @click.stop="confirmRemove(d)"
              >
                确认移除
              </button>
              <button class="af-btn af-btn--sm af-btn--ghost" type="button" @click.stop="cancelRemove">
                取消
              </button>
            </template>
            <template v-else>
              <button
                class="af-btn af-btn--sm af-btn--primary"
                type="button"
                data-act="create"
                :disabled="!canCreateSession(d)"
                :aria-disabled="!canCreateSession(d)"
                :title="offlineReason(d) || `在这台设备上开一个会话`"
                @click.stop="onCreate(d)"
              >
                用它开会话
              </button>
              <button
                class="af-btn af-btn--sm af-btn--ghost af-dev-remove"
                type="button"
                :title="`把 ${d.name} 从设备列表里移除（它上面的会话与轨迹都保留）`"
                :data-remove="d.id"
                @click.stop="askRemove(d.id)"
              >
                移除
              </button>
            </template>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
/* 表格外框 `.af-tablewrap` 在 `styles/components.css` §8：两张表共用同一个框，
   各存一份就是两份逐字相同的副本。

   竖向居中而不是基线对齐：两个 span 不同字号（名字 13px / node_id 10px），
   baseline 对齐时基线与视觉中心不重合，node_id 会偏离格子中心。 */
.af-dev-name__link {
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: var(--af-accent);
  cursor: pointer;
  text-align: left;
  text-decoration: underline;
  text-underline-offset: 2px;
  max-width: 100%;
}
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
.af-dev-remove:hover {
  color: var(--af-danger);
}
.af-dev-confirm {
  font-size: var(--af-text-2xs);
  margin-right: var(--af-space-2);
}
</style>
