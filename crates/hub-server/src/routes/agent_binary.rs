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
//! 2. `/opt/agentfleet/dist/afcli-{os}-{arch}[.exe]`（`Dockerfile.hub` 放的桌面载荷）；
//! 3. `/usr/local/bin/{os}/{arch}/afcli` 与 `…/afcli-{os}-{arch}`（分架构摆放，
//!    用归一化后的短名）；
//! 4. `/usr/local/bin/afcli`（= `Dockerfile.hub` 的 `COPY` 目标，**仅 Linux 请求**
//!    ——不拿 Linux 那份冒充 Windows/macOS）；
//! 5. `/usr/local/lib/agentfleet/…`、`~/.agentfleet/…`（裸机与本地调试，同上）。
//!
//! ## `?os=` / `?arch=`：先归一化成闭集，再只用于定位
//!
//! 浏览器把**它自己**的平台报进来，Hub 先把它归一化成枚举，再去拼
//! `/opt/agentfleet/dist/`（`Dockerfile.hub` 放的桌面载荷）与分架构目录的候选路径。
//! 闭集之外的写法（`..`、路径分隔符、空串、大小写变体）一律 400：否则
//! `?arch=../../…` 会被拼进 `/usr/local/bin/{os}/{arch}/afcli`，`tokio::fs::read`
//! 就会读到部署目录外的文件（例如 `/data/setup-token.txt`）。`AFCLI_BINARY_PATH`
//! 与 `HOME` 是运维侧环境变量、不受请求控制，所以显式配置仍按原样独占。
//!
//! 要的平台这台 Hub 没有，就如实 404。**绝不拿 Hub 自己那份 Linux afcli 冒充别的
//! 平台**：那会让设备下到一个能 `chmod +x` 但运行即 `Exec format error` 的文件，
//! 而报错发生在设备上、排查从错误的地方开始。所以不带平台信息的平铺候选（一定是
//! Linux 二进制）只在 `os=linux` 时才参与探测。
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
/// 它能供货的架构（镜像里只有同架构那一份）。缺省值同样要过闭集校验：编译目标
/// 不在闭集里时省略参数的请求回 400，而不是拿生字符串拼出越界路径。
const DEFAULT_OS: &str = std::env::consts::OS;
const DEFAULT_ARCH: &str = std::env::consts::ARCH;

