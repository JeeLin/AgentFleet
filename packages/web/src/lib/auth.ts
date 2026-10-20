/* ===========================================================================
   auth.ts —— 认证流程的三个端点与它们的错误码（§2.4 / §5.2 的「认证流程」）
   ---------------------------------------------------------------------------
   这个文件只做两件事：① 把 setup（`/setup` 端点 + `/api/setup` 探针）与
   `/login` 的请求/响应写成类型；② 把服务端的**每一个错误码**翻译成
   「下一步该做什么」。

   ── 为什么这层翻译必须存在，而且不许合并 ────────────────────────────
   规范 §4.7 把登录的五种情形、setup 的三种失败**定死为分开呈现**：

     密码错        → 就地重试
     Hub 未初始化   → 去 setup（换的是一个动作，不是重试）
     已初始化      → 直接去登录（setup 只能做一次）
     token 不匹配   → 重新取 token
     token 被锁     → 重启 Hub（取 token 已经没用了）
     rate_limited  → 等
     login_busy    → 等（Hub 正忙，不是你的问题）

   压成一句「登录失败」等于把「下一步该做什么」还给用户瞎猜。所以这个文件
   里没有任何 `default: '失败'` 的合并分支：未知 code 会把 **code 本身**
   带出来（前端落后于 Hub 时，露出那个 code 比编一句好话有用，与
   `api.ts` 的 `createSessionErrorLabel` 同一条理由）。

   ── 错误码的唯一来源 ──────────────────────────────────────────────
   全部摘自 `crates/hub-server/src/routes/setup.rs` 与
   `crates/hub-server/src/middleware_auth.rs`，并被
   `crates/hub-server/tests/login_flow.rs` 逐条锁住：

     GET  /api/setup  200 `{"initialized": false}` / 409 `already_initialized`
     POST /setup  403 `setup_token_required` / `setup_token_mismatch` /
                      `setup_token_locked`，400 `password_too_short`，
                  409 `already_initialized`
     POST /login  200 `{"ok": true}` + `af_session` / `af_csrf` 两条 cookie，
                  401 `invalid_credentials`，429 `rate_limited` / `login_busy`，
                  403 `hub_not_initialized`（未初始化时 `require_auth` 先挡）

   ── setup token 从哪来：**页面上写清楚，不假装能自动获取** ────────────
   token 只有两条出口：Hub 启动时打到 **stderr** 的那一行，以及
   `<data>/setup-token.txt`（0600，`setup.rs:294` 的 `write_setup_token_file`）。
   没有「点一下自动拿」的接口，也不该有——那等于把 token 本身变成了
   任何人可请求的资源，setup token 的全部意义就没了。
   =========================================================================== */

import { ApiError, getJson, postJson } from './http'

/**
 * `GET /api/setup` 的响应体。只在**未初始化**时拿到（已初始化是 409）。
 *
 * 路径不是 `/setup`：那一条是**页面**（SPA 外壳），硬加载时必须回 HTML。
 * 一条路径不能既回页面又回 JSON，所以探针另起 `/api/setup`
 * （`crates/hub-server/src/lib.rs` 里写了为什么这么拆）。
 */
export interface SetupState {
  initialized: boolean
}

/** `POST /setup` 的响应体。`setup.rs:136`：`{"initialized": true}`。 */
export interface SetupResult {
  initialized: boolean
}

/** `POST /login` 的响应体。`setup.rs:263`：`{"ok": true}`。 */
export interface LoginResult {
  ok: boolean
}

/** `POST /setup` 的请求体。`username` 后端收下但忽略（单用户）。 */
export interface SetupInput {
  password: string
  username?: string
}

/** `POST /login` 的请求体。 */
export interface LoginInput {
  password: string
  username?: string
}

/**
 * `X-Setup-Token` 的头名。
 *
 * 必须与 `crates/hub-server/src/lib.rs:41` 的
 * `SETUP_TOKEN_HEADER = "x-setup-token"` 一致；这个接口在
 * `AUTH_EXEMPT_PATHS` 里，所以它**不是**状态变更意义上的 CSRF 目标
 * （`middleware_auth.rs` 规则 3 只对带 session 的请求生效），但它自己
 * 有一个更强的凭据：setup token 本身。
 */
export const SETUP_TOKEN_HEADER = 'X-Setup-Token'

/** 密码最短长度。必须与 `crates/hub-core/src/auth.rs:194` 的 `< 8` 一致。 */
export const PASSWORD_MIN_LENGTH = 8

/** 问 Hub 设过密码没有。已初始化时后端回 409 `already_initialized`。 */
export function fetchSetupState(): Promise<SetupState> {
  return getJson<SetupState>('/api/setup')
}

/**
 * 设密码。
 *
 * `X-Setup-Token` 由调用方从页面上粘进来，**这个模块不自己去找 token**：
 * 前端没有、也不该有拿 token 的通路（见文件头）。
 */
export function submitSetup(token: string, input: SetupInput): Promise<SetupResult> {
  return postJson<SetupResult>('/setup', input, {
    headers: { [SETUP_TOKEN_HEADER]: token },
  })
}

/** 换 session + CSRF cookie。成功由浏览器落到 `af_session` / `af_csrf` 上。 */
export function submitLogin(input: LoginInput): Promise<LoginResult> {
  return postJson<LoginResult>('/login', input)
}

