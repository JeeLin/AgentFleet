<script setup lang="ts">
/* ===========================================================================
   ConvFlow —— 对话流
   ---------------------------------------------------------------------------
   **对话是 agent 说的话，exec 是跑一条命令**，两者在这个组件里共用一行输入框，
   但它们不是一回事，所以从这个里程碑起把话说清楚：

   - 选「对话」：把整段文本交给设备的 ACP 子进程，回复以事件流回来，
     **写进 trajectory_event**（页面刷新后还在）；
   - 选「exec」：一次性命令，跑完通道就关，输出一次回完，**不进轨迹**。

   以前输入区恒冒充对话区（「把一行命令交给 exec connector」是当时唯一有数据
   源的部分），页面上却没有一句「这不是对话」。现在两件事分开标的，
   因为同一个输入框里有两种语义而不说明，用户会把「跑完没有任何消息」
   读成「功能坏了」。

   ── 帧格式是设备侧定的契约 ──────────────────────────────────────
   `node-core` 的 `conversation.rs` 往通道里推六类帧：`batch` / `turn_end` /
   `ready` / `error` / `request` / `exited`。这里**只解释不发明**：缺字段就用
   保守空值，不在前端编一条「看起来像回复」的消息。

   `batch.events` 里每条是 ACP 原样的 `update` 对象（`sessionUpdate` 标类型、
   `seq` 由设备编号）。**未知类型照样画**，只是没有专门的版式——新版本 agent
   推的类型我们还没见过，过滤掉就是让用户看不见 agent 说了什么。

   ── 断口要画出来，不许假装连续 ──────────────────────────────────
   `seq` 由设备编号、中间丢一段在 Hub 侧看得出来（`gaps`）。把缺的那一截
   悄悄拼上，用户会以为 agent 就是说了这些。所以：缺 N 条就写「缺失 N 条」，
   末轮没有 `turn_end` 就写「本轮未完成」。**不补内容**——轨迹备份格里 halo
   补不回来（ACP 没有重发机制，`session/load` 是重建流，不是原始事件流）。

   ── 为什么不是 shell ──────────────────────────────────────────
   exec 目标下，一行按空白切成 `command` + `args`，引号只用来把带空格的参数
   合成一个。管道 / 重定向 / 通配 / 变量展开**都不生效**：exec connector 走
   `execvp(2)`，不是 `/bin/sh -c`。这条写在提示里。
   =========================================================================== */

import { computed, ref, watch } from 'vue'

import EmptyState from '@/domains/shared/components/EmptyState.vue'
import type { TrajectoryEvent, TrajectoryGap } from '@/domains/session/api'

/** 输入区的发送目标。`exec` 是一次性命令，`chat` 才是对话。 */
export type ConvTarget = 'exec' | 'terminal' | 'chat'

/** 一次 exec 请求。字段与 `node-core/src/connector/exec.rs` 的 `ExecParams` 对齐。 */
export interface ConvExecRequest {
  /** 程序名（一行里的第一个词）。 */
  command: string
  /** 参数。**不做 shell 展开**：引号只用来把带空格的参数合成一个。 */
  args: string[]
  /**
   * 相对工作区根。恒为 `''`——工作区根就是会话路径（创建后锁定），
   * 而这个页面没有第二个根。
   */
  cwd: string
}

/**
 * 一段序号空洞：`[from, to)` 里的 seq 都不在库里。
 *
 * 与 Hub 侧 `hub-storage/src/trajectory.rs` 的 `Gap` 同一套坐标：`to` 是
 * **那个存在的 seq**、充当排他上界，所以「缺几条」是 `to - from`。前端不自己
 * 算这个减法——Hub 已经把它算进 `missing` 字段里了，两处各算一份会分叉。
 */
/**
 * 断口形状直接用 `api.ts` 的 `TrajectoryGap`。
 *
 * 本组件曾经自己声明一份同形的 `ConvGap`：两份字段一样、注释一样，而
 * `SessionPage` 传给 `ConvFlow` 与 `TrajDrawer` 的是**同一个数组实例**，
 * 本地那份不增加任何类型信息，只多一个会跟着漂的副本。
 */

/** 一条待渲染的对话事件。已按 `seq` 排好、断口已插进去。 */
export type ConvRow =
  | { kind: 'gap'; key: string; missing: number; after: number | null }
  | {
      kind: 'msg'
      key: string
      who: 'user' | 'agent' | 'system'
      text: string
      /**
       * 本地回显行：已经发出去、但轨迹里还没有的那一句。
       *
       * 只说明「页面把它发出去了」，**不说明 Hub 或设备收到了**——所以渲染成
       * 「待确认」而不是正常已送达的样子。
       */
      pending?: true
    }
  | {
      kind: 'tool'
      key: string
      /** ACP 的 `toolCallId`。合并同一次工具调用靠它。 */
      id: string
      name: string
      status: string
      detail: string
    }
  | { kind: 'turn'; key: string; reason: string; incomplete?: false }
  | { key: string; kind: 'incomplete' }
  /**
   * 元数据事件（可用命令清单、token 用量）压成的一行小字。
   *
   * 与 `msg` 分开是因为它不是「谁说了句话」：它占一行但**不该抢走对话的注意力**。
   * 真机一轮回复固定推 `available_commands_update` 与 `usage_update` 两条，
   * 若按未知类型兜底成 `系统 <原始 JSON>`，用户先看到两坨协议文本才看到回复。
   */
  | { kind: 'meta'; key: string; text: string }

