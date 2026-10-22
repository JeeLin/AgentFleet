/* ===========================================================================
   shell-keys.ts —— 外壳级键位的唯一安装点（规范 §4.2）
   ---------------------------------------------------------------------------
   外壳级的键位（`?` / `g s` / `⌃B` / `/` / `Esc`）**只在这个文件里注册一次**。
   页面级的键位（会话工作台的 `o` `[` `]` `\`、面板里的 `⌃L` `⌃S`）由页面
   自己按需注册——「本页有没有注册 handler」就是 scope 的生效判据。

   ── 为什么回调走一个模块级槽位 ────────────────────────────────────
   `keynav.register` 拒绝重复注册，而页面组件在测试里会被多次挂载。
   如果每次挂载都注册一次，第二个用例就在模块加载期抛错；如果只在第一次
   挂载时注册，闭包里捕获的是**第一个**组件的状态，卸载后整条链路指向
   一个死掉的实例。所以 handler 只注册一次，读的是当前挂上的那组回调。

   ── `g` 序列 ──────────────────────────────────────────────────────
   800ms 窗口（`PREFIX_MS`），**没配上的第二个键不吞**：用户可能只是在
   打一段以 g 开头的字。`g` 自己不进 `KEYS` 表——它是前缀，不是动作。
   =========================================================================== */

import { PREFIX, PREFIX_MS, dispatch, isTyping, registerInternal } from './keynav'

export interface ShellBindings {
  /** 跳到一条路由。 */
  navigate: (to: string) => void
  /** 折叠 / 展开侧栏。 */
  toggleRail: () => void
  /** 打开键位帮助。 */
  openHelp: () => void
  /** 关闭帮助面板；没有打开时返回 `false`。 */
  closeHelp: () => boolean
  /** 聚焦本页的搜索框；本页没有搜索时返回 `false`（键还给浏览器）。 */
  focusSearch: () => boolean
}

let bindings: ShellBindings | null = null
let installed = false

/** 让外壳的键位指向这一组回调。返回解绑函数（只解开自己这一组）。 */
export function bindShell(b: ShellBindings): () => void {
  bindings = b
  return () => {
    if (bindings === b) bindings = null
  }
}

/** 安装外壳级键位。重复调用是空操作。 */
export function installShellKeys(): void {
  if (installed || typeof document === 'undefined') return
  installed = true

  registerInternal('help:toggle', () => {
    if (!bindings) return false
    // `?` 是开关：帮助开着时再按 `?` 关掉（§4.2 的行为是「打开 / 关闭」）。
    if (bindings.closeHelp()) return true
    bindings.openHelp()
    return true
  })

  registerInternal('layer:escape', () => {
    if (!bindings) return false
    // 只有真的有层可关时才吞 Esc；否则让浏览器处理（清掉输入框的选择等）。
    return bindings.closeHelp()
  })

  registerInternal('rail:toggle', () => {
    if (!bindings) return false
    bindings.toggleRail()
    return true
  })

  registerInternal('search:focus', () => {
    if (!bindings) return false
    return bindings.focusSearch()
  })

  registerInternal('nav:sessions', () => nav('/sessions'))
  registerInternal('nav:new-session', () => nav('/sessions/new'))
  registerInternal('nav:devices', () => nav('/devices'))
  registerInternal('nav:agents', () => nav('/agents'))
  registerInternal('nav:settings', () => nav('/settings'))

  document.addEventListener('keydown', onKeydown)
}

function nav(to: string): boolean {
  if (!bindings) return false
  bindings.navigate(to)
  return true
}

/** `g` 序列的等待状态：记下按下它的时刻。 */
let prefixAt = 0
let prefixTimer: ReturnType<typeof setTimeout> | null = null

function clearPrefix(): void {
  prefixAt = 0
  if (prefixTimer) {
    clearTimeout(prefixTimer)
    prefixTimer = null
  }
}

function onKeydown(e: KeyboardEvent): void {
  // 中文拼音选字期间全部让路
  if (e.isComposing) return
  // 浮层开着时由浮层自己处理（焦点栈的 Esc 已由上面的 handler 接住）
  if (e.defaultPrevented) return

  // `?` / `/` / `g` 这类**无修饰键**的单键在输入区内不抢
  const typing = isTyping(e.target)

  const mod = e.ctrlKey || e.metaKey
  if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'b') {
    dispatch('rail:toggle', e)
    return
  }
  if (modeOnly(e)) {
    if (!typing && e.key === '?') {
      dispatch('help:toggle', e)
      return
    }
    if (!typing && e.key === '/') {
      if (dispatch('search:focus', e)) return
      return
    }
    // 序列的第二个键优先于「重新武装前缀」：否则 `g g` 永远触发不了——
    // 第二键 `g` 会命中下面那个分支，把窗口续期一遍再 return。
    // `PREFIX` 里有 `g`（`g g` = traj:toggle）时，这里必须是序列命中路径。
    if (prefixAt && Date.now() - prefixAt <= PREFIX_MS) {
      const act = PREFIX.get(e.key)
      clearPrefix()
      // **第二键也要判 typing**：第一键判过不代表第二键安全——用户在搜索框
      // 里敲「g」再敲「t」，第二键落在输入区里，不该被解释成
      // 「跳到 /trajectory」并把那两个字符吃掉。
      if (typing) return
      if (act) {
        dispatch(act, e)
        return
      }
      // 没配上：不吞，用户可能只是在打一段以 g 开头的字
      return
    }
    if (!typing && e.key === 'g') {
      // 前缀本身不吞键以外的任何东西：它只是在等第二个键
      e.preventDefault()
      clearPrefix()
      prefixAt = Date.now()
      prefixTimer = setTimeout(clearPrefix, PREFIX_MS)
      return
    }
    if (e.key === 'Escape') {
      dispatch('layer:escape', e)
      return
    }
  }
}

/** 没有 ⌃ / ⌘ / ⌥（⇧ 允许：`?` 本身就是 ⇧/）。 */
function modeOnly(e: KeyboardEvent): boolean {
  return !e.ctrlKey && !e.metaKey && !e.altKey
}
