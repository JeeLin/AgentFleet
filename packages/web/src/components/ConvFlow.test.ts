import { describe, expect, it } from 'vitest'

import ConvFlow, { type ConvExecRequest } from '@/components/ConvFlow.vue'
import type { TrajectoryEvent } from '@/lib/api'
import { cssRuleOf, readRepoFile } from '@/test/css'
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

/** 选一个发送目标（默认是对话，所以 exec 用例必须先选）。 */
async function pickTarget(w: ReturnType<typeof mount>, value: string) {
  await w.find<HTMLSelectElement>('[data-conv-target]').setValue(value)
  await w.vm.$nextTick()
}

async function submit(w: ReturnType<typeof mount>) {
  await w.find('form').trigger('submit')
  await w.vm.$nextTick()
}

/** 造一条落库的事件（与 `GET /api/sessions/{id}/trajectory` 的 `events` 同形）。 */
function event(id: number, seq: number, kind: string, payload: unknown): TrajectoryEvent {
  return { id, seq, kind, payload: JSON.stringify(payload), duration_ms: null, duration_source: 'none', created_at: 'unix:1' }
}

// ---------------------------------------------------------------------------
// 空态：没有 agent 时明写「为什么不能对话」，而不是一句「无数据」
// ---------------------------------------------------------------------------

describe('ConvFlow：没有 agent 时说清为什么不能对话', () => {
  it('空态写的是「没有选 agent」，不是一句没来由的「无数据」', () => {
    const w = mount()
    const empty = w.element.querySelector('.af-empty')
    expect(empty?.textContent).toContain('还不能对话')
    expect(empty?.textContent).toContain('没有选 agent')
  })

  it('横幅说清 Hub 不替用户挑 agent——那是写进历史的决定', () => {
    const w = mount()
    const note = w.element.querySelector('.af-stage-note')
    expect(note?.getAttribute('role')).toBe('note')
    expect(note?.textContent).toContain('没有选 agent')
    expect(note?.textContent).toContain('不替用户挑一个')
  })

  it('「对话」目标不可选，且不可选的原因写在页面上', () => {
    const w = mount()
    const opt = w.element.querySelector<HTMLOptionElement>('option[value="chat"]')
    expect(opt?.disabled).toBe(true)
    expect(w.element.textContent).toContain('不能对话：没有选 agent')
  })

  it('对话区是 role="log"（追加消息时读屏会播报）', () => {
    const w = mount()
    expect(w.element.querySelector('[role="log"]')?.getAttribute('aria-label')).toBe('对话流')
  })
})

// ---------------------------------------------------------------------------
// 对话：说一句话 → `say`
// ---------------------------------------------------------------------------

describe('ConvFlow：对话目标把整段文本交给 agent', () => {
  /** 能对话的会话：选了 agent + 通道已开。 */
  function ready(props: Partial<Record<string, unknown>> = {}) {
    return mount({ agentId: 'opencode', ...props })
  }

  it('默认是「对话」而不是 exec——这个组件的名字是「对话流」', () => {
    const w = ready()
    expect(w.find<HTMLSelectElement>('[data-conv-target]').element.value).toBe('chat')
  })

  it('发送 = 发一个 `say`，文本原样（不切词、不解释）', async () => {
    const w = ready()
    await type(w, '帮我看看这个目录下的测试为什么红')
    await submit(w)

    expect(w.emitted('say')?.[0]).toEqual(['帮我看看这个目录下的测试为什么红'])
    expect(w.emitted('run')).toBeUndefined()
  })

  it('中文输入法选字期间的 ⏎ 不发送（那是把候选词上屏）', async () => {
    const w = ready()
    await type(w, 'ls')
    await input(w).trigger('keydown', { key: 'Enter', isComposing: true })
    await w.vm.$nextTick()
    expect(w.emitted('say')).toBeUndefined()
  })

  it('⇧⏎ 不发送（那是换行）', async () => {
    const w = ready()
    await type(w, 'ls')
    await input(w).trigger('keydown', { key: 'Enter', shiftKey: true })
    await w.vm.$nextTick()
    expect(w.emitted('say')).toBeUndefined()
  })

  it('发送后清空输入框', async () => {
    const w = ready()
    await type(w, 'hi')
    await submit(w)
    expect(input(w).element.value).toBe('')
  })

  it('选了 agent 就能发——通道是在发的第一句话时开的，不是先开好再发', async () => {
    // 判据里不能含「通道已开」：那会死锁（目标选不了 → 发不出去 → 永远开不了）。
    const w = mount({ agentId: 'opencode' })
    await pickTarget(w, 'chat')
    await type(w, 'hi')
    await submit(w)
    expect(w.emitted('say')).toEqual([['hi']])
  })

  it('开通道失败：把原因说出来，但不拦住重试', async () => {
    const w = mount({ agentId: 'opencode', convError: 'agent_unavailable' })
    await pickTarget(w, 'chat')
    await type(w, 'hi')
    await submit(w)
    // 不拦发送：agent 不可用是设备侧当时的状态，过一会可能就好了
    expect(w.emitted('say')).toEqual([['hi']])
    expect(w.element.textContent).toContain('agent_unavailable')
    expect(w.element.textContent).toContain('再发一次会重试')
  })
})

