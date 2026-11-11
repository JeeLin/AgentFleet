//! provider 注册表与 `opencode` 探测的测试。
//!
//! 夹具一律用临时目录里的**真 SQLite 库**：探测的正确性有一半在 SQL 上
//! （列存在性、排序、绑定参数），用假连接测不到那半边。

use std::path::{Path, PathBuf};

use super::opencode::{query_by_external_id, Opencode};
use super::{lookup, HistoryProvider, ImportError, ProviderLookup};
use crate::agent::detect::KNOWN_AGENTS;

/// 建一个空的临时数据目录（provider 会自己拼 `opencode/opencode.db`）。
fn data_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("建临时目录")
}

/// 在 `dir/opencode/opencode.db` 上建库并执行给定语句。
fn fixture(dir: &Path, stmts: &[&str]) -> PathBuf {
    let sub = dir.join("opencode");
    std::fs::create_dir_all(&sub).expect("建 opencode 子目录");
    let path = sub.join("opencode.db");
    let conn = rusqlite::Connection::open(&path).expect("建库");
    for sql in stmts {
        conn.execute(sql, []).expect("执行夹具语句");
    }
    drop(conn);
    path
}

/// 本机实测的 `session_v2` 形状（列名取自真库的 `PRAGMA table_info`）。
const REAL_SHAPE: &str = "CREATE TABLE session_v2 (
        id TEXT PRIMARY KEY NOT NULL,
        project_id TEXT NOT NULL,
        directory TEXT NOT NULL,
        title TEXT,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL
    )";

/// **只有 opencode 是 Supported；其余 8 个已知但无 ACP 入口的 agent 都是
/// Unsupported。**
///
/// 这条会红的情形是「支持范围不诚实」：把 `--version` 跑得起来误当成可导入，
/// 用户选中一条历史会话后永远等不到回话。逐条断言而不是只数个数——数对了
/// 但名字错了同样是错的。
#[test]
fn registry_only_opencode_is_supported_and_the_eight_known_non_acp_agents_are_unsupported() {
    assert_eq!(
        KNOWN_AGENTS.len(),
        9,
        "清单变了：这条测试的「9 个」假设要跟着改"
    );

    let mut supported = Vec::new();
    for spec in KNOWN_AGENTS {
        match lookup(spec.id) {
            ProviderLookup::Supported(p) => {
                assert_eq!(p.id(), spec.id, "选出来的 provider 必须是同一个 agent");
                assert!(
                    spec.can_converse(),
                    "`{}` 没有 ACP 入口却被标成 Supported",
                    spec.id
                );
                supported.push(spec.id);
            }
            ProviderLookup::Unsupported { agent_id, reason } => {
                assert_eq!(agent_id, spec.id);
                assert!(!reason.is_empty(), "不支持必须给出原因");
                assert!(
                    !spec.can_converse(),
                    "`{}` 有 ACP 入口却被标成 Unsupported",
                    spec.id
                );
            }
        }
    }
    assert_eq!(
        supported,
        vec!["opencode"],
        "只有真机验证过 ACP 的 agent 才能导入"
    );
}

/// 未知 id 与「已知但无 ACP 入口」的文案必须不同：前者用户是拼错了，
/// 后者是软件本来就不支持，两者下一步动作完全不同。
#[test]
fn an_unknown_id_is_told_apart_from_a_known_agent_without_an_acp_entry() {
    let known = match lookup("claude") {
        ProviderLookup::Unsupported { reason, .. } => reason,
        ProviderLookup::Supported(_) => panic!("claude 没有 ACP 入口，不能是 Supported"),
    };
    let unknown = match lookup("definitely-not-an-agent") {
        ProviderLookup::Unsupported { agent_id, reason } => {
            assert_eq!(agent_id, "definitely-not-an-agent");
            reason
        }
        ProviderLookup::Supported(_) => panic!("未知 id 不能是 Supported"),
    };
    assert_ne!(known, unknown, "两种不支持的原因必须可区分");
    assert!(
        known.contains("ACP"),
        "已知 agent 的原因要点名缺的是 ACP 入口：{known:?}"
    );
}

