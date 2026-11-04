/* ===========================================================================
   keynav.ts —— 键位表的唯一真相源（规范 §4.2）
   ---------------------------------------------------------------------------
   `KEYS` 是全站键位的唯一真相源，`?` 帮助面板由它渲染，所以「文档写的键位」
   与「按下会发生的键位」不可能漂移。三条配套约束：

     ① `register` 拒绝表外 act —— 一个 `?` 里查不到的键位，对键盘用户等于不存在
     ② `register` 拒绝重复注册 —— 一个动作只能有一个主人，否则行为取决于
        加载顺序且没有任何提示
     ③ `unimplemented(act)` 回答「这个 act 有没有实现」——`?` 面板据此标注
        「（未实现）」，而不是等用户按到死键

   ── 实现可以不住在 `handlers` 表里 ────────────────────────────────
   `list:*` 由 `shared/listbox.ts` 的容器级监听器处理、`focus:*` 由
   `shared/focus.ts` 的 `trapTab` 处理、`file:*` 由 `domains/session/FilesPanel.vue`
   的根元素监听器处理——它们都不适合进全局表（一个页面可能同时持有多个
   文件面板，而 `register` 拒绝重复注册）。这类 act 由实现处调
   `declareImplemented(act)` 声明，于是键位表仍然是「承诺」的唯一出处。

   ── 只有一份键位表 ──────────────────────────────────────────────
   这张表是**唯一**的键位真相源：`?` 帮助面板由 `KEYS` 渲染、`register`
   只认表内 act、`docs/design-system.md` §4.2 由 `keynav.test.ts` grep 对齐。

   历史上 `prototype/assets/keynav.js` 里还有一张手抄的副本（原型是纯静态
   HTML，引不了 TS 与 SFC，只能抄），于是两边会漂、也确实漂过。**原型已整体
   删除（2026-11-03）**，那个副本和盯着它漂移的测试组一起消失了。不要再引入
   第二份键位表：一份键位写两遍，报错时机会从「构建期抛错」退化成「用户按下去
   没反应」。
   =========================================================================== */

import { pageTitle } from './page-titles'

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
 * 而 `src/shared/keynav.test.ts` 会把条目数与 `docs/design-system.md` §4.2
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
      { keys: ['g', 's'], act: 'nav:sessions', label: pageTitle('sessions'), scope: 'global' },
      { keys: ['g', 'd'], act: 'nav:devices', label: pageTitle('devices'), scope: 'global' },
      { keys: ['g', ','], act: 'nav:settings', label: pageTitle('settings'), scope: 'global' },
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
      /*
        键位**没有改**，改的是语义（v0.5.0 裁决 C：右侧只有一格）。
        label 必须跟着改：`?` 面板是用户查得到的承诺，而按 `]` 现在会
        **替换**当前工具并**释放**它的通道——对终端就是杀掉正在跑的 shell。
        改实现不改文案，`?` 会继续承诺「切换到下一个面板」，而实际发生的是
        替换与释放，用户完全无从预料（这是本里程碑唯一的已知降级）。
      */
      { keys: ['o'], act: 'panel:open', label: '聚焦右侧那一格（没有则打开 Git）', scope: 'session' },
      {
        keys: ['['],
        act: 'panel:prev',
        label: '换成上一个工具（当前工具会被替换，通道释放：终端会被杀掉）',
        scope: 'session',
      },
      {
        keys: [']'],
        act: 'panel:next',
        label: '换成下一个工具（当前工具会被替换，通道释放：终端会被杀掉）',
        scope: 'session',
      },
      { keys: ['\\'], act: 'dock:toggle', label: '打开 / 收起右侧那一格', scope: 'session' },
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
 * 合法。
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

/**
 * 键位描述 → act 的反查表。
 *
 * 与 `ACTS` 走的是**同一份 `KEYS`**，但方向相反：`ACTS` 是 act → 定义，
 * 这张是「用户按了哪个键」→ 「该做什么」。页面据此判断「我按的这个键在
 * 表里属于哪个 act、那个 act 归不归我管」——会话工作台据此只认 `scope:
 * 'session'` 的那几条，而不必再抄一份反向表。
 */
