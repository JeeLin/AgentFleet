/* ===========================================================================
   ProjectSettingsPage 的测试。
   ---------------------------------------------------------------------------
   这一页与 `BindPage` 不同：它**会改变项目的状态**（升形态、归档、改色），
   而其中三项里有两项是不可逆或半不可逆的。要守的四件事：

   1. **四个区块都在**，且「配额不在项目上」这个真实约束被说出来——不是漏做。
   2. **未接后端的东西必须是灰的且说明原因**：一个点了必然报错的按钮
      比没有按钮更糟。这是本页最容易假实现的地方。
   3. **只有 `isolated` 才显示「升级为中心式」**：`central` 是终点、`mirror`
      不在允许表里。反向验证过——三项断言各自对应一种形态。
   4. **写失败不能把页面变成「打不开这个项目」**：内容与按钮必须留在页面上
      （照 `ArchivedPage` 那个真实缺陷）。

   还守一条纯净性：**页面不能自己拼 URL**，一律经 `api.ts`。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import ProjectSettingsPage from '@/domains/project/ProjectSettingsPage.vue'
import type { Project } from '@/domains/project/api'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate } from '@/router'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 12; i++) {
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

function mountAt(id = 'p_1'): ReturnType<typeof mountIn<typeof ProjectSettingsPage>> {
  navigate(`/project/${encodeURIComponent(id)}/settings`, { replace: true })
  return mountIn(ProjectSettingsPage)
}

/** 只声明读端点；写端点由各用例按需追加（漏声明会拿到 500，测试立刻红）。 */
function setup(over: Partial<Project> = {}) {
  return stubFetch({
    '/api/projects/p_1': { body: project(over) },
  })
}

/** 找到某个 section 元素（按 aria-label）。 */
function section(
  w: ReturnType<typeof mountIn<typeof ProjectSettingsPage>>,
  label: string,
): Element | null {
  return w.element.querySelector(`section[aria-label="${label}"]`)
}

/** 页面上所有「点了会真的发请求」的按钮的文字。 */
function buttonLabels(w: ReturnType<typeof mountIn<typeof ProjectSettingsPage>>): string[] {
  return Array.from(w.element.querySelectorAll('button')).map((b) => (b.textContent ?? '').trim())
}

describe('ProjectSettingsPage：四个区块都在', () => {
  it('合并权 / 隔离策略 / 配额与清理 / 主题色 / 归档 一个不少', async () => {
    setup()
    const w = mountAt()
    await flush(w)

    for (const label of ['这个项目的合并权', '隔离策略', '磁盘配额与清理', '项目主题色', '归档这个项目']) {
      expect(section(w, label), `缺区块「${label}」`).not.toBeNull()
    }
  })

  it('三档清理策略写清了差别，不是只给名字', async () => {
    setup()
    const w = mountAt()
    await flush(w)
    const text = w.element.textContent ?? ''
    expect(text).toContain('留 30 天')
    expect(text).toContain('留 7 天')
    expect(text).toContain('尽快清')
  })

  it('说清「没有删除」——归档不是删除', async () => {
    setup()
    const w = mountAt()
    await flush(w)
    const text = w.element.textContent ?? ''
    expect(text).toContain('没有「删除」')
    expect(text).toContain('归档不是删除')
  })
})

describe('ProjectSettingsPage：未接后端的区块必须如实说明且不可点', () => {
  it('合并权三档的按钮全部 disabled，并说明后端还没接', async () => {
    setup()
    const w = mountAt()
    await flush(w)

    const region = section(w, '这个项目的合并权')
    expect(region).not.toBeNull()

    // 反向验证：把 disabled 去掉，这条会红。
    const buttons = Array.from(region?.querySelectorAll('button') ?? [])
    expect(buttons.length).toBeGreaterThanOrEqual(3)
    for (const b of buttons) {
      expect(b.disabled, `「${b.textContent?.trim()}」不该是可点的：后端没有这个字段`).toBe(true)
    }

    // 光灰掉不够——用户要知道为什么。只灰不说等于让人以为是自己权限不够。
    expect(w.element.textContent).toContain('后端还没接')
  })

  it('配额与清理说明了它挂在绑定上，并给出通往绑定页的入口', async () => {
    setup()
    const w = mountAt()
    await flush(w)

    const text = w.element.textContent ?? ''
    // 这是数据模型的真实形状：binding.disk_quota_bytes / binding.cleanup_policy。
    expect(text).toContain('绑定')
    expect(text).toContain('不是项目上')

    // 入口必须存在且可点——只说「去别处改」而没链接等于没做。
    const toBind = Array.from(w.element.querySelectorAll('button')).filter(
      (b) => (b.textContent ?? '').trim() === '绑定与配额',
    )
    expect(toBind.length).toBeGreaterThanOrEqual(1)
  })
})

