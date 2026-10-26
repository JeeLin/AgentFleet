//! agent 运行时探测（子任务 3，要求 1「cli 能上报 agent 运行时」）。
//!
//! 探测做两件事：可执行文件找不找得到，以及它的 `--version` 能不能在超时内
//! 答出来。**探测不到就是「没装」，不编造**——Hub 侧按上报的能力做硬过滤
//! （约束 9），一份撒谎的能力表会把会话派给一台根本没装那个 agent 的机器。
//!
//! # 为什么「从哪里找」是参数
//!
//! [`detect_all`] 收 `lookup: &dyn Fn(&str) -> Option<PathBuf>`，而不是自己去读
//! `std::env::var("PATH")`。原因有两条，都不是风格问题：
//!
//! 1. **单测不能改 `PATH`**：`std::env::set_var` 在测试进程里是全局的，而
//!    cargo 默认并发跑测试——一个用例改 `PATH`，另一个用例同时读，得到的是
//!    两者的混合。这类失败还是间歇性的。
//! 2. 把「从哪找」抽出来之后，「装了」与「没装」两种情形都能用**真的可执行
//!    文件**测出来，不必为了测试去 `PATH` 里塞假货。
//!
//! # 超时是硬要求
//!
//! M1 的教训：`exec` 在 `open()` 里同步跑命令，堵住了 worker 的 `select!`
//! 心跳臂，症状是「心跳无故迟到」。所以这里每一个子进程都有超时，
//! 超时就杀进程并记成「已装但探测失败」——**不让任何 agent 二进制把心跳拖死**。
//!
//! 另外，探测**本身也不在心跳路径上跑**：`heartbeat.rs` 只消费探测结果
//! （一份缓存），不在这里调 [`detect_all`]。见 [`super::runtime::Detected`]。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 一次 `--version` 探测的默认超时。
///
/// 2 秒是量出来的：本机 opencode v2.1.4 的 `--version` 立即返回（十几毫秒）。
/// 留两个数量级的余量给慢机器 / 冷缓存，同时保证即使某个 agent 挂死，
/// 一次全量探测也不会拖过心跳超时（`OFLINE_AFTER` 是 45 秒）。
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 一个已知的 agent 种类：显示名 + 可执行文件名 + 怎么问版本。
///
/// 用常量表而不是配置文件：**这是探测的输入，不是用户配置**。用户能改的是
/// 一个 agent **怎么被启动**（`agent_config` 的命令/参数/环境变量），
/// 不是「怎么问 opencode 版本」——那是这个 agent 自己的 CLI 契约。
#[derive(Debug, Clone, Copy)]
pub struct AgentSpec {
    /// 稳定标识，Hub 侧 `agent_runtime.agent_id`。**不随显示名变**。
    pub id: &'static str,
    /// 显示名。
    pub name: &'static str,
    /// 可执行文件名（会在 `PATH` 里找这个）。
    pub bin: &'static str,
    /// 问版本的参数。
    pub version_args: &'static [&'static str],
    /// 让这个 agent **进入 ACP 模式**的参数。
    ///
    /// # 为什么必须有这一项
    ///
    /// 实测：`opencode` 不带子命令直接跑是一个普通 CLI，它读 argv 而不是读
    /// stdin 上的 JSON-RPC。于是 `initialize` 发出去**永远等不到响应**，
    /// 症状是「点开对话就卡住」——而不是任何一条能读的错误。带 `acp` 才是
    /// ACP 服务。
    ///
    /// 它**不由 Hub 配置下发**（`agent_config.args` 是用户追加的参数）：
    /// 「怎么进 ACP 模式」是这个 agent 自己的 CLI 契约，与「怎么问版本」同一
    /// 性质。让用户去配它，等于要求用户读我们没写的文档。
    pub acp_args: &'static [&'static str],
}

/// 内置的 agent 清单。
///
/// 只列**本里程碑真的验证过**的：opencode v2.1.4（ACP 地基实测过）。
/// 不预置 claude / codex 等未验证的条目——「上报了但跑不起来」比「没上报」危险。
pub const KNOWN_AGENTS: &[AgentSpec] = &[AgentSpec {
    id: "opencode",
    name: "opencode",
    bin: "opencode",
    version_args: &["--version"],
    acp_args: &["acp"],
}];

