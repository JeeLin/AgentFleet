/* ===========================================================================
   原型锚点守卫（v0.8.1 第5轮 G42）
   ---------------------------------------------------------------------------
   `ProjectSettingsPage.vue` 的骨架注释曾写着「照原型 `project-settings.html:14-24`：
   …内容在 `.af-body` 里」——而原型的 `:14` 是空行、`.af-body` 在 **37** 行。同批
   另外 3 页（`archived` / `project-new` / `project-bind`）的同一句也都从 14 起算，
   同样差一行。这些是**手抄的行号**，没有任何门禁看着，于是错了六轮没人发现。

   本文件是那道门：**按注释里的行号回读原型文件，断言那一行确实是注释声称的元素。**
   不是「文件存在」，也不是「行号在范围内」——必须命中具体选择器。这样下一次
   原型加一行注释导致整体下移时，红的是这条，而不是让读者继续被误导。

   为什么不扫全仓所有 `file:line`：那需要一个通配的解析器与白名单（`crates/*`、
   `docs/*.md` 都无法这样核）。这里只钉**本批**四个页头骨架锚点，范围小、
   判据硬、失败信息直接说出「第 N 行是 X，期望是 Y」。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

/** `packages/web/src/domains/project` → 仓库根下的 `prototype/`。 */
const PROTOTYPE = resolve(dirname(fileURLToPath(import.meta.url)), '../../../../..', 'prototype')

function line(file: string, n: number): string {
  return readFileSync(resolve(PROTOTYPE, file), 'utf8').split('\n')[n - 1]?.trim() ?? ''
}

/**
 * 四个项目页的骨架锚点：`[原型文件, 页头起始行, `.af-body` 行]`。
 *
 * 取值来源是 `sed -n` 回读实测，不是猜的；页面注释与测试注释都引用同一对数。
 */
const PAGES: Array<[string, number, number]> = [
  ['project-settings.html', 15, 37],
  ['archived.html', 15, 34],
  ['project-new.html', 15, 23],
  ['project-bind.html', 15, 24],
]

describe('原型锚点：页面骨架注释引用的行号必须命中', () => {
  for (const [file, headLine, bodyLine] of PAGES) {
    it(`${file}:${headLine} 是 \`header.af-head\`，:${bodyLine} 是 \`.af-body\``, () => {
      expect(line(file, headLine), `${file}:${headLine} 不是页头`).toContain(
        '<header class="af-head">',
      )
      expect(line(file, bodyLine), `${file}:${bodyLine} 不是内容区`).toContain(
        '<div class="af-body',
      )
      // 页头必须在内容区之前——顺序反了说明有人把两层写成嵌套。
      expect(headLine, '页头应先于内容区').toBeLessThan(bodyLine)
    })
  }

  it('project-settings.html:14 是空行（所以「照原型 :14-24」那句必然是错的）', () => {
    // 这条是**反向说明**：留着它，是为了让下一个读到「:14」的人一眼看到
    // 「14 行什么都没有」。上面那条已经钉住正确值。
    expect(line('project-settings.html', 14)).toBe('')
  })
})
