/* ===========================================================================
   domains/device/api.ts —— `/api/devices` 的读取层（设计规范 §6 的数据契约）
   ---------------------------------------------------------------------------
   这一层只做两件事：① 把 JSON 的**形状**写成类型；② 给出设备列表的两个
   函数（读 / 删）。页面不自己拼 URL、不自己解响应、不自己判错误码——错误
   一律是 `shared/http.ts` 的 `ApiError`，页面只读 `code` 做分支。

   传输层（`fetch` / CSRF / 401 / 403 的处置）在 `shared/http.ts`，本文件
   只声明形状与端点。
   =========================================================================== */

import { del, getJson, request, type RequestOptions } from '@/shared/http'

/** 设备态。与 `hub-server/src/routes/api.rs` 的 `DeviceStatus` 逐字一致。 */
export type ApiDeviceStatus = 'online' | 'reconnecting' | 'offline'

/** `GET /api/devices` 的一项（§6.1）。 */
export interface DeviceSummary {
  /** node_id，永不变（升级只换 worker，约束 12）。 */
  id: string
  name: string
  /** `os/arch`，未上报时为空串。 */
  platform: string
  node_version: string
  status: ApiDeviceStatus
  /** `unix:<秒>` 原文；从没上报过是 `null`。**不是**人读的相对时间。 */
  last_seen: string | null
  /** 0–100；`null` = 未上报（不是 0）。 */
  cpu: number | null
  mem: number | null
  gpu: boolean
  /**
   * **已装**的 agent 名（含装了但跑不起来的）。
   *
   * 「已装」不是「能对话」：装了但没有 ACP 入口的（`Probe::NotCapable`）也在
   * 这里面，而它存在的意义就是让用户看见「为什么我的 agent 不能对话」——
   * 展示时必须按 `usable_agents` 分两档，混在一起等于没接这个字段。
   */
  agents: string[]
  /**
   * **已装且现在能对话**的 agent 名。建会话的可选项。
   *
   * 与后端 `POST /api/sessions` 的 409 门禁**同源**（都读 `agent_runtime`，
   * 都只看 `available`），所以页面在这里见到的名字一定建得成。
   */
  usable_agents: string[]
  /** `status` 是否为 Hub 内存推导值（§6.1 的「推」徽标）。 */
  derived: boolean
  /** 前 3 + `***` + 后 3，Hub 已打码。 */
  key_fingerprint: string
}

/** 读设备列表。空库返回 `[]`——那是「没有设备」，不是「Hub 坏了」。 */
export function listDevices(options?: RequestOptions): Promise<DeviceSummary[]> {
  return getJson<DeviceSummary[]>('/api/devices', options)
}

/** 浏览器平台推断结果。名字必须与 Hub 产物名一致：`mac`（不是 `macos`）、`amd64`/`arm64`。 */
export interface DownloadPlatform {
  os: 'windows' | 'mac' | 'linux'
  arch: 'amd64' | 'arm64'
}

/**
 * 下载入口给用户看的**闭集**：六种平台组合，一种都不能多、一种都不能少。
 *
 * 为什么不给自由输入：`?os=` / `?arch=` 在 Hub 侧是闭集（`agent_binary.rs:154`
 * 的 `normalize_query_platform`），闭集之外的写法直接 400。让用户敲字符串，
 * 唯一的结果是「敲错 → 400 → 看起来像下载坏了」。所以选项表就是后端闭集的
 * **一次投影**，`value` 一定落在 `DownloadPlatform` 里。
 *
 * `label` 用用户认得的说法（`Windows x64` / `macOS Apple Silicon`），`value` 用
 * 后端产物名（`amd64` / `mac`）——两者混用的话，用户在界面上看到 `mac`
 * 会以为是自己打错了。
 */
export const DOWNLOAD_PLATFORM_OPTIONS = [
  { label: 'Windows x64', os: 'windows', arch: 'amd64' },
  { label: 'Windows ARM64', os: 'windows', arch: 'arm64' },
  { label: 'macOS Intel', os: 'mac', arch: 'amd64' },
  { label: 'macOS Apple Silicon', os: 'mac', arch: 'arm64' },
  { label: 'Linux x64', os: 'linux', arch: 'amd64' },
  { label: 'Linux ARM64', os: 'linux', arch: 'arm64' },
] as const satisfies ReadonlyArray<DownloadPlatform & { label: string }>

