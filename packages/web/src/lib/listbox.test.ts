import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { ENTER_EVENT, TOGGLE_EVENT, attach, currentRow, listOf, selectRow } from './listbox'
import { resetForTest } from './focus'

/**
 * 设一个「初始选中行」。
 *
 * 不能用 `row.focus()` 造：focus 会冒泡 focusin，listbox 的 focusin 监听器
 * 立刻会把该行写成 aria-selected——那是产品行为，于是测试里手工 setAttribute
 * 造的初始态会被覆盖。`attach(...).select(row, false)` 只改属性不聚焦。
 */
function preselect(list: HTMLElement, row: HTMLElement): void {
  selectRow(row, false)
  void list
}

/** 一个三行列表：容器是唯一 Tab 落点，行是 tabindex="-1"。 */
function listFixture(opts: { role?: 'list' | 'tree'; wrap?: boolean } = {}): HTMLElement {
  document.body.innerHTML = `
    <button id="outside">外面</button>
    <div id="list" data-role="${opts.role ?? 'list'}" tabindex="0">
      <button data-row id="r1" tabindex="-1">一</button>
      <button data-row id="r2" tabindex="-1">二</button>
      <div><button data-row id="r3" tabindex="-1">三</button></div>
    </div>
  `
  return document.getElementById('list') as HTMLElement
}

function row(id: string): HTMLElement {
  return document.getElementById(id) as HTMLElement
}

function press(target: HTMLElement, key: string, init: KeyboardEventInit = {}): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, cancelable: true, bubbles: true, ...init })
  target.dispatchEvent(e)
  return e
}

beforeEach(() => {
  resetForTest()
  document.body.innerHTML = ''
})

afterEach(() => {
  resetForTest()
  document.body.innerHTML = ''
})

describe('attach：角色', () => {
  it('list 容器补上 role="listbox"', () => {
    const list = listFixture()
    attach(list)
    expect(list.getAttribute('role')).toBe('listbox')
  })

  it('tree 容器补上 role="tree"', () => {
    const list = listFixture({ role: 'tree' })
    attach(list)
    expect(list.getAttribute('role')).toBe('tree')
  })

  it('显式 role 与 DOM 上的 data-role 不一致时以显式为准', () => {
    const list = listFixture({ role: 'list' })
    attach(list, { role: 'tree' })
    expect(list.dataset.role).toBe('tree')
  })
})

describe('方向键', () => {
  it('↓ 从第一行移到下一行并写 aria-selected', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    press(row('r1'), 'ArrowDown')
    expect(document.activeElement).toBe(row('r2'))
    expect(row('r2').getAttribute('aria-selected')).toBe('true')
    expect(row('r1').getAttribute('aria-selected')).toBe('false')
  })

  it('↑ 从第二行回到第一行', () => {
    const list = listFixture()
    attach(list)
    row('r2').focus()
    press(row('r2'), 'ArrowUp')
    expect(document.activeElement).toBe(row('r1'))
  })

  it('j / k 与 ↑↓ 等价（列表容器内）', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    press(row('r1'), 'j')
    expect(document.activeElement).toBe(row('r2'))
    press(row('r2'), 'k')
    expect(document.activeElement).toBe(row('r1'))
  })

  it('到头不绕回（默认）', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    press(row('r1'), 'ArrowUp')
    expect(document.activeElement).toBe(row('r1'))
    row('r3').focus()
    press(row('r3'), 'ArrowDown')
    expect(document.activeElement).toBe(row('r3'))
  })

  it('wrap: true 时到头绕回', () => {
    const list = listFixture()
    attach(list, { wrap: true })
    row('r1').focus()
    press(row('r1'), 'ArrowUp')
    expect(document.activeElement).toBe(row('r3'))
    press(row('r3'), 'ArrowDown')
    expect(document.activeElement).toBe(row('r1'))
  })

  it('容器刚获得焦点时先落到行上，随后 ↑↓ 相对当前行走', () => {
    // 方向键从容器直接起跳的路径不成立：focusin 已把焦点交给当前/第一行，
    // 所以焦点永远不会停在容器上「等一个 ↓」。这条断言的是实际顺序。
    const list = listFixture()
    attach(list)
    list.focus()
    expect(document.activeElement).toBe(row('r1'))
    press(row('r1'), 'ArrowDown')
    expect(document.activeElement).toBe(row('r2'))
    press(row('r2'), 'ArrowUp')
    expect(document.activeElement).toBe(row('r1'))
  })

  it('焦点不在列表里时方向键不动（不劫持全站 ↑↓）', () => {
    const list = listFixture()
    attach(list)
    const outside = row('outside')
    outside.focus()
    const e = press(outside, 'ArrowDown')
    expect(document.activeElement).toBe(outside)
    expect(e.defaultPrevented).toBe(false)
  })

  it('按键被阻止默认（不让浏览器滚页面）', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    expect(press(row('r1'), 'ArrowDown').defaultPrevented).toBe(true)
  })

  it('拼音选字期间的按键不生效', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    press(row('r1'), 'ArrowDown', { isComposing: true })
    expect(document.activeElement).toBe(row('r1'))
  })
})