/// 探测一个 agent 的结果。
///
/// **三态不是两态**（设计核对 H-7）：`available` 是布尔时无法区分「没装」与
/// 「装了但探测失败」，而子任务 5 的 409 门禁要的是「已装**且**可用」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// 可执行文件不存在。
    NotInstalled,
    /// 找到了可执行文件，`--version` 也答出来了。
    Ready {
        version: String,
        /// 探测到的可执行文件路径。落库不是为了展示，是为了**下次能直接执行它**
        /// （`PATH` 里可能有两份，探测到的那份才算数）。
        path: PathBuf,
    },
    /// 找到了可执行文件，但跑不起来 / 超时 / 退出码非 0。
    ///
    /// `reason` 是给人看的一句话，会存进 `agent_runtime.probe_error`。
    /// 没有它，页面上只有一个 `available=false`，用户无从下手。
    Failed { reason: String, path: PathBuf },
}

impl Probe {
    /// 能不能现在拿它对话。门禁判据就是这一条。
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// 探测到的可执行文件路径（没装时为 `None`）。
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::NotInstalled => None,
            Self::Ready { path, .. } | Self::Failed { path, .. } => Some(path),
        }
    }

    /// 版本串；只有 `Ready` 有。
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Ready { version, .. } => Some(version),
            _ => None,
        }
    }
}

/// 探一个 agent。
///
/// `lookup` 是「名字 → 可执行文件路径」，单测注入假的，生产传
/// [`path_lookup`]。这样做是为了让单测**不必改 `PATH`**（见模块注释）。
pub fn detect_one(
    spec: &AgentSpec,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
    timeout: Duration,
) -> Probe {
    let Some(path) = lookup(spec.bin) else {
        return Probe::NotInstalled;
    };
    match run_version(&path, spec.version_args, timeout) {
        Ok(version) => Probe::Ready { version, path },
        Err(reason) => Probe::Failed { reason, path },
    }
}

/// 探全部已知 agent。返回顺序与 [`KNOWN_AGENTS`] 一致（稳定，便于 diff）。
pub fn detect_all(
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
    timeout: Duration,
) -> Vec<(&'static AgentSpec, Probe)> {
    KNOWN_AGENTS
        .iter()
        .map(|spec| (spec, detect_one(spec, lookup, timeout)))
        .collect()
}

/// 生产用的「名字 → 路径」：在 `PATH` 里找。
///
/// 自己走 `PATH` 而不是 `Command::new(name)`：后者找不到时返回的是
/// `io::ErrorKind::NotFound`，与环境相关的其它错误（权限、EACCES）混在一起，
/// 而「没装」和「装了但没执行权限」在页面上要显示成不同的话。
pub fn path_lookup(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| is_executable(candidate))
}

