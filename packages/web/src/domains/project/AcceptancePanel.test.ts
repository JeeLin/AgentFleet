/* ===========================================================================
   AcceptancePanel 组件测试
   ---------------------------------------------------------------------------
   对应里程碑 §4 的测试 12 / 13，加上「判定码未全绿 → 内联二次确认」
   「没有判定码 → 需人工判定」两条规格。每条都写**判红方式**。

   这个面板**没有「判失败」按钮**：`review → failed` 不是合法边，服务端 409。
   一个按下去只会报错的按钮比没有按钮更坏——用户会以为是自己点错了。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import AcceptancePanel, { CRITERIA_TEXT } from '@/domains/project/AcceptancePanel.vue'
import type { Task } from '@/domains/project/taskTypes'
import { mountIn } from '@/test/mount'

const HERE = dirname(fileURLToPath(import.meta.url))
const STARTED_SECS = 1_700_000_000
const NOW = (STARTED_SECS + 600) * 1000

function task(over: Partial<Task> = {}): Task {
  return {
    id: 'tsk_1042',
    project_id: 'prj_1',
    title: '把构建时间降到 60s 内',
    state: 'review',
    done_when: { criteria: 'all_tests_pass', description: '构建 < 60s' },
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
      retry_limit: 2,
    },
    version: 1,
    superseded_by: null,
    intent_ref: null,
    executions: [],
    ...over,
  }
}

function mountPanel(t: Task = task()) {
  return mountIn(AcceptancePanel, { props: { task: t, now: NOW } })
}

function textOf(w: { element: HTMLElement }): string {
  return w.element.textContent ?? ''
}

function action(w: { element: HTMLElement }, name: string): HTMLButtonElement | null {
  return w.element.querySelector<HTMLButtonElement>(`[data-action='${name}']`)
}

describe('AcceptancePanel：三个动作，没有「判失败」', () => {
  it('合入主干 / 打回并写明原因 / 只保留分支 三个都在', () => {
    // 判红方式：删掉任意一个动作 → 红。
    const w = mountPanel()
    for (const name of ['pass', 'reject', 'keep-branch']) {
      expect(action(w, name), `缺动作 ${name}`).not.toBeNull()
    }
  })

  it('第 4 个动作「查看执行复盘」渲染但 disabled（不静默裁掉）', () => {
    // 规格 task-model.md:275 与原型 task.html:174-177 都是四个动作，本轮只做三个。
    // 判红方式：删掉这个按钮 → 红（用户会以为复盘功能不存在）；
    //           把 disabled 去掉 → 红（会指向 v0.8.2 才有的骨架上）。
    const w = mountPanel()
    const replay = action(w, 'replay') as HTMLButtonElement | null
    expect(replay, '复盘按钮必须渲染').not.toBeNull()
    expect(replay!.disabled, '复盘页在 v0.8.2 仍是骨架，按钮必须 disabled').toBe(true)
    expect(replay!.getAttribute('title')).toContain('v0.8.2')
  })

  it('面板内没有任何「判失败」类按钮', () => {
    // 判红方式：加一个写着「判失败」的按钮 → 红（那条边服务端 409）。
    const w = mountPanel()
    const labels = Array.from(w.element.querySelectorAll('button')).map((b) => b.textContent ?? '')
    expect(labels.length).toBeGreaterThan(0)
    for (const label of labels) {
      expect(label).not.toContain('判失败')
      expect(label).not.toContain('标记失败')
      expect(label).not.toContain('失败')
    }
    // 顺带把整块文本也扫一遍：换个说法（「判为失败」）同样不行。
    expect(textOf(w)).not.toContain('判失败')
  })

  it('「只保留分支」的 tooltip 不撒谎（本轮 = 打回 + 记原因）', () => {
    // 判红方式：把 title 里的 v0.8.2 说明删掉 → 红。规格要求「不撒谎」：
    // 本轮并没有真的保留分支，只是打回 + 记原因。
    const btn = action(mountPanel(), 'keep-branch')
    expect(btn?.getAttribute('title')).toContain('v0.8.2')
    expect(btn?.getAttribute('title')).toContain('打回')
  })
})

describe('AcceptancePanel：判定码只显示「未判定」，不编绿', () => {
  it('有判定码 → 显示人话 + 「未判定」，不给绿点', () => {
    // 判红方式：把「未判定」换成「✓ 已通过」→ 红。本轮没有验收执行器，
    // 编一个绿就是假话（`task-model.md` §5 的硬规则 1 的前提）。
    const w = mountPanel()
    expect(textOf(w)).toContain(CRITERIA_TEXT.all_tests_pass)
    expect(textOf(w)).toContain('未判定')
    const status = w.element.querySelector("[data-criteria-status='unknown']")
    expect(status, '判定码区必须有「未判定」标记').not.toBeNull()
    expect(status?.textContent).not.toContain('✓')
    expect(textOf(w)).not.toContain('已通过')
  })

  it('四个闭集码都有人话；未知码原样显示（不猜）', () => {
    // 判红方式：漏一个码 → 红；把未知码硬翻成某个已知码 → 红。
    expect(Object.keys(CRITERIA_TEXT).sort()).toEqual(
      ['all_tests_pass', 'build_passes', 'review_approved', 'command_exit_zero'].sort(),
    )
    for (const [code, text] of Object.entries(CRITERIA_TEXT)) {
      expect(text, `${code} 没有中文人话`).toMatch(/[\u4e00-\u9fa5]/)
      expect(text).not.toBe(code)
    }

    const unknown = mountPanel(
      task({ done_when: { criteria: 'looks_good', description: null }, criteria_known: false }),
    )
    expect(textOf(unknown)).toContain('looks_good')
    expect(textOf(unknown)).toContain('未知判定码')
  })

  it('criteria 为 null → 显示「本任务无判定码，需人工判定」', () => {
    // 判红方式：把这条分支改回 `''` 或 `null` → 红。
    const w = mountPanel(
      task({
        done_when: { criteria: null, description: '构建时间肉眼可感地变短' },
        needs_manual_acceptance: true,
      }),
    )
    expect(textOf(w)).toContain('本任务无判定码，需人工判定')
    expect(w.element.querySelector("[data-criteria-status='unknown']")).toBeNull()
  })

  it('criteria_known === false → 判定码区不给绿点，且原文可见', () => {
    // 判红方式：不看 `criteria_known` 直接给绿点/正常态 → 红。
    const w = mountPanel(task({ criteria_known: false }))
    expect(textOf(w)).toContain('判定码不在已知集合内')
    const raw = w.element.querySelector("[data-raw-criteria]")
    expect(raw?.textContent, '存储里的原文必须可见').toBe('all_tests_pass')
    expect(w.element.querySelector("[data-criteria-status='unknown']")).not.toBeNull()
  })
})

describe('AcceptancePanel：合入主干要内联二次确认，且必须有 note', () => {
  it('判定未全绿 → 第一下展开确认区，不直接发 pass', async () => {
    // 判红方式：把「先展开确认」那层去掉（第一下就 emit）→ 红：
    // 那就是静默越过判定。
    const w = mountPanel()
    await action(w, 'pass')?.click()
    expect(w.emitted('pass')).toBeUndefined()
    expect(w.element.querySelector("[data-confirm='pass']"), '确认区没出现').not.toBeNull()
    expect(textOf(w)).toContain('判定未通过，仍要合并')
  })

  it('note 空 → 确认键 disabled；写了 note → enabled 且发出 pass 带 note', async () => {
    // 判红方式：把 `passConfirmDisabled` 去掉 → 红（允许静默越过）。
    const w = mountPanel()
    await action(w, 'pass')?.click()
    const confirm = action(w, 'pass-confirm')
    expect(confirm?.disabled, 'note 空时确认键必须 disabled').toBe(true)

    const input = w.element.querySelector<HTMLInputElement>("input[aria-label='越过判定的理由']")
    expect(input).not.toBeNull()
    if (input) {
      input.value = '判定未通过，仍要合并：测试机器缺依赖，本地已验证'
      input.dispatchEvent(new Event('input'))
    }
    await Promise.resolve()
    expect(action(w, 'pass-confirm')?.disabled, '写了 note 应当可用').toBe(false)

    await action(w, 'pass-confirm')?.click()
    expect(w.emitted('pass')).toEqual([['判定未通过，仍要合并：测试机器缺依赖，本地已验证']])
  })

  it('只填空格也算没写（trim 之后为空）', async () => {
    // 判红方式：把 `trim().length === 0` 改成 `length === 0` → 红。
    const w = mountPanel()
    await action(w, 'pass')?.click()
    const input = w.element.querySelector<HTMLInputElement>("input[aria-label='越过判定的理由']")
    if (input) {
      input.value = '    '
      input.dispatchEvent(new Event('input'))
    }
    await Promise.resolve()
    expect(action(w, 'pass-confirm')?.disabled).toBe(true)
  })

  it('取消能收起确认区（一次误点不该把面板卡在确认态）', async () => {
    // 判红方式：把「取消」按钮删掉 → 红。
    const w = mountPanel()
    await action(w, 'pass')?.click()
    await action(w, 'pass-cancel')?.click()
    expect(w.element.querySelector("[data-confirm='pass']")).toBeNull()
  })
})

describe('AcceptancePanel：打回与只保留分支', () => {
  it('打回发 reject，带原因', async () => {
    // 判红方式：把 `emit('reject', …)` 删掉 → 红。
    const w = mountPanel()
    const input = w.element.querySelector<HTMLInputElement>("input[aria-label='验收备注']")
    if (input) {
      input.value = '改了不该改的文件'
      input.dispatchEvent(new Event('input'))
    }
    await action(w, 'reject')?.click()
    expect(w.emitted('reject')).toEqual([['改了不该改的文件']])
  })

  it('只保留分支发 keep-branch（不是 reject，也不是 pass）', async () => {
    // 判红方式：让它发 reject → 红：两个动作在页面上是两件事。
    const w = mountPanel()
    await action(w, 'keep-branch')?.click()
    expect(w.emitted('keep-branch')).toEqual([['']])
    expect(w.emitted('reject')).toBeUndefined()
    expect(w.emitted('pass')).toBeUndefined()
  })
})

describe('AcceptancePanel：组件纪律（源码级）', () => {
  it('不发请求、不持定时器、不读 Date.now()', () => {
    // 判红方式：在组件里加一次 fetch / 一个 setInterval / 一个 Date.now() → 红。
    const raw = readFileSync(resolve(HERE, 'AcceptancePanel.vue'), 'utf-8')
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
})
