import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import { DEVICE_STATUSES, STATES, describeStatus } from './state'
import type { AnyStatus, GlyphFill } from './state'

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

  it('七个态就是规范 §3.4 表格里的那些', () => {
    expect(Object.keys(STATES).sort()).toEqual(
      [
        'active',
        'disconnected',
        'ended',
        'offline',
        'online',
        'reconnecting',
        'unknown',
      ].sort(),
    )
  })
})

describe('四通道：颜色 + 字形 + 填充 + 文字', () => {
  it('每个态都带文字（文字是主通道，颜色与字形只是备份）', () => {
    for (const [name, spec] of Object.entries(STATES)) {
      expect(spec.label, `${name} 没有文字`).toBeTruthy()
    }
  })

  it('每个态都有字形且字形唯一（字形是灰度下的识别通道）', () => {
    const glyphs = new Set<string>()
    for (const [name, spec] of Object.entries(STATES)) {
      expect(spec.glyph.length, `${name} 的字形为空`).toBeGreaterThan(0)
      // online 与 active 允许共用 ●（两者都是 ok 绿实心，规范 §3.4 如此）
      glyphs.add(spec.glyph)
    }
    expect(glyphs.size).toBeGreaterThanOrEqual(3)
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
