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
/** 原型那份手抄的键位表。它不是真相源，但它的漂移必须被看见（见下面那组）。 */
const PROTOTYPE_KEYNAV = resolve(REPO_ROOT, 'prototype/assets/keynav.js')

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
    // 表格第三列是 act，形如 `nav:sessions`；首列是键位（`| **全局** | ...`）。
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

describe('原型 keynav.js 与真相源的漂移（没有生成器，靠断言看着）', () => {
  /**
   * 原型那份表是**手抄**的（`prototype/*.html` 引不了 TS 与 SFC），它长这样：
   * `{ k: 'g s', act: 'nav:sessions', desc: '会话列表' },`
   *
   * 一份键位在两个文件里各写一遍 = 两个 act 共用一条键位描述的同一类错误，
   * 只是它的报错时机从「构建期抛错」退到了「用户按下去没反应」。
   */
  function prototypeBindings(): Array<{ keys: string; act: string }> {
    const js = readFileSync(PROTOTYPE_KEYNAV, 'utf-8')
    return [...js.matchAll(/\{ k: '(.+?)',\s*act: '([^']+)'/g)].map((m) => ({
      // `dock:toggle` 在原型源码里写成 `'\\'`（JS 字符串字面量转义），
      // 而生产 `KEYS` 里是 `'\'`——比的是键面不是源码，所以这里解一层。
      keys: (m[1] as string).replace(/\\\\/g, '\\'),
      act: m[2] as string,
    }))
  }

  /**
   * 两边共有的 act 的键位描述必须逐字相同。
   *
   * 原型把 `↑ ↓` 写成一栏、生产拆成 `['↑','↓']` 再 `join(' ')`，所以比较时
   * 两边都归一成「空格连接的键位串」。
   */
  it('两边共有的 act，键位描述相同（改了生产忘记同步原型 → 红）', () => {
    const proto = prototypeBindings()
    expect(proto.length, '原型键位表解析出来是空的——正则或文件布局变了').toBeGreaterThan(0)

    const mine = new Map<string, string[]>()
    for (const k of flatKeys()) {
      const list = mine.get(k.act) ?? []
      list.push(k.keys.join(' '))
      mine.set(k.act, list)
    }

    const protoKeys = new Map<string, string[]>()
    for (const b of proto) {
      const list = protoKeys.get(b.act) ?? []
      list.push(b.keys)
      protoKeys.set(b.act, list)
    }

    for (const [act, mineKeys] of mine) {
      const theirs = protoKeys.get(act)
      if (!theirs) continue
      expect(
        [...theirs].sort(),
        `act ${act} 的键位在两边不同：生产 ${mineKeys.join(' / ')}、原型 ${theirs.join(' / ')}`,
      ).toEqual([...mineKeys].sort())
    }
  })

  /**
   * 原型多出来的 act **恰好**是已登记的那两条。
   *
   * 用差集的**精确相等**而不是 `not.toContain`：`nav:new-session` 与
   * `nav:agents` 已从导航并出（design-system §5.1），它们是历史欠账、不是
   * 新的漂移；而**任何第四条**都是新欠账，测试必须红着让人去改原型。
   */
  it('原型多出来的 act 恰好是 nav:new-session 与 nav:agents 两条历史欠账', () => {
    const mine = new Set(acts())
    const extra = new Set(prototypeBindings().map((b) => b.act).filter((a) => !mine.has(a)))
    expect([...extra].sort()).toEqual(['nav:agents', 'nav:new-session'])
  })

  /**
   * `PREFIX` 是第二张表，与 `KEYS` 同源同理，也抄了两遍。
   *
   * 生产那份是从 `KEYS` 推导的（`keynav.ts` 的 `PREFIX`），原型那份是手写的
   * 字面量对象——所以它能单独漂。这里只锁「原型多出来的目标同样是那两条」。
   */
  it('原型 PREFIX 多出来的目标与 KEYS 的差集一致（两张表不许各自漂）', () => {
    const js = readFileSync(PROTOTYPE_KEYNAV, 'utf-8')
    const block = /var PREFIX = \{([\s\S]*?)\};/.exec(js)?.[1]
    expect(block, '原型 keynav.js 里找不到 `var PREFIX = {`').toBeTruthy()
    const targets = [...(block as string).matchAll(/'([a-z]+:[a-z-]+)'/g)].map((m) => m[1] as string)
    expect(targets.length).toBeGreaterThan(0)

    const extra = [...new Set(targets)].filter((a) => !hasAct(a)).sort()
    expect(extra).toEqual(['nav:agents', 'nav:new-session'])
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

  it('components.css 的小节数与 §3 的「22 组组件（22 节）」一致', () => {
    const doc = readFileSync(DESIGN_DOC, 'utf-8')
    const m = doc.match(/（22 节/)
    expect(m, 'design-system.md §3 不再声明组件节数').not.toBeNull()
    const css = readFileSync(COMPONENTS_CSS, 'utf-8')
    // 小节标题是 `   3. Button` 这种「行首缩进 + 序号 + 点 + 标题」的形状。
    const sections = (css.match(/^\s+\d+\.\s+\S/gm) ?? []).length
    expect(sections).toBe(22)
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
    expect(unimplemented('nav:sessions')).toBe(true)
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
    expect(PREFIX.size).toBe(4)
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
    expect(actForKey('g s')).toBe('nav:sessions')
    expect(actForKey('g g')).toBe('traj:toggle')
  })

  it('表外的键位描述返回 undefined', () => {
    expect(actForKey('z')).toBeUndefined()
    expect(actForKey('g z')).toBeUndefined()
  })

  it('hintOf 给出键位提示，act 不在表里时是空串', () => {
    expect(hintOf('panel:open')).toBe('o')
    expect(hintOf('nav:sessions')).toBe('g s')
    expect(hintOf('nope:nope')).toBe('')
  })

  it('PREFIX_LEAD 是 g，且 PREFIX 覆盖所有 g 开头的键位', () => {
    expect(PREFIX_LEAD).toBe('g')
    const expected = flatKeys().filter((k) => k.keys.length === 2 && k.keys[0] === 'g')
    expect(PREFIX.size).toBe(expected.length)
  })
})