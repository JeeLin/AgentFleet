import { describe, expect, it } from 'vitest'

import AgentDetailPage from '@/domains/agent/AgentDetailPage.vue'
import type { AgentDetail } from '@/domains/agent/api'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate, route } from '@/router'

/** 等 fetch 链 + Vue 渲染队列都走完。 */
async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/**
 * 一份 agent 详情。**默认是「一次都没跑过」那一态**（`success_rate: null`）：
 * 那是最容易被写错的一态（写成 0%），所以让它当基准，其余用例各自覆盖。
 */
function detail(over: Partial<AgentDetail> = {}): AgentDetail {
  return {
    name: 'opencode',
    available: true,
    error: null,
    devices: [
      {
        node_id: 'n_1',
        device_name: 'mbp.local',
        available: true,
        probe_error: null,
        executions: 0,
      },
    ],
    task_history: [],
    breakdown: { succeeded: 0, failed: 0, aborted: 0, in_flight: 0 },
    success_rate: null,
    task_history_caveat: CAVEAT,
    ...over,
  }
}

const CAVEAT =
  '执行记录里没有记「这条是哪个 agent 跑的」，所以下面按「执行所在的那台设备上装了这个 agent」反推。' +
  '同一台设备上装了多个 agent 时，它们会共享同一批执行。'

/** 把地址停在 agent 详情页，再挂组件（页面从 `route.value.params.name` 读名字）。 */
function mountAt(name: string) {
  navigate(`/agents/${name}`)
  return mountIn(AgentDetailPage)
}

describe('AgentDetailPage：0/0 不是 0%', () => {
  it('一次都没跑过时成功率显示「—」，不显示 0%', async () => {
    stubFetch({ '/api/agents/opencode': { body: detail() } })
    const w = mountAt('opencode')
    await flush(w)

    const rate = w.element.querySelector('.af-agd__rate-value')?.textContent?.trim()
    expect(rate).toBe('—')
    // 反证：这是「还没跑过」，不是「一次都没成」。**只断那个数字格子**——
    // 给用户解释「0/0 不是一个比例」的那句话里本来就会出现「0%」字样，
    // 断全页文本会把那句正确的说明也判成错。
    expect(rate).not.toContain('0%')
    expect(w.element.textContent ?? '').toContain('0/0 不是一个比例')
  })

  it('说清为什么是「—」——0/0 不是一个比例', async () => {
    stubFetch({ '/api/agents/opencode': { body: detail() } })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没跑过任何任务')
    expect(text).toContain('不是 0%')
  })

  it('跑过但全败是**真实的** 0%，不能被误伤成「—」', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          success_rate: 0,
          breakdown: { succeeded: 0, failed: 2, aborted: 0, in_flight: 0 },
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const rate = w.element.querySelector('.af-agd__rate-value')?.textContent?.trim()
    expect(rate).toBe('0%')
    expect(w.element.textContent ?? '').toContain('已结束 2 次')
  })

  it('成功率正常时显示百分数，并列出已成 / 已败 / 已中止', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          success_rate: 66,
          breakdown: { succeeded: 2, failed: 1, aborted: 0, in_flight: 1 },
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(w.element.querySelector('.af-agd__rate-value')?.textContent?.trim()).toBe('66%')
    expect(text).toContain('已结束 3 次')
    // 进行中的**不进分母**：这句话要写在页面上，否则用户算不出 66 是怎么来的。
    expect(text).toContain('进行中的不进分母')
  })
})

describe('AgentDetailPage：口径说明必须渲染出来', () => {
  it('把 task_history_caveat 原文显示在页面上', async () => {
    stubFetch({ '/api/agents/opencode': { body: detail() } })
    const w = mountAt('opencode')
    await flush(w)

    // 断原文而不是断「有这段」：这段话是随响应下发的，页面**照抄**。
    // 页面自己编一句不一样的话，就等于用户读到的口径与实际统计口径不同。
    expect(w.element.textContent ?? '').toContain(CAVEAT)
  })

  it('有历史时那句话也在——不能只在空态里出现', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          success_rate: 100,
          breakdown: { succeeded: 1, failed: 0, aborted: 0, in_flight: 0 },
          task_history: [
            {
              execution_id: 'e1',
              task_id: 't1',
              task_title: '加个健康检查',
              project_id: 'p1',
              project_name: '指标台',
              device_id: 'n_1',
              state: 'succeeded',
              created_at: 'unix:1700000000',
            },
          ],
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    expect(w.element.textContent ?? '').toContain(CAVEAT)
  })
})

