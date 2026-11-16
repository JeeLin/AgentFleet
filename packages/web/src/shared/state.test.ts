import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import {
  DEVICE_STATUSES,
  GLYPH_SHARING_GROUPS,
  STATES,
  TASK_STATUSES,
  describeStatus,
} from './state'
import type { AnyStatus, GlyphFill } from './state'
import { TASK_STATE_WIRE } from '@/domains/project/taskTypes'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../..')
const COMPONENTS_CSS = resolve(REPO_ROOT, 'packages/web/src/styles/components.css')

/** `components.css` §5 给 `data-state` 上的全部取值。 */
function cssStates(): string[] {
  const css = readFileSync(COMPONENTS_CSS, 'utf-8')
  return [...css.matchAll(/\.af-state\[data-state='([a-z]+)'\]/g)].map((m) => m[1] as string)
}

describe('态表与 components.css §5 一一对应', () => {
  it('表里的每个态都在 CSS 里有颜色规则', () => {
    const inCss = new Set(cssStates())
    for (const state of Object.keys(STATES)) {
      expect(inCss.has(state), `STATE 有 ${state}，.af-state[data-state] 没给它上色`).toBe(true)
    }
  })

  it('CSS 里的每个态都在表里有（CSS 多一个值就是一处永远走不到的死规则）', () => {
    // 例外：`building` / `readonly` 是 §3.4 表格里没有、也没有任何 `stateHTML`
    // 会产生的值——照 §3.4 的表格实现，它们永远走不到。这条断言把这件事
    // 钉住，等哪天组件真的用到它们，本测试会红着提醒补进表里。
    const unused = new Set(['building', 'readonly'])
    for (const state of cssStates()) {
      if (unused.has(state)) continue
      expect(STATES[state as AnyStatus], `components.css 有 ${state}，STATES 里没有`).toBeDefined()
    }
    expect(Object.keys(STATES)).not.toContain('building')
    expect(Object.keys(STATES)).not.toContain('readonly')
  })

  it('十三个态就是规范 §3.4 表格里的七态 + 任务六态', () => {
    // 判红方式：漏一个任务态 → 这条红；反过来，如果只改了这条而没往
    // `STATES` 里补，`Object.keys` 也会少一个。这也是本条断言的用处：
    // 它是「词表 == 规格」的单一断言，别的用例都从 `STATES` 派生。
    //
    // 为什么从「七个态」改成「十三个态」：v0.8.0 把任务六态并进了同一张表
    // （里程碑 §4 —— 任务态与设备/会话态共用 `describeStatus` → `StateBadge`，
    // 不另造第二个徽标）。**不是**放宽门禁：`unused` 排除集一个字没动。
    expect(Object.keys(STATES).sort()).toEqual(
      [
        'active',
        'disconnected',
        'ended',
        'offline',
        'online',
        'reconnecting',
        'unknown',
        'pending',
        'queued',
        'running',
        'review',
        'done',
        'failed',
      ].sort(),
    )
    expect(Object.keys(STATES)).toHaveLength(13)
  })
})

/**
 * 字形复用白名单的**依据在 `state.ts` 里**（`GLYPH_SHARING_GROUPS`），
 * 测试只引用它，不另抄一份：抄两份的话「改了实现忘了改测试」会变成
 * 「两边都错但互相印证」。
 *
 * 白名单里为什么有这几组（`docs/design-system.md` §3.4 + 里程碑 §4）：
 * - `●`：`online` / `active` / `running` —— 「绿实心 = 活的」；设备态与
 *   任务态不同屏，里程碑 §4 明写这是允许的那一处复用；
 * - `◐`：`reconnecting` / `disconnected` —— 会话态与设备态同形，§3.4 既有；
 * - `○`：`offline` / `ended` —— 同上（都是「已经不在」）。
 */

