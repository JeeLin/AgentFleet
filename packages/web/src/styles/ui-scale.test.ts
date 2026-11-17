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
  const css = readRepoFile('packages/web/src/styles/components.css')

  it('头/体/脚三个类都有定义', () => {
    for (const cls of ['.af-panel', '.af-panel__head', '.af-panel__body', '.af-panel__foot']) {
      expect(css, `${cls} 缺定义`).toContain(`${cls} {`)
    }
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
     全量覆盖，我那 20 行从头到尾没生效——与 F-2 修掉的 bug 同型。
     判据：任何在 components.css 与 prototype-shell.css **两边**都定义的
     顶层选择器，都必须有明确说明；这里只钉住我们新加的这一族不许重名。 */
  it('新加的面板类不得在 prototype-shell.css 里重名（否则静默失效）', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    for (const cls of ['.af-panel', '.af-panel__foot', '.af-panel__title', '.af-panel__head']) {
      expect(shell, `${cls} 在 prototype-shell.css 里也定义了，会被它覆盖`).not.toMatch(
        new RegExp(`^\\${cls}\\s*\\{`, 'm'),
      )
    }
  })
})

describe('紧凑空态（子任务 12）', () => {
  const sfc = readRepoFile('packages/web/src/domains/shared/components/EmptyState.vue')
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
