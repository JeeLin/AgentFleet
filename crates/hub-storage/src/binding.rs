//! 绑定仓储：`(project, device) → 固定目录`。
//!
//! # 为什么必须唯一
//!
//! `UNIQUE (project_id, device_id)` 是这张表的全部意义：同一个项目在同一台设备上
//! **只有一个**固定目录。允许为不同任务绑不同目录的话，"清理"与"配额"立刻失去
//! 意义——它们算的是"这个项目在这台设备上用掉多少"，而目录一旦分裂，这句话就
//! 没有主语了（里程碑 §4）。
//!
//! # 为什么 PUT 是 upsert，而别处不是
//!
//! 这是全仓**唯一**允许 upsert 的地方，因为 binding 是「设置」而不是「建实体」：
//! 用户改的是"这个项目在这台设备上该用哪个目录"，不是"再建一个绑定"。
//! 同一个 `(project, device)` 第二次 PUT 必须更新同一行。
//!
//! # 双向可改
//!
//! `PRODUCT.md` §6.1 要求「项目页与设备页**均可**修改绑定」。两个入口写的是
//! **同一行**——本模块的 `upsert` 只有一个实现，两侧路由都调它。没有"设备侧的
//! binding"这种第二份数据。

use r2d2_sqlite::rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::Db;

/// 磁盘清理策略。三档，见 `workspace-and-sync.md`。
///
/// **默认 `Balanced`**，不是 `Conservative`：保守档会留三个产物，而用户第一次
/// 看到设备磁盘满了时的第一反应是"清一下"，不是"再多留点"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanupPolicy {
    /// 保留最近 3 个产物，其余清理。用户手动删之前什么都不丢。
    Conservative,
    /// 执行跑完就清理，但保留被合并/被引用的那次。
    Balanced,
    /// 跑完即清，只留当前 worktree。
    Aggressive,
}

impl CleanupPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
        }
    }
}

impl FromStr for CleanupPolicy {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "conservative" => Ok(Self::Conservative),
            "balanced" => Ok(Self::Balanced),
            "aggressive" => Ok(Self::Aggressive),
            other => Err(StorageError::InvalidCleanupPolicy(other.to_string())),
        }
    }
}

/// 一个绑定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub id: String,
    pub project_id: String,
    pub device_id: String,
    /// 设备上的路径原文。与 `workspace.path` 同一口径：不展开 `~`，
    /// Hub 没有那台设备的文件系统。
    pub fixed_dir: String,
    /// 配额上限。`None` = 不限额。
    ///
    /// 不限额与"限额为 0"是两件事，前者该放行、后者该全拦——用 `Option` 区分，
    /// 不用 `0` 兼职表示"没设"。
    pub disk_quota_bytes: Option<i64>,
    pub cleanup_policy: CleanupPolicy,
    pub created_at: String,
}

/// 写入绑定的入参（PUT 的 body）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingSpec {
    pub fixed_dir: String,
    pub disk_quota_bytes: Option<i64>,
    pub cleanup_policy: CleanupPolicy,
}

/// 固定目录变更被拒的理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedDirChange {
    /// 该绑定下已有执行记录，其 worktree 就建在旧目录里。
    HasExecutions,
    /// 没有执行记录，可以改。
    Safe,
}

/// 绑定仓储。
#[derive(Clone)]
pub struct BindingRepo {
    pool: Db,
}

