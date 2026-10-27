/* ===========================================================================
   LoginPrompt 的测试：钉四件事
   ---------------------------------------------------------------------------
   ① **401 不再把人扔去登录页**——这个框存在的全部理由。所以每条用例都断言
      `location.pathname` 没变（`/sessions`，也就是弹框前那一页）。
   ② **输对密码 → 关框、原地继续**：不导航、不刷新。背后的页面是同一个 DOM
      实例（这是「恢复原来的操作」的机制，见组件文件头「为什么不重放」）。
   ③ **取消 → 才跳 `/login`**：三条关闭路径（Esc / 点遮罩 / 取消按钮）里只有
      取消这一条 emit `go-login`，另两条只是关框。
   ④ **五种错误分开呈现**（§4.7）：密码错 / Hub 未初始化 / 429 限流 / Hub 忙 /
      未知码，下一步各不相同——压成一句「登录失败」就是把判断还给用户瞎猜。

   请求直接打 `/login`（不走 `http.ts`），这是组件自己的设计（见文件头：
   `http.ts` 把 401 一律算作 session 过期，会把「密码错」变成死循环）。
   =========================================================================== */

import { defineComponent, h, ref } from 'vue'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import LoginPrompt from '@/components/LoginPrompt.vue'
import { depth, pop, push, resetForTest } from '@/lib/focus'
import { matchRoute, route } from '@/router'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 不持有 wrapper 时的 flush（Teleport 出来的框不在 wrapper 树上）。 */
async function flushMount(): Promise<void> {
  for (let i = 0; i < 6; i++) await settle(1)
}

/**
 * 像 `App.vue` 那样**受控地**挂着这个框。
 *
 * 为什么必须有个宿主：`open` 是受控 prop，「关掉」这个动作由父级把 `open`
 * 置回 `false` 完成（`LoginPrompt` 只 emit 请求）。直接 `mountIn(LoginPrompt)`
 * 的话父级永远不会响应，emit 过 `close` 之后框还挂在 DOM 上——那测的就是
 * 「一个没人接的组件」，不是「401 之后会发生什么」。
 *
 * 返回的 `open` 就是那个开关：`open.value = true` 等于「一次 401 撞上了」。
 */
function mountPrompt(): {
  wrapper: ReturnType<typeof mountIn>
  open: ReturnType<typeof ref<boolean>>
  goLogin: () => number
} {
  const open = ref(false)
  let goLoginCount = 0
  const Host = defineComponent({
    setup() {
      return () =>
        h(LoginPrompt, {
          open: open.value,
          onClose: () => {
            open.value = false
          },
          onGoLogin: () => {
            goLoginCount += 1
          },
        })
    },
  })
  const wrapper = mountIn(Host)
  return { wrapper, open, goLogin: () => goLoginCount }
}

/** 打开后的框本体（Teleport 到 body，所以不在 `w.element` 里）。 */
function prompt(doc: Document = document): HTMLElement {
  const el = doc.querySelector<HTMLElement>('[data-loginprompt]')
  if (!el) throw new Error('login prompt not rendered')
  return el
}

function passwordInput(): HTMLInputElement {
  const el = document.querySelector<HTMLInputElement>('#login-prompt-password')
  if (!el) throw new Error('password input not rendered')
  return el
}

/**
 * 填密码并提交。
 *
 * 用 `form.dispatchEvent(new Event('submit'))` 而不是 `requestSubmit()`：
 * 后者会走一遍按钮的 disabled 校验，而「空密码时按钮禁用」是另一条用例要的
 * 行为——这里要测的是「密码填了、按下去之后发生什么」。
 * `@submit.prevent` 仍然接得住（Vue 把原生 submit 事件归一成同一个 handler）。
 */
async function submit(password: string): Promise<void> {
  const input = passwordInput()
  input.value = password
  input.dispatchEvent(new Event('input'))
  await settle(2)
  const form = input.closest('form')
  expect(form, '表单不见了').not.toBeNull()
  form!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
  await settle(2)
  await flushMount()
}

