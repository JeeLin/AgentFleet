//! supervisor 角色：拉起 worker、看退出码、替换与回滚（`DECISIONS.md` D1/D2）。
//!
//! **决策不在这里**：全在 [`node_core::supervisor::decide`]，那是纯函数，
//! 已被 100% 行覆盖。本文件是 executor——把 `Decision` 变成真实副作用。
//!
//! 拆开的理由与 D2 一致：把 spawn / rename / 写状态内联进阻塞循环，
//! 要覆盖它就只能真的删掉正在运行的二进制。

use std::io::{Sink, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use node_core::entry::{self, ConsoleEcho, ROLE_SUPERVISOR};
use node_core::supervisor::{
    self, exit_code, CrashStats, Decision, ExitInfo, Inputs, EXIT_STORM_WINDOW_SECS,
};
use node_core::update::{self, UpdatePhase, UpdateState};

use crate::runargs::WORKER_FLAG;

/// supervisor 停下等人的退出码。
///
/// 不属于 worker 的退出码协议（`0/10/11/12`）——那套是 supervisor 读 worker 的。
/// 这条只是让 systemd / journal 里一眼看出「它自己停了，且不是被 kill 的」。
pub const SUPERVISOR_STOPPED: u8 = 20;

/// 子进程 argv = **父进程 argv 原样 + `--worker`**（D1）。
///
/// 「原样」是刻意的：用户给的参数（`AFCLI_HUB_URL` 由环境继承，其余全部
/// 自动传下去）不必在这里重新拼一遍——拼一遍就多一处可能漏字段的地方，
/// 而漏字段的后果是 worker 跑在另一个数据目录 / 连着另一个 Hub。
///
/// 幂等：父进程本来就带 `--worker`（Docker 里直接指定、或人手误敲）时
/// 不重复追加，否则会出现 `run --worker --worker`（标准①b）。
pub fn worker_argv(parent_argv: &[String]) -> Vec<String> {
    let mut argv = parent_argv.to_vec();
    if !argv.iter().any(|a| a == WORKER_FLAG) {
        argv.push(WORKER_FLAG.to_string());
    }
    argv
}

/// 进程退出码 → 退出码协议。
///
/// 被信号杀死时 `ExitStatus::code()` 是 `None`——那是**崩溃**（标准②：
/// `kill -9` 之后 supervisor 要能重拉且 `node_id` 不变），
/// 而不是让 supervisor 自己 panic：`-9` 不是合法的 i32。
pub fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(exit_code::CRASH)
}

/// 等子进程退出并折算成退出码。**`Err` 留给调用方决定怎么留痕**。
///
/// ## 那条 `Err` 分支怎么走得到
///
/// `std::process::Child::wait` 的文档（`process.rs`，`Child::wait`）原文是
/// 「This function will continue to have the same return value after it has
/// been called at least once」——即**调用过一次之后，后续调用返回同一个值**，
/// 而不是报错。所以「先 wait 一次、再 wait 一次拿 `ECHILD`」构造不出来；
/// 子进程被信号杀死时也返回 `Ok`，只是 `status.code()` 为 `None`，走的是
/// [`exit_code_of`]。
///
/// 能稳定走到的是另一种生产形态：父进程继承 `SIGCHLD=SIG_IGN` 时内核
/// 自动回收子进程，`wait` 拿不到可回收的 pid，返回 `ECHILD`。改信号处置
/// 是进程级的，所以下面 `a_wait_failure_counts_as_a_crash_and_says_so`
/// 在专用子进程里做；那条用例覆盖 `Err` 分支和它留下的那行日志。
fn wait_code(child: &mut std::process::Child) -> std::io::Result<i32> {
    child.wait().map(|status| exit_code_of(&status))
}

/// 启动后第一次循环：还没有「上一次 worker 退出」可观测。
/// 造一个「正常退出」的观测即可——此刻还没崩过，不存在需要退避的失败。
fn initial_exit() -> ExitInfo {
    ExitInfo::new(exit_code::NORMAL, 0)
}

/// 启动对账判定为「上次替换没做完」时，`decide` 的输入退出码。
///
/// 复用 `exit(10)`（worker 请求更新）那条路：此刻磁盘上的状态正是
/// 「worker 请求了更新、supervisor 还没换」，而 `decide` 对 `ContinueReplace`
/// 的处理方式就是落到 `exit(10)` 分支。喂别的码会走到「有更新在途 →
/// 这一版不健康 → 回滚」，把一次没做完的升级误当成失败。
fn startup_exit(reconciled: update::Reconcile) -> ExitInfo {
    match reconciled {
        update::Reconcile::ContinueReplace => ExitInfo::new(exit_code::UPDATE_REQUEST, 0),
        update::Reconcile::Nothing
        | update::Reconcile::AlreadyReplaced
        | update::Reconcile::Inconsistent => initial_exit(),
    }
}

/// `decide` 需要的是**磁盘事实**，不是状态文件里那个路径字符串长什么样
/// （D2-⑥：按非空判会驱动一次指向不存在文件的回滚）。
fn backup_available(state: Option<&UpdateState>, derived: &Path) -> bool {
    state.is_some_and(|s| s.backup_usable_for()) && derived.exists()
}

/// 记一次退出，顺带把两个「按时间驱动」的口径更新掉。
///
/// [`CrashStats`] 只管计数，什么时候滑窗、什么时候算「跑稳了」由这里按时钟决定：
/// - 相邻两次退出间隔 ≥ 10s → 清一次窗口计数（否则凌晨 3 点那次崩溃还留着白天的计数）；
/// - 跑满 10s 才退出 → 不是崩溃循环，连续计数归零、退避回落，否则一次偶发崩溃
///   会让此后每次重启都背 30s。
pub fn record_exit(stats: &mut CrashStats, exit: &ExitInfo, since_prev: Option<Duration>) {
    if let Some(gap) = since_prev {
        if gap >= Duration::from_secs(EXIT_STORM_WINDOW_SECS) {
            stats.window_elapsed();
        }
    }
    stats.record_exit(exit);
    if exit.ran_for_secs >= EXIT_STORM_WINDOW_SECS {
        stats.ran_stably();
    }
}

/// supervisor 需要的全部东西。
///
/// `C` / `E` 是**用户正盯着的那块屏幕**（stdout / stderr）。前台启动时
/// supervisor 的每一行日志都要同时回显到它们上面，否则 `afcli run` 看起来
/// 就是「启动了却什么都不说」（用户反馈原话）。用 [`ConsoleEcho`] 包一层，
/// 是因为 `write_console` 按值收 writer，而 `say` 在整个 `supervise` 循环里
/// 会被反复调用——克隆出来的句柄都指向同一个 fd / 同一块缓冲。
pub struct Supervisor<C: std::io::Write, E: std::io::Write> {
    /// 父进程 argv（不含程序名），原样传给 worker。
    argv: Vec<String>,
    data_dir: PathBuf,
    /// 被管理的二进制 = supervisor 自己（D2：supervisor 懒生效，
    /// 被换掉的文件下次被拉起时才生效）。
    exe: PathBuf,
    console: ConsoleEcho<C>,
    err: ConsoleEcho<E>,
}

