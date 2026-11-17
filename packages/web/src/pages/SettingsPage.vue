<script setup lang="ts">
/* ===========================================================================
   SettingsPage —— 设置（规范 §5.3：M2 范围，只定结构）
   ---------------------------------------------------------------------------
   §5.3 把这一页标成 M2、「只定结构，控件全禁用」。所以这里**没有一项能改**：
   每个控件都禁用，旁边写明它 M2 会做什么。

   ── 为什么不放几行「看起来能用」的输入框 ─────────────────────────
   一个能打字、按保存却没反应的设置页，比一张全是禁用控件的表更糟：
   用户会以为保存失败，然后反复试。禁用 + 一句「M2」是唯一诚实的形态。

   ── 这一页不显示任何 secret ──────────────────────────────────────
   约束 13：secret 值永不回显。设备密钥指纹在设备页逐台显示，
   这里连一行假的 `sk-…` 都不放——那会让人以为某处真存了一个 key。

   ── 五个分区的来历（v0.8.0 子任务 12）──────────────────────────────
   `prototype/settings.html` 共 **8 个 `<h2>`**：7 个 `.af-region__title` 分区
   （`:31` 合并权 / `:82` 决策超时 / `:111` 磁盘 / `:148` 通知出口 / `:186` 外观 /
   `:258` 语言 / `:305` 哪些东西删不掉）+ `:326` 一个 `.af-drawer__title` 抽屉。
   本页落地**其中五个**（前五个），语言与「哪些东西删不掉」逐条登记不修理由
   （见 `docs/design-system.md` §10.2 的 4-6 / 4-7）。而本页此前是四行
   通用占位（Hub 连接 / 改密码 / 模型 API key / 节点凭据），**一个分区都对不上**。

   里程碑测试标准 3 要求这五个分区标题逐个存在（表驱动断言）。补齐时的取舍：

   1. **五个分区标题与三档/三行的「内容表」照原型搬**——那些是产品承诺
      （默认最保守、不可逆操作不自动执行、磁盘参与派活过滤），不是数据。
   2. **控件一律禁用**，并沿用本页既有的「旁边写明 M2 会做什么」口径。
      原型的开关是 `aria-checked` 的 `<span>`（纯展示），这里用真的 `<input>`
      + `disabled`：读屏能认出它是开关，而用户点不动它——与原型意图一致。
   3. **原型有、本页不做的两块**已登记进 `docs/Bugs` 与差异清单：
      「语言」（原型 `:256`，本版只有中文，摆一个切不动的开关更糟）与
      「哪些东西删不掉」（原型 `:303`，它是**只读事实表**，与设置无关，
      归项目设置页）。

   原有的四行（Hub 连接 / 改密码 / 模型 API key / 节点凭据）**不删**：
   它们是这一页此前唯一诚实的部分，且「节点凭据」指向设备页指纹那条路
   有测试守着（约束 11）。它们归入「Hub 连接」分区之后。
   =========================================================================== */

import { onMounted, ref } from 'vue'

import { navigate } from '@/router'
import PageHead from '@/domains/shared/components/PageHead.vue'
import {
  DEFAULT_THEME,
  currentTheme,
  followOS,
  setTheme,
  toggleFollowOS,
  type ThemeId,
} from '@/styles/theme'

interface Row {
  title: string
  hint: string
  /** 控件的占位文字。**不是**真实值。 */
  placeholder: string
  /** M2 时这一项会做什么。 */
  m2: string
  type?: 'text' | 'password'
}

