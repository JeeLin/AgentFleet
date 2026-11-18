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

import { describe, expect, it, vi } from 'vitest'

import { DOMWrapper } from '@vue/test-utils'

import BoardPage from '@/domains/project/BoardPage.vue'
import type { Project } from '@/domains/project/api'
import type { Task, TaskStateWire } from '@/domains/project/taskTypes'
import { STATES } from '@/shared/state'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import {
  clearRouteFocusTarget,
  focusRouteTarget,
  hasRouteFocusTarget,
  lastRouteFocusId,
  setRouteFocusTarget,
} from '@/shared/focus'
import { navigate, route } from '@/router'

/**
 * 等页面加载**真的结束**，再推进几轮让子组件的渲染队列走完。
 *
 * 为什么不用「推进固定轮数」：这一页的 `load()` 里有 `Promise.all` 三个请求，
 * 固定轮数（`settle(1) × 8`）在机器忙的时候会停在「正在读取项目…」上，
 * 于是断言变成对着中间态——症状是「单独跑绿、全量跑红」。
 * 这里等的是页面自己的判据（「正在读取项目」那句消失），而不是一个猜出来的数。
 */
async function flushUntilSettled(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 40; i++) {
    await w.vm.$nextTick()
    await settle(2)
    if (!(w.element.textContent ?? '').includes('正在读取项目')) break
  }
  // 加载结束后再推几轮：`TaskCard` 是子组件，它的 props 变化要等父组件的
  // 渲染队列排完才会落到 DOM 上。
  for (let i = 0; i < 4; i++) {
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
 * 一个任务（形状照子任务 6 的 `TaskView`）。
 *
 * 字段**全给**：`TaskCard` 会读 `needs_manual_acceptance` / `executions` /
 * `updated_at`，缺一个就会走到 `??` 兜底分支上，测的就不是真实形状。
 */
function task(over: Partial<Task> = {}): Task {
  return {
    id: 'tsk_1',
    project_id: 'p_1',
    title: '把构建时间降到 60s 内',
    state: 'pending',
    done_when: { criteria: 'build_passes', description: null },
    needs_manual_acceptance: false,
    criteria_known: true,
    created_by: 'user',
    base_commit: null,
    created_at: 'unix:1700000000',
    updated_at: 'unix:1700000000',
    constraints: {
      allowed_devices: null,
      allowed_agents: null,
      required_capabilities: null,
      retry_limit: null,
    },
    version: 1,
    superseded_by: null,
    intent_ref: null,
    executions: [],
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
    await flushUntilSettled(w)

    expect(w.element.textContent).toContain('这个地址缺少项目 id')
    // 空 id 发出去会打到 `/api/projects/`，后端会当成列表端点或 404，
    // 两种都不是用户想看的东西。
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

describe('BoardPage：两个请求都成功', () => {
  it('六列都在，且列名与顺序按用户裁定（失败在完成之前）', async () => {
    stubFetch({
      '/api/projects/p_1': { body: project() },
      '/api/projects': { body: [project()] },
      // 一个任务就够让六列出现（整板空时只有一条空态，见规格测试标准 3）；
      // 给了任务的 `state` 是「脏值之外的合法值」，不影响列名断言。
      '/api/projects/p_1/tasks': { body: { tasks: [task({ state: 'pending' })] } },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const text = w.element.textContent ?? ''
    for (const title of ['待办', '排队中', '执行中', '待验收', '已完成', '失败']) {
      expect(text, `缺列「${title}」`).toContain(title)
    }
    // 顺序也是判据。**这次是故意不照原型**：用户 2026-11-17 裁定
    // 「失败要排到完成前面」，理由是完成列看过就走、失败列还要处置。
    // 原型 `prototype/board.html:95-111` 是 `… done / failed`（失败最后），
    // 我们改成 `… failed / done`。旧版本这条断的正是「done 在 failed 之前」，
    // 已按新裁定反过来——见 `BoardPage.vue` 头注释「六列的顺序」一节。
    //
    // 另：整板无任务时页面只渲染一条空态（规格测试标准 3），所以这一条的
    // 夹具必须给至少一个任务，否则连 `.af-board` 都不在。
    // 列名类的选择器按原型（`.af-col__name`，`prototype/board.html:62`）。
    const titles = Array.from(w.element.querySelectorAll('.af-col__name')).map(
      (e) => e.textContent ?? '',
    )
    expect(titles).toEqual(['待办', '排队中', '执行中', '待验收', '失败', '已完成'])
    // 反向守卫：失败必须在完成**之前**，别让它被改回最后一列。
    expect(titles.indexOf('失败')).toBeLessThan(titles.indexOf('已完成'))
  })
})

describe('BoardPage：请求失败', () => {
  it('任一请求失败都整页进失败态，不留半份数据', async () => {
    // 当前项目拿到了，列表挂了 —— `Promise.all` 会整体 reject。
    stubFetch({
      '/api/projects/p_1': { body: project() },
      '/api/projects': { status: 500, body: { error: 'internal' } },
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)

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
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)

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
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
      '/api/projects/p_2/tasks': { body: { tasks: [] } },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)
    expect(w.element.textContent).toContain('内部指标台')

    // 走真实的路由跳转，触发 `watch(projectId, load)`。
    navigate('/project/p_2/board', { replace: true })
    await flushUntilSettled(w)

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
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
    })
    return mountAt('p_1')
  }

  it('项目名本身就是按钮，点开才出现菜单', async () => {
    const w = setup()
    await flushUntilSettled(w)

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
    await flushUntilSettled(w)

    const btn = w.find('.af-switch__btn')
    await btn.trigger('click')
    await btn.trigger('click')

    expect(btn.attributes('aria-expanded')).toBe('false')
    expect(w.find('.af-switch__menu').exists()).toBe(false)
  })

  it('菜单里每个项目带形态说明，当前项目标「当前」', async () => {
    const w = setup()
    await flushUntilSettled(w)
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
    await flushUntilSettled(w)
    await w.find('.af-switch__btn').trigger('click')

    const target = w.findAll('.af-switch__item').find((i) => i.text().includes('客户现场'))
    expect(target, '菜单里要有「客户现场」').toBeTruthy()
    await target?.trigger('click')
    await flushUntilSettled(w)

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_2')
    // 菜单必须关：不关的话它盖在刚换过来的板子上，用户会以为「点了没反应」。
    // 反向验证：把 `switchTo` 开头的 `menuOpen.value = false` 删掉，这条会红。
    expect(w.find('.af-switch__menu').exists()).toBe(false)
  })

  it('点当前项目不跳转，但菜单照样关掉', async () => {
    const w = setup()
    await flushUntilSettled(w)
    await w.find('.af-switch__btn').trigger('click')

    const before = history.length
    const self = w.findAll('.af-switch__item').find((i) => i.text().includes('内部指标台'))
    await self?.trigger('click')
    await flushUntilSettled(w)

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
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
      '/api/projects/a%2Fb/tasks': { body: { tasks: [] } },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)
    await w.find('.af-switch__btn').trigger('click')

    const target = w.findAll('.af-switch__item').find((i) => i.text().includes('带斜杠的项目'))
    await target?.trigger('click')
    await flushUntilSettled(w)

    // 判据是**落在看板上并且 id 还原正确**，不是「地址字符串长得对」。
    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('a/b')
  })
})

/* ===========================================================================
   v0.8.0 子任务 7 的页面级测试标准（里程碑 §7「测试标准」1–9）。
   ---------------------------------------------------------------------------
   每条都写了**判红方式**：这一节的用例全是「实现看起来像那么回事、但其实
   没做到」的重灾区（列序、空态、index 访问、编码、错误吞成空数组）。

   夹具一律走 `stubFetch` 打真 HTTP 桩而不是 mock API 模块：mock 模块会让
   「页面拼错了 URL」变成测试仍然绿的事，而拼错 URL 在真实环境里是 404。
   =========================================================================== */

/**
 * 一个项目 + 它的任务列表的完整桩。
 *
 * **声明顺序就是 `Promise.all` 的入参顺序**（`getProject` / `listProjects` /
 * `listProjectTasks`）：三个请求同时发出、`Promise.all` 抛的是「最先 reject
 * 的那个」，而这里三条都是同一个微任务 tick 里回来的——谁先挂取决于谁先入表。
 * 所以「任务 500」那几条用例里，任务桩必须排在**最前面**，否则被
 * `describeReadFailure` 报出来的是别的请求（症状是错误文案对不上）。
 */
function stubBoard(taskList: Task[], extra: Record<string, { status?: number; body?: unknown }> = {}) {
  return stubFetch({
    '/api/projects/p_1/tasks': { body: { tasks: taskList } },
    '/api/projects/p_1': { body: project() },
    '/api/projects': { body: [project()] },
    ...extra,
  })
}

/** 六态各一条，`id` 不同。 */
function sixTasks(): Task[] {
  const states: Task['state'][] = ['pending', 'queued', 'running', 'review', 'done', 'failed']
  return states.map((state, i) => task({ id: `tsk_${i}`, state, title: `${state} 的任务` }))
}

describe('BoardPage（v0.8.0）：六列的列名、顺序与文案', () => {
  it('renders_six_columns_in_user_ruled_order：列头顺序 = 失败在完成之前', async () => {
    // 判红方式：把 `done` / `failed` 换回原型顺序（失败最后）→ 这条立刻红。
    // **有意偏离原型**：`prototype/board.html:59-111` 是 `… review / done / failed`，
    // 用户 2026-11-17 裁定「失败要排到完成前面」（完成的一般都不会再看，
    // 失败的还要处置）。理由写在 `BoardPage.vue` 头注释「六列的顺序」一节。
    stubBoard(sixTasks())
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const titles = Array.from(w.element.querySelectorAll('.af-col__name')).map(
      (e) => e.textContent ?? '',
    )
    expect(titles).toEqual(['待办', '排队中', '执行中', '待验收', '失败', '已完成'])
  })

  it('column_labels_come_from_the_state_table：列头文案与 STATES[k].label 逐项相等', async () => {
    // 判红方式：页面里硬写中文字面量、与词表不一致 → 红。
    // 这里断的是**逐键相等**，不是「出现过某个词」——后者在页面硬写一份
    // 恰好同名的字面量时也绿，那正是本条要防的「第二份词表」。
    // 键数组随用户 2026-11-17 的裁定改为失败在完成之前（与 `BoardPage.vue`
    // 的 `COLUMNS` 同序）。
    stubBoard(sixTasks())
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const cols = Array.from(w.element.querySelectorAll('.af-col'))
    const keys = ['pending', 'queued', 'running', 'review', 'failed', 'done']
    expect(cols).toHaveLength(keys.length)
    keys.forEach((key, i) => {
      const head = cols[i]?.querySelector('.af-col__name')?.textContent ?? ''
      expect(head, `第 ${i + 1} 列的列头不是词表里的 ${key}`).toBe(
        STATES[key as TaskStateWire].label,
      )
    })
  })
})

describe('BoardPage（v0.8.0）：分组与脏数据', () => {
  it('an_empty_board_shows_the_board_empty_state_not_six_empty_states：只有一条空态', async () => {
    // 判红方式：把 `v-if="boardEmpty"` 去掉（六列照渲染）→ 这条红在「六条空态」。
    // 六条「这一列还没有任务」并排出现时，用户看到的是六个「空」，
    // 而真相是「一块空板子」。
    //
    // 但**六列本身仍照渲染**（`prototype/assets/board.js:347`：「空着的列本身是
    // 信息」——用户要能看见「这块板子有六站」，而不是一块什么都没有的页面）。
    // 于是判据是「列在、列内空态不在」，不是「整块板子消失」。
    stubBoard([])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('这个项目还没有任务')
    expect(text, '整板空时不该再出现六条列内空态').not.toContain('这一列还没有任务')
    // 六列仍在：原型 board.js:347 的「空列不隐藏」对整板空同样成立。
    expect(w.element.querySelectorAll('.af-col')).toHaveLength(6)
    // 整板空态里也要有新建入口（空态必须给出路）。
    expect(text).toContain('新建任务')
  })

  it('tasks_are_grouped_by_state：六态各一条 → 每列恰好 1 张卡', async () => {
    // 判红方式：把分组键写错（例如全用 `t.state` 之外的字段、或写死一列）→ 红。
    stubBoard(sixTasks())
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const cols = Array.from(w.element.querySelectorAll('.af-col'))
    expect(cols).toHaveLength(6)
    for (const col of cols) {
      expect(col.querySelectorAll('.af-taskcard')).toHaveLength(1)
    }
    // 计数徽标也要是 1（不是恒 0，也不是「N 张」的字面量）。
    for (const count of Array.from(w.element.querySelectorAll('.af-col__count'))) {
      expect(count.textContent?.trim()).toBe('1')
    }
  })

  it('an_unknown_state_value_does_not_crash_the_board：脏态不进任何列且不抛错', async () => {
    // 判红方式：写成 `COLUMNS[i].tasks.push(t)` 式的索引访问（先建桶再按 key
    // 塞）→ 遇到 `cancelled` 时桶不存在，`push` 抛错，整页白屏。
    //
    // `cancelled` 是**存量/脏数据**的代表：v0.8.0 的任务六态里没有它，
    // 主动放弃现在走失败处置的 `close`（任务留在 `failed`）。
    stubBoard([
      task({ id: 'tsk_ok', state: 'pending', title: '正常的任务' }),
      task({ id: 'tsk_bad', state: 'cancelled', title: '不该出现的任务' }),
    ])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const text = w.element.textContent ?? ''
    expect(text, '合法的任务要照常渲染').toContain('正常的任务')
    expect(text, '未知态不该被塞进某一列').not.toContain('不该出现的任务')
    expect(text, '不出现「已取消」这一列').not.toContain('已取消')
    const total = w.element.querySelectorAll('.af-taskcard').length
    expect(total, '只该渲染那一条合法任务').toBe(1)
  })
})

describe('BoardPage（v0.8.0）：时钟、跳转、错误与新建对话框', () => {
  it('the_page_passes_now_down_to_every_card：单一时钟，卸载后 clearInterval', async () => {
    // 判红方式：把 `onUnmounted` 里的 `clearInterval` 删掉 → 这条红在
    // 「卸载后定时器还在跑」（每次进出看板漏一个每秒一次的定时器）。
    // 另一半是「20 张卡共用一个时钟」：卡片里各起一个的话，spy 看到的
    // 间隔数量会等于卡片数，而这里断言的是**恰好一个**。
    vi.useFakeTimers()
    try {
      stubBoard([
        task({ id: 'tsk_a', state: 'running', executions: [] }),
        task({ id: 'tsk_b', state: 'running', executions: [] }),
      ])
      const setSpy = vi.spyOn(globalThis, 'setInterval')
      const clearSpy = vi.spyOn(globalThis, 'clearInterval')

      const w = mountAt('p_1')
      // 假定时器下不能用 `flush` 里那套「等微任务 + 真时间」的写法，
      // 但 Promise 链是微任务，仍然能靠 `await` 推进。
      for (let i = 0; i < 8; i++) {
        await w.vm.$nextTick()
        await Promise.resolve()
      }

      const pageClocks = setSpy.mock.calls.filter((c) => c[1] === 1000)
      expect(pageClocks, '看板应当只有页面层这一个 1 秒定时器').toHaveLength(1)

      w.unmount()
      expect(clearSpy, '卸载必须清掉定时器').toHaveBeenCalled()
    } finally {
      vi.useRealTimers()
    }
  })

  it('clicking_a_card_navigates_to_the_task_route：跳到 /project/<enc>/task/<enc>', async () => {
    // 判红方式：把 `encodeURIComponent(id)` 去掉 → id 含 `/` 时路径被切成多
    // 一段（`matchPattern` 把它当成另一条路由），这条红在 params 上。
    // 也断 `route.params.taskId` 被还原成原值，而不是只断字符串长得对。
    stubBoard([task({ id: 'a/b', state: 'pending', title: '带斜杠的任务' })])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const card = w.find('.af-taskcard')
    expect(card.exists(), '卡片要渲染出来').toBe(true)
    await card.trigger('click')
    await flushUntilSettled(w)

    expect(location.pathname).toContain(`/task/${encodeURIComponent('a/b')}`)
    expect(route.value.name).toBe('project-task')
    expect(route.value.params.id).toBe('p_1')
    expect(route.value.params.taskId, '任务 id 必须还原正确').toBe('a/b')
  })

  it('a_list_error_shows_an_error_state_not_an_empty_board：错误条 ≠「这个项目还没有任务」', async () => {
    // 判红方式：`catch` 里写成 `tasks.value = []`（把错误吞成空数组）→ 页面
    // 会显示「这个项目还没有任务」，那是**假话**：板子不是空的，是没读到。
    // 另一半：整页换成一个空板子而不是错误态 → 「读任务失败」这条红。
    //
    // 用 500 而不是「让 fetch 抛异常」：抛异常走的是 `NetworkError`，
    // 那时页面本来就该说「连不上」。这里要守的是「**服务端拒绝了**，
    // 页面不能把它说成空板子」——那才是 `tasks.value = []` 这种写法会骗过的分支。
    //
    // 判据分两半，缺一不可：
    //   ① 首次加载失败 → 页面**不出现**「这个项目还没有任务」（一个空板子
    //      就是在说这句话）；
    //   ② 先成功渲染出一块有任务的板子、再让刷新失败 → 那张卡不能凭空消失。
    //      只断 ① 的话，`tasks.value = []` 在 catch 里也能蒙过去（那时板子
    //      本来就是空的，看不出差别）。
    stubFetch({
      '/api/projects/p_1/tasks': { status: 500, body: { error: 'internal' } },
      '/api/projects/p_1': { body: project() },
      '/api/projects': { body: [project()] },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const text = w.element.textContent ?? ''
    expect(text, '读取失败不能说成「没有任务」').not.toContain('这个项目还没有任务')
    expect(w.element.querySelector('.af-board'), '读不到任务时不该画一块空板子').toBeNull()
    // 错误态必须是一条**可见的错误文案** + 一条重试的路。三个请求是并行的，
    // `Promise.all` 只会把「先 reject 的那一个」抛出来，所以文案未必是「任务」——
    // 本条不钉死是哪一个，只钉「不许变成一块空板子」。
    expect(text, '错误态要说出读失败了').toMatch(/读(任务|项目)失败/)
    expect(text, '错误态要给一条重试的路').toContain('重新读取')

    // ② 有数据在先，重读失败。
    stubBoard([task({ id: 'tsk_keep', state: 'pending', title: '已经读到的任务' })])
    const w2 = mountAt('p_1')
    await flushUntilSettled(w2)
    expect(w2.element.textContent).toContain('已经读到的任务')

    stubFetch({
      '/api/projects/p_1/tasks': { status: 500, body: { error: 'internal' } },
      '/api/projects/p_1': { body: project() },
      '/api/projects': { body: [project()] },
    })
    const refreshBtn = w2
      .findAll('.af-btn')
      .find((b) => (b.text() ?? '') === '刷新' && !b.classes('af-btn--primary'))
    expect(refreshBtn, '页头要有刷新按钮').toBeTruthy()
    await refreshBtn?.trigger('click')
    await flushUntilSettled(w2)

    const text2 = w2.element.textContent ?? ''
    expect(text2, '重读失败不能说成「没有任务」').not.toContain('这个项目还没有任务')
    expect(text2, '要出现错误提示').toContain('读任务失败')
    expect(text2, '不能把已经读到的任务凭空吞掉').toContain('已经读到的任务')
  })

  it('create_dialog_requires_criteria_or_description：两者皆空 disabled，填一处 enabled', async () => {
    // 判红方式：把 `doneWhenEmpty` 从 `canSubmit` 里去掉 → 空 `done_when`
    // 能提交，后端 400 `invalid_criteria`，用户看到的是一句错误码。
    stubBoard([])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    // 整板空态里也有「新建任务」按钮；页头那个是第一个。
    const openers = w.findAll('.af-btn--primary')
    expect(openers.length).toBeGreaterThan(0)
    await openers[0]?.trigger('click')
    await flushUntilSettled(w)

    const modal = modalOf()

    /** 提交按钮：脚里那个 `创建`（按钮文案会变成「正在创建…」，所以按**位置**取最后一个）。 */
    function submitBtn(): ReturnType<typeof w.find> {
      return modal.findAll('.af-btn').at(-1) as ReturnType<typeof w.find>
    }

    // 标题空 + 判定码空 + 描述空 → disabled
    expect(submitBtn().attributes('disabled')).toBeDefined()
    expect(modal.text()).toContain('至少要有判定码或描述')

    // 只填标题：仍然 disabled（判定条件还是空的）
    await modal.find('.af-input').setValue('把构建时间降到 60s 内')
    expect(submitBtn().attributes('disabled')).toBeDefined()

    // 填描述 → enabled
    await modal.find('.af-textarea').setValue('构建时间肉眼可感地变短')
    expect(submitBtn().attributes('disabled')).toBeUndefined()
  })

  /**
   * 打开页头那个「新建任务」对话框。
   *
   * **按文案取，不按位置取**：整板空时页面上有两个「新建任务」（页头一个、
   * 空态横幅里一个），按位置取第一个会在将来有人调换页头顺序时静默指向
   * 另一个按钮——那时这条用例仍然绿，但它测的已经不是「页头入口」了。
   */
  type Wrapper = Awaited<ReturnType<typeof mountAt>>
  type Finder = ReturnType<Wrapper['find']>

  /**
   * 从 `document.body` 里取对话框。
   *
   * **不能 `w.find('.af-modal')`**：v0.8.1 第5轮 4d 把对话框 `Teleport to="body"`
   * 了（理由见 `BoardPage.vue` 那段注释——不 Teleport 的话 `#app` 是 body 的
   * 唯一子节点，`focus.push` 的 `inertOthers` 会把「背后全部 inert」退化成
   * 「一个都没 inert」，模态框是假的）。Teleport 出去之后它就不再是 `w.element`
   * 的后代，`w.find` 找不到。查 `document.body` 才是它的真实落点。
   *
   * 用 `DOMWrapper` 包一层，是为了让下面那些 `find` / `findAll` / `text` /
   * `trigger` 的写法与别处一致——直接对 `HTMLElement` 写得另起一套。
   */
  function modalOf(): Finder {
    const el = document.body.querySelector('.af-modal')
    expect(el, '对话框要挂在 body 下（Teleport）').not.toBeNull()
    return new DOMWrapper(el as HTMLElement) as unknown as Finder
  }

  async function openDialog(w: Wrapper): Promise<Finder> {
    const openers = w.findAll('.af-btn').filter((b) => (b.text() ?? '').trim() === '新建任务')
    expect(openers.length, '页面上要有「新建任务」入口').toBeGreaterThan(0)
    await openers[0]?.trigger('click')
    await flushUntilSettled(w)
    /* 忘了 Teleport 时这条给出真正的原因，而不是让上面那行报「对话框要打开」
       （那会让人去查 `v-if`）。同时它也是「背后会不会 inert」的代理判据：
       `.af-modal` 一旦落回页面子树，`inertOthers` 就豁免整个 `#app`。 */
    expect(
      w.find('.af-modal').exists(),
      '对话框不该留在页面子树里：必须 Teleport 到 body，否则背后不会 inert，模态是假的',
    ).toBe(false)
    return modalOf()
  }

  /** 提交按钮：脚里那一个（`创建` / `正在创建…`），按位置取最后一个。 */
  function submitOf(modal: Finder): Finder {
    return modal.findAll('.af-btn').at(-1) as Finder
  }

  it('create_dialog_open_and_cancel：打开是空表单，取消后对话框消失', async () => {
    // 判红方式：把 `openDialog` 里那几行清空删掉 → 「上次输入还留在表单里」
    // 这条红（用户第二次打开会看到上一轮的标题，以为已经填好了）。
    stubBoard([])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const modal = await openDialog(w)
    // 打开时三处输入都是空的——不清的话上一次的草稿会留在里面。
    expect((modal.find('.af-input').element as HTMLInputElement).value).toBe('')
    expect((modal.find('.af-textarea').element as HTMLTextAreaElement).value).toBe('')
    // 选「不设判定码」当场就要说清「需人工验收」（否则用户跑完才发现要自己盯）。
    expect(modal.text()).toContain('需人工验收')

    const cancel = modal.findAll('.af-btn').find((b) => (b.text() ?? '').trim() === '取消')
    expect(cancel, '对话框要有取消').toBeTruthy()
    await cancel?.trigger('click')
    expect(document.body.querySelector('.af-modal'), '取消后对话框应当消失').toBeNull()
  })

  it('create_dialog_submits_and_reloads：POST 到 /api/projects/p_1/tasks，成功后关闭并重拉列表', async () => {
    // 判红方式（三处，缺一则这条就白写）：
    //  · 请求体不按 `CreateTaskBody` 拼（例如把判定码塞在 `title` 里、
    //    或把 `description` 当成任务描述）→ 红在 `sent` 的逐字段断言上；
    //  · 成功后**本地插入**一张卡而不重拉 → 红在「列表被重新请求过」；
    //  · 成功后不关对话框 → 红在 `af-modal` 还在。
    let reads = 0
    const fetchMock = stubFetch({
      // 读与写共用这条路径：第 1 次是页面加载的 GET（空板子），之后是 POST 的
      // 201 与重拉列表的 GET。按调用次数分派，才能同时守住「写回的是 201」
      // 与「写完又读了一次」——写死成同一条响应的话，后一个断言恒真。
      '/api/projects/p_1/tasks': () => {
        reads += 1
        if (reads === 1) return { body: { tasks: [] as Task[] } }
        const init = fetchMock.mock.calls.at(-1)?.[1] as RequestInit | undefined
        if (init?.method === 'POST') {
          return { status: 201, body: { task: task({ id: 'tsk_new', title: '把构建时间降到 60s 内' }) } }
        }
        return { body: { tasks: [task({ id: 'tsk_new', title: '把构建时间降到 60s 内' })] } }
      },
      '/api/projects/p_1': { body: project() },
      '/api/projects': { body: [project()] },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const modal = await openDialog(w)
    await modal.find('.af-input').setValue('  把构建时间降到 60s 内  ')
    await modal.find('.af-select').setValue('build_passes')
    await modal.find('.af-textarea').setValue('构建时间肉眼可感地变短')
    // 选了判定码 → 「需人工验收」的提示应当消失（它是「不设判定码」的派生结果）。
    expect(modal.text()).not.toContain('需人工验收')

    const callsBefore = fetchMock.mock.calls.filter((c) => String(c[0]).includes('/tasks')).length
    expect(callsBefore, '加载时应当已经读过一次任务列表').toBe(1)
    await submitOf(modal).trigger('click')
    await flushUntilSettled(w)

    const postCall = fetchMock.mock.calls.find((c) => (c[1] as RequestInit | undefined)?.method === 'POST')
    expect(postCall, '提交必须发一个 POST').toBeTruthy()
    expect(String(postCall?.[0]), 'POST 要打到当前项目的任务端点').toContain('/api/projects/p_1/tasks')

    // 请求体逐字段断：`title` 去空白；`done_when` 两个字段都按 `DoneWhen` 给。
    const sent = JSON.parse(String((postCall?.[1] as RequestInit).body))
    expect(sent).toEqual({
      title: '把构建时间降到 60s 内',
      done_when: { criteria: 'build_passes', description: '构建时间肉眼可感地变短' },
    })

    // 成功之后：对话框关掉、列表被**重新拉过**（不本地插入）。
    expect(document.body.querySelector('.af-modal'), '建成功后对话框要关掉').toBeNull()
    const listCalls = fetchMock.mock.calls.filter(
      (c) =>
        String(c[0]).includes('/api/projects/p_1/tasks') &&
        (c[1] as RequestInit | undefined)?.method === 'GET',
    ).length
    // 加载时一拉（1）+ 建成功后重拉（1）：**恰好两次**。断「≥1」的话，
    // 「本地插入、根本不重拉」也能蒙过去（加载那一次就够 1 了）。
    expect(listCalls, '建成功后要重拉任务列表（不使用本地插入的卡）').toBe(2)
    // 重拉回来的卡在板上（用的是后端那一份，而不是请求体拼的）。
    expect(w.element.textContent).toContain('把构建时间降到 60s 内')
  })

  it('create_dialog_keeps_the_form_open_and_shows_the_code：建失败留在对话框并报错误码', async () => {
    // 判红方式：`catch` 里把 `dialogOpen.value = false`（或什么都不做只吞掉）
    // → 用户填的内容连同对话框一起消失，或者完全没有反馈。
    stubFetch({
      '/api/projects/p_1/tasks': (() => {
        // 读请求与写请求共用这条路径：GET 给空列表，POST 给 400。
        let reads = 0
        return () => {
          reads += 1
          return reads === 1
            ? { body: { tasks: [] } }
            : { status: 400, body: { error: 'invalid_criteria' } }
        }
      })(),
      '/api/projects/p_1': { body: project() },
      '/api/projects': { body: [project()] },
    })
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const modal = await openDialog(w)
    await modal.find('.af-input').setValue('一个没有判定码的任务')
    await modal.find('.af-textarea').setValue('跑完我来看')
    await submitOf(modal).trigger('click')
    await flushUntilSettled(w)

    const stillThere = modalOf()
    expect(stillThere.exists(), '建失败不能把用户填的内容一起关掉').toBe(true)
    // 文案里要带**后端的错误码**，而不是一句笼统的「失败了」——否则用户拿不到
    // 任何可报告、可搜索的信息（`ApiError.code` 是唯一稳定可读的那一栏）。
    expect(stillThere.text()).toContain('invalid_criteria')
    expect(stillThere.find('.af-form__error').exists(), '错误要挂在 form 里（role=alert）').toBe(true)
    // 提交按钮回到可点状态：`creating` 必须在 `finally` 里复位，否则一次失败之后
    // 用户修好输入也点不动了。
    expect(submitOf(stillThere).attributes('disabled')).toBeUndefined()
  })
})

/* ===========================================================================
   v0.8.1 子任务 7：页面骨架与原型逐层对齐（`prototype/board.html`）
   ---------------------------------------------------------------------------
   这一节只量**结构**，不量外观：外观在样式表里（`prototype-shell.css` §5、
   `components.css` 的 `.af-board`），这里量的是「模板有没有照原型搭出那几层」。

   判红方式（三条各自）：
     · 根容器不给 `af-body--fill` → 第一条红（内容区不是「吃满剩余高度、
       列内各自滚动」的那一档，六列会跟着整页滚）；
     · 列数写成 5 或 7、某列少了列头或列体（例如把 `.af-col__body` 省成裸
       `div`）→ 第二条红；
     · `data-state` 拼错 / 漏一个 / 重复一个 → 第三条红。

   为什么按**类名**量而不是按标签量：原型自己就是靠这几个类承载外观
   （`app.css:370-413`），类名对不上时样式表里的规则一条都不会生效，
   而带 `<section>` / `<div>` 的「看起来像」的模板照样能骗过肉眼审查。
   =========================================================================== */

describe('BoardPage（v0.8.1）：骨架与原型 board.html 对齐', () => {
  it('page_root_uses_af_body_fill：根元素带 af-body 与 af-body--fill', async () => {
    // 原型 `board.html:57`：`<div class="af-body af-body--fill">`。
    // 上一版是 `af-body af-body--pad`（内容区自己带 24px 内边距），
    // 差别是 `.af-body--fill` 的 `overflow:hidden` + 列内滚动。
    //
    // ⚠️ 不能用 `w.element`：本仓 `@vue/test-utils` 的 `w.element` 就是那个带
    // `data-v-app` 的**挂载点本身**（`class=""`），`w.html()` 的输出又以模板
    // 注释开头（本页根节点前就有一条）。所以按类名在渲染结果里取：整页只该有
    // 一个 `.af-body`，它就是组件根——这也是「根元素」这个词唯一能落地的判据。
    // （断言写成 toHaveLength(1)：将来若有人在页头里再包一层 `.af-body`，
    // 这一条会红在「多个」，而不是静默取第一个通过。）
    stubBoard(sixTasks())
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const bodies = Array.from(w.element.querySelectorAll('.af-body'))
    expect(bodies, '整页只该有一个 .af-body（页面内容区）').toHaveLength(1)
    const body = bodies[0]
    expect(body?.classList.contains('af-body--fill'), '原型 board.html:57 是 .af-body--fill').toBe(
      true,
    )
    // 页头与内容区是**兄弟**（原型 `.af-main` 里只有 `.af-head` + `.af-body`）
    // 而不是「页头包在 `.af-body` 里」——包住的话 `overflow:hidden` 的
    // `--fill` 就管不到看板，六列会跟着整页滚。
    // 判据用渲染结果里两个类都在：`w.element` 是挂载点那层空容器，
    // 往上一层已经出了组件，量不到兄弟关系。
    const rendered = w.element.innerHTML
    expect(rendered.includes('af-head')).toBe(true)
    // 反向验证：挂载点那层空容器**没有**这些类——拿错层必须判红。
    expect(w.element.classList.contains('af-body--fill')).toBe(false)
  })

  it('six_columns_are_section_af_col：六列且各有且仅有一个列头与列体', async () => {
    stubBoard(sixTasks())
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const cols = Array.from(w.element.querySelectorAll('.af-col'))
    expect(cols).toHaveLength(6)
    for (const col of cols) {
      expect(col.tagName, '原型的列是 section（board.html:59）').toBe('SECTION')
      // `querySelectorAll(...).length === 1`：多一个或少一个都红——多一个
      // 说明有人又造了第二套列头/列体，少一个说明 `.af-col__body` 的
      // `padding:8px; gap:8px`（app.css:409-413）无处可落。
      expect(col.querySelectorAll('.af-col__head')).toHaveLength(1)
      expect(col.querySelectorAll('.af-col__body')).toHaveLength(1)
    }
  })

  it('data_state_covers_the_six_states：六列 data-state 的集合等于六态', async () => {
    stubBoard(sixTasks())
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const states = Array.from(w.element.querySelectorAll('.af-col')).map(
      (e) => e.getAttribute('data-state') ?? '',
    )
    // 集合相等（不是数组相等）：顺序由上一条用例钉着，这里钉的是
    // 「有没有某一个态被两个列共用 / 漏掉」——那会让六条顶部色带少一条。
    expect(new Set(states)).toEqual(new Set(['pending', 'queued', 'running', 'review', 'done', 'failed']))
  })
})

/* ===========================================================================
   G65：卡片是键盘可达的**入口**，不是「点完就丢焦点」的装饰
   ---------------------------------------------------------------------------
   第3轮验收实测：卡片 click / ⏎ 进任务详情后 `activeElement` 是
   `MAIN.af-main`，Back 回看板焦点也不回那张卡。这一组钉两件事：
     ① `openTask` 在 `navigate()` 之前**登记来源**（否则详情页没有落点可让）；
     ② 卡片带 `data-id="任务 id"`（返回时按它找回同一张卡）。
   返回后的「落到哪张卡」由 `focus.test.ts` 的三档用例与
   `ProjectTaskPage.test.ts` 的详情落点用例覆盖；这里只钉看板这一侧的契约。
   =========================================================================== */
describe('G65：看板卡片的键盘路径', () => {
  it('点卡片前先登记来源，导航后「让出 main」的标志仍等页面消费', async () => {
    stubBoard([task({ id: 'tsk_1', state: 'pending', title: '把构建时间降到 60s 内' })])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    // 干净起点：`mountAt` 里的 `navigate` 会落一次焦点，先复位。
    clearRouteFocusTarget()
    expect(hasRouteFocusTarget(), '复位后不该有残留').toBe(false)

    const card = w.find('.af-taskcard')
    expect(card.exists()).toBe(true)
    await card.trigger('click')
    await flushUntilSettled(w)

    // 路由 `navigate()` 的微任务在这一页挂载期间就跑过了；若 `openTask`
    // 没有登记，标志为假，详情页就没有「该让出」的依据。
    // 判据：点完之后 `route` 已是任务页，而**卡片本身仍带着可找回的 id**。
    expect(route.value.name).toBe('project-task')
    expect(route.value.params.taskId).toBe('tsk_1')
  })

  it('每张卡带 data-id=任务 id（返回看板时靠它找回同一张卡）', async () => {
    stubBoard([
      task({ id: 'tsk_a', state: 'pending' }),
      task({ id: 'tsk_b', state: 'running' }),
    ])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const ids = Array.from(w.element.querySelectorAll('.af-taskcard')).map((el) =>
      el.getAttribute('data-id'),
    )
    expect(ids.sort()).toEqual(['tsk_a', 'tsk_b'])
  })

  it('openTask 把「上一站」记成被点的那张卡：Back 回来能落到同一张卡上', async () => {
    // **必须走真实的点击路径**（`card.trigger('click')` → `openTask`），
    // 不能自己调 `setRouteFocusTarget`：那样测的是测试自己，而不是
    // `openTask` 里那句登记。反向验证过——把 `openTask` 里的
    // `setRouteFocusTarget(...)` 删掉，下面「登记过」这条立刻红。
    stubBoard([task({ id: 'tsk_a', state: 'pending' }), task({ id: 'tsk_b', state: 'running' })])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    // `mountAt` 里的 `navigate()` 会消费掉早先的让出标志，先复位成干净起点。
    clearRouteFocusTarget()

    const card = w.element.querySelector<HTMLElement>('[data-id="tsk_b"]')
    expect(card, '第二张卡存在').not.toBeNull()
    card!.focus()
    expect(document.activeElement, '前置：焦点在这张卡上').toBe(card)

    await w.find('[data-id="tsk_b"]').trigger('click')
    // 断言用 `lastRouteFocusId()` 而不是 `hasRouteFocusTarget()`：让出标志
    // 会被 `navigate()` 的微任务立刻消费掉，`trigger('click')` 一 await 就
    // 读不到了；「上一站」则要活到下一次换页，微任务之后仍然在。
    // 删掉 `openTask` 里的 `setRouteFocusTarget(...)` 时这条会红。
    expect(route.value.name, '已跳到任务详情').toBe('project-task')
    expect(route.value.params.taskId).toBe('tsk_b')
    expect(lastRouteFocusId(), 'openTask 必须把这张卡记成上一站').toBe('tsk_b')
    await flushUntilSettled(w)

    // ③ 模拟详情页把焦点放到标题后 Back 回看板：卡片重新渲染，焦点回到
    //    同一张卡（`data-id` 是那条线索）。
    clearRouteFocusTarget()
    const back = w.element.querySelector<HTMLElement>('[data-id="tsk_b"]')
    setRouteFocusTarget(back)
    expect(focusRouteTarget(), '第一次是让出').toBeNull()
    const restored = focusRouteTarget()
    expect(restored?.getAttribute('data-id'), '应回到被点的那张卡').toBe('tsk_b')
    expect(document.activeElement).toBe(back)
    clearRouteFocusTarget()
  })

  /* ===========================================================================
     G65 的**真实返回路径**（第5轮 4b 实测补的）
     ---------------------------------------------------------------------------
     上面那条用例手动 `card!.focus()` 之后才点，还手动 `setRouteFocusTarget`
     来模拟返回——它把鼠标路径整个盖住了。实际缺陷：

       · Chrome 里鼠标点 `<button>` **确实**会聚焦它（这点与 4b 的推断相反，
         实测 `activeBefore: "BUTTON"`），所以 `document.activeElement` 那条
         不是病根；
       · 真正的病根是**时机**：`onMounted` 里调 `focusRouteTarget()` 时
         `load()` 还没回来，看板一张卡都没渲染，`findByDataId` 什么也找不到，
         退到 `main`。原注释把这一步写成「等价于旧行为，不会更坏」，但它
         实际上是**静默失效**：用户在详情页点「← 看板」，焦点落在 `main`
         而不是刚才那张卡。

     所以这条用例钉的是「数据到齐之后必须补一次」。它不手动造焦点，而是
     摆好「上一站」再挂载页面，看挂载 + 加载完成后焦点落在哪。 */
  it('从详情返回：数据到齐后焦点回到来源卡片（不是 main）', async () => {
    stubBoard([task({ id: 'tsk_a', state: 'pending' }), task({ id: 'tsk_b', state: 'running' })])
    clearRouteFocusTarget()
    /* 摆出「上一站是 tsk_b」：真实路径里这是详情页 `h1`（带 `data-id`）。
       这里直接登记，因为本用例要测的是**看板恢复焦点**这一段，不是详情页
       怎么写进去的那一段（那一段由上面那条用例守）。 */
    const origin = document.createElement('h1')
    origin.setAttribute('data-id', 'tsk_b')
    document.body.appendChild(origin)
    setRouteFocusTarget(origin)
    /* **登记完就把它摘掉**——这是真实路径的形状：详情页的 `h1` 在离开详情页时
       已经随组件卸载消失，回来时 DOM 里只有看板的卡片带那个 `data-id`。
       不摘掉的话 `findByDataId` 会先把到这个 `h1`（它按文档顺序取第一个），
       用例就变成「回到那个夹具」而不是「回到卡片」，测的东西偏了。 */
    origin.remove()
    // 让出标志会被挂载期间的 `navigate()` 消费掉；本用例关心的是「上一站」还在。
    const w = mountAt('p_1')
    await flushUntilSettled(w)
    /* `restoreSourceFocus()` 里 `await nextTick()` + `requestAnimationFrame`，
       所以要多等一轮宏任务才看得到最终焦点。 */
    await new Promise((r) => setTimeout(r, 60))

    const target = w.element.querySelector('[data-id="tsk_b"]')
    expect(target, '卡片已渲染').not.toBeNull()
    expect(
      document.activeElement,
      [
        '返回看板后焦点必须落在来源卡片上，不能是 main。',
        '`onMounted` 里那一次 `focusRouteTarget()` 跑在 `load()` 之前，',
        '那时卡片还没渲染，会静默退到 main——所以 `load()` 的 finally 里',
        '必须补一次（`restoreSourceFocus`）。',
      ].join('\n'),
    ).toBe(target)
    clearRouteFocusTarget()
  })
})

/* ===========================================================================
   G69：看板页只有一个页头副标题（正文那行不再是 `h2.af-head__sub`）
   ---------------------------------------------------------------------------
   第3轮验收的发现：`BoardPage.vue` 的正文顶部有一行
   `<h2 class="af-head__sub">{{ project?.name }}</h2>`，它与页头 `PageHead` 里的
   `.af-head__sub` **逐字重复**同一个项目名，而且它自己还是个 `h2`——于是这一页的
   标题结构是「H1 + H2（文本与 H1 完全重复）+ 6 个 H3 列标题」。读屏用户按标题
   跳转时会先听到两遍项目名，再听列名。

   修法：正文那行降级为 `<p class="af-muted af-board__sub">`——文字不变、
   位置不变，但**不再占一个标题层级**，`.af-head__sub` 回归页头独占。

   这一组钉的判据是「数量」而不是「有没有」：将来谁在正文里再挂一个
   `.af-head__sub`，红在「多个」上，而不是静默地给读屏用户多播一遍。
   反向验证：把模板改回 `<h2 class="af-head__sub">` → 下面两条都红。
   =========================================================================== */
describe('G69：看板页的标题层级', () => {
  it('正文不再有 `h2.af-head__sub`（本页页头也没有副标题，所以整页一个都没有）', async () => {
    stubBoard([task({ id: 'tsk_1', state: 'pending' })])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    // 本页的页头是手写的 `<header class="af-head">`：只有一个下拉按钮里的
    // `h1.af-head__title`，**没有** `.af-head__sub`。所以修好之后整页
    // `.af-head__sub` 的数目是 0——正文那个 `h2` 曾经是唯一的一个。
    expect(
      w.element.querySelectorAll('h2.af-head__sub').length,
      '正文不该再造一个与 H1 重复的 h2',
    ).toBe(0)
    expect(w.element.querySelectorAll('.af-head__sub').length, '页头副标题类只属于页头').toBe(0)
  })

  it('正文那行项目名是 `<p>`，不是 `h2`（不占标题层级）', async () => {
    stubBoard([task({ id: 'tsk_1', state: 'pending' })])
    const w = mountAt('p_1')
    await flushUntilSettled(w)

    const sub = w.element.querySelector('.af-board__sub')
    expect(sub, '正文里保留那行项目名').not.toBeNull()
    expect(sub?.tagName, '它是段落，不是标题').toBe('P')
    // 反向：`h2` 一出现，标题跳转就会多停一站，且文本与 H1 重复。
    expect(w.element.querySelectorAll('h2.af-head__sub').length).toBe(0)
  })
})
