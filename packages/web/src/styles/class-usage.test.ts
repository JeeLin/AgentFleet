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

import { classNamesIn, stripCssComments } from '@/test/css-scan'

const SRC = join(process.cwd(), 'src')
const REPO = join(SRC, '..', '..', '..')

/**
 * 仓库里**样式可能被定义**的地方。`styles/` 与页面级共享类都算。
 *
 * ⚠️ 新增样式表时必须加进这里，否则那个文件里定义的类会被判成「不存在」——
 * 症状是**假阳性**（门禁红了但代码是对的），而人的自然反应是去改模板而不是
 * 修这份名单。`tokens.css` 不在此列：它只定义变量，不定义类。
 */
const STYLE_SHEETS = [
  'packages/web/src/styles/components.css',
  'packages/web/src/styles/prototype-shell.css',
  'packages/web/src/page-chrome.css',
]

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

/* 剥注释的实现在 `@/test/css-scan`。理由不变：CSS 注释里也写着类名
   （解释「为什么」），不剥会把定义数算错。 */

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
    ['af-card', '卡片钩子：外观来自同一元素上的 `af-taskcard` 等具体类'],

    // ── ① 续：根标记类（外观全部由后代选择器或同一元素上的具体类承担）─────────
    // 说明：下面这一族的豁免在 v0.8.1 子任务 10 曾整块涉及，但「页面外壳那一族」
    // （`.af-page` / `__head` / `__title` / `__actions` / `__sub` / `__subtitle` /
    // `--flush`）的豁免已**整块删除**：那一族在原型
    // `prototype/assets/app.css` 里命中 0，是实现自造的第二套页面设计；随着
    // 12 个页面 + 骨架页组件全部迁到 `.af-head` + `.af-body`，模板侧实测已
    // **0 处**使用（2026-11-17，按 `class="..."` 静态属性统计）。留着豁免就是
    // 对门禁的静默关闭——所以连同上一条「只在还没迁移时成立」的守卫一起删了，
    // 另有一条**反向**守卫：这一族不许再用、也不许再定义。
    ['af-catalog', '`AgentCatalog` 的根标记类：后代选择器 `.af-catalog .af-catalog__x` 里已包含它'],
    ['af-agd', '`AgentDetailPage` 的根标记类：同上，`.af-agd .af-agd__x`'],
    ['af-devd', '`DeviceDetailPage` 的根标记类：同上'],
    ['af-taskd', '`ProjectTaskPage` 的根标记类：同上'],
    ['af-acceptancepanel', '`AcceptancePanel` 的根标记类：外观全部走 `.af-region` + `.af-acceptancepanel__x`'],
    ['af-failurepanel', '`FailurePanel` 的根标记类：同上'],
    ['af-devpage__detail', '`DevicesPage` 的设备详情分区钩子：与 `af-region` 一起用，外观全部来自 `.af-region`'],
    ['af-auth--setup', '`SetupPage` 的 `af-auth` 变体钩子：当前与 `af-auth` 同观感，保留是为了将来真要分叉时有地方加'],
    ['af-auth__note--info', '`af-auth__note` 的语义变体：说明性提示与错误提示目前同观感，靠 `role="alert"` 区分对读屏的语义'],
    ['af-compose__why', '输入框下方的「为什么发不出去」钩子：文案本身承担全部信息，无需额外样式'],
    ['af-files__line', '文件预览的行钩子：行高与字体由 `.af-files__code` 容器给'],
    ['af-traj__gaps', '轨迹里的「中间有省略」标记：与 `data-traj-gaps` 属性配合，样式由 `.af-traj__gap` 承担'],
    ['af-wb__baserev', '会话工作台起点标记：与 `data-base-rev` 属性配合，外观由 `.af-wb__meta` 承担'],
    ['af-wb__baseov', '同上，起点「超出上限」那一档'],
    // ② 原型类名，外观由本地类承担
    //
    // `.af-fold` / `.af-fold__sum` / `.af-fold__body` 曾挂在这里（生产侧当时用
    // 自造的 `.af-ov__fold`）。v0.8.1 子任务 6 已把总览页换成原型的
    // `details.af-fold` 结构，这三条在 `prototype-shell.css:449-469` 有真规则，
    // 豁免随之删除——留着就是给已定义的类再开一个静默通道。
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
    // 正向：`.af-rail__mark` / `.af-head__title` 都必须真被找到，
    // 否则 isDefined 恒返回 false，上面那条断言会因为「全都缺」而看起来也在工作。
    // 正例**必须取自原型**（`prototype/assets/app.css`）：用自造类当正例，等于
    // 让一条「要删掉自造层」的门禁反过来把自造类当成既有事实（`.af-auth__mark`
    // 就曾这样混进来——原型里 `grep -c 'af-auth' → 0`）。
    expect(isDefined('af-rail__mark')).toBe(true)
    expect(isDefined('af-head__title')).toBe(true)
    // 反向：编一个不存在的名字，必须判缺——否则上面那条断言恒真。
    expect(isDefined('af-definitely-not-a-real-class')).toBe(false)
    // 上面守卫的落点：`.af-page__title` 的规则**真的**已经删了（不是被判成缺）。
    expect(isDefined('af-page__title')).toBe(false)
  })

  /*
    自造的页面外壳那一族**不许回来**。

    这一族（`.af-page` / `__head` / `__title` / `__actions` / `__sub` /
    `__subtitle` / `--flush` / `__section`）在原型 `prototype/assets/app.css`
    里命中 0，是实现自造的第二套页面设计，用户 2026-11-16 裁定「以原型为准，
    现在的前端代码我很不满意」——实现侧自造的类名层一律删除，不迁移、不保留
    别名。

    v0.8.1 全部 12 个页面 + 骨架页组件迁完之后，模板侧实测 0 处使用，于是原先
    的「整族豁免」(`MIGRATION_EXEMPT`) 与它的「只在还没迁移时成立」守卫一起删了。
    豁免没了，这条门禁就得靠下面这条反向用例守住**回潮**：要么有人重新在模板里
    写这些类，要么有人把规则加回样式表，两条都判红。

    `af-page__subtitle` 也在名单里：`BoardPage` 曾用它标页内分区标题，现在用原型
    的 `.af-region__title`。
  */
  it('自造的页面外壳类不许回到模板或样式表里（用户裁定「以原型为准」）', () => {
    const BANNED = [
      'af-page',
      'af-page--flush',
      'af-page__head',
      'af-page__title',
      'af-page__actions',
      'af-page__sub',
      'af-page__subtitle',
      'af-page__section',
    ]
    // ① 模板里不许再写（静态 class 属性）。
    const inTemplates: string[] = []
    for (const [file, src] of VUE_SRC) {
      const used = staticClasses(src)
      for (const name of BANNED) if (used.includes(name)) inTemplates.push(`${file}: .${name}`)
    }
    expect(
      inTemplates,
      `这些模板又用上了自造的页面外壳类（应改用 .af-head / .af-body / .af-region）：\n${inTemplates.join('\n')}`,
    ).toEqual([])

    // ② 样式表里不许再定义。
    const redefined = BANNED.filter((name) => isDefined(name))
    expect(
      redefined,
      `样式表里重新定义了自造外壳类：${redefined.join(', ')}——自造层不许回来`,
    ).toEqual([])

    // 判定本身有效：这条守卫必须能区分「名单里的」与「真实在用的」。
    // 拿一个确实有规则的类当正例，否则 `staticClasses` / `isDefined` 若双双
    // 失效（一个恒返回空、一个恒返回 false），上面两条断言会在空集合上恒真。
    expect(staticClasses(VUE_SRC.get('packages/web/src/App.vue') ?? '')).toContain('af-shell')
    expect(isDefined('af-shell')).toBe(true)
  })

  /*
    **v0.8.1 第5轮删掉的「整族零消费」类不许回来**（G53/G54）。

    上面那条门禁只拦「模板用了、样式表没定义」的方向；这一条拦**反方向**：
    「样式表定义了、模板没人用」。第5轮按 `grep` 复算删掉了 18 个这样的族
    （`/tmp/a_work/dead.txt`，复算脚本 `/tmp/a_work/dead.mjs`），规则删了、
    原地留的是墓碑注释。要真用上某一族，正确动作是**连着模板消费者一起写**
    ——那会让这条守卫自然变红，逼着改动者把豁免与理由补上，而不是让规则
    悄悄挂在样式表里冒充已实现。

    名单按族写（不含变体后缀前缀的根名）。**判据是「样式表里出现 `.name`
    选择器」**：模板侧出现不算（那正是「要回来了」的正确姿势的一半，
    另一半必须等规则一起回来、让这一条红了以后由人裁决）。
  */
  it('第5轮删掉的整族零消费类不许只以规则形式回来（要有就要连消费者一起要）', () => {
    const DELETED_FAMILIES = [
      'af-backoff',
      'af-bar',
      'af-check',
      'af-diff',
      'af-frozen',
      'af-meter',
      'af-meters',
      'af-num-c',
      'af-palette',
      'af-sep',
      'af-sim',
      'af-split',
      'af-sr',
      'af-strong',
      'af-tabs',
      'af-toast',
      'af-toasts',
      'af-tip',
    ]
    /* 反向自检：名单里必须**一个**都还没有定义——否则这条守卫上线即红，
       说明有族被漏删（那就该去删规则，而不是把名字从名单里拿掉）。 */
    const defined = DELETED_FAMILIES.filter((name) => isDefined(name))
    expect(
      defined,
      `这些族在第5轮已按「整族零消费即删」删除（原地留有墓碑注释），现在样式表里又出现了：\n` +
        `${defined.join(', ')}\n` +
        `要恢复请连着模板消费者一起加，并把名字从这份名单里移出、写明理由`,
    ).toEqual([])
    // 判定有效：拿一个真实存在的类当正例，防止 isDefined 恒 false 时恒真。
    expect(isDefined('af-shell')).toBe(true)
  })
})

