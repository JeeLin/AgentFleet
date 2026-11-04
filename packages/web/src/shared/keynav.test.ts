import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { afterEach, describe, expect, it } from 'vitest'

import {
  KEYS,
  PREFIX,
  PREFIX_LEAD,
  PREFIX_MS,
  SCOPE_LABEL,
  actCount,
  actForKey,
  acts,
  bindingOf,
  declareImplemented,
  dispatch,
  flatKeys,
  hasAct,
  hasHandler,
  hintOf,
  isActivatable,
  isTyping,
  missingActs,
  onHandlersChanged,
  register,
  registerInternal,
  unimplemented,
} from './keynav'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../..')
const DESIGN_DOC = resolve(REPO_ROOT, 'docs/design-system.md')
// 校验我们**自己维护**的资产，不是原型那一份：原型是上游参照，
// 它的数字变了不该让前端门禁变红，反之我们的资产漂了就该红。
const TOKENS_CSS = resolve(REPO_ROOT, 'packages/web/src/styles/tokens.css')
const COMPONENTS_CSS = resolve(REPO_ROOT, 'packages/web/src/styles/components.css')

function keyEvent(key: string, init: KeyboardEventInit = {}): KeyboardEvent {
  return new KeyboardEvent('keydown', { key, cancelable: true, ...init })
}

describe('KEYS 与设计文档一致（grep 校验，不靠人眼）', () => {
  it('act 条目数与 design-system.md §4.2 表格里的「N 个 act」一致', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    const m = doc.match(/当前键位表（(\d+) 个 act）/)
    expect(m, 'design-system.md §4.2 的「当前键位表（N 个 act）」标题不见了').not.toBeNull()
    const declared = Number(m?.[1])
    expect(declared).toBeGreaterThan(0)
    expect(actCount()).toBe(declared)
  })

  it('§4.2 表格里的每个 act 都出现在 KEYS 里（文档写了、表里没有 = 死键）', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    const section = doc.slice(doc.indexOf('### 4.2'), doc.indexOf('### 4.3'))
    // 表格第三列是 act，形如 `nav:overview`；首列是键位（`| **全局** | ...`）。
    const inTable = new Set<string>()
    for (const line of section.split('\n')) {
      if (!line.startsWith('|')) continue
      const cells = line.split('|').map((c) => c.trim().replace(/\*\*/g, ''))
      for (const cell of cells) {
        for (const m of cell.matchAll(/\b([a-z]+:[a-z-]+)\b/g)) inTable.add(m[1] as string)
      }
    }
    expect(inTable.size).toBeGreaterThan(0)
    for (const act of inTable) {
      expect(hasAct(act), `§4.2 有 ${act}，KEYS 表里没有`).toBe(true)
    }
  })

  it('KEYS 里的每个 act 都在 §4.2 表格里（表里有、文档没写 = 用户查得到的承诺缺文档）', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    const section = doc.slice(doc.indexOf('### 4.2'), doc.indexOf('### 4.3'))
    for (const act of acts()) {
      expect(section, `${act} 在 KEYS 里，design-system.md §4.2 没写`).toContain(act)
    }
  })

  it('表内每个 act 恰好有一个定义（alias 键位共用一个 act，不是两个动作）', () => {
    expect(actCount()).toBe(new Set(acts()).size)
  })

  it('键位描述在表内唯一：两个 act 不许共用同一个键位', () => {
    const seen = new Map<string, string>()
    for (const k of flatKeys()) {
      const desc = k.keys.join(' ')
      const owner = seen.get(desc)
      expect(owner, `键位 ${desc} 被 ${owner} 与 ${k.act} 共用`).toBeUndefined()
      seen.set(desc, k.act)
    }
  })

  it('每个 act 的 scope 与它所在组声明的 scope 一致', () => {
    for (const g of KEYS) {
      for (const k of g.keys) expect(k.scope, `${k.act} 的 scope 与所属组不符`).toBe(g.scope)
    }
  })

  it('每一组都非空，且都有中文标签', () => {
    expect(KEYS.length).toBeGreaterThan(0)
    for (const g of KEYS) {
      expect(g.keys.length).toBeGreaterThan(0)
      expect(SCOPE_LABEL[g.scope], `${g.group} 的 scope ${g.scope} 没有标签`).toBeTruthy()
    }
  })
})


