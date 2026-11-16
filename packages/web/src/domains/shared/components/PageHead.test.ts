/* ===========================================================================
   PageHead：页面头的结构契约 + 「≥2 页才建」的门槛
   ---------------------------------------------------------------------------
   两条分开测，各自对应一处会**静默**出错的地方：

   ① **结构**：组件必须渲染原型那一行页头——`header.af-head`
      （`prototype/index.html:15`），主区（默认插槽）是它的直接子节点，动作
      （`#actions`）排在 `.af-head__spacer` **之后**。少了 `af-head` 这一类名，
      页头就退回成一条裸 flex 行——`.af-head` 的弹性行、间距、贴顶、底部那条
      线全在它身上（`prototype/assets/app.css:118-124`）。
      少了 `af-head__spacer`，右侧动作会被挤在标题后面——原型靠它
      （`margin-left: auto`，`app.css:130`）把动作顶到右边，**不是**套一层
      动作 div（那个自造的 `.af-page__actions` 已随类名删除）。
   ② **调用点数量**：为一个页面抽组件是伪复用。这条断言读的是**仓库真源码**，
      不是某个 mock——把调用点删到 1 处时它必须红。

   为什么用读文件而不是 mount 出全部页面：这里要数的是「有哪几个文件在用」，
   mount 一个页面要它自己的路由 / 网络桩，量出来的东西反而绕。
   =========================================================================== */

import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'

import { describe, expect, it } from 'vitest'

import PageHead from '@/domains/shared/components/PageHead.vue'
import { mountIn } from '@/test/mount'

const SRC = join(process.cwd(), 'src')

/** 仓库根（`packages/web` 的上两级）：用来把绝对路径写成 `packages/web/src/…`。 */
const REPO = join(SRC, '..', '..', '..')

/**
 * 递归列出 `src/` 下全部 `.vue`，返回相对仓库根的路径（与文档里的写法一致）。
 *
 * 用相对**仓库根**的路径而不是绝对路径：`readRepoFile`（`test/css.ts`）与
 * `components.test.ts` 的受管名单都是这个口径，两处能直接对照。
 */
function vueFiles(dir: string): string[] {
  const out: string[] = []
  for (const name of readdirSync(dir)) {
    const full = join(dir, name)
    if (statSync(full).isDirectory()) out.push(...vueFiles(full))
    else if (name.endsWith('.vue')) out.push(relative(REPO, full))
  }
  return out
}

/** 用了某个组件的文件（按 `<组件名` 计，模板里的用法只有这一种形状）。 */
function callSites(component: string): string[] {
  const re = new RegExp(`<${component}[\\s>/]`)
  return vueFiles(SRC)
    .filter((f) => re.test(readFileSync(join(REPO, f), 'utf-8')))
    .sort()
}

describe('PageHead 的结构', () => {
  it('根是 .af-head，主区在外层、动作排在 .af-head__spacer 之后', () => {
    const w = mountIn(PageHead, {
      slots: { default: '<h1 class="af-head__title">标题</h1>', actions: '<button class="af-btn">刷新</button>' },
    })
    const root = w.element as HTMLElement
    expect(root.tagName, '页头必须是 header 元素（原型 index.html:15）').toBe('HEADER')
    expect(root.classList.contains('af-head')).toBe(true)
    // 原型没有自造的 `.af-page__head` / `__actions` 这两层。
    expect(root.classList.contains('af-page__head'), '自造的 .af-page__head 不许回来').toBe(false)
    // 主区必须是**直接子节点**：被包进动作那一侧就会被顶到右边、当按钮排
    expect(root.querySelector(':scope > .af-head__title')).not.toBeNull()
    const spacer = root.querySelector(':scope > .af-head__spacer')
    expect(spacer, '右侧动作要靠 .af-head__spacer（margin-left:auto）顶过去').not.toBeNull()
    const actions = root.querySelector(':scope > .af-btn')
    expect(actions, '动作必须是页头的直接子节点，且排在 spacer 之后').not.toBeNull()
    expect(spacer?.nextElementSibling, '动作必须紧跟在 spacer 后面').toBe(actions)
  })

  it('没有动作时仍然渲染出 .af-head__spacer（页头形状不随内容变）', () => {
    const w = mountIn(PageHead, { slots: { default: '<h1 class="af-head__title">标题</h1>' } })
    expect((w.element as HTMLElement).querySelector(':scope > .af-head__spacer')).not.toBeNull()
  })
})

