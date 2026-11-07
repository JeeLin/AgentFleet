//! `/api/projects`：项目实体（`v0.7.0` 子任务 2）。
//!
//! **形态是项目的一个属性，不是三种产品**（`PRODUCT.md` §3.2 原文）。
//! 所以这里**没有**三套 handler、没有 `match shape` 的业务分支：
//! 唯一允许按形态分支的地方是「Hub 上要不要有裸仓库」，
//! 它已经在 `ProjectShape::needs_hub_repo` 里，且只该被仓库初始化那一处调用。
//!
//! 三条口径（与 `routes/api.rs` 同一套）：
//!
//! - **错误体只有固定 code**（`{"error": "<code>"}`，约束 13）。
//! - **非法枚举值一律报错，不回落默认值**：`shape` / `theme_color` 从库里读回
//!   来不认识时报 500，而不是当成 `central`——后者会让一个隔离式项目在下次
//!   读取时声称「代码在 Hub 上」，而 Hub 上什么都没有。
//! - **项目只归档不删除**：`PRODUCT.md` §3.2 明文第一版不提供删除。删除会让已
//!   合入的任务历史、失败归因、成本记录一起丢失。这里连路由都不注册。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::err;
use hub_storage::project::{NewProject, Project, ProjectRepo, ProjectShape, ThemeColor};
use hub_storage::StorageError;

use crate::{ApiError, AppState};

/// 一次项目读写的响应体。
///
/// 直接把存储层的 `Project` 序列化出去，**不另建一套 DTO**：两者的字段集合
/// 现在完全一致，多一层映射只会多一处需要同步的地方。若将来要藏字段
/// （比如内部 id），那时再建 DTO，现在不需要。
#[derive(Debug, Clone, Serialize)]
pub struct ProjectView {
    /// 项目 id。页面拿它拼 `/project/:id/board`。
    pub id: String,
    pub name: String,
    /// `central` / `mirror` / `isolated`。
    pub shape: &'static str,
    /// 没选主题色时为 `null`（页面用中性灰，不编一个默认色）。
    pub theme_color: Option<&'static str>,
    /// 镜像式项目的上游 git 远端；其余形态 / v6 之前的存量项目为 `null`。
    pub upstream_url: Option<String>,
    /// 非空即已归档。**用 `Option` 而不是布尔**：页面要显示「什么时候归档的」，
    /// 而布尔会把这个时间点丢掉，逼前端再问一次。
    pub archived_at: Option<String>,
    pub created_at: String,
}

impl From<Project> for ProjectView {
    fn from(p: Project) -> Self {
        Self {
            id: p.id,
            name: p.name,
            shape: p.shape.as_str(),
            theme_color: p.theme_color.map(|c| c.as_str()),
            upstream_url: p.upstream_url,
            archived_at: p.archived_at,
            created_at: p.created_at,
        }
    }
}

/// `POST /api/projects` 的请求体。
#[derive(Debug, Deserialize)]
pub struct CreateProjectBody {
    pub name: String,
    /// 缺省为 `isolated`：**最保守的那个**。
    ///
    /// 判据是「猜错的代价」——把 `central` 猜成 `isolated` 只是让用户多点一次
    /// 升级，而把 `isolated` 猜成 `central` 会让 Hub 预期上面有一个根本不该存在的
    /// 仓库，且（按 §3.2）这个方向**不能改回去**。默认值必须落在能反悔的那一侧。
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub theme_color: Option<String>,
    /// 镜像式必填的上游 git 远端。非镜像式传了也**忽略**（见
    /// [`normalize_upstream_url`]），避免老前端多带一个字段就 400。
    #[serde(default)]
    pub upstream_url: Option<String>,
}