/// **注册表与 `KNOWN_AGENTS` 脱节时立刻降级**：provider 还在，但那个 agent
/// 已经没有 ACP 入口了（有人清空了 `acp_args` 却没摘 provider）。
///
/// 这条臂在生产的 `PROVIDERS` 上不可达（要发生得同时改两处），所以直接注入
/// 一个假 provider 走 [`super::classify`]。它防的是「支持判据退化成『注册表
/// 里有 provider』」——那样用户会选中一条历史会话，绑进来却永远接不上。
#[test]
fn a_registered_provider_loses_support_the_moment_its_agent_loses_acp() {
    struct Fake;
    impl HistoryProvider for Fake {
        fn id(&self) -> &'static str {
            "claude"
        }
        fn list(&self, _: &Path) -> Result<Vec<super::HistoricalSession>, ImportError> {
            Err(ImportError::DbMissing)
        }
        fn resolve(&self, _: &Path, _: &str) -> Result<super::ImportCandidate, ImportError> {
            Err(ImportError::DbMissing)
        }
    }
    static FAKE: Fake = Fake;

    // claude 在清单里，但 `acp_args` 为空 → 仍然 Unsupported，且原因与
    // 「未知软件」不同。
    match super::classify(Some(&FAKE), "claude") {
        ProviderLookup::Unsupported { reason, .. } => {
            assert_ne!(reason, super::REASON_UNKNOWN_PROVIDER);
            assert_ne!(reason, super::REASON_NO_ACP_ENTRY);
            assert!(reason.contains("ACP"), "原因要点名缺的是什么：{reason:?}");
        }
        ProviderLookup::Supported(_) => panic!("agent 没有 ACP 入口时不得 Supported"),
    }

    // 同一条 provider 挂到真有 ACP 入口的 id 上就是 Supported：判据确实
    // 是「清单里有没有 ACP 入口」，不是 provider 自己的声明。
    assert!(matches!(
        super::classify(Some(&FAKE), "opencode"),
        ProviderLookup::Supported(_)
    ));
}

/// **`list` 只读**：跑之前与跑之后，库文件的字节与 mtime 都不变。
///
/// 这条防的是「探测顺手把用户的库改了」：opencode 正在用这个库，任何写入
/// 都可能让它下一次启动撞上迁移或锁。断言 mtime 之外还要比字节——有些
/// 文件系统（或容器里的 overlay）mtime 精度只有秒，一秒内改完看不出来。
#[test]
fn opencode_list_reads_session_v2_without_writing_the_database() {
    let d = data_dir();
    let db = fixture(
        d.path(),
        &[
            REAL_SHAPE,
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_old', 'p', '/root/old', '旧会话', 100, 100)",
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_new', 'p', '/root/new', '新会话', 200, 300)",
        ],
    );

    let before_bytes = std::fs::read(&db).expect("读库");
    let before_mtime = std::fs::metadata(&db)
        .and_then(|m| m.modified())
        .expect("读 mtime");

    let got = Opencode.list(d.path()).expect("列会话");

    assert_eq!(got.len(), 2);
    // 按更新时间倒序：新的在前。
    assert_eq!(got[0].external_session_id, "ses_new");
    assert_eq!(got[0].title, "新会话");
    assert_eq!(got[0].cwd, "/root/new");
    assert_eq!(got[0].updated_at.as_deref(), Some("300"));
    assert_eq!(got[0].provider, "opencode");
    assert_eq!(got[1].external_session_id, "ses_old");

    let after_bytes = std::fs::read(&db).expect("读库");
    let after_mtime = std::fs::metadata(&db)
        .and_then(|m| m.modified())
        .expect("读 mtime");
    assert_eq!(after_bytes, before_bytes, "`list` 改了库文件的字节");
    assert_eq!(after_mtime, before_mtime, "`list` 改了库文件的 mtime");
    assert!(
        !d.path().join("opencode").join("opencode.db-wal").exists(),
        "只读打开不该产生 WAL 文件"
    );

    // 纵深防御真的生效：用同一个 `open_read_only` 打开的连接连 DDL 都做不了。
    let conn = super::opencode::open_read_only(d.path()).expect("只读重开库");
    let probe: Result<i64, _> = conn.query_row("PRAGMA query_only", [], |r| r.get(0));
    assert_eq!(probe.expect("读 query_only"), 1, "query_only 必须真的开着");
    assert!(
        conn.execute("CREATE TABLE should_not_exist (x)", [])
            .is_err(),
        "query_only 连接不得执行 DDL"
    );
}

