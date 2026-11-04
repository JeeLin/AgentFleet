/* ===========================================================================
   FilesPanel 的测试（里程碑 13c）
   ---------------------------------------------------------------------------
   三条测试标准全部**前端可独立完成**（里程碑 13c 的依赖段明确写了
   「刻意不排 files connector 子任务」）：目录树导航、越界错误态渲染、脏标记。
   没有一条需要真 connector——组件不发请求、不持通道是它的设计前提
   （见组件头注释），所以这里喂的是**与 `files.rs` 载荷同形状**的 mock，
   而不是另一套测试专用形状。

   真 connector（子任务 9）到位后要补的是**宿主的接线测试**（`list` 请求 →
   `channel.frame` → 喂进 props），不是这里：本文件的断言在那种情况下
   一行都不用改。
   =========================================================================== */

import { nextTick } from 'vue'
import { describe, expect, it, vi } from 'vitest'

import FilesPanel, {
  type FileEntry,
  type FileListing,
  type FilePreview,
  type FilesError,
} from '@/domains/session/FilesPanel.vue'
import { mountIn } from '@/test/mount'

/* ---------------------------------------------------------------- mock 数据 */

function dir(name: string, over: Partial<FileEntry> = {}): FileEntry {
  return { name, kind: 'dir', size: 0, mtime_ms: null, ...over }
}

function file(name: string, size: number): FileEntry {
  return { name, kind: 'file', size, mtime_ms: 1700000000000 }
}

function link(name: string, target: string | null): FileEntry {
  return { name, kind: 'link', size: 0, mtime_ms: null, target }
}

/** 一层的清单。`''` = 工作区根。 */
function layer(path: string, entries: FileEntry[], over: Partial<FileListing> = {}): FileListing {
  return { path, entries, total: entries.length, truncated: false, ...over }
}

function listings(...layers: FileListing[]): Map<string, FileListing> {
  return new Map(layers.map((l) => [l.path, l]))
}

function press(el: HTMLElement, key: string): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
  el.dispatchEvent(e)
  return e
}

function treeRows(w: { element: HTMLElement }): HTMLElement[] {
  return Array.from(w.element.querySelectorAll<HTMLElement>('.af-files__tree [data-row]'))
}

function mount(props: Partial<Record<string, unknown>> = {}) {
  return mountIn(FilesPanel, {
    props: {
      root: '/srv/alpha',
      cwd: [],
      listings: listings(),
      channelReady: true,
      ...props,
    },
  })
}

/** 根目录：一个目录、两个文件、一个越界节点、一个越界链接。 */
const ROOT = layer('', [
  dir('src'),
  file('README.md', 2048),
  file('Cargo.toml', 806),
  dir('..'),
  link('target', '/mnt/big/target'),
])
const SRC = layer('src', [file('main.rs', 4096), dir('lib')])
const LIB = layer('lib', [file('mod.rs', 1234)])

/* ==========================================================================
   一、目录树导航（↑↓ / Home / End / Enter）
   ========================================================================== */

