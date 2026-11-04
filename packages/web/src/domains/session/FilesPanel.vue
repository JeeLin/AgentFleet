<script lang="ts">
/* ===========================================================================
   非 setup 块：`file:*` 的实现声明。
   ---------------------------------------------------------------------------
   `⌃S`（保存）与 `Backspace`（上一级）是 `scope: 'panel'` 的两个键，它们由
   本组件根元素上的局部监听器处理，**不进 keynav 的全局 handler 表**：
   `register` 拒绝重复注册，而一个宿主可能同时持有多个文件面板（N 个 tab），
   组件级注册等于把「谁抢到 `file:save`」变成挂载顺序问题。

   声明放在**模块加载时**（而不是 `setup` 里）：`?` 面板读的是「表里有、
   谁实现了」，而组件可能根本没被挂载过——声明是「这个 act 有实现」这件事
   的事实，与有没有实例无关。
   =========================================================================== */

import { declareImplemented } from '@/shared/keynav'

declareImplemented('file:save')
declareImplemented('file:up')
</script>

<script setup lang="ts">
/* ===========================================================================
   FilesPanel —— 文件面板：树 + 面包屑 + 预览（规范 §5.4 / §3.14 / §7.4）
   ---------------------------------------------------------------------------
   面板里只有**一棵树的读法**：根是会话的工作区路径（`root` prop），没有第二
   个根，也没有「切换工作区」的控件（§6.2：工作区是会话的字段，不是路由）。

   ── 这个组件不知道通道 ──────────────────────────────────────────────
   它**不 import `channel.ts`，也不发任何请求**。数据全由宿主从 props 喂进来，
   意图全通过 emits 报上去。理由：通道的生命周期（懒建立、关面板才关、切 tab
   不关）是**页面**的事，面板若自己持有一条 `ChannelHandle`，就会出现「面板
   以为通道活着、页面已经把它关了」的第二份真相（规范 §5.5.1）。

   于是本文件在真 files connector（子任务 9）到位前后**形状不变**：宿主现在
   喂 mock，将来喂 `channel.frame` 解出来的载荷。

   ── 数据是「一层一层」的，不是「一整棵树」 ──────────────────────────
   files connector 的 `list` 是**一次一层**（`{"op":"list","path":"src"}` →
   该层 entries；`crates/node-core/src/connector/files.rs`）。没有「递归返回
   整棵树」这种 op。所以这里持有的是 `Map<相对目录路径, 该层条目>`：列过的
   层才有一层的行；没列过的目录只有那一行本身（`aria-expanded=false`，没有
   子行）。展开一层 = emit `list` → 宿主去取 → 条目回来后行出现。这也顺带
   避开了「先把整棵树拉下来」在一个大工作区上卡死。

   ── 键盘：listbox.ts，容器是唯一 Tab 落点 ─────────────────────────
   ↑↓ / Home / End / ⏎ 全部由 `listbox.ts` 提供（`onEnter` 钩子），行是
   `<button tabindex="-1">`（§3.8：用 div 会让 Tab 根本进不去那棵树）。

   `⌃S`（保存）与 `Backspace`（上一级）是 §4.2 里 `scope: 'panel'` 的两个键。
   它们**挂在本组件的根元素上，不进 keynav 的全局注册表**：`register` 对重复
   注册抛错，而一个宿主可能同时持有多个文件面板（N 个 tab），组件级注册等于
   把「谁抢到 `file:save`」变成挂载顺序问题。panel 域的键本来就只在焦点位于
   本面板时有意义——局部监听器的作用域恰好就是这个域，不需要全局表参与。
   **宿主若改为在 SessionPage 里注册 `file:save` / `file:up`，请删掉
   `onKeydown`，否则会存两次。**

   **Space 是本组件自己接的（唯一一处偏离 listbox，注释在下）**：
   `listbox.ts` 的 `toggle()` 见到行上有 `aria-expanded` 就**自己翻 DOM 属性
   并切 `[data-kids]` 的 `hidden`**，然后 `return true`——它假设「展开」是纯
   DOM 层的嵌套结构。本组件的展开是**数据驱动**的（展开要 emit `list` 去设备
   上取一层），DOM 里没有 `[data-kids]`，交给它翻就等于「属性翻了、行没出来」。
   所以 Space 在捕获阶段被本组件接走，`stopPropagation` 让它到不了 listbox。

   ── 越界是四路证据，不是一句报错（§7.4） ─────────────────────────
   ① 树上：越界节点**照常显示**（斜纹 + 删除线 + ⊘），不隐藏；
   ② 面包屑：越界的那一层标红；
   ③ 导航越界：解释性错误块，给「为什么」+「怎么才能进去」；
   ④ 写越界：与③分开的另一块（`writeError`），编辑框 `aria-invalid`。
   ③ 与 ④ 分开是因为下一步不同：③ 的下一步是「别进去」，④ 的下一步是
   「改路径 / 改会话」。

   每条越界文案都带一句「这条边界只管页面用户」：agent 用它自己装在设备上的
   工具，不经过 files connector（§7.4 的关键补充）。不写这句，用户会以为
   「打开工作区就能把 agent 也限制住」——那是本项目反复强调不要搞混的两条
   独立路径（约束 3）。

   ── 预览必须给结论（§3.14） ────────────────────────────────────────
   `kind` 由 node 侧判定后随载荷回来（`text` / `binary` / `truncated`）。
   组件**不嗅探内容**：同一个文件两端得出不同结论比不预览更糟。

   ── 脏标记 ─────────────────────────────────────────────────────────
   编辑区内容 ≠ 打开时的内容 → `dirty`，emit `dirty-change`。组件**不自己
   持久化**：「切走再回来还在」与「关 tab 会丢未保存改动」都是宿主那一层的
   事实（§5.5.2），面板只负责把「有未保存改动」这件事报上去。

   ── 大文件档不可编辑 ───────────────────────────────────────────────
   截断档（> 1 MB）只读前后各 4 KB。若让它可编辑，存回去就是用这两段覆盖
   整个文件——那是一次静默的数据损坏。所以那一档明写「编辑整份请让 agent 做，
   浏览器不是 IDE」。
   =========================================================================== */

