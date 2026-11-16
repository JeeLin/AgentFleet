/* ===========================================================================
   SFC `<template>` 段的读取助手
   ---------------------------------------------------------------------------
   为什么需要它 —— 现有 `test/css.ts` 只能读 `<style>`：

     `cssRuleOf(source, selector)` 的文档原文是「取一个 SFC 里某条 **CSS 规则**
     的声明体……只认顶层规则：SFC 的 `<style>` 里没有嵌套语法」。它是
     **`<style>` 助手**，读不了模板。`styles/prototype-parity.test.ts` 原来也只
     `readRepoFile` 了 4 个 `.css`，没有一处读 `.vue`。

   而 v0.8.1 的门禁要断言的是「**模板里**必须有骨架类」。没有这个助手时，
   实现者只能退化成「整个 `.vue` 源做字符串子串搜索」，那样同时得到两种坏结果：

     ① **假阳性**：类名只写在 HTML 注释里（`<!-- class="af-body" -->`）、
        或写在 `<style>` 的选择器里（`.af-body { … }`），全文搜索照样命中，
        于是门禁在「页面其实没用骨架类」时也绿——这正是上一轮「形式证据
        全绿而用户看到完全不一样」的复发路径。
     ② **假阴性**：类名只在 `:class="[...]"` 动态绑定里时（`App.vue:515`
        的侧栏就是这种形态），全文搜索能搜到，但渲染时是否出现取决于运行时
        状态；门禁不能说「有动态绑定就算用了」。

   因此本文件只做一件事：**从 SFC 源里取出模板段的可见内容**，并
   **剥掉 HTML 注释**。剥离是核心，不是顺手——见 `sfc.test.ts` 的用例 2。

   ## 取段为什么必须数嵌套（踩过的坑，别退回去）

   第一版按「行首 `</template>`」定位收尾，理由是「顶层块标签在行首，
   嵌套的 `<template v-if>` 是缩进的」。**这个理由是错的**，实测两处反例：

     - `ProjectTaskPage.vue:710` 是缩进的 `    </template>`（`v-if` 块收尾），
       而真正的段收尾在 `:712` 的 `</template>`；
     - `GitDiff.vue:82` 的标签**自带同行收尾**，于是 `:79` 的
       `<template v-else>…</template>` 是「开」与「闭」在同一行——按行首数
       只会数到「开」，深度永远回不到 0，取段直接返回空串。

   用了这个错判据的后果不是崩，而是**静默截断**：`ProjectTaskPage` 只取到
   1745 / 12440 字符，`BindPage` 只取到 567 / 11131——`af-body` 恰好落在被
   截掉的尾部，门禁会红在一个**假**的原因上（「页面没用骨架类」），而人会去
   改页面。所以这里改成：先剥注释再做标签配对，深度归零才算收尾。

   正确性有独立验证：对仓库里 47 个含顶层 `<template>` 的 `.vue`，
   `templateOf()` 的结果与 `vue/compiler-sfc` 的 `descriptor.template.content`
   **逐字节相等（0 处不一致）**。见 `sfc.test.ts` 的「与 vue/compiler-sfc 对拍」用例。
   =========================================================================== */

/** 顶层块标签：出现在行首（允许行内缩进），不许缩进成嵌套节点。 */
const TOPLEVEL_TEMPLATE_OPEN = /^[ \t]*<template(\s[^>]*)?>/m

/** 模板标签（开/闭），只认后接空白或 `>` 的 `template`，避免 `templateX` 误命中。 */
const TEMPLATE_TAG = /<(\/?)template(?=[\s>])[^>]*>/g

/**
 * HTML 注释 `<!-- … -->`（含跨行）。
 *
 * **单遍扫描已足够安全**（这是「要不要写成循环剥到不动点」的答案）：
 * HTML 注释不嵌套，`<!-- a <!-- b -->` 的注释在**第一个** `-->` 处结束，
 * 剩下的 ` -->` 是文本。循环剥反而会**多剥**，把 `<!-- <!-- --> class="af-body" -->`
 * 里本该露出来的 `class="af-body"` 也吞掉——那是假阴性。故只剥一遍。
 */