/**
 * 下拉框用的稳定字符串值（`windows/arm64`）。
 *
 * 为什么不直接把 `{os, arch}` 对象塞进 `v-model`：`<select>` 的 `value` 只能是
 * 字符串，塞对象时 Vue 会拿 `[object Object]` 去比对选项，结果是「选完回到第一项」，
 * 而症状看起来像下拉框坏了。拼成 `os/arch` 之后，它同时是人可读的诊断信息。
 */
export function platformKey(p: DownloadPlatform): string {
  return `${p.os}/${p.arch}`
}

/**
 * 把下拉框的值解析回闭集内的平台；认不出来返回 `null`（= 「请选择平台」）。
 *
 * 走查 `DOWNLOAD_PLATFORM_OPTIONS` 而不是按 `/` 切开再断言：切开的写法会接受
 * 任何 `x/y`，把非法值一路带到 `?os=` 上、由后端 400 兜底——那时错误信息
 * 只剩一个 `bad_request`，看不出是前端把值放歪了。
 */
export function parsePlatformKey(key: string): DownloadPlatform | null {
  const hit = DOWNLOAD_PLATFORM_OPTIONS.find((o) => platformKey(o) === key)
  return hit ? { os: hit.os, arch: hit.arch } : null
}

/**
 * 从 UA / `navigator.platform` 推断**浏览器这台机器**的平台。
 *
 * Hub 缺省用 `std::env::consts::OS`（Linux Hub 上恒为 `linux`），不把平台传出去
 * 就会让 Windows/macOS 用户拿到一个跑不起来的 Linux 二进制。这里只做推断、
 * 不碰 DOM/网络，所以可以是纯函数、可以逐条 UA 单测。
 *
 * 判据的顺序是 Windows → Mac → Linux：Windows 的 UA 里也可能带 `Linux`
 * （WSL/兼容层字符串），先判 Windows 才不会把它归错。`x86_64`/`amd64`/`WOW64`
 * 归 `amd64`，`aarch64`/`arm64`/`Apple Silicon` 归 `arm64`。
 *
 * **macOS 认不出架构时按 `arm64` 兜底**：`Dockerfile.hub` 里只有
 * `afcli-macos-arm64`（CI 只在 Apple Silicon runner 上编桌面产物），而
 * Apple Silicon 上的 Chrome/Safari 为了兼容老网页，UA 仍写
 * `Intel Mac OS X`、`navigator.platform` 仍是 `MacIntel`——按「看不出就是
 * amd64」会让绝大多数 Mac 用户点下载直接 404。只有明确写了 `x86_64` 等
 * 架构字样的才归 amd64，那时 Hub 如实回报没有该产物。
 */
export function detectDownloadPlatform(userAgent: string, platform = ''): DownloadPlatform {
  const ua = `${userAgent} ${platform}`
  const lower = ua.toLowerCase()
  const isArm = /aarch64|arm64|apple\s*(?:silicon|m[1-9])/.test(lower)
  // 不能把 UA 里的 `Intel` 当成 x86 判据：Apple Silicon 上的 Safari/Chrome
  // 照样写 `Intel Mac OS X`、`navigator.platform` 照样是 `MacIntel`。只认
  // 明确的架构字样；认不出就是 arm64（镜像里唯一的 macOS 产物）。
  const isIntel = /x86_64|x86-64|amd64|wow64|win64/.test(lower)

  if (/win(?:dows|32|64|nt)|\bwindows\b/.test(lower)) {
    return { os: 'windows', arch: 'amd64' }
  }
  if (/macintosh|\bmac\s*os\b|\bmacos\b|\bmac\b|darwin/.test(lower)) {
    return { os: 'mac', arch: isArm || !isIntel ? 'arm64' : 'amd64' }
  }
  if (/linux|x11|ubuntu|fedora|debian|\bbsd\b/.test(lower)) {
    return { os: 'linux', arch: isArm ? 'arm64' : 'amd64' }
  }
  // `navigator.platform` 单独给 `MacIntel`/`Win32`/`Linux x86_64` 时上面已覆盖；
  // 完全认不出来时退回 Linux/amd64（CI 与 happy-dom 的默认值都是它）。
  return { os: 'linux', arch: 'amd64' }
}

