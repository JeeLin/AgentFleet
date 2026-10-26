/* ===========================================================================
   channel.ts —— 通道生命周期（子任务 5a 之上的薄层）
   ---------------------------------------------------------------------------
   一条通道 = 面板与设备之间的一条 `(node_id, channel_id)`。这个文件负责
   **什么时候建、什么时候关、输出往哪送**，不负责渲染——渲染是面板组件的事。

   ── 生命周期只有两条规则（规范 §5.5 / §7.2） ─────────────────────
   1. **懒建立**：面板第一次交互才 `open_channel`。打开一个关着的面板不建
      通道，因为还没有任何数据要流动。
   2. **关面板才 `close_channel`**；**切 tab 不关**。`DECISIONS.md` D10：
      切 tab 是视图切换，不是生命周期边界。边界是「关掉这个 tab」。

   代价是 N 个 tab = N × 面板数条活跃通道，所以 tab 上要显示面板数徽标。

   ── 为什么不在这里排队重发 ────────────────────────────────────────
   下行断线期间发出去的 `/send` **直接丢弃并计数**，由 UI 标注「有 N 条已丢」。
   排队会让重连之后一整段字符乱序灌进 PTY——那比丢几个字符难查得多，而
   `/send` 本来就是无状态裸字节（`protocol.md` §7 的可靠投递只覆盖带 `id`
   的消息）。

   ── 上行都要 CSRF，下行不要 ───────────────────────────────────────
   下行是 `EventSource`，**它发不了自定义请求头**，所以 CSRF 双重提交只可能
   落在三个上行端点上。`http.ts` 按方法自动回填 `X-CSRF-Token`，这里只管
   用 `request()`。

   ── `Last-Event-ID` 由浏览器带 ────────────────────────────────────
   Hub 的 `Envelope::to_event` 给每条事件设了 `id:`（`routes/stream.rs`），
   所以 `EventSource` 断线重连时会自动带 `Last-Event-ID`，Hub 从有界回放
   缓冲里补齐。前端**不手写重连**：`EventSource` 自带指数退避，手写一层只会
   和它抢。
   =========================================================================== */

import { del, postJson, request } from './http'

/** SSE 连接状态。`connecting` 是首次建连与 EventSource 自动重连期间。 */
export type StreamState = 'connecting' | 'online' | 'offline'

/** 通道自身状态。文案映射在 `CHANNEL_STATE_LABEL`。 */
export type ChannelState = 'idle' | 'opening' | 'ready' | 'failed' | 'closed'

/** 通道帧的载荷。`channel.frame` 事件的 `payload` 就是它本身——别再剥一层。 */
export interface ChannelFrame {
  node_id: string
  channel_id: string
  /** `channel_data` 的 `c` 字段（terminal 是 `{ bytes }` / `{ resize }`）。 */
  payload: unknown
}

/**
 * 建通道时的默认尺寸。
 *
 * 与 node 侧 `terminal.rs` 的 `DEFAULT_COLS/ROWS` 同一个值、同一个理由：
 * 80×24 是所有终端的出厂默认，也是「还没量到尺寸」时唯一站得住的值。
 * 它只是 `open_channel` 的 `params` 与 xterm 的初值——第一次 fit 成功后
 * 立刻被真值覆盖。
 *
 * 放在这里而不是 `TerminalPanel.vue`：面板与页面都要用它（面板量不到尺寸时
 * 用它当 xterm 初值，页面建通道时用它当下发值），两处各写一份必然漂。
 */
export const DEFAULT_COLS = 80
export const DEFAULT_ROWS = 24