/**
 * 背后那一页的骨架：`main[tabindex="-1"]` + 一个按钮。
   「恢复原来的操作」的判据就是它还在、还指着同一个节点。

   注意 `mountIn` 之后才建它不行——`mountIn` 自己会 append 一个 host div，
   而 `scaffoldPage` 用 `innerHTML` 覆盖会把那个 host 摘掉。所以 scaffold 先行，
   host 作为最后一个 body 子节点追加（Teleport 出来的框也是 body 子节点，
   顺序对两者都没影响）。
*/
function scaffoldPage(): { main: HTMLElement; btn: HTMLButtonElement } {
  const host = document.createElement('div')
  document.body.appendChild(host)
  const frag = document.createElement('div')
  frag.innerHTML = `
    <main tabindex="-1">
      <h1>设备</h1>
      <button id="refresh">刷新</button>
    </main>
  `
  const main = frag.firstElementChild as HTMLElement
  document.body.insertBefore(main, host)
  return {
    main,
    btn: main.querySelector('#refresh') as HTMLButtonElement,
  }
}

const originalPath = location.pathname

/** 把地址摆到 `/devices`：401 通常发生在设备页（它一次打两个端点）。 */
function atDevices(): void {
  window.history.replaceState(null, '', '/devices')
  route.value = matchRoute('/devices')
}

beforeEach(() => {
  resetForTest()
  atDevices()
})

afterEach(() => {
  resetForTest()
  route.value = matchRoute(originalPath)
  window.history.replaceState(null, '', originalPath)
  document.body.innerHTML = ''
})

describe('LoginPrompt：401 之后是弹框，不是跳页', () => {
  it('开着：模态挂到 body 上，背后那一页没有被拆掉', async () => {
    const page = scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    expect(prompt().getAttribute('role')).toBe('dialog')
    expect(prompt().getAttribute('aria-modal')).toBe('true')
    expect(prompt().parentElement).toBe(document.body)
    // 核心：页面还在，还是同一个 DOM 节点（没有被整页跳转拆掉重建）
    expect(document.querySelector('main')).toBe(page.main)
    expect(document.querySelector('#refresh')).toBe(page.btn)
    expect(location.pathname).toBe('/devices')
  })

  it('背后内容被 inert（§4.1），焦点进框', async () => {
    const page = scaffoldPage()
    page.btn.focus()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    expect(depth()).toBe(1)
    expect(page.main.inert).toBe(true)
    expect(document.activeElement).toBe(passwordInput())
  })

  it('Tab 被圈在框里（§3.10）', async () => {
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }))
    expect(prompt().contains(document.activeElement)).toBe(true)
    expect(depth()).toBe(1)
  })

  it('关着时什么都不渲染', async () => {
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: false } })
    await flush(w)

    expect(document.querySelector('[data-loginprompt]')).toBeNull()
    expect(depth()).toBe(0)
  })
})

describe('LoginPrompt：输对密码', () => {
  it('成功 → 关框、emit close、留在原页面（不导航、不刷新）', async () => {
    const page = scaffoldPage()
    stubFetch({ '/login': { body: { ok: true } } })
    const { wrapper: w, open, goLogin } = mountPrompt()
    open.value = true
    await flush(w)

    await submit('correct horse battery')

    // 受控：父级把 open 置回 false，框随之消失（见 mountPrompt 的注释）
    expect(open.value).toBe(false)
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
    // 没有 emit go-login：那一条是「取消」专用的
    expect(goLogin()).toBe(0)
    // 地址没动、页面还是原来那个 DOM
    expect(location.pathname).toBe('/devices')
    expect(document.querySelector('main')).toBe(page.main)
    expect(depth()).toBe(0)
  })

  it('请求走 /login 且带 same-origin 凭据（不走 http.ts，见组件文件头）', async () => {
    scaffoldPage()
    const fetchMock = stubFetch({ '/login': { body: { ok: true } } })
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    await submit('correct horse battery')

    const [url, init] = fetchMock.mock.calls.at(-1) as [string, RequestInit]
    expect(new URL(url, 'http://hub.local').pathname).toBe('/login')
    expect(init.method).toBe('POST')
    expect(init.credentials).toBe('same-origin')
    expect(init.body).toBe(JSON.stringify({ password: 'correct horse battery' }))
    expect(new Headers(init.headers).get('Content-Type')).toBe('application/json')
  })

  it('提交期间按钮禁用且文案是「验证中…」（不画假的进度）', async () => {
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
    scaffoldPage()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    const input = passwordInput()
    input.value = 'correct horse battery'
    input.dispatchEvent(new Event('input'))
    await settle(2)
    const form = input.closest('form')
    expect(form).not.toBeNull()
    form!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await settle(2)

    const submitBtn = prompt().querySelector<HTMLButtonElement>('button[type="submit"]')
    expect(submitBtn?.textContent).toContain('验证中')
    expect(submitBtn?.disabled).toBe(true)
    expect(prompt().querySelector('[role="progressbar"], progress')).toBeNull()

    release!()
    await flush(w)
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })
})

