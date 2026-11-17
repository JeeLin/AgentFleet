/* ===========================================================================
   OverviewPage 的测试。
   ---------------------------------------------------------------------------
   这一页是**全站入口**（`/`）。它的三种状态必须分开，因为它们对用户的含义
   完全不同：
     · 加载中   —— 等一下就好
     · 读失败   —— 出错了，要重试
     · 真的没有 —— 下一步是去建一个

   把「读失败」写成「还没有项目」是这一页最容易犯的错：用户会以为自己的项目
   全没了。所以这里**逐条断状态文案互斥**，而不是只看「页面上有没有字」。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import OverviewPage from '@/domains/project/OverviewPage.vue'
import type { Project, ProjectBinding } from '@/domains/project/api'
import type { Execution, Task } from '@/domains/project/taskTypes'
import { mountIn } from '@/test/mount'
import { readRepoFile } from '@/test/css'
import { settle, stubFetch } from '@/test/http'
import { navigate, route } from '@/router'

async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

function mountAtRoot(): ReturnType<typeof mountIn<typeof OverviewPage>> {
  navigate('/', { replace: true })
  return mountIn(OverviewPage)
}

function project(over: Partial<Project> = {}): Project {
  return {
    id: 'p_1',
    name: '内部指标台',
    shape: 'central',
    theme_color: null,
    archived_at: null,
    created_at: 'unix:1700000000',
    ...over,
  }
}

function task(over: Partial<Task> = {}): Task {
  return {
    id: 'tsk_1',
    project_id: 'p_1',
    title: '把构建时间降到 60s 内',
    state: 'pending',
    done_when: { criteria: 'build_passes', description: null },
    needs_manual_acceptance: false,
    criteria_known: true,
    created_by: 'user',
    base_commit: null,
    created_at: 'unix:1700000000',
    updated_at: 'unix:1700000000',
    constraints: {
      allowed_devices: null,
      allowed_agents: null,
      required_capabilities: null,
      retry_limit: null,
    },
    version: 1,
    superseded_by: null,
    intent_ref: null,
    executions: [],
    ...over,
  }
}

/**
 * 一条执行记录。
 *
 * **夹具只给 Hub 真会发的字段**：`ExecutionView`（`routes/tasks.rs:144`）是
 * `id/state/base_commit/agent_id/failure_kind/reason/evidence/started_at/ended_at`，
 * 没有设备。夹具里多塞一个字段，页面就能读到一个真链路上不存在的值，而两边
 * 测试各自绿——2026-11-17 就是这么发现「正在跑 · device-a」从来没显示过的。
 */
function execution(id: string, over: Partial<Execution> = {}): Execution {
  return {
    id,
    state: 'running',
    base_commit: null,
    agent_id: 'claude-code',
    failure_kind: null,
    reason: null,
    evidence: null,
    started_at: 'unix:1700000100',
    ended_at: null,
    ...over,
  }
}

function binding(deviceId: string, over: Partial<ProjectBinding> = {}): ProjectBinding {
  return {
    id: `bnd_${deviceId}`,
    project_id: 'p_1',
    device_id: deviceId,
    fixed_dir: '/srv/af/project',
    disk_quota_bytes: null,
    cleanup_policy: 'balanced',
    used_bytes: null,
    ...over,
  }
}

describe('OverviewPage：真的没有项目', () => {
  it('空态说「还没有项目」并给出建项目那条路', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('还没有项目')
    // 光说「没有」不够：新建状态下第一件事就是建一个，必须直接给路。
    expect(text).toContain('项目是这里的起点')
    // 「暂无数据」是加载失败的措辞，不能出现在空态里。
    expect(text).not.toContain('暂无数据')
    // 空态里的重试按钮不该有——没有东西可重试。
    expect(text).not.toContain('重试')
  })

  it('空态和页头各有一个「新建项目」，且空态那个是主按钮', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const news = w.findAll('button').filter((b) => b.text() === '新建项目')
    expect(news.length).toBe(2)
    expect(news.some((b) => b.classes('af-btn--primary'))).toBe(true)
  })
})

