import { afterEach, beforeEach, describe, expect, it } from 'vitest'

import TrajDrawer from '@/components/TrajDrawer.vue'
import { depth, resetForTest, topContainer } from '@/lib/focus'
import { mountIn } from '@/test/mount'

/**
 * 测试里的页面骨架（规范 §4.1 的浮层形状）：
 *
 *   <main tabindex="-1">        焦点栈的兜底落点
 *     <button id="opener">      触发元素
 *   <div id="behind">           背后内容（开层后必须 inert）
 *
 * 抽屉 Teleport 到 body，所以它自己是 body 的直接子节点——「背后全部 inert」
 * 这条不变量才有意义（留在 #app 里的话整棵树都被当成「容器所在的子树」）。
 */
function scaffold(): { opener: HTMLElement; behind: HTMLElement; behindBtn: HTMLElement } {
  document.body.innerHTML = `
    <main tabindex="-1">
      <button id="opener">轨迹</button>
    </main>
    <div id="behind"><button id="behind-btn">背后</button></div>
  `
  return {
    opener: document.getElementById('opener') as HTMLElement,
    behind: document.getElementById('behind') as HTMLElement,
    behindBtn: document.getElementById('behind-btn') as HTMLElement,
  }
}

function mount(props: Partial<Record<string, unknown>> = {}, opener: HTMLElement | null = null) {
  return mountIn(TrajDrawer, {
    props: {
      open: true,
      opener,
      title: '修 ws 断线重连',
      deviceName: 'mbp.local',
      path: '/srv/alpha',
      ...props,
    },
  })
}

/** 打开抽屉：等 Teleport + push 那一拍（watch 里走的是微任务）。 */
async function openDrawer(
  props: Partial<Record<string, unknown>> = {},
  opener: HTMLElement | null = null,
) {
  const w = mount(props, opener)
  await Promise.resolve()
  await w.vm.$nextTick()
  return w
}

function drawer(doc: Document = document): HTMLElement {
  const el = doc.querySelector<HTMLElement>('[data-traj]')
  if (!el) throw new Error('drawer not rendered')
  return el
}

function press(key: string): void {
  document.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }))
}

beforeEach(() => {
  resetForTest()
  document.body.innerHTML = ''
})

afterEach(() => {
  resetForTest()
  document.body.innerHTML = ''
})

describe('TrajDrawer：是抽屉不是第三栏', () => {
  it('挂到 body 上的覆盖层：`role="dialog"` + `aria-modal`，不带分栏语义', async () => {
    const s = scaffold()
    const w = await openDrawer({}, s.opener)

    const el = drawer()
    expect(el.getAttribute('role')).toBe('dialog')
    expect(el.getAttribute('aria-modal')).toBe('true')
    expect(el.parentElement).toBe(document.body)
    // 没有 role="separator" / 没有把手：它不参与分栏
    expect(el.querySelector('[role="separator"]')).toBeNull()
    void w
  })

  it('Teleport 到 body 才能让背后真的 inert（留在页面里则整棵树都被当成容器内）', async () => {
    const s = scaffold()
    await openDrawer({}, s.opener)

    // `inert` 是原生特性：真浏览器里它沿着祖先**继承**到整棵子树。
    // happy-dom 与测试 setup 里的 polyfill 只在被赋值的那个元素上成立，
    // 所以断言落在「body 的直接子节点里，除抽屉之外都带了 inert」——
    // 那是焦点栈真正写下去的东西，也是「背后被关掉」的判据。
    expect(s.behind.inert).toBe(true)
    expect(drawer().inert).toBe(false)
    const inerted = Array.from(document.body.children).filter((el) => el !== drawer())
    expect(inerted.length).toBeGreaterThan(0)
    for (const el of inerted) expect((el as HTMLElement).inert).toBe(true)
  })
})

describe('TrajDrawer：开合走焦点栈', () => {
  it('打开即入栈，首焦点落在关闭按钮上', async () => {
    const s = scaffold()
    s.opener.focus()
    await openDrawer({}, s.opener)

    expect(depth()).toBe(1)
    expect(topContainer()).toBe(drawer())
    expect(document.activeElement).toBe(drawer().querySelector('[data-traj-close]'))
  })

  it('opener 为 null 时入栈不报错（焦点栈会退到 main）', async () => {
    scaffold()
    await openDrawer({}, null)
    expect(depth()).toBe(1)
  })

  it('Tab 被圈在抽屉里（§3.10）', async () => {
    scaffold()
    await openDrawer({}, null)

    const el = drawer()
    press('Tab')
    // 抽屉里只有一个可聚焦元素（关闭按钮）：Tab 不该把它带出去
    expect(el.contains(document.activeElement)).toBe(true)
    expect(depth()).toBe(1)
  })
})

