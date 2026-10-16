//! 升级状态机与 `update-state.json` 读写。
//!
//! 对应 `DECISIONS.md` D2。worker 下载并校验新二进制后**只写状态文件就退出**，
//! 替换与回滚全部由 supervisor 执行——**执行回滚的代码不参与被回滚**。
//!
//! 本文件刻意不含任何进程操作：读写状态文件、状态流转与回滚判定都是纯逻辑，
//! 好让它们能在 100% 覆盖率门禁下被完整单测（真去 rename 二进制的分支只能靠
//! 集成测试，那是 `node-cli/src/supervisor.rs` 的事）。

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 升级状态。
///
/// 流转（正常升级路径）：
///
/// ```text
/// Idle ──► Requested ──► StartingNew ──► Committed
///            │                │
///            │                └──► RollingBack ──► RolledBack
///            └──► Failed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    /// 无升级在进行。
    Idle,
    /// worker 已下载并校验完新二进制，写好状态文件后 `exit(10)`。
    /// **此时二进制尚未被替换**。
    Requested,
    /// supervisor 已完成替换，备份存在 `.bak`，正在观察新版健康状况。
    StartingNew,
    /// 新版健康检查通过，升级完成。
    Committed,
    /// 新版不健康，正在换回 `.bak`。
    RollingBack,
    /// 已换回旧版，设备恢复可用。
    RolledBack,
    /// 升级或回滚本身失败——**需要人介入**，不再自动重试。
    Failed,
}

impl UpdatePhase {
    /// 本阶段是否意味着「有一轮升级在途中」。
    ///
    /// **这是 D2-① 的核心**：REX 把 fast_crash 判定放在读状态之前，
    /// 于是新版本秒崩时 supervisor 直接 exit、**永不回滚**（砖设备）。
    /// 我们把它定义成纯函数、由调用方在读状态**之后**判定。
    pub fn in_flight(self) -> bool {
        matches!(
            self,
            UpdatePhase::Requested | UpdatePhase::StartingNew | UpdatePhase::RollingBack
        )
    }

    /// 是否已是终态（不再自动动作）。
    ///
    /// 与 `in_flight()` 互补：`Requested`/`StartingNew`/`RollingBack` 三态归
    /// in_flight，其余四态归终态，两者恰好覆盖全部 7 个状态。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            UpdatePhase::Idle
                | UpdatePhase::Committed
                | UpdatePhase::RolledBack
                | UpdatePhase::Failed
        )
    }
}

impl fmt::Display for UpdatePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            UpdatePhase::Idle => "idle",
            UpdatePhase::Requested => "requested",
            UpdatePhase::StartingNew => "starting_new",
            UpdatePhase::Committed => "committed",
            UpdatePhase::RollingBack => "rolling_back",
            UpdatePhase::RolledBack => "rolled_back",
            UpdatePhase::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// `update-state.json` 的内容。
///
/// `generation` 是**本轮备份的唯一标识**（D2-③）：备份失败或备份来自上一轮时，
/// `generation` 与当前轮不符，supervisor 拒绝拿它回滚——否则一次「备份没写成」
/// 就会把设备降级到几个月前的陈旧版本，且用户毫不知情。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    pub phase: UpdatePhase,
    /// 目标版本号。
    pub version: String,
    /// 已下载待替换的二进制路径。
    #[serde(default)]
    pub staged_path: String,
    /// 本轮备份路径（`.bak`）。
    #[serde(default)]
    pub backup_path: String,
    /// 本轮更新的世代号。与 `generation()` 不符的备份**不可用于回滚**。
    #[serde(default)]
    pub generation: u64,
    /// 已回滚次数。`>= 1` 后再次需要回滚 → `Failed`，防死循环（D2-⑤）。
    #[serde(default)]
    pub rollback_count: u32,
}

impl UpdateState {
    /// 全新一轮升级的初始状态。
    pub fn requested(version: &str, staged: &Path, backup: &Path, generation: u64) -> Self {
        Self {
            phase: UpdatePhase::Requested,
            version: version.to_string(),
            staged_path: staged.display().to_string(),
            backup_path: backup.display().to_string(),
            generation,
            rollback_count: 0,
        }
    }

    /// 备份是否可用于本轮回滚。
    ///
    /// 三条都要满足：指向了文件、世代号对得上、且尚未回滚过。
    /// 最后一条对应 D2-⑤——REX 靠 `rename` 把 `.bak` 挪走来"意外"防住死循环，
    /// 那是副作用不是防线；我们显式判定。
    pub fn backup_usable_for(&self) -> bool {
        !self.backup_path.is_empty() && self.generation > 0 && self.rollback_count == 0
    }

    /// 是否必须停下等人介入。
    pub fn needs_human(&self) -> bool {
        self.phase == UpdatePhase::Failed
            || (self.phase.in_flight() && self.rollback_count > 0 && !self.backup_usable_for())
    }
}

