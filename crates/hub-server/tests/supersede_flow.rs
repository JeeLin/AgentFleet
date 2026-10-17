// 子任务 7「防重复注册与连接顶替」的端到端测试。
//
// 两种「同一台设备又来了」必须分开：
//   ① 同 node_id + 同指纹 = 同一台设备重连 → 新的顶掉旧的；
//   ② 新 node_id + 已存在指纹 = 另一台机器用同一密钥 → 拒绝。
//
// 关键不变量：「不再重连」不能只依赖 disconnect 帧送达——帧会丢，
// 必须有 gen 兜底（标准③）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use std::time::{Duration, Instant};

use common::{connect_enrolled, device_info, wait_disconnected, Hub};
use node_core::conn::{connect_once, ConnOutcome};
use protocol::frame::Frame;
use protocol::message::types;

impl Hub {
    fn device_count(&self) -> usize {
        self.state.devices().list().unwrap().len()
    }

    fn stored_gen(&self, node_id: &str) -> u64 {
        self.state
            .devices()
            .find_by_id(node_id)
            .unwrap()
            .expect("device row must exist")
            .last_seen_gen
    }
}

/// 标准④：顶替之后，注册表里存的是**新**连接。
///
/// 顺序也很关键：先换表、后通知（§7.1）。反过来的话会有一小段时间
/// 路由指向一个即将关闭的句柄。
#[tokio::test]
async fn the_newer_connection_takes_over_the_registry_slot() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    let gen1 = est1.auth_ok.gen;

    let est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    let gen2 = est2.auth_ok.gen;

    assert!(gen2 > gen1, "新连接必须拿到更大的 gen: {gen1} → {gen2}");
    let current = hub
        .state
        .connections()
        .get(&node_id)
        .expect("注册表里必须有一条");
    assert_eq!(current.gen, gen2, "表里必须是新连接");
}

/// 标准②：旧连接收到 disconnect{superseded, gen}。
#[tokio::test]
async fn the_displaced_connection_is_told_it_was_superseded() {
    let hub = Hub::boot().await;
    let (mut est1, node_id, kp) = connect_enrolled(&hub).await;

    let _est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");

    let deadline = Instant::now() + Duration::from_secs(3);
    let outcome = loop {
        match est1.next_text().await {
            Ok(Some(text)) => {
                let f: Frame = serde_json::from_str(&text).unwrap();
                if f.msg_type == types::DISCONNECT {
                    break f;
                }
            }
            Ok(None) => panic!("hub closed the connection without a superseded frame"),
            Err(e) => panic!("unexpected read error: {e:?}"),
        }
        assert!(Instant::now() < deadline, "3s 内没收到 superseded 帧");
    };
    assert_eq!(outcome.c["reason"], "superseded");
    assert!(
        outcome.c["gen"].as_u64().is_some(),
        "superseded 帧必须带 gen: {:?}",
        outcome.c
    );
}

/// 标准③（兜底路径）：不断言帧送达——直接构造「旧连接带着落后 gen 重连」，
/// 且顶替它的那条**还活着**。
///
/// 顶替者必须活着，这不是测试写法的问题而是语义：它自己也走了的话，
/// 旧连接回来没有第二条连接跟它抢，放行才是对的。
#[tokio::test]
async fn a_reconnect_with_a_stale_gen_is_rejected_as_superseded() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    let gen1 = est1.auth_ok.gen;
    drop(est1);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    // 新连接顶掉旧的，并且保持活着
    let _est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    let gen2 = hub
        .state
        .connections()
        .get(&node_id)
        .expect("the new connection must be registered")
        .gen;

    // 旧连接「没收到 superseded 帧」，带着它记得的 gen1 又来了
    let outcome = connect_once(
        &hub.ws_url(),
        &kp,
        Some(&node_id),
        Some(gen1),
        device_info(),
    )
    .await;
    match outcome {
        Err(ConnOutcome::Superseded { gen }) => {
            assert!(
                gen > gen1,
                "回给它的 gen 必须比它带去的新: 带去 {gen1}，回来 {gen}"
            );
            assert_eq!(gen, gen2, "必须是顶替者那条连接的 gen");
        }
        other => panic!("期望被判为 superseded，实际 {other:?}"),
    }
}

