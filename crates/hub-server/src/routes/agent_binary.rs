//! `/api/agent-binary`：从**当前 Hub 实例**下载 `afcli`，让「装设备」不必先去 GitHub。
//!
//! ## 为什么要有这个接口
//!
//! 原来的接入引导是「在设备上跑 `afcli run <hub-url>`」——可那台设备上首先得有
//! `afcli`。GitHub Releases 能用，但两个场景会卡住：
//!
//! - **内网 / 离线部署**：Hub 自己就在隔离网里，设备够不到 GitHub；
//! - **版本对齐**：Release 页面上有好几个版本，选错一个就连不上（协议可能已变）。
//!
//! 从 Hub 自己下发就同时消灭这两件事：拿到的**一定**是「这个 Hub 会认的」那一个。
//!
//! ## 查找顺序照 REX 的 `AgentBinaries::find`（多候选路径）
//!
//! 单一固定路径的问题不是「不够灵活」，而是**部署形态一多就必然漏**：镜像里在
//! `/usr/local/bin`、裸机可能在 `/usr/local/lib/agentfleet`、本地调试在 `~/.agentfleet`。
//! 只认一条路径时，每种新部署形态都要改代码。所以按候选列表依次探测：
//!
//! 1. `AFCLI_BINARY_PATH`（显式配置，永远最优先——运维明确指定时不许猜）；
//! 2. `/usr/local/bin/{os}/{arch}/afcli` 与 `…/afcli-{os}-{arch}`（分架构摆放）；
//! 3. `/usr/local/bin/afcli`（= `Dockerfile.hub` 的 `COPY` 目标，镜像里唯一的形态）；
//! 4. `/usr/local/lib/agentfleet/…`、`~/.agentfleet/…`（裸机与本地调试）。
//!
//! ## `?os=` / `?arch=`：解析出来**只用于定位**，不虚构
//!
//! REX 的镜像里内嵌了**所有**架构的 agent 二进制，所以它能对任意 `os/arch` 供货。
//! 我们的 `Dockerfile.hub` 只 `COPY` 了**同架构**的那一份（`${TARGETARCH}`）——
//! 于是这里的参数只干一件事：把「你要的那个平台」拼进候选路径。要的平台这台 Hub
//! 没有，就如实 404。**绝不拿本机那份冒充别的架构**：那会让设备下到一个能 `chmod +x`
//! 但运行即 `Exec format error` 的文件，而报错发生在设备上、排查从错误的地方开始。
//!
//! ## 二进制从哪来
//!
//! **不嵌进 Hub 二进制。** `Dockerfile.hub` 里 `afcli` 是 `COPY` 到
//! `/usr/local/bin/afcli` 的独立文件（同一个镜像里两个进程共用一份静态二进制），
//! 运行时读它即可。把它再 `include_dir!` 进 `afhub` 会让镜像里有两个副本
//! （约 20MB 重复），而「单二进制」那条硬需求讲的是**前端资产**，不是把
//! 所有可执行文件都塞进一个。
//!
//! ## 找不到时的行为
//!
//! **404 + 固定 code，不 500。** 裸机跑 `afhub`（不经镜像）时磁盘上没有
//! `afcli` 文件，这是**正常部署形态**，不是服务故障——500 会让运维去翻日志
//! 找一个不存在的 bug。
//!
//! ## 认证
//!
//! **要 session**：它不下发密钥，但「谁能把二进制拿走」不该对未登录者开放——
//! 一个能匿名取 20MB 文件的端点就是个放大攻击面。所以本路由注册在
//! `build_app` 的 `.layer(require_auth)` **之前**，与其余 `/api/*` 一致。

use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::{ApiError, AppState};

/// 环境变量名：`afcli` 二进制在磁盘上的位置。显式配置时**只认它**，不再猜。
pub const AFCLI_PATH_ENV: &str = "AFCLI_BINARY_PATH";

/// 当前平台缺省名（`os`/`arch` 参数省略时用）。
///
/// `std::env::consts` 给的是编译期常量，与本机真实平台一致——Hub 自己的架构就是
/// 它能供货的架构（镜像里只有同架构那一份）。
const DEFAULT_OS: &str = std::env::consts::OS;
const DEFAULT_ARCH: &str = std::env::consts::ARCH;

/// `?os=&arch=` 两个可选参数。
///
/// 用 `Option<String>` 而不是给默认值：省略时用**编译期平台常量**，而不是在
/// 反序列化时塞一个猜测值。两者在正常路径上等价，但「没传」与「传了空串」在
/// 日志里应该分得开。
#[derive(Debug, Default, Deserialize)]
pub struct AgentBinaryQuery {
    pub os: Option<String>,
    pub arch: Option<String>,
}

/// 把 `os` 归一化成候选路径里用的短名。
///
/// `std::env::consts::OS` 在 Linux 上是 `linux`、macOS 上是 `macos`，而 Release
/// 产物与用户口中的平台名是 `mac` / `linux` / `windows`。两个都接受，避免
/// 「同一台机器按文档写 `mac` 要 404、按 `consts::OS` 写 `macos` 能用」。
fn normalize_os(os: &str) -> &str {
    match os {
        "macos" => "mac",
        // 反方向也认：文档里写 `mac` 的人，与 `consts::OS` 给出 `macos` 的人，
        // 指的是同一台机器。
        "darwin" => "mac",
        other => other,
    }
}