/**
 * 下载入口的**默认值**：猜一个，但猜不出来就**不猜**。
 *
 * 与 `detectDownloadPlatform` 的差别只有最后那一条兜底：那个函数的契约是
 * 「永远给一个平台」（单测里要稳定），而这里是 UI 默认值——把
 * 「认不出的浏览器」默认成 `Linux x64` 会让 Windows 用户以为自己选对了，
 * 点下去拿到一个跑不起来的 Linux 文件，而界面上没有任何东西提示他选错了。
 * 所以认不出来时返回 `null`，页面据此禁用下载按钮并提示「请选择平台」——
 * 让人主动选一次，比给一个看似合理的错答案好。
 *
 * **猜错不影响功能**：用户随时能改下拉框。这也是它只是「默认值」、
 * 不写进地址栏也不持久化的理由。
 */
export function guessDownloadPlatform(userAgent: string, platform = ''): DownloadPlatform | null {
  // UA 与 platform 拼起来后仍然认不出任何一类系统（含空串）→ 不猜。
  const probe = `${userAgent} ${platform}`.trim()
  const recognized =
    /win(?:dows|32|64|nt)|\bwindows\b|macintosh|\bmac\s*os\b|\bmacos\b|\bmac\b|darwin|linux|x11|ubuntu|fedora|debian|\bbsd\b/i.test(
      probe,
    )
  return recognized ? detectDownloadPlatform(userAgent, platform) : null
}

/**
 * 移除一台设备。
 *
 * # 它**不删**这台设备上的会话与轨迹
 *
 * 会话是用户的数据，设备只是一条通路。删完之后那些会话仍在 `/api/sessions`
 * 里，`workspace.node_id` 变成 `null`、`workspace.removed_node_id` 记下原 id——
 * 所以调用方**不需要**在成功之后去清会话列表或本地缓存。
 *
 * # 404 是「本来就不存在」，不是「删失败」
 *
 * 抛出的 `ApiError.code` 是 `not_found`。重复点击、或另一处已经删过，都会走到
 * 这一条；调用方应当把它当成「目标状态已达成」，而不是显示一个错误。
 */
export function deleteDevice(id: string, options?: RequestOptions): Promise<void> {
  return del(`/api/devices/${encodeURIComponent(id)}`, options)
}

/** 设备是否「现在能建会话」。只有 `offline` 不行——`reconnecting` 仍算可用（§7.3）。 */
export function canCreateSession(device: DeviceSummary): boolean {
  return device.status !== 'offline'
}

/* ===========================================================================
   设备详情（v0.7.0 子任务 7）——**从设备侧看项目**
   ---------------------------------------------------------------------------
   `PRODUCT.md` §3 的主轴是「做事从项目进，管理从设备进」。项目主轴改版之后
   设备页恰恰要**变多**：运维时的第一反应永远是「这台机器怎么了」，而那个
   问题项目页回答不了。

   三块新数据（绑定关系 / 磁盘 / 失败归因）全部是既有表的读，没有新表。
   =========================================================================== */

/** 一条绑定在设备视角下的样子（带项目名，省掉一次项目列表请求）。 */
export interface DeviceBinding {
  id: string
  project_id: string
  project_name: string
  /** 设备上的路径原文（不展开 `~`：Hub 没有那台设备的文件系统）。 */
  fixed_dir: string
  /** `null` = 不限额。**不是 0**：0 是一个会把一切都拦掉的真实配额。 */
  disk_quota_bytes: number | null
  cleanup_policy: 'conservative' | 'balanced' | 'aggressive'
  /**
   * 这个项目在这台设备上占了多少字节。
   *
   * **`null` 不是 0**：真实用量只有设备自己量得出来（它才有那个盘）。
   * `null` 说的是「设备还没上报」，`0` 说的是「一点不占」——两句话。
   */
  used_bytes: number | null
}