const ACT_BY_KEY: ReadonlyMap<string, string> = (() => {
  const out = new Map<string, string>()
  for (const g of KEYS) {
    for (const k of g.keys) {
      const desc = k.keys.join(' ')
      if (!out.has(desc)) out.set(desc, k.act)
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
 * 一个键位描述对应哪个 act。不在表里返回 `undefined`。
 *
 * 描述的形状是 `keys.join(' ')`：单键是 `'o'`，`g` 序列是 `'g s'`。
 */
export function actForKey(key: string): string | undefined {
  return ACT_BY_KEY.get(key)
}

/**
 * 一个 act 的键位提示（侧栏角标、状态条 `<kbd>` 用）。
 *
 * 从表里读，不另抄一份：抄一份的话，改了 `KEYS` 的键位而忘了改提示，
 * 界面上就会同时出现两个「g s」。
 */
export function hintOf(act: string): string {
  const b = bindingOf(act)
  return b ? b.keys.join(' ') : ''
}

/**
 * `g` 序列的前缀键。
 *
 * 单独写出来而不是散在各处：状态条要显示「`<kbd>g</kbd> 跳转」，
 * `shell-keys.ts` 要判「用户是不是在按前缀」——两处说的是同一个键。
 */
export const PREFIX_LEAD = 'g'

/**
 * `g` 序列的目标：按下 `g` 后的第二个键决定去向。
 *
 * **从 `KEYS` 推导**而不是另抄一张表：抄的那张表与 `KEYS` 漂了时，
 * `g s` 会跳到一个 `?` 面板里查不到的 act，而 `?` 面板显示的键位仍然
 * 是对的——用户按了没反应，却查不出为什么。
 *
 * 800ms 窗口内没配上**不吞键**——用户只是在打一段以 `g` 开头的字。
 */
export const PREFIX: ReadonlyMap<string, string> = (() => {
  const out = new Map<string, string>()
  for (const g of KEYS) {
    for (const k of g.keys) {
      if (k.keys.length !== 2 || k.keys[0] !== PREFIX_LEAD) continue
      const second = k.keys[1] as string
      if (!out.has(second)) out.set(second, k.act)
    }
  }
  return out
})()

/** `g` 序列的窗口。 */
export const PREFIX_MS = 800

/** handler 返回 `true` 表示吞掉这个键；`false` / `undefined` 表示还给浏览器。 */
export type KeyHandler = (e: KeyboardEvent, data?: unknown) => boolean | undefined

const handlers = new Map<string, KeyHandler>()

/**
 * 不在全局表里的实现：`list:*` / `focus:*` / `file:*`。
 *
 * 这些 act 由各自的模块处理（容器级监听器 / 焦点栈 / 面板根元素），
 * 放进全局表会让「谁抢到这个键」取决于挂载顺序。声明之后，`?` 面板
 * 就不再把它们标成「（未实现）」。
 */
const declared = new Set<string>()

/**
 * handler 表变化的版本号 + 订阅者。
 *
 * 「表里有、谁实现了」是一个**模块级可变状态**，而 `?` 面板要按它重画。
 * 页面侧靠 `onHandlersChanged` 订阅，把这个变化接到自己的响应式状态上——
 * 否则 `computed` 会在第一次求值后把结论冻住（注册发生在挂载期，晚于首屏渲染）。
 */
const handlerListeners = new Set<() => void>()

function bumpHandlers(): void {
  for (const fn of handlerListeners) fn()
}

/**
 * 订阅「handler 表变了」。返回退订函数。
 *
 * 存在的理由：`register` 发生在组件挂载期，而首屏渲染早于它——不订阅的话，
 * `?` 面板会永远停在首屏那一刻的结论上（那时一个 handler 都还没注册）。
 */
export function onHandlersChanged(fn: () => void): () => void {
  handlerListeners.add(fn)
  return () => {
    handlerListeners.delete(fn)
  }
}

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
  bumpHandlers()
}

/**
 * 撤销一个 act 的 handler。没有注册过时是空操作。
 *
 * 存在的唯一理由是**页面级键位随页面挂载 / 卸载**：会话工作台的
 * `panel:open` / `dock:toggle` 在离开工作台后必须失效，否则用户回到设备页
 * 按 `o` 会打开一个已经不存在的面板。`register` 拒绝重复注册，所以没有
 * 对应的 `unregister` 时，第二个工作台实例（测试里、或同一个页面重挂载）
 * 会直接在挂载期抛错。
 *
 * 外壳级键位（`?` / `g s` / `⌃B`）**不**走这条路：它们由
 * `shell-keys.ts` 注册一次、永远在，靠回调槽位跟随当前组件。
 */
export function unregister(act: string): void {
  handlers.delete(act)
  bumpHandlers()
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
  bumpHandlers()
}

/**
 * 声明「这个 act 已实现，但实现不住在全局 handler 表里」。
 *
 * 用它的地方：`list:*`（容器级监听器）、`focus:*`（焦点栈的 `trapTab`）、
 * `file:*`（文件面板根元素）。它们不进 `register` 是因为全局表只允许一个
 * 主人，而一个页面可能同时持有多个文件面板。
 *
 * 表外 act 抛错：声明一个用户查不到的 act，等于在表外偷偷加了一个承诺。
 */
export function declareImplemented(act: string): void {
  if (!ACTS.has(act)) {
    throw new Error(
      `keynav: 声明的 act "${act}" 不在 KEYS 表里。` +
        '  先把它加进 keynav.ts 的 KEYS，再声明实现。',
    )
  }
  if (declared.has(act)) return
  declared.add(act)
  bumpHandlers()
}

/** 是否已有 handler。 */
export function hasHandler(act: string): boolean {
  return handlers.has(act)
}

/**
 * 这个 act 是不是**没有**实现。
 *
 * 签名按规格：`unimplemented(act: string)`——「这个 act 实现了吗」是一个
 * 可以逐条问的问题，而「返回全部未实现 act 的数组」是另一个问题
 * （`missingActs()`）。合成一个函数会让调用方自己写
 * `list.includes(act)`——那正是易错的那一步。
 */
export function unimplemented(act: string): boolean {
  return !handlers.has(act) && !declared.has(act)
}

/** 表里有、但没人实现的 act 清单。`?` 面板与控制台报警共用它。 */
export function missingActs(): string[] {
  return [...ACTS.keys()].filter((a) => unimplemented(a))
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

/**
 * 键位表按扁平形状展开。
 *
 * **只有测试按这个形状消费**：`?` 面板（`App.vue`）直接按组渲染 `KEYS`
 * （`v-for="g in KEYS"` → `v-for="k in g.keys"`），它要的就是分组结构，
 * 不需要扁平表。扁平表是为了测试能一条条走过去而不必先按组展开。
 */
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
