/* ===========================================================================
   CSS 结构扫描助手（v0.8.1 第4轮）
   ---------------------------------------------------------------------------
   **为什么需要它**：`ui-scale.test.ts` 里两条守卫的初版都在**按行跑正则**，
   于是各自漏掉一整类真实书写形态，且漏法是静默的（变异后仍全绿）：

   1. 「两份样式表选择器无交集」初版只认 `^\.cls\s*\{`（行首单类 + 同一行开括号）。
      实测漏掉五种形态——`@media` 里缩进两格的规则、逗号组 `.a, .b { … }`、
      逗号组里**后一个**类、`.` 与 `{` 不在同一行（`.af-col\n{`）、伪类
      `.af-col:hover`。其中逗号组那两种最要命：`.af-kv__k, .af-kv dt {` 这种写法
      让一次真实的跨文件重名整轮没被看见。
   2. 「表单行盒算术」初版从**规则文本**里用字面量正则取 padding / border，
      真实取值（`height` / `padding` / `border-width`）一条没读出来。于是加
      `max-height: 16px`、把 `.af-select` 的 `line-height` 改回 `normal`、
      把边框改成 2px —— 三种「用户报的 bug 回来了」的写法全部判绿。

   本模块把这两件事都变成**结构性**的：先把注释剥干净，再按花括号配对扫出
   每个规则头（含 `@media` 等 at-rule 内的），头按逗号切分、逐个取 `.class`；
   声明体按 `;` 切分后取真实取值。这样「书写形态」不再是变量。
   =========================================================================== */

/** 去掉 `/* … *\/` 注释。字符串里的 `/*` 在 CSS 里不存在，不必特殊处理。 */
export function stripCssComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '')
}

export interface CssRule {
  /** 完整选择器文本（已去首尾空白、保留逗号），如 `.a, .b:hover`。 */
  selector: string
  /** 声明体原文（不含两侧花括号）。 */
  body: string
  /** at-rule 链，如 `['@media (max-width: 900px)']`；顶层规则为空数组。 */
  atRules: string[]
  /** 规则头在整个源串里的起始偏移（排序 / 判先后用）。 */
  offset: number
}

/**
 * 把一份 CSS 拆成规则列表：剥注释 → 按花括号配对 → 逐层下探 `@media` 等块。
 *
 * **按花括号配对而不是按行**是这里的关键：多行写法、同行的 `; }`、
 * 嵌套 at-rule 都自然被覆盖，不需要为每种形态补一条正则。
 */
export function parseCssRules(src: string): CssRule[] {
  const text = stripCssComments(src)
  const out: CssRule[] = []

  /** 从 `from` 开始，把 `{` 之前的文本当作头、配对到对应的 `}` 为止。 */
  const walk = (from: number, to: number, atRules: string[]): void => {
    let i = from
    while (i < to) {
      const open = text.indexOf('{', i)
      if (open === -1 || open >= to) return
      const head = text.slice(i, open).trim()
      // 找到与之配对的 `}`：从 `open` 起数花括号。
      let depth = 0
      let close = -1
      for (let j = open; j < to; j++) {
        if (text[j] === '{') depth++
        else if (text[j] === '}') {
          depth--
          if (depth === 0) {
            close = j
            break
          }
        }
      }
      if (close === -1) throw new Error(`CSS 花括号不平衡（偏移 ${open} 处的规则头没有闭合）`)
      if (head.startsWith('@')) {
        // at-rule：往里再下探一层，自身不算规则（`@media` / `@supports` / …）。
        walk(open + 1, close, [...atRules, head])
      } else if (head) {
        out.push({ selector: head, body: text.slice(open + 1, close), atRules, offset: i })
      }
      i = close + 1
      // 吃掉分隔下一个规则的 `;`（`@import` / `@charset` 这类无块语句）。
      while (i < to && (text[i] === ';' || text[i] === '\n' || text[i] === ' ')) i++
    }
  }

  walk(0, text.length, [])
  return out
}

/**
 * 规则头里出现的类名（带前导 `.`），已按逗号组拆开。
 *
 * 只认「整个复合选择器就是一个类」（`.af-x` / `.af-x:hover` /
 * `.af-x[data-open='true']` 这类修饰算同一类，但 `.af-drawer .af-x` 这种
 * **后代选择器**不算）。跨文件重名守卫要的是后者：`.af-drawer .af-msg` 是
 * 有意的局部覆盖，`.af-msg { … }` 与 `.af-msg { … }` 才是「后导入者静默全量取胜」。
 */
export function classNamesInRule(rule: CssRule): string[] {
  const out: string[] = []
  for (const part of rule.selector.split(',')) {
    const sel = part.trim()
    /* 复合选择器 = 单个类 + 任意数量的修饰（伪类 / 伪元素 `::before` / 属性）。
       只要中间出现空格、`>`、`+`、`~` 就说明是组合选择器，不算「定义了这个类」。 */
    const m = /^\.(-?[a-zA-Z_][a-zA-Z0-9_-]*)((?::?:?[a-zA-Z-]+(?:\([^)]*\))?|\[[^\]]*\])*)$/.exec(sel)
    if (m) out.push(`.${m[1]}`)
  }
  return out
}

/**
 * 一份样式表里出现的全部类名。`wholeSubject` 语义同 {@link classNamesInRule}；
 * 默认 `false` 时连后代选择器里的类也算（查「这个类有没有被定义过」时用）。
 */
export function classNamesIn(css: string, opts: { wholeSubject?: boolean } = {}): Set<string> {
  const out = new Set<string>()
  for (const rule of parseCssRules(css)) {
    if (opts.wholeSubject) {
      for (const cls of classNamesInRule(rule)) out.add(cls)
    } else {
      for (const part of rule.selector.split(',')) {
        for (const cls of part.matchAll(/\.(-?[a-zA-Z_][a-zA-Z0-9_-]*)/g)) out.add(`.${cls[1]}`)
      }
    }
  }
  return out
}

/**
 * 所有规则里**声明过**某属性的条目，形如 `{selector, value}`。
 *
 * `value` 是给到的属性名（含 `-` 前缀族）的原值文本，不做单位换算——
 * 换算交给调用方的 token 解析，因为 token 可能是 `var()` / `calc()`。
 */
export function declarationsOf(
  rules: CssRule[],
  property: string,
): Array<{ selector: string; value: string; rule: CssRule }> {
  const re = new RegExp(`(?:^|;)\\s*${property.replace(/[-]/g, '\\-')}\\s*:\\s*([^;]+)`, 'g')
  const out: Array<{ selector: string; value: string; rule: CssRule }> = []
  for (const rule of rules) {
    for (const m of rule.body.matchAll(re)) out.push({ selector: rule.selector, value: m[1].trim(), rule })
  }
  return out
}

/**
 * 一条规则的声明体里某属性的值（同名多次取**最后一条**——CSS 里后者胜）。
 * 找不到返回 `undefined`，刻意不返回空串：调用方必须区分「没写」与「写了空值」。
 */
export function propValue(rule: CssRule, property: string): string | undefined {
  const hits = declarationsOf([rule], property).map((d) => d.value)
  return hits.length ? hits[hits.length - 1] : undefined
}

/** 找到选择器**文本完全等于** `selector` 的那条规则（`selector` 可含逗号组）。 */
export function ruleFor(rules: CssRule[], selector: string): CssRule | undefined {
  return rules.find((r) => r.selector.replace(/\s+/g, ' ').trim() === selector)
}
