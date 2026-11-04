<script setup lang="ts">
/* ===========================================================================
   BindPage —— 绑定设备（`prototype/project-bind.html`，v0.7.0）
   ---------------------------------------------------------------------------
   这个页面回答一个问题：**这个项目的活由哪台机器干、在哪个目录里干**。
   它不是一个「填个表单」的页面——绑错了会污染一台机器的缓存、或者让任务
   永远派不出去，所以原型用了五个区块把「为什么是这样」讲清楚。

   ── 五个区块，一个都不能省 ────────────────────────────────────────
   1. 选一台设备      —— 隔离式项目**只有一台可选**，其余不出现在列表里
   2. 固定目录        —— 路径一漂缓存全废，所以它不是「随便填个路径」
   3. 任务怎么跑      —— 工作树共享历史、不共享 node_modules，这是代价
   4. 配额与清理      —— 配额决定允许多大，清理决定跑完怎么收回
   5. 绑定之后能改什么 —— 用户最想问的其实是「我现在决定的能不能反悔」

   省掉 2/3/4 会让这一页变成「填两个框点确定」，而用户在心里没有底的情况下
   选出来的值，将来会以「缓存又失效了」「任务怎么老停在待办」的形式找回来。

   ── 为什么隔离式只有一台设备可选 ──────────────────────────────────
   隔离式项目的代码永不出设备，所以它的绑定只能是「代码已经在的那台」。
   原型把另外两台**留在表里但变成不可选**，而不是过滤掉——用户看得见
   「为什么那台不行」，才不会再去找一遍。

   ── 三种失败分开说 ────────────────────────────────────────────────
   读不到项目 / 读不到设备列表 / 写失败，三条路各有自己的提示。混成一条
   「操作失败」的话，用户不知道该刷新还是该改输入。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import {
  deleteProjectBinding,
  getProject,
  listProjectBindings,
  putProjectBinding,
  type Project,
  type ProjectBinding,
} from '@/domains/project/api'
import { listDevices, type DeviceSummary } from '@/domains/device/api'
import { ApiError, describeReadFailure } from '@/shared/http'
import { bytes, percent } from '@/shared/format'
import { navigate, route } from '@/router'

const projectId = computed(() => route.value.params.id ?? '')

const project = ref<Project | null>(null)
const devices = ref<DeviceSummary[]>([])
const bindings = ref<ProjectBinding[]>([])
const loading = ref(true)
/** 读失败。整页进错误态——手上没有数据时，让用户看到半份更糟。 */
const failure = ref('')
/** 写失败。**与 `failure` 分开**：写失败时列表还要留在页面上。 */
const actionFailure = ref('')
const saving = ref(false)

/* ---- 表单状态 ---------------------------------------------------------- */

const deviceId = ref('')
const fixedDir = ref('')
const quotaGb = ref('20')
const cleanupPolicy = ref('balanced')

/**
 * 清理策略三档。说明句照 `PRODUCT.md` 的口径，**每档都要写清差别**：
 * 只给「保守 / 平衡 / 激进」三个词，用户没法选——他不知道自己在保守什么。
 */
const POLICIES = [
  { key: 'balanced', label: '平衡：缓存留 7 天，产物跑完即清' },
  { key: 'conservative', label: '保守：缓存留 30 天，产物留 3 个做诊断' },
  { key: 'aggressive', label: '激进：缓存和产物都尽快清' },
] as const

/** 隔离式项目：代码永不出设备，所以只有一台能选。 */
const isolated = computed(() => project.value?.shape === 'isolated')

/**
 * 这台设备能不能选。
 *
 * 隔离式项目下**只有已绑定的那台**能选；还没有绑定时，一台都不让选——
 * 因为 Hub 不知道哪台机器上有这份代码，让用户随便挑等于在库里写一个
 * 派不出去的活。这是原型的第四条 callout 说的事。
 */
function selectable(d: DeviceSummary): boolean {
  if (!isolated.value) return true
  return bindings.value.some((b) => b.device_id === d.id)
}

