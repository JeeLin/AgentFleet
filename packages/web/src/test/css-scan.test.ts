/* ===========================================================================
   `css-scan.ts` 的自测（v0.8.1 第4轮）
   ---------------------------------------------------------------------------
   这些夹具直接来自一次**变异实测**：`ui-scale.test.ts` 初版的两条守卫按行跑
   正则，下面每种书写形态都实测漏网（加回规则后 18/18 仍判绿）。把每种形态
   钉成一条用例，守卫才不会在下次「顺手改写」时又悄悄失去覆盖。
   =========================================================================== */
import { describe, expect, it } from 'vitest'

import {
  classNamesIn,
  classNamesInRule,
  declarationsOf,
  parseCssRules,
  propValue,
  ruleFor,
  stripCssAndLineComments,
} from '@/test/css-scan'

describe('parseCssRules / classNamesIn', () => {
  it('单行规则', () => {
    expect([...classNamesIn('.af-a { color: red; }')]).toEqual(['.af-a'])
  })

  it('多行规则（. 与 { 不在同一行）', () => {
    expect([...classNamesIn('.af-a\n{ color: red; }')]).toEqual(['.af-a'])
  })

  it('逗号组：每个类都算', () => {
    expect([...classNamesIn('.af-a, .af-b { color: red; }')].sort()).toEqual(['.af-a', '.af-b'])
  })

  it('@media 内缩进的规则同样扫得到', () => {
    const css = '@media (max-width: 900px) {\n  .af-a { color: red; }\n}'
    expect([...classNamesIn(css)]).toEqual(['.af-a'])
  })

  it('嵌套 @media 的 at-rule 链被记录', () => {
    const css = '@media (max-width: 900px) { .af-a { color: red; } }'
    const [rule] = parseCssRules(css)
    expect(rule.atRules).toEqual(['@media (max-width: 900px)'])
    expect(rule.selector).toBe('.af-a')
  })

  it('伪类 / 属性选择器算同一个类（wholeSubject）', () => {
    for (const sel of ['.af-a:hover', '.af-a[data-open=\'true\']', '.af-a::before']) {
      const css = `${sel} { color: red; }`
      const rule = parseCssRules(css)[0]
      expect(classNamesInRule(rule), sel).toEqual(['.af-a'])
    }
  })

  it('后代 / 子代 / 兄弟组合**不算**「定义了这个类」', () => {
    for (const sel of ['.af-x .af-a', '.af-x > .af-a', '.af-x + .af-a', '.af-x ~ .af-a']) {
      const css = `${sel} { color: red; }`
      expect([...classNamesIn(css, { wholeSubject: true })], sel).toEqual([])
    }
  })

  it('注释里的类名不算', () => {
    const css = '/* .af-ghost { color: red; } */ .af-real { color: red; }'
    expect([...classNamesIn(css)]).toEqual(['.af-real'])
  })

  it('花括号不平衡会抛（而不是静默截断）', () => {
    expect(() => parseCssRules('.af-a { color: red;')).toThrow(/不平衡/)
  })

  it('propValue 取最后一条同名声明（CSS 后者胜）', () => {
    const [rule] = parseCssRules('.af-a { padding: 0; padding: 4px 8px; }')
    expect(propValue(rule, 'padding')).toBe('4px 8px')
  })

  it('propValue 区分「没写」与「写了 0」', () => {
    const [rule] = parseCssRules('.af-a { padding: 0; }')
    expect(propValue(rule, 'padding')).toBe('0')
    expect(propValue(rule, 'margin')).toBeUndefined()
  })

  it('ruleFor 只认选择器文本完全相等（逗号组整体）', () => {
    const rules = parseCssRules('.af-a, .af-b { color: red; } .af-a { color: blue; }')
    expect(ruleFor(rules, '.af-a, .af-b')?.body).toContain('red')
    expect(ruleFor(rules, '.af-a')?.body).toContain('blue')
  })

  /* `declarationsOf` 是 G57 实际指向的那个 export：它**没有外部消费者**，
     只被同文件的 `propValue`（:167）调用，而且原先连一条测试都没有——
     等于「零消费 + 零覆盖」，删掉它能全绿、写坏它也能全绿。
     （G57 报告里同时点名的 `stripCssAndLineComments`「全仓 0 调用」确实已不成立，
     但它**不是「只需订正记录」**——这条追述当时写的就是这个错判，已订正：
     实测它有不少调用点，而更关键的是**它本身有一个会造假的 bug**：
     它把 `<script>` 里的 `//` 当成 CSS 行注释起头，于是整行后面的内容被吃掉，
     「否定断言 + 会丢内容的扫描器 = 守卫假绿」（G83，AGENTS.md 维度 25）。
     所以当时的结论「不必改代码」是错的，代码改了。这一段留着，是为了记住
     「调用点多少」与「它算得对不对」是两件事。）

     `propValue` 只喂它**单条**规则，于是「跨多条规则收集」这条唯一的差异
     从未被走到；这里直接钉住它。 */
  it('declarationsOf 跨多条规则收集，且保留每条各自的 selector', () => {
    const rules = parseCssRules('.af-a { color: red } .af-b { color: blue; color: green }')
    const hits = declarationsOf(rules, 'color')
    expect(hits.map((h) => [h.selector, h.value])).toEqual([
      ['.af-a', 'red'],
      // 同名多次声明**两条都返回**（与 `propValue` 的「后者胜」不同）：
      // 前者要的是「哪里写过」，后者要的是「最终生效的是哪个」。
      ['.af-b', 'blue'],
      ['.af-b', 'green'],
    ])
    expect(hits.every((h) => typeof h.rule.body === 'string'), '每条结果都带原规则').toBe(true)
  })

  /* 属性名必须**从声明开头或分号之后**起算。这条是在 G72 的守卫里实测踩到
     的：`new RegExp('color:')` 会命中 `border-color:` 的尾巴，于是「主按钮
     禁用态的字色」取到了边框色，算出一个漂亮但没有意义的对比度。 */
  it('declarationsOf 不会把 `border-color` 当成 `color`', () => {
    const rules = parseCssRules('.af-a { border-color: red; color: blue }')
    expect(declarationsOf(rules, 'color').map((h) => h.value)).toEqual(['blue'])
  })
})

