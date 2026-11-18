import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { register, unregister } from './keynav'
import { bindShell, installShellKeys } from './shell-keys'

/**
 * 造一个 `ShellBindings`，每个回调都是 `vi.fn`，返回 `ret` 里指定的值。
 *
 * **默认值刻意「不吞键」**（`closeHelp` / `focusSearch` 返回 `false`）：这样
 * 用例必须自己说明它要哪一种结果，而不是继承一个碰巧让断言变绿的值。
 */
function spyBindings(ret: {
  closeHelp?: boolean
  focusSearch?: boolean
} = {}) {
  return {
    navigate: vi.fn(),
    toggleRail: vi.fn(),
    openHelp: vi.fn(),
    closeHelp: vi.fn(() => ret.closeHelp ?? false),
    focusSearch: vi.fn(() => ret.focusSearch ?? false),
  }
}

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


  /* =========================================================================
     **导航用例（v0.8.1 第5轮 4c F6 / G81）**：在此之前没有任何用例断言
     `g s` / `g d` / `g a` / `g ,` 真的导航到了**正确的地址**。4c 实测——把
     `nav()` 改成直接 `return true`（不调 `bindings.navigate`）、把 `nav:overview`
     的 handler 改成 `return true`，相关用例全绿、整套用例也全绿。
     也就是说「按 `g s` 跳总览」这个功能坏了没人知道。

     判据与上面四组相同：**按了什么键 → navigate 收到哪个地址**。
     `toHaveBeenCalledWith` 是关键——只断言 `toHaveBeenCalled()` 的话，
     「g s 导航去了 /devices」照样绿。
     ========================================================================= */
  it('g s → navigate("/")（总览；v0.7.0 起会话轴改成项目轴）', () => {
    const b = withBindings()
    pressOn(document.body, 'g')
    pressOn(document.body, 's')
    expect(b.navigate).toHaveBeenCalledTimes(1)
    expect(b.navigate).toHaveBeenCalledWith('/')
  })

  it('g d → navigate("/devices")', () => {
    const b = withBindings()
    pressOn(document.body, 'g')
    pressOn(document.body, 'd')
    expect(b.navigate).toHaveBeenCalledTimes(1)
    expect(b.navigate).toHaveBeenCalledWith('/devices')
  })

  it('g a → navigate("/agents")', () => {
    const b = withBindings()
    pressOn(document.body, 'g')
    pressOn(document.body, 'a')
    expect(b.navigate).toHaveBeenCalledTimes(1)
    expect(b.navigate).toHaveBeenCalledWith('/agents')
  })

  it('g , → navigate("/settings")（与 g s 的地址不能互换）', () => {
    const b = withBindings()
    pressOn(document.body, 'g')
    pressOn(document.body, ',')
    expect(b.navigate).toHaveBeenCalledTimes(1)
    expect(b.navigate).toHaveBeenCalledWith('/settings')
  })

  it('四个导航键两两不混淆（g s 不会跳去 devices 等）', () => {
    const seq: Array<[string, string]> = [
      ['s', '/'],
      ['d', '/devices'],
      ['a', '/agents'],
      [',', '/settings'],
    ]
    for (const [second, path] of seq) {
      const b = withBindings()
      pressOn(document.body, 'g')
      pressOn(document.body, second)
      expect(b.navigate, `g ${second} 应导航到 ${path}`).toHaveBeenCalledWith(path)
      expect(b.navigate, `g ${second} 不应导航到别处`).toHaveBeenCalledTimes(1)
    }
  })

  it('g 序列超时（1.5s）后第二键不再触发导航', () => {
    /* 用 vi.useFakeTimers 钉住时间。没有这条的话，「把超时改成 0」这类
       变异不会被抓——而那会让 g s 必须在一帧内按完。 */
    vi.useFakeTimers()
    try {
      const b = withBindings()
      pressOn(document.body, 'g')
      vi.advanceTimersByTime(1600)
      pressOn(document.body, 's')
      expect(b.navigate, '超时后 g s 不应触发导航').not.toHaveBeenCalled()
    } finally {
      vi.useRealTimers()
    }
  })
})

/* ===========================================================================
   下面四组：`?` / `Esc` / `⌃B` / `/` 四个外壳级处理器**各自的归宿**。
   ---------------------------------------------------------------------------
   这一组存在的理由（2026-11 第3轮测试验证 P13 / P14 实测）：在此之前
   `shell-keys.test.ts` 只有上面 3 条，全在测 `g` 序列，于是四个处理器
   **一行都没被执行过**——把 `help:toggle` 的处理器改成恒早返（`?` 永久失效）
   或把 `rail:toggle` 的 `toggleRail()` 改成 `openHelp()`（⌃B 打开帮助而不是
   收左栏），整套用例照样全绿。

   判据一律是**「按了什么键 → 调了哪个回调」**，而不是「回调被调用过」：
   四个动作共享同一个 `bindings` 槽位，只断言 `toHaveBeenCalled()` 的话，
   「按 `?` 却调了 toggleRail」照样绿——那正是 P14 那种破坏的形态。
   所以每条都成对断言：**该调的调了、不该调的没调**。

   这些是**全局**键位：坏了影响每一页，不是某一页的细节。
   =========================================================================== */

