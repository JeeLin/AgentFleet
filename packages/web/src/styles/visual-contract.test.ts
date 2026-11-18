import { describe, expect, it } from 'vitest'
import { readRepoFile } from '@/test/css'
import { parseCssRules, propValue, ruleFor } from '@/test/css-scan'

import type { CssRule } from '@/test/css-scan'


/* ===========================================================================
   visual-contract.test.ts —— 视觉属性的**回归网**（第5轮）
   ---------------------------------------------------------------------------
   **为什么还要第三个样式测试文件**：`ui-scale.test.ts` 的阶梯守卫只写了
   *不等式*（`toBeGreaterThan` / `>= 2`），它守的是「三档字号有序」。实测过：
   把 `--af-text-card` 从 `17px` 改成 `18px`（**仍在合法阶梯内**），20/20 全绿。
   第2轮做的 18 个变异里只有 7 个判红（39%）。也就是说「改成另一个合法值」
   这一类改动，在旧网上**一条都不会红**。

   本文件把「视觉属性 = 这个具体值」写成一张**显式的三元组表**：
   `选择器 → 属性 → 期望值`，期望值是**在真实浏览器里量出来的字面量**，
   不是由被测实现再算一次（不写 `var()`、不读 token 再和自己的解析结果比）。
   每一行都做过变异验证（改坏那个值 → 这条红 → 还原）。

   ---------------------------------------------------------------------------
   **实测采集条件**（每一行都在表里注明来源；三个页面同一次会话）
   ---------------------------------------------------------------------------
   环境：Vite dev server `http://127.0.0.1:3002`（Chromium via playwright 1.52，
   `getComputedStyle`）。后端 `127.0.0.1:8080`，登录方式为密码登录。
   主题：`warm`（看板页 HTML 上 `data-theme="warm"` 是当时生效的那套；
   四套主题下的焦点环颜色另由「焦点环」那一节逐套断言）。
   视口：**1440 × 900**（设备像素比 1，无缩放）。

     ├─ 看板页  `/project/KGYpUHNFXzFkR8allYhVIQ/board`
     │    实测 2026-11-18，页头标题文案「UI 重设计验收」，
     │    6 列、0 张卡片（空看板）。
     ├─ 设置页  `/settings`
     │    页头 70.75px 高（标题 + 副标题两行，`min-height: 56px` 自己长高）。
     ├─ 设备页  `/devices`
     │    页头 56px（单行，`--af-head-h` 正好卡住）。
     └─ 看板页（有卡片时）
          为了量 `.af-taskcard:focus-visible`，在演示看板上用「新建任务」
          对话框真实创建了一张任务卡（标题「视觉契约探针任务」，
          判定码留空、填了描述），**然后键盘 Tab 到那张真卡片**才读到 outline。
          这一条不是近似：`el.matches(':focus-visible')` 实测 `true`。

   ---------------------------------------------------------------------------
   **拿不到精确值的地方，如实标注**（详见各处 `NOTE`）：
   ---------------------------------------------------------------------------
   1. `.af-head__sub` 的 `color` 是 `var(--p-fg-3)` 算出来的 rgb——另一个 lane
      正在改 `--p-fg-3`。所以本文件**不断言它的值**，只断言它**引用了**这条
      变量（「存在」层级），颜色达标由 `theme-css.test.ts` 负责。
   2. `.af-taskcard:focus-visible` 的 `outline-color` 走的是 `var(--af-border-focus)`
      →`var(--p-acc)`，四套主题各一个值。这里只钉「宽度 / 线型 / offset」这些
      **主题无关**的字面量，颜色由下一节按主题逐个断言 `--af-border-focus`
      有值且引用的就是 `--p-acc`。
      ⚠️ **这条规则已经不在 `components.css` 了**（v0.8.1 第5轮，G40）：模板两处
      都写 `class="af-card af-taskcard"`，而 `prototype-shell.css` 后导入、同特异性，
      于是 `components.css` 那份 `.af-taskcard:focus-visible` **从来没生效过**——
      真正画环的是 `.af-card:focus-visible`。所以下面的契约行与断言都改指
      **shell** 里的 `.af-card:focus-visible`，并另加一条反向断言：`components.css`
      里**不许**再出现 `.af-taskcard:focus-visible`（留着只会让人以为环在那儿）。
      （此处原本还有一条 NOTE 说的是 `.af-tip:focus-visible`：那整族在同一轮里按
      「整族零消费即删」删除，连着守着它的用例一起删，所以那条 NOTE 不复存在。）
   =========================================================================== */

const SHELL = 'packages/web/src/styles/prototype-shell.css'
const COMPONENTS = 'packages/web/src/styles/components.css'
const TOKENS = 'packages/web/src/styles/tokens.css'