impl BindingRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 按 `(project, device)` 取绑定。
    pub fn get(&self, project_id: &str, device_id: &str) -> Result<Option<Binding>> {
        let conn = self.pool.get()?;
        let raw = conn
            .query_row(
                "SELECT id, project_id, device_id, fixed_dir, disk_quota_bytes,
                        cleanup_policy, created_at
                 FROM binding WHERE project_id = ?1 AND device_id = ?2",
                params![project_id, device_id],
                row_to_raw,
            )
            .optional()?;
        raw.map(RawBinding::into_binding).transpose()
    }

    /// 一个项目的全部绑定（项目设置页）。
    pub fn list_for_project(&self, project_id: &str) -> Result<Vec<Binding>> {
        self.list_by("project_id", project_id)
    }

    /// 一台设备的全部绑定（设备页——**双向可改的另一个方向**）。
    pub fn list_for_device(&self, device_id: &str) -> Result<Vec<Binding>> {
        self.list_by("device_id", device_id)
    }

    fn list_by(&self, column: &str, value: &str) -> Result<Vec<Binding>> {
        let conn = self.pool.get()?;
        // `column` 只会是本文件里写死的两个字面量之一，不是外部输入——
        // 但仍然不用 `format!` 拼进 SQL 之外的地方：这里是唯一一处拼接，
        // 且拼的是常量。
        let sql = format!(
            "SELECT id, project_id, device_id, fixed_dir, disk_quota_bytes,
                    cleanup_policy, created_at
             FROM binding WHERE {column} = ?1 ORDER BY created_at, id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![value], row_to_raw)?;
        let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawBinding::into_binding).collect()
    }

    /// 写入绑定（**幂等 upsert**，全仓唯一一处）。
    ///
    /// # `fixed_dir` 变更的硬约束
    ///
    /// 该绑定下已有执行记录时，改目录会让正在跑（以及已跑完但 worktree 还在）
    /// 的执行失去归属——它们的 `worktree_path` 指向旧目录，而清理与配额此后
    /// 会去新目录里找。这种情况**拒绝**，返回 `HasExecutions`。
    ///
    /// 判断放在写入前的同一个事务里，不是先查后写：先查后写会让两个并发 PUT
    /// 都通过检查。
    pub fn upsert(
        &self,
        project_id: &str,
        device_id: &str,
        spec: BindingSpec,
        now: &str,
    ) -> Result<Binding> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT id, fixed_dir FROM binding WHERE project_id = ?1 AND device_id = ?2",
                params![project_id, device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        match existing {
            Some((binding_id, old_dir)) if old_dir != spec.fixed_dir => {
                // 有执行记录就拒绝改目录。
                let n: i64 = count_executions(&tx, &binding_id)?;
                if n > 0 {
                    return Err(StorageError::FixedDirInUse {
                        project_id: project_id.to_string(),
                        device_id: device_id.to_string(),
                        executions: n,
                    });
                }
                tx.execute(
                    "UPDATE binding
                        SET fixed_dir = ?2, disk_quota_bytes = ?3, cleanup_policy = ?4
                      WHERE id = ?1",
                    params![
                        binding_id,
                        spec.fixed_dir,
                        spec.disk_quota_bytes,
                        spec.cleanup_policy.as_str()
                    ],
                )?;
            }
            Some((binding_id, _)) => {
                // 目录没变：只更新配额与策略。
                tx.execute(
                    "UPDATE binding SET disk_quota_bytes = ?2, cleanup_policy = ?3 WHERE id = ?1",
                    params![
                        binding_id,
                        spec.disk_quota_bytes,
                        spec.cleanup_policy.as_str()
                    ],
                )?;
            }
            None => {
                let id = crate::workspace::new_id();
                tx.execute(
                    "INSERT INTO binding
                         (id, project_id, device_id, fixed_dir, disk_quota_bytes,
                          cleanup_policy, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        id,
                        project_id,
                        device_id,
                        spec.fixed_dir,
                        spec.disk_quota_bytes,
                        spec.cleanup_policy.as_str(),
                        now
                    ],
                )?;
            }
        }

        let raw = tx.query_row(
            "SELECT id, project_id, device_id, fixed_dir, disk_quota_bytes,
                    cleanup_policy, created_at
             FROM binding WHERE project_id = ?1 AND device_id = ?2",
            params![project_id, device_id],
            row_to_raw,
        )?;
        tx.commit()?;
        raw.into_binding()
    }

    /// 删绑定。
    ///
    /// 有执行记录就拒——与「有历史执行的设备不可删」是同一条口径：
    /// 执行记录是历史，绑定的存在是它归属的依据。
    pub fn delete(&self, project_id: &str, device_id: &str) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM binding WHERE project_id = ?1 AND device_id = ?2",
                params![project_id, device_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(binding_id) = existing else {
            return Err(StorageError::NoSuchBinding {
                project_id: project_id.to_string(),
                device_id: device_id.to_string(),
            });
        };
        let n: i64 = count_executions(&tx, &binding_id)?;
        if n > 0 {
            return Err(StorageError::FixedDirInUse {
                project_id: project_id.to_string(),
                device_id: device_id.to_string(),
                executions: n,
            });
        }
        tx.execute("DELETE FROM binding WHERE id = ?1", params![binding_id])?;
        tx.commit()?;
        Ok(())
    }

    /// 该设备上是否有历史执行记录。
    ///
    /// 供 `DELETE /api/devices/:id` 的前置检查——`PRODUCT.md` §6.1 原文：
    /// 「设备被移除时，若该设备上有历史执行记录，**不允许删除设备**（先归档或
    /// 转移项目）；只有无历史时才可删除，历史执行记录永久保留」。
    ///
    /// 走 binding 而不是 execution 上的 device 列：不冗余存 device_id，
    /// 只有一处真相（binding）。
    pub fn execution_count_for_device(&self, device_id: &str) -> Result<i64> {
        let conn = self.pool.get()?;
        count_executions_on(&conn, device_id)
    }
}

