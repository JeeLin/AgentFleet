/* ===========================================================================
   domains/project/taskApi.ts —— 任务的读取 / 写入层（v0.8.0 子任务 7）
   ---------------------------------------------------------------------------
   与 `domains/project/api.ts`（项目与绑定）、`domains/device/api.ts` 同一套口径：
   这一层只做两件事——把响应体的**形状**写成类型，给出端点的函数。页面不自己
   拼 URL、不自己解响应、不自己判错误码；错误一律是 `shared/http.ts` 的
   `ApiError`，页面只读 `code` 做分支。传输层（cookie / CSRF / 401 / 403）全在
   `shared/http.ts`。

   ── 为什么不叫 `api.ts` ────────────────────────────────────────────
   同目录已有 `api.ts`（项目 + 绑定）。再放一个 `api.ts` 会让
   `import … from './api'` 在同一条路径上指两个模块，靠「谁先被解析」决定
   语义——这种歧义不会报错，只会在某次重构后静默换掉数据来源。

   ── 类型从 `taskTypes.ts` re-export，不在本文件重复定义 ──────────────
   组件（`TaskCard` / `FailurePanel` / `AcceptancePanel`）依赖的是
   `taskTypes.ts`，本文件与它们共用同一份 `Task` / `Execution`。两份定义迟早
   会漂：一边加了字段、另一边还是旧的，而 TS 只在**恰好在两边都用**的地方报错，
   页面里「拿 Task 当 Task 用」是看不出来的。

   ── 端点的响应体都包了一层 `{ task: … }` / `{ tasks: … }` ────────────
   这是子任务 6 的实际形状（不是裸对象）。所以本层的函数返回**解开后的**值，
   让页面侧只有一种形状要看。包一层的好处（后端视角）：将来加 `warnings`
   之类的兄弟字段不必破坏既有调用方。

   ── PATCH 的约束四列是「双层 Option」 ────────────────────────────────
   `PatchTaskBody` 里四个约束字段是 `Option<Option<T>>`（`routes/tasks.rs:216-232`）：

     键缺失      → 不改
     显式 null   → 清成「不限」（`NULL`）
     给值        → 改成它

   所以本文件把它们声明成 `string | null`（`?` 只能表达「缺」，表达不了
   「显式清空」）。写成 `string | undefined` 会让「清空」这个操作在类型层就
   做不到，而它恰恰是用户真会做的事（与 `patchProject` 的 `theme_color`
   是同一条理由）。
   =========================================================================== */

import { getJson, patchJson, postJson, type RequestOptions } from '@/shared/http'
import type { DoneWhen, Execution, Task, TaskStateWire } from './taskTypes'

/* ===========================================================================
   类型：从 `taskTypes.ts` 原样再导出
   =========================================================================== */

export type {
  DoneWhen,
  Execution,
  FailureKind,
  Task,
  TaskConstraints,
  TaskStateWire,
} from './taskTypes'

/* ===========================================================================
   判定码（下拉用）
   =========================================================================== */

/**
 * 判定码闭集（`hub_storage::task::CRITERIA_CODES`，后端 400 的
 * `detail.allowed` 里回的就是这四个）。
 *
 * 写在前端是为了**在下拉里列出来**，不是为了做校验：真伪由后端
 * `DoneWhen::validate` 判（闭集外的值回 400 `invalid_criteria`）。所以
 * 这四条用后端常量拼出来会更好，但前端拿不到 Rust 常量——于是这里抄一份
 * **只用于展示**，并在 `taskApi.test.ts` 里钉住「四条一个不少」。
 */
export const CRITERIA_CODES = [
  { code: 'all_tests_pass', label: '全部测试通过' },
  { code: 'build_passes', label: '构建通过' },
  { code: 'review_approved', label: '有人工验收通过' },
  { code: 'command_exit_zero', label: '命令退出码为 0' },
] as const

/**
 * 建任务的请求体（`CreateTaskBody`，`routes/tasks.rs:184-192`）。
 *
 * `done_when` **必填**：后端缺这个键会直接 400（连「怎么算完成」都没写的
 * 任务建出来只会空转）。四条约数**全可选**，没给的列写 `NULL`（= 不限）——
 * 建任务时「清空」与「不填」是同一件事，所以这里没有双层 Option。
 */
