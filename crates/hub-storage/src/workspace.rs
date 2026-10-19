//! 工作区仓储。
//!
//! 工作区 = **设备上的一个 `(node_id, path)`**（约束 3、约束 10）：
//! terminal / files / git 三个 connector 都绑在它上面，生命周期随会话。
//!
//! 两处路径判据都在**这一层**，因为设备文件系统在别的机器上：Hub 既无法
//! `canonicalize`，也不该有入站通道去替设备做这件事（约束 1）。
//!
//! - [`WorkspaceRepo::validate_path`]：字面路径（`~`、`/a/b`）或相对路径
//!   （`src/app`，相对设备上的登录目录）。
//! - [`WorkspaceRepo::validate_relative`]：子路径（`src/main.rs`、`./a`），
//!   按逻辑段判 `..`，越界一律拒。
//!
//! **符号链接逃逸不在这一层判**：`path` 指向的真实目录只有设备能读，
//! 节点侧的强制在 files connector（`canonicalize` 后判是否仍在根内）。
//! Hub 这一层判不了，也不假装判了。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};
use crate::Db;

/// 合法字面路径被拒时的措辞。要说清**规则**而不是只说「非法」：
/// 调用方据此给用户可读的理由（设计规范 §7.4 要越界可见）。
const ABSOLUTE_PATH_REASON: &str = "must start with '/' or '~'";
const OUTSIDE_ROOT_REASON: &str = "must stay inside the workspace root";

/// 工作区行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub node_id: String,
    /// 设备上的路径原文（`~/src/app` 或 `/srv/app`），不做 `~` 展开。
    pub path: String,
    pub created_at: String,
}

/// 工作区仓储。`Clone` 只复制连接池句柄（`r2d2::Pool` 本身是 `Arc`）。
#[derive(Clone)]
pub struct WorkspaceRepo {
    pool: Db,
}

