/* ===========================================================================
   listbox.ts —— 列表 / 树导航（规范 §4.4）
   ---------------------------------------------------------------------------
   标准 listbox 模式：**容器是唯一的 Tab 落点，行是 `tabindex="-1"`**。
   Tab 进容器，方向键在行间走——所以「表格」与「列表」是两种东西，
   用错了键盘用户就得 Tab 一百次才能动一行。

   ── 为什么不注册全局单键 ────────────────────────────────────────────
   ↑↓/Home/End/Enter **只在容器内生效**，监听器挂在容器上而不是 document。
   否则顶部会话 tab 条的 ←→ 与列表的 ↑↓ 会互相抢键：原型 keynav.js 正是
   全局单键派发，结果「列表导航」在会话工作台上被 tab 条的 handler 抢走。
   规范 §4.4 的 Enter 语义属于**行**，不属于键盘监听器：行自己决定
   `data-href` 跳转 / 点内含的 a|button / 否则抛 `af:enter` 事件。

   ── 选中态为什么写 aria-selected ─────────────────────────────────────
   加个背景色对辅助技术不存在。`aria-selected` 才是「哪一行被选中」的可读形态，
   而 `AF.currentRow()` 也靠它判断，所以它不是装饰。
   =========================================================================== */

import { isActivatable } from './keynav'

/** 容器角色：`list` 是列表，`tree` 是树（目录行另有 `aria-expanded`）。 */
export type ListRole = 'list' | 'tree'

/** 行被 Enter 激活、且行自己也没有可点的 a/button 时抛的事件名。 */
export const ENTER_EVENT = 'af:enter'

/** 行被 Space 切换、且行不是目录行时抛的事件名。 */
export const TOGGLE_EVENT = 'af:toggle'

export interface ListboxOptions {
  /** 容器角色。默认 `list`。 */
  role?: ListRole
  /** ↑↓ 到头时是否绕回另一端。默认不绕回。 */
  wrap?: boolean
  /**
   * Enter **归行**：不点行内的 a / button，直接交给 `onEnter`（或抛 `ENTER_EVENT`）。
   *
   * 默认行为（规范 §4.4）是「行内有可点的 a|button 就点它」——对文件树
   * 这类「行本身就是那个控件」的列表是对的。但一个**表格行**里常驻着一个
   * 行内动作按钮（设备行的「用它开会话」），那条规则会让 ⏎ 变成「点那个
   * 按钮」：用户想「看这一台」，却新建了一个会话。所以这类行必须显式声明
   * Enter 属于行，而不是让 DOM 里的按钮顺序决定 Enter 的语义。
   */
  rowEnter?: boolean
  /**
   * Enter 的兜底。行有 `data-href` 或内含 a/button 时**不**调它——
   * 「Enter 的语义属于行」是规范 §4.4 的否决项，不许在监听器里猜。
   * 设了 `rowEnter` 的行不受此限：那样写出来的意图就是「Enter 归我」。
   */
  onEnter?: (row: HTMLElement, e: KeyboardEvent) => boolean
  /** Space 的兜底。目录行自己切 `aria-expanded`，不调它。 */
  onToggle?: (row: HTMLElement, e: KeyboardEvent) => boolean
}

export interface ListboxHandle {
  /** 容器。 */
  readonly el: HTMLElement
  /** 当前可见的行（不含 `[hidden]` 祖先里的）。 */
  rows(): HTMLElement[]
  /** 当前选中行；没有返回 `null`。 */
  current(): HTMLElement | null
  /** 移动焦点与选中。`delta` 为负向上。返回是否移动了。 */
  move(delta: number): boolean
  /** 选中指定行并聚焦它。 */
  select(row: HTMLElement, focus?: boolean): void
  /** 取消整组的选中（Esc 的语义）。 */
  clearSelection(): void
  /** 移除监听器。组件卸载时调。 */
  detach(): void
}

function isRow(el: Element): el is HTMLElement {
  return el instanceof HTMLElement && el.hasAttribute('data-row')
}

/**
 * 可见的行。`[hidden]` 祖先里的行不算——折叠起来的子目录不能被
 * 方向键走到（原型 keynav.js §5 的 `!r.closest('[hidden]')` 判据）。
 */
