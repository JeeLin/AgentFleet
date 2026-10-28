/* ===========================================================================
   components.css 的**归属**断言
   ---------------------------------------------------------------------------
   这个文件守的是一条容易被违反、且违反后不会报错的规矩：

     「一条规则 **只有一个家**。」

   同一条规则在两个文件里各存一份（组件 scoped 块 + 全局表）时，改一处不改
   另一处是**静默**的——没有编译错误、没有测试变红，只有某一天两个长得一样
   的框开始不一样。所以这里不用 `toContain`（它看不见「出现了两次」），而是
   **数出现次数**：跨全部可能有它的文件，必须恰好 1 次。

   判据取 `选择器 + 声明体`，不是整段文本：声明体相同才叫重复，光是类名出现
   （模板里的 `class="af-tablewrap"`）不算。

   ── 为什么必须先剥注释 ────────────────────────────────────────────
   `src/test/css.ts` 的 `cssRuleOf` **不**剥注释，本文件也不直接用它：这些
   CSS 的注释里就写着类名与属性值（解释「为什么这么写」），不剥掉的话把声明
   整段删光、只在注释里留一句，断言照样绿——那是恒真断言，正是 `css.ts` 文件
   头点名的那类假测试。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import { readRepoFile } from '@/test/css'

/**
 * 可能有这些规则的全部文件：全局表 + 用到它们的组件 / 页面。
 *
 * 只列**本里程碑归属范围内**的 .vue——`ConvFlow.vue` 不在其中（见
 * `src/components/ConvFlow.vue` 的 `.af-stage-note` 待办），所以不接受
 * 「把某个文件从这份名单里删掉」当作通过方式。
 *
 * **名单本身就是门禁的边界**：`countRule` 只遍历这里，所以漏掉一个文件等于
 * 那条规则在它身上完全不受管——而症状是数出来变**小**，不会变红。这份名单
 * 曾漏掉 `TrajDrawer.vue`（自带十几条 `.af-traj__*` 规则）与 `SessionPage.vue`
 * / `DevicesPage.vue`，而文件头当时已经自称「跨全部可能有它的文件」。
 * 加新组件时**先加进这里**，再写它的 scoped 规则。
 */
const OWNED = [
  'packages/web/src/styles/components.css',
  'packages/web/src/components/AgentCatalog.vue',
  'packages/web/src/components/DeviceTable.vue',
  'packages/web/src/components/SessionTable.vue',
  'packages/web/src/components/TrajDrawer.vue',
  'packages/web/src/pages/DevicesPage.vue',
  'packages/web/src/pages/NewSessionPage.vue',
  'packages/web/src/pages/SessionPage.vue',
  'packages/web/src/pages/SessionsPage.vue',
]

/** 剥掉 `/* *\/` 注释，只留真正的 CSS。 */
function stripComments(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, '')
}

/** 归一化：空白折叠成一个空格，缩进 / 换行差异不算「不同」。 */
function norm(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}

/**
 * 数一条规则在全部受管文件里出现了几次。
 *
 * `rule` 写成 `选择器 { 声明 }` 的完整形状（声明体之间用分号分隔即可，
 * 顺序敏感——两条规则声明顺序不同就是不逐字相同，那不是重复）。
 */
function countRule(rule: string): number {
  const needle = norm(rule)
  let n = 0
  for (const file of OWNED) {
    // 干草堆同样要归一化：同一条规则在全局表里是一行、在组件的 scoped 块里
    // 是每行一条声明（缩进也不同），两边都折叠成一行才数得出来。
    const css = norm(stripComments(readRepoFile(file)))
    let from = 0
    for (;;) {
      const at = css.indexOf(needle, from)
      if (at < 0) break
      n += 1
      from = at + needle.length
    }
  }
  return n
}

/** 这条规则是否住在全局表里（而不是住在某一个组件的 scoped 块里）。 */
function inSharedSheet(rule: string): boolean {
  const css = norm(stripComments(readRepoFile('packages/web/src/styles/components.css')))
  return css.includes(norm(rule))
}

