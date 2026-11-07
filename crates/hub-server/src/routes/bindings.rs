//! `/api/projects/{id}/bindings` 与 `/api/devices/{id}/bindings`：
//! `(项目, 设备) → 固定目录`（`v0.7.0` 子任务 4）。
//!
//! # 双向入口，同一行数据
//!
//! `PRODUCT.md` §6.1 要求「项目页与设备页**均可**修改绑定」：用户是在看板里
//! 想「这个项目该跑在哪台机器上」，也**会**在设备页里想「这台机器上跑了哪些
//! 项目」。两个入口都必须能改，否则另一侧就只是只读视图。
//!
//! 但**只有一份数据**。这里四条路由（项目侧 GET/PUT、设备侧 GET/PUT）全部落到
//! [`BindingRepo`] 的同两个方法上，`upsert` 只有一个实现。若哪天出现"设备侧的
//! binding"这种第二份存储，清理与配额算的就不再是同一件事。
//!
//! # PUT 是全仓唯一允许 upsert 的地方
//!
//! 因为它是**设置**而不是建实体：用户改的是"这个项目在这台设备上该用哪个
//! 目录"，不是"再建一个绑定"。所以同一个 `(project, device)` 重复 PUT 更新
//! 同一行，行数不变。
//!
//! # `fixed_dir` 有执行记录之后不能改
//!
//! worktree 建在固定目录下，改了目录等于让正在跑的执行失去归属。存储层已经
//! 在同一事务里判过（`upsert` 返回 [`hub_storage::StorageError::FixedDirInUse`]，
//! 带执行条数），这里只负责把它翻成**可区分的 409**——笼统的 400 会让用户
//! 以为是自己填错了。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use hub_storage::binding::{Binding, BindingRepo, BindingSpec, CleanupPolicy};
use hub_storage::project::ProjectRepo;
use hub_storage::StorageError;

use crate::{err, ApiError, AppState};

/// 一次绑定读写的响应体。
///
/// 与 `ProjectView` 同一口径：直接序列化存储层的结构，**不另建 DTO**。
/// 字段集合现在一致，多一层映射只多一处要同步的地方。
#[derive(Debug, Clone, Serialize)]
pub struct BindingView {
    pub id: String,
    pub project_id: String,
    pub device_id: String,
    /// 设备上的路径原文（不展开 `~`：Hub 没有那台设备的文件系统）。
    pub fixed_dir: String,
    /// `null` = 不限额。**不是 0**：0 是一个会把一切都拦掉的真实配额。
    pub disk_quota_bytes: Option<i64>,
    /// `conservative` / `balanced` / `aggressive`。
    pub cleanup_policy: &'static str,
    pub created_at: String,
}

impl From<Binding> for BindingView {
    fn from(b: Binding) -> Self {
        Self {
            id: b.id,
            project_id: b.project_id,
            device_id: b.device_id,
            fixed_dir: b.fixed_dir,
            disk_quota_bytes: b.disk_quota_bytes,
            cleanup_policy: b.cleanup_policy.as_str(),
            created_at: b.created_at,
        }
    }
}

/// `PUT` 的请求体。
#[derive(Debug, Deserialize)]
pub struct PutBindingBody {
    pub fixed_dir: String,
    /// 不给 = 不限额。**与"给 0"分开**：后者会把一切拦住。
    #[serde(default)]
    pub disk_quota_bytes: Option<i64>,
    /// 不给时用存储层的默认值（`balanced`），**不在这里编一个**。
    #[serde(default)]
    pub cleanup_policy: Option<String>,
}

fn repo(state: &AppState) -> BindingRepo {
    BindingRepo::new(state.pool().clone())
}

fn projects(state: &AppState) -> ProjectRepo {
    ProjectRepo::new(state.pool().clone())
}

/// 当前时间，`unix:<秒>`。与全仓其余 repo 同一口径（`now` 由调用方传入）。
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// 把 `StorageError::FixedDirInUse` 翻成带条数的 409。
///
/// **带条数**是刻意的：用户要知道「有 3 条执行记录」，而不是只看到「不能改」。
/// 只说「不能改」的话，他无法判断是「等一下再说」还是「永远不行」。
fn fixed_dir_locked(executions: i64) -> ApiError {
    ApiError::conflict_with(
        err::FIXED_DIR_LOCKED,
        || serde_json::json!({ "executions": executions }),
    )
}

