import { readFileSync } from 'node:fs'
import { fileURLToPath, URL as NodeUrl } from 'node:url'

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { ChannelHub, channelErrorLabel } from './channel'
import { ApiError } from './http'
import { settle, stubFetch } from '@/test/http'

/** 一个能手动派发事件的假 EventSource：测试不碰真网络。 */
class FakeEventSource {
  static instances: FakeEventSource[] = []
  readonly url: string
  closed = false
  private readonly listeners = new Map<string, Set<(ev: Event) => void>>()

  constructor(url: string) {
    this.url = url
    FakeEventSource.instances.push(this)
  }

  addEventListener(type: string, fn: (ev: Event) => void): void {
    const set = this.listeners.get(type) ?? new Set()
    set.add(fn)
    this.listeners.set(type, set)
  }

  close(): void {
    this.closed = true
  }

  emit(type: string, data?: unknown): void {
    const ev = data === undefined ? new Event(type) : new MessageEvent(type, { data: JSON.stringify(data) })
    for (const fn of this.listeners.get(type) ?? []) fn(ev)
  }
}

function makeHub(): ChannelHub {
  FakeEventSource.instances = []
  return new ChannelHub({ createEventSource: (url) => new FakeEventSource(url) as unknown as EventSource })
}

function source(): FakeEventSource {
  const s = FakeEventSource.instances[0]
  if (!s) throw new Error('no EventSource was created')
  return s
}

function sentCalls(mock: ReturnType<typeof vi.fn>): Array<{ url: string; init: RequestInit }> {
  return mock.mock.calls.map(([url, init]) => ({
    url: new URL(String(url), 'http://hub.local').pathname,
    init: (init ?? {}) as RequestInit,
  }))
}

beforeEach(() => {
  FakeEventSource.instances = []
})

afterEach(() => {
  document.body.innerHTML = ''
})

describe('懒建立：拿到句柄不等于开了通道', () => {
  it('handle() 不发任何请求，状态是 idle', () => {
    const fetchMock = stubFetch({})
    const hub = makeHub()
    const ch = hub.handle('a\u0000terminal', { sessionId: 's_1', connector: 'terminal' })
    expect(ch.state()).toBe('idle')
    expect(fetchMock).not.toHaveBeenCalled()
    expect(FakeEventSource.instances).toHaveLength(0)
  })

  it('open() 才 POST /api/channels 并建 SSE', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const ch = hub.handle('k', { sessionId: 's_1', connector: 'terminal', params: { cwd: '/w' } })
    await ch.open()

    expect(ch.state()).toBe('ready')
    expect(ch.id()).toBe('ch-1')
    const calls = sentCalls(fetchMock)
    expect(calls).toHaveLength(1)
    expect(calls[0]?.url).toBe('/api/channels')
    expect(calls[0]?.init.method).toBe('POST')
    expect(JSON.parse(String(calls[0]?.init.body))).toEqual({
      session_id: 's_1',
      connector: 'terminal',
      params: { cwd: '/w' },
    })
    expect(FakeEventSource.instances).toHaveLength(1)
  })

  it('重复 open() 是幂等：通道只建一次', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const ch = hub.handle('k', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    await ch.open()
    expect(fetchMock).toHaveBeenCalledTimes(1)
  })
})

describe('帧分发', () => {
  it('channel.frame 按 channel_id 找到对应句柄，payload 不再剥一层', async () => {
    stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const a = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    const b = hub.handle('b', { sessionId: 's_2', connector: 'terminal' })
    await a.open()
    const got: unknown[] = []
    a.onFrame((f) => got.push(f.payload))
    b.onFrame(() => got.push('wrong handle'))

    source().emit('channel.frame', {
      node_id: 'n_1',
      channel_id: 'ch-1',
      payload: { bytes: 'hello' },
    })

    expect(got).toEqual([{ bytes: 'hello' }])
  })

  it('不是合法 JSON 的帧被丢掉，不炸整条流', async () => {
    stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    const got: unknown[] = []
    ch.onFrame((f) => got.push(f))

    const raw = new MessageEvent('channel.frame', { data: '{not json' })
    for (const fn of (source() as unknown as { listeners: Map<string, Set<(e: Event) => void>> }).listeners.get(
      'channel.frame',
    ) ?? []) {
      fn(raw)
    }

    expect(got).toEqual([])
    expect(hub.state()).toBe('connecting')
  })
})