const props = withDefaults(
  defineProps<{
    /** 会话所属设备名。提示里重复一次，两个会话并排时靠它分。 */
    deviceName: string
    /** 会话工作区路径（锁定）。exec 的 cwd 相对它解析。 */
    path: string
    /** 会话已结束：输入区不可用（不会再建通道）。 */
    ended?: boolean
    /**
     * 会话所属设备已被删除。**与 `ended` 是两个事实**，理由也分两句说：
     * 「会话结束了」是会话自己的状态，「设备没了」是它落脚的那台机器没了。
     * 两者都让输入区不可用，但用户读到的原因必须是他实际撞上的那一条。
     */
    deviceGone?: boolean
    /**
     * 设备已删除时那句原因（页面持有一份，因为它同时写在横幅里）。
     * 不在这里另写一句：同一个事实在两处用不同措辞，用户会以为说的是两件事。
     */
    deviceGoneWhy?: string
    /** 终端面板是否已有可用通道（决定「终端」这个目标能不能选）。 */
    terminalReady?: boolean
    /**
     * 对话通道最近一次失败的错误码。
     *
     * **它不拦发送**：agent 不可用是设备侧当时的状态，过一会可能就好了。
     * 通道是「说第一句话时才开」的，所以失败只在**已经发过**之后才可能有值——
     * 那时把原因显示出来，而把重试的路留着（再发一次就会再开一次）。
     */
    convError?: string | null
    /** 轨迹抽屉是否开着——让这个入口能显示展开态。 */
    trajOpen?: boolean
    /** 这个会话选了哪个 agent。没有就不能对话，原因要写在页面上。 */
    agentId?: string | null
    /**
     * 累积的对话事件。**按物理顺序**，与 `GET /api/sessions/{id}/trajectory`
     * 返回的 `events` 同形（每条带 `seq` / `kind` / `payload`）。
     */
    events?: TrajectoryEvent[]
    /** 序号断口。Hub 算好给过来，前端不重算。 */
    gaps?: TrajectoryGap[]
    /** 最后一轮**既没有开始标记也没有结束标记**：结尾丢了。 */
    incomplete?: boolean
    /**
     * 最后一轮**有 `turn_start`、没有 `turn_end`**：一轮开了还没关。
     *
     * 与 `incomplete` 互斥。不代表 agent 还活着（开始标记落库后设备掉了也是
     * 这个状态），所以只用它决定「别报未完成」，不用它说「正在回复」。
     */
    running?: boolean
  }>(),
  {
    ended: false,
    terminalReady: false,
    convError: null,
    trajOpen: false,
    agentId: null,
    events: () => [],
    gaps: () => [],
    incomplete: false,
    running: false,
    deviceGone: false,
    deviceGoneWhy: '这个会话所属的设备已被删除',
  },
)

const emit = defineEmits<{
  /** 跑一条一次性命令：交给 exec connector。 */
  (e: 'run', req: ConvExecRequest): void
  /** 把一段文本送进终端面板的通道（等于在终端里敲这一行）。 */
  (e: 'send', text: string): void
  /** 说一句话：交给设备的 ACP 子进程。 */
  (e: 'say', text: string): void
  /**
   * 停止正在跑的那一轮：发 `session/cancel` 到设备，**不关闭会话**。
   * 只在回复在飞时才可点（`:disabled` 已经拦住了闲着时的点击，这里再兜底一次）。
   */
  (e: 'cancel'): void
  /**
   * 打开轨迹抽屉。`opener` 是触发它的那个元素——抽屉关闭后焦点还给它，
   * 所以触发方必须把它交出来，而不是让抽屉去猜 `activeElement`
   * （Safari 里点按钮不移动焦点，那时猜出来的是 body）。
   */
  (e: 'open-traj', opener: HTMLElement | null): void
}>()

const EMPTY_HAS_AGENT =
  '选好目标后按 ⏎ 发送。选「对话」时，回复会以事件流回来并写进轨迹——' +
  '刷新页面后还在。'

const EMPTY_NO_AGENT =
  '这个会话没有选 agent，所以还不能对话。' +
  '消息要经过设备上的 agent 才有回复，而 Hub 不替你挑一个——' +
  '那会替你做一个写进会话历史的决定。'

const draft = ref('')
/** 默认是 `chat` 而不是 `exec`：这个组件的名字是「对话流」。 */
const target = ref<ConvTarget>('chat')
const trajBtnEl = ref<HTMLButtonElement | null>(null)
const flowEl = ref<HTMLElement | null>(null)
/** 用户手动往上翻过就先不自动滚——自动滚回去等于抢走他正在读的位置。 */
let pinnedToBottom = true