/// `PATCH /api/projects/{id}` 的请求体。
///
/// **`shape` 不在这里**：形态变更是独立入口（`POST .../shape`），因为它不是
/// 「改一个字段」——`isolated → central` 要求先在设备上把代码推成提交、Hub 建
/// 裸仓库并 fetch 成功。把它混进 PATCH 会让调用方以为改字段就够了（见子任务 2）。
#[derive(Debug, Deserialize)]
pub struct PatchProjectBody {
    #[serde(default)]
    pub name: Option<String>,
    /// **三层语义**，`None` / `Some(None)` / `Some(Some(c))`：
    /// - 键缺失 → `None`：别动；
    /// - 显式 `null` → `Some(None)`：清除主题色（回到「没选」的中性灰）；
    /// - `Some(Some(c))` → 设成这个颜色。
    ///
    /// 只写 `Option<ThemeColor>` 就分不出「没给」和「给 null」，
    /// 而「把选过的颜色清掉」是用户真会做的操作。
    #[serde(default, deserialize_with = "double_option")]
    pub theme_color: Option<Option<String>>,
}

/// `POST /api/projects/{id}/shape` 的请求体。
#[derive(Debug, Deserialize)]
pub struct ShapeBody {
    pub shape: String,
}

/// 把一个「可选字段」解析成双层 `Option`。
///
/// `serde` 默认把缺失和 `null` 都变成 `None`，而 PATCH 需要区分它们。
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

fn repo(state: &AppState) -> ProjectRepo {
    ProjectRepo::new(state.pool().clone())
}

/// 当前时间，写成 `unix:<秒>`。
///
/// **与全仓其余 repo 同一个口径**：`now` 由调用方传入，repo 自己不读时钟
/// （那样它在测试里就不可注入了）。这里只在这一层读一次。
///
/// 时钟早于 `UNIX_EPOCH` 时（系统时间被拨回去）取 0 而不是 panic：约束 21
/// 不许生产代码 panic，而 Hub 是常驻服务，`SystemTimeError` 不该把进程带走。
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("unix:{secs}")
}

/// `shape` 字符串 → 枚举。非法值报 400，**不回落**。
fn parse_shape(s: &str) -> Result<ProjectShape, ApiError> {
    s.parse::<ProjectShape>()
        .map_err(|_| ApiError::bad_request(err::INVALID_PROJECT_SHAPE))
}

fn parse_theme(s: &str) -> Result<ThemeColor, ApiError> {
    s.parse::<ThemeColor>()
        .map_err(|_| ApiError::bad_request(err::INVALID_THEME_COLOR))
}

/// 镜像式项目的上游 URL：**校验 + 持久化**，不做 clone / fetch。
///
/// 窄口径（与 `NewProjectPage.vue` 里的前端校验同一套）：trim 后非空，且
/// 至少是 `http(s)://`、`ssh://`、`git@host:path` 三者之一。刻意不做网络探测、
/// 不解析 URL：Hub 也不是 git 客户端，这一层只需要挡住「空字符串 / 随便
/// 一句话」把项目建出来；真正的可达性由后续同步环节负责。
///
/// 非镜像式传了 `upstream_url` 一律**忽略**（不报错也不落库）：上游只对镜像式
/// 有意义，中心式 / 隔离式带着一个地址反而会让读的人以为代码会从那儿来。
///
/// 两个错误码与 `parse_shape` 同一个 400 口径：
/// - `upstream_url_required`：键缺失、`null`、trim 后为空；
/// - `upstream_url_invalid`：非空但不像 git 远端。
fn normalize_upstream_url(
    shape: ProjectShape,
    raw: Option<String>,
) -> Result<Option<String>, ApiError> {
    if shape != ProjectShape::Mirror {
        return Ok(None);
    }
    let url = raw
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("upstream_url_required"))?;
    let looks_like_git_remote = url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.strip_prefix("git@").is_some_and(|rest| {
            rest.split_once(':')
                .is_some_and(|(host, path)| !host.is_empty() && !path.is_empty())
        });
    if !looks_like_git_remote {
        return Err(ApiError::bad_request("upstream_url_invalid"));
    }
    Ok(Some(url))
}

