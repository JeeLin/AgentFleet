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
    // **只查「能装字符串的那些 input」**（text / password / email / …）：
    // `input[type=checkbox].value` 在 DOM 里恒为 `'on'`，那不是一个被回显的值，
    // 而是浏览器的缺省属性。原来那条 `input` 全量选择器在页面只有文本框时
    // 恰好等价，加了通知出口的开关之后就不等价了——修选择器，不修断言口径。
    const values = Array.from(
      w.element.querySelectorAll<HTMLInputElement>(
        'input:not([type=checkbox]):not([type=radio])',
      ),
    ).map((i) => i.value)
    expect(values.length).toBeGreaterThan(0)
    for (const v of values) expect(v).toBe('')
  })

  it('不列 M5+ 的开关（留一个灰掉的导航项等于在导航里承诺）', () => {
    const w = mountIn(SettingsPage)
    const text = w.element.textContent ?? ''
    expect(text).toContain('任务分发')
    expect(text).toContain('推理网关')
    expect(text).toContain('M5+')
    // 页面上没有任何可操作控件（只有指向设备页的那一个链接按钮）。
    //
    // **选择器从 `select` 改成 `select:not([disabled])`**（v0.8.0 子任务 12）：
    // 原来那条 `select` 不带 `:not([disabled])`，于是任何一个**禁用**的下拉
    // （磁盘默认清理策略，原型 `settings.html:126` 就有）都会被算成可操作控件。
    // 这条用例的意图是「没有能点的东西」，禁用控件点不动，不该算进来——
    // 这不是放宽口径，是把选择器修回它本来要表达的意思。
    const controls = Array.from(
      w.element.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled])',
      ),
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

/**
 * 原型五个分区（v0.8.0 子任务 12，里程碑测试标准 3）。
 *
 * `prototype/settings.html` 有五个 `<h2>`：合并权（`:31`）/ 决策超时（`:82`）/
 * 磁盘（`:111`）/ 通知出口（`:148`）/ 外观（`:186`）。此前这一页一个都没有。
 *
 * **表驱动**：缺一个即红，且红的时候直接看得出缺的是哪一个。
 *
 * ── 为什么用 `includes` 之外还钉「精确标题」 ────────────────────────
 * 实测过一次：把「外观」改成「外观-DISABLED」，`includes` 版本**仍然全绿**
 * ——因为 `'外观-DISABLED'.includes('外观')` 为真。那样这条断言就挡不住
 * 「分区还在但内容被掏空 / 改名」这类退化，而它恰恰是这次要防的东西。
 * 所以这里同时断言 `EXPECTED_TITLES` 里那个**完整标题字符串**逐字存在。
 */
const PROTOTYPE_SECTIONS = ['合并权', '决策请求超时', '磁盘', '通知出口', '外观'] as const

/**
 * 五个分区的**完整标题原文**（照 `prototype/settings.html` 的 `<h2>` 逐字）。
 * 与上面的短名分开：短名给人读（失败信息里说「缺磁盘」），这一份给断言用。
 */
const EXPECTED_TITLES = [
  'AgentFleet 的合并权',
  '决策请求超时之后怎么处理',
  '磁盘',
  '通知出口',
  '外观',
] as const

describe('SettingsPage：原型的五个分区', () => {
  it('五个分区标题逐字存在（精确匹配，不吃「包含」的便宜）', () => {
    const w = mountIn(SettingsPage)
    const titles = Array.from(w.element.querySelectorAll('.af-region__title')).map(
      (el) => el.textContent?.trim() ?? '',
    )
    for (const t of EXPECTED_TITLES) {
      expect(titles, `分区标题里没有「${t}」，实际是 ${JSON.stringify(titles)}`).toContain(t)
    }
  })

  for (const title of PROTOTYPE_SECTIONS) {
    it(`分区「${title}」存在`, () => {
      const w = mountIn(SettingsPage)
      const titles = Array.from(w.element.querySelectorAll('.af-region__title')).map(
        (el) => el.textContent?.trim() ?? '',
      )
      expect(
        titles.some((t) => t.includes(title)),
        `分区标题里找不到「${title}」，实际是 ${JSON.stringify(titles)}`,
      ).toBe(true)
    })
  }

  it('五个分区都在这，且都还没有可操作控件', () => {
    const w = mountIn(SettingsPage)
    const titles = Array.from(w.element.querySelectorAll('.af-region__title')).map(
      (el) => el.textContent?.trim() ?? '',
    )
    for (const t of PROTOTYPE_SECTIONS) {
      expect(titles.some((x) => x.includes(t)), `缺分区「${t}」`).toBe(true)
    }
    // 原型合并权是三档、默认最保守（`settings.html:45`）。档 0 要标「当前」，
    // 否则用户看不出自己现在在哪一档。
    const text = w.element.textContent ?? ''
    expect(text).toContain('档 0')
    expect(text).toContain('当前')
    expect(text).toContain('已选中')
    // 不可逆操作不自动执行：这是「决策超时」那一块的全部要点。
    expect(text).toContain('保持中止，不自动清理')
  })
})