/** 开一条通道所需的全部输入。 */
export interface ChannelSpec {
  sessionId: string
  /** `terminal` / `files` / `exec` / `acp`。 */
  connector: string
  /** connector 私有参数（terminal 的 `cwd` / `cols` / `rows` 等）。 */
  params?: Record<string, unknown>
  /**
   * 开通道走哪个端点。缺省（以及 `channel`）是通用的 `POST /api/channels`。
   *
   * `conversation` 走 `POST /api/sessions/{id}/conversation`：
   *
   * - **`acp` 不是 connector**，它不出现在设备的能力上报里，所以通用端点
   *   的能力门禁永远判它「未上报」并 409；
   * - 对话的启动配置（`agent_config`）存在 Hub 侧，设备没有那张表。只有专用
   *   端点会把它读出来随 `params` 下发，通用端点只透传页面给的东西——
   *   让页面自己拼配置，等于把「设备上用哪份配置」变成可自相矛盾的输入。
   *
   * 两种端点返回同一个 `OpenedChannel`，所以此后的一切（信用、SSE、关闭）
   * 完全复用。
   */
  openVia?: 'channel' | 'conversation'
}

/** `POST /api/channels` 的响应。 */
interface OpenedChannel {
  channel_id: string
  node_id: string
  session_id: string
}

/** 一个面板持有的一条通道。 */
export interface ChannelHandle {
  /** 调用方的键（每个 tab 每个面板各一个，例如 `s-1:terminal`）。 */
  readonly key: string
  /** `idle` → `opening` → `ready`；失败进 `failed`，关掉进 `closed`。 */
  state(): ChannelState
  /** Hub 生成的 channel_id；还没开时为 `null`。 */
  id(): string | null
  /** 最近一次失败的错误码（`device_offline` / `session_ended` / …）。 */
  errorCode(): string | null
  /**
   * 断线期间被丢弃的输入条数。
   *
   * 这个数必须能被 UI 读出来：丢弃是**有意的**（排队会让重连后的字符
   * 乱序灌进 PTY），但用户有权知道「我刚才敲的东西没进去」。
   */
  dropped(): number
  /** 建通道（幂等：已 ready 时直接返回）。 */
  open(): Promise<void>
  /**
   * 送一段文本。断线期间**丢弃**并计入 `dropped`；通道还没建好时**排队**，
   * 建好后按原顺序补发。
   *
   * 两者的分野是「用户还能不能继续用」：断线时 PTY 那段连接已经不可信，
   * 补发会把字符乱序灌进去；而未建通道只是「还没通」，等它通了再发就是
   * 用户本来想要的顺序。
   */
  send(text: string): void
  /** 终端尺寸。未建通道 / 断线时记为「待发」，建好或重连后补一次。 */
  resize(cols: number, rows: number): void
  /** 关通道。幂等。 */
  close(): Promise<void>
  /** 订阅本通道的帧。返回退订函数。 */
  onFrame(fn: (frame: ChannelFrame) => void): () => void
  /** 订阅状态变化。返回退订函数。 */
  onState(fn: (state: ChannelState) => void): () => void
}

export interface HubOptions {
  /** 注入 EventSource 的构造器（测试用）。缺省用全局的那个。 */
  createEventSource?: (url: string) => EventSource
  /** 建连的路径。缺省 `/api/stream`。 */
  streamPath?: string
}

/** 通道状态的短文案（面板头右侧那个标签，§7.2）。 */
export const CHANNEL_STATE_LABEL: Readonly<Record<ChannelState, string>> = {
  idle: '未建立',
  opening: '建立中',
  ready: '已连接',
  failed: '建立失败',
  closed: '已释放',
}

/**
 * 一个页面共用一个 `ChannelHub`：一条 SSE 订阅、一张
 * `(node_id, channel_id) → 通道` 的分派表。**不是每条通道一条 SSE**——
 * 那会把「N 个 tab 的代价」再乘一次。
 *
 * 分派键是**二元组**而不是 `channel_id` 单值：协议侧的通道键就是
 * `(node_id, channel_id)`（约束 4），两台设备各开一条 `channel_id` 相同的
 * 通道时，单值键会让帧串到对方的终端里，而接收侧没有任何机制能发现。
 */
export class ChannelHub {
  private readonly createEventSource: (url: string) => EventSource
  private readonly streamPath: string

