/* ===========================================================================
   taskApi 的契约测试（v0.8.0 子任务 7）
   ---------------------------------------------------------------------------
   这一节守的不是「函数返回了什么东西」，而是**四个在页面测试里看不见的东西**：

   1. URL 与 HTTP 方法拼得对（拼错在页面上表现为 404，而 mock 过 API 模块的
      页面测试照样绿）；
   2. 响应体外面那层 `{ task }` / `{ tasks }` 被**解开**了（不解开，页面拿到
      的是 `{ task: … }`，`t.state` 恒 `undefined`，六列全空却不报错）；
   3. 请求体的键名是蛇形（`done_when` / `expected_version` / `retry_limit`）——
      写成驼峰后端会当成未提供，任务静默丢掉判定码；
   4. PATCH 的四条约束能表达「**显式清空**」：`{ retry_limit: null }` 必须出现
      在 JSON 里。写成 `undefined` 的话键会消失，于是「清成不限」变成「不动」，
      而这两件事在界面上是同一个按钮。

   打的是真 `fetch` 桩（`stubFetch`）而不是 mock 本模块：mock 本模块会让
   上面第 1 / 3 条变成恒真。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import {
  CRITERIA_CODES,
  createTask,
  getTask,
  listProjectTasks,
  patchTask,
  pushTaskState,
  submitAcceptance,
  submitFailure,
} from '@/domains/project/taskApi'
import type { Task } from '@/domains/project/taskTypes'
import { stubFetch } from '@/test/http'

/** 一个任务（形状照子任务 6 的 `TaskView`，字段全给）。 */
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
    version: 3,
    superseded_by: null,
    intent_ref: null,
    executions: [],
    ...over,
  }
}

/** 取第 n 次（默认最后一次）`fetch` 调用的实参。 */
function callArgs(fn: ReturnType<typeof stubFetch>, n = -1): [string, RequestInit] {
  const calls = fn.mock.calls
  const idx = n < 0 ? calls.length + n : n
  const c = calls[idx] as unknown as [string, RequestInit]
  return [String(c[0]), c[1] ?? {}]
}

/** 取请求体并解成对象（`sendJson` 已经把对象序列化成 JSON 串）。 */
function bodyOf(fn: ReturnType<typeof stubFetch>, n = -1): Record<string, unknown> {
  const [, init] = callArgs(fn, n)
  return JSON.parse(String(init.body)) as Record<string, unknown>
}

