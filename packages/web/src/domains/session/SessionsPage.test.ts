import { describe, expect, it } from 'vitest'

import SessionsPage from '@/domains/session/SessionsPage.vue'
import { device, mountIn, session } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

const DEVICES = [device({ id: 'n_1', name: 'mbp.local' })]

const SESSIONS = [
  session({ id: 's_1', state: 'running', workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
  session({ id: 's_2', state: 'ended', workspace: { node_id: 'n_1', path: '/srv/beta', path_locked: true } }),
]

describe('SessionsPage：渲染', () => {
  it('分成进行中与历史两区，各自给出数量', async () => {
    stubFetch({ '/api/sessions': { body: SESSIONS }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('进行中')
    expect(text).toContain('历史')
    expect(w.element.querySelectorAll('section')).toHaveLength(2)
    expect(w.element.textContent).toContain('alpha')
    expect(w.element.textContent).toContain('beta')
  })

  it('这一页没有多会话 tab 条——tab 是工作台内部的切换器', async () => {
    stubFetch({ '/api/sessions': { body: SESSIONS }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    expect(w.element.querySelector('[role="tablist"]')).toBeNull()
    expect(w.element.querySelector('.af-tabbar')).toBeNull()
  })

  it('只有进行中的会话时不画一个空的历史区', async () => {
    stubFetch({ '/api/sessions': { body: [SESSIONS[0]] }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    expect(w.element.querySelectorAll('section')).toHaveLength(1)
    expect(w.element.textContent).not.toContain('历史')
  })

  it('设备名取自设备列表，查不到时显示 node_id', async () => {
    stubFetch({
      '/api/sessions': {
        body: [session({ workspace: { node_id: 'n_ghost', path: '/srv/x', path_locked: true } })],
      },
      '/api/devices': { body: DEVICES },
    })
    const w = mountIn(SessionsPage)
    await flush(w)

    expect(w.element.textContent).toContain('n_ghost')
  })
})

describe('SessionsPage：空态', () => {
  it('一个会话都没有时说清会话落在哪儿，并给出两条出路', async () => {
    stubFetch({ '/api/sessions': { body: [] }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有会话')
    expect(text).toContain('设备页')
    expect(text).toContain('新建会话')
    expect(w.element.querySelector('tbody')).toBeNull()
  })
})

describe('SessionsPage：搜索', () => {
  it('按标题 / 设备 / 路径过滤', async () => {
    stubFetch({ '/api/sessions': { body: SESSIONS }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    const input = w.element.querySelector<HTMLInputElement>('[data-role="search"]')
    expect(input).not.toBeNull()
    await w.vm.$nextTick()
    input!.value = 'beta'
    input!.dispatchEvent(new Event('input'))
    await flush(w)

    expect(w.element.textContent).toContain('beta')
    expect(w.element.textContent).not.toContain('alpha')
  })

  it('匹配不到时给出「没有匹配」而不是「还没有会话」', async () => {
    stubFetch({ '/api/sessions': { body: SESSIONS }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    const input = w.element.querySelector<HTMLInputElement>('[data-role="search"]')
    input!.value = 'zzz-nope'
    input!.dispatchEvent(new Event('input'))
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('没有匹配的会话')
    expect(text).not.toContain('还没有会话')
  })

  it('清空搜索后列表回来，焦点回到搜索框', async () => {
    stubFetch({ '/api/sessions': { body: SESSIONS }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    const input = w.element.querySelector<HTMLInputElement>('[data-role="search"]')
    input!.value = 'zzz-nope'
    input!.dispatchEvent(new Event('input'))
    await flush(w)

    await w.element.querySelector<HTMLElement>('.af-empty__actions button')?.click()
    await flush(w)

    expect(w.element.textContent).toContain('alpha')
    expect(document.activeElement).toBe(w.element.querySelector('[data-role="search"]'))
  })
})

describe('SessionsPage：读失败', () => {
  it('Hub 返回错误码时显示可读理由', async () => {
    stubFetch({ '/api/sessions': { status: 503, body: { error: 'unavailable' } }, '/api/devices': { body: [] } })
    const w = mountIn(SessionsPage)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('unavailable')
  })
})

describe('SessionsPage：进入会话', () => {
  it('点标题进工作台，地址栏是 /sessions/<id>', async () => {
    stubFetch({ '/api/sessions': { body: SESSIONS }, '/api/devices': { body: DEVICES } })
    const w = mountIn(SessionsPage)
    await flush(w)

    await w.element.querySelector<HTMLElement>('tbody .af-link')?.click()
    expect(location.pathname).toBe('/sessions/s_1')
  })
})