/**
 * 输入区整体不可用。
 *
 * 两个理由合在这里只因为「能不能打字」这一个结果是同一个；**原因不合并**
 * ——`ended` 与 `deviceGone` 各写各的一句（见 `whyBlocked`）。
 */
const composeBlocked = computed(() => props.ended || props.deviceGone)

const canSubmit = computed(() => !composeBlocked.value && draft.value.trim().length > 0)

/** 「对话」这个目标为什么不能用。
 *
 * **判据只有 `agentId`**：对话通道是**说第一句话时才开**的（见页面
 * `ensureChatChannel`），所以「通道还没建」不是不能发，而是**还没发**——
 * 把它算进判据就死锁了：目标选不了 → 发不出去 → 通道永远建不起来。
 *
 * 通道建不建的**结果**由 `convError` 表达，而且它不拦发送：agent 不可用
 * 是设备侧当时的状态，过一会可能就好了，所以这里要留重试的路。
 */
const chatBlocked = computed(() => {
  if (props.ended) return '会话已结束'
  // 设备被删：这里不是「等它回来」，而是「它不会再回来了」。说成「会话已结束」
  // 会把用户引去查会话，而会话好好地在库里。
  if (props.deviceGone) return '所属设备已被删除'
  if (!props.agentId) return '没有选 agent'
  return ''
})

const canChat = computed(() => chatBlocked.value === '')

/**
 * 发送按钮**为什么**被禁用。空串 = 现在能按。
 *
 * 与 `:disabled` 分开绑：一个 `disabled` 属性只说得清「不能按」，说不出是
 * 「还没打字」还是「这个会话没有 agent」——而这两条的出路完全不同（打一行字
 * vs 回去补选 agent），页面该给的解释也必须是用户真撞上的那一条。判据顺序
 * 即优先级：先看会话与设备这两个「不会再变回来」的事实，再看用户自己能改的
 * （打字 / 换目标）。
 *
 * **它不等于 `canSubmit`**：后者是发送路的门禁（`submit()` 用它），不含
 * 「目标是 chat 而 chat 不可用」这一条——那一条只决定按钮长什么样，因为
 * `submit()` 内部会各自再判一次。
 */
const runBlocked = computed(() => {
  if (props.ended) return 'ended'
  if (props.deviceGone) return 'device-removed'
  if (draft.value.trim().length === 0) return 'empty'
  if (target.value === 'chat' && !canChat.value) return 'no-agent'
  return ''
})

/** 禁用原因的一句话，挂在按钮的 `title` 上——灰着的按钮不说话就是个谜。 */
const runBlockedWhy = computed(
  () =>
    ({
      ended: '这个会话已结束：不会再建立通道',
      'device-removed': props.deviceGoneWhy
        ? `${props.deviceGoneWhy}：发不出消息`
        : '所属设备已被删除：发不出消息',
      empty: target.value === 'chat' ? '先写一句话' : '先写一条命令',
      'no-agent': '这个会话没有选 agent，不能对话',
      '': '',
    })[runBlocked.value],
)

/** 停止按钮为什么被禁用。空串 = 可以按（有一轮在飞）。 */
const stopBlocked = computed(() => {
  // 判据是「最后一轮没有 `turn_end`」，也就是 `running` 或 `incomplete`
  // 任一为真。注意它**不等于**「agent 正在回复」——协议上「还在跑」与
  // 「结尾丢了」长得一样（`session-and-trajectory.md` 已裁定不承诺补齐），
  // 所以 `turn_end` 一旦丢失，这个按钮会一直可点，而按下去设备只会留痕、
  // 不回话。那是协议边界，不是这个判据写错了。
  if (!props.running && !props.incomplete) return 'idle'
  return ''
})

/** 停止按钮的禁用原因文案。 */
const stopBlockedWhy = computed(() => {
  if (!props.running && !props.incomplete) {
    return '没有正在回复的轮次：等 agent 回一句话后，这里才能点'
  }
  return ''
})

/**
 * 请求掐掉正在跑的那一轮（`session/cancel`），**不关闭会话**。
 *
 * 不发文本、也不碰输入框——那是 `submit()` 的事；这个按钮只是把「停止」
 * 这件事交给页面，由它去调 Hub 的 cancel 端点。
 */
function stop(): void {
  if (stopBlocked.value) return
  emit('cancel')
}

/**
 * 输入区为什么不可用（整段一句，写在提示行里）。两个事实两句话，
 * 顺序与页面两条横幅一致：先说会话自己的状态，再说设备。
 */
const whyBlocked = computed(() => {
  if (props.ended) return '这个会话已结束：工作区不再可达，面板与命令都不会再建立通道。'
  if (props.deviceGone) {
    return `${props.deviceGoneWhy}：对话与轨迹都保留着，但已经发不出消息，也跑不了命令。`
  }
  return ''
})

