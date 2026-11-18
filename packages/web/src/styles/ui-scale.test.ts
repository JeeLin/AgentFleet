import { describe, expect, it } from 'vitest'
import { readRepoFile } from '@/test/css'
import {
  classNamesIn,
  classNamesInRule,
  parseCssRules,
  propValue,
  ruleFor,
  stripCssComments,
  type CssRule,
} from '@/test/css-scan'

import { readdirSync, readFileSync } from 'node:fs'
import { resolve, sep } from 'node:path'

/* ===========================================================================
   子任务 12 的门禁：层级阶梯 + 卡片分区
   ---------------------------------------------------------------------------
   **这个文件守的是「视觉属性」，而 happy-dom 不做布局**——它量不出行高、间距、
   字号的实际渲染值。所以这里退一步：断言**令牌定义与类存在性**，几何由
   子任务 14 在活 Hub 上用 playwright 实测（见本文的 28 组合表）。

   为什么仍然值得写：本里程碑已有实证——「行高 1.7 改成 1.65」这种变异，
   整套用例全绿（当时的具体条数已不必记，写了就会过期）。没有这层断言，
   改坏 `--af-text-display` 也没人发现。

   **判据分两层，不是一个正则通吃**（v0.8.1 第5轮 G77/G78/G85 与 G87 之后
   的实际形态，原措辞「断言规则文本而非结构」已不准）：
     · **结构层**：规则用 `parseCssRules` 解析（展开 `@media`、带 `atRules`
       链、按花括号配对），字号声明按规则逐条收集——不是按行跑正则，
       所以缩进在 `@media` 里的、多行声明的、同一属性写两遍的都逃不掉；
     · **取值层**：token 的 px 值仍是从声明文本里取（`var()` 转发也跟到底），
       不用 `getComputedStyle`——这是有意的妥协，理由写在 `tokens.css` 的
       取值注释里。
   也就是说：**它守的是"哪条规则声明了什么"，不是"浏览器最终算出多少"**。
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
 *
 * **根目录用 `process.cwd()` 而不是 `import.meta.url`**（v0.8.1 第5轮）：
 * 本套用例在 `packages/web` 下跑，`cwd` 就是包根；而 `fileURLToPath(new URL(…,
 * import.meta.url))` 在 vitest 的模块图里拿到的不是 `file:` URL，
 * 实测抛 `TypeError: The URL must be of scheme file`——守卫自己先炸，
 * 比不写还糟。
 */