/// 标准③ 的边界：顶替者自己也走了 → 旧连接带着落后 gen 回来应当放行。
///
/// 缺这条的话，「兜底」会退化成「一旦被顶替过就永久锁死」：
/// Hub 重启或新连接掉线之后，设备再也接不上，且无自愈路径。
#[tokio::test]
async fn a_stale_gen_is_forgiven_once_the_superseding_connection_is_gone() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    let gen1 = est1.auth_ok.gen;
    drop(est1);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    let est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    drop(est2);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    let est3 = connect_once(
        &hub.ws_url(),
        &kp,
        Some(&node_id),
        Some(gen1),
        device_info(),
    )
    .await
    .expect("顶替者已经走了，旧连接必须能回来");
    assert!(est3.auth_ok.gen > gen1);
}

/// 标准③ 的另一半：没带 gen 的重连必须放行。
///
/// 老节点或首次重连的帧里没有这个字段，按 0 处理会让它们永远被判成
/// 已被顶替（约束 14：前向兼容不许这样断）。
#[tokio::test]
async fn a_reconnect_without_a_gen_is_accepted() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    let gen1 = est1.auth_ok.gen;
    drop(est1);
    wait_disconnected(&hub, &node_id, Duration::from_secs(3)).await;

    let est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("没带 gen 的重连必须放行");
    assert!(
        est2.auth_ok.gen > gen1,
        "新连接拿到更大的 gen: {} → {}",
        gen1,
        est2.auth_ok.gen
    );
}

/// 标准⑦：不同指纹各自注册，互不干扰。
#[tokio::test]
async fn two_different_keys_enroll_as_two_devices() {
    let hub = Hub::boot().await;
    let (est_a, id_a, _kp_a) = connect_enrolled(&hub).await;
    let (est_b, id_b, _kp_b) = connect_enrolled(&hub).await;

    assert_ne!(id_a, id_b, "两套密钥必须是两台设备");
    assert_eq!(hub.device_count(), 2);
    assert!(hub.state.connections().get(&id_a).is_some());
    assert!(hub.state.connections().get(&id_b).is_some());
    drop((est_a, est_b));
}

/// 标准①：同指纹第二次 enroll（带合法签名）不新增行——
/// 它是同一台设备又来了一次，复用已有 node_id。
///
/// 「无合法签名」那条在 registry.rs 的单测里
/// （same_fingerprint_enroll_twice_reports_already_registered）：
/// 那里能直接构造「已存在指纹 + 未验签」的组合，端到端造不出来
/// （端到端路径上签名一定会被验）。
#[tokio::test]
async fn enrolling_the_same_key_twice_does_not_create_a_second_device() {
    let hub = Hub::boot().await;
    let (est1, id_a, kp) = connect_enrolled(&hub).await;
    drop(est1);
    wait_disconnected(&hub, &id_a, Duration::from_secs(3)).await;

    // 同一把钥匙再走一次 enroll 形态（不带 node_id）
    let est2 = connect_once(&hub.ws_url(), &kp, None, None, device_info())
        .await
        .expect("同指纹 + 合法签名 = 同一台设备，必须放行");
    assert_eq!(
        est2.auth_ok.node_id, id_a,
        "必须复用已有的 node_id，而不是签发新的"
    );
    assert_eq!(hub.device_count(), 1, "device 表行数不变");
}

/// 标准⑧：连接代次落库。
///
/// 「重启后仍生效」的那一半由 ConnectionRegistry::with_boot_gen 保证
/// （进程重启后 gen 从库里最大值续上，不会归零把设备锁在门外）。
#[tokio::test]
async fn the_connection_gen_is_persisted_on_the_device_row() {
    let hub = Hub::boot().await;
    let (est, node_id, _kp) = connect_enrolled(&hub).await;
    let gen = est.auth_ok.gen;
    assert!(
        hub.stored_gen(&node_id) >= gen,
        "库里的 last_seen_gen 必须跟上连接代次"
    );
}

/// 顶替后旧连接断开，不得把新连接摘掉（标准⑥：只有当前连接能摘表）。
#[tokio::test]
async fn a_displaced_connection_does_not_remove_the_new_one() {
    let hub = Hub::boot().await;
    let (est1, node_id, kp) = connect_enrolled(&hub).await;
    let _est2 = connect_once(&hub.ws_url(), &kp, Some(&node_id), None, device_info())
        .await
        .expect("reconnect must succeed");
    drop(est1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        hub.state.connections().get(&node_id).is_some(),
        "旧连接断开不得把新连接摘掉"
    );
}
