import { describe, expect, it } from 'vitest'

import NewSessionPage from '@/pages/NewSessionPage.vue'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

const ONLINE = device({ id: 'n_1', name: 'mbp.local', status: 'online' })

function type(input: HTMLInputElement, value: string): void {
  input.value = value
  input.dispatchEvent(new Event('input'))
}

describe('NewSessionPage：渲染', () => {
  it('三步都在同一列里，agent 那一步明写 M3 起可选', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('① 设备')
    expect(text).toContain('② agent')
    expect(text).toContain('③ 工作区路径')
    expect(text).toContain('M3 起可选')
    expect(text).toContain('现在没有可选 agent')
  })

  it('路径是创建期的一次性输入，页面上写明建立后只读', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('建立后不能换设备、不能换 agent、路径不可改')
    expect(text).toContain('必须')
    expect(text).toContain('已经存在于选中的设备上')
  })

  it('这一页没有「打开工作区」入口——工作区不是路由', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('没有任何「打开工作区」的入口')
    const hrefs = Array.from(w.element.querySelectorAll('a')).map((a) => a.getAttribute('href'))
    expect(hrefs.some((h) => h?.includes('/w/') || h?.includes('workspace'))).toBe(false)
  })
})

describe('NewSessionPage：空态', () => {
  it('一台设备都没有时说清会话必须落在设备上，并给出去设备页的路', async () => {
    stubFetch({ '/api/devices': { body: [] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('没有设备可以开会话')
    expect(text).toContain('afcli run')
    expect(w.element.querySelector('.af-opt')).toBeNull()
  })
})

describe('NewSessionPage：选设备', () => {
  it('用 ↑↓ 移动、⏎ 选中一台设备（列表是唯一 Tab 落点）', async () => {
    stubFetch({
      '/api/devices': { body: [device({ id: 'n_1', name: 'a' }), device({ id: 'n_2', name: 'b' })] },
      '/api/agents': { body: [] },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const list = w.element.querySelector<HTMLElement>('.af-opts')
    expect(list?.getAttribute('tabindex')).toBe('0')
    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('.af-opt'))
    expect(rows.every((r) => r.getAttribute('tabindex') === '-1')).toBe(true)

    rows[1]!.focus()
    rows[1]!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))
    await flush(w)

    expect(rows[1]?.getAttribute('aria-selected')).toBe('true')
    expect(w.element.querySelector('.af-summary')?.textContent).toContain('b')
  })

  it('离线设备仍列出来并写明原因，但不会被自动选中', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_off', status: 'offline' })] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const row = w.element.querySelector<HTMLElement>('.af-opt')
    expect(row?.getAttribute('data-inert')).toBe('true')
    expect(row?.textContent).toContain('已判离线')
    expect(row?.getAttribute('aria-selected')).toBe('false')
  })

  it('换设备清掉路径：路径是「这台设备上的」一个目录', async () => {
    stubFetch({
      '/api/devices': { body: [device({ id: 'n_1', name: 'a' }), device({ id: 'n_2', name: 'b' })] },
      '/api/agents': { body: [] },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('.af-opt'))
    await rows[0]?.click()
    await flush(w)
    const path = w.element.querySelector<HTMLInputElement>('#path')
    type(path!, '/srv/alpha')
    await flush(w)
    expect(path?.value).toBe('/srv/alpha')

    await rows[1]?.click()
    await flush(w)
    expect(w.element.querySelector<HTMLInputElement>('#path')?.value).toBe('')
  })
})

describe('NewSessionPage：提交', () => {
  async function pickAndType(w: ReturnType<typeof mountIn>, path: string): Promise<void> {
    await w.element.querySelector<HTMLElement>('.af-opt')?.click()
    await flush(w)
    type(w.element.querySelector<HTMLInputElement>('#path')!, path)
    await flush(w)
  }

  it('三项齐了才能建；缺哪项错误就指哪一项', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const submit = w.element.querySelector<HTMLButtonElement>('.af-submit button')
    expect(submit?.disabled).toBe(true)

    await pickAndType(w, 'relative/path')
    expect(w.element.querySelector('.af-field__error')?.textContent).toContain('以 / 或 ~ 开头')

    type(w.element.querySelector<HTMLInputElement>('#path')!, '/srv/alpha')
    await flush(w)
    expect(w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled).toBe(false)
  })

  it('提交成功后 replace 跳到工作台（后退不该回到提交过的表单）', async () => {
    const fn = stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [] },
      '/api/sessions': {
        status: 201,
        body: {
          id: 's_new',
          agent_id: null,
          state: 'running',
          created_at: 'unix:1700000000',
          ended_at: null,
          workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true },
        },
      },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await pickAndType(w, '/srv/alpha')

    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    expect(location.pathname).toBe('/sessions/s_new')
    const post = fn.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'POST')
    expect(post).toBeTruthy()
    expect(JSON.parse(String((post?.[1] as RequestInit).body))).toEqual({ node_id: 'n_1', path: '/srv/alpha' })
  })

  it('建会话失败时把 Hub 的错误码翻成人话，并留在页面上', async () => {
    stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [] },
      '/api/sessions': { status: 409, body: { error: 'device_offline' } },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await pickAndType(w, '/srv/alpha')

    const before = location.pathname
    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    const alert = w.element.querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('离线')
    // 失败不跳走：地址没动，表单里的选择也还在，用户改一下就能重试
    expect(location.pathname).toBe(before)
    expect(w.element.querySelector<HTMLInputElement>('#path')?.value).toBe('/srv/alpha')
    expect(w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled).toBe(false)
  })

  it('取消回会话列表', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const cancel = Array.from(w.element.querySelectorAll<HTMLAnchorElement>('.af-submit a'))[0]
    cancel!.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
    await flush(w)
    expect(location.pathname).toBe('/sessions')
  })
})

describe('NewSessionPage：读失败', () => {
  it('Hub 连不上时给出可读理由', async () => {
    stubFetch({ '/api/devices': { status: 500, body: { error: 'boom' } }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('boom')
  })
})
