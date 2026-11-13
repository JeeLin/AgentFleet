/* ===========================================================================
   模板里用到的 `af-*` 类名，必须在样式表里**有定义**
   ---------------------------------------------------------------------------
   守的是一类不会报错的缺陷：`.af-head__title` 在 `BoardPage.vue` 里被用了两次
   （看板页头的项目名、新建任务对话框的标题），而 `components.css` 里一条规则
   都没有——于是那两个 `h1` / `h2` 吃的是**浏览器默认尺寸**（34px / 24px、
   `font-weight: 700`）。没有编译错误、没有测试变红、没有任何控制台警告，
   只是「项目名比页面标题大一圈」。它被发现了纯属偶然：收口 token 时顺手
   做了「使用侧减定义侧」的差集。

   判据（刻意保守，宁可漏判也不要误报）：
     - 只看 `class="…"` 里以 `af-` 开头的类名；
     - 定义处 = `components.css` + `page-chrome.css` + 该文件自己的 `<style>`
       再加全仓任何 `.vue` 的 `<style>`（scoped 样式会在编译期加 data 属性，
       但选择器里的类名不变，跨文件定义也确实是「有定义」）；
     - 动态拼接的类名（`:class="{ … }"`、`class="af-x--${…}"`）**不判**：
       它们展开后是什么只有运行时才知道，硬判会造出一批假红灯。

   为什么独立成文件而不是塞进 `components.test.ts`：那个文件量的是「一条规则
   只有一个家」（数次数），这里量的是「用到的类有没有家」（存在性）——
   两个问题方向相反，混在一起时改一个断言会连带影响另一个的阅读。
   =========================================================================== */

import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'

import { describe, expect, it } from 'vitest'

const SRC = join(process.cwd(), 'src')
const REPO = join(SRC, '..', '..', '..')

/** 仓库里**样式可能被定义**的地方。`styles/` 与页面级共享类都算。 */
const STYLE_SHEETS = ['packages/web/src/styles/components.css', 'packages/web/src/page-chrome.css']

function vueFiles(dir: string): string[] {
  const out: string[] = []
  for (const name of readdirSync(dir)) {
    const full = join(dir, name)
    if (statSync(full).isDirectory()) out.push(...vueFiles(full))
    else if (name.endsWith('.vue')) out.push(relative(REPO, full))
  }
  return out
}

/** 取 `<style>` 块里的 CSS（没有就返回空串）。 */
function styleBlock(vue: string): string {
  return [...vue.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]).join('\n')
}

/** 剥注释：CSS 注释里也写着类名（解释「为什么」），不剥会把定义数算错。 */
function stripCssComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, '')
}

const VUE = vueFiles(SRC)
const VUE_SRC = new Map(VUE.map((f) => [f, readFileSync(join(REPO, f), 'utf-8')]))

/** 全仓所有样式文本（全局表 + 每个 `.vue` 的 `<style>`），已剥注释。 */
const ALL_CSS = [
  ...STYLE_SHEETS.map((f) => readFileSync(join(REPO, f), 'utf-8')),
  ...VUE.map((f) => styleBlock(VUE_SRC.get(f) as string)),
]
  .map(stripCssComments)
  .join('\n')

/** 某个类名在样式表里是否有**选择器**（`.name` 后面跟非标识符字符）。 */
function isDefined(name: string): boolean {
  return new RegExp(`\\.${name}(?![a-zA-Z0-9_-])`).test(ALL_CSS)
}

/** 一个文件模板里静态写出的 `af-*` 类名。 */
function staticClasses(vue: string): string[] {
  const out = new Set<string>()
  for (const m of vue.matchAll(/class="([^"]*)"/g)) {
    // `class="af-x--${…}"` 这类插值跳过整条属性：展开后是什么只有运行时知道
    if (m[1].includes('${') || m[1].includes('{{')) continue
    for (const name of m[1].split(/\s+/)) if (name.startsWith('af-')) out.add(name)
  }
  return [...out]
}