/**
 * 磁盘那一块的汇总。**没有绑定时整个 `disk` 是 `null`**。
 *
 * 回 `{used: 0}` 的话页面会渲染成「已用 0 B」，而用户真正需要知道的是
 * 「这台机器还没被任何项目用起来」。
 */
export interface DeviceDisk {
  quota_bytes: number | null
  used_bytes: number | null
  /** 0–100。`null` = 算不出来（没配额，或设备还没报用量）。**不是 0%**。 */
  percent: number | null
  /** 还能不能派活。配额为 0 时是 `false`（一个字节都放不下）。 */
  accepts_new_work: boolean
}

/**
 * 执行结果分布。
 *
 * **`aborted` 不是失败**：「我们主动停的」与「它自己挂的」在归因里是两个
 * 结论，混成一个会让一台从没出过问题的机器看起来有失败。
 */
export interface FailureBreakdown {
  succeeded: number
  failed: number
  aborted: number
  /** 还在跑或还没开工。**不进成功率的分母**。 */
  in_flight: number
}

/**
 * 分母：只算**已经结束**的执行。
 *
 * 后端 `FailureBreakdown::settled()` 是同一个口径，`success_rate` 也是按它算的。
 * 前端需要自己再算一遍的唯一理由是**要把这个数显示出来**（「已结束 N 次」），
 * 好让用户看得出上面那个百分比是怎么来的。
 *
 * **写成共享函数而不是每页各写一遍**：设备页与 agent 页都要这个数，各写一份
 * 的话，将来「中止算不算结束」这种口径一变（它算，因为用户主动停了也是结束），
 * 就会一页对一页错——而错的那页会显示一个和 `success_rate` 对不上的分母。
 */
export function settledCount(b: FailureBreakdown | null | undefined): number {
  return b ? b.succeeded + b.failed + b.aborted : 0
}

/** 删除前置条件的结论，不只是布尔值。 */
export interface Deletable {
  allowed: boolean
  /** 拒绝的理由（人话）。`allowed === true` 时是 `null`。 */
  reason: string | null
  executions: number
}

/** `GET /api/devices/:id/detail` 的响应。 */
export interface DeviceDetail {
  /** 与列表里那一项**同一个形状**：设备页头部本来就用那套字段。 */
  summary: DeviceSummary
  /** 空数组 = 真的没绑（页面说「还没有绑定项目」）。 */
  bindings: DeviceBinding[]
  disk: DeviceDisk | null
  failure_breakdown: FailureBreakdown
  deletable: Deletable
}

/**
 * 读设备详情。
 *
 * 路径带 `/detail` 而不是复用 `/api/devices/{id}`：后者已经存在吗？没有——
 * 但 `DELETE /api/devices/{id}` 在，同一路径加一个 GET 会让「读详情」与
 * 「删设备」共用一条路径，而这两件事的失败后果差了一个数量级。分开更安全。
 */
export function getDeviceDetail(
  id: string,
  options?: RequestOptions,
): Promise<DeviceDetail> {
  return getJson<DeviceDetail>(`/api/devices/${encodeURIComponent(id)}/detail`, options)
}

/** 清理策略的中文说明。**三档的差别要写出来**，只给名字等于没接这个字段。 */
export function describeCleanupPolicy(policy: DeviceBinding['cleanup_policy']): string {
  switch (policy) {
    case 'conservative':
      return '保守：依赖缓存留 30 天，产物留最近 3 个做诊断'
    case 'balanced':
      return '平衡：依赖缓存留 7 天，产物跑完即清'
    case 'aggressive':
      return '激进：依赖缓存尽快清，产物跑完即清'
    default:
      // 走到这里说明后端加了第四档而前端没跟上。**把原值露出来**，不要
      // 编一句「未知策略」——那个词用户搜不到任何东西；原值至少能让
      // 他在日志与配置里对上。
      //
      // 之所以要有这条 `default`：`tsconfig` 没开 `noImplicitReturns`，
      // 少了它这个函数会在新档上隐式返回 `undefined`，而调用处按 `string`
      // 用，页面就渲染出一块空白说明。
      return `未知策略（${String(policy)}）`
  }
}