impl WorkspaceRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 连接池访问器（供同一 crate 内的联表查询用）。
    pub fn pool(&self) -> &Db {
        &self.pool
    }

    /// 取或建一个工作区（**在调用方的事务内**）。
    ///
    /// upsert 而不是先查后插：两个并发请求会在 SELECT 之后同时插，两条都撞
    /// UNIQUE，让「两个会话挂同一个工作区」——本该是合法业务状态——表现得
    /// 像错误。`ON CONFLICT` 让它退化成「后者拿到同一行」。
    ///
    /// 回填 `created_at` 与插入时相同：`(node_id, path)` 上工作区是**一次性**的
    /// 事实，改它就等于凭空多出一次「这个目录被打开过」。
    pub fn upsert_in(
        &self,
        tx: &Transaction<'_>,
        node_id: &str,
        path: &str,
        now: &str,
    ) -> Result<Workspace> {
        tx.execute(
            "INSERT INTO workspace (id, node_id, path, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(node_id, path) DO UPDATE SET created_at = excluded.created_at",
            params![new_id(), node_id, path, now],
        )?;
        Ok(tx.query_row(
            "SELECT id, node_id, path, created_at FROM workspace
             WHERE node_id = ?1 AND path = ?2",
            params![node_id, path],
            row_to_workspace,
        )?)
    }

    /// 按 `(node_id, path)` 取工作区。
    pub fn find(&self, node_id: &str, path: &str) -> Result<Option<Workspace>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, node_id, path, created_at FROM workspace
             WHERE node_id = ?1 AND path = ?2",
            params![node_id, path],
            row_to_workspace,
        )
        .optional()
        .map_err(StorageError::from)
    }

    /// 按 id 取工作区。
    pub fn by_id(&self, id: &str) -> Result<Option<Workspace>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, node_id, path, created_at FROM workspace WHERE id = ?1",
            params![id],
            row_to_workspace,
        )
        .optional()
        .map_err(StorageError::from)
    }

    /// 某台设备上的全部工作区。
    pub fn list_for_node(&self, node_id: &str) -> Result<Vec<Workspace>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, node_id, path, created_at FROM workspace
             WHERE node_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(params![node_id], row_to_workspace)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// 删工作区。
    ///
    /// **不是 `ON DELETE CASCADE`**：删之前显式查有没有会话指着它，有就拒
    /// （`WorkspaceInUse`）——「关掉最后一个会话才允许删工作区」这条业务规则
    /// 写在 schema 的外键动作里没人看得见，写在这里才看得见。
    pub fn delete(&self, id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let count = "SELECT count(*) FROM session WHERE workspace_id = ?1";
        let n: i64 = conn.query_row(count, params![id], |r| r.get(0))?;
        if n > 0 {
            return Err(StorageError::WorkspaceInUse(id.to_string()));
        }
        // 影响 0 行不是错误：工作区本来就不存在，删掉是幂等的
        conn.execute("DELETE FROM workspace WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 字面路径是否可作为工作区根。
    ///
    /// 绝不用 `Path::normalize`：`a/../b` 会被它悄悄改写成 `b`，而 Hub 要的是
    /// **原样物化**——否则用户输入的路径与设备上被打开的目录对不上，而 Hub
    /// 没有任何手段能发现这件事（约束 1）。
    pub fn validate_path(path: &str) -> Result<()> {
        if path.starts_with('/') || path.starts_with('~') {
            return Ok(());
        }
        Err(StorageError::InvalidWorkspacePath {
            path: path.to_string(),
            reason: ABSOLUTE_PATH_REASON,
        })
    }

    /// 子路径是否仍落在工作区根内（逻辑判据，不碰设备文件系统）。
    ///
    /// `.`/`..` 按**逻辑段**处理：连续的 `//` 与末尾 `/` 折叠，`a/./b` 等价
    /// `a/b`，而 `.`/`..` 段保留（要判它们）。这样 `/root/a/../..` 能被正确
    /// 判成越界，而 `/root/./a` 不会。
    pub fn validate_relative(rel: &str) -> Result<()> {
        let mut depth: i64 = 0;
        for seg in rel.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(StorageError::PathOutsideRoot {
                            path: rel.to_string(),
                            reason: OUTSIDE_ROOT_REASON,
                        });
                    }
                }
                _ => depth += 1,
            }
        }
        Ok(())
    }

    /// 字面路径 + 子路径的组合判据（开通道时 Hub 塞进 params 的根就是前一半）。
    pub fn validate_workspace_path(root: &str, rel: &str) -> Result<()> {
        Self::validate_path(root)?;
        Self::validate_relative(rel)
    }
}

/// 会话与工作区 id 的生成器。走 `identity::random_id()`（128 位随机 base64url），
/// 与 `device.id` 同一份实现：本仓只有一个 id 生成器，而 node_id 的语义
/// （绑定设备身份）不适用于工作区/会话。
pub(crate) fn new_id() -> String {
    identity::random_id()
}

pub(crate) fn row_to_workspace(
    r: &r2d2_sqlite::rusqlite::Row<'_>,
) -> std::result::Result<Workspace, r2d2_sqlite::rusqlite::Error> {
    row_to_workspace_at(r, 0)
}

/// 从 `offset` 列起读工作区的四列（联表查询里工作区排在会话之后时用）。
pub(crate) fn row_to_workspace_at(
    r: &r2d2_sqlite::rusqlite::Row<'_>,
    offset: usize,
) -> std::result::Result<Workspace, r2d2_sqlite::rusqlite::Error> {
    Ok(Workspace {
        id: r.get(offset)?,
        node_id: r.get(offset + 1)?,
        path: r.get(offset + 2)?,
        created_at: r.get(offset + 3)?,
    })
}

/// 给定时刻的工作区与它上面「最近一次创建」的会话，供 Hub 拼 API 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSession {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub session: crate::session::Session,
}

