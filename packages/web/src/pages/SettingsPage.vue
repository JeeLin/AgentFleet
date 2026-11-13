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
   `prototype/settings.html` 有**五个** `<h2>` 分区（`:31` 合并权 / `:82` 决策
   超时 / `:111` 磁盘 / `:148` 通知出口 / `:186` 外观），而本页此前是四行
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

import { navigate } from '@/router'

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

/** 清理策略三档（原型 `:126-130`）。与绑定时那个下拉是同一套取值。 */
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
</script>

<template>
  <div class="af-page af-settings">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">设置</h1>
        <p class="af-page__sub">
          M2 范围。这一页先定结构：<b>没有任何一项现在能改</b>，每个控件都是禁用的，
          旁边写明 M2 时会做什么。列在这里是因为导航需要知道它存在，不是为了先画一个假表单。
        </p>
      </div>
      <span class="af-spacer"></span>
      <span class="af-badge af-badge--off">M2 起可改</span>
    </div>

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

    <!-- ⑤ 外观（原型 `:184-252`） -->
    <section class="af-region af-settings__region" aria-label="外观">
      <div class="af-region__head">
        <h2 class="af-region__title">外观</h2>
        <span class="af-region__note">只影响这台设备，不同步到别的机器</span>
      </div>
      <div class="af-region__body">
        <p class="af-settings__p">
          四套配色（甲 · 中性冷灰 / 乙 · 暖米灰 / 丙 · 纸白偏冷 / 丁 · 米白偏暖），
          以及「跟随系统的明暗设置」。
        </p>
        <div class="af-settings__themes">
          <button
            v-for="(name, i) in ['甲 · 中性冷灰', '乙 · 暖米灰', '丙 · 纸白偏冷', '丁 · 米白偏暖']"
            :key="name"
            class="af-btn af-btn--sm"
            type="button"
            :class="{ 'af-btn--primary': i === 0 }"
            disabled
          >
            {{ name }}
          </button>
        </div>
        <div class="af-callout af-settings__callout">
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
</template>

<style scoped>
.af-settings {
  max-width: 900px;
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
.af-settings__themes {
  display: flex;
  gap: var(--af-space-2);
  flex-wrap: wrap;
  margin-bottom: var(--af-space-3);
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
