<script setup lang="ts">
/* ===========================================================================
   GitPanel —— git 面板：分支 + 改动列表 + 逐文件 diff（规范 §5.4 / §3.14）
   ---------------------------------------------------------------------------
   两个分段：**改动**（`status` + 逐文件 `diff`）与**历史**（`log` + 某提交的
   `diff`）。它们是同一个工作区、同一份 git 状态的两种看法，所以是同一个
   面板内的视图切换（`docs/DECISIONS.md` D10），不是第二个面板、也不是路由。

   **没有任何写操作，且这不是「暂未实现」**：commit / stage / checkout 要连带
   决定提交者身份从哪来、钩子要不要跑、失败怎么回滚——三件都不是「看它改了什么」
   需要的，而做错任何一件都会改到用户的工作区。

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

import { computed, ref } from 'vue'

import GitDiff from '@/components/GitDiff.vue'
import GitLog, { type GitCommit } from '@/components/GitLog.vue'
import type { ChannelState } from '@/lib/channel'

/**
 * `status` 回帧里的一个文件条目。两列状态码原样保留。
 *
 * `path` 是**请求用标识**（原样填回 `{"op":"diff","path":…}`），
 * `orig` 只是重命名 / 复制的**旧路径**，仅供显示。两者不合并：
 * 合并出来的 `新 <- 旧` 不是任何一条真实路径，拿它去问 diff 只会得到空串。
 */
export interface GitFile {
  x: string
  y: string
  path: string
  /** 重命名 / 复制的旧路径；无则 `undefined`。 */
  orig?: string
}

/**
 * 一份 diff 的视图形状。`path` 与 `truncated` 都来自设备回帧。
 *
 * 导出而**不在页面里另写一份**：两个地方各写一次同样的三个字段，改一个忘一个
 * 时类型检查不会说话（结构类型兼容），而页面传错字段是运行期才看得见的事。
 */
export interface GitDiffView {
  path: string | null
  /** unified diff 正文。 */
  unified: string
  /** 设备侧截断了（超帧预算）。**必须显式告知**，不能静默少给。 */
  truncated: boolean
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
  diff: GitDiffView | null
  /** 正在取 diff 的路径。 */
  reading: string | null
  /** 问 status / diff 被拒的原因。 */
  error: string | null
  /** 历史提交。`null` = 还没问过（切到历史分段时才问）。 */
  commits: GitCommit[] | null
  /** 还有更早的提交没列出来。 */
  logTruncated: boolean
  /** 正在读取哪条提交的 diff（完整 hash）。 */
  logReading: string | null
  /** 当前展示的 diff 属于哪条提交（完整 hash）。`null` = 不是历史 diff。 */
  logSelected: string | null
}>()

const emit = defineEmits<{
  /** 用户点刷新：重问一次 status。 */
  (e: 'refresh'): void
  /**
   * 用户点了改动列表里的一行：取它的 diff。
   *
   * 第二个参数是重命名 / 复制的**旧路径**：问 diff 时必须把它一起给
   * `git`，否则 pathspec 只含新路径，git 会把它当「新建文件」，
   * 页面于是把一次改名显示成「新建了这个文件」。
   */
  (e: 'diff', path: string, orig?: string): void
  /** 用户切到历史分段：问一次 log。 */
  (e: 'log'): void
  /**
   * 用户切回改动分段。
   *
   * **必须有这一侧**：两个分段共用页面上的同一份 `ws.git.diff`，历史侧往里
   * 写过「某条提交的 diff」。只有 `log` 一侧 emit 的话，页面没有任何入口在
   * 「离开历史」时把那份 diff 撤掉——切回改动后左栏是工作区文件列表、无一行
   * 高亮，右栏却还显示着上一条提交的改动，两个提示指向两个事实。
   */
  (e: 'changes'): void
  /** 用户点某条提交：取它的 diff（**完整** hash）。 */
  (e: 'open-commit', hash: string): void
}>()

/**
 * 分段：改动 / 历史。
 *
 * **切到历史时才问 log**：`status` 是打开面板就要看的，历史不是——每次打开
 * 面板都跑一次 `git log` 只是替用户做了一个他没要的请求。
 */
type Segment = 'changes' | 'log'

const segment = ref<Segment>('changes')

function selectSegment(next: Segment): void {
  if (segment.value === next) return
  segment.value = next
  // 写成两支而不是 `emit(next)`：`defineEmits` 里两个事件签名不同
  // （一个无参、一个无参但名字不同），把 `Segment` 直接传进去时 TS 无法在
  // 重载之间消歧，只能靠字面量。
  if (next === 'log') emit('log')
  else emit('changes')
}

/**
 * 分段条的方向键。
 *
 * 写了 `role="tablist"` 就得让左右键能用——否则屏幕阅读器会报「tablist 里
 * 方向键无反应」。与 `DevicesPage` 的分段条同一套语义。
 */
function onSegKey(e: KeyboardEvent): void {
  if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return
  const next = e.key === 'ArrowRight' ? 'log' : 'changes'
  // 两端不环绕：两项的分段条上环绕只会让「已经到头了」看不出来。
  if (next === segment.value) {
    e.preventDefault()
    return
  }
  selectSegment(next)
  e.preventDefault()
}

/**
 * 头一行的写法。
 *
 * detached 时**不能留空白**：空白会被读成「还在加载」。空仓库（有分支名但
 * 没有 head）也不留空白——写「还没有提交」，因为那正是空的**原因**。
 *
 * 没有分支名却有 head 时**要把 head 写出来**：那意味着 `rev-parse
 * --abbrev-ref HEAD` 没能给出名字（比如设备侧读到了 `HEAD` 但 `symbolic-ref`
 * 失败），而提交号是已知的事实。把它丢掉换一句「（分支未知）」等于用一句
 * 「不知道」覆盖一条已知信息，与本面板「空白会被读成还在加载」的口径也相反。
 */