const placeholder = computed(() => {
  if (props.ended) return '这个会话已结束：面板与命令都不会再建立通道'
  if (props.deviceGone) return `${props.deviceGoneWhy}：发不出消息`
  if (target.value === 'chat') {
    return props.agentId
      ? `对 ${props.agentId} 说一句话`
      : '这个会话没有选 agent，不能对话'
  }
  return `在 ${props.path} 里跑一条命令（不是 shell：管道与重定向不会生效）`
})

const targetHint = computed(() => {
  if (composeBlocked.value) return ''
  if (target.value === 'chat') {
    if (!canChat.value) return chatBlocked.value
    // 开通道失败要说出来：输入框已经清空，而用户没看到任何回复——
    // 不说明的话他会以为「发出去了没回音」，然后一遍遍重发。
    if (props.convError) return `上一次没开成对话通道（${props.convError}）。再发一次会重试。`
    // 「慢」要说在前面：实测有一次 130 秒才回完。不预告的话，用户会把
    // 长时间没有回复读成「卡住了」，然后重复发送。
    return `交给 ${props.agentId}：回复可能很长也很慢（实测最慢一次 130 秒才回完），` +
      '期间页面不会卡住——事件是流式的'
  }
  if (target.value === 'exec') {
    return 'exec 是一次性命令：跑完就结束、通道随即关闭，输出与退出码一次回完（没有交互，也不进轨迹）'
  }
  return props.terminalReady
    ? '送进已打开的终端面板：等于在终端里敲这一行并回车'
    : '终端面板还没有通道：先按 o 打开面板并与它交互一次'
})

/**
 * 已发出、但轨迹里还没读回来的那几句（本地回显）。
 *
 * # 为什么需要它
 *
 * user 行唯一的来源是库里那条 `user_message_chunk`（见 `rowOf`），而页面只在
 * 轨迹重读回来后才知道它存在（`SessionPage.refreshTrajectory` 只信库里的行）。
 * 两者之间有一段空白，长度取决于该事件何时落库；实测 agent 最慢一次 130 秒才
 * 回完，那段时间界面上一句话都没有，用户读成「消息没发出去」，然后一遍遍重发。
 *
 * # 去重
 *
 * 轨迹里一旦出现一条 `user_message_chunk` 包含某句待定文本，就把它从这里删掉：
 * 那说明它已经落库、已经由 `rows` 从库里画出来了，留着会画两遍。
 */
const pending = ref<{ id: number; text: string }[]>([])
let pendingSeq = 0

/** 把 Hub 给的事件排成行，断口插在它该在的位置。 */
const rows = computed<ConvRow[]>(() => {
  const out: ConvRow[] = []
  // **断口按「它前面断了的那条存在的事件」索引，也就是 `gap.to`**。
  //
  // `Gap` 是半开区间 `[from, to)`：`to` 是那个**存在**的 seq。所以断口标记
  // 画在 `seq == to` 那条事件之前。
  //
  // 按 `from` 索引是错的，而且错得很隐蔽：断口里的 seq 都不在库里，
  // 于是「找到 seq == from 的那条事件」永远找不到，标记一个也不画——
  // 页面看起来就是连续的。这正是这条功能要防的事。
  const gapBefore = new Map<number, TrajectoryGap>()
  for (const g of props.gaps) {
    gapBefore.set(g.to, g)
  }
  for (const ev of props.events) {
    const gap = gapBefore.get(ev.seq)
    if (gap) {
      // 画一句「缺了 N 条」而不是悄悄补上内容——补上就等于撒谎，
      // 因为没有第 3 层（见模块注释）能把它们找回来。
      out.push({
        kind: 'gap',
        key: `gap-${gap.from}`,
        // `after` 是断口前最后一条**存在**的 seq，也就是 `from - 1`。
        missing: gap.missing,
        after: gap.from - 1,
      })
    }
    const row = rowOf(ev)
    if (!row) continue

    // 合并：**说话人没换**的连续 chunk 接在上一条消息后面。
    //
    // 这是「流式追加」的兑现处。实测一次 500 字回复是 30–80 个 chunk，
    // 逐条铺开会把一句话切成几十行——而它们本来就是同一句话被拆开发来的。
    // 判据只有「上一条是不是同一个说话人的消息」：ACP 的 chunk 既没有
    // 「这条说完了」的标记，也没有会话内 id，能用的只有说话人这个信号。
    const prev = out.at(-1)
    if (row.kind === 'msg' && prev && prev.kind === 'msg' && prev.who === row.who) {
      prev.text += row.text
      continue
    }
    // 合并同一条工具调用：实测 ACP 为同一个 `toolCallId` 连发
    // `tool_call_update`（live 收到过 4 条）。中间态的意义是「它后来变成了
    // 什么」，不是「它曾经是什么」，所以只留最后一次的状态。
    if (row.kind === 'tool' && row.id && prev && prev.kind === 'tool' && prev.id === row.id) {
      prev.status = row.status
      prev.detail = row.detail || prev.detail
      continue
    }
    out.push(row)
  }
  // 只有在**既没有开始标记也没有结束标记**时才说「结尾丢了」。一轮开了还没关
  // （`running`）是正常的长回复状态——设备侧实测过一次 130 秒才回完，把那整段
  // 时间标成「未完成」是在报一个没发生的事实。
  if (props.incomplete) {
    out.push({ key: 'incomplete', kind: 'incomplete' })
  }
  // 本地回显：已经发出去、但轨迹里还没出现的那几句。接在最后，因为它们就是
  // 最新的。**不标成「已送达」**——页面不知道 Hub 收没收到，只知道自己发出去了。
  for (const p of pending.value) {
    out.push({ kind: 'msg', key: `pending-${p.id}`, who: 'user', text: p.text, pending: true })
  }
  return out
})

