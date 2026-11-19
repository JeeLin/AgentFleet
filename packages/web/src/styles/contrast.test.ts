/* ===========================================================================
   对比度守卫：四套主题 × 三个背景层 × 三级文字（v0.8.1 第5轮 G71/G72/G73）
   ---------------------------------------------------------------------------
   为什么需要这个文件 —— 这一轮关掉的三条 🔴 全是「注释自称达标、实测不达标」：

     · G71 `--p-fg-3` 在深色面（warm / neutral）上系统性不达标。注释写的是
       「4.81:1 on 页头底」——那只是**页面底**这一个层，而 fg-3 还大量用在
       surf-1 / surf-2 / surf-3（看板列、卡片、卡片悬停）上，那三层更亮，
       实测 4.44 / 4.06 / 3.68（warm）与 4.37 / 3.98 / 3.56（neutral）。
       一条注释只报一个背景层的数字，就等于把另外三层藏起来了。
     · G72 主按钮禁用态 `opacity: 0.5` 叠在强调色实底上，四套全部 ~1.1:1。
     · G73 paper / cream 的 fg-2 与 fg-3 只差 1.03× / 1.06×——三级文字梯子
       在这两套主题里**实际只有两级**。

   本文件的判据：**自己算**，不信注释。注释里的数字只能当线索，不能当证据。
   同一段注释在三个文件里各写一个「4.79:1」，抄错一次就三处一起错。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import { parseCssRules } from '@/test/css-scan'

const STYLES = resolve(dirname(fileURLToPath(import.meta.url)))

/* ── 颜色与对比度的最小实现 ─────────────────────────────────────────────
   只支持这个项目里用到的两种写法：`#rrggbb` 与 `rgba(r, g, b, a)`。
   故意**不支持**命名色与 `hsl()`：多支持一种写法，就多一条「守卫算出来的
   颜色和浏览器算出来的不是同一个」的可能。主题文件里没有它们。 */

type Rgb = [number, number, number]

function parseColor(raw: string): Rgb {
  const text = raw.trim()
  const hex = /^#([0-9a-fA-F]{6})$/.exec(text)
  if (hex) {
    const n = hex[1]
    return [parseInt(n.slice(0, 2), 16), parseInt(n.slice(2, 4), 16), parseInt(n.slice(4, 6), 16)]
  }
  const rgba = /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*(?:,\s*([\d.]+)\s*)?\)$/.exec(text)
  if (rgba) return [Number(rgba[1]), Number(rgba[2]), Number(rgba[3])]
  throw new Error(`parseColor 不认识的写法：\`${text}\`——新写法要先扩这个函数，别绕过它`)
}

