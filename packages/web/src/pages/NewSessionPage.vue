<script setup lang="ts">
/* ===========================================================================
   NewSessionPage —— 新建会话（规范 §5.3 / §7.0）
   ---------------------------------------------------------------------------
   这一页存在的意义是**让工作区路径成为流程中的一步，而不是一个地方**。
   路径只在第三步出现一次，建立后不可改；全页没有任何链接指向「工作区」。

   ── 三步，但不是向导 ──────────────────────────────────────────────
   三步在同一列里上下排开，右栏是常驻摘要。不是「下一步 / 上一步」的分页
   向导：用户需要同时看到已选什么才能判断下一步选什么，而分页会把前面
   的选择藏起来。三步都不承接焦点（只是一条进度指示），键盘用户不会为了
   看进度多按三次 Tab。

   ── ② 为什么要在这里选 agent ─────────────────────────────────────
   会话能不能对话，判据只有 `agent_id`：工作台的「对话」目标在没有
   `agent_id` 时是不可选的。所以这一步不是装饰——**跳过它建出来的会话
   是一个永远无法对话的死会话**，而它看起来是「建成功了」。

   ── 不选要显式，不能默认 ──────────────────────────────────────────
   后端允许 `agent_id` 为空（M1 的会话本来就可以没有 agent，这条不退化），
   但页面必须让用户**自己点一下「不指定」**，并把后果写在旁边。默认不选
   等于替用户做了一个他没有做过的决定。

   ── 两档，不是一档 ────────────────────────────────────────────────
   装了但跑不起来的（`Probe::NotCapable`）**列出来但不可选**，并把设备给的
   原因带出来。删掉它就回到「用户以为系统没扫到」，而那条四态设计唯一的
   目的正是让人看见「为什么我的 agent 不能对话」。
   =========================================================================== */

import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import StateBadge from '@/components/StateBadge.vue'
import type { AgentSummary, DeviceSummary } from '@/lib/api'
import { canCreateSession, createSession, createSessionErrorLabel, listAgents, listDevices } from '@/lib/api'
import { relativeTime } from '@/lib/format'
import { ApiError, describeReadFailure } from '@/lib/http'
import { attach, type ListboxHandle } from '@/lib/listbox'
import { navigate, route } from '@/router'

const devices = ref<DeviceSummary[]>([])
const agents = ref<AgentSummary[]>([])
const loading = ref(true)
const loadError = ref<string | null>(null)

const pickedDeviceId = ref<string | null>(route.value.query.get('device'))
const pickedPath = ref('')
/**
 * 选中的 agent。`null` = **还没选**，`''` = 用户**显式选了「不指定」**。
 *
 * 这两态必须分开：都是「空」的话，页面无法区分「用户还没决定」与
 * 「用户决定不要 agent」，于是「不指定」这个决定的后果（会话不能对话）
 * 永远说不出来——而那条后果正是本步存在的理由。
 */
const pickedAgent = ref<string | null>(null)
const submitting = ref(false)
const submitError = ref<string | null>(null)

const pickedDevice = computed(() => devices.value.find((d) => d.id === pickedDeviceId.value) ?? null)
const deviceUsable = computed(() => pickedDevice.value !== null && canCreateSession(pickedDevice.value))
const pathValid = computed(() => /^(\/|~)/.test(pickedPath.value.trim()))

/**
 * 这台设备上**能对话**的 agent 名。
 *
 * 取 `usable_agents`（与后端的 409 门禁同源）而不是 `agents`：`agents` 里
 * 含「装了但跑不起来」的那些，把它们列进可选项等于让用户建一个死会话。
 */
const usableOnDevice = computed(() => pickedDevice.value?.usable_agents ?? [])
/**
 * 这台设备**装了但现在不能对话**的 agent，连带原因。
 *
 * 口径是「这台设备装了的（`agents`）减去能用的（`usable_agents`）」，**不是**
 * 「目录里没出现在这台设备可用清单里的」——后者会把「装在别的设备上、
 * 这台根本没装」的那些也算成装坏了，那是谎报。
 *
 * 灰显但**不删**：正是这些行在回答「为什么我的 agent 不能对话」。
 */
