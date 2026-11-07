<script setup lang="ts">
/* ===========================================================================
   NewProjectPage —— 新建项目（`prototype/project-new.html`，v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   形态选择卡是这一页的核心，**不是单选框**：三个名字对第一次见到的人没有
   区别，而它们的差别恰恰是产品最容易讲不清的地方。所以每张卡必须给一句
   「代码从哪来、到哪去」（`PRODUCT.md` §3.2 原表的口径）。

   ── 为什么默认选隔离式 ────────────────────────────────────────────
   判据是「猜错的代价」：把中心式猜成隔离式只是让用户多点一次升级，
   而把隔离式猜成中心式会让 Hub 预期上面有一个根本不该存在的仓库，且按
   §3.2 这个方向**不能改回去**。默认值必须落在能反悔的那一侧。
   后端也是这么定的（`routes/projects.rs` 的 `CreateProjectBody`）。
   =========================================================================== */

import { ref } from 'vue'

import { createProject } from '@/domains/project/api'
import { ApiError } from '@/shared/http'
import { navigate } from '@/router'

/** 三种形态。说明句照 `PRODUCT.md` §3.2 的表，不另造说法。 */
const SHAPES = [
  {
    key: 'central',
    name: '中心式',
    from: '代码从 Hub 的仓库来',
    to: '任务产出推回 Hub',
    hint: 'Hub 上存全部代码，起点是 Hub 上的 commit。适合自己从头做的项目。',
  },
  {
    key: 'mirror',
    name: '镜像式',
    from: '代码从上游仓库来',
    to: 'Hub 镜像一份，任务产出推回 Hub',
    hint: 'Hub 镜像 GitHub 等上游仓库，起点是镜像的 commit。适合给别人的项目做贡献。',
  },
  {
    key: 'isolated',
    name: '隔离式',
    from: '代码只在设备上',
    to: '永不上传，Hub 只存元数据',
    hint: 'Hub 不知道代码长什么样，但仍能看到每次执行的对话与 diff。适合客户现场、内网、合规。',
  },
] as const

const name = ref('')
const shape = ref<string>('isolated')
const upstreamUrl = ref('')
const submitting = ref(false)
const failure = ref('')

/**
 * 与后端 `routes/projects.rs::normalize_upstream_url` 同一套窄口径：
 * trim 后非空，且至少是 `http(s)://` / `ssh://` / `git@host:path` 之一。
 * 不做网络探测——页面也不该在建项目时等一次 DNS / TLS 握手。
 */
function looksLikeGitRemote(url: string): boolean {
  if (url.startsWith('http://') || url.startsWith('https://') || url.startsWith('ssh://')) {
    return true
  }
  const sep = url.indexOf(':')
  const scp = url.startsWith('git@') && sep > 4 ? [url.slice(4, sep), url.slice(sep + 1)] : []
  return scp.length === 2 && scp[0] !== '' && scp[1] !== ''
}

async function submit(): Promise<void> {
  const trimmed = name.value.trim()
  if (!trimmed) {
    failure.value = '给项目起个名字。'
    return
  }
  const url = upstreamUrl.value.trim()
  let upstream_url: string | undefined
  if (shape.value === 'mirror') {
    if (!url) {
      failure.value = '镜像式项目要填一个上游 git 仓库地址。'
      return
    }
    if (!looksLikeGitRemote(url)) {
      failure.value = '上游地址要像 git 远端：http(s)://、ssh:// 或 git@主机:路径。'
      return
    }
    upstream_url = url
  }
  submitting.value = true
  failure.value = ''
  try {
    const p = await createProject({ name: trimmed, shape: shape.value, upstream_url })
    // `replace`：用户按「后退」不该回到一张已经提交过的表单。
    navigate(`/project/${encodeURIComponent(p.id)}/board`, { replace: true })
  } catch (e) {
    failure.value =
      e instanceof ApiError ? `建项目失败（${e.code}）。` : '建项目失败：Hub 连不上。'
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <section class="af-page">
    <header class="af-page__head">
      <h1 class="af-page__title">新建项目</h1>
    </header>

    <form class="af-form" @submit.prevent="submit">
      <label class="af-field">
        <span class="af-field__label">项目名</span>
        <input
          v-model="name"
          class="af-input"
          type="text"
          placeholder="比如：内部指标台"
          autocomplete="off"
        />
      </label>

      <fieldset class="af-fieldset">
        <legend class="af-field__label">代码在哪</legend>
        <div class="af-shapecards">
          <label
            v-for="s in SHAPES"
            :key="s.key"
            class="af-shapecard"
            :data-selected="shape === s.key"
          >
            <input v-model="shape" class="af-shapecard__radio" type="radio" :value="s.key" />
            <span class="af-shapecard__name">{{ s.name }}</span>
            <span class="af-shapecard__flow">{{ s.from }} → {{ s.to }}</span>
            <span class="af-shapecard__hint">{{ s.hint }}</span>
          </label>
        </div>
      </fieldset>

      <label v-if="shape === 'mirror'" class="af-field">
        <span class="af-field__label">上游 git 仓库地址</span>
        <input
          v-model="upstreamUrl"
          class="af-input"
          type="text"
          placeholder="比如：https://github.com/you/metrics.git"
          autocomplete="off"
        />
        <span class="af-field__hint">镜像式项目要指一个上游；http(s)://、ssh:// 或 git@主机:路径。</span>
      </label>

      <p v-if="failure" class="af-form__error" role="alert">{{ failure }}</p>

      <div class="af-form__actions">
        <button type="button" class="af-btn" @click="navigate('/')">取消</button>
        <button type="submit" class="af-btn af-btn--primary" :disabled="submitting">
          {{ submitting ? '正在创建…' : '创建项目' }}
        </button>
      </div>
    </form>
  </section>
</template>