describe('Home / End', () => {
  it('Home 到第一行、End 到最后一行', () => {
    const list = listFixture()
    attach(list)
    row('r2').focus()
    press(row('r2'), 'Home')
    expect(document.activeElement).toBe(row('r1'))
    press(row('r1'), 'End')
    expect(document.activeElement).toBe(row('r3'))
  })

  it('空列表上不吞键', () => {
    document.body.innerHTML = '<div id="list" data-role="list" tabindex="0"></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    expect(press(list, 'End').defaultPrevented).toBe(false)
    expect(press(list, 'ArrowDown').defaultPrevented).toBe(false)
  })

  it('折叠起来的子目录里的行不可达', () => {
    document.body.innerHTML =
      '<div id="list" data-role="tree" tabindex="0">' +
      '<button data-row id="r1" tabindex="-1" aria-expanded="true">父</button>' +
      '<div data-kids id="kids"><button data-row id="r2" tabindex="-1">子</button></div></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list, { role: 'tree' })
    row('r1').focus()
    press(row('r1'), 'End')
    expect(document.activeElement).toBe(row('r2'))
    // Space 折叠的必须是**当前行**；焦点在 r2 上时 r2 不是目录行，
    // 它走「多选切换」分支。
    row('r1').focus()
    press(row('r1'), ' ')
    expect(row('r1').getAttribute('aria-expanded')).toBe('false')
    expect(row('kids').hasAttribute('hidden')).toBe(true)
    press(row('r1'), 'End')
    expect(document.activeElement).toBe(row('r1'))
  })
})

describe('Enter：语义属于行', () => {
  it('行有 data-href 时跳到那个地址', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0"><button data-row id="r1" tabindex="-1" data-href="/sessions/s1">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    // `navigate` 是可注入的跳转口：happy-dom 的 `location.assign` 不可 spy，
    // 而「href 分支有没有跑」必须能被否定——否则删掉该分支用例仍然绿。
    const navigate = vi.fn()
    attach(list, { navigate })
    row('r1').focus()
    press(row('r1'), 'Enter')
    expect(navigate).toHaveBeenCalledWith('/sessions/s1')
  })

  it('data-href 被接住时不再抛 af:enter（两条路不能都走）', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0"><button data-row id="r1" tabindex="-1" data-href="/sessions/s1">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list, { navigate: vi.fn() })
    const spy = vi.fn()
    list.addEventListener(ENTER_EVENT, spy)
    row('r1').focus()
    press(row('r1'), 'Enter')
    expect(spy).not.toHaveBeenCalled()
  })

  it('rowEnter 的行不走 data-href（Enter 归行自己）', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0"><button data-row id="r1" tabindex="-1" data-href="/sessions/s1">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    const navigate = vi.fn()
    const onEnter = vi.fn(() => true)
    attach(list, { navigate, onEnter, rowEnter: true })
    row('r1').focus()
    press(row('r1'), 'Enter')
    expect(navigate).not.toHaveBeenCalled()
    expect(onEnter).toHaveBeenCalled()
  })

  it('行内含 button 时点它', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0">' +
      '<div data-row id="r1" tabindex="-1"><button id="inner">打开</button></div></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    const inner = row('inner')
    const spy = vi.fn()
    inner.addEventListener('click', spy)
    row('r1').focus()
    press(row('r1'), 'Enter')
    expect(spy).toHaveBeenCalledTimes(1)
  })

  it('三样都没有时抛 af:enter 给页面接', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0"><button data-row id="r1" tabindex="-1">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    const spy = vi.fn()
    list.addEventListener(ENTER_EVENT, spy)
    row('r1').focus()
    press(row('r1'), 'Enter')
    expect(spy).toHaveBeenCalledTimes(1)
    expect(spy.mock.calls[0]?.[0]).toBeInstanceOf(CustomEvent)
  })

  it('onEnter 兜底被调用时以它的返回值为准', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0"><button data-row id="r1" tabindex="-1">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    const spy = vi.fn(() => false)
    attach(list, { onEnter: spy })
    row('r1').focus()
    const e = press(row('r1'), 'Enter')
    expect(spy).toHaveBeenCalledTimes(1)
    expect(e.defaultPrevented).toBe(false)
  })

  it('没有行可激活时 Enter 不动', () => {
    document.body.innerHTML = '<div id="list" data-role="list" tabindex="0"></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    expect(press(list, 'Enter').defaultPrevented).toBe(false)
  })
})