const brokenOnDevice = computed(() => {
  const usable = new Set(usableOnDevice.value)
  return (pickedDevice.value?.agents ?? [])
    .filter((name) => !usable.has(name))
    .map((name) => {
      const known = agents.value.find((a) => a.name === name)
      return { name, error: known?.error ?? null }
    })
})
/** 建出来的会话能不能对话——`agent_id` 是唯一判据。 */
const willConverse = computed(() => (pickedAgent.value ?? '').length > 0)
/** 缺哪一项，错误就指哪一项（不是笼统的「请填写完整」）。 */
const missing = computed(() => {
  const out: string[] = []
  if (!pickedDevice.value) out.push('device')
  if (pickedAgent.value === null) out.push('agent')
  if (!pathValid.value) out.push('path')
  return out
})
const canSubmit = computed(() => missing.value.length === 0 && deviceUsable.value && !submitting.value)

const devList = ref<HTMLElement | null>(null)
let devHandle: ListboxHandle | null = null

onMounted(async () => {
  await load()
  if (devList.value) {
    devHandle = attach(devList.value, {
      role: 'list',
      onEnter: (row) => {
        const id = row.dataset.deviceId
        if (id) pickDevice(id)
        return true
      },
      onToggle: (row) => {
        const id = row.dataset.deviceId
        if (id) pickDevice(id)
        return true
      },
    })
  }
})

onBeforeUnmount(() => {
  devHandle?.detach()
  devHandle = null
})

async function load(): Promise<void> {
  loading.value = true
  loadError.value = null
  try {
    const [devs, ags] = await Promise.all([listDevices(), listAgents()])
    devices.value = devs
    agents.value = ags
  } catch (err) {
    loadError.value = describeReadFailure(err, '设备列表')
  } finally {
    loading.value = false
  }
}

function pickDevice(id: string): void {
  const device = devices.value.find((d) => d.id === id)
  if (!device) return
  pickedDeviceId.value = id
  // 换设备只清路径：路径是「这台设备上的」一个目录，换机器就不成立了。
  pickedPath.value = ''
  // 但 agent **必须重新选**：能用的那批是「这台设备上的」，换机器之后
  // 原先选中的那个可能根本没装。留着旧选择等于让门禁去兜一个页面
  // 本来就能当场发现的错误。
  pickedAgent.value = null
  submitError.value = null
}

/**
 * 选一个 agent。`''` 是显式的「不指定」。
 *
 * 不选不拦着（后端允许空 `agent_id`，M1 的会话本来就可以没有 agent），
 * 但后果写在页面上——不写的话用户会以为「创建成功 = 能对话」。
 */
function pickAgent(name: string): void {
  pickedAgent.value = name
  submitError.value = null
}

