/* ===========================================================================
   四套主题：**实现自洽 + 无障碍**（原型降级为参照物）
   ---------------------------------------------------------------------------
   ── 本次判据换靶（用户裁定，v0.8.1 第8轮）────────────────────────────
   从这一轮起，**`prototype/` 不再是标准**。标准是 `packages/web` 的实际实现；
   原型退回「新页面可以参考它的页面架构」这一层参考资料的地位。

   旧判据是「实现的每个 `--p-*` 必须逐值等于 `prototype/styles/theme-*.css`」，
   它有两个问题，都是这次换靶要解决的：

     ① **靶子错位**：原型自己有可访问性缺陷（`--fg-3` 四套全部低于 AA、
        `--queued` 在纸白/米白上 4.37 / 4.25）。把它当标准，就等于要求实现
        连缺陷一起照抄；实现想修，反而要挤进「有意偏离」这张表里自证清白。
        那套流程本末倒置——**修好可访问性不该是「偏离」，它就是基线**。
     ② **它守不住它想守的东西**：「逐值相等」在原型不再更权威之后，
        无法回答「这个值对不对」。真正要守的是「这个色压在它真实的落点
        背景上读不读得清」。

   换靶后的判据分两层：

     ① **实现自洽**：四份主题文件的**变量名集合完全一致**（可切换的前提），
        且每个 `--p-*` 都在四份里都有定义。这一层与原型无关。
     ② **无障碍硬判据**：语义色对**它的真实落点背景**必须达到 WCAG AA 4.5:1。
        落点不是「页面底」，而是这个色真正被用到的那些层（`--p-surf-1/2/3`、
        以及各自 `-bg` 半透明底的**合成色**）——半透明底要先按 alpha 合到
        下一层实体底上再算，直接量 `rgba(...)` 会得出虚高的读数。

   ── 原型对照还留不留 ────────────────────────────────────────────────
   留，但**降级为差异清单**：实现与原型不一致时，不再直接判红，而是要求
   该变量在 `PROTOTYPE_DIFFS` 里有一条理由。这样「搬的时候手一抖少一位
   十六进制」这类错误仍然抓得住（它没有理由），而「实现主动修好」只需
   写一句话，不必把修好说成「偏离」。
   =========================================================================== */
import { describe, expect, it } from 'vitest'

import { customPropsIn, stripCssComments } from '@/test/css-scan'

import { readRepoFile } from '@/test/css'

const THEMES = ['neutral', 'warm', 'paper', 'cream'] as const

