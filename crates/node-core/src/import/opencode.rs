//! `opencode` 的历史会话 provider。
//!
//! 只读打开 `<data_dir>/opencode/opencode.db`（Linux 上是
//! `~/.local/share/opencode/opencode.db`），查 `session_v2`。**不写、不建表、
//! 不迁移**：那个库属于用户正在用的 opencode，任何写入都可能让它下一次启动
//! 撞上迁移或锁。
//!
//! # 为什么不写死列名
//!
//! `session_v2` 的列随 opencode 版本变（本机实测 37 列，含 `time_created` /
//! `time_updated`）。写死 `SELECT id, title, directory, updated_at` 在它改一列
//! 名字之后**整条 provider 直接报错**，而用户会以为是我们坏了。所以先读
//! `PRAGMA table_info(session_v2)`，按**列名**映射；只有 `id` 是必需的，
//! 其余按存在性降级（`title` 缺失 → 空串，时间列缺失 → falls back to id DESC，
//! `updated_at` is None）。见 [`Columns::order_by`] 与 `list()` 的 `ORDER BY`。
//!
//! 拼进 SQL 的列名**只来自本文件的常量**：`table_info` 读回来的名字只用于
//! 「这个常量在不在表里」的比对，绝不把库里的字符串拼进语句。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, Row, TransactionBehavior};

use super::{HistoricalSession, ImportCandidate, ImportError};

/// 数据目录下的 opencode 子目录名。
const DATA_SUBDIR: &str = "opencode";
/// opencode 的会话库文件名。
const DB_FILE: &str = "opencode.db";
/// 会话表名。opencode v2 起是这个（旧版 `session` 表已不是真相源）。
const TABLE: &str = "session_v2";

const ID_COL: &str = "id";
const TITLE_COL: &str = "title";
/// 工作目录列：v2 叫 `directory`。
const DIRECTORY_COL: &str = "directory";
/// 同义列名（只在 `directory` 缺失时才用）。
const CWD_COL: &str = "cwd";
/// 更新时间列，按优先级尝试。
const UPDATED_AT_COL: &str = "updated_at";
const UPDATED_COL: &str = "updated";
/// 时间列全都缺失时的兜底：`time_created` 也在，但只作为排序依据的次选。
const CREATED_AT_COL: &str = "created_at";
const TIME_CREATED_COL: &str = "time_created";
const TIME_UPDATED_COL: &str = "time_updated";
/// 列表一次最多读多少行。
///
/// `afcli import list` 是给人挑一条会话用的，屏幕上放不下无限行；而
/// `query_map` 会把整张 `session_v2` 连同 `title` / `directory` 文本收进
/// `Vec`，行数到百万级就是 O(行数) 的峰值内存。上限把用户可见列表长度与
/// 库里历史总量解耦。写成 `&str` 是因为它只作为 SQL 字面量拼进语句
/// （`LIMIT` 不接受绑定参数那种写法之外的形式也可以，但字面量编译期可见、
/// 测试也直接读这个常量）。
pub(super) const LIST_LIMIT: &str = "500";

/// provider id，与 `KNOWN_AGENTS` 里 opencode 的 `id` 同源。
const PROVIDER_ID: &str = "opencode";

/// 本 provider 的唯一实例（注册表里存 `&'static`）。
pub static OPENCODE: Opencode = Opencode;

/// `opencode` 的历史会话 provider。
#[derive(Debug, Clone, Copy)]
pub struct Opencode;

impl super::HistoryProvider for Opencode {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn list(&self, data_dir: &Path) -> Result<Vec<HistoricalSession>, ImportError> {
        list_rows(data_dir, None)
    }

