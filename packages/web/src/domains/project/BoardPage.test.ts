/* ===========================================================================
   BoardPage 的测试。
   ---------------------------------------------------------------------------
   这一页有三条**不跑就会静默坏掉**的分支：地址里没有项目 id、两个请求里
   任何一个失败、换了项目之后整页重载。加上本里程碑新修的切换器
   （`.af-switch`，项目名本身就是按钮）。

   切换器的两条判据刻意分开断：
   · 菜单能开能关     —— 只做「显示当前项目名」是最容易骗过审查的假实现；
   · 切换后菜单关掉   —— 不关的话换了项目菜单还盖在新内容上，用户会以为
                         「点了没反应」。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import BoardPage from '@/domains/project/BoardPage.vue'
import type { Project } from '@/domains/project/api'
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

/**
 * 把地址停在某个项目的看板，再挂组件。
 *
 * `route` 是模块级单例，上一个用例跳走之后它还留在那个地址上，所以每条用例
 * 都必须自己把起点钉死。
 */
function mountAt(id: string): ReturnType<typeof mountIn<typeof BoardPage>> {
  navigate(`/project/${encodeURIComponent(id)}/board`, { replace: true })
  return mountIn(BoardPage)
}

describe('BoardPage：地址里没有项目 id', () => {
  it('直接说缺 id，且**一个请求都不发**', async () => {
    // 裸 `/project/` 不是一条路由，所以拿 `/` 来造「params.id 为空」这一态：
    // 这里要守的是页面自己的守卫，不是路由表。
    navigate('/', { replace: true })
    const fetchMock = stubFetch({})
    const w = mountIn(BoardPage)
    await flush(w)

    expect(w.element.textContent).toContain('这个地址缺少项目 id')
    // 空 id 发出去会打到 `/api/projects/`，后端会当成列表端点或 404，
    // 两种都不是用户想看的东西。
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

describe('BoardPage：两个请求都成功', () => {
  it('六列都在，且列名与顺序照六态', async () => {
    stubFetch({
      '/api/projects/p_1': { body: project() },
      '/api/projects': { body: [project()] },
    })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    for (const title of ['待办', '进行中', '待验收', '失败', '完成', '已取消']) {
      expect(text, `缺列「${title}」`).toContain(title)
    }
    // 顺序也是判据：「失败」必须排在「完成」之前，否则需要处置的一列会被
    // 滚出视野。这里断 DOM 里的实际次序，不断文字是否出现。
    const titles = Array.from(w.element.querySelectorAll('.af-board__coltitle')).map(
      (e) => e.textContent ?? '',
    )
    expect(titles.indexOf('失败')).toBeLessThan(titles.indexOf('完成'))
    expect(titles).toHaveLength(6)
  })
})

describe('BoardPage：请求失败', () => {
  it('任一请求失败都整页进失败态，不留半份数据', async () => {
    // 当前项目拿到了，列表挂了 —— `Promise.all` 会整体 reject。
    stubFetch({
      '/api/projects/p_1': { body: project() },
      '/api/projects': { status: 500, body: { error: 'internal' } },
    })
    const w = mountAt('p_1')
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('打不开这个项目')
    expect(text).toContain('读项目失败')
    // 失败态里不该还挂着六列：那会让人以为板子是空的而不是没读到。
    expect(w.element.querySelector('.af-board')).toBeNull()
  })

  it('没有项目 id 之外，读取失败也给一条回总览的路', async () => {
    stubFetch({
      '/api/projects/p_1': { status: 404, body: { error: 'not_found' } },
      '/api/projects': { body: [] },
    })
    const w = mountAt('p_1')
    await flush(w)

    expect(w.element.textContent).toContain('回总览')
  })
})

describe('BoardPage：换项目重载', () => {
  it('地址里的项目变了就重读，不沿用上一份数据', async () => {
    const fetchMock = stubFetch({
      '/api/projects/p_1': { body: project({ id: 'p_1', name: '内部指标台' }) },
      '/api/projects/p_2': { body: project({ id: 'p_2', name: '客户现场' }) },
      '/api/projects': {
        body: [project({ id: 'p_1', name: '内部指标台' }), project({ id: 'p_2', name: '客户现场' })],
      },
    })
    const w = mountAt('p_1')
    await flush(w)
    expect(w.element.textContent).toContain('内部指标台')

    // 走真实的路由跳转，触发 `watch(projectId, load)`。
    navigate('/project/p_2/board', { replace: true })
    await flush(w)

    expect(w.element.textContent).toContain('客户现场')
    // 反向验证：把 `watch(projectId, load)` 删掉，这条会红在「还显示旧项目名」。
    const detailCalls = fetchMock.mock.calls.filter((c) =>
      String(c[0]).includes('/api/projects/p_2'),
    )
    expect(detailCalls.length).toBeGreaterThan(0)
  })
})

describe('BoardPage：项目切换器', () => {
  /** 切到 p_1，并让列表里有第二个项目可选。 */
  function setup() {
    stubFetch({
      '/api/projects/p_1': { body: project({ id: 'p_1', name: '内部指标台' }) },
      '/api/projects/p_2': { body: project({ id: 'p_2', name: '客户现场', shape: 'isolated' }) },
      '/api/projects': {
        body: [
          project({ id: 'p_1', name: '内部指标台', shape: 'central' }),
          project({ id: 'p_2', name: '客户现场', shape: 'isolated' }),
        ],
      },
    })
    return mountAt('p_1')
  }

  it('项目名本身就是按钮，点开才出现菜单', async () => {
    const w = setup()
    await flush(w)

    const btn = w.find('.af-switch__btn')
    expect(btn.exists()).toBe(true)
    expect(btn.text()).toContain('内部指标台')
    expect(btn.attributes('aria-expanded')).toBe('false')
    // 收起时不该有菜单——它不是一直挂着只是看不见。
    expect(w.find('.af-switch__menu').exists()).toBe(false)

    await btn.trigger('click')
    expect(btn.attributes('aria-expanded')).toBe('true')
    expect(w.find('.af-switch__menu').exists()).toBe(true)
  })

  it('再点一次关掉菜单', async () => {
    const w = setup()
    await flush(w)

    const btn = w.find('.af-switch__btn')
    await btn.trigger('click')
    await btn.trigger('click')

    expect(btn.attributes('aria-expanded')).toBe('false')
    expect(w.find('.af-switch__menu').exists()).toBe(false)
  })

  it('菜单里每个项目带形态说明，当前项目标「当前」', async () => {
    const w = setup()
    await flush(w)
    await w.find('.af-switch__btn').trigger('click')

    const items = w.findAll('.af-switch__item')
    // 两个项目 + 「全部项目」
    expect(items.length).toBe(3)

    const text = w.element.textContent ?? ''
    expect(text).toContain('中心式')
    expect(text).toContain('隔离式')

    const current = w.findAll('.af-switch__item[aria-current="true"]')
    expect(current).toHaveLength(1)
    expect(current[0]?.text()).toContain('内部指标台')
  })

  it('选另一个项目就跳过去，并把菜单关掉', async () => {
    const w = setup()
    await flush(w)
    await w.find('.af-switch__btn').trigger('click')

    const target = w.findAll('.af-switch__item').find((i) => i.text().includes('客户现场'))
    expect(target, '菜单里要有「客户现场」').toBeTruthy()
    await target?.trigger('click')
    await flush(w)

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_2')
    // 菜单必须关：不关的话它盖在刚换过来的板子上，用户会以为「点了没反应」。
    // 反向验证：把 `switchTo` 开头的 `menuOpen.value = false` 删掉，这条会红。
    expect(w.find('.af-switch__menu').exists()).toBe(false)
  })

  it('点当前项目不跳转，但菜单照样关掉', async () => {
    const w = setup()
    await flush(w)
    await w.find('.af-switch__btn').trigger('click')

    const before = history.length
    const self = w.findAll('.af-switch__item').find((i) => i.text().includes('内部指标台'))
    await self?.trigger('click')
    await flush(w)

    expect(history.length).toBe(before)
    expect(route.value.params.id).toBe('p_1')
    // 「选中自己」也是一个点击，菜单该收起来——否则看起来像没响应。
    expect(w.find('.af-switch__menu').exists()).toBe(false)
  })

  it('项目 id 里带 `/` 时路径仍能匹配到看板', async () => {
    // 不编码的话 `matchPattern` 会把 id 切成两段，落进 `not-found`。
    stubFetch({
      '/api/projects': {
        body: [project({ id: 'p_1' }), project({ id: 'a/b', name: '带斜杠的项目' })],
      },
      '/api/projects/p_1': { body: project({ id: 'p_1' }) },
      '/api/projects/a%2Fb': { body: project({ id: 'a/b', name: '带斜杠的项目' }) },
    })
    const w = mountAt('p_1')
    await flush(w)
    await w.find('.af-switch__btn').trigger('click')

    const target = w.findAll('.af-switch__item').find((i) => i.text().includes('带斜杠的项目'))
    await target?.trigger('click')
    await flush(w)

    // 判据是**落在看板上并且 id 还原正确**，不是「地址字符串长得对」。
    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('a/b')
  })
})
