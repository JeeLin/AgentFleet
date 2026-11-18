/* ===========================================================================
   ProjectTaskPage 的页面级测试（里程碑 §8「测试标准」1–10）
   ---------------------------------------------------------------------------
   这一页只做**拼装与接线**：五段的出现条件、两个面板的出现条件、六个
   emit 各自打到哪个端点、写冲突时用户的输入还在不在。面板内部的断言
   （4 行数据、5 个动作、二次确认）在子任务 4 的测试文件里，这里不重复——
   重复的那一份迟早会和组件的实现漂成两种说法。

   夹具走 `stubFetch` 打真 HTTP 桩而不是 mock `taskApi` 模块：mock 模块会
   让「页面拼错了 URL / 漏了 expected_version」变成测试仍然绿的事，而拼错
   URL 在真实环境里是 404。

   每条用例都写了**判红方式**——这一节全是「看起来像那么回事、其实没做到」
   的重灾区（硬编码 id、判定码误接成 pass、冲突时清空表单）。
   =========================================================================== */

import { describe, expect, it, vi } from 'vitest'

import ProjectTaskPage from '@/domains/project/ProjectTaskPage.vue'
import type { Execution, Task } from '@/domains/project/taskTypes'
import { mountIn } from '@/test/mount'
import { clearRouteFocusTarget, setRouteFocusTarget } from '@/shared/focus'
import { settle, stubFetch } from '@/test/http'
import { navigate } from '@/router'

/* ---- 夹具 --------------------------------------------------------------- */

const SECS = 1_700_000_000

function execution(over: Partial<Execution> = {}): Execution {
  return {
    id: 'exe_1',
    state: 'succeeded',
    base_commit: 'abc1234def5678',
    agent_id: 'claude',
    failure_kind: null,
    reason: null,
    evidence: null,
    started_at: `unix:${SECS}`,
    ended_at: `unix:${SECS + 120}`,
    ...over,
  }
}

/**
 * 一个任务。字段**全给**：`FailurePanel` 读 `constraints` / `executions`，
 * `AcceptancePanel` 读 `done_when` / `criteria_known` / `needs_manual_acceptance`
 * ——缺一个就会走到 `??` 兜底分支上，测的就不是真实形状。
 */
function task(over: Partial<Task> = {}): Task {
  return {
    id: 'task_1',
    project_id: 'p_1',
    title: '把构建时间降到 60s 内',
    state: 'pending',
    done_when: { criteria: 'build_passes', description: null },
    needs_manual_acceptance: false,
    criteria_known: true,
    created_by: 'user',
    base_commit: 'abc1234def5678',
    created_at: `unix:${SECS}`,
    updated_at: `unix:${SECS}`,
    constraints: {
      allowed_devices: null,
      allowed_agents: null,
      required_capabilities: null,
      retry_limit: null,
    },
    version: 3,
    superseded_by: null,
    intent_ref: null,
    executions: [],
    ...over,
  }
}

/**
 * 等页面加载真的结束。
 *
 * 判据是页面自己的话（「正在读取任务…」消失），而不是推进固定轮数：固定
 * 轮数在机器忙的时候会停在中间态上，症状是「单独跑绿、全量跑红」。
 */
