import { describe, expect, it } from 'vitest'
import { readRepoFile } from '@/test/css'
import { classNamesIn, parseCssRules, propValue, ruleFor, stripCssComments } from '@/test/css-scan'

import { readdirSync, readFileSync } from 'node:fs'
import { resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

/* ===========================================================================
   子任务 12 的门禁：层级阶梯 + 卡片分区
   ---------------------------------------------------------------------------
   **这个文件守的是「视觉属性」，而 happy-dom 不做布局**——它量不出行高、间距、
   字号的实际渲染值。所以这里退一步：断言**令牌定义与类存在性**，几何由
   子任务 14 在活 Hub 上用 playwright 实测（见本文的 28 组合表）。

   为什么仍然值得写：本里程碑已有实证——「行高 1.7 改成 1.65」这种变异，
   1171 条用例全绿。没有这层断言，改坏 `--af-text-display` 也没人发现。

   **断言的是规则文本而非结构**：`--af-text-display` 的取值用正则从
   `tokens.css` 里抠出来对比，而不是 `getComputedStyle`。这是有意的妥协，
   理由写在 `tokens.css` 的取值注释里。
   =========================================================================== */

/** 从 `:root` 里取一个 token 的数值（px）。找不到就抛——静默返回 0 会让断言假绿。 */
function tokenPx(css: string, name: string): number {
  const m = new RegExp(`--${name}:\\s*(\\d+(?:\\.\\d+)?)px`).exec(css)
  if (!m) throw new Error(`token --${name} 未找到或以非 px 定义`)
  return Number(m[1])
}

/**
 * 全仓 `.vue` 里出现某类名（或任意片段）的文件路径列表。
 *
 * **不钉单文件**：把「谁消费了这类」写死成一个路径，改天消费者挪到别的页面，
 * 守卫就会指着一个无辜的文件报错（v0.8.1 第4轮的 `.af-panel__foot` 就是这样）。
 */
function vueFilesUsing(needle: string): string[] {
  const webRoot = fileURLToPath(new URL('../..', import.meta.url))
  const hits: string[] = []
  const walk = (dir: string): void => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = resolve(dir, entry.name)
      if (entry.isDirectory()) walk(full)
      else if (entry.name.endsWith('.vue') && readFileSync(full, 'utf-8').includes(needle)) {
        hits.push(full.slice(webRoot.length).split(sep).join('/'))
      }
    }
  }
  walk(resolve(webRoot, 'src'))
  return hits.sort()
}

describe('层级阶梯（子任务 12）', () => {
  const tokens = readRepoFile('packages/web/src/styles/tokens.css')

  it('三档字号严格递增，且 display > md 至少 5px', () => {
    const display = tokenPx(tokens, 'af-text-display')
    const card = tokenPx(tokens, 'af-text-card')
    const body = tokenPx(tokens, 'af-text-md')

    // 严格递增：任何一档被改回与另一档同值，这里就红。
    expect(display).toBeGreaterThan(card)
    expect(card).toBeGreaterThan(body)

    /* display 与 body 的差必须够大才「扫一眼分得清」。原来 18 vs 14.5 只差
       3.5px，是用户判定「页面很差」的直接原因之一，所以钉住这个下界。 */
    expect(display - body).toBeGreaterThanOrEqual(5)
  })

  it('相邻两档至少差 2px（避免撞档）', () => {
    const display = tokenPx(tokens, 'af-text-display')
    const card = tokenPx(tokens, 'af-text-card')
    const body = tokenPx(tokens, 'af-text-md')
    expect(display - card).toBeGreaterThanOrEqual(2)
    expect(card - body).toBeGreaterThanOrEqual(2)
  })

  it('可读测度令牌存在且是合理值', () => {
    const m = /--af-measure:\s*(\d+)ch/.exec(tokens)
    expect(m, '--af-measure 必须以 ch 定义').not.toBeNull()
    const ch = Number(m![1])
    // 45~90ch 是可读区间；超出就是「没量过」的取值。
    expect(ch).toBeGreaterThanOrEqual(45)
    expect(ch).toBeLessThanOrEqual(90)
  })

  /* **这条守的是一类反复出现的缺陷**：标题类（`*__title` / `*__heading`）
     用了比正文更小的字号。实证两处——`.af-region__title` 原为 13.5px、
     `.af-panel__title` 原为 14.5px（与正文同档），都压不住内容。
     标题可以同大但要靠字重/明度区分，**绝不能更小**。 */
  it('任何 *__title 的字号都不小于正文', () => {
    const body = tokenPx(tokens, 'af-text-md')
    const cssFiles = [
      readRepoFile('packages/web/src/styles/components.css'),
      readRepoFile('packages/web/src/styles/prototype-shell.css'),
    ]
    const offenders: string[] = []
    // 只查用 token 声明的字号；写死 px 的另由 design-system 的取值纪律管。
    for (const css of cssFiles) {
      const re = /^(\.[a-z][a-z0-9_-]*__title[a-z0-9_-]*)\s*\{([^}]*)\}/gm
      for (const m of css.matchAll(re)) {
        const sel = m[1]
        const bodyBlock = m[2]
        // 只看直接声明 font-size 的那条；继承的不算。
        const fs = /font-size:\s*var\((--[a-z0-9-]+)\)/.exec(bodyBlock)
        if (!fs) continue
        const name = fs[1].replace(/^--/, '')
        // text-card / text-display / text-lg 及以上都合规；sm/xs/2xs 违规。
        const pxMatch = new RegExp(`--${name}:\\s*(\\d+(?:\\.\\d+)?)px`).exec(tokens)
        if (!pxMatch) continue // 非字号 token（如 --p-t-sm）跳过，由主题对照管
        if (Number(pxMatch[1]) < body) offenders.push(`${sel} = ${pxMatch[1]}px < ${body}px`)
      }
    }
    expect(offenders, offenders.join('; ')).toEqual([])
  })
})

