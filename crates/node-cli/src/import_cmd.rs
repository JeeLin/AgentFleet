//! `afcli import`：列出并绑定外部 agent 的历史会话（v0.7.4）。
//!
//! 导入 = **绑定非搬运**：CLI 只从 provider 拿到 `acp_session_id` / `cwd` /
//! `title`，向 Hub 发一次设备签名 HTTP 请求；历史消息仍归 agent 自己管。
//!
//! 三条硬边界：
//! 1. 不调用 `connect_once`、不启动 worker / supervisor——第二条 WS 握手会被
//!    代次机制顶替正在运行的设备连接。
//! 2. 不写 `identity.json`：`Identity::load` 只读；没有 `node_id` 就提示
//!    「请先 enroll」，绝不临时 enroll。
//! 3. 错误只输出稳定 code + 人话：不打印 DB 绝对路径、cookie、私钥或原始
//!    服务器 HTML。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use node_core::identity::{Identity, IDENTITY_FILE};
use node_core::import::{self, ImportError, ProviderLookup};

use crate::import_http::{self, ImportHttpError};
use crate::runargs;

/// 子命令自己的用法。顶层 `USAGE` 只列一行，详细参数在这里。
const USAGE: &str = "\
用法:
  afcli import list --provider <id> [--data-dir DIR]
      列出该 agent 在本机的历史会话
  afcli import bind --provider <id> --session <external-id> [--hub <url|host:port>] [--data-dir DIR]
      把选中的历史会话绑定成 Hub 会话（一次设备签名 HTTP 请求）

--provider 目前只支持 opencode。--hub 缺省取 AFCLI_HUB_URL，再缺省
ws://127.0.0.1:8080/ws；host:port / ws:// / wss:// 会推导成同源 http(s)。
--data-dir 指定平台数据基目录（与 AFCLI_DATA_DIR 同义），缺省用平台标准目录。
导入只读 identity.json，不会 enroll、不会建立 WS 连接、不会启动 worker。";

/// 一条解析后的 `import` 命令。
#[derive(Debug)]
enum Command {
    List(ListArgs),
    Bind(BindArgs),
}

#[derive(Debug)]
struct ListArgs {
    provider: String,
    data_dir: Option<PathBuf>,
}

#[derive(Debug)]
struct BindArgs {
    provider: String,
    session: String,
    hub: Option<String>,
    data_dir: Option<PathBuf>,
}

/// 跑一遍 `afcli import`，返回退出码。
pub fn run<W1: Write, W2: Write>(
    args: &[String],
    data_dir: &Path,
    mut out: W1,
    mut err: W2,
) -> ExitCode {
    match parse(args) {
        Ok(Command::List(args)) => run_list(args, data_dir, &mut out, &mut err),
        Ok(Command::Bind(args)) => run_bind(args, data_dir, &mut out, &mut err),
        Err(message) => {
            drop(writeln!(err, "afcli import: usage: {message}"));
            drop(writeln!(err, "{USAGE}"));
            ExitCode::FAILURE
        }
    }
}

fn parse(rest: &[String]) -> Result<Command, String> {
    let sub = rest
        .first()
        .ok_or_else(|| "缺少 list / bind 子命令".to_string())?;
    let tail = rest.get(1..).unwrap_or(&[]);
    match sub.as_str() {
        "list" => parse_list(tail).map(Command::List),
        "bind" => parse_bind(tail).map(Command::Bind),
        other => Err(format!("未知的 import 子命令 `{other}`")),
    }
}

fn parse_list(args: &[String]) -> Result<ListArgs, String> {
    let mut provider = None;
    let mut data_dir = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--provider" => {
                provider = Some(take_value(args, i, "--provider")?.to_string());
                i += 2;
            }
            "--data-dir" => {
                data_dir = Some(PathBuf::from(take_value(args, i, "--data-dir")?));
                i += 2;
            }
            other => return Err(format!("未知参数 `{other}`")),
        }
    }
    let provider = provider.ok_or_else(|| "缺少 --provider".to_string())?;
    Ok(ListArgs { provider, data_dir })
}

