/* ===========================================================================
   tabs.ts —— 多会话 tab 的状态机（规范 §5.5）
   ---------------------------------------------------------------------------
   这里只放**可序列化**的那一半：有哪些 tab、哪个是激活的、关掉一个之后
   该激活谁。面板、通道、终端输出这些活在内存里，挂在 `SessionWorkspace`
   对象上（`SessionPage.vue`），不进 sessionStorage。

   ── 为什么关 tab 的落点要在这里算 ─────────────────────────────────
   §5.5.5 记的「索引必须在过滤前取」是本项目真正踩过的 bug：先过滤再
   `indexOf` 得到 `-1`，于是「该激活哪一个」永远算错，而症状只在
   「关掉最后一个 tab」时表现为跳错会话。所以这条规则写成一个**纯函数**，
   有单测盯着，不散在组件的点击处理里。

   ── 存储格式 ──────────────────────────────────────────────────────
   只存 `id / title / path / nodeId`。路径是会话的属性，tab 标题由它派生
   （后端没有 title 字段），存一份标题是为了刷新后 tab 条不必等会话列表
   回来才画得出来。
   =========================================================================== */

/** 一条 tab 的元信息。**可序列化**，会进 sessionStorage。 */
export interface TabMeta {
  /** 会话 id（`/sessions/:id` 的那一段）。 */
  id: string
  /** 显示名：工作区路径的最后一段。 */
  title: string
  /** 工作区路径（只读）。 */
  path: string
  /**
   * 归属节点，面板开通道时 Hub 自己从会话查，这里只用于显示。
   *
   * `null` = 这台设备已被删除。**不回落成空串**：空串看起来像一个 node_id
   * （只是短了点），会让「设备已移除」这一态在类型里消失，模板里所有
   * 「万一没有值」的分支都不会被发现。
   */
  nodeId: string | null
  /**
   * 会话起点（完整 40 位 hash）。`null` = 没有，原因看 `baseRevStatus`。
   *
   * **随 tab 持久化**：刷新页面时 `GET /api/sessions` 还会再给一遍，但那条
   * 请求可能失败或慢；先把已知的值留在 tab 上，「起点」这一行就不会在刷新的
   * 一瞬间闪成「未记录」——那会让人以为数据丢了。
   */
  baseRev: string | null
  /**
   * 起点为什么有／没有（Hub 的五态）。
   *
   * **缺字段的老数据读成 `'unknown'`**（见 `isTabMeta`）：v0.6.0 之前写进
   * localStorage 的 tab 没有这两个键，而那时也确实没有记录起点——`unknown`
   * 描述的就是这个事实，不是「解析失败随便填一个」。
   */
  baseRevStatus: 'ok' | 'not_a_repo' | 'no_head' | 'unavailable' | 'unknown'
}

export interface TabsState {
  tabs: TabMeta[]
  activeId: string | null
}

export const EMPTY_TABS: TabsState = { tabs: [], activeId: null }

/** 打开（或激活）一个 tab。已存在时**保留原位置**，只切激活。 */
export function openTab(state: TabsState, meta: TabMeta): TabsState {
  const existing = state.tabs.find((t) => t.id === meta.id)
  if (existing) {
    // 刷新一次元信息：设备名 / 路径之外的东西（标题）可能变了
    return {
      tabs: state.tabs.map((t) => (t.id === meta.id ? { ...t, ...meta } : t)),
      activeId: meta.id,
    }
  }
  return { tabs: [...state.tabs, meta], activeId: meta.id }
}

/** 激活一个存在的 tab。不存在的 id 不改变状态（深链接到一个已关掉的会话时用）。 */
export function activateTab(state: TabsState, id: string): TabsState {
  if (!state.tabs.some((t) => t.id === id)) return state
  return { ...state, activeId: id }
}

/** 关掉一个 tab 的结果。`nextActive` 为 `null` 表示一个都不剩。 */
export interface CloseResult {
  state: TabsState
  nextActive: string | null
}

/**
 * 关掉一个 tab，并算出关掉之后该激活谁。
 *
 * **索引在过滤前取**（§5.5.5）：关掉当前 tab → 原位置的右邻；关掉最后一个
 * → 左邻；关掉的不是当前 tab → 激活者不变。
 */
export function closeTab(state: TabsState, id: string): CloseResult {
  const idx = state.tabs.findIndex((t) => t.id === id)
  if (idx < 0) return { state, nextActive: state.activeId }

  const rest = state.tabs.filter((t) => t.id !== id)
  if (state.activeId !== id) {
    // 关的不是正在看的那个：激活者不变，也不会跳
    return { state: { tabs: rest, activeId: state.activeId }, nextActive: state.activeId }
  }
  const neighbor = rest[idx] ?? rest[idx - 1] ?? null
  const nextActive = neighbor ? neighbor.id : null
  return { state: { tabs: rest, activeId: nextActive }, nextActive }
}