describe('stripCssAndLineComments：字符串里的 `//` 不是注释', () => {
  /* **这一组守的是「否定断言 + 丢内容的扫描器」这个组合**（v0.8.1 第5轮 4b
     实测）。初版逐行 `line.indexOf('//')` 之后整行截断，注释里还写着「本仓没
     有把 `//` 放在字符串里的写法」——实测这句是错的，`NewProjectPage.vue:65`
     的 `url.startsWith('http://')` 就是。

     危害不在「少剥了几个字符」，而在调用它的 6 处全是
     `expect(code.match(/fetch/) ?? []).toHaveLength(0)` 这类**否定断言**：
     截断让本该被看见的 `fetch` 隐形，守卫误判为通过。所以下面第三、四条不是
     「顺便测一下」，它们是这个 bug 的真实形态。 */

  it('`http://` 在字符串里时整行保留（生产代码里的真实写法）', () => {
    const src = "if (url.startsWith('http://') || url.startsWith('ssh://')) { ok() }"
    expect(stripCssAndLineComments(src)).toBe(src)
  })

  it('`http://` 在双引号与模板串里同样保留', () => {
    const src = 'const a = "http://hub.local"; const b = `ws://x.y`'
    expect(stripCssAndLineComments(src)).toBe(src)
  })

  it('**判红机制**：同行的 `fetch` 不许被 `http://` 截断吞掉（否定断言误判为通过）', () => {
    // 正是初版会误判为绿的形状：`//` 一截，`void fetch(url)` 就消失了。
    const src = "if (url.startsWith('http://')) { void fetch(url) }"
    expect(stripCssAndLineComments(src), '整行里没有真注释，一个字都不该少').toBe(src)
    expect(/fetch/.test(stripCssAndLineComments(src)), 'fetch 必须仍然扫得到').toBe(true)
  })

  it('转义引号不会让字符串提前收尾', () => {
    const src = "const s = 'a\\'//b'; const t = real // 注释"
    expect(stripCssAndLineComments(src).trimEnd()).toBe("const s = 'a\\'//b'; const t = real")
  })

  it('真正的行注释照旧剥掉，且保留换行（行号对得上）', () => {
    const src = 'const a = 1 // 注释里有 fetch\nconst b = 2'
    const out = stripCssAndLineComments(src)
    expect(out).not.toContain('fetch')
    expect(out.split('\n'), '行数不变').toHaveLength(2)
    expect(out.split('\n')[1]).toBe('const b = 2')
  })
})
