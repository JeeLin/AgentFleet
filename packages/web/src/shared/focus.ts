/* ===========================================================================
   focus.ts —— 焦点栈（规范 §4.1 / §9.1）
   ---------------------------------------------------------------------------
   焦点归「最内层活动容器」。容器打开时，背后内容全部 `inert`；容器关闭时，
   焦点还给打开它的那个元素。

   ── 为什么「关闭只有一条路」是纪律 ──────────────────────────────────
   Esc、点遮罩、点关闭按钮、路由跳转前——四条路四条实现，只要有一条忘了
   还焦点，键盘用户就「丢了」。所以任何浮层组件都**只**调 `pop`，
   自己不许调 `el.focus()`。

   ── 与 keynav.ts 的分工 ─────────────────────────────────────────────
   键位派发在 `keynav.ts`，本模块只管**容器**：栈、inert、焦点落点。
   两者互不知情，于是轨迹抽屉注册 `list` 之外的键位时不必碰栈。

   本模块实现了 `focus:next` / `focus:prev`（浮层开着时 Tab 在浮层内循环），
   但它们**不住在 keynav 的全局 handler 表里**——那一表只放「全局单键」，
   而 Tab 的语义取决于当前最内层容器。所以这里声明一次实现，让 `?` 面板
   不把它们标成「（未实现）」。
   =========================================================================== */

import { declareImplemented } from './keynav'

declareImplemented('focus:next')
declareImplemented('focus:prev')

interface FocusLayer {
  container: HTMLElement
  /** 打开它的元素。还焦点时用它。 */
  opener: HTMLElement | null
}

const stack: FocusLayer[] = []

/** 可聚焦元素选择器。Tab 顺序的判据。 */
const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), ' +
  'select:not([disabled]), textarea:not([disabled]), ' +
  '[tabindex]'

/** `push` 时优先找的元素（原型叫 `[data-autofocus]`）。 */
const AUTOFOCUS = '[data-autofocus]'

/** 元素是否是可见的（`[hidden]` 祖先、`display:none`、`inert` 祖先都不算）。 */
export function isVisible(el: Element): boolean {
  if (el.closest('[hidden]')) return false
  if (el.closest('[inert]')) return false
  const win = el.ownerDocument.defaultView
  if (!win) return false
  return win.getComputedStyle(el).display !== 'none'
}

/**
 * 元素是否落在 Tab 顺序里。
 *
 * 只看**元素自己**的 `tabindex`，不看祖先：焦点环上唯一该被排除的是
 * `main[tabindex="-1"]` 与行 `tabindex="-1"` 这类「可编程聚焦但不由键盘穿过」
 * 的元素。用 `closest()` 排除的话，一个 `tabindex="-1"` 的 Modal 容器
 * 会把它里面**全部**控件一并排掉——而原型 §4.3 的浮层容器恰恰就是
 * `tabindex="-1"`，于是 trapTab 会认为「一个可聚焦元素都没有」，
 * 把每一次 Tab 都吞掉：模态里彻底走不动。
 */
export function isTabbable(el: Element): boolean {
  if (el.getAttribute('tabindex')?.trim() === '-1') return false
  return isVisible(el)
}

/**
 * 把除 `except` 之外的所有 body 直接子节点设为 `inert`。
 *
 * 原生 `inert` 同时管焦点、指针事件与辅助树——比 `aria-hidden` +
 * `tabindex` 手工管三处可靠（且 `aria-hidden` 单独用还有个坑：
 * 它对可聚焦元素是违规用法）。`aria-hidden` 仍设一份给辅助技术兜底。
 */
function inertOthers(except: HTMLElement | null, on: boolean): void {
  const body = except?.ownerDocument.body ?? document.body
  for (const el of Array.from(body.children)) {
    if (except && (el === except || el.contains(except))) continue
    ;(el as HTMLElement).inert = on
    if (on) el.setAttribute('aria-hidden', 'true')
    else el.removeAttribute('aria-hidden')
  }
}

export interface PushOptions {
  /** 打开者。缺省取 `document.activeElement`。 */
  opener?: HTMLElement | null
}

/**
 * 打开一层浮层：入栈、把背后设为 inert、把焦点交给第一个可聚焦元素。
 * 返回入栈的容器（调用方通常还要对它做尺寸与事件绑定）。
 */