async function submit(): Promise<void> {
  const device = pickedDevice.value
  if (!device || !pathValid.value || submitting.value) return
  if (!deviceUsable.value) {
    submitError.value = '这台设备现在离线，Hub 连不上它，会话建不起来。'
    return
  }
  submitting.value = true
  submitError.value = null
  try {
    const session = await createSession({
      node_id: device.id,
      path: pickedPath.value.trim(),
      // `''`（显式不选）要落成 `null` 而不是空串：库里 `agent_id` 是
      // `NULL` 表示「没有 agent」，空串是一句不同的话（会被当成有值）。
      agent_id: pickedAgent.value || null,
    })
    // `replace`：提交过的表单不该留在后退路径上
    navigate(`/sessions/${encodeURIComponent(session.id)}`, { replace: true })
  } catch (err) {
    submitError.value =
      err instanceof ApiError ? createSessionErrorLabel(err.code) : '建会话没有成功：Hub 没有回应。'
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <div class="af-page af-wizard">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">新建会话</h1>
        <p class="af-page__sub">
          选设备 → 选 agent → 选路径。路径是<b>创建期的一次性输入，建立后只读</b>——
          换路径就是换一个会话。这个页面没有任何「打开工作区」的入口。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">刷新</button>
    </div>

    <div v-if="loadError" class="af-inline-error" role="alert">
      {{ loadError }}
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
    </div>

    <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载设备与 agent">
      <div v-for="i in 3" :key="i" class="af-skeleton af-skeleton--row"></div>
    </div>

    <EmptyState
      v-else-if="devices.length === 0"
      glyph="▣"
      title="没有设备可以开会话"
      text="会话必须落在某台设备上。先在设备上跑 afcli run &lt;hub-url&gt; 接入一台，它出现在设备页之后就能在这里选。"
    >
      <a class="af-btn" href="/devices" @click.prevent="navigate('/devices')">看设备</a>
    </EmptyState>

    <div v-else class="af-picker">
      <div class="af-col gap-8">
        <!-- ① 设备 -->
        <section aria-labelledby="h-dev">
          <div class="af-row gap-4 af-sechead">
            <h2 id="h-dev" class="af-t-label">① 设备</h2>
            <span class="af-spacer"></span>
            <span class="af-t-mute af-keyhint"><kbd>↑</kbd><kbd>↓</kbd> 移动 · <kbd>⏎</kbd> 选中</span>
          </div>
          <div
            ref="devList"
            class="af-opts"
            data-role="list"
            tabindex="0"
            role="listbox"
            :aria-invalid="pickedDevice ? 'false' : 'true'"
            aria-label="选择设备（↑↓ 移动，⏎ 选中）"
          >
            <button
              v-for="d in devices"
              :key="d.id"
              class="af-opt"
              type="button"
              data-row
              tabindex="-1"
              role="option"
              :data-device-id="d.id"
              :aria-selected="pickedDeviceId === d.id"
              :data-inert="canCreateSession(d) ? undefined : 'true'"
              @click="pickDevice(d.id)"
            >
              <span class="af-opt__main">
                <StateBadge :state="d.status" :derived="d.derived" />
                <b>{{ d.name }}</b>
                <span class="af-badge">{{ d.platform || '平台未上报' }}</span>
                <span class="af-badge" :class="d.agents.length ? 'af-badge--agent' : 'af-badge--off'">
                  {{ d.agents.length ? `${d.agents.length} 个 agent` : '未上报 agent' }}
                </span>
                <span class="af-t-mute af-opt__meta">心跳 {{ relativeTime(d.last_seen) }}</span>
              </span>
              <span v-if="!canCreateSession(d)" class="af-opt__why">已判离线 · 会话建不起来</span>
            </button>
          </div>
          <p class="af-field__hint">
            离线设备仍然列出来：它不该从候选里消失，用户需要知道「这台机器存在，但现在够不着」。
            「重连中」仍可建会话——设备上的进程还活着，只是 Hub 刚刚断开（45 秒内）。
          </p>
        </section>

        <!-- ② agent -->
        <section aria-labelledby="h-agent">
          <div class="af-row gap-4 af-sechead">
            <h2 id="h-agent" class="af-t-label">② agent</h2>
            <span class="af-badge af-badge--agent">决定这个会话能不能对话</span>
            <span class="af-spacer"></span>
            <span class="af-t-mute af-keyhint">数据源 /api/agents · 只看这台设备</span>
          </div>

          <p v-if="!pickedDevice" class="af-field__hint">先选一台设备：能用的 agent 是「那台机器上装了什么」。</p>

          <template v-else>
            <div v-if="usableOnDevice.length === 0" class="af-agentnote">
              <p>
                <b>这台设备上现在没有能对话的 agent。</b>
                Hub 不替你在设备上装东西——<b>agent 由你自己装</b>，装好后它在下一次心跳里出现。
              </p>
            </div>

            <div class="af-opts" data-role="agent-choice">
              <button
                v-for="a in usableOnDevice"
                :key="a"
                class="af-opt"
                type="button"
                :data-agent-option="a"
                :aria-pressed="pickedAgent === a ? 'true' : 'false'"
                @click="pickAgent(a)"
              >
                <span class="af-opt__main">
                  <span class="af-radio" aria-hidden="true" :data-on="pickedAgent === a ? 'true' : 'false'"></span>
                  <b>{{ a }}</b>
                </span>
              </button>
              <!-- 「不指定」恒在，连一个可用 agent 都没有时也必须在：
                   会话没有 agent 仍然成立（终端 / exec 照常可用），
                   少了这一项就等于「没有 agent 就不能开会话」。 -->
              <button
                class="af-opt"
                type="button"
                data-agent-option=""
                :aria-pressed="pickedAgent === '' ? 'true' : 'false'"
                @click="pickAgent('')"
              >
                <span class="af-opt__main">
                  <span class="af-radio" aria-hidden="true" :data-on="pickedAgent === '' ? 'true' : 'false'"></span>
                  <b>不指定 agent</b>
                </span>
                <span class="af-opt__why">终端 / exec 可用，但不能对话</span>
              </button>
            </div>

            <p v-if="brokenOnDevice.length" class="af-field__hint">
              这台设备上探到但用不了的（<span v-for="a in brokenOnDevice" :key="a.name" class="af-badge af-badge--off">{{
                a.name
              }}<span v-if="a.error">：{{ a.error }}</span></span>）——灰在旁边不可选，但不等于看不见。
            </p>
            <p v-else-if="usableOnDevice.length" class="af-field__hint">
              清单来自设备扫描，只列现在能对话的那些（<code>agent_runtime.available</code>）。
            </p>
            <p v-if="pickedAgent === ''" class="af-field__error" role="alert">
              这个会话将<b>不能对话</b>：没有 agent 就没有对话。终端与 exec 照常可用。
            </p>
          </template>
        </section>

        <!-- ③ 路径 -->
        <section aria-labelledby="h-path">
          <div class="af-row gap-4 af-sechead">
            <h2 id="h-path" class="af-t-label">③ 工作区路径</h2>
            <span class="af-spacer"></span>
            <span class="af-t-mute af-keyhint">创建期输入 · 建立后只读</span>
          </div>
          <div class="af-field">
            <label class="af-field__label" for="path">路径（绝对路径，必须以 / 或 ~ 开头）</label>
            <input
              id="path"
              v-model="pickedPath"
              class="af-input af-fp"
              placeholder="~/src/AgentFleet"
              autocomplete="off"
              spellcheck="false"
              :aria-invalid="pickedPath && !pathValid ? 'true' : 'false'"
              aria-describedby="pathhint"
              @input="submitError = null"
            />
            <p v-if="pickedPath && !pathValid" class="af-field__error">
              路径要以 / 或 ~ 开头。相对路径在不同设备上指的不是同一个地方。
            </p>
            <p v-else id="pathhint" class="af-field__hint">
              路径必须<b>已经存在于选中的设备上</b>。这里没有内容寻址仓库，也没有工作区供料——
              供料能力已整体撤出，不在任何阶段；所以建不出一个空工作区，空工作区看起来像成功了，
              进去才发现没有项目文件。
            </p>
          </div>
        </section>

        <hr class="af-hairline" />

        <div class="af-row gap-4 af-submit">
          <button class="af-btn af-btn--primary af-btn--lg" type="button" :disabled="!canSubmit" @click="submit">
            {{ submitting ? '建立中…' : '创建会话' }}
          </button>
          <a class="af-btn af-btn--lg" href="/sessions" @click.prevent="navigate('/sessions')">取消</a>
          <span class="af-spacer"></span>
          <span class="af-t-mute af-keyhint">三项齐了才能建；缺哪项，错误就指哪一项</span>
        </div>
        <p v-if="submitError" class="af-field__error" role="alert">{{ submitError }}</p>
        <p v-else-if="pickedDevice && !deviceUsable" class="af-field__error" role="alert">
          这台设备已判离线，会话建不起来。等它重新上线，或换一台设备。
        </p>
        <p v-else-if="missing.length && (pickedPath || pickedDevice)" class="af-t-mute af-keyhint">
          {{
            missing.includes('device')
              ? '还差：选一台设备。'
              : missing.includes('agent')
                ? '还差：选一个 agent（或显式选「不指定」）。'
                : '还差：填一条路径。'
          }}
        </p>
      </div>

      <aside class="af-summary" aria-label="本次会话的已选项">
        <div class="af-t-label">会话摘要</div>
        <div class="af-sumrow">
          <span class="af-sum__k">设备</span>
          <span class="af-sum__v" :data-empty="!pickedDevice">{{ pickedDevice?.name ?? '未选' }}</span>
        </div>
        <div class="af-sumrow">
          <span class="af-sum__k">agent</span>
          <span class="af-sum__v" :data-empty="!willConverse">
            {{ pickedAgent === null ? '未选' : willConverse ? pickedAgent : '不指定（不能对话）' }}
          </span>
        </div>
        <div class="af-sumrow">
          <span class="af-sum__k">路径</span>
          <span class="af-sum__v af-fp" :data-empty="!pathValid">{{ pathValid ? pickedPath : '未选' }}</span>
        </div>
        <hr class="af-hairline" />
        <p class="af-field__hint">
          建立后<b>不能换设备、不能换 agent、路径不可改</b>——轨迹、基线与产物都记在
          <span class="af-fp">(node_id, path)</span> 上。换环境就是新会话。
        </p>
        <hr class="af-hairline" />
        <p class="af-field__hint">
          装 agent 不归我们做：agent 由用户自己装在这台设备上，我们只扫描并配置。
          托管覆盖层是 M4 且已推迟——这个页面只有「选」，没有「装」，也没有「打开工作区」。
        </p>
      </aside>
    </div>
  </div>
</template>

<style scoped>
.af-wizard {
  max-width: 1080px;
}
.af-picker {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 260px;
  gap: var(--af-space-9);
  align-items: start;
}
.af-sechead {
  margin-bottom: var(--af-space-4);
}
/* 键位提示 `.af-keyhint` 的字号住在 `styles/components.css` §2：新建页与会话
   列表页共用同一档，各存一份就是两份逐字相同的副本。

   下面 `.af-opts` 是这一页独有的「三步选项行」形状，不跟着搬。 */
.af-opts {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
/* 选项行是真 button：Tab 到容器、方向键在行间走（listbox.ts 负责），
   行本身仍要能被指针直接点。 */
.af-opt {
  display: flex;
  align-items: flex-start;
  gap: var(--af-space-4);
  width: 100%;
  padding: var(--af-space-3) var(--af-space-4);
  text-align: left;
  background: var(--af-surface-1);
  border: 1px solid var(--af-border-default);
  border-left: 2px solid var(--af-border-default);
  border-radius: var(--af-radius-md);
  font-family: inherit;
  font-size: var(--af-text-md);
  color: var(--af-fg-secondary);
  cursor: pointer;
  transition: background-color var(--af-dur-instant) linear, border-color var(--af-dur-instant) linear;
}
.af-opt:hover {
  background: var(--af-surface-2);
  border-color: var(--af-border-strong);
  color: var(--af-fg-primary);
}
.af-opt[aria-selected='true'] {
  background: var(--af-accent-bg);
  border-color: var(--af-accent-line);
  border-left-color: var(--af-accent);
  color: var(--af-fg-primary);
}
/* 离线是「考虑过但现在不行」，不是「不存在」：降亮 + 原因，仍可选中 */
.af-opt[data-inert='true'] {
  color: var(--af-fg-tertiary);
}
.af-opt[data-inert='true'] .af-opt__main b {
  color: var(--af-fg-tertiary);
}
.af-opt__main {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: var(--af-space-3);
  flex-wrap: wrap;
}
.af-opt__main b {
  color: var(--af-fg-primary);
  font-weight: var(--af-weight-medium);
}
.af-opt__meta {
  font-size: var(--af-text-sm);
}
.af-opt__why {
  flex: 0 0 auto;
  align-self: center;
  font-size: var(--af-text-sm);
  color: var(--af-danger-fg);
}
/* 单选的圆点。用 `aria-pressed` 而不是原生 `<input type=radio>`：这一组里
   每一项都是完整的一行（带说明文字），原生 radio 只包得住标签本身。 */
.af-radio {
  flex: 0 0 auto;
  width: 10px;
  height: 10px;
  border: 1px solid var(--af-border-strong);
  border-radius: 50%;
}
.af-radio[data-on='true'] {
  border-color: var(--af-accent);
  background: var(--af-accent);
}
.af-agentnote {
  padding: var(--af-space-4) var(--af-space-5);
  background: var(--af-surface-1);
  border: 1px solid var(--af-border-default);
  border-left: 2px solid var(--af-border-strong);
  border-radius: var(--af-radius-md);
  font-size: var(--af-text-md);
  line-height: var(--af-leading-base);
  color: var(--af-fg-secondary);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
.af-agentnote p {
  margin: 0;
}
.af-agentnote code {
  font-family: var(--af-font-mono);
  background: var(--af-surface-2);
  padding: 0 var(--af-space-1);
  border-radius: var(--af-radius-sm);
}
.af-submit {
  flex-wrap: wrap;
}
.af-summary {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
  padding: var(--af-space-5);
  background: var(--af-surface-1);
  border: 1px solid var(--af-border-default);
  border-radius: var(--af-radius-md);
  position: sticky;
  top: var(--af-space-6);
}
.af-sumrow {
  display: flex;
  align-items: baseline;
  gap: var(--af-space-3);
  font-size: var(--af-text-md);
}
.af-sum__k {
  flex: 0 0 46px;
  font-size: var(--af-text-sm);
  letter-spacing: var(--af-tracking-label);
  text-transform: uppercase;
  color: var(--af-fg-disabled);
}
.af-sum__v {
  min-width: 0;
  color: var(--af-fg-primary);
  overflow-wrap: anywhere;
}
.af-sum__v[data-empty='true'] {
  color: var(--af-fg-disabled);
}
@media (max-width: 900px) {
  .af-picker {
    grid-template-columns: minmax(0, 1fr);
  }
  .af-summary {
    position: static;
  }
}
</style>
