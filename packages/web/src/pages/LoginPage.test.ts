/* ===========================================================================
   LoginPage 的测试：只钉三件事
   ---------------------------------------------------------------------------
   ① **五种错误分开呈现**（§4.7）——密码错 / 未初始化 / 会话失效 / 429 /
      Hub 正忙的下一步动作不同，压成一句「登录失败」就是把判断还给用户瞎猜。
   ② 成功后去哪：`INDEX_PATH`（`/sessions`），不是某个具体会话。
   ③ 无障碍与「不画假的」：label 挂得上、密码框是 `type=password` 且带
      autocomplete、错误是 `role=alert`、提交期间只有一句「验证中…」
      而没有进度条或提前出现的成功态。

   请求走的是真 `http.ts`（只桩 `fetch`）：认证流程的价值一半在
   「错误码怎么翻成人话」，把它 mock 掉就测不到那条翻译。
   =========================================================================== */

import { afterEach, describe, expect, it, vi } from 'vitest'

import LoginPage from '@/pages/LoginPage.vue'
import { INDEX_PATH, matchRoute, route } from '@/router'
import { settle, stubFetch } from '@/test/http'
import { mountIn } from '@/test/mount'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 填密码并提交。 */
async function login(w: ReturnType<typeof mountIn>, password: string): Promise<void> {
  const input = w.element.querySelector<HTMLInputElement>('#password')
  expect(input, '密码输入框不见了').not.toBeNull()
  input!.value = password
  input!.dispatchEvent(new Event('input'))
  await flush(w)
  await w.element.querySelector<HTMLFormElement>('form')?.requestSubmit()
  await flush(w)
}

/** 把页面摆到 `/login`（含或不含 `?expired=1`）。地址是模块级历史，每条用例自己负责。 */
function atLogin(query = ''): void {
  window.history.replaceState(null, '', `/login${query}`)
  route.value = matchRoute(`/login${query}`)
}

const originalPath = location.pathname

/** 挂载前先把地址摆正：`LoginPage` 会读 `location.search` 判断「会话失效」。 */
function mountLogin(query = ''): ReturnType<typeof mountIn> {
  atLogin(query)
  return mountIn(LoginPage)
}

afterEach(() => {
  route.value = matchRoute(originalPath)
  window.history.replaceState(null, '', originalPath)
})

describe('LoginPage：表单与无障碍', () => {
  it('密码框有 label、type=password、autocomplete=current-password', () => {
    const w = mountLogin()

    const input = w.element.querySelector<HTMLInputElement>('#password')
    expect(input?.type).toBe('password')
    expect(input?.getAttribute('autocomplete')).toBe('current-password')

    // label 的 for 必须真的指到那个输入框（`<label>` 写着 but for 写错等于没有标签）
    const label = w.element.querySelector<HTMLLabelElement>('label[for="password"]')
    expect(label?.textContent).toContain('密码')
    expect(w.element.querySelector(`#${input?.id}`)).toBe(input)
  })

  it('页面上说清「密码不通过邮箱找回」，并指向首次 setup', () => {
    const w = mountLogin()
    const text = w.element.textContent ?? ''
    expect(text).toContain('不通过邮箱找回')
    expect(text).toContain('首次 setup')
  })

  it('空密码时提交按钮禁用（不发一个必然 400 的请求）', () => {
    const w = mountLogin()
    const submit = w.element.querySelector<HTMLButtonElement>('button[type="submit"]')
    expect(submit?.disabled).toBe(true)
  })

  it('没有外壳 chrome：页面上没有侧栏导航', () => {
    const w = mountLogin()
    expect(w.element.querySelector('.af-sidebar')).toBeNull()
    // `main` 是页面的根节点，`tabindex="-1"` 让 `focusMain()` 落得住（§4.3）
    const main = w.element.matches('main') ? w.element : w.element.querySelector('main')
    expect(main?.classList.contains('af-auth')).toBe(true)
    expect(main?.getAttribute('tabindex')).toBe('-1')
  })
})