import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import { cwdPathOf, isOutsideSeg, splitPath } from '@/shared/files-path'
import { orDash } from '@/shared/format'
import { isTyping } from '@/shared/keynav'
import { attach, type ListboxHandle } from '@/shared/listbox'

/* ---------------------------------------------------------------- 数据契约 */

export type FileKind = 'dir' | 'file' | 'link' | 'other' | 'unknown'

/** files connector `list` 回的一项（`node-core/src/connector/files.rs`）。 */
export interface FileEntry {
  name: string
  kind: FileKind
  /** 字节。目录恒为 0；**帧里没给是 `null`**（0 与「不知道」是两件事）。 */
  size: number | null
  /** Unix 毫秒；取不到是 `null`（时钟回拨等情形），不是 0。 */
  mtime_ms: number | null
  /** 仅 `kind === 'link'`。读不到目标时 `null`。 */
  target?: string | null
}

/** `list` 的回帧。一次一层。 */
export interface FileListing {
  /** 这一层的**相对**路径。`''` = 工作区根。 */
  path: string
  entries: FileEntry[]
  /** 目录里的实际条目数。`total > entries.length` 即被截断；帧里没给是 `null`。 */
  total: number | null
  truncated: boolean
}

/** `read` 的回帧。三档结论由 `kind` 给出。 */
export interface FilePreview {
  path: string
  /** 文件总字节（截断时大于 `text` 的实际长度）；帧里没给是 `null`。 */
  size: number | null
  kind: 'text' | 'binary' | 'truncated'
  truncated: boolean
  /** `kind === 'text'` 时给。二进制不强行转 UTF-8。 */
  text?: string
  /** 总是给（base64）。M1 的预览不解码它——二进制档只显示类型与大小。 */
  content_b64?: string
}

/** 越界 / 失败。③（导航）与④（写）是两个字段，不说混。 */
export interface FilesError {
  /** 用户自己写的那个写法（`..` / `/etc` / 链接目标），**不是**解析后的绝对路径。 */
  path: string
  /** 为什么进不去 / 为什么没写成。 */
  reason: string
  /**
   * 机器码。页面只用它做一件事：把「越界」与「别的失败」分开渲染——
   * 越界给边界解释（§7.4 ③），别的失败给原始原因。
   */
  code?: string | null
}

const props = withDefaults(
  defineProps<{
    /** 会话工作区路径——**树的唯一根**，显示用（面包屑第一层）。 */
    root: string
    /** 当前目录，相对根。每一项是一层目录名。 */
    cwd: readonly string[]
    /** 已列过的层：`相对路径 → 该层条目`。没列过的层没有键。 */
    listings: ReadonlyMap<string, FileListing>
    /** 正在列的层（相对路径）。 */
    loading?: ReadonlySet<string>
    /** 当前打开的文件预览。没有打开文件时 `null`。 */
    preview?: FilePreview | null
    /** 正在读的文件（相对路径）。 */
    readingPath?: string | null
    /** 导航越界 / 读失败（③）。 */
    error?: FilesError | null
    /** 写越界 / 写失败（④）。与③分开：下一步不同。 */
    writeError?: FilesError | null
    /** 写操作进行中。 */
    saving?: boolean
    /** 会话已结束：工作区不再可达，面板只读。 */
    ended?: boolean
    /** 通道建好了吗。没建好时不能把空树说成「目录是空的」。 */
    channelReady?: boolean
  }>(),
  {
    loading: () => new Set<string>(),
    preview: null,
    readingPath: null,
    error: null,
    writeError: null,
    saving: false,
    ended: false,
    channelReady: false,
  },
)

