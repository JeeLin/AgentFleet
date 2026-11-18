import { beforeEach, describe, expect, it, vi } from 'vitest'

import { KEYS, actCount } from '@/shared/keynav'
import { depth, isTabbable } from '@/shared/focus'
import { device, mountIn, session } from '@/test/mount'
import { stubFetch } from '@/test/http'
import { cssRuleOf, readRepoFile } from '@/test/css'
import { stripCssComments } from '@/test/css-scan'
import {
  EMPTY_SHELL_API as EMPTY_API,
  calledUrls,
  clickRailToggle,
  flushShell as flush,
  goTo,
  mountShell as mountApp,
  railOf,
  sentBodiesOf,
} from '@/test/app-shell'

describe('App：外壳骨架', () => {
  // 一级导航**简化自**原型 `shell.js:17-46` 的 5 组 NAV，实现是四组：全站 / 项目 /
  // 收件箱 / 管理面（原型多一组「通知」、且「项目」组是常量）。逐组差异见
  // `App.vue` 的 `NAV_GROUPS` 上方那张对照表。
  // 「项目」组里每项是一个项目（数据来自 `listProjects()`，与总览页同一个端点）。
  // 七个项目内页面（看板 / 任务 / 执行 / 复盘 / 时间线 / 会话 / 项目设置，
  // `router.ts:129-140`）**不进左栏**——它们从某个项目点进去。
  // `新建项目` 是**动作**而不是地方，挂在「项目」组标题右侧的 `.af-rail__add` 上。
  it('左栏是四组（全站 / 项目 / 收件箱 / 管理面），组内项数对得上', async () => {
    stubFetch({ ...EMPTY_API, '/api/projects': { body: [{ id: 'p_1', name: '示例项目' }] } })
    const w = mountApp('/')
    await flush(w)

    const groups = Array.from(w.element.querySelectorAll<HTMLElement>('.af-rail__group'))
    expect(groups.map((g) => g.querySelector('span')?.textContent)).toEqual([
      '全站',
      '项目',
      '收件箱',
      '管理面',
    ])
    // 每组后面跟一个 `.af-rail__list`：全站 2 项 / 项目 1 项（来自 API）/
    // 收件箱 2 项 / 管理面 3 项。
    const lists = Array.from(w.element.querySelectorAll<HTMLElement>('.af-rail__list'))
    expect(lists.map((ul) => ul.querySelectorAll('a').length)).toEqual([2, 1, 2, 3])
  })

  /*
    「总览」在左栏里**只出现一次**：一次是导航项自己的文字，不是「标题 + 条目」。

    必须是「恰好一次」而不是 `toContain`：后者对多出来的那一份视而不见，
    重复正是这条缺陷本身。数的是左栏的**直接文字**——`title` 是同一句话的
    属性副本，不算第二次渲染。

    反向验证：把分组标题那个 `<span>` 改成显示 `item.label`（或把 `<h2>` 收回
    外壳）→ `count` 变 2 以上，红。
  */
  it('左栏里「总览」只出现一次（没有和标题重复）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const rail = w.element.querySelector<HTMLElement>('.af-rail')
    if (!rail) throw new Error('no rail')

    const text = Array.from(rail.querySelectorAll<HTMLElement>('*'))
      .filter((el) => el.children.length === 0)
      .map((el) => el.textContent ?? '')
    expect(text.filter((t) => t.includes('总览')).length).toBe(1)
  })

  it('当前页在导航上有 aria-current', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/devices')
    await flush(w)

    const current = Array.from(w.element.querySelectorAll('[aria-current="page"]')).map((el) =>
      el.textContent?.trim(),
    )
    expect(current).toEqual(['设备'])
  })

  /*
    七个项目内页面**不进左栏**，而这不是「少画几个链接」——它是导航的语义边界：
    左栏是「你可以在哪些地方之间跳」，项目内页面只能从某个项目进去（否则
    「看板」这一项会让用户以为它属于某个跟他无关的项目）。

    `router.ts:129-140` 的项目内路由共 7 条，`App.vue` 的 `NAV_GROUPS` 注释里
    逐个列了名字。这条用例把「注释里那份清单」与「左栏实际渲染」对上：
    每条项目内路由的 `match[0]` 都不许出现在左栏任何链接的 `aria-current`
    判定里（也就是不许有任何一项把项目内路由算成「当前页」）。

    判红方式：往 `NAV_GROUPS` 的任一 `match` 里塞一个项目内路由名
    （例如给「全站」加 `board`）→ 挂到 `/project/x/board` 时左栏会有一项亮起，
    本用例红在「项目内路由不许在左栏高亮」。
  */
  it('项目内页面不进左栏：挂在看板页时左栏没有任何一项是当前页', async () => {
    stubFetch({
      ...EMPTY_API,
      '/api/projects': { body: [{ id: 'p_1', name: '示例项目' }] },
      '/api/projects/p_1': { body: { id: 'p_1', name: '示例项目', shape: 'central' } },
      '/api/projects/p_1/tasks': { body: { tasks: [] } },
    })
    const w = mountApp('/project/p_1/board')
    await flush(w)

    const rail = w.element.querySelector<HTMLElement>('aside.af-rail')
    expect(rail, '左栏必须存在').not.toBeNull()
    // 「项目」组里那个项目的链接按 `isCurrentProject` 标 `aria-current`（那是对的：
    // 左栏该一直指着你在的项目）。所以判据限定在**静态分组的列表**里。
    const staticItems = Array.from(rail!.querySelectorAll('.af-rail__list:not(.af-rail__list--projects) a'))
    expect(staticItems.length, '静态分组里应当有 7 个链接').toBe(7)
    const litOnes = staticItems.filter((a) => a.getAttribute('aria-current') === 'page')
    expect(
      litOnes.map((a) => a.textContent?.trim()),
      '看板是项目内页面，静态分组里不该有一项亮起',
    ).toEqual([])
  })

  it('导航项是真 <a href>：中键 / ⌘ 点击能走浏览器自己的路径', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    // 「项目」组那一列是数据驱动的（这里没有项目），所以取的是三条
    // 静态组里的链接：全站 2 + 收件箱 2 + 管理面 3。
    const hrefs = Array.from(w.element.querySelectorAll<HTMLAnchorElement>('.af-rail__list a')).map(
      (a) => a.getAttribute('href'),
    )
    expect(hrefs).toEqual([
      '/',
      '/projects/archived',
      '/inbox',
      '/inbox/activity',
      '/devices',
      '/agents',
      '/settings',
    ])
  })

  it('main 是 tabindex="-1"（可编程聚焦，但不由键盘穿过）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(w.element.querySelector('main')?.getAttribute('tabindex')).toBe('-1')
  })

  /* ===========================================================================
     外壳的结构与原型对齐（v0.8.1 子任务 2）
     ---------------------------------------------------------------------------
     原型 `prototype/index.html:12` 的外壳是 `.af-shell`（两个网格区：左栏 +
     主区），左栏是 `aside.af-rail`。实现此前自造了一套「应用外壳」四件套
     （根 / 侧栏 / 全局顶栏 / 全局状态条，原型命中 0），两套网格同时
     管一棵子树。这一组钉住「只剩原型那一套」。
     ======================================================================== */

  it('外壳根是 .af-shell，里面只有 aside.af-rail 与 main.af-main 两个网格区', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const shell = w.element.querySelector<HTMLElement>('.af-shell')
    expect(shell, '外壳根必须是原型的 .af-shell').not.toBeNull()
    // 网格区是**直接子节点**：少一层嵌套，收起态那条
    // `html[data-rail='off'] .af-shell { grid-template-columns: 0 1fr }` 才对得上。
    const aside = shell?.querySelector(':scope > aside.af-rail')
    expect(aside, '左栏必须是 .af-shell 的直接子节点 aside.af-rail').not.toBeNull()
    expect(shell?.querySelector(':scope > main.af-main')).not.toBeNull()
    // 自造层已删除：外壳里不许再出现那四个类名中的任何一个。
    // 判据串用 `\u002d` 拼（不是把字面量直接写出来）：全仓门禁扫的是源码
    // 文本，写出来会让本文件自己命中自己。
    const gone = ['af-app', 'af\u002dsidebar', 'af\u002dtopbar', 'af-statusbar']
    for (const name of gone) {
      expect(w.element.querySelector(`.${name}`), `.${name} 是自造层，必须已删除`).toBeNull()
    }
  })

  it('左栏恰好 4 个分组标题，且「新建项目」是动作挂在「项目」组标题上', async () => {
    stubFetch({ ...EMPTY_API, '/api/projects': { body: [{ id: 'p_1', name: '示例项目' }] } })
    const w = mountApp('/')
    await flush(w)

    expect(w.element.querySelectorAll('.af-rail__group').length).toBe(4)
    const add = w.element.querySelector<HTMLAnchorElement>('.af-rail__group .af-rail__add')
    expect(add?.getAttribute('href')).toBe('/projects/new')
  })

  it('左栏底部的状态那几行落在 .af-rail__foot 里，不是全局状态条', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const foot = w.element.querySelector<HTMLElement>('.af-rail__foot')
    expect(foot).not.toBeNull()
    // 原型 `shell.js:146-151` 的两行：第一行「单用户 · Hub 本地」是事实，
    // 第二行是计数（真实数据，读不到就不渲染）。
    expect(foot?.textContent).toContain('单用户 · Hub 本地')
    // 「Hub 已连接」是上一版自造的底部状态条文案，原型底部没有它。
    // 用户 2026-11-17：「左侧栏底部的和原型不一致」→ 这条文案必须消失。
    expect(foot?.textContent).not.toContain('Hub 已连接')
    const aside = w.element.querySelector('.af-rail')
    expect(aside?.textContent).toContain('单用户 · Hub 本地')
    expect(w.element.querySelector('footer')).toBeNull()
  })

  /*
    页面标题在整页里**只出现一次**。

    原型没有全局顶栏（`prototype/index.html:15` 的页头属于页面自己），所以外壳
    再渲染一个 `<h2>{{ docTitle }}</h2>` 就是第二个「总览」。放宽成
    `toContain` 或 `>= 1` 都抓不到它——重复正是这条缺陷本身。

    反向验证：把 `<header><h2>{{ docTitle }}</h2></header>` 加回外壳模板 → 红
    （实测 `findAll('h1')` 仍为 1，所以要数的是 h1+h2 的全部标题）。
  */
  it('左栏底部第二行：设备数 / Agent 数用真实数据，且版本号带 v 前缀', async () => {
    stubFetch({
      ...EMPTY_API,
      '/api/devices': { body: [{ id: 'n_1' }, { id: 'n_2' }, { id: 'n_3' }] },
      '/api/agents': { body: [{ id: 'a_1' }, { id: 'a_2' }] },
    })
    const w = mountApp('/')
    await flush(w)

    // 原型 `shell.js:149` 的形态是「3 台设备 · 6 个 Agent · v0.6.0」。
    // **那三个数字是写死的假数据**，用户 2026-11-17 裁定「用真实数据」，
    // 所以这里喂 3 台设备 / 2 个 Agent，断言它真的数了、而不是印一句常量。
    //
    // 反向验证：把 `loadRailCounts` 里的 `devices.value.length` 改成常量 3 →
    // 数字仍对（巧合）；改成 `0` → 红。把 `v{{ appVersion }}` 的 `v` 删掉 → 红
    // （`v9.9.9-test` 是 `test/setup.ts` 给的兜底值）。
    const meta = w.element.querySelector<HTMLElement>('.af-rail__meta')
    expect(meta).not.toBeNull()
    const lines = Array.from(meta!.children).map((c) => c.textContent?.replace(/\s+/g, ' ').trim())
    expect(lines[0]).toBe('单用户 · Hub 本地')
    expect(lines[1]).toContain('3 台设备')
    expect(lines[1]).toContain('2 个 Agent')
    // 版本号带 `v` 前缀，与原型 `v0.6.0` 同形。
    const mono = meta!.querySelector('.af-mono')
    expect(mono?.textContent?.trim()).toBe('v9.9.9-test')
  })

  it('左栏底部第二行：计数读不到时整行不渲染（不写假数字）', async () => {
    // 设备接口 500 → 该行整行消失，第一行仍在。
    // **不许回落到「0 台设备」**：那是在说一句假话，而左栏是导航不是状态面板。
    stubFetch({
      ...EMPTY_API,
      '/api/devices': { status: 500, body: { error: 'boom' } },
    })
    const w = mountApp('/')
    await flush(w)

    const meta = w.element.querySelector<HTMLElement>('.af-rail__meta')
    expect(meta).not.toBeNull()
    expect(meta!.textContent).toContain('单用户 · Hub 本地')
    expect(meta!.textContent).not.toContain('台设备')
    expect(meta!.querySelector('.af-mono')).toBeNull()
  })

  it('左栏底部的 line-height 是字面量 1.7（原型 app.css:104 刻意偏离全局 1.65）', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')

    // **这条只能断声明文本，量不出像素**：happy-dom 不做布局，实测拿不到
    // 19.55px 与 18.975px 的差别；变异验证确认了这一点（把 1.7 改回
    // `var(--p-lh-body)`，其余 40 条用例全绿）。所以退一步，直接钉住「这一处
    // 写的是字面量 1.7」这个事实——它是用户 2026-11-17 报的「底部和原型不一致」
    // 里唯一能静态看守的一条。
    //
    // 原型 `app.css:102-106`：
    //   .af-rail__foot { ... font-size: var(--t-2xs); color: var(--fg-3);
    //                    line-height: 1.7; }
    // 而 `--lh-body` 是 1.65（原型 `tokens.css:27`），正文用的那个。这一处是
    // 原型**唯一**刻意不跟全局行高走的地方，别「统一」回去。
    const at = css.indexOf('\n.af-rail__foot {')
    expect(at, '.af-rail__foot 的规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(css.indexOf('{', at) + 1, css.indexOf('}', at))

    expect(block, '底部的行高必须是字面量 1.7').toContain('line-height: 1.7;')
    expect(block, '不许改回全局的 --p-lh-body（1.65）').not.toContain('var(--p-lh-body)')
  })

  it('收起态：底部动作图标紧跟导航之后，不被 margin-top:auto 推到栏底', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')

    // 展开态 `.af-rail__foot` 的 `margin-top: auto` 是对的（原型 `app.css:103`
    // 靠它把底部推到 224px 栏底）。但 56px 图标条里只有 7 个导航图标
    // （y≈65..345），`auto` 会把两颗动作图标推到 y≈820/870，中间空出 ~475px
    // ——视觉上像两段断开的 UI（2026-11-17 浏览器实测发现并修掉）。
    //
    // 这条只能断声明文本：happy-dom 不做布局，量不出 475px 的空隙。
    // 实测数值（playwright，1440×900）：修前 footY=787 / lastNavBottom=345 →
    // gap=442px；修后 footMarginTop='0px' / footY=505 / gap=0。
    const at = css.indexOf("html[data-rail='off'] .af-rail__foot {")
    expect(at, '图标条里的 .af-rail__foot 规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(css.indexOf('{', at) + 1, css.indexOf('}', at))
    expect(block, '图标条里的底部必须 margin-top: 0（否则被推到栏底）').toContain('margin-top: 0')
    expect(block, '不许留着 auto').not.toContain('margin-top: auto')
  })

  it('收起态：项目列表整块隐藏（否则留下看不见的、说不出名字的焦点陷阱）', async () => {
    stubFetch({ ...EMPTY_API, '/api/projects': { body: [{ id: 'p_1', name: '示例项目' }] } })
    const w = mountApp('/')
    await flush(w)

    // 收起态的两条规则必须成对存在：
    //   ① `.af-rail__label { display: none }` —— 图标条没有文字
    //   ② `.af-rail__list--projects { display: none }` —— 项目行整块消失
    //
    // **缺 ② 是一个无障碍缺陷**，不是视觉瑕疵：项目 `<a>` 的无障碍名来自
    // 可见文字（它没有 `aria-label`），① 把文字吃掉之后，这些链接既看不见、
    // 又没有名字，却仍然**可聚焦、占 40px 高**。实测（playwright，1440×900）：
    // 修前收起态 `.af-rail__list a` 有 12 个，其中 5 个 `aria-label === null`，
    // 且 7 个真图标之间有 179px 的空段（项目行占位）。
    //
    // 反向验证：删掉 ② 那条规则 → 本用例红。
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const at = css.indexOf("html[data-rail='off'] .af-rail__list--projects {")
    expect(at, '项目列表的收起态规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(css.indexOf('{', at) + 1, css.indexOf('}', at))
    expect(block).toContain('display: none')

    // 结构上也要对得上：项目列表挂了这个可区分的类名，非项目列表没挂。
    const projectList = w.element.querySelector('.af-rail__list--projects')
    expect(projectList, '项目列表必须带 .af-rail__list--projects').not.toBeNull()
    const allLists = Array.from(w.element.querySelectorAll('.af-rail__list'))
    expect(
      allLists.filter((l) => l.classList.contains('af-rail__list--projects')).length,
      '只有「项目」组那一列带这个类',
    ).toBe(1)
  })

  it('整页只有一个页面标题（外壳不再重复渲染 docTitle）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const headings = Array.from(w.element.querySelectorAll<HTMLElement>('h1, h2')).map(
      (h) => h.textContent?.trim() ?? '',
    )
    expect(headings.filter((t) => t.includes('总览')).length).toBe(1)
    // 而且那个标题在**主区**里（是页面自己的页头），不在左栏里。
    const main = w.element.querySelector('main.af-main')
    expect(main?.querySelectorAll('h1').length).toBe(1)
  })
})