describe('FilesPanel：树导航（listbox：容器是唯一 Tab 落点）', () => {
  it('容器 tabindex=0，行 tabindex=-1（Tab 一次进树，不是穿过每一行）', () => {
    const w = mount({ listings: listings(ROOT) })
    const tree = w.element.querySelector('.af-files__tree')
    expect(tree?.getAttribute('tabindex')).toBe('0')
    for (const row of treeRows(w)) expect(row.getAttribute('tabindex')).toBe('-1')
  })

  it('行是真 <button>（§3.8：用 div 会让 Tab 进不去树）', () => {
    const w = mount({ listings: listings(ROOT) })
    for (const row of treeRows(w)) expect(row.tagName).toBe('BUTTON')
  })

  it('↓ 走到下一行并写 aria-selected（选中态必须可读，不只是加背景色）', () => {
    const w = mount({ listings: listings(ROOT) })
    const rows = treeRows(w)
    rows[0]?.focus()
    press(rows[0] as HTMLElement, 'ArrowDown')
    expect(document.activeElement).toBe(rows[1])
    expect(rows[1]?.getAttribute('aria-selected')).toBe('true')
    expect(rows[0]?.getAttribute('aria-selected')).toBe('false')
  })

  it('↑ 在首行不越界（不绕回末行：树不是转盘）', () => {
    const w = mount({ listings: listings(ROOT) })
    const rows = treeRows(w)
    rows[0]?.focus()
    press(rows[0] as HTMLElement, 'ArrowUp')
    expect(document.activeElement).toBe(rows[0])
  })

  it('End 到最后一行，Home 回第一行', () => {
    const w = mount({ listings: listings(ROOT) })
    const rows = treeRows(w)
    rows[0]?.focus()
    press(rows[0] as HTMLElement, 'End')
    expect(document.activeElement).toBe(rows[rows.length - 1])
    press(rows[rows.length - 1] as HTMLElement, 'Home')
    expect(document.activeElement).toBe(rows[0])
  })

  it('⏎ 在文件行上发 read（路径相对工作区根）', () => {
    const w = mount({ listings: listings(ROOT) })
    const row = treeRows(w).find((r) => r.dataset.path === 'README.md') as HTMLElement
    row.focus()
    press(row, 'Enter')
    expect(w.emitted('read')?.[0]).toEqual(['README.md'])
  })

  it('⏎ 在目录行上展开：已列过的一层不重取（一次一层的根因）', async () => {
    const w = mount({ listings: listings(ROOT, SRC, LIB) })
    const row = treeRows(w).find((r) => r.dataset.path === 'src') as HTMLElement
    expect(row.getAttribute('aria-expanded')).toBe('false')
    row.focus()
    press(row, 'Enter')
    await nextTick()
    // 已经列过（SRC 在 listings 里）→ 只展开，不重取
    expect(w.emitted('list')).toBeUndefined()
    expect(row.getAttribute('aria-expanded')).toBe('true')
  })

  it('⏎ 展开一个还没列过的目录 → 发 list', () => {
    const w = mount({ listings: listings(ROOT) })
    const row = treeRows(w).find((r) => r.dataset.path === 'src') as HTMLElement
    row.focus()
    press(row, 'Enter')
    expect(w.emitted('list')?.[0]).toEqual(['src'])
  })

  it('展开后子层已列过 → 子行出现，且缩进更深', async () => {
    const w = mount({ listings: listings(ROOT, SRC, LIB) })
    const row = treeRows(w).find((r) => r.dataset.path === 'src') as HTMLElement
    row.focus()
    press(row, 'Enter')
    await nextTick()
    const paths = treeRows(w).map((r) => r.dataset.path)
    expect(paths).toContain('src/main.rs')
    // 没列过的 `src/lib` 的子层不在列表里：那不是空目录，是还没问过设备
    const libRow = treeRows(w).find((r) => r.dataset.path === 'src/lib') as HTMLElement
    expect(libRow.getAttribute('aria-expanded')).toBe('false')
    expect(Number((libRow.style.paddingLeft || '').replace('px', ''))).toBeGreaterThan(
      Number((row.style.paddingLeft || '').replace('px', '')),
    )
  })

  it('Space 展开 / 收起目录（listbox 的 toggle 语义）', async () => {
    const w = mount({ listings: listings(ROOT, SRC) })
    const row = treeRows(w).find((r) => r.dataset.path === 'src') as HTMLElement
    row.focus()
    press(row, ' ')
    await nextTick()
    expect(row.getAttribute('aria-expanded')).toBe('true')
    expect(treeRows(w).map((r) => r.dataset.path)).toContain('src/main.rs')
    press(row, ' ')
    await nextTick()
    expect(row.getAttribute('aria-expanded')).toBe('false')
    // 收起不清已列的条目（再展开不用重取）
    expect(w.emitted('list')).toBeUndefined()
  })

  it('Tab 进容器后焦点落在第一行（否则 ↓ 失灵）', () => {
    const w = mount({ listings: listings(ROOT) })
    const tree = w.element.querySelector<HTMLElement>('.af-files__tree')
    tree?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    expect(document.activeElement).toBe(treeRows(w)[0])
  })
})