describe('TrajDrawer：三条关闭路径，焦点都回到触发元素', () => {
  it('Esc 关闭 → 焦点回到触发元素', async () => {
    const s = scaffold()
    s.opener.focus()
    const w = await openDrawer({}, s.opener)
    expect(document.activeElement).not.toBe(s.opener)

    press('Escape')
    await w.vm.$nextTick()

    expect(w.emitted('close')).toHaveLength(1)
    expect(depth()).toBe(0)
    expect(document.activeElement).toBe(s.opener)
    expect(s.behind.inert).toBe(false)
  })

  it('点关闭按钮 → 焦点回到触发元素', async () => {
    const s = scaffold()
    s.opener.focus()
    const w = await openDrawer({}, s.opener)

    await drawer().querySelector<HTMLElement>('[data-traj-close]')?.click()
    await w.vm.$nextTick()

    expect(w.emitted('close')).toHaveLength(1)
    expect(depth()).toBe(0)
    expect(document.activeElement).toBe(s.opener)
  })

  it('点遮罩 → 焦点回到触发元素', async () => {
    const s = scaffold()
    s.opener.focus()
    const w = await openDrawer({}, s.opener)

    await drawer().click()
    await w.vm.$nextTick()

    expect(w.emitted('close')).toHaveLength(1)
    expect(depth()).toBe(0)
    expect(document.activeElement).toBe(s.opener)
  })

  it('点在抽屉**里**不关闭（只有点在遮罩自己上才算）', async () => {
    const s = scaffold()
    const w = await openDrawer({}, s.opener)

    drawer().querySelector<HTMLElement>('.af-traj__panel')?.click()
    await w.vm.$nextTick()

    expect(w.emitted('close')).toBeUndefined()
    expect(depth()).toBe(1)
  })

  it('关闭按钮是真 <button> 且有无障碍名（键盘用户也要能关）', async () => {
    scaffold()
    await openDrawer({}, null)
    const close = drawer().querySelector('[data-traj-close]')
    expect(close?.tagName).toBe('BUTTON')
    expect(close?.getAttribute('aria-label')).toContain('关闭轨迹抽屉')
  })
})

describe('TrajDrawer：父级自己把 open 置回 false 也要出栈', () => {
  it('不 watch 这条路径的话焦点栈上会留一个已经不存在的层', async () => {
    const s = scaffold()
    s.opener.focus()
    const w = await openDrawer({}, s.opener)
    expect(depth()).toBe(1)

    await w.setProps({ open: false })
    await w.vm.$nextTick()

    expect(depth()).toBe(0)
    expect(document.activeElement).toBe(s.opener)
  })

  it('开着就卸载：走完整的关闭路径，不把焦点留在 body 上', async () => {
    const s = scaffold()
    s.opener.focus()
    const w = await openDrawer({}, s.opener)

    w.unmount()

    expect(depth()).toBe(0)
    expect(document.activeElement).toBe(s.opener)
  })
})

describe('TrajDrawer：空态不含假行', () => {
  it('空态明写「还没有事件」并解释什么是轨迹', async () => {
    scaffold()
    await openDrawer({}, null)

    const empty = drawer().querySelector('.af-empty')
    expect(empty?.textContent).toContain('还没有事件')
    // 「轨迹是什么」要说清楚：它是 append-only 流水，而对话是它的折叠读法。
    // 不说这句，用户会去比「对话里有几条、轨迹里有几条」——本该逐条对齐的两栏
    // 被当成了两套数据。
    expect(empty?.textContent).toContain('trajectory_event')
    expect(empty?.textContent).toContain('同一份数据的两种看法')
    // M1 那句「M3 起有数据」已经不成立了：轨迹现在真的会写。
    expect(empty?.textContent).not.toContain('M3 起有数据')
  })

  it('没有事件时不画一条轨迹行（不编一条假的来填空白）', async () => {
    scaffold()
    await openDrawer({}, null)

    const el = drawer()
    expect(el.querySelectorAll('[data-traj-row]')).toHaveLength(0)
    expect(el.querySelectorAll('[data-traj-gap]')).toHaveLength(0)
  })

  it('读失败时说清原因，而不是给一个空抽屉', async () => {
    scaffold()
    await openDrawer({ error: '会话不存在' }, null)

    const err = drawer().querySelector('.af-traj__error')
    expect(err?.getAttribute('role')).toBe('alert')
    expect(err?.textContent).toContain('会话不存在')
    // 空态不该同时出现：那是「没有轨迹」的意思，与「读不到」不是一回事
    expect(drawer().querySelector('.af-empty')).toBeNull()
  })
})