/// 按 REX 的候选顺序算出所有可能的路径（**只算，不判定存在**）。
///
/// 抽成纯函数是为了能单测：路径拼接错误在集成测试里表现为「404 了但文件明明在」，
/// 排查要起整个 app；这里直接对返回值断言即可。
pub fn candidate_paths(configured: Option<&str>, os: &str, arch: &str) -> Vec<PathBuf> {
    // 显式配置最优先，且**独占**：运维写明了路径还去猜别的，等于悄悄忽略配置。
    if let Some(p) = configured {
        return vec![PathBuf::from(p)];
    }

    let os = normalize_os(os);
    let mut out = Vec::new();

    // 分架构摆放：`{dir}/{os}/{arch}/afcli` 与 `{dir}/{os}-{arch}/afcli`。
    // 两种分隔符都试：`os/arch` 是 REX 的目录习惯，`os-arch` 是 CI artifact 的
    // 命名习惯（`dist/${TARGETARCH}/afcli-linux-amd64-musl` 那一套）。
    //
    // **不照搬 REX 的容器内二进制目录**：它的目录名把设备侧二进制称作 agent，
    // 违反 `AGENTS.md` 约束 2（`check.sh` 的 C2 专门拦这个），而且本仓的
    // `Dockerfile.hub` 里根本没有那个目录——照抄只会多一条永远命不中的候选，
    // 让「试过哪些路径」的日志更难读。
    for dir in ["/usr/local/bin", "/usr/local/lib/agentfleet"] {
        for sub in [format!("{os}/{arch}"), format!("{os}-{arch}")] {
            out.push(PathBuf::from(dir).join(&sub).join("afcli"));
            out.push(
                PathBuf::from(dir)
                    .join(&sub)
                    .join(format!("afcli-{os}-{arch}")),
            );
        }
    }

    // 无架构信息的平铺摆放：`Dockerfile.hub` 的 `COPY … /usr/local/bin/afcli`。
    // 这台 Hub 只装同架构那一份，所以「按平台名找不到」时它仍是正确答案。
    out.push(PathBuf::from("/usr/local/bin/afcli"));

    // 裸机与本地调试。
    out.push(PathBuf::from("/usr/local/lib/agentfleet/afcli"));
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join(".agentfleet").join("afcli"));
    }

    out
}

/// 从环境变量算出生效的显式路径。**每次请求现算**而不是缓存到 `AppState`：
///
/// 缓存会让「改配置要重启」成为一条隐含前提，而它从没被写进文档；现算的代价
/// 可忽略（一次 `std::env::var`，不碰磁盘）。**注意本函数必须在 `.await` 之前
/// 调用完**——`std::env` 的读取是同步的，把它夹在 await 之间只会让借用检查器
/// 多绕一圈。
fn configured_path() -> Option<String> {
    // 空串按「没配」处理：`AFCLI_BINARY_PATH=` 是「我不想配」的常见写法，
    // 而把它当路径会让 `candidate_paths` 只试一个空路径、永远 404 且不报出
    // 「你配错了」。这与 `AFHUB_ALLOW_NON_LOOPBACK` 只认确切值同理。
    std::env::var(AFCLI_PATH_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// `GET /api/agent-binary?os=&arch=`：把 `afcli` 作为附件下发。
pub async fn get_agent_binary(
    State(_state): State<AppState>,
    Query(q): Query<AgentBinaryQuery>,
) -> Response {
    let os = q.os.as_deref().unwrap_or(DEFAULT_OS);
    let arch = q.arch.as_deref().unwrap_or(DEFAULT_ARCH);
    let candidates = candidate_paths(configured_path().as_deref(), os, arch);

    // 依次探测，第一个**读得出来且非空**的胜出。
    //
    // 「读得出来」而不只是「存在」：`exists()` 之后到 `read()` 之间文件可能被删
    // （或权限不足），那会让一个 `if path.exists()` 的判断把 500 漏出去。
    for path in &candidates {
        let Ok(bytes) = tokio::fs::read(path).await else {
            continue;
        };
        if bytes.is_empty() {
            // 0 字节文件不是「空的 afcli」，是坏部署。当成成功下发会让设备拿到一个
            // `chmod +x` 后立刻 `Exec format error` 的文件，而报错发生在设备上。
            // 继续试下一个候选：分架构那份坏了，平铺那份可能还是好的。
            tracing::warn!(path = %path.display(), "afcli 二进制是 0 字节，跳过该候选");
            continue;
        }
        return binary_response(bytes, os, arch, path);
    }

    // **不把路径写进响应体**（约束 13：错误体只有固定 code）。
    // 路径只进服务端日志——它属于部署细节，不该告诉匿名探路者。
    tracing::warn!(
        os,
        arch,
        tried = ?candidates,
        "找不到可下发的 afcli 二进制；配置 `{AFCLI_PATH_ENV}` 或把 afcli 放到候选路径"
    );
    ApiError::new(StatusCode::NOT_FOUND, "agent_binary_unavailable").into_response()
}

/// 组装成功响应。抽出来是为了让上面那个循环只有一条出口。
fn binary_response(bytes: Vec<u8>, os: &str, arch: &str, path: &Path) -> Response {
    // 文件名带平台：从这一台 Hub 下发的可能是分架构目录里那一份，而**用户存下来
    // 之后**要靠文件名分辨。`Content-Disposition` 的值必须是静态 `&str` 才能塞进
    // `HeaderValue::from_static`，所以这里走 `from_str` 并在失败时回落到无平台名。
    let filename = format!("afcli-{}-{}", normalize_os(os), arch);
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"afcli\""));

    tracing::debug!(path = %path.display(), os, arch, "下发 afcli 二进制");

    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response()
}
