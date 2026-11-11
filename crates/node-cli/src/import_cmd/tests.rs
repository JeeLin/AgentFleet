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
    assert_eq!(data_base(None, injected), Path::new("/home/u/.local/share"));
    assert_eq!(
        identity_file(&data_base(None, injected)),
        Path::new("/home/u/.local/share/agentfleet/identity.json")
    );
    assert_eq!(
        data_base(Some(Path::new("/tmp/xdg")), injected),
        Path::new("/tmp/xdg")
    );
    assert_eq!(
        identity_file(&data_base(Some(Path::new("/tmp/xdg")), injected)),
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

/// `resolve` 判了非法路径、但同一行的精确读取同时失败时，失败原因必须是
/// `provider_unavailable`——不是把读错误 `.ok()` 吞掉后误报 `invalid_path`。
#[test]
fn a_provider_read_error_is_provider_unavailable_not_invalid_path() {
    struct BrokenMeta;
    impl import::HistoryProvider for BrokenMeta {
        fn id(&self) -> &'static str {
            "opencode"
        }
        fn list(&self, _: &Path) -> Result<Vec<HistoricalSession>, ImportError> {
            Err(ImportError::DbMissing)
        }
        fn resolve(&self, _: &Path, _: &str) -> Result<import::ImportCandidate, ImportError> {
            Err(ImportError::InvalidWorkspacePath)
        }
        fn session_meta(
            &self,
            _: &Path,
            _: &str,
        ) -> Result<Option<HistoricalSession>, ImportError> {
            // 模拟「resolve 之后 DB 被删 / 库结构漂移」：读错误不得被吞。
            Err(ImportError::SchemaMismatch)
        }
    }

    let meta = bind_metadata(&BrokenMeta, Path::new("/tmp/xdg"), "ses_x");
    let failure = invalid_path_failure(meta);
    assert_eq!(failure.code(), "provider_unavailable");
    assert_ne!(failure.code(), "invalid_path");
}
