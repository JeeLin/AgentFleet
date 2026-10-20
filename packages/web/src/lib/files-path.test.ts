/* ===========================================================================
   files-path 的测试：页面与面板共用的那份越界 / cwd 判据
   ---------------------------------------------------------------------------
   这两条判据以前在 `SessionPage` 与 `FilesPanel` 里各写一份，逐字相同——
   重复本身不是问题，**漂**才是：两份里改了一份，同一条路径会出现「面板说
   越界、页面说能进」的自相矛盾。所以这里锁的是口径，不是实现细节。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import { cwdPathOf, isOutsideSeg } from '@/lib/files-path'

describe('isOutsideSeg', () => {
  it('认 `..` 与绝对路径，两种明写在路径里的逃逸', () => {
    expect(isOutsideSeg('..')).toBe(true)
    expect(isOutsideSeg('/etc')).toBe(true)
    expect(isOutsideSeg('/')).toBe(true)
  })

  it('普通层名、含点的层名、空层名都不是逃逸（不靠名字猜链接）', () => {
    for (const seg of ['src', '..hidden', 'a..b', '', 'sub/dir']) {
      expect(isOutsideSeg(seg)).toBe(false)
    }
  })
})

describe('cwdPathOf', () => {
  it('每层一个数组项，拼成相对根的路径', () => {
    expect(cwdPathOf(['src', 'lib'])).toBe('src/lib')
    expect(cwdPathOf([])).toBe('')
  })

  it('空段不进栈（`a//b` 在 node 侧是另一个目录）', () => {
    expect(cwdPathOf(['a', '', 'b'])).toBe('a/b')
    expect(cwdPathOf(['', ''])).toBe('')
  })
})