/// `GET /api/projects?include_archived=1`：项目列表。
///
/// 默认**不含**已归档的项目：归档的语义就是「不出现在列表与看板首页」。
/// 需要看归档的那一页（`/projects/archived`）显式带 `include_archived=1`。
pub async fn list_projects(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    // `1` / `true` 都收：前端拼查询串时两种写法都出现过，为这个差异报错是自找的。
    let include = matches!(
        q.get("include_archived").map(String::as_str),
        Some("1") | Some("true")
    );
    let projects = repo(&state)
        .list(include)
        .map_err(|e| ApiError::internal_with("cannot list projects", e))?;
    Ok(Json(projects.into_iter().map(ProjectView::from).collect()))
}

/// `GET /api/projects/{id}`：单个项目。
pub async fn get_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ProjectView>, ApiError> {
    let project = repo(&state)
        .get(&id)
        .map_err(|e| ApiError::internal_with("cannot read project", e))?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(project.into()))
}

/// `POST /api/projects`：建项目。
///
/// **建裸仓库不在这一层**（存储层的 `create` 注释里写着）：Hub 侧仓库初始化要碰
/// 文件系统，那归子任务 3。本 handler 只写那一行数据；因此建一个 `central`
/// 项目之后，磁盘上暂时没有仓库——这是本里程碑的已知中间状态，
/// 由子任务 3 补齐，不是遗漏。
pub async fn create_project(
    State(state): State<AppState>,
    Json(body): Json<CreateProjectBody>,
) -> Result<(StatusCode, Json<ProjectView>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request(err::BAD_BODY));
    }
    let shape = match body.shape.as_deref() {
        Some(s) => parse_shape(s)?,
        None => ProjectShape::Isolated,
    };
    let theme_color = match body.theme_color.as_deref() {
        Some(s) => Some(parse_theme(s)?),
        None => None,
    };
    // 必须在写库**之前**校验：缺 URL 的镜像式项目一旦落库就再也补不回来
    // （PATCH 不收这个字段，第一版也不提供删除）。
    let upstream_url = normalize_upstream_url(shape, body.upstream_url)?;

    let at = now();
    let project = repo(&state)
        .create(
            NewProject {
                name: name.to_string(),
                shape,
                theme_color,
                upstream_url,
            },
            &at,
        )
        .map_err(|e| ApiError::internal_with("cannot create project", e))?;
    Ok((StatusCode::CREATED, Json(project.into())))
}

/// `PATCH /api/projects/{id}`：改名称与主题色。
///
/// **不碰 `shape`**：见 [`PatchProjectBody`] 的注释。
pub async fn patch_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchProjectBody>,
) -> Result<Json<ProjectView>, ApiError> {
    let r = repo(&state);

    // 先确认项目存在：否则 `rename` 会在「没这行」时返回一个信息更少的错误，
    // 而 404 是页面唯一能据此跳到「项目不存在」空态的信号。
    let current = r
        .get(&id)
        .map_err(|e| ApiError::internal_with("cannot read project", e))?
        .ok_or_else(ApiError::not_found)?;

    // 名称与颜色是**一次**写入（存储层只有一个 `rename`），所以先把两个字段
    // 各自解析成「最终值」，再调一次。分两次调会写出一个中间态，
    // 而中间态在并发下可能被读到。
    let name = match body.name {
        Some(n) => {
            let n = n.trim();
            if n.is_empty() {
                return Err(ApiError::bad_request(err::BAD_BODY));
            }
            n.to_string()
        }
        None => current.name.clone(),
    };
    let theme_color = match body.theme_color {
        // 键缺失：保持原值。
        None => current.theme_color,
        // 显式 null：清除。
        Some(None) => None,
        // 给了值：设成它。
        Some(Some(s)) => Some(parse_theme(&s)?),
    };

    let updated = r
        .rename(&id, &name, theme_color)
        .map_err(|e| ApiError::internal_with("cannot rename project", e))?;
    Ok(Json(updated.into()))
}

