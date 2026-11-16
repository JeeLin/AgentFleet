/* ===========================================================================
   TaskCard 组件测试
   ---------------------------------------------------------------------------
   这一批用例对应里程碑 §4「测试标准」的 2 / 3 / 4 / 5 / 6 / 7 / 8 / 9 / 10 / 15。
   每条都写了**判红方式**——不是格式要求，是因为「这条测试其实永远不会红」
   是这个仓库里已经踩过的坑（见 `state.test.ts` 里的同类注释）。

   夹具一律显式给全字段：`task()` 的缺省值是**待办 + 无判定码**，任何要测
   `running` / `failed` 的用例都必须自己把 `state` 与 `execution` 摆好，
   否则会测到一条走不到的分支（里程碑 §4 对测试 7 / 10 特别点了这一条）。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import TaskCard, { FAILURE_TEXT, formatElapsed } from '@/domains/project/TaskCard.vue'
import type { Execution, FailureKind, Task } from '@/domains/project/taskTypes'
import { mountIn } from '@/test/mount'

const HERE = dirname(fileURLToPath(import.meta.url))
const COMPONENTS_CSS = resolve(HERE, '../../styles/components.css')
const STATE_TS = resolve(HERE, '../../shared/state.ts')

/** 固定时刻：`started_at` 用 `unix:` 秒（与 `format.ts:parseStamp` 同一种格式）。 */
const STARTED_SECS = 1_700_000_000
const STARTED_MS = STARTED_SECS * 1000
/** 3 分 12 秒之后。 */
const NOW_3M12S = STARTED_MS + (3 * 60 + 12) * 1000

/** 一条执行记录。 */
function execution(over: Partial<Execution> = {}): Execution {
  return {
    id: 'exe_1',
    state: 'failed',
    base_commit: 'abc1234def',
    agent_id: 'claude',
    failure_kind: null,
    reason: null,
    evidence: null,
    started_at: `unix:${STARTED_SECS}`,
    ended_at: `unix:${STARTED_SECS + 192}`,
    ...over,
  }
}

/** 一个任务。缺省是**待办 + 无判定码**（于是 `needs_manual_acceptance` 为真）。 */
function task(over: Partial<Task> = {}): Task {
  return {
    id: 'tsk_1',
    project_id: 'prj_1',
    title: '把构建时间降到 60s 内',
    state: 'pending',
    done_when: { criteria: null, description: '构建时间肉眼可感地变短' },
    needs_manual_acceptance: true,
    criteria_known: true,
    created_by: 'user',
    base_commit: 'abc1234def',
    created_at: `unix:${STARTED_SECS}`,
    updated_at: `unix:${STARTED_SECS}`,
    constraints: {
      allowed_devices: null,
      allowed_agents: null,
      required_capabilities: null,
      retry_limit: 2,
    },
    version: 1,
    superseded_by: null,
    intent_ref: null,
    executions: [],
    ...over,
  }
}

function mountCard(t: Task, now: number, ex: Execution | null = t.executions?.[0] ?? null) {
  return mountIn(TaskCard, { props: { task: t, now, execution: ex } })
}

/** 整张卡的文本（含隐藏元素里的，断言「不出现某个词」时才够狠）。 */
function textOf(w: { element: HTMLElement }): string {
  return w.element.textContent ?? ''
}

/**
 * 剥掉组件源码里的注释（`//` 与 `/* … *\/` 两种）。
 *
 * 源码级断言（不许 `Date.now()` / 不许 `setInterval`）看的是**代码**，
 * 不是注释——注释里写「本组件不读 Date.now」当然会命中那串字符，
 * 若把注释也算进去，这条门就变成「别在注释里提它」，与约束本身无关。
 *
 * 只做行级粗剥：这些断言的目标串不会出现在字符串字面量里（组件不发请求、
 * 不拼这些词），所以不需要一个真正的词法器。
 */
function stripComments(src: string): string {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .split('\n')
    .map((line) => {
      const at = line.indexOf('//')
      return at >= 0 ? line.slice(0, at) : line
    })
    .join('\n')
}