const emit = defineEmits<{
  /** 打开一个节点（越界节点也会报：点了要给解释，§7.4 ③）。 */
  (e: 'open', path: string): void
  /** 列一层（展开目录）。 */
  (e: 'list', path: string): void
  /** 面包屑：回到第 `depth` 层（`0` = 根）。 */
  (e: 'goto', depth: number): void
  /** 读一个文件。 */
  (e: 'read', path: string): void
  /** 保存。 */
  (e: 'save', payload: { path: string; text: string }): void
  /** 撤销编辑，回到打开时的内容。 */
  (e: 'revert', path: string): void
  /** 脏状态变化（tab 上的脏标记由宿主挂）。 */
  (e: 'dirty-change', dirty: boolean): void
  /** 关掉越界 / 读失败块。 */
  (e: 'dismiss-error'): void
  /** 关掉写失败块。 */
  (e: 'dismiss-write-error'): void
}>()

/* ---------------------------------------------------------------- 路径 */

/** 面包屑的一层。 */
interface Crumb {
  label: string
  /** 这一层的相对路径（根是 `''`）。 */
  path: string
  /** 点它要去的深度（根 = 0）。 */
  depth: number
  /** 越界的层标红（§7.4 ②）。 */
  outside: boolean
}

/**
 * 相对路径的拼接。**空段不进栈**：`['a', '', 'b']` 会拼出 `a//b`，而那一层
 * 列不出来（node 侧按 `a//b` 去找是另一个目录）。
 *
 * 切分规则来自 `lib/files-path.ts`——页面与面板各写一份 `split('/')` 时，
 * 同一路径会在一处算出 2 段、另一处算出 1 段，面包屑与树高亮对不上。
 */
function joinPath(base: string, name: string): string {
  const parts = splitPath(base)
  if (name.length > 0) parts.push(name)
  return parts.join('/')
}

/** 当前目录的相对路径。 */
const cwdPath = computed(() => cwdPathOf(props.cwd))

/** 当前层的清单。**没列过是「还没问过设备」，不是「空目录」——渲染不同。 */
const listing = computed<FileListing | null>(() => props.listings.get(cwdPath.value) ?? null)
const listingLoading = computed(() => props.loading.has(cwdPath.value))

const crumbs = computed<Crumb[]>(() => {
  const out: Crumb[] = [{ label: props.root, path: '', depth: 0, outside: false }]
  let path = ''
  props.cwd.forEach((seg, i) => {
    path = joinPath(path, seg)
    out.push({ label: seg, path, depth: i + 1, outside: isOutsideSeg(seg) })
  })
  return out
})

/* ---------------------------------------------------------------- 树 */

/** 树的一行。渲染出的形状与 `listbox.ts` 的约定一致。 */
interface Row {
  /** 相对根的路径（含它自己的名字）。 */
  path: string
  name: string
  kind: FileKind
  /** 目录行：展开了吗。 */
  expanded: boolean
  size: number | null
  target: string | null
  /** 越界节点照常显示，标记它（§7.4 ①）。 */
  outside: boolean
  depth: number
  /**
   * meta 列的文案（越界给「不可进入」，链接给目标，文件给大小）。
   *
   * **算一次存下来**：模板里 `v-if="metaOf(row)"` 与 `{{ metaOf(row) }}`
   * 各调一次，而 `metaOf` 内部要调 `humanBytes` 组装字符串——每帧对可见行
   * 做两遍同样的组装。
   */
  meta: string
}

/** 已展开的目录（相对路径集合）。折叠**不**清已列的条目——再展开不用重取。 */
const expanded = ref<Set<string>>(new Set())

/**
 * 可见的行：当前层的条目 + 已展开子目录里**已列过**的条目。
 *
 * 没列过的目录没有子行——那不是「空目录」，是「还没问过设备」。
 */
