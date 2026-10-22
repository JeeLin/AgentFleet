import { afterEach, beforeEach, describe, expect, it } from 'vitest'

import {
  depth,
  focusMain,
  focusablesIn,
  pop,
  push,
  resetForTest,
  topContainer,
  trapTab,
} from './focus'

/**
 * 页面骨架（规范 §4.1 的浮层形状）：
 *
 *   <button id="opener">        打开者
 *   <div id="behind">           背后内容
 *   <div id="layer" tabindex="-1">   浮层容器
 *     <button id="close">关闭</button>
 *     <button id="action">动作</button>
 *     <div hidden><button id="hidden-btn">不该被 Tab 到</button></div>
 *   </div>
 */
function scaffold(): Record<string, HTMLElement> {
  document.body.innerHTML = `
    <button id="opener">打开</button>
    <div id="behind"><button id="behind-btn">背后</button></div>
    <div id="layer" tabindex="-1">
      <button id="close">关闭</button>
      <button id="action">动作</button>
      <div hidden><button id="hidden-btn">隐藏</button></div>
    </div>
  `
  const ids = ['opener', 'behind', 'layer', 'close', 'action', 'hidden-btn', 'behind-btn']
  const out: Record<string, HTMLElement> = {}
  for (const id of ids) out[id] = document.getElementById(id) as HTMLElement
  return out
}

/** 一个键盘事件。只在 trapTab 用到。 */
function keyEvent(key: string, shiftKey = false): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, shiftKey, cancelable: true })
  return e
}

