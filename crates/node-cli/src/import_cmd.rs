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

use node_core::identity::{Identity, APP_DIR, IDENTITY_FILE};
use node_core::import::{self, HistoricalSession, ImportError, ProviderLookup};

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

/// 两个子命令共有的 flag：`--provider` / `--data-dir` 与未知参数。
///
/// 返回 `CommonFlags::Skip(2)` 表示本 parser 不必再处理这个 flag；
/// 其余分支由各 parser 自己处理。错误字符串与拆分前逐字相同。
enum CommonFlags {
    Skip(usize),
    NotCommon,
}

fn parse_common_flag(
    args: &[String],
    i: usize,
    arg: &str,
    provider: &mut Option<String>,
    data_dir: &mut Option<PathBuf>,
) -> Result<CommonFlags, String> {
    match arg {
        "--provider" => {
            *provider = Some(take_value(args, i, "--provider")?.to_string());
            Ok(CommonFlags::Skip(2))
        }
        "--data-dir" => {
            *data_dir = Some(PathBuf::from(take_value(args, i, "--data-dir")?));
            Ok(CommonFlags::Skip(2))
        }
        _ => Ok(CommonFlags::NotCommon),
    }
}

fn missing_provider() -> String {
    "缺少 --provider".to_string()
}

fn parse_list(args: &[String]) -> Result<ListArgs, String> {
    let mut provider = None;
    let mut data_dir = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let common = parse_common_flag(args, i, arg.as_str(), &mut provider, &mut data_dir)?;
        if let CommonFlags::Skip(step) = common {
            i += step;
            continue;
        }
        return Err(format!("未知参数 `{arg}`"));
    }
    let provider = provider.ok_or_else(missing_provider)?;
    Ok(ListArgs { provider, data_dir })
}

fn parse_bind(args: &[String]) -> Result<BindArgs, String> {
    let mut provider = None;
    let mut session = None;
    let mut hub = None;
    let mut data_dir = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let common = parse_common_flag(args, i, arg.as_str(), &mut provider, &mut data_dir)?;
        if let CommonFlags::Skip(step) = common {
            i += step;
            continue;
        }
        match arg.as_str() {
            "--session" => {
                session = Some(take_value(args, i, "--session")?.to_string());
                i += 2;
            }
            "--hub" => {
                hub = Some(take_value(args, i, "--hub")?.to_string());
                i += 2;
            }
            other => return Err(format!("未知参数 `{other}`")),
        }
    }
    let provider = provider.ok_or_else(missing_provider)?;
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
    let base = data_base(args.data_dir.as_deref(), data_dir);
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
    let base = data_base(args.data_dir.as_deref(), data_dir);

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
    let resolved = provider.resolve(&base, session);
    // `resolve` 不带 cwd/title（它只服务于绑定所需字段）。这两个展示字段从
    // **同一行**的精确查询里取一次：`session_meta` 是单行参数化查询，`bind`
    // 因此不做任何全表 `list()`。Windows 盘符提示与 title 共用这一行。
    //
    // 读失败**绝不**用 `.ok()` 吞掉：那会把 DbMissing / SchemaMismatch /
    // Decode 静默改判成 `invalid_path`，与 `run_list` 的口径分叉。
    let meta = bind_metadata(provider, &base, session);
    let candidate = match resolved {
        Ok(candidate) => candidate,
        Err(ImportError::InvalidWorkspacePath) => {
            return report(err, invalid_path_failure(meta));
        }
        Err(error) => return report(err, map_import_error(error)),
    };

    // 取不到就用空串：Hub 侧 title 只是展示字段，不该因此阻断导入。
    let title = match meta {
        Ok(row) => row.map(|row| row.title).unwrap_or_default(),
        Err(failure) => return report(err, failure),
    };

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

/// 绑定时取那一条会话的完整元数据（cwd / title）。
///
/// provider 的读错误按 [`map_import_error`] 归到 `provider_unavailable` 等
/// 稳定 code，**绝不**静默降级成「没有元数据」。
fn bind_metadata(
    provider: &dyn import::HistoryProvider,
    base: &Path,
    session: &str,
) -> Result<Option<HistoricalSession>, ImportFailure> {
    provider
        .session_meta(base, session)
        .map_err(map_import_error)
}

/// `resolve` 报路径不合法时的最终失败分类：元数据读失败优先于路径结论，
/// 行可读时才保留「Windows 盘符」这句可读原因。
fn invalid_path_failure(meta: Result<Option<HistoricalSession>, ImportFailure>) -> ImportFailure {
    match meta {
        Ok(Some(row)) => ImportFailure::InvalidPath {
            windows: is_windows_drive_path(&row.cwd),
        },
        Ok(None) => ImportFailure::InvalidPath { windows: false },
        Err(failure) => failure,
    }
}

fn provider_for(id: &str) -> Result<&'static dyn import::HistoryProvider, ImportFailure> {
    match import::lookup(id) {
        ProviderLookup::Supported(provider) => Ok(provider),
        ProviderLookup::Unsupported { agent_id, reason } => Err(
            ImportFailure::UnsupportedProvider(format!("{agent_id}（{reason}）")),
        ),
    }
}

/// 求平台数据基目录：`--data-dir` 覆盖优先，缺省只取注入的
/// `<base>/agentfleet` 的父目录（无父目录时原样返回）。
///
/// **不解析、不校验、不规范化路径**：这里只做「覆盖或取父目录」这一步，
/// 路径语义归 provider / 身份文件各自处理。
fn data_base(flag: Option<&Path>, injected: &Path) -> PathBuf {
    match flag {
        Some(dir) => dir.to_path_buf(),
        None => injected
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| injected.to_path_buf()),
    }
}

fn identity_file(base: &Path) -> PathBuf {
    // 用 `node_core::identity::APP_DIR` 而不是手抄子目录名；**不**调用
    // `data_dir_in()`：那个入口会创建目录，导入只读、不得写文件系统。
    base.join(APP_DIR).join(IDENTITY_FILE)
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
#[path = "import_cmd/tests.rs"]
mod tests;