/** 原来的四行。保留 + 归入「Hub 连接」分区（见文件头第 3 条）。 */
const ROWS: readonly Row[] = [
  {
    title: 'Hub 连接',
    hint: '绑定地址、TLS、心跳间隔。',
    placeholder: 'M2 起可改',
    m2: '改绑定地址与 TLS 需要重启 Hub；保存前会先说明影响，不静默重启。',
  },
  {
    title: '改密码',
    hint: '单用户唯一凭据。',
    placeholder: 'M2 起可改',
    m2: '改完其它已登录会话不失效——会话绑定凭据版本，改密只递增版本。',
    type: 'password',
  },
  {
    title: '模型 API key',
    hint: 'agent 运行时使用的密钥。',
    placeholder: 'M2 起可填',
    m2: '只写不读：保存后只显示前 3 位与末 3 位；换 key 不重启已有会话。',
    type: 'password',
  },
  {
    title: '节点凭据',
    hint: '每台设备自己的身份密钥。',
    placeholder: '按设备显示指纹',
    m2: '这里是展示窗口，不是保险箱：只列指纹，私钥永不上传（约束 11）。',
  },
]

/**
 * 合并权三档（原型 `prototype/settings.html:44-61`）。
 *
 * **默认档 0，升档只能人操作**：合主干是唯一会污染主干的动作，其他错误都
 * 还能退回某次执行，只有合错了会变成历史里的一行。所以默认不给这个权力。
 */
interface MergeTier {
  tier: string
  can: string
  when: string
  /** 当前选中档。**写死 0**：升档要人操作，而这一版还没有写入口。 */
  current?: boolean
}

const MERGE_TIERS: readonly MergeTier[] = [
  {
    tier: '档 0',
    can: '只能你或预设规则合并。管家能给判断、能推你一把，合主干要你点头',
    when: '起步阶段，还没建立信任',
    current: true,
  },
  {
    tier: '档 1',
    can: '管家可以合并，但每次都写进总结并通知你',
    when: '用过一段时间，想省点操作',
  },
  {
    tier: '档 2',
    can: '管家可以合并，只通知，不写总结',
    when: '已经信任，只要知道发生过',
  },
]

/**
 * 决策超时的两行（原型 `:89-98`）。
 *
 * 判据是「做错了能不能退回来」：业务决策可逆 → 按默认项继续；磁盘清理
 * 不可逆 → 保持中止。**这两行是产品承诺**，不是可选配置。
 */
interface TimeoutRow {
  kind: string
  fallback: string
  tone: 'ok' | 'danger'
  why: string
}

const TIMEOUT_ROWS: readonly TimeoutRow[] = [
  {
    kind: '业务决策（用 Redis 还是 SQLite）',
    fallback: '按标注的默认项继续',
    tone: 'ok',
    why: '做错了下一步还能改回来，让执行继续比停住更有价值。没标默认项就中止',
  },
  {
    kind: '磁盘清理确认',
    fallback: '保持中止，不自动清理',
    tone: 'danger',
    why: '删了就没了。不可逆操作不该有「超时默认执行」',
  },
]

/**
 * 清理策略三档（原型 `:126-130`）。与绑定时那个下拉是同一套**取值**。
 *
 * 文案**故意不复用** `domains/device/api.ts` 的 `describeCleanupPolicy()`：
 * 那里是「策略落在设备上会怎样」的一整句解释（「产物留最近 3 个做诊断」），
 * 这里是设置页的简短选项名（「产物留 3 个做诊断」）。两者读者与长度预算
 * 不同，硬统一会把其中一处撑到不合适的位置。**三档的档位与量词必须一致**，
 * 口径由 `SettingsPage.test.ts` 与设备页的那条用例分别钉住。
 */
const CLEANUP_OPTIONS = [
  '平衡：缓存留 7 天，产物跑完即清',
  '保守：缓存留 30 天，产物留 3 个做诊断',
  '激进：缓存和产物都尽快清',
] as const

/**
 * 通知出口的三个开关（原型 `:152-163`）。
 *
 * 原型用 `<span role="switch" aria-checked>` 是**纯展示**；这里用真的
 * `<input type="checkbox">` + `disabled`。两者都不可点，但后者读屏能认出
 * 它是开关并读出「已勾选 / 未勾选」——原型那种写法在屏幕阅读器里只是一个
 * 有 `role` 的 span。
 */
interface NotifyExit {
  label: string
  on: boolean
}

const NOTIFY_EXITS: readonly NotifyExit[] = [
  { label: '站内收件箱', on: true },
  { label: '邮件', on: false },
  { label: 'IM 机器人', on: true },
]

