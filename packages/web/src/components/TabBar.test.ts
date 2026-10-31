import { describe, expect, it, vi } from 'vitest'

import TabBar from '@/components/TabBar.vue'
import { mountIn } from '@/test/mount'
import type { TabMeta } from '@/lib/tabs'
import type { PanelName } from '@/lib/panel'

const TABS: TabMeta[] = [
  { id: 's_1', title: 'alpha', path: '/srv/alpha', nodeId: 'n_1' },
  { id: 's_2', title: 'beta', path: '/srv/beta', nodeId: 'n_1' },
  { id: 's_3', title: 'gamma', path: '/srv/gamma', nodeId: 'n_2' },
]

function mountBar(activeId: string | null = 's_1', panelIcons: Record<string, PanelName | null> = {}) {
  const w = mountIn(TabBar, {
    props: {
      tabs: TABS,
      activeId,
      panelIcons,
      connOf: (nodeId: string | null) => (nodeId === 'n_2' ? 'offline' : 'online'),
    },
  })
  return w
}

function label(el: Element, id: string): HTMLElement {
  const found = el.querySelector<HTMLElement>(`[data-tab-label="${id}"]`)
  if (!found) throw new Error(`no label for ${id}`)
  return found
}

function press(el: HTMLElement, key: string): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
  el.dispatchEvent(e)
  return e
}

describe('TabBar：一个 tab stop（roving tabindex）', () => {
  it('只有当前 tab 的标签是 tabindex="0"，其余是 -1', () => {
    const w = mountBar('s_2')
    const stops = Array.from(w.element.querySelectorAll<HTMLElement>('[role="tab"]'))
    expect(stops.map((el) => el.getAttribute('tabindex'))).toEqual(['-1', '0', '-1'])
  })

  it('没有激活 tab 时一个 tab stop 都没有（空态不留下悬空的焦点）', () => {
    const w = mountBar(null)
    const stops = Array.from(w.element.querySelectorAll<HTMLElement>('[role="tab"]'))
    expect(stops.every((el) => el.getAttribute('tabindex') === '-1')).toBe(true)
  })

  it('‹活动› 的 aria-selected 只有一条是真的', () => {
    const w = mountBar('s_1')
    const selected = Array.from(w.element.querySelectorAll('[role="tab"]')).filter(
      (el) => el.getAttribute('aria-selected') === 'true',
    )
    expect(selected).toHaveLength(1)
    expect(selected[0]).toBe(label(w.element, 's_1'))
  })
})

describe('TabBar：←→ 移动并激活', () => {
  it('→ 发出 activate，目标就是下一个 tab', () => {
    const w = mountBar('s_1')
    press(label(w.element, 's_1'), 'ArrowRight')
    expect(w.emitted('activate')?.[0]).toEqual(['s_2'])
  })

  it('← 发出 activate，目标就是上一个 tab', () => {
    const w = mountBar('s_2')
    press(label(w.element, 's_2'), 'ArrowLeft')
    expect(w.emitted('activate')?.[0]).toEqual(['s_1'])
  })

  it('Home / End 到两端', () => {
    const w = mountBar('s_2')
    press(label(w.element, 's_2'), 'Home')
    press(label(w.element, 's_2'), 'End')
    expect(w.emitted('activate')).toEqual([['s_1'], ['s_3']])
  })

  it('到头不绕回：最后一格按 → 不发事件，也不吞键', () => {
    const w = mountBar('s_3')
    const e = press(label(w.element, 's_3'), 'ArrowRight')
    expect(w.emitted('activate')).toBeUndefined()
    expect(e.defaultPrevented).toBe(false)
  })

  it('第一格按 ← 同样不绕回', () => {
    const w = mountBar('s_1')
    press(label(w.element, 's_1'), 'ArrowLeft')
    expect(w.emitted('activate')).toBeUndefined()
  })

  it('拼音选字期间的按键不生效', () => {
    const w = mountBar('s_1')
    const e = new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true, isComposing: true })
    label(w.element, 's_1').dispatchEvent(e)
    expect(w.emitted('activate')).toBeUndefined()
  })
})