describe('TrajDrawer：把轨迹事件画成行', () => {
  /** 造一条轨迹事件。 */
  function ev(id: number, seq: number, kind: string, payload: unknown = {}) {
    return { id, seq, kind, payload: JSON.stringify(payload), created_at: '2026-10-26 10:00:00' }
  }

  it('每行带 seq / kind / payload，条数写在脚注里', async () => {
    scaffold()
    await openDrawer(
      {
        events: [
          ev(1, 1, 'agent_message_chunk', { content: [{ type: 'text', text: 'hi' }] }),
          ev(2, 2, 'turn_end', { stopReason: 'end_turn' }),
        ],
      },
      null,
    )

    const el = drawer()
    expect(el.querySelectorAll('[data-traj-row]')).toHaveLength(2)
    expect(el.textContent).toContain('agent_message_chunk')
    expect(el.textContent).toContain('turn_end')
    expect(el.querySelector('.af-traj__foot')?.textContent).toContain('2 条')
  })

  it('断口画在缺的那截上，写明缺几条', async () => {
    scaffold()
    // seq 1、2 在，5 在：缺的是 3、4
    await openDrawer(
      {
        events: [ev(1, 1, 'agent_message_chunk'), ev(2, 2, 'agent_message_chunk'), ev(3, 5, 'turn_end')],
        gaps: [{ from: 3, to: 5, missing: 2 }],
      },
      null,
    )

    const gap = drawer().querySelector('[data-traj-gap]')
    // 先断言它画出来了：按 `from` 索引会一个都不画（那条 seq 本就不在库里），
    // 而页面看起来就是连续的——正是这个功能要防的事。
    expect(gap, '断口标记必须画出来').not.toBeNull()
    expect(gap?.textContent).toContain('缺失 2 条')
    expect(gap?.textContent).toContain('seq 2 之后断开')
    expect(drawer().querySelector('.af-traj__foot')?.textContent).toContain('断口 1 处')
  })

  it('没有断口就不画断口标记——不谎报', async () => {
    scaffold()
    await openDrawer({ events: [ev(1, 1, 'turn_end')], gaps: [] }, null)
    expect(drawer().querySelectorAll('[data-traj-gap]')).toHaveLength(0)
  })

  it('末轮没收口 → 写「本轮未完成」', async () => {
    scaffold()
    await openDrawer({ events: [ev(1, 1, 'agent_message_chunk')], incomplete: true }, null)

    const el = drawer().querySelector('[data-traj-incomplete]')
    expect(el?.textContent).toContain('本轮未完成')
    expect(el?.textContent).toContain('turn_end')
  })

  it('有更多时给「加载更多」按钮，正在取时禁用它', async () => {
    scaffold()
    const w = await openDrawer({ events: [ev(1, 1, 'turn_end')], hasMore: true }, null)

    const btn = drawer().querySelector<HTMLButtonElement>('[data-traj-more]')
    expect(btn, 'hasMore 为真时必须有按钮').not.toBeNull()
    expect(btn?.disabled).toBe(false)
    await btn?.click()
    expect(w.emitted('more')).toHaveLength(1)

    await w.setProps({ loading: true })
    expect(drawer().querySelector<HTMLButtonElement>('[data-traj-more]')?.disabled).toBe(true)
  })

  it('没有更多时不给按钮——不给一个按下去没有反应的东西', async () => {
    scaffold()
    await openDrawer({ events: [ev(1, 1, 'turn_end')], hasMore: false }, null)
    expect(drawer().querySelector('[data-traj-more]')).toBeNull()
  })

  it('payload 过长时截断显示（只影响显示，不影响数据）', async () => {
    scaffold()
    const long = 'x'.repeat(400)
    await openDrawer({ events: [{ id: 1, seq: 1, kind: 'tool_call', payload: long, created_at: '' }] }, null)

    const shown = drawer().querySelector('.af-traj__payload')?.textContent ?? ''
    expect(shown.length).toBeLessThan(140)
    expect(shown).toContain('…')
  })
})

describe('TrajDrawer：抽屉头重复一次身份', () => {
  it('设备与路径在抽屉头重复（两个会话并排时靠它分）', async () => {
    scaffold()
    await openDrawer({}, null)

    const head = drawer().querySelector('.af-traj__head')
    expect(head?.textContent).toContain('mbp.local')
    expect(head?.textContent).toContain('/srv/alpha')
    expect(head?.textContent).toContain('修 ws 断线重连')
  })
})
