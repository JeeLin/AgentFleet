/* ===========================================================================
   OverviewPage 的测试。
   ---------------------------------------------------------------------------
   这一页是**全站入口**（`/`）。它的三种状态必须分开，因为它们对用户的含义
   完全不同：
     · 加载中   —— 等一下就好
     · 读失败   —— 出错了，要重试
     · 真的没有 —— 下一步是去建一个

   把「读失败」写成「还没有项目」是这一页最容易犯的错：用户会以为自己的项目
   全没了。所以这里**逐条断状态文案互斥**，而不是只看「页面上有没有字」。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import OverviewPage from '@/domains/project/OverviewPage.vue'
import type { Project } from '@/domains/project/api'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate, route } from '@/router'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

function mountAtRoot(): ReturnType<typeof mountIn<typeof OverviewPage>> {
  navigate('/', { replace: true })
  return mountIn(OverviewPage)
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

describe('OverviewPage：真的没有项目', () => {
  it('空态说「还没有项目」并给出建项目那条路', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有项目')
    // 光说「没有」不够：新建状态下第一件事就是建一个，必须直接给路。
    expect(text).toContain('项目是这里的起点')
    // 「暂无数据」是加载失败的措辞，不能出现在空态里。
    expect(text).not.toContain('暂无数据')
    // 空态里的重试按钮不该有——没有东西可重试。
    expect(text).not.toContain('重试')
  })

  it('空态和页头各有一个「新建项目」，且空态那个是主按钮', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const news = w.findAll('button').filter((b) => b.text() === '新建项目')
    expect(news.length).toBe(2)
    expect(news.some((b) => b.classes('af-btn--primary'))).toBe(true)
  })
})

describe('OverviewPage：有项目', () => {
  it('每个项目给名字、形态中文名与一句「代码从哪来」', async () => {
    stubFetch({
      '/api/projects': {
        body: [
          project({ id: 'p_1', name: '内部指标台', shape: 'central' }),
          project({ id: 'p_2', name: '客户现场', shape: 'isolated' }),
        ],
      },
    })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('内部指标台')
    expect(text).toContain('客户现场')
    expect(text).toContain('中心式')
    expect(text).toContain('隔离式')
    // 形态只给名字不够：三个名字对第一次见到的人没有区别，必须带一句
    // 「代码从哪来、到哪去」。
    expect(text).toContain('代码在 Hub 的仓库里')
    expect(text).toContain('代码只在设备上，永不上传')
  })

  it('点项目进它的看板', async () => {
    stubFetch({ '/api/projects': { body: [project({ id: 'p_7', name: '内部指标台' })] } })
    const w = mountAtRoot()
    await flush(w)

    const link = w.findAll('.af-projectlist__link')[0]
    expect(link, '列表里要有一个可点的项目').toBeTruthy()
    await link?.trigger('click')

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_7')
  })

  it('项目 id 里带 `/` 时路径仍能匹配到看板', async () => {
    stubFetch({
      '/api/projects': { body: [project({ id: 'a/b', name: '带斜杠的项目' })] },
      '/api/projects/a%2Fb': { body: project({ id: 'a/b' }) },
    })
    const w = mountAtRoot()
    await flush(w)

    await w.findAll('.af-projectlist__link')[0]?.trigger('click')
    await flush(w)

    // 反向验证：把 `OverviewPage.vue` 里那处的 `encodeURIComponent` 删掉，
    // 这条会红在 `not-found` 上。
    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('a/b')
  })

  it('没设主题色时不画色点（不拿 null 当 gray）', async () => {
    stubFetch({ '/api/projects': { body: [project({ theme_color: null })] } })
    const w = mountAtRoot()
    await flush(w)

    expect(w.element.querySelector('.af-projectlist__dot')).toBeNull()
  })
})

describe('OverviewPage：读失败', () => {
  it('说「读取项目失败」，与「还没有项目」互斥', async () => {
    stubFetch({ '/api/projects': { status: 500, body: { error: 'internal' } } })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('读取项目失败')
    // 这条是整页的核心判据：读失败绝不能显示成「你没有项目」。
    expect(text).not.toContain('还没有项目')
    expect(text).toContain('重试')
  })

  it('重试会重新请求，成功后显示列表', async () => {
    let first = true
    const fetchMock = stubFetch({
      '/api/projects': () => {
        if (first) {
          first = false
          return { status: 500, body: { error: 'internal' } }
        }
        return { body: [project({ id: 'p_1', name: '内部指标台' })] }
      },
    })
    const w = mountAtRoot()
    await flush(w)
    expect(w.element.textContent).toContain('读取项目失败')

    const retry = w.findAll('button').find((b) => b.text() === '重试')
    await retry?.trigger('click')
    await flush(w)

    expect(w.element.textContent).toContain('内部指标台')
    expect(w.element.textContent).not.toContain('读取项目失败')
    expect(fetchMock.mock.calls.length).toBeGreaterThanOrEqual(2)
  })
})

describe('OverviewPage：待办那块的归属', () => {
  it('待办摘要写成空态并说明归 v0.8.0，不留白', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    // 留白会被当成加载失败，所以必须显式说明。
    expect(text).toContain('待办摘要要等任务模型落地')
    expect(text).toContain('v0.8.0')
  })
})
