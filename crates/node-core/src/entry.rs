//! 设备端日志的写入侧：supervisor 与 worker 两边共用同一份格式。
//!
//! 与 [`crate::log`]（落盘 + 轮转 + 读）分开：那一层是**机制**，这里
//! 是**格式与调用点**。让两个进程各自拼一遍时间戳与级别，格式迟早分叉——
//! 而排障时正是靠 `afcli log` 把两边的记录按时间拼起来读（标准⑨）。
//!
//! 原来这一层在 `node-cli`（进程入口那一侧）。它跟着 worker 一起搬进
//! `node-core`：worker 是**要被升级换掉的文件**（约束 12），而 connector
//! 只能被已认证的 WS 连接使用——把通道处理留在 node-cli 就等于让通道能力
//! 跟着 worker 换版本走，于是「已注册的能力」与「这一版实际能跑的代码」
//! 变成两件事。

use std::path::Path;

use crate::log;

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

/// 启动时轮转一次日志。supervisor 与 worker 各调一次，轮转失败只记一行日志
/// （写不出去正是「轮转失败」的后果之一，再 `eprintln` 一遍是同一件事说两遍）。
///
/// 运行期判断大小会给每个进程加一次 stat 到热路径，而轮转的代价（复制整个文件）
/// 本来就不该由日志频率触发。
pub fn rotate_at_startup(data_dir: &Path, role: &str) {
    let path = log::log_path(data_dir);
    // coverage: excluded — 轮转失败需要日志目录不可写，而 `rotate_if_needed`
    // 的失败路径本身已登记在 node-core/src/log.rs；这里只做「再记一行」。
    if let Err(e) = log::rotate_if_needed(&path) {
        // coverage: excluded — 轮转失败需要日志目录不可写，而 `rotate_if_needed`
        // 的失败路径本身已登记在 node-core/src/log.rs；这里只做「再记一行」。
        write(
            data_dir,
            role,
            "info",
            &format!("cannot rotate {}: {e}", path.display()),
        );
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
        let p = std::env::temp_dir().join(format!("afcli-entry-{name}"));
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

    /// `rotate_at_startup` 是两个入口（supervisor / worker）唯一的轮转调用点：
    /// 超限日志在启动时被挪到 `.1`，且**两个角色都会触发**——曾经两个
    /// `rotate_log` 各写一份、只差一个角色常量，一条改了另一条漏掉。
    #[test]
    fn rotate_at_startup_rotates_for_both_roles() {
        for role in [ROLE_SUPERVISOR, ROLE_WORKER] {
            let d = dir("rotate-startup");
            let path = log::log_path(&d);
            std::fs::create_dir_all(path.parent().expect("logs dir")).expect("mkdir");
            std::fs::write(&path, vec![b'x'; (log::MAX_LOG_BYTES + 1) as usize]).expect("fill");
            rotate_at_startup(&d, role);
            assert!(
                path.with_file_name("afcli.log.1").exists(),
                "{role}: oversized log must rotate at startup"
            );
            assert!(!path.exists(), "{role}: current name must be freed");
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// 小日志不轮转：轮转的代价是复制整个文件，不该每次启动都白付一遍。
    #[test]
    fn a_small_log_is_not_rotated_at_startup() {
        let d = dir("rotate-skip");
        write(&d, ROLE_WORKER, "info", "tiny");
        rotate_at_startup(&d, ROLE_WORKER);
        let text = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(text.contains("tiny"), "the original line must stay put");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 轮转失败也**必须留下痕迹**：`rotate_at_startup` 是启动路径，失败时
    /// 只靠 `log.rs` 返回的 `Err` 就静默了——排障时看到「日志没轮转」却
    /// 不知道为什么。这里补写的那行就是原因。
    #[test]
    fn a_failed_rotation_is_recorded_rather_than_silently_dropped() {
        let d = dir("rotate-fail");
        let path = log::log_path(&d);
        std::fs::create_dir_all(path.parent().expect("logs dir")).expect("mkdir");
        std::fs::write(&path, vec![b'x'; (log::MAX_LOG_BYTES + 1) as usize]).expect("fill");
        // 最旧一代换成目录：`remove_file` 对目录必失败（unlink 目录 = EPERM），
        // 于是轮转整体报错。用目录而不是去掉写权限——root 绕过权限位，
        // 而这个失败与以什么身份运行无关。
        std::fs::create_dir(path.with_file_name("afcli.log.3")).expect("dir blocker");

        rotate_at_startup(&d, ROLE_SUPERVISOR);

        let text = std::fs::read_to_string(&path).expect("log still there");
        assert!(
            text.contains("cannot rotate"),
            "轮转失败必须留下痕迹：{text:?}"
        );
        assert!(
            text.contains("remove oldest rotation"),
            "要说清是哪一步失败的，否则这行等于没写：{text:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
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