function vueFilesUsing(needle: string): string[] {
  const webRoot = resolve(process.cwd())
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

/**
 * `components.css` 里「整个主体就是这一个类」的规则所声明的属性集合。
 *
 * 上一条守卫需要它来回答「`.af-taskcard` 现在还剩哪几条独有声明」——
 * 刻意只认**单类整个主体**（`classNamesInRule` 那一级）：`.af-taskcard__title`
 * 或 `.af-drawer .af-taskcard` 不是「给卡片本体定外观」，不该混进来。
 */
function RULES_OWN(cls: string): Set<string> {
  const out = new Set<string>()
  for (const rule of parseCssRules(readRepoFile('packages/web/src/styles/components.css'))) {
    if (!classNamesInRule(rule).includes(cls)) continue
    for (const m of stripCssComments(rule.body).matchAll(/(?:^|;)\s*([a-z-]+)\s*:/g)) out.add(m[1])
  }
  return out
}

/* 取一条规则体里**每一条** `font-size` 声明（同名多次全收）。
   `propValue` 只取末条——那对「最终生效值」是对的，但这里要问的是
   「有没有任何一条写小了」，取末条恰好会漏掉前面那条（G78）。

   **放在模块级而不是某个 describe 里**（v0.8.1 第5轮 G87）：两个 describe
   都要用它——「标题字号下限」与「字号档别名」。当初写在前者内部，后者只能
   自己另写一份「体内只有一条 font-size」的正则，于是多属性共存的工具类被
   整条跳过（4c Y3 的逃逸口）。共用一份实现，两个判据就不会分叉。 */
function fontSizeDeclsIn(rule: CssRule): string[] {
  const out: string[] = []
  for (const part of rule.body.split(';')) {
    const at = part.indexOf(':')
    if (at < 0) continue
    if (part.slice(0, at).trim() !== 'font-size') continue
    out.push(part.slice(at + 1).trim())
  }
  return out
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
  it('任何 *__title / *__heading 的字号都不小于正文', () => {
    /* ── 这条守的是一类反复出现的缺陷 ────────────────────────────────────
       标题类（`*__title` / `*__heading`）用了比正文更小的字号。实证两处——
       `.af-region__title` 原为 13.5px、`.af-panel__title` 原为 14.5px（与正文
       同档），都压不住内容。标题可以同大但要靠字重/明度区分，**绝不能更小**。

       ── 为什么从「按行跑正则」改成「结构化扫描」（v0.8.1 第5轮 G77/G78/G85）──
       初版判据是 `/^(\.[a-z][a-z0-9_-]*__title[a-z0-9_-]*)\s*\{([^}]*)\}/gm`，
       4c 的变异矩阵证明它有三重死角，**连续两轮（round3 F2/P29、round4 F2）
       原样存活**：

         · **G77 行首锚**：`^` + `m` 要求选择器在行首，于是缩进在 `@media` 块
           里的规则一律扫不到。实测把 `@media` 内的 `.af-panel__title` 降到
           2xs 全绿。
         · **G78 同名第二条**：`[^}]*` 只能吃单行体，且 `matchAll` 对同一选择器
           的**第二次出现**各算一条、各自通过。于是在 `.af-panel__title` 里先写
           合法值再写 `font-size: var(--af-text-2xs)`（后者胜，真实 computed
           是 11px）——**全绿**。
         · **G85 别名 token**：`--p-t-md` 这类主题别名不在 `tokens.css` 的
           `--name: Npx` 里，`pxMatch` 取不到就 `continue`（静默跳过）。实测把
           `.af-drawer__title` 从 `--p-t-md` 降到 `--p-t-2xs` 全绿。

       现在改为：`parseCssRules` 展开 `@media`（带 `atRules` 链）→ 逐个选择器
       收集**每一条** `font-size` 声明 → 用能跟 `var()` 转发的 `pxOfToken` 还原
       实际 px → 要求**全部**命中值都不小于正文。这样「事后追加一条更小的」
       和「写进 @media」都被同一条判据覆盖。 */
    const body = tokenPx(tokens, 'af-text-md')
    const cssFiles = [
      readRepoFile('packages/web/src/styles/components.css'),
      readRepoFile('packages/web/src/styles/prototype-shell.css'),
    ]

    /* 字号令牌 → px。**必须跟 `var()` 转发**：主题文件里 `--p-t-md: var(--af-text-md)`
       这类别名很常见，只看 `tokens.css` 会把它们全当成「非字号 token」跳过——
       而那正是 G85 里 `.af-drawer__title` 逃逸的机制。 */
    /* **文件名动态枚举，不手写清单**：初版手写了 `theme-dark.css`，而实际叫
       `theme-warm.css`——`readRepoFile` 直接 ENOENT，守卫当场炸。手写清单还会在
       新增主题时静默漏掉一份（少一份 = 某些别名 token 解析不出来 = 静默跳过）。 */
    const THEME_TOKENS = readdirSync(resolve(process.cwd(), 'src/styles'))
      .filter((f) => f.endsWith('.css'))
      .map((f) => readFileSync(resolve(process.cwd(), 'src/styles', f), 'utf-8'))
      .join('\n')
    function pxOfToken(name: string, depth = 0): number | null {
      const direct = new RegExp(`--${name}:\\s*(\\d+(?:\\.\\d+)?)px`).exec(THEME_TOKENS)
      if (direct) return Number(direct[1])
      if (depth >= 5) return null
      const alias = new RegExp(`--${name}:\\s*var\\(--([a-z0-9-]+)\\)`).exec(THEME_TOKENS)
      return alias ? pxOfToken(alias[1], depth + 1) : null
    }

    const offenders: string[] = []
    /** 被扫到（并成功解析出 px）的类名——供下面的枚举校验用。 */
    const seen = new Set<string>()
    let checked = 0
    for (const css of cssFiles) {
      for (const rule of parseCssRules(css)) {
        /* 选择器可能是一组（`.a, .b`）：逐个取以 `__title` / `__heading` 结尾
           的那个类。`classNamesInRule` 已按逗号切好，这里再用一次同样的口径。 */
        /* 两个 helper 都返回**带点**的 `.af-x`；统一在这里去掉前导点，后续
           `seen` / 豁免表 / 报错文案全部用不带点的形式。 */
        for (const raw of classNamesInRule(rule)) {
          const cls = raw.replace(/^\./, '')
          if (!/__title$|__heading$|__title--|__heading--/.test(cls)) continue
          /* **同名多次声明全部收集**（G78）：`declarationsOf` 返回该属性在
             匹配规则里的每一条，不是只取末条——我们要的是「有没有任何一条
             写小了」，不是「最终生效值是多少」（后者会让前面那条更小的被发现
             吗？不会，取末条恰好漏掉它）。 */
          for (const decl of fontSizeDeclsIn(rule)) {
            const m = /^var\(--([a-z0-9-]+)\)$/.exec(decl.trim())
            if (!m) continue // 写死 px 的另由 design-system 的取值纪律管
            const px = pxOfToken(m[1])
            if (px === null) continue
            checked += 1
            seen.add(cls)
            const where = rule.atRules.length ? ` @${rule.atRules.join(' ')}` : ''
            if (px < body) offenders.push(`.${cls}${where} = ${px}px < ${body}px`)
          }
        }
      }
    }
    /* **这条守卫自己不能空转**（v0.8.1 第5轮 4c 的教训）：`offenders` 恒空无法
       区分「真没问题」与「一条都没扫到」。初版我拍了个 `checked > 10` 的阈值，
       实测只扫到 8 条就判红——那个数字是我凭空想的，不是事实。
       现在改成**枚举校验**：样式表里每一个 `*__title` / `*__heading` 类都必须
       有字号声明被扫到，缺一个就点名。这样新增一个标题类而守卫漏掉它会立刻暴露，
       也不依赖任何拍出来的阈值。 */
    const declared = new Set<string>()
    for (const css of cssFiles) {
      /* `classNamesIn` 返回带点的 `.af-x`，而上面循环里 `classNamesInRule` 不带点——
         两处口径必须一致，否则枚举校验永远说「没覆盖到」。统一取不带点的形式。 */
      for (const cls of classNamesIn(css)) if (/__title$|__heading$/.test(cls)) declared.add(cls.slice(1))
    }
    /* 显式豁免：这些类只有非字号声明（`margin: 0` 之类），没有「写成更小」的余地。
       **豁免要写理由**，否则下一个人不知道该不该往下加。 */
    const NO_FONT_SIZE_BY_DESIGN = new Set([
      'af-switch__title', // 只有 `margin: 0`，字号继承自 `.af-switch`
    ])
    const missed = [...declared].filter((c) => !NO_FONT_SIZE_BY_DESIGN.has(c) && !seen.has(c))
    expect(
      missed,
      `这些 *__title/__heading 类没有被字号守卫覆盖到（判据失效或新类漏登记）：${missed.join(', ')}`,
    ).toEqual([])
    expect(checked, '一条字号都没扫到——守卫彻底失效').toBeGreaterThan(0)
    expect(offenders, offenders.join('; ')).toEqual([])
  })
})

