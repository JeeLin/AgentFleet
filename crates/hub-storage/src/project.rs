//! 项目仓储。
//!
//! 项目 = **代码的去处**（`PRODUCT.md` §3.2）。三种形态（中心式 / 镜像式 /
//! 隔离式）是同一个表的 `shape` 列的三个取值，**不是三种产品**：Task / Binding /
//! 看板 / 管家共用一套，差别只在「代码从哪来、到哪去」这一层。
//!
//! # 可归档，不可删除
//!
//! 本模块**没有**、也不该有 `delete`。`PRODUCT.md` §3.2 的理由是「失败归因和
//! 审计需要历史」——一个项目哪怕跑了十次失败，那些记录对排障比一个干净的空
//! 项目更有价值。归档（`archive`）承担「我不想再看见它」这个需求，且可撤销。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::Db;

/// 项目的代码去向。**是属性，不是类型**（`PRODUCT.md` §3.2 原文：
/// 「形态是项目的一个属性，不是三种产品」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectShape {
    /// 代码在 Hub 的裸仓库里，全部代码 + 元数据都存 Hub。
    Central,
    /// 上游（GitHub 等）+ Hub 镜像。起点是 Hub 上镜像的 commit。
    Mirror,
    /// 代码**只在指定设备上，永不出该设备**。Hub 只存元数据。
    Isolated,
}

impl ProjectShape {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Central => "central",
            Self::Mirror => "mirror",
            Self::Isolated => "isolated",
        }
    }

    /// 这种形态的项目，Hub 上该不该有一个裸仓库。
    ///
    /// 建项目时用它决定要不要 `git init --bare`；升级（`isolated → central`）
    /// 时用它决定要不要补建。**唯一合法的按形态分支**——其余业务层一律不许
    /// `match shape`，见里程碑设计核对点 4。
    pub fn needs_hub_repo(self) -> bool {
        match self {
            Self::Central | Self::Mirror => true,
            Self::Isolated => false,
        }
    }

    /// 能否从 `self` 变成 `to`。
    ///
    /// **只有 `isolated → central` 一条**，且这不是实现偏好：
    /// `PRODUCT.md` §3.2 原文「用户可以随时把隔离式转成中心式（推上去即可），
    /// 反之不提供——把已经上传的代码收回，产品做不到真收」。
    ///
    /// 其余方向一律 `false`，包括 `central → mirror`：镜像式要指一个上游，
    /// 而中心式的代码没有上游，「改成镜像」等于凭空造一个上游出来。
    pub fn can_transition_to(self, to: Self) -> bool {
        matches!((self, to), (Self::Isolated, Self::Central))
    }
}

impl FromStr for ProjectShape {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "central" => Ok(Self::Central),
            "mirror" => Ok(Self::Mirror),
            "isolated" => Ok(Self::Isolated),
            // 不回落成默认值：静默存成 `central` 会让一个隔离式项目
            // 在下次读取时声称「代码在 Hub 上」，而 Hub 上什么都没有。
            other => Err(StorageError::InvalidProjectShape(other.to_string())),
        }
    }
}

/// 项目主题色。**只做视觉区分，不影响任何业务逻辑**（`PRODUCT.md` §3.2）。
///
/// 做成枚举而不是裸字符串，是因为它要出现在三处界面（看板卡片左边框、
/// 全局任务流的项目标签、项目页页眉）——有了枚举，前端拿到的取值集合是封闭的，
/// 不用为「后端返回了一个 CSS 认不出的颜色名」写兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeColor {
    Blue,
    Green,
    Purple,
    Orange,
    Gray,
}

impl ThemeColor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Purple => "purple",
            Self::Orange => "orange",
            Self::Gray => "gray",
        }
    }
}

impl FromStr for ThemeColor {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "blue" => Ok(Self::Blue),
            "green" => Ok(Self::Green),
            "purple" => Ok(Self::Purple),
            "orange" => Ok(Self::Orange),
            "gray" => Ok(Self::Gray),
            other => Err(StorageError::InvalidThemeColor(other.to_string())),
        }
    }
}

