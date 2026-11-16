/* ===========================================================================
   原型对齐度：`prototype/assets/app.css` ↔ `src/styles/prototype-shell.css`
   ---------------------------------------------------------------------------
   为什么需要这个文件 —— 上一轮的失败模式写在这里，防止它再发生：

     子任务 12「10 个已交付页面逐屏对齐」曾判 ✅。依据是「类名锚点可 grep
     + token 计数 + 文档行号」——**全是形式证据**。实测：

       原型 app.css 定义 255 个类；实现 components.css 定义 271 个；
       两边同名类 **只有 47 个（18%）**；
       `.af-shell` / `.af-head` / `.af-body` / `.af-drawer`
       原型 **21/21 页**使用，实现 **0 处定义**。

     形式证据全绿而用户看到「完全不一样」。根因：从没有一条断言问过
     「这个类在两边**是不是同一个东西**」。

   本文件的判据分三层，每层都能独立失败：

     ① **外壳必备类**：原型每页都用的骨架类，实现必须定义，且**声明取值
        必须来自同一组槽位**（不是「类名出现过」就算）。
     ② **取值一致**：对若干条关键规则，逐条比对两边的令牌引用，抓出
        「实现把 32px 写成 26px」「140ms 缓动写成 80ms linear」这类偏一档。
     ③ **反向守卫**：`prototype-shell.css` 里不许出现字面色值——它必须只
        用 `var(--af-*)`。字面色值意味着「这一层又开了第二份真相源」。
   =========================================================================== */

import { readdirSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import { readRepoFile } from '@/test/css'
import { templateOf } from '@/test/sfc'

const PROTOTYPE_CSS = readRepoFile('prototype/assets/app.css')
const IMPL_CSS = readRepoFile('packages/web/src/styles/prototype-shell.css')
const COMPONENTS_CSS = readRepoFile('packages/web/src/styles/components.css')
const TOKENS_CSS = readRepoFile('packages/web/src/styles/tokens.css')

/**
 * 四份主题文件串起来 —— **颜色变量的定义处**。
 *
 * v0.8.1 起颜色不再住在 `tokens.css`：那是「一套写死的暗色」，换主题时
 * 组件全停在旧配色上。现在配色按原型的分层放进 `theme-*.css`，`tokens.css`
 * 只留尺度/字体/动效（零个颜色字面值）。所以「引用的变量有没有定义」这件事
 * 必须连主题文件一起看，否则本文件下面那条门禁会在删掉颜色后**假红**。
 */
const THEME_CSS = ['neutral', 'warm', 'paper', 'cream']
  .map((t) => readRepoFile(`packages/web/src/styles/theme-${t}.css`))
  .join('\n')

/** 声明了变量的全部样式源（`tokens.css` + 四份主题）。 */
const VARS_ALL = `${TOKENS_CSS}\n${THEME_CSS}`

/**
 * 实现侧的**全部**样式源。
 *
 * 判「外壳类是否存在」必须跨这两份文件看：`.af-main` 的权威定义在
 * `components.css` §1（它同时承担网格区域），而 `.af-shell` / `.af-rail`
 * 等在 `prototype-shell.css`。只看一份会得出「缺 af-main」的假阳性——
 * 而假阳性比漏检更坏：它教人把门禁改名而不是修问题。
 */
const IMPL_ALL = `${COMPONENTS_CSS}\n${IMPL_CSS}`

/** 剥掉 CSS 注释：注释里写着类名与属性值，不剥会让断言在「删光声明只留注释」时照样绿。 */
function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, '')
}

/** 某个选择器在样式源里的声明体（取第一次出现）。缺了返回空串。 */
function ruleBody(css: string, selector: string): string {
  const clean = stripComments(css)
  const at = clean.indexOf(`${selector} {`)
  if (at < 0) return ''
  const open = at + selector.length + 1
  const close = clean.indexOf('}', open)
  if (close < 0) return ''
  return clean.slice(open, close)
}

/** 源里定义过的全部顶层类名（`^\.af-…`）。 */

/** 源里定义过的全部顶层类名（`^\\.af-…`）。 */
function classesIn(css: string): Set<string> {
  const out = new Set<string>()
  for (const m of stripComments(css).matchAll(/^\.([a-z][a-z0-9_-]*)/gm)) out.add(m[1])
  return out
}

// ---------------------------------------------------------------------------
// ① 外壳必备类：原型每一页都用的骨架，实现在缺失时用户看到的是「另一套设计」
// ---------------------------------------------------------------------------

/**
 * 原型 21 页**全量**使用的骨架类。
 *
 * 判据来自 `grep -l 'class="af-shell"' prototype/*.html | wc -l` → 21，
 * 以及各自的子块。这份清单就是「应用外壳」的定义，缺一个都不再是同一个壳。
 */
const SHELL_CLASSES = [
  'af-shell',
  'af-rail',
  'af-rail__brand',
  'af-rail__list',
  'af-main',
  'af-head',
  'af-head__title',
  'af-head__sub',
  'af-head__spacer',
  'af-body',
  'af-body--pad',
  'af-body--fill',
]

