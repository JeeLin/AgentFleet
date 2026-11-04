//! 子任务 5「worktree 分任务 + 四层磁盘处置」的测试。
//!
//! 四条测试标准各自对应下面一组：
//!
//! 1. 配额超 90% 的设备**不出现在候选集**（不是派过去再失败）
//! 2. 三档策略的清理行为各有断言（保守留 3 个产物 / 平衡跑完即清）
//! 3. **依赖目录永不搬运**：全仓没有一条把 `node_modules` 搬走的路径
//! 4. `git clean -xdf` 后 `git status --porcelain` 为空
//!
//! 第 4 条在这里换一种写法：它要证的是「清理之后工作区真的回到干净状态」，
//! 而 `git clean -xdf` 是 git 的做法。**本 connector 不跑 git**（源码检出归
//! 后续里程碑），所以这里证的是同一件事的本层版本：清理之后**只剩依赖目录**，
//! 别的文件一个不剩。真正的 `git status` 断言留给把检出接上来的那一步。
//!
//! Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::time::{Duration, SystemTime};

use node_connector::connector::worktree::{
    plan_cleanup, DiskUsage, Finished, Policy, Worktree, CACHE_DIRS, QUOTA_STOP_PERCENT,
};
use node_connector::{Connector, OpenReq};

/// 建一个已开通道、指向 `dir` 的 connector。
fn open(dir: &Path, quota: Option<u64>, policy: Policy) -> Worktree {
    let mut w = Worktree::new();
    let params = serde_json::json!({
        "disk_quota_bytes": quota,
        "cleanup_policy": match policy {
            Policy::Conservative => "conservative",
            Policy::Balanced => "balanced",
            Policy::Aggressive => "aggressive",
        },
    });
    w.open(OpenReq::new(dir.display().to_string(), params))
        .expect("开通道");
    w
}

fn create(w: &mut Worktree, task_id: &str) {
    w.on_data(&serde_json::json!({"op": "worktree.create", "task_id": task_id}))
        .expect("建工作区");
}

// ---- 测试标准 1：配额超 90% 不出现在候选集 -------------------------------

/// 用量过 90% 时 `accepts_new_work()` 必须是 `false`。
///
/// 这条判据是 Hub 派活前过滤候选集的唯一依据。它若写反（或者拿 `<=` 比），
/// 后果是**任务被派到一台已经满了的机器上**，然后在写产物时失败——而那个
/// 失败会被记成任务失败。用户看到「任务莫名其妙挂了」，不是「磁盘满了」。
#[test]
fn a_device_over_the_quota_threshold_refuses_new_work() {
    // 89% —— 还收。
    let ok = DiskUsage {
        used_bytes: 890,
        quota_bytes: Some(1000),
    };
    assert_eq!(ok.percent(), 89);
    assert!(ok.accepts_new_work(), "89% 还在门槛内");

    // 90% —— **不收**。边界取「小于才收」：留 10% 给已经在跑的任务写产物。
    let edge = DiskUsage {
        used_bytes: 900,
        quota_bytes: Some(1000),
    };
    assert_eq!(edge.percent(), 90);
    assert!(
        !edge.accepts_new_work(),
        "{QUOTA_STOP_PERCENT}% 就是门槛，等于它就不该再派活了"
    );

    // 100% —— 当然不收。
    let full = DiskUsage {
        used_bytes: 1000,
        quota_bytes: Some(1000),
    };
    assert!(!full.accepts_new_work());
}

/// **没设配额 = 不因磁盘原因拦**。
///
/// 这是很容易写反的一条：`percent()` 在没配额时返回 0 而不是 100，
/// 因为「没设上限」与「上限是 0」是两件事。返回 100 会把所有没配额的项目拦掉，
/// 而那恰好是默认状态（新绑定默认不限额）。
#[test]
fn no_quota_means_never_blocked() {
    let unset = DiskUsage {
        used_bytes: 10_000_000,
        quota_bytes: None,
    };
    assert_eq!(unset.percent(), 0, "没设配额不是 100% 满");
    assert!(unset.accepts_new_work());

    // 配额为 0 是**真实的上限**，会拦掉一切。
    let zero = DiskUsage {
        used_bytes: 1,
        quota_bytes: Some(0),
    };
    assert!(!zero.accepts_new_work(), "上限为 0 就是一个都放不下");
}