/** 一份样式表 → 规则列表（只解析一次，全表共用）。 */
const RULES: Record<string, CssRule[]> = {
  [SHELL]: parseCssRules(readRepoFile(SHELL)),
  [COMPONENTS]: parseCssRules(readRepoFile(COMPONENTS)),
  [TOKENS]: parseCssRules(readRepoFile(TOKENS)),
}

/**
 * 一行契约。
 *
 * - `file`     —— 规则所在的那份样式表（相对仓库根）。
 * - `selector` —— 规则头**原文**（`ruleFor` 要求文本完全相等）。
 * - `prop`     —— 属性名。
 * - `expect`   —— **浏览器实测得到的字面量**。写的是 CSS 源里那个值，不是
 *                 computed value：px 值两者相等（本表刻意只收 px / 无单位 /
 *                 关键字这类源值与 computed 值同形的属性），`rgb()` 与 `ch`
 *                 这类「源值 ≠ computed 值」的属性另设 `computed` 字段说明实测值。
 * - `measured` —— 这次实测在哪一页、什么条件下量到的；写进断言消息，
 *                 红了以后不用回去翻报告就知道该去哪页复现。
 * - `computed` —— 可选：实测的 computed value（仅作记录，不参与断言；
 *                 参与断言就必须是字面量，见文件头「禁止 var() 解析」）。
 */
interface ContractRow {
  file: string
  selector: string
  prop: string
  expect: string
  measured: string
  computed?: string
}

/* ===========================================================================
   1. 契约表 —— 每一行都来自一次真实浏览器实测
   ---------------------------------------------------------------------------
   分组说明写了「为什么这个值重要」，不是复述值本身。
   =========================================================================== */
