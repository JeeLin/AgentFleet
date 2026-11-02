//! 版本化迁移。
//!
//! 驱动方式：`PRAGMA user_version`。每次 `open()` 按序执行缺失的迁移，
//! 每个迁移在单事务内完成（失败即回滚，`user_version` 保持上一版本）。

use rusqlite::Connection;
use tracing::info;

use crate::error::{Result, StorageError};

/// 当前 schema 版本。新增表/列时 +1 并在此处追加迁移。
pub const SCHEMA_VERSION: i64 = 4;

/// 一个迁移：版本号 + 在该事务内执行的 SQL。
struct Migration {
    version: i64,
    sql: &'static str,
    /// 该迁移**重建表**（建新表 → `INSERT SELECT` → `DROP` → `RENAME`）。
    ///
    /// 为真时，迁移前后要把 `PRAGMA foreign_keys` 关掉再打开。**这不是可选的
    /// 优化，不加就必然失败**：`PRAGMA foreign_keys=ON` 下 `DROP TABLE workspace`
    /// 会去检查还有没有行指着它——v2 的 `session.workspace_id` 正是
    /// `REFERENCES workspace(id)`，于是 SQLite 直接甩
    /// `FOREIGN KEY constraint failed (1811)`，整条 v3 回滚。
    ///
    /// 官方重建表流程（SQLite 文档「Making Other Kinds Of Table Schema Changes」）
    /// 原文就要求在重建期间关掉外键。这里照做，且**只在需要的迁移上关**——
    /// 全局关会让普通迁移里的孤儿行静默通过。
    ///
    /// 两个反直觉点：
    /// 1. `PRAGMA foreign_keys` 在**事务内是空操作**，必须开在 `BEGIN` 之前、
    ///    关在 `COMMIT` 之后。写成事务内的一句 `pragma` 会静默不生效。
    /// 2. 关掉期间检查不了外键，所以迁移自身要把数据搬对（`INSERT SELECT`
    ///    原样搬）。迁移跑完后 `foreign_keys` 必须恢复原值——失败路径也恢复。
    rebuilds_tables: bool,
}

/// 迁移清单。**必须按 version 升序**，且 version 从 1 开始连续。
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("schema.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 2,
        sql: include_str!("schema_v2.sql"),
        rebuilds_tables: false,
    },
    Migration {
        version: 3,
        sql: include_str!("schema_v3.sql"),
        // v3 去外键：SQLite 不支持 DROP CONSTRAINT，只能重建 workspace / session
        rebuilds_tables: true,
    },
    Migration {
        version: 4,
        sql: include_str!("schema_v4.sql"),
        // 两条 ADD COLUMN，不重建表。**必须为 false**：为 true 会去关外键，
        // 而这里没有任何要重建的东西——多关一次只会让迁移期间的外键检查失效。
        rebuilds_tables: false,
    },
];