/**
 * 装好外壳键位 + 一组新的 spy 回调，并在用例结束时解绑。
 *
 * `bindShell` 是模块级槽位（`shell-keys.ts:34`），用例之间必须解开——
 * 不解的话一个用例留下的 spy 会被下一条用例的按键继续调用，症状是
 * 「单跑绿、全量红」。
 */
function withBindings(
  ret: { closeHelp?: boolean; focusSearch?: boolean } = {},
): ReturnType<typeof spyBindings> {
  const b = spyBindings(ret)
  const stop = bindShell(b)
  unbinders.push(stop)
  return b
}

const unbinders: Array<() => void> = []

beforeEach(() => {
  installShellKeys()
})

afterEach(() => {
  for (const stop of unbinders.splice(0)) stop()
  document.body.innerHTML = ''
})

/** 往一个元素上派发 keydown；可选修饰键。 */
function pressOn(
  target: EventTarget,
  key: string,
  mods: { ctrlKey?: boolean; metaKey?: boolean; shiftKey?: boolean; altKey?: boolean } = {},
): KeyboardEvent {
  const ev = new KeyboardEvent('keydown', {
    key,
    bubbles: true,
    cancelable: true,
    ...mods,
  })
  target.dispatchEvent(ev)
  return ev
}

/** 造一个输入区（`isTyping` 认的四种之一）。 */
function typingTarget(tag: 'input' | 'textarea' = 'input'): HTMLElement {
  const el = document.createElement(tag)
  document.body.appendChild(el)
  el.focus()
  return el
}

describe('shell-keys：`?` → help:toggle', () => {
  it('帮助没开时按 `?` 打开帮助，且不顺手收左栏', () => {
    const b = withBindings({ closeHelp: false })

    const ev = pressOn(document.body, '?')

    // 分支命中：closeHelp 先问「有没有层可关」，答案是「没有」→ 走 openHelp
    expect(b.closeHelp, '`?` 必须先问「帮助是不是开着」').toHaveBeenCalledTimes(1)
    expect(b.openHelp, '帮助没开时 `?` 必须打开它').toHaveBeenCalledTimes(1)
    // 反向：`?` 只能做这一件事
    expect(b.toggleRail, '`?` 不该动左栏（那是 ⌃B 的活）').not.toHaveBeenCalled()
    expect(b.navigate).not.toHaveBeenCalled()
    expect(ev.defaultPrevented, '`?` 被接住了就该 preventDefault').toBe(true)
  })

  it('帮助开着时按 `?` 关掉它，且不再调 openHelp（开合开关，不是单向打开）', () => {
    const b = withBindings({ closeHelp: true })

    const ev = pressOn(document.body, '?')

    expect(b.closeHelp).toHaveBeenCalledTimes(1)
    expect(b.openHelp, '已经关掉了就不该再打开（否则按 `?` 永远关不掉）').not.toHaveBeenCalled()
    expect(b.toggleRail).not.toHaveBeenCalled()
    expect(ev.defaultPrevented).toBe(true)
  })

  it('焦点在输入区里时 `?` 是在打字，不抢', () => {
    const b = withBindings({ closeHelp: false })

    const ev = pressOn(typingTarget(), '?')

    expect(b.openHelp, '输入框里敲 `?` 不该打开帮助面板').not.toHaveBeenCalled()
    expect(b.closeHelp).not.toHaveBeenCalled()
    expect(ev.defaultPrevented, '没被接住就不该 preventDefault（那个字符要留给输入框）').toBe(
      false,
    )
  })
})

describe('shell-keys：Esc → layer:escape', () => {
  it('有层可关时 Esc 关掉它并吞键', () => {
    const b = withBindings({ closeHelp: true })

    const ev = pressOn(document.body, 'Escape')

    expect(b.closeHelp).toHaveBeenCalledTimes(1)
    // 反向：Esc 不是「什么都关」，更不是开帮助
    expect(b.openHelp, 'Esc 不该打开帮助').not.toHaveBeenCalled()
    expect(b.toggleRail).not.toHaveBeenCalled()
    expect(ev.defaultPrevented, '有层可关时 Esc 归我们').toBe(true)
  })

  it('没有层可关时 Esc 还给浏览器（清输入框选择等）', () => {
    const b = withBindings({ closeHelp: false })

    const ev = pressOn(document.body, 'Escape')

    expect(b.closeHelp).toHaveBeenCalledTimes(1)
    expect(ev.defaultPrevented, '没有层时不该吞 Esc').toBe(false)
  })

  it('焦点在输入区里按 Esc 照样问「有没有层可关」（Esc 不受输入区限制）', () => {
    // 与 `?` / `/` 不同：Esc 是「退出当前态」的通用手势，在输入框里也要能
    // 关掉浮层——`shell-keys.ts:151` 的 Escape 分支**没有** `!typing` 前置。
    const b = withBindings({ closeHelp: true })

    const ev = pressOn(typingTarget(), 'Escape')

    expect(b.closeHelp).toHaveBeenCalledTimes(1)
    expect(ev.defaultPrevented).toBe(true)
  })

  it('Esc 带 ⌃/⌘ 时不归外壳管（那是终端的键）', () => {
    const b = withBindings({ closeHelp: true })

    const ev = pressOn(document.body, 'Escape', { ctrlKey: true })

    expect(b.closeHelp, '⌃Esc 是终端的键，外壳不该抢').not.toHaveBeenCalled()
    expect(ev.defaultPrevented).toBe(false)
  })
})