/// 文件存在且可执行。
///
/// Unix 上判 mode 位；其它平台退化成「存在即可」（本仓只在 Unix 上跑，
/// 但留这个分支避免 `cfg` 之外的路径悄悄恒假）。
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// 跑 `<path> <args>` 取第一行输出（去空白）。
///
/// **必须有超时**：用一个线程等进程，主线程带超时地等它。不用
/// `tokio::process`，因为探测发生在 worker 的同步路径上（心跳 tick 臂里只
/// 消费缓存，但缓存是在别处刷新的）；引入 async 会让这条路径要求一个
/// runtime 句柄，把探测绑死在 tokio 上。
///
/// 超时后 `kill()` 子进程并返回 `Err`——**杀掉而不是放着**，否则一个挂死的
/// agent 会一直占着一个进程槽，每轮探测再起一个。
/// 跑 `<path> <args>` 取第一行输出（去空白）。
fn run_version(path: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法执行：{e}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(format!("退出码 {}", status.code().unwrap_or(-1)));
                }
                // 进程已退出，读 stdout 不会再阻塞。
                let out = child
                    .wait_with_output()
                    .map_err(|e| format!("读取输出失败：{e}"))?;
                let text = String::from_utf8_lossy(&out.stdout);
                let first = text.lines().next().unwrap_or("").trim();
                if first.is_empty() {
                    return Err("版本输出为空".to_string());
                }
                return Ok(first.to_string());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // 杀掉再回收，避免僵尸进程
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("探测超时（{}ms）", timeout.as_millis()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("等待进程失败：{e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// **`id` 与 `name` 现在恰好相同，但它们是两个字段，契约必须钉住。**
    ///
    /// 会话门禁按 `agent_id` 判（`agent_runtime.agent_id`），页面下拉按 `name`
    /// 显示。两者一旦分叉，「页面上选得到、点创建 409」——而这是个只有
    /// 真正点下去才会发现的 bug。这条测试把它变成 `cargo test` 当场红。
    ///
    /// 如果将来确实需要让它们不同，先改 `device_summary` / `usable_agents`
    /// 的键（那里也用 `name`），再删这条测试。
    #[test]
    fn every_known_agent_has_an_id_equal_to_its_display_name_for_now() {
        for spec in KNOWN_AGENTS {
            assert_eq!(
                spec.id, spec.name,
                "`{}` 的 id 与 name 不同了：门禁按 id、下拉按 name，两处会分叉",
                spec.id
            );
        }
    }

    /// `id` 必须唯一：它是 `agent_runtime` 主键的一半。两个 spec 撞 id
    /// 会让一台设备上的两个 agent 互相覆盖。
    #[test]
    fn known_agent_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in KNOWN_AGENTS {
            assert!(seen.insert(spec.id), "重复的 agent id: {}", spec.id);
        }
    }

    /// 一个「能跑」的可执行文件：`#!/bin/sh\necho 9.9.9`。
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::io::Write;
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).expect("建可执行文件");
        f.write_all(body.as_bytes()).expect("写");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    fn lookup_of(map: Vec<(&'static str, PathBuf)>) -> impl Fn(&str) -> Option<PathBuf> {
        let m: HashMap<String, PathBuf> = map
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        move |name: &str| m.get(name).cloned()
    }

    const SPEC: AgentSpec = AgentSpec {
        id: "fake",
        name: "fake",
        bin: "fake",
        version_args: &["--version"],
        acp_args: &["acp"],
    };

    #[test]
    fn not_installed_is_its_own_state_not_a_failure() {
        let lookup = lookup_of(vec![]);
        let got = detect_one(&SPEC, &lookup, DEFAULT_PROBE_TIMEOUT);
        assert_eq!(got, Probe::NotInstalled);
        assert!(!got.is_available());
        assert_eq!(got.path(), None, "没装就没有路径");
        assert_eq!(got.version(), None);
    }

    #[test]
    fn an_installed_agent_reports_its_version() {
        let d = tempdir();
        let bin = write_script(&d, "fake", "#!/bin/sh\necho 9.9.9\n");
        let lookup = lookup_of(vec![("fake", bin.clone())]);
        let got = detect_one(&SPEC, &lookup, DEFAULT_PROBE_TIMEOUT);
        assert_eq!(
            got,
            Probe::Ready {
                version: "9.9.9".to_string(),
                path: bin.clone()
            }
        );
        assert!(got.is_available(), "探测成功 = 可对话");
        assert_eq!(got.path(), Some(bin.as_path()));
    }

    #[test]
    fn a_nonzero_exit_is_failed_not_ready_so_the_gate_stays_closed() {
        // 「装了但跑不起来」必须是第三个态：当成 ready 会让 409 门禁放行，
        // 用户点开对话才失败。
        let d = tempdir();
        let bin = write_script(&d, "fake", "#!/bin/sh\nexit 3\n");
        let lookup = lookup_of(vec![("fake", bin)]);
        let got = detect_one(&SPEC, &lookup, DEFAULT_PROBE_TIMEOUT);
        assert!(matches!(got, Probe::Failed { .. }), "got {got:?}");
        assert!(!got.is_available(), "跑不起来就不能算可用");
        assert!(got.version().is_none());
        // 路径仍然要报出来：用户要看的是「我装的那个坏了」
        assert!(got.path().is_some());
    }

    #[test]
    fn empty_version_output_is_failed() {
        let d = tempdir();
        let bin = write_script(&d, "fake", "#!/bin/sh\ntrue\n");
        let lookup = lookup_of(vec![("fake", bin)]);
        let got = detect_one(&SPEC, &lookup, DEFAULT_PROBE_TIMEOUT);
        assert!(matches!(got, Probe::Failed { .. }), "got {got:?}");
    }

    /// **探测必须在超时内返回**（设计核对：原「不影响心跳节奏」原样不可测）。
    ///
    /// 用 `sleep 3600` 的假二进制：如果超时没生效，这条测试会挂住整个测试
    /// 进程（而不是失败），所以断言里同时给了余量上限——真挂住时至少能靠
    /// 测试超时看出问题。
    #[test]
    fn a_hanging_agent_binary_cannot_block_probing_past_the_timeout() {
        let d = tempdir();
        let bin = write_script(&d, "fake", "#!/bin/sh\nsleep 3600\n");
        let lookup = lookup_of(vec![("fake", bin)]);
        let timeout = Duration::from_millis(300);
        let start = Instant::now();
        let got = detect_one(&SPEC, &lookup, timeout);
        let elapsed = start.elapsed();
        assert!(
            matches!(got, Probe::Failed { .. }),
            "超时必须记成 Failed，got {got:?}"
        );
        assert!(
            elapsed < timeout + Duration::from_secs(2),
            "探测必须在超时附近返回，实际 {elapsed:?}"
        );
    }

    #[test]
    fn detect_all_keeps_the_catalog_order_and_covers_every_known_agent() {
        let lookup = lookup_of(vec![]);
        let got = detect_all(&lookup, DEFAULT_PROBE_TIMEOUT);
        assert_eq!(
            got.len(),
            KNOWN_AGENTS.len(),
            "每个已知 agent 都要有一条结果"
        );
        let ids: Vec<&str> = got.iter().map(|(s, _)| s.id).collect();
        let expect: Vec<&str> = KNOWN_AGENTS.iter().map(|s| s.id).collect();
        assert_eq!(ids, expect, "顺序必须与清单一致（便于 diff）");
        assert!(
            got.iter().all(|(_, p)| *p == Probe::NotInstalled),
            "空查找表就是「全都没装」，不得编造"
        );
    }

    #[test]
    fn the_catalog_has_no_duplicate_ids_or_empty_names() {
        let mut seen = std::collections::HashSet::new();
        for s in KNOWN_AGENTS {
            assert!(seen.insert(s.id), "agent id 重复：{}", s.id);
            assert!(!s.name.is_empty(), "显示名不能为空");
            assert!(!s.bin.is_empty(), "可执行名不能为空");
            assert!(!s.version_args.is_empty(), "必须知道怎么问版本");
        }
    }

    #[test]
    fn path_lookup_finds_a_real_binary_and_misses_a_bogus_one() {
        // 走真实 PATH：`sh` 一定在，随机串一定不在。
        // 这条不注入假查找表，为的是证明 `path_lookup` 自己是对的。
        assert!(path_lookup("sh").is_some(), "sh 应该在 PATH 里");
        assert!(
            path_lookup("agentfleet-definitely-not-installed-xyz").is_none(),
            "不存在的名字必须返回 None"
        );
    }

    /// 对**真实安装的 opencode** 走一遍完整探测（真 PATH + 真子进程）。
    ///
    /// 上面几条都用注入的查找表与假脚本，证明的是逻辑；这条证明的是
    /// **对真货也成立**——`--version` 的实际输出形态（`opencode v2.1.4`）
    /// 与假脚本的 `9.9.9` 不同，扫描 / 解析上若有假设，只有这条会红。
    ///
    /// 装与没装都算通过（CI 上通常没装），所以**两种情形各断言一次**，
    /// 而不是 `if installed { ... }` 之后什么都不做——那样没装时就等于没测。
    #[test]
    fn a_real_opencode_install_is_detected_end_to_end() {
        let lookup = path_lookup;
        let probe = detect_one(&KNOWN_AGENTS[0], &lookup, DEFAULT_PROBE_TIMEOUT);
        match probe {
            Probe::NotInstalled => {
                // 没装：必须是 NotInstalled，不能是 Failed（那会把「没装」
                // 显示成「装坏了」）
                assert!(!probe.is_available());
                assert_eq!(probe.path(), None);
            }
            Probe::Ready { ref version, .. } => {
                assert!(!version.is_empty(), "版本串不能为空：{version:?}");
                // 真实输出是 `opencode v2.1.4` 这种一行文本，不是裸版本号
                assert!(
                    version.chars().any(|c| c.is_ascii_digit()),
                    "版本里至少要有数字：{version:?}"
                );
                assert!(probe.is_available());
            }
            Probe::Failed { ref reason, .. } => {
                panic!("本机装了 opencode 却探测失败，这是真问题：{reason}");
            }
        }
    }

    /// 测试用临时目录。不引 `tempfile` 依赖（node-core 目前没有它），
    /// 用进程 id + 计数器保证并发测试不撞。
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("af-detect-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("建临时目录");
        p
    }
}