impl<C: std::io::Write, E: std::io::Write> Supervisor<C, E> {
    /// 生产构造：被管理的二进制就是当前这个可执行文件。
    // 单元、覆盖率不计；单测入口是 `with_exe`。
    pub fn new(
        argv: Vec<String>,
        data_dir: PathBuf,
        console: ConsoleEcho<C>,
        err: ConsoleEcho<E>,
    ) -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(crate::BIN_NAME));
        Self::with_exe_and_console(argv, data_dir, exe, console, err)
    }

    /// 显式指定二进制**和**两块控制台。前台启动与「控制台要能看到日志」
    /// 的用例走这里；`C` / `E` 允许是非 `'static` 的借用（`&mut Vec<u8>`），
    /// 因为 supervisor 不跨线程持有它们。
    pub fn with_exe_and_console(
        argv: Vec<String>,
        data_dir: PathBuf,
        exe: PathBuf,
        console: ConsoleEcho<C>,
        err: ConsoleEcho<E>,
    ) -> Self {
        Self {
            argv,
            data_dir,
            exe,
            console,
            err,
        }
    }
}

/// 单测入口：控制台输出丢进 `sink()`。
///
/// 单独一个 `impl Supervisor<Sink, Sink>` 而不是放在泛型 impl 里：
/// `with_exe` 返回的是具体类型，泛型参数无从推断，测试里 `let sup = ...`
/// 也就不需要写类型标注。
impl Supervisor<Sink, Sink> {
    /// 显式指定被管理的二进制。单测要对着临时目录里的假二进制做替换/回滚，
    /// 不能拿测试进程自己当目标。
    pub fn with_exe(argv: Vec<String>, data_dir: PathBuf, exe: PathBuf) -> Self {
        Self::with_exe_and_console(
            argv,
            data_dir,
            exe,
            ConsoleEcho::new(std::io::sink()),
            ConsoleEcho::new(std::io::sink()),
        )
    }
}

impl<C: std::io::Write, E: std::io::Write> Supervisor<C, E> {
    /// 阻塞循环：拉起 worker → 阻塞等它退出 → `decide` → 执行决策 → 再拉起。
    /// 返回进程退出码。
    pub fn supervise(&self) -> std::process::ExitCode {
        self.rotate_log();
        self.say(&format!(
            "supervisor starting, argv: {}",
            self.argv.join(" ")
        ));

        let mut stats = CrashStats::default();
        // `None` = 还没有过一次 worker 退出（首轮）。
        let mut pending: Option<ExitInfo> = None;
        let mut last_exit_at: Option<Instant> = None;
        loop {
            let (state, reconciled) = self.read_state();
            let state_ref = state.as_ref();

            let (exit, reconciled_for_decide) = match pending.take() {
                Some(observed) => {
                    record_exit(&mut stats, &observed, last_exit_at.map(|t| t.elapsed()));
                    last_exit_at = Some(Instant::now());
                    // 对账只在首轮做：此后状态文件由本进程独占写入，
                    // 「崩溃落在 rename 与写状态之间」这个窗口不会再出现（D2-⑥）。
                    (observed, update::Reconcile::Nothing)
                }
                None => (startup_exit(reconciled), reconciled),
            };

            let decision = supervisor::decide(Inputs {
                exit,
                state: state_ref,
                stats,
                reconcile: reconciled_for_decide,
                backup_available: backup_available(state_ref, &self.backup_path(state_ref)),
            });

            match decision {
                Decision::Spawn {
                    backoff_secs,
                    reason,
                } => {
                    self.say(&format!("{reason}; backing off {backoff_secs}s"));
                    std::thread::sleep(Duration::from_secs(backoff_secs));
                }
                Decision::ReplaceAndSpawn { version, reason } => {
                    self.say(&format!("{reason}: target version {version}"));
                    // 失败臂用不可写的备份路径覆盖；成功臂由下面的
                    // successful_replace_then_rollback_reaches_the_stop_decision 驱动下一轮决策。
                    if let Err(e) = self.replace(state_ref, &version) {
                        // D2-④：替换失败即**放弃本次更新**，绝不带着半换好的
                        // 二进制继续 spawn——那正是「说一套做一套」的现场。
                        self.say(&format!("replacement to {version} failed: {e}"));
                        self.persist_state(&self.failed_state(state_ref, &version));
                        return stopped();
                    }
                }
                // 失败臂由 `a_failed_rollback_stops_the_supervisor` 覆盖；
                // 成功臂由下面的 successful_replace_then_rollback_reaches_the_stop_decision 覆盖。
                Decision::Rollback { reason } => {
                    self.say(&reason);
                    if let Err(e) = self.rollback(state_ref) {
                        self.say(&format!("rollback failed: {e}"));
                        return stopped();
                    }
                }
                Decision::Stop { reason } => {
                    // **必须留痕**：停下来的 supervisor 不会出现在 `ps` 里，
                    // 设备表现为「连不上又不报错」——那是最难查的一种失联。
                    self.say(&format!("stopping, human intervention required: {reason}"));
                    self.say("the device will stay unreachable until it is fixed");
                    // **同一句也要进 stdout**：supervisor 停下来后设备表现为
                    // 「连不上又不报错」，而 `say` 只写日志文件——看终端的人
                    // 只见到进程平静地退出（码 20），不知道该动哪边。
                    // stdout 在 systemd 下进 journal，这条才是运维的第一落点。
                    drop(writeln!(
                        self.console.clone(),
                        "afcli: stopping, human intervention required: {reason}\n\
                         afcli: the device will stay unreachable until it is fixed; \
                         see {}",
                        node_core::log::log_path(&self.data_dir).display()
                    ));
                    self.persist_state(&self.failed_state(state_ref, ""));
                    return stopped();
                }
            }
            pending = Some(self.spawn_and_wait());
        }
    }