describe('shell-keys：⌃B → rail:toggle', () => {
  it('⌃B 收 / 展左栏，且**不**打开帮助', () => {
    // P14 的形态正是这一条：`toggleRail()` 被写成 `openHelp()`。
    // 只断言「被调用过」是看不出区别的——两个回调都在同一个 bindings 上。
    const b = withBindings()

    const ev = pressOn(document.body, 'b', { ctrlKey: true })

    expect(b.toggleRail, '⌃B 必须收 / 展左栏').toHaveBeenCalledTimes(1)
    expect(b.openHelp, '⌃B 打开帮助是错的（P14）').not.toHaveBeenCalled()
    expect(b.closeHelp).not.toHaveBeenCalled()
    expect(b.focusSearch).not.toHaveBeenCalled()
    expect(ev.defaultPrevented).toBe(true)
  })

  it('⌘B 同样生效（macOS 的修饰键与 ⌃ 等价）', () => {
    const b = withBindings()

    pressOn(document.body, 'b', { metaKey: true })

    expect(b.toggleRail).toHaveBeenCalledTimes(1)
    expect(b.openHelp).not.toHaveBeenCalled()
  })

  it('大写 B 也认（Caps Lock / ⇧ 下 `key` 是 "B"）', () => {
    const b = withBindings()

    pressOn(document.body, 'B', { ctrlKey: true })

    expect(b.toggleRail).toHaveBeenCalledTimes(1)
  })

  it('**焦点在输入区里 ⌃B 照样生效**（用户可能在终端里按它）', () => {
    // `keynav.ts:422-424`：「修饰键（⌃B 等）在输入区内**照常**生效」。
    // 这一条与 `?` / `/` 的输入区用例**方向相反**，是同一道分界的两侧。
    const b = withBindings()

    const ev = pressOn(typingTarget(), 'b', { ctrlKey: true })

    expect(b.toggleRail, '⌃B 不受输入区限制').toHaveBeenCalledTimes(1)
    expect(ev.defaultPrevented).toBe(true)
  })

  it('带 ⇧ 或 ⌥ 的 ⌃B 不是这条键位（⌃⇧B 留给别处）', () => {
    const b = withBindings()

    pressOn(document.body, 'b', { ctrlKey: true, shiftKey: true })
    pressOn(document.body, 'b', { ctrlKey: true, altKey: true })

    expect(b.toggleRail, '⌃⇧B / ⌃⌥B 不该被当成 ⌃B').not.toHaveBeenCalled()
  })
})

describe('shell-keys：`/` → search:focus', () => {
  it('本页有搜索框时 `/` 把焦点送过去并吞键', () => {
    const b = withBindings({ focusSearch: true })

    const ev = pressOn(document.body, '/')

    expect(b.focusSearch, '`/` 必须问「本页的搜索框在哪」').toHaveBeenCalledTimes(1)
    // 反向：`/` 不该顺手做别的事
    expect(b.openHelp).not.toHaveBeenCalled()
    expect(b.toggleRail).not.toHaveBeenCalled()
    expect(ev.defaultPrevented, '接住了就该 preventDefault').toBe(true)
  })

  it('本页没有搜索框时 `/` 还给浏览器（键不吞、不假装成功）', () => {
    // `shell-keys.ts:30-31` 的契约：「本页没有搜索时返回 false（键还给浏览器）」。
    const b = withBindings({ focusSearch: false })

    const ev = pressOn(document.body, '/')

    expect(b.focusSearch).toHaveBeenCalledTimes(1)
    expect(ev.defaultPrevented, '没有搜索框时不该吞掉 `/`').toBe(false)
  })

  it('焦点在输入区里时 `/` 是在打字，不抢', () => {
    const b = withBindings({ focusSearch: true })

    const ev = pressOn(typingTarget(), '/')

    expect(b.focusSearch, '输入框里敲 `/` 不该把焦点抢走').not.toHaveBeenCalled()
    expect(ev.defaultPrevented).toBe(false)
  })

  it('textarea 里 `/` 同样不抢（判据是 isTyping，不是「只认 input」）', () => {
    const b = withBindings({ focusSearch: true })

    const ev = pressOn(typingTarget('textarea'), '/')

    expect(b.focusSearch, 'textarea 也是输入区').not.toHaveBeenCalled()
    expect(ev.defaultPrevented).toBe(false)
  })
})