/** 某台设备上已经绑了哪些项目。空数组 = 还没被任何项目用过。 */
function boundProjects(d: DeviceSummary): string {
  const mine = bindings.value.filter((b) => b.device_id === d.id)
  if (mine.length === 0) return '还没有'
  return mine.map((b) => (b.project_id === projectId.value ? '本项目' : b.project_id)).join(' · ')
}

/** 已选设备上有没有超出配额线——超出时任务会停在待办，绑定本身仍能建。 */
const selectedBinding = computed(() =>
  bindings.value.find((b) => b.device_id === deviceId.value),
)

const quotaBytes = computed<number | null>(() => {
  const gb = Number(quotaGb.value)
  if (!Number.isFinite(gb) || gb <= 0) return null
  return Math.round(gb * 1024 * 1024 * 1024)
})

async function load(): Promise<void> {
  const id = projectId.value
  if (!id) {
    failure.value = '这个地址缺少项目 id。'
    loading.value = false
    return
  }
  loading.value = true
  failure.value = ''
  try {
    // 三个请求并行：项目定形态、设备是候选、绑定定哪台已被占。
    const [p, ds, bs] = await Promise.all([
      getProject(id),
      listDevices(),
      listProjectBindings(id),
    ])
    project.value = p
    devices.value = ds
    bindings.value = bs
    // 已经绑过就先把表单填成现状：这一页同时也是「改绑定」的入口。
    const existing = bs[0]
    if (existing) {
      deviceId.value = existing.device_id
      fixedDir.value = existing.fixed_dir
      quotaGb.value =
        existing.disk_quota_bytes === null
          ? ''
          : String(Math.round(existing.disk_quota_bytes / 1024 / 1024 / 1024))
      cleanupPolicy.value = existing.cleanup_policy
    }
  } catch (e) {
    project.value = null
    failure.value = describeReadFailure(e, '项目')
  } finally {
    loading.value = false
  }
}

async function save(): Promise<void> {
  actionFailure.value = ''
  if (!deviceId.value) {
    actionFailure.value = '先选一台设备。'
    return
  }
  // 空目录会让 worktree 建在设备的根上——那是「清空整块盘」的前一步，
  // 后端也会拒（400 bad_body），但在这里就拦掉能给出更具体的提示。
  if (!fixedDir.value.trim()) {
    actionFailure.value = '固定目录不能为空。'
    return
  }
  saving.value = true
  try {
    await putProjectBinding(projectId.value, deviceId.value, {
      fixed_dir: fixedDir.value.trim(),
      disk_quota_bytes: quotaBytes.value,
      cleanup_policy: cleanupPolicy.value,
    })
    // 写成功就重读：配额、形态、绑定列表都可能被这次写改动，
    // 前端自己拼一份「应该是新的」状态迟早会和库里对不上。
    await load()
    navigate(`/project/${encodeURIComponent(projectId.value)}/board`)
  } catch (e) {
    actionFailure.value = describeWriteFailure(e)
  } finally {
    saving.value = false
  }
}

/**
 * 写失败的措辞。
 *
 * `fixed_dir_locked` 要**带出条数**：用户需要知道「有 3 条执行记录」才能
 * 判断是等一下还是永远不行。后端把条数放在 `detail.executions` 里。
 */
function describeWriteFailure(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'fixed_dir_locked') {
      return '改不了固定目录：这个目录上已经有执行记录，换目录会让那些记录的路径失效。'
    }
    return `保存失败（${e.code}）。`
  }
  return '保存失败：Hub 连不上。'
}

async function unbind(target: string): Promise<void> {
  actionFailure.value = ''
  saving.value = true
  try {
    await deleteProjectBinding(projectId.value, target)
    await load()
  } catch (e) {
    actionFailure.value =
      e instanceof ApiError ? `解除绑定失败（${e.code}）。` : '解除绑定失败：Hub 连不上。'
  } finally {
    saving.value = false
  }
}

onMounted(load)
</script>

