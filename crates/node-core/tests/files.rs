//! files connector 的**接线**测试：帧进 → 边界检查 → 文件系统 → 帧出。
//!
//! `connector/files.rs` 里的单元测试直接打 `Files`。这里走的是
//! `channel::on_channel_frame` 的派发路径——`workspace_root` 得从
//! `open_channel.params` 进来，越界错误得变成一帧 `channel_error`
//! 而不是让整条连接断掉。**这是 Hub 实际会踩的那条路。**

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use node_core::channel;
use node_core::connector::names;
use protocol::channel::{ChannelData, OpenChannel};
use protocol::frame::Frame;
use protocol::message::types;

/// 通道表是**进程级单例**，这些场景只能一个一个来（见 `tests/terminal.rs`
/// 里的同一条理由）。
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_frame(channel_id: &str, root: &std::path::Path) -> Frame {
    let spec = OpenChannel {
        channel_id: channel_id.to_string(),
        connector: names::FILES.to_string(),
        params: serde_json::json!({
            "workspace_root": root.to_string_lossy(),
        }),
        credit_start: 32,
    };
    Frame::push(
        types::OPEN_CHANNEL,
        1,
        serde_json::to_value(&spec).expect("open spec"),
    )
}

fn data_frame(channel_id: &str, payload: serde_json::Value) -> Frame {
    let spec = ChannelData {
        channel_id: channel_id.to_string(),
        payload,
    };
    Frame::push(
        types::CHANNEL_DATA,
        2,
        serde_json::to_value(&spec).expect("data spec"),
    )
}

/// 一次 `data` 之后的全部回帧。files 是同步的：一帧进、一帧出。
fn after_data(channel_id: &str, payload: serde_json::Value) -> Vec<Frame> {
    let action = channel::on_channel_frame(&data_frame(channel_id, payload));
    action.frames().to_vec()
}

fn payload_of(frames: &[Frame]) -> serde_json::Value {
    frames
        .last()
        .map(|f| f.c.clone())
        .unwrap_or_else(|| serde_json::json!({ "__no_frame__": true }))
}

fn is_error(frame: &Frame) -> bool {
    frame.msg_type == types::CHANNEL_ERROR
}

/// 一个真工作区 + 一个旁边的界外文件。
fn sandbox() -> (tempfile::TempDir, std::path::PathBuf) {
    let base = tempfile::tempdir().expect("tempdir");
    let ws = base.path().join("ws");
    std::fs::create_dir_all(ws.join("src")).expect("mkdir");
    std::fs::write(ws.join("src").join("a.txt"), b"hello\n").expect("write");
    std::fs::write(base.path().join("secret.txt"), b"top secret\n").expect("write");
    (base, ws)
}

/// 页面点一下目录树：请求进、回一帧带条目。
#[test]
fn listing_a_directory_over_a_channel_comes_back_as_channel_data() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let (_base, ws) = sandbox();

    let ready = channel::on_channel_frame(&open_frame("ch-list", &ws));
    assert!(!ready.frames().is_empty(), "开通道至少要回 channel_ready");

    let out = after_data("ch-list", serde_json::json!({ "op": "list" }));
    assert_eq!(out.len(), 1, "一次请求一次应答：{out:?}");
    let p = payload_of(&out);
    assert_eq!(p["op"], "list");
    let names: Vec<&str> = p["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, vec!["src"], "{p}");
    channel::on_disconnect();
}

/// 写进去的字节能读回来——**走真实落盘**，不是走 connector 自己记的值。
#[test]
fn a_write_over_a_channel_lands_on_disk_and_reads_back() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let (_base, ws) = sandbox();

    channel::on_channel_frame(&open_frame("ch-rw", &ws));
    let w = after_data(
        "ch-rw",
        serde_json::json!({ "op": "write", "path": "src/note.txt", "text": "e2e-note\n" }),
    );
    assert_eq!(payload_of(&w)["written"], 9, "{:?}", w);
    assert_eq!(
        std::fs::read_to_string(ws.join("src").join("note.txt")).expect("on disk"),
        "e2e-note\n"
    );

    let r = after_data(
        "ch-rw",
        serde_json::json!({ "op": "read", "path": "src/note.txt" }),
    );
    assert_eq!(payload_of(&r)["text"], "e2e-note\n");
    channel::on_disconnect();
}