async function flushUntilSettled(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 40; i++) {
    await w.vm.$nextTick()
    await settle(2)
    if (!(w.element.textContent ?? '').includes('正在读取任务')) break
  }
  for (let i = 0; i < 4; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 把地址停在某个任务上，再挂组件（`route` 是模块级单例，每条用例自己钉起点）。 */
function mountAt(id: string): ReturnType<typeof mountIn<typeof ProjectTaskPage>> {
  navigate(`/project/p_1/task/${encodeURIComponent(id)}`, { replace: true })
  return mountIn(ProjectTaskPage)
}

/** `GET /api/tasks/{id}` 的响应体。 */
function detail(t: Task, executions: Execution[] = []): { status?: number; body?: unknown } {
  return { body: { task: t, executions } }
}

/** 一次写请求的解析结果：路径 + 请求体。 */
interface Posted {
  path: string
  body: Record<string, unknown>
}

/** 从 fetch 桩里挑出某个端点上的 POST。 */
function posts(fetchMock: ReturnType<typeof stubFetch>, suffix: string): Posted[] {
  return fetchMock.mock.calls
    .filter((c) => ((c[1] as RequestInit | undefined)?.method ?? 'GET').toUpperCase() === 'POST')
    .map((c) => {
      const init = c[1] as RequestInit
      return {
        path: new URL(String(c[0]), 'http://hub.local').pathname,
        body: JSON.parse(String(init.body)) as Record<string, unknown>,
      }
    })
    .filter((p) => p.path === suffix)
}

function textOf(w: { element: HTMLElement }): string {
  return w.element.textContent ?? ''
}

/** 取面板里某个 `data-action` 按钮。 */
function actionBtn(w: { element: HTMLElement }, name: string): HTMLButtonElement | null {
  return w.element.querySelector<HTMLButtonElement>(`[data-action='${name}']`)
}

/** 往一个输入框里填值（`v-model` 靠 `input` 事件同步）。 */
async function fill(w: { element: HTMLElement }, aria: string, value: string): Promise<void> {
  const input = w.element.querySelector<HTMLInputElement>(`input[aria-label='${aria}']`)
  expect(input, `找不到输入框「${aria}」`).not.toBeNull()
  if (input) {
    input.value = value
    input.dispatchEvent(new Event('input'))
  }
  await Promise.resolve()
}

/* ===========================================================================
   1. reads_task_id_from_the_route_params
   =========================================================================== */

describe('ProjectTaskPage：任务 id 从地址读', () => {
  it('reads_task_id_from_the_route_params：route 参数给 task_1 → getTask 以 task_1 调用', async () => {
    // 判红方式：把 id 硬编码成任何字面量（或改从 props 拿）→ 这条红。
    // v0.8.0 之前 `taskId` 在 `src/` 内**零读取**，所以这条是「深链接真的
    // 能打开一个任务」的第一道证。
    const fetchMock = stubFetch({ '/api/tasks/task_1': detail(task()) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const got = fetchMock.mock.calls.map((c) => new URL(String(c[0]), 'http://hub.local').pathname)
    expect(got).toContain('/api/tasks/task_1')
    expect(textOf(w)).toContain('把构建时间降到 60s 内')
  })

  it('id 里带 `/` 时按段编码，请求打到被编码的那条路径上', async () => {
    // 判红方式：去掉 `encodeURIComponent` → 请求路径被切成两段，桩打不中。
    const fetchMock = stubFetch({
      '/api/tasks/a%2Fb': detail(task({ id: 'a/b', title: '带斜杠的任务' })),
    })
    const w = mountAt('a/b')
    await flushUntilSettled(w)

    const got = fetchMock.mock.calls.map((c) => new URL(String(c[0]), 'http://hub.local').pathname)
    expect(got).toContain('/api/tasks/a%2Fb')
    expect(textOf(w)).toContain('带斜杠的任务')
  })

  it('地址里没有任务 id → 说「参数错误」，且**一个请求都不发**', async () => {
    // 判红方式：把 `missingId` 守卫删掉 → 会打 `/api/tasks/`，后端把它当成
    // 列表端点或 404，两种都不是用户想看的东西。
    navigate('/', { replace: true })
    const fetchMock = stubFetch({})
    const w = mountIn(ProjectTaskPage)
    await flushUntilSettled(w)

    expect(textOf(w)).toContain('参数错误')
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('404 task_not_found → 「任务不存在或已删除」+ 返回看板（不给重试）', async () => {
    // 判红方式：把 404 混进通用错误分支 → 用户看到一个永远不会成功的「重新读取」。
    stubFetch({ '/api/tasks/task_1': { status: 404, body: { error: 'task_not_found' } } })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const text = textOf(w)
    expect(text).toContain('任务不存在或已删除')
    expect(text, '404 不该给「重新读取」——重试一千次还是 404').not.toContain('重新读取')
    expect(w.findAll('.af-btn').some((b) => b.text().includes('返回看板'))).toBe(true)
  })

  it('其他读失败 → 错误条 + 一条重试的路（不画半份数据）', async () => {
    // 判红方式：catch 里把错误吞成「空任务」→ 页面会显示一块空详情，
    // 那是在说「这个任务是空的」，而真相是没读到。
    stubFetch({ '/api/tasks/task_1': { status: 500, body: { error: 'internal' } } })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    expect(textOf(w)).toContain('读任务失败')
    expect(w.element.querySelector("[data-section='origin']")).toBeNull()
    expect(w.findAll('.af-btn').some((b) => b.text().includes('重新读取'))).toBe(true)
  })
})

/* ===========================================================================
   2. renders_all_five_implemented_sections
   =========================================================================== */

describe('ProjectTaskPage：五段 + 两个占位段', () => {
  it('renders_all_five_implemented_sections：五个段都在，两个占位段各带版本', async () => {
    // 判红方式：删掉任意一段 → 红。占位段单独断 `data-section`，因为只断文字
    // 会漏掉「两段挤成一段」（同一句文案出现两次也能骗过 `toContain`）。
    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'running' }), [
        execution({ state: 'running', ended_at: null }),
      ]),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const text = textOf(w)
    for (const title of ['这个任务是怎么来的', '执行记录', '任务约束', '产出']) {
      expect(text, `缺段「${title}」`).toContain(title)
    }
    for (const key of ['origin', 'executions', 'constraints', 'output']) {
      expect(
        w.element.querySelector(`[data-section='${key}']`),
        `缺段 data-section=${key}`,
      ).not.toBeNull()
    }

    const cost = w.element.querySelector("[data-section='cost']")
    const butler = w.element.querySelector("[data-section='butler']")
    expect(cost, '缺「花的时间和钱」占位段').not.toBeNull()
    expect(butler, '缺「管家的判断」占位段').not.toBeNull()
    expect(cost?.textContent ?? '').toContain('v0.8.2')
    expect(butler?.textContent ?? '').toContain('v0.9.0')
    // 判红方式：把占位做成 `<a href="#">` / 一个灰按钮 → 红（按下去没反应会把
    // 「还没做」伪装成「坏了」）。
    expect(cost?.querySelector('a, button')).toBeNull()
    expect(butler?.querySelector('a, button')).toBeNull()
  })

  it('失败面板 / 验收面板各自的段也在（failed + review 一次看全五段）', async () => {
    // 判红方式：把两个面板写进 `v-else` 链里、导致同页只能出现一个 → 红。
    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'failed' }), [
        execution({ state: 'failed', failure_kind: 'task_failed' }),
      ]),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.querySelector("[data-panel='failure']"), '失败面板是第 3 段').not.toBeNull()

    stubFetch({ '/api/tasks/task_1': detail(task({ state: 'review' }), []) })
    const w2 = mountAt('task_1')
    await flushUntilSettled(w2)
    expect(w2.element.querySelector("[data-panel='acceptance']"), '验收面板是第 4 段').not.toBeNull()
  })

  it('需人工验收的任务在页头挂标签（有判定码的不挂）', async () => {
    // 判红方式：不看 `needs_manual_acceptance` 一律挂 → 红。
    stubFetch({
      '/api/tasks/task_1': detail(
        task({ needs_manual_acceptance: true, done_when: { criteria: null, description: '跑通' } }),
      ),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.querySelector("[data-tag='manual']")?.textContent).toContain('需人工验收')

    stubFetch({ '/api/tasks/task_1': detail(task({ needs_manual_acceptance: false })) })
    const w2 = mountAt('task_1')
    await flushUntilSettled(w2)
    expect(w2.element.querySelector("[data-tag='manual']")).toBeNull()
  })
})