describe('面板三分区（子任务 12）', () => {
  const tokens = readRepoFile('packages/web/src/styles/tokens.css')

  /* **这条守的是一个真实回潮（v0.8.1 第3轮）**：为了把两处内联 `font-size`
     收进样式表，`components.css` 补了 `.af-t-2xs` / `.af-t-sm` 两条——而
     `prototype-shell.css` 的 `.af-xs` / `.af-2xs` 早就在做同一件事
     （`--p-t-2xs` 就是 `var(--af-text-2xs)`），键位提示句另有
     `.af-keyhint`。于是同一档字号有两个名字，改一处另一处不跟。**判据**：每条声明
     `font-size: var(<字号令牌>)` 的规则，其取值令牌不许被第二条规则重复消费
     （同一 token 只许一个工具类）。豁免：组件本体类（`*__title` / `*card*`
     这类）不在此列——它们是组件的一部分，不是通用工具类。 */
  /* **判据：显式列出「档位工具类」这个名字空间，不许同一 token 挂两个名字。**
     -------------------------------------------------------------------------
     这条守的是一个真实回潮（v0.8.1 第3轮）：为把两处内联 `font-size` 收进样式表，
     `components.css` 补了 `.af-t-2xs` / `.af-t-sm`，而 `prototype-shell.css` 的
     `.af-xs` / `.af-2xs` 早在做同一件事（`--p-t-2xs` 就是 `var(--af-text-2xs)`）。
     同一档字号两个名字，改一处另一处不跟。

     ── 为什么判据从「结构特征」改成「显式名单」（v0.8.1 第5轮 4c Y3）─────────
     初版判据是「体内**只有**一条 `font-size` 的规则」（正则
     `^font-size:\s*var\(…\);\s*$`）。它有两个毛病，恰好一漏一误：

       · **漏**（Y3 的逃逸口）：`.af-x { font-size: var(--af-text-2xs);
         letter-spacing: .04em; }` —— 顺手带一条别的属性，整条规则被跳过。
         而「给字号档造别名」的真实写法正是这个形态。
       · **误**：把判据放宽成「体内出现过该 token」之后（我试过），
         `.af-btn` / `.af-input` / `.af-taskcard__*` 等 50 多个**组件本体类**
         全部撞进来——它们当然会写 `font-size: var(--af-text-sm)`，
         那不是「造别名」，那就是它自己的字号。初版之所以没炸，
         是因为「只有一条声明」这个结构特征**碰巧**当了组件类的过滤器。

     所以正确的判据不是「谁声明了这个 token」（组件与工具类都会），而是
     **「谁是这个 token 的通用档位工具类」**——那是一个刻意维护的小名单：
     类名本身就在描述档位（`.af-xs` / `.af-2xs` / `.af-t-label` / `.af-keyhint`）。
     名单外的类即使字号相同也不参与（它们是组件，不是档位）。

     新加一个档位工具类时，把它加进 `TIER_UTILITIES`：如果它复用了一个已被
     占用的 token，这条会立刻报红；如果它真的需要新档位，那是新 token。 */
  /* **只收「纯档位」工具类**：类名本身就在说字号（`.af-xs` = 12.5px 档、
     `.af-2xs` = 11.5px 档），换个组件用也是这个意义。`components.css:179-183`
     的注释把这个约定写死了：「字号工具类不在这里重复造……只留颜色类」。

     **不收的是「带字体角色」的类**：`.af-t-label`（大写小标签）与
     `.af-keyhint`（键位提示句）都取 `--af-text-sm`，但它们不是同一个角色的两个
     名字——前者要 uppercase + tracking，后者是等宽键帽旁的说明句。
     若把它们也放进名单，这条守卫会要求二者合并，那是错的方向：
     合并后「大写标签」和「键位提示」就被绑死在同一个类上，将来一个要改档
     就得连带另一个。《这是「档位」与「排版角色」的区别，见 AGENTS.md 维度 24：
     类型与判据分叉。》 */
  const TIER_UTILITIES = [
    // `prototype-shell.css:662-663`：原型 app.css:170-185 的排版小工具，纯档位
    '.af-xs',
    '.af-2xs',
  ]

  it('同一档字号令牌只许一个工具类消费（不许给字号档造别名）', () => {
    const cssFiles = [
      readRepoFile('packages/web/src/styles/components.css'),
      readRepoFile('packages/web/src/styles/prototype-shell.css'),
    ]
    /* 先把两个文件里所有规则的「单类选择器 → font-size token」收齐 */
    const tokenOf = new Map<string, string[]>()
    for (const css of cssFiles) {
      for (const rule of parseCssRules(css)) {
        const sel = rule.selector.replace(/\s+/g, ' ').trim()
        if (!/^\.[a-z][a-z0-9_-]*$/.test(sel)) continue
        for (const decl of fontSizeDeclsIn(rule)) {
          const m = /^var\((--(?:af|p)-t(?:ext)?-[a-z0-9]+)\)$/.exec(decl.trim())
          if (!m) continue
          // 把 --p-* 别名还原到真正的字号令牌，否则 --p-t-2xs 与 --af-text-2xs 会被当成两档。
          const raw = m[1]
          const alias = new RegExp(`${raw}:\\s*var\\((--[a-z0-9-]+)\\)`).exec(tokens)
          tokenOf.set(sel, [...(tokenOf.get(sel) ?? []), alias ? alias[1] : raw])
        }
      }
    }
    /* 名单里每个工具类都必须真的存在、且真的只取一个档位——防止名单本身烂掉
       （删了规则忘了删名单，或一个工具类同时取两档 = 它自己内部就在打架）。 */
    const bad: string[] = []
    for (const util of TIER_UTILITIES) {
      const toks = tokenOf.get(util)
      if (!toks) bad.push(`${util}：名单里登记了，但样式表里没有这条单类规则`)
      else if (toks.length > 1) bad.push(`${util}：自己声明了 ${toks.length} 档字号（${toks.join(' / ')}）`)
    }
    expect(bad, `档位工具类名单与样式表不一致：${bad.join('; ')}`).toEqual([])

    const users = new Map<string, string[]>()
    for (const util of TIER_UTILITIES) {
      for (const tok of tokenOf.get(util) ?? []) users.set(tok, [...(users.get(tok) ?? []), util])
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
    /* 门面名与真身名二选一，**但不许是别的颜色**。
       为什么允许 `--af-fg-strong`：G92 裁定「原型在用 → 补消费者」。原型
       `--fg-strong` 在 `app.css` 被用 33 次，而实现这 17 处一直直接写
       `var(--p-fg-strong)` 绕过 `--af-*` 门面层，于是 `--af-fg-strong`
       成了一个定义了、却零消费者的条目。补上消费者之后，门面层才真的
       在服役——**这条断言的意图是「标题要 strong 前景」，两个名字指向同一个
       真身，强度不变**。写成 `toContain` 单个名字会把「实现该走哪一层」
       这个不该由本用例决定的事钉死（`--af-fg-strong` 时会**误红**）。
       （两者的定义位置**都在主题层**，不在 `tokens.css`：`tokens.css` 里
       26 个 `--p-*` 全是尺寸/圆角映射，颜色 `--p-*` 一律由四套主题给字面值，
       `--af-fg-strong: var(--p-fg-strong)` 也写在四套主题里。所以
       `--af-fg-strong` 转发的就是同层同主题的 `--p-fg-strong`，不存在
       「跨层取到别的值」这条隐蔽分叉。）
       **判据强度没有放宽**：正则里 `-fg-strong` 是紧接 `fg` 的字面，
       所以写成 `--p-fg-2` / `--af-fg-2` / `--p-fg` / 写死色值都判红。
       （这句话原写作「反过来也挡不住有人换成 `--p-fg-2`」——实测是错的，
       新正则**挡得住**；注释如实陈述判据能力，才不会被下一个人当成
       「这里有个洞」去补一条更松的断言。）
       **第 7 轮补强：判据锚定到 `color:` 这一条声明，不再看整个规则块。**
       原写法 `expect(block).toMatch(...)` 有真实缺口——在块内**追加**一条
       `outline-color: var(--p-fg-2)`（`color` 仍是正确的 strong），
       28 条用例全绿，而注释却声称「`--p-fg-2` 仍然判红」。根因是
       `cssBlockOf` 把块内注释也算进来、`toMatch` 命中任意一处即过。
       现在用 `declOf()` 剥注释并按属性名取值，追加别的属性不再能干扰判据。 */
    expect(declOf(css, '.af-panel__title', 'color')).toMatch(/--(?:af|p)-fg-strong/)
    expect(block).toContain('--af-weight-semibold')
  })

  /* `declOf` 自己的判据要能验：它是上面那条「标题必须 strong」的承重墙，
     写错了不会有人发现（测试还是绿的）。三条最小事实，用内联 CSS 钉住——
     不依赖真实样式表，改样式不会让它假红。 */
  it('declOf 取「最后一条」属性值，剥注释，忽略别的属性', () => {
    // 注释放在**真声明之后**：不剥注释时「取最后一条」会读到注释里那条，
    // 这条断言立刻红——两个机制（剥注释 / 取最后）各自都被钉住。
    const sample = `
      .x {
        color: var(--af-fg-strong);
        outline-color: var(--p-fg-2);
        /* color: var(--p-fg-2); */
      }
    `
    expect(declOf(sample, '.x', 'color')).toBe('var(--af-fg-strong)')
    // 只出现一次时如实取那一次
    expect(declOf(sample, '.x', 'outline-color')).toBe('var(--p-fg-2)')
    // 属性不存在 → 空串（交给调用方断言判红，不静默通过）
    expect(declOf(sample, '.x', 'background-color')).toBe('')
    // 同块内写两条 → CSS 后者胜，必须取最后一条
    const twice = `.y { color: var(--af-fg-strong); color: var(--p-fg-2); }`
    expect(declOf(twice, '.y', 'color')).toBe('var(--p-fg-2)')
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
     （行首、到 `{` 为止）不许重复出现。
     **第5轮扩到三份样式表**（G24）：原来只守 `components.css`，于是
     `prototype-shell.css`（后导入、赢面更大的那一份）与 `tokens.css`
     （变量真相源）里的「后一条静默全量覆盖前一条」完全没网。 */
  const DECLARED_FILES = [
    'packages/web/src/styles/components.css',
    'packages/web/src/styles/prototype-shell.css',
    'packages/web/src/styles/tokens.css',
  ] as const

  for (const file of DECLARED_FILES) {
    it(`${file.replace('packages/web/src/styles/', '')} 里没有「后者静默全量覆盖前者」的重复选择器`, () => {
      const css = readRepoFile(file)
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
      /* **`@media` 里「同一选择器被写两次」也要看得见**（v0.8.1 第5轮 4c Y1）。
         初版 `if (rule.atRules.length > 0) continue` 把媒体查询整块跳过，
         理由是「`@media` 内重写是合法覆盖」——**覆盖**确实合法，但**同一媒体
         条件里写两遍**不合法：后者静默吞掉前者的同名属性，与顶层那条是同一个
         病，只是藏进了 at-rule 里。

         我最初的修法比这更激进：把「`@media` 里出现顶层没有的属性」一律报红。
         实测立刻炸出 4 处（`.af-git__split` 的小屏竖排、`.af-git__files` 的
         `max-height`/`border-bottom`、`.af-rail__brand .af-navbtn` 的
         `display: none`）——**它们全是正当的响应式覆盖**：小屏换布局本来就
         要引入顶层没有的属性。那条路是错的，已否决。

         现在按 at-rule 链分组：同一个 `atRules` 链下，同一选择器出现两次就是
         真重复——这才是「写漏了一条，后者吞了」的形态。

         ── **残余风险（已知局限，用户裁定接受）** ──────────────────────
         「同一选择器被拆到**两个不同**断点里、各补一部分属性」仍然守不住：
         它和合法的响应式覆盖在**语法上完全同形**（都是「@media 里出现顶层没有
         的属性」），靠静态扫描分不开。4c Y1 描述的那个坏例
         （`@media { .af-badge { min-width: 44px } }`）就是这样逃走的。
         要真分开得把每个页面的实际布局宽度与 4 个断点一起建模——那是个会
         自证的模型（模型算错时守卫照样绿），代价远大于收益。
         补偿措施在下面那条「断点集合必须显式登记」：断点数量与取值是枚举的，
         新增断点要写明理由，至少不让断点随手长出来。 */
      const perContext = new Map<string, Map<string, string[]>>()
      for (const rule of parseCssRules(css)) {
        const sel = rule.selector.replace(/\s+/g, ' ').trim()
        const props = [...rule.body.matchAll(/(?:^|;)\s*([a-z-]+)\s*:/g)].map((m) => m[1])
        if (rule.atRules.length > 0) {
          const ctx = rule.atRules.join(' / ')
          if (!perContext.has(ctx)) perContext.set(ctx, new Map())
          const inCtx = perContext.get(ctx)!
          const prev = inCtx.get(sel) ?? []
          for (const p of props) {
            if (prev.includes(p)) overridden.push(`${sel} 的 ${p}（同一条 ${ctx} 里写了两次）`)
          }
          inCtx.set(sel, [...prev, ...props])
          continue
        }
        const prev = declaredBefore.get(sel) ?? []
        for (const p of props) {
          if (prev.includes(p)) overridden.push(`${sel} 的 ${p}`)
        }
        declaredBefore.set(sel, [...prev, ...props])
      }
      expect(
        overridden,
        `后一条规则重写了前一条已声明的属性（前者是死声明）：${overridden.join(', ')}`,
      ).toEqual([])
    })
  }

  /* **断点必须显式登记**（v0.8.1 第5轮 G87 的补偿措施）：上一条承认了
     「同选择器拆到两个断点各补一半」守不住。能守的那一半是**断点本身**——
     断点数量是枚举的，多一个就得在这里登记并写明理由。这样至少不会出现
     「随手加一个 1337px 的断点，某条规则只在那里面改了一半」。 */
  const BREAKPOINTS = new Set([
    '(max-width: 720px)', // 会话工作台：Git 面板由左右分栏改上下堆叠
    '(max-width: 900px)', // 外壳：左栏整个不渲染，靠底部面板；看板列回到单列
    '(max-width: 1100px)', // 看板：六列挤在一屏时的列宽下限（G70）
  ])

  it('样式表里出现的媒体断点都在登记表里（新增断点要写明理由）', () => {
    const found = new Set<string>()
    for (const file of ['packages/web/src/styles/components.css', 'packages/web/src/styles/prototype-shell.css']) {
      /* **用结构化解析器取 at-rule 链，不用正则扫原文**：初版写的是
         `/@media\s*\(([^)]*\)?[^)]*)\)/g`，结果它跨过了注释和后面的整段规则，
         把两千多字的注释与十几条规则一起当成「断点字符串」吃了进来
         （AGENTS.md 维度 25 的反面：扫描器既不能丢内容，也不能吃错边界）。
         `parseCssRules` 已经剥了注释、按花括号配对，`atRules` 就是准确的链。 */
      for (const rule of parseCssRules(readRepoFile(file))) {
        for (const at of rule.atRules) {
          const m = /^@media\s+(.+)$/.exec(at.trim())
          if (m) found.add(m[1].replace(/\s+/g, ' ').trim())
        }
      }
    }
    const unknown = [...found].filter((b) => !BREAKPOINTS.has(b))
    expect(
      unknown,
      `出现了未登记的媒体断点：${unknown.join(', ')}——`
        + '把它加进 BREAKPOINTS 并在这里写清它服务哪一处布局；'
        + '断点随手长出来就会变成「一半规则在某个宽度改过、另一处没改」的来源',
    ).toEqual([])
  })

  /* **跨文件的「后者胜」也要看得见**（G24 的另一半）：`index.css` 的导入顺序
     是 tokens → components → prototype-shell → 4 份主题，**顺序本身**是一条
     没人读的隐藏规则（`index.css` 文件头就是这么写的）。两条判据：
       ① 三个基座文件必须按这个次序出现——把 `components` 排到 `tokens` 之前，
          `var()` 会先解析成空值；把 `prototype-shell` 排到 `components` 之前，
          「原型搬取层」会被「补漏版」盖掉。
          **注意：这与 18% 类名重合率无关**——重合率是静态 `grep` 的统计量，
          `@import` 顺序对它零影响（第6轮订正了原先写在这里的错误归因）。
       ② 主题四份必须排在最后且顺序固定：它们定义字面 `--p-*`，要压过
          `tokens.css` 的同名 `var()` 映射层。 */
  it('index.css 的导入顺序是 tokens → components → shell → 四份主题（顺序就是规则）', () => {
    const index = readRepoFile('packages/web/src/styles/index.css')
    const imports = [...index.matchAll(/@import\s+'\.\/([a-z-]+)\.css'/g)].map((m) => m[1])
    expect(
      imports,
      'index.css 的 @import 顺序变了：后者会静默覆盖前者，且没有任何报错',
    ).toEqual(['tokens', 'components', 'prototype-shell', 'theme-neutral', 'theme-warm', 'theme-paper', 'theme-cream'])
    /* 反向：`main.ts` 不许绕过 index.css 直接导入子文件——那会让「谁先加载」
       重新变成一条散落在各处的隐藏规则。 */
    const main = readRepoFile('packages/web/src/main.ts')
    expect(main, 'main.ts 必须只 import styles/index.css（不要各组件各自 import 子样式表）').not.toMatch(
      /import\s+'\.\/styles\/(?:tokens|components|prototype-shell|theme-[a-z]+)\.css'/,
    )
  })

  /* **同一元素上的两个 `af-*` 类，不许一方把另一方全量盖掉**（v0.8.1 第5轮，
     G40 的通用网）。实证：`TaskCard.vue:129` / `BoardPage.vue:771` 写的是
     `class="af-card af-taskcard"`，而 `.af-card` 住在**后导入**的
     `prototype-shell.css`、与 `.af-taskcard` 同为单类选择器、特异性相同——
     于是 `components.css` 里那 14 条声明 **13 条从上线那天起就是死的**，
     `:hover` / `:focus-visible` 也一起废掉。它不报错、不变红，
     只能靠人看出来（这次是靠 G40 的报告看出来的）。

     **判据**：从模板里取每个 `class="…"` 静态属性里出现的 af-* 类，两两成对；
     若两份样式表里各有一条**单类整个主体**的规则（`classNamesInRule` 那一级），
     且其中一条声明的属性被另一条**逐条重写**（cover = 前者的属性集合是后者的
     子集），这一对就是「后者全量覆盖前者」。

     **为什么可以按「主体全量」判、而不像同文件那条判「任一属性重写」**：
     跨文件的赢家是**整条规则体**——后导入者不会只盖一个属性，它盖的是
     同特异性的整个声明块。所以「前者的属性全在后者里」就等于「前者一句没生效」。

     **白名单**只有一条：`af-card + af-taskcard`（`.af-taskcard` 现在只剩
     `align-items: stretch`，`.af-card` 里没有它——这是**有意**的「基础类 +
     一条独有微调」，不是遮蔽）。新增豁免必须在下面写清为什么那个组合成立。 */
  const TWO_CLASS_OVERLAP_ALLOWLIST = new Set(['af-card + af-taskcard'])

  it('同一元素上的两个 af-* 类不许一方全量覆盖另一方（`.af-card` / `.af-taskcard` 那次）', () => {
    const a = classNamesIn(readRepoFile('packages/web/src/styles/components.css'), { wholeSubject: true })
    const b = classNamesIn(readRepoFile('packages/web/src/styles/prototype-shell.css'), { wholeSubject: true })
    /** 某类在**所有**样式表里那些「整个主体就是它」的规则的属性集合（并集）。 */
    const propsOf = (cls: string): Set<string> => {
      const out = new Set<string>()
      for (const file of [
        'packages/web/src/styles/components.css',
        'packages/web/src/styles/prototype-shell.css',
      ]) {
        for (const rule of parseCssRules(readRepoFile(file))) {
          if (!classNamesInRule(rule).includes(cls)) continue
          for (const m of stripCssComments(rule.body).matchAll(/(?:^|;)\s*([a-z-]+)\s*:/g)) out.add(m[1])
        }
      }
      return out
    }

    const covered: string[] = []
    /** 模板里静态 `class="…"` 的扫描范围：`packages/web/src` 下的全部 `.vue`。
        递归辅助与 `vueFilesUsing` 是同一套写法（cwd = `packages/web`）。 */
    const walkVue = (dir: string): void => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const full = resolve(dir, entry.name)
        if (entry.isDirectory()) walkVue(full)
        else if (entry.name.endsWith('.vue')) {
          const src = readFileSync(full, 'utf-8')
          const file = full.slice(process.cwd().length + 1)
          for (const m of src.matchAll(/class="([^"]*)"/g)) {
            if (m[1].includes('${') || m[1].includes('{{')) continue
            const afs = [...new Set(m[1].split(/\s+/).filter((c) => c.startsWith('af-')))]
            for (const x of afs) {
              for (const y of afs) {
                if (x === y) continue
                // 只在「两份样式表各有一份」时才谈得上跨文件覆盖。
                const both = a.has(`.${x}`) && b.has(`.${y}`) && a.has(`.${y}`) && b.has(`.${x}`)
                if (!both) continue
                const px = propsOf(`.${x}`)
                const py = propsOf(`.${y}`)
                if (px.size === 0 || py.size === 0) continue
                const xsCoveredByY = [...px].every((p) => py.has(p))
                if (!xsCoveredByY) continue
                const key = [`${x} + ${y}`, `${y} + ${x}`].find((k) => TWO_CLASS_OVERLAP_ALLOWLIST.has(k))
                if (key) continue
                covered.push(`${file}: \`${x}\` 的 ${px.size} 条声明全被 \`${y}\` 重写（${[...px].join(' / ')}）`)
              }
            }
          }
        }
      }
    }
    walkVue(resolve(process.cwd(), 'src'))
    expect(
      [...new Set(covered)],
      `同一元素上两个 af-* 类，其中一个整块失效（改它不会影响页面）：\n${[...new Set(covered)].join('\n')}`,
    ).toEqual([])
  })

  /* 上一条守着「不许再有新的」。这一条守着**已修的那一对真的修好了**：
     `.af-taskcard` 必须仍是 `.af-card` 的严格子集（只剩一条独有声明），
     且那条独有声明就是 `align-items: stretch`。两条合起来才排得掉
     「改回 14 条声明」与「把独有的一条也删了」两种回潮。 */
  it('`.af-taskcard` 只保留 `.af-card` 覆盖不到的那一条声明（`align-items`）', () => {
    /* `ruleFor` 要求选择器**全文相等**，而 `.af-card` 出现在 `:hover` /
       `:focus-visible` 与后代选择器里的次数远多于本体，所以这里按
       「整个主体恰好是 `.af-card`」筛，而不是 `find(r => r.selector === '.af-card')`。 */
    const shell = parseCssRules(readRepoFile('packages/web/src/styles/prototype-shell.css'))
    const cardRules = shell.filter((r) => classNamesInRule(r).includes('.af-card'))
    expect(cardRules.length, 'shell 里没有 `.af-card`——卡片视觉基准没了').toBeGreaterThan(0)
    const cardProps = new Set(
      cardRules.flatMap((r) =>
        [...stripCssComments(r.body).matchAll(/(?:^|;)\s*([a-z-]+)\s*:/g)].map((m) => m[1]),
      ),
    )
    const own = RULES_OWN('.af-taskcard')
    expect(own.size, '`.af-taskcard` 基础块被整条删掉了（`align-items: stretch` 是它唯一的存在理由）').toBeGreaterThan(0)
    const blind = [...own].filter((p) => cardProps.has(p))
    expect(
      blind,
      `\`.af-taskcard\` 里的 ${blind.join(' / ')} 被 \`.af-card\` 重写（同特异性、后导入 → 从不生效）`,
    ).toEqual([])
    expect([...own], '`.af-taskcard` 的独有声明必须正好是 `align-items`').toEqual(['align-items'])
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
     prototype-shell.css 那边一律带前缀（`.af-drawer` / `html[data-rail='off']`），
     只在特定作用域里改几个声明。

     **`.af-region__head` 那一条已删（v0.8.1 第5轮，G39）**：它放行的是 shell
     里的 `.af-region--warn .af-region__head { background: … }`，而 `.af-region--warn`
     **零模板消费者**——白名单拿一个没人用的变体，给一处真实的重叠开了口子。
     变体连同这条豁免一起删。下面的「白名单条目已不再是真实重叠」反向断言会
     把这种**幽灵条目**判红，所以删规则时必须同时删条目，否则这里立刻红。 */
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
  ]

  it('两份样式表不许有同名类的全量定义（严格口径为空）', () => {
    const shell = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const a = classNamesIn(css, { wholeSubject: true })
    const b = classNamesIn(shell, { wholeSubject: true })
    const both = [...a].filter((s) => b.has(s)).sort()
    expect(both, `同时定义在两个文件里（后导入的 prototype-shell.css 会全覆盖）：${both.join(', ')}`)
      .toEqual([])
    /* **自检：扫描器必须真的在数东西**（v0.8.1 第5轮，G23）。这条守卫最容易
       的死法不是「漏了一种书写形态」，而是**两边都空**——空集与空集的交集
       当然还是空集，守卫恒真。两个文件都在、扫描器也该跑得出几百个类，
       数量崩掉只可能是 `parseCssRules` / `classNamesIn` 被改坏，或文件路径
       拼错读进了空串。所以把规模下限钉在这里。 */
    expect(a.size, 'components.css 一个「单类整个主体」的类都没扫出来').toBeGreaterThan(100)
    expect(b.size, 'prototype-shell.css 一个「单类整个主体」的类都没扫出来').toBeGreaterThan(50)
    /* 再补一个**已知真重叠**的正例。没有这条，「扫描器瞎了 → 交集空 →
       断言绿」的死法就没人拦。正例取自**宽松口径**（它数的是「类名同时
       出现在两份样式表」，包括后代选择器里的命中），且点名一个**不在严格
       交集里**的类——严格口径为空是**期望行为**，不能拿它当正例。
       **正例是 `.af-btn`（见 :744）**：shell 的收起态覆盖里确实有它。
       原注释在这里写「正例：`.af-region__title`」，但 `.af-region*` 规则
       只在 `components.css`（shell 里 0 条，第6轮查 git 历史确认从未移植），
       所以那个名字根本不在 shell 里定义——正例是假的，只是断言那行写的是
       `.af-btn` 才没出事。已改正。 */
    const looseA = classNamesIn(css, { wholeSubject: false })
    const looseB = classNamesIn(shell, { wholeSubject: false })
    const looseBoth = [...looseA].filter((s) => looseB.has(s)).sort()
    expect(looseBoth.length, '宽松口径的跨文件命中数不该少于白名单本身——扫描器瞎了').toBeGreaterThanOrEqual(
      LOOSE_OVERLAP_ALLOWLIST.length,
    )
    expect(looseBoth.includes('.af-btn'), '正例：.af-btn 应当同时出现在两份样式表里（shell 的收起态覆盖）').toBe(true)
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

/**
 * 取一条规则里**某条属性**的声明体（剥注释、按属性名锚定）。
 *
 * 为什么规则块不够用：`cssBlockOf` 取到第一个 `}` 为止，**把块内注释也算了进去**，
 * 而 `toMatch` 只要命中一处就过。所以「块里出现过 `--af-fg-strong`」不等于
 * 「`color` 用的是它」——第 7 轮 4c 用一个变异证明了这一点：
 *
 *   .af-panel__title {
 *     ...
 *     color: var(--af-fg-strong);          ← 正确的那条还在
 *     outline-color: var(--p-fg-2);        ← 追加一条别的属性
 *   }
 *
 * 28 条用例**全绿**，可注释里明明写着「`--p-fg-2` 仍然判红」。原因是断言
 * `toMatch` 命中的是 `color` 那行，追加的 `outline-color` 从未被看。
 * **判据从此锚定到属性名**：`color: X` 里的 X 必须匹配，块内别处出现什么都不算。
 *
 * 找不到该属性返回空串（与 `cssBlockOf` 一致，交给调用方的断言判红）。
 */
function declOf(css: string, selector: string, prop: string): string {
  const raw = cssBlockOf(css, selector)
  // 剥块注释：注释里提到 token 名不算「用了它」（与 token-orphans 守卫同一口径）。
  const body = raw.replace(/\/\*[\s\S]*?\*\//g, '')
  // **取最后一条**：同一条规则里写两次 `color:` 时，CSS 由后者胜出。
  // 取第一条会让「追加一条 `color: var(--p-fg-2)`」这种变异逃逸——
  // 页面上真正显示的是 `--p-fg-2`，而断言读到的是前面那条 strong。
  const all = [...body.matchAll(new RegExp(`(?:^|[;{\\s])${prop}\\s*:\\s*([^;]+);`, 'g'))]
  return all.length ? all[all.length - 1][1].trim() : ''
}