describe('App：跳转后焦点落在 main', () => {
  beforeEach(() => {
    document.body.innerHTML = ''
  })

  it('首屏就把焦点交给 main（换页的 watch 不会为初始值触发）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(document.activeElement).toBe(w.element.querySelector('main'))
  })

  it('换页后焦点也落在 main，并给 h1 补上 tabindex 让 ⇧Tab 能回到标题', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const other = document.createElement('button')
    document.body.appendChild(other)
    other.focus()
    expect(document.activeElement).toBe(other)

    window.history.pushState(null, '', '/devices')
    window.dispatchEvent(new PopStateEvent('popstate'))
    await flush(w)

    const main = w.element.querySelector('main')
    expect(document.activeElement).toBe(main)
    expect(main?.querySelector('h1')?.getAttribute('tabindex')).toBe('-1')
  })
})

describe('App：键位帮助由 KEYS 渲染', () => {
  /** 帮助面板 Teleport 到 body，所以只能在 document 上找（不在外壳子树里）。 */
  function helpEl(): HTMLElement | null {
    return document.querySelector<HTMLElement>('.af-help')
  }

  it('打开帮助后列出 KEYS 的全部组与条目（不手写第二份表）', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    const help = helpEl()
    expect(help).not.toBeNull()
    // 一行一个键位条目；同一个 act 的 alias（`t` 与 `g g`）各占一行，
    // 所以行数是「条目数」而不是 act 数——表里有多少条，面板里就有多少条。
    const bindings = KEYS.reduce((n, g) => n + g.keys.length, 0)
    expect(help?.querySelectorAll('[data-unimplemented]').length).toBe(bindings)
    expect(actCount()).toBeLessThanOrEqual(bindings)

    for (const group of KEYS) {
      expect(help?.textContent).toContain(group.group)
    }
  })

  it('标题里说明这张表就是键位实现本身', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    expect(helpEl()?.textContent).toContain('这张表就是键位实现本身')
  })

  it('点帮助面板里的关闭按钮能关掉它', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)
    await helpEl()?.querySelector<HTMLElement>('.af-help__head button')?.click()
    await flush(w)

    expect(helpEl()).toBeNull()
  })

  it('帮助开着时把外壳置 inert——背后不是「还能 Tab 过去」的', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)
    const sidebar = w.element.querySelector<HTMLElement>('.af-rail')

    await w.element.querySelector<HTMLElement>('[title^="键位帮助"]')?.click()
    await flush(w)

    // `inertOthers` 把「body 的直接子节点里、不含浮层容器的那几个」置 inert，
    // 所以**外壳的挂载根**（测试里的 host div，真浏览器里的 `#app`）才带上这个
    // 属性——`inert` 不继承，断言子树里某个元素自己是白费。
    // 帮助面板留在 `#app` 里时，`#app` 是 body 唯一的子节点且 `contains` 命中
    // 宿主 → 一个节点都不会被 inert。所以它必须 Teleport 到 body。
    const host = sidebar?.closest<HTMLElement>('body > *')
    expect(host?.inert, '背后那一棵必须整棵 inert').toBe(true)
    expect(host?.getAttribute('aria-hidden')).toBe('true')
    // 侧栏里的可聚焦项因此也不再可 Tab（`isTabbable` 读的正是 `inert`）
    expect(isTabbable(sidebar as Element)).toBe(false)
    expect(helpEl()).not.toBeNull()
  })
})

