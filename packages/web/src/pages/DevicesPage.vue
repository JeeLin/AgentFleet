<script setup lang="ts">
/* ===========================================================================
   DevicesPage —— 设备列表（规范 §5.3）
   ---------------------------------------------------------------------------
   这一页只有一个会话入口：「用它开会话」。**没有面板 / 工作区入口**：
   终端与文件绑定会话的工作区 `(node_id, path)`，生命周期随会话（约束 3）。

   状态只看心跳年龄：45 秒内是 `reconnecting`（Hub 内存推导值，带「推」徽标），
   超过 45 秒判 `offline`。这个判据来自 `security-and-lifecycle.md` §5，不是
   页面自己定的。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import DeviceTable from '@/components/DeviceTable.vue'
import type { DeviceSummary } from '@/lib/api'
import { listDevices } from '@/lib/api'
import { describeReadFailure } from '@/lib/http'
import { percent, relativeTime } from '@/lib/format'
import { navigate } from '@/router'

const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const error = ref<string | null>(null)
const inspected = ref<DeviceSummary | null>(null)

const online = computed(() => devices.value.filter((d) => d.status === 'online'))
const reconnecting = computed(() => devices.value.filter((d) => d.status === 'reconnecting'))
const offline = computed(() => devices.value.filter((d) => d.status === 'offline'))

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    devices.value = await listDevices()
  } catch (err) {
    error.value = describeReadFailure(err, '设备列表')
  } finally {
    loading.value = false
  }
}

/** 行 ⏎：把详情显示在表格上方的一块内联区，而不是模态。
 *  模态会锁焦点，而「扫一眼设备」不该打断用户本来在做的事。 */
function inspect(device: DeviceSummary): void {
  inspected.value = inspected.value?.id === device.id ? null : device
}

function createSession(device: DeviceSummary): void {
  navigate(`/sessions/new?device=${encodeURIComponent(device.id)}`)
}
</script>

<template>
  <div class="af-page">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">设备</h1>
        <p class="af-page__sub">
          每台设备一条 WebSocket 主动外联，Hub 不需要任何到设备的入站端口。
          状态只看心跳：45 秒内没心跳但连接刚断是「重连中」，超过 45 秒判「离线」。
          终端与文件是<b>会话内</b>的面板——这一页没有打开它们的入口。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">
        刷新
      </button>
    </div>

    <div class="af-row gap-4 af-dev-summary">
      <span class="af-t-mute"><span class="af-dot" style="color: var(--af-ok)"></span> 在线 {{ online.length }}</span>
      <span class="af-t-mute"><span class="af-dot" style="color: var(--af-warn)"></span> 重连中 {{ reconnecting.length }}</span>
      <span class="af-t-mute"><span class="af-dot" style="color: var(--af-danger)"></span> 离线 {{ offline.length }}</span>
      <span class="af-spacer"></span>
      <span class="af-t-mute af-t-num" style="font-size: var(--af-text-sm)">共 {{ devices.length }} 台</span>
    </div>

    <div v-if="error" class="af-inline-error" role="alert">
      {{ error }}
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
    </div>

    <!-- 加载态是骨架，不是空态：两者混用会让用户以为设备没了（§4.8） -->
    <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载设备">
      <div v-for="i in 4" :key="i" class="af-skeleton af-skeleton--row"></div>
    </div>

    <EmptyState
      v-else-if="devices.length === 0"
      glyph="▣"
      title="还没有设备接入"
      text="在设备上跑一条命令就会出现在这里：afcli run &lt;hub-url&gt;。首次启动会自动注册并生成密钥对，Hub 只存指纹。"
    />

    <template v-else>
      <DeviceTable :devices="devices" :on-inspect="inspect" :on-create="createSession" />

      <section v-if="inspected" class="af-inspect" aria-label="设备详情">
        <div class="af-row gap-4">
          <span class="af-panel__title">{{ inspected.name }}</span>
          <span class="af-badge">{{ inspected.platform || '平台未上报' }}</span>
          <span v-if="inspected.gpu" class="af-badge af-badge--ok">GPU</span>
          <span class="af-spacer"></span>
          <button class="af-btn af-btn--sm af-btn--ghost" type="button" @click="inspected = null">
            收起
          </button>
        </div>
        <div class="af-kv">
          <span class="af-kv__k">node_id</span>
          <span class="af-kv__v af-fp af-selectable">{{ inspected.id }}</span>
          <span class="af-kv__k">密钥指纹</span>
          <span class="af-kv__v af-fp">{{ inspected.key_fingerprint }}</span>
          <span class="af-kv__k">afcli</span>
          <span class="af-kv__v">{{ inspected.agent_version || '未上报' }}</span>
          <span class="af-kv__k">最近心跳</span>
          <span class="af-kv__v">{{ relativeTime(inspected.last_seen) }}</span>
          <span class="af-kv__k">资源</span>
          <span class="af-kv__v">
            CPU {{ percent(inspected.cpu) }} · 内存 {{ percent(inspected.mem) }}
            <span v-if="inspected.cpu === null" class="af-t-mute">（未上报，不是 0%）</span>
          </span>
          <span class="af-kv__k">凭证</span>
          <span class="af-kv__v af-t-mute">
            设备私钥只在设备上生成、永不上传；Hub 存的只有上面那串指纹。
          </span>
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
.af-dev-summary {
  margin-bottom: var(--af-space-5);
  font-size: var(--af-text-sm);
}
.af-inspect {
  margin-top: var(--af-space-5);
  padding: var(--af-space-4) var(--af-space-5);
  background: var(--af-surface-1);
  border: 1px solid var(--af-border-default);
  border-radius: var(--af-radius-md);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
.af-kv {
  display: grid;
  grid-template-columns: 88px 1fr;
  gap: var(--af-space-2) var(--af-space-4);
  font-size: var(--af-text-sm);
}
.af-kv__k {
  color: var(--af-fg-tertiary);
  text-transform: uppercase;
  letter-spacing: var(--af-tracking-label);
  font-size: var(--af-text-sm);
  padding-top: 1px;
}
.af-kv__v {
  color: var(--af-fg-secondary);
  overflow-wrap: anywhere;
}
</style>