describe('ProjectSettingsPage：形态决定能不能升级', () => {
  it('隔离式显示「升级为中心式」', async () => {
    setup({ shape: 'isolated' })
    const w = mountAt()
    await flush(w)

    expect(buttonLabels(w)).toContain('升级为中心式')
    expect(w.element.textContent).toContain('这一步不可撤销')
  })

  it('中心式不显示升级按钮（已是终点），并说明没有下一档', async () => {
    setup({ shape: 'central' })
    const w = mountAt()
    await flush(w)

    expect(buttonLabels(w)).not.toContain('升级为中心式')
    expect(w.element.textContent).toContain('已经是中心式')
  })

  it('镜像式不显示升级按钮，并说明为什么不支持变更', async () => {
    setup({ shape: 'mirror' })
    const w = mountAt()
    await flush(w)

    expect(buttonLabels(w)).not.toContain('升级为中心式')
    expect(w.element.textContent).toContain('镜像式不支持变更形态')
  })

  it('未知形态如实显示原文，不渲染成空白', async () => {
    // 后端 shape 是开放集合（api.ts 顶部那段）。后端将来加第四种形态时，
    // 页面必须显示得出来，而不是一片空白。
    setup({ shape: 'worktree-farm' })
    const w = mountAt()
    await flush(w)

    expect(w.element.textContent).toContain('worktree-farm')
  })
})

