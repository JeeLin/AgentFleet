<script setup lang="ts">
/* ===========================================================================
   GitPanel —— git 面板：分支 + 改动列表 + 逐文件 diff（规范 §5.4 / §3.14）
   ---------------------------------------------------------------------------
   这一版是 **v0.4.0 的只读视图**：`status` 与 `diff`。log（历史提交）在
   同一里程碑的下一步接进来，所以这里的布局已经给它留了位置——但没有
   提交 / 暂存 / 切分支的按钮，也不会有：那要决定提交者身份从哪来、钩子
   要不要跑、失败怎么回滚，三件都不是「看它改了什么」需要的。

   ── 这个组件不知道通道 ──────────────────────────────────────────────
   与 `FilesPanel` 同一条纪律（§5.5.1）：数据从 props 进，意图从 emits 出。
   通道的生命周期是页面的事，面板自己持有一条句柄就会出现「面板以为通道
   活着、页面已经把它关了」的第二份真相。

   ── 三种「空」必须长得不一样 ────────────────────────────────────────
   §3.12 的纪律在这里有三个落点，混掉任何一个都会被读成「坏了」：

   1. **还没问过**（`clean === null`）：说什么都太早，写「正在读取工作区状态」；
   2. **没有改动**（`clean === true`）：这是一条**结论**，要明写「没有未提交的
      改动」，而不是画一个空列表——空列表读起来像「没读到」；
   3. **不是 git 仓库**（`notARepo`）：这是**正常状态**，中性说明 + 指路，
      **不报红**。红框会让用户以为要去修什么，而这里没什么可修的。

   ── 状态码不翻译 ────────────────────────────────────────────────────
   `M` / `A` / `D` / `R` / `??` 是 git 用户的共同词汇。译成「修改 / 新增」
   会让用户回头去对照 git 输出才能确认，等于把一次翻译变成两次。
   =========================================================================== */

import { computed } from 'vue'

import GitDiff from '@/components/GitDiff.vue'
import type { ChannelState } from '@/lib/channel'

/** `status` 回帧里的一个文件条目。两列状态码原样保留。 */
export interface GitFile {
  x: string
  y: string
  path: string
}

const props = defineProps<{
  /** 通道状态。只用来决定「还没连上」那一句怎么写。 */
  state: ChannelState
  /** 分支名。`null` = 还不知道。 */
  branch: string | null
  /** 短 HEAD。 */
  head: string | null
  /** detached HEAD：头一行要写 `(detached at abc1234)` 而不是留空白。 */
  detached: boolean
  /** 有未提交改动吗。`null` = 还没问过（与 `false` 是两件事）。 */
  clean: boolean | null
  /** 改动文件列表。 */
  files: GitFile[]
  /** 设备说「这里不是 git 仓库」。正常状态，不报红。 */
  notARepo: boolean
  /** 当前展示的 diff。`null` = 用户还没点开任何文件。 */
  diff: { path: string | null; unified: string; truncated: boolean } | null
  /** 正在取 diff 的路径。 */
  reading: string | null
  /** 问 status / diff 被拒的原因。 */
  error: string | null
}>()

const emit = defineEmits<{
  /** 用户点刷新：重问一次 status。 */
  (e: 'refresh'): void
  /** 用户点某个文件：取它的 diff。 */
  (e: 'diff', path: string): void
}>()

/**
 * 头一行的写法。
 *
 * detached 时**不能留空白**：空白会被读成「还在加载」。空仓库（有分支名但
 * 没有 head）也不留空白——写「还没有提交」，因为那正是空的**原因**。
 */
const branchLine = computed(() => {
  if (props.detached) {
    return props.head ? `(detached at ${props.head})` : '(detached)'
  }
  if (props.branch) {
    return props.head ? `${props.branch} · ${props.head}` : `${props.branch} · 还没有提交`
  }
  return props.head ?? '（分支未知）'
})

/** 两列状态码拼成一个两字符的徽标。`??` 就是两列都是 `?`，原样显示。 */
function codeOf(f: GitFile): string {
  return `${f.x}${f.y}`
}

