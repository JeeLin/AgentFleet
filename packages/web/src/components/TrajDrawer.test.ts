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
  it('空态明写「M3 起有数据」并解释为什么', async () => {
    scaffold()
    await openDrawer({}, null)

    const empty = drawer().querySelector('.af-empty')
    expect(empty?.textContent).toContain('M3 起有数据')
    expect(empty?.textContent).toContain('trajectory_event')
    expect(empty?.textContent).toContain('不画假行')
  })

  it('不画一条轨迹行：没有 seq / kind 的条目', async () => {
    scaffold()
    await openDrawer({}, null)

    const el = drawer()
    expect(el.querySelectorAll('.af-msg')).toHaveLength(0)
    expect(el.querySelectorAll('li')).toHaveLength(0)
    expect(el.querySelector('.af-traj__body')?.textContent).not.toContain('0ms')
  })

  it('预告 M3 每行的五项字段，但不把它们当成一行数据', async () => {
    scaffold()
    await openDrawer({}, null)

    const foot = drawer().querySelector('.af-traj__foot')
    expect(foot?.textContent).toContain('seq')
    expect(foot?.textContent).toContain('归属')
    expect(foot?.textContent).toContain('本阶段不画这些行')
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