    fn resolve(
        &self,
        data_dir: &Path,
        external_session_id: &str,
    ) -> Result<ImportCandidate, ImportError> {
        if external_session_id.trim().is_empty() {
            return Err(ImportError::BlankSessionId);
        }
        // 0 行不是一种独立的公开错误（`ImportError` 没有 NotFound）：用户点到了
        // 一条不存在的会话，按「这一行读不出来」归到 Decode，不把库里的字符串
        // 带到错误里。
        let row = query_by_external_id(data_dir, external_session_id)?
            .into_iter()
            .next()
            .ok_or(ImportError::Decode)?;
        validate_workspace_path(&row.cwd)?;
        Ok(ImportCandidate {
            provider: self.id(),
            // **原样**交出去：opencode 的 `ses_*` 就是 ACP `session/load` 认的 id。
            acp_session_id: row.external_session_id,
            cwd: row.cwd,
        })
    }

    fn session_meta(
        &self,
        data_dir: &Path,
        id: &str,
    ) -> Result<Option<HistoricalSession>, ImportError> {
        // 与 `resolve` 共用同一条单行参数化查询：`bind` 只要这一行的 cwd/title，
        // 不需要把整张 `session_v2` 读进内存。
        Ok(query_by_external_id(data_dir, id)?.into_iter().next())
    }
}

/// `list()` 与 `query_by_external_id()` 共用的读取事务。
///
/// # 为什么 `Columns::read` 与 `prepare` 必须在同一个 deferred 事务里
///
/// 两次调用之间若 opencode 正在迁移表结构（把 `session_v2` 改名 / 改列），
/// 第二次 `prepare` 会在**已经变了**的 schema 上执行：旧实现把它一律映射为
/// `Decode`，用户看到「a session row could not be read」，与真实原因无关。
/// `BEGIN DEFERRED` 在第一次读（`PRAGMA table_info`）时拿共享快照，后面的
/// `prepare` 看到的是同一份 schema；别的连接仍能读，写方要等我们这几次读结束。
/// 事务是**只读连接上的读事务**，不写、不建表、不迁移，符合里程碑边界。
///
/// 如果事务已经开始但 schema 还是不匹配（例如另一进程已经提交了迁移，我们才
/// 打开），`prepare` 失败要回 `SchemaMismatch` 而不是 `Decode`：那是「库形状
/// 变了」，不是「一行数据读不出来」。
///
/// `on_columns` 是测试接缝：让用例能在 `Columns::read` 返回后、`prepare` 前
/// 模拟一次迁移，验证上面的映射；生产调用一律传 `None`。
fn with_read_snapshot<T>(
    data_dir: &Path,
    on_columns: Option<&dyn Fn(&Path)>,
    build: impl FnOnce(&Connection, &Columns) -> Result<T, ImportError>,
) -> Result<T, ImportError> {
    let mut conn = open_read_only(data_dir)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(classify_sql_error)?;
    let cols = Columns::read(&tx)?;
    if let Some(hook) = on_columns {
        hook(data_dir);
    }
    let out = build(&tx, &cols)?;
    // 读事务：显式 rollback，绝不 `commit()`——连接本身是 READ_ONLY，但把
    // 「不写」写成结构上也成立更清楚，也让事务立刻释放读快照。
    tx.rollback().map_err(classify_sql_error)?;
    Ok(out)
}

/// 把 SQLite 的 prepare / 查询错误归类。
///
/// SQL 里的标识符全部来自本文件常量，唯一的动态因素是**库里的 schema**：
/// `no such table` / `no such column` / `has no column named` 都是「对方库正在
/// 迁移或漂了」→ `SchemaMismatch`；其余（真正的行解码失败、磁盘读错等）才是
/// `Decode`。不把 SQL / 原始 DB 字符串带到错误里（约束 13）。
pub(super) fn classify_sql_error(e: rusqlite::Error) -> ImportError {
    let text = e.to_string().to_ascii_lowercase();
    if text.contains("no such table")
        || text.contains("no such column")
        || text.contains("has no column named")
        || text.contains("no such column:")
    {
        ImportError::SchemaMismatch
    } else {
        ImportError::Decode
    }
}