describe('App：帮助面板的 Esc / 焦点栈', () => {
  function helpEl(): HTMLElement | null {
    return document.querySelector<HTMLElement>('.af-help')
  }

  /** 会话工作台的路由：轨迹抽屉只在那页上注册 `traj:toggle`。 */
  function mountWorkbench(): ReturnType<typeof mountIn> {
    return mountApp('/project/p_1/session/s_1')
  }

  it('? → t → Esc：关掉的是栈顶那一层，另一层不动，栈也不残留', async () => {
    stubFetch({
      '/api/sessions': {
        body: [
          session({ id: 's_1', workspace: { node_id: 'n_1', path: '/srv/app', path_locked: true } }),
        ],
      },
      '/api/devices': { body: [device()] },
    })
    const w = mountWorkbench()
    await flush(w)

    // `?` 打开帮助（进焦点栈）
    document.dispatchEvent(new KeyboardEvent('keydown', { key: '?', bubbles: true, cancelable: true }))
    await flush(w)
    expect(helpEl()).not.toBeNull()

    // `t` 打开轨迹抽屉（第二层）——**它才是栈顶**
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 't', bubbles: true, cancelable: true }))
    await flush(w)
    expect(document.querySelector<HTMLElement>('[data-traj]')).not.toBeNull()

    // 第一次 Esc 关**栈顶**（抽屉），不是帮助：
    // `KEYS` 里 `layer:escape` 的 label 就是「关闭最上层浮层」，而
    // `TrajDrawer.vue` 与 `App.vue` 的 `onHelpKeydown` 都写着「只在本层是
    // 栈顶时生效」——两个 document 级监听靠这条互斥，否则一次 Esc 关两层。
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    await flush(w)

    expect(document.querySelector<HTMLElement>('[data-traj]')).toBeNull()
    expect(helpEl(), '帮助在栈底，不该被这一次 Esc 带走').not.toBeNull()

    // 第二次 Esc 关剩下的那一层
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    await flush(w)
    expect(helpEl()).toBeNull()
    // 栈真的空了：没有指向已移除元素的空壳层
    expect(depth()).toBe(0)
  })
})

