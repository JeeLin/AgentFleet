import { describe, expect, it } from 'vitest'

import {
  SPLIT_DEFAULT,
  SPLIT_MAX,
  SPLIT_MIN,
  SPLIT_STEP,
  clampSplit,
  splitFromDrag,
  stepSplit,
} from './split'

describe('clampSplit', () => {
  it('区间内原样返回', () => {
    expect(clampSplit(56)).toBe(56)
    expect(clampSplit(SPLIT_MIN)).toBe(SPLIT_MIN)
    expect(clampSplit(SPLIT_MAX)).toBe(SPLIT_MAX)
  })

  it('越界收到 30–70（拖过头不是把整块面板拖没）', () => {
    expect(clampSplit(0)).toBe(SPLIT_MIN)
    expect(clampSplit(99)).toBe(SPLIT_MAX)
    expect(clampSplit(-40)).toBe(SPLIT_MIN)
  })

  it('非有限值回落默认值，不把 NaN 写进样式', () => {
    expect(clampSplit(Number.NaN)).toBe(SPLIT_DEFAULT)
    expect(clampSplit(Number.POSITIVE_INFINITY)).toBe(SPLIT_MAX)
    expect(clampSplit(Number.NEGATIVE_INFINITY)).toBe(SPLIT_MIN)
  })
})

describe('stepSplit', () => {
  it('← → 一次 4%', () => {
    expect(stepSplit(56, 'left')).toBe(56 - SPLIT_STEP)
    expect(stepSplit(56, 'right')).toBe(56 + SPLIT_STEP)
  })

  it('到头就停在端点', () => {
    expect(stepSplit(SPLIT_MIN, 'left')).toBe(SPLIT_MIN)
    expect(stepSplit(SPLIT_MAX, 'right')).toBe(SPLIT_MAX)
  })

  it('Home / End 到两端', () => {
    expect(stepSplit(56, 'home')).toBe(SPLIT_MIN)
    expect(stepSplit(56, 'end')).toBe(SPLIT_MAX)
  })

  it('端点附近也收进区间（56 + 4 * 5 > 70）', () => {
    expect(stepSplit(68, 'right')).toBe(SPLIT_MAX)
    expect(stepSplit(32, 'left')).toBe(SPLIT_MIN)
  })
})

describe('splitFromDrag', () => {
  it('按容器宽度换算成百分比', () => {
    // 1000px 里拖 100px = 10%
    expect(splitFromDrag(56, 100, 1000)).toBe(66)
    expect(splitFromDrag(56, -100, 1000)).toBe(46)
  })

  it('拖出容器也收进 30–70', () => {
    expect(splitFromDrag(56, 2000, 1000)).toBe(SPLIT_MAX)
    expect(splitFromDrag(56, -2000, 1000)).toBe(SPLIT_MIN)
  })

  it('容器还没量到宽度时不动（不能算出 Infinity）', () => {
    expect(splitFromDrag(56, 100, 0)).toBe(56)
    expect(splitFromDrag(56, 100, Number.NaN)).toBe(56)
  })
})