describe('Space：目录行与多选行', () => {
  it('目录行切 aria-expanded 并折叠 data-kids', () => {
    document.body.innerHTML =
      '<div id="list" data-role="tree" tabindex="0">' +
      '<button data-row id="r1" tabindex="-1" aria-expanded="true">父</button>' +
      '<div data-kids><button data-row id="r2" tabindex="-1">子</button></div></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list, { role: 'tree' })
    row('r1').focus()
    press(row('r1'), ' ')
    expect(row('r1').getAttribute('aria-expanded')).toBe('false')
    expect(list.querySelector('[data-kids]')?.hasAttribute('hidden')).toBe(true)
  })

  it('普通行切 aria-selected 并抛 af:toggle', () => {
    const list = listFixture()
    attach(list)
    const spy = vi.fn()
    list.addEventListener(TOGGLE_EVENT, spy)
    row('r1').focus()
    press(row('r1'), ' ')
    expect(row('r1').getAttribute('aria-selected')).toBe('true')
    expect(spy).toHaveBeenCalledTimes(1)
  })

  it('按钮聚焦时 Space 让给按钮（不抢）', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0">' +
      '<div data-row id="r1" tabindex="-1"><button id="inner">按我</button></div></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    const spy = vi.fn()
    list.addEventListener(TOGGLE_EVENT, spy)
    row('inner').focus()
    press(row('inner'), ' ')
    expect(spy).not.toHaveBeenCalled()
  })

  it('onToggle 兜底优先于事件', () => {
    const list = listFixture()
    const spy = vi.fn(() => false)
    attach(list, { onToggle: spy })
    row('r1').focus()
    press(row('r1'), ' ')
    expect(spy).toHaveBeenCalledTimes(1)
  })
})

describe('focusin / pointerdown', () => {
  it('容器获得焦点时把焦点交给选中行，没有则第一行', () => {
    const list = listFixture()
    attach(list)
    preselect(list, row('r2'))
    list.focus()
    list.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    expect(document.activeElement).toBe(row('r2'))
  })

  it('容器获得焦点且无选中行时给第一行', () => {
    const list = listFixture()
    attach(list)
    list.focus()
    list.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    expect(document.activeElement).toBe(row('r1'))
  })

  it('空容器获得焦点不动（不把焦点丢到 body 之后再推）', () => {
    document.body.innerHTML = '<div id="list" data-role="list" tabindex="0"></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    list.focus()
    list.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    expect(document.activeElement).toBe(list)
  })

  it('行获得焦点时同步 aria-selected', () => {
    const list = listFixture()
    attach(list)
    row('r3').focus()
    list.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    expect(row('r3').getAttribute('aria-selected')).toBe('true')
    expect(row('r1').getAttribute('aria-selected')).toBe('false')
  })

  it('指针点在行上时把焦点给那一行', () => {
    const list = listFixture()
    attach(list)
    const e = new PointerEvent('pointerdown', { bubbles: true })
    row('r3').dispatchEvent(e)
    expect(document.activeElement).toBe(row('r3'))
    expect(row('r3').getAttribute('aria-selected')).toBe('true')
  })

  it('指针点在 tabindex="0" 的行上不动（那一行本就在 Tab 序里）', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0"><button data-row id="r1" tabindex="0">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    attach(list)
    row('r1').dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
    expect(row('r1').hasAttribute('aria-selected')).toBe(false)
  })
})