const CONTRACT: ContractRow[] = [
  /* ---- 页头标题 `.af-head__title` ------------------------------------ */
  {
    /* 字号这一条**刻意写成两层**：这里钉「规则引用的是 display 档」，
       字面量 22px 由下面的 `TOKEN_LITERALS` 钉。为什么不在这一行直接写
       `expect: '22px'`：CSS 源里就是 `var(--af-text-display)`，写 22px 会让
       这条断言常红——「源值」与「computed 值」在这一个属性上不同形。
       两层合起来等价于「页面上这个标题最终是 22px」：引用被改 → 这行红；
       token 被改 → `--af-text-display is 22px` 那行红。 */
    file: SHELL,
    selector: '.af-head__title',
    prop: 'font-size',
    expect: 'var(--af-text-display)',
    measured:
      '看板页 /project/KGYpUHNFXzFkR8allYhVIQ/board · 1440×900 · 标题「UI 重设计验收」→ computed 22px；' +
      '设置页 /settings、设备页 /devices、/inbox、/inbox/activity、/projects/new、/sessions/new 同为 22px',
    computed: '22px',
  },
  {
    file: SHELL,
    selector: '.af-head__title',
    prop: 'font-weight',
    expect: 'var(--af-weight-semibold)',
    measured: '看板页 + 设置页 + 设备页 · 1440×900 · 实测 computed 600',
    computed: '600',
  },
  {
    file: SHELL,
    selector: '.af-head__title',
    prop: 'line-height',
    expect: '1',
    measured:
      '看板页 · 1440×900 · 实测 computed 22px（= 22 × 1）。**不是**继承 1.65：' +
      '22 × 1.65 = 36.3px 会把 56px 的页头撑破（设置页实测页头 70.75px 是标题+副标题两行的正常长高）',
    computed: '22px',
  },
  {
    file: SHELL,
    selector: '.af-head__title',
    prop: 'letter-spacing',
    expect: '-0.01em',
    measured: '看板页 · 1440×900 · 实测 computed -0.22px（= -0.01 × 22px）',
    computed: '-0.22px',
  },
  {
    file: SHELL,
    selector: '.af-head__title',
    prop: 'white-space',
    expect: 'nowrap',
    measured: '看板页 · 1440×900 · 实测 nowrap（页头是扫视用的单行短语）',
  },

  /* ---- 页头副标题 `.af-head__sub` ------------------------------------ */
  {
    file: SHELL,
    selector: '.af-head__sub',
    prop: 'font-size',
    expect: 'var(--p-t-xs)',
    measured:
      '看板页（`<h2 class="af-head__sub">UI 重设计验收</h2>`）· 1440×900 → computed 12.5px；' +
      '设置页 /settings、设备页 /devices 同为 12.5px',
    computed: '12.5px',
  },
  {
    file: SHELL,
    selector: '.af-head__sub',
    prop: 'white-space',
    expect: 'nowrap',
    measured: '看板页 · 1440×900 · 实测 nowrap',
  },
  /* NOTE（如实标注）：`color` 是 `var(--p-fg-3)`。另一个 lane 正在改这个 token，
     所以这里**不断言具体色值**，只断言这一行**仍然引用 `--p-fg-3`**（见 §2 的
     `expect: 'var(--p-fg-3)'` 行）。它守的是「别把辅助文字色改成 --p-fg-2 或写死
     一个色值」——那是语义变化；具体色值由 `theme-css.test.ts` 的达标断言管。 */
  {
    file: SHELL,
    selector: '.af-head__sub',
    prop: 'color',
    expect: 'var(--p-fg-3)',
    measured:
      '看板页 · 1440×900 · 实测 computed rgb(140, 136, 125)（warm 主题的 --p-fg-3）。' +
      '**具体色值不断言**：另一个 lane 正在改 --p-fg-3，只钉「引用了它」这一条语义',
  },

  /* ---- 页头里的「标题 + 副标题」包裹 div（G3：本轮新增的两条规则）----
     ---------------------------------------------------------------------
     这两条规则支撑 `PageHead.vue` 文档写明的**合法写法**：3 个页面的副标题
     是一整句 20-40 字说明，排在标题右边会把标题挤到换行，所以那几页在默认槽
     里放一个包住「标题 + 副标题」的 `<div>`，副标题落到标题下面。此时
     `PageHead` 的槽位结构与类名不变，**排版由这两条选择器决定**。

     **G3 说的就是它们没有守卫**：删掉整套 `.af-head > div` 规则，页面不会报错，
     只是三页的页头各自塌成一种高度——正是用户报过的那个 bug 的形状。
     实测两页确认规则在工作：`/devices/probe-device` 与 `/agents/probe-agent`
     页头都是 **70.75px**（标题 22 + 副标题 18.75 × 2 行 + gap 4 = 67.75，含边框），
     而单行页 `/project/…/bind` 是 **56px**。 */
  {
    file: SHELL,
    selector: '.af-head > div',
    prop: 'display',
    expect: 'flex',
    measured:
      '/devices/probe-device 与 /agents/probe-agent · 1440×900 · 包裹 div 实测 computed flex；' +
      '两页页头高都是 70.75px（副标题换到标题下面后的自然高度）',
  },
  {
    file: SHELL,
    selector: '.af-head > div',
    prop: 'flex-direction',
    expect: 'column',
    measured:
      '/devices/probe-device · 1440×900 · 实测 computed column——**这一条就是「副标题落到' +
      '标题下面」**，改成 row 就退回原型那种「副标题在标题右边」的排版，长说明会把标题挤到换行',
  },
  {
    file: SHELL,
    selector: '.af-head > div',
    prop: 'justify-content',
    expect: 'center',
    measured:
      '/devices/probe-device · 1440×900 · 实测 computed center。配合 `.af-head` 的 56px ' +
      'min-height：单行时包裹列在 56px 里垂直居中，两行时长高到 70.75px',
  },
  {
    file: SHELL,
    selector: '.af-head > div',
    prop: 'gap',
    expect: 'var(--p-s1)',
    measured: '/devices/probe-device · 1440×900 · 实测 computed gap 4px（= --p-s1）',
    computed: '4px',
  },
  {
    /* `min-width: 0` 不是装饰：flex 项默认 `min-width: auto`，标题 + 副标题
       都是 `nowrap`，没有这一条时包裹列会被内容顶宽、把页头撑出横向溢出。 */
    file: SHELL,
    selector: '.af-head > div',
    prop: 'min-width',
    expect: '0',
    measured:
      '/devices/probe-device · 1440×900 · 实测 computed 0px。去掉它，nowrap 的副标题会把' +
      '包裹列顶宽、页头横向溢出（flex 项默认 min-width: auto）',
    computed: '0px',
  },
  {
    file: SHELL,
    selector: '.af-head > div .af-head__sub',
    prop: 'white-space',
    expect: 'nowrap',
    measured:
      '/devices/probe-device 与 /agents/probe-agent · 1440×900 · 实测 computed nowrap。' +
      '页头是扫视用的，长说明该由 `.af-body` 承载',
  },
  {
    file: SHELL,
    selector: '.af-head > div .af-head__sub',
    prop: 'overflow',
    expect: 'hidden',
    measured:
      '/devices/probe-device · 1440×900 · 实测 computed hidden——配合 `text-overflow: ellipsis` ' +
      '才出省略号；少了它长文案直接溢出去而不是截断',
  },
  {
    file: SHELL,
    selector: '.af-head > div .af-head__sub',
    prop: 'text-overflow',
    expect: 'ellipsis',
    measured: '/devices/probe-device · 1440×900 · 实测 computed ellipsis',
  },
  {
    file: SHELL,
    selector: '.af-head > div .af-head__sub',
    prop: 'line-height',
    expect: 'var(--af-leading-snug)',
    measured:
      '/devices/probe-device · 1440×900 · 实测 computed 18.75px（= 12.5 × 1.5）。' +
      '这个数是页头高度算术的一半：两行副标题 37.5px + 标题 22 + gap 4 = 63.5',
    computed: '18.75px',
  },
  {
    file: SHELL,
    selector: '.af-head > div .af-head__sub',
    prop: 'min-width',
    expect: '0',
    measured: '/devices/probe-device · 1440×900 · 实测 computed 0px（同上：允许收窄才出省略号）',
    computed: '0px',
  },

  /* ---- 左栏品牌方块 `.af-rail__mark` -------------------------------- */
  {
    file: SHELL,
    selector: '.af-rail__mark',
    prop: 'width',
    expect: '26px',
    measured: '看板页 · 1440×900 · 实测 computed 26px（原型 app.css:67-73 的 26px 方块）',
    computed: '26px',
  },
  {
    file: SHELL,
    selector: '.af-rail__mark',
    prop: 'height',
    expect: '26px',
    measured: '看板页 · 1440×900 · 实测 computed 26px',
    computed: '26px',
  },

  /* ---- 左栏项目名 `.af-rail__name` ---------------------------------- */
  {
    file: SHELL,
    selector: '.af-rail__name',
    prop: 'font-weight',
    expect: 'var(--af-weight-semibold)',
    measured: '看板页 · 1440×900 · 实测 computed 600（改回 regular/400 在旧网里全绿）',
    computed: '600',
  },
  {
    file: SHELL,
    selector: '.af-rail__name',
    prop: 'font-size',
    expect: 'var(--p-t-sm)',
    measured: '看板页 · 1440×900 · 实测 computed 13.5px',
    computed: '13.5px',
  },

  /* ---- 左栏页脚 `.af-rail__foot` ------------------------------------ */
  {
    file: SHELL,
    selector: '.af-rail__foot',
    prop: 'line-height',
    expect: '1.7',
    measured:
      '看板页 · 1440×900 · 实测 computed 19.55px（= 11.5px × 1.7）。' +
      '**刻意不是 `var(--p-lh-body)`（1.65）**：1.65 时是 18.975px，差 0.575px ' +
      '就是用户报的「底部和原型不一致」之一',
    computed: '19.55px',
  },
  {
    file: SHELL,
    selector: '.af-rail__foot',
    prop: 'font-size',
    expect: 'var(--p-t-2xs)',
    measured: '看板页 · 1440×900 · 实测 computed 11.5px',
    computed: '11.5px',
  },

  /* ---- 任务卡的键盘焦点（无障碍） ----------------------------------- */
  {
    /* **这条环的真身在 shell，不在 components**（G40）：模板写的是
       `class="af-card af-taskcard"`，`prototype-shell.css` 后导入、同特异性，
       所以 `components.css` 那份 `.af-taskcard:focus-visible` 从来没生效过。
       下面 §3 有一条反向断言：`components.css` 里不许再写它。 */
    file: SHELL,
    selector: '.af-card:focus-visible',
    prop: 'outline-offset',
    expect: '-2px',
    measured:
      '看板页（真实创建一张任务卡后，从「刷新」按 2 次 Tab 到 `button.af-card.af-taskcard`）· 1440×900 · ' +
      '`el.matches(":focus-visible")` 为 true，实测 computed outline = "rgb(194,164,105) solid 2px"、' +
      'outline-offset = -2px。**内缩**而不是外扩：卡片坐在列体里，外扩的环会被列边界裁掉',
    computed: '-2px',
  },

  /* ---- 正文测度 ------------------------------------------------------ */
  {
    /* **断的是规则，不是 token**（G25）：这一行原来指向 tokens.css 的
       `--af-measure: 68ch`，可 68ch 是「测度的值」，不是「有人在用它」——
       `.af-empty--compact .af-empty__text { max-width: var(--af-measure) }`
       这条**消费者**此前完全没有网：把 `max-width` 换成 `44ch` 或整条删掉，
       页面上的空态横幅就会拖成一条长线，而所有断言仍然绿。
       所以这一行改钉那条规则引用的是哪个 token，字面量 68ch 仍由下面的
       `TOKEN_LITERALS` 钉住（两层合起来才是「横幅最终 510.5px」）。 */
    file: COMPONENTS,
    selector: '.af-empty--compact .af-empty__text',
    prop: 'max-width',
    expect: 'var(--af-measure)',
    measured:
      '看板页「整板空」横幅的 `.af-empty__text`（`<EmptyState compact>` 渲染的就是这两个类）· ' +
      '1440×900 · 实测 computed max-width 510.548px、该元素 1ch = 7.5081px → 510.548 / 7.5081 ≈ **68.0ch**；' +
      '设备页同一条 510.548px。⚠️ 这是**反推**出来的 ch 数（浏览器把 ch 算成了 px），不是直接读到的 "68ch"',
  },

  /* ---- 新增的间距 / 行高常量 ----------------------------------------- */
  {
    file: COMPONENTS,
    selector: '.af-panel__body > p + p',
    prop: 'margin-top',
    expect: 'var(--p-s3)',
    measured:
      '看板页 + 设备页 · 1440×900 · 段落间距。**没有直接量到**：演示库里 `.af-panel__body` 里没有两个相邻 `<p>`，' +
      '所以这条的期望值取自 CSS 源与 `--p-s3` 之外的原型口径（6px = `--af-space-3`）；' +
      '它在变异测试里靠「规则被删/改值」判红，不是靠 computed value',
  },
]