describe('App：不知道的地址', () => {
  it('给一个空态而不是白屏，并指回会话列表', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/nope')
    await flush(w)

    expect(w.element.querySelector('.af-empty')?.textContent).toContain('这个地址没有页面')
    expect(document.title).toContain('找不到页面')
  })
})

describe('App：文档标题', () => {
  it('每个页面都有独立标题', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)
    expect(document.title).toBe('总览 · AgentFleet')
  })

  /**
   * `/agents` 已删除，但**不能 404**：它曾是一级导航项，书签与文档链接
   * 都指向它。外壳在首屏与换页时把它重定向到设备页的目录分段。
   *
   * 反向验证：把 `App.vue` 里 `redirectLegacy()` 那两次调用删掉，这条会红在
   * 「地址没被换掉」。
   */
  /*
    `/agents` 在 v0.7.0 起是**真页面**（`prototype/agents.html`），不再是设备页的
    一个分段。

    v0.5.0 时它确实被重定向到 `/devices?view=agents`，那时侧栏只有三项、Agent
    目录并进设备页。新原型把它分回来：设备回答「这台机器在不在线」，Agent 回答
    「这台机器上装了什么、能不能跑」——同一批数据，运维视角与能力视角。
    所以这条用例现在钉的是**地址不动、页面自己渲染出来**。
  */
  it('/agents 是 Agent 页本身，不再重定向到设备分段', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/agents')
    await flush(w)

    // 地址**不动**：重定向会把它改成 /devices?view=agents，而那样用户点回来
    // 就找不到这一页了。
    expect(location.pathname).toBe('/agents')
    expect(document.title).toBe('Agent · AgentFleet')
    // 渲染的是页面本身，不是 404 空态。
    expect(w.element.querySelector('.af-empty')?.textContent ?? '').not.toContain(
      '这个地址没有页面',
    )
  })
})

