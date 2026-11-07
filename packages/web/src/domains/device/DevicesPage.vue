<script setup lang="ts">
/* ===========================================================================
   DevicesPage —— 设备与 Agent（规范 §5.1 / §5.3）
   ---------------------------------------------------------------------------
   这一页只有一个会话入口：「用它开会话」。**没有面板 / 工作区入口**：
   终端与文件绑定会话的工作区 `(node_id, path)`，生命周期随会话（约束 3）。

   状态只看心跳年龄：45 秒内是 `reconnecting`（Hub 内存推导值，带「推」徽标），
   超过 45 秒判 `offline`。这个判据来自 `security-and-lifecycle.md` §5，不是
   页面自己定的。

   ── 为什么「Agent 目录」并到了这里 ────────────────────────────────
   用户裁定侧栏只留**会话 / 设备 / 设置**三项，`/agents` 路由删除。理由不是
   「页数少一点」：这两块看的是**同一个对象**——「这台机器上有什么」。设备行
   说机器在不在线，Agent 目录说它上面装了哪些 agent；分两条路由时用户要在两个
   地址之间跳，而两者拉的是同一批设备、同一份心跳事实。

   分段用**查询参数** `?view=agents` 表达而不是第二条路由：分段切换是视图切换
   （DECISIONS D10），不该进历史记录栈——否则「后退」变成在分段之间来回跳。
   地址栏仍然跟着走，所以「打开 Agent 目录」这件事还能深链接（旧 `/agents`
   书签由 `App.vue` 重定向到这里，见那里的 `LEGACY_PATHS`）。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import AgentCatalog from '@/domains/agent/AgentCatalog.vue'
import EmptyState from '@/domains/shared/components/EmptyState.vue'
import DeviceTable from '@/domains/device/DeviceTable.vue'
import type { DeviceSummary } from '@/domains/device/api'
import {
  deleteDevice,
  detectDownloadPlatform,
  downloadAgentBinary,
  listDevices,
} from '@/domains/device/api'
import { describeReadFailure } from '@/shared/http'
import { percent, relativeTime } from '@/shared/format'
import { navigate, route } from '@/router'

/**
 * 当前分段。**从 `route` 读、不另外存一份**：地址栏是真相源，存一份本地
 * 状态会让「前进/后退」与页面显示的段落不一致，而症状是「地址写着 agents、
 * 屏幕上却是设备表」。
 */
type Segment = 'devices' | 'agents'
const segment = computed<Segment>(() =>
  route.value.query.get('view') === 'agents' ? 'agents' : 'devices',
)

/** 目录组件的句柄：顶部那个「刷新」要同时刷当前分段。 */
const catalog = ref<InstanceType<typeof AgentCatalog> | null>(null)

function selectSegment(next: Segment): void {
  // 视图切换：`replaceState` 不进历史栈（见文件头注释）。用 `navigate`
  // 的 `replace` 而不是 `history` 直改，好让 `route` 这个 ref 跟着更新。
  const to = next === 'agents' ? '/devices?view=agents' : '/devices'
  navigate(to, { replace: true })
}

/** 顶部刷新：只刷当前分段，不替用户做多余请求。 */
function refreshCurrent(): void {
  if (segment.value === 'agents') catalog.value?.load()
  else void load()
}

const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const error = ref<string | null>(null)
const inspected = ref<DeviceSummary | null>(null)
/** 移除失败的理由。与 `error` 分开：读失败会顶掉整张表，写失败只该提示一条。 */
const removeError = ref<string | null>(null)

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

/**
 * 从这一台 Hub 下载 `afcli`。
 *
 * 空态里那条「在设备上跑 afcli run」，前提是那台设备上**已经有** afcli。
 * 没有的人在离线/内网环境里够不到 GitHub，于是空态就成了一句空话——所以
 * 这里给它一条实际的路。
 *
 * 三种结果分别处置，**不能合并成一条「下载失败」**：
 * - 成功 → 触发浏览器下载；
 * - `agent_binary_unavailable` → 裸机跑 afhub 时磁盘上没有 afcli，这是
 *   **正常部署形态**，提示应说「这台 Hub 没配」，不是「坏了」；
 * - 其它 → 真的出错了。
 */
const downloadState = ref<'idle' | 'busy'>('idle')
const downloadError = ref<string | null>(null)

async function downloadCli(): Promise<void> {
  downloadState.value = 'busy'
  downloadError.value = null
  try {
    // 把**浏览器这台机器**的平台交给 Hub：不传时后端按 `std::env::consts::OS`
    // 回落，而 Hub 跑在 Linux 上，Windows/macOS 用户就会拿到 Linux 二进制。
    // 推断在 `api.ts` 里是纯函数，页面只读 `navigator` 这两个现成字段。
    const { blob, filename } = await downloadAgentBinary({
      ...detectDownloadPlatform(navigator.userAgent, navigator.platform),
    })
    // `URL.createObjectURL` + 一个隐藏 `<a>`：这是唯一在主流浏览器上都能
    // 触发下载、且不必让用户先存到别处的做法。用完立刻 revoke——不释放会在
    // 页面存活期间一直占着这份 blob。
    const url = URL.createObjectURL(blob)
    try {
      const a = document.createElement('a')
      a.href = url
      // 用服务端给的文件名：它带平台，用户同时有多台异构机器时分得清。
      a.download = filename
      document.body.appendChild(a)
      a.click()
      a.remove()
    } finally {
      URL.revokeObjectURL(url)
    }
  } catch (err) {
    const code = (err as { code?: string } | null)?.code
    downloadError.value =
      code === 'agent_binary_unavailable'
        ? '这台 Hub 没有可下发的 afcli：直接跑 afhub（不经容器）时它是外挂文件，需要在 Hub 机器上配 AFCLI_BINARY_PATH 指向它。'
        : describeReadFailure(err, 'afcli')
  } finally {
    downloadState.value = 'idle'
  }
}

