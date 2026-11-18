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
     （G57 报告里同时点名的 `stripCssAndLineComments`「全仓 0 调用」已不成立：
     `FailurePanel.test.ts` / `OverviewPage.test.ts` / `TaskCard.test.ts` 共 6 处在用，
     而且它存在的理由——扫整份 `.vue` 时要连 `<script>` 里的 `//` 一起剥——
     正是那几处的用法。所以那条只需订正记录，不必改代码。）

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
