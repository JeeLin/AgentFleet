/* ===========================================================================
   SetupPage 的测试：钉住「三种失败各给下一步命令」
   ---------------------------------------------------------------------------
   最要紧的两条：
   ① `setup_token_mismatch` 与 `setup_token_locked` **不是一回事**——
      前者再取一次 token 就行，后者 token 已经死了（失败 10 次），
      只能重启 Hub（启动时重新生成 token 并清零失败计数）。合并成一句
      「setup 失败」等于让用户拿着一个永远不可能被接受的 token 反复重试。
   ② **页面必须说清 token 从哪来**（stderr / `<data>/setup-token.txt` 0600），
      而且不许假装能自动获取——前端没有、也不该有那条通路。

   ── 为什么要一个「按方法分派」的桩 ────────────────────────────────
   页面挂载时 `GET /api/setup` 探路，提交时 `POST /setup`——两条请求的响应
   必须分开：探路固定「未初始化」（否则表单根本不渲染），提交按用例回不同的
   错误码。`test/http.ts` 的 `stubFetch` 只能给每条路径一份固定响应，所以这里
   用 `mockImplementation` 按方法分派。探路与提交曾经同路径不同方法，路径
   拆开之后按方法分派的写法仍然最省事（探路不该拿到提交那个错误码）。
   =========================================================================== */

import { afterEach, describe, expect, it } from 'vitest'

import SetupPage from '@/pages/SetupPage.vue'
import { LOGIN_PATH, matchRoute, route } from '@/router'
import { settle, stubFetch } from '@/test/http'
import { mountIn } from '@/test/mount'

type Stub = ReturnType<typeof stubFetch>

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

const originalPath = location.pathname

function atSetup(): void {
  window.history.replaceState(null, '', '/setup')
  route.value = matchRoute('/setup')
}

afterEach(() => {
  route.value = matchRoute(originalPath)
  window.history.replaceState(null, '', originalPath)
})

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

/** 探路成功（未初始化）、POST 回 `{ status, error }`。 */
function stubSetup(post: { status: number; error: string } | { ok: true }): Stub {
  const stub = stubFetch({ '/api/setup': { body: { initialized: false } } })
  stub.mockImplementation(async (_input: RequestInfo | URL, init?: RequestInit) => {
    const method = (init?.method ?? 'GET').toUpperCase()
    if (method === 'POST') {
      return 'ok' in post ? json(200, { initialized: true }) : json(post.status, { error: post.error })
    }
    return json(200, { initialized: false })
  })
  return stub
}

/** 填两个框（默认值都能过前端校验）并提交。 */
async function fillAndSubmit(
  w: ReturnType<typeof mountIn>,
  values: { token?: string; password?: string } = {},
): Promise<void> {
  const token = w.element.querySelector<HTMLInputElement>('#setup-token')
  const password = w.element.querySelector<HTMLInputElement>('#setup-password')
  expect(token, 'token 输入框不见了').not.toBeNull()
  expect(password, '密码输入框不见了').not.toBeNull()

  token!.value = values.token ?? 'afst_pasted-from-the-machine'
  token!.dispatchEvent(new Event('input'))
  password!.value = values.password ?? 'correct horse battery'
  password!.dispatchEvent(new Event('input'))
  await flush(w)

  await w.element.querySelector<HTMLFormElement>('form')?.requestSubmit()
  await flush(w)
}

async function submitWith(post: { status: number; error: string } | { ok: true }): Promise<ReturnType<typeof mountIn>> {
  atSetup()
  stubSetup(post)
  const w = mountIn(SetupPage)
  await flush(w)
  await fillAndSubmit(w)
  return w
}

