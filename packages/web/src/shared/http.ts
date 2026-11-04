/* ===========================================================================
   http.ts —— `fetch` 的薄封装，全站唯一的请求出口
   ---------------------------------------------------------------------------
   这个文件承担三件事，且只有它承担：

     ① 错误形态统一 —— 服务端错误一律 `{ "error": "<code>" }`
        （`crates/hub-server` 的 `ApiError`），这里收敛成同一个 `ApiError`
        类，页面只判一个形状。
      ② **401 弹重新登录框 / 403 `hub_not_initialized` 跳 setup** —— 这两个
         都不是某个端点的事，任何一次请求都可能撞上。后者见下方「为什么 403
         `hub_not_initialized` 必须单独认」，前者见「为什么 401 不再整页跳转」。
      ③ **CSRF 双重提交** —— 从 `af_csrf` cookie 读出值回填 `X-CSRF-Token` 头。

    ── 为什么 401 不再整页跳转 ──────────────────────────────────────
    401（session 过期）撞上的那一刻，用户正在某个页面做一件事：扫设备列表、
    在会话里敲终端、新建会话表单填了一半。`location.assign('/login')` 是一次
    **整页跳转**：它把正在做的事、正在填的表、连同地址栏里那个会话 id 一起丢掉，
    用户回来还得自己导航回去——而重新登录本来只需要一个密码。

    所以现在这里只**弹一个密码框**（`shared/login-prompt.ts` 的 `promptLogin`），
    背后的页面原样活着。输对密码 → 关框，原地继续；点「取消」→ 那一下才真的去
    `/login`（`App.vue` 的 `onLoginCancel`，见那里为什么跳转住在外壳）。

    仍然照常抛 `ApiError(401, 'session_expired')`：调用方必须**停下**，
    而不是在没有凭据的状态里继续渲染一屏「读不到的列表」。

    ── `POST /login` 的 401 不是 session 过期 ─────────────────────────
    `middleware_auth.rs:53/64` 只对**非豁免路径**发 401；`POST /login` 自己在
    `AUTH_EXEMPT_PATHS` 里，它的 401 是 `invalid_credentials`——**密码错**。
    把那个 401 也当成「session 过期」会变成死循环（弹框 → 密码错 401 → 再弹框），
    所以登录请求走 `shared/auth.ts` 的 `postLogin`——它**直接 `fetch`**，
    不经过本模块（本模块的 401 分支正是那个会弹框的分支）。

   ── 为什么 403 `hub_not_initialized` 必须单独认 ─────────────────────
   全新部署第一次打开控制台时，Hub 还停在初始化模式：
   `middleware_auth.rs:33-36` 的规则 1 把**除 `/healthz` `/setup` `/ws`
   之外的一切**都挡成 **403 `hub_not_initialized`**（不是 401——那时 Hub
   根本还不存在「用户」，401 会误导成「你没登录」）。

   前端只认 401 的年代，这一条会落到页面的通用错误分支里，显示成
   「读设备列表失败（hub_not_initialized）」之类——用户看到一句权限错误，
   而正确的下一步是**去 setup 设密码**。于是没有任何路径能走到 `/setup`。
   这就是「全新部署打不开控制台」那条 bug 的根因，正面修复在这里：
   认这个码、跳 `SETUP_PATH`、仍然照常抛 `ApiError` 让页面停下。

   判据是 **(403 且 code === 'hub_not_initialized')**，不是「403 就跳」：
   403 还可能是 `setup_token_mismatch`（setup 页自己要就地显示的）、
   `csrf_token_missing_or_mismatch`（不该把人从页面上赶走）。
   把它们一并跳走，会把「表单填错了」变成「跳到一个同样填不对的页面」。

   ── 为什么 CSRF 在这一层，而不是某个端点里 ──────────────────────────
   `crates/hub-server/src/middleware_auth.rs:24` 的
   `STATE_CHANGING_METHODS = &["POST", "PUT", "PATCH", "DELETE"]`，
   `:66-69` 规则 3「状态变更要 CSRF 双重提交——`X-CSRF-Token` 头必须与
   `af_csrf` cookie 同值」。

   这是一个**横切关注点**：子任务 5a 的四个上行端点（开通道 / 送字节 /
   关通道 / resize）、子任务 5 的 `POST /api/sessions`、子任务 12 的新建
   会话表单，全是状态变更方法。漏掉这一步的后果是**终端每一次键盘输入都拿
   403 `csrf_token_missing_or_mismatch`，而测试全绿**——因为组件测试打的是
   mock，没有任何一条会经过这个封装去验证它真的带了头。

   `af_csrf` 按设计**不是 HttpOnly**：`crates/hub-server/src/routes/setup.rs:278`
   「CSRF cookie 值。非 HttpOnly——前端要能读出来回填到 `X-CSRF-Token`」，
   `crates/hub-server/tests/login_flow.rs:543` 锁住这条（若哪天它变成
   HttpOnly，那个测试会红）。所以前端技术上做得到，但必须有人做——
   就是这里。

   ── 与 cookie 库的分界 ─────────────────────────────────────────────
   不引 cookie 解析库：`af_csrf` 的值是 base64url 或十六进制
   （`setup.rs` 的 `random_token`），不含需要百分号解码的字符，
   而读取在这里只需要 `name=value` 这一种形状——与
   `middleware_auth.rs:96` 手工解析 cookie 是同一条理由。
   =========================================================================== */