const rows = computed<Row[]>(() => {
  const out: Row[] = []
  const seen = new Set<string>()

  function walk(path: string, depth: number): void {
    // `seen` 兜住条目里的环：一个目录把自己列进去会让递归永不结束，
    // 而这种形状在设备侧是可能构造出来的（绑定挂载、链接环）。
    if (seen.has(path)) return
    seen.add(path)
    const layer = props.listings.get(path)
    if (!layer) return
    for (const entry of layer.entries) {
      const child = joinPath(path, entry.name)
      const isDir = entry.kind === 'dir'
      const outside = isOutsideSeg(entry.name)
      out.push({
        path: child,
        name: entry.name,
        kind: entry.kind,
        expanded: isDir && expanded.value.has(child),
        size: entry.size,
        target: entry.target ?? null,
        outside,
        depth,
        meta: metaOf(entry, outside),
      })
      if (isDir && expanded.value.has(child)) walk(child, depth + 1)
    }
  }

  walk(cwdPath.value, 0)
  return out
})

/** 行的缩进：12px 基准 + 每层 14px（原型 `files.html` 的取值）。 */
function indentOf(depth: number): string {
  return `${12 + depth * 14}px`
}

/**
 * 每一档的说法。`other` 与 `unknown` 由 node 侧真的产出：前者是管道 / socket
 * 这类既非文件也非目录的东西，后者是列目录期间元数据读不到（竞态删除）。
 * 两档都不当文件、也不当目录——猜错会让用户去点一个不存在的东西。
 */
const KIND_LABEL: Record<FileKind, string> = {
  dir: '目录',
  file: '文件',
  link: '符号链接',
  other: '其他',
  unknown: '读不到类型',
}

function twistyOf(row: Row): string {
  if (row.kind === 'dir') return row.expanded ? '▾' : '▸'
  if (row.kind === 'link') return '→'
  return '·'
}

function ariaLabelOf(row: Row): string {
  const bits = [row.name, KIND_LABEL[row.kind]]
  if (row.kind === 'file') bits.push(humanBytes(row.size))
  if (row.outside) bits.push('工作区之外，不可进入')
  if (row.kind === 'link' && row.target) bits.push(`指向 ${row.target}`)
  if (row.kind === 'unknown') bits.push('列目录时已读不到它的元数据')
  return bits.join('，')
}

/** 行的 meta 列：越界给「不可进入」，链接给目标，文件给大小。 */
function metaOf(entry: FileEntry, outside: boolean): string {
  if (outside) return '不可进入'
  if (entry.kind === 'link') return entry.target ? `→ ${entry.target}` : '→ ?'
  if (entry.kind === 'file') return humanBytes(entry.size)
  if (entry.kind === 'unknown') return '类型未知'
  return ''
}

/* ---------------------------------------------------------------- 键盘 */

const treeEl = ref<HTMLElement | null>(null)
const hostEl = ref<HTMLElement | null>(null)
let handle: ListboxHandle | null = null

onMounted(() => {
  hostEl.value?.addEventListener('keydown', onKeydown, true)
  const el = treeEl.value
  if (!el) return
  handle = attach(el, {
    role: 'tree',
    // ↑↓ 到头不绕回：树不是转盘，绕回会让「到底了」这件事看不出来
    wrap: false,
    // Enter 归行：行本身就是那个控件（行内没有要点的子按钮）
    rowEnter: true,
    onEnter: (row) => {
      const path = row.dataset.path ?? ''
      if (!path) return false
      if (row.dataset.outside === 'true') {
        // 越界节点**可点**，点了给解释：隐藏它比显示它更让人不安
        emit('open', path)
        return true
      }
      if (row.dataset.kind === 'dir') {
        toggleDir(path)
        return true
      }
      if (row.dataset.kind === 'file') {
        emit('read', path)
        return true
      }
      // link / other / unknown：不猜，报上去让宿主决定。真 connector 到位后
      // 链接按「进入时解析」处理（`resolve_within` 对存在的目标
      // `canonicalize`），那是连接器的判断，不该在渲染层替它决定。
      emit('open', path)
      return true
    },
  })
})

/**
 * 面板的键盘入口：一个捕获阶段挂在**根元素**上的处理器，按焦点落在哪个区
 * 域分派（树 / 编辑区）。
 *
 * **必须是捕获阶段**：Space 要抢在 `listbox.ts` 的导航处理器之前，而那个
 * 处理器挂在树上（根的后代）——冒泡阶段里后代永远比根先拿到键。同一元素
 * 上挂两个冒泡监听器也不行：那会让「谁先拿到 Space」取决于 `onMounted`
 * 里的语句顺序，改一行就换一个行为。
 *
 * **三个键只挂一处、只判一次焦点**：分三个监听器写就得在三处各判一次
 * 「焦点在不在本面板里」，而漏判那一处的后果是面板外的退格被吃掉。
 */