/* ===========================================================================
   session 过期 → 弹框，不是整页跳走
   ---------------------------------------------------------------------------
   这一组测的是**接在一起的两半**：`http.ts` 的 401 分支弹框（`promptLogin`），
   外壳把框挂出来、并在取消时跳 `/login`。两半各自有单测，但「401 之后用户
   眼前是什么」只有合起来才存在——所以这里从一次真实的 `request()` 出发。
   =========================================================================== */

describe('App：401 之后', () => {
  it('页面里的请求撞 401 → 弹出密码框，地址没动、页面没重建', async () => {
    const fetchMock = stubFetch({ ...EMPTY_API, '/api/devices': { status: 401, body: { error: 'session_expired' } } })
    const w = mountApp('/devices')
    await flush(w)

    const shell = w.element.querySelector('.af-shell')
    /* 用 harness 的 `calledUrls` 而不是就地 `mock.calls.map`（G88）：
       这个函数原本零消费者、注释还在论证「为何该留」——真正该做的是让它
       有消费者，而不是删掉它再让下一处继续就地重写一遍。 */
    expect(calledUrls(fetchMock)).toContain('/api/devices')

    const box = document.querySelector<HTMLElement>('[data-loginprompt]')
    expect(box).not.toBeNull()
    // 整页跳转会重建整个 JS 上下文；这里外壳还是原来那个 DOM 节点
    expect(w.element.querySelector('.af-shell')).toBe(shell)
    expect(location.pathname).toBe('/devices')
    // 401 仍然照常抛给页面（页面停在错误态，不继续渲染一屏读不到的列表）
    await flush(w)
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()
  })

  it('输对密码 → 关框并留在原页面（不导航、不刷新）', async () => {
    const fetchMock = stubFetch({
      ...EMPTY_API,
      '/api/devices': { status: 401, body: { error: 'session_expired' } },
      '/login': { body: { ok: true } },
    })
    const w = mountApp('/devices')
    await flush(w)
    const before = w.element.querySelector('.af-shell')
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()

    const input = document.querySelector<HTMLInputElement>('#login-prompt-password')
    expect(input).not.toBeNull()
    input!.value = 'correct horse battery'
    input!.dispatchEvent(new Event('input'))
    await flush(w)
    document.querySelector<HTMLFormElement>('#login-prompt-password')!.closest('form')!.dispatchEvent(
      new Event('submit', { bubbles: true, cancelable: true }),
    )
    await flush(w)

    expect(document.querySelector('[data-loginprompt]')).toBeNull()
    /* **发出去的口令就是用户输的那个**（G88）：此前这条只断言「框关了」——
       而框关掉只要求 `/login` 回了 ok，哪怕发的是空串或别的字段名也照样绿。
       用 harness 的 `sentBodiesOf` 按 URL 取出那次 POST 的 JSON 体。 */
    const urls = calledUrls(fetchMock)
    const bodies = sentBodiesOf(fetchMock)
    const loginIdx = urls.findIndex((u) => u.includes('/login'))
    expect(loginIdx, '提交后必须真的向 /login 发过请求').toBeGreaterThanOrEqual(0)
    expect(bodies[loginIdx], '/login 的请求体必须是 JSON 对象').toMatchObject({
      password: 'correct horse battery',
    })
    // 「不导航」不是只看地址：外壳 DOM 必须是同一个实例
    expect(location.pathname).toBe('/devices')
    expect(w.element.querySelector('.af-shell')).toBe(before)
  })

  it('点「取消」→ 那一下才整页跳去 /login', async () => {
    const assign = vi.spyOn(location, 'assign').mockImplementation(() => {})
    stubFetch({ ...EMPTY_API, '/api/devices': { status: 401, body: { error: 'session_expired' } } })
    const w = mountApp('/devices')
    await flush(w)
    expect(document.querySelector('[data-loginprompt]')).not.toBeNull()

    Array.from(document.querySelectorAll<HTMLButtonElement>('[data-loginprompt] button'))
      .filter((b) => b.textContent?.includes('取消'))
      .forEach((b) => void b.click())
    await flush(w)

    expect(assign).toHaveBeenCalledWith('/login')
    // 关框与跳转都发生了：框不该留在屏幕上
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })

  it('/login 上不挂这个框（那一页自己的 401 是密码错，不是过期）', async () => {
    stubFetch({})
    const w = mountApp('/login')
    await flush(w)

    // `/login` 是认证页：不带外壳（§5.1），自然也没有这个框
    expect(w.element.querySelector('.af-shell')).toBeNull()
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })

  it('硬开 /login 仍然渲染完整的登录页（不是只留一个弹框）', async () => {
    // 弹框是「 session 过期」的出路，不是它的**唯一**出路：用户主动退出、
    // 或直接敲地址进 /login 时，要看到的是一页完整的登录表单。
    // 少了这一条，401 处置就会变成「永远弹框、登录页再也打不开」。
    stubFetch({})
    const w = mountApp('/login')
    await flush(w)

    const page = w.element.querySelector('.af-auth')
    expect(page).not.toBeNull()
    expect(page?.querySelector('input[type="password"]')).not.toBeNull()
    expect(page?.querySelector('button[type="submit"]')?.textContent).toContain('进入控制台')
    // 仍然没有外壳 chrome：认证页不带导航（§5.1）
    expect(w.element.querySelector('.af-rail')).toBeNull()
    expect(document.querySelector('[data-loginprompt]')).toBeNull()
  })
})