/// `POST /api/projects/{id}/shape`：形态升级（**只允许 `isolated → central`**）。
///
/// # 为什么是 400 而不是 403
///
/// 被拒的理由不是「你没权限」，而是「这个变更在产品上不存在」
/// （`PRODUCT.md` §3.2：把已经上传的代码收回，产品做不到）。403 会让页面
/// 提示「联系管理员」，而单用户自我托管里没有那个人。
///
/// # 为什么现在不做「先建仓库再改字段」
///
/// 子任务 2 的测试标准 2b 要求「升级中途建仓库失败时 `shape` 不得变成
/// `central`」。仓库初始化归子任务 3，此时还没有可失败的建仓步骤，
/// 所以现在**只改字段**是对的：没有仓库这一步，也就没有半成品状态。
/// 子任务 3 落地时要把建仓与落库并成一个事务——那时这条 handler 会变。
pub async fn change_shape(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ShapeBody>,
) -> Result<Json<ProjectView>, ApiError> {
    let to = parse_shape(&body.shape)?;
    let updated = repo(&state).upgrade_shape(&id, to).map_err(|e| match e {
        // 形态转换被拒 → 400，且**带原因**：`{"error": "...", "from": ..., "to": ...}`
        // 只报「不允许」用户不知道该改成什么。
        StorageError::ShapeTransitionRefused { from, to } => ApiError::bad_request_with(
            err::SHAPE_TRANSITION_REFUSED,
            || serde_json::json!({ "from": from, "to": to }),
        ),
        StorageError::ProjectNotFound { .. } => ApiError::not_found(),
        // coverage: excluded — `StorageError` 余下的变体都是数据库故障（连接池取不到、
        // SQLite 报错、序列化失败）。它们要求把库打坏才走得到，而那类故障在
        // `hub-storage` 自己的单测里已经有专门构造。这里保留兜底是为了**不把
        // 未知错误翻译成 404**——那会让「库坏了」看起来像「项目不存在」。
        other => ApiError::internal_with("cannot change project shape", other),
    })?;
    Ok(Json(updated.into()))
}

/// `POST /api/projects/{id}/archive`：归档。
///
/// **归档不打断正在跑的执行**（子任务 2 的生命周期规则）：它只让项目从列表与
/// 看板首页消失、停止派发新任务；历史执行记录 / 轨迹 / 管家总结全部保留。
pub async fn archive_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ProjectView>, ApiError> {
    let at = now();
    let updated = repo(&state).archive(&id, &at).map_err(|e| match e {
        StorageError::ProjectNotFound { .. } => ApiError::not_found(),
        // coverage: excluded — 同 `change_project_shape` 的兜底：余下的变体是数据库故障，
        // 正常文件系统构造不出。留它是为了不把「库坏了」说成「项目不存在」。
        other => ApiError::internal_with("cannot archive project", other),
    })?;
    Ok(Json(updated.into()))
}