    /// 拉起 worker 并阻塞到它退出，返回这次退出的观测。
    ///
    /// stdout/stderr 直接继承：worker 的输出要进 supervisor 所在的 journal，
    /// 中间再插一层转发只会让排障多一跳。
    // 在单元测试里等于递归起自己。
    fn spawn_and_wait(&self) -> ExitInfo {
        let started = Instant::now();
        let mut child = match Command::new(&self.exe)
            .args(worker_argv(&self.argv))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                // 拉不起来不是「等一会儿就好」的事（多半是二进制被换掉了或
                // 权限没了）。记下来并按「崩溃」观测走决策：连续两次拉不起
                // 就撞上退出风暴，supervisor 停下来等人。
                self.say(&format!("cannot spawn worker: {e}"));
                return ExitInfo::new(exit_code::CRASH, 0);
            }
        };
        let code = match wait_code(&mut child) {
            Ok(code) => code,
            Err(e) => {
                self.say(&format!("cannot wait for worker: {e}"));
                exit_code::CRASH
            }
        };
        ExitInfo::new(code, started.elapsed().as_secs())
    }

    /// 读状态 + 对账。
    ///
    /// 状态文件每次都重读：worker 随时可能写它（`exit(10)` 那条路）。
    /// supervisor 不缓存——拿一份十几秒前的快照去判「二进制换没换」，
    /// 判错方向永远是「以为没换」。
    fn read_state(&self) -> (Option<UpdateState>, update::Reconcile) {
        let (state, read) = update::read_state(&self.state_path());
        if read.is_problem() {
            // 损坏不致命，但必须留痕：一个坏掉的状态文件静默降级成
            // 「没有升级在进行」，会让更新流程做出错误判断。
            self.say(&format!("update state unreadable: {read:?}"));
        }
        let reconciled = update::reconcile(
            state.as_ref(),
            &self.staged_path(state.as_ref()),
            &self.backup_path(state.as_ref()),
        );
        (state, reconciled)
    }

    /// 执行 `ReplaceAndSpawn`：备份当前二进制 → 记账 → rename(staged → exe)。
    ///
    /// **备份先行且必须成功**（D2-③）：备份失败后若还去 rename，一旦新版起不来，
    /// 回滚会拿到上一轮遗留的 `.bak`，把设备降级到几个月前的陈旧版本而无人察觉。
    /// `rename` 而不是「先删再复制」：Linux 上覆盖正在运行的二进制会 `ETXTBSY`。
    fn replace(&self, state: Option<&UpdateState>, version: &str) -> Result<(), String> {
        let staged = self.staged_path(state);
        let backup = self.backup_path(state);
        if !staged.exists() {
            return Err(format!("staged binary missing: {}", staged.display()));
        }
        self.say(&format!(
            "backing up {} to {}",
            self.exe.display(),
            backup.display()
        ));
        std::fs::copy(&self.exe, &backup)
            .map_err(|e| format!("backup to {}: {e}", backup.display()))?;

        // 状态在 rename **之前**落：写失败就当场放弃更新（账本写不上去时
        // 继续换二进制 = 说一套做一套），而写成功之后崩溃只可能停在
        // 「已换完、还没记账」，由启动对账认出来（D2-⑥）。
        let mut next = state.cloned().unwrap_or_else(|| self.blank_state(version));
        next.phase = UpdatePhase::StartingNew;
        next.version = version.to_string();
        next.staged_path = staged.display().to_string();
        next.backup_path = backup.display().to_string();
        self.persist_state(&next);

        std::fs::rename(&staged, &self.exe)
            .map_err(|e| format!("rename {} into place: {e}", staged.display()))?;
        self.say(&format!(
            "replaced {} with {}, backup kept at {}",
            self.exe.display(),
            staged.display(),
            backup.display()
        ));
        Ok(())
    }

    /// 执行 `Rollback`：把 `.bak` 换回原位。
    ///
    /// `rename` 会**消耗掉**备份。这是顺带的效果，不是防线：真正的防线是
    /// `rollback_count`（D2-⑤）——`backup_usable_for()` 会因为它增长而返回假。
    fn rollback(&self, state: Option<&UpdateState>) -> Result<(), String> {
        let backup = self.backup_path(state);
        if !backup.exists() {
            return Err(format!("no backup to roll back to: {}", backup.display()));
        }
        std::fs::rename(&backup, &self.exe)
            .map_err(|e| format!("rename {} into place: {e}", backup.display()))?;

        let mut next = state.cloned().unwrap_or_else(|| self.blank_state(""));
        next.phase = UpdatePhase::RolledBack;
        next.rollback_count += 1;
        // 备份已被 rename 消耗。路径留着会让下一次启动的 `reconcile`
        // 以为「备份还在」——留着比清空更容易误判。
        next.backup_path.clear();
        self.persist_state(&next);
        self.say("rolled back to the previous binary");
        Ok(())
    }

    /// 造一条可用的状态记录（状态文件不存在时）。路径按 executor 的规则算出来，
    /// 决策层不猜路径。
    fn blank_state(&self, version: &str) -> UpdateState {
        UpdateState::requested(
            version,
            &self.staged_path(None),
            &self.backup_path(None),
            // 世代号从 1 起：0 会让 `backup_usable_for()` 为假，
            // 于是刚做好的备份立刻被判「不可用于回滚」。
            1,
        )
    }

    /// 标 `Failed` 的状态记录。没有既有状态时用版本号/version 空白的兜底记录——
    /// `decide` 只在有状态时才返回需要写状态的决策，这里只是不让写状态这条
    /// 路径因为「文件恰好不存在」而消失。
    fn failed_state(&self, state: Option<&UpdateState>, version: &str) -> UpdateState {
        let mut next = state.cloned().unwrap_or_else(|| self.blank_state(version));
        next.phase = UpdatePhase::Failed;
        next
    }

    /// 写状态。**写失败必须留痕**（D2-④），且不能把调用方带崩：
    /// 状态文件是账本，账写不上去时设备该跑还是要跑。
    fn persist_state(&self, state: &UpdateState) {
        if let Err(e) = update::write_state(&self.state_path(), state) {
            self.say(&format!(
                "cannot write {}: {e} — the update bookkeeping is now behind the disk",
                self.state_path().display()
            ));
        }
    }

    fn state_path(&self) -> PathBuf {
        update::state_path_for(&self.exe)
    }

    /// 本轮 staged 二进制的路径。状态文件里记的优先（谁下载谁记路径）；
    /// 没记时用 `<exe>.new` 这个约定名。
    fn staged_path(&self, state: Option<&UpdateState>) -> PathBuf {
        match state.map(|s| s.staged_path.as_str()) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => self.exe.with_extension("new"),
        }
    }

    /// 本轮备份路径。与 [`Self::staged_path`] 同规则。
    fn backup_path(&self, state: Option<&UpdateState>) -> PathBuf {
        match state.map(|s| s.backup_path.as_str()) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => update::backup_path_for(&self.exe),
        }
    }

    /// 启动时轮转一次日志。
    fn rotate_log(&self) {
        entry::rotate_at_startup(&self.data_dir, ROLE_SUPERVISOR);
    }

    /// 一行日志：落**同一份文件**，同时回显到前台控制台。
    ///
    /// 级别固定 `info`——决策与故障都值得进同一份文件，而 supervisor 的
    /// 日志量本来就小（每次 worker 退出才一行）。回显走 stdout：它是用户
    /// 敲下 `afcli run` 时正盯着的那块屏幕，systemd 下会进 journal。
    /// 文件里绝不出现两份（见 [`entry::write_console`]）。
    fn say(&self, msg: &str) {
        entry::write_console(
            &self.data_dir,
            ROLE_SUPERVISOR,
            "info",
            msg,
            self.console.clone(),
            self.err.clone(),
        );
    }
}

