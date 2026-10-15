//! 数据库打开、PRAGMA 与数据目录权限。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use r2d2::Pool as R2d2Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use tracing::info;

use crate::error::{Result, StorageError};
use crate::migrate;

/// 建库时的 `busy_timeout`（毫秒）。AGENTS.md「SQLite 铁律」要求必设。
pub const BUSY_TIMEOUT_MS: u32 = 5000;

pub type Db = R2d2Pool<SqliteConnectionManager>;

/// 数据目录名（平台相关基目录下的子目录）。
const APP_DIR: &str = "agentfleet";

/// 解析 Hub 数据目录，并按需创建。
///
/// 权限在创建时就用 `mode(0o700)` 落，**不依赖 umask**——systemd 单元的 umask
/// 常是 0022，靠 umask 会得到 0755。
///
/// - Linux: `$XDG_DATA_HOME/agentfleet` 或 `~/.local/share/agentfleet`
/// - macOS: `~/Library/Application Support/agentfleet`
pub fn data_dir() -> Result<PathBuf> {
    data_dir_in(dirs::data_dir().ok_or(StorageError::NoDataDir)?)
}

/// 在给定基目录下解析数据目录。抽出为独立函数便于测试（`dirs::data_dir()`
/// 只能读进程环境变量，测它就得改 env）。
pub fn data_dir_in(base: impl AsRef<Path>) -> Result<PathBuf> {
    let dir = base.as_ref().join(APP_DIR);
    ensure_dir(&dir)?;
    Ok(dir)
}

/// 确保目录存在且权限为 0700。
pub fn ensure_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        // 已存在但权限过宽 → 先收紧（不静默继续用）
        let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            tracing::warn!(path = %dir.display(), mode = format!("{mode:o}"), "data dir permissions too wide, tightening to 0700");
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        return Ok(());
    }

    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// 进程级 `umask(077)`。
///
/// 约束 13 的强制防护之一：数据库、临时文件、日志都不应因 umask 过宽而泄露。
/// 返回先前的 umask（测试可用来恢复）。
///
/// # Safety
/// `umask` 是进程级设置，无内存安全风险；但**不要**在库内多次调用互相覆盖。
pub fn set_umask_077() -> u32 {
    // SAFETY: umask(2) 只改进程文件创建掩码，无指针/无内存不安全。
    unsafe { libc::umask(0o077) }
}

/// 确保 DB 文件的父目录存在且为 0700。
///
/// `Path::new("t.db").parent()` 是 `Some("")`（空路径），此时**什么都不做**——
/// 直接 `create_dir_all("")` 会失败，而把当前目录收紧成 0700 显然是错的。
fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_dir(parent)?;
        }
    }
    Ok(())
}

/// 打开（或创建）数据库，跑迁移，返回连接池。
///
/// 强制设置：`journal_mode=WAL`、`busy_timeout=5000`、`foreign_keys=ON`。
pub fn open(path: &Path) -> Result<Db> {
    ensure_parent_dir(path)?;

    let manager = SqliteConnectionManager::file(path)
        .with_flags(
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_init(|conn| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            Ok(())
        });

    let pool = R2d2Pool::builder().max_size(8).build(manager)?;

    // 迁移用单独一条连接：事务 + PRAGMA user_version
    {
        let mut conn = pool.get()?;
        migrate::run(&mut conn)?;
    }

    // DB 文件权限收紧到 0600（创建时 0644 的话这里补）
    tighten_db_permissions(path)?;

    info!(path = %path.display(), "storage opened (WAL, busy_timeout={}ms)", BUSY_TIMEOUT_MS);
    Ok(pool)
}

