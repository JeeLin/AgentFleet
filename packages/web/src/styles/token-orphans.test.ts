/* ===========================================================================
   每一个「定义了但零 var() 消费」的自定义属性，都必须在 DECLARED_ORPHANS 里
   写明理由（反向守卫）
   ---------------------------------------------------------------------------
   4a S6（G92）在 `styles/` 里数出一批「已定义、全仓无 `var()` 引用」的
   `--af-*` / `--p-*` 名字。第一版处置是「纯档位删掉、语义槽留着」，删到一半
   被用户叫停，理由是——

     「不然后续增加对应内容的时候，不确定最初的设计数据」

   这条理由是对的，而且比「删了会不会红」更根本：一个 token 之所以存在，
   往往是因为**它记录了一个设计判断**，而不是因为此刻有人读它。
   `--af-text-lg: 15px` 的行尾注释写着「面板标题（原型无对应档）」，
   它的意思是「原型没有这一档，这是我们自己加的」；删掉它，下一个人对着
   原型数档位时会以为漏了一档，然后**重新发明一个不同值的 15px**。
   `--p-t-lg: var(--af-text-xl)` 的行尾注释写着「18：页面标题」，更直接——
   它的消费者被 `prototype-shell.css:360` 的注释记着「用 display 档（22px）
   而不是原来的 `--p-t-lg`（18px）：实测 18px 压不住正文」。
   **「它曾经被用过、被什么替换了」本身就是设计数据。**

   所以本轮处置从「删」改成「登记 + 钉住」：
     · 全部保留，一个不删；
     · 每一个在 `DECLARED_ORPHANS` 里写明**为什么留**，理由必须可证伪；
     · 这条用例把「实际孤儿名单」与「登记表」钉成**双向一致**：
         ① 出现一个新孤儿（定义了、没人用、也没登记）→ 判红，逼着你
            要么补消费者、要么写理由，不许再攒；
         ② 登记项被删（表里有、代码里没了）→ 判红，防止理由与代码脱节；
         ③ 登记项被真正消费了（表里有、但它已经有 `var()` 消费者）→ 判红，
            提示把它从表里移除——**登记不是永久豁免**，一旦它重新服役，
            这条豁免就该消失。

    ── 数字为什么是 34（判据演进史，每一步都可复算，别删）──────────────
    三个数都由「同一套 `sourceFiles()` + 只改消费判据这一项」复算得出
    （第 6 轮 4a 指出我原先写的 20 / 27 两个数**任何一种口径都复算不出来**，
    已按复算结果改正）：

    · **17** —— 不剥注释、不排除定义右侧的朴素扫描；
    · **26（+9）** —— 剥掉注释：注释里提到 token 名**不是**消费者（见下节，
      `--af-text-lg` 就是被一句散文注释「引用」的）；
    · **34（+8）** —— 再排除「定义右侧的 `var()`」：`--x: var(--y)` 里的
      `var(--y)` 是在给 `x` 赋值，不代表 `y` 被谁读过。这一步多出 8 个
      曾被「假装有消费者」的名字——7 个 `THEME_SOURCE_SLOTS` 真身槽位
      （`--p-fg-strong` / `--p-sh-1` / `--p-sh-2` / `--p-scrim` /
      `--p-inset-c` / `--p-add-ink-c` / `--p-del-ink-c`）**加分列的**
      `PROTOTYPE_SCALE_BRIDGES`（`--af-space-13`，见 ⑤ 组头：它是纯档位，
      不能混进「四套主题各一套」的真身组）。

    · `--af-fg-strong`（补了 17 处消费者）在 `RETIRED_ORPHANS`，不计入 34。
    所以现在：**34 条登记 + 1 条退役**。

    ── 判据为什么必须「剥注释后再扫」────────────────────────────────────
   这是本轮我自己踩的坑：初版用朴素正则扫 `var(--x)`，得到 **17** 个孤儿（见
   上面的复算，原先这里写的「20 个」是记错的口径，任何判据都复算不出 20）；
   实际上 `--af-text-lg` 唯一被「引用」的地方是 `tokens.css:115` 的一句
   散文注释——`也不取 15px：与 --af-text-lg 撞档`。把它当消费者，
   就漏掉了它其实是孤儿。所以本文件**先 `stripCssAndLineComments`**
   再扫，且对 `.vue` / `.ts` 也剥 `//` 行注释：注释里提到 token 名
   **不是**消费者，只说明它被讨论过。

   同理，`.ts` 里用字符串按名取 token（`tokenPx(tokens, 'af-text-md')`）
   **算**消费者——那是真的在按名字读它，只是读法不是 `var()`。
   所以扫描除了 `var(--x)`，还要收 `'--x'` / `'x'`（无前缀）两种字面量。

   ── 与 `class-usage.test.ts` 的分工 ──────────────────────────────────
   那边管**类名**（`.af-*` 有没有定义 / 有没有人用），这边管**自定义属性**
   （`--af-*` / `--p-*` 有没有人 `var()`）。两个反向守卫形状相同、对象不同，
   刻意分开：混在一起时改一条断言会连带影响另一边的读数。

   ── 下面 34 条理由里的「N 次」是什么口径（写死，别再各写一套）─────────
   **全部是「`var()` 引用计数」，范围 = `packages/web/src/` 的 `.css` + `.vue`，
   不含 `theme-*.css` 里 `--x: var(--y)` 这种转发定义，不含测试文件。**
   说「同族在役」时，数的是同族各名字的 `var()` 引用次数，不是「有几个名字存在」。
   原型侧同理：`grep -o 'var(--名字)' prototype/assets/app.css | wc -l`。
   写理由时要么两个数都给（名字数 + 次数），要么只用次数并写明「`var()` 计数」。
   **不许把「4 个类型」与「40 次总数」并列成「四个（共 40 次）」**——那是两套口径
   混在一句话里，读者会以为每个类型各 10 次。
   =========================================================================== */

