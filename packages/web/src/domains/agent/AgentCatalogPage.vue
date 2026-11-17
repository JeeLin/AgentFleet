<script setup lang="ts">
/* ===========================================================================
   AgentCatalogPage —— Agent（`prototype/agents.html`，v0.7.0 子任务 8）
   ---------------------------------------------------------------------------
   Agent 从 v0.7.0 起重回**一级导航**（新原型把它单独成一页）。v0.5.0 曾把它
   并进设备页的 `?view=agents` 分段，那时侧栏只留三项；项目主轴改版后
   「从设备 / agent 进」是管理面的两个并列入口，分开才对。

   这一页是**真数据**：`AgentCatalog` 本来就自读自管（`GET /api/devices` 的
   agent 字段），不需要重写。子任务 7 已经补上了每台设备上的可用性与原因。

   ── 分区对齐（v0.8.0 子任务 12）────────────────────────────────────
   原型 `prototype/agents.html` 有 4 个 `<h2>` 分区，此前本页只有 1 个
   （「任务与成功率」）。补齐的判据是**原型有的必须有**，四块与原型一一对应：

     | 原型 | 分区 | 本页怎么落地 |
     |---|---|---|
     | `:36` | 设备上的 Agent | 交给 `<AgentCatalog>`（它已有真数据与空态） |
     | `:85` | 启动配置 | 新增分区说明；Hub 侧还没有读写启动配置的端点 |
     | `:122` | 能力由任务授予 | 新增分区，静态表（产品原则，不是每页不同的数据） |
     | `:166` | 问 Agent 的事 | 新增分区，指向管家（v0.8.x 未接） |

   **原型第 2 块（启动配置）在原型里是「某个 agent 在某台设备上的配置漂移」**，
   而 Hub 现在**没有任何读写启动配置的接口**（`domains/agent/api.ts` 只有
   `listAgents` / `getAgentDetail`）。按规格「缺 API 的不在本子任务补」，
   这一块画成**结构完整但数据缺席**的分区，并写明缺的是什么能力——
   与「留白」区分开，留白会被当成加载失败（规范 §3.11）。

   ── 为什么第 3、4 块不读数据 ──────────────────────────────────────
   原型这两块本身就是**固定的产品说明**（「能力由任务授予」是一张三行的
   可否表、「问 Agent 的事」是一段引导），不是按设备变化的数据。把它们做成
   数据驱动等于凭空发明一套后端契约——那正是本子任务明令不做的。

   **不把 `<AgentCatalog>` 的加载逻辑复制过来**：它有 `load` 与自身的空态，
   复制一份必然漂移（一份加了重试、另一份没加）。
   =========================================================================== */

import AgentCatalog from '@/domains/agent/AgentCatalog.vue'
import PageHead from '@/domains/shared/components/PageHead.vue'

/**
 * 「能力由任务授予」那张表。**照 `prototype/agents.html:126-147` 逐行搬**，
 * 不合并、不改写：这三行是产品边界（什么能做、什么不做），措辞一变意思就变。
 *
 * `ok` 只决定徽标配色，不参与任何判定——这里的「能不能」是产品承诺，
 * 不是运行时计算结果。
 */
interface CapabilityRow {
  /** 方式 */
  way: string
  /** 能不能 */
  verdict: string
  /** 徽标变体：`ok` 主路径 / `danger` 禁止 / `info` 并行支持 */
  tone: 'ok' | 'danger' | 'info'
  /** 为什么 */
  why: string
}

const CAPABILITIES: readonly CapabilityRow[] = [
  {
    way: 'Agent 直接调用任意连接器',
    verdict: '禁止',
    tone: 'danger',
    why: '等于开一条统一提权通道，还要我们替它维护权限',
  },
  {
    way: '按任务授予「技能」（这个任务可以连 db-prod）',
    verdict: '主路径',
    tone: 'ok',
    why: '任务没声明就没有，不是默认能力',
  },
  {
    way: '设备上起一个 MCP server，Agent 按标准协议连',
    verdict: '并行支持',
    tone: 'ok',
    why: '通用接口，支持 MCP 的 Agent 都能用，我们不必为每个 Agent 写适配',
  },
]
</script>