describe('OverviewPage：有项目', () => {
  it('每个项目给名字、形态中文名与一句「代码从哪来」', async () => {
    stubFetch({
      '/api/projects': {
        body: [
          project({ id: 'p_1', name: '内部指标台', shape: 'central' }),
          project({ id: 'p_2', name: '客户现场', shape: 'isolated' }),
        ],
      },
    })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('内部指标台')
    expect(text).toContain('客户现场')
    expect(text).toContain('中心式')
    expect(text).toContain('隔离式')
    // 形态只给名字不够：三个名字对第一次见到的人没有区别，必须带一句
    // 「代码从哪来、到哪去」。
    expect(text).toContain('代码在 Hub 的仓库里')
    expect(text).toContain('代码只在设备上，永不上传')
  })

  it('点项目进它的看板', async () => {
    stubFetch({ '/api/projects': { body: [project({ id: 'p_7', name: '内部指标台' })] } })
    const w = mountAtRoot()
    await flush(w)

    const link = w.findAll('.af-region__title a')[0]
    expect(link, '区域头里的项目名要可点').toBeTruthy()
    await link?.trigger('click')

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_7')
  })

  it('项目 id 里带 `/` 时路径仍能匹配到看板', async () => {
    stubFetch({
      '/api/projects': { body: [project({ id: 'a/b', name: '带斜杠的项目' })] },
      '/api/projects/a%2Fb': { body: project({ id: 'a/b' }) },
    })
    const w = mountAtRoot()
    await flush(w)

    await w.findAll('.af-region__title a')[0]?.trigger('click')
    await flush(w)

    // 反向验证：把 `OverviewPage.vue` 里那处的 `encodeURIComponent` 删掉，
    // 这条会红在 `not-found` 上。
    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('a/b')
  })

  it('色点是轮换的装饰色槽，不随主题色变化（原型 dot-a/b/c）', async () => {
    stubFetch({
      '/api/projects': {
        body: [
          project({ id: 'p_1', theme_color: null }),
          project({ id: 'p_2', theme_color: 'green' }),
          project({ id: 'p_3', theme_color: 'gray' }),
          project({ id: 'p_4', theme_color: 'blue' }),
        ],
      },
    })
    const w = mountAtRoot()
    await flush(w)

    const dots = Array.from(w.element.querySelectorAll('.af-dot'))
    expect(dots).toHaveLength(4)
    // 原型 `index.html:39/64/87` 三个色槽轮着用，**不承载信息**。
    // 所以判据是「按位置循环」，不是「按 theme_color 取色」——后者会让
    // 一个装饰点被读成「绿色 = 这个项目成功」。
    expect(dots.map((d) => d.className)).toEqual([
      'af-dot dot-a',
      'af-dot dot-b',
      'af-dot dot-c',
      'af-dot dot-a',
    ])
    // 反向验证：把 `dotClass(i)` 里的 `i` 换回常量，这条立刻红。
    expect(dots.every((d) => d.getAttribute('aria-hidden') === 'true')).toBe(true)
    expect(dots.every((d) => d.getAttribute('data-color') === null)).toBe(true)
  })
})

describe('OverviewPage：读失败', () => {
  it('说「读取项目失败」，与「还没有项目」互斥', async () => {
    stubFetch({ '/api/projects': { status: 500, body: { error: 'internal' } } })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    expect(text).toContain('读取项目失败')
    // 这条是整页的核心判据：读失败绝不能显示成「你没有项目」。
    expect(text).not.toContain('还没有项目')
    expect(text).toContain('重试')
  })

  it('重试会重新请求，成功后显示列表', async () => {
    let first = true
    const fetchMock = stubFetch({
      '/api/projects': () => {
        if (first) {
          first = false
          return { status: 500, body: { error: 'internal' } }
        }
        return { body: [project({ id: 'p_1', name: '内部指标台' })] }
      },
    })
    const w = mountAtRoot()
    await flush(w)
    expect(w.element.textContent).toContain('读取项目失败')

    const retry = w.findAll('button').find((b) => b.text() === '重试')
    await retry?.trigger('click')
    await flush(w)

    expect(w.element.textContent).toContain('内部指标台')
    expect(w.element.textContent).not.toContain('读取项目失败')
    expect(fetchMock.mock.calls.length).toBeGreaterThanOrEqual(2)
  })
})