function onKeydown(e: KeyboardEvent): void {
  const target = e.target instanceof HTMLElement ? e.target : null
  if (!target || !hostEl.value?.contains(target)) return
  if (e.isComposing) return

  if (e.key === ' ') {
    toggleRowOnSpace(e, target)
    return
  }
  if (e.key === 's' && (e.ctrlKey || e.metaKey)) {
    // 浏览器的「保存网页」在文件面板里永远不是用户想要的那件事
    e.preventDefault()
    if (canEdit.value && dirty.value && !props.ended) onSave()
    return
  }
  // 输入区内不抢 Backspace：那是在删字符（§4.2「单键在输入区内不抢」）
  if (e.key === 'Backspace' && !isTyping(target)) {
    const depth = props.cwd.length
    if (depth === 0) return
    e.preventDefault()
    emit('goto', depth - 1)
  }
}

/**
 * Space 展开 / 收起焦点所在的目录行（自己接的理由见文件头）。
 *
 * 只认树内的目录行：别处的 Space 该干什么还干什么（按钮的「按下」）。
 */
function toggleRowOnSpace(e: KeyboardEvent, target: HTMLElement): void {
  const el = treeEl.value
  if (!el || !el.contains(target)) return
  const row = target.closest<HTMLElement>('[data-row]')
  if (!row || row.dataset.kind !== 'dir') return
  // 越界目录不展开：它没有「里面有什么」这回事（§7.4 ①）
  if (row.dataset.outside === 'true') return
  const path = row.dataset.path ?? ''
  if (!path) return
  e.preventDefault()
  e.stopPropagation()
  toggleDir(path)
}

onBeforeUnmount(() => {
  hostEl.value?.removeEventListener('keydown', onKeydown, true)
  handle?.detach()
  handle = null
})

/** 展开 / 收起一个目录。没列过的层顺手报 `list`（一次一层的根因）。 */
function toggleDir(path: string): void {
  if (expanded.value.has(path)) {
    expanded.value.delete(path)
    return
  }
  expanded.value.add(path)
  if (!props.listings.has(path)) emit('list', path)
}

/* ---------------------------------------------------------------- 预览 */

/** 编辑区内容。`null` = 没有可编辑的内容。 */
const draft = ref<string | null>(null)
/** 打开时的原文：判脏与撤销都靠它。 */
const original = ref<string | null>(null)

watch(
  () => props.preview,
  (p) => {
    const text = p?.text ?? null
    draft.value = text
    original.value = text
  },
  { immediate: true },
)

/** 脏 = 编辑内容 ≠ 打开时的内容。二进制 / 截断档没有 `text`，恒不脏。 */
const dirty = computed(() => draft.value !== null && draft.value !== original.value)

watch(dirty, (v) => emit('dirty-change', v))

/**
 * 只有**整份文本**可编辑（§3.14）。
 *
 * 截断档不可编辑：编辑框里只有头尾各 4 KB，存回去等于用这两段覆盖整个
 * 文件——那是一次静默的数据损坏。
 */
const canEdit = computed(() => props.preview?.kind === 'text' && draft.value !== null)

const previewLines = computed<string[]>(() => {
  const text = props.preview?.text
  return text ? text.split('\n') : []
})

/**
 * 截断档：中间没显示的行数。
 *
 * **算不出来就说「已截断」而不给行数**——`hiddenLines` 只在能算时给。
 * node 侧返回的是头尾各 4 KB 的拼接，中间的行数只有它知道；这里按显示出来的
 * 行数推算，算不出来就留 `null`，不编一个数字。
 */
const hiddenLines = computed<number | null>(() => {
  const p = props.preview
  if (!p || !p.truncated) return null
  const shown = previewLines.value.length
  return shown > 2 ? shown - 2 : null
})

function onDraftInput(e: Event): void {
  draft.value = (e.target as HTMLTextAreaElement | null)?.value ?? null
}

function onSave(): void {
  const p = props.preview
  if (!p || draft.value === null) return
  emit('save', { path: p.path, text: draft.value })
}

function onRevert(): void {
  const p = props.preview
  if (!p) return
  draft.value = original.value
  emit('revert', p.path)
}

/* ---------------------------------------------------------------- 越界 */

/** ④ 的下一步与 ③ 不同：写越界要改路径，读越界要「别进去」。 */
const navError = computed(() => props.error)
const writeErr = computed(() => props.writeError)

/** 越界文案里必须写明这条边界**只管页面用户**（§7.4 关键补充）。 */
const BOUNDARY_NOTE =
  '这条边界只对页面用户生效：agent 用它自己装在设备上的工具读写文件，' +
  '不经过 files connector——打开工作区不会限制住 agent。'