export function push(container: HTMLElement, opts: PushOptions = {}): HTMLElement {
  const opener = opts.opener !== undefined ? opts.opener : (document.activeElement as HTMLElement | null)
  stack.push({ container, opener })
  // 先全解、再置：嵌套开层时第一层在 push 时把自己也标成了 inert，
  // 而它正是新层的宿主——焦点推不出去，Tab 直接穿层。「最内层容器一定可用」
  // 是这里的不变量，不依赖 pop 的对偶实现正确。
  inertOthers(null, false)
  inertOthers(container, true)
  /* 挑首焦点：`data-autofocus` → 真正能 Tab 到的元素 → 容器自己。
     **不能直接拿 FOCUSABLE 的第一个**：它含 `[tabindex]`，而对话框/抽屉的
     惯例是给内层 `role="dialog"` 挂 `tabindex="-1"` 让脚本能聚焦它。那种
     元素的 `tabIndex === -1`，**不在** Tab 序列里；把它当首焦点会让 Tab
     从它的下一个元素起算，于是它前面的第一个真实输入框被整个跳过。
     第 6 轮 4d 在活浏览器上抓到：新建任务对话框 Tab 十步的序列是
     `INPUT(空) → SELECT → TEXTAREA → BUTTON → INPUT(空)` 循环，**「标题」
     输入框从未出现**——因为首焦点落在了内层 `.af-modal`（`tabindex="-1"`）上。
     判据用 `el.tabIndex >= 0`：原生可聚焦元素（input/button/a[href]…）默认
     已是 0，`tabindex="-1"` 才是 -1。 */
  const candidates = [...container.querySelectorAll<HTMLElement>(FOCUSABLE)]
  const first =
    container.querySelector<HTMLElement>(AUTOFOCUS) ??
    /* 还要**可见**：`[hidden]` 里的按钮也是 `FOCUSABLE` 的匹配，但 `focus()`
       对它无效——浏览器会把焦点留在原处（或落到 body），于是「首焦点」实际上
       没进对话框。这是与 `tabIndex >= 0` 相邻的同一类缺陷（挑错了元素），
       第 7 轮实测确认：`<div hidden><button></div><input>` 的结构下焦点停在
       `hidden-btn`。`isVisible()` 本文件已有（Tab 陷阱用它），这里复用。 */
    candidates.find((el) => el.tabIndex >= 0 && isVisible(el))
  if (first) first.focus()
  else if (container.hasAttribute('tabindex')) container.focus()
  return container
}

/** 栈里是否还开着浮层。`?` 面板与 Esc 的判据。 */
export function depth(): number {
  return stack.length
}

/**
 * 关闭一层浮层并还焦点。`container` 为 `undefined` 时关最上层。
 *
 * 还焦点的兜底：opener 可能已经被移除（点它的那一行刚被删了），
 * 这时退到 `main[tabindex="-1"]` 而不是留下悬空引用。
 *
 * 返回是否真的关掉了一层——关一个不存在的容器返回 `false`，
 * 于是「Esc 被按了两次」与「Esc 关掉一层」在调用方可区分。
 */
export function pop(container?: HTMLElement): boolean {
  const idx = container === undefined ? stack.length - 1 : findIndexOf(container)
  if (idx < 0) return false
  const layer = stack[idx] as FocusLayer
  stack.splice(idx, 1)
  // 先解除 inert，再还焦点，最后按剩余栈顶重算 inert。
  //
  // 顺序是有约束的：opener 在浮层之外，若先按栈顶重算 inert，opener 会先被
  // 标成 inert，而 inert 元素上的 focus() 什么也不做——关闭动作会静默失焦。
  // 而关掉中间一层时会剩下一个更内层的层，所以也不能只解掉这一层。
  inertOthers(null, false)
  if (layer.opener && document.contains(layer.opener)) layer.opener.focus()
  else document.querySelector<HTMLElement>('main[tabindex="-1"]')?.focus()
  // 栈空了就没有「背后」——此时把 `except` 传 null 会把**所有**内容标成 inert，
  // 整页从此点不动。只有还开着层时才重算。
  const remaining = topContainer()
  if (remaining) inertOthers(remaining, true)
  return true
}