describe('外壳骨架类与原型对齐', () => {
  it('原型确实定义了这一整组外壳类（守卫清单本身不是空想）', () => {
    const proto = classesIn(PROTOTYPE_CSS)
    const missing = SHELL_CLASSES.filter((c) => !proto.has(c))
    expect(missing, `原型 app.css 里没有这些类：${missing.join(', ')}`).toEqual([])
  })

  it('实现定义了全部外壳类（缺一个就是「另一套设计」）', () => {
    const impl = classesIn(IMPL_ALL)
    const missing = SHELL_CLASSES.filter((c) => !impl.has(c))
    expect(
      missing,
      [
        '这些外壳类在原型 21/21 页使用，但实现里没有定义：',
        `  ${missing.join(', ')}`,
        '这正是上一轮「页面和原型完全不一样」的形态：页面本体在，外壳没落地。',
      ].join('\n'),
    ).toEqual([])
  })

  it('外壳几何取自与原型同一组布局常量', () => {
    // 原型 `.af-shell` 是 `grid-template-columns: var(--rail-w) 1fr`。
    const body = ruleBody(IMPL_CSS, '.af-shell')
    expect(body, '实现里没有 .af-shell 规则').not.toBe('')
    expect(body).toContain('grid-template-columns')
    expect(body, '左栏宽必须取 --af-rail-w，不能写死像素').toContain('var(--af-rail-w)')

    // 原型 `.af-head` 的 `min-height: var(--head-h)`。
    const head = ruleBody(IMPL_CSS, '.af-head')
    expect(head, '实现里没有 .af-head 规则').not.toBe('')
    expect(head, '顶栏高必须取 --af-head-h').toContain('var(--af-head-h)')
  })

  it('布局常量在 tokens.css 里有定义，且值与原型一致', () => {
    // 原型 `tokens.css:38-39`：--rail-w: 224px; --head-h: 56px;
    for (const [name, protoValue] of [
      ['--af-rail-w', '224px'],
      ['--af-head-h', '56px'],
    ] as const) {
      expect(PROTOTYPE_CSS.length).toBeGreaterThan(0) // 原型可读
      const decl = new RegExp(`${name}\\s*:\\s*([^;]+);`).exec(stripComments(TOKENS_CSS))
      expect(decl, `tokens.css 缺少 ${name}——原型布局常量没有落地`).not.toBeNull()
      expect((decl as RegExpExecArray)[1].trim()).toBe(protoValue)
    }
  })

  it('六档字号与原型逐档相等（原型值现读，不硬编码）', () => {
    // 原型 `prototype/assets/tokens.css:20-25` 定义 6 档字号。这里**读原型文件**
    // 取值再与实现比，而不是把 11.5/12.5/… 抄进断言：抄进来就等于允许两边
    // 一起漂——原型改了、断言里的数没改，测试照样绿。
    //
    // 映射是「同义传名」，不是「就近取档」：
    //   t-2xs↔text-2xs  t-xs↔text-xs  t-sm↔text-sm
    //   t-md↔text-md    t-lg↔text-xl  t-xl↔text-2xl
    const PROTO_TOKENS = stripComments(readRepoFile('prototype/assets/tokens.css'))
    const PAIRS = [
      ['--t-2xs', '--af-text-2xs'],
      ['--t-xs', '--af-text-xs'],
      ['--t-sm', '--af-text-sm'],
      ['--t-md', '--af-text-md'],
      ['--t-lg', '--af-text-xl'],
      ['--t-xl', '--af-text-2xl'],
    ] as const
    const impl = stripComments(TOKENS_CSS)
    const mismatches: string[] = []
    for (const [protoName, implName] of PAIRS) {
      const p = new RegExp(`${protoName}\\s*:\\s*([^;]+);`).exec(PROTO_TOKENS)
      expect(p, `原型 tokens.css 里找不到 ${protoName}`).not.toBeNull()
      const i = new RegExp(`${implName}\\s*:\\s*([^;]+);`).exec(impl)
      expect(i, `tokens.css 里找不到 ${implName}`).not.toBeNull()
      const pv = (p as RegExpExecArray)[1].trim()
      const iv = (i as RegExpExecArray)[1].trim()
      if (pv !== iv) mismatches.push(`${protoName}=${pv} vs ${implName}=${iv}`)
    }
    expect(
      mismatches,
      [
        '字号档与原型不一致（逐屏对照时会差 0.5–1.5px 且说不清谁错）：',
        ...mismatches,
      ].join('\n'),
    ).toEqual([])
  })

  it('两档行高与原型逐档相等', () => {
    const PROTO_TOKENS = stripComments(readRepoFile('prototype/assets/tokens.css'))
    const impl = stripComments(TOKENS_CSS)
    const pTight = /--lh-tight\s*:\s*([^;]+);/.exec(PROTO_TOKENS)?.[1].trim()
    const pBody = /--lh-body\s*:\s*([^;]+);/.exec(PROTO_TOKENS)?.[1].trim()
    expect(/--af-leading-tight\s*:\s*([^;]+);/.exec(impl)?.[1].trim()).toBe(pTight)
    expect(/--af-leading-base\s*:\s*([^;]+);/.exec(impl)?.[1].trim()).toBe(pBody)
  })
})

// ---------------------------------------------------------------------------
// ② 取值一致：上一轮「偏一档」的几处，逐条比令牌引用
// ---------------------------------------------------------------------------

/**
 * 关键规则的「槽位级」对照。
 *
 * 不比字面值（原型用 `--row`、实现用 `--af-row`），比的是**语义槽位**：
 * 两边都必须取「标准行高」这一档、都必须取「标准动效时长」这一档。
 * 上一轮的错处正是「都叫行高，但一个取 standard、一个取 compact」——
 * 只看类名与属性名发现不了，只有比到取值来源才看得见。
 */