/**
 * 是越界还是别的失败。
 *
 * 判据只有 `code`（node 侧 `resolve_within` 给的 `OutsideWorkspace`），
 * **`path` 的字面写法只是兜底**——符号链接逃逸不看名字，只有码能区分。
 */
function isOutsideError(e: FilesError | null): boolean {
  if (!e) return false
  if (e.code === 'path_outside_root' || e.code === 'outside_workspace') return true
  return isOutsideSeg(e.path)
}

/* ---------------------------------------------------------------- 空态 */

/**
 * 「树是空的」至少有两种，文案必须不同：
 * 通道还没建 = 还没问过设备；已建但这一层没条目 = 这个目录真的是空的。
 * 混成一句「没有文件」会让用户在通道建好之前以为工作区是空的——
 * 那正是 §3.12 要区分「正在加载」与「这里本来该有东西但没有」的理由。
 */
const treeEmptyText = computed(() => {
  if (props.ended) return '会话已结束：工作区不再可达，面板不会再建立通道。'
  if (!props.channelReady) return '通道还没建立：第一次与面板交互时建立，之后这里出现工作区的目录。'
  if (listingLoading.value) return '正在读这一层…'
  return '这一层是空的。'
})

/* ---------------------------------------------------------------- 字节 */

/**
 * 人读的字节数。
 *
 * `null` 是「这一档没给大小」→ `—`；**0 是「0 B」不是 `—`**：0 字节的空文件
 * 是真实存在的事实，而「不知道」不是。口径与 `format.ts` 的 `percent(null)`
 * 一致：取不到字段一律画 `—`，不拿 0 顶上。
 */
function humanBytes(n: number | null): string {
  if (n === null || !Number.isFinite(n) || n < 0) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}
</script>

