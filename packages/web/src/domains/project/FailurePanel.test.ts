/* ===========================================================================
   FailurePanel 组件测试
   ---------------------------------------------------------------------------
   对应里程碑 §4 的测试 4 / 5 / 11 / 14 / 15，加上「4 行数据 + 5 个动作」
   与「复盘按钮渲染但 disabled」这两条规格。每条都写**判红方式**。

   这个面板是**全站唯一的失败处置入口**（`task-model.md` §4 四条硬规则 4），
   所以「5 个动作一个都不能少」本身就是要测的东西——少一个就是少一条边。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import FailurePanel, { FAILURE_DIAGNOSIS } from '@/domains/project/FailurePanel.vue'
import type { Execution, FailureKind, Task } from '@/domains/project/taskTypes'
import { mountIn } from '@/test/mount'

const HERE = dirname(fileURLToPath(import.meta.url))
const STARTED_SECS = 1_700_000_000
const NOW = (STARTED_SECS + 600) * 1000

function execution(over: Partial<Execution> = {}): Execution {
  return {
    id: 'exe_2',
    state: 'failed',
    base_commit: 'abc1234def',
    agent_id: 'claude',
    failure_kind: 'task_failed',
    reason: '3 failed',
    evidence: '3 条断言失败',
    started_at: `unix:${STARTED_SECS}`,
    ended_at: `unix:${STARTED_SECS + 192}`,
    ...over,
  }
}

function task(over: Partial<Task> = {}): Task {
  return {
    id: 'tsk_1042',
    project_id: 'prj_1',
    title: '把构建时间降到 60s 内',
    state: 'failed',
    done_when: { criteria: 'all_tests_pass', description: null },
    needs_manual_acceptance: false,
    criteria_known: true,
    created_by: 'user',
    base_commit: 'abc1234def',
    created_at: `unix:${STARTED_SECS}`,
    updated_at: `unix:${STARTED_SECS}`,
    constraints: {
      allowed_devices: null,
      allowed_agents: null,
      required_capabilities: null,
      retry_limit: 3,
    },
    version: 1,
    superseded_by: null,
    intent_ref: null,
    executions: [execution(), execution({ id: 'exe_3' })],
    ...over,
  }
}

function mountPanel(t: Task = task(), ex: Execution | null = execution()) {
  return mountIn(FailurePanel, { props: { task: t, execution: ex, now: NOW } })
}

function textOf(w: { element: HTMLElement }): string {
  return w.element.textContent ?? ''
}

/** 按 `data-action` 取按钮。 */
function action(w: { element: HTMLElement }, name: string): HTMLButtonElement | null {
  return w.element.querySelector<HTMLButtonElement>(`[data-action='${name}']`)
}

describe('FailurePanel：4 行数据（`task-model.md:229-232`）', () => {
  it('失败原因 / 证据 / 第几次 / 产出 四行都在', () => {
    // 判红方式：删掉任意一行（例如「产出」）→ 红。
    const w = mountPanel()
    for (const field of ['reason', 'evidence', 'attempt', 'output']) {
      expect(w.element.querySelector(`[data-field='${field}']`), `缺 ${field} 行`).not.toBeNull()
    }
    expect(textOf(w)).toContain('失败原因')
    expect(textOf(w)).toContain('证据')
    expect(textOf(w)).toContain('第几次')
    expect(textOf(w)).toContain('产出')
  })

  it('第几行显示「第 N 次 · 上限 M 次」', () => {
    // 判红方式：把执行条数写死成 1 → 红（夹具里是 2 次执行、上限 3）。
    const w = mountPanel()
    const attempt = w.element.querySelector("[data-field='attempt']")?.textContent ?? ''
    expect(attempt).toContain('第 2 次')
    expect(attempt).toContain('上限 3 次')
  })

  it('没有执行记录时不编产出，并说「没有产出」', () => {
    // 判红方式：`outputText` 去掉 `if (!ex)` 分支 → 红（会编一个分支名出来）。
    const w = mountPanel(task({ executions: [] }), null)
    const output = w.element.querySelector("[data-field='output']")?.textContent ?? ''
    expect(output).toContain('没有产出')
  })
})