describe('四通道：颜色 + 字形 + 填充 + 文字', () => {
  it('每个态都带文字（文字是主通道，颜色与字形只是备份）', () => {
    for (const [name, spec] of Object.entries(STATES)) {
      expect(spec.label, `${name} 没有文字`).toBeTruthy()
    }
  })

  it('字形只按白名单复用：白名单之外每个字形只属于一个态', () => {
    // 判红方式：给 `done` 也用 `●` → 红（`●` 已由三态共用，第四处复用会让
    // 「绿实心」失去单义性）；给 `review` 用 `◐` → 红（会并进
    // reconnecting / disconnected 那组）。
    const owners = new Map<string, string[]>()
    for (const [name, spec] of Object.entries(STATES)) {
      expect(spec.glyph.length, `${name} 的字形为空`).toBeGreaterThan(0)
      owners.set(spec.glyph, [...(owners.get(spec.glyph) ?? []), name])
    }
    expect(owners.size).toBeGreaterThanOrEqual(3)
    for (const [glyph, states] of owners) {
      const group =
        GLYPH_SHARING_GROUPS.find((g) => g.includes(states[0] as AnyStatus)) ?? [
          states[0] as AnyStatus,
        ]
      expect(states.sort(), `字形 ${glyph} 的归属态不在白名单里`).toEqual([...group].sort())
    }
  })

  it('白名单本身是对的：每个组里所有态的字形确实相同，且组外没有态用这个字形', () => {
    // 判红方式：往白名单里塞一个「其实不同形」的组 → 红（白名单不是免死金牌）。
    for (const group of GLYPH_SHARING_GROUPS) {
      const glyphs = new Set(group.map((s) => STATES[s].glyph))
      expect(glyphs.size, `白名单组 ${group.join('/')} 的字形并不相同`).toBe(1)
      const glyph = group[0] ? STATES[group[0]].glyph : ''
      const actual = Object.entries(STATES)
        .filter(([, spec]) => spec.glyph === glyph)
        .map(([name]) => name)
      expect(actual.sort(), `字形 ${glyph} 的实际归属与白名单不符`).toEqual([...group].sort())
    }
  })

  it('把 13 个态摊平成表：排除三处白名单复用后，其余 6 个字形互不相同', () => {
    // 判红方式（两条，都是真的能红）：
    //   ① 给 `review` 也用 `◐` → 红（`◐` 已属 reconnecting / disconnected）；
    //   ② 给 `done` 改用 `●` → 红（`●` 已被三态占用）。
    //
    // 刻意**不**写成「新增五态与既有七态不等」：那种断言恒真
    // （`✓` / `✕` / `◔` / `◑` / `◻` 与 `●◐○` 天生不同族），永远不会红，等于没测。
    const flat = Object.entries(STATES).map(([name, spec]) => ({ state: name, glyph: spec.glyph }))
    expect(flat).toHaveLength(13)
    const allowed = new Set<AnyStatus>(GLYPH_SHARING_GROUPS.flat())
    const lone = flat.filter((e) => !allowed.has(e.state as AnyStatus))
    // 白名单里的 7 个态（3+2+2）之外，剩 6 个各占一个字形。
    expect(lone, '白名单内的态是 3+2+2=7 个，白名单外 6 个').toHaveLength(6)
    expect(new Set(lone.map((e) => e.glyph)).size, '白名单外的 6 态字形互不相同').toBe(lone.length)
    // 13 个态 → 9 个不同字形：3 组复用各贡献一个字形，其余 6 个各占一个。
    const uniqueGlyphs = new Set(flat.map((e) => e.glyph))
    expect(uniqueGlyphs.size, '字形去重后应当是 9 个').toBe(
      6 + GLYPH_SHARING_GROUPS.length,
    )
  })

  it('填充通道：CSS 只给 filled 上规则，其余两种靠字形本身', () => {
    const css = readFileSync(COMPONENTS_CSS, 'utf-8')
    const styled = [...css.matchAll(/\[data-glyph='([a-z]+)'\]/g)].map((m) => m[1] as string)
    // §5 只写了一条 `font-style: normal`（实心圆在衬线 fallback 下别画斜）：
    // `◐` 天生半轮、`○` / `◌` 天生空心或虚点，它们不需要额外规则。
    // 这条断言的作用是：哪天有人给 partial / hollow 也加一条，先想清楚
    // 它改的是哪个通道——别让「四通道」悄悄塌成「颜色 + 字形」两通道。
    expect(styled).toEqual(['filled'])
    const vocab: readonly GlyphFill[] = ['filled', 'partial', 'hollow']
    for (const [name, spec] of Object.entries(STATES)) {
      expect(vocab, `${name} 的填充 ${spec.fill} 不在词表里`).toContain(spec.fill)
    }
  })

  it('在线与运行中同形，运行中与断线同形——绿/琥珀在两条轴上复用', () => {
    expect(STATES.online.glyph).toBe(STATES.active.glyph)
    expect(STATES.reconnecting.glyph).toBe(STATES.disconnected.glyph)
  })

  it('unknown 与 offline 严格区分（规范 §5.3：不知道 ≠ 知道它不在）', () => {
    expect(STATES.unknown.glyph).not.toBe(STATES.offline.glyph)
    expect(STATES.unknown.label).not.toBe(STATES.offline.label)
    expect(STATES.unknown.fill).not.toBe(STATES.offline.fill)
  })
})