<template>
  <div ref="hostEl" class="af-fpane af-files">
    <!-- ② 面包屑：每一层都可见，越界那一层标红 -->
    <div class="af-files__crumbs" role="navigation" aria-label="工作区路径">
      <template v-for="(c, i) in crumbs" :key="`${c.depth}:${c.path}`">
        <span v-if="i > 0" class="af-t-mute" aria-hidden="true"> / </span>
        <button
          class="af-files__crumb"
          type="button"
          :data-oob="c.outside ? 'true' : undefined"
          :aria-current="i === crumbs.length - 1 ? 'true' : undefined"
          :data-crumb-depth="c.depth"
          @click="emit('goto', c.depth)"
        >
          {{ c.label }}
        </button>
      </template>
      <span class="af-spacer"></span>
      <!-- 脏标记：tab 上那一个由宿主挂，这里是就地的一份 -->
      <span
        v-if="dirty"
        class="af-badge af-badge--warn"
        data-dirty
        title="有未保存的改动：⌃S 保存，关掉这个 tab 会丢掉它"
      >
        未保存
      </span>
    </div>

    <!-- ③ 导航越界 / 读失败：解释性错误，给「为什么」+「怎么才能进去」 -->
    <div v-if="navError" class="af-files__err" role="alert" data-nav-error>
      <div class="af-files__errhead">
        <span class="af-badge af-badge--danger">
          {{ isOutsideError(navError) ? '越界' : '读不到' }}
        </span>
        <span class="af-fp af-files__errpath">{{ navError.path }}</span>
        <span class="af-spacer"></span>
        <button
          class="af-btn af-btn--sm af-btn--ghost"
          type="button"
          aria-label="关闭这条越界说明"
          @click="emit('dismiss-error')"
        >
          ✕
        </button>
      </div>
      <p class="af-files__why">{{ navError.reason }}</p>
      <p v-if="isOutsideError(navError)" class="af-field__hint af-files__next">
        怎么才能进去：新建会话时把这个路径选进工作区——工作区路径在建会话时确定，之后不可改。
      </p>
      <p v-if="isOutsideError(navError)" class="af-field__hint">{{ BOUNDARY_NOTE }}</p>
    </div>

    <!-- 树 -->
    <div class="af-fpane__body af-files__body">
      <div
        ref="treeEl"
        class="af-tree af-files__tree"
        data-role="tree"
        tabindex="0"
        role="tree"
        aria-label="工作区文件树（↑↓ 移动，Space 展开 / 收起，⏎ 进入）"
        data-panel-focus
      >
        <button
          v-for="row in rows"
          :key="row.path"
          class="af-tree__row"
          type="button"
          data-row
          tabindex="-1"
          role="treeitem"
          aria-selected="false"
          :aria-expanded="row.kind === 'dir' ? (row.expanded ? 'true' : 'false') : undefined"
          :data-path="row.path"
          :data-kind="row.kind"
          :data-outside="row.outside ? 'true' : undefined"
          :data-excluded="row.outside ? 'true' : undefined"
          :aria-label="ariaLabelOf(row)"
          :style="{ paddingLeft: indentOf(row.depth) }"
        >
          <span class="af-tree__twisty" aria-hidden="true">{{ twistyOf(row) }}</span>
          <span class="af-tree__name">{{ row.name }}</span>
          <span v-if="row.meta" class="af-tree__meta">{{ row.meta }}</span>
        </button>
      </div>

      <!-- 空态：区分「还没问过设备」与「这一层真的是空的」 -->
      <p v-if="!rows.length" class="af-files__void af-field__hint" data-tree-empty>
        {{ treeEmptyText }}
      </p>
      <p v-if="listingLoading" class="af-files__more af-field__hint">正在读这一层…</p>
      <p v-if="listing?.truncated" class="af-files__more af-field__hint">
        这一层只显示了前 {{ listing.entries.length }} 项（共 {{ orDash(listing.total) }} 项）：
        很大的目录按层截断，不是报错。
      </p>
    </div>

    <!-- 预览：三档结论，每档给下一步（§3.14） -->
    <div class="af-files__preview" role="region" aria-label="文件预览">
      <div class="af-files__pvhead">
        <span class="af-files__pvname">{{ preview ? preview.path : '预览' }}</span>
        <span class="af-spacer"></span>
        <span v-if="preview" class="af-badge">{{ humanBytes(preview.size) }}</span>
        <span v-if="preview?.kind === 'binary'" class="af-badge af-badge--warn">二进制</span>
        <span v-else-if="preview?.kind === 'truncated'" class="af-badge af-badge--warn">已截断</span>
        <span v-else-if="preview" class="af-badge af-badge--ok">文本</span>
        <span v-else class="af-badge af-badge--off">未打开</span>
        <button
          v-if="canEdit"
          class="af-btn af-btn--sm"
          type="button"
          data-save
          :disabled="!dirty || saving || ended"
          :title="ended ? '会话已结束：工作区不再可达' : dirty ? '保存（⌃S）' : '没有改动可存'"
          @click="onSave"
        >
          保存 <kbd>⌃S</kbd>
        </button>
        <button
          v-if="canEdit && dirty"
          class="af-btn af-btn--sm af-btn--ghost"
          type="button"
          data-revert
          @click="onRevert"
        >
          撤销
        </button>
      </div>

      <!-- 没打开 / 正在读：不画假行 -->
      <p v-if="!preview && !readingPath" class="af-files__void af-field__hint" data-preview-empty>
        在左边选一个文件读它。内容由设备上的 files connector 给，这里不预置任何文件。
      </p>
      <p v-else-if="readingPath" class="af-files__void af-field__hint">
        正在读 {{ readingPath }}…
      </p>

      <!-- 二进制：类型 + 大小 + 「不支持预览」 -->
      <div v-else-if="preview?.kind === 'binary'" class="af-files__concl" data-kind-binary>
        <p class="af-files__why">二进制文件不支持预览。</p>
        <p class="af-field__hint">
          类型按内容判定（含 NUL 字节或不是合法 UTF-8），大小 {{ humanBytes(preview.size) }}。
          需要改它请让 agent 做——浏览器不是 IDE。
        </p>
      </div>

      <!-- 截断：头尾各 4 KB + 中间一句「已截断」 -->
      <div v-else-if="preview?.kind === 'truncated'" class="af-files__concl" data-kind-truncated>
        <p class="af-files__why">
          文件超过 1 MB（{{ humanBytes(preview.size) }}），只读了前后各 4 KB。
        </p>
        <div class="af-files__code">
          <div v-for="(line, i) in previewLines" :key="i" class="af-files__line">
            <span class="ln">{{ String(i + 1).padStart(4, ' ') }} </span
            ><span>{{ line }}</span>
          </div>
        </div>
        <p class="af-files__cut af-field__hint">
          … 已截断<template v-if="hiddenLines !== null">，中间约 {{ hiddenLines }} 行未显示</template>
          — 大文件只读前后各 4 KB。
        </p>
        <p class="af-field__hint">编辑整份请让 agent 做，浏览器不是 IDE。</p>
      </div>

      <!-- 文本：内容 + 可编辑（行号在编辑区里没有意义，所以给的是编辑框） -->
      <div v-else-if="preview" class="af-files__textwrap" data-kind-text>
        <textarea
          class="af-textarea af-files__edit"
          :value="draft ?? ''"
          :readonly="ended"
          :aria-invalid="writeErr ? 'true' : undefined"
          :aria-label="`编辑 ${preview.path}`"
          spellcheck="false"
          @input="onDraftInput"
        ></textarea>
        <p class="af-field__hint af-files__edithint">
          {{
            ended
              ? '会话已结束：只读。'
              : dirty
                ? '有未保存的改动：⌃S 保存，关掉这个 tab 会丢掉它。'
                : '与设备上的内容一致。'
          }}
        </p>
      </div>

      <!-- ④ 写越界 / 写失败：与③分开，下一步不同 -->
      <div v-if="writeErr" class="af-files__writeerr" role="alert" data-write-error>
        <div class="af-files__errhead">
          <span class="af-badge af-badge--danger">
            {{ isOutsideError(writeErr) ? '写越界' : '保存失败' }}
          </span>
          <span class="af-fp af-files__errpath">{{ writeErr.path }}</span>
          <span class="af-spacer"></span>
          <button
            class="af-btn af-btn--sm af-btn--ghost"
            type="button"
            aria-label="关闭这条保存失败说明"
            @click="emit('dismiss-write-error')"
          >
            ✕
          </button>
        </div>
        <p class="af-field__error">{{ writeErr.reason }}</p>
        <p class="af-field__hint">
          {{
            isOutsideError(writeErr)
              ? '改成一个工作区内的路径再存；工作区在建会话时确定，之后不可改。'
              : '改动还在编辑区里，没有被写走。'
          }}
        </p>
      </div>
    </div>
  </div>
