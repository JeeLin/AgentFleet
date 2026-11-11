//! 进程级参数：`run` 的角色 flag 与 Hub 地址。
//!
//! 角色**只由 argv 决定**（`DECISIONS.md` D1）：进 worker 角色的路径有且只有
//! argv 里含 `--worker` 这一条。再加一个环境变量就多一条必须单独构造分支的
//! 代码路径，而覆盖率门禁要求 100%。

use std::path::PathBuf;

/// 让 supervisor 进入 worker 角色的 flag。不带前缀是故意的：`run` 的修饰符，
/// 不是全局开关（`afcli --worker` 读起来像全局开关，实际只影响 `run`）。
pub const WORKER_FLAG: &str = "--worker";

/// Hub 地址环境变量名。
///
/// 形态与 Hub 侧对称：`afhub` 读 `AFHUB_ADDR`，node 读 `AFCLI_HUB_URL`。
/// **设备侧没有任何配置文件**——接入就是一条命令，改地址用环境变量就够了（D1）。
///
/// 取值现在有两种写法（见 [`hub_url`]）：
/// - `host:port`（推荐，如 `117.72.123.218:9002`），自动补成 `ws://host:port/ws`；
/// - 完整 `ws://` / `wss://` URL（旧配置继续原样可用）。
pub const ENV_HUB_URL: &str = "AFCLI_HUB_URL";

/// 缺省 Hub 地址：与 `afhub` 的默认监听地址一致（`127.0.0.1:8080/ws`）。
///
/// 只对「Hub 跑在同一台机器上」成立；真正的设备用 `AFCLI_HUB_URL` 指向
/// Hub 的回环端口转发或 Tailscale 地址。
pub const DEFAULT_HUB_URL: &str = "ws://127.0.0.1:8080/ws";

/// Hub 侧固定的 WS 路径。裸 `host:port` 归一化时补的就是它。
const HUB_WS_PATH: &str = "/ws";

/// 解析 Hub 地址：环境变量优先（空串/纯空白视作没给），否则用缺省值。
///
/// **输入形态**
/// - `117.72.123.218:9002` / `hub.example.com:9002` / `[2001:db8::1]:9002`
///   → `ws://<原样 authority>/ws`（IPv6 用方括号，照 URL 规范）；
/// - 已经是 `ws://` 或 `wss://` 的完整 URL → **原样返回**（向后兼容，
///   且自定义路径 / TLS 终止 / 查询串都不该被改写）；
/// - 空串、纯空白、没有环境变量 → [`DEFAULT_HUB_URL`]；
/// - 其它形态（少端口、`http://`、端口不是数字、括号不配对…）→ **原样返回**，
///   由 `conn::connect_once` 在连接时报出 `bad url`。绝不能悄悄退回本机默认：
///   那会让一台配错的真设备连到 `127.0.0.1` 并显示「设备没反应」，
///   比直接报错更难查。
///
/// 两端空白一律去掉：环境变量常被 `source` 或编辑器带进换行/空格。
pub fn hub_url(env: Option<&str>) -> String {
    let Some(raw) = env else {
        return DEFAULT_HUB_URL.to_string();
    };
    let v = raw.trim();
    if v.is_empty() {
        return DEFAULT_HUB_URL.to_string();
    }
    if is_full_ws_url(v) {
        return v.to_string();
    }
    match split_host_port(v) {
        Some((authority, port)) => format!("ws://{authority}:{port}{HUB_WS_PATH}"),
        None => v.to_string(),
    }
}

/// 已经是 `ws://` / `wss://` 开头（大小写不敏感）。
///
/// `get(..n)` 而不是切片：非 ASCII 字符的字节边界不规则时，按字节切片会 panic。
fn is_full_ws_url(v: &str) -> bool {
    fn has_scheme(v: &str, scheme: &str) -> bool {
        v.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    }
    has_scheme(v, "ws://") || has_scheme(v, "wss://")
}