fn parse_bind(args: &[String]) -> Result<BindArgs, String> {
    let mut provider = None;
    let mut session = None;
    let mut hub = None;
    let mut data_dir = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--provider" => {
                provider = Some(take_value(args, i, "--provider")?.to_string());
                i += 2;
            }
            "--session" => {
                session = Some(take_value(args, i, "--session")?.to_string());
                i += 2;
            }
            "--hub" => {
                hub = Some(take_value(args, i, "--hub")?.to_string());
                i += 2;
            }
            "--data-dir" => {
                data_dir = Some(PathBuf::from(take_value(args, i, "--data-dir")?));
                i += 2;
            }
            other => return Err(format!("未知参数 `{other}`")),
        }
    }
    let provider = provider.ok_or_else(|| "缺少 --provider".to_string())?;
    let session =
        session.ok_or_else(|| "缺少 --session（先运行 `afcli import list`）".to_string())?;
    if session.trim().is_empty() {
        return Err("--session 不能为空".to_string());
    }
    Ok(BindArgs {
        provider,
        session,
        hub,
        data_dir,
    })
}

fn take_value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str, String> {
    let value = args.get(i + 1).ok_or_else(|| format!("{flag} 缺少值"))?;
    // 下一个 flag 不是值。真实取值（provider / session / URL / 路径）都不以
    // `--` 开头；把 `--hub --provider x` 当成 hub 值会得到一个无意义地址。
    if value.starts_with("--") {
        return Err(format!("{flag} 缺少值"));
    }
    Ok(value.as_str())
}

fn run_list<W: Write, E: Write>(
    args: ListArgs,
    data_dir: &Path,
    out: &mut W,
    err: &mut E,
) -> ExitCode {
    let provider = match provider_for(&args.provider) {
        Ok(provider) => provider,
        Err(failure) => return report(err, failure),
    };
    let base = resolve_base(args.data_dir.as_deref(), data_dir);
    let sessions = match provider.list(&base) {
        Ok(sessions) => sessions,
        Err(error) => return report(err, map_import_error(error)),
    };
    if sessions.is_empty() {
        drop(writeln!(out, "没有可导入的历史会话"));
        return ExitCode::SUCCESS;
    }
    for (index, session) in sessions.iter().enumerate() {
        let title = if session.title.is_empty() {
            "（无标题）"
        } else {
            session.title.as_str()
        };
        let updated = session.updated_at.as_deref().unwrap_or("-");
        drop(writeln!(
            out,
            "{}. {}\t{}\t{}\t{}",
            index + 1,
            session.external_session_id,
            title,
            session.cwd,
            updated
        ));
    }
    ExitCode::SUCCESS
}

fn run_bind<W: Write, E: Write>(
    args: BindArgs,
    data_dir: &Path,
    out: &mut W,
    err: &mut E,
) -> ExitCode {
    // 先做纯解析：坏地址在任何文件 / 网络访问之前就失败。
    let env_hub = std::env::var(runargs::ENV_HUB_URL).ok();
    let origin = match runargs::http_origin(&hub_source(args.hub.as_deref(), env_hub.as_deref())) {
        Ok(origin) => origin,
        Err(reason) => return report(err, ImportFailure::InvalidHubUrl(reason)),
    };
    let provider = match provider_for(&args.provider) {
        Ok(provider) => provider,
        Err(failure) => return report(err, failure),
    };
    let base = resolve_base(args.data_dir.as_deref(), data_dir);

    // 身份**只读**加载。没有 node_id 就停下——绝不临时 enroll，也绝不发请求。
    let identity = match Identity::load(&identity_file(&base)) {
        Ok(identity) => identity,
        Err(_) => return report(err, ImportFailure::NotEnrolled),
    };
    let node_id = match identity.node_id() {
        Some(id) if !id.trim().is_empty() => id,
        _ => return report(err, ImportFailure::NotEnrolled),
    };

    let session = args.session.trim();
    let candidate = match provider.resolve(&base, session) {
        Ok(candidate) => candidate,
        Err(ImportError::InvalidWorkspacePath) => {
            // `resolve` 只回错误、不回 cwd；要给出「Windows 盘符」这句可读原因，
            // 得回头从同一张表里读那一行的 cwd。只读、失败就退回通用错误。
            let windows = provider
                .list(&base)
                .ok()
                .and_then(|rows| {
                    rows.into_iter()
                        .find(|row| row.external_session_id == session)
                        .map(|row| is_windows_drive_path(&row.cwd))
                })
                .unwrap_or(false);
            return report(err, ImportFailure::InvalidPath { windows });
        }
        Err(error) => return report(err, map_import_error(error)),
    };

    // `resolve` 不带 title（它只服务于绑定所需字段），从同一份列表里取。
    // 取不到就用空串：Hub 侧 title 只是展示字段，不该因此阻断导入。
    let title = provider
        .list(&base)
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|row| row.external_session_id == session)
                .map(|row| row.title)
        })
        .unwrap_or_default();

    let signed = match import_http::build_signed_body(
        identity.key(),
        &node_id,
        candidate.provider,
        &candidate,
        &title,
    ) {
        Ok(signed) => signed,
        Err(error) => return report(err, map_http_error(error)),
    };
    let outcome = match import_http::post(&origin, &signed) {
        Ok(outcome) => outcome,
        Err(error) => return report(err, map_http_error(error)),
    };

    drop(writeln!(
        out,
        "导入成功：session_id = {}",
        outcome.session_id
    ));
    drop(writeln!(out, "打开既有会话继续对话"));
    ExitCode::SUCCESS
}

