/* ===========================================================================
   ArchivedPage 的测试。
   ---------------------------------------------------------------------------
   这一页的存在本身就是产品承诺（`PRODUCT.md` §3.2：第一版不提供项目删除，
   归档**不是删除**）。所以最要紧的一条判据是**取消归档之后列表真的回流**：
   只发请求不重读的话，项目已经回去了而页面上还挂着它，用户会以为没生效。

   另两条容易做错的：只列已归档的（`archived_at !== null`，而不是把整份列表
   渲染出来），以及没有归档项时说的是「还没有归档」而不是「暂无数据」。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import ArchivedPage from '@/domains/project/ArchivedPage.vue'
import type { Project } from '@/domains/project/api'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate } from '@/router'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
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

function mountPage(): ReturnType<typeof mountIn<typeof ArchivedPage>> {
  navigate('/projects/archived', { replace: true })
  return mountIn(ArchivedPage)
}

/** 归档页要的是 `?include_archived=1`；`stubFetch` 按 pathname 分派，查询串不进 key。 */
const LIST = '/api/projects'

describe('ArchivedPage：只列已归档的', () => {
  it('活着的不出现在这一页', async () => {
    stubFetch({
      [LIST]: {
        body: [
          project({ id: 'p_1', name: '还在做的', archived_at: null }),
          project({ id: 'p_2', name: '做完的', archived_at: 'unix:1700000000' }),
        ],
      },
    })
    const w = mountPage()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('做完的')
    // 反向验证：把 `archived` 的 filter 去掉，这条会红。
    expect(text).not.toContain('还在做的')
  })

  it('请求带 include_archived=1，否则归档项根本读不到', async () => {
    const fetchMock = stubFetch({ [LIST]: { body: [] } })
    const w = mountPage()
    await flush(w)

    expect(String(fetchMock.mock.calls[0]?.[0])).toContain('include_archived=1')
  })

  it('归档时间显示成人话', async () => {
    stubFetch({
      [LIST]: { body: [project({ id: 'p_2', name: '做完的', archived_at: 'unix:1700000000' })] },
    })
    const w = mountPage()
    await flush(w)

    expect(w.element.textContent).toContain('归档')
  })
})

describe('ArchivedPage：没有归档项', () => {
  it('空态说「还没有归档的项目」，并解释归档之后会出现在这里', async () => {
    stubFetch({ [LIST]: { body: [] } })
    const w = mountPage()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有归档的项目')
    // 用户需要知道「怎么让它到这里来」，只说「没有」不够。
    expect(text).toContain('归档')
    expect(text).not.toContain('暂无数据')
  })

  it('页头常驻一句「归档不删除任何东西」', async () => {
    stubFetch({ [LIST]: { body: [] } })
    const w = mountPage()
    await flush(w)

    // 这句话是产品承诺，不该只在空态里出现。
    expect(w.element.textContent).toContain('归档不删除任何东西')
  })
})

describe('ArchivedPage：取消归档', () => {
  it('点一下真的回流：项目从列表里消失', async () => {
    let archived = true
    const fetchMock = stubFetch({
      [LIST]: () => ({
        body: archived
          ? [project({ id: 'p_2', name: '做完的', archived_at: 'unix:1700000000' })]
          : [],
      }),
      '/api/projects/p_2/unarchive': () => {
        archived = false
        return { body: project({ id: 'p_2', name: '做完的', archived_at: null }) }
      },
    })
    const w = mountPage()
    await flush(w)
    expect(w.element.textContent).toContain('做完的')

    const btn = w.findAll('button').find((b) => b.text() === '取消归档')
    expect(btn, '每行要有「取消归档」按钮').toBeTruthy()
    await btn?.trigger('click')
    await flush(w)

    // 判据是**回流**：只发请求不重读的话，这里还会挂着「做完的」。
    // 反向验证：把 `restore()` 里的 `await load()` 删掉，这条会红。
    expect(w.element.textContent).not.toContain('做完的')
    expect(w.element.textContent).toContain('还没有归档的项目')

    // 确实打了 unarchive 端点，而不是只重读了一遍列表。
    const calls = fetchMock.mock.calls.map((c) => String(c[0]))
    expect(calls.some((u) => u.includes('/api/projects/p_2/unarchive'))).toBe(true)
  })

  it('失败时带出错误码，且不把项目从列表里拿掉', async () => {
    stubFetch({
      [LIST]: {
        body: [project({ id: 'p_2', name: '做完的', archived_at: 'unix:1700000000' })],
      },
      '/api/projects/p_2/unarchive': { status: 409, body: { error: 'project_busy' } },
    })
    const w = mountPage()
    await flush(w)

    await w.findAll('button').find((b) => b.text() === '取消归档')?.trigger('click')
    await flush(w)

    expect(w.element.textContent).toContain('取消归档失败（project_busy）')
    // 失败就是没恢复：项目必须**还在列表里**，否则用户会以为成功了。
    //
    // 这条一开始是红的，而且红得对：错误提示和整页错误态合用一个 ref，于是
    // 「取消归档失败」把整份列表替成了「读取失败」——用户看不到那一行，也就
    // 没法再试一次。修法是把动作失败拆成独立的 `actionFailure`，让它与列表
    // 并存，而不是二选一。
    expect(w.element.textContent).toContain('做完的')
    expect(w.element.querySelectorAll('.af-projectlist__row')).toHaveLength(1)
    // 也不该把它报成「读取失败」——读是成功的，失败的是一次写动作。
    expect(w.element.textContent).not.toContain('读取失败')
  })
})

describe('ArchivedPage：读失败', () => {
  it('说「读取失败」并给重试，不显示成「还没有归档」', async () => {
    stubFetch({ [LIST]: { status: 500, body: { error: 'internal' } } })
    const w = mountPage()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('读取失败')
    expect(text).toContain('重试')
    // 与 OverviewPage 同一条判据：读失败不能伪装成「你没有」。
    expect(text).not.toContain('还没有归档的项目')
  })
})