// ---------------------------------------------------------------------------
// 渲染：按帧格式画出消息、工具、收尾
// ---------------------------------------------------------------------------

describe('ConvFlow：把轨迹事件画成对话', () => {
  it('agent 的一段话画成一行 agent 消息', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'agent_message_chunk', { content: [{ type: 'text', text: '你好' }] }),
      ],
    })
    await w.vm.$nextTick()
    const msg = w.element.querySelector('[data-conv-msg]')
    expect(msg?.getAttribute('data-who')).toBe('agent')
    expect(msg?.textContent).toContain('你好')
  })

  /**
   * 真机实测的载荷：`opencode acp` 的 `agent_message_chunk` 里 `content` 是
   * **单个对象** `{type, text}`，不是数组。数组形态的夹具走的是另一条分支，
   * 所以这条必须用对象形态，否则「整串 JSON 上屏」这个缺陷测不出来。
   */
  it('content 是单个对象（真机 opencode acp 形态）时只画 text，不画整串 JSON', async () => {
    const realPayload =
      '{"sessionUpdate":"agent_message_chunk","messageId":"msg_17d0",' +
      '"content":{"type":"text","text":"好的，我在。有什么需要我帮忙的吗？"}}'
    const w = mount({
      agentId: 'opencode',
      events: [{ id: 1, seq: 1, kind: 'agent_message_chunk', payload: realPayload }],
    })
    await w.vm.$nextTick()

    const msg = w.element.querySelector('[data-conv-msg]')
    expect(msg?.getAttribute('data-who')).toBe('agent')
    expect(msg?.textContent).toContain('好的，我在。有什么需要我帮忙的吗？')
    expect(msg?.textContent).not.toContain('sessionUpdate')
  })

  it('用户的一段话画成一行 user 消息', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [event(1, 1, 'user_message_chunk', { content: [{ type: 'text', text: '帮我' }] })],
    })
    await w.vm.$nextTick()
    const msg = w.element.querySelector('[data-conv-msg]')
    expect(msg?.getAttribute('data-who')).toBe('user')
    expect(msg?.textContent).toContain('帮我')
  })

  /**
   * 真机一轮回复固定推三条：`available_commands_update` / `agent_message_chunk`
   * / `usage_update`（`scripts/e2e-m2.sh` 的实测注释）。前两条里两条是元数据。
   *
   * 缺陷原样：它们落进 `default` 臂，画成「系统 available_commands_update
   * {"sessionUpdate":…}」——用户先看到两坨协议 JSON 才看到回复。
   */
  it('元数据事件压成一句人话，不把原始 JSON 端上对话区', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'available_commands_update', {
          sessionUpdate: 'available_commands_update',
          availableCommands: [{ name: 'init' }, { name: 'compact' }],
        }),
        event(2, 2, 'agent_message_chunk', { content: { type: 'text', text: '好的，我在。' } }),
        event(3, 3, 'usage_update', { sessionUpdate: 'usage_update', used: 1234, size: 200000 }),
      ],
    })
    await w.vm.$nextTick()

    const metas = Array.from(w.element.querySelectorAll('[data-conv-meta]')).map(
      (e) => e.textContent ?? '',
    )
    expect(metas).toHaveLength(2)
    expect(metas[0]).toContain('可用命令')
    expect(metas[0]).toContain('init')
    expect(metas[1]).toContain('1234')
    expect(metas[1]).toContain('200000')

    // 元数据不许再冒充对话行，也不许出现原始 JSON。
    const text = w.element.textContent ?? ''
    expect(text).not.toContain('sessionUpdate')
    expect(text).not.toContain('"availableCommands"')
    // 正文那一条照旧可见——这才是用户真正要看的东西。
    const msg = w.element.querySelector('[data-conv-msg]')
    expect(msg?.textContent).toContain('好的，我在。')
  })

  it('认不出的元数据形状说不撒谎的一句话，而不是退回原始 JSON', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [event(1, 1, 'usage_update', { guess: 'no used/size here' })],
    })
    await w.vm.$nextTick()
    const meta = w.element.querySelector('[data-conv-meta]')
    expect(meta?.textContent).toContain('token 用量')
    expect(meta?.textContent).not.toContain('{')
  })

  /**
   * **连收 N 个 chunk 要拼成一条消息**，而不是 N 条。
   *
   * 这是子任务 7 点名的硬断言（「消息体为拼接结果，且**不重复、不丢**」）。
   * 一次 500 字回复实测是 30–80 个 chunk，若每个 chunk 各成一行，页面会
   * 把一句话切成几十条——而流式追加正是这个功能存在的理由。
   */
  it('连续的 agent chunk 拼成一条消息，不重复也不丢', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'agent_message_chunk', { content: [{ type: 'text', text: '你好' }] }),
        event(2, 2, 'agent_message_chunk', { content: [{ type: 'text', text: '，' }] }),
        event(3, 3, 'agent_message_chunk', { content: [{ type: 'text', text: '世界' }] }),
      ],
    })
    await w.vm.$nextTick()

    const msgs = w.element.querySelectorAll('[data-conv-msg]')
    // 先断言条数：一条消息三行的话，「拼接」根本没发生
    expect(msgs.length, '三个 chunk 必须是同一条消息').toBe(1)
    expect(msgs[0]?.textContent).toContain('你好，世界')
  })

  it('user chunk 与 agent chunk 不拼在一起——说话人换了就是另一条', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'user_message_chunk', { content: [{ type: 'text', text: '问题' }] }),
        event(2, 2, 'agent_message_chunk', { content: [{ type: 'text', text: '回答' }] }),
      ],
    })
    await w.vm.$nextTick()

    const msgs = w.element.querySelectorAll('[data-conv-msg]')
    expect(msgs.length).toBe(2)
    expect(msgs[0]?.getAttribute('data-who')).toBe('user')
    expect(msgs[1]?.getAttribute('data-who')).toBe('agent')
  })

  /**
   * **同一条 tool_call 的 pending → in_progress → completed 只留最后状态**。
   *
   * 实测 ACP 会为同一个 `toolCallId` 连发 `tool_call_update`（本机 live 收到
   * 4 条）。逐条铺开会把一次工具调用画成四块，其中三块是中间态——中间态
   * 的意义在于「它后来变成了什么」，不是「它曾经是什么」。
   */
  it('同一工具调用的多次 update 合并成一块，显示最后的状态', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'tool_call', { toolCallId: 't1', title: 'bash', status: 'pending' }),
        event(2, 2, 'tool_call_update', { toolCallId: 't1', status: 'in_progress' }),
        event(3, 3, 'tool_call_update', { toolCallId: 't1', status: 'completed' }),
      ],
    })
    await w.vm.$nextTick()

    const tools = w.element.querySelectorAll('[data-conv-tool]')
    expect(tools.length, '同一 toolCallId 的更新要合并').toBe(1)
    expect(tools[0]?.textContent).toContain('completed')
    expect(tools[0]?.textContent).not.toContain('pending')
  })

  it('不同工具调用各占一块——合并只认同一个 toolCallId', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'tool_call', { toolCallId: 't1', title: 'bash', status: 'completed' }),
        event(2, 2, 'tool_call', { toolCallId: 't2', title: 'read', status: 'completed' }),
      ],
    })
    await w.vm.$nextTick()
    expect(w.element.querySelectorAll('[data-conv-tool]').length).toBe(2)
  })

  it('工具调用画成可折叠的块，默认收起（一屏里几十个工具会淹掉对话）', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'tool_call', { title: 'bash', status: 'pending', rawInput: { command: 'ls' } }),
      ],
    })
    await w.vm.$nextTick()
    const tool = w.element.querySelector('[data-conv-tool]')
    expect(tool?.textContent).toContain('bash')
    // 默认收起：展开是用户的动作，不是默认
    expect(tool?.getAttribute('data-open')).toBe('false')
  })

  it('turn_end 画成一轮的收尾标记', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [event(1, 1, 'turn_end', { stopReason: 'end_turn' })],
    })
    await w.vm.$nextTick()
    expect(w.element.querySelector('[data-conv-turn]')?.textContent).toContain('end_turn')
  })

  /**
   * **未知事件类型照样画，不丢。**
   *
   * 新版本 agent 会推我们没见过的 `sessionUpdate`。在这里过滤掉，用户就
   * 看不见 agent 说过的一部分话——而它恰恰是「不承诺完整、但必须可见」里
   * 那个「可见」的部分。
   */
  it('不认识的事件类型照样画出来，不静默丢弃', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [event(1, 1, 'some_new_kind_from_a_later_version', { foo: 'bar' })],
    })
    await w.vm.$nextTick()
    const msg = w.element.querySelector('[data-conv-msg]')
    expect(msg?.getAttribute('data-who')).toBe('system')
    expect(msg?.textContent).toContain('some_new_kind_from_a_later_version')
  })

  it('payload 不是合法 JSON 时退化成原文，不整条丢掉', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [{ id: 1, seq: 1, kind: 'agent_message_chunk', payload: 'not json{' }],
    })
    await w.vm.$nextTick()
    // 丢掉的表现是「agent 说了话、页面上什么都没有」，那比一段原文糟
    expect(w.element.querySelector('[data-conv-msg]')?.textContent).toContain('not json{')
  })
})