/// `?os=&arch=` 两个可选参数。
///
/// 用 `Option<String>` 而不是给默认值：省略时用**编译期平台常量**，而不是在
/// 反序列化时塞一个猜测值。传了空串属于闭集之外的非法值，归一化时直接拒（400），
/// 不会悄悄回落成缺省平台。
#[derive(Debug, Default, Deserialize)]
pub struct AgentBinaryQuery {
    pub os: Option<String>,
    pub arch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CanonicalOs {
    Windows,
    Mac,
    Linux,
}

impl CanonicalOs {
    fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Mac => "mac",
            Self::Linux => "linux",
        }
    }

    /// `Dockerfile.hub` 的产物叫 `afcli-macos-arm64`，而归一后的短名是 `mac`。
    /// 两种写法都列进候选，否则镜像里那份真实存在的文件永远命中不到。
    fn artifact_names(self) -> &'static [&'static str] {
        match self {
            Self::Mac => &["mac", "macos"],
            Self::Windows => &["windows"],
            Self::Linux => &["linux"],
        }
    }

    /// 路径与 `Content-Disposition` 共用一个判据：找得到 `.exe` 却下载成
    /// 无后缀名，浏览器会因缺扩展名拒跑。
    fn exe_suffix(self) -> &'static str {
        match self {
            Self::Windows => ".exe",
            Self::Mac | Self::Linux => "",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CanonicalArch {
    Amd64,
    Arm64,
}

impl CanonicalArch {
    fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

/// 归一化后的平台。路径与响应头只允许由它拼出。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Platform {
    os: CanonicalOs,
    arch: CanonicalArch,
}

/// `?os=` / `?arch=` 只认闭集里的确切写法，其余一律拒绝。
///
/// 不做 trim / 大小写折叠：那会把「看起来像合法值」的输入悄悄改成另一个平台，
/// 也会让日志与响应头里出现调用方没写过的名字。
fn normalize_query_platform(os: &str, arch: &str) -> Option<Platform> {
    let os = match os {
        "windows" => CanonicalOs::Windows,
        "mac" | "macos" | "darwin" => CanonicalOs::Mac,
        "linux" => CanonicalOs::Linux,
        _ => return None,
    };
    let arch = match arch {
        "amd64" | "x86_64" => CanonicalArch::Amd64,
        "arm64" | "aarch64" => CanonicalArch::Arm64,
        _ => return None,
    };
    Some(Platform { os, arch })
}

/// 下载文件名只由归一化后的枚举拼出，拼不出分隔符或头注入字符。
fn download_filename(platform: Platform) -> String {
    format!(
        "afcli-{}-{}{}",
        platform.os.as_str(),
        platform.arch.as_str(),
        platform.os.exe_suffix()
    )
}

/// 按 REX 的候选顺序算出所有可能的路径（**只算，不判定存在**）。
///
/// 抽成纯函数是为了能单测：路径拼接错误在集成测试里表现为「404 了但文件明明在」，
/// 排查要起整个 app；这里直接对返回值断言即可。闭集之外的 `os`/`arch` 返回
/// `None`，调用方据此回 400。
pub fn candidate_paths(configured: Option<&str>, os: &str, arch: &str) -> Option<Vec<PathBuf>> {
    normalize_query_platform(os, arch).map(|platform| candidates_for(configured, platform))
}

/// 只接受归一化后的平台：所有路径片段都是枚举 `as_str()` 或固定字面量，HTTP 侧
/// 没有任何字符串能到达这里。`configured` 与 `HOME` 来自运维侧环境变量，不受
/// 请求控制，所以显式配置仍按原样独占。
fn candidates_for(configured: Option<&str>, platform: Platform) -> Vec<PathBuf> {
    // 显式配置最优先，且**独占**：运维写明了路径还去猜别的，等于悄悄忽略配置。
    if let Some(p) = configured {
        return vec![PathBuf::from(p)];
    }

    let Platform { os, arch } = platform;
    let os_name = os.as_str();
    let arch_name = arch.as_str();
    let suffix = os.exe_suffix();
    let mut out = Vec::new();

    // `Dockerfile.hub` 的桌面载荷：文件名就是产物名（`afcli-windows-amd64.exe` /
    // `afcli-macos-arm64`），不是子目录。它排在平铺那份**之前**——平铺那份是
    // Hub 自己执行的 Linux 二进制，绝不能拿来回应 Windows/macOS 的请求。
    for name in os.artifact_names() {
        out.push(PathBuf::from(format!(
            "/opt/agentfleet/dist/afcli-{name}-{arch_name}{suffix}"
        )));
    }

    // 分架构摆放的其它形态：部署方也可能把分平台产物按目录摆。
    for dir in ["/usr/local/bin", "/usr/local/lib/agentfleet"] {
        for sub in [
            format!("{os_name}/{arch_name}"),
            format!("{os_name}-{arch_name}"),
        ] {
            out.push(PathBuf::from(dir).join(&sub).join(format!("afcli{suffix}")));
            out.push(
                PathBuf::from(dir)
                    .join(&sub)
                    .join(format!("afcli-{os_name}-{arch_name}{suffix}")),
            );
        }
    }

    // 无架构信息的平铺摆放：`Dockerfile.hub` 的 `COPY … /usr/local/bin/afcli`、
    // 裸机与本地调试的 `~/.agentfleet/afcli`。**它们一律是 Linux 二进制**
    // （`afhub` 只发 Linux 目标）：Windows/macOS 请求不许落到它们身上，否则
    // 又是「点下载拿到 Linux 文件」。Linux 的两种 arch 都留着：分架构候选
    // 排在前，同一份平铺文件是否真是请求的架构由部署方决定（既有语义）。
    if os == CanonicalOs::Linux {
        out.push(PathBuf::from("/usr/local/bin/afcli"));
        out.push(PathBuf::from("/usr/local/lib/agentfleet/afcli"));
        if let Ok(home) = std::env::var("HOME") {
            out.push(PathBuf::from(home).join(".agentfleet").join("afcli"));
        }
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
    // 归一化一次，同一份 `Platform` 既造候选路径又造响应头；闭集之外的值
    // 不碰磁盘、不进日志，直接回固定 code 的 400。
    let Some(platform) = normalize_query_platform(os, arch) else {
        return ApiError::bad_request_body().into_response();
    };
    let candidates = candidates_for(configured_path().as_deref(), platform);

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
        return binary_response(bytes, platform, path);
    }

    // **不把路径写进响应体**（约束 13：错误体只有固定 code）。
    // 路径只进服务端日志——它属于部署细节，不该告诉匿名探路者。
    tracing::warn!(
        os = platform.os.as_str(),
        arch = platform.arch.as_str(),
        tried = ?candidates,
        "找不到可下发的 afcli 二进制；配置 `{AFCLI_PATH_ENV}` 或把 afcli 放到候选路径"
    );
    ApiError::new(StatusCode::NOT_FOUND, "agent_binary_unavailable").into_response()
}

/// 组装成功响应。抽出来是为了让上面那个循环只有一条出口。
fn binary_response(bytes: Vec<u8>, platform: Platform, path: &Path) -> Response {
    // 文件名带平台：从这一台 Hub 下发的可能是分架构目录里那一份，而**用户存下来
    // 之后**要靠文件名分辨。只由归一化枚举拼出，原始 query 到不了这里，
    // `Content-Disposition` 就不会被塞进 `/` 或换行。
    let filename = download_filename(platform);
    // 归一化后的名字一定合法；仍不 `unwrap`——常驻进程不因「不可能失败」退出。
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"afcli\""));

    tracing::debug!(
        path = %path.display(),
        os = platform.os.as_str(),
        arch = platform.arch.as_str(),
        "下发 afcli 二进制"
    );

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