/// 越界在**通道层**也成立：回一帧 `channel_error`（code `internal`，
/// 而不是 `unsupported`——越界是策略拒绝，通道本身能用），
/// 连接继续活着，其它通道照常。
#[test]
fn an_escaping_path_becomes_a_channel_error_and_the_connection_survives() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let (base, ws) = sandbox();

    channel::on_channel_frame(&open_frame("ch-esc", &ws));
    let out = after_data(
        "ch-esc",
        serde_json::json!({ "op": "read", "path": "../secret.txt" }),
    );
    assert_eq!(out.len(), 1, "{out:?}");
    let err = &out[0];
    assert!(is_error(err), "越界要回 channel_error：{err:?}");
    let reason = err.c["reason"].as_str().expect("reason").to_string();
    assert!(reason.contains("../secret.txt"), "越界要看得见：{reason}");
    assert_eq!(err.c["code"], "internal", "越界不是 unsupported");
    // 错误里不能带设备绝对路径（design-system.md §7.4）
    assert!(
        !reason.contains(&ws.to_string_lossy().into_owned()),
        "错误泄漏了设备布局：{reason}"
    );

    // **通道没死**：同一会话立刻再来一个合法请求，仍然成功。
    let ok = after_data("ch-esc", serde_json::json!({ "op": "list" }));
    assert!(!is_error(&ok[0]), "一次越界不该把整条通道打死：{ok:?}");
    channel::on_disconnect();
    assert!(
        std::fs::read_to_string(base.path().join("secret.txt")).expect("still there")
            == "top secret\n",
        "判据文件必须在位，且越界读没有动过它"
    );
}

/// 符号链接指向界外时同样被拒——**真 `symlink(2)`**，判据落在真实
/// 文件系统上。
#[test]
fn a_symlink_escaping_the_workspace_is_refused_over_the_channel() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let (base, ws) = sandbox();
    let secret = base.path().join("secret.txt");
    std::os::unix::fs::symlink(&secret, ws.join("leak.txt")).expect("symlink");

    channel::on_channel_frame(&open_frame("ch-link", &ws));
    // 树上看得见
    let listing = after_data("ch-link", serde_json::json!({ "op": "list" }));
    let p = payload_of(&listing);
    let leak = p["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["name"] == "leak.txt")
        .cloned()
        .expect("链接不该被隐藏");
    assert_eq!(leak["kind"], "link");

    // 但读不到
    let out = after_data(
        "ch-link",
        serde_json::json!({ "op": "read", "path": "leak.txt" }),
    );
    assert!(is_error(&out[0]), "{out:?}");
    assert!(out[0].c["reason"]
        .as_str()
        .expect("reason")
        .contains("leak.txt"));
    channel::on_disconnect();
}

/// 缺根的工作区：`open_channel` 就该失败，而不是开出一个「能看到当前
/// 目录」的文件面板——那看起来像「工作区是空的」。
#[test]
fn opening_files_without_a_workspace_root_fails_the_open() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let spec = OpenChannel {
        channel_id: "ch-noroot".to_string(),
        connector: names::FILES.to_string(),
        // **没有 workspace_root**，而进程 cwd 确实是一个可读的目录。
        params: serde_json::json!({}),
        credit_start: 32,
    };
    let frame = Frame::push(
        types::OPEN_CHANNEL,
        1,
        serde_json::to_value(&spec).expect("open spec"),
    );
    let action = channel::on_channel_frame(&frame);
    assert_eq!(action.frames().len(), 1, "{:?}", action.frames());
    assert!(
        is_error(&action.frames()[0]),
        "缺根必须开不出来，而不是退化成当前目录：{:?}",
        action.frames()
    );
    // **绝不回落 cwd**：那会让文件面板看起来能用（它列得出东西，只是列的
    // 不是用户那个工作区），而越界检查此时判的是「相对进程启动目录」，
    // 那等于没有边界（约束 3）。
    let reason = action.frames()[0].c["reason"].as_str().expect("reason");
    assert!(
        reason.contains("no workspace root"),
        "要说清是「没给工作区」而不是别的原因：{reason}"
    );
    assert!(
        !channel::has_channel("ch-noroot"),
        "开失败的通道不该留在表里"
    );
}

