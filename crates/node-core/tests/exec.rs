//! exec connector 的**接线**测试：帧进 → 真 fork/exec → 一帧出。
//!
//! `connector/exec.rs` 里的单元测试直接打 `Exec`。这里走的是 Hub 实际会
//! 走的那条路：`open_channel` 一到就**执行**（一次性命令没有「先开再发」
//! 两步），结果随 `channel_ready` 之后的那一帧 `channel_data` 回来。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use base64ct::{Base64, Encoding};
use node_core::channel::{self, Action};
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

/// `Action` 里的帧。
///
/// **不提供 `.frames()` 便捷方法**：它会把「没有应答」（`Action::None`）
/// 与「连接该结束了」（`Action::End`）都变成一个空切片。
fn frames_of(action: &Action) -> &[Frame] {
    match action {
        Action::Send(frames) => frames,
        other => panic!("expected frames, got {other:?}"),
    }
}

fn workspace() -> tempfile::TempDir {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::write(base.path().join("in-ws.txt"), b"inside\n").expect("write");
    base
}

/// 一次执行：**第一帧是 `channel_ready`，第二帧是 `channel_data`（结果），
/// 第三帧是 `close_channel`**。
///
/// 顺序本身就是判据——结果必须落在 ready 之后，而「这条通道结束了」必须
/// 落在结果之后（页面先看到结果，才知道通道为什么结束）。
///
/// **这些帧不是一次 `on_channel_frame` 交出来的**：命令跑在专用线程上
/// （见 `connector/exec.rs` 的模块注释），`open_channel` 只回 `channel_ready`，
/// 结果与关闭由 `channel::pump` 在后续轮次交出。所以这里要把它泵干——
/// 判据没变，变的是「谁在第几轮说」。
fn run(channel_id: &str, root: &std::path::Path, params: serde_json::Value) -> Vec<Frame> {
    let action = channel::on_channel_frame(&open_exec_frame(channel_id, root, params));
    let mut frames = frames_of(&action).to_vec();
    assert_eq!(
        frames.first().map(|f| f.msg_type.as_str()),
        Some(types::CHANNEL_READY),
        "第一帧必须是 channel_ready：{:?}",
        frames.iter().map(|f| &f.msg_type).collect::<Vec<_>>()
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !frames.iter().any(|f| f.msg_type == types::CLOSE_CHANNEL) {
        assert!(
            std::time::Instant::now() < deadline,
            "60 秒都没等到 close_channel：{:?}",
            frames.iter().map(|f| &f.msg_type).collect::<Vec<_>>()
        );
        for (id, produced) in channel::pump() {
            if id == channel_id {
                frames.extend(produced);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert_eq!(
        frames.len(),
        3,
        "一次性命令只产出一帧结果加一帧关闭：{frames:?}"
    );
    assert!(!is_error(&frames[1]), "不该是错误：{:?}", frames[1]);
    assert_eq!(
        frames[2].msg_type,
        types::CLOSE_CHANNEL,
        "跑完的通道必须自己关掉：{frames:?}"
    );
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
    // **通道自己关掉了**：一次性命令跑完就是结束了，node 侧据此发
    // `close_channel` 并释放资源——不指望页面记得关通道（非页面调用方
    // 根本不会去关）。所以断连时**没有**活通道要释放。
    assert_eq!(
        channel::live_count(),
        0,
        "跑完的 exec 通道必须自己释放：{frames:?}"
    );
    assert_eq!(channel::on_disconnect(), 0, "没有活通道可释放");
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
        let frames = frames_of(&action);
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
    let frames = frames_of(&action);
    assert!(is_error(&frames[0]), "{frames:?}");
    assert!(!channel::has_channel("ch-noroot"));
}

/// 一次性通道**不接受第二帧**。这是「exec 只面向页面与 Hub」的形状：
/// 连重跑都要新开通道，于是每一次执行都必带一份自己的超时与输出上限。
///
/// **通道层上是静默的**：exec 跑完就把这条通道关了，页面已经收到
/// `close_channel`；再往一条已关的通道发数据，与「往一条不存在的通道
/// 发数据」同属一类——回错反而会让 Hub 以为这条通道死了。
#[test]
fn a_spent_exec_channel_ignores_a_second_command_in_silence() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    run(
        "ch-once",
        ws.path(),
        serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", "touch first.txt"],
            "cwd": ".",
        }),
    );
    assert!(
        ws.path().join("first.txt").exists(),
        "第一条命令必须真的跑了——否则「第二条没跑」什么也证明不了"
    );

    let spec = protocol::channel::ChannelData {
        channel_id: "ch-once".to_string(),
        payload: serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", "touch second.txt"],
            "cwd": ".",
        }),
    };
    let frame = Frame::push(
        types::CHANNEL_DATA,
        2,
        serde_json::to_value(&spec).expect("data spec"),
    );
    let action = channel::on_channel_frame(&frame);
    assert!(
        matches!(action, Action::None),
        "已关的通道不该有应答：{action:?}"
    );
    assert!(
        !ws.path().join("second.txt").exists(),
        "第二条命令不该被执行——exec 只跑一次"
    );
    channel::on_disconnect();
}

