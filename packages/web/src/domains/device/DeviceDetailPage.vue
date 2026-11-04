<script setup lang="ts">
/* ===========================================================================
   DeviceDetailPage —— 设备详情（`prototype/devices.html`，v0.7.0 子任务 7）
   ---------------------------------------------------------------------------
   这一页回答「**这台机器怎么了**」，是设备视角。项目页回答「这个项目该跑在
   哪台机器上」——两个问题都要能问，所以设备页在新主轴上**变多而不是变少**。

   四块内容：
   1. 机器本身（状态 / 平台 / 资源 / 指纹）—— 原来就有的，一块不少
   2. 绑定关系：这台机器上跑了哪些项目、各自钉在哪个目录
   3. 磁盘：配额与用量
   4. 失败归因：栽在哪一类
   5. 能不能删：**说出理由**，不只是把按钮灰掉

   ── 三个「空」必须分开 ────────────────────────────────────────────
   页面上最容易被写成同一个空白的三种状态，这里各有一个：

   | 状态 | 显示 |
   |---|---|
   | 还在加载 | 骨架（不是空态，§4.8） |
   | 真的没有 | 「还没有绑定项目」+ 一句怎么绑 |
   | 读不出来 | 红条 + 重试 |

   混用会让用户以为数据没了，而实际只是慢了一秒。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import StateBadge from '@/domains/shared/components/StateBadge.vue'
import type { DeviceBinding, DeviceDetail } from '@/domains/device/api'
import { describeCleanupPolicy, getDeviceDetail, settledCount } from '@/domains/device/api'
import { describeReadFailure } from '@/shared/http'
import { percent, relativeTime } from '@/shared/format'
import { navigate, route } from '@/router'

/**
 * 设备 id 从**地址**读，不走 props。
 *
 * 与其余页面同一口径（`SessionPage` / `ExecutionPage` 都是这么做的）：
 * `App.vue` 用 `<component :is="page">` 渲染，不传任何 props。为这两页单独
 * 开一条 props 通路，会让「页面参数从哪来」在仓里变成两个答案。
 */
const deviceId = computed(() => route.value.params.id ?? '')

const detail = ref<DeviceDetail | null>(null)
const loading = ref(true)
const error = ref<string | null>(null)

/**
 * 这台设备**确实不存在**（404）。
 *
 * 与 `error` 分开是必须的：读失败是「现在不知道」，可以说「过一会儿再试」；
 * 404 是「知道了——没有这台设备」，重试一千次也还是 404。把两者塞进同一条
 * 红条，页面就会给一个永远不会成功的「重试」按钮，而那行字（`读设备详情失败
 * （not_found）`）对用户毫无意义——它既没说设备没了，也没说会话还在。
 *
 * 404 在设备页是一个**有信息量的结局**：删设备不删会话（用户 2026-10-26 的
 * 「删除语义三分离」），所以「找不到这台设备」后面必须接一句「会话还在」。
 */
const gone = ref(false)

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  gone.value = false
  try {
    detail.value = await getDeviceDetail(deviceId.value)
  } catch (err) {
    detail.value = null
    if ((err as { code?: string } | null)?.code === 'not_found') {
      gone.value = true
    } else {
      error.value = describeReadFailure(err, '设备详情')
    }
  } finally {
    loading.value = false
  }
}

const summary = computed(() => detail.value?.summary ?? null)
const bindings = computed<DeviceBinding[]>(() => detail.value?.bindings ?? [])
const breakdown = computed(() => detail.value?.failure_breakdown ?? null)

/**
 * 失败归因的展示顺序。**写死**，不按数量排：按数量排会让同一页每次刷新
 * 都换一次顺序，用户没法比较两次看到的东西。
 *
 * 「中止」单列且措辞注明「不是失败」——混进失败的话，一台只是被用户主动停了
 * 两次的机器会显示「失败 2 次」，用户会去查一个不存在的问题。
 */
const failureRows = computed(() => {
  const b = breakdown.value
  if (!b) return []
  return [
    { key: 'succeeded', label: '成功', value: b.succeeded },
    { key: 'failed', label: '失败', value: b.failed },
    { key: 'aborted', label: '中止（不是失败）', value: b.aborted },
    { key: 'in_flight', label: '进行中（不进成功率）', value: b.in_flight },
  ]
})

/** 失败是否集中在某一类。集中才说「集中在」，否则如实说「偶发」。 */
const failureHeadline = computed(() => {
  const b = breakdown.value
  if (!b) return ''
  const total = settledCount(b)
  if (total === 0) return '还没有跑过任何任务，所以还没有归因可说。'
  if (b.failed === 0) return '没有失败记录。'
  return b.failed > b.succeeded
    ? '失败多于成功，这台机器的环境可能有问题。'
    : '失败少于成功，看起来是偶发。'
})