/// 把 `PRAGMA table_info` 的错误归类。
///
/// 坏 SQLite 文件在**打开连接时不会立刻失败**：`Connection::open_with_flags`
/// 是惰性的，第一次读（这里就是 `PRAGMA table_info`）才报 `file is not a
/// database` / `database disk image is malformed`。那属于「这个库读不出来」
/// → `Decode`，不是「表形状不认识」→ `SchemaMismatch`；把两者混成同一个
/// 文案，正是里程碑点名「只读打开失败」要区分的那件事。
fn classify_schema_read_error(e: rusqlite::Error) -> ImportError {
    let text = e.to_string().to_ascii_lowercase();
    if text.contains("not a database")
        || text.contains("malformed")
        || text.contains("database corruption")
    {
        ImportError::Decode
    } else {
        classify_sql_error(e)
    }
}

/// `list()` 的实现。`on_columns` 只在测试里用来模拟「读完 schema 后表被改名」。
fn list_rows(
    data_dir: &Path,
    on_columns: Option<&dyn Fn(&Path)>,
) -> Result<Vec<HistoricalSession>, ImportError> {
    with_read_snapshot(data_dir, on_columns, |conn, cols| {
        let mut sql = select_prefix(cols);
        sql.push_str(" ORDER BY \"");
        sql.push_str(cols.order_by());
        sql.push_str("\" DESC");
        // 同一时间戳的多条会话按 id 定序，否则两次调用可能换顺序。
        // `DESC` 要逐列写：`ORDER BY a, b DESC` 只把 `b` 降序，`a` 仍是升序。
        if cols.order_by() != ID_COL {
            sql.push_str(", \"");
            sql.push_str(ID_COL);
            sql.push_str("\" DESC");
        }
        sql.push_str(" LIMIT ");
        sql.push_str(LIST_LIMIT);

        let mut stmt = conn.prepare(&sql).map_err(classify_sql_error)?;
        let rows = stmt
            .query_map([], |row| cols.to_session(row))
            .map_err(classify_sql_error)?;

        let mut out = Vec::new();
        for row in rows {
            let session = row.map_err(classify_sql_error)?;
            // 空 id 的行无法被选中（`resolve` 按 id 查），也点不出任何结果，
            // 所以跳过——但**不因为一行坏数据就让其余 1000 条消失**。
            if session.external_session_id.is_empty() {
                continue;
            }
            out.push(session);
        }
        Ok(out)
    })
}

/// 测试接缝：在 `Columns::read` 与 `prepare` 之间跑一次 `on_columns`。
///
/// 生产路径不会调用它；它的存在是为了让「schema 正在变更」这条竞态有可执行
/// 断言，而不是只写在注释里。
#[cfg(test)]
pub(super) fn list_with_schema_hook(
    data_dir: &Path,
    hook: &dyn Fn(&Path),
) -> Result<Vec<HistoricalSession>, ImportError> {
    list_rows(data_dir, Some(hook))
}

/// 按外部 id 查原始会话行（最多一行），只读、不写库。
///
/// 这是 `resolve` 与参数化测试共用的 crate 内 helper：把「0 行」保留成
/// `Ok(vec![])`，测试才能直接断言参数化 SQL 的「returns 0 rows, no error,
/// no extra rows」；公开的 `resolve` 仍按契约把 0 行归到 `ImportError::Decode`
/// （错误枚举里没有 NotFound）。SQL 里的 id 是绑定参数 `?1`，不是拼接文本。
pub(super) fn query_by_external_id(
    data_dir: &Path,
    external_session_id: &str,
) -> Result<Vec<HistoricalSession>, ImportError> {
    with_read_snapshot(data_dir, None, |conn, cols| {
        let mut sql = select_prefix(cols);
        sql.push_str(" WHERE \"");
        sql.push_str(ID_COL);
        sql.push_str("\" = ?1 LIMIT 1");

        let mut stmt = conn.prepare(&sql).map_err(classify_sql_error)?;
        let rows = stmt
            .query_map([external_session_id], |row| cols.to_session(row))
            .map_err(classify_sql_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(classify_sql_error)?);
        }
        Ok(out)
    })
}

