//! afhub：Hub 的 HTTP / WS 服务进程。
//!
//! 约束 1：**Hub 零入站端口**——本二进制只做两件事：
//! 1. 监听设备侧**主动外联**的 `/ws` 升级；
//! 2. 托管页面静态资源与 `/setup` `/login` 等少量 HTTP 路由。
//!
//! 监听地址与数据目录由环境变量配置（`AFHUB_ADDR` / `AFHUB_DATA_DIR`），
//! 路由装配在 [`hub_server::build_app`]，本文件只负责进程生命周期与
//! 数据目录初始化，便于「不起真端口」地单测路由本身。
//!
//! 本文件的每条路径都由 `tests/afhub_process.rs` **真的 spawn 这个二进制**覆盖：
//! 起服务 → `GET /healthz` → SIGTERM/SIGINT 优雅关闭 → 拒绝非回环地址 → 端口被占。
//! 唯一不可控的一条（`Runtime::new()` 失败）已就地标注 `coverage: excluded`。

use std::net::SocketAddr;
use std::path::PathBuf;

use hub_storage::db::{data_dir, ensure_dir, open_in, set_umask_077};

/// 监听地址环境变量名。缺省为 [`DEFAULT_ADDR`]。
const ENV_ADDR: &str = "AFHUB_ADDR";
/// 数据目录环境变量名。缺省走 `hub_storage::db::data_dir()`。
const ENV_DATA_DIR: &str = "AFHUB_DATA_DIR";
/// 默认监听地址。**只绑回环**：Hub 不对局域网暴露，
/// 远程访问走 Tailscale / SSH 端口转发（§2.4）。
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

fn main() -> std::process::ExitCode {
    // umask 必须**先于**任何文件创建落，否则第一个文件会带着宽权限落地
    set_umask_077();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        // coverage: excluded — 建不出 OS 线程（fd/内存耗尽或线程数受限），
        // 无可控手段在测试里构造；守卫逻辑本身由各处错误分支覆盖。
        Err(e) => return fail(format_args!("cannot start tokio runtime: {e}")),
    };

    match rt.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // 配置先于一切副作用：地址不合法就不该在磁盘上留下数据目录 / 数据库
    let addr = resolve_addr()?;
    let dir = resolve_data_dir()?;
    let (db_path, pool) = open_in(&dir)?;
    tracing::info!(dir = %dir.display(), db = %db_path.display(), "hub storage ready");

    // 持有连接池到进程结束：drop 掉会让运行中的查询拿到 "closed pool"
    let _pool = pool;

    // SIGINT（Ctrl-C）与 SIGTERM（systemd / docker stop 默认）都要能优雅关闭
    let mut sigterm = sigterm_listener()?;

    let app = hub_server::build_app();
    let listener =
        tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> {
                Box::new(std::io::Error::new(
                    e.kind(),
                    format!("cannot bind {addr}: {e}"),
                ))
            })?;
    tracing::info!(addr = %addr, "afhub listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("afhub: SIGINT, shutting down"),
                _ = sigterm.recv() => tracing::info!("afhub: SIGTERM, shutting down"),
            }
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
    tracing::info!("afhub stopped");
    Ok(())
}

/// 解析监听地址，**只允许回环**。
///
/// 这是约束 1 的落点：Hub 进程永远不绑非回环地址，局域网里就没有可达的 Hub 端口，
/// 远程访问一律走 Tailscale / SSH 端口转发（§2.4）。传了非回环地址直接**拒绝启动**，
/// 而不是「先绑上去再说」——self-host 场景下误配 `0.0.0.0` 等于把 `/setup` 暴露给同网段。
///
/// 只接受 **IP 字面量**（`127.0.0.1` / `::1` 或同段），**不接受主机名**（含 `localhost`）：
/// 主机名解析结果是不是回环依赖 DNS（本地 hosts、容器 DNS、缓存都可能给出不同答案），
/// 不能让「是否暴露给局域网」这件事取决于一次名字解析。写 `localhost` 会被当作
/// 「不是 socket 地址」拒绝，提示用户改用 `127.0.0.1`。
fn resolve_addr() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let raw = std::env::var(ENV_ADDR).unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let addr: SocketAddr = raw
        .parse()
        .map_err(|e| format!("{ENV_ADDR}={raw}: not a socket address ({e})"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "{ENV_ADDR}={raw}: refusing to bind a non-loopback address; \
             afhub only listens on loopback (127.0.0.0/8 or ::1)"
        )
        .into());
    }
    Ok(addr)
}

/// 解析数据目录：`AFHUB_DATA_DIR` 优先，否则落到平台标准目录。
fn resolve_data_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    match std::env::var_os(ENV_DATA_DIR) {
        Some(raw) if !raw.is_empty() => {
            let dir = PathBuf::from(raw);
            ensure_dir(&dir)?;
            Ok(dir)
        }
        // 环境变量缺省（或显式给空串）→ 平台标准目录。
        // 目录权限由 ensure_dir 收紧到 0700（约束 13），不依赖调用方的 umask。
        _ => Ok(data_dir()?),
    }
}

/// 注册 SIGTERM 处理器。
///
/// 注册失败按**启动失败**处理（`run` 用 `?` 传播成非零退出），而不是静默变成
/// 「收不到 SIGTERM、部署方只能 SIGKILL 掉」，那样每次关机都会丢未落盘的写入。
fn sigterm_listener() -> std::io::Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
}

/// 启动错误出口：统一前缀打到 stderr，返回失败退出码。
fn fail(msg: impl std::fmt::Display) -> std::process::ExitCode {
    eprintln!("afhub: {msg}");
    std::process::ExitCode::FAILURE
}