/** 轨迹里已经出现的 user 文本（拼成一段，便于按包含关系比对）。 */
const dbUserText = computed(() =>
  props.events
    .filter((ev) => ev.kind === 'user_message_chunk')
    .map((ev) => chunkText(parsePayload(ev.payload), ev.payload))
    .join(''),
)

/**
 * 轨迹已经读回来某句待定文本之后，把它从本地回显里撤掉。
 *
 * 不写在 `rows` 里（那会在 computed 里改 ref），改成 watch：一旦库里的 user 文本
 * 包含某句待定内容，就说明它已经作为库里的一行画出来了，留着会画两遍。按**包含**
 * 匹配而不是全等——ACP 会把一句话拆成多个 chunk，而这里拼起来的是全文。
 */
watch(dbUserText, (text) => {
  if (!pending.value.length || !text) return
  const left = pending.value.filter((p) => !text.includes(p.text.trim()))
  if (left.length !== pending.value.length) pending.value = left
})

const hasRows = computed(() => rows.value.length > 0)

/**
 * 一条落成库的事件 → 一行。
 *
 * `payload` 是原始 JSON 字符串：**解开失败就退化成原文**，不整条丢掉。
 * 丢掉的表现是「agent 说了话，页面上什么都没有」，而那比一段看不懂的 JSON 糟。
 */
function rowOf(ev: TrajectoryEvent): ConvRow | null {
  const p = parsePayload(ev.payload)
  switch (ev.kind) {
    case 'turn_end':
      return { kind: 'turn', key: `${ev.id}`, reason: str(p?.stopReason) }
    case 'user_message_chunk':
      return { kind: 'msg', key: `${ev.id}`, who: 'user', text: chunkText(p, ev.payload) }
    case 'agent_message_chunk':
      return { kind: 'msg', key: `${ev.id}`, who: 'agent', text: chunkText(p, ev.payload) }
    case 'tool_call':
    case 'tool_call_update':
      return {
        kind: 'tool',
        key: `${ev.id}`,
        id: toolId(p),
        name: toolName(p, ev.kind),
        status: str(p?.status) || 'running',
        detail: toolDetail(p),
      }
    case 'available_commands_update':
      return { kind: 'meta', key: `${ev.id}`, text: commandsNote(p) }
    case 'usage_update':
      return { kind: 'meta', key: `${ev.id}`, text: usageNote(p) }
    default:
      // 未知事件类型照样画（`dynamic_system.md` 的要求与此一致）：它也是
      // agent 说过的话的一部分，静默丢掉就是让用户看不见。
      return { kind: 'msg', key: `${ev.id}`, who: 'system', text: fallbackText(ev, p) }
  }
}

/**
 * 消息行头的那三个字。
 *
 * 用「我 / agent / 系统」而不是 ACP 的事件类型名（`agent_message_chunk`）：
 * 行头是给人读的，而类型名是协议里的东西——把协议名直接端上页面，
 * 用户看到的是一串不认识的下划线。
 */
function whoLabel(who: 'user' | 'agent' | 'system'): string {
  if (who === 'user') return '我'
  if (who === 'agent') return 'agent'
  return '系统'
}

function parsePayload(raw: string): Record<string, unknown> | null {
  try {
    const v: unknown = JSON.parse(raw)
    return v && typeof v === 'object' ? (v as Record<string, unknown>) : null
  } catch {
    return null
  }
}

function str(v: unknown): string {
  return typeof v === 'string' ? v : ''
}

/**
 * chunk 的文本在 `content` 里。`content` 有三种实测形态，都要认：
 * 字符串、数组（多模态 / `session/load` 重放）、以及**单个对象**
 * `{type:"text", text:"…"}`——`opencode acp` 的实时 `agent_message_chunk`
 * 就是最后这种，只认数组会让整串 payload 原样上屏。
 *
 * `raw` 是**原始 payload 字符串**，专门给「解不开 JSON」那一档用：那一档必须
 * 原样显示出来，而不是退化成一个空对象——`JSON.stringify(p ?? {})` 会得到
 * `{}`，于是 agent 说的话在页面上变成一对空花括号，而那比一段看不懂的原文
 * 糟得多（用户连「它到底说了什么」都没了）。
 */