/* ===========================================================================
   3. a_null_intent_ref_says_manually_created
   =========================================================================== */

describe('ProjectTaskPage：怎么来的（不编来源）', () => {
  it('a_null_intent_ref_says_manually_created：写「人工创建」，且不含任何假来源 id', async () => {
    // 判红方式：`intent_ref` 为 null 时留白、或按模板拼一个 id 形状的假来源 → 红。
    stubFetch({ '/api/tasks/task_1': detail(task({ intent_ref: null })) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const origin = w.element.querySelector("[data-section='origin']")
    expect(origin, '缺「怎么来的」一段').not.toBeNull()
    const t = origin?.textContent ?? ''
    expect(t).toContain('人工创建')
    // 反向验证：整段里不许出现任何 id 形状的「来源」。
    expect(t).not.toMatch(/\bintent_[A-Za-z0-9]+/)
    expect(t).not.toMatch(/\bint_[A-Za-z0-9]+/)
  })

  it('有 intent_ref 时原样显示那一个 id（不美化、不吞掉）', async () => {
    // 判红方式：一律写「人工创建」→ 红（那是另一种撒谎）。
    stubFetch({ '/api/tasks/task_1': detail(task({ intent_ref: 'int_9f2c' })) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const field = w.element.querySelector("[data-field='intent_ref']")
    expect(field?.textContent).toContain('int_9f2c')
    expect(field?.textContent).not.toContain('人工创建')
  })
})

/* ===========================================================================
   4. empty_executions_shows_the_empty_state_not_an_empty_table
   =========================================================================== */

describe('ProjectTaskPage：执行记录', () => {
  it('empty_executions_shows_the_empty_state_not_an_empty_table', async () => {
    // 判红方式：渲染一张只有表头的空表 → 红。空表头会让人以为「记录丢了」，
    // 而真相是「还没跑过」（规格把这句话定死成「还没有执行过」）。
    stubFetch({ '/api/tasks/task_1': detail(task(), []) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const section = w.element.querySelector("[data-section='executions']")
    expect(section?.textContent).toContain('还没有执行过')
    expect(section?.querySelector('table'), '空的时候不该有一张只有表头的表').toBeNull()
  })

  it('有执行记录时逐条渲染：序号 / 状态 / 起点 / 失败原因 / 起止', async () => {
    // 判红方式：只渲染最近一条、或按 id 排序 → 「第几次」会和真实顺序对不上。
    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'failed' }), [
        // 故意**倒序**给：页面必须自己按 `started_at` 排出「第 1 / 第 2 次」。
        execution({
          id: 'exe_2',
          state: 'failed',
          failure_kind: 'task_failed',
          started_at: `unix:${SECS + 500}`,
        }),
        execution({ id: 'exe_1', state: 'succeeded', started_at: `unix:${SECS}` }),
      ]),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const rows = Array.from(w.element.querySelectorAll("[data-section='executions'] tbody tr"))
    expect(rows).toHaveLength(2)
    expect(rows[0]?.textContent).toContain('第 1 次')
    expect(rows[0]?.textContent).toContain('succeeded')
    expect(rows[0]?.textContent, '起点要显示（短 sha）').toContain('abc1234')
    expect(rows[1]?.textContent).toContain('第 2 次')
    expect(rows[1]?.querySelector("[data-field='failure-kind']")?.textContent).toContain('task_failed')
  })
})

/* ===========================================================================
   5. the_failure_panel_appears_only_when_failed_or_last_execution_failed
   =========================================================================== */

describe('ProjectTaskPage：失败面板的出现条件', () => {
  it('the_failure_panel_appears_only_when_failed_or_last_execution_failed', async () => {
    // 三条用例：
    //   ① `failed` 任务                    → 出现
    //   ② `pending` 任务 + 最近一条执行失败 → 出现（任务已回待办但上次是失败）
    //   ③ `running` 任务 + 执行中           → **不出现**
    // 判红方式：只看 `task.state === 'failed'` → ② 红（用户看不到「上次为什么
    // 失败」，也就拿不到那五个处置动作）；无条件渲染 → ③ 红。

    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'failed' }), [
        execution({ state: 'failed', failure_kind: 'task_failed' }),
      ]),
    })
    const w1 = mountAt('task_1')
    await flushUntilSettled(w1)
    expect(
      w1.element.querySelector("[data-panel='failure']"),
      '① failed 任务要出现失败面板',
    ).not.toBeNull()

    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'pending' }), [
        execution({ state: 'failed', failure_kind: 'env_broken' }),
      ]),
    })
    const w2 = mountAt('task_1')
    await flushUntilSettled(w2)
    expect(
      w2.element.querySelector("[data-panel='failure']"),
      '② 任务回了 pending 但上次执行失败 → 仍要出现',
    ).not.toBeNull()

    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'running' }), [
        execution({ state: 'running', ended_at: null }),
      ]),
    })
    const w3 = mountAt('task_1')
    await flushUntilSettled(w3)
    expect(
      w3.element.querySelector("[data-panel='failure']"),
      '③ 正常在跑的任务不该有失败面板',
    ).toBeNull()
  })

  it('「最近一条」按时间算，不是按数组最后一项', async () => {
    // 判红方式：取 `executions[executions.length - 1]` 而不是按 `started_at`
    // 排 → 这一条红。接口不保证顺序，赌顺序的实现迟早会在某次后端改动后
    // 显示错的那一条。
    stubFetch({
      '/api/tasks/task_1': detail(task({ state: 'running' }), [
        // 最后一项是**较早**的那条（成功）；真正最近的那条失败了。
        execution({ id: 'exe_a', state: 'failed', failure_kind: 'task_failed', started_at: `unix:${SECS + 500}` }),
        execution({ id: 'exe_b', state: 'succeeded', started_at: `unix:${SECS}` }),
      ]),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.querySelector("[data-panel='failure']"), '最近一条是失败 → 面板要出现').not.toBeNull()
  })
})