describe('formatElapsed：把毫秒翻成人话，且绝不产出百分比', () => {
  it('秒 / 分 / 时 / 天 四档都给人话', () => {
    // 判红方式：把任一档的单位写错（例如把 `3600000` 写成「60 分」）→ 红。
    expect(formatElapsed(0)).toBe('0 秒')
    expect(formatElapsed(59_000)).toBe('59 秒')
    expect(formatElapsed(192_000)).toBe('3 分 12 秒')
    expect(formatElapsed(3_600_000)).toBe('1 小时')
    expect(formatElapsed(90_000_000)).toBe('1 天 1 小时')
  })

  it('返回值里不出现百分号（进度不许编，`PRODUCT.md:328-330`）', () => {
    // 判红方式：把 `formatElapsed` 改成返回 `${pct}%` → 红。
    for (const ms of [0, 1_000, 59_000, 3_600_000, 90_000_000]) {
      expect(formatElapsed(ms), `formatElapsed(${ms}) 里出现了百分号`).not.toMatch(/\d+\s*%/)
      expect(formatElapsed(ms)).not.toContain('%')
    }
  })

  it('负数按 0 处理（设备时钟比 Hub 快时不显示「-3 秒」）', () => {
    // 判红方式：去掉 `Math.max(0, …)` → 红。
    expect(formatElapsed(-5_000)).toBe('0 秒')
  })
})

describe('TaskCard：每态渲染哪些字段（里程碑 §4 的显式表）', () => {
  it('running 卡的已跑时长只来自 now prop：改 now → 文本变', () => {
    // 夹具**必须**是 `state:'running'` + 固定 `started_at`：里程碑 §4 只给
    // running 卡渲染已跑时长，不给这个前置就测不到。
    const t = task({ state: 'running', executions: [execution({ state: 'running' })] })
    const a = mountCard(t, NOW_3M12S)
    expect(textOf(a)).toContain('已跑 3 分 12 秒')

    // **判红方式②（行为级）**：把 `now` 换成一个别的时刻，文本必须变。
    // 若实现里偷偷用了 `Date.now()` 或 `startedAt` 当「现在」，这条就红。
    const b = mountCard(t, NOW_3M12S + 60_000)
    expect(textOf(b)).toContain('已跑 4 分 12 秒')
    expect(textOf(a), '两个 now 必须给出不同文本').not.toBe(textOf(b))
  })

  it('非 running 的卡不渲染已跑时长（now 只被 running 消费）', () => {
    // 判红方式：把 `v-if="isRunning"` 去掉 → 这条红。
    for (const state of ['pending', 'queued', 'review', 'done']) {
      const w = mountCard(task({ state, executions: [execution()] }), NOW_3M12S)
      expect(textOf(w), `${state} 卡不该显示已跑时长`).not.toContain('已跑')
    }
  })

  it('failed 卡渲染失败人话，running 卡不渲染', () => {
    // 判红方式：把失败行的 `v-if="isFailed"` 去掉 → 红（所有卡都长出一行红字）。
    const failed = mountCard(
      task({ state: 'failed', executions: [execution({ failure_kind: 'task_failed' })] }),
      NOW_3M12S,
    )
    expect(textOf(failed)).toContain(FAILURE_TEXT.task_failed)

    const running = mountCard(
      task({ state: 'running', executions: [execution({ state: 'running', failure_kind: 'task_failed' })] }),
      NOW_3M12S,
    )
    expect(textOf(running), 'running 卡不该出现失败人话').not.toContain(FAILURE_TEXT.task_failed)
  })

  it('「需人工验收」标签按 needs_manual_acceptance 出现', () => {
    const yes = mountCard(task({ state: 'review', needs_manual_acceptance: true }), NOW_3M12S)
    expect(textOf(yes)).toContain('需人工验收')
    const no = mountCard(task({ state: 'review', needs_manual_acceptance: false }), NOW_3M12S)
    expect(textOf(no)).not.toContain('需人工验收')
  })
})