describe('面板三分区（子任务 12）', () => {
  const tokens = readRepoFile('packages/web/src/styles/tokens.css')

  /* **这条守的是一个真实回潮（v0.8.1 第3轮）**：为了把两处内联 `font-size`
     收进样式表，`components.css` 补了 `.af-t-2xs` / `.af-t-sm` 两条——而
     `prototype-shell.css` 的 `.af-xs`（`:608`）/ `.af-2xs`（`:609`）早就在做
     同一件事（`--p-t-2xs` 就是 `var(--af-text-2xs)`），键位提示句另有
     `.af-keyhint`。于是同一档字号有两个名字，改一处另一处不跟。**判据**：每条声明
     `font-size: var(<字号令牌>)` 的规则，其取值令牌不许被第二条规则重复消费
     （同一 token 只许一个工具类）。豁免：组件本体类（`*__title` / `*card*`
     这类）不在此列——它们是组件的一部分，不是通用工具类。 */
  it('同一档字号令牌只许一个工具类消费（不许给字号档造别名）', () => {
    const cssFiles = [
      readRepoFile('packages/web/src/styles/components.css'),
      readRepoFile('packages/web/src/styles/prototype-shell.css'),
    ]
    /* 只查「一个类名 + 一条 font-size」这种纯工具类规则：选择器以单类开头、
       不含伪类/后代/属性，声明体里除了 font-size 只有空白或注释。
       **组件本体类豁免**：`.af-state__glyph` 是 12px 状态字形的一部分
       （与 `line-height: 1`、`[data-glyph]` 变体成组），不是通用的「字号阶梯」——
       它碰巧取到 2xs 这个值，不代表它是一个可以复用的档位工具类。 */
    const COMPONENT_OWNED = /^\.af-state__/
    const users = new Map<string, string[]>()
    for (const css of cssFiles) {
      const stripped = stripCssComments(css)
      for (const m of stripped.matchAll(/(?:^|\n)(\.[a-z][a-z0-9_-]*)\s*\{([^}]*)\}/g)) {
        const sel = m[1]
        const body = m[2].trim()
        if (COMPONENT_OWNED.test(sel)) continue
        const fs = /^font-size:\s*var\((--(?:af|p)-t(?:ext)?-[a-z0-9]+)\);\s*$/.exec(body)
        if (!fs) continue
        // 把 --p-* 别名还原到真正的字号令牌，否则 --p-t-2xs 与 --af-text-2xs 会被当成两档。
        const raw = fs[1]
        const alias = new RegExp(`${raw}:\\s*var\\((--[a-z0-9-]+)\\)`).exec(tokens)
        const token = alias ? alias[1] : raw
        users.set(token, [...(users.get(token) ?? []), sel])
      }
    }
    const dupes = [...users.entries()]
      .filter(([, sels]) => sels.length > 1)
      .map(([tok, sels]) => `${tok} ← ${sels.join(' / ')}`)
    expect(dupes, `同一档字号有多个工具类（改一处另一处不跟）：${dupes.join('; ')}`).toEqual([])
  })

  const css = readRepoFile('packages/web/src/styles/components.css')

  /* **只断言实际存在的那几个**（第3轮精简）：这里原本还有 `.af-panel__foot`，
     而那一族**零模板消费者**——守卫在保护一段死代码，死代码又因为「有测试
     守着」显得像生效中的东西。脚分区需要时再加，加的时候连同它的消费者一起加。 */
  it('头/体三个类都有定义', () => {
    for (const cls of ['.af-panel', '.af-panel__head', '.af-panel__body']) {
      expect(css, `${cls} 缺定义`).toContain(`${cls} {`)
    }
  })

  /* **反向守卫：`.af-panel__foot` 不得只以规则形式回来**。它被删是因为没人用；
     如果哪天又需要，必须同时有模板消费者（下面那条「新加的类必须在模板里出现」
     会把「只加规则不加消费者」判红）。

     **消费者扫全仓 `.vue`，不钉某个文件**（v0.8.1 第4轮）：初版把消费者固定成
     `DevicesPage.vue`，而那一页 0 处 `af-panel__foot`——真有人按注释把规则加回来、
     消费者写进别的页面，这条会判红却把原因指向 `DevicesPage`，等于给下一个人
     一条错误线索。 */
  it('.af-panel__foot 要么不定义，要么必须在模板里被用到', () => {
    if (!css.includes('.af-panel__foot')) return
    const users = vueFilesUsing('af-panel__foot')
    expect(users, '.af-panel__foot 定义了但全仓没有一个 .vue 消费者').not.toEqual([])
  })

  it('面板标题用 card 档 + strong 前景（字号与明度一起抬）', () => {
    const block = cssBlockOf(css, '.af-panel__title')
    expect(block).toContain('--af-text-card')
    expect(block).toContain('--p-fg-strong')
    expect(block).toContain('--af-weight-semibold')
  })

  it('正文段落收窄到可读测度，但面板 body 本身不收窄', () => {
    const para = cssBlockOf(css, '.af-panel__body > p')
    expect(para).toContain('--af-measure')
    // `.af-panel__body` 本体不得带 max-width——里面可能有表格/看板要全宽。
    const body = cssBlockOf(css, '.af-panel__body')
    expect(body).not.toContain('max-width')
  })

  /* **这条守的是我自己反复踩的坑**：同一选择器在一个文件里定义两次，后者
     静默生效。写这次重设计时我连续踩了三次（`.af-card` 跨文件重名、
     `.af-panel__foot` 同文件两次、`.af-panel__head` 同文件两次），每次都是
     靠肉眼看出来，没有网。这里补上网：`components.css` 里顶层选择器
     （行首、到 `{` 为止）不许重复出现。 */
  it('components.css 里没有「后者静默全量覆盖前者」的重复选择器', () => {
    /* 按**结构**统计而不是按行：`parseCssRules` 已剥注释、按花括号配对，
       多行选择器、逗号组、`@media` 里的规则都在同一条口径下。
       （初版按行只认「行首单类 + 同行 `{` 结尾」，漏掉了 `.af-projectlist__dot
       { font-size: 0; … }` 这种**声明体写在同一行**的形态。）

       **判的是「静默全量覆盖」，不是「选择器出现两次」**：同一条选择器拆成
       几条互补的规则（几何一条、视觉一条）是**有意**的，只要前一条声明的
       属性没有被后一条全部重写，就没有内容丢失。只有当后一条重写了前一条的
       某个属性时，前一条的那条声明才是死的——那才是这条守卫要抓的东西。 */
    const declaredBefore = new Map<string, string[]>()
    const overridden: string[] = []
    for (const rule of parseCssRules(css)) {
      // 只统计**顶层**规则（不在任何 at-rule 里）：`@media` 内重写是合法覆盖。
      if (rule.atRules.length > 0) continue
      const sel = rule.selector.replace(/\s+/g, ' ').trim()
      const props = [...rule.body.matchAll(/(?:^|;)\s*([a-z-]+)\s*:/g)].map((m) => m[1])
      const prev = declaredBefore.get(sel) ?? []
      for (const p of props) {
        if (prev.includes(p)) overridden.push(`${sel} 的 ${p}`)
      }
      declaredBefore.set(sel, [...prev, ...props])
    }
    expect(overridden, `后一条规则重写了前一条已声明的属性（前者是死声明）：${overridden.join(', ')}`).toEqual(
      [],
    )
  })

  /* **这条守的是一个真踩过的坑**：第一版我新写了 `.af-card` 当容器，
     而 `prototype-shell.css` 的 `.af-card`（看板任务卡）后导入、同选择器
     全量覆盖，我那 20 行从头到尾没生效——与 F-2 修掉的 bug 同型。 */
  it('新加的面板类不得在 prototype-shell.css 里重名（否则静默失效）', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    for (const cls of ['.af-panel', '.af-panel__title', '.af-panel__head']) {
      expect(shell, `${cls} 在 prototype-shell.css 里也定义了，会被它覆盖`).not.toMatch(
        new RegExp(`^\\${cls}\\s*\\{`, 'm'),
      )
    }
  })

  /* **往上一条的缺口补网（v0.8.1 第3轮）**：上一条只钉了 4 个白名单类，于是
     `.af-row` 与 `.af-spacer` 这两条**跨文件重名**在网外活了整轮——两份都在，
     `components.css` 那份（`flex: 1 1 auto` / 不换行 + 6px gap）从来是死的。
     这里改成**全量求交集**，分两档口径：

     **严格口径**（`wholeSubject: true`）必须为**空**：`components.css` 与
     `prototype-shell.css` 不许有同名类的「全量定义」。后导入的
     `prototype-shell.css` 会整条覆盖前者，靠导入顺序决定谁赢是缺陷。

     **宽松口径**（`wholeSubject: false`，连后代选择器里的类也算）允许重叠，
     但**命中必须全部落在下面的白名单内**。宽松口径回答的是另一个问题：
     「这个类名是否同时出现在两份样式表里」——`.af-drawer .af-msg` 这种
     有意的局部覆盖也算命中，所以它天然不可能是空集。
     **这一步是语法规则，不是语义判断**：守卫读不出「这个重叠是善意的还是
     事故」，它只认「选择器结构」这一条语法性质——`.af-drawer .af-msg`
     是后代选择器，`.af-msg { … }` 是单类定义。所以「有意覆盖」不能靠
     语言解释，只能写成**可枚举的数据**（下表）：新增一处重叠就必须往表里
     加一行并写清理由，漏加即红。

     **第4轮换成结构扫描**：初版按行跑 `^\.cls\s*\{`，只看行首单类 + 同行开括号，
     五种真实形态全部漏检，且每条都变异验证过（加回规则后本文件全绿、一条不红）：
     ① `@media` 里缩进两格的规则；② 逗号组 `.a, .b { … }`；③ 逗号组里**后一个**
     类 `.x, .af-col { … }`；④ `.af-col` 与 `{` 不在同一行；⑤ 伪类 `.af-col:hover`。
     一次**真实的**漏网就是 ③——`.af-kv__k, .af-kv dt {` 让 `.af-kv__k` 的跨文件
     重名（`components.css` 的死副本）整轮没被看见。现在改用 `parseCssRules`
     （剥注释 → 花括号配对 → 逗号切分 → 取类名），五种形态一律覆盖。
     **没有媒体条件豁免**：初版曾给「两边都在 `@media` 里」开一道口子，变异实测
     漏网（同一断点下双方各写一份 `.af-probe` 仍判绿）。命中一律按上面的白名单判。

     **守卫没有能力判死活**：它只数「类名在两处出现」。一条规则是否真的生效、
     某个声明是否是死代码，要逐条声明在浏览器里量（`.af-board` 的
     `overflow-x: visible` 就是这样被误删过一次）——那是实测的事，不是本守卫的结论。 */

  /* 宽松口径允许的重叠。**每一项都是「有意的局部覆盖」**，不是全量重定义：
     prototype-shell.css 那边一律带前缀（`.af-drawer` / `html[data-rail='off']`
     / `.af-region--warn`），只在特定作用域里改几个声明。 */
  const LOOSE_OVERLAP_ALLOWLIST: Array<{ cls: string; why: string }> = [
    {
      cls: '.af-btn',
      why: 'shell 只有 `html[data-rail=\'off\'] .af-rail__actions .af-btn`：收起左栏时收窄动作区按钮，桌面定义仍在 components.css。',
    },
    {
      cls: '.af-msg',
      why: 'shell 只有 `.af-drawer .af-msg`：抽屉里消息行用 2px gap 的紧凑排版，会话页的定义不动。',
    },
    {
      cls: '.af-msg__body',
      why: 'shell 只有 `.af-drawer .af-msg__body`：抽屉字号降到 sm，会话页正文档不变。',
    },
    {
      cls: '.af-msg__who',
      why: 'shell 只有 `.af-drawer .af-msg__who`：抽屉署名行字号降到 2xs。',
    },
    {
      cls: '.af-region__head',
      why: 'shell 只有 `.af-region--warn .af-region__head`：warning 变体换底色，基底来自 components.css。',
    },
  ]

  it('两份样式表不许有同名类的全量定义（严格口径为空）', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const a = classNamesIn(css, { wholeSubject: true })
    const b = classNamesIn(shell, { wholeSubject: true })
    const both = [...a].filter((s) => b.has(s)).sort()
    expect(both, `同时定义在两个文件里（后导入的 prototype-shell.css 会全覆盖）：${both.join(', ')}`)
      .toEqual([])
  })

  /* **宽松口径的命中必须全部在白名单内**：白名单外的命中说明新增了一处
     「类名同时出现在两份样式表」，要么合并到一处，要么在这里写清为什么
     它是善意的局部覆盖。 */
  it('宽松口径的跨文件重叠全部落在白名单内', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const a = classNamesIn(css, { wholeSubject: false })
    const b = classNamesIn(shell, { wholeSubject: false })
    const both = [...a].filter((s) => b.has(s)).sort()
    const allowed = new Set(LOOSE_OVERLAP_ALLOWLIST.map((e) => e.cls))
    const unexpected = both.filter((s) => !allowed.has(s))
    expect(
      unexpected,
      `新增的跨文件重叠，未登记为有意的局部覆盖：${unexpected.join(', ')}`,
    ).toEqual([])
    /* 反向：白名单里的每一项都**仍然**是真实重叠。类被删/改名后留着一条
       幽灵白名单，下一个人就会以为这里还有一处要照顾。 */
    const stale = [...allowed].filter((s) => !both.includes(s)).sort()
    expect(stale, `白名单条目已不再是真实重叠（类被删或改名）：${stale.join(', ')}`).toEqual([])
    /* 白名单每条都必须带理由——空理由等于没有解释。 */
    const noReason = LOOSE_OVERLAP_ALLOWLIST.filter((e) => e.why.trim().length < 10).map((e) => e.cls)
    expect(noReason, `白名单条目缺理由：${noReason.join(', ')}`).toEqual([])
  })

  /* 白名单必须逐条可解释：断言每条理由提到的前缀确实出现在 shell 里那条规则上。
     只测「命中数 == 5」是规模断言，不证明重叠是善意的。 */
  it('白名单里的每条理由都对应 shell 里一条带作用域的规则', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const rules = parseCssRules(shell)
    const SCOPED = /^(?:html\[[^\]]*\]|\.[a-z][a-z0-9_-]*(?:--[a-z0-9_-]+)?)\s+/
    const unscoped: string[] = []
    for (const entry of LOOSE_OVERLAP_ALLOWLIST) {
      const hits = rules.filter((r) =>
        r.selector.split(',').some((part) => new RegExp(`\\${entry.cls}(?![a-zA-Z0-9_-])`).test(part.trim())),
      )
      if (hits.length === 0) unscoped.push(`${entry.cls}: 在 shell 里找不到规则`)
      for (const r of hits) {
        for (const part of r.selector.split(',')) {
          if (!new RegExp(`\\${entry.cls}(?![a-zA-Z0-9_-])`).test(part)) continue
          if (!SCOPED.test(part.trim())) unscoped.push(`${entry.cls}: "${part.trim()}" 不是带作用域的覆盖`)
        }
      }
    }
    expect(unscoped, `白名单声称「有意的局部覆盖」，但 shell 里是裸定义：${unscoped.join('; ')}`)
      .toEqual([])
  })
})

