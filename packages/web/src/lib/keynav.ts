/* ===========================================================================
   keynav.ts —— 键位表的唯一真相源（规范 §4.2）
   ---------------------------------------------------------------------------
   `KEYS` 是全站键位的唯一真相源，`?` 帮助面板由它渲染，所以「文档写的键位」
   与「按下会发生的键位」不可能漂移。三条配套约束：

     ① `register` 拒绝表外 act —— 一个 `?` 里查不到的键位，对键盘用户等于不存在
     ② `register` 拒绝重复注册 —— 一个动作只能有一个主人，否则行为取决于
        加载顺序且没有任何提示
     ③ `unimplemented()` 列出「表里有、没人实现」的 act —— 开发期在控制台报警，
        而不是等用户按到死键

   ── 与原型 keynav.js 的差异（不是搬，是纠正）───────────────────────
   原型把 `traj:toggle` 写进了「会话工作台」组，而 `g g` 这个键位在**该组没有
   scope**，`g a` 的 `g` 按前缀语义被指到了 `nav:agents`。两处都是笔误：
   规范 §4.2 的表里 `t` 与 `g g` 归在「会话工作台」（scope=session）。
   本表按规范抄，并给「跳转」组补上 scope=global——它的五个键位里，
   `g s` 那个 `s` 在会话工作台上正是 list:enter 的键位，没有 scope 就必然打架。
   =========================================================================== */

/** 键位生效范围。 */
export type KeyScope = 'global' | 'list' | 'session' | 'panel'

/** 一个动作可由多个键位触发（`t` 与 `g g` 同为 `traj:toggle`）。 */
export interface KeyBinding {
  /** 键位描述，直接进 `<kbd>`。与规范 §4.2 的表格逐字一致。 */
  keys: string[]
  /** 动作名。必须唯一（同一 act 可有多个 keys）。 */
  act: string
  /** 说明文字，`?` 面板第二列。 */
  label: string
  /** 生效范围。缺省 `global`。 */
  scope: KeyScope
  /** 同一 act 的另一个键位。`?` 面板里标注它，不重复算一个承诺。 */
  alias?: boolean
}

/** 一组键位。`?` 面板按组渲染。 */
export interface KeyGroup {
  group: string
  scope: KeyScope
  keys: KeyBinding[]
}

/**
 * 全局键位表。**这个表是契约**：增删一条即改设计文档，
 * 而 `src/lib/keynav.test.ts` 会把条目数与 `docs/design-system.md` §4.2
 * 的「当前键位表（N 个 act）」grep 对齐——文档与表漂移时门禁变红。
 */