/* ===========================================================================
   反向断言（v0.8.1 第5轮 4c F7 / G82）：**定义了但零消费**的类。
   ---------------------------------------------------------------------------
   原有 4 条 `it` 全是「模板用了但样式表里没有」这个方向——那是「缺定义」。
   反方向（孤儿规则）此前**没有任何守卫**：4c 实测新增 `.af-head__note-probe`
   等从未被任何 `.vue` 使用的类，111 条样式守卫全绿。CSS 是按需匹配的，
   孤儿规则不报错、不影响渲染，只会越积越多——第5轮 4a 一口气清出
   71 个零消费类就是证据。

   判据刻意保守（与正向一致：宁可漏判不误报）：
     · 只收「类名作为**主体**出现在选择器里」的定义（`classNamesIn(css, { wholeSubject: true })`
       的口径——`.af-card:focus-visible` 算定义了 `af-card`，`.af-empty--compact .af-empty__text`
       只算定义了后者的**祖先链**，不算单独定义）；
     · 消费侧扫「静态 class 属性」+「`:class` 对象/字符串字面量里的 `af-*`」，
       与动态拼接同样保守处理；
     · 豁免表**每条写理由**。

   4c 的处置警告：`.af-projectlist__dot` 与 `.af-drawer--open` **不可删**
   ——前者属性互补（`[data-color]` 变体族，4a S5 实证两块规则不是覆盖关系），
   后者是动态绑定。白名单必须显式含它们，否则这条守卫会把要保留的类
   误判成孤儿。
   =========================================================================== */
