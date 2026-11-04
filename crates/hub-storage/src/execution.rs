//! 执行仓储：一次「把这个任务放到某台设备上跑」。
//!
//! # `base_commit` 是起点唯一真相源
//!
//! v0.6.0 的交接规则：`execution.base_commit` 是起点唯一真相源，
//! `session.base_rev` 降级为只读快照。判据很直接——**任何一处把起点当权威读，
//! 都必须读本表的 `base_commit`**，不从 session 读。
//!
//! 这条规则要挡住的具体错误是：页面显示"这次执行从 X 开始"，而 X 取自
//! `session.base_rev`（那是一个可能为空、可能过期的快照）。两个值不一致时
//! 以 execution 为准，且 session 那一个不该再被写。
//!
//! # `Task : Execution = 1:N`
//!
//! 同一个任务可以跑多次：失败重试、换设备重跑、同一个验收跑三台机对比。
//! 每次执行各有自己的起点与 worktree，互不影响。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::Db;

/// 执行的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionState {
    /// 已派发但设备还没开工（worktree 可能还没建）。
    Pending,
    /// 设备侧在跑。
    Running,
    /// 跑完了，验收待判。
    Succeeded,
    /// 跑完了，但失败（原因见轨迹，不在本表）。
    Failed,
    /// 被中止（用户取消 / 设备掉线 / 换了设备重跑）。**不是失败**——
    /// 「我们主动停的」与「它自己挂的」在失败归因里是两件事。
    Aborted,
}

impl ExecutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Aborted)
    }
}

impl FromStr for ExecutionState {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "aborted" => Ok(Self::Aborted),
            other => Err(StorageError::InvalidExecutionState(other.to_string())),
        }
    }
}

/// 一次执行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub task_id: String,
    pub binding_id: String,
    /// **起点唯一真相源**。`None` 只在隔离式且设备还没报起点时出现。
    pub base_commit: Option<String>,
    /// 设备上的绝对路径。`None` = worktree 还没建。
    pub worktree_path: Option<String>,
    pub state: ExecutionState,
    pub created_at: String,
}

/// 建执行的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewExecution {
    pub task_id: String,
    pub binding_id: String,
    pub base_commit: Option<String>,
    pub worktree_path: Option<String>,
}

/// 执行仓储。
#[derive(Clone)]
pub struct ExecutionRepo {
    pool: Db,
}

impl ExecutionRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 建执行。状态恒为 `pending`。
    pub fn create(&self, new: NewExecution, now: &str) -> Result<Execution> {
        let conn = self.pool.get()?;
        let id = crate::workspace::new_id();
        conn.execute(
            "INSERT INTO execution
                 (id, task_id, binding_id, base_commit, worktree_path, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
            params![
                id,
                new.task_id,
                new.binding_id,
                new.base_commit,
                new.worktree_path,
                now
            ],
        )?;
        let raw = conn.query_row(
            &format!("{SELECT_COLS} WHERE id = ?1"),
            params![id],
            row_to_raw,
        )?;
        raw.into_execution()
    }

    pub fn get(&self, id: &str) -> Result<Option<Execution>> {
        let conn = self.pool.get()?;
        let raw = conn
            .query_row(
                &format!("{SELECT_COLS} WHERE id = ?1"),
                params![id],
                row_to_raw,
            )
            .optional()?;
        raw.map(RawExecution::into_execution).transpose()
    }

    /// 一个任务的全部执行，最近建的在前。
    ///
    /// 看板的"这个任务跑过几次"与设备页的失败归因都走这里。
    pub fn list_for_task(&self, task_id: &str) -> Result<Vec<Execution>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE task_id = ?1 ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![task_id], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawExecution::into_execution).collect()
    }

    /// 一个绑定上的全部执行（设备页的配额与失败归因）。
    pub fn list_for_binding(&self, binding_id: &str) -> Result<Vec<Execution>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE binding_id = ?1 ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![binding_id], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawExecution::into_execution).collect()
    }

    /// 改状态。任务必须存在，否则静默作用在 0 行上。
    pub fn set_state(&self, id: &str, state: ExecutionState) -> Result<Execution> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE execution SET state = ?2 WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        if n == 0 {
            return Err(StorageError::ExecutionNotFound { id: id.to_string() });
        }
        self.get(id)?
            .ok_or_else(|| StorageError::ExecutionNotFound { id: id.to_string() })
    }

    /// 回填起点。
    ///
    /// 只在"派发时拿不到起点、设备后来报了"这条路径上调用。**写进去之后就
    /// 不再改**：起点是这次执行的历史事实，中途改写会让 diff 的语义变掉。
    pub fn set_base_commit(&self, id: &str, base_commit: &str) -> Result<Execution> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE execution SET base_commit = ?2 WHERE id = ?1 AND base_commit IS NULL",
            params![id, base_commit],
        )?;
        if n == 0 {
            // 要么不存在、要么已经有起点了。后者是幂等成功（重复上报同一起点），
            // 但"已经有别的起点"必须让人知道——静默成功会让两次起点不一致
            // 永远不被发现。
            let current = self
                .get(id)?
                .ok_or_else(|| StorageError::ExecutionNotFound { id: id.to_string() })?;
            if current.base_commit.as_deref() != Some(base_commit) {
                return Err(StorageError::BaseCommitAlreadySet {
                    id: id.to_string(),
                    existing: current.base_commit.unwrap_or_default(),
                });
            }
            return Ok(current);
        }
        self.get(id)?
            .ok_or_else(|| StorageError::ExecutionNotFound { id: id.to_string() })
    }
}