/**
 * 四行摘要：**两行真数据 + 两行显式空态**（用户 2026-11-16 裁定）。
 *
 * 原型 `index.html:41-53` 每个项目块内是「上次做到哪 / 正在跑 / 等你决定 /
 * 最近总结」四行。实现里能拿到真数据的是中间两行（`GET /api/projects/{id}/tasks`
 * 与 `/bindings`），头尾两行分别依赖执行时间线（v0.8.2）与管家总结（v0.9.0）。
 *
 * **判红方式**：把「上次做到哪」那行删掉（行数变 3）或把它的空态文字删掉，
 * 本块第一条立刻红；把 `data-row="running"` 换掉，第二条红。
 * 空态那一行的关键不是「有字」，而是**说出了归哪一版**——留白会被当成加载失败，
 * 编一句假摘要会被验收当成真数据。
 */
describe('OverviewPage：四行摘要（两行真数据 + 两行显式空态）', () => {
  it('每个项目一块区域，块内四行摘要 + 两个动作', async () => {
    stubFetch({
      '/api/projects': { body: [project({ id: 'p_1' }), project({ id: 'p_2' })] },
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
      '/api/projects/p_2/tasks': { body: { tasks: [] } },
      '/api/projects/p_1/bindings': { body: [] },
      '/api/projects/p_2/bindings': { body: [] },
    })
    const w = mountAtRoot()
    await flush(w)

    const regions = w.element.querySelectorAll('.af-groups .af-region')
    expect(regions, '一个项目一块 `.af-region`').toHaveLength(2)

    for (const region of Array.from(regions)) {
      // 四行摘要在 `.af-region__body` 里；最后一个 `.af-row` 是动作行。
      const rows = region.querySelectorAll('.af-region__body > .af-row')
      expect(rows, '四行摘要 + 一行动作').toHaveLength(5)
      const labels = Array.from(rows).map(
        (r) => r.querySelector('.af-ov__k')?.textContent?.trim() ?? '',
      )
      // 顺序也是判据：原型从上到下就是这四句，其中两行没有 `.af-ov__k`（动作行）。
      expect(labels).toEqual(['上次做到哪', '正在跑', '等你决定', '最近总结', ''])
      // 两个动作都指向真实页面（看板 / 设置）。执行监控是骨架页且要 execId，
      // 见 `OverviewPage.vue` 头注释的「一处与原型的有意偏离」。
      expect(region.querySelectorAll('.af-region__body > .af-row--tight .af-btn')).toHaveLength(2)
    }
  })

  it('空着的那两行说出归属，不编假摘要', async () => {
    stubFetch({
      '/api/projects': { body: [project({ id: 'p_1' })] },
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
      '/api/projects/p_1/bindings': { body: [] },
    })
    const w = mountAtRoot()
    await flush(w)

    const text = w.element.textContent ?? ''
    // 「上次做到哪」要执行时间线 → v0.8.2；「最近总结」要管家 → v0.9.0。
    expect(text).toContain('v0.8.2')
    expect(text).toContain('v0.9.0')
    // 假的相对时间会被验收当成真数据。页面里不许出现「小时前」「分钟前」。
    expect(text).not.toMatch(/\d+\s*(小时|分钟|天)前/)
  })

  it('「正在跑」「等你决定」读真数据：running / failed / review 各自计数', async () => {
    stubFetch({
      '/api/projects': { body: [project({ id: 'p_1' })] },
      '/api/projects/p_1/tasks': {
        body: {
          tasks: [
            task({ id: 't1', state: 'running', executions: [execution('exe_1')] }),
            task({ id: 't2', state: 'failed' }),
            task({ id: 't3', state: 'review' }),
            // 两个 pending：让「失败」与「待办」的条数**不相等**。相等时把
            // `failed` 的过滤条件换成 `pending` 也照样是 1，断言会假绿
            // （2026-11-17 变异验证实测过这一条）。
            task({ id: 't4', state: 'pending' }),
            task({ id: 't5', state: 'pending' }),
          ],
        },
      },
      '/api/projects/p_1/bindings': { body: [binding('n_1'), binding('n_2')] },
    })
    const w = mountAtRoot()
    await flush(w)

    const root = w.element
    expect(root.querySelector('[data-row="running"]')?.textContent).toContain('1 个执行进行中')
    const decide = root.querySelector('[data-row="decide"]')?.textContent ?? ''
    expect(decide).toContain('1 个失败待处置')
    expect(decide).toContain('1 张待验收')
    expect(root.querySelector('.af-region__note')?.textContent).toContain('2 台已绑定')
  })

  it('「正在跑」不许写出设备名——Hub 根本不发那个字段', async () => {
    // 这一段是**对着活 Hub 实测出来的**（2026-11-17）：`ExecutionView`
    // （`crates/hub-server/src/routes/tasks.rs:144`）九个字段里没有
    // `device_id`，所以 `e.device_id` 恒为 `undefined`。曾经的夹具自己造了
    // 这个字段，于是「device-a」在单测里显示得好好的，真链路上那一行永远是
    // 「1 个执行进行中」——两边各测各的，中间那段没人验。
    //
    // 判据分两半：① 页面不出现任何设备 id 形状的串；② `Execution` 类型里
    // 不再声明 `device_id`（声明了就等于给下一次「读一个不存在的字段」发许可）。
    stubFetch({
      '/api/projects': { body: [project({ id: 'p_1' })] },
      '/api/projects/p_1/tasks': {
        body: { tasks: [task({ id: 't1', state: 'running', executions: [execution('exe_1')] })] },
      },
      '/api/projects/p_1/bindings': { body: [binding('n_1')] },
    })
    const w = mountAtRoot()
    await flush(w)

    const running = w.element.querySelector('[data-row="running"]')?.textContent ?? ''
    expect(running).toContain('1 个执行进行中')
    expect(running).not.toContain('n_1')

    const src = readRepoFile('packages/web/src/domains/project/taskTypes.ts')
    const execBlock = /export interface Execution \{[\s\S]*?\n\}/.exec(src)?.[0] ?? ''
    expect(execBlock, 'Execution 接口还在').not.toBe('')
    // 剥注释再判：留着一段「这里没有 device_id」的说明是**要的**（防下一个人
    // 又补回来），但它不该被同一条断言当成违规。
    const code = execBlock.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '')
    expect(code, 'Hub 不发 device_id，类型里就不许声明它').not.toMatch(/\bdevice_id\b/)
  })

  it('摘要读不到时写「读不到」，不把失败写成 0', async () => {
    stubFetch({
      '/api/projects': { body: [project({ id: 'p_1' })] },
      '/api/projects/p_1/tasks': { status: 500, body: { error: 'internal' } },
      '/api/projects/p_1/bindings': { status: 500, body: { error: 'internal' } },
    })
    const w = mountAtRoot()
    await flush(w)

    const root = w.element
    const running = root.querySelector('[data-row="running"]')?.textContent ?? ''
    // 「没有在跑的执行」是**真 0**；读失败必须说读失败，两者不能混。
    expect(running).not.toContain('没有在跑的执行')
    expect(root.querySelector('.af-region__note')?.textContent).toContain('绑定数读不到')
    // 项目名本身还在：摘要失败不该把整块吞掉。
    expect(root.textContent).toContain('内部指标台')
  })
})