describe('LoginPrompt：取消才跳 /login', () => {
  it('点「取消」→ 关框 + emit go-login（父级据此跳登录页）', async () => {
    scaffoldPage()
    const { wrapper: w, open, goLogin } = mountPrompt()
    open.value = true
    await flush(w)

    await prompt()
      .querySelectorAll<HTMLButtonElement>('button')
      .forEach((b) => {
        if (b.textContent?.includes('取消')) void b.click()
      })
    await flush(w)

    expect(goLogin()).toBe(1)
    // 组件自己不跳页：跳转是外壳的策略（见 `App.vue` 的 `onLoginCancel`）
    expect(location.pathname).toBe('/devices')
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
    expect(depth()).toBe(0)
  })

  it('Esc → 只关框，不跳页', async () => {
    scaffoldPage()
    const { wrapper: w, open, goLogin } = mountPrompt()
    open.value = true
    await flush(w)

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    await flush(w)

    expect(goLogin()).toBe(0)
    expect(open.value).toBe(false)
    expect(location.pathname).toBe('/devices')
  })

  it('关闭按钮（✕）同样只关框', async () => {
    scaffoldPage()
    const { wrapper: w, open, goLogin } = mountPrompt()
    open.value = true
    await flush(w)

    await prompt().querySelector<HTMLButtonElement>('.af-modal__head button')?.click()
    await flush(w)

    expect(goLogin()).toBe(0)
    expect(open.value).toBe(false)
    expect(location.pathname).toBe('/devices')
  })

  it('关掉之后表单是干净的：下一次弹开是空框，不是上一次的残留', async () => {
    stubFetch({ '/login': { status: 401, body: { error: 'invalid_credentials' } } })
    scaffoldPage()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)
    await submit('wrong-password')
    expect(prompt().querySelector('[role="alert"]')).not.toBeNull()

    open.value = false
    await flush(w)
    open.value = true
    await flush(w)

    expect(passwordInput().value).toBe('')
    expect(prompt().querySelector('[role="alert"]')).toBeNull()
  })
})