/** 一条可呈现的错误：一句话主文 + 可选的「下一步」。 */
export interface AuthFailure {
  /** 后端错误码原样保留（未知码也在，别删）。 */
  code: string
  /** 这条错了，下一步该做什么。**不写「请重试」**——那等于没说。 */
  next: string
  /**
   * 表单还能不能继续提交。
   *
   * `false` 的那几种（未初始化 / 已初始化 / token 被锁 / 401 跳登录）
   * 都是「再按一次也不会变」的情形：把按钮留在那儿只会换来一轮瞎试。
   */
  retryable: boolean
  /** 这一条要不要直接把人送走（同时附上目标路径）。 */
  goTo?: '/login' | '/setup'
}

/**
 * 登录失败 → 可呈现的失败。
 *
 * 五种情形分开（§4.7）：密码错 / Hub 未初始化 / 会话失效 / 429 限流 /
 * Hub 正忙。前两个与后三个的**下一步完全不同**，一条都不许合并。
 */
export function describeLoginFailure(err: unknown): AuthFailure {
  if (!(err instanceof ApiError)) {
    return {
      code: 'network',
      next: 'Hub 连不上。确认 afhub 进程还在，再试一次。',
      retryable: true,
    }
  }
  switch (err.code) {
    case 'invalid_credentials':
      return {
        code: err.code,
        next: '密码不对。这是唯一凭据，没有邮箱可以找回；忘了就要能登上运行 Hub 的那台机器去重置。',
        retryable: true,
      }
    case 'hub_not_initialized':
      return {
        code: err.code,
        next: 'Hub 还没设过密码。先去完成首次 setup，再回来登录。',
        retryable: false,
        goTo: '/setup',
      }
    case 'session_expired':
      // `http.ts` 在 401 上统一抛这个：它已经跳了一次 /login，这里只解释原因。
      return {
        code: err.code,
        next: '登录态已失效（cookie 过期或签名不匹配）。重新登录即可。',
        retryable: true,
      }
    case 'rate_limited':
      return {
        code: err.code,
        next: '尝试太频繁，这个 IP 被临时锁住了（连续失败 10 次 → 锁 300 秒）。等冷却结束再试。',
        retryable: true,
      }
    case 'login_busy':
      return {
        code: err.code,
        next: 'Hub 正忙（登录并发闸门占满）。这不是密码的问题，过几秒再试。',
        retryable: true,
      }
    case 'invalid_request_body':
      return {
        code: err.code,
        next: '请求体没被 Hub 读懂（形状不对或超长）。刷新页面后重填。',
        retryable: true,
      }
    case 'internal_error':
      return {
        code: err.code,
        next: 'Hub 内部出错（详情只在 Hub 日志里）。看一眼 afhub 的日志再试。',
        retryable: true,
      }
    default:
      return {
        code: err.code,
        next: `登录没有成功，Hub 回的是 ${err.status} ${err.code}。`,
        retryable: true,
      }
  }
}

/**
 * setup 失败 → 可呈现的失败。
 *
 * 三种（加两种参数错）各自给出**下一步命令**，绝不合并成「setup 失败」：
 * `setup_token_mismatch` 与 `setup_token_locked` 尤其不是一回事——
 * 前者再取一次 token 就行，后者已经是「这个 token 死了」，得重启 Hub
 * 才会重新生成（启动时发现仍未初始化就生成新 token，失败计数随之清零）。
 */
export function describeSetupFailure(err: unknown): AuthFailure {
  if (!(err instanceof ApiError)) {
    return {
      code: 'network',
      next: 'Hub 连不上。确认 afhub 进程还在，再试一次。',
      retryable: true,
    }
  }
  switch (err.code) {
    case 'setup_token_required':
      return {
        code: err.code,
        next: '没带 setup token。把 Hub 启动时打出的那一行 token（或 <data>/setup-token.txt 的内容）粘进来。',
        retryable: true,
      }
    case 'setup_token_mismatch':
      return {
        code: err.code,
        next: '这个 token 对不上。重新取一次：看 Hub 启动时 stderr 的那一行，或读 <data>/setup-token.txt（只读得到，0600）。',
        retryable: true,
      }
    case 'setup_token_locked':
      return {
        code: err.code,
        // 这一条不是「再试一次」：token 已经作废，失败次数用尽（10 次）。
        next: 'setup token 已被锁死（失败次数用尽）。取新 token 的唯一办法是重启 Hub——重启后它会重新生成，并写回 stderr 与 <data>/setup-token.txt。',
        retryable: false,
      }
    case 'already_initialized':
      return {
        code: err.code,
        next: 'Hub 已经设过密码了，setup 只能做一次。直接去登录。',
        retryable: false,
        goTo: '/login',
      }
    case 'password_too_short':
      return {
        code: err.code,
        next: `密码至少 ${PASSWORD_MIN_LENGTH} 位——这是唯一凭据，没有邮箱可以找回。`,
        retryable: true,
      }
    case 'hub_not_initialized':
      // 不会出现在 POST /setup 上（它自己在初始化模式里），但 GET 侧的
      // 中间件故障可能以这个码回来；保留一条准确的说法而不是编一个。
      return {
        code: err.code,
        next: 'Hub 的初始化状态读不出来。看 afhub 日志——这是 Hub 侧的问题，不是表单填错了。',
        retryable: true,
      }
    case 'internal_error':
      return {
        code: err.code,
        next: 'Hub 内部出错（详情只在 Hub 日志里）。密码**没有**被写进去，Hub 仍停在初始化模式。',
        retryable: true,
      }
    default:
      return {
        code: err.code,
        next: `setup 没有成功，Hub 回的是 ${err.status} ${err.code}。`,
        retryable: true,
      }
  }
}