/// 状态文件路径：与被管理的二进制同目录、同 stem。
pub fn state_path_for(exe: &Path) -> PathBuf {
    exe.with_extension("update-state.json")
}

/// 备份路径：`.bak`（Windows 上还会用到 `.old` 让位，见 DECISIONS D2）。
pub fn backup_path_for(exe: &Path) -> PathBuf {
    exe.with_extension("bak")
}

/// 更新残留文件后缀，尽力清理（失败只 warn——Windows 上 supervisor
/// 可能正从 `.old` 运行，删不掉是正常的）。
pub const LEFTOVER_EXTS: [&str; 2] = ["old", "bak"];

/// 读状态文件。
///
/// **读不到 / 解析失败一律返回 `None` 而不是 `Err`**，调用方据此按「无升级在进行」
/// 处理（正常无升级时文件本来就不存在）。但**解析失败必须能被发现**，
/// 所以额外返回 [`StateRead`] 让调用方决定要不要打告警——
/// 一个损坏的状态文件静默降级成「无更新」，会让更新流程做出错误判断。
pub fn read_state(path: &Path) -> (Option<UpdateState>, StateRead) {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, StateRead::Absent),
        Err(e) => (None, StateRead::Unreadable(e.to_string())),
        Ok(text) => match serde_json::from_str(&text) {
            Ok(s) => (Some(s), StateRead::Ok),
            Err(e) => (None, StateRead::Corrupt(e.to_string())),
        },
    }
}

/// 读状态文件的诊断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateRead {
    /// 文件不存在（正常：没有升级在进行）。
    Absent,
    /// 读到了且合法。
    Ok,
    /// 存在但读不了（权限、I/O 错误）。
    Unreadable(String),
    /// 存在但解析不了（写了一半崩溃 / 被人手改坏）。
    Corrupt(String),
}

impl StateRead {
    /// 是否属于「需要人看一眼」的情况。
    ///
    /// 损坏不是致命——supervisor 仍能正常跑 worker——但**必须留痕**：
    /// 否则一次写坏的状态文件会让人误以为"升级流程没触发"。
    pub fn is_problem(&self) -> bool {
        matches!(self, StateRead::Unreadable(_) | StateRead::Corrupt(_))
    }
}

/// 原子写状态文件：`tmp` → `fsync` → `rename`。
///
/// **不用 `File::create` 直写**：写一半崩溃会留下截断的 JSON，下次启动解析失败，
/// 更新流程就永久卡在未知状态。rename 在同一文件系统内是原子的。
pub fn write_state(path: &Path, state: &UpdateState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state).map_err(|e| format!("serialize state: {e}"))?;
    let tmp = path.with_extension("json.tmp");

    // 状态文件里没有秘密，但它记录着本机的升级路径与版本；
    // 与 identity.json 同标准收紧，且同样不依赖 umask。
    write_private(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename state into place: {e}"))?;
    Ok(())
}

/// 以 0600 权限原子写入字节内容。
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut f = opts
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", path.display()))?;
    Ok(())
}

/// supervisor 启动时的**状态对账**（D2-⑥）。
///
/// 崩溃可能落在 `rename(exe)` 与 `write(StartingNew)` 之间：
/// 此时状态文件仍是 `Requested`，但 staged 已被挪走。REX 在这里报
/// "staged not found" 然后回滚——**把一次已经成功的更新换回旧版**。
///
/// 判据：staged 不在，但备份已不在（说明它已被 rename 消耗）
/// 或目标文件已是新版 → 认定替换已完成，继续观察，不回滚。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    /// 无升级在进行，正常启动 worker。
    Nothing,
    /// 有待替换的 staged，继续走替换流程。
    ContinueReplace,
    /// 崩溃发生在替换之后、状态写入之前——**已替换**，转入 StartingNew。
    AlreadyReplaced,
    /// 有升级在途但 staged 消失且不满足「已替换」特征——**需要人介入**。
    Inconsistent,
}