describe('LoginPrompt：五种错误分开呈现（§4.7）', () => {
  it('密码错（401 invalid_credentials）：就地重试、不是「会话失效」', async () => {
    stubFetch({ '/login': { status: 401, body: { error: 'invalid_credentials' } } })
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    await submit('wrong-password')

    const alert = prompt().querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('密码不对')
    expect(alert?.textContent).toContain('invalid_credentials')
    // 同一个 401 状态码，两种完全相反的意思：这里是「密码错」，不是「又过期了」
    expect(alert?.textContent).not.toContain('登录态已失效')
    expect(passwordInput().getAttribute('aria-invalid')).toBe('true')
    // 密码清掉：错的密码留在框里既没用，又多留一份明文在 DOM 上
    expect(passwordInput().value).toBe('')
    // 可重试：框还开着、按钮还在、页面没动
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()
    expect(location.pathname).toBe('/devices')
  })

  it('Hub 未初始化（403 hub_not_initialized）：给「去 setup」，不是一句权限错误', async () => {
    stubFetch({ '/login': { status: 403, body: { error: 'hub_not_initialized' } } })
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    await submit('some-password')

    const alert = prompt().querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('Hub 还没设过密码')
    expect(alert?.textContent).toContain('hub_not_initialized')
    // 下一步是**换一个动作**，不是重试
    const toSetup = alert?.querySelector<HTMLButtonElement>('button')
    expect(toSetup?.textContent).toContain('setup')

    await toSetup?.click()
    await flush(w)
    expect(location.pathname).toBe('/setup')
    expect(w.emitted('close')).toHaveLength(1)
  })

  it('429 rate_limited：告诉用户等，而不是「密码错」', async () => {
    stubFetch({ '/login': { status: 429, body: { error: 'rate_limited' } } })
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    await submit('some-password')

    const text = prompt().querySelector('[role="alert"]')?.textContent ?? ''
    expect(text).toContain('尝试太频繁')
    expect(text).toContain('rate_limited')
    expect(text).not.toContain('密码不对')
    expect(location.pathname).toBe('/devices')
  })

  it('429 login_busy：与限流分开说——「Hub 正忙，不是密码的问题」', async () => {
    stubFetch({ '/login': { status: 429, body: { error: 'login_busy' } } })
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    await submit('some-password')

    const text = prompt().querySelector('[role="alert"]')?.textContent ?? ''
    expect(text).toContain('Hub 正忙')
    expect(text).toContain('login_busy')
    expect(text).not.toContain('尝试太频繁')
  })

  it('未知错误码原样带出，不编一句「登录失败」', async () => {
    stubFetch({ '/login': { status: 500, body: { error: 'some_new_code' } } })
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    await submit('some-password')

    const text = prompt().querySelector('[role="alert"]')?.textContent ?? ''
    expect(text).toContain('some_new_code')
    expect(text).not.toContain('登录失败')
  })

  it('Hub 连不上（网络错误）：与「密码错」分开说', async () => {
    vi.stubGlobal('fetch', vi.fn(() => Promise.reject(new TypeError('Failed to fetch'))))
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    await submit('some-password')

    const text = prompt().querySelector('[role="alert"]')?.textContent ?? ''
    expect(text).toContain('Hub 连不上')
    expect(text).toContain('network')
    expect(text).not.toContain('密码不对')
  })
})

/**
 * `handleLayerKeydown`（`lib/focus.ts`）是登录框与轨迹抽屉**共用**的那一份键盘
 * 处理。这里断言共用件真的跑在这个框里，并且**框自己的差异**（不传 opener、
 * 关掉时清表单）没有被共用件吞掉。
 */
