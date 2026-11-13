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

/// 一次执行失败属于哪一种（`task-model.md:126-135` 那张表）。
///
/// **八种，不是七种**：前七条是「哪里出了毛病」，第八条 `superseded` 是
/// 「不是失败的失败」——需求变了、任务被新任务取代，它不是缺陷。
///
/// **字符串值必须与规格表第一列逐字一致**：它们是缺陷归因统计的键
/// （「失败集中在哪一类」按它分组），改名等于换语义，而且统计会把老数据
/// 当成另一类。同理**不做 `snake_case` 自动转换**、不吃别名——见
/// [`FromStr`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    /// 1 设备上起点不是期望 commit / 环境声明不符（`task-model.md:128`）
    BaselineMismatch,
    /// 2 依赖装不上、工具链缺失、系统库不符（`task-model.md:129`）
    EnvBroken,
    /// 2.5 执行中磁盘写满 / 超预算被中止（`task-model.md:130`）
    DiskExhausted,
    /// 3 agent 崩了、超时、协议错、上下文爆了（`task-model.md:131`）
    AgentError,
    /// 4 agent 正常但事情没做成（测试红、构建失败）（`task-model.md:132`）
    TaskFailed,
    /// 5 设备掉线 / Hub 重启 / 任务被取消（`task-model.md:133`）
    Interrupted,
    /// 6 派不出去：无合格设备、隔离策略、配额不足（`task-model.md:134`）
    Rejected,
    /// 7 「不是失败的失败」：需求变了，被新任务取代（`task-model.md:135`）。
    /// **不占重试次数、不触发重试、不进失败归因统计**（`task-model.md:138`）。
    Superseded,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BaselineMismatch => "baseline_mismatch",
            Self::EnvBroken => "env_broken",
            Self::DiskExhausted => "disk_exhausted",
            Self::AgentError => "agent_error",
            Self::TaskFailed => "task_failed",
            Self::Interrupted => "interrupted",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
        }
    }

    /// 这一次「失败」该不该计入重试次数。
    ///
    /// `superseded` 为 `false`——它是「需求变了」，不是「没做成」。
    /// 算成一次重试的后果很具体：用户每改一次需求就吃掉一格重试额度，
    /// 撞上 `retry_limit` 之后任务彻底失败，而这正是 `task-model.md:113-117`
    /// 明令避免的事。
    ///
    /// **判定只写在这里**：不要在调用方再写一遍 `!= Superseded`，
    /// 写两处迟早不一致（一处加了特判、另一处没加）。
    ///
    /// 不接受 `&self` 之外的形态是刻意的：`FailureKind` 是 `Copy` 的小枚举，
    /// 传值比拼引用短，且 `kind.counts_as_retry()` 读起来就是一句判断。
    pub fn counts_as_retry(self) -> bool {
        !matches!(self, Self::Superseded)
    }
}

impl FromStr for FailureKind {
    type Err = StorageError;

    /// 只认八个规格字符串，**非法值报错不回落**。
    ///
    /// 不回落成 `AgentError` 之类的「最接近的那个」：那会把「上游送来了一个
    /// 我们不认识的名字」伪装成一个具体的失败形态，缺陷归因里凭空多出一批
    /// `agent_error`，而真正的问题（拼错的字符串 / 新形态没同步）永远不出现。
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "baseline_mismatch" => Ok(Self::BaselineMismatch),
            "env_broken" => Ok(Self::EnvBroken),
            "disk_exhausted" => Ok(Self::DiskExhausted),
            "agent_error" => Ok(Self::AgentError),
            "task_failed" => Ok(Self::TaskFailed),
            "interrupted" => Ok(Self::Interrupted),
            "rejected" => Ok(Self::Rejected),
            "superseded" => Ok(Self::Superseded),
            other => Err(StorageError::InvalidFailureKind(other.to_string())),
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