/// 只读打开会话库。
///
/// 先判文件存在再 open：`SQLITE_CANTOPEN` 会把「没装 / 没跑过 opencode」与
/// 「文件坏了」混成同一个错误，而这两种情形对用户的下一步完全不同。
/// `query_only` 是纵深防御——`READ_ONLY` 已经挡住了写，这里再声明一次，
/// 免得将来有人把 flag 改宽了却没人发现。
pub(super) fn open_read_only(data_dir: &Path) -> Result<Connection, ImportError> {
    let path = db_path(data_dir);
    if !path.is_file() {
        return Err(ImportError::DbMissing);
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ImportError::Decode)?;
    // 用字面 `ON`（静态 SQL，不含任何库路径 / 列名 / 用户输入）：整数 1 虽等价，
    // 但设计要求里的语句就是这一条。
    conn.execute_batch("PRAGMA query_only = ON")
        .map_err(|_| ImportError::Decode)?;
    Ok(conn)
}

/// 会话库位置。`data_dir` 由调用方给（生产是平台数据目录，测试是临时目录）。
fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DATA_SUBDIR).join(DB_FILE)
}

/// `SELECT "id", ... FROM "session_v2"`——拼进去的标识符**全部来自本文件的
/// 常量**（`Columns` 里存的是 `&'static str`），库里的字符串只用于比对存在性。
fn select_prefix(cols: &Columns) -> String {
    let mut sql = String::from("SELECT ");
    for (i, name) in cols.selected().iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('"');
        sql.push_str(name);
        sql.push('"');
    }
    sql.push_str(" FROM \"");
    sql.push_str(TABLE);
    sql.push('"');
    sql
}

/// 从 `session_v2` 实际读到的列名里挑出要用的那几个。
struct Columns {
    title: Option<&'static str>,
    cwd: Option<&'static str>,
    updated: Option<&'static str>,
}

impl Columns {
    /// 读 `PRAGMA table_info`。表不存在时返回 0 行，于是 `id` 缺失 →
    /// `SchemaMismatch`；不另查 `sqlite_master`，少一条口径。
    fn read(conn: &Connection) -> Result<Self, ImportError> {
        let sql = format!("PRAGMA table_info(\"{TABLE}\")");
        let mut stmt = conn.prepare(&sql).map_err(classify_schema_read_error)?;
        let mut names: Vec<String> = Vec::new();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(classify_schema_read_error)?;
        for name in rows {
            names.push(name.map_err(classify_schema_read_error)?);
        }
        let has = |c: &str| names.iter().any(|n| n == c);
        if !has(ID_COL) {
            return Err(ImportError::SchemaMismatch);
        }
        Ok(Self {
            title: has(TITLE_COL).then_some(TITLE_COL),
            cwd: if has(DIRECTORY_COL) {
                Some(DIRECTORY_COL)
            } else {
                has(CWD_COL).then_some(CWD_COL)
            },
            // `updated_at` 是文档里的名字；本机实测列叫 `time_updated`。
            // 按「最像更新时间」的顺序挑，全都缺失时退回创建时间。
            updated: [
                UPDATED_AT_COL,
                UPDATED_COL,
                TIME_UPDATED_COL,
                CREATED_AT_COL,
                TIME_CREATED_COL,
            ]
            .into_iter()
            .find(|c| has(c)),
        })
    }