/**
 * 外观：四套配色（原型 `settings.html:191-231`）。
 *
 * 名字与说明**逐字照原型**：`desc` 里的对比度（9.3:1 / 12.8:1）不是装饰，
 * 它是「这套配色能不能当正文用」的可核对数字。改配色时必须同步改这里，
 * 否则卡片会替一套它已不再是的配色作证。
 */
interface ThemeChoice {
  id: ThemeId
  name: string
  desc: string
}

const THEME_CHOICES: readonly ThemeChoice[] = [
  { id: 'neutral', name: '甲 · 中性冷灰', desc: '暗色 · 冷调 · 正文 9.3:1' },
  { id: 'warm', name: '乙 · 暖米灰', desc: '暗色 · 暖调 · 蓝光更少' },
  { id: 'paper', name: '丙 · 纸白偏冷', desc: '亮色 · 冷调 · 正文 12.8:1' },
  { id: 'cream', name: '丁 · 米白偏暖', desc: '亮色 · 暖调 · 蓝光更少' },
]

/**
 * 选中态是**本地状态**，不是「渲染时问一次模块」。
 *
 * `currentTheme()` 要在挂载后才读：模块在 `main.ts` 里更早应用主题，但设置页
 * 也可能在测试里被单独挂载（那时 `document` 上还没有任何主题）。用一个 ref
 * 保存「现在选的是哪套」，点击时先改 ref 再落盘——界面不必等存储写成功。
 */
const picked = ref<ThemeId>(DEFAULT_THEME)
const followOSOn = ref(false)

onMounted(() => {
  picked.value = currentTheme()
  followOSOn.value = followOS()
})

/**
 * 选一套配色。
 *
 * **手动选一套 = 不再跟随系统**（原型 `swatch.js:106-111` 就是这么做的）：
 * 用户刚明确点了一套，界面却还按系统明暗自动切换，那次点击看起来就是没生效。
 *
 * 用 `setTheme` 而不是 `applyTheme`：前者会**持久化**（写 localStorage `af-theme`
 * 并把 `?theme=` 换掉，刷新/复制地址都还是这套），后者只改 DOM。
 */
function pickTheme(id: ThemeId): void {
  picked.value = setTheme(id)
  followOSOn.value = followOS()
}

/**
 * 跟随系统开关。切换后立刻按新的来源重算一次当前配色（原型 `swatch.js:126-131`）。
 *
 * 开关状态由模块决定并返回，页面不自己算——否则两边各存一份布尔值，
 * 迟早不一致（比如 localStorage 抛异常时模块仍能工作，页面那份却已经翻了）。
 */
function onToggleFollowOS(): void {
  const r = toggleFollowOS()
  followOSOn.value = r.follow
  picked.value = r.theme
}
</script>