fn provider_for(id: &str) -> Result<&'static dyn import::HistoryProvider, ImportFailure> {
    match import::lookup(id) {
        ProviderLookup::Supported(provider) => Ok(provider),
        ProviderLookup::Unsupported { agent_id, reason } => Err(
            ImportFailure::UnsupportedProvider(format!("{agent_id}（{reason}）")),
        ),
    }
}

/// `run_with` 注入的是 `<平台数据目录>/agentfleet`；provider 与身份文件
/// 都按平台数据目录定位（`opencode/opencode.db`、`agentfleet/identity.json`）。
fn resolve_base(flag: Option<&Path>, injected: &Path) -> PathBuf {
    match flag {
        Some(dir) => dir.to_path_buf(),
        None => injected
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| injected.to_path_buf()),
    }
}

fn identity_file(base: &Path) -> PathBuf {
    base.join("agentfleet").join(IDENTITY_FILE)
}

/// `--hub` 优先，其次 `AFCLI_HUB_URL`，最后本机缺省 WS 地址。
///
/// 环境变量以参数注入（而不是在函数里读），这样「flag 优先 / 空值跳过」
/// 这两条判定可以在单测里固定住，不必改动进程环境。
fn hub_source(flag: Option<&str>, env: Option<&str>) -> String {
    if let Some(value) = flag {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }
    match env {
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => runargs::DEFAULT_HUB_URL.to_string(),
    }
}

/// 盘符路径（`C:\...` / `C:/...`）：opencode 在 Windows 上记录的是这种 cwd，
/// 而 Hub 工作区只接受 `/` 或 `~` 开头。这里只用于把错误说清楚，不扩路径语义。
fn is_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    match (bytes.first(), bytes.get(1), bytes.get(2)) {
        (Some(first), Some(&b':'), Some(sep)) => {
            first.is_ascii_alphabetic() && (*sep == b'\\' || *sep == b'/')
        }
        _ => false,
    }
}

/// CLI 的稳定失败分类。`code()` 是脚本可判定的部分，`message()` 是给人看的。
enum ImportFailure {
    UnsupportedProvider(String),
    NotEnrolled,
    ProviderUnavailable,
    InvalidSession,
    InvalidPath { windows: bool },
    InvalidHubUrl(String),
    Network(String),
    BadSignature,
    AgentUnavailable,
    AlreadyImported,
    BadResponse,
    Internal(String),
    Server(String),
}