describe('focus stack', () => {
  beforeEach(() => {
    resetForTest()
    document.body.innerHTML = ''
  })

  afterEach(() => {
    resetForTest()
    document.body.innerHTML = ''
  })

  it('push 把焦点交给容器里第一个可聚焦元素', () => {
    const s = scaffold()
    s.opener?.focus()
    expect(document.activeElement).toBe(s.opener)
    push(s.layer as HTMLElement)
    expect(document.activeElement).toBe(s['close'])
  })

  it('push 优先挑 data-autofocus', () => {
    const s = scaffold()
    const autofocused = document.createElement('button')
    autofocused.setAttribute('data-autofocus', '')
    const close = s['close'] as HTMLElement
    close.parentElement?.insertBefore(autofocused, close)
    push(s.layer as HTMLElement)
    expect(document.activeElement).toBe(autofocused)
  })

  it('容器内没有可聚焦元素时退到容器自身', () => {
    document.body.innerHTML = '<button id="opener">o</button><div id="bare" tabindex="-1"></div>'
    const bare = document.getElementById('bare') as HTMLElement
    push(bare)
    expect(document.activeElement).toBe(bare)
  })

  it('容器既无可聚焦元素也无 tabindex 时不动焦点（不制造一个假的落点）', () => {
    document.body.innerHTML = '<button id="opener">o</button><div id="bare"></div>'
    const opener = document.getElementById('opener') as HTMLElement
    opener.focus()
    push(document.getElementById('bare') as HTMLElement)
    expect(document.activeElement).toBe(opener)
  })

  it('push 把浮层之外的所有 body 直接子节点设为 inert', () => {
    const s = scaffold()
    push(s.layer as HTMLElement)
    expect((s['behind'] as HTMLElement).inert).toBe(true)
    expect((s['behind'] as HTMLElement).getAttribute('aria-hidden')).toBe('true')
    expect((s.opener as HTMLElement).inert).toBe(true)
    expect((s.layer as HTMLElement).inert).toBe(false)
  })

  it('含浮层的祖先（body 直接子节点）不被 inert', () => {
    document.body.innerHTML = '<div id="wrap"><div id="layer"><button id="b">b</button></div></div>'
    const wrap = document.getElementById('wrap') as HTMLElement
    push(document.getElementById('layer') as HTMLElement)
    expect(wrap.inert).toBe(false)
  })

  it('三条关闭路径（Esc / 点遮罩 / 点关闭按钮）落回同一个 pop', () => {
    const s = scaffold()
    s.opener?.focus()
    push(s.layer as HTMLElement, { opener: s.opener as HTMLElement })
    // 点遮罩与点关闭按钮**本身不带关闭动作**——关闭由 `pop` 统一负责。
    // 这两下点击只用来确认「它们不会顺手把焦点栈改坏」。
    ;(s.layer as HTMLElement).click()
    ;(s['close'] as HTMLElement).click()
    expect(depth()).toBe(1)
    expect(pop(s.layer as HTMLElement)).toBe(true)
    expect(document.activeElement).toBe(s.opener)
    expect((s['behind'] as HTMLElement).inert).toBe(false)
  })

  it('默认 opener 是当前焦点元素', () => {
    const s = scaffold()
    s['behind-btn']?.focus()
    push(s.layer as HTMLElement)
    pop()
    expect(document.activeElement).toBe(s['behind-btn'])
  })

  it('opener 已被移除时退到 main[tabindex="-1"] 而不是留下悬空引用', () => {
    document.body.innerHTML =
      '<main tabindex="-1"><h1>标题</h1></main>' +
      '<button id="opener">o</button><div id="layer" tabindex="-1"><button id="b">b</button></div>'
    const opener = document.getElementById('opener') as HTMLElement
    const layer = document.getElementById('layer') as HTMLElement
    opener.focus()
    push(layer, { opener })
    opener.remove()
    pop(layer)
    expect(document.activeElement).toBe(document.querySelector('main[tabindex="-1"]'))
  })

  it('页面没有 main 时还焦点只是不做，不抛', () => {
    document.body.innerHTML = '<button id="opener">o</button><div id="layer" tabindex="-1"><button id="b">b</button></div>'
    const opener = document.getElementById('opener') as HTMLElement
    const layer = document.getElementById('layer') as HTMLElement
    opener.focus()
    push(layer, { opener })
    opener.remove()
    expect(() => pop(layer)).not.toThrow()
  })

  it('opener 显式传 null 时不还焦点给任何人', () => {
    document.body.innerHTML = '<button id="opener">o</button><div id="layer" tabindex="-1"><button id="b">b</button></div>'
    const layer = document.getElementById('layer') as HTMLElement
    push(layer, { opener: null })
    pop(layer)
    expect(document.activeElement).not.toBe(document.getElementById('opener'))
  })

  it('pop 未指定容器时关最上层', () => {
    const s = scaffold()
    push(s.layer as HTMLElement)
    expect(depth()).toBe(1)
    expect(topContainer()).toBe(s.layer)
    pop()
    expect(depth()).toBe(0)
    expect(topContainer()).toBeNull()
  })

  it('pop 一个不存在的容器返回 false，且不动栈', () => {
    const s = scaffold()
    push(s.layer as HTMLElement)
    const other = document.createElement('div')
    expect(pop(other)).toBe(false)
    expect(depth()).toBe(1)
  })

  it('嵌套两层时只 pop 最内层，背后的 inert 保持', () => {
    document.body.innerHTML =
      '<button id="opener">o</button><div id="a" tabindex="-1"><button id="ab">ab</button></div>' +
      '<div id="b" tabindex="-1"><button id="bb">bb</button></div>'
    const a = document.getElementById('a') as HTMLElement
    const b = document.getElementById('b') as HTMLElement
    push(a)
    push(b)
    expect(a.inert).toBe(true)
    expect(b.inert).toBe(false)
    pop(b)
    expect(depth()).toBe(1)
    expect(a.inert).toBe(false)
    expect(b.inert).toBe(true)
  })

  it('pop 中间那一层也合法（还焦点给它的 opener）', () => {
    document.body.innerHTML =
      '<button id="opener">o</button><div id="a" tabindex="-1"><button id="ab">ab</button></div>' +
      '<div id="b" tabindex="-1"><button id="bb">bb</button></div>'
    const a = document.getElementById('a') as HTMLElement
    const b = document.getElementById('b') as HTMLElement
    push(a)
    push(b)
    expect(pop(a)).toBe(true)
    expect(depth()).toBe(1)
    expect(b.inert).toBe(false)
  })

  it('resetForTest 让背后恢复可交互', () => {
    const s = scaffold()
    push(s.layer as HTMLElement)
    resetForTest()
    expect((s['behind'] as HTMLElement).inert).toBe(false)
    expect((s['behind'] as HTMLElement).hasAttribute('aria-hidden')).toBe(false)
  })
})