// ---------------------------------------------------------------------------
// 断口：不完整必须可见，且不许假装连续
// ---------------------------------------------------------------------------

describe('ConvFlow：断口画出来，不假装连续', () => {
  it('中间缺一段 → 写「缺失 N 条」，写的是几就是几', async () => {
    const w = mount({
      agentId: 'opencode',
      // seq 1、2 在，5 在：缺的是 3、4
      events: [
        event(1, 1, 'agent_message_chunk', { content: [{ type: 'text', text: 'a' }] }),
        event(2, 2, 'agent_message_chunk', { content: [{ type: 'text', text: 'b' }] }),
        event(3, 5, 'agent_message_chunk', { content: [{ type: 'text', text: 'c' }] }),
      ],
      gaps: [{ from: 3, to: 5, missing: 2 }],
    })
    await w.vm.$nextTick()
    const gap = w.element.querySelector('[data-conv-gap]')
    // 先断言它**画出来了**：按 `from` 索引断口会一个都不画（那条 seq 本就不在），
    // 而页面看起来就是连续的——那正是这个功能要防的事。
    expect(gap, '断口标记必须画出来').not.toBeNull()
    expect(gap?.textContent).toContain('缺失 2 条')
    expect(gap?.textContent).toContain('seq 2 之后断开')
  })

  it('末轮没有 turn_end → 写「本轮未完成」', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [event(1, 1, 'agent_message_chunk', { content: [{ type: 'text', text: 'a' }] })],
      incomplete: true,
    })
    await w.vm.$nextTick()
    const el = w.element.querySelector('[data-conv-incomplete]')
    expect(el?.textContent).toContain('本轮未完成')
    expect(el?.textContent).toContain('turn_end')
  })

  it('没有断口就不画断口标记——不谎报', async () => {
    const w = mount({
      agentId: 'opencode',
      events: [
        event(1, 1, 'agent_message_chunk', { content: [{ type: 'text', text: 'a' }] }),
        event(2, 2, 'turn_end', { stopReason: 'end_turn' }),
      ],
      gaps: [],
      incomplete: false,
    })
    await w.vm.$nextTick()
    expect(w.element.querySelectorAll('[data-conv-gap]')).toHaveLength(0)
    expect(w.element.querySelectorAll('[data-conv-incomplete]')).toHaveLength(0)
  })
})