/* ==========================================================================
   二、越界：四路证据（§7.4）
   ========================================================================== */

describe('FilesPanel：越界的四路证据', () => {
  const OUT: FilesError = {
    path: '../etc',
    reason: '它在这个会话的工作区路径之外。',
    code: 'path_outside_root',
  }

  it('① 越界节点照常显示（斜纹 + ⊘ + 不可进入），不隐藏', () => {
    const w = mount({ listings: listings(ROOT) })
    const rows = treeRows(w)
    // `..` 与指向界外的 `target` 链接都在树里
    const parent = rows.find((r) => r.dataset.path === '..')
    expect(parent).not.toBeNull()
    expect(parent?.getAttribute('data-excluded')).toBe('true')
    expect(parent?.getAttribute('aria-label')).toContain('不可进入')
    expect(parent?.textContent).toContain('不可进入')

    const ln = rows.find((r) => r.dataset.path === 'target')
    expect(ln?.getAttribute('data-kind')).toBe('link')
    // 链接的目标给出来：隐藏会让用户以为树不完整，而 `ls` 里看得到它
    expect(ln?.textContent).toContain('/mnt/big/target')
  })

  it('① 越界节点可点，⏎ 报 open（点了给解释，不是静默失败）', () => {
    const w = mount({ listings: listings(ROOT) })
    const row = treeRows(w).find((r) => r.dataset.outside === 'true') as HTMLElement
    row.focus()
    press(row, 'Enter')
    expect(w.emitted('open')?.[0]).toEqual(['..'])
  })

  it('② 面包屑越界的那一层标红', () => {
    const w = mount({ cwd: ['..', 'etc'], listings: listings() })
    const oob = Array.from(w.element.querySelectorAll<HTMLElement>('.af-files__crumb[data-oob]'))
    expect(oob.map((c) => c.textContent?.trim())).toEqual(['..'])
    // 根那一段不标红：越界是「越出去的那一段」的属性（§7.4 ②）
    expect(w.element.querySelector('.af-files__crumb')?.getAttribute('data-oob')).toBeNull()
  })

  it('③ 导航越界：解释性错误块给「为什么」+「怎么才能进去」', () => {
    const w = mount({ listings: listings(ROOT), error: OUT })
    const block = w.element.querySelector('[data-nav-error]')
    expect(block?.getAttribute('role')).toBe('alert')
    expect(block?.textContent).toContain('越界')
    expect(block?.textContent).toContain('../etc')
    expect(block?.textContent).toContain('工作区路径之外')
    expect(block?.textContent).toContain('怎么才能进去')
    expect(block?.textContent).toContain('新建会话时')
  })

  it('③ 越界文案写明这条边界只管页面用户（agent 不经 files connector）', () => {
    const w = mount({ listings: listings(ROOT), error: OUT })
    const text = w.element.querySelector('[data-nav-error]')?.textContent ?? ''
    expect(text).toContain('只对页面用户生效')
    expect(text).toContain('不经过 files connector')
  })

  it('③ 非越界的读失败不冒充越界（下一步不同，不说混）', () => {
    const w = mount({
      listings: listings(ROOT),
      error: { path: 'gone.txt', reason: '文件不存在。', code: 'not_found' },
    })
    const block = w.element.querySelector('[data-nav-error]')
    expect(block?.textContent).toContain('读不到')
    expect(block?.textContent).not.toContain('怎么才能进去')
    expect(block?.textContent).not.toContain('新建会话时')
  })

  it('④ 写越界是独立的一块，且与③分开（下一步不同）', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'a.txt', size: 4, kind: 'text', truncated: false, text: 'abcd' },
      writeError: { path: '../etc/passwd', reason: '写路径越界。', code: 'path_outside_root' },
    })
    const block = w.element.querySelector('[data-write-error]')
    expect(block?.getAttribute('role')).toBe('alert')
    expect(block?.textContent).toContain('写越界')
    expect(block?.textContent).toContain('改成一个工作区内的路径再存')
    // 编辑框标红（④ 的就地证据）
    expect(w.element.querySelector('.af-files__edit')?.getAttribute('aria-invalid')).toBe('true')
    // ③ 没有出现
    expect(w.element.querySelector('[data-nav-error]')).toBeNull()
  })

  it('④ 非越界的写失败说「改动还在编辑区里」，不是「改路径」', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'a.txt', size: 4, kind: 'text', truncated: false, text: 'abcd' },
      writeError: { path: 'a.txt', reason: '设备离线，没写成。', code: 'device_offline' },
    })
    const block = w.element.querySelector('[data-write-error]')
    expect(block?.textContent).toContain('保存失败')
    expect(block?.textContent).toContain('改动还在编辑区里')
  })

  it('越界 / 错误块都有关闭入口，关掉走 emit（不自己吞状态）', async () => {
    const w = mount({ listings: listings(ROOT), error: OUT })
    await w.element.querySelector<HTMLElement>('[data-nav-error] button')?.click()
    expect(w.emitted('dismiss-error')).toHaveLength(1)
  })
})