impl ImportFailure {
    fn code(&self) -> &str {
        match self {
            Self::UnsupportedProvider(_) => "unsupported_provider",
            Self::NotEnrolled => "not_enrolled",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::InvalidSession => "invalid_session",
            Self::InvalidPath { .. } => "invalid_path",
            Self::InvalidHubUrl(_) => "invalid_hub_url",
            Self::Network(_) => "network",
            Self::BadSignature => "bad_signature",
            Self::AgentUnavailable => "agent_unavailable",
            Self::AlreadyImported => "already_imported",
            Self::BadResponse => "bad_response",
            Self::Internal(_) => "internal",
            Self::Server(code) => code,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::UnsupportedProvider(id) => {
                format!("该 provider 不支持导入：{id}（没有已验证的 ACP 入口）")
            }
            Self::NotEnrolled => "请先 enroll（未找到可用的设备身份）".to_string(),
            Self::ProviderUnavailable => {
                "找不到该 provider 的本机会话库，或库结构不受支持".to_string()
            }
            Self::InvalidSession => "会话 id 不能为空".to_string(),
            Self::InvalidPath { windows: true } => "该会话的工作目录是 Windows 盘符路径；\
                 当前 Hub 工作区只接受 / 或 ~ 开头，Windows 设备暂不支持导入"
                .to_string(),
            Self::InvalidPath { windows: false } => "该会话的工作目录不合法：\
                 Hub 工作区只接受 / 或 ~ 开头的绝对路径"
                .to_string(),
            Self::InvalidHubUrl(reason) => format!("无法从 Hub 地址推导 HTTP 源：{reason}"),
            Self::Network(detail) => format!("无法连接 Hub：{detail}"),
            Self::BadSignature => "Hub 拒绝了设备签名".to_string(),
            Self::AgentUnavailable => "该 agent 在设备上不可用".to_string(),
            Self::AlreadyImported => "这条历史会话已经导入过".to_string(),
            Self::BadResponse => "Hub 返回了无法解析的响应".to_string(),
            Self::Internal(detail) => format!("本地请求构造失败：{detail}"),
            Self::Server(code) => format!("Hub 返回错误：{code}"),
        }
    }
}

fn report<E: Write>(err: &mut E, failure: ImportFailure) -> ExitCode {
    drop(writeln!(
        err,
        "afcli import: {}: {}",
        failure.code(),
        failure.message()
    ));
    ExitCode::FAILURE
}

fn map_import_error(error: ImportError) -> ImportFailure {
    match error {
        ImportError::InvalidWorkspacePath => ImportFailure::InvalidPath { windows: false },
        ImportError::BlankSessionId => ImportFailure::InvalidSession,
        ImportError::DbMissing | ImportError::SchemaMismatch | ImportError::Decode => {
            ImportFailure::ProviderUnavailable
        }
        // 走到这里说明注册表与清单脱节；按「不可用」处理，不把内部 reason 拼进 code。
        ImportError::UnknownProvider => ImportFailure::ProviderUnavailable,
    }
}

fn map_http_error(error: ImportHttpError) -> ImportFailure {
    match error {
        ImportHttpError::Network(detail) => ImportFailure::Network(detail),
        ImportHttpError::BadResponse => ImportFailure::BadResponse,
        ImportHttpError::Local(detail) => ImportFailure::Internal(detail),
        ImportHttpError::Server { code } => {
            let code = sanitize_server_code(&code);
            match code.as_str() {
                "device_signature_invalid" | "bad_signature" => ImportFailure::BadSignature,
                "agent_unavailable" => ImportFailure::AgentUnavailable,
                "invalid_workspace_path" | "invalid_path" => {
                    ImportFailure::InvalidPath { windows: false }
                }
                "import_already_exists" | "already_imported" => ImportFailure::AlreadyImported,
                _ => ImportFailure::Server(code),
            }
        }
    }
}

