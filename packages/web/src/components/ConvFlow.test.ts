import { describe, expect, it } from 'vitest'

import ConvFlow from '@/components/ConvFlow.vue'
import type { ConvExecRequest } from '@/components/ConvFlow.vue'
import { mountIn } from '@/test/mount'

function mount(props: Partial<Record<string, unknown>> = {}) {
  return mountIn(ConvFlow, {
    props: { deviceName: 'mbp.local', path: '/srv/alpha', ...props },
  })
}

function input(w: ReturnType<typeof mount>) {
  return w.find<HTMLTextAreaElement>('[data-conv-input]')
}

async function type(w: ReturnType<typeof mount>, text: string) {
  await input(w).setValue(text)
  await w.vm.$nextTick()
}

/** 提交表单（等价于点「执行」）。 */
async function submit(w: ReturnType<typeof mount>) {
  await w.find('form').trigger('submit')
  await w.vm.$nextTick()
}

describe('ConvFlow：空态是明写出来的，不是「看起来空掉了」', () => {
  it('空态标题明写「M3 起有数据」，并解释为什么空', () => {
    const w = mount()
    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('M3 起有数据')
    expect(empty?.textContent).toContain('trajectory_event')
    expect(empty?.textContent).toContain('不画假消息')
  })

  it('顶部横幅写清 M1 无数据的原因——那是 AgentsPage 那条横幅的同一套语言', () => {
    const w = mount()
    const note = w.element.querySelector('.af-stage-note')
    expect(note?.getAttribute('role')).toBe('note')
    expect(note?.textContent).toContain('M1 无数据')
    expect(note?.textContent).toContain('ACP 归 M3')
  })

  it('不画一条假消息：没有 .af-msg，也没有工具块', () => {
    const w = mount()
    expect(w.element.querySelectorAll('.af-msg')).toHaveLength(0)
    expect(w.element.querySelectorAll('.af-tool')).toHaveLength(0)
    expect(w.element.textContent).not.toContain('User')
  })

  it('对话区是 role="log"（M3 追加消息时读屏会播报）', () => {
    const w = mount()
    expect(w.element.querySelector('[role="log"]')?.getAttribute('aria-label')).toContain('M3 起有数据')
  })
})

describe('ConvFlow：输入区能真的触发 exec', () => {
  it('执行一行命令 = 发一个 exec 请求（command + args），cwd 恒空', async () => {
    const w = mount()
    await type(w, 'git status --short')
    await submit(w)

    expect(w.emitted('run')?.[0]).toEqual([
      { command: 'git', args: ['status', '--short'], cwd: '' } satisfies ConvExecRequest,
    ])
  })

  it('引号只用来把带空格的参数合成一个——不是 shell', async () => {
    const w = mount()
    await type(w, `grep -n "two words" src/lib`)
    await submit(w)

    expect(w.emitted('run')?.[0]?.[0]).toEqual({
      command: 'grep',
      args: ['-n', 'two words', 'src/lib'],
      cwd: '',
    })
  })

  it('提示里写明没有 shell（管道与重定向不会生效）', () => {
    const w = mount()
    expect(input(w).element.placeholder).toContain('不是 shell')
    expect(w.element.textContent).toContain('管道与重定向')
  })

  it('⏎ 执行', async () => {
    const w = mount()
    await type(w, 'ls')
    await input(w).trigger('keydown', { key: 'Enter' })
    await w.vm.$nextTick()
    expect(w.emitted('run')?.[0]?.[0]).toMatchObject({ command: 'ls' })
  })

  it('⇧⏎ 不执行（那是换行）', async () => {
    const w = mount()
    await type(w, 'ls')
    await input(w).trigger('keydown', { key: 'Enter', shiftKey: true })
    await w.vm.$nextTick()
    expect(w.emitted('run')).toBeUndefined()
  })

  it('输入法选字期间的 ⏎ 不执行（那是把候选词上屏）', async () => {
    const w = mount()
    await type(w, 'ls')
    await input(w).trigger('keydown', { key: 'Enter', isComposing: true })
    await w.vm.$nextTick()
    expect(w.emitted('run')).toBeUndefined()
  })

  it('执行后清空输入框——不留一行「到底跑了没有」的悬念', async () => {
    const w = mount()
    await type(w, 'ls')
    await submit(w)
    expect(input(w).element.value).toBe('')
  })

  it('空输入不发请求，按钮也是禁用的', async () => {
    const w = mount()
    expect(w.find<HTMLButtonElement>('[data-conv-run]').element.disabled).toBe(true)
    await type(w, '   ')
    await submit(w)
    expect(w.emitted('run')).toBeUndefined()
  })

  it('目标切到终端时发的是一段文本，且补一个换行（等于在终端里回车）', async () => {
    const w = mount({ terminalReady: true })
    const select = w.find<HTMLSelectElement>('[data-conv-target]')
    await select.setValue('terminal')
    await type(w, 'cargo test')
    await submit(w)

    expect(w.emitted('send')?.[0]).toEqual(['cargo test\n'])
    expect(w.emitted('run')).toBeUndefined()
  })

  it('终端还没有通道时那个目标不可选——不给一个按下去没反应的选项', () => {
    const w = mount({ terminalReady: false })
    const opt = w.element.querySelector<HTMLOptionElement>('option[value="terminal"]')
    expect(opt?.disabled).toBe(true)
  })
})

describe('ConvFlow：会话已结束时输入区整体禁用', () => {
  it('输入框与执行按钮都禁用，且说清为什么', async () => {
    const w = mount({ ended: true })
    expect(input(w).element.disabled).toBe(true)
    expect(w.find<HTMLButtonElement>('[data-conv-run]').element.disabled).toBe(true)
    expect(w.element.textContent).toContain('不会再建立通道')

    // 禁用态下即使绕过按钮直接提交也不发请求：那个会话不会再建通道
    await type(w, 'ls')
    await submit(w)
    expect(w.emitted('run')).toBeUndefined()
  })
})

describe('ConvFlow：轨迹入口', () => {
  it('点「轨迹」把触发元素一起报出去（抽屉要还焦点给它）', async () => {
    const w = mount()
    const btn = w.find<HTMLButtonElement>('[data-conv-traj]')
    await btn.trigger('click')

    const payload = w.emitted('open-traj')?.[0]
    expect(payload?.[0]).toBe(btn.element)
  })

  it('轨迹按钮带 aria-expanded——它的状态要能被读出来', () => {
    const closed = mount({ trajOpen: false })
    expect(closed.find('[data-conv-traj]').attributes('aria-expanded')).toBe('false')
  })
})

describe('ConvFlow：位置信息重复一次', () => {
  it('提示里重复设备与路径（两个会话并排时靠它分）', () => {
    const w = mount()
    const hint = w.element.querySelector('.af-compose__hint')
    expect(hint?.textContent).toContain('mbp.local')
    expect(hint?.textContent).toContain('/srv/alpha')
  })
})