/* ==========================================================================
   三、脏标记
   ========================================================================== */

describe('FilesPanel：脏标记', () => {
  const TEXT: FilePreview = { path: 'a.txt', size: 4, kind: 'text', truncated: false, text: 'abcd' }

  it('内容与打开时一致时没有脏标记（0 处改动不是要展示的信息）', () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    expect(w.element.querySelector('[data-dirty]')).toBeNull()
    expect(w.emitted('dirty-change')).toBeUndefined()
  })

  it('改动编辑区 → 出现「未保存」并报 dirty-change(true)', async () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')
    expect(ta).not.toBeNull()
    ta!.value = 'abcdef'
    await ta!.dispatchEvent(new Event('input'))
    expect(w.element.querySelector('[data-dirty]')?.textContent).toContain('未保存')
    expect(w.emitted('dirty-change')?.at(-1)).toEqual([true])
  })

  it('改回原文 → 脏标记消失（脏是「与原文不同」，不是「动过」）', async () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')!
    ta.value = 'xyz'
    await ta.dispatchEvent(new Event('input'))
    expect(w.element.querySelector('[data-dirty]')).not.toBeNull()
    ta.value = 'abcd'
    await ta.dispatchEvent(new Event('input'))
    expect(w.element.querySelector('[data-dirty]')).toBeNull()
    expect(w.emitted('dirty-change')?.at(-1)).toEqual([false])
  })

  it('未保存时保存按钮才可用（没有改动可存就不给按）', async () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    const save = w.element.querySelector<HTMLButtonElement>('[data-save]')!
    expect(save.disabled).toBe(true)
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')!
    ta.value = 'abcdef'
    await ta.dispatchEvent(new Event('input'))
    expect(save.disabled).toBe(false)
  })

  it('保存发 { path, text }，撤销发 revert 且回到原文', async () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')!
    ta.value = 'zzz'
    await ta.dispatchEvent(new Event('input'))
    await w.element.querySelector<HTMLElement>('[data-save]')?.click()
    expect(w.emitted('save')?.[0]).toEqual([{ path: 'a.txt', text: 'zzz' }])

    await w.element.querySelector<HTMLElement>('[data-revert]')?.click()
    expect(w.emitted('revert')?.[0]).toEqual(['a.txt'])
    expect(w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')?.value).toBe('abcd')
    expect(w.element.querySelector('[data-dirty]')).toBeNull()
  })

  it('会话已结束时编辑区只读，保存按钮禁用（工作区不再可达）', () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT, ended: true })
    expect(w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')?.readOnly).toBe(true)
    expect(w.element.querySelector<HTMLButtonElement>('[data-save]')?.disabled).toBe(true)
    expect(w.element.textContent).toContain('只读')
  })
})

