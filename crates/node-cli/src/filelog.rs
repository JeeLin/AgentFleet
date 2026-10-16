//! 设备端日志的写入侧：supervisor 与 worker 两边共用同一份格式。
//!
//! 与 [`node_core::log`]（落盘 + 轮转 + 读）分开：那一层是**机制**，这里
//! 是**格式与调用点**。让两个进程各自拼一遍时间戳与级别，格式迟早分叉——
//! 而排障时正是靠 `afcli log` 把两边的记录按时间拼起来读（标准⑨）。

use std::path::Path;

use node_core::log;

/// supervisor 在日志里的角色名。
pub const ROLE_SUPERVISOR: &str = "supervisor";

/// worker 在日志里的角色名。
pub const ROLE_WORKER: &str = "worker";

/// 写一行到 `<data-dir>/logs/afcli.log`。
///
/// 角色排在级别前面：日志是两边混写的，排障第一眼要能分清这行是谁写的。
pub fn write(data_dir: &Path, role: &str, level: &str, msg: &str) {
    let line = format!("{} [{role}] {level} {msg}", now_unix_secs());
    // 写日志失败**不**让调用方退出：磁盘满或目录被删不该把「排障手段」
    // 升级成「设备掉线」。但也不能静默吞掉——唯一还能去的地方是 stderr。
    if let Err(e) = log::append_line(&log::log_path(data_dir), &line) {
        eprintln!("afcli [{role}]: 无法写入日志（{e}）：{msg}");
    }
}

/// 秒级精度足够：日志不参与任何判定，也不需要毫秒级排序。
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("afcli-filelog-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("tempdir");
        p
    }

    /// 排障时靠 `afcli log` 把 supervisor 与 worker 两边的记录按时间拼起来读，
    /// 所以角色必须排在级别前面，且行尾有换行（否则两条记录会粘成一行）。
    #[test]
    fn a_log_line_leads_with_the_role_and_ends_with_a_newline() {
        let d = dir("role");
        write(&d, ROLE_WORKER, "info", "connected");
        let text = std::fs::read_to_string(log::log_path(&d)).expect("log file");
        assert!(text.ends_with('\n'), "missing newline: {text:?}");
        let line = text.lines().next().expect("one line");
        assert!(line.contains("[worker]"), "got {line}");
        assert!(line.contains("info"), "got {line}");
        assert!(line.contains("connected"), "got {line}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 两个角色各写一行，读回来必须能按角色分开——混着写也不能互相覆盖。
    #[test]
    fn both_roles_append_to_the_same_file_in_order() {
        let d = dir("both");
        write(&d, ROLE_SUPERVISOR, "info", "spawned");
        write(&d, ROLE_WORKER, "warn", "retrying");
        let lines = log::tail(&log::log_path(&d), 10).expect("tail");
        assert_eq!(lines.len(), 2, "got {lines:?}");
        assert!(lines[0].contains("[supervisor]"), "got {:?}", lines[0]);
        assert!(lines[1].contains("[worker]"), "got {:?}", lines[1]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 写不进去时**不能**让调用方退出：磁盘满或目录被删不该把「排障手段」
    /// 升级成「设备掉线」。这里用一个普通文件当目录，构造确定性的失败。
    #[test]
    fn a_failed_write_is_survived_not_fatal() {
        let base = dir("fail");
        let blocker = base.join("logs");
        std::fs::write(&blocker, "not a directory").expect("blocker file");
        // 不 panic、不返回、不静默：stderr 那一路由 eprintln 负责。
        write(&base, ROLE_SUPERVISOR, "error", "disk is full");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 秒级时间戳：日志不参与判定，也不需要毫秒排序。
    #[test]
    fn the_timestamp_is_seconds() {
        let d = dir("secs");
        write(&d, ROLE_WORKER, "info", "x");
        let text = std::fs::read_to_string(log::log_path(&d)).expect("log");
        let stamp: u64 = text
            .split_whitespace()
            .next()
            .expect("stamp")
            .parse()
            .expect("numeric");
        // 2020-01-01 之后。写死 1.5e9 会随时间变假，所以只断言量级。
        assert!(stamp > 1_577_836_800, "stamp {stamp}");
        assert!(stamp < 100_000_000_000, "stamp {stamp} looks like millis");
        let _ = std::fs::remove_dir_all(&d);
    }
}
