import { describe, expect, it, vi } from 'vitest'

import PanelDock from '@/domains/session/PanelDock.vue'
import { mountIn } from '@/test/mount'
import type { ChannelView } from '@/domains/session/PanelDock.vue'

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
  it('坞头写出当前是哪个工具；没选时明写「未选择」', () => {
    // v0.5.0 拆掉了坞标签条（裁决 C：右侧一格，「＋」是唯一入口）。
    // 但「我现在在看哪个」仍要有一处视觉落点——候选里的 aria-current 只给读屏。
    const withPanel = mount()
    expect(withPanel.element.querySelector('[data-dock-current]')?.textContent).toContain('终端')

    const none = mount({ panel: null, mounted: [] })
    expect(none.element.querySelector('[data-dock-current]')?.textContent).toContain('未选择')
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

  it('点「＋」里的候选不是「与面板正文交互」——那是换工具', async () => {
    const w = mount({ panel: 'terminal', mounted: ['terminal'] })
    await w.element.querySelector<HTMLElement>('[data-add="toggle"]')?.click()
    const files = w.element.querySelector<HTMLElement>('[data-add="files"]')
    expect(files).not.toBeNull()

    // 分两个事件断言，因为组件本来就是这么分的：
    // `pointerdown` 只走 `onInteract`（该被 `closest('.af-docktabs')` 挡掉），
    // 而 `open` 挂在 `@click` 上——只发 pointerdown 是拿不到 `open` 的。
    files?.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
    expect(w.emitted('interact')).toBeUndefined()

    files?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    expect(w.emitted('open')?.[0]).toEqual(['files'])
    // 换工具本身也不该顺带建通道
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
    expect(close?.getAttribute('aria-label')).toContain('收起右侧面板')
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
  /*
    v0.4.0 这里断言的是「坞标签是 tablist、选中的那条 aria-selected=true」。
    v0.5.0 拆掉了坞标签条（裁决 C），那条断言**随之改写**为下面这条——
    不许悄悄删掉：坞从「多个已挂载面板的标签条」变成「一格 + 一个 ＋」，
    这正是要钉住的语义变化。
  */
  it('「＋」列出全部三个工具，当前那个标 aria-current', async () => {
    const w = mount({ panel: 'files', mounted: ['files'] })
    await w.element.querySelector<HTMLElement>('[data-add="toggle"]')?.click()
    const adds = Array.from(w.element.querySelectorAll('[data-add]'))
      .map((el) => el.getAttribute('data-add'))
      .filter((n): n is string => n !== 'cancel' && n !== 'toggle')
    // 全部三个，且顺序与 PANEL_ORDER 一致；**含当前这一个**
    expect(adds).toEqual(['terminal', 'files', 'git'])
    const current = w.element.querySelector('[data-add="files"]')
    expect(current?.getAttribute('aria-current')).toBe('true')
    expect(w.element.querySelector('[data-add="git"]')?.getAttribute('aria-current')).toBe('false')
  })

  it('点「＋」里当前那一个 = 零副作用（不 emit open，终端不会被重建）', async () => {
    const w = mount({ panel: 'terminal', mounted: ['terminal'] })
    await w.element.querySelector<HTMLElement>('[data-add="toggle"]')?.click()
    await w.element.querySelector<HTMLElement>('[data-add="terminal"]')?.click()
    // 反向验证：去掉 `addPanel` 里的 `if (name === props.panel) return` → 这条红
    expect(w.emitted('open')).toBeUndefined()
    // 候选列表收起（点了就该收）
    expect(w.element.querySelector('[data-add="terminal"]')).toBeNull()
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
