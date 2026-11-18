/* ===========================================================================
   NewProjectPage 的测试。
   ---------------------------------------------------------------------------
   这是本里程碑**唯一会写数据**的页面（`POST /api/projects`），所以三条分支
   各配一条用例：空名拒绝、成功之后怎么离开、两种失败措辞。

   为什么要断「跳转用的是 replace」：这不是风格问题。用 `push` 的话，用户建完
   项目按「后退」会回到那张已经提交过的表单，再点一次「创建」就多出一个同名
   项目。而这条语义在页面上**看不见**，只有测试守得住。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import NewProjectPage from '@/domains/project/NewProjectPage.vue'
import { mountIn } from '@/test/mount'
import { settle, stubFetch } from '@/test/http'
import { navigate, route } from '@/router'

/** 一个建成功的响应体。 */
const CREATED = {
  id: 'p_9',
  name: '内部指标台',
  shape: 'isolated',
  theme_color: null,
  upstream_url: null,
  archived_at: null,
  created_at: 'unix:1700000000',
}

/** 等 fetch 链 + Vue 渲染队列都走完。 */
async function flush(w: ReturnType<typeof mountIn>): Promise<void> {
  for (let i = 0; i < 8; i++) {
    await w.vm.$nextTick()
    await settle(1)
  }
}

/**
 * 把地址停在新建页，再挂组件。
 *
 * `route` 是模块级的单例，上一个用例跳走之后它还留在那个地址上。不重置的话，
 * 「没跳走」这类断言会**因为上一个用例的残留而误判**——第一次写这几条时
 * 就红在这里（失败用例拿到了上一条用例留下的 `board`）。
 */
function mountAtNew(): ReturnType<typeof mountIn<typeof NewProjectPage>> {
  navigate('/projects/new', { replace: true })
  return mountIn(NewProjectPage)
}

/** 填名字再提交。提交走表单的 submit，和用户按下回车/点按钮是同一条路径。 */
async function submitWith(w: ReturnType<typeof mountIn>, name: string): Promise<void> {
  await w.find('input[type="text"]').setValue(name)
  await w.find('form').trigger('submit')
  await flush(w)
}

/** 选中一张形态卡（与用户点卡片同一条 `v-model` 路径）。 */
async function pickShape(w: ReturnType<typeof mountIn>, key: string): Promise<void> {
  const radio = w.find(`input[type="radio"][value="${key}"]`)
  await radio.setValue()
  await flush(w)
}

describe('NewProjectPage：默认形态', () => {
  it('默认选隔离式，不是中心式', async () => {
    stubFetch({})
    const w = mountAtNew()
    await flush(w)

    // 判据是「猜错的代价」：隔离式猜错只是多点一次升级，中心式猜错改不回去。
    // 所以断言的是**选中了哪张卡**，而不是「有这三张卡」。
    const checked = w.find('input[type="radio"]:checked')
    expect((checked.element as HTMLInputElement).value).toBe('isolated')

    const selected = w.findAll('[data-selected="true"]')
    expect(selected).toHaveLength(1)
    expect(selected[0]?.text()).toContain('隔离式')
  })
})

