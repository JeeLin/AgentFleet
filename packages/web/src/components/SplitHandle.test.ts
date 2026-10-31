import { describe, expect, it } from 'vitest'

import SplitHandle from '@/components/SplitHandle.vue'
import { mountIn } from '@/test/mount'

/** 组件的根就是分隔条本身——`querySelector` 不查根节点，所以直接取根。 */
function handle(w: ReturnType<typeof mountIn>): HTMLElement {
  const el = w.element
  if (!el.matches('[role="separator"]')) throw new Error('root is not a separator')
  return el
}

function press(el: HTMLElement, key: string): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
  el.dispatchEvent(e)
  return e
}

describe('SplitHandle：可聚焦的分隔条', () => {
  it('是 role="separator"、可聚焦，并报出当前位置', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    const el = handle(w)
    expect(el.getAttribute('tabindex')).toBe('0')
    expect(el.getAttribute('aria-orientation')).toBe('vertical')
    expect(el.getAttribute('aria-valuenow')).toBe('56')
    expect(el.getAttribute('aria-valuemin')).toBe('30')
    expect(el.getAttribute('aria-valuemax')).toBe('70')
    expect(el.getAttribute('aria-valuetext')).toContain('56%')
  })
})

describe('SplitHandle：键盘可调', () => {
  it('→ 加 4%，← 减 4%', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    press(handle(w), 'ArrowRight')
    press(handle(w), 'ArrowLeft')
    expect(w.emitted('update:modelValue')).toEqual([[60], [52]])
  })

  it('Home 到 30%，End 到 70%', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    press(handle(w), 'Home')
    press(handle(w), 'End')
    expect(w.emitted('update:modelValue')).toEqual([[30], [70]])
  })

  it('已经在下限时再按 ← 不发事件，也不吞键', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 30 } })
    const e = press(handle(w), 'ArrowLeft')
    expect(w.emitted('update:modelValue')).toBeUndefined()
    expect(e.defaultPrevented).toBe(false)
  })

  it('已经在上限时再按 → 不发事件', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 70 } })
    press(handle(w), 'ArrowRight')
    expect(w.emitted('update:modelValue')).toBeUndefined()
  })

  it('中间值时吞掉方向键，免得页面跟着滚', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    expect(press(handle(w), 'ArrowRight').defaultPrevented).toBe(true)
  })

  it('拼音选字期间的按键不动分栏', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    const e = new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true, cancelable: true, isComposing: true })
    handle(w).dispatchEvent(e)
    expect(w.emitted('update:modelValue')).toBeUndefined()
  })
})

describe('SplitHandle：拖动', () => {
  it('按下并移动时按轨道宽度换算成百分比', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 50 } })
    const el = handle(w)
    // happy-dom 里父元素量不到宽度（getBoundingClientRect 恒 0），`splitFromDrag`
    // 因此不移动：这条断言的是「量不到宽度时给一个合法值，而不是 NaN」。
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true, button: 0, clientX: 500, pointerId: 1 }))
    el.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, clientX: 900, pointerId: 1 }))
    expect(w.emitted('update:modelValue')).toEqual([[50]])
  })

  it('拖动期间给把手加 data-dragging，松开后去掉', async () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    const el = handle(w)
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true, button: 0, clientX: 500, pointerId: 2 }))
    await w.vm.$nextTick()
    expect(el.getAttribute('data-dragging')).toBe('true')
    el.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 2 }))
    await w.vm.$nextTick()
    expect(el.hasAttribute('data-dragging')).toBe(false)
  })

  it('拖动期间关掉文本选中，结束后恢复（不然整块界面被刷蓝）', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    const el = handle(w)
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true, button: 0, clientX: 500, pointerId: 3 }))
    expect(document.body.style.userSelect).toBe('none')
    el.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 3 }))
    expect(document.body.style.userSelect).toBe('')
  })

  it('右键按下不启动拖动（那是上下文菜单）', async () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    handle(w).dispatchEvent(
      new PointerEvent('pointerdown', { bubbles: true, cancelable: true, button: 2, clientX: 500, pointerId: 4 }),
    )
    await w.vm.$nextTick()
    expect(handle(w).hasAttribute('data-dragging')).toBe(false)
  })
})

describe('SplitHandle：双击恢复缺省（v0.5.0）', () => {
  it('双击发 reset 事件（缺省值由宿主决定，组件不写死）', async () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 70 } })
    handle(w).dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
    await w.vm.$nextTick()
    // 反向验证：去掉模板上的 @dblclick="emit('reset')" → 这条红
    expect(w.emitted('reset')).toHaveLength(1)
  })

  it('双击**不**顺带 emit 一个宽度值——组件不知道缺省是多少', async () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 70 } })
    handle(w).dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
    await w.vm.$nextTick()
    expect(w.emitted('update:modelValue'), '缺省值不该由把手决定').toBeUndefined()
  })

  it('双击一次只发一次 reset（不是连发）', async () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 30 } })
    handle(w).dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
    await w.vm.$nextTick()
    expect(w.emitted('reset')).toHaveLength(1)
  })

  it('仍然没有 direction prop，aria-orientation 仍是 vertical（左右语义不回退）', () => {
    const w = mountIn(SplitHandle, { props: { modelValue: 56 } })
    // v0.5.0 没有纵向分栏，所以不给 direction：没有第二个使用场景时加参数
    // 等于为一个不存在的需求预留 API（子任务 4 的裁定）。
    expect(w.props()).not.toHaveProperty('direction')
    expect(handle(w).getAttribute('aria-orientation')).toBe('vertical')
  })
})