  private source: EventSource | null = null
  private streamState: StreamState = 'connecting'
  private readonly byChannel = new Map<string, ChannelHandleImpl>()
  private readonly byKey = new Map<string, ChannelHandleImpl>()
  private readonly streamListeners = new Set<(s: StreamState) => void>()

  constructor(opts: HubOptions = {}) {
    this.streamPath = opts.streamPath ?? '/api/stream'
    this.createEventSource =
      opts.createEventSource ??
      ((url) => {
        if (typeof EventSource === 'undefined') {
          throw new Error('channel: EventSource 不可用')
        }
        return new EventSource(url)
      })
  }

  /** 现在的下行连接状态。 */
  state(): StreamState {
    return this.streamState
  }

  /** 下行状态变化（面板头上的「连接中 / 已连接 / 已断开」）。 */
  onStreamState(fn: (s: StreamState) => void): () => void {
    this.streamListeners.add(fn)
    return () => this.streamListeners.delete(fn)
  }

  /** 建 SSE。重复调用是空操作——一个页面只需要一条。 */
  connect(): void {
    if (this.source) return
    try {
      const src = this.createEventSource(this.streamPath)
      this.source = src
      src.addEventListener('open', () => this.setStream('online'))
      src.addEventListener('error', () => this.setStream('offline'))
      src.addEventListener('session.state', (ev) => this.onSessionState(ev))
      src.addEventListener('channel.frame', (ev) => this.onFrame(ev))
    } catch {
      // 构造失败（环境没有 EventSource、或 URL 被拦）——走同一条「断开」路径，
      // 让面板显示「已断开·有 N 条已丢」，而不是在启动时抛一个没人接的异常。
      this.setStream('offline')
    }
  }

  /** 断开 SSE 并释放全部通道。只在页面卸载时调（切 tab **不调**）。 */
  dispose(): void {
    for (const ch of [...this.byChannel.values()]) void ch.close()
    this.byChannel.clear()
    this.byKey.clear()
    this.source?.close()
    this.source = null
    this.streamListeners.clear()
  }

  /** 取（或建）一个键对应的通道句柄。**不建通道**，只是拿到句柄。 */
  handle(key: string, spec: ChannelSpec): ChannelHandle {
    const existing = this.byKey.get(key)
    if (existing) return existing
    const ch = new ChannelHandleImpl(key, spec, this)
    this.byKey.set(key, ch)
    return ch
  }

  /** 已开着的通道数（tab 上的面板数徽标用它）。 */
  openCount(): number {
    return [...this.byKey.values()].filter((c) => c.state() === 'ready').length
  }

  // ---- 内部 ----------------------------------------------------------------

  /** 通道句柄在 `open()` 成功后登记，好让帧能按 `(node_id, channel_id)` 找到它。 */
  bind(nodeId: string, channelId: string, ch: ChannelHandleImpl): void {
    this.byChannel.set(channelKeyOf(nodeId, channelId), ch)
  }

  unbind(nodeId: string, channelId: string): void {
    this.byChannel.delete(channelKeyOf(nodeId, channelId))
  }

  /** 断线或未连接时 `send` 该丢弃。 */
  canSend(): boolean {
    return this.streamState === 'online'
  }

  private setStream(s: StreamState): void {
    if (this.streamState === s) return
    this.streamState = s
    if (s === 'online') {
      // 重连后把断线期间攒下的尺寸补发：`resize` 没有乱序风险，
      // 最新一次就是对的（`send` 不同，所以那条一律丢弃，见 `send`）。
      for (const ch of this.byKey.values()) ch.flushPending()
    }
    for (const fn of this.streamListeners) fn(s)
  }