describe('TaskCard：假的失败不显示「第 N 次」', () => {
  it('superseded 不显示次数（它不占重试，`task-model.md:138`）', () => {
    // 夹具必须是 `state:'failed'` + `failure_kind:'superseded'` + **两次执行**：
    // 若写成 `state:'running'`，这条在正确实现上恒真（本来就不显示次数）。
    //
    // 判红方式：去掉 `failure_kind === 'superseded'` 的跳过 → 红。
    const ex = [
      execution({ id: 'exe_1', failure_kind: 'task_failed' }),
      execution({ id: 'exe_2', failure_kind: 'superseded' }),
    ]
    const w = mountCard(task({ state: 'failed', executions: ex }), NOW_3M12S, ex[1] as Execution)
    expect(textOf(w)).toContain(FAILURE_TEXT.superseded)
    expect(textOf(w), 'superseded 不该出现「第 2 次」').not.toContain('第 2 次')
    expect(textOf(w), 'superseded 不该出现「第 1 次」').not.toContain('第 1 次')
    expect(textOf(w)).not.toContain('次')
  })

  it('普通失败的卡显示「第 2 次」', () => {
    // 与上一条成对：没有这条，上一条可以靠「永远不显示次数」蒙混过关。
    const ex = [
      execution({ id: 'exe_1', failure_kind: 'env_broken' }),
      execution({ id: 'exe_2', failure_kind: 'task_failed' }),
    ]
    const w = mountCard(task({ state: 'failed', executions: ex }), NOW_3M12S, ex[1] as Execution)
    expect(textOf(w)).toContain('第 2 次')
  })
})

describe('TaskCard：八种 failure_kind 都有人话', () => {
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

  it('八个键逐个有中文人话，且都不等于原值', () => {
    // 判红方式：只映射 7 条（漏 `superseded`）→ 红。
    expect(Object.keys(FAILURE_TEXT).sort()).toEqual([...KINDS].sort())
    for (const kind of KINDS) {
      const text = FAILURE_TEXT[kind]
      expect(text, `${kind} 没有中文人话`).toBeTruthy()
      expect(text, `${kind} 的人话是原样英文`).not.toBe(kind)
      // 人话里至少有一个汉字（防止有人把英文注释当人话塞进去）。
      expect(text, `${kind} 的人话里没有中文`).toMatch(/[\u4e00-\u9fa5]/)
    }
  })

  it('表驱动的八条都在卡上渲染出来', () => {
    for (const kind of KINDS) {
      const t = task({ state: 'failed', executions: [execution({ failure_kind: kind })] })
      expect(textOf(mountCard(t, NOW_3M12S)), `${kind} 的卡上没有人话`).toContain(
        FAILURE_TEXT[kind],
      )
    }
  })

  it('未知的 failure_kind 显示「未知原因」，不原样吐英文', () => {
    // 判红方式：把 `?? '未知原因'` 去掉（直接索引表）→ 红。
    const t = task({ state: 'failed', executions: [execution({ failure_kind: 'some_new_kind' })] })
    const text = textOf(mountCard(t, NOW_3M12S))
    expect(text).toContain('未知原因')
    expect(text).not.toContain('some_new_kind')
  })
})

describe('TaskCard：绝不显示百分比', () => {
  it('六种态的整卡 HTML 都不匹配 /\\d+\\s*%/（`PRODUCT.md:328-330` 守卫）', () => {
    // 判红方式：卡上出现「已完成 40%」之类的文本 → 红。
    const states = ['pending', 'queued', 'running', 'review', 'done', 'failed']
    for (const state of states) {
      const w = mountCard(
        task({ state, executions: [execution({ state, failure_kind: 'task_failed' })] }),
        NOW_3M12S,
      )
      expect(w.element.innerHTML, `${state} 卡的 HTML 里出现了百分比`).not.toMatch(/\d+\s*%/)
      expect(textOf(w)).not.toContain('%')
    }
  })
})