describe('FailurePanel：证据为 none 时显示「无证据」', () => {
  it("evidence: 'none' → 文本是「无证据」，不含字面 none", () => {
    // 判红方式：去掉 `ev === 'none'` 的那个分支 → 红（会显示字面 `none`）。
    const w = mountPanel(task(), execution({ evidence: 'none' }))
    const row = w.element.querySelector("[data-field='evidence']")?.textContent ?? ''
    expect(row).toContain('无证据')
    expect(row).not.toContain('none')
  })

  it('evidence: null 也显示「无证据」；有证据时原样显示', () => {
    // 与上一条成对：没有这条，「无证据」可以是硬编码的。
    const empty = mountPanel(task(), execution({ evidence: null }))
    expect(empty.element.querySelector("[data-field='evidence']")?.textContent).toContain('无证据')
    const real = mountPanel(task(), execution({ evidence: '3 条断言失败' }))
    expect(real.element.querySelector("[data-field='evidence']")?.textContent).toContain(
      '3 条断言失败',
    )
  })
})

describe('FailurePanel：5 个动作一个都不许少', () => {
  it('换设备重派 / 改任务再跑 / 放宽约束 / 看执行复盘 / 关掉 五个按钮都在', () => {
    // 判红方式：删掉任意一个动作 → 红。里程碑 §4「不允许静默裁剪」：
    // `failed → queued` 这条边就靠「换设备重派」才有 UI 入口。
    const w = mountPanel()
    for (const name of ['retry', 'edit', 'relax', 'replay', 'close']) {
      expect(action(w, name), `缺动作 ${name}`).not.toBeNull()
    }
    const labels = Array.from(w.element.querySelectorAll('button')).map((b) => b.textContent?.trim())
    expect(labels).toEqual(
      expect.arrayContaining(['换设备重派', '改任务再跑', '放宽约束', '看执行复盘', '关掉']),
    )
  })

  it('「看执行复盘」渲染但 disabled，title 说清「执行复盘 v0.8.2 上线」', () => {
    // 判红方式：把 `disabled` 去掉（做成死链接）→ 红；把 title 改掉 → 红。
    const replay = action(mountPanel(), 'replay')
    expect(replay).not.toBeNull()
    expect(replay?.disabled, '复盘页 v0.8.2 才上线，本轮必须 disabled').toBe(true)
    expect(replay?.getAttribute('title')).toBe('执行复盘 v0.8.2 上线')
  })

  it('其余四个动作是可用状态（disabled 只给复盘）', () => {
    // 判红方式：顺手给所有按钮都加 disabled → 红。
    const w = mountPanel()
    for (const name of ['retry', 'edit', 'relax']) {
      expect(action(w, name)?.disabled, `${name} 不该 disabled`).toBe(false)
    }
  })
})

describe('FailurePanel：事件', () => {
  it('换设备重派发 retry（不带参数——它不挑设备）', async () => {
    // 判红方式：把 `emit('retry')` 删掉或改成带参 → 红。
    const w = mountPanel()
    await action(w, 'retry')?.click()
    expect(w.emitted('retry')).toHaveLength(1)
    expect(w.emitted('retry')?.[0]).toEqual([])
  })

  it('放宽约束发 relax', async () => {
    // 判红方式：把 emit 删掉 → 红。
    const w = mountPanel()
    await action(w, 'relax')?.click()
    expect(w.emitted('relax')).toHaveLength(1)
  })

  it('改任务再跑发 edit，带原因', async () => {
    // 判红方式：把 input 的 v-model 去掉 → 红（reason 恒为空串）。
    const w = mountPanel()
    const input = w.element.querySelector<HTMLInputElement>("input[aria-label='改任务再跑的原因']")
    expect(input).not.toBeNull()
    if (input) {
      input.value = '判定太严，改成 build_passes'
      input.dispatchEvent(new Event('input'))
    }
    await action(w, 'edit')?.click()
    expect(w.emitted('edit')).toEqual([['判定太严，改成 build_passes']])
  })
})

