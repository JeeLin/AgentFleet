/* ===========================================================================
   四套主题的 CSS 逐值对照原型
   ---------------------------------------------------------------------------
   原型配色真相源是 `prototype/styles/theme-*.css` 四份文件，**不是**
   `prototype/assets/tokens.css`（那个文件零个颜色变量）。本用例把两边的
   `--p-*` 与 `--*` 变量逐一比对，防止「搬的时候手一抖少一位十六进制」。

   为什么值得这么细：配色错了不会报错、不会崩，只是「看着不像」。上一轮就是
   漏了这一层，页面底是 `#080a0d` 而原型是 `#191d21`，四条关键色全不一致。
   =========================================================================== */
import { describe, expect, it } from 'vitest'

import { readRepoFile } from '@/test/css'

const THEMES = ['neutral', 'warm', 'paper', 'cream'] as const

/** 取一个 CSS 文件里 `--name: value;` 的映射（覆盖多值写法：一行里多条声明）。 */
function vars(css: string): Map<string, string> {
  const out = new Map<string, string>()
  // 去掉注释，否则注释里的 `--bg: #fff` 之类会被当成定义
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, '')
  for (const m of stripped.matchAll(/(--[a-z0-9-]+)\s*:\s*([^;}]+)/g)) {
    out.set(m[1], m[2].trim())
  }
  return out
}

/** 原型名 → 实现 `--p-*` 名的对应。含改名的那几个（scrim/inset/add/del/dot-*）。 */
const NAME_MAP: Record<string, string> = {
  '--inset': '--p-inset-c',
  '--warn': '--p-warn',
  '--add': '--p-add-ink-c',
  '--del': '--p-del-ink-c',
  '--dot-a': '--p-dot-a',
  '--dot-b': '--p-dot-b',
  '--dot-c': '--p-dot-c',
}

function implName(proto: string): string {
  return NAME_MAP[proto] ?? `--p-${proto.slice(2)}`
}

/** 归一空白：`rgba(8,10,13,0.62)` 与 `rgba(8, 10, 13, 0.62)` 视为同值。 */
function norm(v: string): string {
  return v.replace(/\s+/g, '').toLowerCase()
}

/**
 * 归一 `var()` 引用里的变量名。
 *
 * 原型写 `--dot-a: var(--acc)`，实现必须写 `var(--p-acc)`——因为实现里不存在
 * 不带前缀的 `--acc`，照抄原型会让这三个变量指向空值（`--dot-*` 直接失效）。
 * 所以「引用名不同」不是搬错了，而是**必须的改名**；测试要认这一点，否则
 * 只有两条路：要么把原型写法照抄成坏代码，要么删掉这三条断言。
 */
function normVarRefs(v: string): string {
  return norm(v).replace(/var\(--([a-z0-9-]+)\)/g, (_m, name: string) => `var(${implName(`--${name}`)})`)
}

describe('四套主题逐值对照原型', () => {
  for (const theme of THEMES) {
    it(`theme-${theme}：每个原型颜色变量都被搬过来且值一致`, () => {
      const proto = vars(readRepoFile(`prototype/styles/theme-${theme}.css`))
      const impl = vars(readRepoFile(`packages/web/src/styles/theme-${theme}.css`))

      // 原型这一份里真正定义颜色的变量。**不硬编码个数**：原型里一行可能写
      // 多条声明（`--pending: #a2a9b2; --pending-bg: ...; --pending-line: ...`），
      // 展开后是 54 条而不是文件头注释说的「38 个变量组」。用组数当断言只会
      // 逼着后来人改数字，改成「两边集合必须对齐」才真的守住一致性。
      const colours = [...proto.keys()].filter((k) => k.startsWith('--'))
      expect(colours.length, `theme-${theme} 原型文件没有颜色变量，路径或解析有问题`).toBeGreaterThan(0)

      const missing: string[] = []
      const differing: string[] = []
      for (const p of colours) {
        const i = implName(p)
        if (!impl.has(i)) {
          missing.push(`${p} → ${i}`)
          continue
        }
        // --dot-* 在原型里是 var(--acc) 之类，实现是 var(--p-acc)——同一回事。
        // **只对原型侧做改名归一**：实现侧已经是 `--p-*`，再走一遍 implName
        // 会变成 `--p-p-acc`（这个 bug 被本测试自己抓出来过）。
        const a = normVarRefs(proto.get(p)!)
        const b = norm(impl.get(i)!)
        if (a !== b) differing.push(`${p} → ${i}：原型 ${a} / 实现 ${b}`)
      }
      expect(missing, `theme-${theme} 漏搬的变量：\n${missing.join('\n')}`).toEqual([])
      expect(differing, `theme-${theme} 取值不一致：\n${differing.join('\n')}`).toEqual([])
    })
  }

  it('四份实现的变量名集合完全一致（可切换的前提）', () => {
    const sets = THEMES.map((t) => [...vars(readRepoFile(`packages/web/src/styles/theme-${t}.css`)).keys()].sort())
    for (let i = 1; i < sets.length; i++) {
      expect(sets[i], `${THEMES[i]} 与 ${THEMES[0]} 的变量名集合不同`).toEqual(sets[0])
    }
  })

  it('默认主题 neutral 的页面底就是原型那个值，不是上一轮自造的那个', () => {
    const impl = vars(readRepoFile('packages/web/src/styles/theme-neutral.css'))
    expect(norm(impl.get('--p-bg')!)).toBe('#191d21')
    // 上一轮自造的 #080a0d 必须已不存在（它是「看着不像」的根因之一）
    expect([...impl.values()].map(norm)).not.toContain('#080a0d')
  })

  it('四份主题都定义了强调色，且四份各不相同（否则切换看不出差别）', () => {
    const accs = THEMES.map((t) => norm(vars(readRepoFile(`packages/web/src/styles/theme-${t}.css`)).get('--p-acc')!))
    expect(new Set(accs).size).toBe(4)
  })
})