/**
 * 原型 `prototype/index.html` 的 4 块结构（v0.8.0 子任务 12）。
 *
 * 此前这一页只有「项目列表 + 待办空态」，原型的三块说明性结构全缺：
 * 为什么按项目分的提示条（`:26`）、已归档折叠（`:112`）、管理面三行表（`:126`）。
 *
 * **判红方式**：删掉其中任一块，对应用例立刻红。
 */
describe('OverviewPage：原型要求的 4 块结构', () => {
  it('提示条解释「这一页为什么是按项目分」', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const callout = w.element.querySelector('.af-callout--info')?.textContent ?? ''
    expect(callout).toContain('这一页为什么是「按项目分」')
    // 这句话要回答的是「设备为什么退到管理面」，不是一句排版说明。
    expect(callout).toContain('设备是拿来干活的')
  })

  it('已归档默认折叠，且写明归档可逆', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const fold = w.element.querySelector('details.af-fold')
    expect(fold, '已归档要用原生 details 折叠，默认收起').not.toBeNull()
    // 没有 `open` 属性 = 默认折叠：归档不该占着主列表的位置。
    expect(fold?.hasAttribute('open')).toBe(false)
    const text = fold?.textContent ?? ''
    // 产品承诺：归档不是删除，随时能取消。这句比「打开归档列表」更重要。
    expect(text).toContain('归档不是删除')
    expect(text).toContain('随时能取消归档')
  })

  it('管理面三行表：设备 / Agent / 设置各自回答什么问题', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const manage = w.element.querySelector('section[aria-label="管理面"]')
    expect(manage, '管理面必须是独立一块 `.af-region`').not.toBeNull()
    const rows = Array.from(manage?.querySelectorAll('tbody tr') ?? [])
    expect(rows).toHaveLength(3)
    const text = rows.map((r) => r.textContent ?? '').join('|')
    expect(text).toContain('设备')
    expect(text).toContain('Agent')
    expect(text).toContain('设置')
    // 「回答什么问题」那一列必须真的在：只列三个名字等于没解释分工。
    expect(text).toContain('有没有漂移')
    expect(text).toContain('磁盘清理策略')
    // 这一块的标题要点明它与管理无关（原型 `:129` 的 region note）。
    const note = manage?.querySelector('.af-region__note')?.textContent ?? ''
    expect(note).toContain('机器怎么样')
  })

  it('管理面「进入」真的能到那三页', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const links = Array.from(w.element.querySelectorAll<HTMLButtonElement>('.af-ov__link'))
    expect(links).toHaveLength(3)
    await links[1]?.click()
    expect(location.pathname).toBe('/agents')
  })
})