/// 一个项目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub shape: ProjectShape,
    /// 没选主题色时为 `None`（页面上用中性灰，不编一个默认色）。
    ///
    /// 存 `Option` 而不是「缺省就 gray」：gray 是一个**用户选过的**颜色，
    /// 与「没选」在页面上应该长得不一样（前者显示色点，后者不显示）。
    pub theme_color: Option<ThemeColor>,
    /// 非空即已归档。归档的项目不出现在列表与看板首页，任务停止派发。
    pub archived_at: Option<String>,
    pub created_at: String,
}

impl Project {
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// 建项目的入参。
///
/// 不提供 `id` / `created_at`：前者由 [`crate::workspace::new_id`] 生成，
/// 后者由调用方给（与全仓其余 repo 同一个 `now: &str` 口径，便于测试注入）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProject {
    pub name: String,
    pub shape: ProjectShape,
    pub theme_color: Option<ThemeColor>,
}

/// 项目仓储。
#[derive(Clone)]
pub struct ProjectRepo {
    pool: Db,
}

impl ProjectRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 建项目。
    ///
    /// **不在这里建裸仓库**：Hub 侧的仓库初始化要碰文件系统，那是 `hub-core`
    /// 的 `repo` 模块的事。本函数只负责那一行数据。
    pub fn create(&self, new: NewProject, now: &str) -> Result<Project> {
        let conn = self.pool.get()?;
        let id = crate::workspace::new_id();
        conn.execute(
            "INSERT INTO project (id, name, shape, theme_color, archived_at, created_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                id,
                new.name,
                new.shape.as_str(),
                new.theme_color.map(ThemeColor::as_str),
                now
            ],
        )?;
        // 在**同一条连接**上读回：刚写的行与其余行走同一套映射，`shape` 的
        // 取值与解析器口径不一致时不会静默放过。
        let raw = conn.query_row(
            "SELECT id, name, shape, theme_color, archived_at, created_at
             FROM project WHERE id = ?1",
            params![id],
            row_to_raw,
        )?;
        raw.into_project()
    }

    /// 按 id 取项目。**归档的项目照样返回**——`GET /:id` 对它必须 200，
    /// 只有这样用户才能进到项目页去取消归档。
    pub fn get(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.pool.get()?;
        let raw = conn
            .query_row(
                "SELECT id, name, shape, theme_color, archived_at, created_at
                 FROM project WHERE id = ?1",
                params![id],
                row_to_raw,
            )
            .optional()?;
        raw.map(RawProject::into_project).transpose()
    }

    /// 全部**未归档**项目，最近建的在前。
    ///
    /// `include_archived` 为真时连归档的一起列（项目设置页要看到它们才能取消
    /// 归档）。**默认不列出归档**：列表页的第一屏就是「我现在在做的项目」。
    pub fn list(&self, include_archived: bool) -> Result<Vec<Project>> {
        let conn = self.pool.get()?;
        let sql = if include_archived {
            "SELECT id, name, shape, theme_color, archived_at, created_at
             FROM project ORDER BY created_at DESC, id DESC"
        } else {
            "SELECT id, name, shape, theme_color, archived_at, created_at
             FROM project WHERE archived_at IS NULL ORDER BY created_at DESC, id DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawProject::into_project).collect()
    }

    /// 改名称与主题色。**`shape` 不在这里改**——它有自己的入口
    /// （[`Self::upgrade_shape`]），因为形态变更不是「改一个字段」。
    pub fn rename(&self, id: &str, name: &str, theme_color: Option<ThemeColor>) -> Result<Project> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE project SET name = ?2, theme_color = ?3 WHERE id = ?1",
            params![id, name, theme_color.map(ThemeColor::as_str)],
        )?;
        if n == 0 {
            return Err(StorageError::ProjectNotFound { id: id.to_string() });
        }
        self.get(id)?
            .ok_or_else(|| StorageError::ProjectNotFound { id: id.to_string() })
    }

    /// 形态升级：**只允许 `isolated → central`**。
    ///
    /// # 为什么不在 `rename` 里一起做
    ///
    /// 形态变更要求「Hub 建裸仓库并 fetch 成功」与「改这一列」是同一件事的两个
    /// 半边。调用方必须把本函数放进**已经能保证仓库就绪**的事务里（或先建好
    /// 仓库再调用、失败则回滚调用方自己的事务）。把两件事塞进一个
    /// `PATCH` 的字段更新里，等于允许「声称中心式但没有仓库」的项目存在。
    ///
    /// 因此本函数只做数据那一半，并在**当前形态不允许升级**时报错——
    /// 拒绝的判据在 [`ProjectShape::can_transition_to`]。
    pub fn upgrade_shape(&self, id: &str, to: ProjectShape) -> Result<Project> {
        let current = self
            .get(id)?
            .ok_or_else(|| StorageError::ProjectNotFound { id: id.to_string() })?;
        if !current.shape.can_transition_to(to) {
            return Err(StorageError::ShapeTransitionRefused {
                from: current.shape.as_str(),
                to: to.as_str(),
            });
        }
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE project SET shape = ?2 WHERE id = ?1",
            params![id, to.as_str()],
        )?;
        self.get(id)?
            .ok_or_else(|| StorageError::ProjectNotFound { id: id.to_string() })
    }

    /// 归档。已归档再归档是幂等的（保留**第一次**的 `archived_at`）。
    ///
    /// 保留第一次的时间而不是覆盖：归档时间是「这个项目什么时候退出的主线」
    /// 这个历史事实，重复点一次按钮不该改写它。
    ///
    /// `now` 由调用方给（`hub-core::registry::now_stamp()`），与全仓其余 repo
    /// 同一个口径——`hub-storage` 不自己读时钟，测试才能注入固定时刻。
    pub fn archive(&self, id: &str, now: &str) -> Result<Project> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE project SET archived_at = ?2 WHERE id = ?1 AND archived_at IS NULL",
            params![id, now],
        )?;
        // 0 行受影响要么不存在、要么已归档——读回一行就分得清，且已归档时
        // 拿回的那行带着**第一次**的 `archived_at`，正是要保留的值。
        self.get(id)?
            .ok_or_else(|| StorageError::ProjectNotFound { id: id.to_string() })
    }

    /// 取消归档。未归档时再取消是幂等的。
    ///
    /// **判据是「这一行在不在」，不是「UPDATE 影响了几行」**：`SET archived_at
    /// = NULL` 对一个本来就没归档的项目是零改动，某些构建下 `changes()` 报 0，
    /// 于是「幂等」会变成一个假的 `ProjectNotFound`。先查存在性再更新。
    pub fn unarchive(&self, id: &str) -> Result<Project> {
        let conn = self.pool.get()?;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM project WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StorageError::ProjectNotFound { id: id.to_string() });
        }
        conn.execute(
            "UPDATE project SET archived_at = NULL WHERE id = ?1",
            params![id],
        )?;
        self.get(id)?
            .ok_or_else(|| StorageError::ProjectNotFound { id: id.to_string() })
    }
}

