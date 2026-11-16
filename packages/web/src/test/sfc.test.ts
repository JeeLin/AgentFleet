/* ===========================================================================
   `test/sfc.ts` 的自测
   ---------------------------------------------------------------------------
   为什么「助手也要测」：`test/sfc.ts` 是 v0.8.1 对齐门禁的**判据本体**。
   判据本身有假阳性时，下游三条门禁会一起假绿，而红不回来的门禁比没有门禁
   更坏（它把「没测」写成「测过了」）。

   本文件的目标是让 `test/sfc.ts` 的每条实现细节都能**单独**变红：

     用例 1  取段范围对（不含 `<script>` / `<style>`
     用例 2  **HTML 注释里的类名不算命中** ← 本文件的核心防假阳性用例
     用例 3  `<style>` 块内容不混进模板
     用例 4  单个根节点 / 多个根节点（含缩进的嵌套 `<template>`）都不崩
     用例 5  `:class` 动态绑定不算静态类名
     用例 6  **与 `vue/compiler-sfc` 对拍**：仓库里每一个含模板的 `.vue`
             逐字节比对取段结果

   用例 6 是「取段实现有没有静默截断」的唯一硬判据。第一版按行首
   `</template>` 定位收尾，在 `ProjectTaskPage` / `BindPage` /
   `DeviceDetailPage` 上静默截断（只取到 1745 / 567 / 583 字符），
   而 `af-body` 恰好都在被截掉的尾部——门禁会红在一个假的原因上。
   =========================================================================== */

import { globSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'
import { parse } from 'vue/compiler-sfc'

import { readRepoFile } from '@/test/css'
import { staticClassesOf, stripHtmlComments, templateOf } from '@/test/sfc'

/** `packages/web`（vitest 的 cwd 是它，但用绝对路径更不依赖 cwd）。 */
const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..')

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

const SINGLE_ROOT = `<script setup lang="ts">
const n = 1
</script>

<template>
  <div class="af-body af-body--pad">
    <header class="af-head">
      <h1 class="af-head__title">{{ n }}</h1>
    </header>
  </div>
</template>

<style scoped>
.af-body { padding: 12px; }
</style>
`

/** 类名**只**出现在 HTML 注释里 —— 全文子串搜索会假阳性命中。 */
const COMMENTED_OUT = `<template>
  <!-- 旧骨架留档：class="af-body" 与 class="af-head" 已迁走 -->
  <div class="af-page">
    <!--
      多行注释里的类名同样不算：
      <div class="af-main">
    -->
    <p>正文</p>
  </div>
</template>
`

const MULTI_ROOT = `<template>
  <header class="af-head af-head--page">
    <h1 class="af-head__title">标题</h1>
  </header>
  <div class="af-body">
    <template v-if="cond">
      <div class="af-region">嵌套 template 不是 SFC 段的收尾</div>
    </template>
  </div>
  <footer class="af-foot">尾巴</footer>
</template>

<script setup lang="ts">
const cond = true
const s = '</template>' // 脚本里的字符串也不是收尾
</script>
`

const DYNAMIC_CLASS = `<template>
  <div class="af-main" :class="[on ? 'af-main--wide' : '']">
    <span v-bind:class="{ 'af-dyn': on }"></span>
  </div>
</template>
`

/** 自带的同行收尾（同一行既开又闭）：按「行首 `</template>`」数会把深度数错。 */
const INLINE_CLOSE = `<template>
  <div class="af-body">
    <p v-if="a">
      <template v-if="b">一</template>
      <template v-else>二</template>
    </p>
    <span class="af-tail">尾部</span>
  </div>
</template>
`

/** 注释里写着 `</template>`：参与配对就会提前收尾。 */
const COMMENT_WITH_CLOSE = `<template>
  <!-- 旧写法在这收尾：</template> 已作废 -->
  <div class="af-body"></div>
  <span class="af-tail">尾部</span>
</template>
`

// ---------------------------------------------------------------------------
// 用例 1：取段范围
// ---------------------------------------------------------------------------

describe('templateOf：只取 <template> 段', () => {
  it('取出模板内容，不含 <script> 与 <style>', () => {
    const t = templateOf(SINGLE_ROOT)
    expect(t).toContain('<div class="af-body af-body--pad">')
    expect(t).toContain('{{ n }}')
    // 三个块必须互不串味
    expect(t, '<script> 内容混进了模板').not.toContain('const n = 1')
    expect(t, '<style> 内容混进了模板').not.toContain('padding: 12px')
    // 开闭标签本身不进结果：否则「模板里有 af-body」会因为标签行也含
    // `<template>` 之外的东西而难以定位真因
    expect(t).not.toContain('<template>')
  })

  it('没有 <template> 段时报错，而不是静默返回空串', () => {
    expect(() => templateOf('<script setup>\nconst x = 1\n</script>\n')).toThrow(
      /没有顶层 <template>/,
    )
  })
})

// ---------------------------------------------------------------------------
// 用例 2：**核心防假阳性** —— HTML 注释里的类名不算命中
// ---------------------------------------------------------------------------

describe('templateOf：剥掉 HTML 注释（防「类名只写在注释里」假阳性）', () => {
  it('单行注释里的 class="af-body" 不出现在提取结果里', () => {
    const t = templateOf(COMMENTED_OUT)
    expect(t, '单行 HTML 注释没被剥掉：类名只写在注释里也会被当成「页面在用」').not.toContain(
      'af-body',
    )
    expect(t).not.toContain('af-head')
    expect(t).not.toContain('旧骨架留档')
  })

  it('多行注释里的类名同样不出现', () => {
    const t = templateOf(COMMENTED_OUT)
    expect(t, '多行 HTML 注释没被剥掉').not.toContain('af-main')
  })

  it('剥注释不误伤真内容：注释外的东西还在', () => {
    const t = templateOf(COMMENTED_OUT)
    expect(t).toContain('class="af-page"')
    expect(t).toContain('正文')
  })

  it('stripHtmlComments 单独可用，且只剥注释', () => {
    expect(stripHtmlComments('a<!-- class="x" -->b')).toBe('ab')
    expect(stripHtmlComments('<!--\n多行\n-->正文')).toBe('正文')
  })
})

// ---------------------------------------------------------------------------
// 用例 3：<style> 不混进模板
// ---------------------------------------------------------------------------

describe('templateOf：<style> 块不进模板', () => {
  it('模板之后写的 <style> 选择器不会混进来', () => {
    const t = templateOf(SINGLE_ROOT)
    expect(t, '.af-body 这类选择器出现在模板结果里：说明取段越界到了 <style>').not.toMatch(
      /\.af-body\s*\{/,
    )
    expect(t).not.toContain('padding')
  })

  it('模板之前写的 <style> 也不会混进来', () => {
    const src = `<style scoped>
.af-head { color: red; }
</style>

<template>
  <div class="af-main"></div>
</template>
`
    const t = templateOf(src)
    expect(t).toContain('class="af-main"')
    expect(t, '模板前的 <style> 混进来了').not.toContain('.af-head {')
  })
})

// ---------------------------------------------------------------------------
// 用例 4：单根 / 多根 / 嵌套 template 都不崩
// ---------------------------------------------------------------------------

describe('templateOf：根节点形态不敏感', () => {
  it('单个根节点', () => {
    expect(() => templateOf(SINGLE_ROOT)).not.toThrow()
    expect(templateOf(SINGLE_ROOT).trim().startsWith('<div')).toBe(true)
  })

  it('多个根节点 + 缩进的嵌套 <template>（Vue 3 允许，取段不许提前收尾）', () => {
    const t = templateOf(MULTI_ROOT)
    expect(() => templateOf(MULTI_ROOT)).not.toThrow()
    // 三个根节点全在结果里：若取段在第一个缩进 `</template>` 处收尾，footer 会丢
    expect(t).toContain('af-head')
    expect(t).toContain('af-region')
    expect(t, '取段在嵌套 </template> 处提前收尾了').toContain('<footer class="af-foot">')
    // 脚本在模板之后，且脚本里有字符串 `</template>`：不许被它带偏
    expect(t).not.toContain('const cond')
  })

  it('同一行内既开又闭的 <template> 不影响收尾定位', () => {
    // 只认「行首的闭标签」会在这里数错深度（GitDiff.vue:79 就是这样）。
    const t = templateOf(INLINE_CLOSE)
    expect(t, '取段被同行收尾的 <template> 带偏，尾部丢了').toContain('<span class="af-tail">')
  })

  it('注释里的 </template> 不参与配对（否则提前收尾）', () => {
    const t = templateOf(COMMENT_WITH_CLOSE)
    expect(t, '注释里的 </template> 被当成段收尾').toContain('<span class="af-tail">')
    expect(t).not.toContain('旧写法在这收尾')
  })

  it('剥注释只剥一遍：不会被嵌套的 `<!-- <!-- -->` 多吞内容（假阴性）', () => {
    // HTML 注释不嵌套：第一个 `-->` 就结束。剥到不动点会把后面的
    // `class="af-body"` 也一起吞掉，于是「注释外的东西」被误删。
    expect(stripHtmlComments('<!-- <!-- --> class="af-body"')).toBe(' class="af-body"')
  })
})

// ---------------------------------------------------------------------------
// 用例 6：与 vue/compiler-sfc 对拍（「取段不许静默截断」的硬判据）
// ---------------------------------------------------------------------------

describe('templateOf：与 vue/compiler-sfc 的 template 块逐字节一致', () => {
  it('仓库里每个含顶层 <template> 的 .vue 都对得上（忽略注释：本助手刻意剥注释）', () => {
    const files = globSync('src/**/*.vue', { cwd: WEB_ROOT }).sort()
    expect(files.length, '一个 .vue 都没找到 —— 对拍会退化成恒真').toBeGreaterThan(40)

    const mismatched: string[] = []
    for (const rel of files) {
      const source = readFileSync(resolve(WEB_ROOT, rel), 'utf-8')
      const { descriptor, errors } = parse(source, { filename: rel })
      if (errors.length > 0) continue // 解析不了的文件不参与对拍
      const raw = (descriptor.template?.content ?? '').trim()
      if (raw === '') continue
      // 期望值也按同一口径剥注释后再比：本助手的**唯一**额外处理就是剥注释，
      // 其余（取段范围）必须与 Vue 编译器逐字符一致。不剥注释就比，比出来的是
      // 「注释条数不一样」，与「取段有没有截断」无关。
      const expected = stripHtmlComments(raw).trim()
      const actual = templateOf(source).trim()
      if (actual !== expected) {
        mismatched.push(`${rel}（mine ${actual.length} 字符 / sfc ${expected.length} 字符）`)
      }
    }
    expect(
      mismatched,
      [
        'templateOf 取到的模板段与 vue/compiler-sfc 不一致 —— 取段范围错了。',
        '取段偏短时，落在尾部的骨架类会被判成「页面没用」，门禁红在假的原因上：',
        ...mismatched,
      ].join('\n'),
    ).toEqual([])
  })
})

// ---------------------------------------------------------------------------
// 用例 5：静态类名 vs 动态绑定
// ---------------------------------------------------------------------------

describe('staticClassesOf：只认静态 class="…"', () => {
  it('静态类名全部收齐', () => {
    const classes = staticClassesOf(DYNAMIC_CLASS)
    expect(classes.has('af-main')).toBe(true)
  })

  it(':class / v-bind:class 里的类名不算（门禁口径：须有静态兜底）', () => {
    const classes = staticClassesOf(DYNAMIC_CLASS)
    expect(classes.has('af-main--wide'), ':class 数组里的类名被当成静态类名了').toBe(false)
    expect(classes.has('af-dyn'), 'v-bind:class 里的类名被当成静态类名了').toBe(false)
  })

  it('注释里的 class="…" 不进类名集合', () => {
    const classes = staticClassesOf(COMMENTED_OUT)
    expect(classes.has('af-body')).toBe(false)
    expect(classes.has('af-head')).toBe(false)
    expect(classes.has('af-page')).toBe(true)
  })

  it('对真实的 SFC 也能用（BoardPage 的静态骨架类）', () => {
    const classes = staticClassesOf(readRepoFile('packages/web/src/domains/project/BoardPage.vue'))
    expect(classes.has('af-body')).toBe(true)
    expect(classes.has('af-head')).toBe(true)
  })
})