    /// 记一次失败：`failure_kind` / `reason` / `evidence` 三列**同一条 `UPDATE`**。
    ///
    /// # 两条输入校验（都来自 `task-model.md:73-79`）
    ///
    /// - `reason` 为空 → 拒绝（[`StorageError::FailureReasonRequired`]）。
    ///   规格原话「没有原因串的失败等于没发生」。判定用 `trim()` 后为空——
    ///   一串空格与空串在「什么都没说」这一点上没有区别。
    /// - `evidence` 为空 → **写成字面量 `'none'`**，不是空串。
    ///   规格：「可空但空要显式标 `none`」。写空串的后果是「有这一列但没内容」
    ///   与「根本没带 evidence 字段」在库里长得一样，失败留痕那一格就废了。
    ///
    /// 三列在同一条语句里写，不是三条：读到「kind 有了但 evidence 还是上一轮的」
    /// 那种半截状态没有任何意义，而失败记录正是要被人反复读的东西。
    pub fn set_failure(
        &self,
        id: &str,
        kind: FailureKind,
        reason: &str,
        evidence: &str,
    ) -> Result<()> {
        if reason.trim().is_empty() {
            return Err(StorageError::FailureReasonRequired);
        }
        // `'none'` 是**规格约定的字面量**（`task-model.md:79`），不是本函数的
        // 自造常量；前端也按这个字面值渲染成「无证据」。
        let evidence = if evidence.trim().is_empty() {
            "none"
        } else {
            evidence
        };
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE execution
                SET failure_kind = ?2, reason = ?3, evidence = ?4
              WHERE id = ?1",
            params![id, kind.as_str(), reason, evidence],
        )?;
        if n == 0 {
            return Err(StorageError::ExecutionNotFound { id: id.to_string() });
        }
        Ok(())
    }

    /// 记开始时间。**只填一次**：`WHERE started_at IS NULL`。
    ///
    /// 重复调用是幂等成功（不报错、不改首次值）。为什么必须保住首次值：
    /// 重试 / 重连会再报一次「开始」，若每次都覆盖，「这次跑了多久」就变成
    /// 「最后一次心跳到结束」，时长统计彻底失真。
    pub fn mark_started(&self, id: &str, at: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE execution SET started_at = ?2 WHERE id = ?1 AND started_at IS NULL",
            params![id, at],
        )?;
        if n == 0 {
            // 0 行有两种：不存在（要报错），或已经开始了（幂等成功）。
            return self.ensure_exists(id);
        }
        Ok(())
    }

    /// 记结束时间。与 [`Self::mark_started`] 同一条口径：`WHERE ended_at IS NULL`，
    /// 重复调用不改首次值。
    pub fn mark_ended(&self, id: &str, at: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE execution SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![id, at],
        )?;
        if n == 0 {
            return self.ensure_exists(id);
        }
        Ok(())
    }

    /// 幂等 setter 的收尾：影响 0 行时区分「不存在」与「已经写过」。
    ///
    /// 抽成一个函数是因为 `mark_started` / `mark_ended` 是同一个判断；
    /// 各写一遍迟早有一处漏掉存在性检查，于是打错 id 也静默成功。
    fn ensure_exists(&self, id: &str) -> Result<()> {
        match self.get(id)? {
            Some(_) => Ok(()),
            None => Err(StorageError::ExecutionNotFound { id: id.to_string() }),
        }
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
                     VALUES ('t_a', 'p_a', 'do it', 'pending', 'unix:0', 'unix:0');",
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

    // ---------------------------------------------------------------------
    // 子任务 5：`FailureKind` + 三个 setter
    // ---------------------------------------------------------------------

    /// 规格表第一列（`task-model.md:128-135`）的**逐字**快照。
    ///
    /// 单独抄一份的原因：下面几条用例要拿它做「代码 == 规格」的比对，
    /// 而比对的两边不能是同一个东西（拿 `as_str()` 和自己比恒真）。
    const SPEC_FAILURE_KINDS: [&str; 8] = [
        "baseline_mismatch",
        "env_broken",
        "disk_exhausted",
        "agent_error",
        "task_failed",
        "interrupted",
        "rejected",
        "superseded",
    ];

    /// 直接读库里失败相关的三列原始值（`Execution` 结构体还没有这三个字段）。
    fn raw_failure(
        er: &ExecutionRepo,
        id: &str,
    ) -> (Option<String>, Option<String>, Option<String>) {
        er.pool
            .get()
            .unwrap()
            .query_row(
                "SELECT failure_kind, reason, evidence FROM execution WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    fn raw_time(er: &ExecutionRepo, id: &str) -> (Option<String>, Option<String>) {
        er.pool
            .get()
            .unwrap()
            .query_row(
                "SELECT started_at, ended_at FROM execution WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    /// 八个规格字符串逐个 `FromStr` / `as_str` 往返，**逐字比对规格表**。
    ///
    /// 判红方式：把自造的 `test_failed` / `build_failed` 之类写进枚举
    /// （`as_str` 与规格表对不上），或 `FromStr` 少认一个值，这条立刻红。
    /// 把 `disk_exhausted` 写成 `disk_full` 也会红——它就是最容易记错的
    /// 那一个（规格里编号是 2.5，容易被合并掉）。
    #[test]
    fn failure_kind_round_trips_the_eight_values() {
        for spec in SPEC_FAILURE_KINDS {
            let kind = FailureKind::from_str(spec)
                .unwrap_or_else(|e| panic!("{spec} 是规格里的值，必须认得：{e:?}"));
            assert_eq!(kind.as_str(), spec, "as_str 必须与规格字符串逐字一致");
        }
        // 反向：枚举里不能有规格之外的值（`as_str` 的像都必须落在表里）。
        let mut seen: Vec<&str> = [
            FailureKind::BaselineMismatch,
            FailureKind::EnvBroken,
            FailureKind::DiskExhausted,
            FailureKind::AgentError,
            FailureKind::TaskFailed,
            FailureKind::Interrupted,
            FailureKind::Rejected,
            FailureKind::Superseded,
        ]
        .iter()
        .map(|k| k.as_str())
        .collect();
        seen.sort_unstable();
        let mut spec_sorted = SPEC_FAILURE_KINDS;
        spec_sorted.sort_unstable();
        assert_eq!(seen, spec_sorted, "枚举里的八条必须恰是规格表那八条");
    }

    /// 规格里没有的名字必须**报错**，不回落成「最接近的那个」。
    ///
    /// `"test_failure"` 是规格里不存在的名字（规格用的是 `task_failed`），
    /// 也正是最容易被顺手接受的一个。
    ///
    /// 判红方式：把 `FromStr` 的 `other` 分支改成 `Ok(Self::TaskFailed)`
    /// （回落），这条立刻红——而且缺陷归因里会凭空多出一批 `task_failed`。
    #[test]
    fn unknown_failure_kind_string_is_rejected() {
        for bad in [
            "test_failure",
            "test_failed",
            "build_failed",
            "",
            "Superseded",
        ] {
            let e =
                FailureKind::from_str(bad).expect_err(&format!("{bad:?} 不在规格八种里，必须报错"));
            assert!(
                matches!(e, StorageError::InvalidFailureKind(ref s) if s == bad),
                "错误要带上是哪串值被拒了，实际 {e:?}"
            );
        }
    }

    /// `superseded` 不计入重试，其余七种都计入。
    ///
    /// 判红方式：漏了这条特判（`counts_as_retry` 恒 `true`），这条立刻红
    /// ——用户每改一次需求就吃掉一格重试额度，撞上 `retry_limit` 后任务
    /// 彻底失败，而那正是 `task-model.md:113-117` 明令避免的事。
    /// 反向把别的种类也判成 `false` 同样红（会让真正的失败无限重试）。
    #[test]
    fn superseded_does_not_count_as_a_retry() {
        assert!(
            !FailureKind::Superseded.counts_as_retry(),
            "需求变了不是没做成，不占重试次数"
        );
        for kind in [
            FailureKind::BaselineMismatch,
            FailureKind::EnvBroken,
            FailureKind::DiskExhausted,
            FailureKind::AgentError,
            FailureKind::TaskFailed,
            FailureKind::Interrupted,
            FailureKind::Rejected,
        ] {
            assert!(
                kind.counts_as_retry(),
                "{:?} 是真正的失败，必须计入重试",
                kind
            );
        }
    }

    /// `kind` / `reason` / `evidence` 三列**同一条 `UPDATE`** 落库。
    ///
    /// 判红方式：只写 `kind` 忘了 `evidence`（或忘了 `reason`），断言红。
    #[test]
    fn set_failure_writes_all_four_columns() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();

        er.set_failure(
            &e.id,
            FailureKind::TaskFailed,
            "集成测试 3 条红",
            "exit code 1; see /srv/app/wt-1/.af/test.log",
        )
        .unwrap();

        assert_eq!(
            raw_failure(&er, &e.id),
            (
                Some("task_failed".to_string()),
                Some("集成测试 3 条红".to_string()),
                Some("exit code 1; see /srv/app/wt-1/.af/test.log".to_string()),
            ),
            "三列必须在同一条 UPDATE 里一起落库"
        );
    }

    /// `evidence: ""` → 库里是字面量 `'none'`，**不是空串**。
    ///
    /// 规格 `task-model.md:79`：「可空但**空要显式标 `none`**」。
    ///
    /// 判红方式：直接把空串写库，这条立刻红——此后「有 evidence 但为空」与
    /// 「根本没带 evidence」在库里长得一样，失败留痕那一格就废了。
    #[test]
    fn an_empty_evidence_is_written_as_the_literal_none() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();

        er.set_failure(&e.id, FailureKind::EnvBroken, "缺 libssl", "")
            .unwrap();
        let (_, _, evidence) = raw_failure(&er, &e.id);
        assert_eq!(
            evidence.as_deref(),
            Some("none"),
            "空 evidence 必须写成字面量 none"
        );

        // 只有空白也算空：一串空格在「什么都没说」上与空串没有区别。
        er.set_failure(&e.id, FailureKind::EnvBroken, "缺 libssl", "   ")
            .unwrap();
        assert_eq!(raw_failure(&er, &e.id).2.as_deref(), Some("none"));
    }

    /// `reason: ""` → **报错**，且库里那一行什么都不变。
    ///
    /// 规格 `task-model.md:75`：「没有原因串的失败等于没发生」。
    ///
    /// 判红方式：把 `set_failure` 里那句 `reason.trim().is_empty()` 判断删掉，
    /// 这条立刻红（会写进一条没有原因的失败）。
    #[test]
    fn a_failure_without_a_reason_is_rejected() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();

        for reason in ["", "   ", "\n\t"] {
            let err = er
                .set_failure(&e.id, FailureKind::AgentError, reason, "stderr: boom")
                .expect_err(&format!("{reason:?} 等于没有原因串，必须被拒"));
            assert!(
                matches!(err, StorageError::FailureReasonRequired),
                "{err:?}"
            );
            assert_eq!(
                raw_failure(&er, &e.id),
                (None, None, None),
                "被拒的失败不得落库（连 kind 也不行——半条失败记录比没有更糟）"
            );
        }
    }

    /// `set_failure` 打错的 id 要报 `ExecutionNotFound`，不能静默成功。
    ///
    /// 判红方式：把 `n == 0` 那段删掉，这条立刻红。
    #[test]
    fn set_failure_on_a_missing_execution_is_named() {
        let (_d, er) = setup();
        let e = er
            .set_failure("nope", FailureKind::Rejected, "无合格设备", "")
            .unwrap_err();
        assert!(matches!(e, StorageError::ExecutionNotFound { .. }), "{e:?}");
    }

    /// 重复调用 `mark_started` / `mark_ended` 不报错、**首次值不被洗掉**。
    ///
    /// `UPDATE ... WHERE started_at IS NULL` 的意义就在这里：重试 / 重连会再报
    /// 一次「开始」，每次都覆盖会让「这次跑了多久」变成「最后一次心跳到结束」。
    ///
    /// 判红方式：把 `AND started_at IS NULL` 删掉，这条立刻红
    /// （`started_at` 变成 `unix:200`）。
    #[test]
    fn mark_started_and_ended_are_idempotent_on_rerun() {
        let (_d, er) = setup();
        let e = er.create(new_exec(), TS).unwrap();
        assert_eq!(raw_time(&er, &e.id), (None, None), "前提：新建时两列都空");

        er.mark_started(&e.id, "unix:100").unwrap();
        er.mark_started(&e.id, "unix:200").unwrap();
        assert_eq!(
            raw_time(&er, &e.id).0.as_deref(),
            Some("unix:100"),
            "重复调用不得改写首次开始时间"
        );

        er.mark_ended(&e.id, "unix:300").unwrap();
        er.mark_ended(&e.id, "unix:400").unwrap();
        assert_eq!(
            raw_time(&er, &e.id),
            (Some("unix:100".to_string()), Some("unix:300".to_string())),
            "两列都保持首次值"
        );
    }

    /// 打错的 id → `ExecutionNotFound`（幂等不等于静默吞掉不存在的执行）。
    ///
    /// 判红方式：删掉 `ensure_exists` 的收尾判断（影响 0 行直接 `Ok`），
    /// 这条立刻红。
    #[test]
    fn mark_started_and_ended_name_a_missing_execution() {
        let (_d, er) = setup();
        let e = er.mark_started("nope", TS).unwrap_err();
        assert!(matches!(e, StorageError::ExecutionNotFound { .. }), "{e:?}");
        let e = er.mark_ended("nope", TS).unwrap_err();
        assert!(matches!(e, StorageError::ExecutionNotFound { .. }), "{e:?}");
    }
}