/** 取一个 CSS 文件里 `--name: value;` 的映射（覆盖多值写法：一行里多条声明）。 */
function vars(css: string): Map<string, string> {
  const out = new Map<string, string>()
  // 去掉注释，否则注释里的 `--bg: #fff` 之类会被当成定义
  const stripped = stripCssComments(css)
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

/**
 * **实现与原型不一致的地方**（含理由）。
 *
 * 注意语义：这张表不再叫「有意偏离」。原型降级为参照物之后，实现改进自己的
 * 可访问性**不需要**向原型自证；这张表存在的唯一目的，是把「实现了改」和
 * 「搬的时候手一抖」区分开——两者都表现为「实现 != 原型」，但前者有理由。
 *
 * 所以判据是：与原型不一致 **且** 不在这张表里 → 判红（大概是搬错了）。
 * 在表里 → 放行，但下面的无障碍硬判据仍会独立量它，该达标的一个都跑不掉。
 */
const PROTOTYPE_DIFFS: Record<string, string> = {
  '--p-fg-3':
    '原型 --fg-3 四套全部低于 WCAG AA 4.5:1，而它被当真实文字色用'
    + '（.af-head__sub / th / .af-field__hint）。实现换成达标值，色相不动、只调亮度。',
  '--p-fg-2':
    '第5轮 G71/G73：fg-3 为达 AA 而提亮后，warm/neutral 的 fg-2 与它只剩 1.16x/1.20x，'
    + 'paper/cream 本就只差 1.09x/1.12x，两级视觉上塌成一级。实现调整 fg-2 使级差 ≥ 1.25x。'
    + '（fg-2 本身调之前就达标，改它纯粹是为了保住「次要 vs 三级」这把梯子。）',
  '--p-queued':
    '第8轮 4d 实测：原型 #8a6208 压在 rgba(...,0.10~0.11) 半透明底上，'
    + '.af-badge--warn 在 paper 只有 4.37:1、cream 4.25:1，不到 4.5。'
    + '落点是 .af-badge--warn / .af-tag--queued / .af-callout--warn 的文字色。'
    + '两套亮主题加深到 #805a07（4.91 / 4.78），-bg/-line 的 RGB 同步换掉。',
  '--p-queued-bg':
    '随 --p-queued 一起换的底色 RGB（138,98,8 -> 128,90,7）。'
    + '只改前景不改底色，前景与底会是两套黄褐，视觉上分层。'
    + 'alpha 不变（paper 0.10 / cream 0.11），所以透明度语义与原型一致。',
  '--p-queued-line':
    '随 --p-queued 一起换的描边 RGB（138,98,8 -> 128,90,7），理由同上。alpha 不变（0.28）。',
  '--p-failed':
    '第8轮 G75 通用判据：原 #dc908b(neutral) / #db9188(warm) 压在自带的半透明 -bg 上、'
    + '合成到 --p-surf-2 后只有 4.44 / 4.43:1。提亮到 #e29a95 → 4.81 / 4.79:1，色相不变。'
    + 'paper/cream 的 #a52a1e 本来就过，不动。',
  '--p-failed-bg':
    '随 --p-failed 一起换的底色 RGB（neutral 220,144,139 / warm 219,145,136 -> 226,154,149），'
    + '理由同上：只改前景不改底色，前景与底会是两套红。alpha 不变。',
  '--p-warn':
    '第9轮：G75 从「手写 5 对」改成「从主题现发现 9 对」后抓出 —— paper/cream 的'
    + '--p-warn 与 --p-queued 是同源同值（都是 #8a6208），上一轮只改了 queued。'
    + '压在自己 -bg 上合成到 surf-1 只有 4.37 / 4.25:1，同步加深到 #805a07 -> 4.91 / 4.78。',
  '--p-warn-bg':
    '随 --p-warn 一起换的底色 RGB（138,98,8 -> 128,90,7），理由同上。alpha 不变（paper 0.10 / cream 0.11）。',
  '--p-warn-line':
    '随 --p-warn 一起换的描边 RGB（138,98,8 -> 128,90,7），理由同上。alpha 不变（0.28）。',
  '--p-review':
    '第9轮同一条判据抓出：neutral/warm 的 --p-review 压在自己 -bg 上、合成到 --p-surf-2 '
    + '只有 4.34 / 4.48:1。暗色主题提亮（#a79ad8 -> #b2a7e1 / #b09ec9 -> #bbaad3）'
    + '-> 4.87 / 4.98。paper/cream 的 #6b3fa0 本来就过（5.69+），不动。',
  '--p-review-bg':
    '随 --p-review 一起换的底色 RGB（neutral 167,154,216 / warm 176,158,201 -> 178,167,225 / 187,170,211），'
    + '理由同上。alpha 不变。',
  '--p-review-line':
    '随 --p-review 一起换的描边 RGB，理由同上。alpha 不变（0.34）。',
}

/** 四套主题 `--p-fg-3` 的达标值（期望值写死在这里，不从实现里再算一遍）。 */
const FG3_TARGETS = {
  neutral: '#969da5',
  warm: '#9d998e',
  paper: '#616972',
  cream: '#6c6758',
} as const

/** WCAG 2.x 相对亮度与对比度。 */
function relLuminance(hex: string): number {
  const v = hex.replace('#', '').toLowerCase()
  const channels = [0, 2, 4].map((i) => parseInt(v.slice(i, i + 2), 16) / 255)
  const [r, g, b] = channels.map((c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4))
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

function contrastRatio(a: string, b: string): number {
  const [hi, lo] = [relLuminance(a), relLuminance(b)].sort((x, y) => y - x)
  return (hi + 0.05) / (lo + 0.05)
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

describe('主题：实现自洽（与原型无关）', () => {
  for (const theme of THEMES) {
    it(`theme-${theme}：与参照物不一致的变量都写了理由，没写的大概是搬错了`, () => {
      const proto = vars(readRepoFile(`prototype/styles/theme-${theme}.css`))
      const impl = vars(readRepoFile(`packages/web/src/styles/theme-${theme}.css`))

      // 原型这一份里真正定义颜色的变量。**不硬编码个数**：原型里一行可能写
      // 多条声明（`--pending: #a2a9b2; --pending-bg: ...; --pending-line: ...`），
      // 展开后是 54 条而不是文件头注释说的「38 个变量组」。用组数当断言只会
      // 逼着后来人改数字，改成「两边集合必须对齐」才真的守住一致性。
      const colours = [...proto.keys()].filter((k) => k.startsWith('--'))
      expect(colours.length, `theme-${theme} 原型文件没有颜色变量，路径或解析有问题`).toBeGreaterThan(0)

      const missing: string[] = []
      const unexplained: string[] = []
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
        if (a === b) continue
        // 与参照物不一致是**允许**的（实现才是标准），但必须写清是哪一种不一致：
        // 是「实现主动改」还是「搬的时候手一抖」。没有理由的差异一律判红——
        // 这条是换靶之后唯一还在防「搬错」的判据，不能松。
        if (!PROTOTYPE_DIFFS[i]) unexplained.push(`${p} → ${i}：参照 ${a} / 实现 ${b}`)
      }
      expect(missing, `theme-${theme} 漏搬的变量：\n${missing.join('\n')}`).toEqual([])
      expect(
        unexplained,
        `theme-${theme} 这些变量与参照物不一致却没写理由（要么补进 PROTOTYPE_DIFFS，要么是搬错了）：\n`
          + unexplained.join('\n'),
      ).toEqual([])
    })
  }

  /* 登记表不能变成「写了就不用管」的垃圾桶：每条登记都必须**至少在一套主题里**
     真的与参照物不一致。某个变量后来被全部改回与原型一致时，这条会红，
     提醒把理由删掉——否则下一个人会读到一条描述着已不存在的差异的理由。
     **按「四套里只要有一套不同」判**：像 `--p-queued` 这种只改了 paper/cream
     两套的登记不该被判过期（早先按「每套都必须不同」写，立刻把 neutral/warm
     误判成过期——同一份实现里出现「有的主题改、有的不改」本来就是常态）。 */
  it('PROTOTYPE_DIFFS 里每条登记都对应一处真实差异（登记表不许留过期条）', () => {
    for (const [name, why] of Object.entries(PROTOTYPE_DIFFS)) {
      expect(typeof why, `${name} 的理由不是字符串`).toBe('string')
      expect(why.length, `${name} 的理由是空话（长度 ${why.length}）`).toBeGreaterThan(10)

      const protoName = Object.keys(NAME_MAP).find((k) => NAME_MAP[k] === name) ?? `--${name.slice(4)}`
      const differsSomewhere: string[] = []
      let compared = 0
      for (const theme of THEMES) {
        const proto = vars(readRepoFile(`prototype/styles/theme-${theme}.css`))
        const impl = vars(readRepoFile(`packages/web/src/styles/theme-${theme}.css`))
        const p = proto.get(protoName)
        if (p === undefined) continue
        compared += 1
        if (norm(impl.get(name) ?? '') !== normVarRefs(p)) differsSomewhere.push(theme)
      }
      // 参照物里根本没有这个变量时无从比较，不能算「过期」也不能算「有效」，
      // 这种情况由上面那条 unexplained 判据管（它只遍历参照物里有的变量）。
      if (compared === 0) continue
      expect(
        differsSomewhere.length,
        `${name} 在四套主题里都与参照物一致了，这条登记已过期，请从 PROTOTYPE_DIFFS 删掉`,
      ).toBeGreaterThan(0)
    }
  })

  /* `--p-fg-3` 的确切值 + 达标。这条**不依赖原型**：期望值是设计定下的
     十六进制（`FG3_TARGETS`），达标是按实现自己的背景算的。
     原型的 `--fg-3` 四套全部低于 AA，所以实现这边的值必然与参照物不同 ——
     但那已经不是「偏离」，是基线差异，理由记在 `PROTOTYPE_DIFFS` 里。 */
  it('--p-fg-3 四套的确切值，且必须达到 WCAG AA 4.5:1（辅助文字读得清）', () => {
    const failures: string[] = []
    for (const theme of THEMES) {
      const impl = vars(readRepoFile(`packages/web/src/styles/theme-${theme}.css`))
      const actual = norm(impl.get('--p-fg-3')!)
      const expected = FG3_TARGETS[theme]
      if (actual !== expected) failures.push(`theme-${theme} 的 --p-fg-3 是 ${actual}，定下的是 ${expected}`)

      const bg = norm(impl.get('--p-bg')!)
      const fg2 = norm(impl.get('--p-fg-2')!)
      const ratio = contrastRatio(expected, bg)
      const ratioFg2 = contrastRatio(fg2, bg)
      if (ratio < 4.5) failures.push(`theme-${theme} 的 --p-fg-3 对页面底只有 ${ratio.toFixed(2)}:1（< 4.5）`)
      /* **页面底不是唯一的落点**（v0.8.1 第5轮 G71）：这一条原先只量 `--p-bg`，
         于是 warm / neutral 判绿，而 fg-3 在更亮的 surf-1 / surf-2 / surf-3 上
         只有 4.44 / 4.06 / 3.68 与 4.37 / 3.98 / 3.56——全部不达 AA。三级文字
         用在看板列（surf-1）、卡片（surf-2）、卡片悬停（surf-3）上，那些层才是
         它真实的背景。只量页面底，等于把最难的那三层藏起来。
         `contrast.test.ts` 有同一判据的完整版（含 G72/G73），这里保留一份
         紧挨着取值守卫，让「改了这个十六进制」与「它是否达标」在同一个文件里。 */
      for (const surf of ['--p-surf-1', '--p-surf-2', '--p-surf-3'] as const) {
        const surface = impl.get(surf)
        /* **不是字面 hex 就判红，而不是跳过**（第9轮 4c 🟢-6）：原先写
           `if (!surface?.startsWith('#')) continue`，于是某主题若把 `--p-surf-*`
           写成 `color-mix()` / `var()`，那一层的检查会**自动消失**——判据绿了，
           但绿色的意思是「没量」，不是「达标」。三层里少一层最难量的亮层，
           正是 G71 当初漏掉 warm/neutral 的形状（只量页面底的时候它们全绿）。
           要求显式字面值：想换写法，先让 `contrastRatio` 会算它，再来改这一行。 */
        if (surface === undefined) {
          failures.push(`theme-${theme} 没有定义 ${surf}——fg-3 在它上面是否达标无从判断`)
          continue
        }
        if (!surface.trim().startsWith('#')) {
          failures.push(
            `theme-${theme} 的 ${surf} 是 \`${surface.trim()}\`（非字面 hex）——`
              + '这条判据只能算字面值。要么写成 hex，要么扩展 contrastRatio，不能靠跳过它换绿灯。',
          )
          continue
        }
        const r = contrastRatio(expected, norm(surface))
        if (r < 4.5) failures.push(`theme-${theme} 的 --p-fg-3 对 ${surf} 只有 ${r.toFixed(2)}:1（< 4.5）`)
      }
      /* 三级层次不能塌，而且要**分得出来**（v0.8.1 第5轮 G73）：原先只要求
         `ratioFg2 > ratio`，于是 paper 的 1.09x、cream 的 1.12x 都能过——两级
         在视觉上已经是同一级，文档却还写着「三级文字梯子」。1.25x 取四套修好
         后的实测最低值（paper 1.25x / cream 1.27x / warm 1.30x / neutral 1.31x）。 */
      const MIN_RAMP = 1.25
      if (ratioFg2 / ratio < MIN_RAMP) {
        failures.push(
          `theme-${theme} 的 --p-fg-3（${ratio.toFixed(2)}:1）与 --p-fg-2（${ratioFg2.toFixed(2)}:1）`
            + `只差 ${(ratioFg2 / ratio).toFixed(2)}x（< ${MIN_RAMP}x），两级塌成一级`,
        )
      }
    }
    expect(failures, `--p-fg-3 未达标注解：\n${failures.join('\n')}`).toEqual([])
  })

  /* `--p-fg-2` 也进了「有意偏离表」（G73 为保住梯子而调整），于是它跳过了逐值
     比对。跳过比对就必须有别的守卫接住，否则这个变量从此无人看管——那正是
     「有意偏离」这个机制最容易被误用的地方：它本意是带着理由偏离，不是把变量
     移出守卫。这条钉住四套的确切值，改了就红。 */
  it('--p-fg-2 四套的确切值（G73 有意偏离原型，值写死在这里）', () => {
    const FG2_TARGETS = {
      neutral: '#adb4bd',
      warm: '#b4afa4',
      paper: '#535a64',
      cream: '#5d574c',
    } as const
    const failures: string[] = []
    for (const theme of THEMES) {
      const impl = vars(readRepoFile(`packages/web/src/styles/theme-${theme}.css`))
      const actual = norm(impl.get('--p-fg-2')!)
      if (actual !== FG2_TARGETS[theme]) {
        failures.push(`theme-${theme} 的 --p-fg-2 是 ${actual}，定下的是 ${FG2_TARGETS[theme]}`)
      }
    }
    expect(failures, `--p-fg-2 值不符：\n${failures.join('\n')}`).toEqual([])
  })

  it('四份实现的变量名集合完全一致（可切换的前提）', () => {
    /* **用 `customPropsIn` 而不是 `vars()`**（v0.8.1 第5轮，G62）：`vars()` 是
       「整份文本跑 `(--x)\s*:` 正则」，于是**注释里**写的 `--af-x: #abc`（解释
       某条取值为什么这样写时的自然写法）会被当成一次真实定义，`var(--x)` 里的
       引用也会被算进名字集合。判「这一份定义了哪些变量」必须看**规则体的左侧**，
       否则一份主题只要在注释里提过某个名字，集合比对就替它遮住了真正漏掉的定义。
       `vars()` 那条「取某个变量的值」的用例维持原样——它要的是值，不是名字集合。 */
    const sets = THEMES.map((t) => [...customPropsIn(readRepoFile(`packages/web/src/styles/theme-${t}.css`))].sort())
    for (let i = 1; i < sets.length; i++) {
      expect(sets[i], `${THEMES[i]} 与 ${THEMES[0]} 的变量名集合不同`).toEqual(sets[0])
    }
    /* 反向：集合不能空。空的集合之间当然「完全一致」——那正是这条守卫最可能的
       假绿形态（解析器坏了、或正则改成只认 `--p-x:` 的拼写）。 */
    expect(sets[0].length, '四份主题一个自定义属性都没解析出来').toBeGreaterThan(10)
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

  /* ── 改名守卫：`var(--p-*)` 的每一个被引用的名字都必须真有定义 ──────────
     为什么需要这条：`--inset` → `--p-inset-c`、`--add` → `--p-add-ink-c`、
     `--del` → `--p-del-ink-c` 是**有意改名**（原型的裸名加 `-c` 后缀，
     表示「这一层是颜色」）。CSS 里引用一个不存在的自定义属性**不会报错**，
     它只是让那条声明静默失效 —— 颜色退回继承值，看起来「有点不对」但查不出来。
     2026-11-17 步骤7 验收专门点了这一条：改名本身合理，但没有断言看守。

     **定义可以在两处之一**，因为令牌分两层：
       · 颜色令牌（`--p-bg` / `--p-inset-c` / `--p-acc` …）→ 四份主题文件，逐套不同
       · 非颜色令牌（`--p-s1` 间距 / `--p-t-sm` 字号 / `--p-r-sm` 圆角 /
         `--p-dur` 动效 / `--p-row` 行高 …）→ `tokens.css` 的映射层，四套共用
     所以判据是「**在这两处的并集里能找到定义**」，而不是「必须在主题文件里」——
     后者会把间距字号误判成漏定义（第一版就是这么红的）。 */
  it('CSS 里引用的每个 --p-* 都在主题文件或 tokens.css 里有定义（改名不会静默失效）', () => {
    const defined = new Set<string>()
    for (const t of THEMES) {
      for (const k of vars(readRepoFile(`packages/web/src/styles/theme-${t}.css`)).keys()) {
        if (k.startsWith('--p-')) defined.add(k)
      }
    }
    // 非颜色令牌的落点：tokens.css 的 `--p-* : var(--af-*)` 映射层
    for (const k of vars(readRepoFile('packages/web/src/styles/tokens.css')).keys()) {
      if (k.startsWith('--p-')) defined.add(k)
    }
    expect(defined.size, '一张 --p-* 都没读到，路径或解析有问题').toBeGreaterThan(0)

    // 扫全部样式表 + SFC 的 <style> 块，收集 `var(--p-...)` 的引用。
    // **主题文件本身也要扫**：三个改名变量的全部引用都写在主题文件的
    // `--af-*: var(--p-*)` 转发行里。第一版漏了这四个文件，于是「在主题里
    // 把 --p-inset-c 拼成 --p-inset-typo」这种错当场漏网（变异 M-A 实测）。
    const cssFiles = [
      ...THEMES.map((t) => `packages/web/src/styles/theme-${t}.css`),
      'packages/web/src/styles/components.css',
      'packages/web/src/styles/prototype-shell.css',
      'packages/web/src/styles/tokens.css',
      'packages/web/src/styles/index.css',
      'packages/web/src/pages/SettingsPage.vue',
      'packages/web/src/pages/LoginPage.vue',
      'packages/web/src/pages/SetupPage.vue',
    ]
    const referenced = new Map<string, string[]>()
    for (const f of cssFiles) {
      const css = stripCssComments(readRepoFile(f))
      for (const m of css.matchAll(/var\(\s*(--p-[a-z0-9-]+)/g)) {
        const name = m[1]
        if (!referenced.has(name)) referenced.set(name, [])
        const owners = referenced.get(name)!
        if (!owners.includes(f)) owners.push(f)
      }
    }
    expect(referenced.size, '一个 var(--p-*) 都没扫到，正则或文件清单有问题').toBeGreaterThan(0)

    const orphans = [...referenced.keys()].filter((n) => !defined.has(n)).sort()
    const detail = orphans.map((n) => `${n}（引用于 ${referenced.get(n)!.join(' / ')}）`).join('\n')
    expect(orphans, `这些 --p-* 被引用但主题文件与 tokens.css 都没有定义，会静默失效：\n${detail}`).toEqual([])
  })

  /* ── 设置页的四张主题预览卡：12 个硬编码色必须与主题文件同步 ──────────
     为什么这几处**必须**硬编码、不能用 `var(--p-*)`：预览卡要显示**另外三套**
     配色的样子，而 `--p-bg` 在当前页面只解析成「正在用的那一套」。变量在这里
     天然做不到，所以硬编码是对的。但硬编码的代价是**会与主题文件漂移** ——
     改了 `theme-warm.css` 的 `--p-bg` 而忘了改预览卡，卡片就会显示一个
     已经不存在的颜色，而且不会报错、只是「看着有点旧」。
     这条用例就是那个同步断言（2026-11-17 步骤4 F-4）：三列色值分别取自
     该主题的 `--p-bg`（页面底） / `--p-surf-2`（卡片条） / `--p-fg`（文字线）。 */
  it('设置页主题预览卡的 12 个色值与四份主题文件逐一对齐', () => {
    const settings = readRepoFile('packages/web/src/pages/SettingsPage.vue')

    /** `.af-theme__prev` / `__pv-bar` / `__pv-line` 分别对应的主题变量。 */
    const SLOTS = [
      ['af-theme__prev', '--p-bg'],
      ['af-theme__pv-bar', '--p-surf-2'],
      ['af-theme__pv-line', '--p-fg'],
    ] as const

    const drift: string[] = []
    for (const theme of THEMES) {
      const themeVars = vars(readRepoFile(`packages/web/src/styles/theme-${theme}.css`))
      for (const [cls, varName] of SLOTS) {
        // 找 `[data-theme-pick='<theme>'] .<cls> { background: #xxx; }`
        const re = new RegExp(
          `\\[data-theme-pick=['"]${theme}['"]\\]\\s*\\.${cls}\\s*\\{[^}]*background:\\s*([^;}]+)`,
        )
        const m = settings.match(re)
        if (!m) {
          drift.push(`设置页缺少 [data-theme-pick='${theme}'] .${cls} 的 background`)
          continue
        }
        const hardcoded = norm(m[1])
        const fromTheme = norm(themeVars.get(varName) ?? '<未定义>')
        if (hardcoded !== fromTheme) {
          drift.push(`${theme} .${cls}：设置页写 ${hardcoded}，而 theme-${theme}.css 的 ${varName} 是 ${fromTheme}`)
        }
      }
    }
    expect(drift, `预览卡与主题文件已漂移：\n${drift.join('\n')}`).toEqual([])
  })

  /* 三个改名变量的正向守卫 */
  it('三个有意改名的颜色变量都存在且有引用（-c 后缀不是笔误）', () => {
    const rename = ['--p-inset-c', '--p-add-ink-c', '--p-del-ink-c'] as const

    for (const name of rename) {
      // ① 四份主题都必须定义它（颜色逐套不同，缺一份就是那套会失效）
      for (const t of THEMES) {
        const css = readRepoFile(`packages/web/src/styles/theme-${t}.css`)
        const defs = vars(css)
        expect(defs.has(name), `theme-${t}.css 缺 ${name}`).toBe(true)

        // ② **在同一个主题文件里**必须被 `var()` 引用，否则是那套主题的死变量。
        // 判据必须逐文件做：第一版把四份主题拼成一个大字符串再数引用，
        // 于是「只在 warm 里把两处引用全断掉」被另外三份的引用顶住了
        // （变异 M-D* 实测漏网）。每份主题自洽才是真正的不变式。
        // 也必须数 `var(--name)` 而不是数 `--name` 子串：定义行自己就含这个
        // 子串，「定义了却没人用」会被定义行满足（变异 M-D 实测漏网）。
        const refs = css.match(new RegExp(`var\\(\\s*${name}\\s*\\)`, 'g')) ?? []
        expect(refs.length, `theme-${t}.css 里 ${name} 定义了却没有 var() 引用（死变量）`).toBeGreaterThan(0)
      }
    }

    // ③ 旧名字（无 -c 后缀）不许再被任何地方引用
    const everywhere = [
      ...THEMES.map((t) => readRepoFile(`packages/web/src/styles/theme-${t}.css`)),
      readRepoFile('packages/web/src/styles/components.css'),
      readRepoFile('packages/web/src/styles/prototype-shell.css'),
    ].join('\n')
    for (const stale of ['--p-inset', '--p-add', '--p-del']) {
      const refs = everywhere.match(new RegExp(`var\\(\\s*${stale}\\s*\\)`, 'g')) ?? []
      expect(refs.length, `旧名 ${stale} 仍被 var() 引用，应已改名为带 -c 后缀`).toBe(0)
    }
  })
})