describe('App：登出', () => {
  /** 左栏底部 `.af-rail__foot` 里那个「退出登录」按钮。 */
  function logoutButton(w: ReturnType<typeof mountIn>): HTMLButtonElement | null {
    const all = Array.from(w.element.querySelectorAll('button'))
    return (all.find((b) => b.getAttribute('aria-label') === '退出登录') as
      | HTMLButtonElement
      | undefined) ?? null
  }

  it('左栏有登出入口', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    expect(logoutButton(w)).not.toBeNull()
  })

  it('点登出会先 POST /logout 再去登录页', async () => {
    const assign = vi.spyOn(location, 'assign').mockImplementation(() => {})
    const fetchMock = stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    logoutButton(w)?.click()
    await flush(w)

    const hit = fetchMock.mock.calls.find((c) => String(c[0]).includes('/logout'))
    expect(hit, '登出前必须先通知 Hub 清 cookie').toBeTruthy()
    expect((hit?.[1] as RequestInit | undefined)?.method).toBe('POST')
    // 「再去登录页」是这条用例名里的承诺：只通知了 Hub 而不跳转的话，
    // 人留在原地，接下来每一个请求都是 401。
    expect(assign).toHaveBeenCalledWith('/login')
    // 还原：下一条用例断言的是真实跳转，留着这个 spy 会让它恒真
    assign.mockRestore()
  })

  it('Hub 清 cookie 失败也要去登录页（否则人被困在一堆 401 弹框里）', async () => {
    stubFetch({ ...EMPTY_API, '/logout': { status: 500 } })
    const w = mountApp('/')
    await flush(w)

    logoutButton(w)?.click()
    await flush(w)

    expect(location.pathname).toBe('/login')
  })
})

/* ===========================================================================
   左栏几何与收起态：**只有原型那一套**
   ---------------------------------------------------------------------------
   用户裁定（2026-11-16）「以原型为准」后，自造的 `.af-app` 网格
   （`44px 208px 1fr` + `[data-rail='collapsed'] .af-brand { flex-direction:
   column }` 的 52px 图标轨）与自造的侧栏一族整体删除。
   收起态改走原型的机制：`html[data-rail='off'] .af-shell
   { grid-template-columns: 0 1fr }` + `html[data-rail='off'] .af-rail
   { transform: translateX(-100%) }`（原型 `app.css:1212-1227`）。

   这一组守三件事，**都不是「样式表里有几行」那种恒真断言**：
   ① 品牌行是 `aside.af-rail` 的**第一个子节点**（与导航同属一列，一起滑走）；
   ② 网格模板只有两列且宽度取 `--af-rail-w`（不写死像素）；
   ③ 收起态**整列滑出**，而不是缩成图标轨——并且唤回按钮仍可 Tab 到。

   反向验证：把品牌行挪出 `aside` → ①红；把 `grid-template-columns` 写成
   `44px 208px 1fr` → ②红；把 `html[data-rail='off'] .af-rail` 那条规则删掉
   或换回 `.af-app[data-rail]` → ③红。
   =========================================================================== */