/** `←` / `→` / `Home` / `End` 的目标。到头不绕回（tab 条不是转盘）。 */
export function neighborId(state: TabsState, id: string, key: 'prev' | 'next' | 'first' | 'last'): string | null {
  const idx = state.tabs.findIndex((t) => t.id === id)
  if (idx < 0) return null
  switch (key) {
    case 'first':
      return (state.tabs[0] as TabMeta).id
    case 'last':
      return (state.tabs[state.tabs.length - 1] as TabMeta).id
    case 'prev':
      return idx > 0 ? (state.tabs[idx - 1] as TabMeta).id : id
    case 'next':
      return idx < state.tabs.length - 1 ? (state.tabs[idx + 1] as TabMeta).id : id
  }
}

/** sessionStorage 的键。`agf.` 前缀避免与别的页面脚本撞名。 */
export const STORAGE_KEY = 'agf.session-tabs'

/** 读回上次的 tab 集合。坏数据一律当作「没有」——它只是视图状态。 */
export function loadTabs(storage: Pick<Storage, 'getItem'> | null | undefined, fallback: TabMeta[] = []): TabsState {
  if (!storage) return { tabs: fallback, activeId: fallback[0]?.id ?? null }
  const raw = storage.getItem(STORAGE_KEY)
  if (!raw) return { tabs: fallback, activeId: fallback[0]?.id ?? null }
  try {
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return { tabs: fallback, activeId: fallback[0]?.id ?? null }
    const tabs = parsed
      .map(normalizeTabMeta)
      .filter((t): t is TabMeta => t !== null)
    if (!tabs.length) return { tabs: fallback, activeId: fallback[0]?.id ?? null }
    return { tabs, activeId: tabs[0]?.id ?? null }
  } catch {
    // 解析失败不是错误：它说明上次写坏了，而视图状态坏了就该重来
    return { tabs: fallback, activeId: fallback[0]?.id ?? null }
  }
}

/** 写回 tab 集合。写失败（隐私模式配额满）静默降级——不影响本次会话。 */
export function saveTabs(storage: Pick<Storage, 'setItem'> | null | undefined, state: TabsState): void {
  if (!storage) return
  try {
    storage.setItem(STORAGE_KEY, JSON.stringify(state.tabs))
  } catch {
    // 存不下就只活在这一个页面里：比因为配额错误整页崩掉好
  }
}

function isTabMeta(v: unknown): v is TabMeta {
  if (typeof v !== 'object' || v === null) return false
  const t = v as Record<string, unknown>
  return (
    typeof t.id === 'string' &&
    typeof t.title === 'string' &&
    typeof t.path === 'string' &&
    typeof t.nodeId === 'string'
  )
}

/**
 * 把 localStorage 里读到的原始对象补齐成 [`TabMeta`]。
 *
 * **为什么不放进 `isTabMeta` 的类型谓词里**：类型谓词只能回答「是不是」，
 * 不能「改一改再说是」。v0.6.0 新增的 `baseRev` / `baseRevStatus` 在**老数据
 * 里根本不存在**，而老数据的其余字段全部合法——让它进不了类型谓词就等于
 * 「升级一次，所有 tab 静默消失」。
 *
 * 缺 `baseRevStatus` 时填 `'unknown'`（不是 `'ok'`）：那时的会话确实没有
 * 记录起点。其余形状不对的值一律归 `'unknown'`，同一条理由。
 */
export function normalizeTabMeta(v: unknown): TabMeta | null {
  if (!isTabMeta(v)) return null
  // `isTabMeta` 是类型谓词，编译后不改变变量的静态类型；这里要先按
  // `unknown` 再按 `Record` 取可选的原始字段，否则 TS 认为 `TabMeta`
  // 与索引签名不重叠。
  const t = v as unknown as Record<string, unknown>
  const rawStatus = t.baseRevStatus
  const baseRevStatus: TabMeta['baseRevStatus'] =
    rawStatus === 'ok' ||
    rawStatus === 'not_a_repo' ||
    rawStatus === 'no_head' ||
    rawStatus === 'unavailable' ||
    rawStatus === 'unknown'
      ? rawStatus
      : 'unknown'
  return {
    id: t.id as string,
    title: t.title as string,
    path: t.path as string,
    nodeId: t.nodeId as string,
    baseRev: typeof t.baseRev === 'string' ? t.baseRev : null,
    baseRevStatus,
  }
}