describe('LoginPrompt：键盘走共用的 handleLayerKeydown', () => {
  it('共用件在框里跑：Esc 走它调的 close（出栈 + 清表单）', async () => {    scaffoldPage()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    const input = passwordInput()
    input.value = 'half-typed'
    input.dispatchEvent(new Event('input'))
    await flush(w)

    const e = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })
    document.dispatchEvent(e)
    await flush(w)

    expect(e.defaultPrevented, 'Esc 由共用件接管').toBe(true)
    expect(open.value).toBe(false)
    expect(depth()).toBe(0)
    // 焦点随层一起没了（`<body>`）：`pop` 的兜底 `main[tabindex="-1"]` 什么都不做。
    //
    // **这条是既存行为，不是共用件引入的**——`pop` 本身没变。原因在测试的宿主上：
    // 401 是本测试自己的 `scaffoldPage` 拼出来的，那份 HTML 落在 `mountPrompt`
    // 之后创建的宿主里，而 `mountIn` 的宿主 div 排在它**前面**；`inertOthers`
    // 给宿主整个标了 `inert`，写 focus 时整个文档「没有可聚焦元素」，于是
    // `activeElement` 变成 `<body>`。真实页面由 Vue 渲染，宿主就是那个 `div#app`。
    // 这里只钉共用件**跑了**（Esc 被接管、出栈），不把一个测试脚手架的产物
    // 当成契约断言。
    expect(depth()).toBe(0)

    // 差异化行为仍在框里：再弹开是空框，不是上一次的残留
    open.value = true
    await flush(w)
    expect(passwordInput().value).toBe('')
  })

  it('框的差异仍在：不传 opener，所以有没有可还的触发元素是框自己的事', async () => {
    const page = scaffoldPage()
    page.btn.focus()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    // 首焦点是 `data-autofocus` 的密码框，不是那个触发 401 的按钮
    expect(document.activeElement).toBe(passwordInput())
    // 打开时没把「刷新」按钮记成 opener（`push(el)` 不带 `{ opener }`）。
    // 抽屉那边同一动作会回到它的按钮——那是两处**有意保留**的差异。
    expect(document.activeElement).not.toBe(page.btn)
  })

  it('输入法选字中的 Esc 不关框（共用件的第一个判据）', async () => {
    scaffoldPage()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    const e = new KeyboardEvent('keydown', {
      key: 'Escape',
      isComposing: true,
      bubbles: true,
      cancelable: true,
    })
    document.dispatchEvent(e)
    await flush(w)

    expect(e.defaultPrevented).toBe(false)
    expect(open.value).toBe(true)
    expect(depth()).toBe(1)
  })

  it('不是栈顶时不接管 Esc（共用件判栈顶：帮助面板盖在框上时不该关掉框）', async () => {
    scaffoldPage()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    const above = document.createElement('div')
    document.body.appendChild(above)
    push(above)
    try {
      const e = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })
      document.dispatchEvent(e)
      await flush(w)

      expect(e.defaultPrevented).toBe(false)
      expect(open.value).toBe(true)
    } finally {
      pop(above)
    }
  })

  it('Tab 圈在框里——由共用件转给 trapTab', async () => {
    scaffoldPage()
    const { wrapper: w, open } = mountPrompt()
    open.value = true
    await flush(w)

    // 从最后一个可聚焦元素（取消）上按 Tab：必须绕回第一个，否则焦点出了框
    const buttons = Array.from(prompt().querySelectorAll<HTMLElement>('form button'))
    const last = buttons[buttons.length - 1] as HTMLElement
    last.focus()

    const e = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true })
    document.dispatchEvent(e)

    expect(e.defaultPrevented).toBe(true)
    // 焦点在框里，且被绕回了**第一个**可聚焦元素（DOM 顺序上的头一个是 ✕），
    // 不是放它出框——`trapTab` 的「最后一个 → 第一个」那条分支。
    expect(prompt().contains(document.activeElement)).toBe(true)
    expect(document.activeElement).toBe(prompt().querySelector('.af-modal__head button'))
  })
})

describe('LoginPrompt：无障碍', () => {
  it('密码框有 label、type=password、autocomplete=current-password', async () => {
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    const input = passwordInput()
    expect(input.type).toBe('password')
    expect(input.getAttribute('autocomplete')).toBe('current-password')
    const label = prompt().querySelector<HTMLLabelElement>('label[for="login-prompt-password"]')
    expect(label?.textContent).toContain('密码')
    expect(prompt().querySelector('#login-prompt-password')).toBe(input)
  })

  it('错误描述由 aria-describedby 指到输入框上（键盘与读屏都听得见）', async () => {
    stubFetch({ '/login': { status: 401, body: { error: 'invalid_credentials' } } })
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)
    await submit('wrong-password')

    const described = passwordInput().getAttribute('aria-describedby') ?? ''
    expect(described).toContain('login-prompt-hint')
    expect(described).toContain('login-prompt-error')
    expect(prompt().querySelector('#login-prompt-error')?.textContent).toContain('密码不对')
  })

  it('空密码时提交按钮禁用（不发一个必然失败的请求）', async () => {
    scaffoldPage()
    const w = mountIn(LoginPrompt, { props: { open: true } })
    await flush(w)

    expect(prompt().querySelector<HTMLButtonElement>('button[type="submit"]')?.disabled).toBe(true)
  })
})