/* ==========================================================================
   四、预览必须给结论（§3.14）
   ========================================================================== */

describe('FilesPanel：预览必须给结论（不许只显示「无法预览」）', () => {
  it('没打开文件时是空态，不画假行', () => {
    const w = mount({ listings: listings(ROOT) })
    const empty = w.element.querySelector('[data-preview-empty]')
    expect(empty).not.toBeNull()
    expect(empty?.textContent).toContain('不预置任何文件')
    expect(w.element.querySelectorAll('.af-files__line')).toHaveLength(0)
  })

  it('文本档可编辑，且标成「文本」', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'a.txt', size: 4, kind: 'text', truncated: false, text: 'abcd' },
    })
    expect(w.element.querySelector('[data-kind-text]')).not.toBeNull()
    expect(w.element.textContent).toContain('文本')
    expect(w.element.querySelector('.af-files__edit')).not.toBeNull()
  })

  it('二进制档给类型 + 大小 +「不支持预览」，不糊一层假预览', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'logo.png', size: 81920, kind: 'binary', truncated: false },
    })
    const block = w.element.querySelector('[data-kind-binary]')
    expect(block?.textContent).toContain('不支持预览')
    expect(block?.textContent).toContain('80.0 KB')
    expect(w.element.querySelector('.af-files__edit')).toBeNull()
    expect(w.element.querySelector('[data-save]')).toBeNull()
  })

  it('> 1 MB 档只读头尾 + 明写「已截断」+「编辑整份请让 agent 做」', () => {
    const long = Array.from({ length: 40 }, (_, i) => `line ${i + 1}`).join('\n')
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'big.log', size: 4 * 1024 * 1024, kind: 'truncated', truncated: true, text: long },
    })
    const block = w.element.querySelector('[data-kind-truncated]')
    expect(block?.textContent).toContain('已截断')
    expect(block?.textContent).toContain('4.0 MB')
    expect(block?.textContent).toContain('前后各 4 KB')
    expect(block?.textContent).toContain('编辑整份请让 agent 做')
    expect(block?.textContent).toContain('浏览器不是 IDE')
    // 截断档不可编辑：编辑框里只有头尾，存回去等于用两段覆盖整个文件
    expect(w.element.querySelector('.af-files__edit')).toBeNull()
    // 也不给保存 / 撤销入口——「不可编辑」必须体现在**没有入口**上，
    // 只断言「没有编辑框」会在模板分支里意外成立而测不到 `canEdit`
    expect(w.element.querySelector('[data-save]')).toBeNull()
    expect(w.element.querySelector('[data-revert]')).toBeNull()
  })

  it('大小没给时预览头是「—」，不是「0 B」（0 字节与不知道是两件事）', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'a.txt', size: null, kind: 'text', truncated: false, text: 'abcd' },
    })
    const badge = w.element.querySelector('.af-files__pvhead .af-badge')
    expect(badge?.textContent?.trim()).toBe('—')
  })

  it('0 字节的文件仍说「0 B」——0 是真实事实，不是「没上报」', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'a.txt', size: 0, kind: 'text', truncated: false, text: '' },
    })
    expect(w.element.querySelector('.af-files__pvhead .af-badge')?.textContent?.trim()).toBe('0 B')
  })

  it('条目里的 size 没给时树的 meta 列是「—」', () => {
    const w = mount({
      listings: listings(layer('', [{ name: 'x.txt', kind: 'file', size: null, mtime_ms: null }])),
    })
    expect(treeRows(w)[0]?.querySelector('.af-tree__meta')?.textContent).toContain('—')
  })

  it('截断档算不出中间行数时不编一个数字', () => {
    const w = mount({
      listings: listings(ROOT),
      preview: { path: 'big.log', size: 2 * 1024 * 1024, kind: 'truncated', truncated: true, text: 'a\nb' },
    })
    const cut = w.element.querySelector('.af-files__cut')?.textContent ?? ''
    expect(cut).toContain('已截断')
    expect(cut).not.toMatch(/中间约/)
  })
})