const branchLine = computed(() => {
  if (props.detached) {
    return props.head ? `(detached at ${props.head})` : '(detached)'
  }
  if (props.branch) {
    return props.head ? `${props.branch} · ${props.head}` : `${props.branch} · 还没有提交`
  }
  // 走到这里一定是「没有分支名」（非 git 仓库由模板的 `v-if="notARepo"`
  // 在更外层整块换掉，不进这个函数）。
  return props.head ? `（分支未知）· ${props.head}` : '（分支未知）'
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

/**
 * 列表里显示的路径：重命名 / 复制时把旧路径补在后面。
 *
 * **只用于显示**——点这一行发出去的是 `f.path`（不含 `orig`）。
 * 显示串与请求标识分开，是因为前者可以是「给人读的合成串」，
 * 而后者必须是 git 认得的一条真实路径。
 */
function displayPath(f: GitFile): string {
  return f.orig ? `${f.path} <- ${f.orig}` : f.path
}

/** 改动分段里，diff 属于哪个文件（用来高亮被点中的行）。 */
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
         这里也不写「错误」「失败」——它没有失败，只是这个工作区不走 git。
         分段条也不画：没有历史可切。 -->
    <div v-if="notARepo" class="af-git__void" data-git-not-a-repo>
      <p>这个工作区不是 git 仓库，所以没有改动与历史可看。</p>
      <p class="af-t-mute">
        这不是故障，也不需要处理。想浏览这里的文件，用坞头的「＋」打开文件面板。
      </p>
    </div>

    <template v-else>
      <!-- 分段：改动 / 历史。同一份 git 状态的两种看法，不是两个面板。 -->
      <div
        class="af-row gap-2 af-seg af-git__seg"
        role="tablist"
        aria-label="git 面板分段"
        @keydown="onSegKey"
      >
        <button
          class="af-btn af-btn--sm"
          :class="{ 'af-btn--primary': segment === 'changes' }"
          type="button"
          role="tab"
          :aria-selected="segment === 'changes'"
          data-git-seg="changes"
          @click="selectSegment('changes')"
        >
          改动
        </button>
        <button
          class="af-btn af-btn--sm"
          :class="{ 'af-btn--primary': segment === 'log' }"
          type="button"
          role="tab"
          :aria-selected="segment === 'log'"
          data-git-seg="log"
          @click="selectSegment('log')"
        >
          历史
        </button>
      </div>

      <!-- ================== 历史分段 ================== -->
      <template v-if="segment === 'log'">
        <!-- `log` 失败时原因写在同一个 `error` 上（`gitRequest` 的 `onError`）：
              这段不读它的话，用户只看到「正在读取提交历史…」永远转下去。 -->
        <p v-if="error" class="af-panel__state" role="alert" data-git-error>{{ error }}</p>
        <div class="af-git__split">
          <div class="af-git__files af-git__files--log">
            <GitLog
              :commits="commits"
              :truncated="logTruncated"
              :reading="logReading"
              :selected="logSelected"
              @open="(hash: string) => emit('open-commit', hash)"
            />
          </div>
          <div class="af-git__diffpane">
            <!-- **这里没有 `stale` 档，是结构决定的**：`SessionPage` 的
                 `gitOpenCommit` 在发请求前就把 `ws.git.diff` 清成 `null`
                 （理由见那里的注释：不清的话从改动分段带过来的工作区 diff
                 会先闪一下），所以点下一条提交时走到的是上面那句
                 「点左边任一提交看它改了什么」，`<GitDiff>` 只有在**已经拿到**
                 这一条提交的 diff 之后才会渲染——那时不存在「上一份」可言。
                 早先这里挂着 `:stale="logReading !== null"`，而那个值永远
                 不可能在渲染 `<GitDiff>` 时为真，是死判据（实测改成
                 `:stale="false"` 全量用例全绿，因为分支根本走不到）。 -->
            <p v-if="!diff" class="af-git__void af-t-mute" data-git-log-no-diff>
              {{ logReading !== null ? '正在读取这条提交的改动…' : '点左边任一提交看它改了什么。' }}
            </p>
            <GitDiff
              v-else
              :unified="diff.unified"
              :truncated="diff.truncated"
              :path="diff.path"
              :stale="false"
            />
          </div>
        </div>
      </template>

      <!-- ================== 改动分段 ================== -->
      <template v-else>
        <!-- 错误横幅**与内容并排**，不替换内容：早先它是 `v-if="error"`、
              排在 `clean === null` 之前，于是出过一次错之后文件列表与 diff
              全被换掉——包括已经成功取到的 diff。 -->
        <p v-if="error" class="af-panel__state" role="alert" data-git-error>{{ error }}</p>

        <!-- ① 还没问过：状态未知。不能声称「干净」，也不能画空列表。 -->
        <p v-if="clean === null" class="af-git__void af-t-mute" data-git-loading>
          正在读取工作区状态…
        </p>

        <!-- ② 问过了，没有改动：这是**结论**，要明写。 -->
        <p v-else-if="clean" class="af-git__void af-t-mute" data-git-clean>
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
                @click="emit('diff', f.path, f.orig)"
              >
                <span class="af-git__code af-fp" aria-hidden="true">{{ codeOf(f) }}</span>
                <span class="af-git__path af-fp">{{ displayPath(f) }}</span>
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
    </template>
  </div>
</template>
