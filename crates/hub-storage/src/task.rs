//! 任务仓储：看板上的一张卡片。
//!
//! 六态见 `task-model.md` §2。**`state` 的取值校验在 Rust 侧**，因为
//! 「哪些态之间能转」本来就是业务规则，而 SQLite 的 `CHECK` 只能表达"取值属于
//! 这个集合"，无法表达"`done` 不能直接回到 `draft`"这类转移约束。把校验拆成
//! 两处只会让"什么合法"有两个答案（`schema_v5.sql` 里写了同一个理由）。
//!
//! # `base_commit` 的两份，不要混
//!
//! - `task.base_commit`：**默认起点**，给"还没派发时页面显示什么"用。
//! - `execution.base_commit`：**唯一真相源**（v0.6.0 交接规则）。
//!
//! 任何一处把起点当权威读，都必须读 execution 的那一份。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::Db;

/// 任务六态（`task-model.md` §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    /// 刚建，还没准备好派发（缺验收标准、缺约束）。
    Draft,
    /// 可派发，等设备/调度。
    Ready,
    /// 有执行在跑。
    Running,
    /// 有人在等：等用户决策、等外部依赖。
    Blocked,
    /// 验收通过。
    Done,
    /// 最后一次执行失败，且已不再重试。
    Failed,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// 这个态是不是「已经结束」。看板用它决定卡片放在"进行中"还是"归档列"。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

impl FromStr for TaskState {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "draft" => Ok(Self::Draft),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "blocked" => Ok(Self::Blocked),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            // 不回落成 `Draft`：把一个已结束的任务显示成草稿，会让用户以为
            // 它从没跑过。
            other => Err(StorageError::InvalidTaskState(other.to_string())),
        }
    }
}

/// 验收标准：**判定码 + 给人看的一句话**（D23）。
///
/// 两列而不是一段自由文本：`criteria` 是机器判的（`all_tests_pass` 等），
/// `description` 是给人读的。合成一段文本会让"按判定码统计哪种验收最常失败"
/// 变成一次自然语言解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneWhen {
    /// 判定码。`None` = 还没定（`draft` 态的正常情况）。
    pub criteria: Option<String>,
    /// 一句话说明。`None` = 还没写。
    pub description: Option<String>,
}

/// 一个任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub state: TaskState,
    pub done_when: DoneWhen,
    /// 谁建的（`user` / `butler` / `external:github` …）。`None` = 早期数据。
    pub created_by: Option<String>,
    /// **默认起点**，不是唯一真相源——见模块文档。
    pub base_commit: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 建任务的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub project_id: String,
    pub title: String,
    pub done_when: DoneWhen,
    pub created_by: Option<String>,
    pub base_commit: Option<String>,
}

/// 任务仓储。
#[derive(Clone)]
pub struct TaskRepo {
    pool: Db,
}

impl TaskRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 建任务。状态恒为 `draft`——「建好就能派发」会让一个缺验收标准的任务
    /// 直接进调度候选集，而那是最常见的失败来源。
    pub fn create(&self, new: NewTask, now: &str) -> Result<Task> {
        let conn = self.pool.get()?;
        let id = crate::workspace::new_id();
        conn.execute(
            "INSERT INTO task
                 (id, project_id, title, state, done_when_criteria, done_when_description,
                  created_by, base_commit, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'draft', ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                id,
                new.project_id,
                new.title,
                new.done_when.criteria,
                new.done_when.description,
                new.created_by,
                new.base_commit,
                now
            ],
        )?;
        let raw = conn.query_row(
            &format!("{SELECT_COLS} WHERE id = ?1"),
            params![id],
            row_to_raw,
        )?;
        raw.into_task()
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>> {
        let conn = self.pool.get()?;
        let raw = conn
            .query_row(
                &format!("{SELECT_COLS} WHERE id = ?1"),
                params![id],
                row_to_raw,
            )
            .optional()?;
        raw.map(RawTask::into_task).transpose()
    }

    /// 一个项目的全部任务，最近建的在前。
    pub fn list_for_project(&self, project_id: &str) -> Result<Vec<Task>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE project_id = ?1 ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![project_id], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawTask::into_task).collect()
    }

    /// 某态的全部任务（调度候选集的入口）。
    pub fn list_by_state(&self, state: TaskState) -> Result<Vec<Task>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "{SELECT_COLS} WHERE state = ?1 ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(params![state.as_str()], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawTask::into_task).collect()
    }

    /// 改状态。
    ///
    /// **不校验转移合法性**：六态之间哪些转移合法是 `v0.8.0`（任务与执行）的
    /// 内容，本里程碑只把状态列立起来。这里能做的是"取值必须合法"与"任务必须
    /// 存在"——后者不做的话，状态更新会静默作用在 0 行上。
    pub fn set_state(&self, id: &str, state: TaskState, now: &str) -> Result<Task> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE task SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state.as_str(), now],
        )?;
        if n == 0 {
            return Err(StorageError::TaskNotFound { id: id.to_string() });
        }
        self.get(id)?
            .ok_or_else(|| StorageError::TaskNotFound { id: id.to_string() })
    }
}

const SELECT_COLS: &str = "SELECT id, project_id, title, state, done_when_criteria,
        done_when_description, created_by, base_commit, created_at, updated_at FROM task";

struct RawTask {
    id: String,
    project_id: String,
    title: String,
    state: String,
    done_when_criteria: Option<String>,
    done_when_description: Option<String>,
    created_by: Option<String>,
    base_commit: Option<String>,
    created_at: String,
    updated_at: String,
}