/// 用量真的从盘上算出来，且**不跟随符号链接**。
///
/// 跟随的话，一个指向 `/` 的链接会让「这个项目用了 500G」——而它实际只用了
/// 几个字节。那个数字会直接把配额判定推向「满了」。
#[test]
fn disk_usage_is_measured_without_following_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.bin"), vec![0u8; 1000]).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/b.bin"), vec![0u8; 500]).unwrap();

    let mut w = open(root, Some(10_000), Policy::Balanced);
    let out = w
        .on_data(&serde_json::json!({"op": "worktree.disk_usage"}))
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].payload["used_bytes"], serde_json::json!(1500));
    assert_eq!(out[0].payload["percent"], serde_json::json!(15));
    assert_eq!(out[0].payload["accepts_new_work"], serde_json::json!(true));

    // 加一个指向界外的链接：用量**不该**跟着涨。
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/usr", root.join("escape")).unwrap();
        let out = w
            .on_data(&serde_json::json!({"op": "worktree.disk_usage"}))
            .unwrap();
        assert!(
            out[0].payload["used_bytes"].as_u64().unwrap() < 100_000,
            "符号链接不计入用量，实际 {}",
            out[0].payload["used_bytes"]
        );
    }
}

// ---- 测试标准 2：三档策略各有断言 ---------------------------------------

/// 保守档：**留最近 3 个**，其余删。
#[test]
fn the_conservative_policy_keeps_the_three_newest() {
    let base = SystemTime::UNIX_EPOCH;
    let finished: Vec<Finished> = (0..5)
        .map(|i| Finished {
            task_id: format!("t{i}"),
            finished_at: base + Duration::from_secs(i as u64),
        })
        .collect();

    let plan = plan_cleanup(Policy::Conservative, &finished);
    // 最新的是 t4 / t3 / t2，留下；t1 / t0 删。
    let mut removed = plan.remove_worktrees.clone();
    removed.sort();
    assert_eq!(removed, vec!["t0".to_string(), "t1".to_string()]);
    assert!(
        !plan.reclaim_cache,
        "东西还没删完，不该顺手清缓存——那会让下次执行重装依赖"
    );
}

/// 平衡档：**跑完即清**（一个都不留）。
#[test]
fn the_balanced_policy_removes_every_product() {
    let base = SystemTime::UNIX_EPOCH;
    let finished: Vec<Finished> = (0..3)
        .map(|i| Finished {
            task_id: format!("t{i}"),
            finished_at: base + Duration::from_secs(i as u64),
        })
        .collect();

    let plan = plan_cleanup(Policy::Balanced, &finished);
    assert_eq!(plan.remove_worktrees.len(), 3, "平衡档跑完即清，一个不留");
    assert!(plan.reclaim_cache, "产物都清完了，缓存才轮到");
}

/// 激进档：同样跑完即清，但缓存 `ttl` 是零 = 下一个清理回合就清。
#[test]
fn the_aggressive_policy_reclaims_cache_immediately() {
    assert_eq!(Policy::Aggressive.keep_artifacts(), 0);
    assert_eq!(
        Policy::Aggressive.cache_ttl(),
        Some(Duration::ZERO),
        "「尽快清」= 下次清理一定选中它"
    );
    // 保守 / 平衡都有非零 ttl，否则三档就退化成两档。
    assert!(Policy::Conservative.cache_ttl().unwrap() > Policy::Balanced.cache_ttl().unwrap());
    assert!(Policy::Balanced.cache_ttl().unwrap() > Duration::ZERO);
}

