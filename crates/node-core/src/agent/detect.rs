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
//! （一份缓存），不在这里调 [`detect_all`]。见 [`crate::agent::cache::Detected`]。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 一次 `--version` 探测的默认超时。
///
/// 2 秒是量出来的：本机 opencode v2.1.4 的 `--version` 立即返回（十几毫秒）。
/// 留两个数量级的余量给慢机器 / 冷缓存，同时保证即使某个 agent 挂死，
/// 一次全量探测也不会拖过心跳超时（`OFFLINE_AFTER` 是 45 秒）。
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
    ///
    /// # 空列表 = 这个 agent **不能对话**
    ///
    /// 不是「还没适配」，而是一条**事实**：它压根没有 ACP 入口，我们也没验证
    /// 过跟它对话。实测 `claude` v2.1.310 没有 `acp` 子命令
    /// （`claude acp` → `unknown command "acp"`），所以它的这一项是空的。
    ///
    /// 这类 agent **仍然出现在上报里**（[`Probe::NotCapable`]）：用户装了它，
    /// 页面上却完全不提，用户无从知道「为什么我的 agent 不在列表里」——那比
    /// 一条写着「这个 agent 不支持对话」的灰字糟糕得多。
    pub acp_args: &'static [&'static str],
}

impl AgentSpec {
    /// 能不能拿它开对话。判据就是清单里有没有 ACP 入口参数。
    pub fn can_converse(&self) -> bool {
        !self.acp_args.is_empty()
    }
}

/// 内置的 agent 清单（「扫描 PATH 自动发现」认的名字表）。
///
/// # 两种条目，一条判据
///
/// `acp_args` 非空 = **真的验证过能对话**（`opencode` 是本里程碑唯一跑通整条
/// 链的：开通道 → 起子进程 → `session/new` → `session/prompt` → 事件流 →
/// `turn_end`，真机 6.45 s）。
///
/// `acp_args` 为空 = 只被发现、**不能对话**。这类条目存在的理由是「让用户看见
/// 事实」：用户装了 `claude`，页面上既不该假装它能用（点下去永远转圈），也不该
/// 什么都不说（用户以为系统没扫到）。于是它出现在列表里，带一句能照做的原因。
///
/// # 加新条目之前先做三件事
///
/// 1. 查出它的 **ACP 入口参数**（`opencode acp` 之于 opencode）。查不出来就
///    别填 `acp_args`——留空是对的，编一个会让「点开对话」变成一次静默挂起。
/// 2. 用真机跑一遍，确认 `initialize` / `session/new` / `session/prompt` 的
///    形态与本仓 `acp.rs` 的假设一致。
/// 3. 配一条 `#[ignore]` 真机测试（模板见 `conversation.rs` 的
///    `a_real_opencode_turn_reaches_the_channel_as_a_batch`）。
///
/// 没做第 2、3 步就填 `acp_args` 的后果**不可见**：单测全绿、页面也正常，只是
/// 用户点开对话后什么都不发生。
///
/// # 为什么名单写死而不是「扫 PATH 里每个可执行文件」
///
/// 全扫的代价是给每个可执行文件起一次 `--version`（本机 PATH 里上百个），而且
/// 会误报——`cargo --version` 也答应得很好。名单里的名字是**人装的 agent 名**，
/// 与「机器上有哪些命令行工具」不是同一个集合。
pub const KNOWN_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "opencode",
        name: "opencode",
        bin: "opencode",
        version_args: &["--version"],
        // 本里程碑真机验证过。不带的后果见 `acp_args` 的文档。
        acp_args: &["acp"],
    },
    // ---- 以下只被发现，不能对话（`acp_args` 为空）----
    //
    // 实测 `claude` v2.1.310：`claude acp` → `unknown command "acp"`。它没有
    // ACP 入口，本仓也不是通过 ACP 跟它说话的，所以**不能**对话。列出来是为了
    // 让用户看到「我装的 claude 被发现了，但它不支持对话」。
    AgentSpec {
        id: "claude",
        name: "claude",
        bin: "claude",
        version_args: &["--version"],
        acp_args: &[],
    },
    // 以下几条**没有在本机验证过 ACP 入口**，因此一律留空：宁可显示成「不支持
    // 对话」，也不要让用户点开一个永远不回话的会话。它们的 `version_args` 是
    // 各自的 CLI 惯例（`--version`），探测失败会显示成「已装但跑不起来」。
    AgentSpec {
        id: "gemini",
        name: "gemini",
        bin: "gemini",
        version_args: &["--version"],
        acp_args: &[],
    },
    AgentSpec {
        id: "codex",
        name: "codex",
        bin: "codex",
        version_args: &["--version"],
        acp_args: &[],
    },
    AgentSpec {
        id: "cursor-agent",
        name: "cursor-agent",
        bin: "cursor-agent",
        version_args: &["--version"],
        acp_args: &[],
    },
    AgentSpec {
        id: "aider",
        name: "aider",
        bin: "aider",
        version_args: &["--version"],
        acp_args: &[],
    },
    AgentSpec {
        id: "crush",
        name: "crush",
        bin: "crush",
        version_args: &["--version"],
        acp_args: &[],
    },
    AgentSpec {
        id: "goose",
        name: "goose",
        bin: "goose",
        version_args: &["--version"],
        acp_args: &[],
    },
    AgentSpec {
        id: "qwen",
        name: "qwen",
        bin: "qwen",
        version_args: &["--version"],
        acp_args: &[],
    },
];