describe('断线：丢弃并计数，不排队', () => {
  async function readyChannel() {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    source().emit('open')
    return { hub, ch, fetchMock }
  }

  it('SSE 在线时 send 发 POST octet-stream', async () => {
    const { ch, fetchMock } = await readyChannel()
    ch.send('ls\r')
    expect(ch.dropped()).toBe(0)
    const sends = sentCalls(fetchMock).filter((c) => c.url.endsWith('/send'))
    expect(sends).toHaveLength(1)
    expect(sends[0]?.init.method).toBe('POST')
    expect(sends[0]?.init.body).toBe('ls\r')
    expect(new Headers(sends[0]?.init.headers).get('Content-Type')).toBe('application/octet-stream')
  })

  it('流断开时 send 被丢弃并计数，不产生请求', async () => {
    const { ch, fetchMock } = await readyChannel()
    const before = fetchMock.mock.calls.length
    source().emit('error')

    ch.send('a')
    ch.send('b')

    expect(ch.dropped()).toBe(2)
    expect(fetchMock.mock.calls.length).toBe(before)
  })

  it('重连回来后计数不会自己清零（用户要知道丢过）', async () => {
    const { ch } = await readyChannel()
    source().emit('error')
    ch.send('x')
    source().emit('open')
    expect(ch.dropped()).toBe(1)
  })
})

describe('通道还没建好：字符排队，不丢弃', () => {
  /**
   * 把 `POST /api/channels` 挂在一个门闩上，直到测试放行——那正是
   * 「点了面板、通道还在建」的那个窗口。
   */
  function gatedOpen() {
    let release: (() => void) | null = null
    const gate = new Promise<void>((r) => {
      release = r
    })
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const path = new URL(String(input), 'http://hub.local').pathname
      if (path === '/api/channels') {
        await gate
        return new Response(JSON.stringify({ channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' }), {
          status: 201,
          headers: { 'Content-Type': 'application/json' },
        })
      }
      if (path === '/api/channels/ch-1/send' || path === '/api/channels/ch-1') {
        return new Response(null, { status: 204 })
      }
      return new Response(JSON.stringify({ error: 'unexpected_request' }), {
        status: 500,
        headers: { 'Content-Type': 'application/json' },
      })
    })
    vi.stubGlobal('fetch', fetchMock)
    return { fetchMock, allow: () => release?.() }
  }

  it('建好前敲的字符一条不发也不计丢，建好后按原顺序补上', async () => {
    const { fetchMock, allow } = gatedOpen()
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    const opening = ch.open()

    // 用户点进终端就敲：通道还在建
    ch.send('e')
    ch.send('c')
    ch.send('ho HELLO\r')
    expect(ch.dropped()).toBe(0)
    expect(sentCalls(fetchMock).filter((c) => c.url.endsWith('/send'))).toHaveLength(0)

    allow()
    await opening
    await settle()

    const sends = sentCalls(fetchMock).filter((c) => c.url.endsWith('/send'))
    expect(sends.map((c) => c.init.body)).toEqual(['e', 'c', 'ho HELLO\r'])
    expect(ch.dropped()).toBe(0)
  })

  it('未建通道时 send 不发请求，也不该把 dropped 抬起来', () => {
    const fetchMock = stubFetch({})
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    ch.send('first')
    expect(fetchMock.mock.calls).toHaveLength(0)
    expect(ch.dropped()).toBe(0)
  })

  it('关面板时丢掉还没发出去的字符，不让它在别处被补发', async () => {
    const { fetchMock, allow } = gatedOpen()
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    const opening = ch.open()
    ch.send('never sent')
    await ch.close()
    allow()
    await opening.catch(() => undefined)
    await settle()

    expect(sentCalls(fetchMock).filter((c) => c.url.endsWith('/send'))).toHaveLength(0)
  })
})