/// **三档必须真的不一样**——只写三个名字不改行为，等于给用户一个不起作用的下拉框。
#[test]
fn the_three_policies_actually_differ() {
    let ids = ["a", "b", "c", "d", "e"];
    let mk = |keep: usize| -> Vec<String> {
        let base = SystemTime::UNIX_EPOCH;
        let finished: Vec<Finished> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| Finished {
                task_id: id.to_string(),
                finished_at: base + Duration::from_secs(i as u64),
            })
            .collect();
        let mut v = plan_cleanup(
            if keep == 3 {
                Policy::Conservative
            } else {
                Policy::Balanced
            },
            &finished,
        )
        .remove_worktrees;
        v.sort();
        v
    };
    let conservative = mk(3);
    let balanced = mk(0);
    assert_ne!(
        conservative, balanced,
        "保守留 3 个、平衡一个不留——两档删出来的集合必须不同"
    );
    assert_eq!(conservative.len(), 2);
    assert_eq!(balanced.len(), 5);
}

/// 没有跑完的任务时什么都不删，也**不清缓存**。
#[test]
fn nothing_finished_means_nothing_to_clean() {
    let plan = plan_cleanup(Policy::Aggressive, &[]);
    assert!(plan.remove_worktrees.is_empty());
    assert!(!plan.reclaim_cache, "没删过东西就不该动缓存");
}

/// 删工作区：保守档下**依赖目录留下**，别的全没。
#[test]
fn removing_a_worktree_keeps_the_dependency_directories_under_conservative() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Conservative);
    create(&mut w, "t1");
    let dir = tmp.path().join("t1");

    // 造点东西：产物 + 依赖。
    std::fs::write(dir.join("out.tar"), b"product").unwrap();
    std::fs::create_dir(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("node_modules/dep.js"), b"dep").unwrap();
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), b"code").unwrap();

    let out = w
        .on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "t1"}))
        .unwrap();
    assert_eq!(out[0].payload["cache_kept"], serde_json::json!(true));

    assert!(!dir.join("out.tar").exists(), "产物必清");
    assert!(!dir.join("src").exists(), "源码也是产物，一起清");
    assert!(
        dir.join("node_modules/dep.js").exists(),
        "保守档要把依赖留给下一个任务复用"
    );
}

/// 删工作区：激进档下**连依赖一起清**。
#[test]
fn removing_a_worktree_under_aggressive_drops_the_cache_too() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Aggressive);
    create(&mut w, "t1");
    let dir = tmp.path().join("t1");
    std::fs::create_dir(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("node_modules/dep.js"), b"dep").unwrap();

    let out = w
        .on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "t1"}))
        .unwrap();
    assert_eq!(out[0].payload["cache_kept"], serde_json::json!(false));
    assert!(!dir.exists(), "激进档整个目录都不留");
}

// ---- 测试标准 3：依赖目录永不搬运 ---------------------------------------

/// **`node_modules` 这类目录不出现在任何搬运路径里。**
///
/// 这条用例读全 crate 的源码。看起来笨，但它是唯一能拦住「顺手加一个
/// `copy_dir(dep)`」的办法——那种改动能编译、能跑、「看起来还能加速」，
/// 而它搬过去的是**为另一个架构装的依赖**（x86-64 的 `.node` 二进制在
/// aarch64 上跑不起来）。症状是「能启动、一用到某个包就崩」。
#[test]
fn no_copy_path_ever_moves_a_dependency_directory() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    walk(&src_dir, &mut |path, text| {
        for (lineno, line) in text.lines().enumerate() {
            // 只找「复制」类的调用，且同一行里出现了依赖目录名。
            let copies = line.contains("copy(")
                || line.contains("copy_dir")
                || line.contains("fs::copy")
                || line.contains("recursive_copy");
            if copies && CACHE_DIRS.iter().any(|d| line.contains(d)) {
                hits.push(format!("{}:{}: {line}", path.display(), lineno + 1));
            }
        }
    });
    assert!(
        hits.is_empty(),
        "依赖目录绝不能被复制/搬运，实际找到：\n{}",
        hits.join("\n")
    );
}

