//! 离线判定的扫瞄器（子任务 6）。
//!
//! 判据只有一条：**连续心跳丢失 > [`OFFLINE_AFTER`]**。不看重连次数、不看重连
//! 失败次数——`security-and-lifecycle.md` §5 特意写了为什么：设备可能「心跳超时
//! 但重连成功」，此时连接是通的，按重连次数判会把活着的设备标成离线，而调度
//! 硬过滤第一步就排除 offline 的设备。
//!
//! 扫瞄器只做「把该标 offline 的标上」，**不删内存上报**：上报删了，
//! `presence()` 就只会返回 `Offline`，重连窗口里的 `Reconnecting`（调度仍可用）
//! 这档状态根本不会出现。

use std::time::Duration;

use hub_core::reports::{timed_out, Reports};
use hub_storage::repo::DeviceRepo;

/// 扫瞄周期。判据是 45s，所以每 15s 看一次就够了——
/// 更密只会白跑 SQL（每台设备一条 UPDATE），而且 SQLite 单写者下是白撞锁。
///
/// 不是 `OFFLINE_AFTER` 的约数也无所谓：判定看的是心跳年龄本身，
/// 扫得晚只会让 offline 落库晚一点，不会误判。
pub const SCAN_INTERVAL: Duration = Duration::from_secs(15);

/// 扫一轮：把心跳已超时的设备标 offline 落库。
///
/// 返回被标离线的 node_id 列表（测的就是它）。**不删内存上报**
/// （理由见模块注释）——设备回一次心跳就会重新 online，删了反而会闪。
///
/// 落库失败不中断扫瞄：一台写失败不该让后面几十台都标不上，记日志往下走。
pub async fn sweep(reports: &Reports, devices: &DeviceRepo) -> Vec<String> {
    let stale = timed_out(reports, std::time::Instant::now());
    let mut marked = Vec::with_capacity(stale.len());
    for id in stale {
        match devices.mark_offline(&id) {
            Ok(()) => marked.push(id),
            // 不静默：这台设备会一直留在 online，而「标不上离线」正是
            // 排查调度异常时最该看到的线索。
            Err(e) => tracing::warn!(node_id = %id, error = %e, "cannot mark device offline"),
        }
    }
    marked
}

