<script setup lang="ts">
/* ===========================================================================
   ConvFlow —— 对话流（规范 §5.4 / §7.5 / §3.11 / §3.10）
   ---------------------------------------------------------------------------
   13d 只做**框架与空态**：对话区在 M1 恒为空，因为这个里程碑的会话是
   **页面级容器**（工作区 + 面板），它不产生真实 agent 轨迹——ACP 归 M3。
   所以这里渲染的是「空态 + 为什么空 + 什么时候会有」，**不画一条假消息**。
   按 `design-system.md:736` 把 User / Agent / Tool / System 四类行全字段
   画出来（含折叠的工具块），那是在实现 M3。

   ── 输入区为什么是这里唯一「有数据源」的部分 ────────────────────
   对话是空的，输入区不是装饰：它把一行命令交给 **exec connector**
   （子任务 10）跑一次。这不是 ACP——没有 agent 参与，不产生轨迹事件，
   所以跑完之后对话流**仍然为空**。这一句必须写在页面上，否则用户会
   读成「执行了但没反应」。

   ── 为什么不做 shell ──────────────────────────────────────────
   一行按空白切成 `command` + `args`，引号只用来把带空格的参数合成一个。
   管道 / 重定向 / 通配 / 变量展开**都不生效**：exec connector 走
   `execvp(2)`，不是 `/bin/sh -c`。这条写在提示里——用户第一次敲
   `grep x * | head` 的时候应该已经知道它不会按 shell 的语义跑。
   与其悄悄丢掉那半句，不如一开始就说明白这里没有 shell。

   ── 空态为什么必须明写「M3 起有数据」 ────────────────────────────
   一个什么都不说的空区域会被读成「加载失败」或「功能没做完」。
   `AgentsPage` 顶上那条 `af-stage-note`（「M1 无数据」+ 为什么）是同一个
   问题的既有答案，这里照它的调性做。

   ── 会话已结束时输入区整体禁用 ────────────────────────────────
   不是灰掉按钮了事：结束的会话不再建任何通道，而「能打字但按下没反应」
   比「不能打字」难懂得多。禁用 + 一句为什么。
   =========================================================================== */

import { computed, ref } from 'vue'

import EmptyState from '@/components/EmptyState.vue'

/** 输入区的发送目标。 */
export type ConvTarget = 'exec' | 'terminal'

/** 一次 exec 请求。字段与 `node-core/src/connector/exec.rs` 的 `ExecParams` 对齐。 */
export interface ConvExecRequest {
  /** 程序名（一行里的第一个词）。 */
  command: string
  /** 参数。**不做 shell 展开**：引号只用来把带空格的参数合成一个。 */
  args: string[]
  /**
   * 相对工作区根。恒为 `''`——工作区根就是会话路径（创建后锁定），
   * 而这个页面没有第二个根（§5.4）。
   */
  cwd: string
}

const props = withDefaults(
  defineProps<{
    /** 会话所属设备名。提示里重复一次，两个会话并排时靠它分。 */
    deviceName: string
    /** 会话工作区路径（锁定）。exec 的 cwd 相对它解析。 */
    path: string
    /** 会话已结束：输入区不可用（不会再建通道）。 */
    ended?: boolean
    /** 终端面板是否已有可用通道（决定「终端」这个目标能不能选）。 */
    terminalReady?: boolean
    /** 轨迹抽屉是否开着——让这个入口能显示展开态。 */
    trajOpen?: boolean
  }>(),
  { ended: false, terminalReady: false, trajOpen: false },
)

const emit = defineEmits<{
  /** 跑一条一次性命令：交给 exec connector。 */
  (e: 'run', req: ConvExecRequest): void
  /** 把一段文本送进终端面板的通道（等于在终端里敲这一行）。 */
  (e: 'send', text: string): void
  /**
   * 打开轨迹抽屉。`opener` 是触发它的那个元素——抽屉关闭后焦点还给它，
   * 所以触发方必须把它交出来，而不是让抽屉去猜 `activeElement`
   * （Safari 里点按钮不移动焦点，那时猜出来的是 body）。
   */
  (e: 'open-traj', opener: HTMLElement | null): void
}>()