#[cfg(test)]
mod tests {
    use super::*;

    /// `Dockerfile.hub` 把桌面产物放在 `/opt/agentfleet/dist/`，文件名就是产物名。
    /// 少了这条候选，Windows/macOS 浏览器点下载只会拿到 Hub 自己那份 Linux 文件。
    #[test]
    fn dist_directory_candidates_are_included() {
        // `Dockerfile.hub` 里那份的**确切名字**：少了它，macOS 浏览器点下载
        // 只会拿到 Hub 自己的 Linux 二进制。
        let got = candidate_paths(None, "macos", "arm64").expect("合法平台组合");
        assert!(
            got.contains(&PathBuf::from("/opt/agentfleet/dist/afcli-macos-arm64")),
            "dist 目录候选必须含镜像里的真实产物名：{got:?}"
        );
        // `mac` / `macos` / `darwin` 是同一台机器，候选表必须一致。
        let a = candidate_paths(None, "mac", "arm64").expect("合法平台组合");
        let b = candidate_paths(None, "macos", "arm64").expect("合法平台组合");
        let c = candidate_paths(None, "darwin", "arm64").expect("合法平台组合");
        assert_eq!(a, b, "`mac` 与 `macos` 必须等价");
        assert_eq!(a, c, "`mac` 与 `darwin` 必须等价");
        assert!(
            a.contains(&PathBuf::from("/opt/agentfleet/dist/afcli-mac-arm64")),
            "归一后的 `mac` 写法也该有一条候选：{a:?}"
        );
        // 显式配置仍然独占：不能因为它排第一就把 dist 候选挤掉语义。
        let configured =
            candidate_paths(Some("/opt/afcli"), "macos", "arm64").expect("合法平台组合");
        assert_eq!(configured.len(), 1, "显式配置时不该再猜 dist 路径");
    }

