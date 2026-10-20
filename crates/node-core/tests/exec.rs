//! exec connector 的**接线**测试：帧进 → 真 fork/exec → 一帧出。
//!
//! `connector/exec.rs` 里的单元测试直接打 `Exec`。这里走的是 Hub 实际会
//! 走的那条路：`open_channel` 一到就**执行**（一次性命令没有「先开再发」
//! 两步），结果随 `channel_ready` 之后的那一帧 `channel_data` 回来。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use base64ct::{Base64, Encoding};
use node_core::channel;
use node_core::connector::names;
use protocol::channel::OpenChannel;
use protocol::frame::Frame;
use protocol::message::types;

/// 通道表是**进程级单例**（见 `tests/terminal.rs` 的同一条理由）。
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_exec_frame(channel_id: &str, root: &std::path::Path, params: serde_json::Value) -> Frame {
    let mut merged = serde_json::Map::new();
    merged.insert(
        "workspace_root".to_string(),
        serde_json::Value::String(root.to_string_lossy().into_owned()),
    );
    if let Some(obj) = params.as_object() {
        for (k, v) in obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    let spec = OpenChannel {
        channel_id: channel_id.to_string(),
        connector: names::EXEC.to_string(),
        params: serde_json::Value::Object(merged),
        credit_start: 32,
    };
    Frame::push(
        types::OPEN_CHANNEL,
        1,
        serde_json::to_value(&spec).expect("open spec"),
    )
}

fn is_error(frame: &Frame) -> bool {
    frame.msg_type == types::CHANNEL_ERROR
}

fn workspace() -> tempfile::TempDir {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::write(base.path().join("in-ws.txt"), b"inside\n").expect("write");
    base
}

/// 一次执行：**第一帧是 `channel_ready`，第二帧是 `exec_result`**。
/// 顺序本身就是判据——结果必须落在 ready 之后。
fn run(channel_id: &str, root: &std::path::Path, params: serde_json::Value) -> Vec<Frame> {
    let action = channel::on_channel_frame(&open_exec_frame(channel_id, root, params));
    let frames = action.frames().to_vec();
    assert_eq!(
        frames.first().map(|f| f.msg_type.as_str()),
        Some(types::CHANNEL_READY),
        "第一帧必须是 channel_ready：{:?}",
        frames.iter().map(|f| &f.msg_type).collect::<Vec<_>>()
    );
    assert_eq!(frames.len(), 2, "一次性命令只产出一帧结果：{frames:?}");
    assert!(!is_error(&frames[1]), "不该是错误：{:?}", frames[1]);
    frames
}

/// 取 `channel_data` 帧的业务载荷。
///
/// 帧的 `c` 是 `ChannelData{channel_id, payload}` 信封，不是裸载荷——
/// **这里逐帧断言 `channel_id` 确实在信封里**：漏了它 Hub 会把帧当成
/// 无法解释的 JSON 静默丢掉（页面一片空白，`/send` 却回 204）。
fn payload(frames: &[Frame]) -> serde_json::Value {
    let env = &frames[1].c;
    assert!(
        env["channel_id"].is_string(),
        "channel_data 的 c 必须带 channel_id 信封：{env}"
    );
    env["payload"].clone()
}

fn decode(field: &serde_json::Value) -> Vec<u8> {
    Base64::decode_vec(field.as_str().expect("b64 field")).expect("base64")
}

/// stdout / stderr / 退出码分别正确，而且 stdout 与 stderr **没有被合流**。
#[test]
fn stdout_stderr_and_the_exit_code_all_come_back_as_one_frame() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    let frames = run(
        "ch-exec",
        ws.path(),
        serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", "printf out-marker; printf err-marker >&2; exit 3"],
        }),
    );
    let p = payload(&frames);
    assert_eq!(p["exit_code"], 3, "{p}");
    assert_eq!(decode(&p["stdout_b64"]), b"out-marker");
    assert_eq!(decode(&p["stderr_b64"]), b"err-marker");
    assert!(!p["truncated"].as_bool().expect("truncated"));
    assert!(!p["timed_out"].as_bool().expect("timed_out"));
    channel::on_disconnect();
}