  private onFrame(ev: Event): void {
    const data = (ev as MessageEvent<string>).data
    let frame: ChannelFrame
    try {
      frame = JSON.parse(data) as ChannelFrame
    } catch {
      // 不是 JSON 的帧是 Hub 的 bug，不是页面的；丢掉这一条，不要整条流崩
      return
    }
    // 两个字段都必须齐：缺 `node_id` 的帧无法定位通道（协议侧的通道键是
    // 二元组），缺 `channel_id` 同理。宁可丢掉这条，也不能把它塞进
    // 「可能属于某条通道」的猜测里。
    if (!frame || typeof frame.channel_id !== 'string' || typeof frame.node_id !== 'string') return
    this.byChannel.get(channelKeyOf(frame.node_id, frame.channel_id))?.deliver(frame)
  }

  private onSessionState(ev: Event): void {
    const data = (ev as MessageEvent<string>).data
    let payload: { session_id?: string; state?: string }
    try {
      payload = JSON.parse(data) as { session_id?: string; state?: string }
    } catch {
      return
    }
    if (payload.state !== 'ended' || typeof payload.session_id !== 'string') return
    // 会话结束了，属于它的通道不会再有任何输出：主动关掉并把状态标成 released。
    // 不这么做的话，面板会一直显示「已连接」，而设备那边已经没有这条通道了。
    for (const [key, ch] of [...this.byChannel.entries()]) {
      if (ch.sessionId() === payload.session_id) {
        this.byChannel.delete(key)
        ch.markEnded()
      }
    }
  }
}

/**
 * 通道的二元键。`node_id` 与 `channel_id` 都不含 `\u0000`，
 * 所以拼接出来仍然是可逆的——`split` 一下就能拿回两段。
 */
function channelKeyOf(nodeId: string, channelId: string): string {
  return `${nodeId}\u0000${channelId}`
}

/** 每个键一条，实现放在这里而不是导出：调用方只该通过 `ChannelHub.handle` 拿到它。 */
class ChannelHandleImpl implements ChannelHandle {
  readonly key: string
  private readonly spec: ChannelSpec
  private readonly hub: ChannelHub

  private current: ChannelState = 'idle'
  private channelId: string | null = null
  /** 通道所属设备。`open()` 成功时从 Hub 的响应里取——分派键要用它。 */
  private nodeId: string | null = null
  private failure: string | null = null
  /**
   * 在建的那次 `open()`。
   *
   * 类型是 `Promise<unknown>` 而不是 `Promise<void>`：建通道的两个端点里，
   * 对话那个是 `Promise<OpenedChannel>`，而 `.then` 链最终归一成 `Promise<void>`。
   * 标成 `void` 会让「先把响应赋进来、再接 then」这种写法编译不过。
   */
  private inflight: Promise<unknown> | null = null

  /** 断线期间丢掉的**文本条数**（UI 要显示的这个数）。 */
  private dropCount = 0

  private readonly frameHandlers = new Set<(f: ChannelFrame) => void>()
  private readonly stateHandlers = new Set<(s: ChannelState) => void>()

  /** 还没建通道时收到的尺寸，建好后补发一次。 */
  private pendingSize: { cols: number; rows: number } | null = null

  /**
   * 通道还没 ready 时敲进来的字符，建好后**按原顺序**补发。
   *
   * 与 `pendingSize` 分开存而不是复用它：尺寸「最新一次就是对的」，而字符
   * 每一次都是用户做过的一件事，全得留下。合成一个字段就必然丢掉一边。
   */
  private readonly pendingText: string[] = []

  constructor(key: string, spec: ChannelSpec, hub: ChannelHub) {
    this.key = key
    this.spec = spec
    this.hub = hub
  }

  state(): ChannelState {
    return this.current
  }

  id(): string | null {
    return this.channelId
  }

  errorCode(): string | null {
    return this.failure
  }

  dropped(): number {
    return this.dropCount
  }

  sessionId(): string {
    return this.spec.sessionId
  }

