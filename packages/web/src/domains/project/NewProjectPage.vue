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

import PageHead from '@/domains/shared/components/PageHead.vue'
import EmptyState from '@/domains/shared/components/EmptyState.vue'
import { createProject, putProjectBinding } from '@/domains/project/api'
import { listDevices, type DeviceSummary } from '@/domains/device/api'
import { ApiError, describeReadFailure } from '@/shared/http'
import { SHAPE_LABELS } from '@/shared/projectShape'
import { navigate } from '@/router'

/** 三种形态。说明句照 `PRODUCT.md` §3.2 的表，不另造说法。 */
const SHAPES = [
  {
    key: 'central',
    name: SHAPE_LABELS.central,
    from: '代码从 Hub 的仓库来',
    to: '任务产出推回 Hub',
    hint: 'Hub 上存全部代码，起点是 Hub 上的 commit。适合自己从头做的项目。',
  },
  {
    key: 'mirror',
    name: SHAPE_LABELS.mirror,
    from: '代码从上游仓库来',
    to: 'Hub 镜像一份，任务产出推回 Hub',
    hint: 'Hub 镜像 GitHub 等上游仓库，起点是镜像的 commit。适合给别人的项目做贡献。',
  },
  {
    key: 'isolated',
    name: SHAPE_LABELS.isolated,
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
/** 1 = 建项目；2 = 选设备与目录。见 `submit()` 的说明。 */
const step = ref<1 | 2>(1)

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
    // **不再直接跳看板**（v0.8.1 子任务 13）：项目建好后进第二步——选设备与目录。
    // 原先是 `navigate(board)`，把用户丢到一块空板子上，再让他自己找「绑定设备」。
    // 而项目建完到能干活的中间还有一步「这活由哪台机器、在哪个目录里干」，
    // 那一步是**必须做的**（不绑定就派不出任务），所以它该在这里接着问完。
    projectId.value = p.id
    step.value = 2
    await loadDevices()
  } catch (e) {
    failure.value =
      e instanceof ApiError ? `建项目失败（${e.code}）。` : '建项目失败：Hub 连不上。'
  } finally {
    submitting.value = false
  }
}

/* ===========================================================================
   第二步：这台项目的活由哪台机器干、在哪个目录里干
   ---------------------------------------------------------------------------
   内容来自 `BindPage`（原型 `project-bind.html` 的五个区块，见那边的文件头）。
   这里**只搬「建项目当下必须决定」的两项**：设备 + 固定目录。

   为什么配额与清理策略不在这一步问：它们有合理默认值（20GB / 平衡），
   而且随时能在项目设置里改；固定目录**不能**随便改（路径一漂缓存全废，
   后端有 `fixed_dir_locked` 顶着），所以它是「现在必须决定」的那一项。

   为什么可以跳过：绑定不是建项目的必要条件，Hub 允许先有项目后绑设备
   （`putProjectBinding` 是独立端点）。所以第二步给「以后再说」，直接去看板。
   =========================================================================== */

const projectId = ref('')
const devices = ref<DeviceSummary[]>([])
const devicesFailed = ref('')
const devicesLoading = ref(false)

/** 设备状态的中文名。三态与后端闭集一一对应（`ApiDeviceStatus`）。 */
const STATUS_LABEL: Record<DeviceSummary['status'], string> = {
  online: '在线',
  reconnecting: '重连中',
  offline: '离线',
}

const deviceId = ref('')
const fixedDir = ref('')
const saving = ref(false)
/** 第二步的写失败。**与 `failure` 分开**：第一步的错误不该在第二步里显示。 */
const bindFailure = ref('')

async function loadDevices(): Promise<void> {
  devicesLoading.value = true
  devicesFailed.value = ''
  try {
    devices.value = await listDevices()
  } catch (e) {
    // 读不到设备不该挡住「项目已经建好了」这件事——第二步允许跳过。
    devicesFailed.value = describeReadFailure(e, '设备列表')
  } finally {
    devicesLoading.value = false
  }
}

/** 选中设备后按项目形态给一个目录默认值：隔离式项目常见的是 `~/work/<名字>`。 */
function pickDevice(id: string): void {
  deviceId.value = id
  if (!fixedDir.value.trim()) {
    const slug = (name.value.trim() || 'project').replace(/\s+/g, '-')
    fixedDir.value = `~/work/${slug}`
  }
}

/**
 * 第二步的「完成」。**绑定失败不跳转**——留在这一屏，用户能改输入重试。
 * 不绑定时走 `finishLater`，同样进看板。
 */
async function finishBind(): Promise<void> {
  bindFailure.value = ''
  if (!deviceId.value) {
    bindFailure.value = '先选一台设备。'
    return
  }
  if (!fixedDir.value.trim()) {
    bindFailure.value = '固定目录不能为空——空目录会让工作树建在设备根上。'
    return
  }
  saving.value = true
  try {
    await putProjectBinding(projectId.value, deviceId.value, {
      fixed_dir: fixedDir.value.trim(),
      cleanup_policy: 'balanced',
    })
    goBoard()
  } catch (e) {
    bindFailure.value =
      e instanceof ApiError ? `绑定失败（${e.code}）。` : '绑定失败：Hub 连不上。'
  } finally {
    saving.value = false
  }
}

