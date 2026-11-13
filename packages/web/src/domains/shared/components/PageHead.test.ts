/* ===========================================================================
   PageHead：页面头的结构契约 + 「≥2 页才建」的门槛
   ---------------------------------------------------------------------------
   两条分开测，各自对应一处会**静默**出错的地方：

   ① **结构**：组件必须渲染 `.af-page__head`，主区（默认插槽）与动作
      （`#actions`）分别落在 `af-page__head` 的直接子节点与 `.af-page__actions`
      里。少了 `af-page__head` 这一类名，页头就退回成一条裸 flex 行——
      而 `.af-page__head` 的两端对齐、间距、`margin-bottom` 全在它身上。
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
  it('根是 .af-page__head，主区在外层、动作在 .af-page__actions 里', () => {
    const w = mountIn(PageHead, {
      slots: { default: '<h1 class="af-page__title">标题</h1>', actions: '<button class="af-btn">刷新</button>' },
    })
    const root = w.element as HTMLElement
    expect(root.classList.contains('af-page__head')).toBe(true)
    // 主区必须是**直接子节点**：塞进 .af-page__actions 就会被当成动作挤到右边
    expect(root.querySelector(':scope > .af-page__title')).not.toBeNull()
    const actions = root.querySelector(':scope > .af-page__actions')
    expect(actions, '动作容器必须存在且是直接子节点').not.toBeNull()
    expect(actions?.querySelector('.af-btn')).not.toBeNull()
  })

  it('没有动作时仍然渲染出 .af-page__actions（页头形状不随内容变）', () => {
    const w = mountIn(PageHead, { slots: { default: '<h1 class="af-page__title">标题</h1>' } })
    expect((w.element as HTMLElement).querySelector(':scope > .af-page__actions')).not.toBeNull()
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
})
