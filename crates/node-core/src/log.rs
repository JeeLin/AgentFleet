//! 日志落盘：supervisor 与 worker 写同一个文件，`afcli log` 读它。
//!
//! 为什么进 M0 而不推给后续（`DECISIONS.md` D4）：升级、回滚、退避只发生在
//! 设备端，Web 侧看不到——正确回滚的 worker 连不上，那条「已回滚」根本发不出去。
//!
//! 两个进程写同一文件靠「单行 + O_APPEND」：行内 `write(2)` 是原子的，
//! 所以不需要加锁。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 日志文件名（落在数据目录下）。
pub const LOG_FILE: &str = "afcli.log";

/// 单个日志文件超过此大小即轮转。
///
/// 8 MiB 让 `log -n 50000` 能一次读完，不必翻到几百个历史文件。
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// 保留的历史文件个数。
pub const KEEP_ROTATIONS: usize = 3;

/// 日志文件路径：`<data-dir>/logs/afcli.log`。
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join(LOG_FILE)
}

/// 第 `n` 代历史日志的路径（`n` 越大越旧）。
fn rotated_path(path: &Path, n: usize) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}.{n}"))
}

/// 需要时轮转：`afcli.log` → `afcli.log.1`，`.1` → `.2`，最旧的丢弃。
///
/// 轮转只在**启动时**做一次，不在每次写入时做——运行期判断大小会给每个
/// 进程都加一次 stat 到热路径，而轮转的代价（复制整个文件）本来就不该
/// 由日志频率触发。
pub fn rotate_if_needed(path: &Path) -> Result<bool, String> {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if size < MAX_LOG_BYTES {
        return Ok(false);
    }

    // 从最旧往最新挪，避免覆盖：先删掉最后一个，再逐个后移
    let oldest = rotated_path(path, KEEP_ROTATIONS);
    if oldest.exists() {
        std::fs::remove_file(&oldest).map_err(|e| format!("remove oldest rotation: {e}"))?;
    }
    for n in (1..KEEP_ROTATIONS).rev() {
        let from = rotated_path(path, n);
        let to = rotated_path(path, n + 1);
        if from.exists() {
            std::fs::rename(&from, &to).map_err(|e| format!("rotate {n}: {e}"))?;
        }
    }
    std::fs::rename(path, rotated_path(path, 1)).map_err(|e| format!("rotate current log: {e}"))?;
    Ok(true)
}