  open(): Promise<void> {
    if (this.current === 'ready') return Promise.resolve()
    if (this.inflight) return this.inflight.then(() => undefined)
    this.set('opening')
    this.failure = null
    // **先建 SSE**：开了通道但没有下行订阅，用户会看到「已连接」却收不到一个字。
    this.hub.connect()
    const opening: Promise<OpenedChannel> =
      this.spec.openVia === 'conversation'
        ? postJson<OpenedChannel>(`/api/sessions/${this.spec.sessionId}/conversation`)
        : postJson<OpenedChannel>('/api/channels', {
            session_id: this.spec.sessionId,
            connector: this.spec.connector,
            params: this.spec.params ?? {},
          })
    this.inflight = opening
      .then((res) => {
        // 建好之前被关掉了（点开面板又立刻关）：这次 open 的成果作废，
        // 否则一个刚 DELETE 掉的通道会被重新标成 ready 并 bind 回 Hub，
        // 排队的字符也会发到它身上。
        if (this.current === 'closed') return
        this.channelId = res.channel_id
        this.nodeId = res.node_id
        this.hub.bind(res.node_id, res.channel_id, this)
        this.set('ready')
        // 先补尺寸再补字符：反过来会让先到的几个字符按上一次的宽度折行。
        if (this.pendingSize) {
          const size = this.pendingSize
          this.pendingSize = null
          this.resize(size.cols, size.rows)
        }
        // 按入队顺序逐条发，一条都不丢（`pendingText` 的注释）。
        // 走 `postText` 而不是 `send()`：此刻 SSE 可能还没 online，而 `send()`
        // 的 `canSend` 分支会把排过的字符丢掉并计数，排队就白排了。
        while (this.pendingText.length > 0) {
          this.postText(this.pendingText.shift() as string)
        }
      })
      .catch((err: unknown) => {
        this.failure = errorCodeOf(err)
        this.set('failed')
        throw err
      })
      .finally(() => {
        this.inflight = null
      })
    return this.inflight.then(() => undefined)
  }

  send(text: string): void {
    // 通道还没建好：排队，建好后按原顺序补发（见 `pendingText`）。
    // **不丢**——用户刚点进终端敲第一个键时通道正在建，那是本产品最常见
    // 的第一个动作，丢了它页面既收不到字符、`dropped()` 也仍是 0，用户
    // 得不到任何信号。
    if (!this.channelId || this.current !== 'ready') {
      this.pendingText.push(text)
      return
    }
    if (!this.hub.canSend()) {
      // 断线：丢弃 + 计数。**不排队**——排队会在重连后把字符乱序灌进 PTY。
      // 计的是「几次输入」，不是字节数：UI 上「有 3 条已丢」比「丢了 41 字节」
      // 更贴近用户刚才做了什么（三次回车 / 一段命令）。
      this.dropCount += 1
      this.notifyState()
      return
    }
    this.postText(text)
  }