/* ===========================================================================
   表单控件的行盒（v0.8.1 第3轮，用户报「所有选择器 展示的选中项 文字都下沉
   超过下面的框了」）
   ---------------------------------------------------------------------------
   根因是**算术**：`height: var(--af-row-compact)`（26px）减去上下 padding
   （`--af-space-2` = 4px × 2）再减 1px 边框 × 2，内容盒只剩 **16px**；
   而 13.5px 字在 `line-height: normal` 下实测文字盒 **18px**。字比盒子高，
   那一行就被顶出下边框。`<select>` 最明显，因为选中项由浏览器原生控件绘制，
   不走 CSS 行盒居中。

   这条门禁不量浏览器（happy-dom 不做布局），只守**算术不变式**：
   内容盒高度必须不小于该字号的文字盒高度。字号到文字盒高度的换算按
   `line-height: normal` 的实测比例取 —— 13.5px → 18px（约 1.333 倍）。
   =========================================================================== */
describe('表单控件行盒（第3轮）', () => {
  const css = readRepoFile('packages/web/src/styles/components.css')
  const tokens = readRepoFile('packages/web/src/styles/tokens.css')

  /** `--name` 的数值：先找 px 字面值，再跟一层 `var()` 转发。 */
  function pxOf(name: string, depth = 0): number {
    const direct = new RegExp(`--${name}:\\s*(\\d+(?:\\.\\d+)?)px`).exec(tokens)
    if (direct) return Number(direct[1])
    const alias = new RegExp(`--${name}:\\s*var\\(--([a-z0-9-]+)\\)`).exec(tokens)
    if (alias && depth < 5) return pxOf(alias[1], depth + 1)
    throw new Error(`token --${name} 解析不出 px（可能是 calc()，需要人来看）`)
  }

  it('输入/选择控件的内容盒装得下文字（不再顶出下边框）', () => {
    const block = cssBlockOf(css, '.af-input, .af-select, .af-textarea')
    expect(block, '取不到 .af-input, .af-select, .af-textarea 规则体').not.toBe('')
    /* **取全部命中该组的规则，不只第一条**：初版只 `ruleFor`（首个精确匹配），
       于是「事后又加一条 `.af-input, .af-select, .af-textarea { … }` 把这组
       改坏」的写法（`max-height: 16px` / `border: 2px`）完全看不见——
       变异实测两条都判绿。CSS 里后者胜，所以对每个**被声明的属性**取
       「所有命中规则里的最后一条」才是真实生效值。 */
    const group = parseCssRules(css).filter((r) => r.selector.replace(/\s+/g, ' ').trim() === '.af-input, .af-select, .af-textarea')
    expect(group.length, '找不到 .af-input, .af-select, .af-textarea 那条规则').toBeGreaterThan(0)
    /** 属性在整组里的生效值：最后一条声明了它的规则胜。 */
    const effective = (property: string): string | undefined => {
      let val: string | undefined
      for (const r of group) val = propValue(r, property) ?? val
      return val
    }

    /* **取值一律从规则体里解析，不用字面量正则**（v0.8.1 第4轮）：初版断言的是
       「规则文本里出现 `padding: 0 var(--af-space-3)` 这个字面串」，而算术用的
       `border = 1` 是硬编码。于是三种「用户报的 bug 回来了」的写法全部判绿：
       ① 加 `max-height: 16px`（内容盒剩 16px，装不下 18px 文字）；
       ② `.af-select { line-height: normal }`（唯一有效的修法被撤销）；
       ③ 边框改成 2px（内容盒 26−4=22，公式仍按 1px 算）。
       现在 height / padding-top / padding-bottom / border-width 四个真实取值
       都从声明体里读，再算内容盒。 */
    const px = (value: string | undefined, what: string): number => {
      if (value === undefined) throw new Error(`${what} 未声明——规则必须有它`)
      const got = /^(-?\d+(?:\.\d+)?)(px)?$/.exec(value.trim())
      if (got) return Number(got[1]) // 单位可省，仅对 0 合法（`padding: 0`）
      const tok = /^var\(--([a-z0-9-]+)\)$/.exec(value.trim())
      if (tok) return pxOf(tok[1])
      throw new Error(`${what} 的取值 ${value} 既不是 px 也不是单层 var(token)，需要人来看`)
    }

    const height = px(effective('height'), 'height')
    const padParts = (effective('padding') ?? '').trim().split(/\s+/)
    // `padding: <top> <right> <bottom> <left>`；1～3 个值时按 CSS 简写规则还原。
    const padTop = px(padParts[0] ?? '', 'padding-top')
    const padBottom = px(padParts[2] ?? padParts[0] ?? '', 'padding-bottom')
    const borderParts = (effective('border') ?? '').trim().split(/\s+/)
    const borderWidth = px(borderParts[0], 'border-width')
    /* 边框宽被调大不会让「文字装不下」，但会让内容盒矮 2px——它和 padding
       是同一个坑的两种写法（初版把 `border = 1` 硬编码，所以 `border: 2px`
       判绿）。这里直接钉住「与设计系统的 1px 发丝线一致」，而不是让它
       悄悄吃掉内容盒。 */
    expect(borderWidth, '边框必须是 1px 发丝线（2px 会再吃掉 2px 内容盒）').toBe(1)
    expect(padTop, '垂直 padding 必须为 0，否则内容盒被吃掉、文字顶出边框').toBe(0)
    expect(padBottom, '垂直 padding 必须为 0，否则内容盒被吃掉、文字顶出边框').toBe(0)
    /* 分写的 `padding-top` / `padding-bottom` 同样吃掉内容盒——简写被覆盖时
       真实生效的是它们，必须一并读（`effective('padding')` 只看简写会漏）。 */
    const padTopLong = effective('padding-top')
    const padBottomLong = effective('padding-bottom')
    if (padTopLong !== undefined) expect(px(padTopLong, 'padding-top'), 'padding-top 必须为 0').toBe(0)
    if (padBottomLong !== undefined)
      expect(px(padBottomLong, 'padding-bottom'), 'padding-bottom 必须为 0').toBe(0)

    /* `max-height` 不得把内容盒压回「装不下文字」——这正是第一类漏网形态。 */
    const maxHeight = effective('max-height')
    const cap = maxHeight === undefined ? Infinity : px(maxHeight, 'max-height')
    expect(cap, `max-height ${cap}px 把行盒压得比 height 还矮`).toBeGreaterThanOrEqual(height)

    // 显式 line-height：calc(row - 2px)，不能是 normal。
    expect(effective('line-height'), 'line-height 必须显式取值（normal 会让文字盒超出内容盒）')
      .toBe('calc(var(--af-row-compact) - 2px)')

    const fontSize = pxOf('af-text-sm')
    const innerH = Math.min(height, cap) - borderWidth * 2 - padTop - padBottom
    /* 文字盒高度按 `line-height: normal` 的实测比 **1.333** 取（13.5px → 18px，
       Chromium 实测，见规则上方注释）。这个比值的出处是 playwright 量的
       墨迹盒，不用「× 1.5 更保守」——那样会把 `border: 2px`（内容盒 22px，
       文字 18px）判成合规，而它其实是把内容盒吃掉了 2px、离 bug 只差一点。
       换字体栈后这个比值可能变；真要改用 token 驱动的 `--af-leading-snug`
       时，必须同时在活 Hub 上复量一次，别只改这里。 */
    const textH = Math.ceil(fontSize * 1.333)
    expect(innerH, `内容盒 ${innerH}px 装不下 ${fontSize}px 文字（按 normal 实测约 ${textH}px）`)
      .toBeGreaterThanOrEqual(textH)
  })

  /* **`.af-select` 不得在别的规则里覆写共用的 `line-height`**：`<select>` 的选中项
     由浏览器原生控件绘制，`line-height` 在 Chrome 里对它**完全无效**（实测 computed
     恒为 `normal`），真正的修法是「垂直 padding 归零 + `height` 撑满」。所以任何
     想靠单写 `.af-select { line-height: … }` 去修的改动都是误解，必须判红——初版
     没有这条反向断言，把 `line-height` 改回 `normal` 仍全绿。
     **排除共用那条组规则**（`.af-input, .af-select, .af-textarea`）：那里写
     `line-height` 是为了 input / textarea 的兄弟控件，select 只是同组列出。 */
  it('.af-select 不得单独覆写 line-height', () => {
    const SHARED = '.af-input, .af-select, .af-textarea'
    const own = parseCssRules(css).filter(
      (r) =>
        r.selector.split(',').some((s) => s.trim() === '.af-select') &&
        r.selector.replace(/\s+/g, ' ').trim() !== SHARED,
    )
    const offenders = own
      .filter((r) => propValue(r, 'line-height') !== undefined)
      .map((r) => `${r.selector} → line-height: ${propValue(r, 'line-height')}`)
    expect(offenders, `select 的原生渲染不吃 line-height，别在这里设：${offenders.join('; ')}`)
      .toEqual([])
  })

  /* **`.af-head` 必须是 `min-height`，不是 `height`**（v0.8.1 第4轮，用户报
     「af-head__title 超到页面顶部外」）。定高 56px + `align-items: center` 时，
     标题 + 副标题的包裹列一旦超过 56px，超出量被**上下均分**，上半截（实测
     5.8px）落到视口外——`/settings` 与 `/agents` 就是这样，标题 `top = -6.3px`。
     `min-height` 让页头自己长高，标题永远在视口内。
     **同时守住「别退回定高」**：有人看到「页头要和左栏对齐」就可能再改回
     `height`，那会把这个 bug 原样带回来。 */
  it('.af-head 用 min-height（定高时标题会被顶出视口）', () => {
    const head = ruleFor(parseCssRules(readRepoFile('packages/web/src/styles/prototype-shell.css')), '.af-head')
    expect(head, '找不到 .af-head 规则').toBeDefined()
    expect(propValue(head!, 'min-height'), '.af-head 缺 min-height').toBe('var(--af-head-h)')
    expect(propValue(head!, 'height'), '.af-head 不得用定高 height——那会让标题被顶出视口').toBeUndefined()
  })

  it('textarea 不吃单行 line-height（多行控件要换行）', () => {
    /* 用 `ruleFor` 按选择器文本定位，而不是按行 `indexOf`：
       共用组 `.af-input, .af-select, .af-textarea {` 也含有 `.af-textarea`
       这串字，按子串找会命中错的规则体。`ruleFor` 要求选择器**完全相等**。 */
    const rule = ruleFor(parseCssRules(css), '.af-textarea')
    expect(rule, '.af-textarea 没有独立规则').toBeDefined()
    const body = stripCssComments(rule!.body)
    expect(body).toContain('--af-leading-snug')
    expect(body).not.toContain('calc(var(--af-row-compact)')
  })
})