describe('token / 组件数与设计文档一致', () => {
  it('tokens.css 的变量数与 §1 的「129 个变量」一致', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    const m = doc.match(/（(\d+) 个变量）/g)
    expect(m, 'design-system.md §1 不再声明 token 变量数').not.toBeNull()
    const declared = Number(/(\d+)/.exec(m?.[0] as string)?.[1])
    const css = readFileSync(TOKENS_CSS, 'utf-8')
    const actual = (css.match(/^\s*--af-[a-z0-9-]+:/gm) ?? []).length
    expect(actual).toBe(declared)
  })

  it('components.css 的小节数与 §3 的「23 组组件（23 节）」一致', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    const m = doc.match(/（23 节/)
    expect(m, 'design-system.md §3 不再声明组件节数').not.toBeNull()
    const css = readFileSync(COMPONENTS_CSS, 'utf-8')
    // 小节标题是 `   3. Button` 这种「行首缩进 + 序号 + 点 + 标题」的形状。
    const sections = (css.match(/^\s+\d+\.\s+\S/gm) ?? []).length
    expect(sections).toBe(23)
  })

  it('每个 token 只在 :root 里定义一次，重复出现的名字只在 reduce-motion 里覆写', () => {
    // 文档里的 129 是「文件里的定义数」，不是「不同变量名数」：5 个动效时长
    // 在 `prefers-reduced-motion` 下被覆写一遍（规范 §2 要求动效可关）。
    // 真正的 bug 形态是**同一个名字在 :root 里定义两次**——那让「后者生效」
    // 变成一条没人读到的隐藏规则。所以这里分开量。
    const css = readFileSync(TOKENS_CSS, 'utf-8')
    const all = [...css.matchAll(/^\s*(--af-[a-z0-9-]+):/gm)].map((m) => m[1] as string)
    const rootBlock = /:root\s*\{([\s\S]*?)\n\}/.exec(css)?.[1] ?? ''
    const inRoot = [...rootBlock.matchAll(/^\s*(--af-[a-z0-9-]+):/gm)].map((m) => m[1] as string)
    expect(new Set(inRoot).size).toBe(inRoot.length)
    expect(all.filter((n) => !inRoot.includes(n))).toEqual([])
  })
})

describe('register', () => {
  it('拒绝表外 act，并说明该先把键位加进 KEYS', () => {
    expect(() => register('nope:nope', () => true)).toThrow(/不在 KEYS 表里/)
  })

  it('拒绝重复注册', () => {
    registerInternal('help:toggle', () => true)
    expect(() => register('help:toggle', () => true)).toThrow(/注册了两次/)
  })

  it('注册后 hasHandler 为真，且 actCount 不变（注册不新增承诺）', () => {
    const before = actCount()
    registerInternal('rail:toggle', () => true)
    expect(hasHandler('rail:toggle')).toBe(true)
    expect(actCount()).toBe(before)
  })
})

describe('unimplemented', () => {
  it('missingActs 只列表里有、没人实现的 act', () => {
    const missing = missingActs()
    expect(missing.length).toBeGreaterThan(0)
    for (const act of missing) expect(hasAct(act)).toBe(true)
  })

  it('注册过的 act 从名单里消失', () => {
    registerInternal('search:focus', () => true)
    expect(missingActs()).not.toContain('search:focus')
    expect(unimplemented('search:focus')).toBe(false)
  })

  it('unimplemented 逐条回答「这个 act 实现了吗」', () => {
    expect(unimplemented('search:focus')).toBe(false)
    expect(unimplemented('nav:butler')).toBe(true)
  })

  it('声明为已实现（实现不住在全局表里）的 act 不算缺失', () => {
    declareImplemented('list:move')
    expect(unimplemented('list:move')).toBe(false)
    expect(missingActs()).not.toContain('list:move')
  })

  it('声明表外 act 抛错（不许有用户查不到的承诺）', () => {
    expect(() => declareImplemented('nope:nope')).toThrow(/不在 KEYS 表里/)
  })

  it('handler 表变化时通知订阅者（? 面板据此重画）', () => {
    let notified = 0
    const off = onHandlersChanged(() => {
      notified += 1
    })
    registerInternal('panel:prev', () => true)
    expect(notified).toBeGreaterThan(0)
    off()
    const seen = notified
    registerInternal('panel:next', () => true)
    expect(notified).toBe(seen)
  })
})