/// 打开日志文件用于追加，**不创建父目录之外的东西、不设权限靠 umask**。
///
/// 权限显式给 0600：日志里会出现 node_id、版本、文件路径，这些是「能定位到
/// 用户某台机器」的信息，不该在多用户机器上被同机其他账号读到
/// （同 `identity.json` 的理由，AGENTS.md 约束 13）。
fn open_append(path: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = path.parent() {
        // 目录也显式给 0700：光把文件设成 0600 不够——目录若按 umask 变成 0755，
        // 同机其他账号就能列出并（在没有写权限时）探知日志文件名与轮转代数。
        std::fs::create_dir_all(parent).map_err(|e| format!("create log dir: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // coverage: excluded — 收紧日志目录权限：目录已存在（create_dir_all 成功）即 set_permissions 必成功，正常文件系统构造不出失败（防御性兜底）。
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("chmod log dir: {e}"))?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(|e| format!("open log: {e}"))
}

/// 追加一行日志。
///
/// 传入**已格式化好的整行**（含换行）——这里不做时间戳与级别格式化，
/// 那是 `tracing` 层的事。两层各管一段，比在这里重新实现一遍 subscriber 更少出错。
///
/// **失败只返回 Err 不 panic**：日志写不进去（磁盘满、目录被删）绝不能让
/// supervisor 或 worker 因此退出——那会让「日志系统故障」升级成「设备掉线」。
pub fn append_line(path: &Path, line: &str) -> Result<(), String> {
    let mut f = open_append(path)?;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    f.write_all(buf.as_bytes())
        .map_err(|e| format!("write log: {e}"))
}

/// `afcli log` 的默认输出行数。
pub const DEFAULT_TAIL_LINES: usize = 200;

/// 读最近 `n` 行，按时间正序返回。
///
/// **只读主文件，不含轮转历史**：用户问「刚才怎么了」，答案几乎总在当前文件里；
/// 把几千行历史一起倒出来反而更难读。历史文件留在盘上，需要时用户自己翻。
///
/// 文件不存在返回空 `Vec`（不是错误）：还没产生过日志是正常状态。
pub fn tail(path: &Path, n: usize) -> Result<Vec<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read log: {e}")),
    };

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > n {
        // `n == 0` 时这里会给出空 vec，但 `afcli log -n 0` 应当什么都不打印。
        lines.drain(..lines.len() - n);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_sits_under_logs_in_the_data_dir() {
        assert_eq!(
            log_path(Path::new("/var/lib/afcli")),
            PathBuf::from("/var/lib/afcli/logs/afcli.log")
        );
    }

    #[test]
    fn rotation_paths_are_ordered_newest_first() {
        let p = log_path(Path::new("/d"));
        assert_eq!(rotated_path(&p, 1), PathBuf::from("/d/logs/afcli.log.1"));
        assert_eq!(rotated_path(&p, 3), PathBuf::from("/d/logs/afcli.log.3"));
    }

    #[test]
    fn append_creates_the_log_dir_and_writes_the_line() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "hello").unwrap();
        append_line(&p, "world").unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "hello\nworld\n", "每行都要有换行，否则会粘连");
    }

    /// 两行拼接时调用方可能没给换行，追加层必须自己补——
    /// 否则 supervisor 与 worker 的日志会粘成一行，无法排查。
    #[test]
    fn append_adds_a_missing_newline_only_once() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "with newline\n").unwrap();
        append_line(&p, "without").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "with newline\nwithout\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn log_is_private_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "secret-ish").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "日志含 node_id 与路径，不该被同机其他账号读到");
    }

    #[test]
    fn log_dir_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "x").unwrap();
        let mode = std::fs::metadata(p.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0, "日志目录不应组/其他可访问");
    }

    #[test]
    fn small_log_is_not_rotated() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "tiny").unwrap();
        assert!(!rotate_if_needed(&p).unwrap());
        assert!(!rotated_path(&p, 1).exists());
    }

    #[test]
    fn missing_log_is_not_rotated() {
        let d = tempfile::tempdir().unwrap();
        assert!(!rotate_if_needed(&log_path(d.path())).unwrap());
    }

    #[test]
    fn oversized_log_is_rotated_with_oldest_dropped() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        // 直接写超限内容，不真的生成 8 MiB
        let big = "x".repeat(MAX_LOG_BYTES as usize + 1);
        append_line(&p, "").unwrap(); // 建好 logs/ 目录（open_append 负责创建）
        std::fs::write(&p, big).unwrap();
        // 先造几代历史，验证最旧的被丢弃而不是无限增长
        for n in 1..=KEEP_ROTATIONS {
            std::fs::write(rotated_path(&p, n), format!("gen{n}")).unwrap();
        }
        assert!(rotate_if_needed(&p).unwrap());

        assert!(rotated_path(&p, 1).exists(), "当前日志应成为 .1");
        assert!(rotated_path(&p, 2).exists(), "原 .1 应后移成 .2");
        assert!(rotated_path(&p, 3).exists(), "原 .2 应后移成 .3");
        assert!(
            !rotated_path(&p, 4).exists(),
            "只保留 KEEP_ROTATIONS 代，更旧的必须丢弃，否则日志无限增长"
        );
    }

    #[test]
    fn tail_returns_the_last_lines_in_chronological_order() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        for i in 1..=10 {
            append_line(&p, &format!("line {i}")).unwrap();
        }
        let got = tail(&p, 3).unwrap();
        assert_eq!(got, vec!["line 8", "line 9", "line 10"]);
    }

    #[test]
    fn tail_of_short_log_returns_everything() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "only").unwrap();
        assert_eq!(tail(&p, 200).unwrap(), vec!["only"]);
    }

    #[test]
    fn tail_of_missing_log_is_empty_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(tail(&log_path(d.path()), 10).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn tail_of_zero_lines_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "a").unwrap();
        assert_eq!(tail(&p, 0).unwrap(), Vec::<String>::new());
    }

    /// 历史文件不参与 `afcli log` 输出——否则输出量取决于轮转次数，
    /// 用户拿不到稳定的行数，且「刚才怎么了」通常就在当前文件里。
    #[test]
    fn tail_ignores_rotated_history() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "current").unwrap();
        std::fs::write(rotated_path(&p, 1), "ancient\n").unwrap();
        assert_eq!(tail(&p, 10).unwrap(), vec!["current"]);
    }

    #[test]
    fn appending_twice_concurrently_keeps_lines_whole() {
        // 两个进程（supervisor / worker）写同一文件的场景。
        // 单次 write 在 O_APPEND 下是原子的，行不会互相截断。
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for j in 0..20 {
                        append_line(&p, &format!("t{i}-{j}")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got.lines().count(), 160);
        for line in got.lines() {
            assert!(
                line.starts_with('t') && line.contains('-'),
                "行被写坏了：{line}"
            );
        }
    }

    #[test]
    fn tail_reports_unreadable_file_rather_than_panicking() {
        let d = tempfile::tempdir().unwrap();
        // 目录顶替文件 → 读不了但存在
        let p = d.path().join("afcli.log");
        std::fs::create_dir(&p).unwrap();
        assert!(tail(&p, 10).is_err(), "读不了必须报错，不能返回空假装没事");
    }

    /// 造一个「体积超了但没真写 8 MiB」的日志：`set_len` 只改元数据，
    /// 稀疏文件读起来是 NUL，而 `rotate_if_needed` 只看 `metadata().len()`。
    /// 用它把「轮转」这件事与「往盘上灌 8 MiB」解耦开。
    #[cfg(unix)]
    fn oversized_sparse_log(p: &Path) {
        std::fs::File::create(p)
            .unwrap()
            .set_len(MAX_LOG_BYTES + 1)
            .unwrap();
        assert_eq!(
            std::fs::metadata(p).unwrap().len(),
            MAX_LOG_BYTES + 1,
            "稀疏文件也要被 stat 认成超限，否则下面测的不是轮转"
        );
    }

    /// 某一代历史文件挪不动时，报错要**点名是哪一代**，而不是甩一句裸 errno。
    ///
    /// 构造法：`.2` 是一个非空目录，`.3` 是一个**悬空符号链接**——
    /// `Path::exists()` 顺着链接看是 false，于是「删掉最旧一代」那步被跳过；
    /// 紧接着 `rename(.2 → .3)` 撞上「目录不能改名成符号链接」(ENOTDIR)。
    /// 而错误信息里的 `rotate 2` 正是排障时要找的那一代。
    #[cfg(unix)]
    #[test]
    fn rotation_reports_which_generation_failed_to_shift() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        append_line(&p, "").unwrap(); // 建好 logs/ 目录
        oversized_sparse_log(&p);

        let from = rotated_path(&p, 2);
        let to = rotated_path(&p, 3);
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("keeps-it-nonempty"), b"x").unwrap();
        symlink("no-such-target", &to).unwrap();
        assert!(!to.exists(), "悬空链接必须被 exists() 当成不存在");

        let err = rotate_if_needed(&p).unwrap_err();
        assert!(
            err.contains("rotate 2"),
            "要说清是哪一代挪不动，实际：{err}"
        );
        assert!(
            p.exists(),
            "任何一代失败都不能把当前日志提前改名：那会丢掉正在写的那份"
        );
    }

    /// 当前日志挪不动时，报错要说是「当前日志」而不是某代历史。
    ///
    /// `/proc/kcore` 的 `stat` 报告约 140 TiB（远超 8 MiB），但它既不能改名、
    /// 也不能被删；`rotate_if_needed` 会走到最后那句 `rotate current log`。
    /// 用它而不是真的造一个 8 MiB 文件，是因为那既慢又依赖盘上剩余空间。
    #[cfg(unix)]
    #[test]
    fn rotation_reports_failure_to_move_the_current_log() {
        if !Path::new("/proc/kcore").exists() {
            return;
        }
        let err = rotate_if_needed(Path::new("/proc/kcore")).unwrap_err();
        assert!(
            err.contains("rotate current log"),
            "当前日志挪不动要说清是它，实际：{err}"
        );
    }

    /// 日志目录**权限收紧失败**时，`append_line` 要把这句错误交回调用方，
    /// 而不是继续用一个可能 0755 的目录写日志。
    ///
    /// 构造法：`logs` 位置放一个指向 `/proc/self/fd` 的符号链接——
    /// `create_dir_all` 见到已存在的目录就成功，而紧接着的
    /// `set_permissions` 在 procfs 上拿到 EPERM。
    #[cfg(unix)]
    #[test]
    fn append_reports_failure_to_tighten_log_dir_permissions() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        let logs = p.parent().unwrap();
        symlink("/proc/self/fd", logs).unwrap();

        let err = append_line(&p, "secret").unwrap_err();
        assert!(
            err.contains("chmod log dir"),
            "目录权限没收紧要说出来，实际：{err}"
        );
    }

    /// 打开日志文件失败时（这里让目录顶替文件 → EISDIR），`append_line`
    /// 报的是 `open log`，且不留下半行内容。
    #[cfg(unix)]
    #[test]
    fn append_reports_failure_to_open_the_log_file() {
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::create_dir(&p).unwrap();
        let err = append_line(&p, "x").unwrap_err();
        assert!(
            err.contains("open log"),
            "打不开日志要说清是在打开阶段，实际：{err}"
        );
    }

    /// 写失败（磁盘满）**只返回 Err，不 panic**：日志系统故障不能把设备带下线。
    ///
    /// `/dev/full` 接受写入并稳定返回 ENOSPC，比把分区写满可控得多；
    /// 这里用符号链接把它放到日志文件的位置，`OpenOptions::append(true)`
    /// 打开后 `write_all` 就会撞上这个错。
    #[cfg(unix)]
    #[test]
    fn append_reports_write_failure_without_panicking() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let p = log_path(d.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink("/dev/full", &p).unwrap();

        let err = append_line(&p, "a line that cannot land").unwrap_err();
        assert!(
            err.contains("write log"),
            "写失败要说清是在写阶段，实际：{err}"
        );
    }
}