/// 对账一次启动现场。
pub fn reconcile(state: Option<&UpdateState>, staged: &Path, backup: &Path) -> Reconcile {
    let Some(s) = state else {
        return Reconcile::Nothing;
    };
    if !s.phase.in_flight() {
        return Reconcile::Nothing;
    }
    if staged.exists() {
        return Reconcile::ContinueReplace;
    }
    // staged 已不在：要么已被 rename 消耗（替换已完成），要么被人删了。
    // 备份也已被 rename 消耗 → 那正是「回滚已执行」，与「替换已完成」同形，
    // 但两者都由 supervisor 自己推进，无需人介入 → 按已替换处理。
    if !backup.exists() {
        Reconcile::AlreadyReplaced
    } else {
        Reconcile::Inconsistent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(dir: &Path) -> UpdateState {
        UpdateState::requested("1.2.3", &dir.join("staged"), &dir.join("afcli.bak"), 7)
    }

    #[test]
    fn in_flight_covers_exactly_the_transient_phases() {
        assert!(UpdatePhase::Requested.in_flight());
        assert!(UpdatePhase::StartingNew.in_flight());
        assert!(UpdatePhase::RollingBack.in_flight());
        for p in [
            UpdatePhase::Idle,
            UpdatePhase::Committed,
            UpdatePhase::RolledBack,
            UpdatePhase::Failed,
        ] {
            assert!(!p.in_flight(), "{p} must not be in flight");
            assert!(p.is_terminal(), "{p} must be terminal");
        }
    }

    /// 七个状态逐个断言 `is_terminal`，并钉住它与 `in_flight()` 恰好互补——
    /// 两边都不落的状态会让 supervisor 既不重启也不升级，设备静默卡死。
    #[test]
    fn is_terminal_is_the_exact_complement_of_in_flight() {
        for (p, in_flight, terminal) in [
            (UpdatePhase::Idle, false, true),
            (UpdatePhase::Requested, true, false),
            (UpdatePhase::StartingNew, true, false),
            (UpdatePhase::Committed, false, true),
            (UpdatePhase::RollingBack, true, false),
            (UpdatePhase::RolledBack, false, true),
            (UpdatePhase::Failed, false, true),
        ] {
            assert_eq!(p.in_flight(), in_flight, "{p}");
            assert_eq!(p.is_terminal(), terminal, "{p}");
            assert_ne!(
                p.in_flight(),
                p.is_terminal(),
                "{p} must fall on one side only"
            );
        }
    }

    #[test]
    fn requested_state_is_in_flight_and_rollback_ready() {
        let d = tempfile::tempdir().unwrap();
        let s = st(d.path());
        assert!(s.phase.in_flight());
        assert!(s.backup_usable_for());
        assert!(!s.needs_human());
        assert_eq!(s.generation, 7);
        assert_eq!(s.rollback_count, 0);
    }

    /// D2-③：备份来自上一轮（generation 为 0）或路径缺失时不得回滚，
    /// 否则会把设备降级到陈旧版本。
    #[test]
    fn stale_or_missing_backup_is_not_rollback_usable() {
        let d = tempfile::tempdir().unwrap();
        let mut s = st(d.path());
        s.generation = 0;
        assert!(!s.backup_usable_for());

        let mut s2 = st(d.path());
        s2.backup_path = String::new();
        assert!(!s2.backup_usable_for());
    }

    /// D2-⑤：回滚过一次后不再回滚，防止「回滚 → 仍失败 → 回滚」死循环。
    #[test]
    fn already_rolled_back_once_refuses_second_rollback() {
        let d = tempfile::tempdir().unwrap();
        let mut s = st(d.path());
        s.rollback_count = 1;
        assert!(!s.backup_usable_for());
        assert!(s.needs_human(), "old version broken too: needs a human");
    }

    #[test]
    fn phase_roundtrips_through_json() {
        for p in [
            UpdatePhase::Idle,
            UpdatePhase::Requested,
            UpdatePhase::StartingNew,
            UpdatePhase::Committed,
            UpdatePhase::RollingBack,
            UpdatePhase::RolledBack,
            UpdatePhase::Failed,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: UpdatePhase = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p, "phase {p} must survive a JSON round-trip");
        }
    }

    /// 旧版本写出的状态文件缺字段时必须能读——**supervisor 的状态文件是
    /// DECISIONS D2 钉死的稳定 ABI**，缺字段即解析失败会让新 supervisor
    /// 认不出旧文件，升级流程直接卡住。
    #[test]
    fn state_written_by_an_older_version_still_parses() {
        let old = serde_json::json!({
            "phase": "starting_new",
            "version": "0.1.0",
        })
        .to_string();
        let s: UpdateState = serde_json::from_str(&old).unwrap();
        assert_eq!(s.phase, UpdatePhase::StartingNew);
        assert_eq!(s.version, "0.1.0");
        assert_eq!(s.generation, 0, "missing field must default, not fail");
        assert_eq!(s.rollback_count, 0);
        assert!(
            !s.backup_usable_for(),
            "default generation can never roll back"
        );
    }

    #[test]
    fn full_state_roundtrips_through_json() {
        let d = tempfile::tempdir().unwrap();
        let s = st(d.path());
        let text = serde_json::to_string(&s).unwrap();
        let back: UpdateState = serde_json::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn absent_file_is_not_a_problem() {
        let d = tempfile::tempdir().unwrap();
        let (state, read) = read_state(&d.path().join("nope.json"));
        assert!(state.is_none());
        assert_eq!(read, StateRead::Absent);
        assert!(!read.is_problem());
    }

    #[test]
    fn written_state_reads_back_identically() {
        let d = tempfile::tempdir().unwrap();
        let p = state_path_for(&d.path().join("afcli"));
        let s = st(d.path());
        write_state(&p, &s).unwrap();
        let (back, read) = read_state(&p);
        assert_eq!(read, StateRead::Ok);
        assert_eq!(back.unwrap(), s);
    }

    #[test]
    fn write_is_atomic_leaving_no_tmp_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = state_path_for(&d.path().join("afcli"));
        write_state(&p, &st(d.path())).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp file must be renamed away");
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = state_path_for(&d.path().join("afcli"));
        write_state(&p, &st(d.path())).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "umask must not widen update-state.json");
    }

    #[test]
    fn corrupt_state_is_reported_not_swallowed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("update-state.json");
        std::fs::write(&p, b"{ not json").unwrap();
        let (state, read) = read_state(&p);
        assert!(state.is_none(), "corrupt file must not deserialize");
        assert!(
            matches!(&read, StateRead::Corrupt(msg) if !msg.is_empty()),
            "a corrupt state file must say why, got {read:?}"
        );
        assert!(read.is_problem(), "corruption must be visible, not silent");
    }

    #[test]
    fn unreadable_state_is_reported_as_problem() {
        // 目录而非文件 → 读不了但存在
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("update-state.json");
        std::fs::create_dir(&p).unwrap();
        let (state, read) = read_state(&p);
        assert!(state.is_none());
        assert!(read.is_problem());
        assert!(matches!(read, StateRead::Unreadable(_)));
    }

    #[test]
    fn paths_sit_next_to_the_binary() {
        assert_eq!(
            state_path_for(Path::new("/usr/local/bin/afcli")),
            PathBuf::from("/usr/local/bin/afcli.update-state.json")
        );
        assert_eq!(
            backup_path_for(Path::new("/usr/local/bin/afcli")),
            PathBuf::from("/usr/local/bin/afcli.bak")
        );
    }

    #[test]
    fn reconcile_without_state_does_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            reconcile(None, &d.path().join("staged"), &d.path().join("bak")),
            Reconcile::Nothing
        );
    }

    #[test]
    fn reconcile_ignores_settled_phases() {
        let d = tempfile::tempdir().unwrap();
        let mut s = st(d.path());
        s.phase = UpdatePhase::Committed;
        assert_eq!(
            reconcile(Some(&s), &d.path().join("staged"), &d.path().join("bak")),
            Reconcile::Nothing
        );
        s.phase = UpdatePhase::Idle;
        assert_eq!(
            reconcile(Some(&s), &d.path().join("staged"), &d.path().join("bak")),
            Reconcile::Nothing
        );
    }

    #[test]
    fn reconcile_continues_when_staged_still_present() {
        let d = tempfile::tempdir().unwrap();
        let staged = d.path().join("staged");
        std::fs::write(&staged, b"x").unwrap();
        std::fs::write(d.path().join("bak"), b"y").unwrap();
        assert_eq!(
            reconcile(Some(&st(d.path())), &staged, &d.path().join("bak")),
            Reconcile::ContinueReplace
        );
    }

    /// D2-⑥ 的核心回归：崩溃落在 rename 之后、状态写入之前时，
    /// 必须认定「已替换」继续跑，**不得误回滚一次已成功的升级**。
    #[test]
    fn reconcile_treats_missing_staged_and_backup_as_already_replaced() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            reconcile(
                Some(&st(d.path())),
                &d.path().join("staged"),
                &d.path().join("bak")
            ),
            Reconcile::AlreadyReplaced
        );
    }

    #[test]
    fn reconcile_flags_inconsistent_when_backup_vanishes_alone() {
        // staged 没了但备份还在 = 有人动了文件，不是我们做的替换
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("bak"), b"y").unwrap();
        assert_eq!(
            reconcile(
                Some(&st(d.path())),
                &d.path().join("staged"),
                &d.path().join("bak")
            ),
            Reconcile::Inconsistent
        );
    }

    #[test]
    fn display_is_snake_case_and_matches_serde() {
        for (p, s) in [
            (UpdatePhase::Idle, "idle"),
            (UpdatePhase::Requested, "requested"),
            (UpdatePhase::StartingNew, "starting_new"),
            (UpdatePhase::Committed, "committed"),
            (UpdatePhase::RollingBack, "rolling_back"),
            (UpdatePhase::RolledBack, "rolled_back"),
            (UpdatePhase::Failed, "failed"),
        ] {
            assert_eq!(p.to_string(), s);
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{s}\""));
        }
    }

    #[test]
    fn leftover_exts_cover_windows_relocation_and_backup() {
        assert_eq!(LEFTOVER_EXTS, ["old", "bak"]);
    }
}