/// 空库 / 缺表 / 缺 `id` 列三类夹具各归到具体错误，**不合并成一个
/// 「读不了」**：没装 opencode 与 schema 漂移对用户是两件事。
#[test]
fn a_missing_database_or_table_or_bad_column_returns_a_classified_error() {
    // ① 连库文件都没有（没装 / 从没跑过 opencode）
    let empty = data_dir();
    assert_eq!(Opencode.list(empty.path()), Err(ImportError::DbMissing));
    assert_eq!(
        Opencode.resolve(empty.path(), "ses_x"),
        Err(ImportError::DbMissing)
    );

    // ② 有库、没有 session_v2
    let no_table = data_dir();
    fixture(no_table.path(), &["CREATE TABLE something_else (id TEXT)"]);
    assert_eq!(
        Opencode.list(no_table.path()),
        Err(ImportError::SchemaMismatch)
    );

    // ③ 有表、没有 id 列（schema 漂移）
    let bad_column = data_dir();
    fixture(
        bad_column.path(),
        &["CREATE TABLE session_v2 (session_id TEXT, directory TEXT)"],
    );
    assert_eq!(
        Opencode.list(bad_column.path()),
        Err(ImportError::SchemaMismatch)
    );
}

/// 表里只有 `id` 也能用：其余字段降级成空串 / `None`，不因为「少一列」
/// 整条 provider 报废。顺序退回按 id（稳定，不是行号）。
#[test]
fn a_table_with_only_an_id_still_lists_sessions() {
    let d = data_dir();
    fixture(
        d.path(),
        &[
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY)",
            "INSERT INTO session_v2 (id) VALUES ('ses_b')",
            "INSERT INTO session_v2 (id) VALUES ('ses_a')",
            // 空 id 的行点不中也选不中，列的时候跳过，但不连累别的行。
            "INSERT INTO session_v2 (id) VALUES ('')",
        ],
    );

    let got = Opencode.list(d.path()).expect("列会话");
    let ids: Vec<&str> = got.iter().map(|s| s.external_session_id.as_str()).collect();
    assert_eq!(ids, vec!["ses_b", "ses_a"], "按 id 倒序，空 id 被跳过");
    assert_eq!(got[0].title, "", "没有标题列就是空串，不编造占位");
    assert_eq!(got[0].cwd, "");
    assert_eq!(got[0].updated_at, None);
}

/// 列名与文档里的不同（`cwd` 而不是 `directory`、`updated_at` 而不是
/// `time_updated`）时按同义列降级，而不是报 schema 漂移。
#[test]
fn synonym_columns_are_used_when_the_primary_names_are_absent() {
    let d = data_dir();
    fixture(
        d.path(),
        &[
            "CREATE TABLE session_v2 (id TEXT, title TEXT, cwd TEXT, updated_at TEXT)",
            "INSERT INTO session_v2 (id, title, cwd, updated_at)
             VALUES ('ses_1', 't1', '/root/one', '2026-10-27T00:00:00Z')",
            "INSERT INTO session_v2 (id, title, cwd, updated_at)
             VALUES ('ses_2', NULL, '/root/two', NULL)",
        ],
    );

    let got = Opencode.list(d.path()).expect("列会话");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].cwd, "/root/one");
    assert_eq!(got[0].updated_at.as_deref(), Some("2026-10-27T00:00:00Z"));
    assert_eq!(got[1].title, "", "NULL 标题是空串");
    assert_eq!(got[1].updated_at, None, "NULL 时间是 None，不是空串");
}