/* ===========================================================================
   0. 表的自检 —— 先保证这张表本身不是空转
   ---------------------------------------------------------------------------
   这几条先跑：表写错了（空表、expect 为空、expect 里塞了 `var()` 却声称是
   字面量），下面逐行的断言就是**假绿**。所以先证明表是「实心」的。
   =========================================================================== */
describe('契约表自检', () => {
  it('表里恰好 27 行（删一行都算在悄悄删契约）', () => {
    /* 初版是 `toBeGreaterThanOrEqual(15)`，而实测表 27 条——删掉 12 行仍然绿
       （v0.8.1 第5轮 4c F5 / G80 的变异实证）。下限只防「删空」，不防「删半」。
       **钉住确切条数**：删一行即红。哪天真要删契约行，必须同步改这个数——
       那是一次显式动作，不是顺手删。 */
    expect(CONTRACT.length).toBe(27)
  })

  it('关键契约组不得缺席（按组计下限，删行即使总数对得上也会被点名）', () => {
    /* 初版白名单写的是 `.af-head` / `.af-btn` / `.af-input`——**那是我编的**，
       表里根本不存在这些 selector（实测只有 10 种）。凭印象写白名单等于
       把「守卫的守卫」建在自己的想象上。改成按表里**真实存在**的组计下限：
       删行会同时破坏总数（27）与本表的分布，两道都会响。 */
    const minPerSelector: Record<string, number> = {
      '.af-head__title': 4,
      '.af-head > div': 4,
      '.af-head > div .af-head__sub': 4,
      '.af-head__sub': 2,
      '.af-rail__name': 1,
      '.af-rail__mark': 1,
      '.af-rail__foot': 1,
      '.af-empty--compact .af-empty__text': 1,
      '.af-panel__body > p + p': 1,
      '.af-card:focus-visible': 1,
    }
    const missing: string[] = []
    for (const [sel, min] of Object.entries(minPerSelector)) {
      const n = CONTRACT.filter((r) => r.selector === sel).length
      if (n < min) missing.push(`${sel}: ${n} < ${min}`)
    }
    const unknown = [...new Set(CONTRACT.map((r) => r.selector))].filter((s) => !(s in minPerSelector))
    expect(
      unknown,
      `契约表出现了本守卫不认识的 selector：${unknown.join(', ')}——`
        + '把它加进 minPerSelector（新契约行要有下限），或说明它为什么不必守',
    ).toEqual([])
    expect(missing, `关键契约组缺行：${missing.join('; ')}`).toEqual([])
  })

  it('每行的 expect 都非空、且每行都写清了在哪一页量到', () => {
    const bad = CONTRACT.filter((r) => !r.expect.trim() || !r.measured.trim() || r.measured.length < 10).map(
      (r) => `${r.selector} / ${r.prop}`,
    )
    expect(bad, `契约行缺期望值或实测出处：${bad.join(', ')}`).toEqual([])
  })

  it('同一个 (文件, 选择器, 属性) 不重复出现（两份期望值会互相掩护）', () => {
    const keys = CONTRACT.map((r) => `${r.file}|${r.selector}|${r.prop}`)
    const dup = keys.filter((k, i) => keys.indexOf(k) !== i)
    expect(dup, `重复的契约行：${dup.join(', ')}`).toEqual([])
  })

  it('每行声明的 `file` 都能被解析出那条规则（选择器拼错会在这里先红）', () => {
    /* 只看 `:root` 之外的类选择器：`:root` 在 tokens.css 里，末尾可能带逗号组，
       由下面的专门断言处理。 */
    const missing = CONTRACT.filter((r) => r.selector !== ':root').filter((r) => !ruleFor(RULES[r.file], r.selector))
    expect(missing.map((r) => `${r.file} :: ${r.selector}`), '契约表指向了不存在的规则').toEqual([])
  })
})

