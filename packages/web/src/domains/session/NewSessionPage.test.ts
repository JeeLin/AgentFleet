import { describe, expect, it } from 'vitest'

import NewSessionPage from '@/domains/session/NewSessionPage.vue'
import { agent, device, mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/** 一台在线设备，上面 opencode 能对话、claude 装了但不能用。 */
const ONLINE = device({
  id: 'n_1',
  name: 'mbp.local',
  status: 'online',
  agents: ['opencode', 'claude'],
  usable_agents: ['opencode'],
})
/** 这台设备上没有能对话的 agent：只有装坏的那个。 */
const NO_USABLE = device({
  id: 'n_2',
  name: 'tower.local',
  status: 'online',
  agents: ['claude'],
  usable_agents: [],
})

/** 勾一个 agent 选项。`''` 是那项显式的「不指定」。 */
async function pickAgent(w: ReturnType<typeof mountIn>, name: string): Promise<void> {
  const el = w.element.querySelector<HTMLElement>(`[data-agent-option="${name}"]`)
  if (!el) throw new Error(`no agent option: ${name}`)
  await el.click()
  await flush(w)
}

type CtxFn = ReturnType<typeof stubFetch>

async function pickDevice(w: ReturnType<typeof mountIn>, index = 0): Promise<void> {
  await w.element.querySelectorAll<HTMLElement>('.af-opts [data-device-id]')[index]?.click()
  await flush(w)
}

function typePath(w: ReturnType<typeof mountIn>, value: string): Promise<void> {
  const el = w.element.querySelector<HTMLInputElement>('#path')
  if (!el) throw new Error('no path input')
  type(el, value)
  return flush(w)
}

function submittedBody(fn: CtxFn): Record<string, unknown> {
  const post = fn.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'POST')
  if (!post) throw new Error('no POST made')
  return JSON.parse(String((post[1] as RequestInit).body)) as Record<string, unknown>
}

function type(input: HTMLInputElement, value: string): void {
  input.value = value
  input.dispatchEvent(new Event('input'))
}

describe('NewSessionPage：渲染', () => {
  it('三步都在同一列里，agent 那一步是可交互的选择', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [agent()] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('① 设备')
    expect(text).toContain('② agent')
    expect(text).toContain('③ 工作区路径')
    expect(text).toContain('决定这个会话能不能对话')
  })

  it('路径是创建期的一次性输入，页面上写明建立后只读', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [agent()] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('建立后不能换设备、不能换 agent、路径不可改')
    expect(text).toContain('必须')
    expect(text).toContain('已经存在于选中的设备上')
  })

  it('这一页没有「打开工作区」入口——工作区不是路由', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [agent()] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('没有任何「打开工作区」的入口')
    const hrefs = Array.from(w.element.querySelectorAll('a')).map((a) => a.getAttribute('href'))
    expect(hrefs.some((h) => h?.includes('/w/') || h?.includes('workspace'))).toBe(false)
  })

  it('说明文字只讲现在成立的事：供料整体撤销、装 agent 不归我们做', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [agent()] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    // 工作区供料在 2026-10-26 被整体撤销，不是「改期到 M2」——所以不许再出现
    // 「M2 才有」这种把撤销说成排期的写法。
    expect(text).toContain('供料能力已整体撤出，不在任何阶段')
    expect(text).not.toContain('M2 才有')
    // 装 agent 从未被排进任何里程碑（撤销，不是挪期）：写的是「用户自己装」。
    expect(text).toContain('agent 由用户自己装在这台设备上')
    expect(text).not.toContain('装 agent 是 M5')
  })
})