/** 打开某个项目的看板。设备页是管理面，不带项目上下文，所以这里显式跳过去。 */
function openProject(projectId: string): void {
  navigate(`/project/${encodeURIComponent(projectId)}/board`)
}

function bytes(n: number | null): string {
  if (n === null) return '—'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let v = n
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i += 1
  }
  return `${i === 0 ? v : v.toFixed(1)} ${units[i]}`
}
</script>

<template>
  <div class="af-page af-devd">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">
          {{ summary?.name ?? '设备' }}
          <StateBadge
            v-if="summary"
            :state="summary.status"
            :derived="summary.derived"
          />
        </h1>
        <p class="af-page__sub">
          这台机器在线吗、绑了哪些项目、磁盘还剩多少、失败集中在哪一类。
          <b>这是设备视角</b>——项目页是同一个项目的另一个视角，两个都要能看。
        </p>
      </div>
      <span class="af-spacer"></span>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="load">
        刷新
      </button>
    </div>

    <div v-if="error" class="af-inline-error" role="alert">
      {{ error }}
      <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
    </div>

    <!-- 加载态是骨架，不是空态：混用会让用户以为设备没了（§4.8） -->
    <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载设备详情">
      <div v-for="i in 3" :key="i" class="af-skeleton af-skeleton--row"></div>
    </div>

    <EmptyState
      v-else-if="gone"
      glyph="▣"
      title="找不到这台设备"
      text="它可能已经被移除。设备被删掉之后，它上面的会话与轨迹仍然保留——去会话列表还能找到。"
    />

    <!-- 读失败是「现在不知道」，与上面那个「知道了，没有」分开。 -->
    <EmptyState
      v-else-if="!detail"
      glyph="◌"
      title="读不出这台设备"
      text="Hub 这次没答上来。设备本身可能没问题——重试一次往往就好了。"
    />

    <template v-else>
      <!-- ① 机器本身：原来就有的内容，一条不少 -->
      <section class="af-region" aria-label="机器信息">
        <div class="af-region__head">
          <h2 class="af-region__title">机器</h2>
          <span class="af-spacer"></span>
          <span class="af-region__note">
            {{ summary?.platform || '平台未上报' }} · 心跳 {{ relativeTime(summary?.last_seen) }}
          </span>
        </div>
        <div class="af-region__body">
          <div class="af-kv">
            <span class="af-kv__k">node_id</span>
            <span class="af-kv__v af-fp af-selectable" title="设备身份锚，永不改变">{{ deviceId }}</span>
            <span class="af-kv__k">密钥指纹</span>
            <span class="af-kv__v af-fp">{{ summary?.key_fingerprint }}</span>
            <span class="af-kv__k">afcli</span>
            <span class="af-kv__v">{{ summary?.node_version || '未上报' }}</span>
            <span class="af-kv__k">资源</span>
            <span class="af-kv__v">
              CPU {{ percent(summary?.cpu ?? null) }} · 内存 {{ percent(summary?.mem ?? null) }}
              <span v-if="summary?.cpu === null" class="af-t-mute">（未上报，不是 0%）</span>
            </span>
            <span class="af-kv__k">已装 agent</span>
            <span class="af-kv__v">
              <template v-if="summary && summary.agents.length > 0">
                {{ summary.agents.join('、') }}
                <span class="af-t-mute">
                  （其中 {{ summary.usable_agents.length }} 个现在能对话）
                </span>
              </template>
              <span v-else class="af-t-mute">这台机器上还没探到任何 agent</span>
            </span>
          </div>
        </div>
      </section>

      <!-- ② 绑定关系：这台机器上跑了哪些项目 -->
      <section class="af-region" aria-label="绑定关系">
        <div class="af-region__head">
          <h2 class="af-region__title">绑定关系</h2>
          <span class="af-spacer"></span>
          <span class="af-region__note">
            项目与设备是「多对多」：同一个项目可以在多台机器上各钉一个目录
          </span>
        </div>
        <div class="af-region__body">
          <EmptyState
            v-if="bindings.length === 0"
            glyph="⌗"
            title="还没有绑定项目"
            text="这台机器还没有被任何项目用起来。去项目页的「绑定」里把项目钉到这台机器上，它才会开始接活。"
          />
          <ul v-else class="af-devd__binds">
            <li v-for="b in bindings" :key="b.id" class="af-devd__bind">
              <button class="af-devd__bind-name" type="button" @click="openProject(b.project_id)">
                {{ b.project_name }}
              </button>
              <code class="af-devd__bind-dir af-selectable">{{ b.fixed_dir }}</code>
              <span class="af-devd__bind-policy af-t-mute">
                {{ describeCleanupPolicy(b.cleanup_policy) }}
              </span>
              <span class="af-devd__bind-used af-t-num">
                用量 {{ bytes(b.used_bytes) }}
                <span v-if="b.used_bytes === null" class="af-t-mute">（设备未上报）</span>
              </span>
            </li>
          </ul>
        </div>
      </section>

      <!-- ③④ 磁盘与失败归因并列：两者一起回答「这机器还能不能干活」 -->
      <div class="af-devd__pair">
        <section class="af-region" aria-label="磁盘">
          <div class="af-region__head">
            <h2 class="af-region__title">磁盘</h2>
          </div>
          <div class="af-region__body">
            <EmptyState
              v-if="!detail.disk"
              glyph="◇"
              title="还没有磁盘数据"
              text="这台机器还没有被任何项目绑定，所以没有配额与用量可说。"
            />
            <dl v-else class="af-kv">
              <dt>配额</dt>
              <dd class="af-num">
                {{ detail.disk.quota_bytes === null ? '不限额' : bytes(detail.disk.quota_bytes) }}
              </dd>
              <dt>已用</dt>
              <dd class="af-num">
                {{ bytes(detail.disk.used_bytes) }}
                <span v-if="detail.disk.used_bytes === null" class="af-t-mute">（设备未上报）</span>
              </dd>
              <dt>使用率</dt>
              <dd class="af-num">{{ percent(detail.disk.percent) }}</dd>
              <dt>还能派活吗</dt>
              <dd>
                <span v-if="detail.disk.accepts_new_work">可以继续派活</span>
                <span v-else class="af-t-mute">
                  配额为 0，一个字节都放不下——先改配额再派
                </span>
              </dd>
            </dl>
            <p class="af-t-mute af-devd__note">
              真实用量只有设备自己量得出来（它才有那个盘）。设备还没上报时这里显示「—」，
              而不是 0——0 会被读成「一点不占」。
            </p>
          </div>
        </section>

        <section class="af-region" aria-label="失败归因">
          <div class="af-region__head">
            <h2 class="af-region__title">失败归因</h2>
          </div>
          <div class="af-region__body">
            <dl class="af-kv">
              <template v-for="row in failureRows" :key="row.key">
                <dt>{{ row.label }}</dt>
                <dd class="af-num" :class="{ 'af-t-mute': row.value === 0 }">{{ row.value }} 次</dd>
              </template>
            </dl>
            <p class="af-t-mute af-devd__note">{{ failureHeadline }}</p>
          </div>
        </section>
      </div>

      <!-- ⑤ 能不能删：说出理由，不只是把按钮灰掉 -->
      <section class="af-region" aria-label="删除">
        <div class="af-region__head">
          <h2 class="af-region__title">能不能删</h2>
        </div>
        <div class="af-region__body">
          <p v-if="detail.deletable.allowed" class="af-devd__del-ok">
            没有历史执行记录，可以移除。移除之后它上面的会话与轨迹<b>仍然保留</b>——
            对话是用户的数据，设备只是一条通路。
          </p>
          <p v-else class="af-devd__del-no">
            <b>不能删除。</b>{{ detail.deletable.reason }}
          </p>
          <p class="af-t-mute af-devd__note">
            移除设备在设备列表页操作（那一页有整张表的上下文）。
            有历史执行时那一步会被拒——这里先说清楚，免得白点一次。
          </p>
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
.af-devd__binds {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
.af-devd__bind {
  display: grid;
  grid-template-columns: minmax(120px, 1fr) minmax(160px, 2fr) auto auto;
  align-items: baseline;
  gap: var(--af-space-3);
  padding-bottom: var(--af-space-3);
  border-bottom: 1px solid var(--af-border-subtle);
  font-size: var(--af-text-sm);
}
.af-devd__bind:last-child {
  border-bottom: none;
  padding-bottom: 0;
}
.af-devd__bind-name {
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: var(--af-accent);
  cursor: pointer;
  text-align: left;
  text-decoration: underline;
  text-underline-offset: 2px;
}
.af-devd__bind-dir {
  font-family: var(--af-font-mono);
  font-size: var(--af-text-sm);
  color: var(--af-fg-secondary);
  overflow-wrap: anywhere;
}
.af-devd__bind-policy {
  font-size: var(--af-text-sm);
}
.af-devd__bind-used {
  font-size: var(--af-text-sm);
  color: var(--af-fg-secondary);
  white-space: nowrap;
}
.af-devd__pair {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
  gap: var(--af-space-5);
}
.af-devd__note {
  margin: var(--af-space-3) 0 0;
  font-size: var(--af-text-sm);
  line-height: 1.6;
}
.af-devd__del-ok {
  margin: 0;
  font-size: var(--af-text-sm);
  color: var(--af-fg-secondary);
}
.af-devd__del-no {
  margin: 0;
  font-size: var(--af-text-sm);
  color: var(--af-fg-secondary);
}
.af-devd__del-no b {
  color: var(--af-danger);
}
</style>
