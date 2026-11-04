/* ===========================================================================
   BindPage 的测试。
   ---------------------------------------------------------------------------
   这一页是 v0.7.0 的主页面，也是**会写数据**的页面（`PUT .../bindings/{id}`）。
   要守的四件事：

   1. 隔离式项目**只有已绑定的设备能选**——这是产品承诺，不是样式细节；
   2. 空目录 / 没选设备时**一个请求都不发**（空目录等于把 worktree 建在盘根）；
   3. 写成功之后跳看板；
   4. 三种失败分开说（读不到 / 写失败 / 解除失败），且**写失败时列表还在**。

   第 4 条是照 `ArchivedPage` 那个缺陷来的：动作失败不该把整页数据换成错误态。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import BindPage from '@/domains/project/BindPage.vue'
import type { Project, ProjectBinding } from '@/domains/project/api'
import { device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate, route } from '@/router'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 10; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

function project(over: Partial<Project> = {}): Project {
  return {
    id: 'p_1',
    name: '内部指标台',
    shape: 'central',
    theme_color: null,
    archived_at: null,
    created_at: 'unix:1700000000',
    ...over,
  }
}

function binding(over: Partial<ProjectBinding> = {}): ProjectBinding {
  return {
    id: 'b_1',
    project_id: 'p_1',
    device_id: 'n_1',
    fixed_dir: '/srv/afleet/metrics',
    disk_quota_bytes: null,
    cleanup_policy: 'balanced',
    used_bytes: null,
    ...over,
  }
}

function mountAt(id = 'p_1'): ReturnType<typeof mountIn<typeof BindPage>> {
  navigate(`/project/${encodeURIComponent(id)}/bind`, { replace: true })
  return mountIn(BindPage)
}

/** 三个请求的最小一套桩。`bindings` 决定已有绑定。 */
function setup(over: {
  proj?: Partial<Project>
  bindings?: ProjectBinding[]
  devices?: ReturnType<typeof device>[]
} = {}) {
  return stubFetch({
    '/api/projects/p_1': { body: project(over.proj) },
    '/api/devices': {
      body: over.devices ?? [
        device({ id: 'n_1', name: 'device-c', status: 'online' }),
        device({ id: 'n_2', name: 'device-a', status: 'online' }),
      ],
    },
    '/api/projects/p_1/bindings': { body: over.bindings ?? [] },
    '/api/projects/p_1/bindings/n_1': { body: binding() },
  })
}

describe('BindPage：原型里的五个区块都在', () => {
  it('五个区块一个不少', async () => {
    setup()
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    for (const title of [
      '选一台设备',
      '固定目录',
      '任务怎么在这个目录里跑',
      '磁盘配额与清理策略',
      '绑定之后可以改什么',
    ]) {
      expect(text, `缺区块「${title}」`).toContain(title)
    }
  })

  it('三档清理策略都写清了差别，不是只给名字', async () => {
    setup()
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    // 只写「保守 / 平衡 / 激进」的话用户没法选——他不知道在保守什么。
    expect(text).toContain('缓存留 7 天')
    expect(text).toContain('缓存留 30 天')
    expect(text).toContain('缓存和产物都尽快清')
  })

  it('「任务怎么跑」说明了工作树的代价，不只说好处', async () => {
    setup()
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('worktree')
    // 代价那一条最容易被省略，而它恰恰是用户要判断的事。
    expect(text).toContain('不共享')
    expect(text).toContain('node_modules')
  })
})