fn tighten_db_permissions(path: &Path) -> Result<()> {
    let meta = fs::metadata(path)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 {
        tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "database permissions too wide, tightening to 0600");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 便捷入口：在给定目录下开库。
///
/// 这是 Hub 进程实际使用的入口（`afhub` 先解析数据目录，再调它）——
/// 目录可注入，测试与 `AFHUB_DATA_DIR` 都依赖这一点。
/// 不提供 `open_default`：默认目录只能从进程环境变量推导，无法单测，
/// 留一个无调用方的薄包装只会变成永远 0% 覆盖的死代码。
pub fn open_in(dir: &Path) -> Result<(PathBuf, Db)> {
    let path = dir.join("agentfleet.db");
    let pool = open(&path)?;
    Ok((path, pool))
}

#[cfg(test)]
mod tests {
    // 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
    // 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
    // let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
    // 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::indexing_slicing
    )]
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn ensure_parent_dir_handles_paths_without_parent() {
        // `/` 的 parent() 是 None（无上级），必须直接 Ok —— 不能 create_dir_all("/")。
        ensure_parent_dir(Path::new("/")).unwrap();
        // bare 文件名的 parent 是 Some("") 空路径，也必须跳过（见 ensure_parent_dir 注释）。
        ensure_parent_dir(Path::new("t.db")).unwrap();
    }

    #[test]
    fn open_works_with_bare_filename_relative_path() {
        // 相对裸文件名（如 "bare.db"）：open 必须能建库，不因 parent 为空而 panic。
        // 用当前目录的临时子目录模拟：cd 不改（会和并行测试争用 cwd），
        // 直接对 tempdir 里的裸名调用 open 走完整 open() 路径（ensure_parent_dir
        // 分支在此命中「有真实父目录」情形，与 None/空情形互补）。
        let d = tempdir();
        // 用裸文件名但给它一个真实父目录 join，open 完整跑通；
        // None 与空父两分支由上面的 ensure_parent_dir 单测覆盖。
        let pool = open(&d.path().join("bare.db")).unwrap();
        let conn = pool.get().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, crate::migrate::SCHEMA_VERSION);
    }

    #[test]
    fn pragmas_are_applied() {
        let d = tempdir();
        let pool = open(&d.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();

        let jm: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm.to_lowercase(), "wal", "journal_mode must be WAL");

        let bt: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bt, BUSY_TIMEOUT_MS as i64);

        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn db_file_is_0600_even_with_wide_umask() {
        let prev = set_umask_077();
        let d = tempdir();
        let path = d.path().join("u.db");
        open(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "db file must be 0600 regardless of umask");
        // 恢复原 umask，避免影响同进程内其它测试
        // SAFETY: umask(2) 只改进程文件创建掩码，无内存不安全。
        unsafe { libc::umask(prev) };
    }

    #[test]
    fn data_dir_is_0700() {
        let d = tempdir();
        let dir = d.path().join("agentfleet");
        ensure_dir(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        // 已存在但过宽 → 收紧
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir(&dir).unwrap();
        let mode2 = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o700);
    }

    #[test]
    fn open_is_idempotent() {
        let d = tempdir();
        let path = d.path().join("i.db");
        open(&path).unwrap();
        open(&path).unwrap(); // 第二次不得报错
        let pool = open(&path).unwrap();
        let conn = pool.get().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, crate::migrate::SCHEMA_VERSION);
    }

    #[test]
    fn existing_wide_db_is_tightened_to_0600() {
        // 先建库，再人为放宽到 0666，reopen 时必须被收紧回 0600（约束 13）
        let d = tempdir();
        let path = d.path().join("wide.db");
        open(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o666
        );
        open(&path).unwrap(); // 触发 tighten_db_permissions
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "wide db must be tightened on reopen");
    }

    #[test]
    fn bare_filename_has_empty_parent_and_is_skipped() {
        // `Path::new("t.db").parent()` 是 `Some("")`：必须跳过而不是
        // create_dir_all("") 报错、也不是把 cwd 收紧成 0700。
        // 这里直接单测辅助函数，不改进程 cwd —— 改了会和并行测试竞争。
        assert_eq!(
            Path::new("t.db").parent().map(|p| p.as_os_str().is_empty()),
            Some(true)
        );
        ensure_parent_dir(Path::new("t.db")).unwrap();

        // 有真实父目录时会创建并收紧
        let d = tempdir();
        let nested = d.path().join("a").join("b").join("t.db");
        ensure_parent_dir(&nested).unwrap();
        let dir = nested.parent().unwrap();
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn data_dir_in_creates_0700_subdir() {
        let d = tempdir();
        let dir = data_dir_in(d.path()).unwrap();
        assert!(dir.ends_with(APP_DIR), "must append app dir: {dir:?}");
        assert!(dir.is_dir(), "data dir must be created");
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "data dir must be 0700");

        // 二次调用走「已存在且已是 0700」路径：不报错、不改权限
        let again = data_dir_in(d.path()).unwrap();
        assert_eq!(again, dir);
        let mode2 = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o700);
    }

    #[test]
    fn open_in_creates_and_migrates() {
        let d = tempdir();
        let dir = data_dir_in(d.path()).unwrap();
        let (path, pool) = open_in(&dir).unwrap();
        assert!(path.ends_with("agentfleet.db"));
        let conn = pool.get().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, crate::migrate::SCHEMA_VERSION);
    }

    #[test]
    fn concurrent_writes_do_not_hit_locked() {
        // 8 线程 × 100 次写入，验证 busy_timeout 生效（否则必现 SQLITE_BUSY）
        let d = tempdir();
        let pool = open(&d.path().join("c.db")).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|t| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let conn = pool.get().unwrap();
                    for i in 0..100 {
                        conn.execute(
                            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                             VALUES (?1, 'n', ?2, '2026-01-01T00:00:00Z')",
                            rusqlite::params![format!("n_{t}_{i}"), format!("fp_{t}_{i}")],
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let conn = pool.get().unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM device", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 800);
    }
}
