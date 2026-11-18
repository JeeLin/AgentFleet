import { afterEach, beforeEach, describe, expect, it } from 'vitest'

import {
  clearRouteFocusTarget,
  depth,
  focusMain,
  focusRouteTarget,
  focusablesIn,
  hasRouteFocusTarget,
  pop,
  push,
  resetForTest,
  setRouteFocusTarget,
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

  /* 回归：内层 `role="dialog"` 带 `tabindex="-1"` 时，不能把它当首焦点。
     第 6 轮 4d 在活浏览器上抓到真实现象——新建任务对话框连按 10 次 Tab，
     `输入(空) → SELECT → TEXTAREA → BUTTON → 输入(空)` 循环，「标题」框
     从未出现。根因是 `container.querySelector(FOCUSABLE)` 取文档序第一个
     匹配，而 `FOCUSABLE` 含 `[tabindex]`，内层 `.af-modal[tabindex="-1"]`
     排在所有输入框之前。这个结构是对话框惯例，所以不是 BoardPage 独有。 */
  it('容器内层带 tabindex="-1" 的对话框不会抢走首焦点', () => {
    document.body.innerHTML = `
      <button id="opener">打开</button>
      <div id="scrim">
        <div id="modal" role="dialog" aria-modal="true" tabindex="-1">
          <input id="title" type="text" />
          <select id="criteria"><option value="">不设</option></select>
        </div>
      </div>
    `
    const scrim = document.getElementById('scrim') as HTMLElement
    const title = document.getElementById('title') as HTMLElement
    const modal = document.getElementById('modal') as HTMLElement
    /* 前提自检判**属性**而非 `tabIndex`：`div` 不带 `tabindex` 属性时
       `tabIndex` 本来也是 `-1`，断言它等于 -1 是恒真的（第7轮 4b 实测：
       把属性整个删掉，39 条用例仍全绿）。要钉住的是「属性在」这件事。 */
    expect(
      modal.getAttribute('tabindex'),
      '夹具前提变了：内层对话框必须带 tabindex="-1" 属性',
    ).toBe('-1')
    push(scrim)
    expect(
      document.activeElement,
      '首焦点必须落在「标题」输入框，而不是 tabindex="-1" 的对话框容器',
    ).toBe(title)
    /* 夹具里的 `opener` 不是装饰：`push()` 会把浮层之外的 body 直接子节点
       设为 inert，它正是这条的观测点（第7轮 4b 指出它此前从未被引用）。 */
    expect(
      (document.getElementById('opener') as HTMLElement).inert,
      '打开浮层后，浮层之外的元素必须 inert',
    ).toBe(true)
  })

  /* 回归：`[hidden]` 里的控件同样是 `FOCUSABLE` 的匹配，但 `focus()` 对它无效，
     焦点会留在原处——「首焦点」实际上没进对话框。与上面那条同属「挑错了元素」。
     第 7 轮实测：`<div hidden><button></div><input>` 下焦点停在 hidden-btn。
     既有缺陷（`82b3d91` 之前就这样），本里程碑内一并修。 */
  it('push 不会把首焦点交给 [hidden] 里的控件', () => {
    document.body.innerHTML = `
      <button id="opener">打开</button>
      <div id="layer">
        <div hidden><button id="hidden-btn">隐藏</button></div>
        <input id="real" type="text" />
      </div>
    `
    const hiddenBtn = document.getElementById('hidden-btn') as HTMLElement
    const real = document.getElementById('real') as HTMLElement
    // 前提自检：hidden 里的按钮确实可聚焦，且排在真实输入框之前
    expect(hiddenBtn.tabIndex, '夹具前提变了：hidden 里的按钮应仍是可聚焦元素').toBe(0)
    expect(
      hiddenBtn.compareDocumentPosition(real) & Node.DOCUMENT_POSITION_FOLLOWING,
      '夹具前提变了：hidden 按钮应排在真实输入框之前，否则证明不了「跳过了它」',
    ).toBeTruthy()
    push(document.getElementById('layer') as HTMLElement)
    expect(
      document.activeElement,
      '首焦点必须跳过 [hidden] 里的控件，落到真实输入框上',
    ).toBe(real)
  })

  /* 上一条只覆盖 `[hidden]` 这一种「不可见」，走的是 `closest('[hidden]')` 兜底。
     `display:none` 是另一条完全独立的路径，且被隐藏者是**祖先**而非自身——
     生产 CSS 里这种形态有 4 处是属性选择器（`.af-tool[data-open='false'] …`、
     `html[data-rail='off'] …`），用户可达。`getComputedStyle(子元素).display`
     在 happy-dom 下仍报 `inline-block`，所以判据必须自己逐级向上查。 */
  it('push 不会把首焦点交给 display:none 祖先里的控件', () => {
    document.body.innerHTML = `
      <div id="scrim">
        <div style="display:none"><button id="ghost">幽灵</button></div>
        <div id="modal" role="dialog" aria-modal="true" tabindex="-1">
          <input id="title" type="text" />
        </div>
      </div>
    `
    const ghost = document.getElementById('ghost') as HTMLElement
    const title = document.getElementById('title') as HTMLElement
    // 前提自检：幽灵按钮自身确实「看起来可见」（display 不是 none），
    // 只有祖先隐藏这一条能把它排除——否则这条用例证明不了「逐级向上查」
    const win = ghost.ownerDocument.defaultView!
    expect(
      win.getComputedStyle(ghost).display,
      '夹具前提变了：幽灵按钮自身不应是 display:none，隐藏必须来自祖先',
    ).not.toBe('none')
    expect(ghost.tabIndex, '夹具前提变了：幽灵按钮应仍是可聚焦元素').toBe(0)
    push(document.getElementById('scrim') as HTMLElement)
    expect(
      document.activeElement,
      '首焦点必须跳过 display:none 祖先里的控件，落到真实输入框上',
    ).toBe(title)
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
/* ===========================================================================
   G65：换页后的焦点落点（第5轮唯一那条真实的键盘可访问性缺陷）
   ---------------------------------------------------------------------------
   症状（第3轮验收实测，`step7-acceptance.md` 发现 #2）：看板里点一张卡（或聚焦
   卡片按 ⏎）进任务详情后 `document.activeElement` 是 `MAIN.af-main`
   （`tabindex="-1"`），第一次 Tab 落到页头那颗「← 看板」；按 Back 回看板，
   焦点也不回那张卡。键盘用户每进出一个任务都得从头 Tab 一遍整页。

   成因：换页后落焦点只有「交回 main」一条路（`focusMain`），它分不出
   「用户从哪儿来」。

   这一组钉的是新语义的三档优先级：
     ① 登记了来源（`setRouteFocusTarget`）→ **让出**这一次，谁都不聚焦；
     ② 没有登记、但有「上一站」带 `data-id` → 回到新 DOM 里的同名元素；
     ③ 都没有 → `main`（逐字等于旧的 `focusMain`，`router.test.ts` 钉着它）。
   =========================================================================== */
describe('G65 focusRouteTarget：换页后的焦点落点', () => {
  beforeEach(() => {
    resetForTest()
    document.body.innerHTML = ''
  })

  afterEach(() => {
    resetForTest()
    document.body.innerHTML = ''
  })

  /** 外壳 + 当前页。`main.af-main[tabindex="-1"]` 是 `App.vue` 的真实形状。 */
  function shell(inner: string): HTMLElement {
    document.body.innerHTML = `<main class="af-main" tabindex="-1">${inner}</main>`
    return document.querySelector('main.af-main') as HTMLElement
  }

  it('没有登记来源时落到 main，并给 h1 补 tabindex="-1"（旧语义不变）', () => {
    const main = shell('<h1>设备</h1>')
    expect(focusRouteTarget()).toBe(main)
    expect(document.activeElement).toBe(main)
    expect(main.querySelector('h1')?.getAttribute('tabindex')).toBe('-1')
  })

  it('没有 main 时返回 null，不抛', () => {
    document.body.innerHTML = '<div>没有 main</div>'
    expect(focusRouteTarget()).toBeNull()
  })

  it('登记了来源 → 这一次调用**让出焦点**，且让出只欠一次', () => {
    // 只看返回值会被「让出」掩盖：这条要证明的是**焦点没被夺走**。
    shell('<h1>设备</h1>')
    const card = document.createElement('button')
    card.setAttribute('data-id', 'tsk_1')
    document.body.append(card)
    card.focus()

    setRouteFocusTarget(card)
    expect(hasRouteFocusTarget()).toBe(true)
    // 「让出」的含义：谁都没被聚焦，`navigate()` 的微任务拿到 null，
    // 于是焦点没从用户刚点的那张卡上被夺走。
    expect(focusRouteTarget()).toBeNull()
    expect(document.activeElement, '焦点仍在卡片上，没被抢到 main').toBe(card)
    // 让出只欠一次：第二次调用就真的落焦点了——按「上一站」的 data-id
    // 找回那张卡（它还在 DOM 里），而不是丢给 `main`。
    expect(hasRouteFocusTarget()).toBe(false)
    expect(focusRouteTarget()).toBe(card)
  })

  it('卡片路径的真实顺序：登记 → 让出 → 页面把焦点放到自己的标题', () => {
    // openTask 登记 → navigate → router 微任务调 focusRouteTarget（让出）→
    // 详情页数据到了、h1 渲染出来 → 详情页自己 `h1.focus()`。
    const main = shell('<h1 tabindex="-1" data-id="tsk_1">把构建时间降到 60s 内</h1>')
    const h1 = main.querySelector('h1') as HTMLElement
    const card = document.createElement('button')
    document.body.append(card)
    card.focus()

    setRouteFocusTarget(card)
    expect(focusRouteTarget(), 'router 的微任务应让出').toBeNull()
    expect(document.activeElement, '还停在卡片上').toBe(card)

    // 详情页落自己的标题（`ProjectTaskPage.vue` 里就是这一句）。
    h1.focus()
    expect(document.activeElement).toBe(h1)
    expect(h1.getAttribute('tabindex'), '可编程聚焦的前提').toBe('-1')
  })

  it('Back 场景：上一站带 data-id → 回到新 DOM 里的同名卡片', () => {
    // 详情页：h1 带任务 id。
    const detail = shell('<h1 tabindex="-1" data-id="tsk_1">任务标题</h1>')
    const detailH1 = detail.querySelector('h1') as HTMLElement
    // 「上一站」由 `setRouteFocusTarget` 记（点卡片时登记的就是那张卡，
    // 它同样带 data-id）——这里用它模拟「上一站是带 id 的元素」。
    setRouteFocusTarget(detailH1)
    focusRouteTarget() // 消费掉「让出」，落点交给页面
    detailH1.focus()

    // Back 回看板：整页换了一份 DOM，卡片带同一个 data-id。
    shell(
      '<p class="af-muted">内部指标台</p>' +
        '<button class="af-taskcard" data-id="tsk_1">把构建时间降到 60s 内</button>' +
        '<button class="af-taskcard" data-id="tsk_2">另一张卡</button>',
    )
    const got = focusRouteTarget()
    expect(got?.getAttribute('data-id'), '应回到同一张卡').toBe('tsk_1')
    expect(got?.tagName).toBe('BUTTON')
    expect(document.activeElement).toBe(got)
  })

  it('回到列表时目标已经不在了（卡片被删）→ 退到 main，不抛', () => {
    const main = shell('<div>看板</div>')
    const gone = document.createElement('button')
    gone.setAttribute('data-id', 'tsk_9')
    document.body.append(gone)
    setRouteFocusTarget(gone)
    focusRouteTarget()
    gone.remove()

    expect(() => focusRouteTarget()).not.toThrow()
    expect(document.activeElement).toBe(main)
  })

  it('`data-id` 里带引号也不会让 querySelector 抛（落焦点抛 = 键盘用户丢了）', () => {
    const main = shell('<div>看板</div>')
    const weird = document.createElement('button')
    weird.setAttribute('data-id', 'a"b')
    document.body.append(weird)
    setRouteFocusTarget(weird)
    focusRouteTarget()
    weird.remove()
    expect(() => focusRouteTarget()).not.toThrow()
    expect(document.activeElement).toBe(main)
  })

  it('clearRouteFocusTarget 把两处记录都清掉', () => {
    const main = shell('<h1>标题</h1>')
    const card = document.createElement('button')
    card.setAttribute('data-id', 'tsk_1')
    document.body.append(card)
    setRouteFocusTarget(card)
    expect(hasRouteFocusTarget()).toBe(true)
    clearRouteFocusTarget()
    expect(hasRouteFocusTarget()).toBe(false)
    // 清空之后：既没有 sticky，也没有「上一站」→ 第 ③ 档。
    expect(focusRouteTarget()).toBe(main)
  })
})