describe('TaskCard：组件纪律（源码级）', () => {
  it('源码里不出现 Date.now()（时钟唯一来源是页面）', () => {
    // 判红方式：把 prop 改回 `now?: number` 并补 `?? Date.now()` → 红。
    // 这是「组件不许自己取时间」的第一道门（第二道是行为级的 now 变化用例）。
    //
    // 断言前先按行剥掉注释：源码里**说明**「不许出现 Date.now()」的注释
    // 本身当然含这串字符。不剥的话这条门只能靠「别写那句注释」才能绿——
    // 那是让注释替实现背锅，正是要避免的那种假绿。
    const code = stripComments(readFileSync(resolve(HERE, 'TaskCard.vue'), 'utf-8'))
    expect(code.match(/Date\.now\(\)/g) ?? []).toHaveLength(0)
  })

  it('不发请求（fetch / request( / http 出现次数为 0）', () => {
    // 判红方式：在组件里发一次请求 → 红。
    const code = stripComments(readFileSync(resolve(HERE, 'TaskCard.vue'), 'utf-8'))
    expect(code.match(/fetch|request\(|https?:/g) ?? []).toHaveLength(0)
  })

  it('不持定时器（setInterval / setTimeout 出现次数为 0）', () => {
    // 判红方式：加一个自动刷新的 setInterval → 红。
    const code = stripComments(readFileSync(resolve(HERE, 'TaskCard.vue'), 'utf-8'))
    expect(code.match(/setInterval|setTimeout/g) ?? []).toHaveLength(0)
  })

  it('点卡片发 open 事件，带上任务 id（不自己跳转）', async () => {
    // 判红方式：把 `emit('open', …)` 删掉 → 红。
    const w = mountCard(task({ id: 'tsk_42' }), NOW_3M12S)
    await w.trigger('click')
    expect(w.emitted('open')).toEqual([['tsk_42']])
  })
})

describe('组件与新增样式只使用已有 token', () => {
  it('三个组件与 components.css 里的 --af- 变量都在 tokens.css 或主题文件里定义过', () => {
    // 判红方式：写一个哪都没有的 `--af-task-xxx` → 红（C5 的补充）。
    //
    // **定义侧要看主题文件**（v0.8.1）：颜色已从 `tokens.css` 搬进
    // `theme-*.css`，只扫 `tokens.css` 会把每一个颜色引用都判成没定义。
    const defined = new Set<string>()
    for (const file of ['tokens.css', 'theme-neutral.css', 'theme-warm.css', 'theme-paper.css', 'theme-cream.css']) {
      const css = readFileSync(resolve(HERE, '../../styles', file), 'utf-8')
      for (const m of css.matchAll(/(--af-[a-z0-9-]+)\s*:/g)) defined.add(m[1] as string)
    }
    const sources = [
      readFileSync(resolve(HERE, 'TaskCard.vue'), 'utf-8'),
      readFileSync(resolve(HERE, 'FailurePanel.vue'), 'utf-8'),
      readFileSync(resolve(HERE, 'AcceptancePanel.vue'), 'utf-8'),
      readFileSync(COMPONENTS_CSS, 'utf-8'),
    ]
    const used = new Set(
      sources.flatMap((s) => [...s.matchAll(/var\((--af-[a-z0-9-]+)\)/g)].map((m) => m[1] as string)),
    )
    const missing = [...used].filter((name) => !defined.has(name))
    expect(missing, `这些 token 在 tokens.css 和主题文件里都没定义：${missing.join(', ')}`).toEqual([])
  })

  it('任务六态的字形字符只来自 shared/state.ts 的词表（卡片不自己写字形）', () => {
    // 判红方式：在卡片里硬写一个 `●` → 红（那是第二份词表，词表会名存实亡）。
    const src = readFileSync(resolve(HERE, 'TaskCard.vue'), 'utf-8')
    const stateSrc = readFileSync(STATE_TS, 'utf-8')
    expect(stateSrc).toContain("running: { state: 'running', glyph: '●'")
    for (const glyph of ['●', '◐', '○', '◔', '◑', '✓', '✕', '◻']) {
      expect(src.includes(`>${glyph}<`), `卡片源码里硬写了字形 ${glyph}`).toBe(false)
    }
  })

  it('只读 props 不写 props（组件不改传进来的任务）', () => {
    // 判红方式：在组件里写 `props.task.state = 'done'` → 红。
    const src = readFileSync(resolve(HERE, 'TaskCard.vue'), 'utf-8')
    expect(src).not.toMatch(/props\.task\.[a-z_]+ *=/)
  })
})

describe('TaskCard：相对时间也来自 now prop', () => {
  it('同一个任务，now 越晚，「N 分钟前」越大', () => {
    // 判红方式：把 `relativeTime(…, props.now)` 改成 `relativeTime(…)`（默认取
    // `Date.now()`）→ 两次渲染文本相同，这条红。
    const t = task({ state: 'done', updated_at: `unix:${STARTED_SECS}` })
    const near = textOf(mountCard(t, STARTED_MS + 60_000))
    const far = textOf(mountCard(t, STARTED_MS + 10 * 60_000))
    expect(near).toContain('1 分钟前')
    expect(far).toContain('10 分钟前')
  })
})