describe('TabBar：关闭', () => {
  it('Delete 关掉当前 tab', () => {
    const w = mountBar('s_2')
    const e = press(label(w.element, 's_2'), 'Delete')
    expect(w.emitted('close')?.[0]).toEqual(['s_2'])
    expect(e.defaultPrevented).toBe(true)
  })

  it('关闭控件是真的 <button>（不是套在标签里的 span）', () => {
    const w = mountBar('s_1')
    const close = w.element.querySelector('[data-tab-close="s_1"]')
    expect(close?.tagName).toBe('BUTTON')
    expect(close?.getAttribute('type')).toBe('button')
    expect(close?.getAttribute('aria-label')).toContain('alpha')
    // 它不在标签按钮内部：嵌进去会被父按钮从 Tab 序列里拿掉
    expect(label(w.element, 's_1').contains(close)).toBe(false)
  })

  it('点关闭按钮只发 close，不连带激活这个 tab', async () => {
    const w = mountBar('s_2')
    await w.element.querySelector<HTMLElement>('[data-tab-close="s_1"]')?.click()
    expect(w.emitted('close')?.[0]).toEqual(['s_1'])
    expect(w.emitted('activate')).toBeUndefined()
  })

  it('这里没有 ⌃W 之类的关闭快捷键：只有 Delete', () => {
    const w = mountBar('s_1')
    press(label(w.element, 's_1'), 'w')
    expect(w.emitted('close')).toBeUndefined()
  })
})

describe('TabBar：当前工具可见', () => {
  /*
    v0.4.0 这一组断言的是「开过面板的 tab 显示面板数徽标（数字 + 「N 个面板」）」。
    那个数字的含义是「这个 tab 上几块面板 = 几条活跃通道」。

    v0.5.0 右侧只剩一格（裁决 C），面板数恒为 1，一个永远是 1 的数字不是信息。
    文档的裁定是**改为在 tab 上显示当前工具的图标**，不是删掉这个位置——
    「这个 tab 现在开着什么」仍然有用，图标是最省的载体。所以这几条**改写**
    （不是删除）：断言载体从数字换成图标，且图标随工具变。
  */
  it('开着工具的 tab 显示那个工具的图标与名字', () => {
    const w = mountBar('s_1', { s_1: 'git' })
    const tool = w.element.querySelector('[data-sid="s_1"] [data-tab-tool]')
    expect(tool, 'tab 上必须有当前工具标记').not.toBeNull()
    expect(tool?.textContent?.trim(), 'git 用 ⑂').toBe('⑂')
    expect(tool?.getAttribute('title')).toContain('Git')
  })

  it('换工具就换图标（不是写死一个）', () => {
    const w = mountBar('s_1', { s_1: 'terminal' })
    const tool = w.element.querySelector('[data-sid="s_1"] [data-tab-tool]')
    expect(tool?.textContent?.trim(), 'terminal 用 ▶').toBe('▶')
    expect(tool?.getAttribute('title')).toContain('终端')
  })

  it('没有工具的 tab 不显示标记（收起坞 ≠ 开着什么）', () => {
    const w = mountBar('s_1', { s_1: null })
    expect(w.element.querySelector('[data-tab-tool]')).toBeNull()
  })

  it('面板数徽标已经没有了（恒为 1 的数字不该占这个位置）', () => {
    const w = mountBar('s_1', { s_1: 'git' })
    // 反向验证：把 TabBar 改回渲染 `{{ panelCounts[tab.id] }}` → 这条红
    expect(Array.from(w.element.querySelectorAll('.af-badge')).map((el) => el.textContent?.trim())).not.toContain(
      '1',
    )
  })

  it('断开的设备在 tab 上有连接态标记', () => {
    const w = mountBar('s_1')
    const tab = w.element.querySelector('[data-sid="s_3"]')
    expect(tab?.getAttribute('data-conn')).toBe('offline')
  })
})

describe('TabBar：无障碍名', () => {
  it('tablist 有可读的名字', () => {
    const w = mountBar('s_1')
    // 根节点本身就是 tablist——querySelector 不查根
    expect(w.element.getAttribute('role')).toBe('tablist')
    expect(w.element.getAttribute('aria-label')).toBe('打开的会话')
    expect(w.element.getAttribute('aria-orientation')).toBe('horizontal')
  })

  it('标签的 title 同时给出路径与 id', () => {
    const w = mountBar('s_1')
    expect(label(w.element, 's_1').getAttribute('title')).toContain('/srv/alpha')
    expect(label(w.element, 's_1').getAttribute('title')).toContain('s_1')
  })
})

describe('TabBar：焦点落点', () => {
  it('移动激活后焦点落在目标 tab 的标签上', async () => {
    const w = mountBar('s_1')
    press(label(w.element, 's_1'), 'ArrowRight')
    await w.vm.$nextTick()
    expect(document.activeElement).toBe(label(w.element, 's_2'))
    void vi
  })
})