<template>
  <!-- 页面骨架照原型 `agents.html:15-23`：`header.af-head` 是 `.af-body` 的
       兄弟，内容在 `.af-body` 里。上一版是自造的 `.af-page` / `.af-page__head`
       / `.af-page__title`——那一族规则已随 v0.8.1 子任务 2 的类名删除，于是
       这一页是一张没有页头几何的裸页面（`class-usage` 门禁只扫 10 个已交付
       页面，`/agents` 不在其中，所以一直没被发现；2026-11-17 子任务 10 逐页
       扫 `af-page` 残留时抓到）。 -->
  <PageHead>
    <div>
      <h1 class="af-head__title">Agent</h1>
      <p class="af-head__sub">
        管理面 · 各设备上装了哪些 agent、哪些跑不起来。Hub 不代装，只扫描与读写启动配置。
      </p>
    </div>
  </PageHead>

  <div class="af-body">
    <div class="af-agentpage__stack">

    <!-- 这里原本是一块通栏 `.af-callout--info`「装 Agent 这件事不归产品管」，
         三段散文占满首屏（v0.8.1 子任务 13 移除）。它要防的误读是真的——
         「这里能装 agent」——但防它的正确位置是**「设备上的 Agent」那块的标题旁**，
         不是页头下面。那句话现在作为 `AgentCatalog` 分区的 note 出现，
         读者在准备找「安装」按钮时正好看见它。 -->

    <!-- ① 设备上的 Agent（原型 `:36`）。
         标题归 `AgentCatalog` 自己写：它要按真数据算出「N 台设备上的 M 个 Agent」
         与「可用 X · 不可用 Y」，本页拿不到那份计数（列表在子组件里）。 -->
    <AgentCatalog />

    <!-- ② 启动配置（原型 `:85`）。
         原型在这里展示「某 agent 在某台设备上的配置漂移 + 怎么处置」。Hub 现在
         没有读写启动配置的端点，所以这里只画**分区与承诺**，并把缺的能力说清：
         说清「还没有」比画一个假的下拉框诚实（与 `SettingsPage` 同一口径）。 -->
    <section class="af-region af-agentpage__region" aria-label="启动配置">
      <div class="af-region__head">
        <h2 class="af-region__title">启动配置</h2>
        <span class="af-region__note">托管文件在设备上只读，本地改动被标成漂移</span>
      </div>
      <div class="af-region__body">
        <p class="af-muted af-agentpage__p">
          每个 agent 的启动配置由 Hub 保存一份，设备上落一份只读副本。本地被改过
          的文件会标成<b>漂移</b>——Hub 不会悄悄覆盖它，也不会替你决定听谁的。
        </p>
        <div class="af-callout">
          <span class="af-callout__k">这一块还没有接数据</span>
          <span>
            「哪份配置漂了、差在哪两行、把本地改动收上来还是把 Hub 配置推下去」
            需要一个读写启动配置的接口，Hub 目前没有。在它落地之前这里不摆一个
            按不动的按钮——那会让人以为功能坏了。
          </span>
        </div>
      </div>
    </section>

    <!-- ③ 能力由任务授予（原型 `:122`）。固定产品说明，不随设备变化。 -->
    <section class="af-region af-agentpage__region" aria-label="能力由任务授予">
      <div class="af-region__head">
        <h2 class="af-region__title">Agent 能用的能力，由任务授予</h2>
        <span class="af-region__note">不是默认开放</span>
      </div>
      <div class="af-region__body af-region__body--flush">
        <table class="af-table">
          <thead>
            <tr>
              <th>方式</th>
              <th>能不能</th>
              <th>为什么</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in CAPABILITIES" :key="row.way">
              <td>{{ row.way }}</td>
              <td>
                <span class="af-badge" :class="`af-badge--${row.tone}`">{{ row.verdict }}</span>
              </td>
              <td class="af-agentpage__why">{{ row.why }}</td>
            </tr>
          </tbody>
        </table>
        <p class="af-muted af-agentpage__note">
          凭据不会进 Agent 的上下文：Agent 说「用 db-prod 跑这条 SQL」，凭据由设备侧注入。
          每一次连接都进审计轨迹。
        </p>
        <p class="af-muted af-agentpage__note af-agentpage__note--last">
          这一页只讲原则。加数据库、看谁用过、轮换，都在<b>技能与 MCP</b> 页——
          那一页还没做，所以这里不给链接：留一个灰掉的入口等于在导航里承诺一件
          本阶段做不到的事。
        </p>
      </div>
    </section>

    <!-- ④ 问 Agent 的事（原型 `:166`）。原型这一块是**管家抽屉**，v0.8.x 未接。 -->
    <section class="af-region af-agentpage__region" aria-label="问 Agent 的事">
      <div class="af-region__head">
        <h2 class="af-region__title">问 Agent 的事</h2>
        <span class="af-region__note">哪个 Agent 更合适，是调度器算的</span>
      </div>
      <div class="af-region__body">
        <p class="af-muted af-agentpage__p">
          「哪个 Agent 更合适」由调度器判定；「这个 Agent 最近为什么老失败」归管家。
          管家面板还没有接进这个版本，所以这里不摆输入框。
        </p>
      </div>
    </section>
    </div>
  </div>
</template>

<style scoped>
/* 提示条与分区的竖向间隔。**不写进 `components.css`**：那是全站契约，
   这里的间距只服务本页的阅读节奏（页内 4 块，块间要留白）。 */
/* 内容层的内边距与块间距：原型 `agents.html:25` 是内联
   `padding:20px 20px 64px` 加 `af-stack`（12px gap）。静态内联样式被
   `prototype-parity` 门禁禁止，所以落成这一层 —— 与另外 9 页同一写法。 */
.af-agentpage__stack {
  padding: var(--p-s5) var(--p-s5) var(--p-s8);
  display: flex;
  flex-direction: column;
  gap: var(--p-s3);
}

.af-agentpage__region {
  margin-top: var(--af-space-6);
}
.af-agentpage__p {
  margin: 0 0 var(--af-space-3);
  font-size: var(--af-text-md);
  line-height: var(--af-leading-base);
  /* 长说明段落收窄到可读测度（v0.8.1 子任务 13）：原来横铺满 1216px，
     四十余字一行，读到一半就丢行。 */
  max-width: var(--af-measure);
}
.af-agentpage__p:last-child {
  margin-bottom: 0;
}
/* 表里的「为什么」列是长句，比默认单元格窄一点更好读 */
.af-agentpage__why {
  font-size: var(--af-text-sm);
}
.af-agentpage__note {
  margin: var(--af-space-3) var(--af-space-4) 0;
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
}
.af-agentpage__note--last {
  margin-bottom: var(--af-space-4);
}
</style>