import { promptLogin } from './login-prompt'

/** CSRF cookie 名。必须与 `crates/hub-server/src/lib.rs:37` 的 `CSRF_COOKIE` 一致。 */
export const CSRF_COOKIE = 'af_csrf'

/** CSRF 头名。必须与 `crates/hub-server/src/lib.rs:39` 的 `CSRF_HEADER` 一致。 */
export const CSRF_HEADER = 'X-CSRF-Token'

/** 会触发 CSRF 双重提交的 HTTP 方法。必须与 `STATE_CHANGING_METHODS` 一致。 */
const STATE_CHANGING_METHODS: ReadonlySet<string> = new Set(['POST', 'PUT', 'PATCH', 'DELETE'])

/**
 * 401 时**弹的框**用的那个登录页路径。
 *
 * 401 现在不跳它（见文件头「为什么 401 不再整页跳转」），只有用户在弹框里
 * 点「取消」时才真的去这里——那一下的跳转在 `App.vue`，这里只是把同一个
 * 字符串留在出事的那一层，免得「401 想去哪儿」这件事要在两个文件里读。
 *
 * 与 `SETUP_PATH` 并列而不是二选一：401 与 403 `hub_not_initialized` 说的是
 * 两件完全不同的事（「你有身份但没登录」/「Hub 还没有用户」），混为一谈就是
 * 死循环——未初始化时 `/login` 的 POST 也是 403。
 */
const LOGIN_PATH = '/login'

/**
 * 「Hub 还没设密码」时跳去的页面。
 *
 * 与 `LOGIN_PATH` 并列而不是二选一：401 与 403 `hub_not_initialized`
 * 说的是两件完全不同的事（「你有身份但没登录」/「Hub 还没有用户」），
 * 跳错一个就是死循环——未初始化时 `/login` 的 POST 也是 403。
 */
const SETUP_PATH = '/setup'

/**
 * 中间件在初始化模式下给的错误码（`middleware_auth.rs:36`）。
 *
 * 写成常量而不是就地写字面量：它是**协议的一部分**（后端测试
 * `login_flow.rs:255` 锁着它），散在各处拼字符串就会漂。
 */
export const HUB_NOT_INITIALIZED = 'hub_not_initialized'

/** 服务端错误响应体的形状：`crates/hub-server` 的 `ApiError` 序列化结果。 */
export interface ApiErrorBody {
  error: string
}

