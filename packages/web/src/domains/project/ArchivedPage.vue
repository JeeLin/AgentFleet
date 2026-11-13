<script setup lang="ts">
/* ===========================================================================
   ArchivedPage —— 已归档的项目（`prototype/archived.html`，v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   归档**不是删除**：历史执行记录、轨迹、管家总结全部保留，且可随时取消归档
   （`PRODUCT.md` §3.2 明文第一版不提供项目删除）。所以这一页的存在本身就是
   产品承诺的一部分——只有归档入口而没有取消归档的入口，归档就是删除。

   这个页面是**真数据**（`GET /api/projects?include_archived=1` 的归档子集），
   因为归档生命周期在子任务 2 已经落地了，没有理由留成骨架。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import { listProjects, unarchiveProject, type Project } from '@/domains/project/api'
import { ApiError, describeReadFailure } from '@/shared/http'
import { relativeTime } from '@/shared/format'
import { navigate } from '@/router'

const all = ref<Project[]>([])
const loading = ref(true)
const failure = ref('')
/**
 * 取消归档失败的提示。**与 `failure` 分开**。
 *
 * 两者合用一个 ref 的话，取消归档失败会让整份列表被错误态替掉——而这个失败
 * 最需要用户看见的恰恰是「那一行还在，我还能再试一次」。读失败是另一回事：
 * 那时手上没有任何数据，整页变错误态才对。
 */
const actionFailure = ref('')
const busyId = ref('')

/** 只看已归档的。后端按 `archived_at IS NULL` 过滤，这里反向挑。 */
const archived = computed(() => all.value.filter((p) => p.archived_at !== null))

/**
 * 归档时会连带发生什么（原型 `archived.html:141-164` 五行，逐行照搬）。
 *
 * **这五行是「敢不敢点归档」的依据**，不是说明文字：尤其「正在跑的执行
 * 不打断」与「定时任务不会自动恢复」两条，用户不知道就会以为归档会杀掉
 * 手上的活、或者以为取消归档之后一切照旧。
 */
const ARCHIVE_EFFECTS = [
  { what: '正在跑的执行', how: '不打断。跑完才归档；跑完的结果照常进待验收' },
  { what: '待办 / 排队的任务', how: '停止派发。它们仍在这项目里，取消归档后接着走' },
  { what: '设备上的工作目录', how: '按清理策略回收。历史记录不受影响' },
  { what: '定时任务', how: '一并暂停，取消归档时不会自动恢复——得你看一眼再开' },
  { what: '管家记忆', how: '全留着。取消归档后它还记得这个项目发生过什么' },
] as const

async function load(): Promise<void> {
  loading.value = true
  failure.value = ''
  try {
    all.value = await listProjects(true)
  } catch (e) {
    failure.value = describeReadFailure(e, '已归档的项目')
  } finally {
    loading.value = false
  }
}

async function restore(id: string): Promise<void> {
  busyId.value = id
  actionFailure.value = ''
  try {
    await unarchiveProject(id)
    await load()
  } catch (e) {
    actionFailure.value =
      e instanceof ApiError ? `取消归档失败（${e.code}）。` : '取消归档失败：Hub 连不上。'
  } finally {
    busyId.value = ''
  }
}

onMounted(load)
</script>