<template>
  <!-- 页面骨架照原型 `settings.html:15-26`：`header.af-head` 是 `.af-body` 的
       兄弟，内容在 `.af-body` 里（那一页的 `.af-body` 内是一层内联
       `padding:20px 20px 64px` 的 `.af-stack`，静态内联样式被门禁禁止，
       落成下面的 `.af-settings__stack`）。上一版是自造的
       `.af-page` / `.af-page__head` / `.af-page__title` / `.af-page__sub`
       ——那一族规则已随 v0.8.1 子任务 2 的类名删除，于是这一页（子任务 11
       交付的配色设置页）一直是**没有页头几何的裸页面**。 -->
  <PageHead>
    <div>
      <h1 class="af-head__title">设置</h1>
      <p class="af-head__sub">
        M2 范围。这一页先定结构：<b>没有任何一项现在能改</b>，每个控件都是禁用的，
        旁边写明 M2 时会做什么。列在这里是因为导航需要知道它存在，不是为了先画一个假表单。
      </p>
    </div>
    <template #actions>
      <span class="af-badge af-badge--off">M2 起可改</span>
    </template>
  </PageHead>

  <div class="af-body">
    <div class="af-settings__stack">

    <!-- ① 合并权（原型 `:29-77`） -->
    <section class="af-region af-settings__region" aria-label="合并权">
      <div class="af-region__head">
        <h2 class="af-region__title">AgentFleet 的合并权</h2>
        <span class="af-region__note">项目级设置 · 默认档 0 · 按项目改在项目设置</span>
      </div>
      <div class="af-region__body">
        <p class="af-settings__p">
          管家能不能自己合主干，这件事在没用过之前答不了，所以做成三档。
          档位是项目级的：不同项目该有不同答案。升档只能你来操作，管家不能自己申请升档。
        </p>
        <div class="af-region af-settings__inner">
          <div class="af-region__body af-region__body--flush">
            <table class="af-table">
              <thead>
                <tr>
                  <th>档</th>
                  <th>管家能做什么</th>
                  <th>适合什么时候</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="t in MERGE_TIERS" :key="t.tier">
                  <td>
                    <strong>{{ t.tier }}</strong>
                    <span v-if="t.current" class="af-badge af-badge--ok af-settings__cur">当前</span>
                  </td>
                  <td>{{ t.can }}</td>
                  <td class="af-settings__when">{{ t.when }}</td>
                  <td>
                    <button class="af-btn af-btn--sm" type="button" disabled>
                      {{ t.current ? '已选中' : '升到这一档' }}
                    </button>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>
        <div class="af-callout af-callout--warn af-settings__callout">
          <span class="af-callout__k">为什么默认最保守</span>
          <span>
            合主干是唯一会污染主干的动作。其他所有错误都还能退回到某一次执行，
            只有合错了会变成历史里的一行。所以默认不给这个权力。
          </span>
        </div>
        <p class="af-settings__p af-t-mute af-settings__p--tight">
          不管哪一档，合并都要留痕：谁合的、什么时候、把哪个分支合到了哪。
        </p>
      </div>
    </section>

    <!-- ② 决策请求超时（原型 `:80-106`） -->
    <section class="af-region af-settings__region" aria-label="决策超时">
      <div class="af-region__head">
        <h2 class="af-region__title">决策请求超时之后怎么处理</h2>
        <span class="af-region__note">按「做错了能不能退回来」区分</span>
      </div>
      <div class="af-region__body af-region__body--flush">
        <table class="af-table">
          <thead>
            <tr>
              <th>请求类型</th>
              <th>超时默认</th>
              <th>为什么</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in TIMEOUT_ROWS" :key="row.kind">
              <td>{{ row.kind }}</td>
              <td>
                <span class="af-badge" :class="`af-badge--${row.tone}`">{{ row.fallback }}</span>
              </td>
              <td class="af-settings__why">{{ row.why }}</td>
            </tr>
          </tbody>
        </table>
        <p class="af-settings__tablefoot af-t-mute">
          两种情况都要留痕（「超时未答，按默认继续」/「超时未答，保持中止」），
          收件箱里那条也不会消失——你回来还看得见它发生过什么。
        </p>
      </div>
    </section>

    <!-- ③ 磁盘（原型 `:109-143`） -->
    <section class="af-region af-settings__region" aria-label="磁盘">
      <div class="af-region__head">
        <h2 class="af-region__title">磁盘</h2>
        <span class="af-region__note">
          配额决定允许多大，调度决定现在跑不跑得动，清理决定跑完怎么收回
        </span>
      </div>
      <div class="af-region__body">
        <div class="af-settings__fields">
          <label class="af-field">
            <span class="af-field__label">超过多少就不再往这台派新任务</span>
            <div class="af-row gap-2">
              <!-- 阈值用 `placeholder` 而不是 `value`：这一页有一条硬规则
                   「任何 input 的 value 都必须是空」（约束 13 的延伸，测试守着
                   `input.value === ''`）。把 90 写进 value 会让那条规则破一个口子，
                   而破口子之后「哪个 value 是真值、哪个是占位」就没人分得清了。 -->
              <input
                class="af-input af-settings__num"
                type="text"
                placeholder="90"
                disabled
                aria-label="磁盘水位阈值百分比（M2 起可改，缺省 90）"
              />
              <span class="af-t-mute">%</span>
            </div>
            <span class="af-field__hint">
              磁盘满了不是性能问题，是任务会静默失败的问题，所以它参与派活前的过滤
            </span>
          </label>
          <label class="af-field">
            <span class="af-field__label">默认清理策略</span>
            <select class="af-input" disabled aria-label="默认清理策略（M2 起可改）">
              <option v-for="o in CLEANUP_OPTIONS" :key="o">{{ o }}</option>
            </select>
            <span class="af-field__hint">绑定项目时可以逐个改</span>
          </label>
        </div>
        <div class="af-callout af-settings__callout">
          <span class="af-callout__k">依赖目录永远不搬</span>
          <span>
            <span class="af-fp">node_modules</span> 这类目录按 lockfile 在本地重建，不在设备间搬运。
            x86-64 和 aarch64 装出来的不是同一份东西，根本不存在「同一份依赖」可以搬。
            复用缓存按 lockfile 哈希命中，产物随工作树一起清。
          </span>
        </div>
      </div>
    </section>

    <!-- ④ 通知出口（原型 `:146-180`） -->
    <section class="af-region af-settings__region" aria-label="通知出口">
      <div class="af-region__head">
        <h2 class="af-region__title">通知出口</h2>
      </div>
      <div class="af-region__body">
        <div class="af-settings__exits">
          <label v-for="e in NOTIFY_EXITS" :key="e.label" class="af-settings__exit">
            <input type="checkbox" :checked="e.on" disabled />
            <span>{{ e.label }}</span>
          </label>
        </div>
        <p class="af-settings__p af-t-mute af-settings__p--tight">
          邮件 / IM 的凭据、推送时机、能做什么不能做什么，都在「通知出口」页——
          那一页还没做，所以这里不给链接。
        </p>
        <div class="af-callout af-settings__callout">
          <span class="af-callout__k">外部 issue 只做单向</span>
          <span>
            导入会把外部 issue 变成这里的一条任务，之后任务的真身留在 Hub；导出是把结论回写成一条评论。
            不做双向同步：两边都是真相，就得每次冲突都选一边为准，另一边看到的就是假数据。
          </span>
        </div>
      </div>
    </section>

    <!-- ⑤ 外观（原型 `:184-252`）。这是**产品功能，不是原型调试开关**：
         四套配色与跟随系统都能真的切换，所以这一块不是禁用控件。 -->
    <section class="af-region af-settings__region" aria-label="外观">
      <div class="af-region__head">
        <h2 class="af-region__title">外观</h2>
        <span class="af-region__note">只影响这台设备，不同步到别的机器</span>
      </div>
      <div class="af-region__body af-stack">
        <!-- 四套配色：直接把「长什么样」摆出来，不靠色块让人猜 -->
        <div class="af-field">
          <span class="af-field__label">配色方案</span>
          <div class="af-themes" role="radiogroup" aria-label="配色方案">
            <button
              v-for="t in THEME_CHOICES"
              :key="t.id"
              type="button"
              class="af-theme"
              role="radio"
              :data-theme-pick="t.id"
              :aria-checked="picked === t.id ? 'true' : 'false'"
              @click="pickTheme(t.id)"
            >
              <span class="af-theme__prev" aria-hidden="true">
                <span class="af-theme__pv-bar"></span>
                <span class="af-theme__pv-line"></span>
                <span class="af-theme__pv-line af-theme__pv-line--short"></span>
              </span>
              <span class="af-theme__name">{{ t.name }}</span>
              <span class="af-theme__desc">{{ t.desc }}</span>
            </button>
          </div>
        </div>

        <!-- 跟随系统：很多人要的不是「选一套」，而是「白天亮、晚上暗」 -->
        <div class="af-row">
          <span
            class="af-switch"
            role="switch"
            :aria-checked="followOSOn ? 'true' : 'false'"
            id="af-follow-os"
            tabindex="0"
            @click="onToggleFollowOS"
            @keydown.enter.prevent="onToggleFollowOS"
            @keydown.space.prevent="onToggleFollowOS"
          >
            <span class="af-switch__track" aria-hidden="true"></span>
            <span>跟随系统的明暗设置</span>
          </span>
          <span class="af-2xs af-mute">打开后不再用上面选的那套，而是按系统自动在冷调 / 暖调里挑</span>
        </div>

        <div class="af-callout">
          <span class="af-callout__k">为什么只存在这台设备上</span>
          <span>
            配色是看的人的事，不是项目的事。在笔记本上换成暖色，不该把同伴的屏幕也一起改掉。
            所以这条存浏览器本地，不走 Hub，也不占一次同步。
          </span>
        </div>
      </div>
    </section>

    <!-- 原有的四行（Hub 连接 / 改密码 / 模型 API key / 节点凭据）。
         **不删**：它们是这一页此前唯一诚实的部分，且「节点凭据」那条路
         （去设备页看指纹）有测试守着。归在五个原型分区之后，
         因为它们对应的是原型里没有单独成区的「连接与凭据」。 -->
    <section class="af-region af-settings__region" aria-label="连接与凭据">
      <div class="af-region__head">
        <h2 class="af-region__title">连接与凭据</h2>
        <span class="af-region__note">这一版只定结构，控件全禁用</span>
      </div>
      <div class="af-region__body af-settings__rows">
        <div v-for="row in ROWS" :key="row.title" class="af-settings__row">
          <div class="af-settings__rowhead">
            <h3 class="af-settings__title">{{ row.title }}</h3>
            <p class="af-field__hint">{{ row.hint }}</p>
          </div>
          <div class="af-settings__rowbody">
            <input
              class="af-input"
              :type="row.type ?? 'text'"
              :placeholder="row.placeholder"
              disabled
              :aria-label="`${row.title}（M2 起可改）`"
            />
            <p class="af-field__hint">{{ row.m2 }}</p>
          </div>
        </div>
      </div>
    </section>

    <div class="af-settings__foot">
      <p class="af-field__hint">
        M1 的凭据只有一处：设备侧的密钥对。它在设备上生成、私钥永不上传，
        Hub 只存指纹——在
        <button class="af-link" type="button" @click="navigate('/devices')">设备页</button>
        逐台可见。
      </p>
      <p class="af-field__hint">
        这里不列「任务分发」「推理网关」这类 M5+ 的开关：留一个灰掉的导航项等于在导航里
        承诺一件本阶段做不到的事。
      </p>
    </div>
    </div>
  </div>