impl RawTask {
    fn into_task(self) -> Result<Task> {
        Ok(Task {
            id: self.id,
            project_id: self.project_id,
            title: self.title,
            state: TaskState::from_str(&self.state)?,
            done_when: DoneWhen {
                criteria: self.done_when_criteria,
                description: self.done_when_description,
            },
            created_by: self.created_by,
            base_commit: self.base_commit,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn row_to_raw(r: &Row<'_>) -> std::result::Result<RawTask, r2d2_sqlite::rusqlite::Error> {
    Ok(RawTask {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        state: r.get(3)?,
        done_when_criteria: r.get(4)?,
        done_when_description: r.get(5)?,
        created_by: r.get(6)?,
        base_commit: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, TaskRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        pool.get()
            .unwrap()
            .execute_batch(
                "INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_a', 'app', 'central', 'unix:0');
                 INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_b', 'other', 'central', 'unix:0');",
            )
            .unwrap();
        (d, TaskRepo::new(pool))
    }

    fn new_task(project: &str) -> NewTask {
        NewTask {
            project_id: project.into(),
            title: "把登录改成 OAuth".into(),
            done_when: DoneWhen {
                criteria: Some("all_tests_pass".into()),
                description: Some("全部测试通过且登录可用".into()),
            },
            created_by: Some("user".into()),
            base_commit: None,
        }
    }

    #[test]
    fn create_round_trips_every_field() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        assert_eq!(t.project_id, "p_a");
        assert_eq!(t.title, "把登录改成 OAuth");
        assert_eq!(t.state, TaskState::Draft, "新任务一律 draft");
        assert_eq!(t.done_when.criteria.as_deref(), Some("all_tests_pass"));
        assert_eq!(
            t.done_when.description.as_deref(),
            Some("全部测试通过且登录可用")
        );
        assert_eq!(t.created_by.as_deref(), Some("user"));
        assert_eq!(t.created_at, TS);
        assert_eq!(t.updated_at, TS);

        let f = tr.get(&t.id).unwrap().unwrap();
        assert_eq!(f.id, t.id);
        assert_eq!(f.done_when, t.done_when);
    }

    /// **新建恒为 `draft`**：「建好就能派发」会让缺验收标准的任务直接进候选集。
    #[test]
    fn a_fresh_task_never_starts_ready() {
        let (_d, tr) = setup();
        let t = tr
            .create(
                NewTask {
                    project_id: "p_a".into(),
                    title: "x".into(),
                    done_when: DoneWhen {
                        criteria: None,
                        description: None,
                    },
                    created_by: None,
                    base_commit: None,
                },
                TS,
            )
            .unwrap();
        assert_eq!(t.state, TaskState::Draft);
        assert_eq!(t.done_when.criteria, None);
    }

    #[test]
    fn set_state_stamps_updated_at_and_checks_existence() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        let r = tr.set_state(&t.id, TaskState::Ready, "unix:9999").unwrap();
        assert_eq!(r.state, TaskState::Ready);
        assert_eq!(r.updated_at, "unix:9999");
        assert_eq!(r.created_at, TS, "created_at 不动");

        let e = tr.set_state("nope", TaskState::Ready, TS).unwrap_err();
        assert!(matches!(e, StorageError::TaskNotFound { .. }), "{e:?}");
    }

    #[test]
    fn list_for_project_is_scoped_and_newest_first() {
        let (_d, tr) = setup();
        let a = tr.create(new_task("p_a"), "unix:10").unwrap();
        let b = tr.create(new_task("p_a"), "unix:20").unwrap();
        tr.create(new_task("p_b"), TS).unwrap();

        let got: Vec<String> = tr
            .list_for_project("p_a")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(got, vec![b.id, a.id], "只含本项目，且新的在前");
    }

    /// `list_by_state`：派发候选集的入口。
    #[test]
    fn list_by_state_filters_on_the_state() {
        let (_d, tr) = setup();
        let a = tr.create(new_task("p_a"), TS).unwrap();
        let b = tr.create(new_task("p_a"), "unix:11").unwrap();
        tr.set_state(&b.id, TaskState::Ready, TS).unwrap();

        let draft = tr.list_by_state(TaskState::Draft).unwrap();
        assert_eq!(draft.len(), 1);
        assert_eq!(draft[0].id, a.id);

        let ready = tr.list_by_state(TaskState::Ready).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, b.id);
    }

    /// `is_terminal` 只对 `Done` / `Failed` 为真。
    #[test]
    fn only_done_and_failed_are_terminal() {
        assert!(TaskState::Done.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        for s in [
            TaskState::Draft,
            TaskState::Ready,
            TaskState::Running,
            TaskState::Blocked,
        ] {
            assert!(!s.is_terminal(), "{s:?} 不该是终态");
        }
    }

    /// 库里的非法状态必须报错，**不回落 `Draft`**——回落会让一个已结束的任务
    /// 显示成草稿，用户会以为它从没跑过。
    #[test]
    fn an_unknown_state_in_the_database_is_an_error_not_a_default() {
        let (_d, tr) = setup();
        let t = tr.create(new_task("p_a"), TS).unwrap();
        tr.pool
            .get()
            .unwrap()
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE task SET state = 'sorta_done';",
            )
            .unwrap();
        let e = tr.get(&t.id).unwrap_err();
        assert!(matches!(e, StorageError::InvalidTaskState(_)), "{e:?}");
    }

    #[test]
    fn every_state_round_trips_through_its_literal() {
        for s in [
            TaskState::Draft,
            TaskState::Ready,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Failed,
        ] {
            assert_eq!(TaskState::from_str(s.as_str()).unwrap(), s);
        }
    }
}