    /// `SELECT` 的列（顺序与 [`Columns::to_session`] 的取列下标一致）。
    fn selected(&self) -> Vec<&'static str> {
        let mut cols = vec![ID_COL];
        cols.extend(self.title);
        cols.extend(self.cwd);
        cols.extend(self.updated);
        cols
    }

    /// 排序依据。没有时间列时按 id 排——id 带随机后缀，顺序不完美，
    /// 但**稳定**；按行号排会在插入后整体漂移。
    fn order_by(&self) -> &'static str {
        match self.updated {
            Some(col) => col,
            None => ID_COL,
        }
    }

    /// 一行的下标布局：`id` 恒在 0，其余按 `selected()` 里的出现顺序。
    fn to_session(&self, row: &Row<'_>) -> rusqlite::Result<HistoricalSession> {
        let external_session_id = text_at(row, 0)?;
        let mut next = 1;
        let mut take = |present: bool| {
            if present {
                let i = next;
                next += 1;
                Some(i)
            } else {
                None
            }
        };
        let title = match take(self.title.is_some()) {
            Some(i) => text_at(row, i)?,
            None => String::new(),
        };
        let cwd = match take(self.cwd.is_some()) {
            Some(i) => text_at(row, i)?,
            None => String::new(),
        };
        let updated_at = match take(self.updated.is_some()) {
            Some(i) => sort_value_at(row, i)?,
            None => None,
        };
        Ok(HistoricalSession {
            provider: PROVIDER_ID,
            external_session_id,
            title,
            cwd,
            updated_at,
        })
    }
}

/// 取一列的文本；NULL 视为空串（`title` / `directory` 在 opencode 里可空）。
fn text_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<String> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => String::new(),
        // 列的实际类型不按声明走时也不 panic：整数直接转成十进制，非 UTF-8
        // 的字节按替换字符降级（opencode 写坏的一列不该让整次 `list` 失败）。
        ValueRef::Text(b) | ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
    })
}

/// 时间列的原样值：opencode 存的是 Unix 毫秒整数，文档里的 `updated_at`
/// 形态则是字符串。两者都照原样转成字符串，**不在这一层做格式化**——
/// 时间怎么显示是 CLI 的事。非 UTF-8 的文本用 [`String::from_utf8_lossy`]
/// 降级（opencode 写坏的一列不该让整次 `list` 失败）。
fn sort_value_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(b) | ValueRef::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
    })
}

/// 工作目录必须绝对或以 `~` 开头。
///
/// 与 `hub_storage::WorkspaceRepo::validate_path` 同口径，但**在这里重写一份**：
/// `node-core` 不能依赖 `hub-storage`（那是 Hub 侧的库，会把 SQLite 池与迁移
/// 一起带进设备二进制）。设备侧先做一次是为了在发请求前就告诉用户「这条会话
/// 的目录不能用」；Hub 仍然做最终校验，两边不互为替代。
fn validate_workspace_path(cwd: &str) -> Result<(), ImportError> {
    if cwd.starts_with('/') || cwd.starts_with('~') {
        return Ok(());
    }
    Err(ImportError::InvalidWorkspacePath)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_database_path_is_the_opencode_subdirectory_of_the_given_data_dir() {
        let p = db_path(Path::new("/tmp/xdg"));
        assert_eq!(p, Path::new("/tmp/xdg/opencode/opencode.db"));
    }

    #[test]
    fn a_workspace_path_must_be_absolute_or_home_relative() {
        assert!(validate_workspace_path("/root/workspace/AgentFleet").is_ok());
        assert!(validate_workspace_path("~/work").is_ok());
        assert_eq!(
            validate_workspace_path("relative/path"),
            Err(ImportError::InvalidWorkspacePath)
        );
        assert_eq!(
            validate_workspace_path(""),
            Err(ImportError::InvalidWorkspacePath)
        );
        assert_eq!(
            validate_workspace_path("C:\\Users\\x"),
            Err(ImportError::InvalidWorkspacePath),
            "Windows 盘符路径不满足 Unix 口径；opencode 跑在同一台设备上，两边一致"
        );
    }
}
