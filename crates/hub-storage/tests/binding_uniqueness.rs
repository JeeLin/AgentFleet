//! 子任务 4 的存储层测试：`(project_id, device_id)` 唯一约束。
//!
//! 与 `crates/hub-server/tests/binding.rs` 分工不同：那边证的是**接口层**没有
//! 把判决做丢（错误码、状态码、两个入口写同一行），这边证的是**约束本身**——
//! 它必须由数据库的 `UNIQUE` 索引兜住，而不是只靠「路由层先查一次」。
//!
//! 差别是有意义的：先查后写会让两个并发 PUT 都通过检查，于是同一个
//! `(project, device)` 落下两行。竞态在正常测试里重现不出来，但**索引在不在**
//! 是能直接查的，所以这里直接查 `sqlite_master`。
//!
//! Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use hub_storage::binding::{BindingRepo, BindingSpec, CleanupPolicy};
use hub_storage::db::{open, Db};
use hub_storage::StorageError;

const TS: &str = "unix:1700000000";

/// 建一个带两个项目、两台设备的空库。
///
/// 直接插行而不是走 `ProjectRepo`：验的是 binding 自己，前置数据的形状越简单
/// 越好；走 repo 会把另一个模块的行为混进失败原因里。
fn setup() -> (tempfile::TempDir, Db) {
    let d = tempfile::tempdir().unwrap();
    let pool = open(&d.path().join("t.db")).unwrap();
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
    (d, pool)
}

fn spec(dir: &str) -> BindingSpec {
    BindingSpec {
        fixed_dir: dir.to_string(),
        disk_quota_bytes: None,
        cleanup_policy: CleanupPolicy::Balanced,
    }
}

fn count(pool: &Db) -> i64 {
    pool.get()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM binding", [], |r| r.get(0))
        .unwrap()
}

fn count_bindings(pool: &Db, project_id: &str, device_id: &str) -> i64 {
    pool.get()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM binding WHERE project_id = ?1 AND device_id = ?2",
            (project_id, device_id),
            |r| r.get(0),
        )
        .unwrap()
}

// ---- 唯一约束 ------------------------------------------------------------

/// **数据库里真有那个唯一索引**。
///
/// 这条用例看起来只是查一行元数据，但它是「唯一约束不由应用层独扛」的唯一
/// 证据：没有索引的话，两个并发 PUT 会在同一个 `(project, device)` 上各插一行，
/// 而这种竞态在测试里几乎不可能重现。
#[test]
fn the_database_itself_enforces_unique_project_and_device() {
    let (_d, pool) = setup();
    let indexes: Vec<String> = {
        let conn = pool.get().unwrap();
        let mut stmt = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type IN ('index','table') AND tbl_name='binding'")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        rows.into_iter().flatten().collect()
    };
    // 表定义上有 UNIQUE，或者有一个 unique index 覆盖这两列。
    let has_inline = indexes
        .iter()
        .any(|s| s.contains("UNIQUE") && s.contains("project_id") && s.contains("device_id"))
        || {
            let conn = pool.get().unwrap();
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_index_list('binding') WHERE \"unique\" = 1")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .iter()
                .any(|idx| {
                    let cols: Vec<String> = {
                        let conn = pool.get().unwrap();
                        let mut c = conn
                            .prepare("SELECT name FROM pragma_index_info(?1)")
                            .unwrap();
                        c.query_map((idx,), |r| r.get::<_, String>(0))
                            .unwrap()
                            .collect::<std::result::Result<Vec<_>, _>>()
                            .unwrap()
                    };
                    cols.len() == 2
                        && cols.contains(&"project_id".to_string())
                        && cols.contains(&"device_id".to_string())
                })
        };
    assert!(
        has_inline,
        "binding 表上必须有 (project_id, device_id) 的唯一约束，实际索引/表定义：{indexes:?}"
    );
}

/// 重复 upsert 不涨行数，且 id 不变。
#[test]
fn upserting_the_same_pair_twice_keeps_one_row_with_the_same_id() {
    let (_d, pool) = setup();
    let repo = BindingRepo::new(pool.clone());

    let first = repo.upsert("p_a", "n_a", spec("/srv/a"), TS).unwrap();
    let second = repo
        .upsert(
            "p_a",
            "n_a",
            BindingSpec {
                fixed_dir: "/srv/a".into(),
                disk_quota_bytes: Some(1000),
                cleanup_policy: CleanupPolicy::Aggressive,
            },
            TS,
        )
        .unwrap();

    assert_eq!(first.id, second.id, "必须是同一行");
    assert_eq!(count(&pool), 1);
    assert_eq!(second.disk_quota_bytes, Some(1000));
    assert_eq!(second.cleanup_policy, CleanupPolicy::Aggressive);
}