/** 统一错误形态。页面 catch 到它，读 `code` 做分支，不读 `message`。 */
export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string) {
    super(`${status} ${code}`)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

/** 网络层失败（离线、DNS、CORS、`fetch` 抛异常）——不是服务端的错。 */
export class NetworkError extends Error {
  constructor(cause: unknown) {
    super('network request failed')
    this.name = 'NetworkError'
    this.cause = cause
  }
}

/**
 * 读一个 cookie 的值。找不到（或值为空）返回 `null`。
 *
 * 手工解析而不是引 cookie 库：本模块只需要 `name=value` 这一种形状，
 * 而 `af_csrf` / `af_session` 的值都是 base64url 或十六进制，
 * 不含需要百分号解码的字符（与 `middleware_auth.rs:96` 同理由）。
 */
export function cookieValue(name: string, cookieHeader: string | null | undefined): string | null {
  if (!cookieHeader) return null
  for (const pair of cookieHeader.split(';')) {
    const idx = pair.indexOf('=')
    if (idx < 0) continue
    if (pair.slice(0, idx).trim() === name) {
      const value = pair.slice(idx + 1).trim()
      return value.length > 0 ? value : null
    }
  }
  return null
}

/** 浏览器环境下读 `af_csrf`。非浏览器（SSR / 单元测试）返回 `null`。 */
export function readCsrfToken(): string | null {
  if (typeof document === 'undefined') return null
  return cookieValue(CSRF_COOKIE, document.cookie)
}

/**
 * 401 的处理：**弹重新登录框**（不是跳转，见文件头「为什么 401 不再整页跳转」）。
 *
 * 它**不读 `path`**：弹框不需要知道来的是哪个端点（401 是横切事实），
 * 而跳转是用户在框里点取消之后的事，那一刀在 `App.vue`（见那里为什么
 * 它必须住在外壳）。`LOGIN_PATH` 仍然由调用方传下来——`setUnauthorizedHandler`
 * 的签名有测试锁着，且「401 想去哪儿」这件事该留在出事的那一层。
 *
 * 所以这里声明成零参箭头函数（而不是 `(_path: string) =>`）：TS 允许用
 * 参数更少的函数去满足参数更多的类型，而反过来不行。写一个 `_path` 再
 * 不读它只会撞 `no-unused-vars`。
 */
let onUnauthorized: (path: string) => void = () => {
  promptLogin()
}

/**
 * 「Hub 未初始化」的处理（403 `hub_not_initialized`）。
 *
 * 与 `onUnauthorized` **分成两个槽位**而不是共用一个回调：两条路径的去处
 * 不同（`/login` vs `/setup`），共用一个槽位就意味着「跳哪儿」这件事被
 * 塞进了 path 参数里——下一个读代码的人会以为两者可以互换。
 *
 * **默认值只是兜底**：外壳（`App.vue`）用 `setNotInitializedHandler` 注册
 * 自己的 `fullPageNavigate`，于是「整页跳转住在外壳」这件事成立；这一行只保证
 * `http.ts` 单独被用时（单测、将来的别的宿主）不会静默地什么都不做。
 * 因此**它不是**「第二个会整页跳转的地方」——没有外壳时它替代外壳，
 * 有外壳时它一次也不执行（`App.vue` 在 setup 体里就注册掉了）。
 */
let onNotInitialized: (path: string) => void = (path) => {
  if (typeof location !== 'undefined') location.assign(path)
}

/**
 * 覆盖 401 的处理（测试用）。
 *
 * 返回一个**还原函数**而不是旧实现：后者逼着每个测试在 `afterEach` 里写一遍
 * 类型标注，还容易漏——漏一个就意味着后面所有 401 用例都被上一个用例污染。
 */
export function setUnauthorizedHandler(fn: (path: string) => void): () => void {
  const prev = onUnauthorized
  onUnauthorized = fn
  return () => {
    onUnauthorized = prev
  }
}

/** 覆盖「Hub 未初始化」的处理（测试用）。返回还原函数，理由同上。 */
export function setNotInitializedHandler(fn: (path: string) => void): () => void {
  const prev = onNotInitialized
  onNotInitialized = fn
  return () => {
    onNotInitialized = prev
  }
}

/** 为一次状态变更请求补上 CSRF 头。返回新建的 headers，不改入参。 */
export function withCsrf(headers: HeadersInit | undefined, method: string): Headers {
  const out = new Headers(headers)
  if (STATE_CHANGING_METHODS.has(method.toUpperCase())) {
    const token = readCsrfToken()
    if (token) out.set(CSRF_HEADER, token)
    // 读不到 token 时**不**编一个空值：空值与服务端 `csrf_ok` 的
    // 「非空且同值」判据必然不符，会得到一句准确描述病因的 403，
    // 而不是一个「头补了却依然不通」的谜。
  }
  return out
}

/** 从响应体里取错误码。取不到（非 JSON / 空体）退回 `http_<status>`。 */
async function errorCode(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as Partial<ApiErrorBody>
    if (typeof body.error === 'string' && body.error.length > 0) return body.error
  } catch {
    // 非 JSON 错误体（代理错误页、空 502）——走下面的兜底
  }
  return `http_${res.status}`
}

export interface RequestOptions extends Omit<RequestInit, 'body'> {
  /** 缺省 `GET`。CSRF 头按**实际发出的方法**决定，不是按调用方有没有写 method。 */
  query?: Record<string, string | number | boolean | undefined | null>
  /** 已序列化的请求体（`JSON.stringify` 的结果）或原始字节。 */
  body?: BodyInit | null
}

/** 把 query 拼到 url 上。空参数不留 `?`。 */
export function withQuery(url: string, query: RequestOptions['query']): string {
  if (!query) return url
  const qs = new URLSearchParams()
  for (const [k, v] of Object.entries(query)) {
    // `undefined` 是「这个参数不存在」，`null` 是「有这个参数但值为空」
    // （例如清空一个筛选条件）。两者语义不同，所以前者跳过、后者留空串。
    if (v === undefined) continue
    qs.set(k, v === null ? '' : String(v))
  }
  const s = qs.toString()
  return s ? `${url}?${s}` : url
}