export interface CreateTaskBody {
  title: string
  done_when: DoneWhen
  constraints?: {
    allowed_devices?: string
    allowed_agents?: string
    required_capabilities?: string
    retry_limit?: number
  }
}

/**
 * 改任务的请求体（`PatchTaskBody`，`routes/tasks.rs:216-232`）。
 *
 * 约束四列是 `T | null` 而不是 `T | undefined` 的双层语义，见文件头。
 * `expected_version` 是可选的 CAS 前置条件：给出且与服务端不符 → 409
 * `task_state_changed`（`detail.expected_version` / `detail.actual_version`），
 * **不碰库**。看板场景不带它（没有用户输入要保留，冲突就重读）。
 */
export interface PatchTaskBody {
  title?: string
  done_when?: DoneWhen
  allowed_devices?: string | null
  allowed_agents?: string | null
  required_capabilities?: string | null
  retry_limit?: number | null
  expected_version?: number
}

/**
 * 验收请求体（`AcceptanceBody`，`routes/tasks.rs`）。
 *
 * `override` 用于「判定码不全绿仍要合入」：那时后端**强制**要 `note`
 * （缺了回 400 `acceptance_note_required`）——这条规则不在这里复刻，
 * 页面侧只负责把 note 传上去。
 */
export interface AcceptanceBody {
  verdict: 'pass' | 'reject'
  note?: string
  override?: boolean
  expected_version?: number
}

/**
 * 失败处置请求体（`FailureBody`，`routes/tasks.rs`）。
 *
 * 三个动作对应 `task-model.md` 的「失败不自动回待办，必须有人处置」：
 * `retry`（换条件重派）→ `queued`；`edit`（改判定条件）→ `pending`；
 * `close`（主动放弃）→ **留在 `failed`**，只是记下原因。所以看板上
 * 「主动放弃」的任务仍显示在失败列——那不是遗漏，是语义。
 */
export interface FailureBody {
  action: 'retry' | 'edit' | 'close'
  failure_kind?: string
  reason?: string
  /** `action: 'edit'` 时同时写进任务的新验收标准。 */
  done_when?: DoneWhen
  expected_version?: number
}

/* ===========================================================================
   端点
   =========================================================================== */

/** `GET /api/projects/{id}/tasks` 的响应体：`{ tasks: [...] }`。 */
interface TaskListResponse {
  tasks: Task[]
}

/** 只回一个任务的几个端点的公共响应体：`{ task: … }`。 */
interface TaskResponse {
  task: Task
}

/** `GET /api/tasks/{id}` 的响应体：任务 + 它的全部执行记录。 */
interface TaskDetailResponse {
  task: Task
  executions: Execution[]
}

/**
 * 读一个项目的全部任务。
 *
 * **一次拿全量**，由页面按 `state` 分组：6 列各打一次就是 6 倍的无谓请求，
 * 而看板量级（几十张卡）下前端分组没有代价。
 *
 * `state` 只给「深链筛选」用（例如从收件箱跳过来），非法值后端回 400
 * `invalid_task_state`——**注意 `draft` 是非法值**（迁移后旧态已不存在）。
 */
export function listProjectTasks(
  projectId: string,
  state?: TaskStateWire,
  options?: RequestOptions,
): Promise<Task[]> {
  return getJson<TaskListResponse>(`/api/projects/${encodeURIComponent(projectId)}/tasks`, {
    ...options,
    // `undefined` 是「不带这个参数」，空串会被后端当成非法值——
    // `withQuery` 已经按这个口径区分两者，这里只管传。
    query: { ...options?.query, state },
  }).then((r) => r.tasks)
}

/**
 * 建任务 → 201 `{ task }`。
 *
 * 返回后端刚存下的那一份（含 `needs_manual_acceptance` / `criteria_known` /
 * `version` 等派生字段）。页面**不要**拿请求体拼一张卡插进列里：那样插进去
 * 的卡缺派生字段，显示的「需人工验收」会是错的。
 */