describe('模板里的 af-* 类名都有定义（`.af-head__title` 那次漏定义的门禁）', () => {
  /**
   * 豁免名单：**每个条目都必须写明为什么「没有规则」是对的**。
   *
   * 一律不豁免「查不到所以大概没问题」的类——那等于把门禁关掉。这里只收
   * 两种情况：
   *   ① **纯钩子**：类名只用来标记「这是一张卡 / 一个分区」，外观由同一元素
   *      上的另一个类或父级后代选择器承担；
   *   ② **原型类名，外观已由等价的本地类承担**：照 `prototype/` 抄结构时把
   *      原型类名一起写进了模板，但生产样式表用的是另一个名字（`.af-ov__fold`
   *      而不是 `.af-fold`）。这类名字留着是**结构对照的锚点**，删掉会让
   *      「哪一块对应原型哪一块」在代码里消失。
   *
   * 2026-11 首次跑这条门禁时命中 20 条，逐条核过之后只补了两条真规则
   * （`.af-head__title` / `.af-num`），其余 18 条落在上面两类里。
   */
  const NO_RULE_BY_DESIGN = new Map<string, string>([
    // ① 纯钩子
    ['af-card', '卡片钩子：外观来自同一元素上的 `af-taskcard` / `af-catalog__card` 等具体类'],
    ['af-page__section', '分区钩子：没有自己的规则，分区标题的间距写在 `.af-page__subtitle` 上（§2 的注释就是这么定的）'],
    ['af-catalog', '`AgentCatalog` 的根标记类：后代选择器 `.af-catalog .af-catalog__x` 里已包含它'],
    ['af-agd', '`AgentDetailPage` 的根标记类：同上，`.af-agd .af-agd__x`'],
    ['af-devd', '`DeviceDetailPage` 的根标记类：同上'],
    ['af-taskd', '`ProjectTaskPage` 的根标记类：同上'],
    ['af-acceptancepanel', '`AcceptancePanel` 的根标记类：外观全部走 `.af-region` + `.af-acceptancepanel__x`'],
    ['af-failurepanel', '`FailurePanel` 的根标记类：同上'],
    ['af-skeletonpage', '`SkeletonPage` 的根标记类：外观走 `.af-skeletonpage__x` 与 `.af-page`'],
    ['af-ov__todo', '`OverviewPage` 的待办分区钩子：与 `af-page__section` 一起用，无自有规则'],
    ['af-devpage__detail', '`DevicesPage` 的设备详情分区钩子：与 `af-region` 一起用，外观全部来自 `.af-region`'],
    ['af-auth--setup', '`SetupPage` 的 `af-auth` 变体钩子：当前与 `af-auth` 同观感，保留是为了将来真要分叉时有地方加'],
    ['af-auth__note--info', '`af-auth__note` 的语义变体：说明性提示与错误提示目前同观感，靠 `role="alert"` 区分对读屏的语义'],
    ['af-compose__why', '输入框下方的「为什么发不出去」钩子：文案本身承担全部信息，无需额外样式'],
    ['af-files__line', '文件预览的行钩子：行高与字体由 `.af-files__code` 容器给'],
    ['af-traj__gaps', '轨迹里的「中间有省略」标记：与 `data-traj-gaps` 属性配合，样式由 `.af-traj__gap` 承担'],
    ['af-wb__baserev', '会话工作台起点标记：与 `data-base-rev` 属性配合，外观由 `.af-wb__meta` 承担'],
    ['af-wb__baseov', '同上，起点「超出上限」那一档'],
    // ② 原型类名，外观由本地类承担
    ['af-fold', '原型 `.af-fold`：生产用 `.af-ov__fold`（`<details>`），模板里留着这个名字是对照锚点'],
    ['af-fold__sum', '原型 `.af-fold__sum`：生产用 `.af-ov__fold > summary`'],
    ['af-fold__body', '原型 `.af-fold__body`：生产用 `.af-ov__fold-body`（选择器里两个名字并列，故意保留）'],
  ])

  it('没有任何页面在用「样式表里不存在的类」', () => {
    const missing: string[] = []
    for (const [file, src] of VUE_SRC) {
      for (const name of staticClasses(src)) {
        if (NO_RULE_BY_DESIGN.has(name)) continue
        if (!isDefined(name)) missing.push(`${file}: .${name}`)
      }
    }
    expect(
      missing,
      `这些类在模板里用了，但样式表里没有定义（元素会退回浏览器默认样式）：\n${missing.join('\n')}`,
    ).toEqual([])
  })

  it('判定本身有效：已知有定义的类判「有」，编造的类判「没有」', () => {
    // 正向：`.af-page__title` / `.af-head__title` 都必须真被找到，
    // 否则 isDefined 恒返回 false，上面那条断言会因为「全都缺」而看起来也在工作。
    expect(isDefined('af-page__title')).toBe(true)
    expect(isDefined('af-head__title')).toBe(true)
    // 反向：编一个不存在的名字，必须判缺——否则上面那条断言恒真。
    expect(isDefined('af-definitely-not-a-real-class')).toBe(false)
  })
})