describe('NewSessionPage：空态', () => {
  it('一台设备都没有时说清会话必须落在设备上，并给出去设备页的路', async () => {
    stubFetch({ '/api/devices': { body: [] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('没有设备可以开会话')
    expect(text).toContain('afcli run')
    expect(w.element.querySelector('.af-opt')).toBeNull()
  })
})

describe('NewSessionPage：选设备', () => {
  it('用 ↑↓ 移动、⏎ 选中一台设备（列表是唯一 Tab 落点）', async () => {
    stubFetch({
      '/api/devices': {
        body: [
          device({ id: 'n_1', name: 'a', agents: ['opencode'], usable_agents: ['opencode'] }),
          device({ id: 'n_2', name: 'b', agents: ['opencode'], usable_agents: ['opencode'] }),
        ],
      },
      '/api/agents': { body: [agent()] },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const list = w.element.querySelector<HTMLElement>('.af-opts')
    expect(list?.getAttribute('tabindex')).toBe('0')
    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('.af-opt'))
    expect(rows.every((r) => r.getAttribute('tabindex') === '-1')).toBe(true)

    rows[1]!.focus()
    rows[1]!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))
    await flush(w)

    expect(rows[1]?.getAttribute('aria-selected')).toBe('true')
    expect(w.element.querySelector('.af-summary')?.textContent).toContain('b')
  })

  it('离线设备仍列出来并写明原因，但不会被自动选中', async () => {
    stubFetch({ '/api/devices': { body: [device({ id: 'n_off', status: 'offline' })] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const row = w.element.querySelector<HTMLElement>('.af-opt')
    expect(row?.getAttribute('data-inert')).toBe('true')
    expect(row?.textContent).toContain('已判离线')
    expect(row?.getAttribute('aria-selected')).toBe('false')
  })

  it('换设备清掉路径：路径是「这台设备上的」一个目录', async () => {
    stubFetch({
      '/api/devices': {
        body: [
          device({ id: 'n_1', name: 'a', agents: ['opencode'], usable_agents: ['opencode'] }),
          device({ id: 'n_2', name: 'b', agents: ['opencode'], usable_agents: ['opencode'] }),
        ],
      },
      '/api/agents': { body: [agent()] },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const rows = Array.from(w.element.querySelectorAll<HTMLElement>('.af-opt'))
    await rows[0]?.click()
    await flush(w)
    const path = w.element.querySelector<HTMLInputElement>('#path')
    type(path!, '/srv/alpha')
    await flush(w)
    expect(path?.value).toBe('/srv/alpha')

    await rows[1]?.click()
    await flush(w)
    expect(w.element.querySelector<HTMLInputElement>('#path')?.value).toBe('')
  })
})

describe('NewSessionPage：提交', () => {
  /** 选第一台设备、勾第一个可用 agent、填路径。 */
  async function fill(w: ReturnType<typeof mountIn>, path: string, Agent = 'opencode'): Promise<void> {
    await pickDevice(w)
    await pickAgent(w, Agent)
    await typePath(w, path)
  }

  it('三项齐了才能建；缺哪项错误就指哪一项', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [agent()] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const submit = w.element.querySelector<HTMLButtonElement>('.af-submit button')
    expect(submit?.disabled, '只填路径也不该能建：agent 那一步还没选').toBe(true)

    await pickDevice(w)
    await flush(w)
    expect(w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled, '没填路径').toBe(true)

    await typePath(w, 'relative/path')
    expect(w.element.querySelector('.af-field__error')?.textContent).toContain('以 / 或 ~ 开头')

    await typePath(w, '/srv/alpha')
    expect(
      w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled,
      'agent 还没选：这一项重则建出一个不能对话的死会话',
    ).toBe(true)

    await pickAgent(w, 'opencode')
    expect(w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled).toBe(false)
  })

  it('选了 agent 才建得出来，且 `agent_id` 真的进请求体', async () => {
    const fn = stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [agent()] },
      '/api/sessions': {
        status: 201,
        body: {
          id: 's_new',
          agent_id: 'opencode',
          state: 'running',
          created_at: 'unix:1700000000',
          ended_at: null,
          workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true },
        },
      },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await fill(w, '/srv/alpha')

    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    expect(location.pathname).toBe('/sessions/s_new')
    expect(submittedBody(fn)).toEqual({ node_id: 'n_1', path: '/srv/alpha', agent_id: 'opencode' })
  })

  it('显式选「不指定」时落 `agent_id: null`，且页面先说清后果', async () => {
    const fn = stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [agent()] },
      '/api/sessions': {
        status: 201,
        body: {
          id: 's_new',
          agent_id: null,
          state: 'running',
          created_at: 'unix:1700000000',
          ended_at: null,
          workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true },
        },
      },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await fill(w, '/srv/alpha', '')

    // 后果要**建之前**就说出来：等建完才发现不能对话，会话已经不可逆了
    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('不能对话')

    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    // 空串**不进**请求体：库里 `agent_id` 用 NULL 表示「没有 agent」
    expect(submittedBody(fn)).toEqual({ node_id: 'n_1', path: '/srv/alpha', agent_id: null })
  })

  /**
   * 「没有能对话的 agent 时仍能用『不指定』建会话」。
   *
   * 判据必须是**真的发出了 POST、且 body 里 `agent_id` 是 `null`**：
   * 只断言「按钮没灰」证明不了会话建得成——按钮可点而提交路径写错
   * （例如 `pickedAgent.value || 'default'`）时，旧写法照样绿。
   */
  it('这台设备没有能对话的 agent 时，仍能用「不指定」建会话', async () => {
    const fn = stubFetch({
      '/api/devices': { body: [NO_USABLE] },
      '/api/agents': { body: [] },
      '/api/sessions': {
        status: 201,
        body: {
          id: 's_new',
          agent_id: null,
          state: 'running',
          created_at: 'unix:1700000000',
          ended_at: null,
          workspace: { node_id: 'n_2', path: '/srv/beta', path_locked: true },
        },
      },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await pickDevice(w)
    await typePath(w, '/srv/beta')

    expect(w.element.textContent).toContain('这台设备上现在没有能对话的 agent')
    // 「不指定」必须还在：没有 agent 也该能开会话（终端 / exec 照常）
    await pickAgent(w, '')
    expect(w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled).toBe(false)

    // 真的按下去：按钮可点 ≠ 会话建成了。
    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    expect(submittedBody(fn)).toEqual({
      node_id: 'n_2',
      path: '/srv/beta',
      agent_id: null,
    })
  })

  it('装了但跑不起来的那个不进可选项，但它的原因要写出来', async () => {
    stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [agent({ name: 'claude', available: false, error: '没有 acp 子命令' })] },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await pickDevice(w)

    // 不可选：让它可选就是让用户建一个永远不能对话的死会话
    expect(w.element.querySelector('[data-agent-option="claude"]')).toBeNull()
    expect(w.element.querySelector('[data-agent-option="opencode"]')).not.toBeNull()
    // 但装坏了这件事必须看得见——这正是 `Probe::NotCapable` 存在的理由
    expect(w.element.textContent).toContain('claude')
    expect(w.element.textContent).toContain('没有 acp 子命令')
  })

  it('换设备要重选 agent：能用清单是「那台机器上的」', async () => {
    stubFetch({
      '/api/devices': {
        body: [
          device({ id: 'n_1', name: 'a', agents: ['opencode'], usable_agents: ['opencode'] }),
          device({ id: 'n_2', name: 'b', agents: ['claude'], usable_agents: ['claude'] }),
        ],
      },
      '/api/agents': { body: [agent()] },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await pickDevice(w, 0)
    await pickAgent(w, 'opencode')
    await typePath(w, '/srv/alpha')

    await pickDevice(w, 1)
    await flush(w)
    expect(w.element.querySelector<HTMLInputElement>('#path')?.value, '路径也清').toBe('')
    const texts = Array.from(w.element.querySelectorAll<HTMLElement>('[data-agent-option]')).map(
      (el) => el.getAttribute('data-agent-option'),
    )
    expect(texts).toContain('claude')
    expect(texts).not.toContain('opencode')
    // 摘要里不能还留着上一台机器上的那个名字。
    //
    // **判据要指向「旧名字不在」，不能写「摘要里有『未选』」**：路径那一格
    // 空着也是「未选」，而路径本来就会被清——于是 agent 没清这个错误会被
    // 路径的「未选」兜住（反例 3 实测确实通过了，白跑一轮）。
    //
    // 提交按钮禁用同样兜不住：路径清空本身就足以禁用它。
    expect(w.element.querySelector('.af-summary')?.textContent).not.toContain('opencode')
  })

  it('换设备后旧选择不残留到请求体里', async () => {
    const fn = stubFetch({
      '/api/devices': {
        body: [
          device({ id: 'n_1', name: 'a', agents: ['opencode'], usable_agents: ['opencode'] }),
          device({ id: 'n_2', name: 'b', agents: ['claude'], usable_agents: ['claude'] }),
        ],
      },
      '/api/agents': { body: [agent()] },
      '/api/sessions': {
        status: 201,
        body: {
          id: 's_new',
          agent_id: 'claude',
          state: 'running',
          created_at: 'unix:1700000000',
          ended_at: null,
          workspace: { node_id: 'n_2', path: '/srv/gamma', path_locked: true },
        },
      },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await pickDevice(w, 0)
    await pickAgent(w, 'opencode')
    await pickDevice(w, 1)
    await pickAgent(w, 'claude')
    await typePath(w, '/srv/gamma')

    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)
    expect(submittedBody(fn)).toEqual({ node_id: 'n_2', path: '/srv/gamma', agent_id: 'claude' })
  })

  it('提交成功后 replace 跳到工作台（后退不该回到提交过的表单）', async () => {
    const fn = stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [] },
      '/api/sessions': {
        status: 201,
        body: {
          id: 's_new',
          agent_id: null,
          state: 'running',
          created_at: 'unix:1700000000',
          ended_at: null,
          workspace: { node_id: 'n_1', path: '/srv/alpha', path_locked: true },
        },
      },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await fill(w, '/srv/alpha')

    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    expect(location.pathname).toBe('/sessions/s_new')
    expect(submittedBody(fn)).toMatchObject({ node_id: 'n_1', path: '/srv/alpha' })
  })

  it('建会话失败时把 Hub 的错误码翻成人话，并留在页面上', async () => {
    stubFetch({
      '/api/devices': { body: [ONLINE] },
      '/api/agents': { body: [] },
      '/api/sessions': { status: 409, body: { error: 'device_offline' } },
    })
    const w = mountIn(NewSessionPage)
    await flush(w)
    await fill(w, '/srv/alpha')

    const before = location.pathname
    await w.element.querySelector<HTMLButtonElement>('.af-submit button')?.click()
    await flush(w)

    const alert = w.element.querySelector('[role="alert"]')
    expect(alert?.textContent).toContain('离线')
    // 失败不跳走：地址没动，表单里的选择也还在，用户改一下就能重试
    expect(location.pathname).toBe(before)
    expect(w.element.querySelector<HTMLInputElement>('#path')?.value).toBe('/srv/alpha')
    expect(w.element.querySelector<HTMLButtonElement>('.af-submit button')?.disabled).toBe(false)
  })

  it('取消回会话列表', async () => {
    stubFetch({ '/api/devices': { body: [ONLINE] }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    const cancel = Array.from(w.element.querySelectorAll<HTMLAnchorElement>('.af-submit a'))[0]
    cancel!.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
    await flush(w)
    expect(location.pathname).toBe('/sessions')
  })
})

describe('NewSessionPage：读失败', () => {
  it('Hub 连不上时给出可读理由', async () => {
    stubFetch({ '/api/devices': { status: 500, body: { error: 'boom' } }, '/api/agents': { body: [] } })
    const w = mountIn(NewSessionPage)
    await flush(w)

    expect(w.element.querySelector('[role="alert"]')?.textContent).toContain('boom')
  })
})