describe('紧凑空态（子任务 12）', () => {  const sfc = readRepoFile('packages/web/src/domains/shared/components/EmptyState.vue')
  const css = readRepoFile('packages/web/src/styles/components.css')

  it('compact prop 默认 false（既有调用点行为不变）', () => {
    expect(sfc).toContain('compact?: boolean')
    expect(sfc).toMatch(/compact:\s*false/)
  })

  it('既有三个 prop 与默认 slot 一个都没丢', () => {
    for (const p of ['glyph?: string', 'title: string', 'text?: string']) {
      expect(sfc, `prop 丢失：${p}`).toContain(p)
    }
    expect(sfc).toContain('<slot />')
  })

  it('compact 渲染横排而非居中', () => {
    expect(sfc).toContain("'af-empty--compact': compact")
    /* 必须取**变体自己那条规则**的声明体。上一版写成
       `cssBlockOf(css, '.af-empty--compact')`，而它匹配的是
       `.af-empty--compact { ... }` 这个前缀的**第一条**——恰好是变体本体，
       看起来对；但把变体内部的选择器改坏（变异 M5）时，`.af-empty--compact {`
       这条前缀仍然命中，断言假绿。所以这里改成「变体块里必须出现
       flex-direction: row」，且用 `{` 锚定到变体本体，不吃到子选择器。 */
    const block = cssBlockOf(css, '.af-empty--compact')
    expect(block, '.af-empty--compact 规则体取不到').not.toBe('')
    expect(block).toContain('flex-direction: row')
    // 反向守卫：默认形态必须是列方向，两者不能都变成 row。
    expect(cssBlockOf(css, '.af-empty')).toContain('flex-direction: column')
  })

  it('.af-empty 本体不再被 prototype-shell.css 二次定义（曾经的覆盖 bug）', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    /* 本条守的是一个真实发生过的 bug：prototype-shell.css 后导入，
       它里面那份 `.af-empty { padding: 24px 16px }` 把 components.css 的
       定义整条盖掉，空态实测 242px 高。只许有一处定义。 */
    expect(shell).not.toMatch(/^\.af-empty\s*\{/m)
  })
})

/** 取一条规则的声明体（到第一个 `}` 为止）。找不到返回空串——由调用方的断言判红。 */
function cssBlockOf(css: string, selector: string): string {
  const i = css.indexOf(`${selector} {`)
  if (i < 0) return ''
  const j = css.indexOf('}', i)
  return css.slice(i, j + 1)
}
