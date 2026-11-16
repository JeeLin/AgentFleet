<script setup lang="ts">
/* ===========================================================================
   AgentCatalog —— Agent 目录（规范 §5.3 / §7.6）
   ---------------------------------------------------------------------------
   数据源是设备心跳上报的 `capabilities.agents`。

   ── 为什么它是组件而不是页面 ──────────────────────────────────────
   用户裁定「只要区分会话 / 设备 / 设置」，`Agent 目录` 并进设备页、`/agents`
   路由删除。理由不是「页数少一点」：设备页与 Agent 目录看的是**同一个对象**
   ——「哪台机器上有什么」。分成两条路由时用户要在两个地址之间跳，而两者
   拉的是同一批设备、同一份心跳事实。

   所以本组件**不带页面外壳**（没有 `h1`、没有 `af-page`）：它渲染在
   `DevicesPage` 的一个分段里，标题与刷新按钮归那一页管。

   ── 列表含义是「装了哪些」而不是「哪些能用」 ──────────────────────
   `/api/agents` 用 `agent_list()` 归并了所有设备的上报，**装了但跑不起来的
   也在其中**（页面要让人看见自己装坏的那个）。真正的可用性门禁在建会话那
   一步（未装或不可用 → 409）。

   注意与 M2 的一处收紧对照：设备**不再上报没装的 agent**（`Probe::NotInstalled`
   不进心跳）。所以这里「有这一行」= 「这台机器上确实有这个可执行文件」，
   而 `available: false` = 「装了，但跑不起来 / 没有 ACP 入口」。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import type { AgentSummary } from '@/domains/agent/api'
import type { DeviceSummary } from '@/domains/device/api'
import { listAgents } from '@/domains/agent/api'
import { listDevices } from '@/domains/device/api'
import { describeReadFailure } from '@/shared/http'
import { navigate } from '@/router'

const emit = defineEmits<{
  /**
   * 用户点了「用它开会话」。**由父页面决定去哪**：目录不该知道路由。
   *
   * v0.8.0：触发它的两个按钮都恒禁用（没有可达的会话入口，见文件头），
   * 所以这里暂时没有调用点。声明保留是给 v0.8.2「从设备开工」用——那时
   * 把按钮的 `disabled` 去掉即可，父页面的处理约定不必重新设计。
   */
  (e: 'start-session'): void
}>()
// 显式标注「暂时没有发起点」，免得 lint 把它当遗漏删掉。
void emit

const agents = ref<AgentSummary[]>([])
const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const error = ref<string | null>(null)

/**
 * 现在能对话的那批。
 *
 * 单独算一次是为了给分区标题写「可用 X · 不可用 Y」——这个数字与列表里
 * `data-usable` 的着色**必须同源**，否则会出现「标题说可用 2、列表里灰了 3 行」
 * 这种自相矛盾的画面。两边都读 `a.available`。
 */
const usable = computed(() => agents.value.filter((a) => a.available))

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

/** 父页面的刷新按钮要能一起刷（设备页顶部只有一个刷新）。 */
defineExpose({ load })

/**
 * 进 agent 详情页。
 *
 * 由本组件自己 `navigate` 而不 emit 给父页面：父页面（设备页的 Agent 分段）
 * 没有别的信息可加，转一手只是多一层中转。开会话那条仍走 `emit`——那个动作
 * 的去向（新建页）属于父页面的决定，与这里不同。
 */
function openDetail(name: string): void {
  navigate(`/agents/${encodeURIComponent(name)}`)
}
</script>

