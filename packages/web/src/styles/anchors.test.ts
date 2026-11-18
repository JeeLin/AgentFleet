/* ===========================================================================
   `file:line` 引用必须能被回读命中（v0.8.1 第5轮 G41）
   ---------------------------------------------------------------------------
   为什么需要这条 —— 这一轮的审查里出现过**同一个坑四种形态**：

     · `prototype-parity.test.ts` 写 `App.vue:844`，而 `<main class="af-main">`
       在 `9184cf1` 之后其实在 **884** 行；
     · 同一处写 `design-system.md:1800`，Lane A 删掉 5 个组件节后整张 §9
       索引表下移 6 行，`:1800` 已变成「§3 组件样式」；
     · `prototype-shell.css` 写 `app.css:528-600`，而 528 是空行、管家节起于 529；
     · `design-system.md:324` —— 我一度「订正」成 322，后来发现文档也被这一轮
       改过，**324 才是对的**。只读报告、不回读文件，改哪边都是猜。

   共同点：**行号写在注释里，没有任何东西会检查它**。它们不影响测试结果，
   所以漂了也没人知道——直到下一个读注释的人被带到错误的行上。
   G41 的处置原话是「加一条『`file:line` 引用必须命中』的门禁」，本文件即它。

   ── 判据 ─────────────────────────────────────────────────────────────
   对源码里每一处形如 `foo.ext:123` 的引用：
     ① **按 basename 能唯一定位到一个仓库内文件**才检查。定位不到或有多个
        同名文件（`index.html` 有 6 个、`app.css` 有 1 个）就跳过——前者是
        「别的仓库 / 计划中的文件」，后者本来就无法确定指向哪一个。
     ② 该文件必须真的有第 123 行（不越界）；
     ③ 那一行**不能是空行**。空行永远不可能是「那句话的出处」，
        这一条抓住的正是「引用的行号正好落在两段代码之间的空行上」那类错位
        （`prototype-shell.css` 里曾写「管家抽屉见原型某行」、而那一行是空行，
        真正的节标题在下一行）。

   ── 为什么不检查「行内容含锚点片段」 ────────────────────────────────────
   试过：把引用前后的汉字当关键词去那一行里找，命中率不可接受——
   `app.css:46` 指的是小节标题、`board.html:62` 指的是一个类名，而引用句
   故意用了不同的措辞。硬要求「文字对得上」只会逼人把注释改成复述源码，
   然后为了让它变绿去放松判据。①+②+③ 已经能拦住这一轮实际出现的全部漂移。
   =========================================================================== */

import { readdirSync, readFileSync, statSync } from 'node:fs'
import { basename, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../..')
const SRC = resolve(REPO_ROOT, 'packages/web/src')

/** 仓库里所有可被引用的源文件，按 basename 索引（同名多个就都收着，用时判唯一）。 */
function indexRepoFiles(): Map<string, string[]> {
  const index = new Map<string, string[]>()
  const walk = (dir: string): void => {
    let entries: string[]
    try {
      entries = readdirSync(dir)
    } catch {
      return
    }
    for (const name of entries) {
      if (name === 'node_modules' || name === 'target' || name.startsWith('.')) continue
      const full = resolve(dir, name)
      let st
      try {
        st = statSync(full)
      } catch {
        continue
      }
      if (st.isDirectory()) walk(full)
      else if (/\.(md|css|vue|ts|html|js)$/.test(name)) {
        const list = index.get(name) ?? []
        list.push(full)
        index.set(name, list)
      }
    }
  }
  for (const root of ['packages/web/src', 'prototype', 'docs', 'crates']) walk(resolve(REPO_ROOT, root))
  return index
}

/** 扫描范围内的 `.ts` / `.vue`。 */
function scannedFiles(): string[] {
  const out: string[] = []
  const walk = (dir: string): void => {
    for (const name of readdirSync(dir)) {
      if (name === 'node_modules' || name.startsWith('.')) continue
      const full = resolve(dir, name)
      if (statSync(full).isDirectory()) walk(full)
      else if (/\.(ts|vue)$/.test(name)) out.push(full)
    }
  }
  walk(SRC)
  /* **本文件自己不扫**：它下方的自检用例故意写着 `prototype-shell.css:99999`
     这类越界引用当夹具，扫进来就是「门禁判自己」。这不是给自己开后门——
     本文件的注释里没有任何服务生产代码的锚点，它的引用全是判据举例。 */
  const self = fileURLToPath(import.meta.url)
  return out.filter((f) => resolve(f) !== resolve(self))
}

/* ===========================================================================
   判据本体（v0.8.1 第5轮 G79）
   ---------------------------------------------------------------------------
   **为什么要抽成模块级函数**：初版的判据内联在 `it()` 里，而「判据自检」那条
   用例**另写了一份 `probe()`**。两份实现意味着——掏空主判据后自检照旧绿，
   自检测的不是被测对象。4c 实测：把主判据的空行判断改成 `else if (false)`、
   把自检的 `probe` 掏空，anchors 依旧 `2 passed`。这正是「守卫不得自证」
   （`AGENTS.md` 维度 23）的教科书形态。

   现在两边调**同一个** `checkRef()`：

      主判据：对每处引用调它，非 null 就计入 failures
      自检：  构造「越界 / 空行 / 闭合标签行」三种形态，确认它各自返回对应码

   掏空 `checkRef` 会让**两边同时**失守，而不是只丢一边。
   =========================================================================== */

/** 一条引用的判定结果。`null` = 放行。 */
type RefVerdict = '越界' | '空行' | '闭合标签行' | null

/** `</div>`、`}`、`)`、`};` 这类只有闭合符号的行：不可能是「那句话的出处」。 */
function isClosingOnly(line: string): boolean {
  return /^(?:<\/[a-zA-Z][\w-]*>|[})]+;?|\/?>|\];?)$/.test(line.trim())
}