/**
 * 状态码的含义，只作为 `title`（悬停提示）。
 *
 * **不是显示文本**：表格里显示的是 git 自己的字母，这里只是给不熟悉的人
 * 一条**按需**的梯子。译名不进表格。
 */
function codeTitle(f: GitFile): string {
  const names: Record<string, string> = {
    M: '已修改',
    A: '新增',
    D: '已删除',
    R: '重命名',
    C: '复制',
    U: '冲突',
    '?': '未跟踪',
    ' ': '未变化',
  }
  const x = names[f.x] ?? f.x
  const y = names[f.y] ?? f.y
  return `index: ${x} / 工作区: ${y}`
}

/** 只挂载一次 diff 时它属于哪个文件（用来高亮被点中的行）。 */
const selected = computed(() => props.diff?.path ?? props.reading)
</script>

<template>
  <div class="af-git" data-git-panel>
    <!-- 头一行**常驻**，包括「不是仓库」的时候：用户刚 `git init` 之后唯一的
         动作就是刷新一次，而刷新按钮藏起来等于让用户重开面板。分支名那一格
         在非仓库时写「不是仓库」——那里本来也没有分支可写。 -->
    <div class="af-git__head">
      <span v-if="notARepo" class="af-git__branch af-fp" data-git-branch>不是 git 仓库</span>
      <span v-else class="af-git__branch af-fp" data-git-branch>{{ branchLine }}</span>
      <span class="af-spacer"></span>
      <span v-if="clean === false" class="af-badge af-badge--warn" data-git-dirty>
        {{ files.length }} 个改动
      </span>
      <button
        class="af-btn af-btn--sm af-btn--ghost"
        type="button"
        data-git-refresh
        :disabled="state !== 'ready'"
        :title="state === 'ready' ? '重新读取工作区状态' : '通道还没连上'"
        @click="emit('refresh')"
      >
        刷新
      </button>
    </div>

    <!-- 「不是 git 仓库」是**正常状态**，所以它前面不画红色、不加 role="alert"。
         这里也不写「错误」「失败」——它没有失败，只是这个工作区不走 git。 -->
    <div v-if="notARepo" class="af-git__void" data-git-not-a-repo>
      <p>这个工作区不是 git 仓库，所以没有改动与历史可看。</p>
      <p class="af-t-mute">
        这不是故障，也不需要处理。想浏览这里的文件，用坞头的「＋」打开文件面板。
      </p>
    </div>

    <template v-else>
      <p v-if="error" class="af-panel__state" role="alert" data-git-error>{{ error }}</p>

      <!-- ① 还没问过：状态未知。不能声称「干净」，也不能画空列表。 -->
      <p
        v-else-if="clean === null"
        class="af-git__void af-t-mute"
        data-git-loading
      >
        正在读取工作区状态…
      </p>

      <!-- ② 问过了，没有改动：这是**结论**，要明写。 -->
      <p
        v-else-if="clean"
        class="af-git__void af-t-mute"
        data-git-clean
      >
        没有未提交的改动。
      </p>

      <!-- ③ 有改动：列表 + 右侧 diff。 -->
      <div v-else class="af-git__split">
        <ul class="af-git__files" role="listbox" aria-label="改动的文件">
          <li v-for="f in files" :key="f.path">
            <button
              class="af-git__file"
              type="button"
              role="option"
              :aria-selected="selected === f.path ? 'true' : 'false'"
              :data-git-file="f.path"
              :title="codeTitle(f)"
              @click="emit('diff', f.path)"
            >
              <span class="af-git__code af-fp" aria-hidden="true">{{ codeOf(f) }}</span>
              <span class="af-git__path af-fp">{{ f.path }}</span>
            </button>
          </li>
        </ul>

        <div class="af-git__diffpane">
          <p v-if="!diff && !reading" class="af-git__void af-t-mute" data-git-no-diff>
            点左边任一文件看它的改动。
          </p>
          <p v-else-if="!diff" class="af-git__void af-t-mute">正在读取 diff…</p>
          <GitDiff
            v-else
            :unified="diff.unified"
            :truncated="diff.truncated"
            :path="diff.path"
            :stale="reading !== null"
          />
        </div>
      </div>
    </template>
  </div>
</template>
