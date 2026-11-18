/* ===========================================================================
   `describeLoginFailure` / `describeSetupFailure` 的**每一个** code 分支。
   ---------------------------------------------------------------------------
   为什么单独一个文件：这两张翻译表是「登录 / setup 出错时用户看到的那一句话」
   的唯一来源（`LoginPage.vue:69` 与 `LoginPrompt.vue:118` 都只做呈现），
   而它们此前只有 `rate_limited` / `login_busy` / `hub_not_initialized`（登录侧）
   与 `setup_token_*` 有网——`auth.ts` 行覆盖 86.62%，未覆盖行
   134, 188-192, 206-216, 278-282。

   对照证据：整段删掉 `case 'rate_limited'` 会判红（第3轮 P18），说明这套文案
   本来可以被守住，缺的只是「每条 case 各一条」。所以这里逐条钉：

     `invalid_request_body`（auth.ts:205-210）——登录侧，请求体没读懂
     `internal_error`（auth.ts:211-216）——登录侧，Hub 内部错
     `hub_not_initialized`（auth.ts:275-282）——**setup 侧**那一条（与登录侧
     的同名 code 是两句不同的话，见下）

   ── 断言的是整个对象，不是 `toContain('Hub')`
   `AuthFailure` 有四个字段，其中 `retryable` 与 `goTo` 是**行为**：它们决定
   按钮还在不在、人要不要被送走。`auth.ts:146-152` 专门论证过
   「`retryable: false` 的那几种再按一次也不会变」。只断言 `next` 的片段
   会漏掉「文案对、`retryable` 被改错」这一类——那等于把按钮留在那儿换来
   一轮瞎试。所以每条都 `toEqual` 整个对象。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import { describeLoginFailure, describeSetupFailure } from '@/shared/auth'
import { ApiError } from '@/shared/http'

describe('describeLoginFailure：此前无网的三个分支', () => {
  it('invalid_request_body：说清「请求体没被读懂」+ 下一步是刷新重填', () => {
    // `auth.ts:205-210`。这一条**不是** `invalid_credentials`：把两者混成
    // 一句「登录失败」会让用户反复改密码，而问题在请求形状。
    expect(describeLoginFailure(new ApiError(400, 'invalid_request_body'))).toEqual({
      code: 'invalid_request_body',
      next: '请求体没被 Hub 读懂（形状不对或超长）。刷新页面后重填。',
      retryable: true,
    })
  })

  it('internal_error：指向 Hub 日志，且与「密码错」区分开', () => {
    // `auth.ts:211-216`。「看一眼 afhub 的日志」是这一条特有的下一步：
    // 改密码不会有任何帮助，且用户无从自己判断。
    expect(describeLoginFailure(new ApiError(500, 'internal_error'))).toEqual({
      code: 'internal_error',
      next: 'Hub 内部出错（详情只在 Hub 日志里）。看一眼 afhub 的日志再试。',
      retryable: true,
    })
  })

  it('两条都不是 default 兜底（形状与 `登录没有成功…` 不同）', () => {
    // default 会写成 `登录没有成功，Hub 回的是 500 internal_error。`——
    // 上面两条 `toEqual` 已经排除了它，这里再把 default 的**形状**钉一句：
    // 否则「把某条 case 删掉」只会表现成 diff 不匹配，而看不出根因是丢了分支。
    const dflt = describeLoginFailure(new ApiError(500, 'a_code_nobody_translated'))
    expect(dflt.next).toBe('登录没有成功，Hub 回的是 500 a_code_nobody_translated。')
    for (const code of ['invalid_request_body', 'internal_error']) {
      expect(describeLoginFailure(new ApiError(500, code)).next).not.toBe(
        `登录没有成功，Hub 回的是 500 ${code}。`,
      )
    }
  })
})

describe('describeSetupFailure：setup 侧的 hub_not_initialized', () => {
  it('hub_not_initialized：说「Hub 的初始化状态读不出来」，并指明这不是表单填错', () => {
    // `auth.ts:275-282`。与 `describeLoginFailure` 的同名 code
    // （`auth.ts:178-184`，说的是「Hub 还没设过密码，先去 setup」）**不是一回事**：
    // setup 页上出现这个码意味着 Hub 侧读状态失败，让用户「去 setup」是把他
    // 送去他自己已经在的那一页。
    expect(describeSetupFailure(new ApiError(500, 'hub_not_initialized'))).toEqual({
      code: 'hub_not_initialized',
      next: 'Hub 的初始化状态读不出来。看 afhub 日志——这是 Hub 侧的问题，不是表单填错了。',
      retryable: true,
    })
  })

  it('它与登录侧的同名 code 是两句不同的话（不许合并）', () => {
    // 这条是本组存在的核心理由。`auth.ts:9-17` 把「Hub 未初始化 → 去 setup」
    // 定死为登录侧的处置；setup 页上再给同一个说法就成了循环。
    const loginSide = describeLoginFailure(new ApiError(403, 'hub_not_initialized'))
    const setupSide = describeSetupFailure(new ApiError(500, 'hub_not_initialized'))

    expect(loginSide.next).not.toBe(setupSide.next)
    // 登录侧才带 `goTo: '/setup'`；setup 侧不该把人往 setup 送
    expect(loginSide.goTo).toBe('/setup')
    expect(setupSide.goTo).toBeUndefined()
    // 两边的分支都真的命中了（而不是各自掉进 default）
    expect(loginSide.next).toContain('还没设过密码')
    expect(setupSide.next).toContain('初始化状态读不出来')
  })

  it('internal_error（setup 侧）：说清「密码没有被写进去、Hub 仍停在初始化模式」', () => {
    // `auth.ts:283-288`。这一句比登录侧的 internal_error 多一件事：
    // 它保证**密码没落库**，所以重试是安全的。少了这半句，用户会不敢重试。
    expect(describeSetupFailure(new ApiError(500, 'internal_error'))).toEqual({
      code: 'internal_error',
      next: 'Hub 内部出错（详情只在 Hub 日志里）。密码**没有**被写进去，Hub 仍停在初始化模式。',
      retryable: true,
    })
  })
})