describe('describeStatus', () => {
  it('表内的态原样返回', () => {
    expect(describeStatus('offline')).toBe(STATES.offline)
  })

  it('表外的值原样回显而不是抛错（服务端加了新状态不能让整个列表渲染不出来）', () => {
    const spec = describeStatus('quiescing')
    expect(spec.state).toBe('quiescing')
    expect(spec.label).toBe('quiescing')
    expect(spec.glyph).toBe('◌')
  })

  it('空串也走回显分支，不抛', () => {
    expect(() => describeStatus('')).not.toThrow()
  })
})

describe('DEVICE_STATUSES', () => {
  it('恰好是设备侧的四个态，且与 STATES 对得上', () => {
    expect([...DEVICE_STATUSES].sort()).toEqual(
      ['offline', 'online', 'reconnecting', 'unknown'].sort(),
    )
    for (const s of DEVICE_STATUSES) expect(STATES[s]).toBeDefined()
  })

  it('不含会话态（会话态不是设备态，混进来会让契约测试放行非法值）', () => {
    for (const s of DEVICE_STATUSES) {
      expect(['active', 'disconnected', 'ended']).not.toContain(s)
    }
  })
})

describe('TASK_STATUSES', () => {
  it('恰好是任务侧六个态，与 STATES 对得上，顺序照 spec 的列序', () => {
    // 判红方式：漏写 `TASK_STATUSES` 里的一项（例如 `review`）→ 红；
    // 或把顺序打乱 → 红（看板的列按这个序排，顺序是契约不是巧合）。
    expect([...TASK_STATUSES]).toEqual(['pending', 'queued', 'running', 'review', 'done', 'failed'])
    for (const s of TASK_STATUSES) expect(STATES[s]).toBeDefined()
  })

  it('不含设备态与会话态（混进来会让看板分列多出永远空的列）', () => {
    const notTask = ['online', 'reconnecting', 'offline', 'unknown', 'active', 'disconnected', 'ended']
    for (const s of TASK_STATUSES) expect(notTask).not.toContain(s)
  })

  it('六个任务态都有中文标签（文字是主通道）', () => {
    // 判红方式：把某个 label 改成英文或空串 → 红。
    const labels = TASK_STATUSES.map((s) => STATES[s].label)
    expect(labels).toEqual(['待办', '排队中', '执行中', '待验收', '已完成', '失败'])
  })

  it('线格式数组 `TASK_STATE_WIRE` 与 `TASK_STATUSES` 是同一组值', () => {
    // 为什么这两处会分叉：`state.ts` 的 `TaskStatus` 是 **UI 词表**（给
    // `StateBadge` 查表用），`taskTypes.ts` 的 `TaskStateWire` 是 **Hub 返回的
    // JSON 里的字符串**。分层不同所以类型分开写，但**值必须同源**——后端加一个
    // 态而这两处有一处没跟上，就会出现「看板认、API 不认」或反之。
    //
    // 这条比对的是 `taskTypes.ts` 导出的 `TASK_STATE_WIRE`（类型约束为
    // `readonly TaskStateWire[]`），不是测试里另抄一份字面量——抄一份的话
    // 被测的是测试自己。
    //
    // 判红方式：只改 `taskTypes.ts` 那份（例如把 `review` 写成 `reviewing`，
    // 需同步改类型否则 `type-check` 先红）→ 红。
    expect([...TASK_STATE_WIRE]).toEqual([...TASK_STATUSES])
  })
})
