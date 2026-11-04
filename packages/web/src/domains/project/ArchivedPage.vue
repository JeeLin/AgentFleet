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
  </section>
</template>