</template>

<style scoped>
.af-files {
  min-height: 0;
}
/* ② 面包屑：每一层都可见，越界的那一层标红——边界被画出来，而不是只在错误里说一次 */
.af-files__crumbs {
  display: flex;
  align-items: center;
  gap: var(--af-space-1);
  flex: 0 0 auto;
  padding: var(--af-space-2) var(--af-space-3);
  font-size: var(--af-text-xs);
  color: var(--af-fg-tertiary);
  border-bottom: 1px solid var(--af-border-subtle);
  background: var(--af-surface-1);
  overflow-x: auto;
}
.af-files__crumb {
  background: none;
  border: 0;
  padding: 0;
  font: inherit;
  color: var(--af-fg-secondary);
  cursor: pointer;
  white-space: nowrap;
}
.af-files__crumb:hover {
  color: var(--af-fg-primary);
}
.af-files__crumb[data-oob='true'] {
  color: var(--af-danger-fg);
}
.af-files__body {
  display: flex;
  flex-direction: column;
  padding: var(--af-space-2) 0;
}
.af-files__tree {
  flex: 0 1 auto;
  overflow: auto;
  min-height: 0;
}
.af-files__void {
  margin: 0;
  padding: var(--af-space-4);
  max-width: 72ch;
  line-height: var(--af-leading-base);
}
.af-files__more {
  margin: var(--af-space-2) var(--af-space-4) 0;
}
/* 预览在树下方而不是右侧：面板坞是窄栏，左右分栏会把两边都挤到 200px 以下 */
.af-files__preview {
  flex: 0 0 auto;
  max-height: 55%;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  padding: var(--af-space-3) var(--af-space-4);
  background: var(--af-inset);
  border-top: 1px solid var(--af-border-subtle);
  overflow: auto;
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-tight);
}
.af-files__pvhead {
  display: flex;
  align-items: center;
  gap: var(--af-space-2);
  flex: 0 0 auto;
}
.af-files__pvname {
  color: var(--af-fg-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.af-files__why {
  margin: 0;
  color: var(--af-fg-primary);
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-snug);
}
.af-files__next {
  margin: 0;
}
.af-files__err,
.af-files__writeerr {
  flex: 0 0 auto;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  margin: 0;
  padding: var(--af-space-3) var(--af-space-4);
  background: var(--af-danger-bg);
  border-top: 1px solid var(--af-danger-line);
  border-bottom: 1px solid var(--af-danger-line);
}
.af-files__writeerr {
  border-radius: var(--af-radius-sm);
  border: 1px solid var(--af-danger-line);
}
.af-files__errhead {
  display: flex;
  align-items: center;
  gap: var(--af-space-2);
  min-width: 0;
}
.af-files__errpath {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.af-files__concl {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-files__code {
  overflow: auto;
  max-height: 150px;
  background: var(--af-surface-0);
  border: 1px solid var(--af-border-subtle);
  border-radius: var(--af-radius-sm);
  padding: var(--af-space-2) 0;
  font-family: var(--af-font-mono);
  white-space: pre;
}
.af-files__cut {
  margin: 0;
  color: var(--af-warn-fg);
}
.af-files__textwrap {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  min-height: 0;
}
.af-files__edit {
  flex: 1 1 auto;
  min-height: 96px;
  font-family: var(--af-font-mono);
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-tight);
}
.af-files__edithint {
  margin: 0;
}
</style>
