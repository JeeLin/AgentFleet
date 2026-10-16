//! 版本化迁移。
//!
//! 驱动方式：`PRAGMA user_version`。每次 `open()` 按序执行缺失的迁移，
//! 每个迁移在单事务内完成（失败即回滚，`user_version` 保持上一版本）。

use rusqlite::Connection;
use tracing::info;

use crate::error::{Result, StorageError};

/// 当前 schema 版本。新增表/列时 +1 并在此处追加迁移。
pub const SCHEMA_VERSION: i64 = 1;

/// 一个迁移：版本号 + 在该事务内执行的 SQL。
struct Migration {
    version: i64,
    sql: &'static str,
}

/// 迁移清单。**必须按 version 升序**，且 version 从 1 开始连续。
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("schema.sql"),
}];

/// 把库升级到 [`SCHEMA_VERSION`]。
///
/// 幂等：已应用的迁移跳过（`schema.sql` 内所有 DDL 均为 `IF NOT EXISTS`）。
pub fn run(conn: &mut Connection) -> Result<()> {
    apply(conn, MIGRATIONS, SCHEMA_VERSION)
}

/// 按给定清单迁移到 `target`。抽出来是为了能注入一条会失败的迁移，
/// 验证「失败即回滚、`user_version` 不前进」（子任务 0 测试标准 ④）。
fn apply(conn: &mut Connection, migrations: &[Migration], target: i64) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    if current > target {
        return Err(StorageError::SchemaTooNew {
            found: current,
            supported: target,
        });
    }

    if current == target {
        return Ok(());
    }

    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)
            .map_err(|source| StorageError::Migration {
                version: m.version,
                source,
            })?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        info!(version = m.version, "applied migration");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn migration_is_idempotent() {
        let mut c = memory();
        run(&mut c).unwrap();
        let v1: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, SCHEMA_VERSION);

        // 第二次执行不得报错、不得倒退
        run(&mut c).unwrap();
        let v2: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, SCHEMA_VERSION);
    }

    #[test]
    fn tables_exist_after_migration() {
        let mut c = memory();
        run(&mut c).unwrap();
        for t in ["device", "secret"] {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {t} missing");
        }
    }

    #[test]
    fn session_table_is_deliberately_absent() {
        // M0 用签名 cookie，不建 session 表（README.md §3）
        let mut c = memory();
        run(&mut c).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn key_fingerprint_has_unique_constraint() {
        let mut c = memory();
        run(&mut c).unwrap();
        let insert = |c: &Connection, id: &str| {
            c.execute(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                 VALUES (?1, 'n', 'fp', '2026-01-01T00:00:00Z')",
                [id],
            )
        };
        insert(&c, "n_a").unwrap();
        // 同指纹不同 node_id 必须被 UNIQUE 挡住
        let e = insert(&c, "n_b").unwrap_err();
        assert!(
            matches!(e, rusqlite::Error::SqliteFailure(_, _)),
            "expected constraint violation, got {e:?}"
        );
    }

    #[test]
    fn status_check_constraint_rejects_unknown_value() {
        let mut c = memory();
        run(&mut c).unwrap();
        let e = c
            .execute(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at, status)
                 VALUES ('n_a','n','fp','2026-01-01T00:00:00Z','reconnecting')",
                [],
            )
            .unwrap_err();
        // 「重连中」只存在于内存，不落库（security §5）
        assert!(
            matches!(e, rusqlite::Error::SqliteFailure(_, _)),
            "got {e:?}"
        );
    }

    #[test]
    fn schema_too_new_is_refused() {
        let mut c = memory();
        run(&mut c).unwrap();
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let e = run(&mut c).unwrap_err();
        assert!(matches!(e, StorageError::SchemaTooNew { .. }), "got {e:?}");
    }

    #[test]
    fn failed_migration_rolls_back() {
        // 注入一个会失败的迁移，验证回滚后 user_version 与表都不变
        let mut c = memory();
        run(&mut c).unwrap();
        let before: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();

        let tx = c.transaction().unwrap();
        let _ = tx.execute_batch("CREATE TABLE boom (x); THIS IS NOT SQL;");
        drop(tx); // 未 commit，事务回滚

        let after: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "user_version changed on failed migration");
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='boom'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "failed migration left a table behind");
        // 库仍可用
        c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).unwrap();
    }

    #[test]
    fn apply_reports_failing_migration_and_leaves_version_intact() {
        // 子任务 0 测试标准 ④：注入一条会 Err 的迁移 →
        // run 返回 Err（带上版本号），user_version 不前进，
        // 该迁移里已执行的语句不留痕迹。
        let mut c = memory();
        let bad = Migration {
            version: 1,
            // 第一条成功、第二条语法错：验证**整条迁移**回滚，
            // 而不只是失败的那一句
            sql: "CREATE TABLE ok_tbl (x); THIS IS NOT SQL;",
        };
        let e = apply(&mut c, &[bad], 1).unwrap_err();
        assert!(
            matches!(e, StorageError::Migration { version: 1, .. }),
            "expected Migration error, got {e:?}"
        );

        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "user_version must not advance on failed migration");
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='ok_tbl'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "partial migration must roll back entirely");
    }
}