/// 把库升级到 [`SCHEMA_VERSION`]。
///
/// 幂等：`user_version` 闸门让每条迁移只跑一遍，已应用的直接跳过。
/// **幂等性不靠 DDL 本身**——`ALTER TABLE ADD COLUMN` 没有 `IF NOT EXISTS`
/// 形式（`schema_v2.sql` 的注释与测试各自说明了这一点），靠的是闸门；闸门说谎时
/// 整条迁移在事务里回滚并报错停下。
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

    for m in migrations
        .iter()
        .filter(|m| m.version > current && m.version <= target)
    {
        // 重建表的迁移（v3）必须在 `foreign_keys=OFF` 下跑：`DROP TABLE workspace`
        // 会撞上还在指着它的 `session.workspace_id` 外键。**PRAGMA 在事务内是
        // 空操作**，所以开关必须在事务外，且**失败也要恢复**——否则一条失败的
        // 迁移会把整个连接池的外键检查永久关掉（那是静默的数据损坏通道）。
        let fk_before: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        if m.rebuilds_tables && fk_before != 0 {
            conn.pragma_update(None, "foreign_keys", "OFF")?;
        }

        let result = (|| -> Result<()> {
            let tx = conn.transaction()?;
            tx.execute_batch(m.sql)
                .map_err(|source| StorageError::Migration {
                    version: m.version,
                    source,
                })?;
            tx.pragma_update(None, "user_version", m.version)?;
            tx.commit()?;
            Ok(())
        })();

        // 无论成败都恢复到迁移前的值：关着外键继续跑，后面所有写入都失去防线
        if m.rebuilds_tables && fk_before != 0 {
            conn.pragma_update(None, "foreign_keys", "ON")?;
        }
        result?;
        info!(version = m.version, "applied migration");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use r2d2_sqlite::rusqlite::params;

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

    /// M1 起 `session` 表存在：它是**工作台会话**（设备 + 工作区 + agent），
    /// 与登录 cookie 无关——登录态的 HMAC 密钥仍在 `secret` 表里。
    ///
    /// 这条断言在 M0 恰恰相反（`session_table_is_deliberately_absent`
    /// 断言不存在），M1 建了表就必须在同一处改写，否则旧事实变成死条款。
    #[test]
    fn session_table_exists_after_migration() {
        let mut c = memory();
        run(&mut c).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "M1 builds the workbench session table");
    }

    /// v2 的两张新表与 `device.public_key` 必须同时落库。
    ///
    /// 只断言表名不够：v2 里真正有信息量的部分是那一列回填（`migrate.rs` 另有
    /// 一条升级测试），而 `ALTER TABLE` 失败时表照样在，只看 `sqlite_master`
    /// 会漏。
    #[test]
    fn workspace_table_and_device_public_key_column_exist_after_migration() {
        let mut c = memory();
        run(&mut c).unwrap();
        let ws: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='workspace'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ws, 1, "workspace table missing");
        let col: i64 = c
            .query_row(
                "SELECT count(*) FROM pragma_table_info('device') WHERE name='public_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(col, 1, "device.public_key must be a real column");
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
        // 判据必须是**约束冲突**本身：`matches!(e, SqliteFailure(_, _))`
        // 只说明「SQLite 报了错」，把列名写错成 `key_fingerprints` 也照样绿。
        assert_eq!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
            "expected a UNIQUE constraint violation, got {e:?}"
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
        assert_eq!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
            "expected a CHECK constraint violation, got {e:?}"
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

    /// 失败的那条迁移必须**整体回滚**，而已经跑过的版本号与数据一个字都不动。
    ///
    /// 起点是一个**已经迁到 v2** 的库：失败发生在「还有后续版本要迁」的路上，
    /// 而不是「第一条迁移就失败」的路上——后者由
    /// `apply_reports_failing_migration_and_leaves_version_intact` 覆盖。
    ///
    /// 走 `apply` 注入失败迁移，而不是自己手搓一个事务：那才是本仓真正的
    /// 迁移路径（`apply` 负责开事务、`execute_batch`、写 `user_version`）。
    #[test]
    fn failed_migration_rolls_back() {
        let mut c = memory();
        run(&mut c).unwrap();
        c.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES ('n_a', 'pi', 'fp_a', 'unix:0')",
            [],
        )
        .unwrap();
        let before: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, SCHEMA_VERSION, "precondition: a migrated database");

        // 注入一条会失败的 v3 迁移：第一条语句成功、第二条语法错。
        // `rebuilds_tables: true` 是刻意的：它让这条用例同时守住「失败路径也要
        // 把 foreign_keys 恢复成 ON」——那是比丢数据更隐蔽的故障：
        // 没恢复的话，进程余下的整个生命周期都在没有外键检查地写库，
        // 而且不会有任何一条测试以外的信号。
        let bad = Migration {
            version: SCHEMA_VERSION + 1,
            sql: "CREATE TABLE boom (x); THIS IS NOT SQL;",
            rebuilds_tables: true,
        };
        let e = apply(&mut c, &[bad], SCHEMA_VERSION + 1).unwrap_err();
        assert!(
            matches!(e, StorageError::Migration { version, .. } if version == SCHEMA_VERSION + 1),
            "got {e:?}"
        );
        assert_eq!(
            c.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "失败的迁移也必须把 foreign_keys 恢复为 ON"
        );

        // 版本号不前进
        let after: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "user_version changed on failed migration");
        // 迁移里已执行的那条语句不留痕迹
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='boom'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "failed migration left a table behind");
        // 既有数据一个字都没动
        let name: String = c
            .query_row("SELECT name FROM device WHERE id = 'n_a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "pi", "failed migration must not touch existing rows");
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
            rebuilds_tables: false,
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

    /// v1 → v2 的**真实升级**：v1 库（M0 留下的）必须拿到 v2 的全部结构，
    /// 且存量的 capabilities JSON 公钥被回填进新列。
    ///
    /// 「拿新库直接跑 `run()`」测不到这条：`schema.sql` 里 v2 的 DDL 根本不在，
    /// 内存库走的是 v1 再 v2 两步，而真实升级路径只跑 v2 那一步。
    #[test]
    fn a_v1_database_upgrades_to_v2_with_its_public_keys_backfilled() {
        let mut c = memory();
        apply(&mut c, MIGRATIONS, 1).unwrap();
        let fp = {
            let kp = identity::KeyPair::generate();
            let pk = kp.public_key_bytes();
            let fp = identity::Fingerprint::of(&pk).to_hex();
            c.execute(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at, capabilities)
                 VALUES ('n_a', 'pi', ?1, 'unix:0', ?2)",
                params![
                    fp,
                    format!(
                        r#"{{"connectors":["terminal"],"public_key":"{}"}}"#,
                        crate::base64url_of(&pk)
                    )
                ],
            )
            .unwrap();
            // 一台 v1 时代就没有公钥的设备（老数据 / 手动插的行）
            c.execute(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                 VALUES ('n_b', 'pi', 'fp_b', 'unix:0')",
                [],
            )
            .unwrap();
            fp
        };
        let expected: String = c
            .query_row(
                "SELECT json_extract(capabilities, '$.public_key') FROM device WHERE id='n_a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "precondition: a v1 database"
        );

        run(&mut c).unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );

        let got: Option<String> = c
            .query_row("SELECT public_key FROM device WHERE id='n_a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            got.as_deref(),
            Some(expected.as_str()),
            "v1 公钥必须回填到新列"
        );
        let none: Option<String> = c
            .query_row("SELECT public_key FROM device WHERE id='n_b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(none, None, "抽不出公钥就留 NULL，不编一个");
        // 回填的是 43 字符的 base64url，且与库内指纹一致（否则重连必然 mismatch）
        let raw = identity::base64url_decode(&got.expect("n_a must have a backfilled key"))
            .expect("backfilled key must be base64url");
        assert_eq!(raw.len(), 32, "公钥是 32 字节 Ed25519 公钥");
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|v: Vec<u8>| format!("bad length {v:?}"))
            .expect("length already asserted");
        assert_eq!(identity::Fingerprint::of(&bytes).to_hex(), fp);
    }

    /// v2 → v3：重建 `workspace` / `session` 两张表去掉外键，**数据不能丢**。
    ///
    /// 这条测试守的是 v3 迁移里最危险的一段：SQLite 不支持
    /// `ALTER TABLE ... DROP CONSTRAINT`，去外键只能建新表 → `INSERT SELECT`
    /// → `DROP` → `RENAME`。四步里任何一步漏了数据，用户已开的会话就没了，
    /// 而 `INSERT SELECT` 只要列数对上就不会报错——**静默丢数据**是这里唯一的
    /// 真实风险，必须用数据断言锁住，不能只断言 `user_version` 前进。
    #[test]
    fn a_v2_database_upgrades_to_v3_without_losing_workspaces_or_sessions() {
        let mut c = memory();
        apply(&mut c, MIGRATIONS, 2).unwrap();
        c.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES ('n_a', 'pi', 'fp_a', 'unix:0')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO workspace (id, node_id, path, created_at)
             VALUES ('w1', 'n_a', '/srv/app', 'unix:10')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO session (id, workspace_id, agent_id, state, created_at)
             VALUES ('s1', 'w1', 'opencode', 'running', 'unix:11')",
            [],
        )
        .unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2,
            "precondition: a v2 database"
        );

        run(&mut c).unwrap();

        // 迁移期间为了重建表把外键关了，**跑完必须打开**。断言放在这里而不是
        // 别处：只有这条路径真的走过 `rebuilds_tables = true` 的开关分支。
        // 忘了恢复的话，进程余下的整个生命周期都在没有外键检查地写库。
        assert_eq!(
            c.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "重建表之后 foreign_keys 必须恢复为 ON"
        );

        // 数据一行不少、字段原样
        let (w_id, w_node, w_path, w_created): (String, String, String, String) = c
            .query_row(
                "SELECT id, node_id, path, created_at FROM workspace WHERE id='w1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (
                w_id.as_str(),
                w_node.as_str(),
                w_path.as_str(),
                w_created.as_str()
            ),
            ("w1", "n_a", "/srv/app", "unix:10")
        );
        let (s_id, s_ws, s_agent, s_state): (String, String, Option<String>, String) = c
            .query_row(
                "SELECT id, workspace_id, agent_id, state FROM session WHERE id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (
                s_id.as_str(),
                s_ws.as_str(),
                s_agent.as_deref(),
                s_state.as_str()
            ),
            ("s1", "w1", Some("opencode"), "running")
        );
        // 新列在存量行上是 NULL，不是空串
        let removed: Option<String> = c
            .query_row(
                "SELECT removed_node_id FROM workspace WHERE id='w1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(removed, None, "存量工作区的 removed_node_id 必须是 NULL");
        let acp: Option<String> = c
            .query_row(
                "SELECT acp_session_id FROM session WHERE id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(acp, None, "存量会话没有 acp_session_id");
    }

    /// **v3 去外键的验收点**：删设备现在真的能执行，且不连带删会话。
    ///
    /// 这是用户裁定「不应该用外链」的直接后果，也是 v2 做不到的事：
    /// v2 里 `workspace.node_id` 是 `ON DELETE CASCADE`、`session.workspace_id`
    /// 是 `ON DELETE RESTRICT`，两条一起让「删一台有会话的设备」在**数据库层
    /// 根本执行不了**——先删 workspace 被 RESTRICT 拦，先删 session 又没人做。
    ///
    /// 这条测试不通过就说明迁移没生效（外键还在），而不是业务层有问题：
    /// 它只发一条裸 `DELETE FROM device`，绕开所有业务代码。
    #[test]
    fn deleting_a_device_keeps_its_sessions_because_the_foreign_keys_are_gone() {
        let mut c = memory();
        run(&mut c).unwrap();
        c.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES ('n_a', 'pi', 'fp_a', 'unix:0')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO workspace (id, node_id, path, created_at)
             VALUES ('w1', 'n_a', '/srv/app', 'unix:10')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO session (id, workspace_id, state, created_at)
             VALUES ('s1', 'w1', 'running', 'unix:11')",
            [],
        )
        .unwrap();

        // 外键开关仍然是 ON——去的是那两条外键，不是关掉这个开关
        let fk: i64 = c
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "PRAGMA foreign_keys 必须仍为 ON");

        c.execute("DELETE FROM device WHERE id='n_a'", []).unwrap();

        let sessions: i64 = c
            .query_row("SELECT count(*) FROM session WHERE id='s1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(sessions, 1, "删设备不得删会话（删除语义三分离）");
        let workspaces: i64 = c
            .query_row("SELECT count(*) FROM workspace WHERE id='w1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(workspaces, 1, "删设备不得删工作区");
    }

    /// 「`user_version` 是迁移唯一的闸门」这条不变式怎么被保住。
    ///
    /// 门本身**可被伪造**：手工改库、从别的版本的文件灌进来都会让它说谎。门一
    /// 失效，v2 就会撞上 `ALTER TABLE` 的 `duplicate column name`（SQLite 里唯一
    /// 不支持 `IF NOT EXISTS` 的 DDL），整条 v2 回滚。
    ///
    /// 所以真正的口径是：**v2 从来不会被执行第二次**，`run()` 第二次是空操作；
    /// 而门一旦说谎，`run()` 必须在动库之前就报 `SchemaTooNew`，而不是带着
    /// 一个半截的库继续跑。两条一起才是「可重复执行」。
    #[test]
    fn the_migration_gate_holds_and_its_lie_is_refused_before_touching_anything() {
        let mut c = memory();
        run(&mut c).unwrap();
        c.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at, public_key)
             VALUES ('n_a', 'pi', 'fp', 'unix:0', 'AAAA')",
            [],
        )
        .unwrap();

        // ① 第二次 run 是空操作（门生效）
        run(&mut c).unwrap();
        assert_eq!(
            c.query_row("SELECT public_key FROM device WHERE id='n_a'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "AAAA"
        );

        // ② 门说谎 → 拒绝，不动库
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let e = run(&mut c).unwrap_err();
        assert!(matches!(e, StorageError::SchemaTooNew { .. }), "got {e:?}");
        assert_eq!(
            c.query_row("SELECT public_key FROM device WHERE id='n_a'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "AAAA",
            "被拒绝的 open 不得改动任何数据"
        );
    }

    /// `user_version` 是迁移唯一的闸门，而它是**可被伪造**的：手工改库、
    /// 从别的版本的文件灌进来都会让它说谎。门一失效，`ALTER TABLE` 就会撞上
    /// 「duplicate column name」，整条 v2 回滚（这正是幂等性靠 `user_version`
    /// 而不靠 DDL 本身的原因）。
    ///
    /// 所以门失效时的正确反应是**报错并停在原处**，而不是继续跑迁移。
    #[test]
    fn a_mismatched_user_version_stops_the_migration() {
        let mut c = memory();
        run(&mut c).unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();

        let e = run(&mut c).unwrap_err();
        assert!(
            matches!(e, StorageError::Migration { version: 2, .. }),
            "got {e:?}"
        );
        // 迁移整体回滚：列还在（v1 之外的什么都没做一半），版本号也没前进
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "失败的迁移不得前进版本号"
        );
        let col: i64 = c
            .query_row(
                "SELECT count(*) FROM pragma_table_info('device') WHERE name='public_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(col, 1, "v2 已经跑过一次，列在库里");
    }

    /// v4：两列都加上，且**存量行**的 `base_rev_status` 必须是 `'unknown'`。
    ///
    /// 这条不是「列在不在」，而是**默认值对不对**：`ALTER TABLE ADD COLUMN`
    /// 会把 DEFAULT 填给存量行。若默认值写成 `'ok'`，所有 v0.6.0 之前建的
    /// 会话都会声称「我的起点是 0x…」，而 `base_rev` 是 NULL——一个看起来
    /// 正常、实际毫无意义的谎话。
    #[test]
    fn migration_v4_adds_columns_with_unknown_for_existing_rows() {
        let mut c = memory();
        // 逐个跑到 v3：模拟一个 v0.6.0 之前就存在、里面有数据的库。
        // 这里直接内联 v1..v3 的 SQL（与 `run` 同一批 `MIGRATIONS`，只是提前停）。
        for m in MIGRATIONS.iter().filter(|m| m.version <= 3) {
            let tx = c.transaction().unwrap();
            tx.execute_batch(m.sql).unwrap();
            tx.pragma_update(None, "user_version", m.version).unwrap();
            tx.commit().unwrap();
        }
        // 在 v3 的形状里插一条会话（此时还没有那两列）。
        c.execute_batch(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                 VALUES ('n_a', 'pi', 'fp_a', 'unix:0');
             INSERT INTO workspace (id, node_id, path, created_at)
                 VALUES ('w_a', 'n_a', '/srv/app', 'unix:0');
             INSERT INTO session (id, workspace_id, agent_id, state, created_at)
                 VALUES ('s_old', 'w_a', 'opencode', 'running', 'unix:0');",
        )
        .unwrap();

        run(&mut c).unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            4,
            "必须升到 v4"
        );

        let (rev, status): (Option<String>, String) = c
            .query_row(
                "SELECT base_rev, base_rev_status FROM session WHERE id = 's_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rev, None, "老会话没有起点");
        assert_eq!(
            status, "unknown",
            "老会话必须是 unknown（「我不知道」），不是 ok"
        );
    }

    /// v4 幂等：闸门生效，连跑两次不报 `duplicate column name`。
    ///
    /// `ALTER TABLE ADD COLUMN` **没有** `IF NOT EXISTS` 形式，重复执行会直接
    /// 报错。所以这条测的是 `user_version` 闸门，而不是 DDL 本身宽容。
    #[test]
    fn migration_v4_is_idempotent() {
        let mut c = memory();
        run(&mut c).unwrap();
        run(&mut c).unwrap();
        run(&mut c).unwrap();
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            4
        );
        for name in ["base_rev", "base_rev_status"] {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('session') WHERE name=?1",
                    [name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "session.{name} 只能有一列，不能重复加");
        }
    }
}
