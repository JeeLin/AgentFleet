import { describe, expect, it } from 'vitest'

import AgentsPage from '@/pages/AgentsPage.vue'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

describe('AgentsPage：渲染', () => {
  it('常驻横幅明写 M1 无数据、M3 起填充（不留一个「做了但永远空」的页面）', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    const note = w.element.querySelector('[role="note"]')
    expect(note?.textContent).toContain('M1 没有 agent 运行时可探测')
    expect(note?.textContent).toContain('M3')

    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('M1 无数据，M3 起填充')
  })

  it('空态说明「这是预期结果」，不是加载失败', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [device(), device({ id: 'n_2' })] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    expect(w.element.querySelector('.af-empty')?.textContent).toContain('M1 的预期结果')
  })

  it('空态给出去处（新建会话），不把用户留在死路上', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    const btn = w.element.querySelector<HTMLButtonElement>('.af-empty__actions button')
    expect(btn?.textContent).toContain('新建会话')
    await btn?.click()
    expect(location.pathname).toBe('/sessions/new')
  })

  it('有 agent 时按名字列出，并说明来源是心跳上报的「已装且就绪」', async () => {
    stubFetch({ '/api/agents': { body: [{ name: 'opencode' }] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    expect(w.element.querySelector('.af-roster')?.textContent).toContain('opencode')
    expect(w.element.textContent).toContain('已装且就绪')
  })

  it('读失败时给可读理由', async () => {
    stubFetch({ '/api/agents': { status: 500, body: { error: 'boom' } }, '/api/devices': { body: [] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('boom')
  })
})
