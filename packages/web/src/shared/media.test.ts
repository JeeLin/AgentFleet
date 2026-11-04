import { afterEach, describe, expect, it, vi } from 'vitest'
import { readdirSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { NARROW_MAX_PX, NARROW_QUERY, watchMediaQuery } from '@/shared/media'
import { readRepoFile } from '@/test/css'

/*
  这些用例锁的是「断点只有一个字面量」这条契约的两端：
   1. 常量与查询串一致；
   2. 样式表里的媒体查询与常量一致。

  第 2 条是真正会漂的那条——CSS 的媒体查询 JS 读不到，所以 900 这个数
  天然有两份。漂开的后果：JS 认为窄屏、CSS 认为宽屏，于是抽屉叠在对话上
  却按并列算宽度；或者反过来，宽屏下抽屉抢走了焦点。
*/

const realMatchMedia = window.matchMedia

afterEach(() => {
  window.matchMedia = realMatchMedia
})

/** 一个可控的 MediaQueryList：`set()` 触发 change 事件。 */
function fakeMql(initial: boolean) {
  const listeners = new Set<(e: MediaQueryListEvent) => void>()
  return {
    matches: initial,
    media: NARROW_QUERY,
    addEventListener: (_: string, fn: (e: MediaQueryListEvent) => void) => {
      listeners.add(fn)
    },
    removeEventListener: (_: string, fn: (e: MediaQueryListEvent) => void) => {
      listeners.delete(fn)
    },
    emit(matches: boolean) {
      this.matches = matches
      for (const fn of listeners) fn({ matches } as MediaQueryListEvent)
    },
    get listenerCount() {
      return listeners.size
    },
  }
}

describe('media：断点只有一处字面量', () => {
  it('查询串由常量拼出来，不是另写一个 900', () => {
    expect(NARROW_MAX_PX).toBe(900)
    expect(NARROW_QUERY).toBe('(max-width: 900px)')
    // 反向验证：把 NARROW_QUERY 改成手写的 '(max-width: 901px)' → 这条红
    expect(NARROW_QUERY).toContain(String(NARROW_MAX_PX))
  })

  it('样式表里的窄屏断点与 NARROW_MAX_PX 一致', () => {
    const src = readRepoFile('packages/web/src/domains/session/SessionPage.vue')
    const queries = [...src.matchAll(/@media \(max-width: (\d+)px\)/g)].map((m) => Number(m[1]))
    expect(queries.length, '窄屏媒体查询必须存在').toBeGreaterThan(0)
    // 反向验证：把 CSS 的 900 改成 899 → 这条红
    for (const px of queries) {
      expect(px, 'CSS 断点必须等于 NARROW_MAX_PX').toBe(NARROW_MAX_PX)
    }
  })

  /*
    ⚠️ 上面那条**只扫 `SessionPage.vue`**，不是全仓。

    本模块的契约范围就是「窄屏抽屉的那个断点」，而抽屉只长在会话页上。
    `SettingsPage.vue` / `NewSessionPage.vue` 也有 900px 的媒体查询，但那两处
    跟抽屉语义无关，**故意不纳入**——把它们一起扫进来，会让「改设置页的响应式
    断点」无缘无故地把这条窄屏抽屉用例弄红。

    下面这条钉的才是**分布本身**：全仓 `src` 下所有 `.vue` 里出现 900px 断点的文件
    集合。若有人新增 / 删除 / 改了某一处，说明断点分布变了，`media.ts` 文件头
    那段「本文件不是全仓唯一源」的说明需要跟着复核。

    ── 为什么必须真扫目录，不能写死一个文件清单 ────────────────────────
    初版写成 `const files = [...三个文件...]` 再断言 `files.filter(...)` 全中。
    那种写法只能发现「这三个里少了一处」，**发现不了「第四个文件新增了一处」**——
    而后者正是本条要防的。文件名含「全仓」而实现只查清单，是名不符实。
    现在用 `readdirSync` 真扫，第四处新增会让 `toEqual` 红。

    ── 扫描范围：`src/` 下全部 `.vue`，按文件名（basename）比对 ──────────
    为什么是「全 `src/` 递归」而不是只扫 `pages/`：`.vue` 不只长在 `pages/` 下，
    `src/App.vue` 与 `src/components/*.vue` 同样是样式落点。只扫 `pages/`
    仍然漏得掉「在 `App.vue` 里新增一处」。为什么按 basename 而不按相对路径：
    断言要钉的是「哪几个文件含这个断点」，路径前缀只会让期望值随目录搬迁而无谓地红，
    而那与断点分布无关。

    ⚠️ 扫描集合是**按 `.vue` 后缀**筛出来的：`src/domains/session/SessionPage.test.ts` 里也
    有这个字面量，但它不是 `.vue`，不在集合内——这是预期的，测试文件不该算作
    「全仓断点分布」的一部分。
  */
  it('全仓 .vue 里出现 900px 断点的文件集合就是这三个（新增第四处会红）', () => {
    const webSrc = resolve(dirname(fileURLToPath(import.meta.url)), '..')
    const withBreakpoint = walkVue(webSrc)
      .filter((p) => readFileSync(p, 'utf8').includes('@media (max-width: 900px)'))
      .map((p) => p.slice(webSrc.length + 1))
      .sort()

    // 反向验证：在 `src/` 下任一 .vue（含 `App.vue` / `components/`）新增一句
    // `@media (max-width: 900px)` → 这条红；把任一现有文件的 900 改成 899 →
    // 这条也红（该文件从集合里消失）。
    expect(withBreakpoint, '全仓（src/ 下所有 .vue）的 900px 断点就这三处').toEqual([
      'domains/session/NewSessionPage.vue',
      'domains/session/SessionPage.vue',
      'pages/SettingsPage.vue',
    ])
  })
})

/** `root` 下所有 `.vue` 文件的绝对路径（递归，跳过 node_modules）。 */
function walkVue(root: string): string[] {
  const out: string[] = []
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (entry.name === 'node_modules') continue
    const full = join(root, entry.name)
    if (entry.isDirectory()) out.push(...walkVue(full))
    else if (entry.name.endsWith('.vue')) out.push(full)
  }
  return out
}