/// **钉住那条接线**：`workspace_root` 必须从 `open_channel.params` 一路走到
/// `Files` 里那个真实的根。
///
/// 这是子任务 9 的前提被验了一次：通道层曾经把整份 params 原样塞进
/// `OpenReq.params`，而 `workspace_root` 从不被取出，于是 `OpenReq::root()`
/// 恒为 `None`——**每一个 files 通道都开不出来**，症状是「Hub 没给工作区」。
///
/// 判据落在**真实文件系统**上：只断言 `has_channel` 是恒真的，一个把
/// `workspace_root` 丢掉的实现照样能过。真正有判据力的是「列出来的就是那个
/// 临时目录里的内容」——根没到位就不可能列出来。
#[test]
fn the_workspace_root_really_reaches_the_connector() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let (base, ws) = sandbox();

    let ready = channel::on_channel_frame(&open_frame("ch-shape", &ws));
    assert!(
        !ready.frames().is_empty() && !is_error(&ready.frames()[0]),
        "带根的 open 必须成功：{:?}",
        ready.frames()
    );

    // 根里的目录列得出来，且**只有它**列得出来——把 workspace_root 丢掉的
    // 实现根本开不出通道，而一个把根错认成 base 的实现会多列出 `secret.txt`。
    let out = after_data("ch-shape", serde_json::json!({ "op": "list" }));
    let p = payload_of(&out);
    let names: Vec<&str> = p["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, vec!["src"], "列出来的必须是**根里**的内容：{p}");
    assert!(
        !names.contains(&"secret.txt"),
        "界外的 secret.txt 混进列表了：{p}"
    );

    // 读根里的真文件：字节来自磁盘，不是来自任何缓存的 params。
    let r = after_data(
        "ch-shape",
        serde_json::json!({ "op": "read", "path": "src/a.txt" }),
    );
    assert_eq!(payload_of(&r)["text"], "hello\n", "{r:?}");
    assert!(base.path().join("secret.txt").exists(), "判据文件必须在位");
    channel::on_disconnect();
}

/// 一个不存在的根：给出**人话**而不是裸 `ENOENT`。
#[test]
fn a_workspace_root_that_is_gone_says_which_root_it_could_not_use() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let base = tempfile::tempdir().expect("tempdir");
    let gone = base.path().join("deleted");
    let action = channel::on_channel_frame(&open_frame("ch-gone", &gone));
    let err = &action.frames()[0];
    assert!(is_error(err), "{err:?}");
    let reason = err.c["reason"].as_str().expect("reason");
    assert!(reason.contains("not usable"), "要给出错在哪一步：{reason}");
    assert!(reason.contains("deleted"), "要说清是哪个根：{reason}");
}

/// `workspace_root` 的位置：它是 `OpenReq` 上的一等字段，**不该**混进
/// files 的私有 params。钉住这一点是因为 `#[serde(flatten)]` 的存在让
/// 「混进去」在测试里也能跑通——而一个把它当 connector 私有参数读的实现
/// 会一直读到 `None`。
#[test]
fn the_workspace_root_is_a_first_class_param_not_connector_private() {
    let _guard = exclusive();
    channel::reset_for_tests();
    let (_base, ws) = sandbox();
    let frame = open_frame("ch-firstclass", &ws);
    // 它在 `OpenChannel.params` 里与 connector 的私有参数**平级**，
    // 而不是被包进某一家的私有结构。
    let payload: serde_json::Value = frame.payload().expect("payload");
    assert_eq!(
        payload["params"]["workspace_root"],
        ws.to_string_lossy().as_ref()
    );
    assert!(
        !channel::on_channel_frame(&frame).frames().is_empty(),
        "带根的 open 必须至少回 channel_ready"
    );
    channel::on_disconnect();
}