describe('AgentDetailPage：跑过的任务', () => {
  it('列出任务标题、项目名与状态', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          success_rate: 50,
          breakdown: { succeeded: 1, failed: 1, aborted: 0, in_flight: 0 },
          task_history: [
            {
              execution_id: 'e2',
              task_id: 't2',
              task_title: '补齐导出',
              project_id: 'p2',
              project_name: '个人笔记站',
              device_id: 'n_2',
              state: 'failed',
              created_at: 'unix:1700000200',
            },
            {
              execution_id: 'e1',
              task_id: 't1',
              task_title: '加个健康检查',
              project_id: 'p1',
              project_name: '指标台',
              device_id: 'n_1',
              state: 'succeeded',
              created_at: 'unix:1700000100',
            },
          ],
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('加个健康检查')
    expect(text).toContain('指标台')
    expect(text).toContain('补齐导出')
    expect(text).toContain('个人笔记站')
    // 状态不只靠颜色：状态徽标必须带文字。
    expect(text).toContain('成功')
    expect(text).toContain('失败')
  })

  it('一次都没跑过时说「还没有跑过任务」，不是一张空表', async () => {
    stubFetch({ '/api/agents/opencode': { body: detail() } })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有跑过任务')
    // 空表头（任务 / 项目 / 设备…）会被读成「有数据但没渲染出来」。
    expect(w.element.querySelector('.af-agd__table')).toBeNull()
  })

  it('任务行已不存在时如实说「任务已不在」，不编一个标题', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          success_rate: 100,
          breakdown: { succeeded: 1, failed: 0, aborted: 0, in_flight: 0 },
          task_history: [
            {
              execution_id: 'e1',
              task_id: 't_gone',
              task_title: null,
              project_id: 'p1',
              project_name: '指标台',
              device_id: 'n_1',
              state: 'succeeded',
              created_at: 'unix:1700000000',
            },
          ],
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('任务已不在')
    // id 要露出来，否则用户没法拿去搜。
    expect(text).toContain('t_gone')
  })
})

describe('AgentDetailPage：装在哪几台机器上', () => {
  it('列出设备名与各自的执行次数', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          devices: [
            {
              node_id: 'n_1',
              device_name: 'mbp.local',
              available: true,
              probe_error: null,
              executions: 3,
            },
            {
              node_id: 'n_2',
              device_name: 'pi.local',
              available: false,
              probe_error: '没有 acp 子命令',
              executions: 0,
            },
          ],
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('mbp.local')
    expect(text).toContain('pi.local')
    expect(text).toContain('3 次执行')
    // 跑不起来的那台要带原因，否则用户不知道该修什么。
    expect(text).toContain('没有 acp 子命令')
  })

  it('点设备名进那台设备的详情页', async () => {
    stubFetch({ '/api/agents/opencode': { body: detail() } })
    const w = mountAt('opencode')
    await flush(w)

    w.element.querySelector<HTMLButtonElement>('.af-agd__dev-name')?.click()
    await settle(1)
    expect(route.value.path).toBe('/devices/n_1')
  })
})

describe('AgentDetailPage：没装过', () => {
  it('404 说「这台机器上没装它」并说明 Hub 不代装', async () => {
    stubFetch({ '/api/agents/ghost': { status: 404, body: { error: 'not_found' } } })
    const w = mountAt('ghost')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('没装它')
    expect(text).toContain('不代装')
    // 404 不是「读失败」，不该出现重试那条红字的措辞。
    expect(text).not.toContain('失败（not_found）')
  })

  it('读失败显示红条与重试，而不是「没装」', async () => {
    stubFetch({ '/api/agents/opencode': { status: 500, body: { error: 'internal' } } })
    const w = mountAt('opencode')
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')).not.toBeNull()
    expect(w.element.textContent ?? '').toContain('重试')
  })
})

describe('AgentDetailPage：全都跑不起来', () => {
  it('把各设备的原因汇总显示出来', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          available: false,
          error: '没有 acp 子命令',
          devices: [
            {
              node_id: 'n_1',
              device_name: 'mbp.local',
              available: false,
              probe_error: '没有 acp 子命令',
              executions: 0,
            },
          ],
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('全都不行')
    expect(text).toContain('没有 acp 子命令')
  })

  /**
   * 全都不行、**且哪台都没报原因**（`error: null`）：页面不该编一句原因，
   * 也不该整块消失——「装了但跑不起来」这个事实本身要留下。
   */
  it('都没报原因时不编一句话，但「跑不起来」这个事实仍在', async () => {
    stubFetch({
      '/api/agents/opencode': {
        body: detail({
          available: false,
          error: null,
          devices: [
            {
              node_id: 'n_1',
              device_name: 'mbp.local',
              available: false,
              probe_error: null,
              executions: 0,
            },
          ],
        }),
      },
    })
    const w = mountAt('opencode')
    await flush(w)

    const text = w.element.textContent ?? ''
    // 设备行里那枚徽标说的是「跑不起来」，不因没有原因而变成「能对话」。
    expect(text).toContain('跑不起来')
    expect(text).not.toContain('全都不行')
  })
})