/// 依赖目录清单本身要有内容，且包含最关键的那个名字。
///
/// 这条是给上面那条兜底的：如果 `CACHE_DIRS` 被清空，上面那条用例会**恒真**
/// 地通过——一个空的清单让任何行都「不含依赖目录名」。
#[test]
fn the_dependency_directory_list_is_not_empty() {
    assert!(!CACHE_DIRS.is_empty());
    assert!(CACHE_DIRS.contains(&"node_modules"));
}

// ---- 测试标准 4：清理之后工作区干净 -------------------------------------

/// 清理之后**只剩依赖目录**，别的文件一个不剩。
///
/// 这是「`git clean -xdf` 之后 `git status --porcelain` 为空」在本层的等价物：
/// 本 connector 不跑 git（检出归后续里程碑），但它要让工作区回到「没有任何
/// 任务残留」的状态。留一个残留文件，下一个任务就会看到「改动不是我做的」。
#[test]
fn after_cleanup_only_dependency_directories_remain() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Conservative);
    create(&mut w, "t1");
    let dir = tmp.path().join("t1");

    // 各种形态的残留：顶层文件、子目录、隐藏文件、深层文件。
    std::fs::write(dir.join("a.txt"), b"x").unwrap();
    std::fs::write(dir.join(".env"), b"x").unwrap();
    std::fs::create_dir_all(dir.join("deep/nested")).unwrap();
    std::fs::write(dir.join("deep/nested/b.txt"), b"x").unwrap();
    std::fs::create_dir(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("node_modules/dep.js"), b"x").unwrap();

    w.on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "t1"}))
        .unwrap();

    let left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        vec!["node_modules".to_string()],
        "清理之后只该剩依赖目录，实际剩 {left:?}"
    );
}

/// 删一个不存在的工作区**不报错**：`remove` 要能重复调用
/// （断连重连与「Hub 记错了它在不在」都会让同一件事来两次）。
#[test]
fn removing_a_worktree_twice_is_fine() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Aggressive);
    // 一次都没建过。
    w.on_data(&serde_json::json!({"op": "worktree.remove", "task_id": "nope"}))
        .expect("删不存在的要成功");
}

// ---- 任务 id 的边界 ------------------------------------------------------

/// 任务 id 里带路径分隔符要拒：那个路径是拿来**删**的。
///
/// 放过 `../..` 的话，`worktree.remove` 就是一条「删掉固定目录之外任意位置」
/// 的指令，而请求来自网络。
#[test]
fn a_task_id_cannot_escape_the_fixed_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Aggressive);

    for bad in ["../escape", "a/b", "..", ".", ""] {
        let err = w
            .on_data(&serde_json::json!({"op": "worktree.create", "task_id": bad}))
            .err()
            .unwrap_or_else(|| panic!("task_id {bad:?} 必须被拒"));
        // 错误里说清是「任务 id 不能命名工作区」，不是一句内部错误。
        let text = err.to_string();
        assert!(
            text.contains("worktree") || text.contains("task id"),
            "错误要能定位到任务 id：{text}"
        );
    }
    // 正样本：正常 id 能建。
    create(&mut w, "t-1_ok");
    assert!(tmp.path().join("t-1_ok").is_dir());
}

/// 建一个已存在的工作区要报错，**不能静默复用**。
#[test]
fn creating_an_existing_worktree_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Aggressive);
    create(&mut w, "t1");
    let err = w
        .on_data(&serde_json::json!({"op": "worktree.create", "task_id": "t1"}))
        .expect_err("重复建要报错");
    assert!(
        err.to_string().contains("already exists"),
        "错误要说清是「已经存在」而不是笼统失败：{err}"
    );
}