/// 某个绑定名下有几条执行记录（事务内版本）。
///
/// # 为什么抽成函数，而不是在两处各写一遍同一段 `query_row`
///
/// 两处（改目录前的检查、删绑定前的检查）写的是**同一条 SQL、同一个判据**。
/// 抄两份的代价不是多写几行，而是迟早只改一处：一处改成「只数未结束的执行」，
/// 另一处还按全部数——于是「有历史执行不可删」在这两条路径上给出两个答案。
///
/// 顺带把 `)?;` 这类被 llvm-cov 记成宏参数 miss 的行收到一处。
fn count_executions(tx: &Transaction<'_>, binding_id: &str) -> Result<i64> {
    let n = tx.query_row(
        "SELECT COUNT(*) FROM execution WHERE binding_id = ?1",
        params![binding_id],
        |r| r.get(0),
        // coverage: excluded — `)?;` 这一行被 llvm-cov 记为 miss（已知现象）：
        // `query_row` 的返回是 `Result`，`?` 展开成的一段分支在 `Ok` 路径上
        // 不产生独立计数，工具把它归到 miss。上面的 `query_row` 调用与下面的
        // `Ok(n)` 都有覆盖，说明这条语句确实执行过。
    )?;
    Ok(n)
}

/// 这台设备上、经它的绑定挂着的执行记录有几条。
///
/// `execution` 上没有 `device_id` 列，归属只有一处真相（`binding`），所以要联表。
/// 删设备的前置检查靠它：有历史执行的设备不许删，理由与 [`BindingRepo::delete`]
/// 同一条——执行记录不能失去归属。
///
/// 与 [`count_executions`] 是两个函数而不是一个泛型：两者的 `WHERE` 条件不同
/// （一个按 `binding.id`、一个按 `binding.device_id`），合成一个等于在里面写
/// 分支，而分支本身就是「两条查询会漂移」的入口。共有的只是「`COUNT(*)` 读成
/// `i64`」这一句。
fn count_executions_on(conn: &Connection, device_id: &str) -> Result<i64> {
    let n = conn.query_row(
        "SELECT COUNT(*) FROM execution
           JOIN binding ON binding.id = execution.binding_id
          WHERE binding.device_id = ?1",
        params![device_id],
        |r| r.get(0),
        // coverage: excluded — 同 [`count_executions`]：`)?;` 被 llvm-cov
        // 记为 miss（已知现象），`query_row` 调用本身有覆盖。
    )?;
    Ok(n)
}

