import { describe, expect, it } from 'vitest'

import SettingsPage from '@/pages/SettingsPage.vue'
import { mountIn } from '@/test/mount'

describe('SettingsPage：不提假的承诺', () => {
  it('每一项都是禁用控件，旁边写明 M2 会做什么', () => {
    const w = mountIn(SettingsPage)

    const inputs = Array.from(w.element.querySelectorAll<HTMLInputElement>('input'))
    expect(inputs.length).toBeGreaterThan(0)
    for (const input of inputs) expect(input.disabled).toBe(true)

    const text = w.element.textContent ?? ''
    expect(text).toContain('M2 范围')
    expect(text).toContain('没有任何一项现在能改')
    expect(text).toContain('M2 起可改')
  })

  it('不显示任何 secret 值（约束 13：永不回显）', () => {
    const w = mountIn(SettingsPage)
    const values = Array.from(w.element.querySelectorAll<HTMLInputElement>('input')).map((i) => i.value)
    for (const v of values) expect(v).toBe('')
  })

  it('不列 M5+ 的开关（留一个灰掉的导航项等于在导航里承诺）', () => {
    const w = mountIn(SettingsPage)
    const text = w.element.textContent ?? ''
    expect(text).toContain('任务分发')
    expect(text).toContain('推理网关')
    expect(text).toContain('M5+')
    // 页面上没有任何可操作控件（只有指向设备页的那一个链接按钮）
    const controls = Array.from(
      w.element.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), select, textarea'),
    )
    expect(controls.map((el) => el.textContent?.trim() ?? el.tagName)).toEqual(['设备页'])
  })

  it('指出去哪儿看真正的凭据：设备页上的指纹', async () => {
    const w = mountIn(SettingsPage)
    const link = w.element.querySelector<HTMLButtonElement>('.af-link')
    expect(link?.textContent).toContain('设备页')
    await link?.click()
    expect(location.pathname).toBe('/devices')
  })
})