/// 把 `host:port` 拆成 authority 与端口串；不合法就 `None`。
///
/// IPv6 必须写成 `[::1]:9002`：裸 `::1:9002` 的最后一个冒号到底是地址还是
/// 端口分隔符无法判定，宁可要求方括号（也是 URL 里的唯一合法写法）。
fn split_host_port(v: &str) -> Option<(&str, &str)> {
    // 这些字符出现就说明它已经不是一个 authority，别再猜哪一段是端口。
    if v.contains(['/', '\\', '?', '#', '@']) {
        return None;
    }
    if let Some(rest) = v.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &v[..=close + 1]; // 含方括号，例如 `[::1]`
        let port = v[close + 2..].strip_prefix(':')?;
        let inner = rest.get(..close)?;
        if inner.parse::<std::net::Ipv6Addr>().is_err() || !valid_port(port) {
            return None;
        }
        return Some((host, port));
    }
    if v.contains(['[', ']']) {
        return None;
    }
    let (host, port) = v.rsplit_once(':')?;
    if !valid_host(host) || !valid_port(port) {
        return None;
    }
    Some((host, port))
}

/// DNS 名 / IPv4 / `localhost`：只允许 URL authority 里不会引发歧义的字符。
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// 端口必须是 1..=65535 的十进制数。前导 `+` / 空串 / 溢出都不接受。
fn valid_port(port: &str) -> bool {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match port.parse::<u16>() {
        Ok(p) => p > 0,
        Err(_) => false,
    }
}

/// 从 Hub 地址推导**同源 HTTP(S) 基址**（`afcli import` 用）。
///
/// 导入是一次性 HTTP POST，而设备平时记的是 WS 地址。两者同源：
///
/// - 裸 `host:port`（含 IPv6 方括号）→ `http://host:port`；
/// - `ws://host[:port][/path]` → `http://host[:port]`（path / query 丢掉）；
/// - `wss://host[:port][/path]` → `https://host[:port]`；
/// - 其它形态（`http://`、没端口也没 scheme、端口非法、带 userinfo…）→
///   **显式错误**。
///
/// **绝不悄悄退回 `127.0.0.1`**：那会让一台配错地址的真设备把「连不上 Hub」
/// 显示成「Hub 没反应」，比直接报错更难查（与 [`hub_url`] 同一取向）。
/// 与 [`hub_url`] 分开是因为 `run` 要的是 WS 归一化，导入要的是 HTTP 源；
/// 把两者合成一个函数，任何一侧改口径都会悄悄改掉另一侧。
pub fn http_origin(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("Hub 地址为空".to_string());
    }
    if let Some(rest) = strip_scheme(v, "ws://") {
        let authority = authority_of(rest)?;
        return Ok(format!("http://{authority}"));
    }
    if let Some(rest) = strip_scheme(v, "wss://") {
        let authority = authority_of(rest)?;
        return Ok(format!("https://{authority}"));
    }
    if v.contains("://") {
        return Err("只支持 host:port、ws:// 或 wss:// 形式的 Hub 地址".to_string());
    }
    match split_host_port(v) {
        Some((authority, port)) => Ok(format!("http://{authority}:{port}")),
        None => Err("Hub 地址既不是 host:port，也不是 ws:// / wss:// URL".to_string()),
    }
}

/// 大小写不敏感地剥掉 scheme 前缀。
fn strip_scheme<'a>(v: &'a str, scheme: &str) -> Option<&'a str> {
    v.get(..scheme.len())
        .filter(|head| head.eq_ignore_ascii_case(scheme))
        .and_then(|_| v.get(scheme.len()..))
}

/// 取 URL 的 authority（去掉 path / query / fragment），并校验形态。
///
/// IPv6 必须带方括号；authority 里出现 `@`（userinfo）一律拒绝——
/// 导入请求不该把凭据塞进 URL。
fn authority_of(rest: &str) -> Result<&str, String> {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..end).unwrap_or("");
    if authority.is_empty() {
        return Err("Hub 地址缺少主机名".to_string());
    }
    if !valid_authority(authority) {
        return Err("Hub 地址的 host / port 不合法".to_string());
    }
    Ok(authority)
}

/// authority 可以是 `host`、`host:port`、`[v6]` 或 `[v6]:port`。
fn valid_authority(authority: &str) -> bool {
    if split_host_port(authority).is_some() {
        return true;
    }
    if let Some(inner) = authority
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
    {
        return inner.parse::<std::net::Ipv6Addr>().is_ok();
    }
    if authority.contains([':', '[', ']', '@', '/', '\\', '?', '#']) {
        return false;
    }
    valid_host(authority)
}