function chunkText(p: Record<string, unknown> | null, raw: string): string {
  if (!p) return raw
  const c = p.content
  if (typeof c === 'string') return c
  if (Array.isArray(c)) {
    const text = c
      .map((b) => {
        const blk = b as Record<string, unknown>
        if (typeof blk.text === 'string') return blk.text
        return ''
      })
      .join('')
    if (text) return text
  }
  if (c && typeof c === 'object' && typeof (c as Record<string, unknown>).text === 'string') {
    return (c as Record<string, unknown>).text as string
  }
  if (typeof p.text === 'string') return p.text
  return raw
}

/**
 * 这次工具调用的 id（ACP 的 `toolCallId`）。
 *
 * **缺了它就必须退回「每条各一块」**，不能退回「按 title 合并」：同名工具
 * 在一轮里被调两次是常事，按名字合并会把两次不同的调用画成一块。
 * 而 `tool_call_update` 与对应的 `tool_call` 同名——合并的正是它们。
 *
 * 拿不到 id 时返回空串，而空串**不参与合并**（见 `rows`）：宁可多画几块，
 * 也不要把不是同一次的工具调用并成一块。
 */
function toolId(p: Record<string, unknown> | null): string {
  const v = p?.toolCallId
  return typeof v === 'string' ? v : ''
}

function toolName(p: Record<string, unknown> | null, kind: string): string {
  const title = typeof p?.title === 'string' ? p.title : ''
  if (title) return title
  const raw = p?.rawInput
  if (raw && typeof raw === 'object') {
    const o = raw as Record<string, unknown>
    for (const k of ['command', 'filePath', 'path', 'pattern']) {
      if (typeof o[k] === 'string') return String(o[k])
    }
  }
  return kind === 'tool_call_update' ? 'tool update' : 'tool'
}

/** 工具块折叠起来的那部分：命令 / 路径优先，拿不到就 dump 整个入参。 */
function toolDetail(p: Record<string, unknown> | null): string {
  const raw = p?.rawInput ?? p?.rawOutput
  if (raw === undefined) return ''
  return typeof raw === 'string' ? raw : JSON.stringify(raw, null, 2)
}

/** 未知类型的兜底文案：带上 `kind`，用户至少能报「哪种事件没在页面上」。 */
function fallbackText(ev: TrajectoryEvent, p: Record<string, unknown> | null): string {
  const head = `${ev.kind}`
  if (!p) return ev.payload
  return `${head} ${JSON.stringify(p)}`
}

/**
 * `available_commands_update` 的一句话：这台 agent 现在能用哪些命令。
 *
 * 命令清单是**元数据**，不是 agent 说的话：它对用户的意义是「这些斜杠命令可用」，
 * 数值本身（每个命令的 `description` / `input`）留在轨迹抽屉里看原文。
 * 认不出 `availableCommands` 的形状时退回一句不撒谎的说明，不打印原始 JSON。
 */
function commandsNote(p: Record<string, unknown> | null): string {
  const list = p?.availableCommands
  if (!Array.isArray(list)) return 'agent 更新了可用命令清单'
  const names = list
    .map((c) => (c && typeof c === 'object' ? str((c as Record<string, unknown>).name) : ''))
    .filter((n) => n !== '')
  if (names.length === 0) return 'agent 更新了可用命令清单（当前为空）'
  return `可用命令：${names.join('、')}`
}

/**
 * `usage_update` 的一句话：这一轮大概用掉多少 context。
 *
 * `used` / `size` 都是 token 数。只报「用了多少 / 一共多少」，不算百分比——
 * `size` 未必是这一轮的窗口上限，自己除出来的百分比会像一句承诺。
 */