// `lastIndexOf` 在 ES2023 才是标准；直接手写倒序查找，让目标工具链不参与这条。
function findIndexOf(container: HTMLElement): number {
  for (let i = stack.length - 1; i >= 0; i--) {
    if ((stack[i] as FocusLayer).container === container) return i
  }
  return -1
}

/** 最内层容器的可聚焦元素。每次调用重算，不缓存快照。 */
export function focusablesIn(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(isTabbable)
}

/** 最内层浮层的容器。没有浮层返回 `null`。 */
export function topContainer(): HTMLElement | null {
  return stack.length ? (stack[stack.length - 1] as FocusLayer).container : null
}

/**
 * 焦点陷阱：把 Tab / ⇧Tab 圈在 `root` 内。
 *
 * **每次 Tab 重新查**：上一版缓存了 focusables 快照，模态 `innerHTML`
 * 重写后过期，圈住的是上一批按钮。焦点不在容器内时拉回第一个——
 * 用户可能从地址栏回来了。
 */
export function trapTab(e: KeyboardEvent, root: HTMLElement): boolean {
  const items = focusablesIn(root)
  if (items.length === 0) {
    e.preventDefault()
    return true
  }
  const first = items[0] as HTMLElement
  const last = items[items.length - 1] as HTMLElement
  const cur = document.activeElement
  if (!root.contains(cur)) {
    e.preventDefault()
    first.focus()
    return true
  }
  if (e.shiftKey && cur === first) {
    e.preventDefault()
    last.focus()
    return true
  }
  if (!e.shiftKey && cur === last) {
    e.preventDefault()
    first.focus()
    return true
  }
  return false
}

/**
 * 浮层的键盘处理：Esc 关本层、Tab 圈在容器里。返回是否接管了这次按键。
 *
 * **「本层是栈顶」由本函数自己判**，调用方不必再判一次。判据必须是栈顶而不是
 * 「我开着」：键盘事件是全站共用的，帮助面板（或另一层浮层）盖在抽屉上时，
 * 一次 Esc 会把两层一起关掉——而 `pop()` 只按栈顶语义工作。
 *
 * `onClose` 要**同步**执行，且必须是组件自己的关闭入口（里面走 `pop`）。
 * 不在这里 `pop` 是因为组件关闭还要 emit（父级复位 `open`），入口只有一个。
 *
 * `e.isComposing` 期间一律让路：中文输入法正在选字时按 Esc 是「撤掉候选词」，
 * 不该顺手把浮层关掉。
 */
export function handleLayerKeydown(
  e: KeyboardEvent,
  container: HTMLElement,
  onClose: () => void,
): boolean {
  if (e.isComposing) return false
  if (topContainer() !== container) return false
  if (e.key === 'Escape') {
    e.preventDefault()
    onClose()
    return true
  }
  if (e.key === 'Tab') {
    // `trapTab` 内部已经 `preventDefault` 了它接管的那些，这里只看它接没接。
    return trapTab(e, container)
  }
  return false
}

/**
 * 跳转后的首焦点（规范 §4.3）：把焦点从 `<body>` 拿出来落到 `main`，
 * 并给 `h1` 补上 `tabindex="-1"`，让「⇧Tab 回标题」成为可能。
 *
 * 用 `main` 而不是 `h1.focus()`：`main` 的 `tabindex="-1"` 是「这里是一片
 * 内容的起点」，比「这里是标题」更准确。
 *
 * **路由落焦点不要直接用它**：`router.ts` 的 `navigate()` / `popstate` 走的是
 * `focusRouteTarget()`——它在「明确定位到来源元素」时才让出焦点，其余情形
 * 才落到这里。本函数保留为**无条件落 main** 的那一档（既有 5 条用例钉着
 * 它的语义，见 `focus.test.ts` / `router.test.ts`）。
 */
export function focusMain(doc: Document = document): HTMLElement | null {
  const main = doc.querySelector<HTMLElement>('main[tabindex="-1"]')
  if (!main) return null
  const h1 = main.querySelector('h1')
  if (h1 && !h1.hasAttribute('tabindex')) h1.setAttribute('tabindex', '-1')
  main.focus()
  return main
}