describe('NewProjectPage：空名', () => {
  it('空名被拒并给出提示，且**一个请求都不发**', async () => {
    const fetchMock = stubFetch({})
    const w = mountAtNew()
    await flush(w)

    await submitWith(w, '   ')

    expect(w.element.textContent).toContain('给项目起个名字')
    // 光断提示不够：真正要守的是「没打后端」。空名发出去会建出一个没有名字的
    // 项目，而那个项目在建好之后没有任何界面能删它（第一版不提供删除）。
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

describe('NewProjectPage：建成功', () => {
  /* **本条在 v0.8.1 子任务 13 改写**（原「跳到新项目的看板，且用 replaceState」）。
     原来建项目成功就直接跳看板；现在成功先进第二步（选设备与目录），
     用户在那里「绑定并进入看板」或「跳过」才跳。所以这条拆成两半：
     ① 建成功后**停在原地、进第二步**（不再跳走）；
     ② 第二步点「跳过」后跳到看板，且用的是 `replace`（后退不回表单）。
     `replace` 的原理由一字未变：用 `push` 的话，用户建完按「后退」会回到
     那张已提交过的表单，再点一次「创建」就多出一个同名项目。 */
  it('建成功后停在第二步（选设备与目录），不直接跳看板', async () => {
    stubFetch({ '/api/projects': { body: CREATED }, '/api/devices': { body: [] } })
    const w = mountAtNew()
    await flush(w)

    const before = history.length
    await submitWith(w, '内部指标台')

    // 还留在新建页——第二步是同页的，不是另一个地址。
    expect(route.value.name).toBe('projects-new')
    expect(history.length).toBe(before)
    // 第二步的入口按钮在，并且第一步的表单已经收起。
    expect(w.text()).toContain('选设备与目录')
    expect(w.find('form').exists()).toBe(false)
  })

  it('第二步点「跳过」才去看板，且用 replaceState（后退不回表单）', async () => {
    // 设备列表为空 → 第二步渲染「还没有设备接入」空态，那个空态里就有「先去看板」。
    stubFetch({ '/api/projects': { body: CREATED }, '/api/devices': { body: [] } })
    const w = mountAtNew()
    await flush(w)
    await submitWith(w, '内部指标台')

    const before = history.length
    // `button:has-text()` 不是 CSS 选择器，vt-dom 的选择器引擎不认；
    // 用 findAll 后按文本挑，与用户点那颗按钮等价。
    const skip = w.findAll('button').find((b) => b.text().includes('先去看板'))
    expect(skip, '第二步空态里应有「先去看板」').toBeTruthy()
    await skip!.trigger('click')
    await flush(w)

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_9')
    expect(history.length).toBe(before)
  })

  it('项目名前后空格被去掉再发给后端', async () => {
    const fetchMock = stubFetch({ '/api/projects': { body: CREATED } })
    const w = mountAtNew()
    await flush(w)

    await submitWith(w, '  内部指标台  ')

    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined
    expect(JSON.parse(String(init?.body)).name).toBe('内部指标台')
  })
})

describe('NewProjectPage：两种失败措辞分开', () => {
  it('后端说不行时带出错误码，用户能拿它去查', async () => {
    stubFetch({
      '/api/projects': { status: 400, body: { error: 'name_taken' } },
    })
    const w = mountAtNew()
    await flush(w)

    await submitWith(w, '内部指标台')

    expect(w.element.textContent).toContain('建项目失败（name_taken）')
    // 失败之后必须留在表单上——跳走的话用户填的东西就没了。
    expect(route.value.name).not.toBe('board')
  })

  it('Hub 连不上时说「连不上」，不说成是后端的错', async () => {
    // 不声明端点 → `stubFetch` 用 500，但那不是 400 那种「后端明确拒绝」。
    // 这里直接让 fetch 抛，模拟真正的网络层失败。
    const w = mountAtNew()
    await flush(w)

    // 先挂载完再换掉 fetch：页面挂载时不发请求，所以安全。
    const original = globalThis.fetch
    globalThis.fetch = (() => Promise.reject(new TypeError('failed to fetch'))) as typeof fetch
    try {
      await submitWith(w, '内部指标台')
    } finally {
      globalThis.fetch = original
    }

    expect(w.element.textContent).toContain('Hub 连不上')
    // 两种措辞必须**不同**：都写成「建项目失败」的话，用户会以为是自己填错了。
    expect(w.element.textContent).not.toContain('建项目失败（')
  })
})

describe('NewProjectPage：镜像式上游 URL', () => {
  it('选镜像式不填 URL 不发请求并给中文提示', async () => {
    const fetchMock = stubFetch({})
    const w = mountAtNew()
    await flush(w)

    await pickShape(w, 'mirror')
    await submitWith(w, '内部指标台')

    expect(w.element.textContent).toContain('镜像式项目要填一个上游 git 仓库地址')
    // 真正要守的是「没打后端」：一个没有上游的镜像式项目建出来之后，
    // 第一版没有任何界面能删它。
    expect(fetchMock).not.toHaveBeenCalled()
    expect(route.value.name).not.toBe('board')
  })

  it('填了 URL 时请求体带 upstream_url', async () => {
    const fetchMock = stubFetch({
      '/api/projects': { body: { ...CREATED, shape: 'mirror' } },
    })
    const w = mountAtNew()
    await flush(w)

    await pickShape(w, 'mirror')
    const url = 'https://github.com/you/metrics.git'
    expect(w.findAll('input[type="text"]')).toHaveLength(2)
    await w.find('input[type="text"]').setValue('内部指标台')
    await w.find('input[placeholder^="比如：https://"]').setValue(`  ${url}  `)
    await w.find('form').trigger('submit')
    await flush(w)

    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined
    const body = JSON.parse(String(init?.body))
    expect(body.shape).toBe('mirror')
    expect(body.upstream_url).toBe(url)
  })
})

describe('NewProjectPage：提交中', () => {
  it('提交中按钮被禁用并改文案，挡住重复提交', async () => {
    let release: (() => void) | undefined
    const gate = new Promise<void>((r) => {
      release = r
    })
    /* **按路径分开应答**（v0.8.1 子任务 13）：这个桩原来对所有请求都回项目体，
       现在提交成功会进第二步、进而请 `/api/devices`，设备列表拿到一个对象
       会让 `devices.length` 变 `undefined`。所以设备请求直接回 `[]`。 */
    globalThis.fetch = (async (input: RequestInfo | URL) => {
      const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
      const path = new URL(raw, 'http://hub.local').pathname
      if (path.startsWith('/api/devices')) {
        return new Response(JSON.stringify([]), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      }
      await gate
      return new Response(
        JSON.stringify({
          id: 'p_9',
          name: 'x',
          shape: 'isolated',
          theme_color: null,
          archived_at: null,
          created_at: 'unix:1700000000',
        }),
        { status: 200, headers: { 'Content-Type': 'application/json' } },
      )
    }) as typeof fetch

    const w = mountAtNew()
    await flush(w)

    await w.find('input[type="text"]').setValue('内部指标台')
    await w.find('form').trigger('submit')

    /* **不能先 `flush(w)` 再看按钮**：这个桩故意卡在 `gate` 上，而 `flush` 只
       等若干个 tick——请求还悬着时表单仍在，按钮该是禁用态。原用例先 flush
       再看，改成两步之后第二步会把表单换掉，于是 `button[type="submit"]`
       取不到。这里只推进一个 tick，让 `submitting` 反映到 DOM 上。 */
    await w.vm.$nextTick()
    await settle(1)

    const btn = w.find('button[type="submit"]')
    expect(btn.text()).toContain('正在创建…')
    expect((btn.element as HTMLButtonElement).disabled).toBe(true)

    release?.()
    await flush(w)
  })
})

describe('NewProjectPage：第二步（选设备与目录）的前置校验', () => {
  /* 为什么这三条单独成组：`finishBind()` 的两条前置校验与 `pickDevice()` 的
     默认目录推导此前**整段没有用例**（`NewProjectPage.vue` 行覆盖 87.27%，
     未覆盖行 68 / 87-89 / 157-192）。反向验证过：把 `finishBind` 里「固定目录
     不能为空」那句换成 `'x'`（即删掉这条校验），整套测试仍然全绿——两个前置
     校验是**没有人看着的**。这三条就是那道门。 */
  const TWO_DEVICES = {
    '/api/projects': { body: CREATED },
    '/api/devices': {
      body: [
        {
          id: 'n_1',
          name: 'mbp.local',
          platform: 'darwin/arm64',
          status: 'online',
          last_seen: 'unix:1700000000',
          cpu: 10,
          mem: 20,
          gpu: false,
          agents: [],
          usable_agents: [],
          derived: false,
          key_fingerprint: '9f3***21b',
          node_version: '0.1.0',
        },
        {
          id: 'n_2',
          name: 'pi',
          platform: 'linux/arm64',
          status: 'offline',
          last_seen: 'unix:1700000000',
          cpu: 5,
          mem: 10,
          gpu: false,
          agents: [],
          usable_agents: [],
          derived: false,
          key_fingerprint: 'aa1***9cc',
          node_version: '0.1.0',
        },
      ],
    },
  }

  /** 把页面推到第二步，并等设备列表落地。 */
  async function reachStep2(w: ReturnType<typeof mountIn>): Promise<void> {
    await flush(w)
    await submitWith(w, '内部指标台')
  }

  /** 点「绑定并进入看板」。 */
  async function clickBind(w: ReturnType<typeof mountIn>): Promise<void> {
    const bind = w.findAll('button').find((b) => b.text().includes('绑定并进入看板'))
    expect(bind, '第二步应有「绑定并进入看板」').toBeTruthy()
    await bind!.trigger('click')
    await flush(w)
  }

  it('没选设备就点「绑定」：给中文提示、**一个绑定请求都不发**、也不跳走', async () => {
    const fetchMock = stubFetch(TWO_DEVICES)
    const w = mountAtNew()
    await reachStep2(w)

    // 一个设备都没选（`deviceId` 仍是 ''）。此时默认目录也还是空的，
    // 所以这条同时守住了「校验顺序」：先说「先选一台设备」，
    // 而不是报一个用户没法理解的目录问题。
    await clickBind(w)

    expect(w.text()).toContain('先选一台设备')
    expect(w.text()).not.toContain('固定目录不能为空')
    // 真正要守的是「没打后端」：拿空 deviceId 去 PUT 会打到
    // `/api/projects/p_9/bindings/`（尾斜杠），后端当成 404，用户看到的是
    // 「绑定失败」，而真相是他还没选设备。
    const puts = fetchMock.mock.calls.filter(
      ([, init]) => (init as RequestInit | undefined)?.method === 'PUT',
    )
    expect(puts, '没选设备时不该发绑定请求').toHaveLength(0)
    expect(route.value.name).toBe('projects-new')
  })

  it('选了设备但把目录清空：拒绝并说明为什么（空目录会把工作树建在设备根上）', async () => {
    const fetchMock = stubFetch(TWO_DEVICES)
    const w = mountAtNew()
    await reachStep2(w)

    // 选第一台设备（走 `@change` → `pickDevice`，与用户点标签同一条路）。
    await w.find('input[name="af-new-device"][value="n_1"]').setValue()
    await flush(w)

    // `pickDevice` 刚推导出默认目录，这里把它清掉——模拟用户手动删空。
    const dir = w.find('input[placeholder^="比如：~/work"]')
    expect((dir.element as HTMLInputElement).value, 'pickDevice 应已填好默认目录').not.toBe('')
    await dir.setValue('   ')
    await flush(w)

    await clickBind(w)

    expect(w.text()).toContain('固定目录不能为空')
    expect(w.text()).toContain('工作树建在设备根上')
    // 目录为空时 PUT 出去的 `fixed_dir` 是 `''`，后端要么拒绝要么建在根上，
    // 两种都不是用户想要的——所以一个请求都不该发。
    const puts = fetchMock.mock.calls.filter(
      ([, init]) => (init as RequestInit | undefined)?.method === 'PUT',
    )
    expect(puts, '固定目录为空时不该发绑定请求').toHaveLength(0)
    expect(route.value.name).toBe('projects-new')
  })

  it('选中设备时按项目名推导默认目录：~/work/<项目名，空格换连字符>', async () => {
    stubFetch(TWO_DEVICES)
    const w = mountAtNew()
    await flush(w)
    // 项目名带一个空格，用来同时验证 slug 规则（`replace(/\s+/g, '-')`）。
    await submitWith(w, '内部 指标台')

    await w.find('input[name="af-new-device"][value="n_1"]').setValue()
    await flush(w)
    const dir = w.find('input[placeholder^="比如：~/work"]')
    expect((dir.element as HTMLInputElement).value).toBe('~/work/内部-指标台')

    // 已经选中的设备再点一次**不覆盖**用户手填的目录——否则改一次设备
    // 就把用户刚敲的路径冲掉了。
    await dir.setValue('/srv/custom')
    await w.find('input[name="af-new-device"][value="n_2"]').setValue()
    await flush(w)
    expect((dir.element as HTMLInputElement).value).toBe('/srv/custom')
  })
})

/* ===========================================================================
   v0.8.1 子任务 7：页面骨架与原型逐层对齐（`prototype/project-new.html`）
   ---------------------------------------------------------------------------
   只量**结构**，不量外观。守的是「根是不是原型那两层的容器」：上一版根是
   自造的 `.af-page`，那一族类在子任务 2 已随类名删除，于是样式表里没有任何
   规则命中它——模板「看起来是页面」，实际是一张没有滚动语义的裸页面。
   外观（正文宽、形态卡间距）在 `.af-new__body` 的 scoped 规则里。

   为什么用 `querySelector` 而不是 `w.element.classList`：`mountIn` 走
   `attachTo`，`w.element` 是**挂载点**那个 host `div`（`class=""`），页面自己
   的根在它里面。拿挂载点问类名永远是 false，这条会以「骨架没改」误红。
   =========================================================================== */

describe('NewProjectPage（v0.8.1）：骨架与原型 project-new.html 对齐', () => {
  it('page_root_uses_af_body：整页一个 .af-body，页头是共享 PageHead 的 .af-head', async () => {
    stubFetch({})
    const w = mountAtNew()
    await flush(w)

    // 原型 `project-new.html:15-23`：`header.af-head`（`:15-21`）与 `.af-body`（`:23`）是两个兄弟。
    // 判据取「整页只有一个」——将来谁在 `.af-body` 里再包一层它，这一条会红在
    // 「多个」而不是静默取第一个通过。
    const bodies = Array.from(w.element.querySelectorAll('.af-body'))
    expect(bodies, '整页只该有一个 .af-body（页面内容区）').toHaveLength(1)
    // 页头若退回自造的类名，`.af-head` 的几何与内边距一条都不生效。
    expect(w.element.querySelectorAll('.af-head'), '页头必须是原型的 .af-head').toHaveLength(1)
    expect(bodies[0]?.classList.contains('af-head'), '页头不能塞在 .af-body 里面').toBe(false)
  })
})