function usageNote(p: Record<string, unknown> | null): string {
  const used = typeof p?.used === 'number' ? p.used : null
  const size = typeof p?.size === 'number' ? p.size : null
  if (used === null && size === null) return 'agent 更新了 token 用量'
  if (size === null) return `token 用量：${used}`
  if (used === null) return `token 用量：上限 ${size}`
  return `token 用量：${used} / ${size}`
}

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
  if (target.value === 'chat') {
    if (!canChat.value) return
    // 先记进本地回显再上报：这一刻页面唯一确定的事实是「我把它发出去了」。
    // 不记的话，从按下发送到库里那条 `user_message_chunk` 被读回来之间是空白，
    // 用户会以为没发出去（见 `pending` 的注释）。
    pending.value = [...pending.value, { id: pendingSeq++, text }]
    emit('say', text)
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

function onScroll(): void {
  const el = flowEl.value
  if (!el) return
  // 底部 24px 内算「贴着底」：消息是一条一条追加上来的，差几像素不该
  // 把用户甩出自动跟随。
  pinnedToBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24
}

/** 有新内容就贴到底——除非用户自己翻上去了。 */
watch(
  () => rows.value.length,
  async () => {
    if (!pinnedToBottom) return
    await Promise.resolve()
    const el = flowEl.value
    if (el) el.scrollTop = el.scrollHeight
  },
)
</script>

<template>
  <div class="af-flowhost">
    <!--
      只在「没有 agent」时挂这条横幅。其他情形不挂：以前那条「M1 无数据」
      在这个里程碑已经不成立了（对话真的能跑），留着它才是在骗人。
    -->
    <div v-if="!agentId && !ended" class="af-stage-note" role="note">
      <span class="af-badge af-badge--warn">不能对话</span>
      <div class="af-stage-note__body">
        <p>
          <b>这个会话没有选 agent</b>，所以「对话」目标不可用。消息要经过设备上
          的 agent 才有回复，Hub 不替用户挑一个——那会替用户做一个写进会话历史的
          决定。
        </p>
      </div>
    </div>

    <!-- 对话流本体：role="log" 是「会不断追加」的语义，读屏会播报新增内容 -->
    <div
      ref="flowEl"
      class="af-flow af-flowhost__flow"
      :class="{ 'af-flow--rows': hasRows }"
      role="log"
      aria-label="对话流"
      data-conv-log
      @scroll="onScroll"
    >
      <template v-if="hasRows">
        <template v-for="row in rows" :key="row.key">
          <!-- 断口：写清缺了几条，不假装连续 -->
          <p v-if="row.kind === 'gap'" class="af-gap" data-conv-gap>
            <span aria-hidden="true">⋯</span>
            此处缺失 {{ row.missing }} 条（seq {{ row.after }} 之后断开）
          </p>

          <p v-else-if="row.kind === 'incomplete'" class="af-gap af-gap--open" data-conv-incomplete>
            <span aria-hidden="true">◌</span>
            本轮未完成：既没有开始标记也没有收到 <span class="af-fp">turn_end</span>，结尾丢了
          </p>

          <div
            v-else-if="row.kind === 'msg'"
            class="af-msg"
            :class="{ 'af-msg--pending': row.pending }"
            :data-who="row.who"
            :data-pending="row.pending || undefined"
            data-conv-msg
          >
            <span class="af-msg__who">{{ whoLabel(row.who) }}</span>
            <div class="af-msg__body">{{ row.text }}</div>
            <!--
              「已发出 / 还没在轨迹里出现」要看得见：这一行的文字是页面自己补的，
              库里还没有它。不标出来的话，用户会以为这是「已送达」的凭据。
            -->
            <span v-if="row.pending" class="af-msg__pending" data-conv-pending>已发送 · 等待回执</span>
          </div>

          <p v-else-if="row.kind === 'meta'" class="af-conv-meta" data-conv-meta>{{ row.text }}</p>

          <details v-else-if="row.kind === 'tool'" class="af-tool" data-open="false" data-conv-tool>
            <summary class="af-tool__head">
              <span class="af-tool__name">{{ row.name }}</span>
              <span class="af-tool__ms">{{ row.status }}</span>
            </summary>
            <pre v-if="row.detail" class="af-tool__body">{{ row.detail }}</pre>
          </details>

          <p v-else-if="row.kind === 'turn'" class="af-turn" data-conv-turn>
            <span aria-hidden="true">✓</span> 本轮结束：{{ row.reason || '未说明原因' }}
          </p>
        </template>
      </template>

      <EmptyState
        v-else-if="!agentId"
        glyph="◇"
        title="还不能对话"
        :text="EMPTY_NO_AGENT"
      />
      <EmptyState v-else glyph="◇" title="还没有消息" :text="EMPTY_HAS_AGENT" />
    </div>

    <form class="af-compose" @submit.prevent="submit">
      <div class="af-compose__row">
        <textarea
          v-model="draft"
          class="af-textarea af-compose__input"
          rows="1"
          :disabled="composeBlocked"
          :aria-label="
            ended
              ? '输入（这个会话已结束：不会再建立通道）'
              : deviceGone
                ? '输入（所属设备已被删除：发不出消息）'
                : '输入：按 ⏎ 发送'
          "
          :placeholder="placeholder"
          data-conv-input
          @keydown="onKeydown"
        ></textarea>

        <select
          v-model="target"
          class="af-select af-compose__target"
          :disabled="composeBlocked"
          aria-label="发送目标"
          data-conv-target
        >
          <option value="chat" :disabled="!canChat">对话 · 交给 agent</option>
          <option value="exec">exec · 一次性命令</option>
          <option value="terminal" :disabled="!terminalReady">终端 · 送进 PTY</option>
        </select>

        <button
          class="af-btn af-btn--primary"
          type="submit"
          :disabled="!canSubmit || (target === 'chat' && !canChat)"
          :data-blocked-by="runBlocked || undefined"
          :title="runBlockedWhy || undefined"
          data-conv-run
        >
          {{ target === 'chat' ? '发送' : '执行' }}
        </button>
      </div>

      <!--
        提示行是**正文**，不是 tooltip：§3.13 要求信息不许只活在 hover 里，
        placeholder 在输入框里有内容之后就看不见了。所以「这里不是 shell」
        与「结束了为什么不能打字」两条都写在这里，而不只写在 placeholder。
        键位与位置在左、次要动作在右；理由另起一行，才挤不掉右边的按钮。
      -->
      <p class="af-compose__hint af-field__hint">
        <span><kbd>⏎</kbd> 发送</span>
        <span><kbd>⇧⏎</kbd> 换行</span>
        <span><kbd>t</kbd> 轨迹抽屉</span>
        <span class="af-spacer"></span>
        <span>{{ deviceName }} · <span class="af-fp">{{ path }}</span></span>
        <span class="af-compose__actions">
          <button
            class="af-btn af-btn--sm af-btn--ghost"
            type="button"
            :disabled="!!stopBlocked"
            :data-blocked-by="stopBlocked || undefined"
            :title="stopBlockedWhy || undefined"
            data-conv-stop
            @click="stop"
          >
            <span aria-hidden="true">⏹</span> 停止
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
        </span>
      </p>

      <!--
        理由单独一行（不是提示行里的一个 flex 项）：面板坞打开时提示行只剩
        约 412px，把这段正文塞进那一行会把右侧的停止 / 轨迹挤到下一行——
        「哪个按钮归哪一块」正是这次要修的。
      -->
      <p v-if="whyBlocked || targetHint" class="af-compose__notes af-field__hint">
        <span v-if="whyBlocked" class="af-compose__why">
          {{ whyBlocked }}
        </span>
        <span v-else-if="target === 'exec'" class="af-compose__why">
          这里不是 shell：一行按空白切成程序名与参数，引号只用来把带空格的参数
          合成一个——管道与重定向、通配与变量展开都不会生效。
        </span>
        <span v-else-if="target === 'chat' && chatBlocked" class="af-compose__why">
          不能对话：{{ chatBlocked }}。
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
  overflow: auto;
}
/* 有内容时改成「顶对齐 + 留白」，不再把整块挤到垂直居中 */
.af-flow--rows {
  justify-content: flex-start;
  gap: var(--af-space-3);
  padding: var(--af-space-4) var(--af-space-5);
}
.af-compose__hint {
  /* 窄列里键位与位置先折行：这一组仍然可以挤，右边的次要动作不许被挤走 */
  flex-wrap: wrap;
  row-gap: var(--af-space-2);
}
/* 次要动作成组。**不设** `margin-left: auto`：提示行在 `.af-compose` 里是
   `align-items: stretch` 的列向 flex 项，这一组已被拉满整行宽度，`justify-content`
   也默认是 `flex-start`——flex 的自动外边距对齐在「整行宽」的纵向容器里只会把
   内容在组内左对齐，两个按钮就会贴着左边缘而不是停在设备路径右边。 */