/* ===========================================================================
   路由级焦点目标（v0.8.1 第5轮 G65）
   ---------------------------------------------------------------------------
   **缺陷**：卡片 click / Enter 进任务详情后 `document.activeElement` 是
   `MAIN.af-main`（`tabindex="-1"`），第一次 Tab 落到页头的「← 看板」；按浏览器
   Back 回到看板时焦点又不回那张卡。键盘用户走一趟就得重新 Tab 一遍整页。

   **成因**：换页后落焦点只有一条路——「交回 main」。`router.ts` 的
   `navigate()` / `startRouter().onPop` 调 `focusMain()`，`App.vue` 的路由
   watch 也内联了同一段。它分不出「用户从哪儿来」，于是每次都把焦点从
   刚点的那张卡拿走。

   **改法**：换页落焦点前先问一句「这次换页有明确来源吗」。有 → 把焦点交给
   **新页面自己的标题**（详情页 `h1`，带 `tabindex="-1"`）；来源元素能救回来
   → 交给它（返回看板时回到同一张卡）；都没有 → `main`（原语义，不动）。

   ── 两张表：sticky（只活一次）+ last（活到下次落焦点） ─────────────
   `sticky`：发起导航的一方（点卡片 / 点列表行）在 `navigate()` **之前**登记。
   它的唯一含义是「**接下来那一次**落焦点别抢 main，让页面自己落标题」。
   登记之后**必须被消费一次**，否则「页面自己落标题」与「落到 main」两条
   规则会在同一次换页里打架。

   `last`：`focusRouteTarget()` **实际聚焦到的**元素。它活到下一次落焦点，
   是「用户上一步站在哪」的等价物。从详情返回时 `sticky` 已经没了，但
   `last`（详情页 `h1`）还带着 `data-id`，于是能按 id 找回同一张卡。

   ── 为什么按 `data-id` 而不是 CSS 类找回来 ────────────────────────
   类名是样式契约（`class-usage.test.ts` 还双向钉着），会随重构漂；也有的
   卡片同时挂四五个类，挑哪个当身份都是猜。`data-*` 天生是行为钩子：
   `TaskCard.vue` 的 `:data-id="task.id"` 与详情页 `h1` 的 `:data-id` 同源，
   两边写的是同一个任务 id，一眼能追。

   ── 一条纪律：`focusRouteTarget()` 幂等且不抛 ─────────────────────
   它会在 `navigate()` 的微任务里、`popstate` 的微任务里、以及页面自己的
   `onMounted` 里被调用（应用了 Lane D 补丁之后 App.vue 的 watch 也调）。
   谁先跑都不该炸；`sticky` 被消费之后重复调用就是「无来源」那一档。
   =========================================================================== */

/**
 * 「这一次换页把落点让给页面自己」的标志。
 *
 * 由 `setRouteFocusTarget()` 置位，由 `focusRouteTarget()` 消费一次。**它不负责
 * 记住上一站**（那是 `lastRouteFocus` 的事）——两件事分开存，是因为它们
 * 生命周期不同：让出只活到第一次落焦点，上一站要活到下一次换页。
 */
let routeFocusYield = false

/** 上一站：发起导航的那个元素（卡片 / 行）。返回时按它的 `data-id` 找回。 */
let lastRouteFocus: HTMLElement | null = null

/**
 * 登记这次导航的**来源元素**。
 *
 * 由发起导航的一方在 `navigate()` 之前调用（看板的 `openTask`）。两件事：
 *   ① 下一次 `focusRouteTarget()` **让出焦点**（页面自己落标题）；
 *   ② 记住它当「上一站」——返回时按 `data-id` 找回同一张卡/行。
 *
 * 传 `null` 等同「本次无来源」（不让出、不记上一站）。
 */
export function setRouteFocusTarget(el: HTMLElement | null): void {
  routeFocusYield = el !== null
  if (el) lastRouteFocus = el
}

/**
 * 清空两处记录。测试用（模块级状态会跨用例污染）。生产代码不需要——
 * 消费与覆盖会自动推进。
 */
export function clearRouteFocusTarget(): void {
  routeFocusYield = false
  lastRouteFocus = null
}

/** 是否还欠着一次「让出 main」。只给外壳接线与测试用。 */
export function hasRouteFocusTarget(): boolean {
  return routeFocusYield
}