  /**
   * 真的把字符发出去。**不问 SSE 在不在线**——调用方负责判断该不该丢。
   *
   * 排队的字符补发时走这里而不是走 `send()`：补发的那一刻 SSE 可能还没
   * `online`（点进终端 → 敲字 → 通道建好，这条路上 SSE 常常还在连），
   * 而 `send()` 的 `canSend` 分支会把它们**丢弃并计数**，排队就白排了。
   */
  private postText(text: string): void {
    if (!this.channelId) return
    // 不 await：键盘输入是「发出去就别挡着下一步」的动作。失败（通道刚被设备
    // 关掉、Hub 409）时只把状态改成 failed，让面板头显示出来。
    void request(`/api/channels/${encodeURIComponent(this.channelId)}/send`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: text,
    }).catch((err: unknown) => {
      this.failure = errorCodeOf(err)
      this.set('failed')
    })
  }

  resize(cols: number, rows: number): void {
    if (!this.canPush()) {
      // 尺寸与按键不同：**最新一次就是对的**，没有乱序风险，所以断线期间
      // 记下来、重连后补发一次。丢掉它只会让 PTY 一直按旧宽度折行。
      this.pendingSize = { cols, rows }
      return
    }
    this.postResize(cols, rows)
  }

  /** 补发待定的尺寸。流恢复在线时由 Hub 调。 */
  flushPending(): void {
    const size = this.pendingSize
    if (!size || !this.canPush()) return
    this.pendingSize = null
    this.postResize(size.cols, size.rows)
  }

  /**
   * 「现在能不能往上推一帧」的唯一判据。`resize` 与 `flushPending` 共用它——
   * 两边各写一遍的后果是形状会漂（曾一个写成三条件合并、另一个拆成两条），
   * 而漂移的那一项正是最难复现的那种：只在「刚连上就发尺寸」时出现。
   */
  private canPush(): boolean {
    return Boolean(this.channelId) && this.current === 'ready' && this.hub.canSend()
  }

  private postResize(cols: number, rows: number): void {
    if (!this.channelId) return
    void postJson(`/api/channels/${encodeURIComponent(this.channelId)}/resize`, {
      cols,
      rows,
    }).catch(() => {
      // 尺寸协商失败不是致命错：PTY 会保持上一次的尺寸，而输入仍然可用。
      // 把面板标成 failed 会把「能打字」的终端画成「建立失败」。
    })
  }

  async close(): Promise<void> {
    // 两条路都要清：**早退那条更要清**——通道还没建好就关面板（点开又立刻
    // 关）时，排着的字符仍在 `open()` 的 promise 里等着，promise 一 resolve
    // 就会把它们发到一个刚被关掉的通道上。
    this.pendingText.length = 0
    this.pendingSize = null
    const id = this.channelId
    const nodeId = this.nodeId
    if (!id) {
      this.set('closed')
      return
    }
    if (nodeId) this.hub.unbind(nodeId, id)
    this.channelId = null
    this.nodeId = null
    this.set('closed')
    try {
      await del(`/api/channels/${encodeURIComponent(id)}`)
    } catch {
      // 关不掉（Hub 已经清了、设备断了）不该拦住 UI：Hub 侧那个端点是幂等的，
      // 而且通道本来就随设备连接生灭（§7.1）。
    }
  }

  onFrame(fn: (f: ChannelFrame) => void): () => void {
    this.frameHandlers.add(fn)
    return () => this.frameHandlers.delete(fn)
  }

  onState(fn: (s: ChannelState) => void): () => void {
    this.stateHandlers.add(fn)
    return () => this.stateHandlers.delete(fn)
  }

  deliver(frame: ChannelFrame): void {
    for (const fn of this.frameHandlers) fn(frame)
  }

  markEnded(): void {
    this.channelId = null
    this.nodeId = null
    this.set('closed')
  }

  private set(s: ChannelState): void {
    if (this.current === s) return
    this.current = s
    this.notifyState()
  }

  private notifyState(): void {
    for (const fn of this.stateHandlers) fn(this.current)
  }
}

/** 从 `ApiError` 里取 code；别的错误一律 `unknown`。 */
function errorCodeOf(err: unknown): string {
  if (typeof err === 'object' && err !== null && 'code' in err) {
    const code = (err as { code: unknown }).code
    if (typeof code === 'string') return code
  }
  if (err instanceof Error && err.name === 'NetworkError') return 'network'
  return 'unknown'
}

/** 建通道失败的可读理由。与 `api.ts` 的同名函数同一条口径。 */
export function channelErrorLabel(code: string | null): string {
  switch (code) {
    case 'device_offline':
      return '设备离线，通道建不起来。'
    case 'session_ended':
      return '会话已结束，不能再开面板。'
    case 'device_busy':
      return '设备读不过来（队列打满），稍后再试。'
    case 'invalid_terminal_size':
      return '终端尺寸不合法。'
    case 'network':
      return 'Hub 连不上。'
    case 'channel_not_yours':
      return '这条通道不属于当前登录身份。'
    default:
      return code ? `通道建立失败（${code}）。` : '通道建立失败。'
  }
}