export const KEYS: readonly KeyGroup[] = [
  {
    group: '全局',
    scope: 'global',
    keys: [
      { keys: ['?'], act: 'help:toggle', label: '打开 / 关闭键位帮助', scope: 'global' },
      { keys: ['/'], act: 'search:focus', label: '聚焦搜索框', scope: 'global' },
      {
        keys: ['Esc'],
        act: 'layer:escape',
        label: '关闭最上层浮层；无浮层时取消选中',
        scope: 'global',
      },
      { keys: ['Tab'], act: 'focus:next', label: '焦点前进（有浮层时在浮层内循环）', scope: 'global' },
      { keys: ['⇧Tab'], act: 'focus:prev', label: '焦点后退（有浮层时在浮层内循环）', scope: 'global' },
      { keys: ['⌃B'], act: 'rail:toggle', label: '折叠 / 展开侧栏', scope: 'global' },
    ],
  },
  {
    group: '跳转',
    scope: 'global',
    keys: [
      { keys: ['g', 's'], act: 'nav:sessions', label: '会话列表', scope: 'global' },
      { keys: ['g', 'n'], act: 'nav:new-session', label: '新建会话', scope: 'global' },
      { keys: ['g', 'd'], act: 'nav:devices', label: '设备列表', scope: 'global' },
      { keys: ['g', 'a'], act: 'nav:agents', label: 'Agent 目录', scope: 'global' },
      { keys: ['g', ','], act: 'nav:settings', label: '设置', scope: 'global' },
    ],
  },
  {
    group: '列表 / 树',
    scope: 'list',
    keys: [
      { keys: ['↑', '↓'], act: 'list:move', label: '移动焦点行（j / k 同）', scope: 'list' },
      { keys: ['⏎'], act: 'list:enter', label: '进入选中项', scope: 'list' },
      { keys: ['Space'], act: 'list:toggle', label: '展开 / 收起目录，或多选切换', scope: 'list' },
    ],
  },
  {
    group: '会话工作台',
    scope: 'session',
    keys: [
      { keys: ['o'], act: 'panel:open', label: '打开一个面板（终端 / 文件）', scope: 'session' },
      { keys: ['['], act: 'panel:prev', label: '切换到上一个已打开的面板', scope: 'session' },
      { keys: [']'], act: 'panel:next', label: '切换到下一个已打开的面板', scope: 'session' },
      { keys: ['\\'], act: 'dock:toggle', label: '打开 / 收起面板坞', scope: 'session' },
      { keys: ['t'], act: 'traj:toggle', label: '打开 / 关闭轨迹抽屉', scope: 'session' },
      {
        keys: ['g', 'g'],
        act: 'traj:toggle',
        label: '打开 / 关闭轨迹抽屉（g 序列）',
        scope: 'session',
        alias: true,
      },
    ],
  },
  {
    group: '终端面板',
    scope: 'panel',
    keys: [{ keys: ['⌃L'], act: 'term:clear', label: '清屏', scope: 'panel' }],
  },
  {
    group: '文件面板',
    scope: 'panel',
    keys: [
      { keys: ['⌃S'], act: 'file:save', label: '保存当前文件', scope: 'panel' },
      { keys: ['Backspace'], act: 'file:up', label: '回到上一级目录', scope: 'panel' },
    ],
  },
]

/**
 * 一个 act 的反查表。构建期即校验，避免运行到一半才发现表本身有问题。
 *
 * 校验的是规范 §4.2 的 `g` 序列纪律：*两个 act 共用一个键位描述*抛错。
 * 同一个 act 有两个不同键位（`t` 与 `g g` 同为 `traj:toggle`）是 alias，
 * 合法。反过来的原型 keynav.js:95 恰好把这两件事写颠倒了——它对「同一 act
 * 两个键位」抛错、对「两个 act 共用键位」放行，于是 `t` 与 `g g` 进表时
 * 整个模块加载即抛，而两个 act 抢同一个键位（`g s` 与 list:enter 的 `s`）
 * 反而悄悄通过。搬代码时照搬这条会连表都载入不进来。
 */
const ACTS: ReadonlyMap<string, KeyBinding> = (() => {
  const out = new Map<string, KeyBinding>()
  const ownerOfKey = new Map<string, string>()
  for (const g of KEYS) {
    for (const k of g.keys) {
      const desc = k.keys.join(' ')
      const owner = ownerOfKey.get(desc)
      if (owner !== undefined && owner !== k.act) {
        throw new Error(
          `keynav: 键位 "${desc}" 被两个 act 共用（${owner} 与 ${k.act}）——` +
            '按这个键时行为取决于注册顺序，没有任何提示',
        )
      }
      ownerOfKey.set(desc, k.act)
      if (!out.has(k.act)) out.set(k.act, k)
    }
  }
  return out
})()

/** 表里一共多少个 act。测试与设计文档对齐用的就是这个数。 */
export function actCount(): number {
  return ACTS.size
}

/** act 是否在表里。 */
export function hasAct(act: string): boolean {
  return ACTS.has(act)
}

/** 全部 act 名（表序）。 */
export function acts(): string[] {
  return [...ACTS.keys()]
}

/** 一个 act 的定义。不在表里返回 `undefined`。 */
export function bindingOf(act: string): KeyBinding | undefined {
  return ACTS.get(act)
}

/**
 * `g` 序列的目标：按下 `g` 后的第二个键决定去向。
 * 800ms 窗口内没配上**不吞键**——用户只是在打一段以 `g` 开头的字。
 */
export const PREFIX: ReadonlyMap<string, string> = new Map([
  ['s', 'nav:sessions'],
  ['n', 'nav:new-session'],
  ['d', 'nav:devices'],
  ['a', 'nav:agents'],
  [',', 'nav:settings'],
  ['g', 'traj:toggle'],
])