/* ===========================================================================
   6. the_acceptance_panel_appears_only_in_review
   =========================================================================== */

describe('ProjectTaskPage：验收面板的出现条件', () => {
  it('the_acceptance_panel_appears_only_in_review', async () => {
    // 判红方式：把条件放宽成「有 done_when 就显示」→ 非 review 的五条红。
    // 画出来再 disabled 会让人以为「差一步就能点」，而那条边只对 review 开放。
    for (const state of ['pending', 'queued', 'running', 'done', 'failed'] as const) {
      stubFetch({ '/api/tasks/task_1': detail(task({ state }), []) })
      const w = mountAt('task_1')
      await flushUntilSettled(w)
      expect(w.element.querySelector("[data-panel='acceptance']"), `${state} 不该有验收面板`).toBeNull()
    }

    stubFetch({ '/api/tasks/task_1': detail(task({ state: 'review' }), []) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.querySelector("[data-panel='acceptance']"), 'review 必须有验收面板').not.toBeNull()
  })
})

/* ===========================================================================
   7 / 8. each_panel_event_calls_its_endpoint / keep_branch_calls_reject_with_a_prefixed_note
   =========================================================================== */

/**
 * 造一个「`failed` 且最近一条执行失败」的任务页，桩里带上两个写端点。
 *
 * 写端点用**函数**桩：每次调用现取同一份任务，这样成功之后的重取
 * （`afterWrite` 里的 `getTask`）拿到的是同一个版本号，下一条断言不会因为
 * 版本号跳了一格而对不上。
 */
function stubFailedPage(over: Partial<Task> = {}) {
  const t = task({
    state: 'failed',
    version: 7,
    executions: [execution({ state: 'failed', failure_kind: 'task_failed' })],
    ...over,
  })
  return stubFetch({
    '/api/tasks/task_1': () => ({ body: { task: t, executions: t.executions ?? [] } }),
    '/api/tasks/task_1/failure': () => ({ body: { task: t } }),
    '/api/tasks/task_1/acceptance': () => ({ body: { task: t } }),
  })
}

describe('ProjectTaskPage：每个面板事件打到哪个端点（表驱动）', () => {
  it('each_panel_event_calls_its_endpoint', async () => {
    // 判红方式：把 `reject` 接成 `pass` 的 body（或漏掉 `expected_version`）→
    // 对应的那一行立刻红。表驱动而不是一串各写一遍的用例：接线在规格里就是
    // **一张表**，测试就该是那张表的镜像。

    // ── 失败面板的三个动作 + 没有端点的 relax ──────────────────────────
    const failureCases: {
      name: string
      act: (w: ReturnType<typeof mountIn<typeof ProjectTaskPage>>) => Promise<void>
      /** `''` = 本轮没有端点，**不许**发请求。 */
      path: string
      body: Record<string, unknown>
    }[] = [
      {
        name: 'FailurePanel @retry',
        act: async (w) => {
          await actionBtn(w, 'retry')?.click()
        },
        path: '/api/tasks/task_1/failure',
        body: { action: 'retry', expected_version: 7 },
      },
      {
        name: 'FailurePanel @edit(reason)',
        act: async (w) => {
          await fill(w, '改任务再跑的原因', '换一台干净基线')
          await actionBtn(w, 'edit')?.click()
        },
        path: '/api/tasks/task_1/failure',
        body: { action: 'edit', reason: '换一台干净基线', expected_version: 7 },
      },
      {
        name: 'FailurePanel @close(reason)',
        act: async (w) => {
          await fill(w, '关掉这个任务的原因', '这件事不做了')
          await actionBtn(w, 'close')?.click()
        },
        path: '/api/tasks/task_1/failure',
        body: { action: 'close', reason: '这件事不做了', expected_version: 7 },
      },
      {
        name: 'FailurePanel @relax（本轮无端点，不发请求）',
        act: async (w) => {
          await actionBtn(w, 'relax')?.click()
        },
        path: '',
        body: {},
      },
    ]

    for (const c of failureCases) {
      const fetchMock = stubFailedPage()
      const w = mountAt('task_1')
      await flushUntilSettled(w)
      await c.act(w)
      await flushUntilSettled(w)

      if (c.path === '') {
        // 规格的接线表里没有 relax，后端也没有对应动作：**不许**伪造一个
        // `{action:'relax'}`（发出去只会得到 400/409，用户看到一句错误码）。
        expect(
          posts(fetchMock, '/api/tasks/task_1/failure'),
          '@relax 不该发任何请求',
        ).toHaveLength(0)
        expect(textOf(w), '@relax 要写明它还没有入口').toContain('v0.8.2')
      } else {
        const sent = posts(fetchMock, c.path)
        expect(sent, `${c.name} 没有打到 ${c.path}`).toHaveLength(1)
        expect(sent[0]?.body, `${c.name} 的请求体不对`).toEqual(c.body)
      }
    }

    // ── 验收面板的两个动作（`review` 才有面板）─────────────────────────
    const acceptCases: {
      name: string
      act: (w: ReturnType<typeof mountIn<typeof ProjectTaskPage>>) => Promise<void>
      body: Record<string, unknown>
    }[] = [
      {
        name: 'AcceptancePanel @pass(note)',
        act: async (w) => {
          // 本轮判定码永远「未判定」→ 第一下只展开确认区，第二下才发。
          await actionBtn(w, 'pass')?.click()
          await fill(w, '越过判定的理由', '判定没跑，人工看过改动')
          await actionBtn(w, 'pass-confirm')?.click()
        },
        body: {
          verdict: 'pass',
          note: '判定没跑，人工看过改动',
          override: true,
          expected_version: 7,
        },
      },
      {
        name: 'AcceptancePanel @reject(note)',
        act: async (w) => {
          await fill(w, '验收备注', '改了不该改的文件')
          await actionBtn(w, 'reject')?.click()
        },
        body: { verdict: 'reject', note: '改了不该改的文件', expected_version: 7 },
      },
    ]

    for (const c of acceptCases) {
      const fetchMock = stubFailedPage({ state: 'review' })
      const w = mountAt('task_1')
      await flushUntilSettled(w)
      await c.act(w)
      await flushUntilSettled(w)

      const sent = posts(fetchMock, '/api/tasks/task_1/acceptance')
      expect(sent, `${c.name} 没有打到验收端点`).toHaveLength(1)
      expect(sent[0]?.body, `${c.name} 的请求体不对`).toEqual(c.body)
    }
  })

  it('keep_branch_calls_reject_with_a_prefixed_note', async () => {
    // 判红方式：把它接成 `pass` → 任务被误判通过，这条必红。
    // 本轮没有分支存储，也没有合入执行器，所以「只保留分支」只能是
    // 打回 + 记原因。
    const fetchMock = stubFailedPage({ state: 'review' })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    await fill(w, '验收备注', '这次改动先留着')
    await actionBtn(w, 'keep-branch')?.click()
    await flushUntilSettled(w)

    const sent = posts(fetchMock, '/api/tasks/task_1/acceptance')
    expect(sent).toHaveLength(1)
    expect(sent[0]?.body.verdict, '只保留分支 ≠ 通过').toBe('reject')
    expect(String(sent[0]?.body.note)).toBe('只保留分支：这次改动先留着')
    expect(sent[0]?.body.expected_version).toBe(7)
  })

  it('「只保留分支映射成打回」的理由写在页面上（不能只待在里程碑文档里）', async () => {
    // 判红方式：把页面上那条说明删掉 → 红。规格 §8 `:1191-1194` 明写
    // 「这一点必须写进页面 tooltip，不能只在里程碑文档里」——所以这里断的是
    // **页面可见文本**，不是某个组件内部的 `title`。
    stubFetch({ '/api/tasks/task_1': detail(task({ state: 'review' }), []) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const note = w.element.querySelector("[data-note='keep-branch']")
    expect(note, '页面上要有一条解释「只保留分支 = 打回」的说明').not.toBeNull()
    const t = note?.textContent ?? ''
    expect(t).toContain('打回')
    expect(t).toContain('v0.8.2')
    expect(t).toContain('只保留分支：')
  })
})

/* ===========================================================================
   9. a_version_conflict_keeps_the_typed_note
   =========================================================================== */

describe('ProjectTaskPage：乐观锁冲突', () => {
  it('a_version_conflict_keeps_the_typed_note：409 之后 note 还在，且出现刷新提示', async () => {
    // 判红方式：在 catch 里清空 note / 重新挂载面板 / 整页换成错误态 → note
    // 输入框里的字没了，这条红。用户刚打的一段理由凭空消失是最难被原谅的
    // 一种「刷新」。
    const t = task({ state: 'review', version: 7 })
    stubFetch({
      '/api/tasks/task_1': () => ({ body: { task: t, executions: [] } }),
      '/api/tasks/task_1/acceptance': () => ({
        status: 409,
        body: {
          error: 'task_state_changed',
          detail: { expected_version: 7, actual_version: 8 },
        },
      }),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    await fill(w, '验收备注', '这段理由不能丢')
    await actionBtn(w, 'reject')?.click()
    await flushUntilSettled(w)

    expect(textOf(w)).toContain('任务刚被更新，已刷新，请重新提交')
    const note = w.element.querySelector<HTMLInputElement>("input[aria-label='验收备注']")
    expect(note, 'note 输入框必须还在（整页不该被换成错误态）').not.toBeNull()
    expect(note?.value, '用户填的 note 必须保留').toBe('这段理由不能丢')
  })

  it('别的写错误只说失败，不冒充冲突', async () => {
    // 判红方式：把所有写错误都说成「任务刚被更新」→ 红（用户会去刷新一个
    // 根本没被别人改过的任务，而真正的原因是「没写理由」）。
    const t = task({ state: 'review', version: 7 })
    stubFetch({
      '/api/tasks/task_1': () => ({ body: { task: t, executions: [] } }),
      '/api/tasks/task_1/acceptance': () => ({
        status: 400,
        body: { error: 'acceptance_note_required' },
      }),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    await fill(w, '验收备注', 'x')
    await actionBtn(w, 'reject')?.click()
    await flushUntilSettled(w)

    const text = textOf(w)
    expect(text).toContain('必须写明理由')
    expect(text).not.toContain('任务刚被更新')
  })
})

/* ===========================================================================
   写成功之后的收口
   =========================================================================== */

describe('ProjectTaskPage：写成功后的收口', () => {
  it('用响应里的 task 覆盖本地，并**重新拉取**执行记录（多了一条）', async () => {
    // 判红方式：成功后不重取 `executions` → 「重试之后还是原来那一条」，
    // 用户以为点了没反应。
    const before = task({ state: 'failed', version: 7, executions: [execution({ id: 'exe_1' })] })
    const after = task({
      state: 'queued',
      version: 8,
      executions: [execution({ id: 'exe_1' }), execution({ id: 'exe_2', state: 'queued' })],
    })
    let reads = 0
    const fetchMock = stubFetch({
      '/api/tasks/task_1': () => {
        reads += 1
        // 第一次读给 before；重试成功后的重取给 after。
        return reads === 1
          ? { body: { task: before, executions: before.executions ?? [] } }
          : { body: { task: after, executions: after.executions ?? [] } }
      },
      '/api/tasks/task_1/failure': () => ({ body: { task: after } }),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.querySelectorAll('[data-execution]')).toHaveLength(1)

    await actionBtn(w, 'retry')?.click()
    await flushUntilSettled(w)

    // 重试成功 → 任务离开失败，失败面板消失，执行记录变成两条。
    expect(w.element.querySelectorAll('[data-execution]')).toHaveLength(2)
    expect(w.element.querySelector("[data-panel='failure']"), 'queued 了就不该再显示失败面板').toBeNull()
    const reads2 = fetchMock.mock.calls.filter(
      (c) => new URL(String(c[0]), 'http://hub.local').pathname === '/api/tasks/task_1',
    )
    expect(reads2.length, '成功之后要重新拉一次执行记录').toBeGreaterThanOrEqual(2)
  })
})

/* ===========================================================================
   产出段
   =========================================================================== */

describe('ProjectTaskPage：产出段（起点 + 合入状态，不画 diff 入口）', () => {
  it('显示起点 commit 与「未合入」，且**没有** diff / 复盘链接', async () => {
    // 判红方式：画一个 `<a href="…/replay">看 diff</a>` → 红。复盘页在
    // v0.8.2 仍是骨架页，画出来就是死链接。
    stubFetch({ '/api/tasks/task_1': detail(task({ state: 'review' }), [execution()]) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const out = w.element.querySelector("[data-section='output']")
    expect(out, '缺产出段').not.toBeNull()
    expect(out?.textContent).toContain('未合入')
    expect(out?.querySelector("[data-field='output-base']")?.textContent).toContain('abc1234')
    // **不编分支名**：`branch` 列尚未落库（`task-model.md` §8 归 v0.8.2），
    // 所以这一格必须说「还没记录」，而不是拼一个 `afleet/<id>/eN`。
    // 判红方式：改回 `afleet/task_1/e1` 之类的派生字符串 → 下面这条红
    // （它会让人拿去 `git checkout` 然后失败）。
    const branch = out?.querySelector("[data-field='branch']")?.textContent ?? ''
    expect(branch, '分支这一格要说清「库里还没有」').toContain('v0.8.2')
    expect(branch, '不许编一个看起来像真 ref 的分支名').not.toContain('afleet/')
    expect(out?.querySelector('a'), '产出段不该有任何链接（diff / 复盘都还没有）').toBeNull()
    expect(out?.textContent, 'diff 入口留 v0.8.2').toContain('v0.8.2')
  })

  it('superseded_by 非空 → 多一行「已由 <id> 接续」', async () => {
    // 判红方式：不渲染这一行 → 红（用户不知道这块产出还有没有人要）。
    stubFetch({ '/api/tasks/task_1': detail(task({ superseded_by: 'task_2' }), []) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const line = w.element.querySelector("[data-field='superseded']")
    expect(line, 'superseded_by 非空时必须多一行').not.toBeNull()
    expect(line?.textContent).toContain('已由')
    expect(line?.textContent).toContain('task_2')
  })
})

/* ===========================================================================
   任务约束段的空态文案
   =========================================================================== */

describe('ProjectTaskPage：任务约束（空值写「未限定」，不留白）', () => {
  it('五条空态文案逐字对上', async () => {
    // 判红方式：把任一条留白、或改成「-」/「无」→ 红。留白会被读成
    // 「这一项读不出来」，而真相是「这一项不限」。
    stubFetch({ '/api/tasks/task_1': detail(task(), []) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const c = w.element.querySelector("[data-section='constraints']")
    expect(c?.textContent).toContain('未限定设备')
    expect(c?.textContent).toContain('未限定 Agent')
    expect(c?.textContent).toContain('未限定能力')
    expect(c?.textContent).toContain('重试上限用默认（2 次）')
    expect(c?.textContent, '版本要显示出来（乐观锁的可读形态）').toContain('第 3 版')
  })

  it('有值的约束照值显示，不写死「未限定」', async () => {
    // 判红方式：把 `||` 兜底写成恒真 → 有值时也写「未限定设备」，红。
    stubFetch({
      '/api/tasks/task_1': detail(
        task({
          constraints: {
            allowed_devices: 'device-a、device-b',
            allowed_agents: 'claude',
            required_capabilities: 'git、node 20',
            retry_limit: 3,
          },
        }),
        [],
      ),
    })
    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const c = w.element.querySelector("[data-section='constraints']")
    expect(c?.textContent).toContain('device-a、device-b')
    expect(c?.textContent).toContain('git、node 20')
    expect(c?.textContent).toContain('3 次')
    expect(c?.textContent).not.toContain('未限定设备')
  })
})

/* ===========================================================================
   时钟纪律（这一页是全页唯一的时钟）
   =========================================================================== */

describe('ProjectTaskPage：时钟', () => {
  it('页面只有一个 1 秒定时器，卸载时清掉', async () => {
    // 判红方式：把 `onUnmounted` 里的 `clearInterval` 删掉 → 每次进出任务页
    // 漏一个每秒一次的定时器。面板自己是**不许**持定时器的（子任务 4 有源码
    // 级断言），所以这一页只该有一个 1 秒定时器。
    vi.useFakeTimers()
    try {
      stubFetch({ '/api/tasks/task_1': detail(task({ state: 'review' }), []) })
      const setSpy = vi.spyOn(globalThis, 'setInterval')
      const clearSpy = vi.spyOn(globalThis, 'clearInterval')

      const w = mountAt('task_1')
      // 假定时器下不能等真时间，但 Promise 链是微任务，仍然能靠 `await` 推进。
      for (let i = 0; i < 12; i++) {
        await w.vm.$nextTick()
        await Promise.resolve()
      }

      const oneSecond = setSpy.mock.calls.filter((c) => c[1] === 1000)
      expect(oneSecond, '任务页应当只有页面层这一个 1 秒定时器').toHaveLength(1)

      w.unmount()
      expect(clearSpy, '卸载必须清掉定时器').toHaveBeenCalled()
    } finally {
      vi.useRealTimers()
    }
  })
})

/* ===========================================================================
   G65：从看板点卡片进来，焦点落在任务标题上（不是 MAIN.af-main）
   ---------------------------------------------------------------------------
   第3轮验收实测的缺陷：卡片 click / ⏎ 进任务详情后
   `document.activeElement === MAIN.af-main`，第一次 Tab 落在页头那颗
   「← 看板」上。这一组钉住修复后的落点：**任务标题 `h1`**（带
   `tabindex="-1"`，所以 ⇧Tab 回标题也成立）。

   反向验证：把 `ProjectTaskPage.vue` 里那个 `watch(() => task.value, …)` 整段
   删掉 → 「焦点落在标题上」那条红（落点退回 `main`）。
   =========================================================================== */
describe('G65：点卡片进来后焦点落在任务标题上', () => {
  it('登记了来源 → 焦点落到 h1 上，不是 main', async () => {
    stubFetch({ '/api/tasks/task_1': detail(task()) })
    // 模拟 `BoardPage.openTask`：先登记来源卡片，再跳转。
    const card = document.createElement('button')
    card.setAttribute('data-id', 'task_1')
    document.body.append(card)
    setRouteFocusTarget(card)

    const w = mountAt('task_1')
    await flushUntilSettled(w)

    const h1 = w.element.querySelector('h1')
    expect(h1, '详情页必须有标题').not.toBeNull()
    expect(document.activeElement, '焦点应落在任务标题上').toBe(h1)
    expect(h1?.getAttribute('tabindex'), '可编程聚焦的前提').toBe('-1')
    // 反向：绝不是那个无条件的 main 落点。
    expect(document.activeElement).not.toBe(document.querySelector('main, .af-main'))
    clearRouteFocusTarget()
  })

  it('直接输地址进来（没有登记来源）也会落到标题上——标题是这一页的起点', async () => {
    // 修复前：这条路径落到 `main`。修复后**也**落到标题：落点是「本页的
    // 第一个可读单元」这件事与「从哪儿来」无关，而从 `main` 起步时第一次
    // Tab 还是会先撞上页头的「← 看板」。
    //
    // 注意这条不是「第一条的复制」：第一条**登记了来源**（走让出那一档），
    // 这一条**完全不登记**——它证明落标题不依赖 sticky 状态。
    clearRouteFocusTarget()
    stubFetch({ '/api/tasks/task_1': detail(task()) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(document.activeElement).toBe(w.element.querySelector('h1'))
  })

  it('h1 带 data-id=任务 id，返回看板时靠它找回同一张卡', async () => {
    stubFetch({ '/api/tasks/task_1': detail(task({ id: 'task_1' })) })
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.querySelector('h1')?.getAttribute('data-id')).toBe('task_1')
    clearRouteFocusTarget()
  })

  it('取不到任务（404）时不抢焦点：没有标题可落，页面给空态', async () => {
    stubFetch({ '/api/tasks/task_1': { status: 404, body: { error: 'task_not_found' } } })
    const card = document.createElement('button')
    card.setAttribute('data-id', 'task_1')
    document.body.append(card)
    setRouteFocusTarget(card)
    // 登记了但页面没有 h1 可落——不该抛，也不该把焦点留在已卸载的卡片上。
    const w = mountAt('task_1')
    await flushUntilSettled(w)
    expect(w.element.textContent).toContain('任务不存在或已删除')
    clearRouteFocusTarget()
  })
})