fn stopped() -> std::process::ExitCode {
    std::process::ExitCode::from(SUPERVISOR_STOPPED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use node_core::log;

    /// 每个测试一个独立目录：测试并行跑，共享目录会让「故意写坏的夹具」
    /// 把别人的 setUp 搞崩。
    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("afcli-sup-{name}"));
        // remove_dir_all 对普通文件**返回 Err**（只删目录）。上一个用例
        // 故意留下的「not a dir」文件必须被 remove_file 清掉，否则
        // create_dir_all 报 EEXIST，而那个 Err 与测试要造的失败一模一样，
        // 于是两个用例互相污染、单独跑却全绿。
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(&p);
        std::fs::create_dir_all(&p).expect("tempdir");
        p
    }

    /// 把 `d` 变成一个普通文件，使它的子路径全部写不进去。
    fn as_file(d: &Path) {
        let _ = std::fs::remove_dir_all(d);
        std::fs::write(d, b"not a dir").expect("blocker");
    }

    fn sup(name: &str) -> (PathBuf, Supervisor<Sink, Sink>) {
        let d = tmp(name);
        let exe = d.join("afcli");
        std::fs::write(&exe, b"#!/bin/true\n").expect("fake binary");
        let sup = Supervisor::with_exe(vec!["run".to_string()], d.clone(), exe);
        (d, sup)
    }

    fn state_at(d: &Path) -> UpdateState {
        UpdateState::requested("9.9.9", &d.join("afcli.new"), &d.join("afcli.bak"), 1)
    }

    /// 子进程 argv = 父 argv 原样 + `--worker`（D1：用户参数全部继承）。
    #[test]
    fn worker_argv_appends_the_worker_flag() {
        let got = worker_argv(&[
            "run".to_string(),
            "--hub".to_string(),
            "ws://h/ws".to_string(),
        ]);
        assert_eq!(
            got,
            vec![
                "run".to_string(),
                "--hub".to_string(),
                "ws://h/ws".to_string(),
                "--worker".to_string()
            ]
        );
    }

    /// 幂等：父 argv 已带 `--worker`（Docker 里直接指定、人手误敲）时
    /// 不能追加第二个，否则会出现 `run --worker --worker`。
    #[test]
    fn worker_argv_is_idempotent() {
        let argv = vec!["run".to_string(), WORKER_FLAG.to_string()];
        assert_eq!(worker_argv(&argv), argv);
        // 在任意位置都算，不只最后一个。
        let argv = vec![WORKER_FLAG.to_string(), "run".to_string()];
        assert_eq!(worker_argv(&argv), argv);
    }

    /// `--worker` 之外的用户参数不能被吞：漏一个就 worker 连到另一个 Hub。
    #[test]
    fn worker_argv_keeps_every_user_flag() {
        let argv: Vec<String> = ["run", "--hub", "ws://h/ws", "-v"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = worker_argv(&argv);
        for a in &argv {
            assert!(got.contains(a), "{a} dropped from {got:?}");
        }
        assert_eq!(got.len(), argv.len() + 1);
    }

    /// 被信号杀死时 `code()` 是 `None`——那是崩溃（标准②），
    /// 不能让 supervisor 自己 panic（`-9` 不是合法 i32）。
    #[test]
    fn a_killed_worker_counts_as_a_crash() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .spawn()
            .expect("spawn sh");
        let status = child.wait().expect("wait");
        assert!(status.code().is_none(), "sh 应被 -9 杀死");
        assert_eq!(exit_code_of(&status), exit_code::CRASH);
    }

    /// **拉不起来**的 worker 算崩溃，并在 journal 里留下**能搜到**的痕迹。
    ///
    /// 判据是「崩溃 + journal 里有 `cannot spawn worker`」，不是「返回了非零」：
    /// 一个静默返回 CRASH 的实现同样能让 supervisor 继续跑下去，但排障的人
    /// 只会看到一个反复重启的 worker，没有任何线索说明是拉不起来还是回收失败。
    ///
    /// 用 `with_exe` 指向一个**存在但不可执行**的文件来稳定触发 spawn 臂；
    /// 这条路径在生产里对应的正是「二进制被换掉了 / 权限没了」。
    ///
    /// **这条用例没有覆盖 `wait` 的失败臂**（`supervisor.rs` 里那处
    /// `cannot wait for worker` 的日志）。用户可见的两条臂文案不同、判据不同，
    /// 但只有 spawn 臂能稳定构造；`wait` 臂为什么构造不出来、为什么它的 `Err`
    /// 分支事实上不可达，写在 [`wait_code`] 的文档里——那里的结论是实测过的
    /// （`Child::wait` 二次调用返回 `Ok(0)` 而不是 `Err`），不是推测。
    /// 所以 `"cannot wait for worker"` 这句文案**目前没有任何测试守着**，
    /// 改掉它不会有任何用例变红——这一点如实记在这里，不假装覆盖到了。
    ///
    /// 另：`ran_for_secs == 0` 这个断言**不是在验证测量结果**。spawn 失败时
    /// `spawn_and_wait` 直接返回 `ExitInfo::new(CRASH, 0)`，那个 0 是写死的，
    /// 断言只是在钉住「这条路径不许报出非零时长」这个约定。
    #[test]
    fn a_worker_that_cannot_be_spawned_is_a_crash_and_says_so() {
        let d = tmp("cannot-spawn");
        let exe = d.join("afcli");
        // 有文件、但没有任何可执行位——`Command::spawn` 必给 EACCES。
        std::fs::write(&exe, b"not executable\n").expect("fake binary");
        let sup = Supervisor::with_exe(vec!["run".to_string()], d.clone(), exe);
        let got = sup.spawn_and_wait();
        assert_eq!(got.code, exit_code::CRASH, "拉不起来必须算崩溃");
        assert_eq!(got.ran_for_secs, 0, "没跑起来的进程不该报出运行时长");
        let logged = std::fs::read_to_string(log::log_path(&d)).unwrap_or_default();
        assert!(
            logged.contains("cannot spawn worker"),
            "必须留下可搜的痕迹，实际日志：{logged:?}"
        );
    }

    /// `wait_code` 是 `spawn_and_wait` 唯一用来读退出码的接缝：`Child::wait`
    /// 成功时既要折算成协议码，也要把 `Ok` 原样交出去。直接在这里喂一条
    /// `exit 10` 的子进程，覆盖的是 `supervise` 正常循环必走的那一行，
    /// 不需要真的 spawn 一个 supervisor 出来。
    #[test]
    #[cfg(unix)] // 用 `sh -c 'kill -9 $$'` 造无退出码的子进程
    fn wait_code_reads_a_live_childs_exit_status() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 10"])
            .spawn()
            .expect("spawn sh");
        let code = wait_code(&mut child).expect("wait must succeed");
        assert_eq!(code, exit_code::UPDATE_REQUEST);
        // 信号杀死时 `status.code()` 是 `None`：折算成 CRASH，不是 panic。
        let mut killed = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .spawn()
            .expect("spawn sh");
        assert_eq!(
            wait_code(&mut killed).expect("wait must succeed"),
            exit_code::CRASH
        );
    }

    /// `spawn_and_wait` 的成功路径：被管理的二进制真的被拉起、真的被等回收，
    /// 且退出码穿过 `wait_code` 原样进 `ExitInfo`。`supervise` 里除 Stop 之外
    /// 的每条决策都会落到这一行，单测用临时脚本把它走完，不必起无限循环。
    #[test]
    #[cfg(unix)] // `#!/bin/sh` 假 worker 脚本 + chmod 0o755
    fn spawn_and_wait_records_a_real_child_exit() {
        let (d, mut s) = sup("wait-success");
        // `sup()` 里的假二进制不是可执行脚本，这里换成一个真的 `exit 11`。
        std::fs::write(&s.exe, "#!/bin/sh\nexit 11\n").expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&s.exe, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        s.argv = vec![];
        let got = s.spawn_and_wait();
        assert_eq!(
            got.code,
            exit_code::HEALTH_FAILURE,
            "exit(11) 必须原样穿过，supervisor 靠它判新版不健康"
        );
        assert!(
            got.ran_for_secs < 5,
            "秒退的脚本不该被记成跑满了窗口：{got:?}"
        );
        assert!(
            s.exe.exists(),
            "spawn_and_wait 不许动被管理的二进制：{:?}",
            s.exe
        );
        let _ = d;
    }

    /// 生产构造函数用的就是「当前可执行文件」：不替换测试进程自身，
    /// 只断言它把 argv / 数据目录原样记下来、exe 指向一个真实存在的路径。
    /// `dispatch` 的生产路径（无测试注入的 exe）走的就是 `new`。
    #[test]
    fn new_manages_the_current_executable_and_keeps_the_argv() {
        let d = tmp("new");
        let argv = vec!["run".to_string(), "--hub".to_string()];
        let s = Supervisor::new(
            argv.clone(),
            d.clone(),
            ConsoleEcho::new(std::io::sink()),
            ConsoleEcho::new(std::io::sink()),
        );
        assert_eq!(s.argv, argv);
        assert_eq!(s.data_dir, d);
        assert!(
            s.exe.is_absolute() && s.exe.exists(),
            "current_exe 必须是一个存在的绝对路径，实际 {:?}",
            s.exe
        );
        assert!(
            s.exe != d.join("afcli"),
            "生产构造不许指向测试夹具里那个假二进制"
        );
    }

    /// 正常退出码必须原样穿过：`exit(10)`（请求更新）与 `exit(11)`（健康失败）
    /// 是 supervisor 决策的全部依据，压缩成「非零」就什么也判不出来。
    #[test]
    fn a_clean_exit_code_passes_through() {
        let ok = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .expect("run sh");
        assert_eq!(exit_code_of(&ok), 0);
        let ten = std::process::Command::new("sh")
            .args(["-c", "exit 10"])
            .status()
            .expect("run sh");
        assert_eq!(exit_code_of(&ten), exit_code::UPDATE_REQUEST);
    }

    /// 首轮没有「上一次退出」可观测，造一个正常退出即可：
    /// 此刻还没崩过，不该背任何退避。
    #[test]
    fn the_first_loop_sees_a_normal_exit() {
        let e = initial_exit();
        assert_eq!(e.code, exit_code::NORMAL);
        assert_eq!(e.ran_for_secs, 0);
    }

    /// 启动对账判定「上次替换没做完」→ 喂 `exit(10)`。
    /// 喂别的码会走到「有更新在途 → 这一版不健康 → 回滚」，
    /// 把一次没做完的升级误判成失败。
    #[test]
    fn an_unfinished_replacement_reuses_the_update_request_exit() {
        assert_eq!(
            startup_exit(update::Reconcile::ContinueReplace).code,
            exit_code::UPDATE_REQUEST
        );
    }

    /// 其余三种对账结果都不是「上次没做完」，一律按首轮处理。
    #[test]
    fn the_other_reconcile_outcomes_start_clean() {
        for r in [
            update::Reconcile::Nothing,
            update::Reconcile::AlreadyReplaced,
            update::Reconcile::Inconsistent,
        ] {
            assert_eq!(startup_exit(r).code, exit_code::NORMAL, "{r:?}");
        }
    }

    /// `decide` 要的是**磁盘事实**：状态文件里记着路径不等于文件在（D2-⑥）。
    #[test]
    fn backup_available_checks_the_disk_not_the_state_string() {
        let d = tmp("bak");
        let s = state_at(&d);
        assert!(!backup_available(Some(&s), &d.join("afcli.bak")));
        std::fs::write(d.join("afcli.bak"), b"old").expect("write bak");
        assert!(backup_available(Some(&s), &d.join("afcli.bak")));
    }

    /// 没有状态文件 → 一律判「不可用」，哪怕 `.bak` 就在盘上。
    ///
    /// 这不是保守而是正确：没有状态就没有「备份属于哪一版、回滚过几次」的
    /// 记账，把来路不明的 `.bak` rename 上去等于用未知版本覆盖已知版本。
    #[test]
    fn backup_available_without_state_is_false_even_if_the_file_exists() {
        let d = tmp("bak2");
        std::fs::write(d.join("afcli.bak"), b"old").expect("write");
        assert!(!backup_available(None, &d.join("afcli.bak")));
    }

    /// 跑满 10s 才退出 → 连击清零：一次偶发崩溃不该让此后每次重启都背 30s。
    #[test]
    fn a_long_running_exit_resets_the_streak() {
        let mut stats = CrashStats {
            recent_exits: 1,
            consecutive: 5,
            rollback_count: 0,
        };
        record_exit(&mut stats, &ExitInfo::new(exit_code::NORMAL, 30), None);
        assert_eq!(stats.consecutive, 0);
    }

    /// 秒崩不清零：那正是「这一版不健康」的信号，退避要继续涨。
    #[test]
    fn a_fast_crash_keeps_the_streak_growing() {
        let mut stats = CrashStats::default();
        for _ in 0..2 {
            record_exit(&mut stats, &ExitInfo::new(exit_code::CRASH, 0), None);
        }
        assert_eq!(stats.consecutive, 2);
        assert_eq!(
            node_core::supervisor::backoff_secs(stats.consecutive),
            4,
            "连续退出的退避必须继续涨"
        );
    }

    /// 相邻两次退出间隔 ≥ 10s → 滑窗：凌晨 3 点的崩溃不该留着白天的计数。
    #[test]
    fn a_long_gap_between_exits_slides_the_window() {
        let mut stats = CrashStats {
            recent_exits: 3,
            consecutive: 3,
            rollback_count: 0,
        };
        record_exit(
            &mut stats,
            &ExitInfo::new(exit_code::CRASH, 0),
            Some(Duration::from_secs(30)),
        );
        assert_eq!(stats.recent_exits, 1, "窗口应先清零再记这一次");
    }

    /// 间隔不足 10s → 窗口不清：连续退出正是要捕捉的形态。
    #[test]
    fn a_short_gap_keeps_the_window_counting() {
        let mut stats = CrashStats {
            recent_exits: 3,
            consecutive: 3,
            rollback_count: 0,
        };
        record_exit(
            &mut stats,
            &ExitInfo::new(exit_code::CRASH, 0),
            Some(Duration::from_secs(2)),
        );
        assert_eq!(stats.recent_exits, 4);
    }

    // ---- executor：真的动文件系统，但目标都是临时目录里的假二进制 ----

    /// 备份先行且必须成功（D2-③）：备份失败后若还去 rename，一旦新版起不来，
    /// 回滚会拿到上一轮遗留的 `.bak`，把设备降级到几个月前的陈旧版本。
    #[test]
    fn replace_backs_up_before_swapping_and_records_the_state() {
        let (d, s) = sup("replace");
        let staged = d.join("afcli.new");
        let backup = d.join("afcli.bak");
        std::fs::write(&staged, b"new binary").expect("staged");
        std::fs::write(&s.exe, b"old binary").expect("exe");

        let st = state_at(&d);
        s.replace(Some(&st), "9.9.9").expect("replace");

        assert_eq!(std::fs::read(&backup).expect("bak"), b"old binary");
        assert_eq!(std::fs::read(&s.exe).expect("exe"), b"new binary");
        assert!(!staged.exists(), "staged 应被 rename 消耗掉");
        let (after, _) = update::read_state(&s.state_path());
        let after = after.expect("state written");
        assert_eq!(after.phase, UpdatePhase::StartingNew);
        assert_eq!(after.version, "9.9.9");
    }

    /// staged 不在 → 放弃更新而不是换一个不存在的文件上去。
    #[test]
    fn replace_refuses_when_the_staged_binary_is_missing() {
        let (d, s) = sup("replace-missing");
        let st = state_at(&d);
        let err = s.replace(Some(&st), "9.9.9").expect_err("must refuse");
        assert!(err.contains("staged binary missing"), "{err}");
        assert!(!d.join("afcli.bak").exists(), "不该留下备份");
        assert_eq!(std::fs::read(&s.exe).expect("exe"), b"#!/bin/true\n");
    }

    /// 备份写不进去（目录不存在）→ 报错且**不动**正在运行的二进制。
    #[test]
    fn replace_aborts_before_touching_the_exe_when_backup_fails() {
        let (d, s) = sup("replace-bakfail");
        let staged = d.join("afcli.new");
        std::fs::write(&staged, b"new").expect("staged");
        // 让备份路径指向一个不可写的位置：用一个普通文件当父目录。
        let mut bad = state_at(&d);
        bad.backup_path = d.join("not-a-dir").join("afcli.bak").display().to_string();
        std::fs::write(d.join("not-a-dir"), b"blocker").expect("blocker");
        let err = s.replace(Some(&bad), "9.9.9").expect_err("must fail");
        assert!(err.contains("backup to"), "{err}");
        // staged 必须留在盘上：备份写不进去就放弃更新，但下载好的新二进制
        // 不能跟着消失——否则下次升级得重新下载一遍。
        assert!(staged.exists(), "staged 必须留着，否则更新凭空消失");
    }

    /// **没有备份可回滚时报错**（D2-⑤ 的另一半）：宁可停下来让人看，
    /// 也不 rename 一个不存在的路径上去——那会让 `exe` 消失，设备连
    /// supervisor 都起不来，是最难恢复的一种故障。
    #[test]
    fn rollback_never_touches_the_exe_when_there_is_no_backup() {
        let (d, s) = sup("rollback-missing2");
        std::fs::write(&s.exe, b"current").expect("exe");
        let st = state_at(&d);
        let err = s.rollback(Some(&st)).expect_err("must refuse");
        assert!(err.contains("no backup"), "{err}");
        assert_eq!(std::fs::read(&s.exe).expect("exe"), b"current");
    }

    /// 没有既有状态文件时也能替换：`blank_state` 补一条可用记录。
    #[test]
    fn replace_without_a_state_file_still_books_its_work() {
        let (d, s) = sup("replace-nostate");
        std::fs::write(d.join("afcli.new"), b"new").expect("staged");
        s.replace(None, "1.0.1").expect("replace");
        let (after, _) = update::read_state(&s.state_path());
        let after = after.expect("state written");
        assert_eq!(after.version, "1.0.1");
        assert!(after.backup_usable_for(), "刚做好的备份必须可用于回滚");
    }

    /// 回滚把 `.bak` 换回原位，并把它从账本上划掉——留着路径会让下一次
    /// 启动的对账以为「备份还在」。
    #[test]
    fn rollback_restores_the_backup_and_clears_its_path() {
        let (d, s) = sup("rollback");
        std::fs::write(d.join("afcli.bak"), b"old").expect("bak");
        std::fs::write(&s.exe, b"new").expect("exe");
        let st = state_at(&d);

        s.rollback(Some(&st)).expect("rollback");

        assert_eq!(std::fs::read(&s.exe).expect("exe"), b"old");
        assert!(!d.join("afcli.bak").exists(), "备份应被 rename 消耗");
        let (after, _) = update::read_state(&s.state_path());
        let after = after.expect("state written");
        assert_eq!(after.phase, UpdatePhase::RolledBack);
        assert_eq!(after.rollback_count, 1);
        assert!(after.backup_path.is_empty());
        assert!(!after.backup_usable_for(), "回滚过一次就不许再回滚（D2-⑤）");
    }

    /// 没有备份可回滚 → 报错，不能把不存在的文件 rename 上去。
    #[test]
    fn rollback_without_a_backup_reports_it() {
        let (d, s) = sup("rollback-missing");
        let st = state_at(&d);
        let err = s.rollback(Some(&st)).expect_err("must fail");
        assert!(err.contains("no backup"), "{err}");
    }

    /// 世代号从 1 起：0 会让 `backup_usable_for()` 为假，刚做好的备份
    /// 立刻被判「不可用于回滚」。
    #[test]
    fn a_blank_state_is_immediately_usable_for_rollback() {
        let (_d, s) = sup("blank");
        let st = s.blank_state("2.0.0");
        assert_eq!(st.generation, 1);
        assert!(st.backup_usable_for());
        assert_eq!(st.version, "2.0.0");
        assert_eq!(st.staged_path, s.staged_path(None).display().to_string());
        assert_eq!(st.backup_path, s.backup_path(None).display().to_string());
    }

    /// `failed_state` 保留既有记录、只改阶段；没有既有记录时给一条兜底的。
    #[test]
    fn a_failed_state_keeps_the_old_record_when_there_is_one() {
        let (d, s) = sup("failed");
        let st = state_at(&d);
        let f = s.failed_state(Some(&st), "");
        assert_eq!(f.phase, UpdatePhase::Failed);
        assert_eq!(f.version, "9.9.9", "既有版本号必须留着");
        let _ = d;
    }

    #[test]
    fn a_failed_state_without_a_prior_record_still_writes_something() {
        let (_d, s) = sup("failed-none");
        let f = s.failed_state(None, "3.0.0");
        assert_eq!(f.phase, UpdatePhase::Failed);
        assert_eq!(f.version, "3.0.0");
    }

    /// 状态文件里记的路径优先；没记时用 `<exe>.new` / `<exe>.bak` 约定名。
    #[test]
    fn paths_prefer_the_state_file_and_fall_back_to_conventions() {
        let (d, s) = sup("paths");
        let st = state_at(&d);
        assert_eq!(s.staged_path(Some(&st)), d.join("afcli.new"));
        assert_eq!(s.backup_path(Some(&st)), d.join("afcli.bak"));

        // 空串视作没记：按非空判会驱动一次指向空路径的操作。
        let mut blank = state_at(&d);
        blank.staged_path = String::new();
        blank.backup_path = String::new();
        assert_eq!(s.staged_path(Some(&blank)), s.exe.with_extension("new"));
        assert_eq!(s.backup_path(Some(&blank)), update::backup_path_for(&s.exe));
        assert_eq!(s.staged_path(None), s.exe.with_extension("new"));
    }

    /// 状态文件读不出来不致命，但必须留痕：静默降级成「没有升级在进行」
    /// 会让更新流程做出错误判断。
    #[test]
    fn a_corrupt_state_file_is_logged_and_treated_as_absent() {
        let (d, s) = sup("corrupt");
        std::fs::write(s.state_path(), b"{ not json").expect("corrupt state");
        let (state, reconciled) = s.read_state();
        assert!(state.is_none(), "坏状态文件不能当成有效状态");
        assert_eq!(reconciled, update::Reconcile::Nothing);
        // 留痕落在 data_dir 的日志里。
        let log = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(
            log.contains("update state unreadable"),
            "坏状态文件必须留痕: {log}"
        );
    }

    /// `persist_state` 写失败不能让 supervisor 崩：账本是账本，设备该跑还是要跑。
    #[test]
    fn a_failed_state_write_is_survived_and_logged() {
        let (d, s) = sup("persist-fail");
        // 用一个普通文件当 data_dir，让日志与状态都写不进去。
        as_file(&d);
        s.persist_state(&s.blank_state("1.0.0"));
        // 不 panic 即通过；副作用是 stderr 上那行「无法写入日志」。
    }

    /// 日志轮转失败只记一行：轮转不成功不该拦住启动。
    #[test]
    fn a_failed_rotation_does_not_block_startup() {
        let (d, s) = sup("rotate-fail");
        as_file(&d);
        s.rotate_log();
    }

    /// `say` 走的是文件日志：supervisor 与 worker 两边混写同一份文件，
    /// 角色名必须能让人一眼分清这行是谁写的。
    #[test]
    fn say_writes_a_line_tagged_with_the_role() {
        let (d, s) = sup("say");
        s.say("hello from the supervisor");
        let log = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(log.contains("[supervisor]"), "{log}");
        assert!(log.contains("hello from the supervisor"), "{log}");
        let _ = d;
    }

    /// 停下来的 supervisor 要有一个可辨识的退出码（不是被 kill 的那种）。
    #[test]
    fn the_stopped_exit_code_is_distinct_from_the_worker_protocol() {
        assert_eq!(stopped(), std::process::ExitCode::from(SUPERVISOR_STOPPED));
        assert_eq!(SUPERVISOR_STOPPED, 20);
        for c in [
            exit_code::NORMAL,
            exit_code::UPDATE_REQUEST,
            exit_code::HEALTH_FAILURE,
            exit_code::CRASH,
        ] {
            assert_ne!(c, SUPERVISOR_STOPPED as i32, "与 worker 退出码协议撞了");
        }
    }

    // ---- supervise：只走「立即停下」这条确定会终止的路 ----

    /// 对账判为 Inconsistent（staged 没了、备份还在）→ `decide` 直接 Stop，
    /// supervise 于是走「停下来等人介入」并返回退出码 20。
    ///
    /// 这是唯一一条**不需要 spawn 真二进制**就能走完 supervise 全程的路径：
    /// 替换/回滚成功之后循环会继续 spawn，测试就得真的删掉正在跑的二进制。
    #[test]
    fn an_inconsistent_update_state_stops_the_supervisor() {
        let (d, s) = sup("inconsistent");
        // 状态在途 + staged 不存在 + 备份存在 → Inconsistent。
        let mut st = state_at(&d);
        st.phase = UpdatePhase::StartingNew;
        std::fs::write(d.join("afcli.bak"), b"old").expect("bak");
        s.persist_state(&st);

        let console = ConsoleEcho::new(Vec::<u8>::new());
        let err = ConsoleEcho::new(Vec::<u8>::new());
        // 换掉 `sup()` 给的 sink 句柄，把这块控制台接进来。
        let s = Supervisor::with_exe_and_console(
            s.argv.clone(),
            d.clone(),
            s.exe.clone(),
            console.clone(),
            err,
        );
        let code = s.supervise();
        assert_eq!(code, std::process::ExitCode::from(SUPERVISOR_STOPPED));

        // 停下来这件事**必须同时进 stdout**：只看终端的人见到的就是进程
        // 平静地退出（码 20），日志文件是第二落点而不是第一落点。
        let stdout = String::from_utf8(console.lock().clone()).expect("utf8");
        assert!(stdout.contains("human intervention required"), "{stdout}");
        assert!(stdout.contains("unreachable"), "{stdout}");
        // `say` 的每一行也在这块 stdout 上——「前台不打日志」正是要修的事。
        assert!(stdout.contains("[supervisor] info"), "{stdout}");
        assert!(stdout.contains("stopping, human intervention"), "{stdout}");

        let log = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(log.contains("human intervention required"), "{log}");
        assert!(log.contains("unreachable"), "{log}");
        let (after, _) = update::read_state(&s.state_path());
        assert_eq!(after.expect("state").phase, UpdatePhase::Failed);
    }

    /// 把假的「被管理二进制」换成一个真的可执行脚本。
    ///
    /// `sup()` 给的是 `#!/bin/true` 但没有可执行位（spawn 必失败），
    /// 要驱动 `supervise` 走完一轮循环就得让它真的跑起来再退出。
    #[cfg(unix)]
    fn install_script(exe: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(exe, body).expect("write script");
        std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// `Decision::Spawn` 不是「立刻拉起」：它先睡够退避再 spawn，然后把观测
    /// 交给**下一轮**循环的 `pending` 分支（`record_exit` + 清空对账）。
    ///
    /// 一条用例同时钉住这两件事：第一轮的 `spawn` 臂、以及第二轮
    /// `pending.take()` 拿到 `Some` 之后的记账。第二轮的退出码是 `exit(11)`
    /// （这一版不健康）且没有任何备份可退，于是 `decide` 判 `Stop`——
    /// `supervise` 自己停下来，不需要 kill，也不会挂住。
    #[test]
    #[cfg(unix)] // 用 `#!/bin/sh` 假 worker 驱动真循环
    fn a_spawn_decision_sleeps_then_observes_the_worker_exit() {
        let (d, s) = sup("spawn-loop");
        install_script(&s.exe, "#!/bin/sh\nexit 11\n");

        let console = ConsoleEcho::new(Vec::<u8>::new());
        let s = Supervisor::with_exe_and_console(
            s.argv.clone(),
            d.clone(),
            s.exe.clone(),
            console.clone(),
            ConsoleEcho::new(std::io::sink()),
        );
        let code = s.supervise();
        assert_eq!(
            code,
            std::process::ExitCode::from(SUPERVISOR_STOPPED),
            "exit(11) 且没有旧版可退时必须停下来等人"
        );

        // 停下来这句话必须进 stdout：终端上看到的就是它，日志是第二落点。
        let stdout = String::from_utf8(console.lock().clone()).expect("utf8");
        assert!(stdout.contains("human intervention required"), "{stdout}");

        let logged = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(
            logged.contains("backing off 1s"),
            "首轮 Spawn 必须说出退避多久，否则运维以为它立刻重启了：{logged}"
        );
        assert!(
            logged.contains("worker exited with code 11"),
            "第二轮的 Stop 理由要带上观测到的退出码：{logged}"
        );
    }

    /// 替换失败即**放弃本次更新**（D2-④）：不许带着半换好的二进制继续 spawn。
    ///
    /// 造法：状态在途 + staged 在盘上 → `decide` 返回 `ReplaceAndSpawn`；
    /// 但把备份路径指到一个普通文件下面，`replace` 的 `fs::copy` 必失败。
    /// 断言的是「停下 + 账本记 `Failed` + 没有 spawn 过 worker」三件事——
    /// 只断言退出码的话，一个「失败后照样 spawn」的实现也能骗过去。
    #[test]
    fn a_failed_replacement_stops_instead_of_spawning_a_worker() {
        let (d, s) = sup("replace-loop");
        let mut st = state_at(&d);
        std::fs::write(d.join("afcli.new"), b"new").expect("staged");
        // 备份写不进去：用一个普通文件当父目录。
        st.backup_path = d.join("not-a-dir").join("afcli.bak").display().to_string();
        std::fs::write(d.join("not-a-dir"), b"blocker").expect("blocker");
        s.persist_state(&st);

        let code = s.supervise();
        assert_eq!(code, std::process::ExitCode::from(SUPERVISOR_STOPPED));

        let logged = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(
            logged.contains("target version 9.9.9"),
            "替换前要说清换到哪一版：{logged}"
        );
        assert!(
            logged.contains("replacement to 9.9.9 failed"),
            "替换失败必须留痕：{logged}"
        );
        assert!(
            logged.contains("backup to"),
            "要指出是备份那一步失败的：{logged}"
        );
        assert!(
            !logged.contains("backing off"),
            "替换失败后不许继续 spawn worker：{logged}"
        );

        let (after, _) = update::read_state(&s.state_path());
        assert_eq!(
            after.expect("state").phase,
            UpdatePhase::Failed,
            "放弃的更新必须在账本上留下 Failed"
        );
    }

    /// 回滚本身失败 → 停下等人（**不能**假装回滚成功继续跑）。
    ///
    /// 造法：`Committed` 阶段 + 备份文件真的在盘上（所以「有 old 可退」成立），
    /// 但把 `exe` 换成一个**目录**：`spawn` 必失败（记为崩溃），
    /// 而 `rename(bak → 目录)` 也必失败——于是第二轮 `decide` 判 `Rollback`、
    /// `rollback` 返回 `Err`，走到「回滚失败」那条臂。
    #[test]
    fn a_failed_rollback_stops_the_supervisor() {
        let (d, s) = sup("rollback-loop");
        let mut st = state_at(&d);
        st.phase = UpdatePhase::Committed;
        std::fs::write(d.join("afcli.bak"), b"old").expect("bak");
        s.persist_state(&st);
        // exe 变成目录：spawn 失败、rename 到它上面也失败。
        std::fs::remove_file(&s.exe).expect("drop the fake binary");
        std::fs::create_dir(&s.exe).expect("exe as a directory");

        let code = s.supervise();
        assert_eq!(code, std::process::ExitCode::from(SUPERVISOR_STOPPED));

        let logged = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(
            logged.contains("cannot spawn worker"),
            "拉不起来要留痕，否则看不出是崩溃还是回收失败：{logged}"
        );
        assert!(
            logged.contains("rolling back"),
            "有 old 可退时必须先尝试回滚：{logged}"
        );
        assert!(
            logged.contains("rollback failed"),
            "回滚失败必须留痕，否则设备会静默停在坏版本上：{logged}"
        );
    }

    /// 两条**成功**臂都要走到，却不能在成功后进入无限 spawn：
    /// `Request + staged 在盘上` 驱动 `ReplaceAndSpawn` 成功；新二进制
    /// `exit 11` 驱动下一轮 `Rollback` 成功；回滚把没有可执行位的旧文件
    /// 换回原位，于是第三次拉起失败，而 `rollback_count == 1` 且备份已被
    /// 消耗，`decide` 只能判 `Stop`。整条链不睡、不 kill、不碰进程级环境。
    #[test]
    #[cfg(unix)] // 用 `#!/bin/sh` 假 worker 驱动真循环
    fn successful_replace_then_rollback_reaches_the_stop_decision() {
        let (d, s) = sup("replace-rollback-success");
        let staged = d.join("afcli.new");
        let backup = d.join("afcli.bak");
        install_script(&staged, "#!/bin/sh\nexit 11\n");
        s.persist_state(&state_at(&d));

        let code = s.supervise();
        assert_eq!(code, std::process::ExitCode::from(SUPERVISOR_STOPPED));

        let logged = std::fs::read_to_string(log::log_path(&d)).expect("log");
        assert!(
            logged.contains("replaced ") && logged.contains("backup kept at"),
            "替换成功必须留痕，实际日志：{logged}"
        );
        assert!(
            logged.contains("rolled back to the previous binary"),
            "回滚成功必须留痕，实际日志：{logged}"
        );
        assert!(
            logged.contains("human intervention required"),
            "回滚后仍拉不起来必须停下等人，实际日志：{logged}"
        );
        assert!(
            logged.contains("cannot spawn worker"),
            "第三轮观测应来自回滚后的旧二进制不可执行：{logged}"
        );
        assert!(
            !staged.exists() && !backup.exists(),
            "staged 与备份分别应被替换、回滚消耗掉"
        );

        let (after, _) = update::read_state(&s.state_path());
        let after = after.expect("state");
        assert_eq!(after.phase, UpdatePhase::Failed, "最终必须停在 Failed");
        assert_eq!(
            after.rollback_count, 1,
            "成功回滚一次后不许再回滚：{after:?}"
        );
    }

    /// `wait_code` 的 `Err` 臂：`Child::wait` 报错时不能把 supervisor 带崩，
    /// 而是按**崩溃**记账并留一行可搜的日志。
    ///
    /// 文档里说这条「实际上不可达」，理由是 `Child::wait` 二次调用返回同一个
    /// `Ok`、被信号杀死也返回 `Ok`。那两条都对，但漏了第三种：**SIGCHLD 被
    /// 设成 `SIG_IGN`** 时内核会自动回收子进程，`wait` 拿不到可回收的 pid，
    /// 于是返回 `ECHILD`。这正是生产里「父进程继承了别人的 SIGCHLD 处置」的
    /// 形态，所以它不是纯理论分支。
    ///
    /// 改信号处置是**进程级**的，本用例在子进程里做：父进程只 spawn 一次
    /// 测试二进制并断言它退出 0，自己那条 `SIGCHLD` 处置一点不动。
    #[test]
    #[cfg(unix)] // 用 SIGCHLD=SIG_IGN 让内核抢走子进程的回收
    fn a_wait_failure_counts_as_a_crash_and_says_so() {
        let out = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "supervisor::tests::a_wait_failure_counts_as_a_crash_child",
            ])
            .env("AF_WAIT_ERR", "1")
            .output()
            .expect("spawn the child test process");
        // 先解码再断言：`assert!` 的消息只在失败时求值，把两段解码放在
        // 消息里会让它们在一条通过的测试里永远覆盖不到。
        let child_stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let child_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "子进程必须绿，stdout：{child_stdout}，stderr：{child_stderr}"
        );
        // `--exact` 拼错名字时 libtest 会「跑了 0 条」并以 0 退出，
        // 只看退出码会把一个什么都没测的子进程当成绿。
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("1 passed"),
            "子进程必须真的跑完那条用例，实际 stdout：{stdout}"
        );
    }

    /// 上一条的子进程本体。没有 `AF_WAIT_ERR` 时什么都不做（no-op，不是跳过）。
    #[test]
    #[cfg(unix)]
    fn a_wait_failure_counts_as_a_crash_child() {
        if std::env::var_os("AF_WAIT_ERR").is_none() {
            return;
        }
        let (d, mut s) = sup("wait-err");
        install_script(&s.exe, "#!/bin/sh\nexit 0\n");
        s.argv = vec![];
        // SAFETY: `signal` 只是把本进程对 SIGCHLD 的处置换成「忽略」，
        // 不涉及任何内存或别名；本进程是这条用例专用的子进程，改完就退出。
        let previous = unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
        assert_ne!(
            previous,
            libc::SIG_ERR,
            "装不上 SIGCHLD 处置，这条用例的前置条件就不成立"
        );

        let got = s.spawn_and_wait();
        assert_eq!(got.code, exit_code::CRASH, "等不到退出码必须按崩溃处理");
        let logged = std::fs::read_to_string(log::log_path(&d)).unwrap_or_default();
        assert!(
            logged.contains("cannot wait for worker"),
            "必须留下可搜的痕迹，实际日志：{logged:?}"
        );
    }
}
