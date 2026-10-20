import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { loginPromptOpen } from './login-prompt'
import {
  ApiError,
  CSRF_COOKIE,
  CSRF_HEADER,
  HUB_NOT_INITIALIZED,
  NetworkError,
  cookieValue,
  del,
  getJson,
  postJson,
  readCsrfToken,
  request,
  setNotInitializedHandler,
  setUnauthorizedHandler,
  withCsrf,
  withQuery,
} from './http'

/** 造一个只用到本模块所需字段的 `Response`。 */
function jsonResponse(status: number, body?: unknown): Response {
  if (body === undefined) return new Response(null, { status })
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

/** 取上一次 `fetch` 的 [url, init]。 */
function lastCall(mock: ReturnType<typeof vi.fn>): { url: string; init: RequestInit } {
  const [url, init] = mock.mock.calls.at(-1) as [string, RequestInit]
  return { url, init }
}

function setCsrfCookie(value: string): void {
  document.cookie = `${CSRF_COOKIE}=${value}; Path=/`
}

describe('cookieValue', () => {
  it('取出目标 cookie 并忽略同名之外的名字', () => {
    expect(cookieValue('af_csrf', 'af_session=s1; af_csrf=abc; other=x')).toBe('abc')
  })

  it('容忍 cookie 两侧的空白', () => {
    expect(cookieValue('af_csrf', '  af_session=s1 ;  af_csrf=abc  ')).toBe('abc')
  })

  it('值为空时返回 null（csrf_ok 的非空判据）', () => {
    expect(cookieValue('af_csrf', 'af_csrf=')).toBeNull()
  })

  it('没有 cookie 头、没有目标项、没有等号时都返回 null', () => {
    expect(cookieValue('af_csrf', null)).toBeNull()
    expect(cookieValue('af_csrf', undefined)).toBeNull()
    expect(cookieValue('af_csrf', 'af_session=s1')).toBeNull()
    expect(cookieValue('af_csrf', 'af_csrf')).toBeNull()
  })

  it('名字相同但嵌在别的值后面不算命中', () => {
    expect(cookieValue('af_csrf', 'xaf_csrf=abc')).toBeNull()
  })
})

describe('readCsrfToken', () => {
  afterEach(() => {
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
  })

  it('从 document.cookie 读出 af_csrf', () => {
    setCsrfCookie('token-from-server')
    expect(readCsrfToken()).toBe('token-from-server')
  })

  it('没有该 cookie 时返回 null', () => {
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
    expect(readCsrfToken()).toBeNull()
  })
})

describe('withCsrf', () => {
  afterEach(() => {
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
  })

  it.each(['POST', 'PUT', 'PATCH', 'DELETE', 'post'])(
    '%s 请求回填 X-CSRF-Token',
    (method) => {
      setCsrfCookie('tok')
      expect(withCsrf(undefined, method).get(CSRF_HEADER)).toBe('tok')
    },
  )

  it.each(['GET', 'HEAD', 'OPTIONS'])('%s 请求不补 CSRF 头', (method) => {
    setCsrfCookie('tok')
    expect(withCsrf(undefined, method).has(CSRF_HEADER)).toBe(false)
  })

  it('读不到 cookie 时不编空值（空值必然与服务端判据不符）', () => {
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
    expect(withCsrf(undefined, 'POST').has(CSRF_HEADER)).toBe(false)
  })

  it('不改入参，且保留调用方已有的头', () => {
    setCsrfCookie('tok')
    const input = new Headers({ 'X-Trace': 'abc' })
    const out = withCsrf(input, 'POST')
    expect(input.has(CSRF_HEADER)).toBe(false)
    expect(out.get('X-Trace')).toBe('abc')
  })
})

describe('withQuery', () => {
  it('没有 query 时原样返回', () => {
    expect(withQuery('/api/devices', undefined)).toBe('/api/devices')
  })

  it('跳过 undefined、序列化数字与布尔、null 成空串', () => {
    expect(withQuery('/api/x', { a: undefined, b: 2, c: true, d: null })).toBe('/api/x?b=2&c=true&d=')
  })

  it('参数全被跳过时不留裸问号', () => {
    expect(withQuery('/api/x', { a: undefined })).toBe('/api/x')
  })

  it('编码特殊字符', () => {
    expect(withQuery('/api/x', { q: 'a b&c' })).toBe('/api/x?q=a+b%26c')
  })
})

describe('request', () => {
  const fetchMock = vi.fn()
  const original = globalThis.fetch
  let redirected: string[] = []
  let restore: () => void

  beforeEach(() => {
    fetchMock.mockReset()
    globalThis.fetch = fetchMock as unknown as typeof fetch
    redirected = []
    restore = setUnauthorizedHandler((path) => {
      redirected.push(path)
    })
    setCsrfCookie('tok')
  })

  afterEach(() => {
    restore()
    globalThis.fetch = original
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
    // 401 用例会置位这个模块级 ref；不还原的话下一个用例一进来就看见一个
    // 「已经开着」的框（而它自己什么都没做）。
    loginPromptOpen.value = false
  })

  it('GET 默认 same-origin，且不带 CSRF 头', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { ok: true }))
    await request('/api/devices')
    const { init } = lastCall(fetchMock)
    expect(init.method).toBe('GET')
    expect(init.credentials).toBe('same-origin')
    expect(new Headers(init.headers).has(CSRF_HEADER)).toBe(false)
  })

  it('状态变更方法自动带上 cookie 里的 token', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { ok: true }))
    await request('/api/sessions', { method: 'POST', body: '{}' })
    const { init } = lastCall(fetchMock)
    expect(new Headers(init.headers).get(CSRF_HEADER)).toBe('tok')
  })

  it('POST 的 body 原样透传，不被重新编码', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, {}))
    const bytes = new Uint8Array([1, 2, 3])
    await request('/api/channels/c1/send', { method: 'POST', body: bytes })
    expect(lastCall(fetchMock).init.body).toBe(bytes)
  })

  it('401 抛 ApiError，且不再整页跳走（跳转是取消时才发生的事）', async () => {
    fetchMock.mockResolvedValue(jsonResponse(401, { error: 'missing_session' }))
    await expect(request('/api/devices')).rejects.toBeInstanceOf(ApiError)
    // 槽位照旧被调用：`setUnauthorizedHandler` 的签名不许变，测试锁着它。
    expect(redirected).toEqual(['/login'])
    // 但**页面没有被跳走**——这一条就是这次改动要的那件事。
    // 跳转发生在用户在框里点「取消」之后，那一刀在 `App.vue`。
    expect(location.pathname).toBe('/sessions')
  })

  it('非 2xx 取服务端 error 码', async () => {
    fetchMock.mockResolvedValue(jsonResponse(409, { error: 'device_offline' }))
    await expect(request('/api/sessions', { method: 'POST' })).rejects.toMatchObject({
      status: 409,
      code: 'device_offline',
    })
  })

  it('错误体不是 JSON 时退回 http_<status>，不把原文当码', async () => {
    fetchMock.mockResolvedValue(new Response('<html>502</html>', { status: 502 }))
    await expect(request('/api/x')).rejects.toMatchObject({ status: 502, code: 'http_502' })
  })

  it('错误体 JSON 里没有 error 字段时同样退回', async () => {
    fetchMock.mockResolvedValue(jsonResponse(500, { nope: 1 }))
    await expect(request('/api/x')).rejects.toMatchObject({ code: 'http_500' })
  })

  it('fetch 抛异常 → NetworkError，不伪装成服务端错误', async () => {
    fetchMock.mockRejectedValue(new TypeError('Failed to fetch'))
    await expect(request('/api/x')).rejects.toBeInstanceOf(NetworkError)
  })
})

