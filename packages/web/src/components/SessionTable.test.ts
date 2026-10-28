import { describe, expect, it, vi } from 'vitest'

import SessionTable from '@/components/SessionTable.vue'
import { mountIn, session } from '@/test/mount'

function rows(wrapper: ReturnType<typeof mountIn>): HTMLElement[] {
  return Array.from(wrapper.element.querySelectorAll<HTMLElement>('tbody [data-row]'))
}

function press(el: HTMLElement, key: string): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
  el.dispatchEvent(e)
  return e
}

const SESSIONS = [
  session({ id: 's_1', workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true } }),
  session({ id: 's_2', workspace: { node_id: 'n_1', path: '/srv/beta', path_locked: true } }),
  session({ id: 's_3', state: 'ended', workspace: { node_id: 'n_2', path: '/srv/gamma', path_locked: true } }),
]

function mountTable(onOpen = vi.fn()) {
  const w = mountIn(SessionTable, {
    props: { sessions: SESSIONS, deviceName: (id: string | null) => (id === 'n_1' ? 'mbp.local' : (id ?? '')), onOpen },
  })
  return { w, onOpen }
}

describe('SessionTable：列表导航（↑↓ / Home / End / Enter）', () => {
  it('容器是唯一的 Tab 落点，行是 tabindex="-1"', () => {
    const { w } = mountTable()
    expect(w.element.querySelector('tbody')?.getAttribute('tabindex')).toBe('0')
    for (const row of rows(w)) expect(row.getAttribute('tabindex')).toBe('-1')
  })

  it('↓ 走到下一行并写 aria-selected', () => {
    const { w } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'ArrowDown')
    expect(document.activeElement).toBe(rows(w)[1])
    expect(rows(w)[1]?.getAttribute('aria-selected')).toBe('true')
    expect(rows(w)[0]?.getAttribute('aria-selected')).toBe('false')
  })

  it('↑ 在首行不越界', () => {
    const { w } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'ArrowUp')
    expect(document.activeElement).toBe(first)
  })

  it('End 到最后一行，Home 回第一行', () => {
    const { w } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'End')
    expect(document.activeElement).toBe(rows(w)[2])
    press(rows(w)[2] as HTMLElement, 'Home')
    expect(document.activeElement).toBe(rows(w)[0])
  })

  it('⏎ 进入这一行的会话', () => {
    const { w, onOpen } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'Enter')
    expect(onOpen).toHaveBeenCalledTimes(1)
    expect(onOpen.mock.calls[0]?.[0]).toMatchObject({ id: 's_1' })
  })
})

describe('SessionTable：会话的显示口径', () => {
  it('标题来自工作区路径末段，后端没有 title 字段', () => {
    const { w } = mountTable()
    const titles = Array.from(w.element.querySelectorAll('.af-link')).map((el) => el.textContent?.trim())
    expect(titles).toEqual(['alpha', 'beta', 'gamma'])
  })

  it('设备列用设备名，查不到时原样显示 node_id', () => {
    const { w } = mountTable()
    const text = w.element.textContent ?? ''
    expect(text).toContain('mbp.local')
    expect(text).toContain('n_2')
  })

  it('没有选 agent 的会话：agent 列显示「—」并说明原因', () => {
    const { w } = mountTable()
    const dash = w.element.querySelector('[title*="没有选 agent"]')
    expect(dash?.textContent?.trim()).toBe('—')
    expect(dash?.getAttribute('title')).toBe('建会话时没有选 agent，这个会话不能对话')
  })

  it('会话 id 与路径都是普通文本，没有第二条工作区入口', () => {
    const { w } = mountTable()
    const links = Array.from(w.element.querySelectorAll('a'))
    expect(links).toHaveLength(0)
    expect(w.element.textContent).toContain('s_1')
    expect(w.element.textContent).toContain('/srv/alpha')
  })
})