/* ==========================================================================
   五、空态与「没问过设备」的区分
   ========================================================================== */

describe('FilesPanel：空态说的是为什么空', () => {
  it('通道还没建立时不说「这一层是空的」——那是还没问过设备', () => {
    const w = mount({ listings: listings(), channelReady: false })
    expect(w.element.querySelector('[data-tree-empty]')?.textContent).toContain('通道还没建立')
    expect(w.element.querySelector('[data-tree-empty]')?.textContent).not.toContain('这一层是空的')
  })

  it('通道已建、这一层没条目 → 明写「这一层是空的」', () => {
    const w = mount({ listings: listings(layer('', [])), channelReady: true })
    expect(w.element.querySelector('[data-tree-empty]')?.textContent).toContain('这一层是空的')
  })

  it('会话已结束：工作区不再可达，不给一个可点的空树', () => {
    const w = mount({ listings: listings(), ended: true })
    expect(w.element.querySelector('[data-tree-empty]')?.textContent).toContain('不再可达')
  })

  it('正在读一层时说「正在读」，不说「空的」（§3.12：骨架与空态要能区分）', () => {
    const w = mount({ listings: listings(), loading: new Set(['']) })
    expect(w.element.textContent).toContain('正在读这一层')
  })

  it('层被截断时明写还有多少项（截断不是报错）', () => {
    const w = mount({ listings: listings(layer('', [dir('a')], { total: 4821, truncated: true })) })
    expect(w.element.textContent).toContain('4821')
    expect(w.element.textContent).toContain('截断')
  })

  it('`total` 没给时是「—」，不是「共 0 项」（编一个 0 等于说这一层是空的）', () => {
    const w = mount({ listings: listings(layer('', [dir('a')], { total: null, truncated: true })) })
    expect(w.element.textContent).toContain('共 — 项')
    expect(w.element.textContent).not.toContain('共 0 项')
  })
})

/* ==========================================================================
   五之二、panel 域的两个键：⌃S 保存 / Backspace 上一级（§4.2）
   ========================================================================== */

describe('FilesPanel：panel 域的两个键', () => {
  const TEXT: FilePreview = { path: 'a.txt', size: 4, kind: 'text', truncated: false, text: 'abcd' }

  function ctrlS(el: HTMLElement): KeyboardEvent {
    const e = new KeyboardEvent('keydown', {
      key: 's',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    })
    el.dispatchEvent(e)
    return e
  }

  it('⌃S 在面板内保存脏文件，并吞掉浏览器的「保存网页」', async () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    const host = w.element
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')!
    ta.value = 'zzz'
    await ta.dispatchEvent(new Event('input'))
    const e = ctrlS(host)
    expect(e.defaultPrevented).toBe(true)
    expect(w.emitted('save')?.[0]).toEqual([{ path: 'a.txt', text: 'zzz' }])
  })

  it('没有改动时 ⌃S 不发保存（没有东西可存）', () => {
    const w = mount({ listings: listings(ROOT), preview: TEXT })
    ctrlS(w.element)
    expect(w.emitted('save')).toBeUndefined()
  })

  it('Backspace 在树上回上一级（发 goto(depth-1)）', () => {
    const w = mount({ cwd: ['src', 'lib'], listings: listings() })
    const row = w.element.querySelector<HTMLElement>('.af-files__tree')!
    row.focus()
    row.dispatchEvent(new KeyboardEvent('keydown', { key: 'Backspace', bubbles: true, cancelable: true }))
    expect(w.emitted('goto')?.[0]).toEqual([1])
  })

  it('编辑区里的 Backspace 不抢：那是在删字符，不是「回上一级」', () => {
    const w = mount({ cwd: ['src'], listings: listings(ROOT), preview: TEXT })
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')!
    const e = new KeyboardEvent('keydown', { key: 'Backspace', bubbles: true, cancelable: true })
    ta.dispatchEvent(e)
    expect(e.defaultPrevented).toBe(false)
    expect(w.emitted('goto')).toBeUndefined()
  })

  it('焦点在本面板之外时三个键都不生效（一个处理器、一次焦点判定）', () => {
    const w = mount({ cwd: ['src'], listings: listings(ROOT, SRC), preview: TEXT })
    const ta = w.element.querySelector<HTMLTextAreaElement>('.af-files__edit')!
    ta.value = 'zzz'
    ta.dispatchEvent(new Event('input'))

    const outside = document.createElement('div')
    document.body.appendChild(outside)
    try {
      for (const e of [
        new KeyboardEvent('keydown', { key: 'Backspace', bubbles: true, cancelable: true }),
        new KeyboardEvent('keydown', { key: 's', ctrlKey: true, bubbles: true, cancelable: true }),
      ]) {
        outside.dispatchEvent(e)
        expect(e.defaultPrevented).toBe(false)
      }
      expect(w.emitted('goto')).toBeUndefined()
      expect(w.emitted('save')).toBeUndefined()

      // 面板外的 Space 也不展开任何一行（cwd 是 src，所以这一层里的目录是 src/lib）
      const row = treeRows(w).find((r) => r.dataset.path === 'src/lib') as HTMLElement
      outside.dispatchEvent(new KeyboardEvent('keydown', { key: ' ', bubbles: true, cancelable: true }))
      expect(row.getAttribute('aria-expanded')).toBe('false')
      expect(w.emitted('list')).toBeUndefined()
    } finally {
      outside.remove()
    }
  })

  it('已经在根上时 Backspace 不动（没有上一级）', () => {
    const w = mount({ cwd: [], listings: listings(ROOT) })
    w.element
      .querySelector<HTMLElement>('.af-files__tree')!
      .dispatchEvent(new KeyboardEvent('keydown', { key: 'Backspace', bubbles: true, cancelable: true }))
    expect(w.emitted('goto')).toBeUndefined()
  })
})

