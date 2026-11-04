/* ===========================================================================
   骨架页的门禁：每一条骨架路由都**指向一个真的页面组件**，且页面顶部写明
   它在哪个版本开放。
   ---------------------------------------------------------------------------
   v0.7.0 子任务 8 的判据是「照着原型点一遍，每一页都点得到、层级对得上」。
   `router.test.ts` 盯的是路由表与匹配；本文件盯的是**骨架页本身没有偷懒**：

   1. 每个骨架页都用了 `SkeletonPage`（而不是一个空 div）——空 div 在页面上
      是一块空白，用户分不清「还没做」和「加载失败」。
   2. 每个骨架页都给了 `version`，且形如 `v0.8.0`。只写「敬请期待」等于把
      「什么时候有」留给用户猜。
   3. `SkeletonPage` 自己把 `version` **渲染出来**：prop 传了而模板没用它，
      是最容易发生的假实现——测试读源码看得见 prop，用户看不见。
   4. 空态文案是中文且是具体句子，不是 `TODO` / `暂无数据`。
   =========================================================================== */

import { readdirSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

import { readRepoFile } from '@/test/css'

/** 本文件在 `src/domains/shared/components/`，往上**三级**才是 `src`。 */
const SRC = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')

/** 读过仓库里一个前端文件的原文（参数相对仓库根）。 */
function read(relative: string): string {
  return readRepoFile(relative)
}

/** `src` 下所有 .vue 的路径（相对仓库根）。 */
function allVue(): string[] {
  const out: string[] = []
  const walk = (dir: string, prefix: string): void => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const rel = `${prefix}/${e.name}`
      if (e.isDirectory()) walk(join(dir, e.name), rel)
      else if (e.name.endsWith('.vue')) out.push(rel)
    }
  }
  walk(SRC, 'packages/web/src')
  return out
}

/** 用了 `SkeletonPage` 的页面。 */
function skeletonPages(): { path: string; src: string }[] {
  return allVue()
    .map((path) => ({ path, src: read(path) }))
    .filter(({ src }) => src.includes('SkeletonPage'))
    // 组件自己不算页面。
    .filter(({ path }) => !path.endsWith('SkeletonPage.vue'))
}

describe('骨架页：每一页都写明什么时候有', () => {
  it('确实存在多个骨架页（不是一条断言在空集合上恒真）', () => {
    // 反向验证：把所有骨架页删掉，这条会红在「骨架页不见了」。
    //
    // **这个数会随着骨架页被实现而下降**，下降本身是好事，不是回归。
    // v0.7.0 收口时 `BindPage` 与 `ProjectSettingsPage` 都已实现（原本是 13 个里的两个），
    // 剩 11 个：v0.8.0 七页、v0.9–12 四页。
    // 改这个数时必须同时确认**少的那一页是真的实现了**，而不是被谁删掉了。
    expect(skeletonPages().length).toBeGreaterThanOrEqual(11)
  })

  it('每个骨架页都给了 `version`，且形如 vX.Y.Z', () => {
    for (const { path, src } of skeletonPages()) {
      const m = src.match(/version="([^"]+)"/)
      expect(m, `${path} 没有给 version：用户不知道这页什么时候有`).not.toBeNull()
      expect((m as RegExpMatchArray)[1], `${path} 的 version 形状不对`).toMatch(/^v\d+\.\d+\.\d+$/)
    }
  })

  it('`version` 真的被渲染出来（prop 传了而模板没用 = 假实现）', () => {
    const src = read('packages/web/src/domains/shared/components/SkeletonPage.vue')
    // 模板里必须出现 {{ version }}，而不是只在 props 声明里出现。
    expect(src).toContain('{{ version }}')
  })

  it('每个骨架页都写了原型文件名，便于实现者对照', () => {
    for (const { path, src } of skeletonPages()) {
      expect(src, `${path} 没写 prototype=`).toMatch(/prototype="[^"]+\.html"/)
    }
  })

  it('空态文案是具体中文句子，不是 TODO / 暂无数据', () => {
    const empty = read('packages/web/src/domains/shared/components/SkeletonPage.vue')
    expect(empty).toContain('这一页还没有可显示的内容')
    // 反向验证：出现这些占位词说明有人在偷懒。
    for (const bad of ['TODO', 'FIXME', '暂无数据', 'Coming soon']) {
      expect(empty, `SkeletonPage 里出现了占位词「${bad}」`).not.toContain(bad)
    }
  })
})

describe('骨架页的版本与里程碑一致', () => {
  /**
   * 每一页声明的版本必须是**本里程碑或之后**：写成 `v0.6.0` 意味着「早就该有
   * 了」，而它现在是空的——那是一个自相矛盾的承诺。
   */
  it('没有一页声称自己在 v0.7.0 之前就开放', () => {
    const minor = (v: string): number => Number(/^v0\.(\d+)\./.exec(v)?.[1] ?? -1)
    for (const { path, src } of skeletonPages()) {
      const v = src.match(/version="(v[^"]+)"/)?.[1] ?? ''
      expect(minor(v), `${path} 声称 ${v} 开放，但 v0.7.0 时它还是空的`).toBeGreaterThanOrEqual(7)
    }
  })
})