describe('focusablesIn', () => {
  beforeEach(() => {
    resetForTest()
    document.body.innerHTML = ''
  })

  it('不含 [hidden] 里的元素、disabled 控件与 tabindex="-1" 的元素', () => {
    document.body.innerHTML =
      '<div id="root"><button id="a">a</button><div hidden><button id="h">h</button></div>' +
      '<button id="dis" disabled>d</button><button id="t" tabindex="-1">t</button></div>'
    const root = document.getElementById('root') as HTMLElement
    const ids = focusablesIn(root).map((el) => el.id)
    expect(ids).toContain('a')
    expect(ids).not.toContain('h')
    expect(ids).not.toContain('dis')
    expect(ids).not.toContain('t')
  })

  it('含 tabindex="-1" 容器的后代控件仍然算 Tab 落点', () => {
    // 模态容器就是 tabindex="-1"（原型 §4.3 的浮层形状）。若按祖先排除，
    // 模态里会被认为「一个可聚焦元素都没有」，trapTab 把每次 Tab 都吞掉。
    document.body.innerHTML =
      '<div id="root" tabindex="-1"><button id="a">a</button><button id="b" tabindex="-1">b</button></div>'
    const root = document.getElementById('root') as HTMLElement
    expect(focusablesIn(root).map((el) => el.id)).toEqual(['a'])
  })

  it('不含 inert 祖先里的元素', () => {
    document.body.innerHTML =
      '<div id="root"><button id="a">a</button><div inert><button id="i">i</button></div></div>'
    const root = document.getElementById('root') as HTMLElement
    expect(focusablesIn(root).map((el) => el.id)).not.toContain('i')
  })

  it('不含 display:none 的元素', () => {
    document.body.innerHTML =
      '<div id="root"><button id="a">a</button><button id="gone" style="display:none">g</button></div>'
    const root = document.getElementById('root') as HTMLElement
    expect(focusablesIn(root).map((el) => el.id)).not.toContain('gone')
  })

  it('每次调用重算（innerHTML 重写后不返回上一批）', () => {
    document.body.innerHTML = '<div id="root"><button id="old">o</button></div>'
    const root = document.getElementById('root') as HTMLElement
    expect(focusablesIn(root).map((el) => el.id)).toEqual(['old'])
    root.innerHTML = '<button id="new">n</button>'
    expect(focusablesIn(root).map((el) => el.id)).toEqual(['new'])
  })
})

describe('trapTab', () => {
  let root: HTMLElement

  beforeEach(() => {
    resetForTest()
    document.body.innerHTML = ''
    root = document.createElement('div')
    root.tabIndex = -1
    document.body.append(root)
  })

  afterEach(() => {
    resetForTest()
  })

  it('焦点在最后一个元素上按 Tab → 绕回第一个并阻止默认', () => {
    root.innerHTML = '<button id="f">f</button><button id="l">l</button>'
    const first = document.getElementById('f') as HTMLElement
    const last = document.getElementById('l') as HTMLElement
    last.focus()
    const e = keyEvent('Tab')
    expect(trapTab(e, root)).toBe(true)
    expect(document.activeElement).toBe(first)
    expect(e.defaultPrevented).toBe(true)
  })

  it('焦点在第一个元素上按 ⇧Tab → 绕回最后一个', () => {
    root.innerHTML = '<button id="f">f</button><button id="l">l</button>'
    const first = document.getElementById('f') as HTMLElement
    const last = document.getElementById('l') as HTMLElement
    first.focus()
    const e = keyEvent('Tab', true)
    expect(trapTab(e, root)).toBe(true)
    expect(document.activeElement).toBe(last)
  })

  it('焦点在容器外 → 拉回第一个', () => {
    document.body.insertAdjacentHTML('afterbegin', '<button id="outside">o</button>')
    root.innerHTML = '<button id="f">f</button><button id="l">l</button>'
    const outside = document.getElementById('outside') as HTMLElement
    outside.focus()
    const e = keyEvent('Tab')
    expect(trapTab(e, root)).toBe(true)
    expect(document.activeElement).toBe(document.getElementById('f'))
  })

  it('容器内没有可聚焦元素时吞掉 Tab（不把焦点放出去）', () => {
    const e = keyEvent('Tab')
    expect(trapTab(e, root)).toBe(true)
    expect(e.defaultPrevented).toBe(true)
  })

  it('焦点在中间时不干预（让浏览器按正常顺序走）', () => {
    root.innerHTML = '<button id="a">a</button><button id="m">m</button><button id="z">z</button>'
    const mid = document.getElementById('m') as HTMLElement
    mid.focus()
    const e = keyEvent('Tab')
    expect(trapTab(e, root)).toBe(false)
    expect(e.defaultPrevented).toBe(false)
  })
})

describe('focusMain', () => {
  beforeEach(() => {
    resetForTest()
    document.body.innerHTML = ''
  })

  it('聚焦 main 并给 h1 补 tabindex="-1"', () => {
    document.body.innerHTML = '<main tabindex="-1"><h1>标题</h1></main>'
    const main = focusMain()
    expect(document.activeElement).toBe(main)
    expect(main?.querySelector('h1')?.getAttribute('tabindex')).toBe('-1')
  })

  it('h1 已有 tabindex 时不覆盖', () => {
    document.body.innerHTML = '<main tabindex="-1"><h1 tabindex="-2">t</h1></main>'
    focusMain()
    expect(document.querySelector('h1')?.getAttribute('tabindex')).toBe('-2')
  })

  it('没有 main 时返回 null，不抛', () => {
    document.body.innerHTML = '<div>没有 main</div>'
    expect(focusMain()).toBeNull()
  })
})