describe('BindPage：地址里没有项目 id', () => {
  it('直接说缺 id，且一个请求都不发', async () => {
    navigate('/', { replace: true })
    const fetchMock = stubFetch({})
    const w = mountIn(BindPage)
    await flush(w)

    expect(w.element.textContent).toContain('这个地址缺少项目 id')
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

describe('BindPage：隔离式项目只有已绑定的设备可选', () => {
  it('没绑定过的设备被禁用，并说明为什么', async () => {
    setup({
      proj: { shape: 'isolated' },
      bindings: [binding({ device_id: 'n_1' })],
    })
    const w = mountAt('p_1')
    await flush(w)

    const radios = Array.from(
      w.element.querySelectorAll<HTMLInputElement>('input[name="af-bind-device"]'),
    )
    expect(radios.length).toBe(2)

    const byValue = new Map(radios.map((r) => [r.value, r]))
    // 已绑定的那台可选，另一台不行。
    expect(byValue.get('n_1')?.disabled).toBe(false)
    expect(byValue.get('n_2')?.disabled).toBe(true)

    // 光灰掉不够：用户要知道原因，否则会以为选上就能升级。
    expect(w.element.textContent).toContain('隔离式项目不能绑到这台')
  })

  it('隔离式项目把限制说在页面上（不是只在代码里）', async () => {
    setup({ proj: { shape: 'isolated' }, bindings: [binding()] })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('隔离式项目只能绑在指定设备上')
    expect(text).toContain('代码永不出设备')
  })

  it('中心式项目所有设备都能选', async () => {
    setup({ proj: { shape: 'central' } })
    const w = mountAt('p_1')
    await flush(w)

    const radios = Array.from(
      w.element.querySelectorAll<HTMLInputElement>('input[name="af-bind-device"]'),
    )
    expect(radios.every((r) => !r.disabled)).toBe(true)
  })
})

describe('BindPage：没有设备', () => {
  it('空态告诉用户先去设备页，而不是一张空表', async () => {
    setup({ devices: [] })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('一台设备都还没注册')
    expect(text).toContain('去设备页')
  })
})

describe('BindPage：保存', () => {
  it('没选设备时拒绝，且不发 PUT', async () => {
    const fetchMock = setup()
    const w = mountAt('p_1')
    await flush(w)

    const before = fetchMock.mock.calls.length
    const save = w.findAll('button').find((b) => b.text().includes('完成绑定'))
    await save?.trigger('click')
    await flush(w)

    expect(w.element.textContent).toContain('先选一台设备')
    const puts = fetchMock.mock.calls.slice(before).filter((c) => c[1]?.method === 'PUT')
    expect(puts).toHaveLength(0)
  })

  it('选了设备但目录为空时拒绝，且不发 PUT', async () => {
    const fetchMock = setup()
    const w = mountAt('p_1')
    await flush(w)

    await w.findAll('input[name="af-bind-device"]')[0]?.setValue(true)
    await flush(w)

    const before = fetchMock.mock.calls.length
    const save = w.findAll('button').find((b) => b.text().includes('完成绑定'))
    await save?.trigger('click')
    await flush(w)

    // 空目录会让 worktree 建在设备的盘根上——那是「清空整块盘」的前一步。
    expect(w.element.textContent).toContain('固定目录不能为空')
    const puts = fetchMock.mock.calls.slice(before).filter((c) => c[1]?.method === 'PUT')
    expect(puts).toHaveLength(0)
  })

  it('填齐之后发 PUT，且 body 去掉前后空格', async () => {
    const fetchMock = setup()
    const w = mountAt('p_1')
    await flush(w)

    await w.findAll('input[name="af-bind-device"]')[0]?.setValue(true)
    await w.find('input[type="text"]').setValue('  /srv/afleet/metrics  ')
    await flush(w)

    const save = w.findAll('button').find((b) => b.text().includes('完成绑定'))
    await save?.trigger('click')
    await flush(w)

    const put = fetchMock.mock.calls.find((c) => c[1]?.method === 'PUT')
    expect(put, '应该发出一条 PUT').toBeTruthy()
    const body = JSON.parse(String(put?.[1]?.body))
    expect(body.fixed_dir).toBe('/srv/afleet/metrics')
    expect(body.cleanup_policy).toBe('balanced')
  })

  it('写成功之后跳看板', async () => {
    setup()
    const w = mountAt('p_1')
    await flush(w)

    await w.findAll('input[name="af-bind-device"]')[0]?.setValue(true)
    await w.find('input[type="text"]').setValue('/srv/afleet/metrics')
    await flush(w)

    await w.findAll('button').find((b) => b.text().includes('完成绑定'))?.trigger('click')
    await flush(w)

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_1')
  })
})

describe('BindPage：三种失败分开说', () => {
  it('读失败：整页错误态，并给回总览的路', async () => {
    stubFetch({
      '/api/projects/p_1': { status: 500, body: { error: 'internal' } },
      '/api/devices': { body: [] },
      '/api/projects/p_1/bindings': { body: [] },
    })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('打不开这个项目')
    expect(text).toContain('回总览')
  })

  it('写失败：提示与列表**并存**，列表不被错误态替掉', async () => {
    const fetchMock = setup({ bindings: [binding({ device_id: 'n_1' })] })
    // 让 PUT 返回 409。
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = new URL(String(input), 'http://hub.local').pathname
      if (init?.method === 'PUT') {
        return new Response(JSON.stringify({ error: 'fixed_dir_locked', detail: { executions: 3 } }), {
          status: 409,
          headers: { 'Content-Type': 'application/json' },
        })
      }
      const body =
        path === '/api/devices'
          ? [device({ id: 'n_1', name: 'device-c' })]
          : path === '/api/projects/p_1'
            ? project()
            : [binding({ device_id: 'n_1' })]
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    })

    const w = mountAt('p_1')
    await flush(w)

    await w.find('input[type="text"]').setValue('/srv/other')
    await flush(w)
    await w.findAll('button').find((b) => b.text().includes('完成绑定'))?.trigger('click')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('固定目录')
    expect(text).toContain('改不了固定目录')
    // 关键：失败之后页面数据还在，用户能看着现状再决定怎么办。
    expect(text).toContain('/srv/afleet/metrics')
    expect(text).not.toContain('打不开这个项目')
  })

  it('解除绑定失败也说清是哪一步失败', async () => {
    const fetchMock = setup({ bindings: [binding({ device_id: 'n_1' })] })
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = new URL(String(input), 'http://hub.local').pathname
      if (init?.method === 'DELETE') {
        return new Response(JSON.stringify({ error: 'not_found' }), {
          status: 404,
          headers: { 'Content-Type': 'application/json' },
        })
      }
      const body =
        path === '/api/devices'
          ? [device({ id: 'n_1', name: 'device-c' })]
          : path === '/api/projects/p_1'
            ? project()
            : [binding({ device_id: 'n_1' })]
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    })

    const w = mountAt('p_1')
    await flush(w)

    const unbind = w.findAll('button').find((b) => b.text() === '解除绑定')
    expect(unbind, '已有绑定时要有解除按钮').toBeTruthy()
    await unbind?.trigger('click')
    await flush(w)

    expect(w.element.textContent).toContain('解除绑定失败（not_found）')
  })
})

describe('BindPage：用量显示', () => {
  it('设备没上报用量时不显示 0 B', async () => {
    setup({ bindings: [binding({ used_bytes: null })] })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('用量还没拿到')
    // `0 B` 会让用户以为它一点不占，而真相是 Hub 根本没拿到数。
    expect(text).not.toContain('已用 0 B')
  })

  it('有上报时显示真实用量与配额占比', async () => {
    setup({
      bindings: [
        binding({ used_bytes: 5 * 1024 * 1024 * 1024, disk_quota_bytes: 20 * 1024 * 1024 * 1024 }),
      ],
    })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('5.0 GB')
    expect(text).toContain('25%')
  })
})
