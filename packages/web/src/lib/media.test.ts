import { afterEach, describe, expect, it, vi } from 'vitest'

import { NARROW_MAX_PX, NARROW_QUERY, watchMediaQuery } from '@/lib/media'
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
    const src = readRepoFile('packages/web/src/pages/SessionPage.vue')
    const queries = [...src.matchAll(/@media \(max-width: (\d+)px\)/g)].map((m) => Number(m[1]))
    expect(queries.length, '窄屏媒体查询必须存在').toBeGreaterThan(0)
    // 反向验证：把 CSS 的 900 改成 899 → 这条红
    for (const px of queries) {
      expect(px, 'CSS 断点必须等于 NARROW_MAX_PX').toBe(NARROW_MAX_PX)
    }
  })
})

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
