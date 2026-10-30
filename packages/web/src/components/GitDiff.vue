<script setup lang="ts">
/* ===========================================================================
   GitDiff —— 一份 unified diff 的只读渲染（规范 §5.4 / §3.14）
   ---------------------------------------------------------------------------
   只做一件事：把设备给的 `unified` 字符串按行画出来，按行的**首字符**上色。

   ── 为什么不做语法高亮 ──────────────────────────────────────────────
   diff 的语法（`+` / `-` / `@@` / `diff --git`）是 git 自己的、稳定的一套；
   语言语法是另一套、每种语言一份。先把前者画对，后者等真有人抱怨再说——
   而给 diff 上色（`+` 绿 / `-` 红）本身不是装饰：它是这一屏唯一的
   「哪边是新增」的线索。

   ── 这个组件不认识通道，也不发请求 ──────────────────────────────────
   与 `FilesPanel` 同一条纪律：数据从 props 进，意图从 emits 出。
   =========================================================================== */

import { computed } from 'vue'

const props = defineProps<{
  /** unified diff 正文。空串 = 这个文件没有改动（或还没取到）。 */
  unified: string
  /** 设备侧截断了（超帧预算）。 */
  truncated: boolean
  /** 这份 diff 属于哪个路径。`null` 只在应答形状异常时出现。 */
  path: string | null
  /** 正在取别的文件的 diff 时，旧内容要淡下去，否则用户会以为已经换了。 */
  stale: boolean
}>()

/**
 * 按行切分并标出每种行。
 *
 * `stale` 时**不切**：切一份马上要被替换的内容没有意义，而切分是 O(n)。
 */
const lines = computed(() => {
  if (props.stale) return []
  const text = props.unified.replace(/\r\n/g, '\n')
  if (!text.length) return []
  return text.split('\n').map((text) => ({ text, kind: kindOf(text) }))
})

/**
 * 一行对应的样式类名。**类型是封闭联合，不是 `string`**：`components.css` 里
 * 每个 kind 各有一条 `[data-kind='…']` 规则，写成 `string` 时新加一个返回值
 * TypeScript 不会提醒「CSS 里没有这一档」，那一行就静默按默认样式渲染。
 *
 * `ctx` 没有自己的 CSS 规则是**故意的**：上下文行就该是默认样式，写一条
 * 与默认值相同的规则只会让人以为它跟别的档一样有特殊处理。
 */
type DiffLineKind = 'meta' | 'hunk' | 'add' | 'del' | 'ctx'

/**
 * 一行的色别，只看首字符。
 *
 * **`+++` / `---` 是文件头而不是增删行**，所以先判它们：只按首字符判的话
 * 两个文件头会被染成绿 / 红，读起来像「这一整行被加了」。
 */
function kindOf(line: string): DiffLineKind {
  if (line.startsWith('+++') || line.startsWith('---')) return 'meta'
  if (line.startsWith('@@')) return 'hunk'
  if (line.startsWith('+')) return 'add'
  if (line.startsWith('-')) return 'del'
  if (line.startsWith('diff ') || line.startsWith('index ')) return 'meta'
  return 'ctx'
}
</script>

<template>
  <div class="af-gdiff" data-git-diff>
    <div class="af-gdiff__head af-t-mute">
      <span class="af-fp">{{ path ?? '（路径未知）' }}</span>
    </div>

    <p v-if="stale" class="af-gdiff__note af-t-mute">正在读取这一份 diff…</p>
    <p v-else-if="!lines.length" class="af-gdiff__note af-t-mute" data-git-diff-empty>
      <!-- 空正文 + 已截断 ≠ 「没有改动」：`truncate_utf8` 在首字节非法 UTF-8 时
           能截到什么都不剩，这时说「没有可显示的改动」是在撒谎。 -->
      <template v-if="truncated">改动太长了，设备侧一截就什么都不剩。</template>
      <template v-else>这个文件没有可显示的改动——它可能是未跟踪的空文件，或改动只涉及模式 / 权限。</template>
    </p>
    <template v-else>
      <pre class="af-gdiff__body" data-git-diff-body><div
        v-for="(line, i) in lines"
        :key="i"
        class="af-gdiff__line"
        :data-kind="line.kind"
      >{{ line.text }}</div></pre>
    </template>
    <!-- 截断要显式说出来：静默少给一半会让用户以为「就改了这些」。
         **放在 `lines.length` 判定之外**：早先它在 `<template v-else>` 里，
         于是空正文 + 截断时提示语不出现，面板只显示「没有可显示的改动」。 -->
    <p v-if="truncated" class="af-gdiff__trunc" role="status" data-git-diff-truncated>
      diff 太长，设备侧已截断：下面这些不是全部改动。把改动拆小、或直接用文件面板看具体位置。
    </p>
  </div>
</template>