/* ===========================================================================
   1. 表驱动断言 —— 每一行都必须**精确等于**期望值
   ---------------------------------------------------------------------------
   只断言「等于」：`toBe` 而不是 `toContain` / `toBeGreaterThan`。
   「改成另一个合法值」正是这一层要接住的东西。
   =========================================================================== */
describe('视觉契约：选择器 → 属性 → 字面量', () => {
  for (const row of CONTRACT) {
    it(`${row.selector} { ${row.prop}: ${row.expect} } — 实测于 ${row.measured.slice(0, 30)}…`, () => {
      const rules = RULES[row.file]
      const rule = ruleFor(rules, row.selector)
      expect(rule, `${row.file} 里找不到选择器 ${row.selector}（规则被删或改名）`).toBeDefined()

      const actual = propValue(rule!, row.prop)
      expect(
        actual,
        `${row.selector} 的 ${row.prop} —— 期望 ${row.expect}\n` +
          `这条期望值是在「${row.measured}」实测得到的。\n` +
          `改了它就等于改了那次实测确认过的视觉效果；请回去在那一页重新量一遍再改这里。`,
      ).toBe(row.expect)
    })
  }
})

/* ===========================================================================
   2. 字号 / 字重 / 行高 token 的**字面量**（表里 expect 是 var() 的那几行）
   ---------------------------------------------------------------------------
   上面每一行断的是「规则引用了哪个 token」，这里断「那个 token 本身是多少」。
   两层合起来才是「页面上那个字最终是 22px / 600 / 1.0」。

   **为什么不在一行里同时断言**：token 的值一变，页面上的 computed value 就变；
   但规则里写的仍然是 `var(--af-text-display)`。两层分开写，红的时候一眼能看出
   是「引用改错了」还是「token 被改值了」。

   NOTE（`--p-fg-3` 不在本表）：另一个 lane 正在改它，见 §1 的 NOTE。
   =========================================================================== */