describe('resize', () => {
  it('未建通道时记下尺寸，建好后补发一次', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
      '/api/channels/ch-1/resize': { empty: true },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    ch.resize(120, 40)
    expect(sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize'))).toHaveLength(0)

    await ch.open()
    source().emit('open')
    // open 的 then 里补发 resize；让微任务走完
    await Promise.resolve()
    await Promise.resolve()

    const resizes = sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize'))
    expect(resizes).toHaveLength(1)
    expect(JSON.parse(String(resizes[0]?.init.body))).toEqual({ cols: 120, rows: 40 })
  })

  it('断线时 resize 记为待发，重连后补一次；不把面板标成 failed', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
      '/api/channels/ch-1/resize': { empty: true },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    source().emit('open')
    source().emit('error')
    const before = sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize')).length
    ch.resize(100, 30)
    expect(sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize'))).toHaveLength(before)
    expect(ch.state()).toBe('ready')

    source().emit('open')
    await Promise.resolve()
    const resizes = sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize'))
    expect(resizes).toHaveLength(before + 1)
    expect(JSON.parse(String(resizes.at(-1)?.init.body))).toEqual({ cols: 100, rows: 30 })
  })

  it('断线期间连调几次 resize 只补发最后一次（最新一次就是对的）', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
      '/api/channels/ch-1/resize': { empty: true },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    source().emit('open')
    source().emit('error')
    const before = sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize')).length
    ch.resize(100, 30)
    ch.resize(120, 36)
    ch.resize(140, 40)
    source().emit('open')
    await Promise.resolve()
    const resizes = sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize'))
    expect(resizes).toHaveLength(before + 1)
    expect(JSON.parse(String(resizes.at(-1)?.init.body))).toEqual({ cols: 140, rows: 40 })
  })

  it('未建通道时的尺寸在 open 之后补发（不然 PTY 一直按 80×24 折行）', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
      '/api/channels/ch-1/resize': { empty: true },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    ch.resize(200, 50)
    await ch.open()
    // set('ready') 时下行可能还没 open（connect 与 POST 并发），尺寸会再挂一次，
    // 等 EventSource 报 open 才发出——这比抢在连接就绪前发更稳
    source().emit('open')
    await Promise.resolve()
    const resizes = sentCalls(fetchMock).filter((c) => c.url.endsWith('/resize'))
    expect(resizes).toHaveLength(1)
    expect(JSON.parse(String(resizes[0]?.init.body))).toEqual({ cols: 200, rows: 50 })
  })
})

