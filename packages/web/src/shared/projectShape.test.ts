import { describe, expect, it } from 'vitest'

import { SHAPE_LABELS, describeShape } from '@/shared/projectShape'

describe('项目形态的中文名只有一份', () => {
  it('三种形态的名字与 `PRODUCT.md` §3.2 的用词一致', () => {
    // 判红方式：把某一个名字改一个字（`中心式` → `集中式`）→ 红。
    // 这条不是抄实现：期望值字面写在用例里，正是为了在词表被改错时挡住。
    expect(SHAPE_LABELS).toEqual({
      central: '中心式',
      mirror: '镜像式',
      isolated: '隔离式',
    })
  })

  it('未知形态返回原文而不是空白', () => {
    // 后端 `shape` 是开放字符串；加了第四种形态时页面必须仍能显示点什么。
    // 判红方式：把 `describeShape` 改成查不到就返回 `''` → 红。
    expect(describeShape('central')).toBe('中心式')
    expect(describeShape('future-shape')).toBe('future-shape')
    expect(describeShape('')).toBe('')
  })

  it('形态名只从 `@/shared/projectShape` 取（词表本身不许有第二份）', async () => {
    // 源码级守卫。判红方式：把 `const SHAPE_LABELS = {...}` 里三条手写名
    // 加回任何一个 `.vue` 的**代码**（不是注释、不是散文）→ 红。
    //
    // 口径写清：形态名在散文里出现是**合法且必要**的（「隔离式项目只能绑在
    // 指定设备上」这类说明句说的就是这件事），所以判据是「模板里直接写字面量
    // 当显示值」——即 `.vue` 里出现 `'中心式'` 这样带引号的字面量。
    const files = import.meta.glob('/src/**/*.vue', { query: '?raw', import: 'default', eager: true })
    const offenders: string[] = []
    for (const [path, src] of Object.entries(files)) {
      if (typeof src !== 'string') continue
      if (/['"`](中心式|镜像式|隔离式)['"`]/.test(src)) offenders.push(path)
    }
    expect(offenders, '形态名必须从 `@/shared/projectShape` 取，不许在页面里手抄').toEqual([])
  })
})