// ---------------------------------------------------------------------------
// exec：保留下来的那半功能（一次性命令，不进轨迹）
// ---------------------------------------------------------------------------

describe('ConvFlow：选 exec 时跑一条一次性命令', () => {
  it('执行一行命令 = 发一个 exec 请求（command + args），cwd 恒空', async () => {
    const w = mount()
    await pickTarget(w, 'exec')
    await type(w, 'git status --short')
    await submit(w)

    expect(w.emitted('run')?.[0]).toEqual([
      { command: 'git', args: ['status', '--short'], cwd: '' } satisfies ConvExecRequest,
    ])
  })

  it('引号只用来把带空格的参数合成一个——不是 shell', async () => {
    const w = mount()
    await pickTarget(w, 'exec')
    await type(w, `grep -n "two words" src/lib`)
    await submit(w)

    expect(w.emitted('run')?.[0]?.[0]).toEqual({
      command: 'grep',
      args: ['-n', 'two words', 'src/lib'],
      cwd: '',
    })
  })

  it('提示里写明没有 shell（管道与重定向不会生效）', async () => {
    const w = mount()
    await pickTarget(w, 'exec')
    expect(w.element.textContent).toContain('管道与重定向')
  })

  it('⏎ 执行', async () => {
    const w = mount()
    await pickTarget(w, 'exec')
    await type(w, 'ls')
    await input(w).trigger('keydown', { key: 'Enter' })
    await w.vm.$nextTick()
    expect(w.emitted('run')?.[0]?.[0]).toMatchObject({ command: 'ls' })
  })

  it('执行后清空输入框——不留一行「到底跑了没有」的悬念', async () => {
    const w = mount()
    await pickTarget(w, 'exec')
    await type(w, 'ls')
    await submit(w)
    expect(input(w).element.value).toBe('')
  })

  it('空输入不发请求，按钮也是禁用的', async () => {
    const w = mount()
    await pickTarget(w, 'exec')
    expect(w.find<HTMLButtonElement>('[data-conv-run]').element.disabled).toBe(true)
    await type(w, '   ')
    await submit(w)
    expect(w.emitted('run')).toBeUndefined()
  })
})

