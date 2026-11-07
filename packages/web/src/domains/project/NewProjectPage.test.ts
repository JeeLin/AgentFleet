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
  it('跳到新项目的看板，且用 replaceState（后退不回表单）', async () => {
    stubFetch({ '/api/projects': { body: CREATED } })
    const w = mountAtNew()
    await flush(w)

    const before = history.length
    await submitWith(w, '内部指标台')

    expect(route.value.name).toBe('board')
    expect(route.value.params.id).toBe('p_9')
    // `replace` 而不是 `push`：历史栈不该变长，否则用户按「后退」会回到
    // 这张已经提交过的表单，再点一次「创建」就多出一个同名项目。
    // 反向验证：把 `NewProjectPage.vue` 的 `{ replace: true }` 删掉，这条会红。
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
    globalThis.fetch = (async () => {
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
    await flush(w)

    const btn = w.find('button[type="submit"]')
    expect(btn.text()).toContain('正在创建…')
    expect((btn.element as HTMLButtonElement).disabled).toBe(true)

    release?.()
    await flush(w)
  })
})