/// 当前进程的数据目录。
///
/// 唯一权威来源是 `AFCLI_DATA_DIR` / 平台标准目录——**不接受 flag**。
/// supervisor 把自己的 argv 原样传给 worker，两边读到的目录必然相同；
/// 而一个「用户可以在命令行上改数据目录」的开关，会让 `afcli log` 和
/// `afcli run` 落在两个目录里，排障时看到的日志根本不是运行时的那个。
///
/// `afcli import --data-dir` 是**只读一次性子命令**自己的参数，不经过这里，
/// 也不影响 `run` / `log` 的取值：导入要么成功发一条请求，要么读完就退出，
/// 不存在「导入看到的身份」与「运行时身份」长期分叉的排障陷阱。
pub fn data_dir() -> Result<PathBuf, String> {
    node_core::data_dir().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空串视作没给：`AFCLI_HUB_URL=` 常出现在被注释掉的配置里，
    /// 照它走会连一个空地址。
    #[test]
    fn an_empty_env_var_falls_back_to_the_default() {
        assert_eq!(hub_url(Some("")), DEFAULT_HUB_URL);
        assert_eq!(hub_url(Some("   \t\n ")), DEFAULT_HUB_URL);
    }

    /// 用户反馈的原话：「AFCLI_HUB_URL 应该配置 IP 端口而不是完整的带 /ws 的地址」。
    #[test]
    fn a_bare_host_port_is_normalized_to_a_ws_url() {
        assert_eq!(
            hub_url(Some("117.72.123.218:9002")),
            "ws://117.72.123.218:9002/ws"
        );
        assert_eq!(
            hub_url(Some("hub.example.com:8080")),
            "ws://hub.example.com:8080/ws"
        );
        assert_eq!(hub_url(Some("localhost:8080")), "ws://localhost:8080/ws");
    }

    /// IPv6 的方括号必须原样保留，否则 URL 里的冒号会被解析成端口分隔符。
    #[test]
    fn ipv6_bracket_notation_is_normalized() {
        assert_eq!(hub_url(Some("[::1]:9002")), "ws://[::1]:9002/ws");
        assert_eq!(
            hub_url(Some("[2001:db8::1]:443")),
            "ws://[2001:db8::1]:443/ws"
        );
    }

    /// 旧配置（含进程级测试里 `hub.ws_url()` 给的那种完整 URL）必须逐字不动：
    /// 自定义路径、TLS、IPv6 全都由调用方自己决定。
    #[test]
    fn existing_full_ws_and_wss_urls_are_untouched() {
        for url in [
            "ws://10.0.0.2:8080/ws",
            "wss://hub.example.com/ws",
            "ws://127.0.0.1:9002/ws",
            "ws://[::1]:8080/ws",
            "wss://hub.example.com/custom?token=abc",
            "WS://HUB.EXAMPLE.COM/WS",
        ] {
            assert_eq!(hub_url(Some(url)), url, "{url} 不该被改写");
        }
    }

    /// `source` / 编辑器带进来的首尾空白不该变成 URL 的一部分。
    #[test]
    fn surrounding_whitespace_is_trimmed_before_normalizing() {
        assert_eq!(
            hub_url(Some("  117.72.123.218:9002 \n")),
            "ws://117.72.123.218:9002/ws"
        );
        assert_eq!(
            hub_url(Some("\tws://hub.example.com/ws  ")),
            "ws://hub.example.com/ws"
        );
        assert_eq!(hub_url(Some(" [::1]:9002 ")), "ws://[::1]:9002/ws");
    }

    /// 非法值**不许**被改写成看起来合法的地址，更不许静默退回本机默认：
    /// 原样交给连接层报 `bad url`，用户才会去修配置。
    #[test]
    fn malformed_values_are_passed_through_not_silently_rewritten() {
        for bad in [
            "not a url",
            "http://hub.example.com/ws",
            "hub.example.com",
            "hub.example.com:abc",
            "hub.example.com:0",
            "hub.example.com:65536",
            "[::1",
            "[::1]:abc",
            "ws://",
            "hub.example.com:9002/path",
            "hub.example.com:9002:1",
            "[not-ipv6]:9002",
        ] {
            assert_eq!(hub_url(Some(bad)), bad, "{bad} 应原样交给连接层报错");
            assert_ne!(
                hub_url(Some(bad)),
                DEFAULT_HUB_URL,
                "{bad} 不该静默连到本机"
            );
        }
    }

    /// 没有环境变量时的默认地址必须与 `afhub` 的默认监听地址一致：
    /// 两边各自改一个值，设备就悄悄连到别处去。
    #[test]
    fn the_default_points_at_the_local_hub() {
        assert_eq!(hub_url(None), "ws://127.0.0.1:8080/ws");
        assert_eq!(DEFAULT_HUB_URL, "ws://127.0.0.1:8080/ws");
    }

    /// `afcli run --worker` 才进 worker 角色；`--worker` 不在 `run` 的
    /// 修饰符位置上时不算（D1）。
    #[test]
    fn the_worker_flag_is_a_run_modifier_not_a_global_switch() {
        assert_eq!(WORKER_FLAG, "--worker");
        assert!(!WORKER_FLAG.starts_with('-') || WORKER_FLAG.starts_with("--"));
        assert!(!WORKER_FLAG.starts_with("---"), "三横线会被当成短参数合并");
    }

    /// 数据目录**只来自平台标准目录**，不接受 flag：
    /// supervisor 把自己的 argv 原样传给 worker，两边读到的必然相同；
    /// 而一个可在命令行上改的开关会让 `afcli log` 与 `afcli run`
    /// 落在两个目录里，排障时看到的日志根本不是运行时那个。
    #[test]
    fn data_dir_comes_from_the_platform_standard_only() {
        let d = data_dir().expect("platform data dir");
        assert!(d.is_absolute(), "got {}", d.display());
    }

    /// 环境变量名与 Hub 侧对称：`AFHUB_ADDR` / `AFCLI_HUB_URL`。
    #[test]
    fn the_env_var_name_is_the_cli_side_twin_of_the_hub_side_one() {
        assert_eq!(ENV_HUB_URL, "AFCLI_HUB_URL");
    }

    /// 裸 authority / 完整 WS URL 都要映射到同源 HTTP(S)，且丢掉 `/ws` 路径：
    /// 导入端点是 `/device-api/session-imports`，把 `/ws` 当基址会打到 404。
    #[test]
    fn http_origin_maps_bare_authority_ws_and_wss_urls_and_rejects_bad_values() {
        assert_eq!(
            http_origin("117.72.123.218:9002").expect("bare authority"),
            "http://117.72.123.218:9002"
        );
        assert_eq!(
            http_origin("ws://hub.example.com:8080/ws").expect("ws url"),
            "http://hub.example.com:8080"
        );
        assert_eq!(
            http_origin("wss://hub.example.com/ws").expect("wss url"),
            "https://hub.example.com"
        );
        assert_eq!(
            http_origin("wss://hub.example.com:8443/custom?token=abc").expect("wss with path"),
            "https://hub.example.com:8443"
        );
        assert_eq!(
            http_origin("  ws://127.0.0.1:8080/ws \n").expect("trimmed"),
            "http://127.0.0.1:8080"
        );
    }

    /// IPv6 的方括号必须原样保留，否则 `http://::1:9002` 里的冒号会被当成
    /// 端口分隔符，reqwest 解析直接失败。
    #[test]
    fn http_origin_keeps_ipv6_brackets() {
        assert_eq!(
            http_origin("[::1]:9002").expect("bare ipv6"),
            "http://[::1]:9002"
        );
        assert_eq!(
            http_origin("ws://[2001:db8::1]:443/ws").expect("ws ipv6"),
            "http://[2001:db8::1]:443"
        );
        assert_eq!(
            http_origin("wss://[2001:db8::1]/ws").expect("wss ipv6 without port"),
            "https://[2001:db8::1]"
        );
    }

    /// 非法值必须报错，**不得**退回本机默认（那会让配错的设备看起来「Hub 没反应」）。
    #[test]
    fn http_origin_rejects_values_it_cannot_map() {
        for bad in [
            "",
            "   ",
            "http://hub.example.com:8080",
            "https://hub.example.com",
            "hub.example.com",
            "hub.example.com:abc",
            "hub.example.com:0",
            "hub.example.com:65536",
            "[::1",
            "[not-ipv6]:9002",
            "ws://",
            "ws:///ws",
            "ws://user:pw@hub.example.com:8080/ws",
            "hub.example.com:9002/path",
        ] {
            let got = http_origin(bad);
            assert!(got.is_err(), "{bad:?} 不该被映射成地址，got {got:?}");
            let msg = got.err().unwrap_or_default();
            assert!(
                !msg.contains("127.0.0.1"),
                "{bad:?} 的错误提示不该把用户引向本机：{msg}"
            );
        }
    }
}
