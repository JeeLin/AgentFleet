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
/// 形态与 Hub 侧对称：`afhub` 读 `AFHUB_ADDR`，node 读 `AFCLI_HUB_URL`
/// （`afcli run` 的 URL）。**设备侧没有任何配置文件**——接入就是一条命令，
/// 改地址用环境变量就够了（D1）。
pub const ENV_HUB_URL: &str = "AFCLI_HUB_URL";

/// 缺省 Hub 地址：与 `afhub` 的默认监听地址一致（`127.0.0.1:8080/ws`）。
///
/// 只对「Hub 跑在同一台机器上」成立；真正的设备用 `AFCLI_HUB_URL` 指向
/// Hub 的回环端口转发或 Tailscale 地址。
pub const DEFAULT_HUB_URL: &str = "ws://127.0.0.1:8080/ws";

/// 解析 Hub 地址：环境变量优先（空串视作没给），否则用缺省值。
pub fn hub_url(env: Option<&str>) -> String {
    match env {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => DEFAULT_HUB_URL.to_string(),
    }
}

/// 当前进程的数据目录。
///
/// 唯一权威来源是 `XDG_DATA_HOME` / 平台标准目录——**不接受 flag**。
/// supervisor 把自己的 argv 原样传给 worker，两边读到的目录必然相同；
/// 而一个「用户可以在命令行上改数据目录」的开关，会让 `afcli log` 和
/// `afcli run` 落在两个目录里，排障时看到的日志根本不是运行时的那个。
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
    }

    #[test]
    fn the_env_var_wins_when_set() {
        assert_eq!(
            hub_url(Some("ws://10.0.0.2:8080/ws")),
            "ws://10.0.0.2:8080/ws"
        );
    }

    /// 没有环境变量时的默认地址必须与 `afhub` 的默认监听地址一致：
    /// 两边各自改一个值，设备就悄悄连到别处去。
    #[test]
    fn the_default_points_at_the_local_hub() {
        assert_eq!(hub_url(None), "ws://127.0.0.1:8080/ws");
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
}