describe('样式表里定义了 af-* 类但模板零消费（孤儿规则，反向）', () => {
  /* ── 消费侧的三种形态 ─────────────────────────────────────────────
     1. 静态 `class="af-a af-b"`：直接切开取词。
     2. `:class` 对象字面量的 key（`{ 'af-drawer--open': x }`）：静态可见。
     3. **模板字符串拼接**（`` `af-badge--${row.tone}` ``）：只有前缀可见，
        后缀来自数据。第四种「只被 .ts 测试夹具 / querySelector 消费」的，
        扫描看不见，走 ORPHAN_OK 显式豁免。

     第 3 种是这轮我自己踩的坑：初版只扫 1 和 2，于是 `af-badge--accent`
     被报成孤儿——而它的消费者写的是 `` `af-badge--${row.tone}` ``，
     `CAPABILITIES` 里 tone 只有 ok/danger/info，accent 确实无人用。
     **结论：既不能只按前缀放过（那会让整族 8 个变体都免检，其中 7 个是死的），
     也不能只认字面量（会误报）。** 做法是「前缀放过 + 后缀必须落在
     该文件里真实出现的字面量集合里」，见下面 dynamicSuffixes。 */
  function consumedClasses(): Set<string> {
    const out = new Set<string>()
    for (const src of VUE_SRC.values()) {
      // ① 静态 class="..."
      for (const m of src.matchAll(/class="([^"]*)"/g)) {
        if (m[1].includes('${') || m[1].includes('{{')) continue
        for (const name of m[1].split(/\s+/)) if (name.startsWith('af-')) out.add(name)
      }
      // ② :class 对象字面量的 key
      for (const m of src.matchAll(/'(\s*af-[a-zA-Z0-9_-]+\s*)'/g)) out.add(m[1].trim())
    }
    return out
  }

  /** `` `af-prefix--${expr}` `` 里前缀 → 该 `expr` 取值域里出现过的字面量的**类名后缀集合**。
   *
   *  取值域取得保守：整个文件里所有字符串字面量（`tone: 'ok' | 'danger'` 的
   *  类型注解、`tone: 'danger'` 的数据都在里面）。宁可放过一个死变体，
   *  也不误报一个活变体——与正向守卫同一口径。 */
  function dynamicSuffixes(): Map<string, Set<string>> {
    const out = new Map<string, Set<string>>()
    for (const src of VUE_SRC.values()) {
      const lits = new Set<string>()
      for (const m of src.matchAll(/'([a-zA-Z0-9_-]+)'/g)) lits.add(m[1])
      for (const m of src.matchAll(/`(af-[a-zA-Z0-9_-]*?[\w-]*)-\$\{/g)) {
        const prefix = m[1]
        if (!out.has(prefix)) out.set(prefix, new Set())
        for (const lit of lits) out.get(prefix)!.add(lit)
      }
    }
    return out
  }

  /** 定义侧：以类为主体出现在选择器里的 af-* 名（wholeSubject 口径）。
   *  **只扫三份完整样式表**，不扫 `.vue` 的 `<style>` 块——后者是逐文件拼接的
   *  片段，可能有 `@media` 之类的上下文残缺，`parseCssRules` 会抛
   *  「花括号不平衡」。正向守卫判「有定义」时用整份 ALL_CSS（宽松、
   *  宁可漏判），反向判「孤儿」时必须结构化解析（严格、可疑即报），
   *  两者口径不同是刻意的。 */
  function definedClasses(): Set<string> {
    const out = new Set<string>()
    for (const css of STYLE_SHEETS) {
      const text = stripCssComments(readFileSync(join(REPO, css), 'utf-8'))
      for (const cls of classNamesIn(text, { wholeSubject: true })) {
        if (cls.startsWith('.af-')) out.add(cls.slice(1))
      }
    }
    return out
  }

  /** 一个类名是否被消费：字面量命中，或落在某个 `` `prefix-${expr}` `` 的取值域里。 */
  function isConsumed(name: string, consumed: Set<string>, dyn: Map<string, Set<string>>): boolean {
    if (consumed.has(name)) return true
    for (const [prefix, suffixes] of dyn) {
      if (!name.startsWith(`${prefix}-`)) continue
      if (suffixes.has(name.slice(prefix.length + 1))) return true
    }
    return false
  }

  /* 扫描看不见的消费者，逐条写理由。**不许写「暂未使用」**——那等于给孤儿开后门，
     第5轮 4a 清出的 71 个零消费类全是这么攒起来的。 */
  const ORPHAN_OK = new Map<string, string>([
    [
      'af-projectlist__dot',
      '`[data-color]` 变体族（blue/green/purple/orange/gray 五条 + 尺寸一条，'
        + '属性互补不是覆盖，4a S5 实证）。**页面暂未渲染它**，但它是「形态 + 语义色」'
        + '这套配色的唯一实现；删了等要用时得把这组令牌映射整套重写。'
        + '4c 处置明确点名：不可删。',
    ],
    [
      'af-head--page',
      '`src/test/sfc.test.ts` 的夹具用它造一个「页头」形态，经 sfc 编译后断言样式生效——'
        + '消费发生在测试夹具里，`.vue` 静态扫描看不见。',
    ],
    [
      'af-body--pad',
      '`BoardPage.test.ts` 断言「看板页的根**不该**带这一类」（内容区自己带内边距会把页头挤成内缩），'
        + '同时三处注释按名引用它解释为什么改用 `.af-head` + `.af-body`。'
        + '删了规则，那些注释与那条断言就失去指涉对象。',
    ],
    /* ── 状态色带：整族保留（用户 v0.8.1 第5轮裁定）─────────────────────
       下面 6 条与**有**消费的 `af-tag--review` / `af-tag--failed`（OverviewPage.vue:426/430）
       是同一族：原型 `app.css:215-231` 为看板六列各定义了一组
       color / background / border-color，共 8 个变体。
       它们不是「零消费的死代码」，而是**一张完整的状态 → 配色映射表**：
       删掉其中任一格，将来要看板显示该状态的标签，就得把三组语义令牌
       （`--p-<state>` / `-bg` / `-line`）重新推一遍，而推错不会报错，只会「颜色不对」。
       与 4c 点名**不可删**的 `af-projectlist__dot`（`[data-color]` 变体族）同一性质。

       为什么不写成「暂未使用」：那种理由等于给孤儿开后门——4a 清出的 71 个零消费类
       全是这么攒起来的。这里的理由是「该族已在服役，此格是它的既定成员」，
       可以被证伪：若哪天 `review`/`failed` 也不再有消费者，整族就该一起删，
       这条豁免随之失效（见下面「同族必须仍有消费者」那条自检）。 */
    ...['pending', 'queued', 'running', 'done', 'butler', 'warn'].map(
      (st): [string, string] => [
        `af-tag--${st}`,
        '看板状态色带的一员（同族 review/failed 在 OverviewPage 服役），'
          + '原型 app.css:215-231 定义的完整映射表，删一格就要重推三组语义令牌',
      ],
    ),
  ])

  it('样式表里定义的 af-* 类都有消费者（模板 / :class 字面量 / 动态前缀 / 显式豁免）', () => {
    const consumed = consumedClasses()
    const dyn = dynamicSuffixes()
    const orphans = [...definedClasses()].filter((c) => !isConsumed(c, consumed, dyn) && !ORPHAN_OK.has(c))
    expect(
      orphans,
      `这些 af-* 类定义了但全仓 .vue 零消费：\n${orphans.join('\n')}\n`
        + '删掉规则（死代码），或把消费者补上，或在 ORPHAN_OK 里写明理由。',
    ).toEqual([])
  })

  it('判定本身有效：动态前缀、字面量、真孤儿三者判对方向', () => {
    /* 自检（与正向的「判定本身有效」同构）。三条缺一不可：
       · `af-drawer--open` 走 :class 字面量——证明形态 ② 没坏；
       · 定义侧能扫到 `af-projectlist__dot`——证明解析器没空转；
       · 动态前缀判定**区分同前缀下的活变体与死变体**——这是本轮最容易写错的一处：
         `af-badge--ok` 必须判「被消费」（`tone: 'ok'` 在文件里），
         而 `af-badge--accent` 必须判「没被消费」（取值域里没有 accent）。
         若只按前缀放过，accent 会被放走；若只认字面量，ok 会被误报。 */
    const consumed = consumedClasses()
    const dyn = dynamicSuffixes()
    expect(consumed.has('af-drawer--open'), 'af-drawer--open 经 :class 字面量被消费——扫描必须看见它').toBe(true)
    expect(definedClasses().has('af-projectlist__dot'), '定义侧必须扫到 af-projectlist__dot').toBe(true)
    expect(isConsumed('af-badge--ok', consumed, dyn), 'af-badge--ok 落在 `af-badge--${tone}` 的取值域里').toBe(true)
    expect(
      isConsumed('af-badge--accent', consumed, dyn),
      'af-badge--accent 不在取值域里（tone 只有 ok/danger/info）——必须判为未消费，否则整族都免检',
    ).toBe(false)
  })

  it('状态色带的豁免前提仍在：同族 review / failed 必须还有真实消费者', () => {
    /* ORPHAN_OK 里 6 条 `af-tag--*` 的理由是「同族在服役，此格是既定成员」。
       这个理由**可以被证伪**——一旦 review/failed 也不再被任何模板用到，
       整族就退化成纯死代码，那 6 条豁免就失去依据，应该一起删。
       这条用例把那个前提钉住：没有它，豁免理由是没法被检验的一句话。 */
    const consumed = consumedClasses()
    const dyn = dynamicSuffixes()
    for (const live of ['af-tag--review', 'af-tag--failed']) {
      expect(
        isConsumed(live, consumed, dyn),
        `状态色带的前提破了：${live} 已无消费者，ORPHAN_OK 里那 6 条 af-tag--* 的豁免理由不再成立，应当整族删除`,
      ).toBe(true)
    }
  })
})