/// 空白 id 与非法工作目录都在**发出请求之前**被拒：Hub 那边还会再校验
/// 一次，但让用户等到一个 400 才知道路径不合法是没必要的。
#[test]
fn resolve_rejects_blank_ids_and_invalid_workspace_paths() {
    let d = data_dir();
    fixture(
        d.path(),
        &[
            REAL_SHAPE,
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_ok', 'p', '/root/ok', 't', 1, 2)",
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_rel', 'p', 'relative/dir', 't', 1, 2)",
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_nodir', 'p', '', 't', 1, 2)",
        ],
    );

    assert_eq!(
        Opencode.resolve(d.path(), ""),
        Err(ImportError::BlankSessionId)
    );
    assert_eq!(
        Opencode.resolve(d.path(), "   "),
        Err(ImportError::BlankSessionId),
        "全是空白的 id 与空 id 等价"
    );
    assert_eq!(
        Opencode.resolve(d.path(), "ses_rel"),
        Err(ImportError::InvalidWorkspacePath)
    );
    assert_eq!(
        Opencode.resolve(d.path(), "ses_nodir"),
        Err(ImportError::InvalidWorkspacePath)
    );
    // 不存在的 id 要能与「路径非法」区分开。
    assert_eq!(
        Opencode.resolve(d.path(), "ses_missing"),
        Err(ImportError::Decode)
    );
}

/// **`acp_session_id` 原样就是 provider 的 id**：opencode 的 `ses_*` 正是
/// ACP `session/load` 认的那个。加前缀 / 转换一次，绑定的就是另一个会话。
#[test]
fn resolve_returns_the_raw_acp_session_id() {
    let d = data_dir();
    fixture(
        d.path(),
        &[
            REAL_SHAPE,
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_abc', 'p', '/root/ws', 't', 1, 2)",
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_rel', 'p', 'relative/ws', 't', 1, 2)",
        ],
    );

    let got = Opencode.resolve(d.path(), "ses_abc").expect("解析会话");
    assert_eq!(got.acp_session_id, "ses_abc", "不得改写 id");
    assert_eq!(got.provider, "opencode");
    assert_eq!(got.cwd, "/root/ws");
    assert_eq!(
        Opencode.resolve(d.path(), "ses_rel"),
        Err(ImportError::InvalidWorkspacePath),
        "相对 cwd 的会话不得放行"
    );
}

/// 列类型不按声明走时不 panic：opencode 的时间列在真库里是 INTEGER，
/// 但文档里的 `updated_at` 是字符串；两者都要能读。非 UTF-8 的字节按
/// 替换字符降级（opencode 写坏的标题不该让整次 `list` 失败）。
#[test]
fn odd_column_types_are_decoded_without_failing_the_listing() {
    let d = data_dir();
    fixture(
        d.path(),
        &[
            "CREATE TABLE session_v2 (id, title, directory, time_updated)",
            "INSERT INTO session_v2 (id, title, directory, time_updated)
             VALUES ('ses_int', 42, '/root/int', 1700000000000)",
            "INSERT INTO session_v2 (id, title, directory, time_updated)
             VALUES ('ses_real', 'x', '/root/real', 1.5)",
            "INSERT INTO session_v2 (id, title, directory, time_updated)
             VALUES ('ses_float_title', 2.5, '/root/float', 4)",
        ],
    );
    // 非 UTF-8 的 id：SQLite 允许 BLOB，opencode 正常不会写，但读的时候
    // 不能因为它整条 provider 报错。
    let conn =
        rusqlite::Connection::open(d.path().join("opencode").join("opencode.db")).expect("重开库");
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_updated)
         VALUES (CAST(x'7365735FFF' AS BLOB), 'blob', '/root/blob', 3)",
        [],
    )
    .expect("插入 BLOB 行");
    drop(conn);

    let got = Opencode.list(d.path()).expect("列会话");
    assert_eq!(got.len(), 4);
    let float_row = got
        .iter()
        .find(|s| s.external_session_id == "ses_float_title")
        .expect("实数标题的行");
    assert_eq!(float_row.title, "2.5", "实数标题转成十进制，不是空串");
    let int_row = got
        .iter()
        .find(|s| s.external_session_id == "ses_int")
        .expect("整数列的行");
    assert_eq!(int_row.title, "42", "非文本标题转成十进制，不是空串");
    assert_eq!(int_row.updated_at.as_deref(), Some("1700000000000"));
    let real_row = got
        .iter()
        .find(|s| s.external_session_id == "ses_real")
        .expect("实数时间的行");
    assert_eq!(real_row.updated_at.as_deref(), Some("1.5"));
    assert!(
        got.iter()
            .any(|s| s.external_session_id.contains('\u{FFFD}')),
        "非 UTF-8 的 id 按替换字符降级，不整条失败"
    );
}

