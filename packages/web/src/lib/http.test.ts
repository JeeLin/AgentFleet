import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  ApiError,
  CSRF_COOKIE,
  CSRF_HEADER,
  NetworkError,
  cookieValue,
  del,
  getJson,
  postJson,
  readCsrfToken,
  request,
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

  it('401 跳 /login 并抛 ApiError', async () => {
    fetchMock.mockResolvedValue(jsonResponse(401, { error: 'missing_session' }))
    await expect(request('/api/devices')).rejects.toBeInstanceOf(ApiError)
    expect(redirected).toEqual(['/login'])
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