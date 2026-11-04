import { describe, expect, it } from 'vitest'

import DeviceDetailPage from '@/domains/device/DeviceDetailPage.vue'
import type { DeviceDetail } from '@/domains/device/api'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate, route } from '@/router'

/** 等 fetch 链 + Vue 渲染队列都走完。 */
async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 一条完整的设备详情。各个用例只覆盖自己关心的那块。 */
function detail(over: Partial<DeviceDetail> = {}): DeviceDetail {
  return {
    summary: device({ id: 'n_1', name: 'mbp.local' }),
    bindings: [],
    disk: null,
    failure_breakdown: { succeeded: 0, failed: 0, aborted: 0, in_flight: 0 },
    deletable: { allowed: true, reason: null, executions: 0 },
    ...over,
  }
}

/** 把地址停在设备详情页，再挂组件（页面从 `route.value.params.id` 读 id）。 */
function mountAt(id: string) {
  navigate(`/devices/${id}`)
  return mountIn(DeviceDetailPage)
}

describe('DeviceDetailPage：原有内容不减少', () => {
  it('机器那一块把设备列表里的字段都带过来（node_id / 指纹 / 平台 / 资源）', async () => {
    stubFetch({ '/api/devices/n_1/detail': { body: detail() } })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('n_1')
    expect(text).toContain('9f3***21b')
    expect(text).toContain('darwin/arm64')
    expect(text).toContain('CPU 20%')
    expect(text).toContain('内存 40%')
  })

  it('新加的三块都在页面上（绑定关系 / 磁盘 / 失败归因）', async () => {
    stubFetch({ '/api/devices/n_1/detail': { body: detail() } })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('绑定关系')
    expect(text).toContain('磁盘')
    expect(text).toContain('失败归因')
    expect(text).toContain('能不能删')
  })
})

describe('DeviceDetailPage：还没绑项目', () => {
  it('说「还没有绑定项目」并告诉用户去哪里绑，而不是一块空白', async () => {
    stubFetch({ '/api/devices/n_1/detail': { body: detail() } })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有绑定项目')
    // 光说「没有」不够：用户要知道下一步去哪。
    expect(text).toContain('去项目页')
  })

  it('没绑定时磁盘那一块也说清是「没有绑定」，不是「已用 0 B」', async () => {
    stubFetch({ '/api/devices/n_1/detail': { body: detail() } })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有磁盘数据')
    expect(text).not.toContain('已用 0 B')
  })
})

describe('DeviceDetailPage：绑定关系', () => {
  it('列出项目名、固定目录与清理策略——三档的差别要写出来，不只是名字', async () => {
    stubFetch({
      '/api/devices/n_1/detail': {
        body: detail({
          bindings: [
            {
              id: 'b1',
              project_id: 'p1',
              project_name: '指标台',
              fixed_dir: '/srv/metrics',
              disk_quota_bytes: 1024,
              cleanup_policy: 'conservative',
              used_bytes: null,
            },
          ],
          disk: { quota_bytes: 1024, used_bytes: null, percent: null, accepts_new_work: true },
        }),
      },
    })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('指标台')
    expect(text).toContain('/srv/metrics')
    // 「保守」+ 实际差别（留 30 天 / 留 3 个），不是只有那个词
    expect(text).toContain('留 30 天')
    expect(text).toContain('留最近 3 个')
  })

  it('设备还没报用量时显示「—」+ 说明，而不是 0 B', async () => {
    stubFetch({
      '/api/devices/n_1/detail': {
        body: detail({
          bindings: [
            {
              id: 'b1',
              project_id: 'p1',
              project_name: '指标台',
              fixed_dir: '/srv/metrics',
              disk_quota_bytes: null,
              cleanup_policy: 'balanced',
              used_bytes: null,
            },
          ],
          disk: { quota_bytes: null, used_bytes: null, percent: null, accepts_new_work: true },
        }),
      },
    })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('用量 —')
    expect(text).toContain('设备未上报')
    // 「0 B」是一个真实测量结果，用来说「没上报」会把人带偏。
    expect(text).not.toContain('0 B')
  })

  it('点项目名进那个项目的看板（设备页是管理面，跳过去才看得到项目内）', async () => {
    stubFetch({
      '/api/devices/n_1/detail': {
        body: detail({
          bindings: [
            {
              id: 'b1',
              project_id: 'p1',
              project_name: '指标台',
              fixed_dir: '/srv/metrics',
              disk_quota_bytes: null,
              cleanup_policy: 'balanced',
              used_bytes: null,
            },
          ],
        }),
      },
    })
    const w = mountAt('n_1')
    await flush(w)

    w.element.querySelector<HTMLButtonElement>('.af-devd__bind-name')?.click()
    await settle(1)
    expect(route.value.path).toBe('/project/p1/board')
  })
})

describe('DeviceDetailPage：失败归因', () => {
  it('中止单列并且注明「不是失败」——混进失败会让人去查一个不存在的问题', async () => {
    stubFetch({
      '/api/devices/n_1/detail': {
        body: detail({
          failure_breakdown: { succeeded: 5, failed: 0, aborted: 2, in_flight: 1 },
        }),
      },
    })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('中止（不是失败）')
    expect(text).toContain('2 次')
    expect(text).toContain('没有失败记录')
    // 进行中要说明它不进成功率
    expect(text).toContain('进行中（不进成功率）')
  })

  it('一次都没跑过时说「还没有跑过任何任务」，不说「没有失败」', async () => {
    stubFetch({ '/api/devices/n_1/detail': { body: detail() } })
    const w = mountAt('n_1')
    await flush(w)

    expect(w.element.textContent ?? '').toContain('还没有跑过任何任务')
  })
})

describe('DeviceDetailPage：能不能删', () => {
  it('不能删时把理由说出来（带条数），不是只把按钮灰掉', async () => {
    stubFetch({
      '/api/devices/n_1/detail': {
        body: detail({
          failure_breakdown: { succeeded: 1, failed: 0, aborted: 0, in_flight: 0 },
          deletable: {
            allowed: false,
            reason: '有 1 条历史执行记录，不能删除。要下线就先归档项目，或把项目转到别的设备。',
            executions: 1,
          },
        }),
      },
    })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('不能删除')
    expect(text).toContain('1 条历史执行记录')
  })

  it('能删时说清「移除之后会话仍然保留」——「删除」这个词会让人以为对话也没了', async () => {
    stubFetch({ '/api/devices/n_1/detail': { body: detail() } })
    const w = mountAt('n_1')
    await flush(w)

    expect(w.element.textContent ?? '').toContain('仍然保留')
  })
})

describe('DeviceDetailPage：三种「空」分开', () => {
  it('读失败显示红条与重试，而不是空态', async () => {
    stubFetch({ '/api/devices/n_1/detail': { status: 500, body: { error: 'internal' } } })
    const w = mountAt('n_1')
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')).not.toBeNull()
    expect(w.element.textContent ?? '').toContain('重试')
  })

  it('设备不存在（404）说「找不到这台设备」，并提一句会话还在', async () => {
    stubFetch({ '/api/devices/n_1/detail': { status: 404, body: { error: 'not_found' } } })
    const w = mountAt('n_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('找不到这台设备')
    expect(text).toContain('会话与轨迹仍然保留')
  })
})