/* ==========================================================================
   六、面包屑与 cwd
   ========================================================================== */

describe('FilesPanel：面包屑', () => {
  it('根那一段是会话的工作区路径（没有第二个根）', () => {
    const w = mount({ root: '/srv/alpha', cwd: [] })
    expect(w.element.querySelector('.af-files__crumb')?.textContent?.trim()).toBe('/srv/alpha')
  })

  it('cwd 逐层展开，点一层发 goto(depth)', async () => {
    const w = mount({ cwd: ['src', 'lib'], listings: listings() })
    const labels = Array.from(w.element.querySelectorAll('.af-files__crumb')).map((c) =>
      c.textContent?.trim(),
    )
    expect(labels).toEqual(['/srv/alpha', 'src', 'lib'])

    await w.element.querySelectorAll<HTMLElement>('.af-files__crumb')[1]?.click()
    expect(w.emitted('goto')?.[0]).toEqual([1])
  })

  it('点根那一段发 goto(0)', async () => {
    const w = mount({ cwd: ['src'] })
    await w.element.querySelector<HTMLElement>('.af-files__crumb')?.click()
    expect(w.emitted('goto')?.[0]).toEqual([0])
  })
})

/* ==========================================================================
   七、不画假数据
   ========================================================================== */

describe('FilesPanel：不画假数据', () => {
  it('没有 listings 时不画任何行（空就是空，用空态说明）', () => {
    const w = mount({ listings: listings(), channelReady: true })
    expect(treeRows(w)).toHaveLength(0)
    expect(w.element.querySelector('[data-tree-empty]')).not.toBeNull()
  })

  it('没列过的目录没有子行——那不是空目录，是还没问过设备', () => {
    const w = mount({ listings: listings(layer('', [dir('src')])) })
    expect(treeRows(w)).toHaveLength(1)
    const src = treeRows(w)[0] as HTMLElement
    expect(src.getAttribute('aria-expanded')).toBe('false')
    expect(w.element.textContent).not.toContain('src/')
  })

  it('面板自己不发请求：没有 fetch / 没有通道依赖', () => {
    const fetchSpy = vi.fn()
    vi.stubGlobal('fetch', fetchSpy)
    const w = mount({ listings: listings(ROOT) })
    const row = treeRows(w).find((r) => r.dataset.path === 'README.md') as HTMLElement
    row.focus()
    press(row, 'Enter')
    expect(fetchSpy).not.toHaveBeenCalled()
  })
})