/// Hub 的错误 code 只保留可判定的 ASCII 片段，防止把 HTML / 路径 / SQL 原文
/// 当成 code 打到终端。
fn sanitize_server_code(code: &str) -> String {
    let cleaned: String = code
        .trim()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "server_error".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn list_requires_a_provider_and_accepts_a_data_dir() {
        assert!(parse(&args(&["list"])).is_err());
        assert!(parse(&args(&["list", "--provider"])).is_err());
        match parse(&args(&[
            "list",
            "--provider",
            "opencode",
            "--data-dir",
            "/tmp/xdg",
        ])) {
            Ok(Command::List(list)) => {
                assert_eq!(list.provider, "opencode");
                assert_eq!(list.data_dir.as_deref(), Some(Path::new("/tmp/xdg")));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
        assert!(parse(&args(&["list", "--provider", "opencode", "--hub", "h:1"])).is_err());
    }

    #[test]
    fn bind_requires_provider_and_session() {
        assert!(parse(&args(&["bind"])).is_err());
        assert!(parse(&args(&["bind", "--provider", "opencode"])).is_err());
        assert!(parse(&args(&[
            "bind",
            "--provider",
            "opencode",
            "--session",
            "  "
        ]))
        .is_err());
        match parse(&args(&[
            "bind",
            "--provider",
            "opencode",
            "--session",
            "ses_1",
            "--hub",
            "127.0.0.1:9002",
        ])) {
            Ok(Command::Bind(bind)) => {
                assert_eq!(bind.session, "ses_1");
                assert_eq!(bind.hub.as_deref(), Some("127.0.0.1:9002"));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn unknown_subcommands_and_flags_are_rejected() {
        assert!(parse(&[]).is_err());
        assert!(parse(&args(&["ls"])).is_err());
        assert!(parse(&args(&[
            "bind",
            "--provider",
            "opencode",
            "--session",
            "s",
            "--x"
        ]))
        .is_err());
    }

    /// 注入的目录是 `<平台数据目录>/agentfleet`；`--data-dir` 直接给平台基目录。
    #[test]
    fn the_base_dir_is_the_parent_of_the_injected_agentfleet_dir() {
        let injected = Path::new("/home/u/.local/share/agentfleet");
        assert_eq!(
            resolve_base(None, injected),
            Path::new("/home/u/.local/share")
        );
        assert_eq!(
            identity_file(&resolve_base(None, injected)),
            Path::new("/home/u/.local/share/agentfleet/identity.json")
        );
        assert_eq!(
            resolve_base(Some(Path::new("/tmp/xdg")), injected),
            Path::new("/tmp/xdg")
        );
        assert_eq!(
            identity_file(&resolve_base(Some(Path::new("/tmp/xdg")), injected)),
            Path::new("/tmp/xdg/agentfleet/identity.json")
        );
    }

    #[test]
    fn windows_drive_paths_are_recognized() {
        for path in ["C:\\Users\\x", "D:/work", "z:\\"] {
            assert!(is_windows_drive_path(path), "{path}");
        }
        for path in ["/tmp/x", "~/work", "C:", "1:\\x", "", "relative\\x"] {
            assert!(!is_windows_drive_path(path), "{path}");
        }
    }

    #[test]
    fn server_codes_are_sanitized_before_printing() {
        assert_eq!(
            sanitize_server_code(" DEVICE_SIGNATURE_INVALID "),
            "device_signature_invalid"
        );
        assert_eq!(sanitize_server_code("<html>bad</html>"), "_html_bad__html_");
        assert_eq!(sanitize_server_code(""), "server_error");
        assert_eq!(sanitize_server_code(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn server_errors_map_to_stable_cli_codes() {
        let mapped = map_http_error(ImportHttpError::Server {
            code: "DEVICE_SIGNATURE_INVALID".to_string(),
        });
        assert_eq!(mapped.code(), "bad_signature");
        let mapped = map_http_error(ImportHttpError::Server {
            code: "AGENT_UNAVAILABLE".to_string(),
        });
        assert_eq!(mapped.code(), "agent_unavailable");
        let mapped = map_http_error(ImportHttpError::Server {
            code: "UNSUPPORTED_IMPORT_PROVIDER".to_string(),
        });
        assert_eq!(mapped.code(), "unsupported_import_provider");
    }

    /// `--hub` 优先；空串 / 纯空白视作没给；环境变量也没有就退回本机缺省。
    /// 绝不把 `--hub ""` 当成「连空地址」。
    #[test]
    fn the_hub_flag_wins_over_the_env_var_and_blank_values_fall_through() {
        assert_eq!(
            hub_source(Some("127.0.0.1:9002"), Some("10.0.0.1:1")),
            "127.0.0.1:9002"
        );
        assert_eq!(
            hub_source(Some("  ws://h:1/ws \n"), Some("10.0.0.1:1")),
            "ws://h:1/ws"
        );
        assert_eq!(hub_source(Some("   "), Some("10.0.0.1:1")), "10.0.0.1:1");
        assert_eq!(hub_source(None, Some(" 10.0.0.1:1 ")), "10.0.0.1:1");
        assert_eq!(hub_source(None, Some("   ")), runargs::DEFAULT_HUB_URL);
        assert_eq!(hub_source(None, None), runargs::DEFAULT_HUB_URL);
    }

    /// 每个分类的 code 是脚本契约，不可随意改名；message 必须是人话且
    /// 不把内部路径 / 原始响应带出去。
    #[test]
    fn every_failure_has_a_stable_code_and_a_readable_message() {
        let cases = [
            (
                ImportFailure::UnsupportedProvider("claude（无 ACP）".to_string()),
                "unsupported_provider",
            ),
            (ImportFailure::NotEnrolled, "not_enrolled"),
            (ImportFailure::ProviderUnavailable, "provider_unavailable"),
            (ImportFailure::InvalidSession, "invalid_session"),
            (
                ImportFailure::InvalidPath { windows: false },
                "invalid_path",
            ),
            (ImportFailure::InvalidPath { windows: true }, "invalid_path"),
            (
                ImportFailure::InvalidHubUrl("坏地址".to_string()),
                "invalid_hub_url",
            ),
            (ImportFailure::Network("refused".to_string()), "network"),
            (ImportFailure::BadSignature, "bad_signature"),
            (ImportFailure::AgentUnavailable, "agent_unavailable"),
            (ImportFailure::AlreadyImported, "already_imported"),
            (ImportFailure::BadResponse, "bad_response"),
            (ImportFailure::Internal("encode".to_string()), "internal"),
            (
                ImportFailure::Server("rate_limited".to_string()),
                "rate_limited",
            ),
        ];
        for (failure, code) in cases {
            assert_eq!(failure.code(), code);
            let message = failure.message();
            assert!(!message.trim().is_empty(), "{code} 必须有人话");
            assert!(
                !message.contains("<html>") && !message.contains("opencode.db"),
                "{code} 的 message 泄漏了内部细节：{message}"
            );
        }
        assert!(ImportFailure::InvalidPath { windows: true }
            .message()
            .contains("Windows 盘符路径"));
        assert!(ImportFailure::NotEnrolled.message().contains("enroll"));
    }

    /// 网络 / 本地构造 / 响应解析三类错误各有独立 code，不能被并成 `internal`。
    #[test]
    fn transport_failures_keep_their_own_codes() {
        assert_eq!(
            map_http_error(ImportHttpError::Network("connection refused".to_string())).code(),
            "network"
        );
        assert_eq!(
            map_http_error(ImportHttpError::BadResponse).code(),
            "bad_response"
        );
        let local = map_http_error(ImportHttpError::Local("cannot encode".to_string()));
        assert_eq!(local.code(), "internal");
        assert!(local.message().contains("cannot encode"));
    }

    /// Hub 的路径类失败也要落回 `invalid_path`，而不是原样透出一个可能含
    /// 服务器内部命名的 code。
    #[test]
    fn server_path_and_already_imported_codes_are_normalized() {
        for code in ["invalid_workspace_path", "INVALID_PATH"] {
            assert_eq!(
                map_http_error(ImportHttpError::Server {
                    code: code.to_string()
                })
                .code(),
                "invalid_path"
            );
        }
        for code in ["import_already_exists", "already_imported"] {
            assert_eq!(
                map_http_error(ImportHttpError::Server {
                    code: code.to_string()
                })
                .code(),
                "already_imported"
            );
        }
        // 未知 code 只保留可判定的 ASCII 片段。
        let unknown = map_http_error(ImportHttpError::Server {
            code: "<html>500</html>".to_string(),
        });
        assert_eq!(unknown.code(), "_html_500__html_");
        assert!(!unknown.message().contains("<html>"));
    }

    #[test]
    fn provider_errors_map_to_cli_codes_without_leaking_paths() {
        let mapped = map_import_error(ImportError::DbMissing);
        assert_eq!(mapped.code(), "provider_unavailable");
        assert!(!mapped.message().contains("opencode.db"));
        let mapped = map_import_error(ImportError::InvalidWorkspacePath);
        assert_eq!(mapped.code(), "invalid_path");
        assert!(!mapped.message().contains("C:"));
    }
}