describe('一条规则只有一个家（跨受管文件数次数，toContain 看不见重复）', () => {
  it('.af-tablewrap：设备表与会话表共用一个框，只在全局表 §8 定义一次', () => {
    const rule = `.af-tablewrap {
      border: 1px solid var(--af-border-subtle);
      border-radius: var(--af-radius-md);
      overflow: auto;
    }`
    // 逐字量，不是 toContain：两份都在时这里是 2，只删一份也是 2
    expect(countRule(rule), '`.af-tablewrap` 必须在受管文件里恰好出现一次').toBe(1)
    // 并且它得住在全局表，不是住在某一个组件里
    expect(inSharedSheet(rule), '`.af-tablewrap` 必须住在 components.css').toBe(true)
  })

  it('.af-keyhint：新建页与会话列表页共用一档字号，只在全局表 §2 定义一次', () => {
    const rule = `.af-keyhint {
      font-size: var(--af-text-sm);
    }`
    expect(countRule(rule), '`.af-keyhint` 必须在受管文件里恰好出现一次').toBe(1)
    expect(inSharedSheet(rule), '`.af-keyhint` 必须住在 components.css').toBe(true)
  })

  /**
   * `.af-stage-note` **故意不在这里收敛**：它在 `AgentCatalog.vue` 与
   * `ConvFlow.vue` 各有三个规则，而两者**不是**逐字相同——
   *   目录那份是一张独立卡片（四边描边 + 圆角 + margin-bottom，正文 md）；
   *   对话那份是贴在流上的一条（只画 border-bottom，正文 xs）。
   * 所以「提到全局」在这题上不是正解：照着「逐字重复」直接合并会把其中
   * 一个渲染成错的。要收敛得先定 `--af-*` 变体，或让两份各自留本地只共用
   * 真正相同的那几条——那需要同时改 `ConvFlow.vue`（不在本文件受管范围内）。
   *
   * 这里钉两件事：两份**确实不同**（免得后来人照旧结论直接合并），而且
   * 差异是**双向**的——不只「目录有卡片味、对话没有」，也要钉住「对话那一份
   * 不能长出卡片味」。只钉单向时，给 `flow` 补一句 `border-radius` 就能把
   * 「贴在流上的一条」悄悄变成「第二张卡片」，而六条断言一条都不红。
   */
  it('.af-stage-note 的两份不是逐字重复（合并前必须先定变体）', () => {
    const catalog = stripComments(readRepoFile('packages/web/src/components/AgentCatalog.vue'))
    const flow = stripComments(readRepoFile('packages/web/src/components/ConvFlow.vue'))
    expect(catalog).toMatch(/\.af-stage-note\s*\{[^}]*border-radius/)
    expect(catalog).toMatch(/\.af-stage-note__body\s*\{[^}]*--af-text-md/)
    expect(flow).toMatch(/\.af-stage-note\s*\{[^}]*border-bottom/)
    expect(flow).toMatch(/\.af-stage-note__body\s*\{[^}]*--af-text-xs/)

    // 反证**双向**：两边都不能出现对方的形态。少写 `flow` 那两条反向断言时，
    // 给 `flow` 补 `border-radius` 会让两份长得一样而测试全绿。
    expect(catalog).not.toMatch(/\.af-stage-note\s*\{[^}]*border-bottom/)
    expect(flow).not.toMatch(/\.af-stage-note\s*\{[^}]*border-radius/)
    expect(catalog).not.toMatch(/\.af-stage-note__body\s*\{[^}]*--af-text-xs/)
    expect(flow).not.toMatch(/\.af-stage-note__body\s*\{[^}]*--af-text-md/)
  })
})

describe('页面级布局的规模（文件头「三族合计 ≤30 条规则」的门禁）', () => {
  /**
   * 「页面级布局」= 页面外壳的那三族：.af-page（列表/设置页外壳）、
   * .af-auth（登录 / setup 的居中窄列）、.af-wb（会话工作台）。
   *
   * 用「条」（规则数）不用「行」：一条规则写成一行还是七行只差换行，行数
   * 量的是排版、不是规模——文件头已经写明这条账。
   */
  const PAGE_LAYOUT = ['.af-page', '.af-auth', '.af-wb']

  it('三族合计不超过 30 条规则（超了就该拆组件，不是继续往页面壳里堆）', () => {
    const css = stripComments(readRepoFile('packages/web/src/styles/components.css'))
    let total = 0
    for (const prefix of PAGE_LAYOUT) {
      const escaped = prefix.replace('.', '\\.')
      const hits = css.match(new RegExp(`^\\s*${escaped}[^{\\n]*\\{`, 'gm')) ?? []
      // 单看一族也要有东西：`.af-page` 是 5 个页面共用的外壳，一条都没有
      // 说明这一族被整个搬走了，页面壳会散掉。
      expect(hits.length, `${prefix} 一族的规则不见了`).toBeGreaterThan(0)
      total += hits.length
    }
    expect(total, `页面级布局三族合计 ${total} 条，超过 30`).toBeLessThanOrEqual(30)
  })
})