/** 不绑定直接去看板。`replace` 的理由与第一步相同：后退不该回到建过的表单。 */
function finishLater(): void {
  goBoard()
}

function goBoard(): void {
  navigate(`/project/${encodeURIComponent(projectId.value)}/board`, { replace: true })
}
</script>

<template>
  <!-- 页面骨架照原型 `project-new.html:15-23`：页头是 `header.af-head`（`:15-21`）
       （返回总览 + 标题 + 副标题 + 右侧「下一步」），内容在 `.af-body`（`:23`）里。
       上一版是自造的 `.af-page` / `.af-page__head` / `.af-page__title` ——
       那一族规则在 v0.8.1 子任务 2 已随类名删除，于是这一页没有页头几何。
       页头改用共享的 `PageHead`（所有已对齐页面的同一写法）。 -->
  <PageHead>
    <button type="button" class="af-btn af-btn--ghost" @click="navigate('/')">
      ← 总览
    </button>
    <h1 class="af-head__title">新建项目</h1>
    <span class="af-head__sub">{{ step === 1 ? '第一步：选形态' : '第二步：谁来干活' }}</span>
  </PageHead>

  <div class="af-body">
    <!-- 原型 `project-new.html:24` 的内容层：带内边距的一列（原型的
         `padding:20px 20px 64px` 是内联样式，实现侧落成 scoped 规则）。

         **两步向导（v0.8.1 子任务 13）**：原来这一页只有「建项目」那一步，
         建完直接跳一块空看板，用户再自己去找「绑定设备」。而「这活由哪台机器、
         在哪个目录里干」是**必须做的**（不绑定派不出任务），所以它被接在这里。
         两步同页，第二步用的是刚建好的项目 id——不用跳转，也不用重新读项目。 -->
    <div class="af-new__body">
    <!-- 步骤指示：两枚数字 + 一条连线。**同时给出「现在第几步」与「一共几步」**，
         否则用户不知道后面还有没有。已完成的那一步打勾，不只靠颜色区分。 -->
    <ol class="af-new__steps" aria-label="创建项目分两步">
      <li class="af-new__step" :data-current="step === 1" :data-done="step > 1">
        <span class="af-new__stepnum" aria-hidden="true">{{ step > 1 ? '✓' : '1' }}</span>
        <span class="af-new__steplabel">建项目</span>
      </li>
      <li class="af-new__steps-line" aria-hidden="true"></li>
      <li class="af-new__step" :data-current="step === 2">
        <span class="af-new__stepnum" aria-hidden="true">2</span>
        <span class="af-new__steplabel">选设备与目录</span>
      </li>
    </ol>

    <form v-if="step === 1" class="af-form" @submit.prevent="submit">
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

    <!-- 第二步：设备 + 固定目录。内容取自 `BindPage` 的「选一台设备」「固定目录」
         两个区块——只有这两项是建项目当下必须定的。配额与清理策略有合理默认值
         且随时可改，留给项目设置。 -->
    <div v-else class="af-new__step2">
      <p class="af-new__lede">
        项目已经建好了。再选一台设备和它的工作目录，任务就能派出去；这一步也可以跳过，之后在项目设置里补。
      </p>

      <div v-if="devicesLoading" class="af-muted">正在读取设备…</div>

      <EmptyState
        v-else-if="devicesFailed"
        compact
        glyph="⚠"
        title="读不到设备列表"
        :text="devicesFailed"
      >
        <button type="button" class="af-btn af-btn--sm" @click="loadDevices">重试</button>
      </EmptyState>

      <EmptyState
        v-else-if="devices.length === 0"
        compact
        glyph="▣"
        title="还没有设备接入"
        text="没有设备也能先建项目，但任务派不出去。到「设备」页跑一条 afcli 命令接入一台，再回来绑定。"
      >
        <button type="button" class="af-btn af-btn--sm" @click="finishLater">先去看板</button>
      </EmptyState>

      <template v-else>
        <fieldset class="af-fieldset">
          <legend class="af-field__label">在哪台设备上干活</legend>
          <div class="af-new__devices">
            <label
              v-for="d in devices"
              :key="d.id"
              class="af-new__device"
              :data-selected="deviceId === d.id"
            >
              <input
                class="af-shapecard__radio"
                type="radio"
                name="af-new-device"
                :value="d.id"
                :checked="deviceId === d.id"
                @change="pickDevice(d.id)"
              />
              <span class="af-new__device-name">{{ d.name }}</span>
              <span class="af-new__device-meta">
                {{ STATUS_LABEL[d.status] }}<template v-if="d.platform"> · {{ d.platform }}</template>
              </span>
            </label>
          </div>
        </fieldset>

        <label class="af-field">
          <span class="af-field__label">固定工作目录</span>
          <input
            v-model="fixedDir"
            class="af-input"
            type="text"
            placeholder="比如：~/work/my-project"
            autocomplete="off"
          />
          <span class="af-field__hint">
            在一台机器上固定下来，之后不再改。路径一漂，那台机器上的缓存就全废了。
          </span>
        </label>

        <p v-if="bindFailure" class="af-form__error" role="alert">{{ bindFailure }}</p>

        <div class="af-form__actions">
          <button type="button" class="af-btn af-btn--ghost" @click="finishLater">
            跳过，先去看板
          </button>
          <button type="button" class="af-btn af-btn--primary" :disabled="saving" @click="finishBind">
            {{ saving ? '正在绑定…' : '绑定并进入看板' }}
          </button>
        </div>
      </template>
    </div>
    </div>
  </div>
