import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { register, unregister } from './keynav'
import { bindShell, installShellKeys } from './shell-keys'

/**
 * 外壳级键位。
 *
 * 重点是 **`g` 序列的第二键**：第一键判过「是不是在输入区里」，第二键
 * 也要判——用户在搜索框里敲 `g` 再敲 `g`，第二键落在输入区里，不该被
 * 解释成「打开 / 关闭轨迹抽屉」并把那个字符吃掉。
 */
describe('shell-keys：g 序列', () => {
  let stop: (() => void) | null = null
  let traj: ReturnType<typeof vi.fn>

  beforeEach(() => {
    installShellKeys()
    traj = vi.fn(() => true)
    register('traj:toggle', traj)
    stop = bindShell({
      navigate: () => undefined,
      toggleRail: () => undefined,
      openHelp: () => undefined,
      closeHelp: () => false,
      focusSearch: () => false,
    })
  })

  afterEach(() => {
    stop?.()
    stop = null
    unregister('traj:toggle')
    document.body.innerHTML = ''
  })

  /** 造一个输入框并聚焦它。 */
  function input(): HTMLInputElement {
    const el = document.createElement('input')
    document.body.appendChild(el)
    el.focus()
    return el
  }

  function press(target: EventTarget, key: string): KeyboardEvent {
    const ev = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
    target.dispatchEvent(ev)
    return ev
  }

  it('输入区里的第一个 g 不武装前缀（那是在打字）', () => {
    const el = input()
    const ev = press(el, 'g')
    expect(ev.defaultPrevented).toBe(false)
    // 前缀没武装：紧接着的第二个 g 也不该被当成序列
    const second = press(el, 'g')
    expect(second.defaultPrevented).toBe(false)
    expect(traj).not.toHaveBeenCalled()
  })

  it('第二键落在输入区里时不吃掉字符，也不触发 g g', () => {
    // 第一键在输入区**外**：前缀被武装
    const first = press(document.body, 'g')
    expect(first.defaultPrevented).toBe(true)

    // 用户把焦点移进搜索框，在 800ms 窗口内敲了第二个 g
    const el = input()
    const second = press(el, 'g')

    expect(second.defaultPrevented, '输入区里的字符不该被吞').toBe(false)
    expect(traj, '输入态下不解释全局键（§4.2）').not.toHaveBeenCalled()
  })

  it('两个键都不在输入区时，g g 照常开合轨迹抽屉', () => {
    press(document.body, 'g')
    const second = press(document.body, 'g')
    expect(traj).toHaveBeenCalledTimes(1)
    expect(second.defaultPrevented).toBe(true)
  })
})