/// 列出工作区时按「每个工作区一个代表会话」组织：设计规范 §6.2 说工作区不是
/// 可获取的对象、只是会话上的字段，所以列表的基本单位是会话。
///
/// 代表规则 = 该工作区上创建时间最新的那个会话。它**不是**「还在跑的那个」：
/// 一个工作区可以同时开着好几个会话，而这条列表是给「我有哪些项目」用的概览，
/// 不是运行态监控（运行态在 SSE 的 `session.state`）。
/// 全删光会话的工作区不出现在这里——它是孤儿，`list_for_node` 才管得到。
pub fn recent_sessions(
    repo: &WorkspaceRepo,
    node_id: Option<&str>,
) -> Result<Vec<WorkspaceSession>> {
    let conn = repo.pool().get()?;
    let mut stmt = conn.prepare(
        "SELECT w.id, w.node_id, w.path, w.created_at,
                s.id, s.workspace_id, s.agent_id, s.state, s.created_at, s.ended_at
         FROM workspace w
         JOIN session s ON s.id = (
             SELECT s2.id FROM session s2
             WHERE s2.workspace_id = w.id
             ORDER BY s2.created_at DESC, s2.id DESC
             LIMIT 1)
         WHERE (?1 IS NULL OR w.node_id = ?1)
         ORDER BY w.created_at, w.id",
    )?;
    let rows = stmt.query_map(params![node_id], |r| {
        let raw = crate::session::RawSession::from_row(r, 4)?;
        Ok((row_to_workspace_at(r, 0)?, raw))
    })?;
    rows.map(|r| {
        let (workspace, raw) = r?;
        Ok(WorkspaceSession {
            session: raw.into_session()?,
            workspace,
        })
    })
    .collect::<Result<Vec<_>>>()
}

/// 一条工作台会话的列表项：会话 + 它的 `(node_id, path)`。
///
/// 设计规范 §6.2 的数据契约：`GET /api/sessions` 的每一项都带工作区，
/// 所以这个结构就是那个形状，不另开一个「列表专用视图」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    #[serde(flatten)]
    pub session: crate::session::Session,
    pub node_id: String,
    pub path: String,
}