function visibleRows(list: HTMLElement): HTMLElement[] {
  return Array.from(list.querySelectorAll<HTMLElement>('[data-row]')).filter(
    (r) => !r.closest('[hidden]') && getComputedStyle(r).display !== 'none',
  )
}

/** 行所在的容器（向上找最近的 list/tree）。 */
export function listOf(el: Element | null): HTMLElement | null {
  if (!el) return null
  const found = el.closest('[data-role="list"], [data-role="tree"]')
  return found instanceof HTMLElement ? found : null
}

/** 容器里当前选中行。 */
export function currentRow(): HTMLElement | null {
  const list = listOf(document.activeElement)
  if (!list) return null
  return visibleRows(list).find((r) => r.getAttribute('aria-selected') === 'true') ?? null
}

/** 把整组的 `aria-selected` 收敛到只有 `row` 是 `true`。 */
export function selectRow(row: HTMLElement, focus = true): void {
  const list = listOf(row)
  if (!list) return
  for (const r of visibleRows(list)) {
    r.setAttribute('aria-selected', r === row ? 'true' : 'false')
  }
  if (focus) row.focus()
}

/**
 * 给一个容器装上列表导航。
 *
 * 行**不需要**由本函数创建：它是纯导航层，渲染是组件的事。
 * 约定由调用方满足：容器 `data-role` + `tabindex="0"`，行 `data-row`
 * + `tabindex="-1"`（原型 §4.4 给的形状）。
 */