/** WCAG 相对亮度。 */
function luminance([r, g, b]: Rgb): number {
  const f = (c: number): number => {
    const s = c / 255
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
  }
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/** WCAG 对比度。参数顺序无关（内部取亮/暗）。 */
function contrast(a: Rgb, b: Rgb): number {
  const la = luminance(a)
  const lb = luminance(b)
  const hi = Math.max(la, lb)
  const lo = Math.min(la, lb)
  return (hi + 0.05) / (lo + 0.05)
}

/** 把半透明色叠到不透明底上。 */
function over(fg: Rgb, alpha: number, bg: Rgb): Rgb {
  return [0, 1, 2].map((i) => Math.round(fg[i] * alpha + bg[i] * (1 - alpha))) as Rgb
}

/* ── 读主题 ───────────────────────────────────────────────────────────── */

const THEMES = ['warm', 'neutral', 'paper', 'cream'] as const
type ThemeName = (typeof THEMES)[number]

/** 一份主题里所有 `--p-*` 的字面值。只收字面色值，`var(--x)` 引用的跳过。 */
function themeTokens(name: ThemeName): Map<string, string> {
  const src = readFileSync(resolve(STYLES, `theme-${name}.css`), 'utf-8')
  const out = new Map<string, string>()
  for (const rule of parseCssRules(src)) {
    for (const m of rule.body.matchAll(/(?:^|;)\s*(--p-[a-zA-Z0-9-]+)\s*:\s*([^;]+)/g)) {
      const value = m[2].trim()
      // `var(--x)` 与 `color-mix()` 这类派生值不在这里解析：要验的层级都写字面值。
      if (value.startsWith('#') || value.startsWith('rgb')) out.set(m[1], value)
    }
  }
  return out
}

const TOKENS = Object.fromEntries(THEMES.map((t) => [t, themeTokens(t)])) as Record<
  ThemeName,
  Map<string, string>
>

function token(name: ThemeName, key: string): Rgb {
  const raw = TOKENS[name].get(key)
  if (raw === undefined) throw new Error(`主题 ${name} 没有定义 ${key}（或它不是字面色值）`)
  return parseColor(raw)
}

/* ── 背景层 ───────────────────────────────────────────────────────────────
   文字不只在页面底上出现。`surf-1` 是区域容器（看板列 / 分组）、`surf-2` 是
   卡片、`surf-3` 是卡片悬停——三层都比页面底**更靠近文字色**，所以页面底
   达标不代表它们达标。G71 的成因正是只量了页面底那一层。 */
const SURFACES = ['--p-bg', '--p-surf-1', '--p-surf-2', '--p-surf-3'] as const

const AA = 4.5 // 正文级（<18.66px 常规字重）的 WCAG AA 阈值

describe('对比度守卫', () => {
  describe('G71 — 三级文字在每一层背景上都达 AA', () => {
    for (const name of THEMES) {
      it(`${name}：--p-fg-3 在 bg / surf-1 / surf-2 / surf-3 上均 ≥ ${AA}:1`, () => {
        const fg3 = token(name, '--p-fg-3')
        const failures: string[] = []
        for (const surf of SURFACES) {
          const ratio = contrast(fg3, token(name, surf))
          if (ratio < AA) {
            failures.push(`${surf} = ${ratio.toFixed(2)}:1`)
          }
        }
        expect(
          failures,
          `主题 ${name} 的 --p-fg-3（${TOKENS[name].get('--p-fg-3')}）在这些层上不达 AA ${AA}:1：`
            + `${failures.join(' / ')}。`
            + '三级文字用在页头副标题、表头、字段提示上——它们不总在页面底上，'
            + '看板列（surf-1）、卡片（surf-2）、卡片悬停（surf-3）上都出现。'
            + '只量页面底会漏掉更亮的那三层（G71 的成因）。',
        ).toEqual([])
      })
    }
  })

  describe('G73 — 四套主题的 fg-2 必须明确强于 fg-3', () => {
    /* 为什么要问「某个背景上的对比度比值」而不是直接比两个颜色的亮度：
       有意义的是**用户看到的**级差——同样两个色，换一个底色级差就变了。

       **这个阈值是在四张面 `SURFACES` 上取最小**，不是只在 `--p-bg` 上量：
       下面循环对 `--p-bg` / `--p-surf-1` / `--p-surf-2` / `--p-surf-3` 各算一次
       `contrast(fg-2, surf) / contrast(fg-3, surf)`，逐张都要 ≥ 阈值。
       （原措辞写成「同一层背景上」，容易读成「只对 `--p-bg` 求值」——
       那是另一种机制，虽然这条的结论碰巧不变。）

       1.25× 取的是四套主题修好后的**实测最低值**，向下取整留一点浮动余量；
       它同时能拦下修复前的状态（paper 1.09× / cream 1.12×）。
       具体每套主题的当前实测值不写死在这里——写死就会过期，
       让失败消息自己报（`ramp.toFixed(2)` 已带上）。 */
    const MIN_RAMP = 1.25

    for (const name of THEMES) {
      it(`${name}：fg-2 在同一层背景上至少比 fg-3 强 ${MIN_RAMP}×`, () => {
        const fg2 = token(name, '--p-fg-2')
        const fg3 = token(name, '--p-fg-3')
        const failures: string[] = []
        for (const surf of SURFACES) {
          const bg = token(name, surf)
          const r2 = contrast(fg2, bg)
          const r3 = contrast(fg3, bg)
          const ramp = r2 / r3
          if (ramp < MIN_RAMP) {
            failures.push(`${surf}: fg-2 ${r2.toFixed(2)} / fg-3 ${r3.toFixed(2)} = ${ramp.toFixed(2)}×`)
          }
        }
        expect(
          failures,
          `主题 ${name} 的文字梯子在 ${MIN_RAMP}× 级差下塌成两级：${failures.join(' / ')}。`
            + 'fg-2（次要）与 fg-3（三级）在视觉上必须分得出来，否则文档里写的'
            + '「三级文字梯子」在这套主题里实际只有两级（G73）。',
        ).toEqual([])
      })
    }
  })

  describe('G72 — 主按钮禁用态的文字可读', () => {
    /* 这条守卫的判据**不是**「有没有写 opacity: 0.5」，而是「算出来的对比度
       达不达标」——因为真正伤害用户的是看不见字，不是用了哪个属性。

       算的是**浏览器最终画出来的颜色**：
         · 禁用主按钮的底与字（`components.css` 里 `.af-btn--primary:disabled`）
         · 如果那两行没写，就继承 `.af-btn:disabled` 的 `opacity`

       `opacity` 会让**整元素**（底 + 字）一起与页面底混合——两个颜色同时
       向背景靠，对比度按比例折半。所以这里必须先叠 opacity，再算对比度；
       直接拿两个源色算，会得到一个现实中不存在的漂亮数字。 */
    const COMPONENTS = resolve(STYLES, 'components.css')

    function disabledPrimaryColors(): { bg: string; fg: string; opacity: number } {
      const src = readFileSync(COMPONENTS, 'utf-8')
      /* 剥掉注释再解析：这段规则的**注释里**就写着 `.af-btn--primary:disabled`
         这个选择器（解释 G72 的由来），不剥的话 `parseCssRules` 会把它当成
         一条真规则的规则头，取到一段没有声明的空体——于是断言「规则存在」
         通过、取颜色却拿到 undefined。 */
      const rules = parseCssRules(src)

      /* 按「选择器里同时含 `.af-btn--primary` 与 `:disabled`」找，不按全文
         精确匹配：多行选择器、逗号组的换行与缩进都可能变，钉死文本的守卫
         会在有人重排格式时误报，而它的本意是守**语义**（主按钮有专用禁用态）。 */
      const primary = rules.find(
        (r) => r.selector.includes('.af-btn--primary') && r.selector.includes(':disabled'),
      )

      const base = rules.find(
        (r) =>
          r.selector.includes('.af-btn:disabled')
          && !r.selector.includes('.af-btn--primary'),
      )
      expect(base, 'components.css 缺少 `.af-btn:disabled` 通用规则').toBeDefined()

      expect(
        primary,
        'components.css 缺少主按钮禁用态的专用规则（`.af-btn--primary:disabled`）。'
          + '只靠通用的 `.af-btn:disabled`（opacity + fg-3）会让主按钮的彩色实底与'
          + '灰字一起淡化到 ~1.1:1，字等于看不见。',
      ).toBeDefined()

      const baseOpacity = /opacity:\s*([\d.]+)/.exec(base!.body)?.[1]

      const pick = (prop: string): string => {
        /* 属性名要**从声明开头或分号之后**起算，否则 `color:` 会匹配到
           `border-color:` 的尾巴上——那样守卫量的是边框色，算出一个漂亮
           但毫无意义的数字（开发时实测踩过：报错信息里写「字 var(--p-line-3)」）。 */
        const v = new RegExp(`(?:^|;)\\s*${prop}:\\s*([^;]+)`).exec(primary!.body)?.[1]?.trim()
        if (v) return v
        const fb = new RegExp(`(?:^|;)\\s*${prop}:\\s*([^;]+)`).exec(base!.body)?.[1]?.trim()
        expect(fb, `主按钮禁用态没写 ${prop}，通用规则也没有可继承的值`).toBeDefined()
        return fb!
      }

      const opacity = Number(
        new RegExp('opacity:\\s*([\\d.]+)').exec(primary!.body)?.[1] ?? baseOpacity ?? '1',
      )
      return {
        bg: pick('background'),
        fg: pick('color'),
        opacity,
      }
    }

    /** 把 `var(--p-x)` 换成主题里的字面色值（只解一层）。 */
    function resolveVar(name: ThemeName, raw: string): Rgb {
      const m = /^var\((--p-[a-zA-Z0-9-]+)\)$/.exec(raw.trim())
      if (m) return token(name, m[1])
      return parseColor(raw)
    }

    for (const name of THEMES) {
      it(`${name}：禁用主按钮的文字 ≥ ${AA}:1（已计入 opacity 淡化）`, () => {
        const { bg, fg, opacity } = disabledPrimaryColors()
        const page = token(name, '--p-bg')
        const bgRgb = resolveVar(name, bg)
        const fgRgb = resolveVar(name, fg)
        // 浏览器实际画出来的：整个元素以 `opacity` 叠到页面底上。
        const paintedBg = over(bgRgb, opacity, page)
        const paintedFg = over(fgRgb, opacity, page)
        const ratio = contrast(paintedFg, paintedBg)
        expect(
          ratio,
          `主题 ${name} 的禁用主按钮文字只有 ${ratio.toFixed(2)}:1（底 ${bg}、字 ${fg}、`
            + `opacity ${opacity}）。`
            + '主按钮禁用态不是边缘情况：表单没填完时它就是禁用的，用户填表全程看着它。'
            + '靠调 opacity 的数值救不回来——半透明会让底与字同时向背景靠，'
            + '对比度按比例折半，实测即使换成 fg-strong 也只能到 3.2–3.9:1（G72）。',
        ).toBeGreaterThanOrEqual(AA)
      })
    }
  })

  /* G75 的通用化（第8轮 4d 实测 `.af-badge--warn` 在 paper 4.36 / cream 4.25）。
     ---------------------------------------------------------------------------
     这一类缺陷的形状是固定的：**语义色的前景压在它自己那层半透明底上**。
     `.af-badge--warn` / `.af-badge--ok` / `.af-tag--queued` / `.af-callout--*`
     都是 `color: var(--本文档-fg)` + `background: var(--本文档-bg)`，而 `-bg`
     是 `rgba(同一个色, 0.10~0.15)`。

     两个容易漏的点，这条判据都堵上：
       ① 直接用 `rgba(...)` 算对比度会得出虚高读数——半透明底必须先按 alpha
          合成到它下面的实体层（这里是 `--p-surf-1`，badge/tag 最常落的地方）上。
       ② 只量页面底不够：badge 很少直接坐在 `--p-bg` 上，它坐在卡片 / 阶段条里。

     判据只对「有 `-bg` 兄弟变量」的语义色生效，且要求前景与底同源（同色相）。
     前景加深不该影响 `-line`（描边不是文字，AA 不管它）。

     **配色对不手写，从主题文件里现发现**（第9轮 4c 实测抓出的洞）：
     这一条最早钉了手写的 5 对（queued/failed/ok/running/pending），而主题里
     实际有 9 对（还漏了 acc/butler/done/review）。手写清单的毛病是——
     以后新增一个语义色，**没人会记得回来把它加进清单**，于是漏检且无声。
     这正是 G75 当初被关掉时的形状（用一个名单堵了一个洞，下一条同类色照样漏）。
     现在改成：扫 ``--p-X-bg`` 这种键，它的前景就是 ``--p-X``，两者都在才算一对。
     下面还有一条反向守卫：发现到的配对数不得少于约定下限。 */
  describe('G75 — 语义色压在它自己的半透明底上仍达 AA（合成后再算）', () => {
    /* 从主题文件现发现的配对。`--p-X-bg` 的兄弟是 `--p-X`——但只有两者都在
       **同一套主题**里定义时才成对（`--p-warn-bg` 的兄弟是 `--p-warn`，
       它是另一层别名；`--p-acc-bg` 的兄弟 `--p-acc` 同理）。 */
    const pairsOf = (name: ThemeName): [string, string][] => {
      const m = TOKENS[name]
      const out: [string, string][] = []
      for (const key of m.keys()) {
        if (!key.endsWith('-bg')) continue
        const fgKey = key.slice(0, -'-bg'.length)
        if (m.has(fgKey)) out.push([fgKey, key])
      }
      return out.sort(([a], [b]) => a.localeCompare(b))
    }

    for (const name of THEMES) {
      it(`${name}：语义色 / 自己的 -bg 合成底 ≥ ${AA}:1`, () => {
        const pairs = pairsOf(name)
        /* 反向守卫：发现机制本身失效（正则/前缀写错、主题文件被改名）时，
           循环会是零次，上面那句 `toEqual([])` 会**空转判绿**。 */
        expect(
          pairs.length,
          `主题 ${name} 里一对「语义色 + 自己的 -bg」都没发现——发现机制失效了，`
            + '下面的循环会空转判绿。当前键：' + [...TOKENS[name].keys()].filter((k) => k.endsWith('-bg')).join(', '),
        ).toBeGreaterThanOrEqual(6)

        const failures: string[] = []
        for (const [fgKey, bgKey] of pairs) {
          const rawBg = TOKENS[name].get(bgKey) as string
          const fg = token(name, fgKey)

          /* `-bg` 是 `rgba(r,g,b,a)`：取它的颜色与 alpha，**按 alpha 合成**到
             它下面最常落的那一层（`--p-surf-1` 是区域容器；卡片是 surf-2）。
             两层都量：同一枚 badge 在看板列里（surf-1）与卡片里（surf-2）都会出现。 */
          const m = /^rgba?\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)\s*(?:,\s*([\d.]+)\s*)?\)$/.exec(
            rawBg.trim(),
          )
          if (!m) continue
          const bgRgb: Rgb = [Number(m[1]), Number(m[2]), Number(m[3])]
          const alpha = m[4] === undefined ? 1 : Number(m[4])

          for (const underKey of ['--p-surf-1', '--p-surf-2'] as const) {
            const painted = over(bgRgb, alpha, token(name, underKey))
            const ratio = contrast(fg, painted)
            if (ratio < AA) {
              failures.push(
                `${fgKey} 压在 ${bgKey}(α${alpha}) 合成到 ${underKey} 上只有 ${ratio.toFixed(2)}:1`,
              )
            }
          }
        }
        expect(
          failures,
          `主题 ${name} 的语义色在自带的半透明底上不达 AA ${AA}:1：\n${failures.join('\n')}\n`
            + '这类组合的形状是固定的：`color: var(--p-X)` + `background: var(--p-X-bg)`，'
            + '而 `-bg` 是同一色相的 rgba。三个坑都会让读数虚高：'
            + '① 直接拿 rgba 算而不是合成到实体层；'
            + '② 只量 --p-bg 而 badge 实际坐在 surf-1/surf-2 上；'
            + '③ 只量中性主题——纸白/米白两套的底最亮，往往就是它们不过。',
        ).toEqual([])
      })
    }
  })
})