const TOKEN_LITERALS: Array<{ name: string; expect: string; why: string }> = [
  {
    name: '--af-text-display',
    expect: '22px',
    why: '页标题实测 22px（看板页 .af-head__title computed font-size）',
  },
  {
    name: '--af-text-card',
    expect: '17px',
    why:
      '卡片/区块标题档**实测 17px**。旧测试（ui-scale）只钉「比 body 大 ≥2px」，' +
      '所以把它改成 18px 是绿的；这里钉死 17px',
  },
  {
    name: '--af-text-md',
    expect: '14.5px',
    why: '正文档实测 14.5px（看板页 .af-card 的 font 继承基准）',
  },
  {
    name: '--af-weight-regular',
    expect: '400',
    why: '四档字重是阶梯的一部分；正则档实测 400',
  },
  {
    name: '--af-weight-medium',
    expect: '500',
    why: '中等档实测 500（左栏当前项 .af-rail__list a[aria-current]）',
  },
  {
    name: '--af-weight-semibold',
    expect: '600',
    why: '半粗档实测 600（.af-head__title / .af-rail__name 的 computed font-weight）',
  },
  {
    name: '--af-weight-bold',
    expect: '700',
    why: '粗体档实测 700（.af-rail__mark 里的品牌字）',
  },
  {
    name: '--af-leading-tight',
    expect: '1.45',
    why: '紧凑行高（表格/列表/终端）',
  },
  {
    name: '--af-leading-snug',
    expect: '1.5',
    why: 'UI 文案行高：页头副标题 12.5px × 1.5 = 18.75px（页头高度算术的一部分）',
  },
  {
    name: '--af-leading-base',
    expect: '1.65',
    why: '正文行高（说明性散文）',
  },
  {
    name: '--af-measure',
    expect: '68ch',
    why: '可读测度：实测反推 68.0ch（见 CONTRACT 表里 --af-measure 那一行的手法）',
  },
]

describe('视觉契约：token 字面量（原子值，不解析 var()）', () => {
  /* tokens.css 顶层有**两段** `:root`：§1–13 的取值定义段，和 §14 的「原型槽位
     别名」段（`--p-*: var(--af-*)` 那一批）。两段都在顶层、都不在 at-rule 里，
     所以「同一个 token 定义两次」只可能发生在**段内或段间**——`keynav.test.ts`
     有一条全量去重断言守着。这里按「所有顶层 :root 的声明体」解析，取最后一条
     （CSS 里后者胜），与浏览器的解析口径一致。 */
  const rootRules = RULES[TOKENS].filter((r) => r.selector === ':root' && r.atRules.length === 0)

  it('tokens.css 的顶层 `:root` 段数是 2（定义段 + 原型别名段），且没有第三段', () => {
    expect(rootRules.length, '顶层 :root 段数变了：新加一段会让同名 token 静默覆盖').toBe(2)
  })

  for (const token of TOKEN_LITERALS) {
    it(`${token.name} 是 ${token.expect}`, () => {
      /* 横跨所有顶层 :root 段，取**最后**一次声明（CSS 后者胜）。 */
      const declared = rootRules.map((r) => propValue(r, token.name)).filter((v): v is string => v !== undefined)
      const actual = declared.length ? declared[declared.length - 1] : undefined
      expect(
        actual,
        `${token.name} —— 期望 ${token.expect}。${token.why}\n` +
          '期望值是浏览器实测后写死的字面量，不是从实现里再算一遍。',
      ).toBe(token.expect)
    })
  }
})