/// 探测一个 agent 的结果。
///
/// **四态不是两态**（设计核对 H-7）：`available` 是布尔时无法区分「没装」与
/// 「装了但探测失败」，而子任务 5 的 409 门禁要的是「已装**且**可用」。
///
/// 第四态 [`Probe::NotCapable`] 是「扫描 PATH 自动发现」加进来的：一个装了、
/// 跑得起来、但**没有 ACP 入口**的 agent。它与 `Failed` 的区别是「它没坏，
/// 只是我们不通过 ACP 跟它说话」，两者的 `error` 文案完全不同——前者用户
/// 无从下手，后者用户去看 `--version` 就知道怎么了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// 可执行文件不存在。
    NotInstalled,
    /// 找到了可执行文件、`--version` 也答出来了，但**它没有 ACP 入口**。
    ///
    /// `version` 与 `path` 照常给出：用户要知道「被发现的到底是哪一个」。
    NotCapable { version: String, path: PathBuf },
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
    ///
    /// **只有 `Ready` 为真**。`NotCapable` 是「装了但不能对话」——它一旦为真，
    /// 用户就能建出一个永远没人回话的会话。
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// 探测到的可执行文件路径（没装时为 `None`）。
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::NotInstalled => None,
            Self::NotCapable { path, .. }
            | Self::Ready { path, .. }
            | Self::Failed { path, .. } => Some(path),
        }
    }

    /// 版本串；`Ready` 与 `NotCapable` 都有（这两个都成功跑过 `--version`）。
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::NotCapable { version, .. } | Self::Ready { version, .. } => Some(version),
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
        // **能跑起来还不够，还得有 ACP 入口**。这一步是「扫描 PATH 自动发现」
        // 的核心判据：`claude --version` 答得很好，但 `claude acp` 是
        // `unknown command`，拿它开对话只会永远等不到 `initialize`。
        Ok(version) => {
            if spec.can_converse() {
                Probe::Ready { version, path }
            } else {
                Probe::NotCapable { version, path }
            }
        }
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
        let m: HashMap<String, PathBuf> =
            map.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
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
            Probe::NotCapable { ref version, .. } => {
                // `KNOWN_AGENTS[0]` 是 opencode，它**有** ACP 入口（`acp_args`
                // 非空），所以不该走到这里。真走到说明有人把 `acp_args` 清空了
                // ——那会让唯一验证过能对话的 agent 变成「不支持对话」。
                panic!("opencode 的 acp_args 被清空了？版本 {version}");
            }
        }
    }

    /// **装了、能跑、但没有 ACP 入口 → `NotCapable`，不是 `Ready`。**
    ///
    /// 这是「扫描 PATH 自动发现」的核心判据。反向验证：把 `detect_one` 里的
    /// `if spec.can_converse()` 改成恒真，这条会红——而红的样子正是用户会
    /// 遇到的：页面上 `claude` 显示成可用，点开对话永远转圈。
    #[test]
    fn an_installed_agent_without_an_acp_entry_is_not_capable_not_ready() {
        let d = tempdir();
        let bin = write_script(&d, "claude", "#!/bin/sh\necho '2.1.310 (Claude Code)'\n");
        let lookup = lookup_of(vec![("claude", bin.clone())]);
        let spec = KNOWN_AGENTS
            .iter()
            .find(|s| s.id == "claude")
            .expect("清单里必须有 claude（要求：扫描要发现它）");

        let got = detect_one(spec, &lookup, DEFAULT_PROBE_TIMEOUT);
        assert!(
            matches!(got, Probe::NotCapable { .. }),
            "claude 没有 acp 子命令，必须是 NotCapable，实际 {got:?}"
        );
        assert!(!got.is_available(), "不能对话就不能过门禁");
        assert_eq!(
            got.version(),
            Some("2.1.310 (Claude Code)"),
            "NotCapable 仍要报版本：用户要确认被发现的到底是哪个"
        );
        assert_eq!(got.path(), Some(bin.as_path()), "路径照常报出来");
    }

    /// `NotCapable` 在上报里带**一句能照做的原因**，而不是「不可用」。
    ///
    /// 页面上「不可用」是废话：用户既不能验证也不能照着做。原因里必须点名
    /// 是哪个可执行文件、缺的是什么。
    #[test]
    fn a_not_capable_agent_reports_why_it_cannot_converse() {
        use crate::agent::cache::{probe_once, Snapshot};
        let d = tempdir();
        let bin = write_script(&d, "claude", "#!/bin/sh\necho 1.0.0\n");
        let lookup = lookup_of(vec![("claude", bin)]);
        // `probe_once` 探整份清单，这里只关心 claude 那一条。
        let Snapshot { agents, .. } = probe_once(&lookup, DEFAULT_PROBE_TIMEOUT);
        let claude = agents
            .iter()
            .find(|a| a.id == "claude")
            .expect("claude 必须在清单里");
        assert!(!claude.available);
        let err = claude.error.as_deref().expect("必须给出原因");
        assert!(
            err.contains("claude") && err.contains("ACP"),
            "原因要点名是哪个程序、缺的是什么，实际：{err:?}"
        );
    }

    /// **`can_converse` 的判据是 `acp_args` 非空**，且清单里恰好只有 opencode 为真。
    ///
    /// 这条会红的情形有两种，都是真问题：
    /// - 有人给没验证过的 agent 填了 `acp_args`（用户点开对话会静默挂起）；
    /// - 有人把 opencode 的 `acp_args` 清空了（唯一能用的 agent 变成不能用）。
    #[test]
    fn only_the_verified_agent_claims_it_can_converse() {
        let capable: Vec<&str> = KNOWN_AGENTS
            .iter()
            .filter(|s| s.can_converse())
            .map(|s| s.id)
            .collect();
        assert_eq!(
            capable,
            vec!["opencode"],
            "只有真机验证过的才能声称能对话；加新的先读 `KNOWN_AGENTS` 的文档"
        );
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