</template>

<style scoped>
/* 内容层：原型 `project-new.html:24` 的 `padding:20px 20px 64px` 是内联样式，
   实现侧落成这一条；`.af-body` 自己只负责滚动（`app.css:132`），不带内边距。 */
.af-new__body {
  padding: var(--p-s6) var(--p-s6) var(--p-s8);
  /* **不再把整页挤在左上角**（v0.8.1 子任务 13，用户：「你这页面都不满」）：
     原来整页只有 `.af-form` 的 `max-width: 72ch`（实测 581px），而内容区有
     1216px，于是表单孤零零贴着左上、右边 600px 全空。这里给内容层一个上限，
     并对齐页头的 24px 缩进。 */
  max-width: 980px;
}

/* 本页的表单铺开用：72ch 是给「读长文本」的上限，而这里是一张分两栏的
   选择表——三张形态卡并排时每张才 188px，标题都放不下一句完整的话。
   **只在这一页覆盖**（`.af-new__body` 作用域内），不动全局 `.af-form`。 */
.af-new__body :deep(.af-form) {
  max-width: none;
}

/* 三张形态卡等高：默认 `align-items: stretch` 已让它们同高，但内容高度不同
   会让「→」箭头与说明句上下错位。把卡内部也拉成列，说明句顶到底对齐。 */
.af-shapecards { align-items: stretch; }
.af-shapecard { height: 100%; }

/* 步骤指示：两枚圆号 + 一条连线。**编号与连线都是形状信息**：
   已完成打勾、当前实心、未到描边——不只靠颜色（高对比度下也分得出）。 */
.af-new__steps {
  display: flex; align-items: center; gap: var(--p-s3);
  margin: 0 0 var(--p-s6); padding: 0; list-style: none;
}
.af-new__step { display: flex; align-items: center; gap: var(--p-s2); }
.af-new__stepnum {
  display: inline-flex; align-items: center; justify-content: center;
  width: 22px; height: 22px; border-radius: var(--p-r-full);
  border: 1px solid var(--p-line-3);
  font-size: var(--af-text-2xs); font-weight: var(--af-weight-semibold);
  color: var(--p-fg-2); background: var(--p-surf-1);
}
.af-new__step[data-current='true'] .af-new__stepnum {
  background: var(--p-acc); border-color: var(--p-acc); color: var(--p-on-acc);
}
.af-new__step[data-done='true'] .af-new__stepnum {
  background: var(--p-done-bg); border-color: var(--p-done); color: var(--p-done);
}
.af-new__steplabel { font-size: var(--af-text-sm); color: var(--p-fg-2); }
.af-new__step[data-current='true'] .af-new__steplabel {
  color: var(--p-fg-strong); font-weight: var(--af-weight-medium);
}
.af-new__steps-line {
  flex: 0 0 auto; width: 40px; height: 1px; background: var(--p-line-3);
}

/* 第二步的引导句：说明「项目已建好、这步可以跳过」。测度收窄，不横铺。 */
.af-new__lede {
  margin: 0 0 var(--p-s5); max-width: var(--af-measure);
  font-size: var(--af-text-sm); color: var(--p-fg-2);
  line-height: var(--af-leading-base);
}

/* 第二步整体：与第一步同宽（`none` 而非 72ch），两屏切换时左边缘与宽度都不跳。 */
.af-new__step2 { display: flex; flex-direction: column; gap: var(--p-s5); }

/* 设备选择卡：横排一条（名字 + 平台/在线状态），选中靠边框 + 底色 + ✓。
   设备数量通常个位数，横排比三列网格好读——每行信息密度一致。 */
.af-new__devices { display: flex; flex-direction: column; gap: var(--af-space-2); }
.af-new__device {
  position: relative; display: flex; align-items: baseline; gap: var(--p-s3);
  padding: var(--p-s3) var(--p-s4); cursor: pointer;
  background: var(--p-surf-1); border: 1px solid var(--p-line-2);
  border-radius: var(--p-r-md);
}
.af-new__device:hover { border-color: var(--p-line-3); background: var(--p-surf-2); }
.af-new__device[data-selected='true'] {
  border-color: var(--p-acc); box-shadow: inset 0 0 0 1px var(--p-acc);
  background: var(--p-surf-2);
}
.af-new__device:focus-within { outline: 2px solid var(--af-border-focus); outline-offset: 2px; }
.af-new__device-name {
  font-size: var(--af-text-md); font-weight: var(--af-weight-medium); color: var(--p-fg-strong);
}
.af-new__device-meta { font-size: var(--af-text-sm); color: var(--p-fg-2); }
</style>
