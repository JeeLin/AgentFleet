/* ===========================================================================
   taskTypes.ts —— 任务三个展示组件共用的类型出口
   ---------------------------------------------------------------------------
   为什么类型单独一个文件、而不是从 `taskApi.ts` 里 import：

   组件**不许**依赖 API 模块。一旦 `TaskCard.vue` 从 `taskApi.ts` 拿类型，
   要么把整个 api 模块拖进组件测试（于是每条用例都得 mock HTTP），要么就得
   在测试里 `vi.mock('@/domains/project/taskApi')` —— 那等于承认「组件其实
   碰到了 HTTP」。组件是纯展示 + 事件（里程碑 §4「组件职责边界」），所以
   它依赖的东西里不该出现 fetch。

   依赖方向是单向的：

       taskTypes.ts  ←  TaskCard / FailurePanel / AcceptancePanel
            ↑
        taskApi.ts（子任务 7 的页面侧取数层，从这里 import 类型）

   ── 字段名与线格式一一对应 ──────────────────────────────────────────
   下面的类型描述的是 **Hub 返回的 JSON 原样**（里程碑子任务 6 的响应体），
   所以字段名是 snake_case：`done_when` / `needs_manual_acceptance` /
   `failure_kind` / `started_at`。**不在这里做驼峰转换**——一层映射就够
   （页面侧要转换就转一次），组件里到处 `??` 兜底两种命名才是最坏的结果。
   =========================================================================== */

/** 任务态的字符串闭集。与 `shared/state.ts` 的 `TaskStatus` 同一组字面值。 */
export type TaskStateWire = 'pending' | 'queued' | 'running' | 'review' | 'done' | 'failed'

/**
 * 上面那个类型的**运行期副本**，供测试核对「与 UI 词表同源」。
 *
 * 为什么要有：`TaskStateWire` 是纯类型，编译后消失，没有任何运行期断言能发现
 * 「Hub 加了一个态、这里与前端的 `TASK_STATUSES` 有一处没跟上」。这份数组
 * 由 `shared/state.test.ts` 直接比对，两处一旦分叉就红。
 *
 * 类型约束 `readonly TaskStateWire[]` 保证它**不会**写出集合外的值——所以它
 * 不会成为第二份「可能更宽松」的真相源。
 */
export const TASK_STATE_WIRE: readonly TaskStateWire[] = [
  'pending',
  'queued',
  'running',
  'review',
  'done',
  'failed',
]

/**
 * 失败的八种形态（`docs/architecture/task-model.md` §3 的表）。
 *
 * 与 `failure_kind === null`（还没失败 / 成功了）区分开：`null` 是「没有失败
 * 原因」，不是「未知原因」。前者不渲染失败行，后者渲染「未知原因」。
 */
export type FailureKind =
  | 'baseline_mismatch'
  | 'env_broken'
  | 'disk_exhausted'
  | 'agent_error'
  | 'task_failed'
  | 'interrupted'
  | 'rejected'
  | 'superseded'

/**
 * 一条执行记录。
 *
 * 所有 `_at` 都是与 `created_at` 同一种时间戳字符串（`unix:<秒>`，
 * `format.ts:parseStamp` 认的那个格式），**不是**毫秒数也不是 ISO。
 * `null` = 那一刻还没发生（例如 `running` 的 `ended_at`）。
 */
export interface Execution {
  id: string
  /** 执行的态。注意它**不是**任务态：`succeeded` / `aborted` 只出现在这一层。 */
  state: string
  base_commit: string | null
  agent_id: string | null
  /** 设备名。`null` = 这条执行还没被派到设备上（或设备已删除）。 */
  device_id?: string | null
  failure_kind: FailureKind | string | null
  reason: string | null
  evidence: string | null
  started_at: string | null
  ended_at: string | null
}

/** `done_when`：判定码（闭集）+ 人话描述，至少一个非空。 */
export interface DoneWhen {
  /**
   * 判定码。闭集是 `all_tests_pass` / `build_passes` / `review_approved` /
   * `command_exit_zero`；但存量数据里可能有闭集外的值，所以这里是 `string`，
   * 由 `criteria_known` 说明它认不认得。
   */
  criteria: string | null
  description: string | null
}

/** 任务级约束（`task-model.md` §7）。`null` = 不限。 */
export interface TaskConstraints {
  allowed_devices: string | null
  allowed_agents: string | null
  required_capabilities: string | null
  /** `null` = 用默认上限（2）。 */
  retry_limit: number | null
}

/** 一个任务，形状照里程碑子任务 6 的响应体（列表与详情共用）。 */
export interface Task {
  id: string
  project_id: string
  title: string
  state: TaskStateWire | string
  done_when: DoneWhen
  /** 派生值（不是列）：`criteria` 空且 `description` 非空 → true。 */
  needs_manual_acceptance: boolean
  /** 派生值：`criteria` 非空但不在闭集里 → false（用于存量数据兜底）。 */
  criteria_known: boolean
  created_by: string
  base_commit: string | null
  created_at: string
  updated_at: string
  constraints: TaskConstraints
  /** 乐观锁。每次「改任务再跑」+1，写请求带回它做 CAS。 */
  version: number
  superseded_by: string | null
  intent_ref: string | null
  /**
   * 该任务的执行记录（详情端点带、列表端点也带）。
   *
   * 组件从它读三件事：`running` 卡的 `started_at`、`failed` 卡与失败面板的
   * `failure_kind` / `evidence`、「第 N 次」的条数。**只读，不改**——
   * 组件不回写任何东西。
   *
   * 声明为可选：列表端点若裁剪了这个数组，组件仍能渲染（只是少了「第 N 次」），
   * 不该因为缺一个字段就整张卡白屏。
   */
  executions?: Execution[]
}