    /// Windows 载荷在镜像里叫 `afcli-windows-amd64.exe`；候选路径不带 `.exe`
    /// 就永远读不到它。`normalize_os` 的其它写法（win32 等）不在契约内。
    #[test]
    fn windows_dist_candidate_has_exe_suffix() {
        let got = candidate_paths(None, "windows", "amd64").expect("合法平台组合");
        let strs: Vec<String> = got.iter().map(|p| p.display().to_string()).collect();
        assert!(
            strs.contains(&"/opt/agentfleet/dist/afcli-windows-amd64.exe".to_string()),
            "Windows dist 候选必须带 .exe：{strs:?}"
        );
        assert!(
            !strs.contains(&"/opt/agentfleet/dist/afcli-windows-amd64".to_string()),
            "不该再猜一个无后缀的 Windows 产物：{strs:?}"
        );
        // 平铺那份 Linux afcli 不能被当成 Windows 的兜底。
        assert!(
            !strs.contains(&"/usr/local/bin/afcli".to_string()),
            "Windows 请求不许落到 Hub 自己的 Linux 二进制：{strs:?}"
        );
    }

    /// Linux 浏览器报 `amd64`/`arm64`（或它们的别名），仍要能读到镜像里平铺那份
    /// afcli；这条候选的归属由 os 决定，不能顺手按编译期 arch 常量收窄。
    #[test]
    fn linux_platform_keeps_the_flat_candidate_for_both_arch_names() {
        for arch in ["amd64", "arm64", "x86_64", "aarch64"] {
            let got = candidate_paths(None, "linux", arch).expect("合法架构");
            assert!(
                got.contains(&PathBuf::from("/usr/local/bin/afcli")),
                "Linux `{arch}` 请求应保留平铺候选：{got:?}"
            );
        }
    }

    /// 浏览器靠 `Content-Disposition` 决定存下来的名字。Windows 少 `.exe`
    /// 会得到一个双击跑不起来的文件（用户还得自己改名、猜是什么）。
    #[test]
    fn windows_download_filename_has_exe_suffix() {
        let r = binary_response(
            b"\x7fELF-fake".to_vec(),
            normalize_query_platform("windows", "amd64").expect("合法平台组合"),
            Path::new("/opt/agentfleet/dist/afcli-windows-amd64.exe"),
        );
        let disp = r
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .expect("成功下发必须有 Content-Disposition");
        assert!(
            disp.contains("afcli-windows-amd64.exe"),
            "Windows 文件名必须带 .exe，实际：{disp}"
        );

        // 反例：非 Windows 平台不加后缀，别把 `.exe` 拼到所有文件名上。
        let mac = binary_response(
            b"\x7fELF-fake".to_vec(),
            normalize_query_platform("macos", "arm64").expect("合法平台组合"),
            Path::new("/opt/agentfleet/dist/afcli-macos-arm64"),
        );
        let mac_disp = mac
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .expect("成功下发必须有 Content-Disposition");
        assert!(
            mac_disp.contains("afcli-mac-arm64\""),
            "macOS 文件名不该带 .exe，实际：{mac_disp}"
        );
    }

    /// 复核里那条 payload：`?arch=amd64/../../…` 曾会被拼进候选路径再被
    /// `tokio::fs::read` 读走。非法值必须在碰磁盘之前就返回 `None`。
    #[test]
    fn path_traversal_in_platform_query_is_rejected() {
        let traversal = "amd64/../../../../data/setup-token.txt";
        assert!(normalize_query_platform("linux", traversal).is_none());
        assert!(candidate_paths(None, "linux", traversal).is_none());
        // 显式配置也不是绕过校验的通道。
        assert!(candidate_paths(Some("/opt/afcli"), "linux", traversal).is_none());
        // os 侧同理。
        assert!(candidate_paths(None, "linux/../../data", "amd64").is_none());

        let got = candidate_paths(None, "linux", "amd64").expect("合法平台组合");
        assert!(
            !got.iter()
                .any(|p| p.display().to_string().contains("setup-token")),
            "合法请求不该产生任何越界候选：{got:?}"
        );
    }