<template>
  <section class="af-page">
    <header class="af-page__head">
      <h1 class="af-page__title">已归档的项目</h1>
      <div class="af-page__actions">
        <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
      </div>
    </header>

    <p class="af-muted">
      归档不删除任何东西：执行记录、轨迹、管家总结都还在，随时可以取消归档。
    </p>

    <p v-if="loading" class="af-muted">正在读取…</p>

    <EmptyState
      v-else-if="failure"
      glyph="⚠"
      title="读取失败"
      :text="failure"
    >
      <button type="button" class="af-btn" @click="load">重试</button>
    </EmptyState>

    <EmptyState
      v-else-if="archived.length === 0"
      glyph="▢"
      title="还没有归档的项目"
      text="项目做完了、暂时不想看见它时，可以在项目设置里归档。归档之后它会出现在这里。"
    >
      <button type="button" class="af-btn" @click="navigate('/')">回总览</button>
    </EmptyState>

    <!-- 取消归档失败的提示**独立于**列表：两个都要在。合成一个错误态的话，
         用户看到的是「读取失败」而列表消失，可实际只是那一行动作没成功。 -->
    <p v-if="actionFailure" class="af-form__error" role="alert">{{ actionFailure }}</p>

    <ul v-if="archived.length > 0" class="af-projectlist">
      <li v-for="p in archived" :key="p.id" class="af-projectlist__row">
        <span class="af-projectlist__name">{{ p.name }}</span>
        <span v-if="p.archived_at" class="af-projectlist__shape">
          {{ relativeTime(p.archived_at) }}归档
        </span>
        <button
          type="button"
          class="af-btn"
          :disabled="busyId === p.id"
          @click="restore(p.id)"
        >
          {{ busyId === p.id ? '正在恢复…' : '取消归档' }}
        </button>
      </li>
    </ul>

    <!-- 两段解释性文案（原型 `archived.html:111-133` 与 `:136-166`）。
         **这类文案是产品承诺，不是装饰**：缺了它，用户就不知道归档能不能
         撤销、也不知道归档时正在跑的任务会不会被打断——而这两件事恰好是
         「敢不敢点归档」的全部依据。里程碑交互设计把这句写死为判据。 -->
    <section class="af-region af-arch__explain" aria-label="为什么这一页没有删除">
      <div class="af-region__head">
        <h2 class="af-region__title">为什么这一页没有「删除」</h2>
        <span class="af-region__note">不是没做，是不提供</span>
      </div>
      <div class="af-region__body">
        <div class="af-callout">
          <span class="af-callout__k">项目是 Hub 的真相源</span>
          <span>
            删掉一个项目，会让已合入的任务历史、失败归因、成本记录一起消失。
            一个项目哪怕跑了十次失败，这些记录对排障比「一个干净的空项目」更有价值。
            所以只有归档，没有删除。
          </span>
        </div>
        <div class="af-callout af-callout--info af-arch__callout">
          <span class="af-callout__k">如果只是想让它消失</span>
          <span>
            归档就够了——它不再出现在总览和看板首页，不派新任务，
            也不会再占用设备上的工作目录。你在这一页随时能找回来。
          </span>
        </div>
      </div>
    </section>

    <section class="af-region af-arch__explain" aria-label="归档的时候发生了什么">
      <div class="af-region__head">
        <h2 class="af-region__title">归档的时候发生了什么</h2>
      </div>
      <div class="af-region__body af-region__body--flush">
        <table class="af-table">
          <tbody>
            <tr v-for="row in ARCHIVE_EFFECTS" :key="row.what">
              <td class="af-arch__what">{{ row.what }}</td>
              <td class="af-arch__how">{{ row.how }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>
  </section>
</template>

<style scoped>
/* 两段解释性文案与列表之间的距离。只服务本页，不进 `components.css`。 */
.af-arch__explain {
  margin-top: var(--af-space-6);
}
/* 第二张 callout 与第一张之间的间距：`.af-region__body` 是普通块容器，
   两个 `.af-callout` 挨着会连成一片，读起来像一段。 */
.af-arch__callout {
  margin-top: var(--af-space-3);
}
/* 「归档的时候发生了什么」两列：左列是对象名，固定窄一点，
   右列是结论（长句）。原型用 `width:38%` 的内联样式，这里换成同一意图的
   scoped 规则，避免在模板里散落内联样式。 */
.af-arch__what {
  width: 38%;
  font-size: var(--af-text-sm);
  color: var(--af-fg-secondary);
}
.af-arch__how {
  font-size: var(--af-text-sm);
}
</style>