/**
 * 「上一站」的 `data-id`（没有上一站时为 `null`）。只给测试用。
 *
 * 与 `hasRouteFocusTarget()` 分开，是因为两者生命周期不同：让出标志在第一次
 * 落焦点后立刻消失（`navigate()` 的微任务就把它消费了），而上一站要活到
 * 下一次换页。想验「`openTask` 登记了来源」这件事，只有这个值在微任务之后
 * 仍然看得到。
 */
export function lastRouteFocusId(): string | null {
  return lastRouteFocus?.getAttribute('data-id') ?? null
}

/**
 * 换页后的首焦点。返回实际聚焦的元素（没聚焦任何人时 `null`）。
 *
 * 三档优先级（**顺序就是判据**）：
 *
 *   1. **还欠着一次「让出」** → 不聚焦任何东西，把标志清掉。
 *      这一次换页的落点由**页面自己**负责（详情页落自己的 `h1`）。
 *      为什么不在这里替页面聚焦标题：本函数被 `navigate()` 的微任务调用，
 *      那一刻 Vue 还没把新页面渲染出来（`App.vue` 的注释写过同一件事），
 *      查到的 `h1` 是上一页的——在旧节点上聚焦等于把焦点扔进一个马上要
 *      消失的元素。
 *
 *   2. **有上一站（`lastRouteFocus` 带 `data-id`）** → 在 `main` 里找同名
 *      `data-id` 的元素并聚焦。这是「详情 → 返回列表」那一步：回看板不是
 *      「点卡片」，没人登记让出，但上一站是详情页带着同一个任务 id 的
 *      `h1`，于是焦点回到**那张卡**上。
 *
 *   3. 其余 → `main` 自己（`h1` 只补 `tabindex="-1"`，不抢焦点）。
 *      这一档**逐字等价于旧的 `focusMain()`**：`router.test.ts` 与
 *      `App.test.ts` 的既有用例断的就是「`activeElement` 是 `main`」。
 *
 * 幂等且不抛：它会被 router 的微任务、popstate 的微任务、页面自己的
 * `onMounted` 各调一次（应用 Lane D 的补丁之后 App.vue 的 watch 也调），
 * 谁先跑都不该炸。
 */
export function focusRouteTarget(doc: Document = document): HTMLElement | null {
  if (routeFocusYield) {
    routeFocusYield = false
    return null
  }
  const main = doc.querySelector<HTMLElement>('main[tabindex="-1"]')
  const h1 = main?.querySelector('h1') ?? null
  if (h1 && !h1.hasAttribute('tabindex')) h1.setAttribute('tabindex', '-1')
  // 找回上一站时**优先在本页 `main` 里找**（那是真实的 App 形状），
  // 找不到就退到整份文档——组件级测试里被挂载的页面不在 `main` 里面，
  // 而「返回时回到来源卡片」这条契约与包不包 `main` 无关。
  const wanted = lastRouteFocus?.getAttribute('data-id') ?? null
  const target =
    findByDataId(main, wanted) ?? findByDataId(doc.documentElement, wanted) ?? main
  if (!target) return null
  target.focus()
  return target
}

/**
 * 在 `root` 里找 `data-id === wanted` 的元素。
 *
 * **不用属性选择器**（`[data-id="${…}"]`）：值里出现引号或反斜杠时，
 * 就算 `CSS.escape` 转义了，`querySelector` 也可能抛 `DOMException`——而本函数
 * 抛异常等于「换页后键盘用户的焦点丢了」。逐元素比属性值是唯一不会因为
 * 数据长得奇怪而失败的写法，且元素数量是「一块板子的卡片数」，成本可忽略。
 */
function findByDataId(root: Element | null, wanted: string | null): HTMLElement | null {
  if (!root || wanted === null || wanted === '') return null
  for (const el of Array.from(root.querySelectorAll<HTMLElement>('[data-id]'))) {
    if (el.getAttribute('data-id') === wanted) return el
  }
  return null
}

/** 清空栈。测试用；生产代码不许调（它会让所有浮层的关闭路径变成空操作）。 */
export function resetForTest(): void {
  stack.length = 0
  inertOthers(null, false)
  clearRouteFocusTarget()
}