/// 全部工作台会话（新的在前），各带工作区的 `(node_id, path)`。
pub fn list_sessions(repo: &WorkspaceRepo) -> Result<Vec<SessionListItem>> {
    let conn = repo.pool().get()?;
    let mut stmt = conn.prepare(
        "SELECT s.id, s.workspace_id, s.agent_id, s.state, s.created_at, s.ended_at,
                w.node_id, w.path
         FROM session s
         JOIN workspace w ON w.id = s.workspace_id
         ORDER BY s.created_at DESC, s.id DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let raw = crate::session::RawSession::from_row(r, 0)?;
        Ok((raw, r.get(6)?, r.get(7)?))
    })?;
    rows.map(|r| {
        let (raw, node_id, path) = r?;
        Ok(SessionListItem {
            session: raw.into_session()?,
            node_id,
            path,
        })
    })
    .collect::<Result<Vec<_>>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;
    use crate::repo::DeviceRepo;
    use crate::session::{SessionRepo, SessionState};

    const TS: &str = "unix:1700000000";

    /// 建两台设备：工作区必须挂在一台**真实存在**的设备上（外键是硬约束），
    /// 所以每个用例都从「有一台可挂的设备」开始。
    fn setup() -> (tempfile::TempDir, WorkspaceRepo, SessionRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        let conn = DeviceRepo::new(pool.clone()).pool().get().unwrap();
        for id in ["n_a", "n_b"] {
            conn.execute(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                 VALUES (?1, 'pi', ?2, 'unix:0')",
                params![id, format!("fp_{id}")],
            )
            .unwrap();
        }
        (d, WorkspaceRepo::new(pool.clone()), SessionRepo::new(pool))
    }

    fn create(sr: &SessionRepo, node: &str, path: &str, at: &str) -> crate::session::Session {
        sr.create(node, path, None, at).unwrap()
    }

    /// 建会话即挂载工作区：一次调用同时产生 workspace 与 session 两行，
    /// 且 session 指向的正是那个工作区。
    #[test]
    fn creating_a_session_mounts_its_workspace_in_one_call() {
        let (_d, _wr, sr) = setup();
        let s = sr
            .create("n_a", "~/src/app", Some("claude-code"), TS)
            .unwrap();
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(s.agent_id.as_deref(), Some("claude-code"));
        assert_eq!(s.ended_at, None);
        assert_eq!(s.created_at, TS);
    }

    /// `(node_id, path)` 唯一：同一工作区的第二个会话拿到**同一行工作区**，
    /// 而不是被 UNIQUE 挡掉。这是工作区作为主实体的全部意义所在。
    #[test]
    fn two_sessions_on_the_same_node_and_path_share_one_workspace() {
        let (_d, wr, sr) = setup();
        let a = create(&sr, "n_a", "/srv/app", TS);
        let b = create(&sr, "n_a", "/srv/app", "unix:1700000009");

        assert_eq!(a.workspace_id, b.workspace_id, "必须复用同一行");
        assert_ne!(a.id, b.id, "会话本身是两条");
        assert_eq!(wr.list_for_node("n_a").unwrap().len(), 1);
        assert_eq!(
            wr.find("n_a", "/srv/app").unwrap().unwrap().created_at,
            "unix:1700000009",
            "upsert 复用时刷新创建时间"
        );
    }

    /// 唯一约束的另一半：同一路径在**不同设备**上是两个工作区。
    #[test]
    fn the_same_path_on_two_nodes_is_two_workspaces() {
        let (_d, wr, sr) = setup();
        let a = create(&sr, "n_a", "/srv/app", TS);
        let b = create(&sr, "n_b", "/srv/app", TS);
        assert_ne!(a.workspace_id, b.workspace_id);
        assert_eq!(wr.list_for_node("n_a").unwrap().len(), 1);
        assert_eq!(wr.list_for_node("n_b").unwrap().len(), 1);
    }

    /// 删会话不删工作区：工作区独立于会话存在（M2 的 manifest 按 `(node_id, path)`
    /// 定位，它得比会话活得久）。
    #[test]
    fn deleting_the_last_session_keeps_the_workspace() {
        let (_d, wr, sr) = setup();
        let s = create(&sr, "n_a", "/srv/app", TS);
        {
            let conn = wr.pool().get().unwrap();
            conn.execute("DELETE FROM session WHERE id = ?1", params![s.id])
                .unwrap();
        }
        assert!(
            wr.by_id(&s.workspace_id).unwrap().is_some(),
            "删会话不得连带删工作区"
        );
        // 而且它还在下一次建会话时被复用，而不是变成第二行
        let again = create(&sr, "n_a", "/srv/app", TS);
        assert_eq!(again.workspace_id, s.workspace_id);
        assert_eq!(wr.list_for_node("n_a").unwrap().len(), 1);
    }

    #[test]
    fn delete_is_refused_while_a_session_still_points_at_the_workspace() {
        let (_d, wr, sr) = setup();
        let s = create(&sr, "n_a", "/srv/app", TS);
        let e = wr.delete(&s.workspace_id).unwrap_err();
        assert!(matches!(e, StorageError::WorkspaceInUse(_)), "got {e:?}");
        assert!(wr.by_id(&s.workspace_id).unwrap().is_some());
    }

    /// 已结束但仍占位的会话同样挡着删除：它还在列表里（`ended` 不是消失）。
    #[test]
    fn an_ended_session_still_holds_the_workspace() {
        let (_d, wr, sr) = setup();
        let s = create(&sr, "n_a", "/srv/app", TS);
        sr.end(&s.id, "unix:1700000100").unwrap();
        assert!(matches!(
            wr.delete(&s.workspace_id).unwrap_err(),
            StorageError::WorkspaceInUse(_)
        ));
    }

    /// 删掉没有会话的工作区；再删一次是幂等的（影响 0 行不是错误）。
    #[test]
    fn deleting_a_free_workspace_works_and_is_idempotent() {
        let (_d, wr, sr) = setup();
        let s = create(&sr, "n_a", "/srv/app", TS);
        {
            let conn = wr.pool().get().unwrap();
            conn.execute("DELETE FROM session WHERE id = ?1", params![s.id])
                .unwrap();
        }
        wr.delete(&s.workspace_id).unwrap();
        assert!(wr.by_id(&s.workspace_id).unwrap().is_none());
        wr.delete(&s.workspace_id).unwrap();
        wr.delete("ws_missing").unwrap();
    }

    /// 工作区必须挂在一台**真实存在**的设备上：外键是硬约束，否则
    /// 「按节点分组聚合」会出现一个页面点不进去的分组。
    #[test]
    fn a_workspace_cannot_reference_an_unknown_device() {
        let (_d, wr, sr) = setup();
        let err = sr.create("n_ghost", "/srv/app", None, TS).unwrap_err();
        assert!(
            matches!(err, StorageError::Sqlite(_)),
            "foreign key violation must surface as a sqlite error, got {err:?}"
        );
        // 整件事回滚：没有半截工作区留在库里
        assert!(wr.find("n_ghost", "/srv/app").unwrap().is_none());
        let n: i64 = wr
            .pool()
            .get()
            .unwrap()
            .query_row("SELECT count(*) FROM workspace", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "失败的事务不得留下工作区行");
    }

    #[test]
    fn find_by_node_and_path_and_by_id() {
        let (_d, wr, sr) = setup();
        let s = create(&sr, "n_a", "~/src/app", TS);
        assert_eq!(
            wr.find("n_a", "~/src/app").unwrap().unwrap().id,
            s.workspace_id
        );
        assert!(wr.find("n_a", "/other").unwrap().is_none());
        assert!(wr.find("n_b", "~/src/app").unwrap().is_none());
        assert!(wr.by_id("ws_missing").unwrap().is_none());
        assert!(wr.by_id(&s.workspace_id).unwrap().is_some());
    }

    /// 列表按创建时间排序：同一毫秒建的两行用 id 兜底，
    /// 否则顺序会随查询计划变，测试与页面都不可复现。
    #[test]
    fn list_for_node_is_ordered_by_creation() {
        let (_d, wr, sr) = setup();
        create(&sr, "n_a", "/c", "unix:30");
        create(&sr, "n_a", "/a", "unix:10");
        create(&sr, "n_a", "/b", "unix:20");
        let paths: Vec<String> = wr
            .list_for_node("n_a")
            .unwrap()
            .into_iter()
            .map(|w| w.path)
            .collect();
        assert_eq!(paths, vec!["/a", "/b", "/c"]);
        assert!(wr.list_for_node("n_b").unwrap().is_empty());
    }

    /// 同一时刻建的两个工作区：顺序仍由 id 兜底，不随查询计划变。
    #[test]
    fn workspaces_created_at_the_same_instant_have_a_stable_order() {
        let (_d, wr, sr) = setup();
        create(&sr, "n_a", "/p1", TS);
        create(&sr, "n_a", "/p2", TS);
        let paths = || -> Vec<String> {
            wr.list_for_node("n_a")
                .unwrap()
                .iter()
                .map(|w| w.path.clone())
                .collect()
        };
        assert_eq!(paths(), paths(), "同一时刻的两行顺序必须稳定");
        assert_eq!(paths().len(), 2);
    }

    /// 工作区根：绝对路径与 `~` 都合法，其余（相对路径、空串）拒。
    /// 相对路径不是不能用——它按设备上的登录目录解析——但那是另一个
    /// 入口（相对子路径），混进来会让同一个工作区有两个可能的位置。
    #[test]
    fn workspace_root_accepts_absolute_and_home_paths_only() {
        assert!(WorkspaceRepo::validate_path("/srv/app").is_ok());
        assert!(WorkspaceRepo::validate_path("~/src/app").is_ok());
        assert!(WorkspaceRepo::validate_path("~").is_ok());
        for bad in ["src/app", "", ".", "..", " /srv/app"] {
            let e = WorkspaceRepo::validate_path(bad).unwrap_err();
            assert!(
                matches!(e, StorageError::InvalidWorkspacePath { .. }),
                "{bad:?} must be refused, got {e:?}"
            );
        }
    }

    /// 子路径判据：`..` 不得越过根。每一条都是一种真实的构造，
    /// 不是同一句话的复述。
    #[test]
    fn relative_paths_may_wander_inside_the_root_but_never_out_of_it() {
        for ok in [
            "",
            ".",
            "src",
            "src/main.rs",
            "./src/main.rs",
            "a/./b",
            "a//b",
            "a/b/",
            "a/../b",
            "a/b/../c",
            "a/b/../../c",
            "..a",
            "a..",
            "...",
        ] {
            assert!(
                WorkspaceRepo::validate_relative(ok).is_ok(),
                "{ok:?} should stay inside"
            );
        }
        for bad in [
            "..",
            "../etc",
            "../..",
            "a/../..",
            "a/b/../../..",
            "a/../../b",
        ] {
            let e = WorkspaceRepo::validate_relative(bad).unwrap_err();
            assert!(
                matches!(e, StorageError::PathOutsideRoot { .. }),
                "{bad:?} must be refused, got {e:?}"
            );
        }
    }

    /// 被拒的错误要**页面看得见**那串用户给的路径和那条规则——
    /// 页面上显示的就是 `Display` 的结果，所以直接断言渲染结果，
    /// 免得「错误变体对了但理由是空的」这种漏网。
    #[test]
    fn refusals_name_the_offending_path_and_the_rule() {
        let err = WorkspaceRepo::validate_relative("../etc/passwd").unwrap_err();
        assert!(
            matches!(err, StorageError::PathOutsideRoot { .. }),
            "must be PathOutsideRoot, got {err:?}"
        );
        let shown = err.to_string();
        assert!(shown.contains("\"../etc/passwd\""), "{shown}");
        assert!(shown.contains("workspace root"), "{shown}");

        let err = WorkspaceRepo::validate_path("relative/path").unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidWorkspacePath { .. }),
            "must be InvalidWorkspacePath, got {err:?}"
        );
        let shown = err.to_string();
        assert!(shown.contains("\"relative/path\""), "{shown}");
        assert!(shown.contains('\''), "{shown}");
    }

    /// 组合判据：根合法 + 子路径合法才放行。任一不合法都拒。
    #[test]
    fn combined_validation_checks_both_halves() {
        assert!(WorkspaceRepo::validate_workspace_path("/srv/app", "src/main.rs").is_ok());
        assert!(WorkspaceRepo::validate_workspace_path("relative", "src").is_err());
        assert!(WorkspaceRepo::validate_workspace_path("/srv/app", "../etc").is_err());
    }

    /// `GET /api/sessions` 的形状：每项都带工作区的 `(node_id, path)`。
    #[test]
    fn session_list_items_carry_their_node_and_path() {
        let (_d, wr, sr) = setup();
        let a = create(&sr, "n_a", "/srv/app", "unix:10");
        let b = create(&sr, "n_b", "/opt/lib", "unix:20");

        let items = list_sessions(&wr).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].session.id, b.id, "新建的在前");
        assert_eq!(items[1].session.id, a.id);
        assert_eq!(
            (items[0].node_id.as_str(), items[0].path.as_str()),
            ("n_b", "/opt/lib")
        );
        assert_eq!(
            (items[1].node_id.as_str(), items[1].path.as_str()),
            ("n_a", "/srv/app")
        );
        assert_eq!(items[1].session.state, SessionState::Running);
    }

    /// 一个工作区开着两个会话时，列表给的是**每个工作区一项**（代表会话），
    /// 不是每个会话一项——否则同一个项目会重复出现，页面无从下手。
    #[test]
    fn a_workspace_with_two_sessions_appears_once_with_its_newest() {
        let (_d, wr, sr) = setup();
        let old = create(&sr, "n_a", "/srv/app", "unix:10");
        let new = create(&sr, "n_a", "/srv/app", "unix:20");
        let other = create(&sr, "n_b", "/opt/lib", "unix:15");

        let rows = recent_sessions(&wr, None).unwrap();
        assert_eq!(rows.len(), 2, "两个工作区 = 两项");
        let app = rows
            .iter()
            .find(|r| r.workspace.path == "/srv/app")
            .unwrap();
        assert_eq!(app.session.id, new.id, "代表会话取最新的那个");
        assert_ne!(app.session.id, old.id);
        assert_eq!(app.workspace.node_id, "n_a");
        assert_eq!(app.session.workspace_id, app.workspace.id);
        let lib = rows
            .iter()
            .find(|r| r.workspace.path == "/opt/lib")
            .unwrap();
        assert_eq!(lib.session.id, other.id);
    }

    /// 同一时刻建的两个会话也要有一个**确定**的代表。
    ///
    /// 注意断言的是「反复查同一个」而不是「某一条」：SQLite 对并列排序键
    /// 保持扫描顺序，所以哪一条当代表是查询计划的性质；契约只要求它稳定
    /// （页面与测试都不能每次刷新换一个项目）。
    #[test]
    fn the_representative_session_is_determined_even_at_the_same_instant() {
        let (_d, wr, sr) = setup();
        let first = create(&sr, "n_a", "/srv/app", TS);
        let second = create(&sr, "n_a", "/srv/app", TS);
        let rows = recent_sessions(&wr, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.workspace_id, first.workspace_id);
        let chosen = rows[0].session.id.clone();
        assert!(
            chosen == first.id || chosen == second.id,
            "代表必须是这两条之一，实际 {chosen}"
        );
        for _ in 0..4 {
            assert_eq!(recent_sessions(&wr, None).unwrap()[0].session.id, chosen);
        }
    }

    /// 按节点过滤：工作区概览是给「这台设备开了哪些目录」用的。
    #[test]
    fn recent_sessions_can_be_scoped_to_one_node() {
        let (_d, wr, sr) = setup();
        create(&sr, "n_a", "/srv/app", "unix:10");
        create(&sr, "n_b", "/opt/lib", "unix:20");

        let only_a = recent_sessions(&wr, Some("n_a")).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].workspace.node_id, "n_a");
        assert_eq!(only_a[0].workspace.created_at, "unix:10");
        assert!(recent_sessions(&wr, Some("n_ghost")).unwrap().is_empty());
    }

    /// 会话全删光的工作区不再出现在列表里（它是孤儿，概览不该显示一个
    /// 点进去什么都没有的条目），但仍在 `list_for_node` 里可查、可删。
    #[test]
    fn a_workspace_with_no_sessions_drops_out_of_the_list() {
        let (_d, wr, sr) = setup();
        let s = create(&sr, "n_a", "/srv/app", TS);
        {
            let conn = wr.pool().get().unwrap();
            conn.execute("DELETE FROM session WHERE id = ?1", params![s.id])
                .unwrap();
        }
        assert!(recent_sessions(&wr, None).unwrap().is_empty());
        assert_eq!(wr.list_for_node("n_a").unwrap().len(), 1);
        wr.delete(&s.workspace_id).unwrap();
    }
}