/* ===========================================================================
   3. 焦点环 —— 全站唯一一条（无障碍承诺）
   ---------------------------------------------------------------------------
   `tokens.css` 的 `:where(a, button, input, select, textarea, [tabindex]):focus-visible`
   是**全站唯一**一条焦点环。上一版把焦点环写在 12 个页面各自 CSS 里，
   于是有的页面有、有的没有；这一版只有这一条。**它被置空 = 所有键盘用户失去
   焦点指示**，而旧网上没有任何用例读过它。

   实测（四套主题逐个量，真实 Tab 到元素后读 computed）：
     neutral  outline = 2px solid rgb(138,176,234) / offset 2px
     warm     outline = 2px solid rgb(194,164,105) / offset 2px
     paper    outline = 2px solid rgb(31,95,158)   / offset 2px
     cream    outline = 2px solid rgb(125,90,12)   / offset 2px
   四套主题的 `--af-focus-ring` 都是 `2px solid <--af-border-focus>`，
   `--af-focus-offset` 都是 `2px`；`--af-border-focus` 都是 `var(--p-acc)`。
   =========================================================================== */
describe('焦点环（全站唯一一条，tokens.css）', () => {
  const FOCUS_SELECTOR = ':where(a, button, input, select, textarea, [tabindex]):focus-visible'

  it('那条 :where(...):focus-visible 规则存在，且只定义了一次', () => {
    const all = RULES[TOKENS]
    /* `parseCssRules` 会把逗号组、后代选择器原样留在 selector 里，所以这里按
       「选择器文本里含 focus-visible 且不是 :focus:not(:focus-visible)」筛。 */
    const rings = all.filter((r) => r.selector.includes(':focus-visible') && !r.selector.includes(':not('))
    expect(rings.map((r) => r.selector), '全站焦点环必须有且只有一条').toEqual([FOCUS_SELECTOR])
  })

  it('焦点环用的是 `--af-focus-ring`，且 offset 是 `--af-focus-offset`（不是写死的 none/0）', () => {
    const rule = ruleFor(RULES[TOKENS], FOCUS_SELECTOR)
    expect(rule, '焦点环规则不见了').toBeDefined()
    const outline = propValue(rule!, 'outline')
    const offset = propValue(rule!, 'outline-offset')
    expect(outline).toBe('var(--af-focus-ring)')
    expect(offset).toBe('var(--af-focus-offset)')
    /* 直接挡住「置空」这一步：置空的人要么把值改成 none，要么把整条删掉
       （上一条接住），要么把变量改成 none/0（下一条接住）。 */
    for (const dead of ['none', '0', '0px', 'unset', 'initial', '']) {
      expect(outline?.trim(), `焦点环被置空（outline: ${dead || '<空>'}）—— 所有键盘用户失去焦点指示`)
        .not.toBe(dead)
    }
  })

  it('`--af-focus-ring` 是「2px 实线 + 主题焦点色」，`--af-focus-offset` 是 2px', () => {
    const root = RULES[TOKENS].find((r) => r.selector === ':root' && r.atRules.length === 0)!
    expect(propValue(root, '--af-focus-ring'), '焦点环本体（粗细/线型/颜色）').toBe(
      '2px solid var(--af-border-focus)',
    )
    expect(propValue(root, '--af-focus-offset'), '焦点环与控件之间那 2px 呼吸').toBe('2px')
  })

  it('四套主题都给了 `--af-border-focus`，且都指向 `--p-acc`（没有哪套主题的焦点环是空的）', () => {
    /* 实测：四套主题的 --af-border-focus 都是 var(--p-acc)，computed 分别
       #8ab0ea / #c2a469 / #1f5f9e / #7d5a0c。这里断「存在且都指向 --p-acc」，
       色值本身由 theme-css.test.ts 的原型对照管。 */
    const missing: string[] = []
    for (const theme of ['neutral', 'warm', 'paper', 'cream']) {
      const css = readRepoFile(`packages/web/src/styles/theme-${theme}.css`)
      const scope = css.slice(css.indexOf('html[data-theme='))
      const rule = parseCssRules(scope).find((r) => r.atRules.length === 0)
      expect(rule, `${theme} 主题文件里没有 html[data-theme=...] { ... } 块`).toBeDefined()
      const value = propValue(rule!, '--af-border-focus')
      if (!value || !value.trim()) missing.push(`${theme}: 没定义 --af-border-focus`)
      else if (value.trim() !== 'var(--p-acc)') missing.push(`${theme}: --af-border-focus = ${value}（不是 var(--p-acc)）`)
    }
    expect(missing, `焦点环颜色在主题里断了：${missing.join('; ')}`).toEqual([])
  })

  it('`.af-card:focus-visible` 有环（卡片是 `<button>`，键盘必须看得见）', () => {
    /* **断在 shell 上**（G40）：真正生效的是 `.af-card:focus-visible`——模板写
       `class="af-card af-taskcard"`，本文件后导入、同特异性，`components.css`
       那份被整条盖掉。 */
    const rule = ruleFor(RULES[SHELL], '.af-card:focus-visible')
    expect(rule, '卡片（= `.af-card`）的键盘焦点环规则不见了').toBeDefined()
    const outline = propValue(rule!, 'outline')
    const offset = propValue(rule!, 'outline-offset')
    expect(outline, '卡片焦点环（实测 2px solid，颜色随主题）').toBe('2px solid var(--af-border-focus)')
    expect(offset, '卡片焦点环内缩 2px（实测 -2px，外扩会被列边界裁掉）').toBe('-2px')
  })

  it('`components.css` 里不再有 `.af-taskcard:focus-visible`（同特异性、后导入，那一份从来没生效）', () => {
    /* 反向守卫：G40 删掉的就是这条**从没生效过**的规则。它若回来了，人会以为
       「任务卡的焦点环在 components.css」——于是改那里、页面毫无变化，而这个
       文件里的 `.af-card:focus-visible` 断言仍然绿。两个真相源必须只剩一个。 */
    const resurrected = RULES[COMPONENTS].filter((r) =>
      r.selector.split(',').some((s) => s.trim() === '.af-taskcard:focus-visible'),
    )
    expect(
      resurrected.map((r) => r.selector),
      '`.af-taskcard:focus-visible` 又回到 components.css 了：它会被 prototype-shell.css 的 ' +
        '`.af-card:focus-visible` 整条盖掉，改它不会影响页面。要改焦点环请改 shell 那条',
    ).toEqual([])
  })
})