/* ==========================================================================
   八、无障碍
   ========================================================================== */

describe('FilesPanel：无障碍', () => {
  it('树是 role=tree，行是 role=treeitem', () => {
    const w = mount({ listings: listings(ROOT) })
    expect(w.element.querySelector('.af-files__tree')?.getAttribute('role')).toBe('tree')
    for (const row of treeRows(w)) expect(row.getAttribute('role')).toBe('treeitem')
  })

  it('树有无障碍名（读屏要播报这是什么、怎么用）', () => {
    const w = mount({ listings: listings(ROOT) })
    const label = w.element.querySelector('.af-files__tree')?.getAttribute('aria-label') ?? ''
    expect(label).toContain('文件树')
    expect(label).toContain('↑↓')
    expect(label).toContain('Space')
  })

  it('行的 aria-label 带类型与大小（键盘用户不看得到图标）', () => {
    const w = mount({ listings: listings(ROOT) })
    const row = treeRows(w).find((r) => r.dataset.path === 'README.md') as HTMLElement
    expect(row.getAttribute('aria-label')).toContain('文件')
    expect(row.getAttribute('aria-label')).toContain('KB')
  })

  it('越界行的 aria-label 说「不可进入」（信息不只活在斜纹里）', () => {
    const w = mount({ listings: listings(ROOT) })
    const row = treeRows(w).find((r) => r.dataset.outside === 'true') as HTMLElement
    expect(row.getAttribute('aria-label')).toContain('不可进入')
  })

  it('面板正文是宿主的焦点落点（data-panel-focus）', () => {
    const w = mount({ listings: listings(ROOT) })
    expect(w.element.querySelector('[data-panel-focus]')).not.toBeNull()
  })
})

/* ==========================================================================
   九、node 侧真的会发的两档 kind：other（管道 / socket）与 unknown（竞态删除）
   ========================================================================== */

describe('FilesPanel：other 与 unknown 两档', () => {
  // `connector/files.rs` 的 ftyp 分支：既非文件也非目录 → "other"；
  // 元数据读不到（列目录后该项被删）→ "unknown"。两档都不该被说成文件。
  const MIXED = layer('', [
    { name: 'pipe', kind: 'other', size: 0, mtime_ms: null },
    { name: 'gone', kind: 'unknown', size: 0, mtime_ms: null },
  ])

  it('other 行说「其他」，不给大小——0 字节是编出来的事实', () => {
    const w = mount({ listings: listings(MIXED) })
    const row = treeRows(w).find((r) => r.dataset.path === 'pipe') as HTMLElement
    expect(row.getAttribute('aria-label')).toContain('其他')
    expect(row.getAttribute('aria-label')).not.toContain('文件')
    expect(row.querySelector('.af-tree__meta')).toBeNull()
  })

  it('unknown 行说「读不到类型」，且 meta 写「类型未知」', () => {
    const w = mount({ listings: listings(MIXED) })
    const row = treeRows(w).find((r) => r.dataset.path === 'gone') as HTMLElement
    expect(row.getAttribute('aria-label')).toContain('读不到类型')
    expect(row.querySelector('.af-tree__meta')?.textContent).toContain('类型未知')
  })

  it('⏎ 在 other / unknown 行上报 open，不猜成 read（node 侧才有判定权）', () => {
    const w = mount({ listings: listings(MIXED) })
    for (const name of ['pipe', 'gone']) {
      const row = treeRows(w).find((r) => r.dataset.path === name) as HTMLElement
      row.focus()
      press(row, 'Enter')
    }
    expect(w.emitted('open')).toEqual([['pipe'], ['gone']])
    expect(w.emitted('read')).toBeUndefined()
  })
})
