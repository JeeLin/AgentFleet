//! 数据库打开、PRAGMA 与数据目录权限。

use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use tracing::info;

use crate::error::{Result, StorageError};
use crate::migrate;

/// 建库时的 `busy_timeout`（毫秒）。AGENTS.md「SQLite 铁律」要求必设。
pub const BUSY_TIMEOUT_MS: u32 = 5000;

pub type Db = Pool<SqliteConnectionManager>;

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
    let base = dirs::data_dir().ok_or(StorageError::NoDataDir)?;
    let dir = base.join(APP_DIR);
    ensure_dir(&dir)?;
    Ok(dir)
}

/// 确保目录存在且权限为 0700。
pub fn ensure_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        // 已存在但权限过宽 → 先收紧（不静默继续用）
        let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            tracing::warn!(
                path = %dir.display(),
                mode = format!("{mode:o}"),
                "data dir permissions too wide, tightening to 0700"
            );
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
    unsafe {
        let prev = libc::umask(0o077);
        prev
    }
}

/// 打开（或创建）数据库，跑迁移，返回连接池。
///
/// 强制设置：`journal_mode=WAL`、`busy_timeout=5000`、`foreign_keys=ON`。
pub fn open(path: &Path) -> Result<Db> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_dir(parent)?;
        }
    }

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

    let pool = Pool::builder().max_size(8).build(manager)?;

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
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:o}"),
            "database permissions too wide, tightening to 0600"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 便捷入口：用默认数据目录下的 `agentfleet.db`。
pub fn open_default() -> Result<(PathBuf, Db)> {
    let dir = data_dir()?;
    let path = dir.join("agentfleet.db");
    let pool = open(&path)?;
    Ok((path, pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
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
        set_umask(prev);
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
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
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