/** `fetch` 薄封装。非 2xx 一律抛 `ApiError`，网络失败抛 `NetworkError`。 */
export async function request(path: string, options: RequestOptions = {}): Promise<Response> {
  const { query, body, ...rest } = options
  const method = (rest.method ?? 'GET').toUpperCase()
  const headers = withCsrf(rest.headers, method)
  const url = withQuery(path, query)

  let res: Response
  try {
    res = await fetch(url, { ...rest, method, headers, body, credentials: 'same-origin' })
  } catch (cause) {
    throw new NetworkError(cause)
  }

  if (res.status === 401) {
    // session 失效是横切事实：任何端点都可能撞上。**不跳转**——弹一个密码框，
    // 背后那页还活着（理由见文件头「为什么 401 不再整页跳转」）。
    onUnauthorized(LOGIN_PATH)
    // 仍然抛：调用方必须停下，而不是在没有凭据的状态里继续渲染一屏
    // 「读不到的列表」（与下面 403 那条同一个理由）。
    throw new ApiError(401, 'session_expired')
  }
  if (!res.ok) {
    const code = await errorCode(res)
    if (res.status === 403 && code === HUB_NOT_INITIALIZED) {
      // Hub 还没有用户（初始化模式）：这不是权限错误，是「该去设密码」。
      // 跳到 /setup 之后仍然抛出去——调用方必须停下，而不是在没有凭据的
      // 状态里继续渲染一屏「读不到的列表」。
      onNotInitialized(SETUP_PATH)
      throw new ApiError(403, code)
    }
    throw new ApiError(res.status, code)
  }
  return res
}

/** GET + 解 JSON。 */
export function getJson<T>(path: string, options?: RequestOptions): Promise<T> {
  return request(path, { ...options, method: 'GET' }).then((r) => r.json() as Promise<T>)
}

/** POST JSON + 解 JSON。 */
export function postJson<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
  return sendJson<T>('POST', path, body, options)
}

/**
 * PATCH JSON + 解 JSON。
 *
 * 与 `postJson` 共用 `sendJson`：两者的差别只有 method 一个词，各写一份
 * 必然漂移（曾经 `postJson` 单独处理 `Content-Type` 与 `string` body）。
 */
export function patchJson<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
  return sendJson<T>('PATCH', path, body, options)
}

/**
 * PUT JSON + 解 JSON。
 *
 * 全仓只有绑定用它（`PUT /api/projects/{id}/bindings/{device_id}`），而那条是
 * **唯一的 upsert**：同一路径既能建也能改，所以用 PUT 而不是 POST/PATCH 二分。
 */
export function putJson<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
  return sendJson<T>('PUT', path, body, options)
}

function sendJson<T>(
  method: string,
  path: string,
  body: unknown,
  options?: RequestOptions,
): Promise<T> {
  const payload: BodyInit | null | undefined =
    body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body)
  const headers = new Headers(options?.headers)
  if (typeof payload === 'string' && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json')
  }
  return request(path, { ...options, method, headers, body: payload }).then(
    (r) => r.json() as Promise<T>,
  )
}

/** DELETE，不解析响应体（关通道等场景服务端可能回 204）。 */
export function del(path: string, options?: RequestOptions): Promise<void> {
  return request(path, { ...options, method: 'DELETE' }).then(() => undefined)
}

/**
 * 读路径的失败 → 一句用户能看懂的话。
 *
 * 三个页面此前各抄了一份（设备列表 / 会话列表 / Agent 目录），而同一份
 * `session_expired` 在其中一处给出了「刷新后重新登录」、另一处给出了
 * 「请重新登录」——两种下一步让用户不知道该做哪个。收成一份之后，
 * `noun`（被读的东西）是唯一的变量。
 *
 * `session_expired` 的下一步**只有一种**：重新登录。`http.ts` 已经弹了
 * 重新登录框，而「刷新页面」会把用户正在做的事（填了一半的表单、终端里
 * 的输出）一起丢掉——那不是「下一步」，是「重来」。
 */
export function describeReadFailure(err: unknown, noun: string): string {
  if (err instanceof ApiError) {
    if (err.code === 'session_expired') return '登录已失效，请重新登录。'
    return `读${noun}失败（${err.code}）。`
  }
  if (err instanceof NetworkError) return 'Hub 连不上。检查 Hub 进程是否还在。'
  return `读${noun}失败。`
}