// ---------------------------------------------------------------------------
// 共用：结束态、终端目标、轨迹入口、位置信息
// ---------------------------------------------------------------------------

describe('ConvFlow：会话已结束时输入区整体禁用', () => {
  it('输入框与按钮都禁用，且说清为什么', async () => {
    const w = mount({ ended: true, agentId: 'opencode' })
    expect(input(w).element.disabled).toBe(true)
    expect(w.find<HTMLButtonElement>('[data-conv-run]').element.disabled).toBe(true)
    expect(w.element.textContent).toContain('不会再建立通道')

    await type(w, 'ls')
    await submit(w)
    expect(w.emitted('say')).toBeUndefined()
    expect(w.emitted('run')).toBeUndefined()
  })
})

describe('ConvFlow：终端目标', () => {
  it('切到终端时发的是一段文本，且补一个换行（等于在终端里回车）', async () => {
    const w = mount({ terminalReady: true })
    await pickTarget(w, 'terminal')
    await type(w, 'cargo test')
    await submit(w)

    expect(w.emitted('send')?.[0]).toEqual(['cargo test\n'])
    expect(w.emitted('say')).toBeUndefined()
    expect(w.emitted('run')).toBeUndefined()
  })

  it('终端还没有通道时那个目标不可选——不给一个按下去没反应的选项', () => {
    const w = mount({ terminalReady: false })
    const opt = w.element.querySelector<HTMLOptionElement>('option[value="terminal"]')
    expect(opt?.disabled).toBe(true)
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

// ---------------------------------------------------------------------------
// 输入区不许横向溢出：面板坞打开时对话列只有约 412px，容不下「一行排开」
// ---------------------------------------------------------------------------

describe('ConvFlow：阶段横幅与 AgentCatalog 的同名块形状不同（不许按「逐字重复」合并）', () => {
  /** 剥掉注释：注释里写着属性名与值，不剥掉的话删光声明断言照样绿。 */
  function ruleOf(file: string, selector: string): string {
    const src = readRepoFile(file).replace(/\/\*[\s\S]*?\*\//g, '')
    const body = cssRuleOf(src, selector)
    expect(body, `${file} 的 ${selector} 规则必须存在`).toBeTruthy()
    return body
  }

  const CATALOG = 'packages/web/src/components/AgentCatalog.vue'
  const FLOW = 'packages/web/src/components/ConvFlow.vue'

  it('对话这份是贴在流上的一条：只画 border-bottom，不是四边描边', () => {
    const here = ruleOf(FLOW, '.af-stage-note')
    expect(here).toMatch(/border-bottom/)
    // 四边描边 + 圆角是「独立卡片」那一档的形状，这份不能沾
    expect(here).not.toMatch(/border-radius/)
    expect(ruleOf(CATALOG, '.af-stage-note')).toMatch(/border-radius/)
  })

  it('两处正文不是同一档字号，所以不能合成一条规则', () => {
    expect(ruleOf(FLOW, '.af-stage-note__body')).toMatch(/--af-text-xs/)
    expect(ruleOf(CATALOG, '.af-stage-note__body')).toMatch(/--af-text-md/)
  })
})

describe('ConvFlow：输入区的行能在窄列里收缩', () => {
  const CSS = readRepoFile('packages/web/src/styles/components.css')

  /**
   * 断言前**必须去掉注释**：这几条规则的注释里就写着属性名与值（解释为什么
   * 要这么写），不剥掉的话把声明删了断言照样绿——那是恒真断言。
   */
  function declarationsOf(selector: string): string {
    const rule = cssRuleOf(CSS, selector)
    expect(rule, `${selector} 规则必须存在`).toBeTruthy()
    return rule.replace(/\/\*[\s\S]*?\*\//g, '')
  }

  // happy-dom 不做排版、vitest 也不注入样式，`getComputedStyle` 读不到——只能读样式源。
  it('输入行允许换行（不许换行就把「发送」「轨迹」推出视口）', () => {
    expect(declarationsOf('.af-compose__row')).toMatch(/flex-wrap:\s*wrap/)
  })

  it('输入框的 min-width 归零（textarea 的 cols=20 默认下限会顶住整行）', () => {
    expect(declarationsOf('.af-compose__input')).toMatch(/min-width:\s*0/)
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

// ---------------------------------------------------------------------------
// 发送按钮：灰着的原因要在 DOM 上分得开
// ---------------------------------------------------------------------------

describe('ConvFlow：发送按钮的禁用原因分得开（不合成一个 disabled）', () => {
  /** 按钮为什么被禁用。没有原因时按钮可点，属性不出现。 */
  function blockedBy(w: ReturnType<typeof mount>): string | undefined {
    return w.find<HTMLButtonElement>('[data-conv-run]').attributes('data-blocked-by')
  }

  /**
   * 每种原因都要能单独被断言：一个 `disabled` 属性只说得清「不能按」，
   * 而「还没打字」与「没有 agent」的出路完全不同。
   */
  it('没打字是 empty（用户自己能改的那一条）', async () => {
    const w = mount({ agentId: 'opencode' })
    expect(blockedBy(w)).toBe('empty')
    await type(w, 'ls')
    // 打完字就不该再是 empty：这条与下面 no-agent 的区分正是本组要钉的
    expect(blockedBy(w)).toBeUndefined()
  })

  it('没有 agent 是 no-agent，与「没打字」不是同一个原因', async () => {
    const w = mount()
    expect(blockedBy(w)).toBe('empty')
    await type(w, 'ls')
    // 关键：字打了，按钮仍然灰着——但原因是 no-agent 而不是 empty
    expect(w.find<HTMLButtonElement>('[data-conv-run]').element.disabled).toBe(true)
    expect(blockedBy(w)).toBe('no-agent')
  })

  it('会话已结束是 ended，设备已删除是 device-removed（两者不可互换）', async () => {
    const endedW = mount({ ended: true, agentId: 'opencode' })
    await type(endedW, 'ls')
    expect(blockedBy(endedW)).toBe('ended')

    const goneW = mount({ deviceGone: true, deviceGoneWhy: '设备 n_9 已被删除', agentId: 'opencode' })
    await type(goneW, 'ls')
    expect(blockedBy(goneW)).toBe('device-removed')
  })

  /** disabled 只说「不能按」，灰着的按钮不说话就是个谜——原因要挂 title 上。 */
  it('每种原因都有一句自己的 title（灰着的按钮不说话就是个谜）', async () => {
    const w = mount({ agentId: 'opencode' })
    const title = () => w.find<HTMLButtonElement>('[data-conv-run]').attributes('title')
    expect(title()).toContain('先写一句话')

    await type(w, 'ls')
    // 能按的时候不挂 title：一个可点的按钮不需要解释自己为什么能点
    expect(title()).toBeUndefined()
  })
})