/*
  401 的**缺省**处理单独一个 describe：上面的 `beforeEach` 把槽位换成了计数器，
  而「401 到底弹不弹框」必须有一条不换槽位的用例，否则那条断言永远是
  「计数 +1」——测试绿着，而用户眼前什么都不弹。
*/
describe('request：401 的缺省处理是弹框，不是跳页', () => {
  const fetchMock = vi.fn()
  const original = globalThis.fetch

  beforeEach(() => {
    fetchMock.mockReset()
    globalThis.fetch = fetchMock as unknown as typeof fetch
    setCsrfCookie('tok')
    loginPromptOpen.value = false
  })

  afterEach(() => {
    globalThis.fetch = original
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
    loginPromptOpen.value = false
  })

  it('401 → 弹重新登录框，且页面没有跳走', async () => {
    fetchMock.mockResolvedValue(jsonResponse(401, { error: 'missing_session' }))
    await expect(request('/api/devices')).rejects.toBeInstanceOf(ApiError)
    expect(loginPromptOpen.value).toBe(true)
    // 这就是整件事：背后的页面还活着，用户只需要一个密码
    expect(location.pathname).toBe('/sessions')
  })

  it('连撞多个 401 只弹一个框（并发请求共用一条 401 路径）', async () => {
    // 设备页一次就打两个端点，两个都会 401。布尔值而不是计数器就是为这个：
    // 计数会让第二个框叠上去，而少关一个就永远关不掉。
    fetchMock.mockResolvedValue(jsonResponse(401, { error: 'missing_session' }))
    const results = await Promise.allSettled([request('/api/devices'), request('/api/sessions')])
    expect(results.every((r) => r.status === 'rejected')).toBe(true)
    expect(loginPromptOpen.value).toBe(true)
    expect(location.pathname).toBe('/sessions')
  })

  it('再下一次成功的请求不会把框收掉（关它只能靠登录成功或取消）', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse(401, { error: 'missing_session' }))
    await expect(request('/api/devices')).rejects.toBeInstanceOf(ApiError)
    expect(loginPromptOpen.value).toBe(true)
    // 401 之后页面自己重试、或用户切了个 tab 又发一次请求：框不该自己消失，
    // 否则用户被扔在一个「没有凭据也看不见原因」的页面上。
    fetchMock.mockResolvedValueOnce(jsonResponse(200, { ok: true }))
    await request('/api/devices')
    expect(loginPromptOpen.value).toBe(true)
  })
})