/// 超时被杀掉并回报，通道**没有被永久占住**。
#[test]
fn a_command_that_never_finishes_is_killed_and_reported() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    let started = std::time::Instant::now();
    let frames = run(
        "ch-timeout",
        ws.path(),
        serde_json::json!({ "command": "/bin/sleep", "args": ["60"], "timeout_ms": 250 }),
    );
    let elapsed = started.elapsed();
    let p = payload(&frames);
    assert!(p["timed_out"].as_bool().expect("timed_out"), "{p}");
    assert_eq!(p["exit_code"], 128 + 9, "SIGKILL 要报成 128+9：{p}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "超时没有真的杀掉子进程：跑了 {elapsed:?}"
    );
    // 通道确实关得掉——一个「杀不掉」的 exec 会让这台设备上每一条
    // 同名通道都再也开不了。
    assert_eq!(channel::on_disconnect(), 1, "超时的通道仍然要被释放");
}

/// 输出超限被截断，`truncated` 为真。
#[test]
fn output_beyond_the_limit_comes_back_truncated() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    let frames = run(
        "ch-trunc",
        ws.path(),
        serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", "i=0; while [ $i -lt 5000 ]; do printf '0123456789'; i=$((i+1)); done"],
            "max_output_bytes": 64,
        }),
    );
    let p = payload(&frames);
    assert!(p["truncated"].as_bool().expect("truncated"), "{p}");
    assert_eq!(decode(&p["stdout_b64"]).len(), 64, "{p}");
    channel::on_disconnect();
}

/// 工作区外的 cwd 被拒——**边界强制在 node 侧**（约束 3：Hub 不信任
/// 页面的路径参数）。
#[test]
fn a_cwd_outside_the_workspace_is_refused() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    for asked in ["..", "/etc", "../../etc"] {
        let action = channel::on_channel_frame(&open_exec_frame(
            "ch-esc",
            ws.path(),
            serde_json::json!({
                "command": "/bin/pwd",
                "cwd": asked,
            }),
        ));
        let frames = action.frames();
        assert_eq!(frames.len(), 1, "{asked}: {frames:?}");
        assert!(is_error(&frames[0]), "{asked} 必须被拒：{:?}", frames[0]);
        let reason = frames[0].c["reason"].as_str().expect("reason").to_string();
        assert!(
            reason.contains("outside the workspace"),
            "{asked}: {reason}"
        );
        assert!(
            !channel::has_channel("ch-esc"),
            "{asked}: 开失败的通道不进表"
        );
    }
}

/// 缺工作区根：`open_channel` 就失败，不开出一个「在任意目录里跑命令」
/// 的通道——那等于把整台设备交出去。
#[test]
fn opening_exec_without_a_workspace_root_fails_the_open() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let spec = OpenChannel {
        channel_id: "ch-noroot".to_string(),
        connector: names::EXEC.to_string(),
        params: serde_json::json!({ "command": "/bin/echo", "args": ["hi"] }),
        credit_start: 32,
    };
    let frame = Frame::push(
        types::OPEN_CHANNEL,
        1,
        serde_json::to_value(&spec).expect("open spec"),
    );
    let action = channel::on_channel_frame(&frame);
    assert!(is_error(&action.frames()[0]), "{:?}", action.frames());
    assert!(!channel::has_channel("ch-noroot"));
}

/// 一次性通道**不接受第二帧**。这是「exec 只面向页面与 Hub」的形状：
/// 连重跑都要新开通道，于是每一次执行都必带一份自己的超时与输出上限。
#[test]
fn a_spent_exec_channel_refuses_a_second_command() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    run(
        "ch-once",
        ws.path(),
        serde_json::json!({ "command": "/bin/true" }),
    );

    let spec = protocol::channel::ChannelData {
        channel_id: "ch-once".to_string(),
        payload: serde_json::json!({ "command": "/bin/false" }),
    };
    let frame = Frame::push(
        types::CHANNEL_DATA,
        2,
        serde_json::to_value(&spec).expect("data spec"),
    );
    let action = channel::on_channel_frame(&frame);
    assert!(is_error(&action.frames()[0]), "{:?}", action.frames());
    let reason = action.frames()[0].c["reason"].as_str().expect("reason");
    assert!(reason.contains("once per channel"), "{reason}");
    channel::on_disconnect();
}

/// exec **不向 agent 暴露**（约束 3）：node 侧不提供任何「枚举 / 调用
/// connector」的入口。这条钉住的是「注册表只按名分派，能力表从它导出」
/// 这条链不被一个便利接口破掉。
#[test]
fn there_is_no_way_to_enumerate_or_invoke_connectors_for_a_local_agent() {
    // node-core 的公开面里**没有**任何形如 `call_connector` / `list_connectors`
    // 的自由函数可供 agent 使用；connector 只能经 `create(name)` 拿实例，
    // 而那个函数是 Hub 下发 `open_channel` 时走的路。
    assert_eq!(node_core::connector::registered_count(), 3);
    assert!(node_core::connector::create("agent-bridge").is_none());
}