/// `GET /api/projects/{project_id}/bindings`：一个项目的全部绑定。
pub async fn list_for_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<Json<Vec<BindingView>>, ApiError> {
    // 项目不存在时 404，而不是返回空数组：空数组会让页面显示「还没有绑定任何
    // 设备」——那句话在项目不存在时是错的，用户会以为是自己还没绑。
    if projects(&state)
        .get(&project_id)
        .map_err(not_found_or_internal)?
        .is_none()
    {
        return Err(ApiError::not_found());
    }
    let rows = repo(&state)
        .list_for_project(&project_id)
        .map_err(internal("cannot list bindings"))?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// `GET /api/devices/{device_id}/bindings`：一台设备上的全部绑定。
///
/// 设备侧不校验设备是否存在：设备注册表是「连接过就算」，而绑定可以指向一台
/// 暂时离线的设备（那正是它有价值的时候——用户想先绑好再上线）。空数组在这里
/// 是**正确答案**。
pub async fn list_for_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> Result<Json<Vec<BindingView>>, ApiError> {
    let rows = repo(&state)
        .list_for_device(&device_id)
        .map_err(internal("cannot list bindings"))?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// `GET /api/projects/{project_id}/bindings/{device_id}`：读一行（项目侧）。
pub async fn get_one_from_project(
    State(state): State<AppState>,
    Path((project_id, device_id)): Path<(String, String)>,
) -> Result<Json<BindingView>, ApiError> {
    get_one_inner(&state, &project_id, &device_id)
}

/// `GET /api/devices/{device_id}/bindings/{project_id}`：读**同一行**（设备侧）。
///
/// 参数顺序跟着地址走，不是两套逻辑——两条路径指向同一个 `(项目, 设备)` 组合。
pub async fn get_one_from_device(
    State(state): State<AppState>,
    Path((device_id, project_id)): Path<(String, String)>,
) -> Result<Json<BindingView>, ApiError> {
    get_one_inner(&state, &project_id, &device_id)
}

fn get_one_inner(
    state: &AppState,
    project_id: &str,
    device_id: &str,
) -> Result<Json<BindingView>, ApiError> {
    match repo(state)
        .get(project_id, device_id)
        .map_err(internal("cannot read binding"))?
    {
        Some(b) => Ok(Json(b.into())),
        None => Err(ApiError::not_found()),
    }
}

/// `PUT /api/projects/{project_id}/bindings/{device_id}`：写一行（upsert）。
///
/// 设备侧的那条 PUT 走 [`put_from_device`]，**落到同一个 `upsert`**。
pub async fn put_from_project(
    State(state): State<AppState>,
    Path((project_id, device_id)): Path<(String, String)>,
    Json(body): Json<PutBindingBody>,
) -> Result<(StatusCode, Json<BindingView>), ApiError> {
    put(state, project_id, device_id, body)
}

/// `PUT /api/devices/{device_id}/bindings/{project_id}`：设备侧的同一行。
///
/// 参数顺序反过来是**地址的写法**，不是两套逻辑：这两条路径指向同一个
/// `(project, device)` 组合，所以在 `Path` 里换一下位置就交回 [`put`]。
pub async fn put_from_device(
    State(state): State<AppState>,
    Path((device_id, project_id)): Path<(String, String)>,
    Json(body): Json<PutBindingBody>,
) -> Result<(StatusCode, Json<BindingView>), ApiError> {
    put(state, project_id, device_id, body)
}

/// 两条 PUT 的唯一实现。
fn put(
    state: AppState,
    project_id: String,
    device_id: String,
    body: PutBindingBody,
) -> Result<(StatusCode, Json<BindingView>), ApiError> {
    // 目录是设备上的路径。空目录名会让 worktree 建在设备的根上——
    // 那是「清空整块盘」的前一步，不能放过。
    let fixed_dir = body.fixed_dir.trim();
    if fixed_dir.is_empty() {
        return Err(ApiError::bad_request(err::BAD_BODY));
    }

    // 项目必须存在：绑定挂在项目上，挂一个不存在的项目等于造了一行孤儿。
    if projects(&state)
        .get(&project_id)
        .map_err(not_found_or_internal)?
        .is_none()
    {
        return Err(ApiError::not_found());
    }

    // 策略不给时交给存储层的默认值；给了就必须是表里的词。
    let cleanup_policy = match body.cleanup_policy.as_deref() {
        None => None,
        Some(s) => Some(
            s.parse::<CleanupPolicy>()
                .map_err(|_| ApiError::bad_request(err::INVALID_CLEANUP_POLICY))?,
        ),
    };

    let spec = BindingSpec {
        fixed_dir: fixed_dir.to_string(),
        disk_quota_bytes: body.disk_quota_bytes,
        // `upsert` 收的是完整 spec，而"不给策略"要沿用默认值。
        // 这里给出存储层的默认值，而不是在路由层再定一份。
        // 不给策略时用存储层的默认档（`impl Default for CleanupPolicy`）。
        // **不在这里写 `Balanced`**：那是默认值，不是路由层的决定。
        cleanup_policy: cleanup_policy.unwrap_or_default(),
    };

    // 先查一次是否已有这一行：存储层的 `upsert` 只回 `Binding`，不回「新建还是
    // 更新」，而 201 / 200 的区别正是页面提示「已绑定」与「已更新」的依据。
    let existed = repo(&state)
        .get(&project_id, &device_id)
        .map_err(internal("cannot read binding"))?
        .is_some();

    let binding = repo(&state)
        .upsert(&project_id, &device_id, spec, &now())
        .map_err(|e| match e {
            StorageError::FixedDirInUse { executions, .. } => fixed_dir_locked(executions),
            StorageError::ProjectNotFound { .. } => ApiError::not_found(),
            // coverage: excluded — 同 `internal` 的理由：余下的是数据库故障。
            other => ApiError::internal_with("cannot save binding", other),
        })?;

    let code = if existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((code, Json(binding.into())))
}

/// 把「项目不存在」与「读库失败」分开：前者 404，后者 500。
///
/// 合成一个会让「库坏了」看起来像「项目不存在」——那种错位会把运维引到
/// 错误的方向去查（`projects.rs` 同一处兜底里写着同样的理由）。
fn not_found_or_internal(e: StorageError) -> ApiError {
    match e {
        StorageError::ProjectNotFound { .. } => ApiError::not_found(),
        // coverage: excluded — 要走到这里必须把库读坏（见 `internal` 的理由）。
        // 留它不是防御性编程：把「库坏了」翻译成 404 会让用户以为项目没了。
        other => internal("cannot read project")(other),
    }
}

/// 内部错误。写成闭包是为了让 `map_err` 那里读起来是一句话。
///
/// `StorageError` 余下的变体都是数据库故障（连接池取不到、SQLite 报错、
/// 序列化失败），留这条兜底是为了不把「库坏了」翻译成 404。
fn internal(step: &'static str) -> impl Fn(StorageError) -> ApiError {
    // coverage: excluded — 上面那个 `Fn` 的构造行：闭包体只在错误路径执行。
    move |e| ApiError::internal_with(step, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthParams, LoginRateLimiter};
    use std::time::Duration;

    fn state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
        let state = AppState::bootstrap_with(
            pool,
            dir.path(),
            AuthParams {
                m_cost: 8192,
                t_cost: 1,
                p_cost: 1,
            },
            Duration::from_millis(200),
            LoginRateLimiter::new(10, 60),
        )
        .unwrap();
        (state, dir)
    }

    fn new_project(state: &AppState, name: &str) -> String {
        projects(state)
            .create(
                hub_storage::project::NewProject {
                    name: name.to_string(),
                    shape: hub_storage::project::ProjectShape::Isolated,
                    theme_color: None,
                    upstream_url: None,
                },
                &now(),
            )
            .expect("create project")
            .id
    }

    fn body(dir: &str) -> PutBindingBody {
        PutBindingBody {
            fixed_dir: dir.to_string(),
            disk_quota_bytes: None,
            cleanup_policy: None,
        }
    }

    /// 故障注入：把一张表改名，让针对它的 SQL 全部报 `no such table`。
    ///
    /// **改名而不是 `DROP TABLE`**：连接池里缓存的语句与表名绑定，改名同样
    /// 让读写当场失败，而行为稳定可复现（同 `tests/common/mod.rs` 的
    /// `break_device_lookup`）。
    fn break_table(state: &AppState, table: &str) {
        state
            .pool()
            .get()
            .unwrap()
            .execute_batch(&format!("ALTER TABLE {table} RENAME TO {table}_moved"))
            .unwrap();
    }

    /// **读库失败不是 404**：`list_for_project` 里项目查不出来时必须回 500。
    ///
    /// 把它翻成 404 的后果是运维去查「哪个项目被删了」，而真相是库坏了。
    /// 这条同时走 `not_found_or_internal` 的 `other` 臂与 `internal` 闭包体。
    #[tokio::test]
    async fn a_broken_project_read_is_a_500_not_a_404() {
        let (state, _d) = state();
        break_table(&state, "project");

        let e = list_for_project(State(state.clone()), Path("p_1".into()))
            .await
            .expect_err("库坏了不许当成「项目不存在」");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// 同一个判据在 PUT 的第一道检查上（`put` 里那次 `projects().get`）。
    #[tokio::test]
    async fn a_broken_project_read_during_put_is_a_500_not_a_404() {
        let (state, _d) = state();
        break_table(&state, "project");

        let e = put(state.clone(), "p_1".into(), "n_1".into(), body("~/code"))
            .expect_err("库坏了不许当成「项目不存在」");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// **写库失败是 500**：`upsert` 的兜底臂（`cannot save binding`）。
    ///
    /// 库坏在写入这一步时，`fixed_dir` 与配额的错误口径都不适用——只能如实
    /// 报内部错误，而不是假装「这个目录被锁了」或「项目不存在」。
    #[tokio::test]
    async fn a_broken_binding_write_is_a_500() {
        let (state, _d) = state();
        let p = new_project(&state, "proj");
        break_table(&state, "binding");

        let e = put(state.clone(), p.clone(), "n_1".into(), body("~/code"))
            .expect_err("写不进去就要报错，不能静默成功");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// 读一行时库坏了（`get_one_inner` 的 `internal("cannot read binding")`）。
    ///
    /// 与「这一行不存在」（404）必须分开：前者是 Hub 的事故，后者是用户的
    /// 事实。合成一个会让「库坏了」看起来像「还没绑过」。
    #[tokio::test]
    async fn a_broken_binding_read_is_a_500_not_a_404() {
        let (state, _d) = state();
        break_table(&state, "binding");

        let e = get_one_from_project(State(state.clone()), Path(("p_1".into(), "n_1".into())))
            .await
            .expect_err("读不出来不许当成「没有这一行」");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // 设备侧的入口指向同一行、同一个实现，参数顺序跟着地址走。
        let e = get_one_from_device(State(state.clone()), Path(("n_1".into(), "p_1".into())))
            .await
            .expect_err("设备侧同一个判据");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// 列表读库失败同样是 500（`internal("cannot list bindings")` 的两处）。
    #[tokio::test]
    async fn a_broken_binding_list_is_a_500() {
        let (state, _d) = state();
        let p = new_project(&state, "proj");
        break_table(&state, "binding");

        let from_project = list_for_project(State(state.clone()), Path(p))
            .await
            .expect_err("项目侧列表读不出来要报错");
        assert_eq!(from_project.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let from_device = list_for_device(State(state.clone()), Path("n_1".into()))
            .await
            .expect_err("设备侧列表读不出来要报错");
        assert_eq!(from_device.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// 正常路径必须仍然成立：新建 201、重复 PUT 200 且只有一行。
    ///
    /// 上面几条都在把库打坏；这条钉的是「打坏之前它本来就是对的」，否则
    /// 一个永远回 500 的实现也能让那几条全绿。
    #[tokio::test]
    async fn a_repeated_put_updates_the_same_row_and_flips_201_to_200() {
        let (state, _d) = state();
        let p = new_project(&state, "proj");

        let (code, first) = put(state.clone(), p.clone(), "n_1".into(), body("~/code"))
            .expect("第一次 PUT 应当成功");
        assert_eq!(code, StatusCode::CREATED);

        let (code, second) = put(state.clone(), p.clone(), "n_1".into(), body("~/code"))
            .expect("第二次 PUT 应当成功");
        assert_eq!(code, StatusCode::OK, "同一组合重复 PUT 是更新，不是新建");
        assert_eq!(second.0.id, first.0.id, "更新的必须是同一行");

        let rows = list_for_project(State(state.clone()), Path(p))
            .await
            .expect("列表")
            .0;
        assert_eq!(rows.len(), 1, "upsert 不许造出第二行");
    }
}
