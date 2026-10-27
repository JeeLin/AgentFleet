import { describe, expect, it } from 'vitest'

import AgentCatalog from '@/components/AgentCatalog.vue'
import { agent, device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/**
 * `AgentCatalog` 原是 `pages/AgentsPage.vue`（一条独立路由 `/agents`）。
 * 用户裁定侧栏只留会话 / 设备 / 设置，目录并进设备页的 `?view=agents` 分段，
 * 所以它降级成组件，测试跟着搬到 `components/` 旁边。
 *
 * **断言一条都没减少**：原来守的产品边界（不编造、装坏的要列出来并带原因、
 * 空态给去处、读失败给可读理由）逐条保留。只改了三处**载体**：
 * - 「点按钮改 URL」改成「发 `start-session` 事件」——组件不该知道路由，
 *   去哪由设备页决定（这也是合并后两处入口能共用同一个组件的原因）；
 * - 「横幅里写着『手动触发』」改成「写着 Hub 从不往设备写可执行文件」——
 *   同一个产品边界（安装不由 Hub 代劳），但 M2 已裁定不支持安装，
 *   旧文案承诺的那条手动流程本身就不存在了；
 * - `.af-page__sub`（页面级副标题）改成本组件内的 `.af-foot` / 横幅正文——
 *   组件不再有页面外壳，那条 `<h1>` 与副标题归 `DevicesPage` 管。
 */
describe('AgentCatalog：渲染', () => {
  it('常驻横幅说明列表取决于设备装了什么，而不是「M1 恒空」', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentCatalog)
    await flush(w)

    const note = w.element.querySelector('[role="note"]')
    expect(note?.textContent).toContain('不再必然为空')
    expect(note?.textContent).toContain('探测不到就是空')
    // 产品边界：Hub 从不往设备写可执行文件（M2 撤销了「安装」这件事）
    expect(note?.textContent).toContain('从不往设备写可执行文件')

    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('还没有设备上报 agent')
  })

  it('空态说明「为什么空」，不是加载失败', async () => {
    stubFetch({
      '/api/agents': { body: [] },
      '/api/devices': { body: [device(), device({ id: 'n_2' })] },
    })
    const w = mountIn(AgentCatalog)
    await flush(w)

    const text = w.element.querySelector('.af-empty')?.textContent ?? ''
    // 要点：说清「装了就会出现」，且「不编造」，而不是一句「什么都没有」
    expect(text).toContain('2 台设备')
    expect(text).toContain('opencode')
    expect(text).toContain('不会编造')
  })

  it('空态给出去处（新建会话），不把用户留在死路上', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [] } })
    const w = mountIn(AgentCatalog)
    await flush(w)

    const btn = w.element.querySelector<HTMLButtonElement>('.af-empty__actions button')
    expect(btn?.textContent).toContain('新建会话')
    // 组件不自己跳：它把意图交给父页面（`emit('start-session')`），
    // 「去哪」由设备页决定——见文件头注释。这里断言**确实发了事件**。
    await btn?.click()
    await settle(1)
    expect(w.emitted('start-session')).toHaveLength(1)
  })

  it('「用它开会话」把意图交给父页面，不自己改地址', async () => {
    stubFetch({ '/api/agents': { body: [agent()] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentCatalog)
    await flush(w)

    const before = location.pathname
    await w.element.querySelector<HTMLButtonElement>('.af-agent button')?.click()
    await settle(1)
    expect(w.emitted('start-session')).toHaveLength(1)
    // 反证：组件里没有 `navigate()`，地址不动
    expect(location.pathname).toBe(before)
  })

  it('有 agent 时按名字列出，并说明列的是「已装」而不是「能用」', async () => {
    stubFetch({ '/api/agents': { body: [agent()] }, '/api/devices': { body: [device()] } })
    const w = mountIn(AgentCatalog)
    await flush(w)

    expect(w.element.querySelector('.af-roster')?.textContent).toContain('opencode')
    // 横幅正文承担了原页面副标题的那条说明：列的是「已装」，不是「能用」
    const note = w.element.querySelector('[role="note"]')?.textContent ?? ''
    expect(note).toContain('装了哪些 agent')
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
    const w = mountIn(AgentCatalog)
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
    const w = mountIn(AgentCatalog)
    await flush(w)

    expect(w.element.querySelector('.af-agent')?.textContent).toContain(
      '现在不能对话（设备没给原因）',
    )
  })

  it('读失败时给可读理由', async () => {
    stubFetch({
      '/api/agents': { status: 500, body: { error: 'boom' } },
      '/api/devices': { body: [] },
    })
    const w = mountIn(AgentCatalog)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('boom')
  })
})