describe('ProjectSettingsPage：写操作', () => {
  it('升级形态 POST 到 /shape，然后重读项目', async () => {
    const fetchMock = stubFetch({
      '/api/projects/p_1': { body: project({ shape: 'isolated' }) },
      '/api/projects/p_1/shape': { body: project({ shape: 'central' }) },
    })
    const w = mountAt()
    await flush(w)

    const btn = Array.from(w.element.querySelectorAll('button')).find(
      (b) => (b.textContent ?? '').trim() === '升级为中心式',
    )
    ;(btn as HTMLButtonElement).click()
    await flush(w)

    const called = fetchMock.mock.calls.map((c) => String(c[0]))
    expect(called.some((u) => u.includes('/api/projects/p_1/shape'))).toBe(true)
    // 重读：形态变更会连带建仓，前端自己拼的「应该是新的」会和库对不上。
    expect(called.filter((u) => u.endsWith('/api/projects/p_1')).length).toBeGreaterThanOrEqual(2)
    expect(w.element.textContent).toContain('已升级为中心式')
  })

  it('形态被拒时把「方向不被允许」说出来，而不是只回一个错误码', async () => {
    // 后端真会拒这条（`project_shape.rs` 断言 400 + `shape_transition_refused`
    // + `detail{from,to}`）。这里模拟的是「两个标签页都开着、另一个先升了级」
    // 这种真实竞态：本页仍以为自己是 isolated，点下去被拒。
    //
    // **这条用例是补上来的**：反向验证时删掉 `shape_transition_refused` 分支，
    // 18 条测试全绿——说明那时没有任何断言在看这个分支。只回「升级失败
    // （shape_transition_refused）」等于把码抛给用户，他不知道是方向反了
    // 还是重复提交，会一直重试。
    stubFetch({
      '/api/projects/p_1': { body: project({ shape: 'isolated' }) },
      '/api/projects/p_1/shape': {
        status: 400,
        body: {
          error: 'shape_transition_refused',
          detail: { from: 'central', to: 'central' },
        },
      },
    })
    const w = mountAt()
    await flush(w)

    const btn = Array.from(w.element.querySelectorAll('button')).find(
      (b) => (b.textContent ?? '').trim() === '升级为中心式',
    )
    ;(btn as HTMLButtonElement).click()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('升级失败')
    expect(text).toContain('单向')
    // 不能把裸错误码甩给用户。
    expect(text).not.toContain('shape_transition_refused')
  })

  it('归档失败时页面内容与按钮都还在（不变成「打不开这个项目」）', async () => {
    stubFetch({
      '/api/projects/p_1': { body: project() },
      '/api/projects/p_1/archive': { status: 500, body: { error: 'disk_full' } },
    })
    const w = mountAt()
    await flush(w)

    const btn = Array.from(w.element.querySelectorAll('button')).find(
      (b) => (b.textContent ?? '').trim() === '归档这个项目',
    )
    ;(btn as HTMLButtonElement).click()
    await flush(w)

    const text = w.element.textContent ?? ''
    // 失败要说清是什么失败。
    expect(text).toContain('归档失败')
    // **反向验证**：把 actionFailure 混进 failure，这两条会红——
    // 那正是 ArchivedPage 当年真实发生的缺陷。
    expect(text).not.toContain('打不开这个项目')
    expect(section(w, '归档这个项目')).not.toBeNull()
  })

  it('已归档时按钮变成「取消归档」，且提示定时任务不会自动恢复', async () => {
    stubFetch({
      '/api/projects/p_1': { body: project({ archived_at: 'unix:1700000001' }) },
      '/api/projects/p_1/unarchive': { body: project({ archived_at: null }) },
    })
    const w = mountAt()
    await flush(w)

    expect(buttonLabels(w)).toContain('取消归档')
    expect(buttonLabels(w)).not.toContain('归档这个项目')

    const btn = Array.from(w.element.querySelectorAll('button')).find(
      (b) => (b.textContent ?? '').trim() === '取消归档',
    )
    ;(btn as HTMLButtonElement).click()
    await flush(w)

    expect(w.element.textContent).toContain('定时任务')
    expect(w.element.textContent).toContain('不会自动恢复')
  })

  it('改主题色 PATCH 项目，传的是具体颜色', async () => {
    const fetchMock = stubFetch({
      '/api/projects/p_1': { body: project() },
      // PATCH 也打到同一个 path；桩按 pathname 分派，所以这里就是它。
    })
    const w = mountAt()
    await flush(w)

    const btn = Array.from(w.element.querySelectorAll('button')).find(
      (b) => (b.textContent ?? '').trim() === '绿',
    )
    ;(btn as HTMLButtonElement).click()
    await flush(w)

    // **不能取第一条**：同一个 path 上先有一次 GET（load），再是 PATCH。
    // 按 method 挑，否则这条断言会在「只有 GET」时也拿到一个 call 而意义全无。
    const patches = fetchMock.mock.calls.filter((c) => {
      const init = (c[1] ?? {}) as RequestInit
      return String(c[0]).endsWith('/api/projects/p_1') && String(init.method).toUpperCase() === 'PATCH'
    })
    expect(patches.length, '没有发出 PATCH').toBe(1)
    expect(String((patches[0][1] as RequestInit).body)).toContain('green')
  })

  it('没设颜色时不显示「清除」——清除一个不存在的东西不是操作', async () => {
    setup({ theme_color: null })
    const w = mountAt()
    await flush(w)
    expect(buttonLabels(w)).not.toContain('清除')
  })

  it('设了颜色时显示「清除」，点了传 null（不是空串）', async () => {
    const fetchMock = stubFetch({
      '/api/projects/p_1': { body: project({ theme_color: 'blue' }) },
    })
    const w = mountAt()
    await flush(w)

    expect(buttonLabels(w)).toContain('清除')
    const btn = Array.from(w.element.querySelectorAll('button')).find(
      (b) => (b.textContent ?? '').trim() === '清除',
    )
    ;(btn as HTMLButtonElement).click()
    await flush(w)

    // 同上：按 method 挑，不吃到开头那次 GET。
    const patches = fetchMock.mock.calls.filter((c) => {
      const init = (c[1] ?? {}) as RequestInit
      return String(c[0]).endsWith('/api/projects/p_1') && String(init.method).toUpperCase() === 'PATCH'
    })
    expect(patches.length, '没有发出 PATCH').toBe(1)
    // 「清除」与「设成空串」在库里是两件事，必须传 null。
    expect(String((patches[0][1] as RequestInit).body)).toContain('null')
  })
})

describe('ProjectSettingsPage：读失败与缺 id', () => {
  it('读不到项目时整页进错误态，并说明是哪个', async () => {
    stubFetch({
      '/api/projects/p_1': { status: 404, body: { error: 'not_found' } },
    })
    const w = mountAt()
    await flush(w)

    expect(w.element.textContent).toContain('打不开这个项目')
  })

  it('地址里没有 id 时直接说，且一个请求都不发', async () => {
    navigate('/', { replace: true })
    const fetchMock = stubFetch({})
    const w = mountIn(ProjectSettingsPage)
    await flush(w)

    expect(w.element.textContent).toContain('这个地址缺少项目 id')
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

describe('ProjectSettingsPage：页面不自己拼 URL', () => {
  it('源码里没有裸 /api/ 字符串', async () => {
    // 与 `api.ts` 顶部的分工一致：页面只调函数。绕过它意味着 CSRF / 401 /
    // 错误码三件事全都得在这一页重写一遍，而漏掉的那一件不会红。
    const { readRepoFile } = await import('@/test/css')
    const src = readRepoFile('packages/web/src/domains/project/ProjectSettingsPage.vue')
    expect(src).not.toContain("'/api/")
    expect(src).not.toContain('"/api/')
  })
})