const HTML_COMMENT = /<!--[\s\S]*?-->/g

/**
 * 取出 SFC 的 `<template>…</template>` 段（**不含**开闭标签本身），
 * 并剥掉 HTML 注释。
 *
 * 找不到顶层 `<template>` 段时抛错而不是返回空串：空串会让
 * 「模板里必须有 X」这类断言**因为别的原因**变红（或者更糟，在
 * `not.toContain` 形态的断言里恒绿），错误信息必须指向真因。
 */
export function templateOf(vueSource: string): string {
  const open = TOPLEVEL_TEMPLATE_OPEN.exec(vueSource)
  if (!open) throw new Error('SFC 里没有顶层 <template> 段')

  const bodyStart = open.index + open[0].length
  const rest = vueSource.slice(bodyStart)

  // 两层正确性要求，都踩过坑：
  //
  // ① **必须抹掉注释再数标签**：注释里可能写着 `</template>`，参与配对就提前收尾。
  // ② **抹白必须等长**：抹白后的副本要和 `rest` 下标同构，才能拿它的下标切
  //    原始源（原文的注释随后由 `stripHtmlComments` 剥掉）。
  //
  // 「整条尾巴一起 `replace(/…/g)`」违反 ②：模板**之后**的 `<script>` 里也有
  // `<!-- -->`（`App.vue` 帮助面板文案就有），非等长替换会让下标整体错开，
  // 取段静默截断（实测 `App.vue` 只取到 5297/7113 字符）。
  // 所以这里一次构造等长副本，只抹白、不删字符。
  const probe = maskHtmlComments(rest)
  if (probe.length !== rest.length) throw new Error('内部错误：抹白副本与原文不等长')

  let depth = 1
  let consumed = 0
  for (;;) {
    TEMPLATE_TAG.lastIndex = consumed
    const tag = TEMPLATE_TAG.exec(probe)
    if (!tag) break
    depth += tag[1] ? -1 : 1
    if (depth === 0) return stripHtmlComments(rest.slice(0, tag.index))
    consumed = tag.index + tag[0].length
  }
  throw new Error('SFC 的 <template> 段没有配对的 </template>')
}

/**
 * 抹白一段文本里的 HTML 注释（**等长**空白，保留换行）。
 *
 * 等长是硬要求：调用方拿抹白副本的下标去切原始源。
 */
function maskHtmlComments(segment: string): string {
  return segment.replace(HTML_COMMENT, (c) => mask(c))
}

function mask(comment: string): string {
  return comment.replace(/[^\n]/g, ' ')
}

/**
 * 剥掉 HTML 注释。
 *
 * **单独导出**是为了让自测能分别验证「取段」与「剥注释」两件事——如果只测
 * 合成结果，「剥注释那行被删掉」和「取段范围恰好不含注释」会混淆。
 */
export function stripHtmlComments(template: string): string {
  return template.replace(HTML_COMMENT, '')
}

/**
 * 模板里出现的**静态**类名集合。
 *
 * - 只认 `class="…"` 与 `class='…'`；`:class` / `v-bind:class` 这类动态绑定
 *   **不算**（口径见里程碑子任务 3 第 1 条：动态绑定不算命中，文件须留有
 *   静态类名兜底）。
 * - 先剥注释再扫，所以注释里的 `class="af-body"` 不会混进来。
 */
export function staticClassesOf(vueSource: string): Set<string> {
  const template = templateOf(vueSource)
  const out = new Set<string>()
  // `(?<![:\w-])class=` 排除 `:class=` / `v-bind:class=`：它们的 `class=` 前面
  // 紧跟 `:`，而静态属性的 `class=` 前面是空白与标签名之间的边界。
  for (const m of template.matchAll(/(?<![:\w-])class\s*=\s*(["'])([^"']*)\1/g)) {
    for (const name of m[2].split(/\s+/)) {
      if (name) out.add(name)
    }
  }
  return out
}