/// 不同设备可以绑不同目录；不同项目也可以绑同一台设备的同一目录。
#[test]
fn the_uniqueness_is_on_the_pair_not_on_either_side_alone() {
    let (_d, pool) = setup();
    let repo = BindingRepo::new(pool.clone());

    repo.upsert("p_a", "n_a", spec("/srv/a"), TS).unwrap();
    repo.upsert("p_a", "n_b", spec("/srv/b"), TS).unwrap();
    repo.upsert("p_b", "n_a", spec("/srv/c"), TS).unwrap();

    assert_eq!(count(&pool), 3, "三对不同组合，三行");
    assert_eq!(count_bindings(&pool, "p_a", "n_a"), 1);
}

// ---- `fixed_dir` 变更的硬约束 --------------------------------------------

/// 没有执行记录时可以改目录。
#[test]
fn the_fixed_dir_can_change_while_there_are_no_executions() {
    let (_d, pool) = setup();
    let repo = BindingRepo::new(pool.clone());

    repo.upsert("p_a", "n_a", spec("/srv/old"), TS).unwrap();
    let updated = repo.upsert("p_a", "n_a", spec("/srv/new"), TS).unwrap();
    assert_eq!(updated.fixed_dir, "/srv/new");
    assert_eq!(count(&pool), 1, "改目录不该插新行");
}

/// 有执行记录时改目录被拒，**且目录一个字节都没动**。
///
/// 后半句是重点：拒了却已经写进去一半，比不拒更糟——用户看到错误提示，
/// 而库里已经是新目录了。
#[test]
fn changing_the_fixed_dir_with_executions_is_refused_and_changes_nothing() {
    let (_d, pool) = setup();
    let repo = BindingRepo::new(pool.clone());

    let b = repo.upsert("p_a", "n_a", spec("/srv/old"), TS).unwrap();
    pool.get()
        .unwrap()
        .execute(
            "INSERT INTO execution (id, task_id, binding_id, state, created_at)
             VALUES ('e1', 't1', ?1, 'succeeded', 'unix:0')",
            (&b.id,),
        )
        .unwrap();

    match repo.upsert("p_a", "n_a", spec("/srv/new"), TS) {
        Err(StorageError::FixedDirInUse { executions, .. }) => assert_eq!(executions, 1),
        other => panic!("应报 FixedDirInUse，实际是 {other:?}"),
    }

    let after = repo.get("p_a", "n_a").unwrap().unwrap();
    assert_eq!(
        after.fixed_dir, "/srv/old",
        "被拒的改动绝不能落库——半截写入比不拒更糟"
    );
    assert_eq!(after.id, b.id, "也不能插一行新的");
}

/// 目录没变时，改配额与策略仍然放行。
///
/// 「目录锁定」不等于「整行锁死」：用户正是**因为**磁盘紧张才想调配额，
/// 而那时往往已经有执行记录。
#[test]
fn quota_and_policy_still_change_after_executions() {
    let (_d, pool) = setup();
    let repo = BindingRepo::new(pool.clone());

    let b = repo.upsert("p_a", "n_a", spec("/srv/old"), TS).unwrap();
    pool.get()
        .unwrap()
        .execute(
            "INSERT INTO execution (id, task_id, binding_id, state, created_at)
             VALUES ('e1', 't1', ?1, 'succeeded', 'unix:0')",
            (&b.id,),
        )
        .unwrap();

    let updated = repo
        .upsert(
            "p_a",
            "n_a",
            BindingSpec {
                fixed_dir: "/srv/old".into(),
                disk_quota_bytes: Some(5_000_000),
                cleanup_policy: CleanupPolicy::Aggressive,
            },
            TS,
        )
        .unwrap();
    assert_eq!(updated.disk_quota_bytes, Some(5_000_000));
    assert_eq!(updated.cleanup_policy, CleanupPolicy::Aggressive);
}

// ---- 设备侧统计 ----------------------------------------------------------

/// `execution_count_for_device` 走 binding 反查，看得到这台设备上的执行。
///
/// 这条是「有历史执行的设备不可删」的依据。它若数错（比如照抄了
/// `WHERE device_id = ...`，而 execution 表上根本没有 device_id 列），
/// 症状是**任何设备都能被删掉**，而删除动作返回成功。
#[test]
fn executions_are_counted_through_the_binding() {
    let (_d, pool) = setup();
    let repo = BindingRepo::new(pool.clone());

    let b = repo.upsert("p_a", "n_a", spec("/srv/a"), TS).unwrap();
    assert_eq!(repo.execution_count_for_device("n_a").unwrap(), 0);

    for id in ["e1", "e2"] {
        pool.get()
            .unwrap()
            .execute(
                "INSERT INTO execution (id, task_id, binding_id, state, created_at)
                 VALUES (?1, 't1', ?2, 'succeeded', 'unix:0')",
                (id, &b.id),
            )
            .unwrap();
    }
    assert_eq!(repo.execution_count_for_device("n_a").unwrap(), 2);
    // 另一台设备不受影响：数错的话这里会一起涨。
    assert_eq!(repo.execution_count_for_device("n_b").unwrap(), 0);
}
