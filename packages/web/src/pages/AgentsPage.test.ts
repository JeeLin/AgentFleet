import { describe, expect, it } from 'vitest'

import AgentsPage from '@/pages/AgentsPage.vue'
import { agent, device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

describe('AgentsPage：渲染', () => {
  // M2 起设备会探测并上报 agent，所以「恒空」这个前提被推翻了。
  // 这里改断言**新的**事实：横幅解释「取决于设备装了什么」，而不是
  // 「M1 没有所以恒空」。原测试的价值（不留一个「做了但永远空」的页面）
  // 保留在「空态仍要解释为什么空」上。
  it('常驻横幅说明列表取决于设备装了什么，而不是「M1 恒空」', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    const note = w.element.querySelector('[role="note"]')
    expect(note?.textContent).toContain('不再必然为空')
    expect(note?.textContent).toContain('探测不到就是空')
    // 安装必须用户手动触发——这条产品边界要写在页面上
    expect(note?.textContent).toContain('手动触发')

    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('还没有设备上报 agent')
  })

  it('空态说明「为什么空」，不是加载失败', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [device(), device({ id: 'n_2' })] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    const text = w.element.querySelector('.af-empty')?.textContent ?? ''
    // 要点：说清「装了就会出现」，且「不编造」，而不是一句「什么都没有」
    expect(text).toContain('2 台设备')
    expect(text).toContain('opencode')
    expect(text).toContain('不会编造')
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

  // 列表含义从「已装且就绪」改成了「已装」：装了但跑不起来的也要列出来
  // （用户要能看见自己装坏的那个），可用性门禁在建会话那一步判。
  it('有 agent 时按名字列出，并说明列的是「已装」而不是「能用」', async () => {
    stubFetch({ '/api/agents': { body: [agent()] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    expect(w.element.querySelector('.af-roster')?.textContent).toContain('opencode')
    const sub = w.element.querySelector('.af-page__sub')?.textContent ?? ''
    expect(sub).toContain('已装')
    // 明确不承诺「就绪」：那是建会话时判的
    expect(sub).not.toContain('已装且就绪')
    expect(sub).toContain('建会话时判定')
  })

  /** 装了但跑不起来的：**列出 + 带原因 + 不可用**，三者缺一不可。 */
  it('装了但不能对话的那个：列出来、带出原因、且「开会话」不可点', async () => {
    stubFetch({
      '/api/agents': {
        body: [
          agent({ name: 'opencode' }),
          agent({ name: 'claude', available: false, error: '没有 acp 子命令' }),
        ],
      },
      '/api/devices': { body: [device()] },
    })
    const w = mountIn(AgentsPage)
    await flush(w)

    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('.af-agent'))
    const broken = rows.find((r) => r.getAttribute('data-usable') === 'false')
    // 列出来：只给可用的那批，「装坏了」这件事在页面上等于不存在
    expect(broken, '不可用的那行必须在列表里').toBeTruthy()
    expect(broken?.textContent).toContain('claude')
    // 带出原因：只把它灰掉，用户读到的是「我装了它但系统说没有」
    expect(broken?.textContent).toContain('没有 acp 子命令')
    // 不可点：点了就会进到一个永远选不到它的新建页
    expect(broken?.querySelector<HTMLButtonElement>('button')?.disabled).toBe(true)

    const ok = rows.find((r) => r.getAttribute('data-usable') === 'true')
    expect(ok?.querySelector<HTMLButtonElement>('button')?.disabled).toBe(false)
  })

  it('设备没给原因时不编一句——只说不能对话', async () => {
    stubFetch({
      '/api/agents': { body: [agent({ name: 'claude', available: false })] },
      '/api/devices': { body: [device()] },
    })
    const w = mountIn(AgentsPage)
    await flush(w)

    expect(w.element.querySelector('.af-agent')?.textContent).toContain('现在不能对话（设备没给原因）')
  })

  it('读失败时给可读理由', async () => {
    stubFetch({ '/api/agents': { status: 500, body: { error: 'boom' } }, '/api/devices': { body: [] } })
    const w = mountIn(AgentsPage)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('boom')
  })
})
