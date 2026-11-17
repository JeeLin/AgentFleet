/* ===========================================================================
   主题切换机制的自测（`src/styles/theme.ts`）
   ---------------------------------------------------------------------------
   这一层是**纯逻辑 + 一个 DOM 属性**，所以能真测：不需要 `getComputedStyle`
   （happy-dom 对这不可靠），断言落在「决定用哪套」和「写没写 data-theme」上。
   颜色值本身由 `theme-css.test.ts` 逐值对照原型。
   =========================================================================== */
import { describe, expect, it, beforeEach, vi } from 'vitest'

import {
  DEFAULT_THEME,
  THEMES,
  applyTheme,
  currentTheme,
  followOS,
  setFollowOS,
  setTheme,
  toggleFollowOS,
  type ThemeId,
} from '@/styles/theme'

/** 每个用例一份干净的 localStorage / URL / data-*。 */
function reset(): void {
  try {
    localStorage.clear()
  } catch {
    /* ignore */
  }
  delete document.documentElement.dataset.theme
  delete document.documentElement.dataset.themeMode
  window.history.replaceState(null, '', '/')
}

beforeEach(reset)

describe('四套主题的元数据', () => {
  it('恰好四套：两暗两亮、冷暖各半（原型 swatch.js:31-36）', () => {
    expect(THEMES.map((t) => t.id)).toEqual(['neutral', 'warm', 'paper', 'cream'])
    expect(THEMES.filter((t) => t.mode === '暗').map((t) => t.id)).toEqual(['neutral', 'warm'])
    expect(THEMES.filter((t) => t.tone === '冷').map((t) => t.id)).toEqual(['neutral', 'paper'])
  })

  it('默认是乙 · 暖米灰（用户裁定 2026-11-16，不照抄原型 index.html:8）', () => {
    // 原型首页先引 `theme-neutral.css` 只说明演示顺序；产品默认是 warm。
    // 判红方式：把 DEFAULT_THEME 改回 'neutral' → 红。
    expect(DEFAULT_THEME).toBe('warm')
  })
})

describe('决定用哪套：优先序', () => {
  it('什么都没设时用默认（warm）', () => {
    expect(currentTheme()).toBe('warm')
  })

  it('localStorage 里的选择生效', () => {
    localStorage.setItem('af-theme', 'paper')
    expect(currentTheme()).toBe('paper')
  })

  it('URL ?theme= 压过 localStorage（可分享链接优先）', () => {
    localStorage.setItem('af-theme', 'paper')
    window.history.replaceState(null, '', '/?theme=cream')
    expect(currentTheme()).toBe('cream')
  })

  it('URL 里是垃圾值时不采信，回落默认而不是返回垃圾', () => {
    window.history.replaceState(null, '', '/?theme=neon')
    expect(currentTheme()).toBe('warm')
  })
})

describe('跟随系统', () => {
  it('开启后按 prefers-color-scheme 选明暗', () => {
    setFollowOS(true)
    expect(followOS()).toBe(true)

    const mq = vi.spyOn(window, 'matchMedia').mockImplementation(
      (q: string) => ({ matches: q.includes('dark'), media: q, addEventListener: vi.fn(), removeEventListener: vi.fn() }) as unknown as MediaQueryList,
    )
    // 暗 + 用户未选过 → 按默认主题（warm）的**冷暖**（暖）取暗色 → warm。
    // 上一版这里期望 neutral：那时兜底写死 '冷'，与默认主题的冷暖无关。
    expect(currentTheme()).toBe('warm')

    mq.mockImplementation(
      (q: string) => ({ matches: !q.includes('dark'), media: q, addEventListener: vi.fn(), removeEventListener: vi.fn() }) as unknown as MediaQueryList,
    )
    // 亮 + 同样按暖调 → cream（不是 paper）。
    expect(currentTheme()).toBe('cream')

    mq.mockRestore()
  })

  it('跟随系统时**保留用户上次的冷暖**，只换明暗（原型 swatch.js:57-67）', () => {
    // 用户上次手选暖调（warm 是暗暖）
    localStorage.setItem('af-theme', 'warm')
    setFollowOS(true)
    // 系统变亮 → 应是「亮 + 暖」= cream，不是亮+冷 paper
    const mq = vi.spyOn(window, 'matchMedia').mockImplementation(
      (q: string) => ({ matches: !q.includes('dark'), media: q, addEventListener: vi.fn(), removeEventListener: vi.fn() }) as unknown as MediaQueryList,
    )
    expect(currentTheme()).toBe('cream')
    mq.mockRestore()
  })
})

describe('应用与持久化', () => {
  it('applyTheme 写 data-theme 与 data-theme-mode，但不落盘', () => {
    applyTheme('paper')
    expect(document.documentElement.dataset.theme).toBe('paper')
    expect(document.documentElement.dataset.themeMode).toBe('light')
    expect(localStorage.getItem('af-theme')).toBeNull()
  })

  it('暗色主题的 data-theme-mode 是 dark', () => {
    applyTheme('warm')
    expect(document.documentElement.dataset.themeMode).toBe('dark')
  })

  it('setTheme 持久化并把 ?theme= 写进 URL', () => {
    setTheme('cream')
    expect(localStorage.getItem('af-theme')).toBe('cream')
    expect(new URLSearchParams(location.search).get('theme')).toBe('cream')
    expect(document.documentElement.dataset.theme).toBe('cream')
  })

  it('setTheme 会关掉「跟随系统」——手动选一套就是明确意图', () => {
    setFollowOS(true)
    setTheme('warm')
    expect(followOS()).toBe(false)
  })

  it('toggleFollowOS 返回开关的新状态与据此算出的主题', () => {
    const r = toggleFollowOS()
    expect(r.follow).toBe(true)
    expect(THEMES.some((t) => t.id === r.theme)).toBe(true)
    expect(document.documentElement.dataset.theme).toBe(r.theme)
  })
})

describe('存储不可用时的健壮性（原型 swatch.js:43-51 的理由）', () => {
  it('localStorage.getItem 抛异常时 currentTheme 不炸，回落默认', () => {
    const spy = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('SecurityError: private mode')
    })
    expect(() => currentTheme()).not.toThrow()
    expect(currentTheme()).toBe('warm')
    spy.mockRestore()
  })

  it('localStorage.setItem 抛异常时 setTheme 仍把主题落到 DOM 上', () => {
    const spy = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('QuotaExceededError')
    })
    expect(() => setTheme('paper')).not.toThrow()
    expect(document.documentElement.dataset.theme).toBe('paper')
    spy.mockRestore()
  })
})

describe('未知 id 不让状态坏掉', () => {
  it('applyTheme 收到不存在的 id 时回落到**默认主题**，而不是写出垃圾 data-theme', () => {
    // 兜底从 `THEMES[0]`（清单第一项 neutral）改成 `DEFAULT_THEME`（warm）：
    // 默认改成 warm 之后，「清单第一项」与「默认主题」已经不是同一个东西，
    // 取前者会让「非法 id」和「什么都没设」落到两套不同的配色上。
    const bad = 'neon' as ThemeId
    const applied = applyTheme(bad)
    expect(applied).toBe('warm')
    expect(document.documentElement.dataset.theme).toBe('warm')
  })
})