    #[test]
    fn invalid_platform_values_are_rejected() {
        for arch in [
            "",
            "..",
            "../",
            "amd64/..",
            "amd64/../../../../data/setup-token.txt",
            "arm64\\..\\setup-token.txt",
            "AMD64",
            " amd64",
            "amd64 ",
            "x86",
            "amd64\n",
            "aarch64/..%2f..",
        ] {
            assert!(
                normalize_query_platform("linux", arch).is_none(),
                "非法的 arch 必须被拒：{arch:?}"
            );
            assert!(candidate_paths(None, "linux", arch).is_none());
            assert!(candidate_paths(Some("/opt/afcli"), "linux", arch).is_none());
        }
        for os in [
            "",
            "..",
            "linux/../../etc",
            "windows\\..\\..",
            "WINDOWS",
            "win32",
            "freebsd",
            "mac os",
            "linux\n",
        ] {
            assert!(
                normalize_query_platform(os, "amd64").is_none(),
                "非法的 os 必须被拒：{os:?}"
            );
            assert!(candidate_paths(None, os, "amd64").is_none());
        }
    }

    /// 归一化之后路径片段全部来自枚举与固定字面量：候选只能落在固定根目录
    /// （或运维侧 `HOME`）之下，且不含 `..` / `.` 组件。
    #[test]
    fn candidates_stay_within_fixed_directory_boundaries() {
        let roots = [
            "/opt/agentfleet/dist",
            "/usr/local/bin",
            "/usr/local/lib/agentfleet",
        ];
        for os in ["windows", "mac", "macos", "darwin", "linux"] {
            for arch in ["amd64", "arm64", "x86_64", "aarch64"] {
                let got = candidate_paths(None, os, arch).expect("合法平台组合");
                assert!(!got.is_empty(), "{os}/{arch} 必须有候选");
                for p in &got {
                    let s = p.display().to_string();
                    assert!(p.is_absolute(), "候选必须是绝对路径：{s}");
                    assert!(
                        !p.components().any(|c| matches!(
                            c,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )),
                        "候选含 `..` / `.` 组件：{s}"
                    );
                    let under_home = std::env::var("HOME")
                        .ok()
                        .is_some_and(|h| p.starts_with(h.as_str()));
                    assert!(
                        under_home || roots.iter().any(|r| p.starts_with(*r)),
                        "候选逃出固定根目录：{s}"
                    );
                }
            }
        }
    }

    /// `Content-Disposition` 只能用归一后的短名：别名与非法串都不能进头。
    #[test]
    fn download_filename_is_canonical_and_cannot_be_polluted() {
        for arch in ["amd64", "x86_64"] {
            let p = normalize_query_platform("windows", arch).expect("合法平台组合");
            assert_eq!(download_filename(p), "afcli-windows-amd64.exe");
            let r = binary_response(b"fake".to_vec(), p, Path::new("/opt/afcli"));
            let disp = r
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .expect("成功下发必须有 Content-Disposition");
            assert!(
                !disp.contains('/') && !disp.contains("..") && !disp.contains('\r'),
                "响应头被污染：{disp}"
            );
        }
        assert_eq!(
            download_filename(normalize_query_platform("linux", "aarch64").expect("合法组合")),
            "afcli-linux-arm64"
        );
        assert_eq!(
            download_filename(normalize_query_platform("macos", "aarch64").expect("合法组合")),
            "afcli-mac-arm64"
        );
        assert_eq!(
            download_filename(normalize_query_platform("darwin", "arm64").expect("合法组合")),
            "afcli-mac-arm64"
        );
        // 非法值造不出 `Platform`，也就造不出任何 `Content-Disposition`。
        for bad in [
            "../../setup-token.txt",
            "amd64/../../x",
            "aarch64\r\nX-Foo: 1",
            "",
        ] {
            assert!(
                normalize_query_platform("linux", bad).is_none(),
                "非法 arch 不该进响应头：{bad:?}"
            );
        }
    }
}
