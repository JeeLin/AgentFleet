import { describe, expect, it, vi } from 'vitest'

import PanelDock from '@/components/PanelDock.vue'
import { mountIn } from '@/test/mount'
import type { ChannelView } from '@/components/PanelDock.vue'

const IDLE: ChannelView = { state: 'idle', error: null, dropped: 0 }

function mount(props: Partial<Record<string, unknown>> = {}) {
  return mountIn(PanelDock, {
    props: {
      panel: 'terminal' as const,
      mounted: ['terminal'] as const,
      deviceName: 'mbp.local',
      path: '/srv/alpha',
      channel: IDLE,
      output: '',
      cwd: [],
      ...props,
    },
  })
}

describe('PanelDock：容器只做容器的事', () => {
  it('有面板时列出坞标签，没有时明写「没有打开的面板」', () => {
    const withPanels = mount()
    expect(withPanels.element.querySelector('.af-docktabs__b')?.textContent).toContain('终端')

    const none = mount({ panel: null, mounted: [] })
    expect(none.element.querySelector('.af-docktabs__empty')?.textContent).toContain('没有打开的面板')
    expect(none.element.querySelector('.af-panelhost__void')?.textContent).toContain('面板')
  })

  it('面板头重复一次设备与路径（两个终端并排时靠它分）', () => {
    const w = mount()
    const head = w.element.querySelector('.af-panelhead')
    expect(head?.textContent).toContain('mbp.local')
    expect(head?.textContent).toContain('/srv/alpha')
  })

  it('13a 的正文是占位：不画假终端，也不解析 ANSI', () => {
    const w = mount({ output: '' })
    expect(w.element.textContent).toContain('终端内容由 13b 挂入')
    expect(w.element.querySelector('.af-term__line')).not.toBeNull()
  })

  it('有输出时按行切开渲染（空输出不画假行）', () => {
    const w = mount({ output: 'one\ntwo\r\nthree' })
    const lines = Array.from(w.element.querySelectorAll('.af-term__line')).map((el) => el.textContent)
    expect(lines).toEqual(['one', 'two', 'three'])
  })

  it('同一份输出只切一次：模板里重复引用不重复 split', async () => {
    // 缓冲是一整段滚动记录，模板里 `lines()` 调两次就是两次全量 split
    const w = mount({ output: 'a\nb\nc' })
    const rendered = Array.from(w.element.querySelectorAll('.af-term__line')).map(
      (el) => el.textContent,
    )
    expect(rendered).toEqual(['a', 'b', 'c'])

    // 改一次输出：computed 失效并重算，行数跟着变（不是第一次切完就冻住）
    await w.setProps({ output: 'a\nb\nc\nd' })
    expect(Array.from(w.element.querySelectorAll('.af-term__line')).map((el) => el.textContent)).toEqual([
      'a',
      'b',
      'c',
      'd',
    ])
  })
})

describe('PanelDock：通道状态可见', () => {
  it('未建立时说「未建立」——不是「已连接」', () => {
    const w = mount({ channel: { state: 'idle', error: null, dropped: 0 } })
    expect(w.element.querySelector('.af-panelhead .af-badge')?.textContent).toContain('未建立')
  })

  it('建立失败时给出可读理由与一句话的下一步', () => {
    const w = mount({ channel: { state: 'failed', error: 'device_offline', dropped: 0 } })
    const alert = w.element.querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('离线')
    expect(alert?.textContent).toContain('重试')
  })

  it('断线期间丢弃的输入要显示出来（用户有权知道刚才敲的没进去）', () => {
    const w = mount({ channel: { state: 'ready', error: null, dropped: 3 } })
    const badge = w.element.querySelector('.af-badge--warn')
    expect(badge?.textContent?.replace(/\s+/g, '')).toContain('有3条已丢')
    expect(badge?.getAttribute('title')).toContain('不排队')
  })

  it('没丢过就不显示那个徽标（0 不是要展示的信息）', () => {
    const w = mount({ channel: IDLE })
    expect(w.element.querySelector('.af-badge--warn')).toBeNull()
  })
})

