import { describe, expect, it } from 'vitest'
import { readRepoFile } from '@/test/css'

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
     `prototype-shell.css:612-613` 的 `.af-xs` / `.af-2xs` 早就在做同一件事
     （`--p-t-2xs` 就是 `var(--af-text-2xs)`），键位提示句另有 `.af-keyhint`。
     于是同一档字号有两个名字，改一处另一处不跟。**判据**：每条声明
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
      const stripped = css.replace(/\/\*[\s\S]*?\*\//g, '')
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
     会把「只加规则不加消费者」判红）。 */
  it('.af-panel__foot 要么不定义，要么必须在模板里被用到', () => {
    if (!css.includes('.af-panel__foot {')) return
    const consumers = readRepoFile('packages/web/src/domains/device/DevicesPage.vue')
    expect(consumers, '.af-panel__foot 定义了但没有任何消费者').toContain('af-panel__foot')
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
  it('components.css 里没有重复定义的顶层选择器', () => {
    const seen = new Map()
    const dups = []
    for (const line of css.split('\n')) {
      const m = /^(\.[a-z][a-z0-9_-]*(?:[^{]*?))\s*\{\s*$/.exec(line.trimEnd())
      if (!m) continue
      const sel = m[1].trim()
      if (seen.has(sel)) dups.push(sel)
      else seen.set(sel, true)
    }
    expect(dups, `重复定义（后者会静默覆盖前者）：${dups.join(', ')}`).toEqual([])
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
     这里改成**全量求交集**：两份样式表的顶层类选择器不许有任何交集。
     真要两边都有，就必须先合并到一处，而不是靠导入顺序决定谁赢。 */
  it('两份样式表的顶层类选择器没有任何交集（后导入者不得静默取胜）', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    /* **必须同时认单行与多行规则**：第一版只匹配 `^\\.cls\\s*{$`（多行形态），
       于是 `.af-row { display: flex; ... }` 这种单行规则被漏掉——变异实测
       （把 `.af-row` 加回 `components.css`）当场漏网，本条仍判绿。 */
    const topLevel = (src: string): Set<string> => {
      const out = new Set<string>()
      const stripped = src.replace(/\/\*[\s\S]*?\*\//g, '')
      for (const line of stripped.split('\n')) {
        const m = /^(\.[a-z][a-z0-9_-]*)\s*\{/.exec(line)
        if (m) out.add(m[1])
      }
      return out
    }
    const a = topLevel(css)
    const b = topLevel(shell)
    const both = [...a].filter((s) => b.has(s)).sort()
    expect(both, `同时定义在两个文件里（后导入的 prototype-shell.css 会全覆盖）：${both.join(', ')}`)
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
    // 这一组共用一条规则，所以取的是组里第一条选择器的声明体。
    const rowCompact = pxOf('af-row-compact')
    const fontSize = pxOf('af-text-sm')
    const border = 1 // `border: 1px solid`
    /* 垂直 padding 必须为 0：只要它非零，内容盒就被吃掉一块，而这正是 bug 成因。
       （零值 + 显式 line-height 是修法；见规则上方的注释。） */
    const pad = /padding:\s*0\s+var\(--af-space-3\)/.test(block)
    expect(pad, '垂直 padding 必须为 0，否则内容盒被吃掉、文字顶出边框').toBe(true)
    // 显式 line-height：calc(row - 2px)，不能是 normal。
    expect(block, 'line-height 必须显式取值（normal 会让文字盒超出内容盒）')
      .toContain('line-height: calc(var(--af-row-compact) - 2px)')
    // 算术：内容盒 = height - 上下边框；文字盒按 normal 实测比 1.333 估。
    const innerH = rowCompact - border * 2
    const textH = Math.ceil(fontSize * 1.333)
    expect(innerH, `内容盒 ${innerH}px 装不下 ${fontSize}px 文字（约 ${textH}px）`)
      .toBeGreaterThanOrEqual(textH)
  })

  it('textarea 不吃单行 line-height（多行控件要换行）', () => {
    /* `cssBlockOf` 用 `indexOf`，`.af-textarea {` 会先命中共用那条组的声明体
       （`.af-input, .af-select, .af-textarea {`），所以这里按**行首**自己定位。 */
    const lines = css.split('\n')
    const at = lines.findIndex((l) => l.trimEnd() === '.af-textarea {')
    expect(at, '.af-textarea 没有独立规则').toBeGreaterThanOrEqual(0)
    const block = lines.slice(at, at + 6).join('\n')
    expect(block).toContain('--af-leading-snug')
    expect(block).not.toContain('calc(var(--af-row-compact)')
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