/** `g` 序列的窗口。 */
export const PREFIX_MS = 800

/** handler 返回 `true` 表示吞掉这个键；`false` / `undefined` 表示还给浏览器。 */
export type KeyHandler = (e: KeyboardEvent, data?: unknown) => boolean | undefined

const handlers = new Map<string, KeyHandler>()

/**
 * 注册一个 act 的 handler。
 *
 * 表外 act 与重复注册都抛错——这两条是「键位即承诺」的守门：
 * 前者让实现不能偷偷加用户查不到的键，后者让「两个页面各注册一个
 * `dock:open`、行为取决于加载顺序」不可能发生。
 */
export function register(act: string, fn: KeyHandler): void {
  if (!ACTS.has(act)) {
    throw new Error(
      `keynav: act "${act}" 不在 KEYS 表里。\n` +
        '  先把它加进 keynav.ts 的 KEYS（用户要能在 `?` 里看到它），' +
        '  再注册 handler。反过来不许：不许有用户找不到的隐藏键位。',
    )
  }
  if (handlers.has(act)) {
    throw new Error(`keynav: act "${act}" 被注册了两次 —— 一个动作只能有一个主人`)
  }
  handlers.set(act, fn)
}

/**
 * 内部注册：Esc / Tab / 焦点栈是本模块与 `focus.ts` 的职责，
 * 它们同样要在 `?` 里查得到，所以走的是同一张表，只是不做重复检查
 * （模块加载时只注册一次，重复调用是 bug 而不是重复注册）。
 *
 * **不许出现第三种路径**——「表里有 act、监听器里直接处理」正是上一版
 * 15 个死键的根因。
 */
export function registerInternal(act: string, fn: KeyHandler): void {
  handlers.set(act, fn)
}

/** 是否已有 handler。 */
export function hasHandler(act: string): boolean {
  return handlers.has(act)
}

/** 表里有、但没人实现的 act。`?` 面板与控制台报警共用它。 */
export function unimplemented(): string[] {
  return [...ACTS.keys()].filter((a) => !handlers.has(a))
}

/**
 * 派发一个 act。
 *
 * handler 返回 `true` 才 `preventDefault`：「不吞」很重要，它让没实现的
 * 动作把键还给浏览器，而不是「按了没反应但键也被抢走」。
 */
export function dispatch(act: string, e: KeyboardEvent, data?: unknown): boolean {
  const fn = handlers.get(act)
  if (!fn) return false
  const consumed = fn(e, data) === true
  if (consumed && e.cancelable) e.preventDefault()
  return consumed
}

/**
 * 单键在输入区内一律不抢——「键盘优先」与「能打字」的分界。
 * 修饰键（⌃B 等）在输入区内**照常**生效，因为用户可能在终端里按它。
 */
export function isTyping(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null
  if (!el || typeof el.tagName !== 'string') return false
  const tag = el.tagName.toUpperCase()
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el.isContentEditable === true
}

/**
 * 一个元素是否处在「按钮语义」上。
 *
 * Space 在按钮 / 链接聚焦时是「按下按钮」，不能抢——
 * 否则键盘用户按 Space 想激活按钮，却触发了树的展开。
 */
export function isActivatable(el: EventTarget | null): boolean {
  const node = el as HTMLElement | null
  if (!node || typeof node.tagName !== 'string') return false
  const tag = node.tagName.toUpperCase()
  return tag === 'BUTTON' || tag === 'A' || node.isContentEditable === true
}

/** 键位表按扁平形状展开——`?` 面板与测试都按这个形状消费。 */
export type FlatKey = ReadonlyArray<{
  keys: string[]
  act: string
  label: string
  scope: string
}>

/** 扁平化键位表（表序、分组序不变）。 */
export function flatKeys(): FlatKey {
  return KEYS.flatMap((g) => g.keys)
}

/** scope 的中文标签，供 `?` 面板标注（非 global 才显示）。 */
export const SCOPE_LABEL: Readonly<Record<KeyScope, string>> = {
  global: '全局',
  list: '列表',
  session: '会话工作台',
  panel: '面板',
}