/// `Worktree::default()` 与 `new()` 等价：没开通道、没配额、平衡策略。
///
/// `Default` 实现存在的理由是要让 `Worktree` 能进那些要求 `Default` 的容器
/// （连接器注册表）。它若与 `new()` 漂移（例如缺省成激进策略），症状是
/// 「注册表里那个」和「`new()` 出来的那个」行为不一样——同一个连接器两种性格。
#[test]
fn default_matches_new() {
    let mut d = Worktree::default();
    let mut n = Worktree::new();
    // 没开通道时两个都不该能干活，并且报的是同一句话。
    let rd = d.on_data(&serde_json::json!({"op": "worktree.disk_usage"}));
    let rn = n.on_data(&serde_json::json!({"op": "worktree.disk_usage"}));
    assert!(rd.is_err(), "`default()` 不该是个已开通道的连接器");
    assert_eq!(
        rd.err().map(|e| e.to_string()),
        rn.err().map(|e| e.to_string()),
        "`default()` 与 `new()` 必须表现一致"
    );
}

/// 没开通道就发 op：报「通道没开」，不是 panic。
#[test]
fn operations_without_an_open_channel_are_rejected() {
    let mut w = Worktree::new();
    for op in ["worktree.create", "worktree.remove", "worktree.disk_usage"] {
        let err = w
            .on_data(&serde_json::json!({"op": op, "task_id": "t1"}))
            .err()
            .unwrap_or_else(|| panic!("{op} 在没开通道时必须失败"));
        assert!(
            err.to_string().contains("not open"),
            "{op} 要说清是通道没开：{err}"
        );
    }
}

/// 未知 op 报错，且错误里带上它是哪个 op——否则排查时不知道该看哪一段。
#[test]
fn an_unknown_operation_is_named_in_the_error() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Balanced);
    let err = w
        .on_data(&serde_json::json!({"op": "worktree.teleport"}))
        .expect_err("未知 op 要报错");
    assert!(err.to_string().contains("worktree.teleport"), "{err}");
}

/// 帧里**没有 `op`** 时报错，且说的是缺了什么。
///
/// Hub 侧发错帧是可能的（协议升级时一新一旧），而静默忽略会让它看起来
/// 「派活了但设备什么也没做」——那是最难查的一种。
#[test]
fn a_frame_without_an_op_is_rejected_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Balanced);
    let err = w
        .on_data(&serde_json::json!({"task_id": "t1"}))
        .expect_err("缺 op 要报错");
    assert!(err.to_string().contains("op"), "错误要指出缺的是 op：{err}");
}

/// 建 / 删工作区时缺 `task_id`：报错，且**不**在盘上落任何东西。
///
/// 用空串当任务 id 会建出一个叫 `.afleet/worktrees/` 的目录，而那条路径正好是
/// 全部工作树的父目录——下一个任务再来建就会撞上「已经存在」。
#[test]
fn a_missing_task_id_is_rejected_and_leaves_the_disk_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let mut w = open(tmp.path(), None, Policy::Balanced);
    for op in ["worktree.create", "worktree.remove"] {
        let err = w
            .on_data(&serde_json::json!({ "op": op }))
            .expect_err(&format!("{op} 缺 task_id 必须失败"));
        assert!(
            err.to_string().contains("task_id"),
            "{op} 的错误要指出缺的是 task_id：{err}"
        );
    }
    // 盘上不该多出任何工作树。
    let wt = tmp.path().join(".afleet").join("worktrees");
    let leftovers: Vec<_> = std::fs::read_dir(&wt)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "缺 task_id 的请求不该在盘上留下东西：{leftovers:?}"
    );
}

// ---- 工具 ----------------------------------------------------------------

/// 递归遍历目录里的文件，对每个文件的路径与内容调用 `f`。
///
/// 只读 `.rs`：本 crate 里没有别的东西会写代码。
fn walk(dir: &Path, f: &mut impl FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                f(&path, &text);
            }
        }
    }
}