.af-compose__actions {
  display: flex;
  align-items: center;
  gap: var(--af-space-2);
}
/* 理由另占一行：`.af-compose` 是 `flex-direction: column; align-items: stretch`，
   所以这一行的盒子本来就占满整行宽，里面的句子靠 `center` 居中——
   而 `.af-field__hint` 的 `text-align` 是缺省的，不写这句它只会吊在左下角。 */
.af-compose__notes {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-1);
  margin: 0;
  text-align: center;
}

/* 断口 / 收尾：这两者是「完整性可见」的两个方向，样式同族（虚线 + 弱色） */
.af-gap {
  margin: 0;
  padding: var(--af-space-2) var(--af-space-3);
  font-size: var(--af-text-xs);
  color: var(--af-fg-tertiary);
  border: 1px dashed var(--af-border-default);
  border-radius: var(--af-radius-md);
  background: var(--af-surface-2);
}
.af-gap--open {
  border-color: var(--af-warn);
  color: var(--af-warn);
}
.af-turn {
  margin: 0;
  font-size: var(--af-text-xs);
  color: var(--af-fg-tertiary);
}

/* 元数据行（可用命令 / token 用量）。比 `.af-turn` 再淡一档、不换行挤压：
   它是背景信息，读到就够，不该和对话正文争注意力。 */
.af-conv-meta {
  margin: 0;
  font-size: var(--af-text-xs);
  color: var(--af-fg-disabled);
}

/* 阶段横幅。视觉语言与 AgentCatalog 的同名块同源（§3.11 的「为什么空」：
   警告色左边线 + 徽标 + 说明，不跟随滚动，也不是 toast），**形状是另一回事**，
   所以这两份各自留在本地、不搬进 `styles/components.css`：
     目录那份是一张**独立卡片**——四边描边 + 圆角 + `margin-bottom`，正文 md；
     对话这份是**贴在流上的一条**——只有 `border-bottom`（与滚动区接壤的那一边
     才需要分界），无圆角无外边距，正文 xs。
   提到全局按「逐字重复」合并会把其中一个渲染成错的：要真收敛得先定
   `--af-*` 变体（见 `src/styles/components.test.ts` 那条用例）。 */
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
