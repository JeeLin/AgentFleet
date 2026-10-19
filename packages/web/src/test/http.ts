/* ===========================================================================
   测试助手：把一个响应体塞进 `fetch`，并清掉上一个用例的残留。
   ---------------------------------------------------------------------------
   为什么不做成「全局 mock fetch」：页面各自声明自己依赖的端点，测试里
   一眼能看到「这一页打了哪几个 URL」。全局 mock 会把「页面偷偷多打了一个
   端点」变成测试仍然绿的事。
   =========================================================================== */

import { afterEach, vi } from 'vitest'

export interface StubResponse {
  status?: number
  body?: unknown
  /** 非 JSON 响应体（例如 204）。 */
  empty?: boolean
}

export type RouteTable = Record<string, StubResponse | (() => StubResponse)>

/**
 * 装一个按 URL 分派的 `fetch`。
 *
 * 未声明的 URL 返回 500 而不是 404：404 在页面里是「资源不存在」的正常
 * 分支，拿它当「测试忘了声明」的兜底会让一条漏声明的请求看起来像产品行为。
 */
export function stubFetch(table: RouteTable): ReturnType<typeof vi.fn> {
  const fn = vi.fn(async (input: RequestInfo | URL) => {
    const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    const path = new URL(raw, 'http://hub.local').pathname
    const entry = table[path]
    if (entry === undefined) {
      return new Response(JSON.stringify({ error: 'unexpected_request' }), {
        status: 500,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    const spec = typeof entry === 'function' ? entry() : entry
    if (spec.empty) return new Response(null, { status: spec.status ?? 204 })
    return new Response(JSON.stringify(spec.body ?? null), {
      status: spec.status ?? 200,
      headers: { 'Content-Type': 'application/json' },
    })
  })
  vi.stubGlobal('fetch', fn)
  return fn
}

/** 每个用例结束时把 fetch 还原，免得第二个文件里的页面拿到上一个用例的桩。 */
afterEach(() => {
  vi.unstubAllGlobals()
})

/** 一次 `tick`：让 `await` 链上的 fetch 与 Vue 的渲染队列都走完。 */
export async function settle(times = 3): Promise<void> {
  for (let i = 0; i < times; i++) await Promise.resolve()
}
