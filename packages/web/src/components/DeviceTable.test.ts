import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it, vi } from 'vitest'

import DeviceTable from '@/components/DeviceTable.vue'
import { device, mountIn } from '@/test/mount'

function rows(wrapper: ReturnType<typeof mountIn>): HTMLElement[] {
  return Array.from(wrapper.element.querySelectorAll<HTMLElement>('tbody [data-row]'))
}

function press(el: HTMLElement, key: string): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
  el.dispatchEvent(e)
  return e
}

describe('DeviceTable：列表导航（↑↓ / Home / End / Enter）', () => {
  const devices = [
    device({ id: 'n_1', name: 'a' }),
    device({ id: 'n_2', name: 'b' }),
    device({ id: 'n_3', name: 'c' }),
  ]

  function mountTable(onInspect = vi.fn(), onCreate = vi.fn()) {
    const w = mountIn(DeviceTable, {
      props: { devices, onInspect, onCreate },
    })
    return { w, onInspect, onCreate }
  }

  it('容器是唯一的 Tab 落点，行是 tabindex="-1"', () => {
    const { w } = mountTable()
    expect(w.element.querySelector('tbody')?.getAttribute('tabindex')).toBe('0')
    for (const row of rows(w)) expect(row.getAttribute('tabindex')).toBe('-1')
  })

  it('↓ 从第一行走到第二行并写 aria-selected', () => {
    const { w } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'ArrowDown')
    expect(document.activeElement).toBe(rows(w)[1])
    expect(rows(w)[1]?.getAttribute('aria-selected')).toBe('true')
    expect(rows(w)[0]?.getAttribute('aria-selected')).toBe('false')
  })

  it('↑ 在首行不越界（不绕回末行）', () => {
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

  it('⏎ 是「看这一台」，不是跳走（设备页的多数访问就是扫一眼）', () => {
    const { w, onInspect, onCreate } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'Enter')
    expect(onInspect).toHaveBeenCalledTimes(1)
    expect(onInspect.mock.calls[0]?.[0]).toMatchObject({ id: 'n_1' })
    expect(onCreate).not.toHaveBeenCalled()
  })

  it('↓ 在行间走时键盘用户不会先 Tab 过每行的按钮', () => {
    const { w } = mountTable()
    const first = rows(w)[0] as HTMLElement
    first.focus()
    press(first, 'ArrowDown')
    // 焦点在行上，不在行内的按钮上
    expect((document.activeElement as HTMLElement).hasAttribute('data-row')).toBe(true)
  })
})

describe('DeviceTable：离线设备', () => {
  it('离线行仍可选中（降亮但不禁用）——排障要复制它的 node_id', () => {
    const w = mountIn(DeviceTable, {
      props: {
        devices: [device({ id: 'n_off', name: 'offline', status: 'offline' })],
        onInspect: vi.fn(),
        onCreate: vi.fn(),
      },
    })
    const row = rows(w)[0] as HTMLElement
    expect(row.getAttribute('data-inert')).toBe('true')
    row.focus()
    expect(document.activeElement).toBe(row)
  })

  it('离线设备的「用它开会话」被禁用，且旁边写清为什么', () => {
    const w = mountIn(DeviceTable, {
      props: {
        devices: [device({ id: 'n_off', status: 'offline' })],
        onInspect: vi.fn(),
        onCreate: vi.fn(),
      },
    })
    const btn = w.element.querySelector<HTMLButtonElement>('tbody button')
    expect(btn?.disabled).toBe(true)
    expect(btn?.getAttribute('aria-disabled')).toBe('true')
    expect(btn?.getAttribute('title')).toContain('45 秒')
    expect(btn?.getAttribute('title')).toContain('重新上线')
  })

  it('重连中的设备可以建会话（45 秒内 PTY 还活着）', () => {
    const w = mountIn(DeviceTable, {
      props: {
        devices: [device({ id: 'n_rc', status: 'reconnecting' })],
        onInspect: vi.fn(),
        onCreate: vi.fn(),
      },
    })
    const btn = w.element.querySelector<HTMLButtonElement>('tbody button')
    expect(btn?.disabled).toBe(false)
  })

  it('点「用它开会话」触发 onCreate，不触发 onInspect', async () => {
    const onInspect = vi.fn()
    const onCreate = vi.fn()
    const w = mountIn(DeviceTable, {
      props: { devices: [device({ id: 'n_1' })], onInspect, onCreate },
    })
    await w.element.querySelector<HTMLButtonElement>('tbody button')?.click()
    expect(onCreate).toHaveBeenCalledTimes(1)
    expect(onInspect).not.toHaveBeenCalled()
  })
})