</template>

<style scoped>
/* 内容层：原型 `settings.html:26` 是一层内联 `padding:20px 20px 64px` 的
   `.af-stack`（`gap: var(--s4)`），并带 `max-width:960px`。
   静态内联样式被 `prototype-parity` 门禁禁止，落成这一层。 */
.af-settings__stack {
  padding: var(--p-s5) var(--p-s5) var(--p-s8);
  display: flex;
  flex-direction: column;
  gap: var(--p-s4);
  max-width: 960px;
}
/* 五个分区之间的竖向节奏。**不写进 `components.css`**：那里是页面外壳的
   契约，这些间距只属于本页的分区堆叠。 */
.af-settings__region {
  margin-bottom: var(--af-space-6);
}
/* 分区内再嵌一个 `.af-region`（合并权那张表）。原型就是这么套的
   （`settings.html:39`）：表格自带外框，与正文分开。 */
.af-settings__inner {
  margin: var(--af-space-3) 0;
}
.af-settings__p {
  margin: 0 0 var(--af-space-3);
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
  /* **不再单独限宽**：本页内容层 `.af-settings__stack` 已经是 960px 的阅读栏，
     段落再套一层 `--af-measure` 会窄成半幅、右边留一条无用的空白（实测过）。
     需要限宽的是「没有外层栏目」的页面，例如 Agent 目录与设备页。 */
}
.af-settings__p--tight {
  margin-bottom: 0;
}
.af-settings__callout {
  margin-top: var(--af-space-3);
}
.af-settings__why,
.af-settings__when {
  font-size: var(--af-text-sm);
}
.af-settings__cur {
  margin-left: var(--af-space-2);
}
.af-settings__tablefoot {
  margin: 0;
  padding: var(--af-space-3) var(--af-space-4);
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
}
.af-settings__num {
  width: 88px;
}
.af-settings__fields {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: var(--af-space-5);
}
.af-settings__exits {
  display: flex;
  align-items: center;
  gap: var(--af-space-5);
  flex-wrap: wrap;
}
.af-settings__exit {
  display: flex;
  align-items: center;
  gap: var(--af-space-2);
  font-size: var(--af-text-sm);
  color: var(--af-fg-secondary);
}
/* ── 外观：四张配色卡 + 跟随系统开关（原型 `app.css:725-793`、`636-651`）──
   这些类在原型里是全局的，但生产样式表尚未移植（`.af-theme*` / `.af-switch__track`
   在 `components.css` / `prototype-shell.css` 里命中 0）。按 design-system §（页面级
   共享类）的做法先落在本页 scoped 块里，逐个值照原型搬，不发明档位。 */