/// `POST /api/projects/{id}/unarchive`：取消归档。
///
/// 有它才谈得上「可随时取消归档」——只有归档入口的归档就是删除。
pub async fn unarchive_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ProjectView>, ApiError> {
    let updated = repo(&state).unarchive(&id).map_err(|e| match e {
        StorageError::ProjectNotFound { .. } => ApiError::not_found(),
        // coverage: excluded — 同 `archive_project` 的兜底。
        other => ApiError::internal_with("cannot unarchive project", other),
    })?;
    Ok(Json(updated.into()))
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

    /// 故障注入：表改名，让针对它的 SQL 当场报 `no such table`。
    /// 与 `tests/common/mod.rs` 的 `break_device_lookup` 同一手法。
    fn break_project_table(state: &AppState) {
        state
            .pool()
            .get()
            .unwrap()
            .execute_batch("ALTER TABLE project RENAME TO project_moved")
            .unwrap();
    }

    /// 形态升级时库坏了 → 500，**不是 404 也不是 400**。
    ///
    /// 404 会让运维去查「哪个项目没了」，400 会让用户以为是自己的参数错了；
    /// 真相是 Hub 自己写不进去。
    #[tokio::test]
    async fn a_broken_shape_upgrade_is_a_500() {
        let (state, _d) = state();
        break_project_table(&state);

        let e = change_shape(
            State(state.clone()),
            Path("p_1".into()),
            Json(ShapeBody {
                shape: "central".into(),
            }),
        )
        .await
        .expect_err("读不出项目就不该声称它已升级");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// 归档时库坏了 → 500。
    #[tokio::test]
    async fn a_broken_archive_is_a_500() {
        let (state, _d) = state();
        break_project_table(&state);

        let e = archive_project(State(state.clone()), Path("p_1".into()))
            .await
            .expect_err("写不进去就要报错");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// 取消归档时库坏了 → 500。
    #[tokio::test]
    async fn a_broken_unarchive_is_a_500() {
        let (state, _d) = state();
        break_project_table(&state);

        let e = unarchive_project(State(state.clone()), Path("p_1".into()))
            .await
            .expect_err("写不进去就要报错");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), "internal_error");
    }

    /// 三条「正常」路径仍然成立：升级、归档、取消归档。
    ///
    /// 上面几条都在把库打坏；这条钉的是「打坏之前它本来就是对的」，否则一个
    /// 永远回 500 的实现也能让那几条全绿。同时验证归档是幂等的（第二次
    /// 保留第一次的时间），那是 `archive` 的对外契约。
    #[tokio::test]
    async fn the_happy_path_upgrades_archives_and_unarchives() {
        let (state, _d) = state();
        let id = repo(&state)
            .create(
                NewProject {
                    name: "proj".into(),
                    shape: ProjectShape::Isolated,
                    theme_color: None,
                    upstream_url: None,
                },
                &now(),
            )
            .expect("create")
            .id;

        let upgraded = change_shape(
            State(state.clone()),
            Path(id.clone()),
            Json(ShapeBody {
                shape: "central".into(),
            }),
        )
        .await
        .expect("isolated → central 是允许的方向")
        .0;
        assert_eq!(upgraded.shape, "central");

        let archived = archive_project(State(state.clone()), Path(id.clone()))
            .await
            .expect("归档")
            .0;
        let first_at = archived.archived_at.clone().expect("归档时间");
        let again = archive_project(State(state.clone()), Path(id.clone()))
            .await
            .expect("重复归档要幂等")
            .0;
        assert_eq!(
            again.archived_at.as_deref(),
            Some(first_at.as_str()),
            "重复归档保留第一次的时间"
        );

        let back = unarchive_project(State(state.clone()), Path(id.clone()))
            .await
            .expect("取消归档")
            .0;
        assert_eq!(back.archived_at, None);
    }

    // ---- 镜像式项目的上游 URL（只做校验 + 持久化，不 clone / fetch）----

    /// 直接打 handler 建一个项目；返回 (状态码 / 视图) 或带码的 400。
    ///
    /// 不绕真路由是为了让这几条用例只盯住 `create_project` 的入参校验与
    /// `ProjectView` 的回显；路由装配 / CSRF / 认证由
    /// `tests/project_shape.rs` 那一层覆盖。`TempDir` 随 `state` 一起返回，
    /// 用例要把两者一起留到断言「落库」做完之后。
    async fn create(
        body: CreateProjectBody,
    ) -> (
        (AppState, tempfile::TempDir),
        Result<(StatusCode, Json<ProjectView>), ApiError>,
    ) {
        let (state, dir) = state();
        let result = create_project(State(state.clone()), Json(body)).await;
        ((state, dir), result)
    }

    /// 镜像式缺 URL（键缺失 / `null` / 只有空格）→ 400，且**什么都没建**。
    ///
    /// 空串那一档尤其要挡：它不是「用户没填」这么简单，落库之后会是一个
    /// 声称有上游、实际哪儿也去不了的镜像式项目，而第一版不提供删除。
    #[tokio::test]
    async fn a_mirror_project_without_an_upstream_url_is_400() {
        for raw in [None, Some("".into()), Some("   \t".into())] {
            let ((state, _d), result) = create(CreateProjectBody {
                name: "镜像".into(),
                shape: Some("mirror".into()),
                theme_color: None,
                upstream_url: raw,
            })
            .await;
            let e = result.expect_err("镜像式没有上游 URL 必须被拒");
            assert_eq!(e.status(), StatusCode::BAD_REQUEST);
            assert_eq!(e.code(), "upstream_url_required");
            assert!(
                repo(&state).list(true).unwrap().is_empty(),
                "被拒的请求不得落库（第一版没有删除入口）"
            );
        }
    }

    /// 非空但根本不像 git 远端 → 另一个 400 码，用户能分清「没填」和「填错」。
    #[tokio::test]
    async fn a_mirror_project_with_a_non_git_upstream_url_is_400() {
        for raw in ["随便写一句", "ftp://example.com/repo.git", "git@github.com"] {
            let ((_state, _d), result) = create(CreateProjectBody {
                name: "镜像".into(),
                shape: Some("mirror".into()),
                theme_color: None,
                upstream_url: Some(raw.into()),
            })
            .await;
            let e = result.expect_err("不像 git 远端的值必须被拒");
            assert_eq!(e.status(), StatusCode::BAD_REQUEST);
            assert_eq!(e.code(), "upstream_url_invalid", "raw={raw}");
        }
    }

    /// 三种合法的 git 远端形态都要**落库并回显**，且去空格后存的是 URL 本身。
    /// 落库的判据走 `ProjectRepo::get` 重新读一行，不是只看 handler 的返回值。
    #[tokio::test]
    async fn a_mirror_project_persists_and_echoes_its_upstream_url() {
        for raw in [
            "https://github.com/you/app.git",
            "http://git.internal/app.git",
            "ssh://git@example.com/you/app.git",
            "git@github.com:you/app.git",
        ] {
            let ((state, _d), result) = create(CreateProjectBody {
                name: "镜像".into(),
                shape: Some("mirror".into()),
                theme_color: None,
                upstream_url: Some(format!("  {raw}  ")),
            })
            .await;
            let (status, Json(view)) = result.expect("合法上游 URL 必须建得出来");
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(view.shape, "mirror");
            assert_eq!(view.upstream_url.as_deref(), Some(raw), "响应体回显上游");
            let stored = repo(&state).get(&view.id).unwrap().expect("项目必须落库");
            assert_eq!(stored.upstream_url.as_deref(), Some(raw), "库里也要有上游");
        }
    }

    /// 中心式 / 隔离式**不受影响**：不带 URL 照常建出来，回显 `null`；
    /// 带了 URL 也照常建成且被忽略（上面 `normalize_upstream_url` 选的是
    /// 「忽略」而不是「拒绝」，这条用例把选型锁住）。
    #[tokio::test]
    async fn non_mirror_shapes_are_unaffected_by_upstream_url() {
        for shape in ["central", "isolated"] {
            let ((_state, _d), result) = create(CreateProjectBody {
                name: "普通项目".into(),
                shape: Some(shape.into()),
                theme_color: None,
                upstream_url: None,
            })
            .await;
            let (status, Json(view)) = result.expect("非镜像式不需要上游 URL");
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(view.shape, shape);
            assert_eq!(view.upstream_url, None);
        }

        let ((state, _d), result) = create(CreateProjectBody {
            name: "普通项目".into(),
            shape: Some("isolated".into()),
            theme_color: None,
            upstream_url: Some("https://github.com/you/app.git".into()),
        })
        .await;
        let (status, Json(view)) = result.expect("非镜像式多带一个上游字段不该 400");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(view.shape, "isolated");
        assert_eq!(view.upstream_url, None, "非镜像式的上游字段被忽略");
        assert_eq!(
            repo(&state).get(&view.id).unwrap().unwrap().upstream_url,
            None,
            "忽略必须是真的没落库，不是只藏了响应字段"
        );
    }
}
