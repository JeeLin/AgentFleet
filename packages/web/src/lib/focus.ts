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
  const first =
    container.querySelector<HTMLElement>(AUTOFOCUS) ??
    container.querySelector<HTMLElement>(FOCUSABLE)
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
 * 跳转后的首焦点（规范 §4.3）：把焦点从 `<body>` 拿出来落到 `main`，
 * 并给 `h1` 补上 `tabindex="-1"`，让「⇧Tab 回标题」成为可能。
 *
 * 用 `main` 而不是 `h1.focus()`：`main` 的 `tabindex="-1"` 是「这里是一片
 * 内容的起点」，比「这里是标题」更准确。
 */
export function focusMain(doc: Document = document): HTMLElement | null {
  const main = doc.querySelector<HTMLElement>('main[tabindex="-1"]')
  if (!main) return null
  const h1 = main.querySelector('h1')
  if (h1 && !h1.hasAttribute('tabindex')) h1.setAttribute('tabindex', '-1')
  main.focus()
  return main
}

/** 清空栈。测试用；生产代码不许调（它会让所有浮层的关闭路径变成空操作）。 */
export function resetForTest(): void {
  stack.length = 0
  inertOthers(null, false)
}