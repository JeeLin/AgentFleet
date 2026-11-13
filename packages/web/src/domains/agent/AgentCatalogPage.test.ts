/* ===========================================================================
   AgentCatalogPage 的测试（v0.8.0 子任务 12）
   ---------------------------------------------------------------------------
   这一页是「逐屏对齐」里差距最大的一页：`prototype/agents.html` 有 4 个
   `<h2>` 分区，实现此前只有 1 个（「任务与成功率」，而且它是**原型里没有的**
   一块——原型的分区是「设备上的 Agent / 启动配置 / 能力由任务授予 / 问 Agent
   的事」）。

   ── 为什么断言「4 个分区标题」而不是「页面变长了」 ────────────────
   分区数是本子任务唯一可证伪的判据（里程碑测试标准 2：`grep -c` 与原型 h2 数
   一致）。断言标题的**文案**而不只是数量：数量对了但四块都叫「配置」的话，
   用户仍然分不清哪一块回答哪个问题。

   ── 判红方式 ────────────────────────────────────────────────────
   把 `AgentCatalog.vue` 里新加的 `af-region__head`（设备上的 Agent 之标题）
   删掉 → 第一条立刻红（只剩 3 个）。把本页任一分区删掉 → 同理。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import AgentCatalogPage from '@/domains/agent/AgentCatalogPage.vue'
import { agent, device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 有数据时四个分区都在（原型 `:36` / `:85` / `:122` / `:166`）。 */
describe('AgentCatalogPage：原型要求的 4 个分区', () => {
  it('原型 4 个 h2 分区逐个存在，且文案对得上', async () => {
    stubFetch({
      '/api/agents': { body: [agent({ name: 'opencode' })] },
      '/api/devices': { body: [device()] },
    })
    const w = mountIn(AgentCatalogPage)
    await flush(w)

    const titles = Array.from(
      w.element.querySelectorAll('.af-region__title'),
    ).map((el) => el.textContent?.trim() ?? '')

    // 判据一：数量与原型 h2 数一致（4）。
    expect(titles).toHaveLength(4)
    // 判据二：每一块回答的问题不同，所以文案必须不同。
    expect(titles[0]).toContain('设备上的')
    expect(titles[1]).toContain('启动配置')
    expect(titles[2]).toContain('能力，由任务授予')
    expect(titles[3]).toContain('问 Agent 的事')
  })

  it('第一块的计数来自真数据（可用 / 不可用分开数）', async () => {
    stubFetch({
      '/api/agents': {
        body: [agent({ name: 'opencode' }), agent({ name: 'claude', available: false })],
      },
      '/api/devices': { body: [device(), device({ id: 'n_2' })] },
    })
    const w = mountIn(AgentCatalogPage)
    await flush(w)

    const head = w.element.querySelector('.af-catalog__head')?.textContent ?? ''
    expect(head).toContain('2 台设备上的 2 个 Agent')
    expect(head).toContain('可用 1')
    expect(head).toContain('不可用 1')
  })
})

/**
 * 「能力由任务授予」是**产品边界**，三行各自是一句承诺。
 *
 * 这三行照 `prototype/agents.html:126-147` 搬：哪条禁止、哪条是主路径、
 * 哪条是并行支持。少一行或把「禁止」写成「暂不支持」，意思就变了——
 * 「暂不支持」暗示将来会支持，而原型的判断是「就不该有这条路」。
 */
describe('AgentCatalogPage：能力由任务授予', () => {
  it('三行都在，且 Agent 直接调连接器是「禁止」而不是「暂不支持」', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [] } })
    const w = mountIn(AgentCatalogPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('Agent 直接调用任意连接器')
    expect(text).toContain('禁止')
    expect(text).not.toContain('暂不支持')
    expect(text).toContain('按任务授予')
    expect(text).toContain('主路径')
    expect(text).toContain('MCP')
    // 凭据不进 Agent 上下文：这是这条边界的安全后果，必须写出来。
    expect(text).toContain('凭据不会进 Agent 的上下文')
  })
})

/**
 * 缺 API 的那两块必须**说清缺什么**，而不是留白或摆假控件。
 *
 * 里程碑「后端流程」明写：某页差异的根因是缺 API 时不在本子任务补，登记说明。
 * 页面上对应的承诺是「别让用户以为功能坏了」——所以「还没有接数据」这句话
 * 是交付物，不是文案装饰。
 */
describe('AgentCatalogPage：缺后端能力的部分如实说明', () => {
  it('启动配置说明它还没有接数据，且不摆一个按不动的按钮', async () => {
    stubFetch({ '/api/agents': { body: [] }, '/api/devices': { body: [] } })
    const w = mountIn(AgentCatalogPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('这一块还没有接数据')
    // 漂移这个承诺要留下（它与「不悄悄覆盖」是同一件事）。
    expect(text).toContain('漂移')
    // 反证：原型这里有两个处置按钮（`prototype/agents.html:110-111`），
    // 但没有端点时不该**渲染出按钮**。断言按钮而不是断言文案：说明句本身
    // 会引用这两个动作名（「把本地改动收上来还是把 Hub 配置推下去」），
    // 用 `not.toContain(文案)` 会把正确的说明写成失败。
    const buttons = Array.from(w.element.querySelectorAll('button')).map((b) =>
      b.textContent?.trim() ?? '',
    )
    expect(buttons).not.toContain('把本地改动收上来')
    expect(buttons).not.toContain('把 Hub 配置推下去')
  })
})