export function attach(el: HTMLElement, opts: ListboxOptions = {}): ListboxHandle {
  const role: ListRole = opts.role ?? (el.dataset.role === 'tree' ? 'tree' : 'list')
  const wrap = opts.wrap ?? false

  // 角色由调用方在 DOM 上声明；这里只在两处不一致时纠正——
  // 一个 `role="treebox"` 却带 `[data-role="list"]` 的容器会让 CSS 与
  // listOf 走岔，而这类拼错在 DOM 里看不出来。
  if (el.dataset.role !== role) el.dataset.role = role
  const domRole = role === 'tree' ? 'tree' : 'listbox'
  if (el.getAttribute('role') !== domRole) el.setAttribute('role', domRole)

  function rows(): HTMLElement[] {
    return visibleRows(el)
  }

  function select(row: HTMLElement, focus = true): void {
    selectRow(row, focus)
  }

  function current(): HTMLElement | null {
    const list = listOf(document.activeElement)
    if (!list) return null
    return visibleRows(list).find((r) => r.getAttribute('aria-selected') === 'true') ?? null
  }

  function move(delta: number): boolean {
    const list = listOf(document.activeElement)
    if (!list) return false
    const all = visibleRows(list)
    if (all.length === 0) return false
    const cur = all.indexOf(document.activeElement as HTMLElement)
    let next: number
    if (cur < 0) {
      // 焦点还在容器上（用户刚 Tab 进来）：向下进第一行，向上进最后一行
      next = delta > 0 ? 0 : all.length - 1
    } else {
      next = cur + delta
      if (next < 0) next = wrap ? all.length - 1 : 0
      else if (next > all.length - 1) next = wrap ? 0 : all.length - 1
    }
    selectRow(all[next] as HTMLElement)
    return true
  }

  function clearSelection(): void {
    for (const r of rows()) r.setAttribute('aria-selected', 'false')
  }

  /** 行的 Enter：语义属于行——有去向就去，没有就抛事件给页面。 */
  function activate(row: HTMLElement, e: KeyboardEvent): boolean {
    // `rowEnter` 的行跳过「点行内控件」这一步：表格行里常驻的动作按钮
    // 不该决定 ⏎ 的含义（见 ListboxOptions.rowEnter）。
    if (!opts.rowEnter) {
      const href = row.dataset.href
      const win = row.ownerDocument.defaultView
      if (href && win) {
        win.location.assign(href)
        return true
      }
      const inner = row.querySelector<HTMLElement>('a[href], button')
      if (inner) {
        inner.click()
        return true
      }
    }
    if (opts.onEnter) return opts.onEnter(row, e)
    row.dispatchEvent(new CustomEvent(ENTER_EVENT, { bubbles: true, detail: { row } }))
    return true
  }

  /** 行的 Space：目录行切 aria-expanded，其余抛事件（或走 onToggle）。 */
  function toggle(row: HTMLElement, e: KeyboardEvent): boolean {
    if (row.hasAttribute('aria-expanded')) {
      const open = row.getAttribute('aria-expanded') === 'true'
      row.setAttribute('aria-expanded', open ? 'false' : 'true')
      const kids = row.parentElement?.querySelector<HTMLElement>('[data-kids]')
      if (kids) kids.hidden = open
      return true
    }
    if (opts.onToggle) return opts.onToggle(row, e)
    const on = row.getAttribute('aria-selected') !== 'true'
    selectRow(row, false)
    row.dispatchEvent(new CustomEvent(TOGGLE_EVENT, { bubbles: true, detail: { row, on } }))
    return true
  }

  function onKeydown(e: KeyboardEvent): void {
    // 中文拼音选字期间的按键不是「用户想操作」
    if (e.isComposing) return
    switch (e.key) {
      case 'ArrowDown':
      case 'j':
        if (move(1)) e.preventDefault()
        return
      case 'ArrowUp':
      case 'k':
        if (move(-1)) e.preventDefault()
        return
      case 'Home': {
        const all = rows()
        if (all.length) {
          selectRow(all[0] as HTMLElement)
          e.preventDefault()
        }
        return
      }
      case 'End': {
        const all = rows()
        if (all.length) {
          selectRow(all[all.length - 1] as HTMLElement)
          e.preventDefault()
        }
        return
      }
      case 'Enter': {
        const row = focusedRow()
        if (row && activate(row, e)) e.preventDefault()
        return
      }
      case ' ': {
        const row = focusedRow()
        if (!row) return
        // Space 在按钮 / 链接聚焦时是「按下按钮」，不抢——**但行自己就是
        // `<button data-row>`**（规范 §4.4 给的形状），所以判据必须是
        // 「焦点在一个不是当前行的可激活控件上」。原型 keynav.js:423 只看
        // tagName，于是 Space 在任何一行上都不生效——`?` 面板里写着
        // 「Space 展开 / 收起目录，或多选切换」，实际按了没反应。
        const ae = document.activeElement as HTMLElement | null
        if (ae && !isRow(ae) && isActivatable(ae)) return
        if (toggle(row, e)) e.preventDefault()
        return
      }
      default:
        return
    }
  }

  /** 当前焦点所在行（焦点在容器上时取选中行，都没有则 `null`）。 */
  function focusedRow(): HTMLElement | null {
    const ae = document.activeElement
    if (ae && isRow(ae)) return ae
    return current()
  }

  /**
   * 容器获得焦点时把焦点交给当前行（或第一行）——
   * 否则用户 Tab 进容器后按 ↓ 失灵（原型 keynav.js §5 的 focusin 分支）。
   * 只在 `focusin` 冒泡阶段判「焦点就是容器本身」，所以不会与 `select()`
   * 造成的行聚焦互相触发。
   */
  function onFocusin(e: FocusEvent): void {
    const t = e.target as HTMLElement | null
    if (!t) return
    if (isRow(t)) {
      selectRow(t, false)
      return
    }
    if (t === el) {
      const all = rows()
      if (all.length === 0) return
      const sel = all.find((r) => r.getAttribute('aria-selected') === 'true')
      ;(sel ?? (all[0] as HTMLElement)).focus()
    }
  }

  /** 指针点在行上时把它变成键盘焦点，否则鼠标点完键盘方向键接着生效。 */
  function onPointerdown(e: PointerEvent): void {
    const t = e.target as HTMLElement | null
    if (!t) return
    const row = t.closest('[data-row]')
    if (row instanceof HTMLElement && row.tabIndex === -1) {
      row.focus()
      selectRow(row, false)
    }
  }

  el.addEventListener('keydown', onKeydown)
  el.addEventListener('focusin', onFocusin)
  el.addEventListener('pointerdown', onPointerdown)

  return {
    el,
    rows,
    current,
    move,
    select,
    clearSelection,
    detach() {
      el.removeEventListener('keydown', onKeydown)
      el.removeEventListener('focusin', onFocusin)
      el.removeEventListener('pointerdown', onPointerdown)
    },
  }
}