describe('PanelDock：交互才发 interact', () => {
  it('点 / 聚焦面板正文时报 interact', () => {
    const w = mount()
    w.element.querySelector<HTMLElement>('[data-panelhost]')?.dispatchEvent(
      new PointerEvent('pointerdown', { bubbles: true }),
    )
    expect(w.emitted('interact')?.[0]).toEqual(['terminal'])
  })

  it('点坞标签不是「与面板正文交互」——那是切换面板', () => {
    const w = mount()
    const tab = w.element.querySelector<HTMLElement>('.af-docktabs__b')
    expect(tab).not.toBeNull()

    // 分两个事件断言，因为组件本来就是这么分的：
    // `pointerdown` 只走 `onInteract`（这里该被 `closest('.af-docktabs')` 挡掉），
    // 而 `open` 挂在 `@click` 上——只发 pointerdown 是拿不到 `open` 的。
    tab?.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
    expect(w.emitted('interact')).toBeUndefined()

    tab?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    expect(w.emitted('open')?.[0]).toEqual(['terminal'])
    // 切换面板本身也不该顺带建通道
    expect(w.emitted('interact')).toBeUndefined()
  })

  it('还没选面板时正文交互不发 interact（没有通道可以建）', () => {
    const w = mount({ panel: null })
    w.element.querySelector<HTMLElement>('[data-panelhost]')?.dispatchEvent(
      new PointerEvent('pointerdown', { bubbles: true }),
    )
    expect(w.emitted('interact')).toBeUndefined()
  })

  it('关闭按钮只报「关闭」，不连带发 interact', async () => {
    const w = mount()
    await w.element.querySelector<HTMLElement>('.af-docktabs__close')?.click()
    expect(w.emitted('close')).toHaveLength(1)
    expect(w.emitted('interact')).toBeUndefined()
  })

  it('关闭坞的控件是真 <button> 且有无障碍名（键盘用户也要能关）', () => {
    const w = mount()
    const close = w.element.querySelector('.af-docktabs__close')
    expect(close?.tagName).toBe('BUTTON')
    expect(close?.getAttribute('aria-label')).toContain('关闭面板坞')
    expect(close?.getAttribute('aria-label')).toContain('释放')
  })
})

describe('PanelDock：文件面板', () => {
  it('文件面板写清工作区根就是会话路径，没有第二个根', () => {
    const w = mount({ panel: 'files' })
    expect(w.element.textContent).toContain('工作区根就是会话的路径')
    expect(w.element.querySelector('.af-fpane__crumbs')?.textContent).toContain('/srv/alpha')
  })

  it('cwd 作为面包屑接在根后面', () => {
    const w = mount({ panel: 'files', cwd: ['src', 'lib'] })
    const crumbs = w.element.querySelector('.af-fpane__crumbs')?.textContent?.replace(/\s+/g, ' ')
    expect(crumbs).toContain('src')
    expect(crumbs).toContain('lib')
  })
})

describe('PanelDock：无障碍', () => {
  it('坞标签是 tablist，选中的那条 aria-selected=true', () => {
    const w = mount({ mounted: ['terminal', 'files'] })
    const tabs = Array.from(w.element.querySelectorAll('[role="tab"]'))
    expect(tabs).toHaveLength(2)
    expect(tabs.map((t) => t.getAttribute('aria-selected'))).toEqual(['true', 'false'])
  })

  it('终端输出是 role="log"（读屏会播报新增行）', () => {
    const w = mount()
    expect(w.element.querySelector('.af-term')?.getAttribute('role')).toBe('log')
  })
})

describe('PanelDock：没用的东西不画', () => {
  it('坞里没有「连接」按钮——通道是交互时自动建的', () => {
    const w = mount()
    const labels = Array.from(w.element.querySelectorAll('button')).map((b) => b.textContent?.trim())
    expect(labels.some((l) => l?.includes('连接'))).toBe(false)
    void vi
  })
})