function createSession(device: DeviceSummary): void {
  // v0.7.0 起会话地址是 `/project/:id/session/:sessionId`，所以新建页需要项目 id
  // 才能把用户送回去。设备页是**管理面**（「管理从设备进」），它本身不带项目
  // 上下文——这里只把设备带下去，项目由新建页按缺省处理（落到总览）。
  navigate(`/sessions/new?device=${encodeURIComponent(device.id)}`)
}

/**
 * 移除一台设备。
 *
 * # 成功之后**只**去掉那一行
 *
 * 会话与轨迹留在 Hub 上，所以这里不碰任何会话状态、不发 `navigate`——去动
 * 别的东西会把「会话还在」这条事实在页面上做反。列表重新拉一次是因为设备行
 * 从 Hub 上没了，本地这一份已经是旧事实。
 *
 * # 404 当成成功
 *
 * 重复点击、或另一个标签页先删了，都会回 `not_found`。目标状态已经达成，
 * 给它画一条红字只会让用户去重试一件已经做完的事。其余错误照常报出。
 */
async function remove(device: DeviceSummary): Promise<void> {
  removeError.value = null
  try {
    await deleteDevice(device.id)
  } catch (err) {
    const code = (err as { code?: string } | null)?.code
    if (code !== 'not_found') {
      removeError.value = `没能移除「${device.name}」：${describeReadFailure(err, '设备')}`
      return
    }
  }
  if (inspected.value?.id === device.id) inspected.value = null
  await load()
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
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="refreshCurrent">
        刷新
      </button>
    </div>

    <!-- 分段条：设备 / Agent。**用 tablist 语义**，因为它在两套内容之间切换
         而不是跳页；`aria-selected` 让屏幕阅读器知道当前在哪个分段。 -->
    <div class="af-row gap-2 af-seg" role="tablist" aria-label="设备页分段">
      <button
        class="af-btn af-btn--sm"
        :class="{ 'af-btn--primary': segment === 'devices' }"
        type="button"
        role="tab"
        :aria-selected="segment === 'devices'"
        @click="selectSegment('devices')"
      >
        设备
      </button>
      <button
        class="af-btn af-btn--sm"
        :class="{ 'af-btn--primary': segment === 'agents' }"
        type="button"
        role="tab"
        :aria-selected="segment === 'agents'"
        @click="selectSegment('agents')"
      >
        Agent 目录
      </button>
      <span class="af-spacer"></span>
      <span class="af-t-mute" style="font-size: var(--af-text-sm)">
        这两块看的是同一批机器：设备说它在不在线，目录说它上面装了什么
      </span>
    </div>

    <AgentCatalog
      v-if="segment === 'agents'"
      ref="catalog"
      @start-session="navigate('/sessions/new')"
    />

    <template v-else>
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

      <div v-if="removeError" class="af-inline-error" role="alert">
        {{ removeError }}
        <button class="af-btn af-btn--sm" type="button" @click="removeError = null">知道了</button>
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
      >
        <!-- 上面那句「在设备上跑 afcli run」有个前提：那台设备上得**先有**
             afcli。离线/内网环境里够不到 GitHub，那段话就成了一句空话——
             所以这里给它一条实际的路。 -->
        <button
          class="af-btn af-btn--sm"
          type="button"
          :disabled="downloadState === 'busy'"
          @click="downloadCli"
        >
          {{ downloadState === 'busy' ? '正在准备下载…' : '下载 afcli' }}
        </button>
        <p v-if="downloadError" class="af-t-mute" role="alert">{{ downloadError }}</p>
      </EmptyState>

      <template v-else>
        <DeviceTable
          :devices="devices"
          :on-inspect="inspect"
          :on-create="createSession"
          :on-remove="remove"
        />

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
            <span class="af-kv__v af-fp af-selectable" title="设备身份锚：与列表里那一行显示的是同一个值">{{ inspected.id }}</span>
            <span class="af-kv__k">密钥指纹</span>
            <span class="af-kv__v af-fp" title="Hub 打码后的凭证指纹（前 3 + *** + 后 3）：凭证问题看它，身份看 node_id">{{ inspected.key_fingerprint }}</span>
            <span class="af-kv__k">afcli</span>
            <span class="af-kv__v">{{ inspected.node_version || '未上报' }}</span>
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
    </template>
  </div>
</template>

<style scoped>
.af-seg {
  margin-bottom: var(--af-space-5);
  align-items: center;
  gap: var(--af-space-2);
}
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