describe('taskApi：URL、方法与响应体的解包', () => {
  it('listProjectTasks 打 GET /api/projects/{id}/tasks 并解开 { tasks }', async () => {
    // 判红方式：`.then((r) => r.tasks)` 删掉（直接返回响应体）→ 返回的不是
    // 数组，`tasks.filter` 在页面上抛；或者 URL 少写 `/tasks` → 这里红。
    const fn = stubFetch({ '/api/projects/p_1/tasks': { body: { tasks: [task()] } } })
    const out = await listProjectTasks('p_1')

    const [url, init] = callArgs(fn)
    expect(new URL(url, 'http://hub.local').pathname).toBe('/api/projects/p_1/tasks')
    expect(init.method).toBe('GET')
    expect(Array.isArray(out)).toBe(true)
    expect(out[0]?.id).toBe('tsk_1')
  })

  it('listProjectTasks 的 state 筛选是可选的：不给就不带参数', async () => {
    // 判红方式：无条件拼 `?state=` → 后端把空串当非法值回 400 `invalid_task_state`。
    const fn = stubFetch({ '/api/projects/p_1/tasks': { body: { tasks: [] } } })
    await listProjectTasks('p_1')
    const [url] = callArgs(fn)
    expect(url).not.toContain('state=')

    await listProjectTasks('p_1', 'failed')
    const [url2] = callArgs(fn)
    expect(new URL(url2, 'http://hub.local').searchParams.get('state')).toBe('failed')
  })

  it('createTask 打 POST 并解开 { task }，请求体键名是蛇形 done_when', async () => {
    // 判红方式：请求体写成 `{ doneWhen }` → 后端缺 `done_when` 键，直接 400；
    // 返回值不解包 → 页面拿 `{ task }` 当任务用，标题显示为空。
    const fn = stubFetch({
      '/api/projects/p_1/tasks': { status: 201, body: { task: task({ id: 'tsk_new' }) } },
    })
    const out = await createTask('p_1', {
      title: '把构建时间降到 60s 内',
      done_when: { criteria: 'build_passes', description: null },
    })

    const [url, init] = callArgs(fn)
    expect(new URL(url, 'http://hub.local').pathname).toBe('/api/projects/p_1/tasks')
    expect(init.method).toBe('POST')
    expect(bodyOf(fn)).toMatchObject({
      title: '把构建时间降到 60s 内',
      done_when: { criteria: 'build_passes', description: null },
    })
    expect(out.id).toBe('tsk_new')
  })

  it('getTask 打 GET /api/tasks/{id} 并同时交出 task 与 executions', async () => {
    // 判红方式：只返回 `r.task` → 执行记录整段丢掉，详情页的「执行记录」
    // 永远是空的；URL 少写 `/api/tasks/` 段 → 这里红。
    const fn = stubFetch({
      '/api/tasks/tsk_1': {
        body: {
          task: task(),
          executions: [
            {
              id: 'exe_1',
              state: 'failed',
              base_commit: null,
              agent_id: 'claude',
              failure_kind: 'task_failed',
              reason: null,
              evidence: null,
              started_at: 'unix:1700000000',
              ended_at: 'unix:1700000100',
            },
          ],
        },
      },
    })
    const out = await getTask('tsk_1')

    const [url] = callArgs(fn)
    expect(new URL(url, 'http://hub.local').pathname).toBe('/api/tasks/tsk_1')
    expect(out.task.id).toBe('tsk_1')
    expect(out.executions).toHaveLength(1)
    // 空数组也要原样是 `[]`：后端有一条测试守着「空列表不是 null」，
    // 这里如果再兜一个 `?? []`，那条保证就白立了。
    expect(Array.isArray(out.executions)).toBe(true)
  })

  it('任务 id 带 `/` 时被编码（不编码会把路径切成多一段）', async () => {
    // 判红方式：URL 里不再 `encodeURIComponent(taskId)` → 下面的 pathname 变成
    // `/api/tasks/a/b`，红。
    const fn = stubFetch({ '/api/tasks/a%2Fb': { body: { task: task() } } })
    await getTask('a/b')
    const [url] = callArgs(fn)
    expect(new URL(url, 'http://hub.local').pathname).toBe('/api/tasks/a%2Fb')
  })
})

describe('taskApi：PATCH 的双层 Option（不改 / 清成不限 / 改成它）', () => {
  it('{ retry_limit: null } 真的被发出去（键在、值为 null）', async () => {
    // 判红方式：把 `PatchTaskBody` 的约束字段写成 `retry_limit?: number`
    // 再 `...(v ?? {})` 拼体 → `null` 被吞掉，键消失，后端当成「不改」。
    // 界面上「清成不限」与「不动」是同一个按钮，这个 bug 用户看不出来。
    const fn = stubFetch({ '/api/tasks/tsk_1': { body: { task: task() } } })
    await patchTask('tsk_1', { retry_limit: null, allowed_devices: null })

    const body = bodyOf(fn)
    expect(Object.keys(body), '显式清空必须留下键').toEqual(
      expect.arrayContaining(['retry_limit', 'allowed_devices']),
    )
    expect(body.retry_limit).toBeNull()
    expect(body.allowed_devices).toBeNull()
  })

  it('只改标题时，四条约束一个都不出现（不能顺手清空）', async () => {
    // 判红方式：把约束写成 `retry_limit: patch.retry_limit`（键恒定存在）→
    // 只想改标题的调用会把所有约束清成 NULL，这条红。
    const fn = stubFetch({ '/api/tasks/tsk_1': { body: { task: task() } } })
    await patchTask('tsk_1', { title: '新标题', expected_version: 3 })

    const body = bodyOf(fn)
    expect(body.title).toBe('新标题')
    expect(body.expected_version).toBe(3)
    for (const key of ['allowed_devices', 'allowed_agents', 'required_capabilities', 'retry_limit']) {
      expect(Object.keys(body), `只想改标题时不该出现 ${key}`).not.toContain(key)
    }
    expect(callArgs(fn)[1].method).toBe('PATCH')
  })
})