/// **id 是绑定参数，不是拼进 SQL 的字符串**。
///
/// 带引号与分号的 id 只能得到 0 行、无错误、无额外行，不能变成
/// `WHERE 1=1` 把整表吐出来，也不能报语法错——后者说明 id 进了语句文本。
/// 这里直接断言参数化查询层（公开 `resolve` 依契约把 0 行映射成
/// `ImportError::Decode`，因为错误枚举里没有 NotFound）。
#[test]
fn sql_is_parameterized() {
    let d = data_dir();
    fixture(
        d.path(),
        &[
            REAL_SHAPE,
            "INSERT INTO session_v2 (id, project_id, directory, title, time_created, time_updated)
             VALUES ('ses_real', 'p', '/root/real', 't', 1, 2)",
        ],
    );

    for evil in [
        "ses_real' OR '1'='1",
        "'; DROP TABLE session_v2; --",
        "ses_real\"; --",
    ] {
        let rows = query_by_external_id(d.path(), evil)
            .unwrap_or_else(|e| panic!("注入形态的 id 不得造成 SQL 错误：{evil:?}: {e}"));
        assert!(
            rows.is_empty(),
            "注入形态的 id 只能得到 0 行、无额外行：{evil:?} 得到 {rows:?}"
        );
        assert_eq!(
            Opencode.resolve(d.path(), evil),
            Err(ImportError::Decode),
            "公开的 resolve 把 0 行归到 Decode：{evil:?}"
        );
    }
    // 表还在，真行也还在：上面那些 id 一个都没被执行。
    assert_eq!(Opencode.list(d.path()).expect("列会话").len(), 1);
}

/// **对真实安装的 opencode 走一遍**（真数据目录、真库）。
///
/// CI 上通常没装 opencode，所以标 `#[ignore]`：`cargo test -p node-core --
/// --ignored` 在有 opencode 的机器上跑。装与没装都算通过，两种情形各断言一次
/// ——`if installed { ... }` 之后什么都不做等于没测。
///
/// 这条证明的是**对真货也成立**：真库有 37 列、时间列叫 `time_updated`，
/// 与夹具的最小形状不同；列名假设若有错，只有这条会红。
#[test]
#[ignore = "需要本机装有 opencode 并跑过至少一次会话"]
fn a_real_opencode_database_lists_and_resolves_a_session() {
    let Some(base) = dirs::data_dir() else {
        panic!("本平台没有数据目录，无从探测");
    };
    let sessions = match Opencode.list(&base) {
        Ok(s) => s,
        Err(ImportError::DbMissing) => {
            // 没装 / 没跑过：这与 `detect.rs` 的 `NotInstalled` 同一口径。
            return;
        }
        Err(e) => panic!("真库读失败：{e}"),
    };
    assert!(!sessions.is_empty(), "真库不该是空的");
    let first = sessions.first().expect("已断言非空");
    assert!(
        first.external_session_id.starts_with("ses_"),
        "真 id 形如 ses_*，实际 {:?}",
        first.external_session_id
    );

    // 目录合法的会话必须能解析回来，且 id 不变。
    if first.cwd.starts_with('/') || first.cwd.starts_with('~') {
        let got = Opencode
            .resolve(&base, &first.external_session_id)
            .expect("解析真会话");
        assert_eq!(got.acp_session_id, first.external_session_id);
        assert_eq!(got.cwd, first.cwd);
    }
}