<template>
  <div class="af-page">
    <div class="af-page__head">
      <button type="button" class="af-btn af-btn--ghost" @click="navigate('/')">
        ← 回总览
      </button>
      <h1 class="af-page__title">绑定设备</h1>
      <span v-if="project" class="af-badge">{{ project.name }}</span>
      <span v-if="isolated" class="af-badge af-badge--warn">隔离式</span>
      <span class="af-page__actions"></span>
      <button
        type="button"
        class="af-btn af-btn--primary"
        :disabled="saving"
        @click="save"
      >
        {{ saving ? '正在保存…' : '完成，进看板' }}
      </button>
    </div>

    <p v-if="loading" class="af-muted">正在读取项目与设备…</p>

    <EmptyState v-else-if="failure" glyph="⚠" title="打不开这个项目" :text="failure">
      <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
    </EmptyState>

    <template v-else>
      <!-- 隔离式项目的限制说在最上面：用户往下滚之前就该知道为什么只有一台能选。 -->
      <div v-if="isolated" class="af-callout af-callout--warn">
        <span class="af-callout__k">隔离式项目只能绑在指定设备上</span>
        <span>
          这个项目的代码永不出设备，所以它的绑定只能是「代码已经在的那台」。
          下面只有已绑定的设备可选，其余在列表里是灰的——免得你选了才发现派不出去。
        </span>
      </div>

      <p v-if="actionFailure" class="af-form__error" role="alert">{{ actionFailure }}</p>

      <!-- ── 1. 选一台设备 ─────────────────────────────────────────── -->
      <section class="af-region" aria-label="选一台设备">
        <div class="af-region__head">
          <h2 class="af-region__title">选一台设备</h2>
          <span class="af-region__note">
            在线 {{ devices.filter((d) => d.status === 'online').length }} 台 ·
            共 {{ devices.length }} 台
          </span>
        </div>
        <div class="af-region__body af-region__body--flush">
          <table v-if="devices.length > 0" class="af-table">
            <thead>
              <tr>
                <th scope="col"><span class="af-muted">选</span></th>
                <th scope="col">设备</th>
                <th scope="col">状态</th>
                <th scope="col">已绑定项目</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="d in devices" :key="d.id">
                <td>
                  <input
                    type="radio"
                    name="af-bind-device"
                    :value="d.id"
                    :checked="deviceId === d.id"
                    :disabled="!selectable(d)"
                    :aria-label="`选择 ${d.name}`"
                    @change="deviceId = d.id"
                  />
                </td>
                <td>
                  <strong>{{ d.name }}</strong>
                  <div class="af-muted">{{ d.platform }}</div>
                  <div v-if="!selectable(d)" class="af-muted">隔离式项目不能绑到这台</div>
                </td>
                <td>
                  <span
                    class="af-badge"
                    :class="d.status === 'online' ? 'af-badge--ok' : 'af-badge--warn'"
                  >
                    {{ d.status === 'online' ? '在线' : d.status }}
                  </span>
                </td>
                <td class="af-muted">{{ boundProjects(d) }}</td>
              </tr>
            </tbody>
          </table>
          <EmptyState
            v-else
            glyph="◈"
            title="一台设备都还没注册"
            text="先去设备页把 agent 装到一台机器上，让它连上来，再回来绑。"
          >
            <button type="button" class="af-btn" @click="navigate('/devices')">
              去设备页
            </button>
          </EmptyState>
        </div>
      </section>

      <div v-if="isolated && devices.length > 0" class="af-callout">
        <span class="af-callout__k">为什么另外几台不能选</span>
        <span>
          不是「灰掉等你升级」，是真的选不了。隔离式项目在库里就带着「只许这台设备」
          这一条，调度时它是第一道过滤，第二道是项目隔离策略。任何一层筛空，
          任务就停在待办并说明原因，不会降级凑一台机器去跑。
        </span>
      </div>

      <!-- ── 2. 固定目录 ───────────────────────────────────────────── -->
      <section class="af-region" aria-label="固定目录">
        <div class="af-region__head">
          <h2 class="af-region__title">固定目录</h2>
          <span class="af-region__note">这台机器上这个项目永远用这个路径</span>
        </div>
        <div class="af-region__body">
          <label class="af-field">
            <span class="af-field__label">路径</span>
            <input
              v-model="fixedDir"
              class="af-input"
              type="text"
              placeholder="比如：/srv/afleet/metrics"
              autocomplete="off"
            />
            <span class="af-field__hint">目录必须已存在，且是一个 git 仓库</span>
          </label>

          <div class="af-callout af-callout--info">
            <span class="af-callout__k">为什么必须固定，不能每次挑</span>
            <span>
              设备上的依赖缓存、构建缓存、编辑器配置全都挂在路径上。
              路径一漂，缓存就全部失效，每次任务都要冷启动——那比不缓存还糟。
              这个路径改一次，等于把那台机器上这个项目的缓存全部作废，
              所以改动要你明确确认。
            </span>
          </div>
        </div>
      </section>

      <!-- ── 3. 任务怎么在这个目录里跑 ─────────────────────────────── -->
      <section class="af-region" aria-label="任务怎么在这个目录里跑">
        <div class="af-region__head">
          <h2 class="af-region__title">任务怎么在这个目录里跑</h2>
          <span class="af-region__note">目录固定 · 工作树可弃 · 缓存可复 · 产物必清</span>
        </div>
        <div class="af-region__body">
          <dl class="af-bind__kv">
            <dt>用工作树</dt>
            <dd>每个任务在这个目录下开一个 git worktree（<code>.wt/task-NNNN</code>）</dd>
            <dt>为什么不是再 clone 一份</dt>
            <dd>
              工作树共享同一份仓库历史，第 N 个任务不多占一份；
              clone 每多一个任务就多一份完整历史
            </dd>
            <dt>代价</dt>
            <dd>
              工作树<b>不共享</b> <code>node_modules</code> 这类构建产物。
              所以依赖缓存按 lockfile 哈希复用，任务产物随工作树一起清
            </dd>
            <dt>跑完怎么收</dt>
            <dd>删掉工作树，缓存留着；不清缓存，因为缓存不是产物</dd>
          </dl>
        </div>
      </section>

      <!-- ── 4. 磁盘配额与清理策略 ─────────────────────────────────── -->
      <section class="af-region" aria-label="磁盘配额与清理策略">
        <div class="af-region__head">
          <h2 class="af-region__title">磁盘配额与清理策略</h2>
          <span class="af-region__note">配额决定允许多大，清理决定跑完怎么收回</span>
        </div>
        <div class="af-region__body">
          <label class="af-field">
            <span class="af-field__label">给这个项目多少磁盘（GB）</span>
            <input
              v-model="quotaGb"
              class="af-input af-bind__quota"
              type="number"
              min="1"
              inputmode="numeric"
            />
            <span class="af-field__hint">
              留空表示不限额。填 0 会拦掉一切派活，所以这里不接受 0。
            </span>
          </label>

          <fieldset class="af-fieldset">
            <legend class="af-field__label">清理策略</legend>
            <label v-for="p in POLICIES" :key="p.key" class="af-bind__policy">
              <input v-model="cleanupPolicy" type="radio" name="af-bind-clean" :value="p.key" />
              <span>{{ p.label }}</span>
            </label>
          </fieldset>

          <div v-if="selectedBinding && selectedBinding.used_bytes !== null" class="af-callout">
            <span class="af-callout__k">这台设备当前的用量</span>
            <span>
              本项目在这台设备上已用 {{ bytes(selectedBinding.used_bytes) }}。
              <template v-if="quotaBytes !== null">
                配额 {{ bytes(quotaBytes) }}，已用
                {{ percent(Math.round(((selectedBinding.used_bytes ?? 0) / quotaBytes) * 100)) }}。
              </template>
            </span>
          </div>
          <div v-else-if="selectedBinding" class="af-callout">
            <span class="af-callout__k">用量还没拿到</span>
            <span>
              设备还没上报这个项目占了多少空间，所以这里不显示数字——
              显示 <code>0 B</code> 会让人以为它一点不占。
            </span>
          </div>
        </div>
      </section>

      <!-- ── 5. 绑定之后可以改什么 ─────────────────────────────────── -->
      <section class="af-region" aria-label="绑定之后可以改什么">
        <div class="af-region__head">
          <h2 class="af-region__title">绑定之后可以改什么</h2>
          <span class="af-region__note">绑定是双向的：项目页和设备页都能改</span>
        </div>
        <div class="af-region__body af-region__body--flush">
          <table class="af-table">
            <thead>
              <tr>
                <th scope="col">想改</th>
                <th scope="col">能不能</th>
                <th scope="col">代价</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td>换设备</td>
                <td><span class="af-badge af-badge--ok">可以</span></td>
                <td class="af-muted">新设备要从缓存冷启动一次</td>
              </tr>
              <tr>
                <td>换固定目录</td>
                <td><span class="af-badge af-badge--warn">可以，但有执行记录后不行</span></td>
                <td class="af-muted">这台机器上这个项目的缓存全部作废</td>
              </tr>
              <tr>
                <td>改配额与清理策略</td>
                <td><span class="af-badge af-badge--ok">随时可以</span></td>
                <td class="af-muted">不影响正在跑的任务，下次派活生效</td>
              </tr>
              <tr>
                <td>解除绑定</td>
                <td><span class="af-badge af-badge--warn">可以</span></td>
                <td class="af-muted">
                  历史执行记录保留；这台设备上该项目的在跑任务会先被取消
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <!-- 已有绑定时给一条解除的入口，否则「换设备」这条路只能靠再绑一次。 -->
      <section v-if="bindings.length > 0" class="af-region" aria-label="已有绑定">
        <div class="af-region__head">
          <h2 class="af-region__title">已有绑定</h2>
          <span class="af-region__note">解除之后历史执行记录仍然保留</span>
        </div>
        <div class="af-region__body af-region__body--flush">
          <table class="af-table">
            <thead>
              <tr>
                <th scope="col">设备</th>
                <th scope="col">固定目录</th>
                <th scope="col">配额</th>
                <th scope="col">清理策略</th>
                <th scope="col"><span class="af-muted">操作</span></th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="b in bindings" :key="b.id">
                <td>{{ b.device_id }}</td>
                <td><code>{{ b.fixed_dir }}</code></td>
                <td>{{ b.disk_quota_bytes === null ? '不限额' : bytes(b.disk_quota_bytes) }}</td>
                <td>{{ b.cleanup_policy }}</td>
                <td>
                  <button
                    type="button"
                    class="af-btn af-btn--danger af-btn--sm"
                    :disabled="saving"
                    @click="unbind(b.device_id)"
                  >
                    解除绑定
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <div class="af-row">
        <button
          type="button"
          class="af-btn af-btn--primary"
          :disabled="saving"
          @click="save"
        >
          {{ saving ? '正在保存…' : '完成绑定，进看板' }}
        </button>
        <button type="button" class="af-btn af-btn--ghost" @click="navigate('/')">
          返回上一步
        </button>
        <span class="af-muted">绑好之后，这个项目就能派活了</span>
      </div>
    </template>
  </div>
</template>

<style scoped>
/* 「任务怎么跑」那张说明表。用 dl 而不是 table：它是术语与解释，
   不是可以排序比较的数据。 */
.af-bind__kv {
  margin: 0;
  display: grid;
  grid-template-columns: minmax(0, 180px) minmax(0, 1fr);
  gap: var(--af-space-2) var(--af-space-4);
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
}
.af-bind__kv dt {
  color: var(--af-fg-secondary);
  font-weight: var(--af-weight-medium);
}
.af-bind__kv dd { margin: 0; color: var(--af-fg-primary); }
.af-bind__kv code {
  font-family: var(--af-font-mono);
  font-size: var(--af-text-xs);
}

.af-bind__quota { width: 96px; }

/* 三档策略一行一条，整行可点：点文字也是点单选框。 */
.af-bind__policy {
  display: flex; align-items: center; gap: var(--af-space-2);
  font-size: var(--af-text-sm);
  cursor: pointer;
}
</style>