export function createTask(
  projectId: string,
  body: CreateTaskBody,
  options?: RequestOptions,
): Promise<Task> {
  return postJson<TaskResponse>(
    `/api/projects/${encodeURIComponent(projectId)}/tasks`,
    body,
    options,
  ).then((r) => r.task)
}

/**
 * 读一个任务 + 它的执行记录。
 *
 * `executions` 在**没有执行记录时是 `[]` 而不是缺字段**（后端有一条测试
 * 守着这件事）。所以这里照 `Execution[]` 收，不做 `?? []`：真缺了是后端的
 * 错，前端悄悄补一个空数组只会把那个错藏起来。
 */
export function getTask(
  taskId: string,
  options?: RequestOptions,
): Promise<{ task: Task; executions: Execution[] }> {
  return getJson<TaskDetailResponse>(`/api/tasks/${encodeURIComponent(taskId)}`, options).then(
    (r) => ({ task: r.task, executions: r.executions }),
  )
}

/**
 * 改标题 / 判定条件 / 约束 → `{ task }`。
 *
 * 约束四列的「不改 / 清成不限 / 改成它」三层语义见文件头。
 *
 * **生产代码目前零调用**：本里程碑的页面里没有编辑任务的入口（改标题与改
 * `done_when` 归 v0.8.2 的任务编辑视图），只有 `taskApi.test.ts` 拿它钉住
 * 请求体与错误码。留着的理由与 `pushTaskState` 同：它是端点契约的前端侧
 * 唯一副本，删了下一个里程碑就会照后端再手写一遍。
 */
export function patchTask(
  taskId: string,
  patch: PatchTaskBody,
  options?: RequestOptions,
): Promise<Task> {
  return patchJson<TaskResponse>(`/api/tasks/${encodeURIComponent(taskId)}`, patch, options).then(
    (r) => r.task,
  )
}

/**
 * 推一次状态转移 → `{ task }`。
 *
 * 前端**不复制状态机**：哪条边合法由后端 `check_transition` 判，非法回
 * 409 `illegal_task_transition`（`detail.from` / `detail.to` / `detail.hint`）。
 * 复制一份 6×6 的合法边表意味着两处都得维护，而漂移的那一次会把非法转移
 * 画成合法。
 *
 * **生产代码目前零调用**：看板本轮不做拖拽换列，详情页的转移走
 * `submitAcceptance` / `submitFailure` 两个具名端点，只有 `taskApi.test.ts`
 * 用它钉住「前端不复制状态机」这条契约。真正需要它是 v0.8.2 的看板批量操作。
 */
export function pushTaskState(
  taskId: string,
  to: TaskStateWire,
  expectedVersion?: number,
  options?: RequestOptions,
): Promise<Task> {
  const body = expectedVersion === undefined ? { to } : { to, expected_version: expectedVersion }
  return postJson<TaskResponse>(
    `/api/tasks/${encodeURIComponent(taskId)}/state`,
    body,
    options,
  ).then((r) => r.task)
}

/**
 * 提交验收判定（`review` 上的「合入主干」/「打回」）→ `{ task }`。
 *
 * `pass` → `done`；`reject` → **回到 `pending`**（不是 `failed`——打回是
 * 「再来一次」，不是「这件事做不成」）。
 */
export function submitAcceptance(
  taskId: string,
  body: AcceptanceBody,
  options?: RequestOptions,
): Promise<Task> {
  return postJson<TaskResponse>(
    `/api/tasks/${encodeURIComponent(taskId)}/acceptance`,
    body,
    options,
  ).then((r) => r.task)
}

/**
 * 失败处置（重试 / 改任务再跑 / 关掉）→ `{ task }`。
 *
 * 三个动作的落点见 `FailureBody` 的注释：只有 `retry` 会离开失败列。
 */
export function submitFailure(
  taskId: string,
  body: FailureBody,
  options?: RequestOptions,
): Promise<Task> {
  return postJson<TaskResponse>(
    `/api/tasks/${encodeURIComponent(taskId)}/failure`,
    body,
    options,
  ).then((r) => r.task)
}