.af-themes {
  display: grid;
  gap: var(--af-space-6);
  grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  margin-top: var(--af-space-4);
}
.af-theme {
  font: inherit;
  text-align: left;
  cursor: pointer;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-4);
  padding: var(--af-space-4);
  background: var(--af-surface-2);
  border: 1px solid var(--af-border-subtle);
  border-radius: var(--af-radius-lg);
  color: var(--af-fg-primary);
  position: relative;
  transition: border-color var(--af-dur-fast) var(--af-ease-out),
    background var(--af-dur-fast) var(--af-ease-out);
}
.af-theme:hover {
  border-color: var(--af-border-strong);
  background: var(--af-surface-3);
}
.af-theme[aria-checked='true'] {
  border-color: var(--af-accent);
  background: var(--af-accent-bg);
  box-shadow: inset 0 0 0 1px var(--af-accent);
}
.af-theme:focus-visible {
  outline: 2px solid var(--af-accent-hover);
  outline-offset: 2px;
}
/* 选中项的勾：不只靠边框，色盲下也分得出来（原型 `app.css:780-790`）。 */
.af-theme::after {
  content: '✓';
  position: absolute;
  top: var(--af-space-4);
  right: var(--af-space-4);
  width: 16px;
  height: 16px;
  border-radius: var(--af-radius-full);
  display: grid;
  place-items: center;
  font-size: var(--af-text-2xs);
  font-weight: var(--af-weight-semibold);
  line-height: 1;
  background: var(--af-accent);
  color: var(--af-fg-on-accent);
  opacity: 0;
  transform: scale(0.7);
  transition: opacity var(--af-dur-fast) var(--af-ease-out),
    transform var(--af-dur-fast) var(--af-ease-out);
}
.af-theme[aria-checked='true']::after {
  opacity: 1;
  transform: scale(1);
}
/* 缩略图用**该套配色自己的色值**，不跟当前主题走——它存在的意义就是
   「显示别套配色的样子」。这是全站唯一允许写死色值的地方
   （原型 `app.css:760-776` 的原话），值逐条照抄。 */
