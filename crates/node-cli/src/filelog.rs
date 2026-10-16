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