/// **一次性命令跑完，node 侧自己把通道关掉**。
///
/// 判据是「**恰好一帧** `close_channel`」+「活通道数归零」两件事：
/// - 「恰好一帧」：重复的关闭帧会让 Hub 那边把一条通道关两次；
/// - 「活通道数归零」：资源没释放就是一台设备上没人读、也拿不到信用的
///   僵尸通道，而 Hub 自己开的通道根本不会去关它。
///
/// 规格说「exec 只跑一次」，但**没人记得关通道**——所以「跑完就地收尾」
/// 必须是通道层的行为，不是页面的义务。
#[test]
fn an_exec_channel_closes_itself_once_when_the_command_is_done() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    let frames = run(
        "ch-selfclose",
        ws.path(),
        serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", "printf done"],
        }),
    );
    let closes: Vec<&Frame> = frames
        .iter()
        .filter(|f| f.msg_type == types::CLOSE_CHANNEL)
        .collect();
    assert_eq!(closes.len(), 1, "关闭帧只能有一帧：{frames:?}");
    assert_eq!(
        closes[0].c["channel_id"], "ch-selfclose",
        "关的必须是这条通道：{frames:?}"
    );
    assert_eq!(
        channel::live_count(),
        0,
        "跑完的通道必须自己释放：{frames:?}"
    );
    assert_eq!(channel::on_disconnect(), 0, "没有活通道可释放");
}

/// exec **不向 agent 暴露**（约束 3）：node 侧不提供任何「枚举 / 调用
/// connector」的入口。这条钉住的是「注册表只按名分派，能力表从它导出」
/// 这条链不被一个便利接口破掉。
#[test]
fn there_is_no_way_to_enumerate_or_invoke_connectors_for_a_local_agent() {
    // node-core 的公开面里**没有**任何形如 `call_connector` / `list_connectors`
    // 的自由函数可供 agent 使用；connector 只能经 `create(name)` 拿实例，
    // 而那个函数是 Hub 下发 `open_channel` 时走的路。
    // **不写死数字**：写死 3 时，加第四个 connector（v0.4.0 的 `git`）会让这条
    // 测试红，而它红的原因与「有没有给 agent 一条枚举入口」毫无关系——于是修它
    // 的人只会把 3 改成 4，判据本身一次都没被验过。真正的判据是「注册表就是
    // `names::ALL` 那一批」，所以与它比。
    assert_eq!(
        node_core::connector::registered_count(),
        node_core::connector::names::ALL.len()
    );
    assert!(node_core::connector::create("agent-bridge").is_none());
}

/// **`open_channel` 不等命令跑完**：一条 `sleep 3` 的开通道必须毫秒级返回。
///
/// 这条锁的是 worker 的心跳不被堵住。`on_channel_frame` 是在 worker 的
/// `select!` 里被调的（`next_text` 臂），它一旦同步等 IO，心跳那条臂就
/// 一起停摆——而 Hub 判离站靠的正是收不到心跳。于是「页面上跑一条
/// `sleep 60`」会让设备在 Hub 眼里掉线，本地却什么错都不报。
///
/// 旧实现在 `open` 里同步跑完命令，默认超时 30 秒、`timeout_ms = 0`
/// 更是无限。判据取 3 秒而不是 60 秒：够长到「同步执行一定来不及」，
/// 又不至于让这条用例真的等满一个超时。
///
/// **顺带断言通道当时还活着**：开跑的那一瞬通道必须在表里，否则
/// 「返回得快」只是因为什么都没建。
#[test]
fn opening_a_slow_channel_returns_before_the_command_finishes() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let ws = workspace();
    let started = std::time::Instant::now();
    let action = channel::on_channel_frame(&open_exec_frame(
        "ch-slow",
        ws.path(),
        serde_json::json!({
            "command": "/bin/sh",
            "args": ["-c", "sleep 3; printf done"],
            "timeout_ms": 0,
        }),
    ));
    let elapsed = started.elapsed();
    let frames = frames_of(&action);
    assert_eq!(
        frames.first().map(|f| f.msg_type.as_str()),
        Some(types::CHANNEL_READY),
        "起跑即就绪：{frames:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "开通道花了 {elapsed:?}——命令是在 open 里同步跑的"
    );
    assert!(
        channel::has_channel("ch-slow"),
        "起跑后通道必须还在表里：{frames:?}"
    );
    assert_eq!(
        channel::live_count(),
        1,
        "起跑后这条通道是活的（还没跑完，不能自称结束）"
    );

    // 命令仍在跑：此刻泵一轮**什么也拿不到**。这不是「没实现」，而是
    // `poll` 的契约——空队列就是「还在跑」的唯一说法。
    assert!(
        channel::pump().iter().all(|(id, _)| id != "ch-slow"),
        "命令还没跑完，pump 不该为它产出任何帧"
    );
    channel::on_disconnect();
}