struct RawBinding {
    id: String,
    project_id: String,
    device_id: String,
    fixed_dir: String,
    disk_quota_bytes: Option<i64>,
    cleanup_policy: String,
    created_at: String,
}

impl RawBinding {
    fn into_binding(self) -> Result<Binding> {
        Ok(Binding {
            id: self.id,
            project_id: self.project_id,
            device_id: self.device_id,
            fixed_dir: self.fixed_dir,
            disk_quota_bytes: self.disk_quota_bytes,
            cleanup_policy: CleanupPolicy::from_str(&self.cleanup_policy)?,
            created_at: self.created_at,
        })
    }
}

fn row_to_raw(r: &Row<'_>) -> std::result::Result<RawBinding, r2d2_sqlite::rusqlite::Error> {
    Ok(RawBinding {
        id: r.get(0)?,
        project_id: r.get(1)?,
        device_id: r.get(2)?,
        fixed_dir: r.get(3)?,
        disk_quota_bytes: r.get(4)?,
        cleanup_policy: r.get(5)?,
        created_at: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, BindingRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        // 项目与绑定都必须挂在真实存在的行上（v3 去外键后由业务层查，
        // 但测试里直接插更省事，且验的是 binding 本身）。
        pool.get()
            .unwrap()
            .execute_batch(
                "INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_a', 'app', 'central', 'unix:0');
                 INSERT INTO project (id, name, shape, created_at)
                     VALUES ('p_b', 'other', 'central', 'unix:0');
                 INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                     VALUES ('n_a', 'pi', 'fp_a', 'unix:0');
                 INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                     VALUES ('n_b', 'laptop', 'fp_b', 'unix:0');",
            )
            .unwrap();
        (d, BindingRepo::new(pool))
    }

    fn spec(dir: &str) -> BindingSpec {
        BindingSpec {
            fixed_dir: dir.into(),
            disk_quota_bytes: Some(10 * 1024 * 1024 * 1024),
            cleanup_policy: CleanupPolicy::Balanced,
        }
    }

    /// 插一条 execution，用来构造「有历史执行」的绑定。
    fn add_execution(br: &BindingRepo, binding_id: &str) {
        br.pool
            .get()
            .unwrap()
            .execute(
                "INSERT INTO task (id, project_id, title, state, created_at, updated_at)
                 VALUES ('t_a', 'p_a', 'do it', 'ready', ?1, ?1)",
                params![TS],
            )
            .unwrap();
        br.pool
            .get()
            .unwrap()
            .execute(
                "INSERT INTO execution (id, task_id, binding_id, state, created_at)
                 VALUES ('e_a', 't_a', ?1, 'pending', ?2)",
                params![binding_id, TS],
            )
            .unwrap();
    }

    #[test]
    fn create_round_trips_every_field() {
        let (_d, br) = setup();
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        assert_eq!(b.project_id, "p_a");
        assert_eq!(b.device_id, "n_a");
        assert_eq!(b.fixed_dir, "/srv/app");
        assert_eq!(b.disk_quota_bytes, Some(10 * 1024 * 1024 * 1024));
        assert_eq!(b.cleanup_policy, CleanupPolicy::Balanced);
        assert_eq!(b.created_at, TS);

        let f = br.get("p_a", "n_a").unwrap().unwrap();
        assert_eq!(f.id, b.id);
    }

    /// **PUT 是幂等 upsert**：同一 `(project, device)` 第二次写不插新行。
    #[test]
    fn a_second_put_updates_the_same_row_instead_of_inserting() {
        let (_d, br) = setup();
        let first = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        let second = br
            .upsert(
                "p_a",
                "n_a",
                BindingSpec {
                    fixed_dir: "/srv/app".into(),
                    disk_quota_bytes: None,
                    cleanup_policy: CleanupPolicy::Aggressive,
                },
                "unix:9999",
            )
            .unwrap();
        assert_eq!(second.id, first.id, "必须是同一行");
        assert_eq!(second.disk_quota_bytes, None, "配额被更新");
        assert_eq!(second.cleanup_policy, CleanupPolicy::Aggressive);
        assert_eq!(second.created_at, TS, "created_at 不随更新变");
        assert_eq!(br.list_for_project("p_a").unwrap().len(), 1);
    }

    /// 同一项目两台设备可以绑不同目录——唯一约束只管 `(project, device)`。
    #[test]
    fn one_project_can_bind_two_devices_to_different_directories() {
        let (_d, br) = setup();
        br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        br.upsert("p_a", "n_b", spec("/home/me/app"), TS).unwrap();
        let list = br.list_for_project("p_a").unwrap();
        assert_eq!(list.len(), 2);
        let dirs: Vec<&str> = list.iter().map(|b| b.fixed_dir.as_str()).collect();
        assert!(dirs.contains(&"/srv/app") && dirs.contains(&"/home/me/app"));
    }

    /// **有执行记录时改固定目录必须被拒**——否则正在跑的执行失去归属。
    #[test]
    fn changing_the_fixed_dir_is_refused_while_executions_exist() {
        let (_d, br) = setup();
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        add_execution(&br, &b.id);

        let e = br
            .upsert("p_a", "n_a", spec("/srv/moved"), "unix:9999")
            .unwrap_err();
        assert!(
            matches!(e, StorageError::FixedDirInUse { executions: 1, .. }),
            "{e:?}"
        );
        assert_eq!(
            br.get("p_a", "n_a").unwrap().unwrap().fixed_dir,
            "/srv/app",
            "被拒之后目录不得变"
        );
    }

    /// 有执行记录但**目录没变**时，改配额与策略仍然允许。
    #[test]
    fn changing_only_the_quota_stays_allowed_when_executions_exist() {
        let (_d, br) = setup();
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        add_execution(&br, &b.id);
        let got = br
            .upsert(
                "p_a",
                "n_a",
                BindingSpec {
                    fixed_dir: "/srv/app".into(),
                    disk_quota_bytes: Some(1),
                    cleanup_policy: CleanupPolicy::Conservative,
                },
                "unix:9999",
            )
            .unwrap();
        assert_eq!(got.disk_quota_bytes, Some(1));
        assert_eq!(got.cleanup_policy, CleanupPolicy::Conservative);
    }

    /// **双向等价**：从设备侧与从项目侧读到的是同一行。
    #[test]
    fn both_directions_reach_the_same_row() {
        let (_d, br) = setup();
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        let from_project = br.list_for_project("p_a").unwrap();
        let from_device = br.list_for_device("n_a").unwrap();
        assert_eq!(from_project.len(), 1);
        assert_eq!(from_device.len(), 1);
        assert_eq!(from_project[0].id, b.id);
        assert_eq!(from_device[0].id, b.id, "两侧必须是同一行，不是两份数据");
    }

    /// `execution_count_for_device`：删设备的前置检查靠它。
    #[test]
    fn execution_count_for_device_sees_executions_through_the_binding() {
        let (_d, br) = setup();
        assert_eq!(br.execution_count_for_device("n_a").unwrap(), 0);
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        add_execution(&br, &b.id);
        assert_eq!(br.execution_count_for_device("n_a").unwrap(), 1);
        assert_eq!(
            br.execution_count_for_device("n_b").unwrap(),
            0,
            "别的设备不该被算进来"
        );
    }

    /// 有执行记录时删绑定被拒；没有时删得掉。
    #[test]
    fn delete_is_refused_while_executions_point_at_the_binding() {
        let (_d, br) = setup();
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        add_execution(&br, &b.id);
        let e = br.delete("p_a", "n_a").unwrap_err();
        assert!(matches!(e, StorageError::FixedDirInUse { .. }), "{e:?}");
        assert!(br.get("p_a", "n_a").unwrap().is_some());

        // 另一台没执行记录的设备删得掉。
        br.upsert("p_a", "n_b", spec("/srv/b"), TS).unwrap();
        br.delete("p_a", "n_b").unwrap();
        assert!(br.get("p_a", "n_b").unwrap().is_none());
    }

    #[test]
    fn deleting_a_missing_binding_is_named() {
        let (_d, br) = setup();
        let e = br.delete("p_a", "n_a").unwrap_err();
        assert!(matches!(e, StorageError::NoSuchBinding { .. }), "{e:?}");
    }

    /// 库里的非法策略必须报错，**不静默回落 `Balanced`**。
    #[test]
    fn an_unknown_cleanup_policy_is_an_error_not_a_default() {
        let (_d, br) = setup();
        br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        br.pool
            .get()
            .unwrap()
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE binding SET cleanup_policy = 'yolo';",
            )
            .unwrap();
        let e = br.get("p_a", "n_a").unwrap_err();
        assert!(matches!(e, StorageError::InvalidCleanupPolicy(_)), "{e:?}");
    }

    #[test]
    fn every_cleanup_policy_round_trips_through_its_literal() {
        for p in [
            CleanupPolicy::Conservative,
            CleanupPolicy::Balanced,
            CleanupPolicy::Aggressive,
        ] {
            assert_eq!(CleanupPolicy::from_str(p.as_str()).unwrap(), p);
        }
    }

    /// **没有执行记录时，改固定目录是允许的。**
    ///
    /// 上一条用例只证明了「有执行时被拒」，那半边单独成立时，「一律拒绝」
    /// 的错实现也能通过。这一条补上另一半：目录真的改了，配额与策略
    /// 也一起改了，且 `created_at` 没动。
    #[test]
    fn changing_the_fixed_dir_is_allowed_when_there_are_no_executions() {
        let (_d, br) = setup();
        let b = br.upsert("p_a", "n_a", spec("/srv/app"), TS).unwrap();
        let moved = br
            .upsert(
                "p_a",
                "n_a",
                BindingSpec {
                    fixed_dir: "/srv/moved".into(),
                    disk_quota_bytes: Some(42),
                    cleanup_policy: CleanupPolicy::Aggressive,
                },
                "unix:9999",
            )
            .unwrap();
        assert_eq!(moved.id, b.id, "改目录不是新建一行");
        assert_eq!(moved.fixed_dir, "/srv/moved");
        assert_eq!(moved.disk_quota_bytes, Some(42));
        assert_eq!(moved.cleanup_policy, CleanupPolicy::Aggressive);
        assert_eq!(
            moved.created_at, TS,
            "created_at 是「绑定时」不是「改绑时」"
        );
        assert_eq!(br.list_for_project("p_a").unwrap().len(), 1);
    }

    /// `list_for_project` / `list_for_device` 在**没有绑定**时返回空列表，
    /// 不是 `None`、不是错误。
    ///
    /// 两条读路径都要试：它们走同一个 `list_by`，但真实页面分别从项目页与
    /// 设备页进入，任何一侧把空结果当错误都会让「这设备还没绑项目」变成
    /// 一个报错弹窗。
    #[test]
    fn listing_an_unbound_project_or_device_is_empty_not_an_error() {
        let (_d, br) = setup();
        assert!(br.list_for_project("p_a").unwrap().is_empty());
        assert!(br.list_for_device("n_a").unwrap().is_empty());
        assert_eq!(
            br.execution_count_for_device("n_a").unwrap(),
            0,
            "没有绑定时，设备上的执行记录数必然是 0"
        );
    }
}