describe('DeviceTable：未上报与推值', () => {
  it('cpu / mem 未上报显示「—」，不是 0%', () => {
    const w = mountIn(DeviceTable, {
      props: {
        devices: [device({ cpu: null, mem: null })],
        onInspect: vi.fn(),
        onCreate: vi.fn(),
      },
    })
    const cells = Array.from(w.element.querySelectorAll('td[data-num]')).map((c) => c.textContent?.trim())
    expect(cells).toContain('—')
    expect(cells).not.toContain('0%')
  })

  it('未上报 agent 列表时明写「未上报」，不留空', () => {
    const w = mountIn(DeviceTable, {
      props: { devices: [device({ agents: [] })], onInspect: vi.fn(), onCreate: vi.fn() },
    })
    expect(w.element.textContent).toContain('未上报')
  })

  /**
   * **装了但跑不起来的要留着，只是不带强调色**。
   *
   * 两档不分的话，用户看到的是一串同样颜色的名字，而其中一半点进去会
   * 409——「为什么我的 agent 不能对话」这条事实在页面上完全不可见。
   */
  it('已装 agent 分「能对话 / 不能对话」两档，且不删掉坏的那个', async () => {
    const w = mountIn(DeviceTable, {
      props: {
        devices: [device({ agents: ['opencode', 'claude'], usable_agents: ['opencode'] })],
        onInspect: vi.fn(),
        onCreate: vi.fn(),
      },
    })
    const badges = Array.from(w.element.querySelectorAll<HTMLElement>('.af-caps .af-badge'))

    // 不删：删了就回到「用户以为系统没扫到」
    expect(badges.map((b) => b.textContent?.trim())).toEqual(['opencode', 'claude'])

    const usable = badges.find((b) => b.textContent?.trim() === 'opencode')
    expect(usable?.getAttribute('data-usable')).toBe('true')
    expect(usable?.className).toContain('af-badge--agent')

    const broken = badges.find((b) => b.textContent?.trim() === 'claude')
    expect(broken?.getAttribute('data-usable')).toBe('false')
    expect(broken?.className).toContain('af-badge--off')
    // 悬停要能读到「为什么」——只在旁边灰一块是不够的
    expect(broken?.getAttribute('title')).toContain('不能对话')
  })

  it('derived 为真时给「推」徽标（状态来源比状态本身更容易被误读）', () => {
    const w = mountIn(DeviceTable, {
      props: {
        devices: [device({ status: 'reconnecting', derived: true })],
        onInspect: vi.fn(),
        onCreate: vi.fn(),
      },
    })
    expect(w.element.textContent).toContain('推')
  })
})

describe('DeviceTable：设备名与指纹这一格的竖向对齐', () => {
  // jsdom 不布局，断言 getComputedStyle 的 align-items 会是假测试——
  // 所以从组件源码取证：这一格必须是视觉居中，且名字必须可收缩。
  const src = readFileSync(
    resolve(dirname(fileURLToPath(import.meta.url)), './DeviceTable.vue'),
    'utf8',
  )

  it('设备名格用视觉居中对齐，不用 baseline（两个 span 不同字号）', () => {
    expect(src).toMatch(/\.af-dev-name\s*\{[^}]*align-items:\s*center/)
  })

  it('名字可收缩：长名字走省略号，不把整列撑宽去挤别的列', () => {
    expect(src).toMatch(/\.af-dev-name__name\s*\{[^}]*min-width:\s*0/)
  })
})