describe('关闭', () => {
  it('close 发 DELETE，状态变 closed，帧不再分发', async () => {
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
      '/api/channels/ch-1': { empty: true },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    const got: unknown[] = []
    ch.onFrame((f) => got.push(f))

    await ch.close()

    expect(ch.state()).toBe('closed')
    expect(ch.id()).toBeNull()
    const deletes = sentCalls(fetchMock).filter((c) => c.init.method === 'DELETE')
    expect(deletes.map((c) => c.url)).toEqual(['/api/channels/ch-1'])

    source().emit('channel.frame', { node_id: 'n_1', channel_id: 'ch-1', payload: { bytes: 'late' } })
    expect(got).toEqual([])
  })

  it('没建过的通道 close 是空操作（不发 DELETE）', async () => {
    const fetchMock = stubFetch({})
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.close()
    expect(ch.state()).toBe('closed')
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('DELETE 失败也不拦住 UI（Hub 侧幂等）', async () => {
    stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
      '/api/channels/ch-1': { status: 500, body: { error: 'boom' } },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    await expect(ch.close()).resolves.toBeUndefined()
    expect(ch.state()).toBe('closed')
  })
})

describe('失败态', () => {
  it('open 失败把错误码写进 failed，状态不再停在 opening', async () => {
    stubFetch({ '/api/channels': { status: 409, body: { error: 'device_offline' } } })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await expect(ch.open()).rejects.toBeInstanceOf(ApiError)
    expect(ch.state()).toBe('failed')
    expect(ch.errorCode()).toBe('device_offline')
  })

  it('失败后再 open 都是重试（不会永久卡住）', async () => {
    let attempt = 0
    const fetchMock = stubFetch({
      '/api/channels': () => {
        attempt += 1
        return attempt === 1
          ? { status: 409, body: { error: 'device_offline' } }
          : { status: 201, body: { channel_id: 'ch-2', node_id: 'n_1', session_id: 's_1' } }
      },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await expect(ch.open()).rejects.toBeTruthy()
    await ch.open()
    expect(fetchMock).toHaveBeenCalledTimes(2)
    expect(ch.state()).toBe('ready')
    expect(ch.errorCode()).toBeNull()
  })
})

describe('会话结束', () => {
  it('session.state=ended 关掉属于该会话的通道并标记 closed', async () => {
    stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const mine = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    const other = hub.handle('b', { sessionId: 's_2', connector: 'terminal' })
    await mine.open()
    source().emit('session.state', { session_id: 's_1', state: 'ended' })
    expect(mine.state()).toBe('closed')
    expect(other.state()).toBe('idle')
  })

  it('别的字段（只是 running）不误杀通道', async () => {
    stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    const ch = hub.handle('a', { sessionId: 's_1', connector: 'terminal' })
    await ch.open()
    source().emit('session.state', { session_id: 's_1', state: 'running' })
    expect(ch.state()).toBe('ready')
  })
})

describe('整体释放', () => {
  it('dispose 关掉所有通道并断开 SSE', async () => {
    const fetchMock = stubFetch({
      '/api/channels': (() => {
        let n = 0
        return () => {
          n += 1
          return { status: 201, body: { channel_id: `ch-${n}`, node_id: 'n_1', session_id: 's_1' } }
        }
      })(),
      '/api/channels/ch-1': { empty: true },
      '/api/channels/ch-2': { empty: true },
    })
    const hub = makeHub()
    await hub.handle('a', { sessionId: 's_1', connector: 'terminal' }).open()
    await hub.handle('b', { sessionId: 's_1', connector: 'files' }).open()
    hub.dispose()
    await Promise.resolve()
    const deletes = sentCalls(fetchMock).filter((c) => c.init.method === 'DELETE')
    expect(deletes).toHaveLength(2)
    expect(source().closed).toBe(true)
    expect(hub.openCount()).toBe(0)
  })
})

describe('通道错误的可读文案', () => {
  it('已知 code 都有中文理由', () => {
    expect(channelErrorLabel('device_offline')).toContain('离线')
    expect(channelErrorLabel('session_ended')).toContain('已结束')
    expect(channelErrorLabel('device_busy')).toContain('稍后')
    expect(channelErrorLabel('invalid_terminal_size')).toContain('尺寸')
    expect(channelErrorLabel('network')).toContain('Hub')
    expect(channelErrorLabel('channel_not_yours')).toContain('登录身份')
  })

  it('未知 code 原样带出，不编一句好话', () => {
    expect(channelErrorLabel('quota_exceeded')).toContain('quota_exceeded')
    expect(channelErrorLabel(null)).toBe('通道建立失败。')
  })
})

describe('CSRF 只在三个上行端点上', () => {
  it('POST 带 X-CSRF-Token（EventSource 发不了头，所以下行不需要）', async () => {
    document.cookie = 'af_csrf=tok-123; Path=/'
    const fetchMock = stubFetch({
      '/api/channels': { status: 201, body: { channel_id: 'ch-1', node_id: 'n_1', session_id: 's_1' } },
    })
    const hub = makeHub()
    await hub.handle('a', { sessionId: 's_1', connector: 'terminal' }).open()
    const headers = new Headers(sentCalls(fetchMock)[0]?.init.headers)
    expect(headers.get('X-CSRF-Token')).toBe('tok-123')
    vi.restoreAllMocks()
    document.cookie = 'af_csrf=; Max-Age=0; Path=/'
  })
})

describe('resize 与 flushPending 的守卫同源', () => {
  // 两个函数各写一遍判据的后果是形状会漂：曾一个写成三条件合并、另一个拆成
  // 两条。jsdom 读不到 private 方法，所以从源码取证两者都走 canPush()。
  const src = readFileSync(fileURLToPath(new NodeUrl('./channel.ts', import.meta.url)), 'utf8')

  function body(name: string): string {
    const m = src.match(new RegExp(`${name}\\([^)]*\\): void \\{([\\s\\S]*?)\\n  \\}`))
    expect(m, `${name} 没找到`).not.toBeNull()
    return m?.[1] ?? ''
  }

  it('两边都调 canPush()，不自己拼判据', () => {
    expect(body('resize')).toContain('canPush()')
    expect(body('flushPending')).toContain('canPush()')
    // 自己拼判据的两个信号：channelId / canSend 直接出现在函数体里
    expect(body('resize')).not.toMatch(/this\.channelId|hub\.canSend\(\)/)
    expect(body('flushPending')).not.toMatch(/this\.channelId|hub\.canSend\(\)/)
  })
})