import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'

import { describe, expect, it } from 'vitest'

import { stripCssAndLineComments } from '@/test/css-scan'

const SRC = join(process.cwd(), 'src')

/** 递归收集 `src/` 下的源文件（`.css` / `.vue` / `.ts` / `.tsx` / `.js`）。 */
function sourceFiles(dir: string): string[] {
  const out: string[] = []
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) {
      if (entry === 'node_modules' || entry === 'dist') continue
      out.push(...sourceFiles(full))
      continue
    }
    if (/\.(css|vue|ts|tsx|js)$/.test(entry) && !entry.endsWith('.test.ts')) out.push(full)
  }
  return out
}

const FILES = sourceFiles(SRC)
/** 每个源文件**剥掉注释**后的正文。剥注释是这条守卫的判据核心，见文件头。 */
const STRIPPED = new Map(
  FILES.map((f) => [relative(join(process.cwd(), '../..'), f), stripCssAndLineComments(readFileSync(f, 'utf-8'))]),
)

/**
 * 全仓**定义**了哪些自定义属性名（`--x: …` 形式的声明左侧）。
 *
 * `(?:^|[{;])\s*` 是 CSS 自定义属性的合法声明位置：行首、`{` 之后、
 * `;` 之后。这样 `var(--x)` 里的引用不会被误当成定义。
 * `.vue` 的 `<style>` 块与 `.ts` 里内联的 CSS 串只要含 `--x:` 也会命中——
 * 宁可多收（多收只会让孤儿更少，不会误报孤儿）。
 */