/// 一行原始列。`shape` / `theme_color` 的解析留在行映射之外——理由与
/// `session.rs` 的 `RawSession` 相同：`query_row` 的映射签名只收
/// `rusqlite::Error`，在里面做业务校验会把「库被改坏了」包成说不清的
/// `FromSqlConversionFailure`，调用方再也分不清它与「库里没有这一行」。
struct RawProject {
    id: String,
    name: String,
    shape: String,
    theme_color: Option<String>,
    archived_at: Option<String>,
    created_at: String,
}

impl RawProject {
    fn into_project(self) -> Result<Project> {
        Ok(Project {
            id: self.id,
            name: self.name,
            shape: ProjectShape::from_str(&self.shape)?,
            theme_color: self
                .theme_color
                .as_deref()
                .map(ThemeColor::from_str)
                .transpose()?,
            archived_at: self.archived_at,
            created_at: self.created_at,
        })
    }
}

fn row_to_raw(r: &Row<'_>) -> std::result::Result<RawProject, r2d2_sqlite::rusqlite::Error> {
    Ok(RawProject {
        id: r.get(0)?,
        name: r.get(1)?,
        shape: r.get(2)?,
        theme_color: r.get(3)?,
        archived_at: r.get(4)?,
        created_at: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, ProjectRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        (d, ProjectRepo::new(pool))
    }

    fn spec(shape: ProjectShape) -> NewProject {
        NewProject {
            name: "app".into(),
            shape,
            theme_color: Some(ThemeColor::Blue),
        }
    }

    #[test]
    fn create_round_trips_every_field() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Central), TS).unwrap();
        assert_eq!(p.name, "app");
        assert_eq!(p.shape, ProjectShape::Central);
        assert_eq!(p.theme_color, Some(ThemeColor::Blue));
        assert_eq!(p.archived_at, None);
        assert!(!p.is_archived());
        assert_eq!(p.created_at, TS);

        let f = pr.get(&p.id).unwrap().unwrap();
        assert_eq!(f.id, p.id);
        assert_eq!(f.shape, ProjectShape::Central);
    }

    /// 没选主题色就是 `None`，**不编一个默认色**：gray 是一个用户选过的颜色，
    /// 与「没选」在页面上该长得不一样。
    #[test]
    fn no_theme_color_stays_none() {
        let (_d, pr) = setup();
        let p = pr
            .create(
                NewProject {
                    name: "app".into(),
                    shape: ProjectShape::Isolated,
                    theme_color: None,
                },
                TS,
            )
            .unwrap();
        assert_eq!(p.theme_color, None);
        assert_eq!(
            pr.get(&p.id).unwrap().unwrap().theme_color,
            None,
            "None 必须原样读回，不能变成某个默认色"
        );
    }

    /// 非法 `shape` 必须报错，**不静默回落 `central`**——回落会让一个隔离式
    /// 项目在下次读取时声称「代码在 Hub 上」，而 Hub 上什么都没有。
    #[test]
    fn an_unknown_shape_in_the_database_is_an_error_not_a_default() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Central), TS).unwrap();
        pr.pool
            .get()
            .unwrap()
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        pr.pool
            .get()
            .unwrap()
            .execute(
                "UPDATE project SET shape = 'weird' WHERE id = ?1",
                params![p.id],
            )
            .unwrap();
        let e = pr.get(&p.id).unwrap_err();
        assert!(
            matches!(e, StorageError::InvalidProjectShape(_)),
            "实际: {e:?}"
        );
    }

    /// 归档：从列表里消失，但 `get` 仍返回（页面要能进去取消归档）。
    #[test]
    fn archiving_hides_from_the_list_but_keeps_get_working() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Central), TS).unwrap();
        assert_eq!(pr.list(false).unwrap().len(), 1);

        let a = pr.archive(&p.id, TS).unwrap();
        assert!(a.is_archived());
        assert_eq!(a.archived_at.as_deref(), Some(TS));

        assert!(pr.list(false).unwrap().is_empty(), "默认列表不含归档项目");
        assert_eq!(pr.list(true).unwrap().len(), 1, "含归档的列表仍有它");
        assert!(
            pr.get(&p.id).unwrap().is_some(),
            "GET /:id 对归档项目必须仍然 200，否则用户进不去取消归档"
        );
    }

    /// 归档是幂等的，且**保留第一次的 `archived_at`**——重复点一次不该改写
    /// 「这个项目什么时候退出主线」这个历史事实。
    #[test]
    fn archiving_twice_keeps_the_first_timestamp() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Central), TS).unwrap();
        pr.archive(&p.id, TS).unwrap();
        let second = pr.archive(&p.id, "unix:9999").unwrap();
        assert_eq!(
            second.archived_at.as_deref(),
            Some(TS),
            "第二次归档不得覆盖第一次的时间"
        );
    }

    #[test]
    fn unarchive_brings_it_back() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Central), TS).unwrap();
        pr.archive(&p.id, TS).unwrap();
        let back = pr.unarchive(&p.id).unwrap();
        assert_eq!(back.archived_at, None);
        assert_eq!(pr.list(false).unwrap().len(), 1);
    }

    /// **`isolated → central` 允许**（`PRODUCT.md` §3.2 的原文承诺）。
    #[test]
    fn isolated_can_upgrade_to_central() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Isolated), TS).unwrap();
        let up = pr.upgrade_shape(&p.id, ProjectShape::Central).unwrap();
        assert_eq!(up.shape, ProjectShape::Central);
    }

    /// **其余方向一律拒绝**，且错误里带两端值——用户得知道「为什么不行」。
    #[test]
    fn every_other_shape_transition_is_refused() {
        let (_d, pr) = setup();
        let cases = [
            (ProjectShape::Central, ProjectShape::Isolated),
            (ProjectShape::Central, ProjectShape::Mirror),
            (ProjectShape::Mirror, ProjectShape::Central),
            (ProjectShape::Mirror, ProjectShape::Isolated),
            (ProjectShape::Isolated, ProjectShape::Mirror),
        ];
        for (from, to) in cases {
            let p = pr.create(spec(from), TS).unwrap();
            let e = pr.upgrade_shape(&p.id, to).unwrap_err();
            assert!(
                matches!(e, StorageError::ShapeTransitionRefused { .. }),
                "{from:?} → {to:?} 必须被拒，实际: {e:?}"
            );
            assert_eq!(
                pr.get(&p.id).unwrap().unwrap().shape,
                from,
                "被拒之后形态不得变"
            );
        }
    }

    /// `can_transition_to` 的三条真值，直接钉住规则本身。
    #[test]
    fn only_isolated_to_central_is_allowed() {
        use ProjectShape::*;
        assert!(Isolated.can_transition_to(Central));
        assert!(!Central.can_transition_to(Isolated));
        assert!(!Central.can_transition_to(Mirror));
        assert!(!Mirror.can_transition_to(Central));
        // 同形态到同形态也是「不允许」：那是空操作，不该被当成一次升级。
        for s in [Central, Mirror, Isolated] {
            assert!(!s.can_transition_to(s));
        }
    }

    /// `needs_hub_repo`：只有隔离式不在 Hub 建仓库。
    #[test]
    fn only_isolated_skips_the_hub_repo() {
        assert!(ProjectShape::Central.needs_hub_repo());
        assert!(ProjectShape::Mirror.needs_hub_repo());
        assert!(!ProjectShape::Isolated.needs_hub_repo());
    }

    #[test]
    fn rename_changes_name_and_color_not_shape() {
        let (_d, pr) = setup();
        let p = pr.create(spec(ProjectShape::Isolated), TS).unwrap();
        let r = pr
            .rename(&p.id, "renamed", Some(ThemeColor::Orange))
            .unwrap();
        assert_eq!(r.name, "renamed");
        assert_eq!(r.theme_color, Some(ThemeColor::Orange));
        assert_eq!(r.shape, ProjectShape::Isolated, "rename 不得碰 shape");
    }

    #[test]
    fn missing_projects_are_named_in_the_error() {
        let (_d, pr) = setup();
        let e = pr.rename("nope", "x", None).unwrap_err();
        assert!(matches!(e, StorageError::ProjectNotFound { .. }), "{e:?}");
        let e = pr.archive("nope", TS).unwrap_err();
        assert!(matches!(e, StorageError::ProjectNotFound { .. }), "{e:?}");
        let e = pr.unarchive("nope").unwrap_err();
        assert!(matches!(e, StorageError::ProjectNotFound { .. }), "{e:?}");
        let e = pr.upgrade_shape("nope", ProjectShape::Central).unwrap_err();
        assert!(matches!(e, StorageError::ProjectNotFound { .. }), "{e:?}");
    }

    #[test]
    fn list_is_newest_first() {
        let (_d, pr) = setup();
        let a = pr.create(spec(ProjectShape::Central), "unix:10").unwrap();
        let b = pr.create(spec(ProjectShape::Central), "unix:20").unwrap();
        let got: Vec<String> = pr.list(false).unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(got, vec![b.id, a.id]);
    }

    #[test]
    fn every_shape_round_trips_through_its_literal() {
        for s in [
            ProjectShape::Central,
            ProjectShape::Mirror,
            ProjectShape::Isolated,
        ] {
            assert_eq!(ProjectShape::from_str(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn every_theme_color_round_trips_through_its_literal() {
        for c in [
            ThemeColor::Blue,
            ThemeColor::Green,
            ThemeColor::Purple,
            ThemeColor::Orange,
            ThemeColor::Gray,
        ] {
            assert_eq!(ThemeColor::from_str(c.as_str()).unwrap(), c);
        }
        assert!(ThemeColor::from_str("chartreuse").is_err());
    }
}