.af-theme__prev {
  display: flex;
  flex-direction: column;
  gap: 5px;
  height: 46px;
  padding: 6px 8px;
  border-radius: var(--af-radius-md);
  border: 1px solid var(--af-border-strong);
  overflow: hidden;
}
.af-theme__pv-bar {
  height: 7px;
  border-radius: 2px;
  margin-bottom: 2px;
}
.af-theme__pv-line {
  height: 4px;
  border-radius: 2px;
  width: 82%;
}
.af-theme__pv-line--short {
  width: 55%;
}
[data-theme-pick='neutral'] .af-theme__prev { background: #191d21; }
[data-theme-pick='neutral'] .af-theme__pv-bar { background: #272c31; }
[data-theme-pick='neutral'] .af-theme__pv-line { background: #cdd2d9; }
[data-theme-pick='warm'] .af-theme__prev { background: #1d1c19; }
[data-theme-pick='warm'] .af-theme__pv-bar { background: #2b2a26; }
[data-theme-pick='warm'] .af-theme__pv-line { background: #d2cec6; }
[data-theme-pick='paper'] .af-theme__prev { background: #eceef1; }
[data-theme-pick='paper'] .af-theme__pv-bar { background: #f8f9fb; }
[data-theme-pick='paper'] .af-theme__pv-line { background: #2a2f36; }
[data-theme-pick='cream'] .af-theme__prev { background: #efece4; }
[data-theme-pick='cream'] .af-theme__pv-bar { background: #faf8f2; }
[data-theme-pick='cream'] .af-theme__pv-line { background: #33302a; }
.af-theme__name {
  font-size: var(--af-text-xs);
  font-weight: var(--af-weight-semibold);
  color: var(--af-fg-primary);
}
.af-theme__desc {
  font-size: var(--af-text-2xs);
  color: var(--af-fg-secondary);
  line-height: var(--af-leading-snug);
}
/* 跟随系统开关：原型是 `span.af-switch[role=switch][aria-checked]` + `__track`
   （`app.css:636-651`）。`.af-switch` 这个名字在生产里已经给了看板页的项目
   切换器，所以这里必须把三件事都写好（inline-flex / gap / 字号），不能只补
   `__track`——否则它会继承切换器的 `position: relative` 与裸排。 */
.af-switch {
  display: inline-flex;
  align-items: center;
  gap: var(--af-space-4);
  font-size: var(--af-text-md);
  color: var(--af-fg-primary);
  cursor: pointer;
}
.af-switch:focus-visible {
  outline: 2px solid var(--af-accent-hover);
  outline-offset: 2px;
}
.af-switch__track {
  width: 34px;
  height: 20px;
  border-radius: var(--af-radius-full);
  flex: 0 0 auto;
  background: var(--af-surface-3);
  border: 1px solid var(--af-border-default);
  position: relative;
  transition: background var(--af-dur-fast) var(--af-ease-out);
}
.af-switch__track::after {
  content: '';
  position: absolute;
  top: 2px;
  left: 2px;
  width: 14px;
  height: 14px;
  border-radius: var(--af-radius-full);
  background: var(--af-fg-secondary);
  transition: transform var(--af-dur-fast) var(--af-ease-out),
    background var(--af-dur-fast) var(--af-ease-out);
}
.af-switch[aria-checked='true'] .af-switch__track {
  background: var(--af-accent);
  border-color: var(--af-accent);
}
.af-switch[aria-checked='true'] .af-switch__track::after {
  transform: translateX(14px);
  background: var(--af-fg-on-accent);
}

/* ── 原来的四行：排版不变，只把外层从「页面级行」改成「分区内的行」 ── */
.af-settings__rows {
  display: flex;
  flex-direction: column;
}
.af-settings__row {
  display: flex;
  align-items: flex-start;
  gap: var(--af-space-6);
  padding: var(--af-space-4) 0;
  border-bottom: 1px solid var(--af-border-subtle);
}
.af-settings__row:last-child {
  border-bottom: 0;
  padding-bottom: 0;
}
.af-settings__row:first-child {
  padding-top: 0;
}
.af-settings__rowhead {
  flex: 0 0 200px;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-settings__title {
  margin: 0;
  font-size: var(--af-text-md);
  font-weight: var(--af-weight-medium);
  color: var(--af-fg-primary);
}
.af-settings__rowbody {
  flex: 1 1 auto;
  max-width: 560px;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-settings__rowbody .af-input {
  width: 100%;
}
.af-settings__foot {
  margin-top: var(--af-space-8);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
  max-width: 70ch;
}
.af-settings__foot p {
  margin: 0;
}
@media (max-width: 900px) {
  .af-settings__row {
    flex-direction: column;
    gap: var(--af-space-3);
  }
  .af-settings__rowhead {
    flex: 0 0 auto;
  }
}
</style>