const SELECT_COLS: &str = "SELECT id, task_id, binding_id, base_commit, worktree_path,
        state, created_at FROM execution";

struct RawExecution {
    id: String,
    task_id: String,
    binding_id: String,
    base_commit: Option<String>,
    worktree_path: Option<String>,
    state: String,
    created_at: String,
}

impl RawExecution {
    fn into_execution(self) -> Result<Execution> {
        Ok(Execution {
            id: self.id,
            task_id: self.task_id,
            binding_id: self.binding_id,
            base_commit: self.base_commit,
            worktree_path: self.worktree_path,
            state: ExecutionState::from_str(&self.state)?,
            created_at: self.created_at,
        })
    }
}

fn row_to_raw(r: &Row<'_>) -> std::result::Result<RawExecution, r2d2_sqlite::rusqlite::Error> {
    Ok(RawExecution {
        id: r.get(0)?,
        task_id: r.get(1)?,
        binding_id: r.get(2)?,
        base_commit: r.get(3)?,
        worktree_path: r.get(4)?,
        state: r.get(5)?,
        created_at: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";
    const REV: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    fn setup() -> (tempfile::TempDir, ExecutionRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(
                "INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_a', 'app', 'central', 'unix:0');
                 INSERT INTO binding
                     (id, project_id, device_id, fixed_dir, cleanup_policy, created_at)
                     VALUES ('b_a', 'p_a', 'n_a', '/srv/app', 'balanced', 'unix:0');
                 INSERT INTO binding
                     (id, project_id, device_id, fixed_dir, cleanup_policy, created_at)
                     VALUES ('b_b', 'p_a', 'n_b', '/srv/b', 'balanced', 'unix:0');
                 INSERT INTO task (id, project_id, title, state, created_at, updated_at)
                     VALUES ('t_a', 'p_a', 'do it', 'ready', 'unix:0', 'unix:0');",
            )
            .unwrap();
        (d, ExecutionRepo::new(pool))
    }

    fn new_exec() -> NewExecution {
        NewExecution {
            task_id: "t_a".into(),
            binding_id: "b_a".into(),
            base_commit: Some(REV.into()),
            worktree_path: Some("/srv/app/wt-1".into()),
        }
    }

    #[test]
    fn create_round_trips_every_field() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();
        assert_eq!(e.task_id, "t_a");
        assert_eq!(e.binding_id, "b_a");
        assert_eq!(e.base_commit.as_deref(), Some(REV));
        assert_eq!(e.worktree_path.as_deref(), Some("/srv/app/wt-1"));
        assert_eq!(e.state, ExecutionState::Pending, "新建恒 pending");
        assert_eq!(e.created_at, TS);

        let f = er.get(&e.id).unwrap().unwrap();
        assert_eq!(f.id, e.id);
        assert_eq!(f.base_commit.as_deref(), Some(REV));
    }

    /// `Task : Execution = 1:N`——同一个任务可以跑多次。
    #[test]
    fn one_task_can_have_several_executions() {
        let (_d, er) = setup();
        let a = er.create(new_exec(), "unix:10").unwrap();
        let b = er
            .create(
                NewExecution {
                    binding_id: "b_b".into(),
                    ..new_exec()
                },
                "unix:20",
            )
            .unwrap();
        assert_ne!(a.id, b.id);
        let list = er.list_for_task("t_a").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, b.id, "新的在前");
    }

    #[test]
    fn list_for_binding_is_scoped() {
        let (_d, er) = setup();
        let a = er.create(new_exec(), TS).unwrap();
        er.create(
            NewExecution {
                binding_id: "b_b".into(),
                ..new_exec()
            },
            TS,
        )
        .unwrap();
        let list = er.list_for_binding("b_a").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, a.id);
    }

    #[test]
    fn set_state_checks_existence() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();
        let r = er.set_state(&e.id, ExecutionState::Running).unwrap();
        assert_eq!(r.state, ExecutionState::Running);
        let e = er.set_state("nope", ExecutionState::Running).unwrap_err();
        assert!(matches!(e, StorageError::ExecutionNotFound { .. }), "{e:?}");
    }

    /// **起点只填一次**：设备后报上来时可以补 `NULL`，但已有的值不被覆盖。
    #[test]
    fn base_commit_is_backfilled_once_and_never_overwritten() {
        let (_d, er) = setup();
        let e = er
            .create(
                NewExecution {
                    base_commit: None,
                    ..new_exec()
                },
                TS,
            )
            .unwrap();
        assert_eq!(e.base_commit, None);

        let filled = er.set_base_commit(&e.id, REV).unwrap();
        assert_eq!(filled.base_commit.as_deref(), Some(REV));

        // 重复上报同一个起点是幂等成功。
        let again = er.set_base_commit(&e.id, REV).unwrap();
        assert_eq!(again.base_commit.as_deref(), Some(REV));

        // 换一个不同的起点必须被拒——那是要修的问题，不是能盖过去的事。
        let other = "cafecafecafecafecafecafecafecafecafecafe";
        let err = er.set_base_commit(&e.id, other).unwrap_err();
        assert!(
            matches!(err, StorageError::BaseCommitAlreadySet { .. }),
            "{err:?}"
        );
        assert_eq!(
            er.get(&e.id).unwrap().unwrap().base_commit.as_deref(),
            Some(REV),
            "被拒之后起点不得变"
        );
    }

    #[test]
    fn set_base_commit_on_a_missing_execution_is_named() {
        let (_d, er) = setup();
        let e = er.set_base_commit("nope", REV).unwrap_err();
        assert!(matches!(e, StorageError::ExecutionNotFound { .. }), "{e:?}");
    }

    /// `aborted` 与 `failed` 是两件事，终态集合要包含两者。
    #[test]
    fn terminal_states_include_aborted_but_not_pending_or_running() {
        assert!(ExecutionState::Succeeded.is_terminal());
        assert!(ExecutionState::Failed.is_terminal());
        assert!(ExecutionState::Aborted.is_terminal());
        assert!(!ExecutionState::Pending.is_terminal());
        assert!(!ExecutionState::Running.is_terminal());
    }

    #[test]
    fn an_unknown_state_in_the_database_is_an_error_not_a_default() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();
        er.pool
            .get()
            .unwrap()
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE execution SET state = 'almost_done';",
            )
            .unwrap();
        let err = er.get(&e.id).unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidExecutionState(_)),
            "{err:?}"
        );
    }

    #[test]
    fn every_state_round_trips_through_its_literal() {
        for s in [
            ExecutionState::Pending,
            ExecutionState::Running,
            ExecutionState::Succeeded,
            ExecutionState::Failed,
            ExecutionState::Aborted,
        ] {
            assert_eq!(ExecutionState::from_str(s.as_str()).unwrap(), s);
        }
    }
}
