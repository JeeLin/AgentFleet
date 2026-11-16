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

import { describe, expect, it } from 'vitest'

import { readRepoFile } from '@/test/css'

const PROTOTYPE_CSS = readRepoFile('prototype/assets/app.css')
const IMPL_CSS = readRepoFile('packages/web/src/styles/prototype-shell.css')
const COMPONENTS_CSS = readRepoFile('packages/web/src/styles/components.css')
const TOKENS_CSS = readRepoFile('packages/web/src/styles/tokens.css')

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

  it('引用的每个变量都在 tokens.css 里定义过（含 --p-* 别名）', () => {
    const cleanTokens = stripComments(TOKENS_CSS)
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
    const pNames = [...section14.matchAll(/^\s*(--p-[a-z0-9-]+)\s*:/gm)].map((m) => m[1])
    expect(pNames.length, '原型槽位别名层不见了').toBeGreaterThanOrEqual(70)
  })
})