describe('dispatch', () => {
  it('没有 handler 时返回 false 且不吞键', () => {
    const e = keyEvent('t')
    expect(dispatch('traj:toggle', e)).toBe(false)
    expect(e.defaultPrevented).toBe(false)
  })

  it('handler 返回 true 时吞键', () => {
    registerInternal('traj:toggle', () => true)
    const e = keyEvent('t')
    expect(dispatch('traj:toggle', e)).toBe(true)
    expect(e.defaultPrevented).toBe(true)
  })

  it('handler 返回 false 时不吞键（把键还给浏览器）', () => {
    registerInternal('dock:toggle', () => false)
    const e = keyEvent('\\')
    expect(dispatch('dock:toggle', e)).toBe(false)
    expect(e.defaultPrevented).toBe(false)
  })

  it('handler 返回 undefined 视为不吞', () => {
    registerInternal('panel:open', () => undefined)
    expect(dispatch('panel:open', keyEvent('o'))).toBe(false)
  })

  it('把 data 透传给 handler', () => {
    let got: unknown
    registerInternal('list:move', (_e, data) => {
      got = data
      return true
    })
    dispatch('list:move', keyEvent('ArrowDown'), 1)
    expect(got).toBe(1)
  })

  it('不可取消的事件上不调 preventDefault（浏览器会忽略，抛错才是问题）', () => {
    registerInternal('panel:next', () => true)
    const e = new KeyboardEvent('keydown', { key: ']', cancelable: false })
    expect(() => dispatch('panel:next', e)).not.toThrow()
  })
})

describe('输入区判定', () => {
  afterEach(() => {
    document.body.innerHTML = ''
  })

  it('input / textarea / select / contenteditable 里算「在打字」', () => {
    document.body.innerHTML =
      '<input id="i"><textarea id="t"></textarea><select id="s"></select><div id="e" contenteditable="true"></div><div id="d"></div>'
    expect(isTyping(document.getElementById('i'))).toBe(true)
    expect(isTyping(document.getElementById('t'))).toBe(true)
    expect(isTyping(document.getElementById('s'))).toBe(true)
    expect(isTyping(document.getElementById('e'))).toBe(true)
    expect(isTyping(document.getElementById('d'))).toBe(false)
  })

  it('null / document 本身不算在打字', () => {
    expect(isTyping(null)).toBe(false)
    expect(isTyping(document)).toBe(false)
  })

  it('按钮与链接算「可激活」——Space 让给按钮', () => {
    document.body.innerHTML = '<button id="b"></button><a id="a"></a><div id="d"></div>'
    expect(isActivatable(document.getElementById('b'))).toBe(true)
    expect(isActivatable(document.getElementById('a'))).toBe(true)
    expect(isActivatable(document.getElementById('d'))).toBe(false)
    expect(isActivatable(null)).toBe(false)
  })
})

describe('g 序列', () => {
  // 三项导航各一个跳转键（`g s` / `g d` / `g ,`）。原先还有 `g n`（新建会话）
  // 与 `g a`（Agent 目录）——两者都已并出导航（见 design-system §5.1）。
  it('三个跳转目标都指向表内的 act，且窗口是 800ms', () => {
    expect(PREFIX_MS).toBe(800)
    expect(PREFIX.size).toBe(5)
    for (const act of PREFIX.values()) expect(hasAct(act), `${act} 不在 KEYS 表里`).toBe(true)
  })

  it('g g 指向 traj:toggle，与键位表一致', () => {
    expect(PREFIX.get('g')).toBe('traj:toggle')
    expect(bindingOf('traj:toggle')?.keys).toEqual(['t'])
  })
})

describe('actForKey / hintOf（键位 → act 的反查）', () => {
  it('单键描述反查到 act', () => {
    expect(actForKey('o')).toBe('panel:open')
    expect(actForKey('?')).toBe('help:toggle')
  })

  it('g 序列用空格连接两个键', () => {
    expect(actForKey('g s')).toBe('nav:overview')
    expect(actForKey('g g')).toBe('traj:toggle')
  })

  it('表外的键位描述返回 undefined', () => {
    expect(actForKey('z')).toBeUndefined()
    expect(actForKey('g z')).toBeUndefined()
  })

  it('hintOf 给出键位提示，act 不在表里时是空串', () => {
    expect(hintOf('panel:open')).toBe('o')
    expect(hintOf('nav:overview')).toBe('g s')
    expect(hintOf('nope:nope')).toBe('')
  })

  it('PREFIX_LEAD 是 g，且 PREFIX 覆盖所有 g 开头的键位', () => {
    expect(PREFIX_LEAD).toBe('g')
    const expected = flatKeys().filter((k) => k.keys.length === 2 && k.keys[0] === 'g')
    expect(PREFIX.size).toBe(expected.length)
  })
})