const EMPTY_TEXT =
  'M3 起有数据。对话是 trajectory_event 按 kind 折叠后的读法，' +
  '而 M1 的会话容器不产生这条事件流（ACP 归 M3）——所以这里恒空是预期结果，' +
  '不是加载失败。这里不画假消息。'

const draft = ref('')
const target = ref<ConvTarget>('exec')
const trajBtnEl = ref<HTMLButtonElement | null>(null)

const canSubmit = computed(() => !props.ended && draft.value.trim().length > 0)

const placeholder = computed(() =>
  props.ended
    ? '这个会话已结束：面板与命令都不会再建立通道'
    : `在 ${props.path} 里跑一条命令（不是 shell：管道与重定向不会生效）`,
)

const targetHint = computed(() => {
  if (props.ended) return ''
  if (target.value === 'exec') {
    return 'exec 是一次性命令：跑完就结束、通道随即关闭，退出码与输出一次回完（没有交互）'
  }
  return props.terminalReady
    ? '送进已打开的终端面板：等于在终端里敲这一行并回车'
    : '终端面板还没有通道：先按 o 打开面板并与它交互一次'
})

/**
 * 一行 → `{ command, args }`。
 *
 * **不是 shell**：只做「按空白切词 + 引号合成一个词」。管道、重定向、
 * 通配、变量展开、命令替换一概没有——exec connector 拿到的就是
 * `execvp(command, args)`，给它 `ls -la` 当程序名只会 ENOENT。
 */
function parseCommand(line: string): { command: string; args: string[] } | null {
  const words = splitWords(line)
  const command = words[0]
  if (!command) return null
  return { command, args: words.slice(1) }
}

/** 按空白切词，`'…'` / `"…"` 内的空白不算分隔。**不支持转义**。 */
function splitWords(line: string): string[] {
  const out: string[] = []
  let cur = ''
  let quote: string | null = null
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null
      else cur += ch
      continue
    }
    if (ch === '"' || ch === "'") {
      quote = ch
      continue
    }
    if (/\s/.test(ch)) {
      if (cur) out.push(cur)
      cur = ''
      continue
    }
    cur += ch
  }
  if (cur) out.push(cur)
  return out
}

function submit(): void {
  if (!canSubmit.value) return
  const text = draft.value
  if (target.value === 'terminal') {
    // 终端要的是「敲了这一行并回车」，所以补一个换行——少一个的话
    // 命令会停在 PTY 里等用户输入。
    emit('send', text.endsWith('\n') ? text : `${text}\n`)
    draft.value = ''
    return
  }
  const parsed = parseCommand(text)
  if (!parsed) return
  emit('run', { command: parsed.command, args: parsed.args, cwd: '' })
  draft.value = ''
}

/**
 * ⏎ 执行，⇧⏎ 换行。
 *
 * `isComposing` 期间不执行：中文输入法选字时按 ⏎ 是「把候选词上屏」，
 * 那一下把半句命令发出去是这类输入框最经典的误发。
 */
function onKeydown(e: KeyboardEvent): void {
  if (e.key !== 'Enter') return
  if (e.isComposing) return
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return
  e.preventDefault()
  submit()
}
</script>