function definedProps(): Map<string, string[]> {
  const out = new Map<string, string[]>()
  for (const [file, text] of STRIPPED) {
    for (const m of text.matchAll(/(?:^|[{;])\s*(--[a-zA-Z0-9_-]+)\s*:/g)) {
      const list = out.get(m[1])
      if (list) list.push(file)
      else out.set(m[1], [file])
    }
  }
  return out
}

/**
 * 全仓**消费**了哪些自定义属性名。
 *
 * 三种读法都算：
 *   ① `var(--x)` —— CSS 里的正常引用；
 *   ② `'--x'` —— 脚本里按名取（`getPropertyValue('--x')`）；
 *   ③ `'x'`（无前缀，如 `'af-text-md'`）—— 测试夹具把前缀补在别处的读法，
 *      例如 `ui-scale.test.ts` 的 `tokenPx(tokens, 'af-text-md')`。
 *
 * ③ 是刻意放宽的：不认它就会把「被测试按名读取」的 token 报成孤儿。
 * 但**注释不算**——`STRIPPED` 已经剥过，这正是本文件头讲的那个坑。
 *
 * ── ① 要排除「定义右侧的 `var()`」──────────────────────────────────────
 * `--af-fg-strong: var(--p-fg-strong)` 里，`var(--p-fg-strong)` 是**声明**
 * 在给 `--af-fg-strong` 赋值，它并不说明 `--p-fg-strong` 被谁读了。
 * 不排除的话会造出「假消费」：实测有 7 个名字只因出现在别人的定义右侧
 * 就被算成「有消费者」——`--af-space-13`、`--p-add-ink-c`、`--p-del-ink-c`、
 * `--p-inset-c`、`--p-scrim`、`--p-sh-1`、`--p-sh-2`。
 * 其中 `--af-space-13` 是纯档位：它明明零消费，却因
 * `--p-s8: var(--af-space-13)` 这条转发被当成人人都在用的 token。
 *
 * 判法：看 `var(` 前面（同一段，未跨 `{` `}` `;`）是不是只有
 * 「`--名字` + `:` + 可选空白」。是 → 定义右侧，跳过。
 */
function consumedProps(): Set<string> {
  const out = new Set<string>()
  for (const text of STRIPPED.values()) {
    for (const m of text.matchAll(/var\(\s*(--[a-zA-Z0-9_-]+)/g)) {
      /* 往回找最近的「分隔符」（`{` `}` `;`），取中间那段看它是不是
         `--x:` 形态。限 200 字符是为了不让长规则体把开销拖大。 */
      const before = text.slice(Math.max(0, m.index - 200), m.index)
      const cut = Math.max(before.lastIndexOf('{'), before.lastIndexOf('}'), before.lastIndexOf(';'))
      const seg = cut >= 0 ? before.slice(cut + 1) : before
      if (/^\s*--[a-zA-Z0-9_-]+\s*:\s*$/.test(seg)) continue // 定义右侧，不算消费
      out.add(m[1])
    }
    for (const m of text.matchAll(/['"`](--[a-zA-Z0-9_-]+)['"`]/g)) out.add(m[1])
    // 无前缀写法：'af-text-md' → --af-text-md
    for (const m of text.matchAll(/['"`](af|p)-([a-zA-Z0-9_-]+)['"`]/g)) out.add(`--${m[1]}-${m[2]}`)
  }
  return out
}

/* ── 登记表：每一个「定义了但零 var() 消费」的名字，逐条写理由 ──────────────
   **不许写「暂未使用」**——那等于给孤儿开后门，G92 之前那批就是「暂未使用」
   攒出来的。每条理由都必须能被证伪：要么指向原型里的一行原文，要么指向
   一条测试/注释里记着的替换史，要么指向「同族其余档位仍在服役」这个事实。

   分组按**保留理由的性质**，不按文件。同一组里的 token 理由相同，
   就不重复写 N 遍。 */
type OrphanReason = { why: string; kin: string }

/* ⑤ **原型槽位的纯档位**：只有 1 个名字，但**不能并入 ④**——
   ④ 的判据是「四套主题各一套字面值」（实测 7 个成员在主题里各出现 4 次），
   而 `--af-space-13` 在四套主题里出现 **0 次**（它只在 `tokens.css` 定义一次）。
   第 6 轮 4a 抓到这条归组矛盾：组头说「四套主题各一套」，组员却是纯档位。
   单列一组，是为了让组头的判据真正成立。 */
const PROTOTYPE_SCALE_BRIDGES: Record<string, OrphanReason> = {
  '--af-space-13': {
    why:
      '**纯档位，但被转发**：56px，原型间距第 8 档 `--s8: 56px` 的精确映射（`--p-s8: var(--af-space-13)`）。'
      + '它自己零直读，而 `--p-s8` 有 12 次直读——**这是「档位 → 原型槽位」的桥**，'
      + '删了 `--p-s8` 就断链。与 `--af-space-14`（72px，原型无对应档）不同：'
      + '13 有原型出处（`prototype/assets/tokens.css:33`）。',
    kin: '--af-space-1..12（在役）、--p-s8（12 次直读）',
  },
}

/* ④ **真身槽位**：它们是被 `--af-*` 门面层**转发出去**的那一端，自身没有规则
   直接读。第 6 轮 4b 指出「消费」口径过宽（把定义右侧的 `var()` 也算消费）后，
   收紧判据才暴露出来——**它们之前一直是靠别人的定义右侧「假装有消费者」**。

   为什么必须保留：这些名字承载的是**配色/几何的字面值**（四套主题各一套），
   门面层的 `--af-*` 只是转发。删了真身，转发链断掉，四套主题的色值就没了落点。
   这与「纯档位没人用」是两件事：`--p-sh-1` 不是「空档位」，它是阴影值本体。 */
const THEME_SOURCE_SLOTS: Record<string, OrphanReason> = {
  '--p-fg-strong': {
    why:
      '**真身槽位**（四套主题各一个字面值：`#dfe3e8` / `#e2ded6` / `#1c2127` / `#23211c`）。'
      + '被 `--af-fg-strong` 与 `--af-term-ansi-bright-white` 两条转发出去（`theme-*.css:75/127/152/222`）。'
      + '自身零规则直接读——G92 把那 17 处都改走 `--af-fg-strong` 门面层之后，'
      + '它就成了「只被转发、不被直读」的真身。这正是门面层该有的形状，不是死代码。',
    kin: '--af-fg-strong（17 次直读，在役）、--af-term-ansi-bright-white',
  },
  '--p-add-ink-c': {
    why:
      '**真身槽位**（diff 新增侧的墨色）。被 `--af-add-ink` 与 `--af-add-ink-strong` '
      + '两条转发（`theme-*.css`），自身零直读。`-c` 后缀是「这一层是颜色」的既有约定'
      + '（见 `theme-neutral.css` 的改名说明：`--add → --p-add-ink-c` 是有意改名）。',
    kin: '--af-add-ink / --af-add-ink-strong（各 1 次直读）',
  },
  '--p-del-ink-c': {
    why:
      '**真身槽位**（diff 删除侧的墨色）。被 `--af-del-ink`、`--af-del-ink-strong`、'
      + '`--af-danger-ink` 三条转发，自身零直读。删了会让 diff 两侧与 danger 墨色同时失去落点。',
    kin: '--af-del-ink / --af-danger-ink（各 1 次直读）',
  },
  '--p-inset-c': {
    why:
      '**真身槽位**（内凹表面的颜色，原型 `--inset` 一族）。被 `--af-inset` 与 '
      + '`--af-term-bg` 转发。四套主题各一套字面值——删了就是丢一份配色数据。',
    kin: '--af-inset（在役）、--af-term-bg',
  },
  '--p-scrim': {
    why:
      '**真身槽位**（遮罩层颜色，浮层背后的压暗）。被 `--af-scrim` 转发。'
      + '四套主题各一套（暗色/亮色的压暗强度不同，不能共用一套值）。',
    kin: '--af-scrim（在役）',
  },
  '--p-sh-1': {
    why:
      '**真身槽位**：阴影第一档的字面值（`0 1px 2px rgba(0,0,0,0.28)`，四套各一份）。'
      + '被 `--af-shadow-1` 转发。原型 `--sh-1` 自己也是零消费（浮层统一用 `-3`），'
      + '但三档是一条阶梯，且**阴影值是颜色**——删了就丢数据。',
    kin: '--af-shadow-1、--af-shadow-3（在役）',
  },
  '--p-sh-2': {
    why:
      '**真身槽位**：阴影第二档字面值（`0 6px 18px rgba(0,0,0,0.38)`，四套各一份），'
      + '被 `--af-shadow-2` 转发。零直读原因与保留理由同 `--p-sh-1`。',
    kin: '--af-shadow-2、--af-shadow-3（在役）',
  },
}

/** ① 原型里有这个槽位，实现必须搬过来（`theme-css.test.ts` 逐值对照原型的门禁）。 */
const PROTOTYPE_COLOR_SLOTS: Record<string, OrphanReason> = {
  /* 注意：`--af-fg-strong` 曾在此，G92 补消费者后已移到 `RETIRED_ORPHANS`。 */
  '--af-pending-fg': {
    why:
      '原型 `--pending` 一族（`prototype/styles/theme-neutral.css:63` 一行三槽）的实现侧转发。'
      + '**但整族三个 `--af-pending-*` 目前零消费者**：`.af-tag--pending` 等规则直接写'
      + '`var(--p-pending*)`，把这一层跨过去了。保留理由不是「有人在用」，而是'
      + '**它就是原型槽位对照表里的那一行**：`theme-css.test.ts` 要求原型每个颜色变量'
      + '都在实现里存在，删了会让「原型有、实现没有」的缺口重新出现。'
      + '要清掉它，得连原型对照口径一起改（那是另一个决定，不在 G92 范围内）。',
    kin: '--af-pending-{bg,line}',
  },
  '--af-pending-bg': {
    why:
      '原型 `--pending-bg`（`prototype/styles/theme-neutral.css:63`，四套各一份）的实现侧转发，'
      + '原型 `app.css` 用 1 次。零消费者原因同 `--af-pending-fg`：实现绕过门面层。',
    kin: '--af-pending-{fg,line}',
  },
  '--af-pending-line': {
    why:
      '原型 `--pending-line`（`prototype/styles/theme-neutral.css:63`，四套各一份）的实现侧转发，'
      + '原型 `app.css` 用 1 次。零消费者原因同 `--af-pending-fg`。',
    kin: '--af-pending-{fg,bg}',
  },
  '--af-shadow-1': {
    why:
      '原型 `--sh-1: 0 1px 2px rgba(0,0,0,0.28)`（四套各一份）的实现侧转发。'
      + '原型自己 `app.css` **零消费**，实现里浮层统一用 `--af-shadow-3`。'
      + '保留：`tokens.css:192` 起 §9 的注释写明「阴影名为 `--af-shadow-1/2/3`」'
      + '——**三档是一条阶梯**，只留第三档会让 `-3` 这个编号失去前两级依据。'
      + '要收就整族收到 `--af-shadow-1`，不能只删中间。',
    kin: '--af-shadow-3（在役，components.css）',
  },
  '--af-shadow-2': {
    why:
      '原型 `--sh-2: 0 6px 18px rgba(0,0,0,0.38)` 的实现侧转发，零消费原因同 `--af-shadow-1`。',
    kin: '--af-shadow-3（在役）、--af-shadow-1',
  },
  '--p-warn': {
    why:
      '**字面色值**（不是转发）：四套主题各一套，`#cdae7a` / `#cfae74` / `#8a6208` / `#8a6208`，'
      + '从原型 `--warn`（`prototype/styles/theme-neutral.css:70` 一行三槽）逐值搬来。原型的 `--warn` 在'
      + '`app.css` 用 **8 次**。实现里 `--af-warn-*` 走的是 `--p-queued*`（两者原型同值），'
      + '另有一族 `--p-warn-c` 专供 `.af-tag--warn`——**裸三兄弟因此空转**，但它们是'
      + '**配色数据本身**：删了就把「原型 warn 是什么色」从代码里抹掉了，'
      + '`theme-css.test.ts` 也会判「原型有、实现没有」。',
    kin: '--p-warn-{bg,line}、--p-warn-{c,bg-c,line-c}（在役）',
  },
  '--p-warn-bg': {
    why:
      '**字面色值**，原型 `--warn-bg` 逐值搬来（四套 alpha 各异：0.14/0.15/0.10/0.11，'
      + '注：paper 是 0.10、cream 是 0.11，不是同一个值——这正是「每份各自计数」的例子）。'
      + '原型 `app.css` 用 5 次。零消费原因与保留理由同 `--p-warn`。',
    kin: '--p-warn、--p-warn-line、--p-warn-bg-c（在役）',
  },
  '--p-warn-line': {
    why:
      '**字面色值**，原型 `--warn-line` 逐值搬来（0.32/0.34/0.28/0.28）。'
      + '原型 `app.css` 用 4 次。零消费原因与保留理由同 `--p-warn`。',
    kin: '--p-warn、--p-warn-bg、--p-warn-line-c（在役）',
  },
}

/** ② 原型槽位对照表 / 尺度别名层的成员，`prototype-parity.test.ts` 要求必须存在。 */
const PROTOTYPE_ALIAS_LAYER: Record<string, OrphanReason> = {
  '--p-s7': {
    why:
      '原型间距 8 档里的**第 7 档**（`prototype/assets/tokens.css:33`：`--s7: 40px`）。'
      + '`tokens.css:302` 起的注释是那张「原型 s1..s8 ↔ 实现 space-1..14」对照表，'
      + '本行 `var(--af-space-12) /* 40px */` 就是表里的一格。'
      + '`prototype-parity.test.ts` 的 `SIZING_ALIASES` **显式要求它存在**'
      + '（注释：少一条就是原型骨架的规则会静默丢样式）。原型自己 `app.css` 零消费，'
      + '实现也零消费——但删了就是对照表挖个洞 + 门禁判红。',
    kin: '--p-s1..s6, --p-s8（在役）',
  },
  '--p-r-lg': {
    why:
      '原型圆角 5 档里的第 4 档（`prototype/assets/tokens.css:35`：`--r-lg: 14px`）。'
      + '**原型 `app.css` 用 2 次**，都在 `:1152` 的 `.af-navpanel` 一行里'
      + '（`border-radius: var(--r-lg) var(--r-lg) 0 0`，导航面板的上两角）——'
      + '不是抽屉：`.af-drawer` 块（`app.css:559-569`）里只有 `--r-md` / `--r-sm`。'
      + '`SIZING_ALIASES` 显式要求存在。'
      + '注意它与 `--p-r-md` 都指向 `--af-radius-lg`（两档压成一档）——'
      + '**这个压缩决定没有留下理由注释**，是这条守卫发现的既有欠账之一。',
    kin: '--p-r-{xs,sm,md,full}（在役）',
  },
  '--p-t-lg': {
    why:
      '原型字号 `--t-lg: 18px` 的传名（原型 `app.css` 用 1 次）。`SIZING_ALIASES` 要求存在。'
      + '**它当前零消费本身就是一条证据**：`tokens.css:339` 的行尾注释写着「18：页面标题」，'
      + '而 `prototype-shell.css:360` 实测得出的结论正好相反——「用 display 档（22px）'
      + '而不是原来的 18px：实测 18px 压不住 14.5px 正文」。'
      + '**这两句注释对「18px 该不该当页面标题」给了相反答案**，是既有欠账；'
      + '`--p-t-lg` 没人读，正是这个矛盾留下的物证。'
      + '注意 `:360` 讲的是 `.af-head__title` 这一个选择器，**不是** `--p-t-lg` 的替换史'
      + '（两者容易混：`--p-t-lg` 是 18px，`--af-text-lg` 是另一条 15px，别混为一谈）。',
    kin: '--p-t-{2xs,xs,sm,md}（在役）',
  },
  '--p-t-xl': {
    why:
      '原型字号 `--t-xl: 23px` 的传名（原型自己 `app.css` 零消费）。'
      + '`SIZING_ALIASES` 要求存在；行尾注释 `/* 23：概览数字 */` 记录了用途。',
    kin: '--p-t-{2xs,xs,sm,md}（在役）、--p-t-lg',
  },
  '--p-lh-tight': {
    why:
      '原型 `--lh-tight: 1.45`（`prototype/assets/tokens.css:28`）的传名。`SIZING_ALIASES` 要求存在。'
      + '原型自己零消费（原型行高一律写 `--lh-body`），实现行高一律写 `--af-leading-*`，'
      + '于是这层也没人读——但它是那张「原型行高两档 ↔ 实现两档」的对照里的一条。',
    kin: '--p-lh-body（在役）',
  },
  '--p-sans': {
    why:
      '原型 `--sans`（`prototype/assets/tokens.css:16-17`，`body` 那一处 `font-family`）的传名。'
      + '`SIZING_ALIASES` 要求存在。实现 `body` 直接写 `--af-font-sans`，所以这层空转——'
      + '但 `tokens.css:38-76` 整段在讲「上一版把 sans 误接成 mono」的事故，'
      + '这两条别名是那段论述的落点。',
    kin: '--p-mono-c（同组）',
  },
  '--p-mono-c': {
    why:
      '原型 `--mono`（`prototype/assets/tokens.css:18`，原型 `app.css` 用 11 次）的传名，'
      + '`-c` 后缀是「避免与 `--af-*` 撞名」的既有约定。`SIZING_ALIASES` 要求存在。'
      + '实现里等宽一律写 `--af-font-mono`（全仓 `var()` 引用 **15 处**：`components.css` 6、'
      + '`prototype-shell.css` 2，另 6 个 `.vue` 各 1-2 处——不含 `tokens.css:357` 这条'
      + '定义右侧，也不含 2 处注释提及），这层代理因此跑了空车。',
    kin: '--p-sans（同组）',
  },
}

/** ③ 原型**没有**这一档，是实现自己加的；注释记录了「为什么加 / 为什么不取邻近值」。 */
const IMPLEMENT_ONLY_TIERS: Record<string, OrphanReason> = {
  '--af-text-lg': {
    why:
      '`tokens.css:91` 行尾注释：`/* 面板标题（原型无对应档）*/`——'
      + '**「原型无对应档」这件事本身就是设计判断的留痕**。'
      + '原型 `--t-*` 只有 6 档（最大 `--t-xl: 23px`），没有 15px。'
      + '更有力的证据在 `tokens.css:115`：`--af-text-card` 之所以不取 15px，理由正是'
      + '「与 `--af-text-lg` 撞档，二者语义不同不能同值」——**删了这个 token，'
      + '另一处取值的理由就失去指涉对象**，下一个人会重新发明一个 15px。'
      + '注意 `docs/design-system.md:1908` 有句过时记载称看板页标题用它，实测零消费，'
      + '那句已在 G93 那批里核对过。',
    kin: '--af-text-{2xs,xs,sm,md,xl,2xl,3xl}（在役 7 档）',
  },
  '--af-text-4xl': {
    why:
      '`tokens.css:95` 行尾注释：`/* 原型总览 hero（原型无对应档）*/`；'
      + '段头 `:82-84` 补充「多出的三档（-lg/-3xl/-4xl）原型没有对应槽位……'
      + '它们不参与『与原型逐档相等』的断言」。未说明为何是 34px（既有欠账）。',
    kin: '--af-text-3xl（在役，登录页标题）',
  },
  '--af-space-14': {
    why:
      '`--af-space-1..13` 之外的**第 14 档**（72px）。原型间距只到 `--s8: 56px`，'
      + '所以这是实现自加的档位，且**没有留取值理由**（同组 `--af-text-lg/-4xl` 至少写了'
      + '「原型无对应档」）。保留而不是删：间距是同一条 2px 基数阶梯，13 档在役、'
      + '删第 14 档会让「14 档」这个被 `tokens.css:144` 段头写明的计数变成 13。'
      + '**若要收，应连同段头「14 档」一起改**，而不是只删一行。',
    kin: '--af-space-1..13（在役 13 档）',
  },
  '--af-radius-none': {
    why:
      '`border-radius: 0` 的具名档。原型圆角从 `--r-xs: 4px` 起、没有 0 档。'
      + '保留理由：`tokens.css:183-184` 段头写「四档，且刻意小」，'
      + '而区块里实际 5 条（none/sm/md/lg/full）——**段头与实况的差**是既有欠账，'
      + '记在这里而不是靠删掉第 5 条来「让数字对上」（那是照文档改代码，AGENTS.md 禁）。'
      + '显式 0 档的用处是把「这里刻意不要圆角」写成可 grep 的意图，而不是漏写。',
    kin: '--af-radius-{sm,md,lg,full}（在役 4 档）',
  },
  '--af-z-shell': {
    why:
      'z 序阶梯的最外一层（`/* 侧栏、顶栏 */`）。同族 `sticky(20)/drawer(40)/modal(60)/'
      + 'help(80)` 四档**全部在役**。`tokens.css:211-213` 记着同族两条被删的先例：'
      + '`--af-z-palette(70)` / `--af-z-toast(90)` **是随它们的消费族整族删除**'
      + '（`.af-palette*` / `.af-toast*` 零模板消费者）。本条不同：`.af-rail` / `.af-head`'
      + '仍在，只是它们的 `z-index` 写成了裸字面量——**档位该用而没用，不是档位该删**。'
      + '保留并登记：将来收「裸字面量 → token」时它是目标的入口。',
    kin: '--af-z-{sticky,drawer,modal,help}（在役 4 档）',
  },
  '--af-dur-slow': {
    why:
      '动效三档的第三档（240ms，`/* 抽屉、布局变化 */`）。原型**没有任何时长令牌**'
      + '（只有 `--ease`），所以这是实现自加的档。'
      + '被 `visual-contract.test.ts:712` 钉住值（`=== "240ms"`），reduce-motion 块里'
      + '覆写为 `0ms` 也被钉住——**它是那条「三档时长都归零」断言的对象**。'
      + '零消费的原因：抽屉实际写 `transition: transform 220ms`（裸值，'
      + '`.af-drawer` 规则里的 `transition: transform 220ms`，`.af-drawer--open` 同值），'
      + '是「该用而没用」。',
    kin: '--af-dur-{instant,fast}（在役 2 档）',
  },
  '--af-add-ink-strong': {
    why:
      'diff 两侧前景的「强」档，与 `--af-add-ink` 转发到**同一个真身** `--p-add-ink-c`。'
      + '原型没有 `--add-ink-strong`（只有 `--add`）。'
      + '**这条是本轮发现的真欠账**：与 `--af-add-ink` 值相同、零注释解释为何分两档。'
      + '保留（同族 `--af-add-ink` 在役于 `.af-gdiff__line[data-kind=add]`，'
      + '强/弱两档是 diff 高亮的既定语言），但「为什么两档同值」需要后续补理由。',
    kin: '--af-add-ink（在役）',
  },
  '--af-del-ink-strong': {
    why:
      '同 `--af-add-ink-strong`：与 `--af-del-ink` 同真身（`--p-del-ink-c`），原型无此槽位，'
      + '零注释解释分档理由。同族 `--af-del-ink` 在役于 `.af-gdiff__line[data-kind=del]`。',
    kin: '--af-del-ink（在役）',
  },
  '--af-danger-ink': {
    why:
      '原型没有 `danger` 名号（对应概念叫 `--del`），本槽转发到 `--p-del-ink-c`。'
      + '同族四个在役，**按类拆开的 `var()` 消费数**：`--af-danger` 8 次、`-fg` 14 次、'
      + '`-bg` 9 次、`-line` 9 次；`-ink` 0 次。（不要把这四个数与「族总 40 次」并列——'
      + '40 是含 `-ink` 在内的全族出现次数，并列会让人误以为 `-ink` 也有一份。）'
      + '保留：它是「danger 的墨色（diff/代码里的深色变体）」'
      + '这一命名意图的落点；删了之后 `-ink` 这个后缀在 danger 族里就没有先例可循。',
    kin: '--af-danger 8 / -fg 14 / -bg 9 / -line 9（`var()` 计数，四份主题定义不计）',
  },
  '--af-border-disabled': {
    why:
      '`theme-neutral.css:142` 行前注释：`/* 原型无 disabled 描边：与 fg-disabled 同理，'
      + '接最弱一档边界 */`——**注释明写了这是一个推断出来的槽位**（原型没给，'
      + '按语义接到 `--p-line-2`）。同族 `subtle/default/strong/focus` 四档在役。'
      + '保留：这是「原型缺哪一档、我们按什么原则补的」的**唯一书面记录**。'
      + '同族里 `--af-fg-disabled` 全仓 `var()` 14 次（`components.css` 10 + 4 个 `.vue`），'
      + '说明 disabled 态本身是存在的。',
    kin: '--af-border-{subtle,default,strong,focus}（在役 4 档）',
  },
  '--af-border-selected': {
    why:
      '`theme-neutral.css:144` 行前注释：`/* 原型无 selected 边界：选中行用的就是'
      + '强调色的弱版 */`——同样是「推断出来的槽位」的书面记录，转发到 `--p-acc-line`。'
      + '同族四档在役。选中态本身有实现（`aria-selected` 系列），只是边界色没走这个 token。',
    kin: '--af-border-{subtle,default,strong,focus}（在役 4 档）',
  },
}

/* 三张表合成一张：守卫只关心「名字 → 理由」。 */
const DECLARED_ORPHANS = new Map<string, OrphanReason>([
  ...Object.entries(PROTOTYPE_COLOR_SLOTS),
  ...Object.entries(PROTOTYPE_ALIAS_LAYER),
  ...Object.entries(IMPLEMENT_ONLY_TIERS),
  ...Object.entries(THEME_SOURCE_SLOTS),
  ...Object.entries(PROTOTYPE_SCALE_BRIDGES),
])

/* ── 已退役的孤儿：曾经零消费者、后来补上了消费者 ────────────────────────
   为什么需要这张表：`--af-fg-strong` 原本在 `PROTOTYPE_COLOR_SLOTS` 里，
   理由写的是「原型用 33 次、实现绕过门面层」。G92 的处置就是把它补回来
   （`components.css` 5 处 + `prototype-shell.css` 10 处 + `NewProjectPage.vue`
   2 处，共 17 处改走 `var(--af-fg-strong)`），
   于是它不再是孤儿，按 ③ 号用例的要求从 `DECLARED_ORPHANS` 移除。

   **别把它和 `--af-term-ansi-bright-white` 混淆**：后者是
   `--af-term-ansi-bright-white: var(--p-fg-strong)`（四份主题各一行），
   消费的是 `--p-fg-strong`，**不是** `--af-fg-strong`。名字里都有 `fg-strong`
   但这个转发不构成对门面层的消费——实测父提交上 `var(--af-fg-strong)`
   出现 **0 次**。这类「相邻行名字相似」很容易被读成「它早就有人用了」。

   但「移除」不等于「这件事没发生过」。这张表把退役记录下来，并用一条用例
   钉住它们**当前确实有消费者**——否则有人把那些 `var()` 改回 `--p-fg-strong`，
   它就会悄悄回到孤儿状态，而 ③ 号用例此时只会说「表里没这个名字」，
   不会提醒补登记。**退役也要有人看着**，这是 G92 留下的教训。 */
const RETIRED_ORPHANS: Record<string, string> = {
  '--af-fg-strong':
    'G92 补消费者后退役：原型 `--fg-strong` 在 `app.css` 被用 33 次，'
    + '实现里那 17 处曾直接写 `var(--p-fg-strong)` 绕过门面层'
    + '（`components.css` 5 + `prototype-shell.css` 10 + `NewProjectPage.vue` 2）。'
    + '改成 `var(--af-fg-strong)` 之后门面层才真的在服役。',
}

describe('零 var() 消费的自定义属性必须逐个登记理由（反向守卫）', () => {
  /** 实际的孤儿集合：定义了、没有任何消费者、且只算 `--af-*` / `--p-*`。 */
  function actualOrphans(): string[] {
    const defined = definedProps()
    const consumed = consumedProps()
    return [...defined.keys()].filter((n) => /^--(af|p)-/.test(n) && !consumed.has(n)).sort()
  }

  it('扫到的源文件数量合理（防止「读成空目录所以永远绿」）', () => {
    // 这条是守卫自己的守卫：`sourceFiles` 若因为路径写错返回空，
    // 下面的孤儿差集与登记表都会「一致地空」，看起来全绿。先钉住规模。
    //
    // **80 是下限不是实测值**：实测 92 个（`src/` 下 159 个源文件减去 67 个
    // `*.test.ts`——测试自己会按名引用 token，算进来会把孤儿全「消费」掉）。
    // 留 12 的余量，是为了删掉几个文件时不误报；写死 92 会让任何一次
    // 文件增减都变成假红灯，那是把守卫变成噪音。
    expect(FILES.length, 'src/ 下扫到的源文件太少，路径可能错了').toBeGreaterThan(80)
    expect(STRIPPED.size).toBe(FILES.length)
  })

  it('判据本身有效：注释里提到 token 名不算消费者', () => {
    /* 这是本文件头讲的那个坑，做成一条独立断言钉住：
       `tokens.css:115` 有一句散文 `也不取 15px：与 --af-text-lg 撞档`。
       若剥注释失效，`--af-text-lg` 会被当成「有消费者」而从孤儿名单里消失，
       于是它悄悄脱管至今。这里直接测 `stripCssAndLineComments` 的行为。 */
    const withComment = '/* 与 --af-NOT-A-CONSUMER 撞档 */\n.a { color: var(--real) }'
    const stripped = stripCssAndLineComments(withComment)
    expect(stripped).not.toContain('--af-NOT-A-CONSUMER')
    expect(stripped).toContain('--real')
    // 行注释同理（`.ts` 里的 `// --af-x` 不算消费）。
    expect(stripCssAndLineComments('// consume --af-fake\nconst x = 1')).not.toContain('--af-fake')
  })

  it('① 没有未登记的孤儿（新增「定义了没人用」的 token 会判红）', () => {
    const undeclared = actualOrphans().filter((n) => !DECLARED_ORPHANS.has(n))
    expect(
      undeclared,
      `这些自定义属性定义了但全仓零 var() 消费，也没登记理由：\n${undeclared.join('\n')}\n`
        + '三选一：① 补消费者；② 删掉；③ 在 DECLARED_ORPHANS 里写明**可证伪**的理由'
        + '（指向原型原文 / 替换史 / 同族在役事实）。不许写「暂未使用」——那是给孤儿开后门。',
    ).toEqual([])
  })

  it('② 登记项都还在代码里（删了 token 却不删理由会判红）', () => {
    const defined = definedProps()
    const stale = [...DECLARED_ORPHANS.keys()].filter((n) => !defined.has(n)).sort()
    expect(
      stale,
      `DECLARED_ORPHANS 里这些名字在代码里已经不存在了：\n${stale.join('\n')}\n`
        + '要么它们被真的删了（那就把理由也删掉），要么名字改了（那理由与代码已经脱节）。',
    ).toEqual([])
  })

  it('③ 登记项都仍是孤儿（被真正消费了就该从表里移除）', () => {
    /* **登记不是永久豁免**：一旦某个 token 重新服役，它就不该再躺在表里——
       否则「登记表」会慢慢变成「所有历史 token 的名册」，反向守卫失去意义。
       这条与 ① 是一对：① 防「新的悄悄进来」，③ 防「旧的赖着不走」。

       判据必须是「**已定义** 且 有了消费者」，不能只写「不在孤儿集合里」：
       token 被整个删掉时它也不在孤儿集合里，那样 ③ 会报「已经重新有消费者了」，
       而真相是「它被删了」——② 才是管那件事的。两条同时红、理由却指错方向，
       是审查维度 18（错误与状态口径）里点名的那类缺陷。 */
    const defined = definedProps()
    const consumed = consumedProps()
    const revived = [...DECLARED_ORPHANS.keys()]
      .filter((n) => defined.has(n) && consumed.has(n))
      .sort()
    expect(
      revived,
      `DECLARED_ORPHANS 里这些名字已经重新有消费者了：\n${revived.join('\n')}\n`
        + '把它们从登记表里移除（豁免应当随重新服役而消失）。',
    ).toEqual([])
  })

  it('每个登记项的理由都不是空话（长度下限 + 禁「暂未使用」类措辞）', () => {
    const BANNED = [/暂未使用/, /暂时没用/, /留着以防/, /以后可能/, /预留/, /^待补/]
    const weak: string[] = []
    for (const [name, { why, kin }] of DECLARED_ORPHANS) {
      if (why.length < 60) weak.push(`${name}: 理由过短（${why.length} 字），多半是在敷衍`)
      if (!kin.trim()) weak.push(`${name}: 没写「同族在役情况」——那是可证伪的关键证据`)
      for (const p of BANNED) if (p.test(why)) weak.push(`${name}: 理由里出现禁用措辞 ${p}`)
    }
    expect(weak, `登记表的理由不合格：\n${weak.join('\n')}`).toEqual([])
  })

  it('④ 退役项确实有消费者（补上的消费者被改回去会判红）', () => {
    /* 见 `RETIRED_ORPHANS` 的注释：退役不等于这件事没发生过。
       如果有人把 `var(--af-fg-strong)` 改回 `var(--p-fg-strong)`，
       它就会**静默**回到孤儿状态——此时 ① 号用例只会说「多了一个未登记的
       孤儿」，不会指认「这是你刚弄坏的」。这条把它接住。

       判据写成「已定义 **且** 有消费者」，不是「不在孤儿集合里」：
       退役项被整个删掉时也不在孤儿集合里，那样这条会静默通过。 */
    const defined = definedProps()
    const consumed = consumedProps()
    const broken = Object.keys(RETIRED_ORPHANS)
      .filter((n) => !(defined.has(n) && consumed.has(n)))
      .sort()
    expect(
      broken,
      `这些名字被登记为「已退役（有消费者）」，但现在没有消费者了：\n${broken.join('\n')}\n`
        + '要么把消费者改回来，要么把它们移回 DECLARED_ORPHANS 重新写明理由。',
    ).toEqual([])
  })
})