describe('taskApi：三个状态写端点', () => {
  it('pushTaskState 打 POST …/state，body 是 { to, expected_version }', async () => {
    // 判红方式：`to` 写成驼峰或漏 `/state` → 后端 404 / 400；`expected_version`
    // 漏了就丢掉 CAS，两个人同时改会静默覆盖（规格 §7 后端流程 4）。
    const fn = stubFetch({ '/api/tasks/tsk_1/state': { body: { task: task() } } })
    await pushTaskState('tsk_1', 'queued', 3)

    const [url, init] = callArgs(fn)
    expect(new URL(url, 'http://hub.local').pathname).toBe('/api/tasks/tsk_1/state')
    expect(init.method).toBe('POST')
    expect(bodyOf(fn)).toEqual({ to: 'queued', expected_version: 3 })
  })

  it('pushTaskState 不给 expectedVersion 时不带这个键', async () => {
    // 判红方式：无条件写 `expected_version: undefined` → `JSON.stringify`
    // 会删掉它（这条仍绿），但换成 `null` 就会被后端当成「给出且不符」→ 409。
    const fn = stubFetch({ '/api/tasks/tsk_1/state': { body: { task: task() } } })
    await pushTaskState('tsk_1', 'queued')
    expect(Object.keys(bodyOf(fn))).toEqual(['to'])
  })

  it('submitAcceptance 打 …/acceptance，verdict 原样传', async () => {
    // 判红方式：把 verdict 映射成布尔（`true/false`）→ 后端 400
    // `invalid_request_body`；漏 `note` → 「打回必须写原因」那条规则失效。
    const fn = stubFetch({ '/api/tasks/tsk_1/acceptance': { body: { task: task() } } })
    await submitAcceptance('tsk_1', { verdict: 'reject', note: '参数顺序错了' })

    const [url] = callArgs(fn)
    expect(new URL(url, 'http://hub.local').pathname).toBe('/api/tasks/tsk_1/acceptance')
    expect(bodyOf(fn)).toEqual({ verdict: 'reject', note: '参数顺序错了' })
  })

  it('submitFailure 打 …/failure，三个动作原样传', async () => {
    // 判红方式：把 `close` 改成 `cancel` → 后端 400 `invalid_failure_kind`；
    // 「主动放弃」在看板上就变成了什么也没发生。
    const fn = stubFetch({ '/api/tasks/tsk_1/failure': { body: { task: task() } } })
    for (const action of ['retry', 'edit', 'close'] as const) {
      await submitFailure('tsk_1', { action, reason: '换个思路' })
      const [url] = callArgs(fn)
      expect(new URL(url, 'http://hub.local').pathname).toBe('/api/tasks/tsk_1/failure')
      expect(bodyOf(fn)).toEqual({ action, reason: '换个思路' })
    }
  })
})

describe('taskApi：写请求要带 CSRF 头（否则后端一律 403）', () => {
  it('POST / PATCH 都带 X-CSRF-Token（读不到 cookie 时不编空值）', async () => {
    // 判红方式：绕开 `postJson`/`patchJson` 直接用裸 `fetch` → 头没了，
    // 生产环境每一次写都是 403 `csrf_token_missing_or_mismatch`，
    // 而所有 mock 层测试照样绿（`http.ts` 文件头说的那个坑）。
    //
    // 测试环境没有 `af_csrf` cookie，所以正确行为是**头不出现**——
    // 编一个空值只会得到一句「头补了却依然不通」的谜。
    document.cookie = 'af_csrf=abc123'
    try {
      const fn = stubFetch({
        '/api/tasks/tsk_1/state': { body: { task: task() } },
        '/api/tasks/tsk_1': { body: { task: task() } },
      })
      await pushTaskState('tsk_1', 'queued')
      await patchTask('tsk_1', { title: 'x' })

      for (const n of [0, 1]) {
        const headers = new Headers(callArgs(fn, n)[1].headers)
        expect(headers.get('X-CSRF-Token'), `第 ${n + 1} 个写请求没带 CSRF 头`).toBe('abc123')
      }
    } finally {
      // 清掉 cookie：`happy-dom` 的 cookie 是跨用例的，留着会让别处的断言漂。
      document.cookie = 'af_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT'
    }
  })
})

describe('taskApi：判定码下拉的四条', () => {
  it('四个判定码一个不少，且都有人话（后端 400 的 detail.allowed 就是这四个）', () => {
    // 判红方式：漏 `command_exit_zero` → 下拉里少一项，用户建不出这类任务，
    // 而后端会接受它（错在前端）。
    expect(CRITERIA_CODES.map((c) => c.code)).toEqual([
      'all_tests_pass',
      'build_passes',
      'review_approved',
      'command_exit_zero',
    ])
    for (const c of CRITERIA_CODES) {
      expect(c.label, `${c.code} 没有人话`).toMatch(/[\u4e00-\u9fa5]/)
    }
  })
})