/* ===========================================================================
   4. prefers-reduced-motion 覆写块（G19）
   ---------------------------------------------------------------------------
   `tokens.css` 末尾有一段 `@media (prefers-reduced-motion: reduce)`，把三条
   时长全部改成 `0ms`。这是无障碍承诺（用户说了不要动，就一点都不动），
   但此前**没有任何用例读过它**。
   （题面提到 `components.css` 也有一处；实际 grep 只有 `tokens.css` 一处。
     `index.css` 里那句只是注释。下面按实际存在的那一处断言。）

   实测：`reducedMotion: 'reduce'` 的浏览器上下文里读 computed，
   `--af-dur-instant/fast/slow` 三条都是 `0ms`；普通上下文是 80/140/240ms。
   =========================================================================== */
describe('prefers-reduced-motion（tokens.css，G19）', () => {
  const REDUCE_MEDIA = '@media (prefers-reduced-motion: reduce)'

  it('tokens.css 里有一处 @media (prefers-reduced-motion: reduce)，且里面定义了三条时长', () => {
    const rules = RULES[TOKENS].filter((r) => r.atRules.includes(REDUCE_MEDIA))
    expect(rules.length, 'prefers-reduced-motion 覆写块不见了（无障碍承诺被静默删掉）').toBeGreaterThan(0)
    const rootInside = rules.find((r) => r.selector === ':root')
    expect(rootInside, '覆写块里没有 :root（三条 --af-dur-* 该住在那里）').toBeDefined()
    for (const name of ['--af-dur-instant', '--af-dur-fast', '--af-dur-slow']) {
      expect(propValue(rootInside!, name), `${name} 在 reduce-motion 块里必须被改成 0ms`).toBe('0ms')
    }
  })

  it('三条时长的「正常值」都非 0（否则上一条是被基线兜绿的）', () => {
    const root = RULES[TOKENS].find((r) => r.selector === ':root' && r.atRules.length === 0)!
    /* 实测：普通上下文 computed 80ms / 140ms / 240ms。这里钉字面量——
       把正常值改成 0 等于「全站没有动效」，与把 reduce 块改成 200ms 是
       两个方向的同一个缺陷。 */
    expect(propValue(root, '--af-dur-instant')).toBe('80ms')
    expect(propValue(root, '--af-dur-fast')).toBe('140ms')
    expect(propValue(root, '--af-dur-slow')).toBe('240ms')
  })

  it('reduce 块里还有「动画时长压到 0.01ms」的那条通配规则（三重保险）', () => {
    const rules = RULES[TOKENS].filter((r) => r.atRules.includes(REDUCE_MEDIA))
    const wildcard = rules.find((r) => r.selector.replace(/\s+/g, ' ').trim() === '*, *::before, *::after')
    expect(wildcard, 'reduce-motion 的兜底通配规则不见了').toBeDefined()
    expect(propValue(wildcard!, 'animation-duration')).toBe('0.01ms !important')
    expect(propValue(wildcard!, 'transition-duration')).toBe('0.01ms !important')
  })
})