/**
 * 从**这一台 Hub** 下载 `afcli`（`GET /api/agent-binary`）。
 *
 * 为什么不让用户去 GitHub Releases 下：内网/离线部署里设备够不到 GitHub；
 * 而 Release 页面上有多个版本，选错一个就连不上（协议可能已变）。从 Hub
 * 自己下发，拿到的**一定**是这个 Hub 会认的那一个。
 *
 * 返回 `Blob` 而不是直接触发下载：触发下载要碰 `document`/`URL`，那是
 * 页面的事；这一层只负责把字节拿回来，才能被单测覆盖（`URL.createObjectURL`
 * 在 jsdom 里要 mock 一大堆）。
 *
 * **Hub 上没有可下发的 afcli 时抛 `ApiError`，`code` 是
 * `agent_binary_unavailable`**：裸机跑 `afhub`（不经镜像）就是这个状态，
 * 属正常部署形态而非故障，所以页面要按「没配」而不是「坏了」来提示。
 *
 * `os` / `arch` 必须显式带上：省略时后端回落到 `std::env::consts::OS`，
 * 而 Hub 跑在 Linux 上——于是 Windows/macOS 用户会拿到一个跑不起来的
 * Linux 文件。名字用后端产物名的那一套（`mac` 而不是 `macos`）。
 *
 * # query 用 `URLSearchParams` 构造，不手拼字符串
 *
 * `os` / `arch` 是用户可选的闭集值（`DOWNLOAD_PLATFORM_OPTIONS`），但**闭集
 * 是页面侧的承诺，不是这一层的保证**——调用方（包括将来的另一个页面、以及
 * 一遍单测）完全可能传进来任意字符串。手拼 `?os=${os}&arch=${arch}` 时，
 * 一个含 `&` / `#` / 空格的值会悄悄把参数拆成别的形状，而症状是「后端回了
 * 400 或选错了架构」，看不出是编码问题。`URLSearchParams` 负责转义，这一层
 * 就永远不需要相信调用方。
 */
export function downloadAgentBinary(
  options?: RequestOptions & { os?: DownloadPlatform['os']; arch?: DownloadPlatform['arch'] },
): Promise<{ blob: Blob; filename: string }> {
  const { os, arch, ...rest } = options ?? {}
  // `undefined` 是「这个参数不存在，让后端按编译期平台回落」——**不能**变成
  // 空串：后端把空串当闭集之外的非法值直接 400（见 `agent_binary.rs:154`
  // 的 `normalize_query_platform`），那会把「没选平台」变成「下载失败」。
  const qs = new URLSearchParams()
  for (const [k, v] of Object.entries(rest.query ?? {})) {
    if (v === undefined) continue
    qs.set(k, v === null ? '' : String(v))
  }
  if (os !== undefined) qs.set('os', os)
  if (arch !== undefined) qs.set('arch', arch)
  const search = qs.toString()
  return request(search ? `/api/agent-binary?${search}` : '/api/agent-binary', {
    ...rest,
    method: 'GET',
    // 已由上面的 `URLSearchParams` 编码进 path，这里不再走 `withQuery`，
    // 否则 `request` 会二次拼一遍 `?`。
    query: undefined,
  }).then(async (r) => {
    // 文件名**从响应头读**，不在前端另写一个。
    //
    // 服务端下发的文件名带着归一化后的平台（`afcli-linux-arm64`），前端写死的 `??
    // 'afcli'` 会把这一信息丢掉——用户同时有 amd64 与 arm64 两台机器时，存下来
    // 的两个文件同名，靠肉眼分不出哪个能跑。两处各写一个名字还必然漂移。
    //
    // 解析失败时回落 `afcli`：拿不到头不是下载失败的理由（RFC 里
    // `Content-Disposition` 是可选头），只是名字退化成旧名字。
    const cd = r.headers.get('content-disposition') ?? ''
    const m = /filename="?([^";]+)"?/.exec(cd)
    return { blob: await r.blob(), filename: m?.[1] ?? 'afcli' }
  })
}