<template>
  <div class="af-flowhost">
    <!-- 这条横幅是「为什么空」：不写它，空对话区会被读成加载失败 -->
    <div class="af-stage-note" role="note">
      <span class="af-badge af-badge--warn">M1 无数据</span>
      <div class="af-stage-note__body">
        <p>
          <b>对话流在 M1 恒定为空</b>：M1 的会话是页面级容器（工作区 + 面板），
          没有 ACP 会话，也不产生 <span class="af-fp">trajectory_event</span>——
          ACP 归 M3。这里不画假消息。
        </p>
        <p class="af-t-mute">
          输入区是唯一有数据源的部分：它把一行命令交给 exec connector 跑一次
          （子任务 10）。那不是对话——跑完不会在这里留下一条消息。
        </p>
      </div>
    </div>

    <!-- 对话流本体：M1 恒空，容器先立好（role="log" 是 M3 追加消息时的语义） -->
    <div class="af-flow af-flowhost__flow" role="log" aria-label="对话流（M1 恒空，M3 起有数据）">
      <EmptyState glyph="◇" title="M3 起有数据" :text="EMPTY_TEXT" />
    </div>

    <form class="af-compose" @submit.prevent="submit">
      <div class="af-compose__row">
        <textarea
          v-model="draft"
          class="af-textarea af-compose__input"
          rows="1"
          :disabled="ended"
          :aria-label="
            ended ? '命令（这个会话已结束：不会再建立通道）' : '命令：一行，按 ⏎ 执行'
          "
          :placeholder="placeholder"
          data-conv-input
          @keydown="onKeydown"
        ></textarea>

        <select
          v-model="target"
          class="af-select af-compose__target"
          :disabled="ended"
          aria-label="发送目标"
          data-conv-target
        >
          <option value="exec">exec · 一次性命令</option>
          <option value="terminal" :disabled="!terminalReady">终端 · 送进 PTY</option>
        </select>

        <button class="af-btn af-btn--primary" type="submit" :disabled="!canSubmit" data-conv-run>
          执行
        </button>

        <button
          ref="trajBtnEl"
          class="af-btn af-btn--sm af-btn--ghost"
          type="button"
          :aria-expanded="trajOpen ? 'true' : 'false'"
          title="打开 / 关闭轨迹抽屉（t）"
          data-conv-traj
          @click="emit('open-traj', trajBtnEl)"
        >
          <span aria-hidden="true">≡</span> 轨迹 <kbd>t</kbd>
        </button>
      </div>

      <!--
        提示行是**正文**，不是 tooltip：§3.13 要求信息不许只活在 hover 里，
        placeholder 在输入框里有内容之后就看不见了。所以「这里不是 shell」
        与「结束了为什么不能打字」两条都写在这里，而不只写在 placeholder。
      -->
      <p class="af-compose__hint af-field__hint">
        <span><kbd>⏎</kbd> 执行</span>
        <span><kbd>⇧⏎</kbd> 换行</span>
        <span><kbd>t</kbd> 轨迹抽屉</span>
        <span class="af-spacer"></span>
        <span>{{ deviceName }} · <span class="af-fp">{{ path }}</span></span>
        <span v-if="ended" class="af-compose__why">
          这个会话已结束：工作区不再可达，面板与命令都不会再建立通道。
        </span>
        <span v-else class="af-compose__why">
          这里不是 shell：一行按空白切成程序名与参数，引号只用来把带空格的参数
          合成一个——管道与重定向、通配与变量展开都不会生效。
        </span>
        <span v-if="targetHint" class="af-t-mute">{{ targetHint }}</span>
      </p>
    </form>
  </div>
</template>

<style scoped>
/* 对话列拿到的是「一竖条」：自己撑满并让消息区滚动，输入区固定在底部 */
.af-flowhost {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.af-flowhost__flow {
  display: flex;
  flex-direction: column;
  justify-content: center;
}
.af-compose__target {
  flex: 0 0 auto;
  height: 44px;
  font-size: var(--af-text-xs);
}
.af-compose__hint {
  flex-wrap: wrap;
  row-gap: var(--af-space-2);
}
/* 「为什么」比键位提示更需要一整行：键位可以挤，理由不能 */
.af-compose__why {
  flex: 1 0 100%;
  order: 1;
}

/* 阶段横幅。与 AgentsPage 的同名块同一套语言（§3.11 的「为什么空」）：
   警告色左边线 + 徽标 + 两段说明，不跟随滚动，也不是 toast。 */
.af-stage-note {
  display: flex;
  align-items: flex-start;
  gap: var(--af-space-4);
  padding: var(--af-space-4) var(--af-space-5);
  background: var(--af-surface-1);
  border-bottom: 1px solid var(--af-border-default);
  border-left: 2px solid var(--af-warn);
}
.af-stage-note__body {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  font-size: var(--af-text-xs);
  line-height: var(--af-leading-base);
  color: var(--af-fg-secondary);
}
.af-stage-note__body p {
  margin: 0;
}
</style>