<template>
  <div class="af-catalog">
    <!-- 空态也要说清「为什么空」，否则验收时会被当成「做了但永远空」 -->
    <div class="af-stage-note" role="note">
      <span class="af-badge af-badge--warn">取决于设备装了什么</span>
      <div class="af-stage-note__body">
        <p>
          各设备探测本机装了哪些 agent 并随心跳上报，所以这里<b>不再必然为空</b>：
          设备上装了 opencode，这里就会多一行。
          <b>探测不到就是空</b>——设备不会编造一个清单，Hub 也不会。
        </p>
        <p class="af-t-mute">
          安装入口 <b>不在这里</b>：agent 由你自己在设备上装，Hub 只下发启动配置，
          <b>从不往设备写可执行文件</b>。
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
      <button
        class="af-btn"
        type="button"
        disabled
        aria-disabled="true"
        title="从 agent 开会话需要先选项目，v0.8.2 上线；现在请到项目里新建会话。"
      >
        新建会话
      </button>
    </EmptyState>

    <template v-else>
      <!-- ①「设备上的 Agent」这一块的标题与计数（原型 `prototype/agents.html:36`）。
           放在 `v-else` 里而不是页面外壳：标题里的「N 台设备上的 M 个 Agent」
           要用本组件刚读到的两份真数据算，父页面拿不到。加载中 / 读失败 / 空态
           时**不显示**——那三种情况下这个计数没有意义（空态自己会说「为什么空」）。 -->
      <div class="af-region__head af-catalog__head">
        <h2 class="af-region__title">
          {{ devices.length }} 台设备上的 {{ agents.length }} 个 Agent
        </h2>
        <span class="af-region__note">
          可用 {{ usable.length }} · 不可用 {{ agents.length - usable.length }}
        </span>
      </div>
      <div class="af-roster">
        <div
          v-for="a in agents"
          :key="a.name"
          class="af-agent"
          :data-usable="a.available ? 'true' : 'false'"
        >
          <!-- 名字是进详情页的入口（v0.7.0 子任务 7）：详情页有「装在哪几台
               机器上 / 跑过哪些任务 / 成功率」，那些放不进一行。 -->
          <button
            class="af-agent__link"
            type="button"
            data-act="detail"
            :title="`看 ${a.name} 装在哪几台机器上、跑过哪些任务`"
            :data-agent-detail="a.name"
            @click="openDetail(a.name)"
          >
            {{ a.name }}
          </button>
          <!-- 不可用的原因**写在这一行**：只把它灰掉，用户看到的是
               「我装了它但系统说没有」，而真正的答案是「它跑不起来」。 -->
          <span v-if="!a.available" class="af-t-mute af-agent__why">
            {{ a.error ?? '现在不能对话（设备没给原因）' }}
          </span>
          <span class="af-spacer"></span>
          <button
            class="af-btn af-btn--sm"
            type="button"
            data-act="start"
            disabled
            aria-disabled="true"
            :title="
              a.available
                ? '从 agent 开会话需要先选项目，v0.8.2 上线；现在请到项目里新建会话。'
                : '这个 agent 现在不能对话'
            "
          >
            用它开会话
          </button>
        </div>
      </div>
      <p class="af-t-mute af-foot">
        定义来自各设备扫描的上报；本组件只读，安装入口不在这里。灰的那批是
        <b>装了但现在不能对话</b>的——它们留在这里正是为了让你看见自己装坏的那个。
      </p>
    </template>
  </div>
</template>

<style scoped>
/* 阶段横幅。**刻意不搬进 `styles/components.css`**：`ConvFlow.vue` 有一份同名
   块，但那是贴在对话流上的一条（只画 `border-bottom`、正文 xs），这份是页面
   里的一张独立卡片（四边描边 + 圆角 + `margin-bottom`、正文 md）。两份只有
   «语言» 同源（警告色左边线 + 徽标），形状不是一回事——按「逐字重复」合并会
   让其中一个变错。判据钉在 `src/styles/components.test.ts`。 */
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
/* 分区标题行：`.af-region__head` 自带下边框与内边距，但它是为「有边框的
   `.af-region` 容器」写的。这里没有外层容器（列表自己就是边框），所以只
   借它的排版，把下边框去掉——否则标题下会多一条与列表上边框重叠的线。 */
.af-catalog__head {
  border-bottom: 0;
  padding-left: 0;
  padding-right: 0;
  background: none;
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
/* 不可用的那档整行降亮，但**不删**：原因要看得见 */
.af-agent[data-usable='false'] {
  color: var(--af-fg-tertiary);
}
.af-agent__link {
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  font-weight: 600;
  color: var(--af-accent);
  cursor: pointer;
  text-decoration: underline;
  text-underline-offset: 2px;
}
.af-agent__why {
  min-width: 0;
  font-size: var(--af-text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.af-foot {
  margin-top: var(--af-space-4);
  font-size: var(--af-text-sm);
}
</style>