/**
 * 管家抽屉壳（子任务 6，原型 `index.html:161-199`）。
 *
 * **只验壳，不验内容**：面板里这一版没有业务逻辑（不发请求、不建建议、不派
 * 任务），只有一句「归 v0.9.0」。判据是「默认不可见 → 点 fab 可见 → Esc 关掉」，
 * 以及「必须写明这一版没有建议」，免得这个壳被当成管家已经能用。
 */
describe('OverviewPage：管家抽屉壳', () => {
  it('默认不可见（aria-hidden），点 fab 才打开', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    // 抽屉 Teleport 到 body（理由见 OverviewPage.vue 的模板注释），所以从
    // `document` 上取，`w.element` 里没有它。
    const drawer = document.querySelector('.af-drawer')
    expect(drawer, '抽屉壳必须在 DOM 里（收起时靠 aria-hidden 退场）').not.toBeNull()
    expect(drawer?.getAttribute('aria-hidden')).toBe('true')
    expect(drawer?.classList.contains('af-drawer--open')).toBe(false)

    // fab 也在 Teleport 出去的浮层容器里（与抽屉同属一层），所以同样从 document 取。
    const fab = document.querySelector<HTMLElement>('.af-fab')
    expect(fab?.getAttribute('aria-expanded')).toBe('false')
    fab?.click()
    await flush(w)

    expect(drawer?.getAttribute('aria-hidden')).toBe('false')
    expect(drawer?.classList.contains('af-drawer--open')).toBe(true)
    expect(fab?.getAttribute('aria-expanded')).toBe('true')
    // 打开后焦点要落在面板里，否则键盘用户 Tab 进去要先穿过整页。
    expect(document.activeElement?.getAttribute('aria-label')).toBe('关闭管家面板')
  })

  it('面板里写明「这一版没有建议」，不画假的今日摘要', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const drawerText = document.querySelector('.af-drawer')?.textContent ?? ''
    expect(drawerText).toContain('v0.9.0')
    // 原型 `:170-174` 是「今天三件事：…」——那是假数据，这一版不许出现。
    expect(drawerText).not.toContain('今天三件事')
    // 输入框与发送按钮必须禁用：能打字却发不出去比看不见更糟。
    const input = document.querySelector<HTMLInputElement>('.af-drawer__compose .af-input')
    expect(input?.disabled).toBe(true)
    const send = document.querySelector<HTMLButtonElement>('.af-drawer__compose .af-btn')
    expect(send?.disabled).toBe(true)
  })

  it('Esc 关掉抽屉——键位挂在 document 上，焦点在页面别处时也要生效', async () => {
    // 这一段是对着活 Hub 实测出来的（2026-11-17）：早先键位是模板上的
    // `@keydown.esc`，挂在 `<aside>` 上。**事件从 aside 里冒上来时它有效**，
    // 于是「dispatch 到 drawer 上」这一种写法从头到尾都是绿的；但用户点了一下
    // 页面别处之后，Esc 一次什么也不发生。判据要压在**事件源不在抽屉里**。
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    // fab 也在 Teleport 出去的浮层容器里（与抽屉同属一层），所以同样从 document 取。
    const fab = document.querySelector<HTMLElement>('.af-fab')
    fab?.click()
    await flush(w)

    const drawer = document.querySelector<HTMLElement>('.af-drawer')
    expect(drawer?.classList.contains('af-drawer--open')).toBe(true)

    // 焦点挪到抽屉之外，再按键：这条路是旧实现死掉的那条。
    ;(document.body as HTMLElement).focus()
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    await flush(w)

    expect(drawer?.classList.contains('af-drawer--open')).toBe(false)
    expect(drawer?.getAttribute('aria-hidden')).toBe('true')
    expect(fab?.getAttribute('aria-expanded')).toBe('false')
    // 还焦点给打开者：焦点落在已收起（`visibility: hidden`）的抽屉里等于丢掉。
    expect(document.activeElement).toBe(fab)
  })

  it('抽屉开着时背后是 inert，关掉后解除——覆盖层不能让人 Tab 进整页', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    const host = w.element.parentElement ?? document.body
    // fab 也在 Teleport 出去的浮层容器里（与抽屉同属一层），所以同样从 document 取。
    const fab = document.querySelector<HTMLElement>('.af-fab')
    fab?.click()
    await flush(w)

    // `push()` 把除容器外的 body 子节点全设 inert；装着本页的那个节点就在其中。
    expect((host as HTMLElement).inert, '抽屉开着时背后必须 inert').toBe(true)
    expect(host.getAttribute('aria-hidden')).toBe('true')

    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    await flush(w)

    expect((host as HTMLElement).inert, '关掉后必须解掉 inert').toBe(false)
    expect(host.hasAttribute('aria-hidden')).toBe(false)
  })

  it('卸载时摘掉 document 上的 keydown——不然下一个页面的 Esc 被它接走', async () => {
    stubFetch({ '/api/projects': { body: [] } })
    const w = mountAtRoot()
    await flush(w)

    // fab 也在 Teleport 出去的浮层容器里（与抽屉同属一层），所以同样从 document 取。
    const fab = document.querySelector<HTMLElement>('.af-fab')
    fab?.click()
    await flush(w)
    const drawer = document.querySelector<HTMLElement>('.af-drawer')
    expect(drawer?.classList.contains('af-drawer--open')).toBe(true)

    // 卸载时抽屉还开着：这是最容易被漏掉的那条路（用户按浏览器后退离开页面）。
    const host = w.element.parentElement ?? document.body
    w.unmount()
    await flush(w)

    expect((host as HTMLElement).inert, '卸载后不能把留着的 inert 带走').toBe(false)
    // 再按一次 Esc 不该抛错、也不该有残留监听处理它。
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  })
})