describe('SetupPage：token 从哪来', () => {
  it('页面写明 token 只在 Hub 的 stderr 与 <data>/setup-token.txt（0600）里', async () => {
    atSetup()
    stubFetch({ '/api/setup': { body: { initialized: false } } })
    const w = mountIn(SetupPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('stderr')
    expect(text).toContain('setup-token.txt')
    expect(text).toContain('0600')
    expect(text).toContain('~/.local/share/agentfleet')
    expect(text).toContain('AFHUB_DATA_DIR')
  })

  it('页面明说 token 不能从浏览器取回（不假装能自动获取）', async () => {
    atSetup()
    stubFetch({ '/api/setup': { body: { initialized: false } } })
    const w = mountIn(SetupPage)
    await flush(w)

    expect(w.element.textContent).toContain('不能从浏览器里取回')
    // 页面上没有一个「自动获取 / 帮我取 token」的按钮
    const buttonTexts = Array.from(w.element.querySelectorAll('button')).map((b) => b.textContent ?? '')
    for (const t of buttonTexts) expect(t).not.toMatch(/自动|取回|获取/)
  })

  it('两个输入都有 label；token 也是 password，密码框 autocomplete=new-password', async () => {
    atSetup()
    stubFetch({ '/api/setup': { body: { initialized: false } } })
    const w = mountIn(SetupPage)
    await flush(w)

    expect(w.element.querySelector('label[for="setup-token"]')?.textContent).toContain('token')
    expect(w.element.querySelector('label[for="setup-password"]')?.textContent).toContain('密码')

    const token = w.element.querySelector<HTMLInputElement>('#setup-token')
    const password = w.element.querySelector<HTMLInputElement>('#setup-password')
    // token 也是密码：别让它以明文躺在屏幕上
    expect(token?.type).toBe('password')
    expect(token?.getAttribute('autocomplete')).toBe('off')
    expect(password?.type).toBe('password')
    expect(password?.getAttribute('autocomplete')).toBe('new-password')
    // 下限与 `hub-core/src/auth.rs` 的 `password.len() < 8` 一致
    expect(w.element.textContent).toContain('至少 8 位')
  })

  it('token 或密码不达标时按钮禁用（不提交一个必然 400 的请求）', async () => {
    atSetup()
    stubFetch({ '/api/setup': { body: { initialized: false } } })
    const w = mountIn(SetupPage)
    await flush(w)

    const submit = (): HTMLButtonElement | null =>
      w.element.querySelector<HTMLButtonElement>('button[type="submit"]')
    expect(submit()?.disabled).toBe(true)

    const token = w.element.querySelector<HTMLInputElement>('#setup-token')!
    token.value = 'afst_x'
    token.dispatchEvent(new Event('input'))
    await flush(w)
    // 只有 token、密码还空着
    expect(submit()?.disabled).toBe(true)

    const password = w.element.querySelector<HTMLInputElement>('#setup-password')!
    password.value = '1234567' // 7 位：差一位
    password.dispatchEvent(new Event('input'))
    await flush(w)
    expect(submit()?.disabled).toBe(true)

    password.value = '12345678' // 8 位：到线
    password.dispatchEvent(new Event('input'))
    await flush(w)
    expect(submit()?.disabled).toBe(false)
  })

  it('强度条只提示、不设假门禁：8 位的密码也能提交', async () => {
    atSetup()
    stubFetch({ '/api/setup': { body: { initialized: false } } })
    const w = mountIn(SetupPage)
    await flush(w)

    const password = w.element.querySelector<HTMLInputElement>('#setup-password')!
    password.value = '12345678'
    password.dispatchEvent(new Event('input'))
    await flush(w)

    // 强度只有 1 格（刚过下限），但按钮不该因此禁用
    const meter = w.element.querySelector('[role="meter"]')
    expect(meter?.getAttribute('aria-valuenow')).toBe('1')
    expect(meter?.getAttribute('aria-valuetext')).toContain('最低长度')

    const token = w.element.querySelector<HTMLInputElement>('#setup-token')!
    token.value = 'afst_x'
    token.dispatchEvent(new Event('input'))
    await flush(w)
    expect(w.element.querySelector<HTMLButtonElement>('button[type="submit"]')?.disabled).toBe(false)
  })
})

describe('SetupPage：三种失败分开呈现', () => {
  it('setup_token_mismatch：告诉用户重新取一次，而不是「失败」', async () => {
    const w = await submitWith({ status: 403, error: 'setup_token_mismatch' })

    const alert = w.element.querySelector('#setup-token-error[role="alert"]')
    expect(alert?.textContent).toContain('token 对不上')
    expect(alert?.textContent).toContain('setup_token_mismatch')
    // 出路是「再取一次」，不是「重启 Hub」
    expect(alert?.textContent).toContain('重新取一次')
    expect(alert?.textContent).not.toContain('重启 Hub')
    expect(w.element.querySelector('.af-auth__note--danger')).toBeNull()
  })

  it('setup_token_locked：说清 token 已死，出路是重启 Hub', async () => {
    const w = await submitWith({ status: 403, error: 'setup_token_locked' })

    const note = w.element.querySelector('.af-auth__note--danger[role="alert"]')
    const text = note?.textContent ?? ''
    expect(text).toContain('锁死')
    // 新 token 靠重启后的启动流程重新生成，不是靠一个恢复命令
    expect(text).toContain('重新生成')
    expect(text).toContain('重启 Hub')
    // 与 mismatch 分明：这一条不该再说「重新取一次就行」
    expect(text).not.toContain('重新取一次')

    // 不可重试：token 已被清空（它已经不可能再被接受）
    expect(w.element.querySelector<HTMLInputElement>('#setup-token')?.value).toBe('')
  })

  it('already_initialized：setup 只能做一次，直接去登录', async () => {
    const w = await submitWith({ status: 409, error: 'already_initialized' })

    const note = w.element.querySelector('.af-auth__note--info[role="alert"]')
    expect(note?.textContent).toContain('已经初始化')

    await w.element.querySelector<HTMLButtonElement>('.af-auth__note--info button')?.click()
    expect(location.pathname).toBe(LOGIN_PATH)
  })

  it('password_too_short：错误挂在密码框上，并说出下限', async () => {
    const w = await submitWith({ status: 400, error: 'password_too_short' })

    const text = w.element.querySelector('#setup-password ~ [role="alert"]')?.textContent ?? ''
    expect(text).toContain('至少 8 位')
    expect(
      w.element.querySelector<HTMLInputElement>('#setup-password')?.getAttribute('aria-invalid'),
    ).toBe('true')
  })

  it('setup_token_required：没带 token 与 token 不对分开说', async () => {
    const w = await submitWith({ status: 403, error: 'setup_token_required' })

    const alert = w.element.querySelector('#setup-token-error[role="alert"]')
    expect(alert?.textContent).toContain('没带 setup token')
    expect(alert?.textContent).toContain('setup_token_required')
  })

  it('未知错误码原样带出，不编一句「setup 失败」', async () => {
    const w = await submitWith({ status: 500, error: 'some_new_code' })

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('some_new_code')
  })

  it('内部错误时说清「密码没被写进去，Hub 仍在初始化模式」', async () => {
    const w = await submitWith({ status: 500, error: 'internal_error' })

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('仍停在初始化模式')
  })

  it('Hub 连不上时说明是网络，不是表单填错；探路失败也不假装「一切就绪」', async () => {
    atSetup()
    const stub = stubFetch({ '/api/setup': { body: { initialized: false } } })
    stub.mockImplementation(async () => {
      throw new TypeError('Failed to fetch')
    })

    const w = mountIn(SetupPage)
    await flush(w)
    await fillAndSubmit(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('Hub 连不上')
    expect(text).toContain('没能确认 Hub 的初始化状态')
  })
})

describe('SetupPage：已初始化时不装成能填', () => {
  it('GET /api/setup 回 409 时直接给「去登录」，不留一张填了也白填的表', async () => {
    atSetup()
    stubFetch({ '/api/setup': { status: 409, body: { error: 'already_initialized' } } })
    const w = mountIn(SetupPage)
    await flush(w)

    expect(w.element.querySelector('.af-auth__note--info')?.textContent).toContain('已经初始化')
    // setup 只能做一次：这里不该再出现表单
    expect(w.element.querySelector('form')).toBeNull()
  })
})

describe('SetupPage：成功', () => {
  it('成功后去 /login（不是 /sessions）：setup 不签发 cookie', async () => {
    atSetup()
    stubSetup({ ok: true })
    const w = mountIn(SetupPage)
    await flush(w)

    const before = history.length
    await fillAndSubmit(w)

    expect(location.pathname).toBe('/login')
    expect(route.value.name).toBe('login')
    // 用掉的表单（token 已被作废）不该留在后退路径上
    expect(history.length).toBe(before)
  })

  it('请求真的把 token 放进了 X-Setup-Token 头（两侧空白去掉）', async () => {
    atSetup()
    const calls: Array<{ method: string; token: string | null }> = []
    const stub = stubFetch({ '/api/setup': { body: { initialized: false } } })
    stub.mockImplementation(async (_input: RequestInfo | URL, init?: RequestInit) => {
      const headers = new Headers(init?.headers)
      calls.push({
        method: (init?.method ?? 'GET').toUpperCase(),
        token: headers.get('X-Setup-Token'),
      })
      return json(200, { initialized: true })
    })

    const w = mountIn(SetupPage)
    await flush(w)
    // 粘进来的 token 常带换行与空格
    await fillAndSubmit(w, { token: '  afst_with_spaces  ' })

    const posts = calls.filter((c) => c.method === 'POST')
    expect(posts).toHaveLength(1)
    // 头名必须与 `lib.rs:41` 的 `SETUP_TOKEN_HEADER = "x-setup-token"` 对上
    expect(posts[0]?.token).toBe('afst_with_spaces')
  })

  it('提交期间只有「提交中…」：不画进度条、不先显示成功', async () => {
    atSetup()
    let release: (() => void) | null = null
    const stub = stubFetch({ '/api/setup': { body: { initialized: false } } })
    stub.mockImplementation(
      async () =>
        new Promise<Response>((resolve) => {
          release = () => resolve(json(200, { initialized: true }))
        }),
    )

    const w = mountIn(SetupPage)
    await flush(w)

    const token = w.element.querySelector<HTMLInputElement>('#setup-token')!
    token.value = 'afst_x'
    token.dispatchEvent(new Event('input'))
    const password = w.element.querySelector<HTMLInputElement>('#setup-password')!
    password.value = 'correct horse battery'
    password.dispatchEvent(new Event('input'))
    await flush(w)

    void w.element.querySelector<HTMLFormElement>('form')?.requestSubmit()
    await flush(w)

    const submit = w.element.querySelector<HTMLButtonElement>('button[type="submit"]')
    expect(submit?.textContent).toContain('提交中')
    expect(submit?.disabled).toBe(true)
    expect(w.element.querySelector('[role="progressbar"], progress')).toBeNull()
    expect(w.element.textContent).not.toContain('setup 完成')

    release!()
    await flush(w)
    expect(location.pathname).toBe('/login')
  })
})