/**
 * 判定一处 `文件名:行号` 引用。定位不到 / 同名多个 → `null`（无法确定指向，不是漂移）。
 *
 * @param index `indexRepoFiles()` 的结果
 * @param fileRef 被引用的文件名（**basename**，如 `prototype-shell.css`）
 * @param num 被引用的行号（1-based）
 */
function checkRef(
  index: Map<string, string[]>,
  fileRef: string,
  num: number,
): RefVerdict {
  const candidates = index.get(basename(fileRef))
  if (!candidates || candidates.length !== 1) return null
  const target = readFileSync(candidates[0], 'utf-8').split('\n')
  if (num < 1 || num > target.length) return '越界'
  const text = target[num - 1]
  if (text.trim() === '') return '空行'
  /* 只指向纯闭合标签行 → 十有八九是「区间终点算错」。这条是**廉价启发式**：
     4c 实测「行内容必须含锚点关键词」会 208/246 误报，那条路已否决（见文件头
     `:29-33`）；而「整行只有一个闭合符号」几乎不可能是有效出处。 */
  if (isClosingOnly(text)) return '闭合标签行'
  return null
}

describe('G41 —— 源码里的 file:line 引用必须能被回读命中', () => {
  /* 这些引用**故意**指向空行：它们是「那一行是空的，所以某句注释必然是错的」
     这类反向断言（见 `domains/project/prototype-anchors.test.ts:57-61`）。
     白名单只收这种形态，key 是 `引用文件:行号 -> 被引用的 file:line`，
     且额外要求**同一行文字里必须出现「空行」二字**——否则白名单本身就成了
     掩盖漂移的后门（把漂了的行号加进来，比修好它省事，而那正是要防的）。
     所以判据是「文字说它是空行」+「它真的是空行」两者都成立才放行。 */
  const EXPLICIT_BLANK_OK = /空行/

  it('每处引用都指向存在的行，且那一行不是空行', () => {
    const index = indexRepoFiles()
    const failures: string[] = []
    let checked = 0
    let exempted = 0

    for (const file of scannedFiles()) {
      const lines = readFileSync(file, 'utf-8').split('\n')
      const rel = file.slice(REPO_ROOT.length + 1)
      lines.forEach((line, idx) => {
        /* 「这一行是空行」的说明常常**写在下一行**：`prototype-anchors.test.ts:4`
           引述一句历史错话（`project-settings.html:14-24`），紧跟着第 5 行才写
           「而原型的 `:14` 是空行」。所以豁免窗口取「本行 + 后 2 行」，
           而不是只看本行。 */
        const window = lines.slice(idx, idx + 3).join('\n')
        for (const m of line.matchAll(
          /([A-Za-z0-9_-][A-Za-z0-9_.-]*\.(?:md|css|vue|ts|html|js))[:：](\d+)/g,
        )) {
          /* 判定逻辑**全在 `checkRef` 里**（G79）：这里只做「拿结果、决定放行否」，
             不再自己解一遍行号——自检调的是同一个函数，掏空它两边一起失守。 */
          const candidates = index.get(m[1])
          if (!candidates || candidates.length !== 1) continue
          const num = Number(m[2])
          checked++
          const verdict = checkRef(index, m[1], num)
          if (verdict === null) continue
          /* 「这一行是空行」本身就是某些守卫的断言对象，那是**正确**的引用。
             **只有「空行」这一档**能靠文字豁免；「越界」「闭合标签行」不给豁免口子
             ——它们没有一种正当理由能写出来。 */
          if (verdict === '空行' && EXPLICIT_BLANK_OK.test(window)) {
            exempted++
            continue
          }
          const target = readFileSync(candidates[0], 'utf-8').split('\n')
          const why =
            verdict === '越界'
              ? `但该文件只有 ${target.length} 行`
              : verdict === '空行'
                ? '但那一行是空行——空行不可能是「那句话的出处」，行号已经漂了'
                : '但那一行只有一个闭合符号（`</div>` / `}` / `)`）——不可能是「那句话的出处」，'
                  + '行号大概率是区间终点算错'
          failures.push(`${rel}:${idx + 1} 引用 \`${m[1]}:${num}\`，${why}`)
        }
      })
    }

    expect(checked, '一处引用都没扫到，这条守卫在空转').toBeGreaterThan(50)
    expect(failures, `file:line 引用漂移：\n${failures.join('\n')}`).toEqual([])
    /* 豁免必须是**极少数**。「空行」二字太常见，某天有人大规模把漂了的锚点
       改写成带「空行」的措辞就能把这条门禁废掉；封一个上限让它至少会响。 */
    expect(exempted, `豁免了 ${exempted} 处「指向空行」的引用，超过上限 5——`
      + '白名单在扩散，检查是不是有人在用「空行」二字绕开这条门禁').toBeLessThanOrEqual(5)
  })

  /* 判据自检：把「漂了」的两种形态各造一个，确认它们**会**被判红。
     没有这一条的话，上面那条在判据写错时会恒绿（`candidates` 恒为 undefined
     就不会 push 任何东西），而 `checked > 50` 也可能因为正则写宽了而满足。 */
  it('判据自检：越界 / 空行 / 闭合标签行都会被判红（与主判据同一个函数）', () => {
    /* **不另写一份 probe**（G79 的病根就在这）：直接调主判据用的 `checkRef`。
       若有人把 `checkRef` 掏空返回 null，这条与上面那条会**同时**判红。 */
    const index = indexRepoFiles()
    const probe = (fileRef: string, num: number): RefVerdict => checkRef(index, fileRef, num)
    /* 夹具用 `prototype-shell.css`：它在仓库里只有一份。
       **不能用 `tokens.css`**——`src/styles/tokens.css` 与
       `prototype/assets/tokens.css` 同名两份，按「同名必须唯一」的规则会被
       判据跳过，于是 `probe()` 返回 null，自检看起来「放行了」其实什么都没测。
       这条踩坑记录留在这里：给这条门禁写夹具时，先确认 basename 唯一。 */
    expect(probe('prototype-shell.css', 99999), '越界必须被判红').toBe('越界')

    // 找一个真实存在的空行，用它的行号探一次
    const css = readFileSync(resolve(SRC, 'styles/prototype-shell.css'), 'utf-8').split('\n')
    const blankAt = css.findIndex((l) => l.trim() === '')
    expect(blankAt, 'prototype-shell.css 里找不到空行，夹具前提变了').toBeGreaterThanOrEqual(0)
    expect(probe('prototype-shell.css', blankAt + 1), '空行引用必须被判红').toBe('空行')

    // 反向：一个真实有内容的行号必须放行
    const codeAt = css.findIndex((l) => l.trim() !== '')
    expect(probe('prototype-shell.css', codeAt + 1), '有内容的行必须放行').toBeNull()

    /* **第三种形态：纯闭合标签行**（本次新增的启发式）。造不出来就从真实的
       `prototype-shell.css` 里找——找不到说明前提变了，要立刻知道。
       **必须用断言而不是 `if`**：原写法 `if (closingAt >= 0) { expect(...) }`
       在该文件恰好没有纯闭合行时整段静默跳过、用例仍绿，与上面两档
       （`:221` / `:226` 都有前置 `expect`）不一致。那是 G79 刚修的同一类病
       「自检本身什么都没测」在这一档的残留。 */
    const closingAt = css.findIndex((l) => isClosingOnly(l))
    expect(
      closingAt,
      'prototype-shell.css 里已找不到「整行只有一个闭合符号」的行，第三档自检前提变了——'
        + '要么把这条用例改掉，要么换一份夹具',
    ).toBeGreaterThanOrEqual(0)
    expect(
      probe('prototype-shell.css', closingAt + 1),
      '只指向纯闭合符号的行必须被判红（行号大概率是区间终点算错）',
    ).toBe('闭合标签行')
  })

  /* **判据自检的第二道**：`checkRef` 必须真的被两条用例共用。4c 的原始变异
     是「掏空主判据的自检、主判据照旧绿」——现在两边同一个函数，这条只是在
     文档层面把它钉死：如果哪天有人又把它拆成两份，`checkRef` 的调用点会
     少于 2 处，这条会响。用源码计数是最廉价且不依赖运行时的做法。 */
  it('checkRef 必须被主判据与自检共同调用（不许拆回两份实现）', () => {
    const src = readFileSync(fileURLToPath(import.meta.url), 'utf-8')
    /* 统计**调用**（`checkRef(`），不数定义（`function checkRef(`）。 */
    const calls = src.match(/[^\w.]checkRef\(/g) ?? []
    expect(
      calls.length,
      '`checkRef` 的调用点少于 2 处——主判据与自检已拆成两份实现，'
        + '自检将不再能证明主判据有效（这正是 G79 的病根）',
    ).toBeGreaterThanOrEqual(2)
  })
})