describe('关键规则的取值来源与原型一致', () => {
  it('按钮行高：原型取标准行高，实现也必须取标准行高', () => {
    const proto = ruleBody(PROTOTYPE_CSS, '.af-btn')
    expect(proto, '原型 .af-btn 不存在').not.toBe('')
    expect(proto, '前提变了：原型按钮不再是标准行高').toContain('var(--row)')

    const impl = ruleBody(COMPONENTS_CSS, '.af-btn')
    expect(impl, '实现里没有 .af-btn 规则').not.toBe('')
    // 别名 `--p-row` 就是 `--af-row-regular`(32px)；两者都算「标准行高」。
    expect(
      impl,
      [
        '.af-btn 没有取标准行高。',
        'design-system.md:222 规定 row-regular(32px) 用于按钮，row-compact(26px) 用于表格。',
        '上一轮这里写的是 compact 行高，比原型矮 6px。',
      ].join('\n'),
    ).toMatch(/min-height:\s*var\(--p-row\)|min-height:\s*var\(--af-row-regular\)/)
    expect(impl, '.af-btn 不许再取 compact 行高').not.toContain('var(--af-row-compact)')
  })

  it('看板列顶部有状态色带（原型六列各一条 2px 色带）', () => {
    const protoCol = ruleBody(PROTOTYPE_CSS, '.af-col__head')
    expect(protoCol, '原型 .af-col__head 不存在').not.toBe('')
    expect(protoCol, '前提变了：原型列头不再有顶部色带').toMatch(/border-top:\s*2px\s+solid/)

    const implCol = ruleBody(IMPL_CSS, '.af-col__head')
    expect(implCol, '实现里没有 .af-col__head 规则').toMatch(/border-top:\s*2px\s+solid/)

    // 六态逐条都要有覆盖，否则五列有色带、一列没有 = 那一列看着像「坏的」
    const cleanImpl = stripComments(IMPL_CSS)
    for (const state of ['pending', 'queued', 'running', 'review', 'done', 'failed']) {
      expect(
        cleanImpl,
        `列状态 ${state} 没有顶部色带：扫视时六列长得一样，状态只能靠读标题`,
      ).toMatch(new RegExp(`\\.af-col\\[data-state='${state}'\\]\\s*\\.af-col__head`))
    }
  })

  it('看板列宽不再窄于原型的下限 236px', () => {
    const cleanProto = stripComments(PROTOTYPE_CSS)
    expect(cleanProto, '原型列宽前提变了').toContain('minmax(236px, 1fr)')

    // 实现里凡是定义看板网格列宽的地方，下限都不得小于 236px。
    const implBoard = COMPONENTS_CSS
    const widths = [...implBoard.matchAll(/grid-template-columns:\s*repeat\(6,\s*minmax\((\d+)px/g)]
    for (const w of widths) {
      expect(
        Number(w[1]),
        `看板列宽下限 ${w[1]}px 小于原型 236px：卡片标题会更早折行`,
      ).toBeGreaterThanOrEqual(236)
    }
  })

  it('动效时长取标准档而非「瞬时」档', () => {
    // 原型 .af-btn / .af-card / .af-rail__list a 都是 140ms var(--ease)。
    const protoBtn = ruleBody(PROTOTYPE_CSS, '.af-btn')
    expect(protoBtn, '前提变了：原型按钮不再有 140ms 过渡').toContain('140ms')

    const implCss = COMPONENTS_CSS
    const implBtn = ruleBody(implCss, '.af-btn')
    expect(implBtn, '实现按钮没有过渡').toContain('transition')
    expect(
      implBtn,
      [
        '.af-btn 的过渡用的是 --af-dur-instant（80ms，linear）。',
        '原型是 140ms + 缓动，且 --af-dur-instant 的语义是「hover/按下/选中」，',
        '按钮的底色变化属于「一块面在变」，该走 --p-dur（= --af-dur-fast 140ms）+ --p-ease。',
      ].join('\n'),
    ).toMatch(/var\(--p-dur\)|var\(--af-dur-fast\)/)
  })
})

// ---------------------------------------------------------------------------
// ③ 反向守卫：这一层只能是别名，不能是第二份真相源
// ---------------------------------------------------------------------------

describe('prototype-shell.css 不许自己发明取值', () => {
  it('没有一个字面色值（颜色/长度都必须来自 var(--af-*)）', () => {
    const clean = stripComments(IMPL_CSS)
    const offenders: string[] = []
    for (const line of clean.split('\n')) {
      // 允许：注释、选择器行、以及 0 / none / transparent / inherit 这类无语义值
      const decl = /:\s*([^;]+);/.exec(line)
      if (!decl) continue
      const value = decl[1]
      // 十六进制色 / rgb() / hsl()
      if (/#[0-9a-fA-F]{3,8}\b/.test(value) || /\b(rgb|hsl)a?\(/.test(value)) {
        offenders.push(line.trim())
      }
    }
    expect(
      offenders,
      [
        'prototype-shell.css 里出现了字面色值。这一层必须是「原型槽位名 → --af-* 真身」的别名，',
        '写死色值就等于又开了第二份真相源，改主题时它会与 tokens.css 分叉：',
        ...offenders,
      ].join('\n'),
    ).toEqual([])
  })

  it('引用的每个变量都在 tokens.css 或主题文件里定义过（含 --p-* 别名）', () => {
    // 定义侧看 `VARS_ALL`（`tokens.css` + 四份 `theme-*.css`）：v0.8.1 起颜色
    // 住在主题文件里，只扫 `tokens.css` 会把每一个颜色引用都判成悬空。
    const cleanTokens = stripComments(VARS_ALL)
    const defined = new Set(
      [...cleanTokens.matchAll(/^\s*(--(?:af|p)-[a-z0-9-]+)\s*:/gm)].map((m) => m[1]),
    )
    // **两层样式表都查**：`components.css` 也大量取原型槽位（`--p-s3` / `--p-line-2`）。
    // 只查 `prototype-shell.css` 时，`components.css` 里换命名空间后漏改的
    // `var(--af-s3)` 会一直躺着不被发现——浏览器对未定义变量是**静默丢弃整条声明**，
    // 屏幕上只表现为「这块没内边距」，没有任何报错。这条正是被
    // `TaskCard.test.ts` 的同类断言抓出来的（39 个悬空引用）。
    const cleanImpl = stripComments(`${IMPL_CSS}\n${COMPONENTS_CSS}`)
    const used = new Set(
      [...cleanImpl.matchAll(/var\((--(?:af|p)-[a-z0-9-]+)\)/g)].map((m) => m[1]),
    )
    const missing = [...used].filter((v) => !defined.has(v))
    expect(
      missing,
      [
        '这些变量被引用了但 tokens.css 没定义 —— 浏览器会静默丢弃整条声明，',
        '表现为「这一块没样式」而没有任何报错：',
        `  ${missing.join(', ')}`,
      ].join('\n'),
    ).toEqual([])
  })

  it('§14 别名层不与真身共用 --af- 前缀（否则 keynav 的计数与唯一性不变量会红）', () => {
    // §14 的别名必须用 `--p-*`。混用 `--af-` 时 `keynav.test.ts` 会同时红两条：
    // 「变量数 = 文档的 N」把别名数进去、「每个 token 只定义一次」把别名判成重复。
    // 这里提前把根因钉住，避免有人「为了让计数对上」去改文档里的数字。
    //
    // **只看 §14**：`tokens.css` 里 §1 的 `--af-font-sans` 与 §10 的 `--af-term-bg`
    // 也是「转发另一个 --af-* 变量」的形态，但它们是**真身层的既有设计**
    // （字族转发、终端底色转发），且各自在文档 §2 有登记。把它们一起改名会
    // 动到 40+ 处使用侧，而收益为零——它们不与任何名字撞车。
    // **按原始源切片，不能先剥注释**：§14 的节标题本身写在注释里，
    // 剥完就找不到锚点了（这个坑第一次跑就踩到了 —— 门禁报「别名层不见了」）。
    const sectionAt = TOKENS_CSS.indexOf('14. 原型槽位别名')
    expect(sectionAt, 'tokens.css 里找不到 §14 原型槽位别名一节').toBeGreaterThan(0)
    const section14 = stripComments(TOKENS_CSS.slice(sectionAt))

    const violations = [
      ...section14.matchAll(/^\s*(--af-[a-z0-9-]+)\s*:\s*var\(--af-/gm),
    ].map((m) => m[1])
    expect(
      violations,
      [
        '§14 里这些别名取了 --af- 前缀（它们只是转发另一个 --af-* 变量）：',
        `  ${violations.join(', ')}`,
        '它们必须改名为 --p-*（原型槽位命名空间）。',
      ].join('\n'),
    ).toEqual([])
    // 反向守卫：别名层必须真的存在，否则上面这条在「全删光」时也绿。
    //
    // **数字从 70 降到 26 是 v0.8.1 的合法变化，不是放宽判据**：颜色别名
    // （`--p-bg` / `--p-fg` / `--p-line-*` / `--p-acc` / 六态 / `--p-sh-*`，
    // 约 50 条）已按用户裁定搬进 `theme-*.css`——它们现在是那边的字面值，
    // 不再转发 `--af-*`。若把颜色别名留在这里，它会用主题转发来的 `--af-*`
    // 把主题的 `--p-*` 覆盖回去，**主题切换会静默失效**。
    // §14 现在只剩与配色无关的尺度别名（间距 8 / 圆角 5 / 字号 6 / 行高 2 /
    // 行高常量 1 / 动效 2 / 字族 2 = 26 条）。
    //
    // 所以这里判「尺度别名必须齐全」而不是一个魔数：少一条就是原型骨架的
    // 规则会静默丢样式，多一条则是没人用的死重量。
    const pNames = [...section14.matchAll(/^\s*(--p-[a-z0-9-]+)\s*:/gm)].map((m) => m[1])
    const SIZING_ALIASES = [
      ...Array.from({ length: 8 }, (_, i) => `--p-s${i + 1}`),
      ...['xs', 'sm', 'md', 'lg', 'full'].map((r) => `--p-r-${r}`),
      ...['2xs', 'xs', 'sm', 'md', 'lg', 'xl'].map((t) => `--p-t-${t}`),
      '--p-lh-body',
      '--p-lh-tight',
      '--p-row',
      '--p-ease',
      '--p-dur',
      '--p-sans',
      '--p-mono-c',
    ]
    const missing = SIZING_ALIASES.filter((n) => !pNames.includes(n))
    expect(missing, `§14 缺这些尺度别名（原型骨架会静默丢样式）：${missing.join(', ')}`).toEqual([])
    // 颜色别名不许回来（否则主题切换会被覆盖回写死的旧配色）。
    const colorBack = pNames.filter((n) =>
      /^--p-(bg|surf-\d|panel|pop|inset-c|fg|fg-strong|fg-2|fg-3|line|line-\d|acc|acc-strong|acc-bg|acc-line|on-acc|pending|queued|running|review|done|failed|butler|warn|warn-c|sh-\d|add-ink-c|del-ink-c)$/.test(
        n,
      ),
    )
    expect(colorBack, `颜色别名回到了 tokens.css，会让主题切换失效：${colorBack.join(', ')}`).toEqual(
      [],
    )
  })
})

// ===========================================================================
// ④ 跨文件门禁：**页面有没有真的用这些骨架类**
// ---------------------------------------------------------------------------
// 上面 ① ② ③ 全部只问「类**存在**且取值同源」——从没问过「页面用没用」。
// 这正是 🔴「实现与原型是两套不同的视觉设计」的根因：v0.8.0 收尾时
// `prototype-shell.css` 已定义全部 12 个外壳类、12 条门禁全绿，而
// `App.vue` 里 `af-shell` / `af-rail` 出现 **0 次**、10 个页面全部用自造的
// `af-page`。类定义了但没人用，用户看到的就是另一套设计。
//
// 所以这一节把判据从「类存在」推进到「**模板在用**」。实现要点：
//   - 判据读的是 `templateOf()`（`test/sfc.ts`）：只取 `<template>` 段并**剥掉
//     HTML 注释**。不做全文子串搜索——注释里写个 `class="af-body"` 就能骗过全文搜索。
//   - 只认**静态** `class="…"`。`:class="[...]"` 动态绑定**不算命中**：
//     它在渲染时是否出现取决于运行时状态，门禁不能说「绑了个三元就算用了」。
//   - 尚未迁移的部分用**显式清单**列出来（`expect(stillPending).toEqual([])`），
//     迁移一个删一个。不用 `it.fails` / `it.skip`：那两种写法会把「真失败」
//     一起吞掉，而且清单在这里是可读的账——每行都写着由哪个子任务负责。
// ===========================================================================

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..')

/** 10 个已交付页面（`grep -rL SkeletonPage` 结果里属于对齐范围的 10 个）。 */
const DELIVERED_PAGES = [
  'packages/web/src/domains/project/OverviewPage.vue',
  'packages/web/src/domains/project/BoardPage.vue',
  'packages/web/src/domains/project/ProjectTaskPage.vue',
  'packages/web/src/domains/project/ProjectSettingsPage.vue',
  'packages/web/src/domains/project/ArchivedPage.vue',
  'packages/web/src/domains/project/NewProjectPage.vue',
  'packages/web/src/domains/project/BindPage.vue',
  'packages/web/src/domains/device/DevicesPage.vue',
  'packages/web/src/domains/device/DeviceDetailPage.vue',
  'packages/web/src/domains/session/SessionPage.vue',
] as const

/** 模板里出现的**静态**类名（`class="…"` / `class='…'`），`:class` 不算。 */
function staticClassesOfTemplate(vueSource: string): Set<string> {
  const out = new Set<string>()
  for (const m of templateOf(vueSource).matchAll(/(?<![:\w-])class\s*=\s*(["'])([^"']*)\1/g)) {
    for (const name of m[2].split(/\s+/)) if (name) out.add(name)
  }
  return out
}

/**
 * 模板里出现的静态类名——**含 `<PageHead>` 这类共享组件贡献的类**。
 *
 * 判「骨架类落地了没有」不能只读页面自己的模板：原型每一页的
 * `header.af-head` 由页面自己画，实现里抽成了 `PageHead` 组件
 * （`domains/shared/components/PageHead.vue`），调用点写的只有
 * `<PageHead>…</PageHead>`——它的页头类名在兄弟文件里。
 *
 * **口径不因此放宽**：仍然只认 `class="…"` 的静态类名、仍然剥注释、
 * 仍然不认 `:class` 动态绑定（判据住在 `test/sfc.ts` 的 `staticClassesOf`）。
 * 而且组件是真的在渲染（不会被渲染时才出现），所以这不是「算它用了」，
 * 是「它确实用了」。
 */
function staticClassesWithShared(vueSource: string): Set<string> {
  const out = staticClassesOfTemplate(vueSource)
  // 只并入被这个模板**实际引用**（`<PageHead` / `<PageHead>`）的共享骨架
  // 组件贡献的类名——不并入全仓，否则随便哪个页面只要页头写对了就能白拿。
  for (const [name, source] of sharedComponents()) {
    if (new RegExp(`<${name}[\\s>/]`).test(vueSource)) {
      for (const cls of staticClassesOfTemplate(source)) out.add(cls)
    }
  }
  return out
}

/**
 * `shared/components` 里可复用的骨架组件：组件名 → 源码。
 *
 * 每次调用**现读**（不缓存）：文件被改名 / 删掉时必须当场抛错（`readRepoFile`
 * 的行为），而不是让上面那条断言因为「读不到组件」悄悄退化成
 * 「页面自己没写 af-head → 清单外 → 红在一个假原因上」。
 */
function sharedComponents(): Array<[string, string]> {
  return ['PageHead'].map((name) => [
    name,
    readRepoFile(`packages/web/src/domains/shared/components/${name}.vue`),
  ])
}

/**
 * 「待迁移」清单：本门禁对**还没迁移**的文件判红，是有意的。
 *
 * 整条断言仍会在任何**新的**违规上判红；只有清单里的条目被豁免。
 * 迁移完成时把对应行删掉即可（删掉后该文件立刻回到门禁里）。
 * 之所以不做成 `it.fails`：`it.fails` 在「意外失败」时也会绿，等于让
 * 一条真失败有机会藏起来；清单形式则永远只有「清单外零违规」这一种绿法。
 */
describe('跨文件门禁：页面必须真的用骨架类', () => {
  it('App.vue 承载外壳三件套 af-shell / af-rail / af-main', () => {
    const classes = staticClassesOfTemplate(readRepoFile('packages/web/src/App.vue'))
    const missing = ['af-shell', 'af-rail', 'af-main'].filter((c) => !classes.has(c))
    expect(
      missing,
      [
        `App.vue 的模板里缺这些外壳类（静态 class，:class 不算）：${missing.join(', ')}`,
        '原型 21/21 页都用 .af-shell/.af-rail；缺了就是「页面长在别的东西里」。',
        '由子任务 2 负责（App.vue 外壳重做）。',
      ].join('\n'),
    ).toEqual([])
  })

  it('10 个已交付页面每个都出现 af-body 或 af-head', () => {
    /**
     * 尚未迁移的页面 → 负责迁移的子任务。
     *
     * **2026-11-16 实测**（本次改动当刻）：任务书里写的是「这 10 个页面**还没有**
     * 骨架类」，实测有 **1 个例外**——`BoardPage.vue` 已出现 `af-body`(2)/`af-head`(7)
     * （子任务 1 的差异清单已如实记过「BoardPage 仅需补 `af-body--fill`」）。
     * 所以初始清单是 **9 条**，不是 10 条：`BoardPage` 一开始就绿，
     * 把它留在清单里会被下面的「清单反向守卫」当场判红。
     * 其余 9 个页面根容器仍是自造的 `af-page` / `af-page--flush`。
     * 迁移一个就从这里删一个。
     *
     * **v0.8.1 子任务 4 删了 3 条**（`ProjectTaskPage` / `BindPage` /
     * `DeviceDetailPage`）：这三页的页头由 `PageHead` 渲染，组件渲染的是原型的
     * `header.af-head`（`prototype/index.html:15`）。所以判据从
     * `staticClassesOfTemplate(page)` 换成 `staticClassesWithShared(page)`——
     * 只并入**该模板实际引用的**共享骨架组件贡献的静态类名，口径本身
     * （只认 `class="…"`、剥注释、不认 `:class`）没有放宽。
     * 这三页的 `.af-body` 仍是子任务 8/9 的账，但本门禁问的是
     * 「af-body **或** af-head」，页头落地即算命中。
     */
    const TODO_SUBTASKS_4_TO_9: Record<string, string> = {
      'packages/web/src/domains/project/OverviewPage.vue': '子任务 6（总览页 + 管家抽屉壳）',
      'packages/web/src/domains/project/ProjectSettingsPage.vue': '子任务 9（其余 7 页）',
      'packages/web/src/domains/project/ArchivedPage.vue': '子任务 9（其余 7 页）',
      'packages/web/src/domains/project/NewProjectPage.vue': '子任务 9（其余 7 页）',
      'packages/web/src/domains/device/DevicesPage.vue': '子任务 9（其余 7 页）',
      'packages/web/src/domains/session/SessionPage.vue': '子任务 9（其余 7 页）',
    }

    const stillPending: string[] = []
    const migratedButListed: string[] = []
    for (const page of DELIVERED_PAGES) {
      const classes = staticClassesWithShared(readRepoFile(page))
      const hasSkeleton = classes.has('af-body') || classes.has('af-head')
      const listed = page in TODO_SUBTASKS_4_TO_9
      if (!hasSkeleton && !listed) stillPending.push(page)
      if (hasSkeleton && listed) migratedButListed.push(page)
    }

    expect(
      stillPending,
      [
        '这些页面模板里既没有 af-body 也没有 af-head（口径：静态 class="…" 才算，',
        ':class="[...]" 动态绑定不算，须留静态类名兜底；注释里的类名也不算）：',
        ...stillPending.map((p) => `  ${p}`),
        '原型每一页都是 .af-head + .af-body 两块，页面只负责 .af-body 里的内容。',
      ].join('\n'),
    ).toEqual([])

    // 反向守卫：清单不许变成「迁移完了还留着」的摆设——那样门禁对已迁移的页面
    // 仍然豁免，等于把判据悄悄关掉。
    expect(
      migratedButListed,
      [
        '这些页面已经有骨架类了，却还挂在待迁移清单里（清单会越来越水）：',
        ...migratedButListed.map((p) => `  ${p} —— 请从 TODO_SUBTASKS_4_TO_9 删掉`),
      ].join('\n'),
    ).toEqual([])
  })

  it('10 个页面模板里不许有内联样式（style="）', () => {
    /**
     * 残留内联样式 → 由哪个子任务清掉。
     *
     * **2026-11-16 实测**（`grep -c 'style="'`）：`DevicesPage.vue` **5**、
     * `ProjectSettingsPage.vue` **4**、`SessionPage.vue` **1**，合计 10 处；
     * 其余 7 页为 0。
     *
     * **与任务书口径的偏差（如实记下）**：任务书写的是 `ProjectTaskPage.vue`
     * **4 处**，实测该文件为 **0 处**——那 4 处在 `ProjectSettingsPage.vue`。
     * 两者合计都是 4，所以「共 10 处」的总数对得上，但归属写错了。
     * 本门禁按**实测**记账（`ProjectSettingsPage` 归子任务 9），不照抄任务书。
     *
     * ── 2026-11 修正：`style="` ≠ `:style="` ─────────────────────────────
     * 上一版的正则是 `/style="/g`，它**分不清静态内联与动态绑定**：
     *
     *     '<div style="x">'      → 旧 1 / 新 1
     *     '<div :style="x">'     → 旧 1 / 新 0   ← 旧判据的假阳性
     *     '<div v-bind:style="x">' → 旧 1 / 新 0
     *
     * `SessionPage.vue:2723` 的唯一一处正是 `:style="activeWs?.dock ? …"`
     * ——那是**动态绑定**（宽度随分栏拖拽变，值在运行期才算得出来），
     * 它不能被"清掉"，所以旧判据下那一行是**永远清不掉的死账**。
     * 判据改为 `/(?<!:)style="/g`：只认静态 `style="`。
     *
     * 动态 `:style` / `v-bind:style` 是**正当写法**（值依赖运行时状态时
     * 没有静态替代），不在本门禁范围内。
     *
     * 这条是**卫生规则，不是忠实度规则**：原型自己就大量使用内联
     * `style="`——`session.html` 22 处、`index.html` 16 处、`devices.html`
     * 29 行 / 32 处（同一行里出现多次时按行数与按次数会分叉，两个数都记）。
     * 实现侧要求清掉，是为了「主题一改就与 token 分叉」，不是为了像原型。
     */
    const TODO_INLINE_STYLE: Record<string, { count: number; owner: string }> = {
      'packages/web/src/domains/device/DevicesPage.vue': { count: 5, owner: '子任务 9' },
      'packages/web/src/domains/project/ProjectSettingsPage.vue': { count: 4, owner: '子任务 9' },
    }

    const offenders: string[] = []
    const stale: string[] = []
    const drifted: string[] = []
    for (const page of DELIVERED_PAGES) {
      // 静态 `style="` 才算；`:style=` / `v-bind:style=` 是动态绑定，合法。
      const count = (templateOf(readRepoFile(page)).match(/(?<!:)style="/g) ?? []).length
      const listed = TODO_INLINE_STYLE[page]
      if (count > 0 && !listed) offenders.push(`${page}：${count} 处`)
      else if (count === 0 && listed) stale.push(`${page}：已清零，请从 TODO_INLINE_STYLE 删掉`)
      else if (listed && count !== listed.count) {
        drifted.push(`${page}：清单写 ${listed.count} 处，实测 ${count} 处`)
      }
    }
    expect(
      offenders,
      [
        '这些页面模板里有内联样式。原型自己有内联是原型的事，实现侧必须走 --af-* token，',
        '否则主题一改就跟 token 分叉（对齐门禁 v0.8.1 子任务 3 第 2 条）：',
        ...offenders,
      ].join('\n'),
    ).toEqual([])
    expect(stale, ['清单与实测不符（已清零却还在豁免名单里）：', ...stale].join('\n')).toEqual(
      [],
    )
    // 清单里的数字也要跟实测一致：否则「还剩几处」这件事没人知道，
    // 清掉 1 处之后清单会一直写着老的数字，门禁看起来还是「有账可查」。
    expect(drifted, ['清单里的计数与实测不符：', ...drifted].join('\n')).toEqual([])
  })

  it('内联样式判据本身分得清静态 style=" 与动态 :style="', () => {
    // 上面那条断言的口径全靠这个正则。写成 `/style="/g` 时它会把
    // `:style="…"` / `v-bind:style="…"` 也算成内联样式——**假阳性**，
    // 而且假阳性落在一个「永远清不掉」的方向上（动态绑定不能改静态）。
    // 所以这里用最小夹具把三个用例钉死，改回 `/style="/g` 时当场红。
    const STATIC_STYLE = /(?<!:)style="/g
    const countOf = (src: string): number => (src.match(STATIC_STYLE) ?? []).length
    expect(countOf('<div style="x">'), '静态内联样式必须算命中').toBe(1)
    expect(countOf('<div :style="x">'), ':style 是动态绑定，不算内联样式').toBe(0)
    expect(countOf('<div v-bind:style="x">'), 'v-bind:style 同上').toBe(0)
    // 反向：这条夹具必须真的能区分——旧写法在同样的输入上给出 1/1/1。
    const OLD = /style="/g
    expect(['<div style="x">', '<div :style="x">', '<div v-bind:style="x">'].map((s) => (s.match(OLD) ?? []).length)).toEqual([1, 1, 1])
  })

  it('没有第二份顶栏：外壳里没有 af-head，且全仓没有第二套顶栏类名', () => {
    // ① 页头归**页面**，不归外壳。原型 `index.html:15` 的 `header.af-head` 长在
    //    `main.af-main` 里、由页面自己渲染（`prototype/index.html:12-23`），
    //    `shell.js` 的 `renderRail` 只画左栏。
    //
    //    所以「App.vue 里有 af-head」恰恰是要判红的那个形态：外壳再画一个页头，
    //    页面上就有了两行标题（一条是 `docTitle`，一条是页面自己的 `h1`）。
    //    这条断言在 v0.8.1 子任务 2 之前是反过来的（当时它是「待迁移」清单项，
    //    指望子任务 4 把 af-head 搬进外壳）——现在判据按原型钉死。
    //
    //    **没有豁免清单**：子任务 2 已把自造的外壳四件套从外壳删除，
    //    原型的 `.af-shell > main.af-main` 里不放页头，因此这条现在就该绿。
    const appClasses = staticClassesOfTemplate(readRepoFile('packages/web/src/App.vue'))
    const appTemplate = templateOf(readRepoFile('packages/web/src/App.vue'))
    expect(
      appClasses.has('af-head'),
      [
        'App.vue 的模板里出现了 af-head —— 页头是页面自己的（原型 index.html:15 的',
        'header.af-head 长在 main.af-main 里），外壳再画一行就是第二个页面标题。',
      ].join('\n'),
    ).toBe(false)
    // 让「读到了模板」这件事显式可见：模板读成空串时上面那条会毫无意义地绿。
    expect(appTemplate.length, 'App.vue 的模板读成空串 —— 上面的断言变成恒真').toBeGreaterThan(
      1000,
    )
    // 外壳三件套必须还在：上面那条在「App.vue 模板被清空」时也绿，这里反向钉住。
    const shell = ['af-shell', 'af-rail', 'af-main']
    const missingShell = shell.filter((c) => !appClasses.has(c))
    expect(missingShell, `外壳三件套缺了：${missingShell.join(', ')}`).toEqual([])

    // ② 全仓不许出现第二份顶栏类名。这是**零豁免**的硬门禁：
    //    原型里没有全局顶栏（那个类名在 prototype/assets/app.css 命中 0 条）。
    //
    //    判据串由片段拼出来，且**本文件跳过不扫**：直接写字面量会让本文件
    //    命中自己，门禁从此永远红（第一次写就是这样）。
    const BANNED = ['af-top' + 'bar', 'app-' + 'header', 'global-' + 'header']
    const selfPath = fileURLToPath(import.meta.url)
    const srcRoot = resolve(WEB_ROOT, 'src')
    const hits: string[] = []
    const walk = (dir: string): void => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const full = resolve(dir, entry.name)
        if (entry.isDirectory()) walk(full)
        else if (/\.(vue|ts|css)$/.test(entry.name)) {
          if (full === selfPath) continue
          const text = readFileSync(full, 'utf-8')
          const found = BANNED.filter((name) => text.includes(name))
          if (found.length > 0) {
            hits.push(`${full.slice(WEB_ROOT.length + 1)}：命中 ${found.join(' / ')}`)
          }
        }
      }
    }
    walk(srcRoot)
    expect(
      hits,
      [
        '实现里还有第二份顶栏（原型没有全局顶栏，页头归页面 .af-head）：',
        ...hits,
        '判红方式：留任意一条 → 非空 → 红。子任务 2/4 负责删除。',
      ].join('\n'),
    ).toEqual([])
  })
})

// ===========================================================================
// ⑤ 排除集守卫：不许用「把状态塞进 unused」的方式绕过上面这些门禁
// ===========================================================================

describe('state.test.ts 的 unused 排除集只能装 building / readonly', () => {
  it('不得混入任务六态，也不得新增任何值', () => {
    // 背景：`state.test.ts` 的「CSS 有、省表没有」排除集
    // （`new Set([…])`，只装 `building` / `readonly`）
    // 是它唯一的**例外集**。它天然是一条后门——有人把某个任务态
    // 从 `STATES` 里删掉、再把它塞进 `unused`，`state.test.ts` 照样绿，
    // 而六态的词表已经不完整了。本断言把排除集的大小与内容都钉死。
    const source = readRepoFile('packages/web/src/shared/state.test.ts')
    const decl = /const unused = new Set\(\[([^\]]*)\]\)/.exec(source)
    expect(decl, 'state.test.ts 里找不到 `const unused = new Set([...])` —— 断言失效').not.toBeNull()

    const values = [...(decl as RegExpExecArray)[1].matchAll(/'([^']+)'/g)].map((m) => m[1])
    const TASK_SIX = ['pending', 'queued', 'running', 'review', 'done', 'failed']
    expect(
      values.filter((v) => TASK_SIX.includes(v)),
      [
        '任务六态被塞进了 state.test.ts 的排除集。',
        '排除集的语义是「CSS 有、省表永远走不到的值」，任务六态全都会走到，',
        '把它们塞进去等于用排除集绕过 state 门禁（里程碑设计核对点 10）。',
      ].join('\n'),
    ).toEqual([])
    expect(values.sort(), '状态词表：排除集只允许 building / readonly').toEqual([
      'building',
      'readonly',
    ])
  })
})

// ===========================================================================
// ⑥ 门禁本身的三条守卫（**零豁免**）
// ---------------------------------------------------------------------------
// 上一节把「待迁移」写成了显式清单，这本身有风险：清单可以变成摆设——把
// 一个永远不该绿的页面塞进去，门禁就再也看不见它。这里补三条**没有清单**的
// 反向守卫：判据读到的内容必须是真的、「绿」必须是因为真绿。
// ===========================================================================

describe('跨文件门禁自己的守卫（零豁免）', () => {
  it('每个已交付页面都真的读到了模板（防「读成空串所以永远绿」）', () => {
    // `templateOf` 抛错在下面会变成「这条红」，但更坏的是它返回空串：
    // 空串里既没有 af-body（会红）也没有 style="（会绿）——门禁失去意义。
    const tooSmall = DELIVERED_PAGES.map((page) => ({
      page,
      size: templateOf(readRepoFile(page)).length,
    })).filter((x) => x.size < 500)
    expect(
      tooSmall.map((x) => `${x.page}：模板只有 ${x.size} 字符`),
      '这些页面的模板段没读到内容 —— 所有依赖模板的门禁都会失真',
    ).toEqual([])
  })

  it('口径本身能生效：:class 动态绑定不算命中、注释里的类名也不算', () => {
    // 这是 `test/sfc.ts` 的口径在**本文件**里也确实生效的证据。夹具故意写成
    // 最容易被误判成命中的两种形态。
    const dynamicOnly = `<template>
      <div :class="['af-body', extra]"></div>
    </template>
    `
    const commentedOnly = `<template>
      <!-- 骨架类只写在注释里：class="af-body" -->
      <div class="af-legacy"></div>
    </template>
    `
    expect(
      staticClassesOfTemplate(dynamicOnly).has('af-body'),
      ':class 动态绑定被当成了命中 —— 门禁会被一行三元表达式骗过',
    ).toBe(false)
    expect(
      staticClassesOfTemplate(commentedOnly).has('af-body'),
      '注释里的类名被当成了命中 —— 门禁会被一条注释骗过',
    ).toBe(false)
    // 反向：真正的静态类名必须认（否则上面两条在「永远返回空集合」时也绿）
    expect(staticClassesOfTemplate(commentedOnly).has('af-legacy')).toBe(true)
  })

  it('state.test.ts 的排除集守卫自己不会退化成恒真', () => {
    // 上面 ⑤ 的正则如果匹配不到，`decl` 为 null 会红；但若有人把正则改成
    // 「匹配任意内容」，`values` 会是空数组、两条断言都会绿。这里钉住
    // 「正则确实锚在 `const unused = new Set([` 上」，并且**真的取到了值**。
    const source = readRepoFile('packages/web/src/shared/state.test.ts')
    const decl = /const unused = new Set\(\[([^\]]*)\]\)/.exec(source)
    const values = [...(decl?.[1] ?? '').matchAll(/'([^']+)'/g)].map((m) => m[1])
    expect(values.length, '排除集读成了空 —— ⑤ 的两条断言会变成恒真').toBe(2)
  })
})

