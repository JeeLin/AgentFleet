import { describe, expect, it } from 'vitest'

import {
  EMPTY_TABS,
  STORAGE_KEY,
  activateTab,
  closeTab,
  loadTabs,
  neighborId,
  openTab,
  saveTabs,
  type TabMeta,
  type TabsState,
} from './tabs'

function meta(id: string, title = id): TabMeta {
  return {
    id,
    title,
    path: `/srv/${title}`,
    nodeId: `n_${id}`,
    // 缺省给 `unknown`：绝大多数用例不关心起点，而这一档不会让它们
    // 意外地看起来测到了「有起点」。要别的态就自己构造。
    baseRev: null,
    baseRevStatus: 'unknown',
  }
}

function stateOf(...ids: string[]): TabsState {
  return { tabs: ids.map((id) => meta(id)), activeId: ids[0] ?? null }
}

/** 指定激活项的状态（`stateOf` 默认激活第一个）。 */
function stateWith(activeId: string, ...ids: string[]): TabsState {
  return { tabs: ids.map((id) => meta(id)), activeId }
}

describe('openTab', () => {
  it('新 tab 追加在末尾并激活', () => {
    const s = openTab(EMPTY_TABS, meta('a'))
    expect(s.tabs.map((t) => t.id)).toEqual(['a'])
    expect(s.activeId).toBe('a')
  })

  it('已存在的 tab 保留原位置，只切激活', () => {
    const before = stateOf('a', 'b', 'c')
    const after = openTab(before, meta('a', '新标题'))
    expect(after.tabs.map((t) => t.id)).toEqual(['a', 'b', 'c'])
    expect(after.activeId).toBe('a')
  })

  it('重开时刷新元信息（路径 / 标题可能变了）', () => {
    const after = openTab(stateOf('a'), meta('a', '重命名'))
    expect(after.tabs[0]?.title).toBe('重命名')
  })
})

describe('activateTab', () => {
  it('激活存在的 tab', () => {
    expect(activateTab(stateOf('a', 'b'), 'b').activeId).toBe('b')
  })

  it('不存在的 id 不改变状态（深链接到已关掉的会话）', () => {
    const before = stateOf('a')
    expect(activateTab(before, 'ghost')).toBe(before)
  })
})

describe('closeTab —— 索引必须在过滤前取', () => {
  it('关掉当前 tab 落在原位置的右邻', () => {
    const r = closeTab(stateWith('b', 'a', 'b', 'c'), 'b')
    expect(r.state.tabs.map((t) => t.id)).toEqual(['a', 'c'])
    expect(r.nextActive).toBe('c')
  })

  it('关掉最后一个落在左邻', () => {
    const r = closeTab(stateWith('c', 'a', 'b', 'c'), 'c')
    expect(r.nextActive).toBe('b')
  })

  it('关掉唯一的 tab 时没有落点', () => {
    const r = closeTab(stateOf('a'), 'a')
    expect(r.state.tabs).toEqual([])
    expect(r.nextActive).toBeNull()
    expect(r.state.activeId).toBeNull()
  })

  it('关掉的不是当前 tab：激活者不变', () => {
    const r = closeTab(stateWith('c', 'a', 'b', 'c'), 'a')
    expect(r.state.activeId).toBe('c')
    expect(r.nextActive).toBe('c')
  })

  it('关一个不存在的 id 是空操作', () => {
    const before = stateOf('a')
    const r = closeTab(before, 'ghost')
    expect(r.state).toBe(before)
    expect(r.nextActive).toBe('a')
  })
})

describe('neighborId', () => {
  const s = stateOf('a', 'b', 'c')

  it('←→ 到相邻项，到头不绕回', () => {
    expect(neighborId(s, 'a', 'next')).toBe('b')
    expect(neighborId(s, 'b', 'prev')).toBe('a')
    expect(neighborId(s, 'a', 'prev')).toBe('a')
    expect(neighborId(s, 'c', 'next')).toBe('c')
  })

  it('Home / End 到两端', () => {
    expect(neighborId(s, 'b', 'first')).toBe('a')
    expect(neighborId(s, 'b', 'last')).toBe('c')
  })

  it('不存在的 id 没有邻居', () => {
    expect(neighborId(s, 'ghost', 'next')).toBeNull()
  })
})

