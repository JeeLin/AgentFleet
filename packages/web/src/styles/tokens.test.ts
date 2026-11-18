/* ===========================================================================
   tokens.css 与设计文档的守恒式（v0.8.1 第5轮 · 从 `shared/keynav.test.ts` 搬来；
   第6轮 G93 改成「每一个计数都要对上」）
   ---------------------------------------------------------------------------
   这条守卫量的是 `styles/tokens.css` 的变量数，却曾经住在
   `src/shared/keynav.test.ts:110`——**守卫与被守对象不在同一个域**。代价是
   实打实的：本轮验证时只跑 `vitest run src/styles` 就判一个探针「漏网」，
   跑全量才发现它是红的（那次变异在 `tokens.css` 里造了第二个同值的字号档）。

   搬到这里之后「改 `tokens.css` 就跑 `src/styles/`」重新成立：守它的用例与
   文件本身在同一个目录。**`docs/design-system.md` 的那些数字不归这个文件改**：
   它们是设计文档的声明，由文档同步那条线维护；本用例只比对「两边是否相等」，
   不相等就红——换了口径（搬走颜色、加字号档）就得两边一起改，这正是它存在
   的意义。

   ── 为什么第6轮要重写（G93） ──────────────────────────────────────────
   旧版只有一条 `tokenDefinitions() === declaredInDoc()`，而 `declaredInDoc()`
   取的是**第一个** `（N 个变量）` 匹配（`m[0]`）。于是文档里隔 20 行的另一批
   数字（「定义行 64」「61 个唯一名」）完全不在门禁覆盖内，随便写都绿——实测
   就是绿的，而它们是**错的**。

   现在的判据换了形状：**文档里每一类计数都按「措辞 → 量什么」显式登记**，
   逐类拿实测值比对：
     · `（N 个变量）` 括注 —— 文件在 §2 的自己那一节里**声称的定义数**，
       按最严格的量算：`--af-*` 与 `--p-*` **加在一起**的唯一名数；
     · `N 个定义行` —— `tokens.css` 里 `--af-*` / `--p-*` 的**声明条数**；
     · `N 个唯一名` —— 同上，去重后的名字数。
   措辞与量的绑定写在 `COUNTERS` 里，**一个措辞一个量，不许含糊**：
   想让门禁绿，就得让措辞与实测值同时成立。

   ── 「定义行」为什么不数物理行 ────────────────────────────────────────
   旧注释说 `grep -cE '^\s*--af-[a-z0-9-]+\s*:'` 数的是「定义行」。可 CSS 允许
   一行里写多条声明（`--p-s1: …; --p-s2: …;`），`grep -c` 数的是**行**，
   真值是**声明条数**。本仓 `tokens.css` 恰好一行一条，两个口径眼下同值；
   但那只是巧合，不是定义。所以这里按**声明条数**数（结构扫描，不是逐行正则），
   文档里的措辞也统一成「定义行」= 一条自定义属性定义占一条，与行无关。

   ⚠️ 用户或平台跑 `vitest run <某个目录>` 时仍可能漏掉别处的守卫；
   全量套件才是判据。这一条只是把「最容易一起被想起来的那一对」放在一起。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import { stripCssComments } from '@/test/css-scan'

/** 本文件在 `packages/web/src/styles/`，往上**四级**才是仓库根。 */
const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../..')
const DESIGN_DOC = resolve(REPO_ROOT, 'docs/design-system.md')
// 校验我们**自己维护**的资产，不是原型那一份：原型是上游参照，
// 它的数字变了不该让前端门禁变红，反之我们的资产漂了就该红。
const TOKENS_CSS = resolve(REPO_ROOT, 'packages/web/src/styles/tokens.css')