describe('request：403 的判据是「且 code === hub_not_initialized」，不是「403 就跳」', () => {
  const fetchMock = vi.fn()
  const original = globalThis.fetch
  let went: string[] = []
  let restore: () => void

  beforeEach(() => {
    fetchMock.mockReset()
    globalThis.fetch = fetchMock as unknown as typeof fetch
    setCsrfCookie('tok')
    went = []
    restore = setNotInitializedHandler((path) => {
      went.push(path)
    })
  })

  afterEach(() => {
    restore()
    globalThis.fetch = original
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
  })

  it('403 hub_not_initialized → 跳 /setup，且照常抛 ApiError', async () => {
    fetchMock.mockResolvedValue(jsonResponse(403, { error: HUB_NOT_INITIALIZED }))
    await expect(request('/api/devices')).rejects.toMatchObject({ status: 403, code: HUB_NOT_INITIALIZED })
    expect(went).toEqual(['/setup'])
  })

  it('403 setup_token_mismatch 不跳：那是 setup 页自己就地显示的错误', async () => {
    fetchMock.mockResolvedValue(jsonResponse(403, { error: 'setup_token_mismatch' }))
    await expect(request('/setup', { method: 'POST' })).rejects.toMatchObject({
      status: 403,
      code: 'setup_token_mismatch',
    })
    expect(went).toEqual([])
  })

  it('403 csrf_token_mismatch 不跳：把人从页面上赶走只会让表单重填', async () => {
    fetchMock.mockResolvedValue(jsonResponse(403, { error: 'csrf_token_missing_or_mismatch' }))
    await expect(request('/api/sessions', { method: 'POST' })).rejects.toMatchObject({
      status: 403,
      code: 'csrf_token_missing_or_mismatch',
    })
    expect(went).toEqual([])
  })

  it('401 不触发 setup 跳转：两件事说的是不同的事', async () => {
    fetchMock.mockResolvedValue(jsonResponse(401, { error: 'missing_session' }))
    restore()
    await expect(request('/api/devices')).rejects.toBeInstanceOf(ApiError)
    expect(went).toEqual([])
  })
})

describe('getJson / postJson / del', () => {
  const fetchMock = vi.fn()
  const original = globalThis.fetch

  beforeEach(() => {
    fetchMock.mockReset()
    globalThis.fetch = fetchMock as unknown as typeof fetch
    setCsrfCookie('tok')
  })

  afterEach(() => {
    globalThis.fetch = original
    document.cookie = `${CSRF_COOKIE}=; Path=/; Max-Age=0`
  })

  it('getJson 解 JSON 并拼 query', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, [{ id: 'd1' }]))
    await expect(getJson('/api/devices', { query: { limit: 5, cursor: undefined } })).resolves.toEqual([
      { id: 'd1' },
    ])
    expect(lastCall(fetchMock).url).toBe('/api/devices?limit=5')
  })

  it('postJson 序列化对象并设 Content-Type，同时带 CSRF 头', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { id: 's1' }))
    await postJson('/api/sessions', { node_id: 'd1', path: '/tmp' })
    const { url, init } = lastCall(fetchMock)
    const headers = new Headers(init.headers)
    expect(url).toBe('/api/sessions')
    expect(init.method).toBe('POST')
    expect(headers.get('Content-Type')).toBe('application/json')
    expect(headers.get(CSRF_HEADER)).toBe('tok')
    expect(init.body).toBe('{"node_id":"d1","path":"/tmp"}')
  })

  it('postJson 收到字符串体时不覆盖调用方已设的 Content-Type', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, {}))
    await postJson('/api/channels/c1/send', 'raw-bytes', {
      headers: { 'Content-Type': 'application/octet-stream' },
    })
    const headers = new Headers(lastCall(fetchMock).init.headers)
    expect(headers.get('Content-Type')).toBe('application/octet-stream')
    expect(headers.get(CSRF_HEADER)).toBe('tok')
  })

  it('postJson 无 body 时不设 Content-Type', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { ok: true }))
    await postJson('/api/sessions')
    const headers = new Headers(lastCall(fetchMock).init.headers)
    expect(headers.has('Content-Type')).toBe(false)
    expect(lastCall(fetchMock).init.body).toBeUndefined()
    // 无 body 仍是状态变更：CSRF 头照样要补
    expect(headers.get(CSRF_HEADER)).toBe('tok')
  })

  it('del 发 DELETE 并忽略 204 的空体', async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }))
    await expect(del('/api/channels/c1')).resolves.toBeUndefined()
    const { init } = lastCall(fetchMock)
    expect(init.method).toBe('DELETE')
    expect(new Headers(init.headers).get(CSRF_HEADER)).toBe('tok')
  })
})