describe('FailurePanel：没写原因的「关掉」按不下去', () => {
  it('reason 空 → 关掉按钮 disabled；写了 → enabled 且发 close 带原因', async () => {
    // 判红方式：去掉 `closeDisabled` 那层 → 红（`task-model.md:244`
    // 要求留痕，没有原因的关闭会把失败变成「消失的失败」）。
    const w = mountPanel()
    const close = action(w, 'close')
    expect(close?.disabled, '没写原因时关掉必须 disabled').toBe(true)

    const input = w.element.querySelector<HTMLInputElement>("input[aria-label='关掉这个任务的原因']")
    expect(input).not.toBeNull()
    if (input) {
      input.value = '这件事不做了'
      input.dispatchEvent(new Event('input'))
    }
    await Promise.resolve()
    expect(action(w, 'close')?.disabled, '写了原因应当可用').toBe(false)
    await action(w, 'close')?.click()
    expect(w.emitted('close')).toEqual([['这件事不做了']])
  })

  it('只填空格也算没写（trim 之后为空）', async () => {
    // 判红方式：把 `trim().length === 0` 改成 `length === 0` → 红。
    const w = mountPanel()
    const input = w.element.querySelector<HTMLInputElement>("input[aria-label='关掉这个任务的原因']")
    if (input) {
      input.value = '   '
      input.dispatchEvent(new Event('input'))
    }
    await Promise.resolve()
    expect(action(w, 'close')?.disabled).toBe(true)
  })
})

describe('FailurePanel：八种失败形态都有诊断人话', () => {
  const KINDS: FailureKind[] = [
    'baseline_mismatch',
    'env_broken',
    'disk_exhausted',
    'agent_error',
    'task_failed',
    'interrupted',
    'rejected',
    'superseded',
  ]

  it('八个键逐个有中文诊断，且都不等于原值', () => {
    // 判红方式：漏一条（例如 `superseded`）→ 红。
    expect(Object.keys(FAILURE_DIAGNOSIS).sort()).toEqual([...KINDS].sort())
    for (const kind of KINDS) {
      const text = FAILURE_DIAGNOSIS[kind]
      expect(text, `${kind} 没有诊断`).toBeTruthy()
      expect(text).not.toBe(kind)
      expect(text, `${kind} 的诊断里没有中文`).toMatch(/[\u4e00-\u9fa5]/)
    }
  })

  it('未知 failure_kind → 「未知原因」，不原样吐英文', () => {
    // 判红方式：去掉 `?? '未知原因'` → 红。
    const w = mountPanel(task(), execution({ failure_kind: 'brand_new_kind' }))
    const row = w.element.querySelector("[data-field='reason']")?.textContent ?? ''
    expect(row).toContain('未知原因')
    expect(textOf(w)).not.toContain('brand_new_kind 的')
  })
})

describe('FailurePanel：组件纪律（源码级）', () => {
  it('不发请求、不持定时器、不读 Date.now()', () => {
    // 判红方式：在组件里加一次 fetch / 一个 setInterval / 一个 Date.now() → 红。
    // 剥掉注释再断言：注释里说明这条纪律本身含有这些字样。
    const raw = readFileSync(resolve(HERE, 'FailurePanel.vue'), 'utf-8')
    const code = raw
      .replace(/\/\*[\s\S]*?\*\//g, '')
      .split('\n')
      .map((line) => {
        const at = line.indexOf('//')
        return at >= 0 ? line.slice(0, at) : line
      })
      .join('\n')
    expect(code.match(/fetch|request\(|https?:/g) ?? []).toHaveLength(0)
    expect(code.match(/setInterval|setTimeout/g) ?? []).toHaveLength(0)
    expect(code.match(/Date\.now\(\)/g) ?? []).toHaveLength(0)
  })

  it('相对时间也走 now prop（now 变了「多久之前」跟着变）', () => {
    // 判红方式：把 `relativeTime(at, props.now)` 改成 `relativeTime(at)` → 红。
    const t = task()
    const ex = execution({ ended_at: `unix:${STARTED_SECS}` })
    const near = mountIn(FailurePanel, { props: { task: t, execution: ex, now: (STARTED_SECS + 60) * 1000 } })
    const far = mountIn(FailurePanel, {
      props: { task: t, execution: ex, now: (STARTED_SECS + 600) * 1000 },
    })
    expect(textOf(near)).toContain('1 分钟前')
    expect(textOf(far)).toContain('10 分钟前')
  })
})