describe('App：左栏几何与收起态', () => {
  it('品牌行是 aside.af-rail 的第一个子节点，与导航同属一列', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    const rail = w.element.querySelector<HTMLElement>('aside.af-rail')
    expect(rail, '左栏必须存在').toBeTruthy()
    const brand = rail?.querySelector<HTMLElement>(':scope > .af-rail__brand')
    expect(brand, '品牌行必须是 aside 的直接子节点').not.toBeNull()
    // 关键：它在 `aside` 里面。挪出去这条立刻红。
    expect(brand?.parentElement).toBe(rail)
  })

  it('网格模板只有两列，左栏宽取 --af-rail-w（不写死像素）', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    const body = cssRuleOf(css, '.af-shell')

    expect(body, '左栏宽必须取 --af-rail-w，不能写死像素').toContain(
      'grid-template-columns: var(--af-rail-w) 1fr',
    )
    expect(body).not.toContain('44px 208px')
    // 自造的那套网格已经没了：外壳的几何**只能**有一条定义。
    // 剥注释再查——两边的注释里都写着 `.af-app`（解释它被删掉的理由）。
    const components = stripCssComments(readRepoFile('packages/web/src/styles/components.css'))
    expect(components).not.toContain('.af-app')
  })

  it('收起态是 56px 图标条（用户 2026-11-17 裁定，有意偏离原型的「整列滑走」）', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')

    // 正面：收起后左栏占 56px 网格轨道，**不位移、不隐藏**。
    // 这三条一起锁住「图标条」这个形态：
    //   · 轨道是 56px（不是 0）——左栏还在
    //   · 没有 `translateX(-100%)`——没有滑出屏幕
    //   · 没有 `visibility: hidden`——图标可点、可聚焦
    expect(cssRuleOf(css, "html[data-rail='off'] .af-shell")).toContain(
      'grid-template-columns: 56px 1fr',
    )
    const railBlock = cssRuleOf(css, "html[data-rail='off'] .af-rail")
    expect(railBlock, '收起态左栏规则必须存在').not.toBe('')
    expect(railBlock, '收起态不许整列滑出屏幕').not.toContain('translateX(-100%)')
    expect(railBlock, '收起态不许把左栏藏起来（图标条必须可见可点）').not.toContain(
      'visibility: hidden',
    )

    // 反向：上一版「52px 图标轨」那套自造做法仍然不许回来（竖排名字当图标）。
    const decls = stripCssComments(css)
    expect(decls).not.toContain('writing-mode: vertical-rl')
    expect(decls).not.toContain('52px')
  })

  it('收起态：图标条常驻，7 个导航项各有可读的 aria-label 与一个 SVG 图标', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    // **判据是「图标条里的每一项都还找得到、读得懂、点得着」**（2026-11-17 改）。
    // 上一版断言的是「唤回胶囊 `.af-recallsplit` 在 `.af-rail` 子树之外」——
    // 那条锁的是「整列滑走 + 浮一颗唤回按钮」的旧方案，而用户改成图标条之后
    // 唤回按钮本身就不该存在了（顶部那颗开关常驻），旧断言会挡住新形态。
    //
    // 现在的三条反向守卫：
    //   ① 唤回胶囊必须消失（否则它和图标条重复）
    //   ② 剩下的按钮里不许再有 `position: fixed` 的悬浮唤回形态
    //   ③ `main.af-main` 不许再靠 `padding-left` 给悬浮按钮让位
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    expect(cssRuleOf(css, "html[data-rail='off'] .af-recallsplit")).toBe('')
    expect(w.element.querySelector('.af-recallsplit')).toBeNull()

    // 让路留白也必须消失：图标条自己占网格轨道，再叠 padding 就是凭空空一块。
    const components = readRepoFile('packages/web/src/styles/components.css')
    expect(cssRuleOf(components, "html[data-rail='off'] .af-main")).toBe('')

    // 7 个导航项：各有一个 SVG 图标，且**无障碍名不依赖图标**。
    // 收起态没有可见文字（`.af-rail__label` 被 `display: none`），所以
    // `aria-label` 是这一项唯一的名字——缺了它，收起态的左栏对读屏就是 7 个空链接。
    const navLinks = Array.from(
      w.element.querySelectorAll<HTMLAnchorElement>('.af-rail__list a[aria-label]'),
    )
    expect(navLinks.length, '导航项必须带 aria-label').toBe(7)
    for (const a of navLinks) {
      expect(a.getAttribute('aria-label')?.trim(), 'aria-label 不许为空').toBeTruthy()
      const svg = a.querySelector('svg')
      expect(svg, `「${a.getAttribute('aria-label')}」必须有一个 SVG 图标`).not.toBeNull()
      // 图标纯装饰：名字由链接的 aria-label 提供，SVG 不能再被读一遍。
      expect(svg?.getAttribute('aria-hidden')).toBe('true')
    }

    /*
      每一张图标都必须是**自己那一张**，不是一堆「总览」。

      上一版这里只断言「有一个 `<svg>`」——于是所有导航项都画成总览的图标
      （`NAV_ICON` 漏一项时的静默兜底就是这个形态）也全绿。判据取的是 SVG
      画出来的路径数据，与 `NAV_ICON_PATHS[item.icon]` 逐项相等。

      判红方式（实测）：把「设备」项的 `icon: 'devices'` 改成 `'overview'` →
      本用例红在「设备画成了总览那个图标」。
    */
    const rendered = navLinks.map((a) => ({
      label: a.getAttribute('aria-label') ?? '',
      // 每张图标的路径拼起来就是它的形状指纹（`NavIcon` 一个 path 一条 `d`）。
      d: Array.from(a.querySelectorAll('svg path')).map((p) => p.getAttribute('d') ?? ''),
    }))
    const { NAV_ICON_PATHS } = await import('@/shared/navIcons')
    const iconOf: Record<string, keyof typeof NAV_ICON_PATHS> = {
      总览: 'overview',
      已归档的项目: 'archived',
      收件箱: 'inbox',
      动态: 'activity',
      设备: 'devices',
      Agent: 'agents',
      设置: 'settings',
    }
    for (const { label, d } of rendered) {
      const key = iconOf[label]
      expect(key, `导航项「${label}」在用例里没有登记图标键`).toBeTruthy()
      expect(d, `导航项「${label}」画成了别的图标（期望 ${String(key)} 的形状）`).toEqual([
        ...NAV_ICON_PATHS[key as keyof typeof NAV_ICON_PATHS],
      ])
    }
    // 7 张图标两两不同：撞形是「形状不可辨」那条用户裁定的直接违反。
    expect(new Set(rendered.map((r) => r.d.join('|'))).size, '图标形状必须两两不同').toBe(
      rendered.length,
    )

    // 展开态那颗开关仍在品牌行里：图标条里它是「展开」的唯一入口（不能删）。
    const btn = w.element.querySelector<HTMLElement>('.af-rail__brand .af-navbtn')
    expect(btn, '品牌行里必须有那颗开关').not.toBeNull()
    expect(isTabbable(btn as Element)).toBe(true)
  })

  it('`.af-shell` 只有两个网格子元素（左栏 + 主区），主区不许被挤到第二行', async () => {
    stubFetch(EMPTY_API)
    const w = mountApp('/')
    await flush(w)

    // 用户 2026-11-17 报「左侧栏底部的和原型不一致」，查下去发现更严重的一条：
    // 删 `.af-recallsplit` 的 CSS 时漏删了 `App.vue` 的 `<button>`，它成了
    // `.af-shell` 的**第三个**网格子元素，抢走第 2 格，于是 `main.af-main`
    // 掉到第 2 行、宽度只剩左栏那么宽。实测（playwright）：
    //   · 残留按钮 {x:224, y:0, w:1216, h:458}
    //   · main.af-main {x:0, y:458, w:224, h:442}
    //   · gridTemplateRows = '458.25px 441.75px'
    // happy-dom 不做布局，量不出 rect，所以这里断**结构**：网格子元素正好两个，
    // 且第二个是 `main.af-main`。多一个子元素就会被挤到下一行，这是同一条缺陷。
    const shell = w.element.querySelector('.af-shell')
    expect(shell).not.toBeNull()
    const kids = Array.from(shell!.children)
    expect(
      kids.length,
      `.af-shell 必须恰好两个网格子元素，实测 ${kids.length} 个：${kids.map((k) => k.tagName.toLowerCase() + '.' + k.className).join(', ')}`,
    ).toBe(2)
    expect(kids[0].tagName.toLowerCase()).toBe('aside')
    expect(kids[1].tagName.toLowerCase()).toBe('main')
    expect(kids[1].className).toContain('af-main')
  })

  it('收起态不该有可拖动的横向滚动条（用户反馈 2026-10-28）', () => {
    const css = readRepoFile('packages/web/src/styles/prototype-shell.css')
    // **必须取顶层那条规则**：`cssRuleOf` 会先命中以 `.af-rail {` 结尾的子串
    // （`html[data-rail='off'] .af-rail {` 里就有），于是读到的是覆盖规则、
    // 永远不含 `overflow-x`。实测踩过（见旧注释）。
    const at = css.indexOf('\n.af-rail {')
    expect(at, '顶层 .af-rail 规则必须存在').toBeGreaterThan(-1)
    const block = css.slice(css.indexOf('{', at) + 1, css.indexOf('}', at))

    // 这条断言锁的是一件容易被当成「没事」的事：**只写 `overflow-y: auto`
    // 时 `overflow-x` 的计算值是 `auto`，不是 `visible`**。CSS Overflow
    // 规范规定一轴非 visible 会把另一轴的 visible 提升成 auto，于是左栏里
    // 任何横向溢出都长成一条可拖动的滚动条，而它拖出来是一片空白。
    //
    // **必须剥掉注释再断言**：直接 `toContain('overflow-x: hidden')` 时，
    // 把那条声明删掉用例照样绿——解释为什么需要它的散文里就有这串字。
    const decls = stripCssComments(block)
    expect(decls, 'overflow-x 必须显式收掉').toContain('overflow-x: hidden')

    // 网格项默认 `min-width: auto`：内容比轨道宽时它会把左栏顶宽。
    expect(decls, '网格项必须允许收窄').toContain('min-width: 0')
  })
})