describe('media：watchMediaQuery', () => {
  it('初值取当前的 matches', () => {
    window.matchMedia = vi.fn(() => fakeMql(true)) as unknown as typeof window.matchMedia
    const { matches, stop } = watchMediaQuery(NARROW_QUERY)
    expect(matches.value).toBe(true)
    stop()
  })

  it('媒体查询变化时跟着变', () => {
    const mql = fakeMql(false)
    window.matchMedia = vi.fn(() => mql) as unknown as typeof window.matchMedia
    const { matches, stop } = watchMediaQuery(NARROW_QUERY)
    expect(matches.value).toBe(false)

    mql.emit(true)
    expect(matches.value).toBe(true)

    mql.emit(false)
    expect(matches.value).toBe(false)
    stop()
  })

  it('stop() 之后不再跟着变（解除订阅，不是只置一个标志）', () => {
    const mql = fakeMql(false)
    window.matchMedia = vi.fn(() => mql) as unknown as typeof window.matchMedia
    const { matches, stop } = watchMediaQuery(NARROW_QUERY)
    stop()
    expect(mql.listenerCount, 'stop 必须真的 removeEventListener').toBe(0)

    const before = matches.value
    mql.emit(true)
    expect(matches.value, 'stop 之后不该再被通知').toBe(before)
  })

  it('没有 matchMedia 时按宽屏走，不抛错', () => {
    // 反向验证：把这里改成 throw → 这条红（SSR / 老环境会整页崩）
    ;(window as unknown as { matchMedia?: unknown }).matchMedia = undefined
    const { matches, stop } = watchMediaQuery(NARROW_QUERY)
    expect(matches.value).toBe(false)
    stop()
  })
})