describe('LoginPage：五种错误分开呈现', () => {
  it('密码错：就地显示、标红输入框、清空密码、留在本页', async () => {
    stubFetch({ '/login': { status: 401, body: { error: 'invalid_credentials' } } })
    const w = mountLogin()
    await login(w, 'wrong-password')

    const alert = w.element.querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('密码不对')
    expect(alert?.textContent).toContain('invalid_credentials')
    // 它**不是**「会话失效」：同一个 401 状态码，两种完全相反的意思
    expect(alert?.textContent).not.toContain('登录态已失效')
    expect(w.element.querySelector<HTMLInputElement>('#password')?.getAttribute('aria-invalid')).toBe('true')
    // 密码清掉：错的密码留在框里既没用，又多留一份明文在 DOM 上
    expect(w.element.querySelector<HTMLInputElement>('#password')?.value).toBe('')
    // 可重试：按钮还在（只是等下一次输入），地址没变
    expect(w.element.querySelector<HTMLButtonElement>('button[type="submit"]')?.disabled).toBe(true)
    expect(location.pathname).toBe('/login')
  })

  it('Hub 未初始化（403 hub_not_initialized）：给出「去 setup」，而不是一句权限错误', async () => {
    stubFetch({ '/login': { status: 403, body: { error: 'hub_not_initialized' } } })
    const w = mountLogin()
    await login(w, 'some-password')

    const note = w.element.querySelector('.af-auth__note--warn')
    expect(note?.textContent).toContain('Hub 还没初始化')
    expect(note?.textContent).toContain('setup')

    // 点它就去 /setup
    await w.element.querySelector<HTMLButtonElement>('.af-auth__note--warn button')?.click()
    expect(location.pathname).toBe('/setup')
  })

  it('429 rate_limited：告诉用户等，而不是「密码错」', async () => {
    stubFetch({ '/login': { status: 429, body: { error: 'rate_limited' } } })
    const w = mountLogin()
    await login(w, 'some-password')

    const text = w.element.querySelector('[role="alert"]')?.textContent ?? ''
    expect(text).toContain('尝试太频繁')
    expect(text).toContain('rate_limited')
    expect(text).not.toContain('密码不对')
  })

  it('429 login_busy：与限流分开说——「Hub 正忙，不是密码的问题」', async () => {
    stubFetch({ '/login': { status: 429, body: { error: 'login_busy' } } })
    const w = mountLogin()
    await login(w, 'some-password')

    const text = w.element.querySelector('[role="alert"]')?.textContent ?? ''
    expect(text).toContain('Hub 正忙')
    expect(text).toContain('login_busy')
  })

  it('会话失效：落地说清「已失效」，不是静默跳转', () => {
    const w = mountLogin('?expired=1')

    expect(w.element.querySelector('[role="status"]')?.textContent).toContain('登录已失效')
  })

  it('未知错误码原样带出，不编一句「登录失败」', async () => {
    stubFetch({ '/login': { status: 500, body: { error: 'some_new_code' } } })
    const w = mountLogin()
    await login(w, 'some-password')

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('some_new_code')
  })
})

describe('LoginPage：成功', () => {
  it('登录成功后去 INDEX_PATH，且不留历史记录', async () => {
    stubFetch({ '/login': { body: { ok: true } } })
    const w = mountLogin()
    const before = history.length

    await login(w, 'correct horse battery')

    expect(INDEX_PATH).toBe('/sessions')
    expect(location.pathname).toBe('/sessions')
    expect(route.value.name).toBe('sessions')
    expect(history.length).toBe(before)
  })

  it('提交期间只有「验证中…」：不画进度条、不先显示成功', async () => {
    let release: (() => void) | null = null
    vi.stubGlobal(
      'fetch',
      vi.fn(
        () =>
          new Promise<Response>((resolve) => {
            release = () => resolve(new Response(JSON.stringify({ ok: true }), { status: 200 }))
          }),
      ),
    )
    const w = mountLogin()

    const input = w.element.querySelector<HTMLInputElement>('#password')
    input!.value = 'correct horse battery'
    input!.dispatchEvent(new Event('input'))
    await flush(w)
    void w.element.querySelector<HTMLFormElement>('form')?.requestSubmit()
    await flush(w)

    const submit = w.element.querySelector<HTMLButtonElement>('button[type="submit"]')
    expect(submit?.textContent).toContain('验证中')
    expect(submit?.disabled).toBe(true)
    // 没有假的加载进度，也没有提前出现的成功态
    expect(w.element.querySelector('[role="progressbar"], progress')).toBeNull()
    expect(w.element.textContent).not.toContain('已登录')

    release!()
    await flush(w)
    expect(location.pathname).toBe('/sessions')
  })
})