describe('App：会话页自动收起侧栏（v0.5.0）', () => {
  const SESSIONS_API = {
    '/api/sessions': {
      body: [session({ id: 's_1', workspace: { node_id: 'n_1', path: '/srv/app', path_locked: true } })],
    },
    '/api/devices': { body: [device()] },
    '/api/agents': { body: [] },
  }

  /*
    读收起态**必须读全局 `document.documentElement`，不能读 `w.element`**：
    见 `@/test/app-shell` 的 `railOf()`（收起态落在
    `<html data-rail="off">`，两个取值归一成 `open` / `collapsed`）。
  */

  /** 点左栏品牌行那颗开关。**每次都重新查**，不复用上一次拿到的节点。 */
  async function clickTopbarRail(w: ReturnType<typeof mountApp>): Promise<void> {
    await clickRailToggle(w)
  }

  it('进会话页收起侧栏，离开后回到进入前的值', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/')
    await flush(w)
    expect(railOf(), '列表页不该收侧栏').toBe('open')

    goTo('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf(), '会话页必须收起').toBe('collapsed')

    goTo('/devices')
    await flush(w)
    expect(railOf(), '离开会话页要还回去').toBe('open')
  })

  /*
    会话里每次点开关都**立刻生效**，且这一次拨动就是用户意图的更新。

    ── 这条锁的是「翻转基准」不是「rail」 ───────────────────────────
    进会话时 `rail` 还是列表页的 `open`，可见态被自动收起成 `collapsed`。
    若 `toggleRail` 以 `rail` 为基准翻，点一次会把 `rail` 翻成 `collapsed`，
    可见态**仍是** `collapsed`——用户点「展开」得到的是「没动」。
    正确的做法是以**可见态**为基准翻，于是这里第一次点击后必须是 `open`。

    反向验证（实测）：把 `toggleRail` 改成
    `autoCollapsed=false; rail=rail==='open'?'collapsed':'open'` →
    第③步实测红（期望 open，实际 collapsed）。

    ── 为什么离开会话后是 `collapsed`（最后一次拨动）而不是 `open` ────
    侧栏开关表达的是「我要不要看侧栏」，这是**用户偏好**，不是页面态。
    在会话里拨它，就是改了偏好；离开会话页没有任何理由把它改回去。
    「自动收起」才是页面态，它才是**不该**越界的那个（会话外永不生效）。
  */
  it('会话里每次点开关都立刻生效，且拨动即用户意图', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/')
    await flush(w)

    // ① 列表页、用户意图 open（初值）
    expect(railOf(), '列表页开着').toBe('open')

    // ② 进会话：自动收起
    goTo('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf(), '会话页必须收起').toBe('collapsed')

    // ③ 点一次开关：必须立刻展开（不是要按两下）
    await clickTopbarRail(w)
    expect(railOf(), '会话里点一次开关就要展开').toBe('open')

    // ④ 再点一次：收起
    await clickTopbarRail(w)
    expect(railOf(), '再点一次收起').toBe('collapsed')

    // ⑤ 离开会话：保持用户最后一次的选择（他刚在会话里收起了侧栏）
    goTo('/devices')
    await flush(w)
    expect(railOf(), '拨动即用户意图，离开会话不撤销它').toBe('collapsed')

    // ⑥ 回列表页再点一次：翻回 open——说明它确实是普通偏好，不是会话残留
    await clickTopbarRail(w)
    expect(railOf(), '会话外仍可正常翻转').toBe('open')
  })

  /*
    用户本来就收起了侧栏：进会话仍是 collapsed，会话里展开后离开仍是 open。

    它与上一条**必须同时存在**，因为两条把「离开时写什么」两侧都钉住了：
    无条件置 open 会让这里第②步（会话页收起）变成 open 而红；无条件置
    collapsed 会让这里最后一步红。
  */
  it('用户本来就收起了侧栏：会话里的拨动同样立刻生效并保持', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/devices')
    await flush(w)

    await clickTopbarRail(w)
    expect(railOf(), '用户自己收起').toBe('collapsed')

    goTo('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf(), '会话页收起').toBe('collapsed')

    // 会话里展开：立刻生效
    await clickTopbarRail(w)
    expect(railOf(), '会话里展开立刻生效').toBe('open')

    // 离开会话页：用户最后的选择是 open，就保持 open。
    // （这条与上一条合起来钉住「离开时不写缺省值」：无条件置 open 会让
    //  上一条的第⑥步之前的 collapsed 断言红；无条件置 collapsed 会让这里红。）
    goTo('/devices')
    await flush(w)
    expect(railOf(), '保持用户最后一次的选择').toBe('open')
  })

  it('深链直接进会话页也收起（不是只有「从列表点进来」才收）', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/project/p_1/session/s_1')
    await flush(w)
    expect(railOf()).toBe('collapsed')
  })

  it('会话页里侧栏开关仍在左栏的 Tab 序列里（手动展开可达）', async () => {
    stubFetch(SESSIONS_API)
    const w = mountApp('/project/p_1/session/s_1')
    await flush(w)
    const btn = w.element.querySelector<HTMLElement>('.af-rail button[aria-label="折叠 / 展开侧栏"]')
    if (!btn) throw new Error('no rail toggle')
    // 自动收起是**值**的变化，不是把按钮藏掉：撤掉它用户就再也展开不了侧栏，
    // 而「找设备 / 换会话」在会话页里就不可达了。
    expect(isTabbable(btn), '折叠态也必须能 Tab 到展开按钮').toBe(true)
  })
})