describe('PageHead 的调用点 ≥ 2 处（一页用一次的抽象是伪复用）', () => {
  it('至少 2 个页面 / 组件在用', () => {
    const sites = callSites('PageHead')
    expect(sites.length, `PageHead 只有 ${sites.length} 个调用点：${sites.join(', ')}`).toBeGreaterThanOrEqual(2)
  })
})

describe('页头不再用「间隔柱」凑两端对齐', () => {
  /**
   * `PageHead` 的意义就是把两种将就写法清掉：用 `<span class="af-spacer">`
   * 或空 `<span class="af-page__actions"></span>` 当间隔柱。它们的可见结果
   * 和组件一样，所以只要没人钉住，下一次改页头时就会有人照着旧代码抄回来——
   * 那时页头又变成两种写法，而组件还在，重复反而更多。
   *
   * **只查页头那一段，不查整个文件**：`.af-spacer` 本身不是坏类名，它在
   * 分区头 / 面板头 / 侧栏底 / 登录框里都是正当用法（`components.css:219`
   * 就是 `flex: 1 1 auto`）。为单页抽一个组件时顺手把这个类从全文件禁掉，
   * 等于用一个假红灯换一次「清理」——所以判据收窄到 `<PageHead>` 到
   * `</PageHead>` 之间的那几行。
   */
  it('PageHead 块内不再出现 af-spacer 或空 af-page__actions 柱', () => {
    const sites = callSites('PageHead')
    expect(sites.length, '没有调用点时这条断言什么都量不到').toBeGreaterThan(0)
    for (const file of sites) {
      const src = readFileSync(join(REPO, file), 'utf-8')
      const at = src.indexOf('<PageHead')
      const end = src.indexOf('</PageHead>', at)
      expect(at, `${file} 里找不到 <PageHead> 块`).toBeGreaterThanOrEqual(0)
      expect(end, `${file} 的 <PageHead> 没有闭合`).toBeGreaterThan(at)
      const block = src.slice(at, end)
      expect(block, `${file} 的页头仍用 .af-spacer 当间隔柱`).not.toContain('class="af-spacer"')
      expect(block, `${file} 的页头仍有空 .af-page__actions 柱`).not.toContain('class="af-page__actions"')
    }
  })

  /**
   * 页头里的类名必须是**原型有的**（`prototype/assets/app.css:118-130`）：
   * `.af-head__title` / `.af-head__sub`，右侧动作由组件内的
   * `.af-head__spacer` 顶住。自造的 `.af-page__*` 那一族规则已删（v0.8.1
   * 子任务 2），留在页头块里就等于这几页的标题退回浏览器默认尺寸——
   * 没有编译错误、没有别的断言会红。
   *
   * 子任务 4 的迁移不变量就落在这条上：**4 个调用点的页头块里
   * `af-page__` 一次都不许出现**。页面根容器上的 `af-page` 不在此列
   * （它不在这段里），归子任务 6/8/9。
   */
  it('调用点的页头块里只有原型的 .af-head__* 类，没有自造的 af-page__*', () => {
    const sites = callSites('PageHead')
    expect(sites.length, '没有调用点时这条断言什么都量不到').toBeGreaterThan(0)
    for (const file of sites) {
      const src = readFileSync(join(REPO, file), 'utf-8')
      const at = src.indexOf('<PageHead')
      const end = src.indexOf('</PageHead>', at)
      const block = src.slice(at, end)
      const legacy = [...block.matchAll(/class="([^"]*)"/g)]
        .flatMap((m) => m[1].split(/\s+/))
        .filter((name) => name.startsWith('af-page__'))
      expect(
        legacy,
        [`${file} 的页头块里还有自造的类名（${legacy.join(', ')}）。`, '页头请用原型的 .af-head__title / .af-head__sub（prototype/assets/app.css:125-130）。'].join('\n'),
      ).toEqual([])
    }
  })
})