describe('sessionStorage 持久化', () => {
  function fakeStorage(initial: Record<string, string> = {}): Storage {
    const map = new Map(Object.entries(initial))
    return {
      get length() {
        return map.size
      },
      clear: () => map.clear(),
      getItem: (k: string) => map.get(k) ?? null,
      key: (i: number) => [...map.keys()][i] ?? null,
      removeItem: (k: string) => void map.delete(k),
      setItem: (k: string, v: string) => void map.set(k, v),
    }
  }

  it('写回后能读回来', () => {
    const store = fakeStorage()
    const s = stateOf('a', 'b')
    saveTabs(store, s)
    const back = loadTabs(store)
    expect(back.tabs.map((t) => t.id)).toEqual(['a', 'b'])
  })

  it('只存 tab 元信息，不存 activeId（刷新后激活第一个）', () => {
    const store = fakeStorage()
    saveTabs(store, { tabs: [meta('a'), meta('b')], activeId: 'b' })
    expect(loadTabs(store).activeId).toBe('a')
  })

  it('坏数据当作「没有」——视图状态坏了就该重来', () => {
    expect(loadTabs(fakeStorage({ [STORAGE_KEY]: 'not json' })).tabs).toEqual([])
    expect(loadTabs(fakeStorage({ [STORAGE_KEY]: '{"tabs":1}' })).tabs).toEqual([])
    expect(loadTabs(fakeStorage({ [STORAGE_KEY]: '[{"id":1}]' })).tabs).toEqual([])
  })

  it('起点两字段随 tab 存取往返', () => {
    const store = fakeStorage()
    const withBase: TabMeta = {
      ...meta('a'),
      baseRev: '3f2a1b0c4d5e6f708192a3b4c5d6e7f809a1b2c3',
      baseRevStatus: 'ok',
    }
    const noBase: TabMeta = { ...meta('b'), baseRev: null, baseRevStatus: 'not_a_repo' }
    saveTabs(store, { tabs: [withBase, noBase], activeId: 'a' })

    const back = loadTabs(store)
    // **整对象相等**，不是只比 `id`：只比 id 的话，字段在写入时被丢掉也照样绿。
    expect(back.tabs).toEqual([withBase, noBase])
    expect(back.tabs[0]?.baseRev).toBe(withBase.baseRev)
    expect(back.tabs[0]?.baseRevStatus).toBe('ok')
    expect(back.tabs[1]?.baseRevStatus).toBe('not_a_repo')
  })

  it('v0.6.0 之前写下的 tab 缺起点字段：读成 unknown，不崩、不丢 tab', () => {
    // 升级前 localStorage 里的真实形态：**没有** `baseRev` / `baseRevStatus`。
    // 若让 `isTabMeta` 把它们当必填，这里会读到空列表——「升级一次，所有
    // 打开的 tab 静默消失」。那时也确实没有记录起点，所以 `unknown` 是对的，
    // 不是「解析失败随便填一个」。
    const legacy = [
      { id: 'a', title: 'alpha', path: '/srv/alpha', nodeId: 'n_1' },
      { id: 'b', title: 'beta', path: '/srv/beta', nodeId: 'n_2' },
    ]
    const back = loadTabs(fakeStorage({ [STORAGE_KEY]: JSON.stringify(legacy) }))
    expect(back.tabs.map((t) => t.id), '老 tab 不能被丢掉').toEqual(['a', 'b'])
    for (const t of back.tabs) {
      expect(t.baseRev).toBeNull()
      expect(t.baseRevStatus).toBe('unknown')
    }
  })

  it('起点字段是垃圾值（不是五态之一）时归 unknown，不把垃圾带上页面', () => {
    // 反向验证：把 `normalizeTabMeta` 的兜底删掉（直接透传原始值），
    // 这里会读到 'okay' 并一路进 `data-base-status` 与文案分档。
    const weird = [{ id: 'a', title: 'alpha', path: '/srv/alpha', nodeId: 'n_1', baseRev: 7, baseRevStatus: 'okay' }]
    const back = loadTabs(fakeStorage({ [STORAGE_KEY]: JSON.stringify(weird) }))
    expect(back.tabs).toHaveLength(1)
    expect(back.tabs[0]?.baseRevStatus).toBe('unknown')
    expect(back.tabs[0]?.baseRev, '`baseRev` 不是字符串就当没有').toBeNull()
  })

  it('没有 storage（隐私模式 / SSR）时回落到 fallback', () => {
    expect(loadTabs(null, [meta('a')]).tabs.map((t) => t.id)).toEqual(['a'])
    expect(() => saveTabs(null, stateOf('a'))).not.toThrow()
  })

  it('写失败不抛（配额满时只活在这一页）', () => {
    const store = fakeStorage()
    store.setItem = () => {
      throw new Error('QuotaExceededError')
    }
    expect(() => saveTabs(store, stateOf('a'))).not.toThrow()
  })
})