/// 扫瞄器：每 [`SCAN_INTERVAL`] 跑一次 [`sweep`]，直到 `shutdown` 被触发。
///
/// `interval` 是入参而不是直接读常量：**测试必须能把它压到毫秒**
/// （标准③ 要等 46s，用真实周期意味着一个用例跑 46 秒）。
///
/// 退出只走 `shutdown`：这个函数本身没有终止条件，靠调用方持有
/// `oneshot::Sender` 来关。用 `tokio::select!` 而不是 `interval.tick()` 之后
/// 再判断——那样关掉扫瞄器最多要再等一个完整周期。
pub async fn run(
    reports: Reports,
    devices: DeviceRepo,
    interval: Duration,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let marked = sweep(&reports, &devices).await;
                if !marked.is_empty() {
                    tracing::info!(count = marked.len(), "marked devices offline");
                }
            }
            _ = &mut shutdown => {
                tracing::info!("presence scanner stopped");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::reports::OFFLINE_AFTER;
    use std::time::Instant;

    fn repo(d: &std::path::Path) -> DeviceRepo {
        DeviceRepo::new(hub_storage::db::open(&d.join("t.db")).unwrap())
    }

    /// 直接插一行 device（schema 只要求 id / name / fingerprint）。
    ///
    /// 用 `format!` 拼而不是绑定参数：`rusqlite` 不是本 crate 的依赖，
    /// 而这几个字符串全是测试自己给的常量。
    fn insert_device(devices: &DeviceRepo, id: &str, fp: &str) {
        devices
            .pool()
            .get()
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                 VALUES ('{id}', 'pi', '{fp}', '2026-01-01T00:00:00Z')"
            ))
            .unwrap();
    }

    fn status(devices: &DeviceRepo, id: &str) -> String {
        devices
            .find_by_id(id)
            .unwrap()
            .expect("device row must exist")
            .status
            .as_str()
            .to_string()
    }

    fn hb(version: &str) -> hub_core::Report {
        hub_core::Report {
            node_version: version.to_string(),
            capabilities: serde_json::json!({"connectors": []}),
            resources: serde_json::Value::Null,
            workspace_roots: Vec::new(),
            at: Instant::now(),
        }
    }

    /// 一台刚上报过的设备：不该被标离线。
    #[tokio::test]
    async fn a_fresh_device_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        insert_device(&devices, "n_1", "fp_1");
        devices
            .touch_online("n_1", "{}", "{}", "2026-01-02T00:00:00Z")
            .unwrap();

        let reports = Reports::new();
        reports.record("n_1", hb("0.1.0"));

        let marked = sweep(&reports, &devices).await;
        assert!(marked.is_empty(), "心跳新鲜的设备不该被标离线");
        assert_eq!(status(&devices, "n_1"), "online");
    }

    /// 心跳停了 45s → 标 offline，且**上报仍在内存里**（重连窗口要靠它）。
    #[tokio::test]
    async fn a_stale_device_is_marked_offline_but_kept_in_memory() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        insert_device(&devices, "n_1", "fp_1");
        devices
            .touch_online("n_1", "{}", "{}", "2026-01-02T00:00:00Z")
            .unwrap();

        let reports = Reports::new();
        // 手工造一份「46 秒前」的上报
        let mut stale = hb("0.1.0");
        stale.at = Instant::now() - OFFLINE_AFTER - Duration::from_secs(1);
        reports.record("n_1", stale);

        let marked = sweep(&reports, &devices).await;
        assert_eq!(marked, vec!["n_1".to_string()], "超时的必须被标离线");
        assert_eq!(status(&devices, "n_1"), "offline");
        // 关键：上报还在 → presence() 才可能给出 Reconnecting
        assert!(reports.get("n_1").is_some(), "上报必须留着");
    }

    /// 心跳恢复 → 不再被标（标准③ 后半句）。
    ///
    /// 回 online 靠的是心跳路径的 `touch_online`；这里验证的是扫瞄器不会
    /// 把已经恢复的设备再打回 offline。
    #[tokio::test]
    async fn a_device_that_heartbeats_again_is_not_marked_again() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        insert_device(&devices, "n_1", "fp_1");

        let reports = Reports::new();
        let mut stale = hb("0.1.0");
        stale.at = Instant::now() - OFFLINE_AFTER - Duration::from_secs(1);
        reports.record("n_1", stale);
        devices.mark_offline("n_1").unwrap();

        // 心跳回来了
        reports.record("n_1", hb("0.1.1"));
        devices
            .touch_online("n_1", "{}", "{}", "2026-01-02T00:00:10Z")
            .unwrap();

        let marked = sweep(&reports, &devices).await;
        assert!(marked.is_empty(), "心跳恢复后不该再被标离线");
        assert_eq!(status(&devices, "n_1"), "online");
    }

    /// 多台一起超时：一次全标上，且顺序稳定（`timed_out` 已排序）。
    #[tokio::test]
    async fn every_stale_device_is_marked_in_a_stable_order() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        let reports = Reports::new();
        for id in ["n_c", "n_a", "n_b"] {
            insert_device(&devices, id, &format!("fp_{id}"));
            let mut stale = hb("0.1.0");
            stale.at = Instant::now() - OFFLINE_AFTER - Duration::from_secs(1);
            reports.record(id, stale);
        }

        let marked = sweep(&reports, &devices).await;
        assert_eq!(marked, vec!["n_a", "n_b", "n_c"], "顺序必须稳定可复现");
    }

    /// 落库失败不中断扫瞄，也**不静默**：写失败的那台不算「已标」。
    #[tokio::test]
    async fn a_failing_write_does_not_stop_the_sweep() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        insert_device(&devices, "n_1", "fp_1");
        let reports = Reports::new();
        let mut stale = hb("0.1.0");
        stale.at = Instant::now() - OFFLINE_AFTER - Duration::from_secs(1);
        reports.record("n_1", stale);

        // 让 UPDATE 失败
        devices
            .pool()
            .get()
            .unwrap()
            .execute_batch("ALTER TABLE device RENAME TO device_moved")
            .unwrap();

        let marked = sweep(&reports, &devices).await;
        assert!(marked.is_empty(), "写失败的那台不算「已标」");
    }

    /// 扫瞄器真跑一轮并标出离线设备（`run` 的 tick 臂，不只是 `sweep`）。
    ///
    /// 与上面几条的区别：这里走的是 `run` 里的 `ticker.tick()` 分支，
    /// 覆盖「扫到东西时记日志」那一行——只测 `sweep` 覆盖不到它。
    #[tokio::test]
    async fn the_scanner_marks_stale_devices_on_its_tick() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        insert_device(&devices, "n_1", "fp_1");
        devices
            .touch_online("n_1", "{}", "{}", "2026-01-02T00:00:00Z")
            .unwrap();

        let reports = Reports::new();
        let mut stale = hb("0.1.0");
        stale.at = Instant::now() - OFFLINE_AFTER - Duration::from_secs(1);
        reports.record("n_1", stale);

        let (_tx, rx) = tokio::sync::oneshot::channel();
        // 第一个 tick 在 tokio::time::interval 下是立即的
        let handle = tokio::spawn(run(
            reports.clone(),
            devices.clone(),
            Duration::from_millis(50),
            rx,
        ));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if status(&devices, "n_1") == "offline" {
                break;
            }
            assert!(Instant::now() < deadline, "扫瞄器始终没把设备标离线");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();
    }

    /// 扫瞄器能被 `shutdown` 干净关掉（不是跑满一个周期才退）。
    #[tokio::test]
    async fn the_scanner_stops_on_shutdown() {
        let d = tempfile::tempdir().unwrap();
        let devices = repo(d.path());
        let reports = Reports::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        // 周期故意给很长：若实现是「先睡完再说」，这个用例就会挂住
        let handle = tokio::spawn(run(reports, devices, Duration::from_secs(3600), rx));
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("扫瞄器必须在 shutdown 后立刻退出")
            .expect("扫瞄器不得 panic");
    }
}