/** 一份样式表里每条自定义属性声明的 `{ name, offset }`（按声明在原串里的位置）。 */
function customPropertyDeclarations(css: string): Array<{ name: string; offset: number }> {
  const stripped = stripCssComments(css)
  const out: Array<{ name: string; offset: number }> = []
  // `--name: value` —— 前面必须是 `{` / `;` / 行首（CSS 自定义属性的声明位置），
  // 这样 `var(--x)` 里的引用不会被当成定义。
  for (const m of stripped.matchAll(/(?:^|[{;])\s*(--[a-zA-Z0-9_-]+)\s*:/g)) {
    out.push({ name: m[1], offset: m.index + m[0].indexOf(m[1]) })
  }
  return out
}

const TOKENS_SRC = readFileSync(TOKENS_CSS, 'utf-8')
const TOKENS_DECLS = customPropertyDeclarations(TOKENS_SRC)

/** 该文件里 `--af-*` / `--p-*` 的声明条数（「定义行」的口径）。 */
function definedLines(): number {
  return TOKENS_DECLS.filter((d) => /^--(af|p)-/.test(d.name)).length
}

/** 同上，去重后的名字数（「唯一名」的口径）。 */
function uniqueNames(): number {
  return new Set(TOKENS_DECLS.filter((d) => /^--(af|p)-/.test(d.name)).map((d) => d.name)).size
}

/**
 * 文档里每一类计数的**措辞 → 量什么**登记表。
 *
 * 加一种措辞就必须在这里给它绑一个量，否则「文档里写了个数但没人量」的洞
 * 会重新长出来（这正是 G93 的成因）。绑的量拿不到就红，不许「没找到就跳过」。
 */
const COUNTERS = [
  {
    /** 文档里出现的形式，例如「（62 个变量）」「130 个定义行」「125 个唯一名」。 */
    label: '个变量',
    pattern: /（(\d+) 个变量）/g,
    /** 措辞的含义：§2 声明的 token 定义数。取最严格的量——含 `--p-*` 别名的唯一名数。 */
    meaning: '§2 一节声明的「token 变量」数（`tokens.css` 里 `--af-*` + `--p-*` 的唯一名，含别名层）',
    actual: uniqueNames,
  },
  {
    label: '个定义行',
    pattern: /(\d+) 个定义行/g,
    meaning: '`tokens.css` 里 `--af-*` + `--p-*` 的自定义属性声明条数（一行写两条算两条）',
    actual: definedLines,
  },
  {
    label: '个唯一名',
    pattern: /(\d+) 个唯一名/g,
    meaning: '同上，按名字去重后的数量（`@media` 里重复定义的档位只算一次）',
    actual: uniqueNames,
  },
] as const

interface Occurrence {
  label: string
  pattern: RegExp
  declared: number
  actual: number
  meaning: string
  /** 文档中的行号（1 起），报错时指出来。 */
  line: number
}

/** 文档里出现的**每一处**计数：一个也不许漏，一个也不许对不上。 */
function occurrencesInDoc(): Occurrence[] {
  const doc = readFileSync(DESIGN_DOC, 'utf-8')
  const lineOf = (offset: number): number => doc.slice(0, offset).split('\n').length
  const out: Occurrence[] = []
  for (const counter of COUNTERS) {
    // 每次重新构造正则：带 `g` 的正则在 matchAll 之间共享 lastIndex，复用会漏。
    const re = new RegExp(counter.pattern.source, 'g')
    for (const m of doc.matchAll(re)) {
      out.push({
        label: counter.label,
        pattern: counter.pattern,
        declared: Number(m[1]),
        actual: counter.actual(),
        meaning: counter.meaning,
        line: lineOf(m.index),
      })
    }
  }
  return out
}

/**
 * **计数区域**（§2 开头到 §3 之前）：这里出现的每一个数字都必须长在
 * 已登记的措辞上，或者被明确标成不受门禁管的「文件:行锚点」。
 *
 * 为什么要按**区域**而不是按行匹配：G93 的成因不是「某个措辞写错了」，而是
 * 「数字被随手写进句子、没有任何机制管它」。只盯「N + 措辞」的话，改个措辞
 * 或整句删掉就能让数字脱管（两种都实测判绿过：`（88 个定义行 / 85 个唯一名）`
 * → `（定义行 88 / 唯一名 85）`、以及整段删掉）。所以判据反过来立：
 * **这片区域里的裸数字默认有罪**，要留它就得把它套进一个被校验的措辞。
 */
const COUNTER_REGION = /## 2\. 设计 token[\s\S]*?## 3\./

/**
 * 文档里**必须存在**的那几处计数——按所在的**小节**钉，而不是只钉「文档里至少出现一次」。
 *
 * 为什么不够：只要求「全文出现过一次」，那么把某一处的整句删掉，另一处的同措辞
 * 还留着，门禁照样绿（实测：删掉 §9 实现索引里那一格的 `（88 个定义行 / 85 个唯一名）`
 * 就判绿了）。数字被删掉，和数字被写错一样，都是「读者拿不到真值」。
 *
 * 所以每一处都登记：**哪一节 + 哪句措辞**。§2 开头的括注是读者的第一站，§9 那格
 * 是「规范条目 → 生产文件」索引，两处都必须有；§2 引用块里的口径说明同理。
 */
const REQUIRED_IN_SECTION = [
  { section: '## 2. 设计 token', label: '个变量', why: '§2 开头那行是读者第一站，必须给出变量数' },
  { section: '## 2. 设计 token', label: '个定义行', why: '§2 必须给出「定义行」这个量' },
  { section: '## 2. 设计 token', label: '个唯一名', why: '§2 必须给出「唯一名」这个量' },
  { section: '## 9.', label: '个定义行', why: '§9 实现索引里的那一格是「规范条目 → 文件」的落点' },
  { section: '## 9.', label: '个唯一名', why: '同上' },
] as const

/** 允许留在计数区域里的数字：`§2.1` 节号、`v0.8.1` 版本号、`tokens.css:207` 这类锚点、日期。 */
function stripAllowedNumbers(line: string): string {
  return line
    .replace(/§\s*\d+(?:\.\d+)*/g, ' ')
    .replace(/v\d+(?:\.\d+)*/g, ' ')
    .replace(/[\w./-]+\.(?:css|ts|vue|md):\d+(?:-\d+)?/g, ' ') // file:line 锚点
    .replace(/\d{4}-\d{1,2}-\d{1,2}/g, ' ') // 日期
    .replace(/[~约]\d+(?:[.,]\d+)*\s*处/g, ' ') // 「~300 处」这类概数，不是计数
    .replace(/--[a-zA-Z0-9_-]+/g, ' ')
}

/**
 * 计数区域里**允许出现**的「N + 量词」字样——白名单，不是黑名单。
 *
 * 为什么从「按措辞认领」换成「按字样发白名单」：按措辞认领是**看句形**判断的，
 * 只要改一个字就能让一个数字脱管，而它脱管之后**读起来仍然像个正经计数**。
 * 实测绕过（当时判绿，是真漏洞）：
 *
 *   「另有一份试验稿是 90 个变量。」   ← `个` + `变量`，量词对得上，但数没被量过
 *
 * 认领式判据只问「这个数字后面跟着已登记的量词吗」，不问「这个数对不对」，
 * 所以任何**多出来的同款说法**都能混进去。改成白名单后，判据变成
 * **「这个数字是否等于某个已登记量的实测值」**——句形怎么变都无所谓，
 * 数字本身才是被校验的东西。白名单由 `COUNTERS` 现算，文档一改它自动跟上。
 *
 * 量词取 `变量`/`定义行`/`唯一名`/`颜色变量`：前者是 `COUNTERS` 的措辞，
 * 后者是主题文件那句「各 74 个 `--af-*` 颜色变量」（它有自己的用例在管，
 * 这里只负责不让它的数字脱管——所以数目也一并登记在 `ANCHORED_COUNTS`）。
 */
const ANCHORED_COUNTS: Array<{ value: () => number; why: string }> = [
  { value: uniqueNames, why: '含别名层的唯一名（「（N 个变量）」那处）' },
  { value: definedLines, why: '声明条数（「N 个定义行」）' },
  // 主题文件每份的 `--af-*` 颜色变量数：四份同值，由 theme-css 那套用例逐个文件核。
  { value: themeColorVariableCount, why: '主题文件每份的 `--af-*` 颜色变量数' },
  // 原型每份主题的「变量组」数（定义行）与展开后的声明条数。§2 引用块用这一对
  // 说明「原型源码写得挤，一行塞好几条」，是**可以复现的事实**，不是修辞。
  { value: () => protoThemeDeclLines(), why: '原型每份主题文件的变量组数（定义行）' },
  { value: () => protoThemeDeclCount(), why: '原型每份主题文件展开后的声明条数' },
]

/** 主题文件之一（四份同构，取一份即可算「每份」的数）。 */
const THEME_NEUTRAL = resolve(REPO_ROOT, 'packages/web/src/styles/theme-neutral.css')

/** 一份主题文件里 `--af-*` 的**唯一名**数（四份各自计数，不跨文件相加）。 */
function themeColorVariableCount(): number {
  return new Set(
    customPropertyDeclarations(readFileSync(THEME_NEUTRAL, 'utf-8'))
      .map((d) => d.name)
      .filter((n) => /^--af-/.test(n)),
  ).size
}

/** 原型主题文件之一（四份同构；§2 引用块拿它说明「一行塞好几条声明」）。 */
const PROTO_THEME = resolve(REPO_ROOT, 'prototype/styles/theme-neutral.css')

/** 原型主题文件里 `--*` 的**声明行**数（「38 个变量组」的口径：一行算一组）。 */
function protoThemeDeclLines(): number {
  const css = stripCssComments(readFileSync(PROTO_THEME, 'utf-8'))
  return css.split('\n').filter((l) => /^\s*--[a-zA-Z0-9_-]+\s*:/.test(l)).length
}

/** 同上，展开成**单条声明**后的条数（一行里塞好几条时比行数多）。 */
function protoThemeDeclCount(): number {
  const css = stripCssComments(readFileSync(PROTO_THEME, 'utf-8'))
  return [...css.matchAll(/(?:^|[;{])\s*(--[a-zA-Z0-9_-]+)\s*:/g)].length
}

/**
 * 计数区域里**没有校验**的数字。返回 `file:line + 片段`，空数组表示全都被管着。
 *
 * 判据只认两件事：
 *   1. `N + 量词` 里的 N 必须**等于**某个登记量的实测值（见 `ANCHORED_COUNTS`）；
 *   2. 反引号里的东西（token 名、示例取值、命令）不算散文里的数字。
 * 其余**任何**裸数字都判红。宁可多报（例如 §2.1 里的对比度比 `9.27:1`——
 * 那是 `9.27:1` 里的数，命中时改写法绕开即可），也不放过一个没人量的数。
 */
function unverifiedNumbersInCounterRegion(): string[] {
  const doc = readFileSync(DESIGN_DOC, 'utf-8')
  const region = COUNTER_REGION.exec(doc)
  expect(region, 'design-system.md §2 的标题不见了（计数区域扫不到）').not.toBeNull()
  const matched = region as RegExpExecArray
  const body = matched[0]
  const firstLine = doc.slice(0, matched.index).split('\n').length
  const allowed = new Set(ANCHORED_COUNTS.map((a) => a.value()))
  const out: string[] = []
  body.split('\n').forEach((raw, i) => {
    // 反引号里的东西不是散文里的计数：token 名、示例取值、`grep -c` 命令都在这里。
    let line = raw.replace(/`[^`]*`/g, ' ')
    line = stripAllowedNumbers(line)
    /* 找「整数 + 个 + 量词」的候选。⚠️ 量词前必须允许空白（`74 个 颜色变量`
       这种断行/断句写法），否则正则根本匹配不上——本用例的第一版就漏在这里：
       `个` 后面紧跟 `([\u4e00-\u9fff]{1,4})` 而中间有空格时整条匹配失败，
       于是「校验」什么都没校验到，还照样判绿。 */
    const bad: string[] = []
    for (const m of line.matchAll(/(?<![\d.])(\d+)\s*个\s*([\u4e00-\u9fff]{1,4})/g)) {
      /* 白名单是**值**不是**措辞**：`85 个变量` 放行是因为 85 被量过，
         `90 个变量` 判红是因为 90 没被量过——句形一模一样，结果相反，
         这正是要的：脱管与否不看怎么写，看数对不对。 */
      if (allowed.has(Number(m[1]))) continue
      bad.push(`${m[1]} 个 ${m[2]}`)
    }
    if (bad.length) {
      out.push(
        `design-system.md:${firstLine + i} 有 ${bad.length} 个没被任何断言校验的数字：${bad.join(' / ')} —「${raw.trim().slice(0, 60)}」（登记量只有 ${[...allowed].join(' / ')}）`,
      )
    }
  })
  return out
}

describe('tokens.css 的计数与设计文档逐处一致', () => {
  it('文档里每一处「（N 个变量）」/「N 个定义行」/「N 个唯一名」都等于它自己措辞所指的实测值', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    // 每一种措辞都必须在文档里真的出现过至少一次：措辞被改掉（例如把
    // 「N 个定义行」改写成「N 行定义」）就等于绕开了这条门禁，这里直接判红。
    for (const counter of COUNTERS) {
      const re = new RegExp(counter.pattern.source, 'g')
      expect(
        [...doc.matchAll(re)].length,
        `design-system.md 里一处「${counter.label}」都没有了——措辞被改写就等于绕开门禁`,
      ).toBeGreaterThan(0)
    }

    // 逐处比对。一条断言覆盖全部出现处，失败时把**每一处**不符的行号、措辞、
    // 文档值、实测值一起列出来：修的人不需要自己去猜是哪一个数字错了。
    const occ = occurrencesInDoc()
    const wrong = occ
      .filter((o) => o.declared !== o.actual)
      .map((o) => `design-system.md:${o.line}「${o.declared} ${o.label}」≠ 实测 ${o.actual}（${o.meaning}）`)
    expect(wrong, `文档的 token 计数与实测不符：\n  ${wrong.join('\n  ')}`).toEqual([])

    // 反向担保：上面那条 `wrong` 列表在「一处都没扫到」时也会是空的，
    // 所以这里钉住「真的扫到了一批」，且实测值不是 0。
    expect(occ.length, 'design-system.md 里一处 token 计数都没扫到').toBeGreaterThanOrEqual(3)
    expect(uniqueNames(), 'tokens.css 里一个 --af-* / --p-* 都没数到').toBeGreaterThan(50)

    for (const counter of COUNTERS) {
      expect(
        occ.filter((o) => o.label === counter.label).length,
        `design-system.md 里「${counter.label}」一种都没了——整句删掉同样是让数字脱管`,
      ).toBeGreaterThan(0)
    }

    // 逐小节钉：某处整句被删掉时，别处的同措辞不该替它顶包。
    for (const req of REQUIRED_IN_SECTION) {
      const start = doc.indexOf(req.section)
      expect(start, `design-system.md 的「${req.section}」小节不见了`).toBeGreaterThanOrEqual(0)
      const nextHeading = doc.indexOf('\n## ', start + req.section.length)
      const section = doc.slice(start, nextHeading < 0 ? undefined : nextHeading)
      expect(
        section.includes(req.label),
        `「${req.section}」小节里一处「${req.label}」都没有了（${req.why}）——删掉计数与写错计数一样，读者都拿不到真值`,
      ).toBe(true)
    }
  })

  it('§2 里的每一个数字要么长在已登记的措辞上，要么根本不该出现', () => {
    // 这一条守的是**「数字脱管」本身**，而不只是「数字写错」：只比对上一条的话，
    // 把整句删掉、或把措辞改写（`（88 个定义行 / 85 个唯一名）` → `（定义行 88 /
    // 唯一名 85）`）都能让数字溜走——两种都实测判绿过。
    const unverified = unverifiedNumbersInCounterRegion()
    expect(
      unverified,
      `§2 里有没被任何断言管着的数字（要么套上「N 个定义行」/「N 个唯一名」/「（N 个变量）」，要么删掉）：\n  ${unverified.join('\n  ')}`,
    ).toEqual([])
  })

  it('「定义行」与「唯一名」是两个不同的量，且差值就是 @media 里的重复档', () => {
    // 文档要同时说这两个数，就必须真的存在差异，否则「两个措辞一个量」是把读者
    // 当傻子。差值必须能被 `tokens.css` 里的重复定义解释清楚。
    const names = TOKENS_DECLS.map((d) => d.name)
    const dup = names.filter((n, i) => names.indexOf(n) !== i)
    expect(new Set(dup).size, 'tokens.css 里没有重复定义的档位，「定义行」与「唯一名」会相等').toBeGreaterThan(0)
    expect(definedLines() - uniqueNames(), '定义行 − 唯一名 ≠ 重复声明条数').toBe(dup.length)
  })
})
