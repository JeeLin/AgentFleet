/* ===========================================================================
   tokens.css 与设计文档的守恒式（v0.8.1 第5轮 · 从 `shared/keynav.test.ts` 搬来）
   ---------------------------------------------------------------------------
   这条守卫量的是 `styles/tokens.css` 里的变量定义数，却曾经住在
   `src/shared/keynav.test.ts:110`——**守卫与被守对象不在同一个域**。代价是
   实打实的：本轮验证时只跑 `vitest run src/styles` 就判一个探针「漏网」，
   跑全量才发现它是红的（那次变异在 `tokens.css` 里造了第二个同值的字号档）。

   搬到这里之后「改 `tokens.css` 就跑 `src/styles/`」重新成立：守它的用例与
   文件本身在同一个目录。**`docs/design-system.md` 的那个数字不归这个文件改**：
   它是设计文档的声明，由文档同步那条线维护；本用例只比对「两边是否相等」，
   不相等就红——换了口径（搬走颜色、加字号档）就得两边一起改，这正是它存在
   的意义。

   ⚠️ 用户或平台跑 `vitest run <某个目录>` 时仍可能漏掉别处的守卫；
   全量套件才是判据。这一条只是把「最容易一起被想起来的那一对」放在一起。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

/** 本文件在 `packages/web/src/styles/`，往上**四级**才是仓库根。 */
const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../..')
const DESIGN_DOC = resolve(REPO_ROOT, 'docs/design-system.md')
// 校验我们**自己维护**的资产，不是原型那一份：原型是上游参照，
// 它的数字变了不该让前端门禁变红，反之我们的资产漂了就该红。
const TOKENS_CSS = resolve(REPO_ROOT, 'packages/web/src/styles/tokens.css')

/** `tokens.css` 里 `--af-*` 的定义行数（一行一条）。 */
function tokenDefinitions(): number {
  return (readFileSync(TOKENS_CSS, 'utf-8').match(/^\s*--af-[a-z0-9-]+:/gm) ?? []).length
}

/** 文档 §2 声称的变量数。 */
function declaredInDoc(): number {
  const doc = readFileSync(DESIGN_DOC, 'utf-8')
  const m = doc.match(/（(\d+) 个变量）/g)
  expect(m, 'design-system.md §2 不再声明 token 变量数').not.toBeNull()
  return Number(/(\d+)/.exec(m?.[0] as string)?.[1])
}

describe('tokens.css 的变量数与设计文档 §2 一致', () => {
  it('tokens.css 的变量数与 §2 的「（64 个变量）」一致', () => {
    // v0.8.1 起颜色搬进 `theme-*.css`，`tokens.css` 只剩尺度/字体/动效/层级。
    // 所以这个数从 136 降到 62——**不是丢东西**，是 74 条颜色定义换了住处。
    // 子任务 12 又加了 `--af-text-display` / `--af-text-card` 两档字号，62 → 64。
    // 这条门禁守的仍然是「文档写的数与文件里的定义行数必须相等」，
    // 换了口径就得两边一起改，这正是它存在的意义。
    expect(tokenDefinitions()).toBe(declaredInDoc())
  })

  it('两边的数都不是 0（防止「两个空值相等」把守恒式蒙过去）', () => {
    // 反向担保：正则被改坏（例如把 `--af-` 前缀写错）时上一条会 0 == 0 恒绿。
    // 这里钉住「文件里真的有一批 token、文档里真的声明了一个正数」。
    expect(tokenDefinitions(), 'tokens.css 里一个 --af-* 都没数到').toBeGreaterThan(50)
    expect(declaredInDoc(), 'design-system.md §2 声明的变量数不是正数').toBeGreaterThan(50)
  })
})