describe('handle', () => {
  it('rows 只返回可见行', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0">' +
      '<button data-row id="r1" tabindex="-1">一</button>' +
      '<div hidden><button data-row id="r2" tabindex="-1">二</button></div></div>'
    const list = document.getElementById('list') as HTMLElement
    const h = attach(list)
    expect(h.rows().map((el) => el.id)).toEqual(['r1'])
  })

  it('rows 不返回 display:none 的行', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0">' +
      '<button data-row id="r1" tabindex="-1">一</button>' +
      '<button data-row id="r2" tabindex="-1" style="display:none">二</button></div>'
    const list = document.getElementById('list') as HTMLElement
    expect(attach(list).rows().map((el) => el.id)).toEqual(['r1'])
  })

  it('select 可不聚焦（只改 aria-selected）', () => {
    const list = listFixture()
    const h = attach(list)
    const outside = row('outside')
    outside.focus()
    h.select(row('r2'), false)
    expect(row('r2').getAttribute('aria-selected')).toBe('true')
    expect(document.activeElement).toBe(outside)
  })

  it('move 在焦点不在列表里时返回 false', () => {
    const list = listFixture()
    const h = attach(list)
    expect(h.move(1)).toBe(false)
  })

  it('move 相对当前选中行走，并写 aria-selected', () => {
    const list = listFixture()
    const h = attach(list)
    row('r2').focus()
    expect(h.move(1)).toBe(true)
    expect(document.activeElement).toBe(row('r3'))
    expect(row('r2').getAttribute('aria-selected')).toBe('false')
  })

  it('clearSelection 清掉整组选中', () => {
    const list = listFixture()
    const h = attach(list)
    selectRow(row('r2'))
    h.clearSelection()
    expect(h.rows().every((r) => r.getAttribute('aria-selected') === 'false')).toBe(true)
  })

  it('current 在没有选中行时返回 null', () => {
    document.body.innerHTML =
      '<div id="list" data-role="list" tabindex="0">' +
      '<button data-row id="r1" tabindex="-1">一</button></div>'
    const list = document.getElementById('list') as HTMLElement
    const h = attach(list)
    row('r1').focus()
    row('r1').setAttribute('aria-selected', 'false')
    expect(h.current()).toBeNull()
  })

  it('detach 之后方向键与 pointerdown 都不再响应', () => {
    const list = listFixture()
    const h = attach(list)
    h.detach()
    row('r1').focus()
    press(row('r1'), 'ArrowDown')
    expect(document.activeElement).toBe(row('r1'))
    row('r2').dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
    expect(document.activeElement).toBe(row('r1'))
  })
})

describe('模块级工具', () => {
  it('listOf 从行向上找到容器', () => {
    document.body.innerHTML =
      '<div id="wrap"><div id="list" data-role="list" tabindex="0"><button id="r1" data-row>x</button></div></div>'
    expect(listOf(row('r1'))?.id).toBe('list')
  })

  it('listOf 对 null 与列表外的元素返回 null', () => {
    document.body.innerHTML = '<button id="out">x</button>'
    expect(listOf(null)).toBeNull()
    expect(listOf(row('out'))).toBeNull()
  })

  it('currentRow 读当前焦点所在列表的选中行', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    preselect(list, row('r3'))
    expect(currentRow()?.id).toBe('r3')
  })

  it('currentRow 在焦点不在列表里时返回 null', () => {
    const list = listFixture()
    attach(list)
    row('r1').focus()
    row('outside').focus()
    expect(currentRow()).toBeNull()
  })

  it('selectRow 对不在列表里的行什么都不做（不抛）', () => {
    const list = listFixture()
    attach(list)
    expect(() => selectRow(row('outside'))).not.toThrow()
  })
})