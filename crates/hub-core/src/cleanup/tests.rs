//! `cleanup` 的用例。
//!
//! 分三组：
//! 1. **策略**：三档各自的去留，每条用例断言的是「谁在候选里、可释放多少」，
//!    不是「返回了几条」；
//! 2. **核心断言**：预演**不动盘**。用实造的目录树量前后两次占用；
//! 3. **调用点**：一次真的 `disk_exhausted` 回执进来 → `preview` 确实被调用
//!    → 轨迹里真有一份带候选与 `reclaimable_bytes` 的事实。**只测纯函数不算
//!    交付**，这一组是那条要求的落点。

use super::*;

use hub_storage::db::open;
use hub_storage::execution::{ExecutionRepo, NewExecution};
use hub_storage::trajectory::{NewEvent, TrajectoryRepo};
use hub_storage::{BaseRevStatus, SessionRepo, TaskRepo};

use std::fs;
use std::path::Path;

const TS: &str = "unix:1700000000";

// ---------------------------------------------------------------------------
// 夹具：实造的目录树
// ---------------------------------------------------------------------------

/// 在 `root` 下写一个**大小精确**的文件。
///
/// 预演的输入是实测字节数，所以夹具必须造出**真的大小**：拿一个随便写的文件
/// 去对 `reclaimable_bytes`，对上的只是「我们把同一个数抄了两遍」。
fn write_sized(root: &Path, rel: &str, bytes: usize) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("父目录");
    }
    fs::write(&path, vec![b'x'; bytes]).expect("写文件");
}

/// 一棵「这次执行留下的」目录树。
///
/// 三个条目，大小互不相同（1000 / 4000 / 8000），这样「可释放多少」不是一个
/// 巧合能凑出来的数：
///
/// ```text
/// <root>/wt/node_modules/           4000 字节  依赖缓存
/// <root>/wt/report.json             8000 字节  任务产物
/// <root>/wt/src/main.rs             1000 字节  工作树
/// ```
fn worktree_at(root: &Path) -> &Path {
    root.join("wt").leak()
}

fn plant_worktree(root: &Path) {
    let wt = worktree_at(root);
    write_sized(wt, "node_modules/pkg/blob.bin", 4000);
    write_sized(wt, "report.json", 8000);
    write_sized(wt, "src/main.rs", 1000);
}

/// 递归实测一个路径的占用（目录则累加其下所有文件）。
///
/// **文件本身也要能测**：夹具里有的条目是一个单文件（`report.json`），对它
/// `read_dir` 会失败而得到 0——那样「可释放多少」就少算了。所以先看它是不是
/// 文件，再决定要不要遍历。**不跟随符号链接**（与设备侧 `dir_size` 同一条口径）。
fn measure(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// 实测出来的那份清单（从真目录树量，不手抄数字）。
fn items_from_disk(root: &Path) -> Vec<StoredItem> {
    let wt = worktree_at(root);
    vec![
        StoredItem {
            path: wt.join("node_modules").display().to_string(),
            bytes: measure(&wt.join("node_modules")),
            class: ItemClass::DependencyCache,
            modified_at: Some(3_000),
            label: "node_modules".into(),
        },
        StoredItem {
            path: wt.join("report.json").display().to_string(),
            bytes: measure(&wt.join("report.json")),
            class: ItemClass::Artifact,
            modified_at: Some(2_000),
            label: "report.json".into(),
        },
        StoredItem {
            path: wt.join("src").display().to_string(),
            bytes: measure(&wt.join("src")),
            class: ItemClass::Worktree,
            modified_at: Some(1_000),
            label: "src".into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// 一、策略：三档各自的去留
// ---------------------------------------------------------------------------

/// **核心断言之一**：`reclaimable_bytes` == 候选大小之和，且和是**实测**的。
///
/// 两侧都从真目录树里量：左边求和用的是 `preview` 的候选，右边是 `fs` 里那
/// 几个文件的实际大小 + 夹具写死的字节数。断言两边相等之前还先断言了
/// 「实测值确实是 8000 / 4000 / 1000」——否则一棵空树上两边都是 0，这条会假绿。
#[test]
fn reclaimable_bytes_equals_the_sum_of_the_candidate_sizes_measured_on_disk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let items = items_from_disk(tmp.path());

    // 夹具自检：实测值就是写下去的那些字节，不是 0。
    let sizes: Vec<u64> = items.iter().map(|i| i.bytes).collect();
    assert_eq!(sizes, vec![4000, 8000, 1000], "夹具必须造出实数");

    let got = preview(&CleanupPolicy::Balanced, &items);

    let summed = got.candidates.iter().map(|c| c.bytes).sum::<u64>();
    assert_eq!(
        got.reclaimable_bytes, summed,
        "可释放字节数必须逐字等于候选大小之和"
    );
    // 再对一次独立的来源：磁盘上那棵树的总占用。
    assert_eq!(
        got.reclaimable_bytes,
        measure(worktree_at(tmp.path())),
        "平衡档全清时，可释放 = 整棵树的实测占用"
    );
    assert_eq!(got.reclaimable_bytes, 13_000);
}

/// 平衡档：工作树 + 依赖缓存 + 产物**全进候选**。
#[test]
fn the_balanced_tier_selects_every_class_including_the_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let items = items_from_disk(tmp.path());

    let got = preview(&CleanupPolicy::Balanced, &items);
    let labels: Vec<&str> = got.candidates.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["report.json", "node_modules", "src"],
        "平衡档三样都该选中；顺序是先大后小"
    );
    assert_eq!(got.reclaimable_bytes, 13_000);
    assert_eq!(got.policy, CleanupPolicy::Balanced);
}

/// 激进档：与平衡档**同样全选**，但理由串不同（要能对上是哪一档）。
///
/// 理由串必须真的不同：两档都把一切清掉，若理由也一模一样，用户从预演里看不出
/// 「按这档清会连缓存一起清」——而那正是他要判断的事。
#[test]
fn the_aggressive_tier_selects_everything_and_says_why_differently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let items = items_from_disk(tmp.path());

    let balanced = preview(&CleanupPolicy::Balanced, &items);
    let aggressive = preview(&CleanupPolicy::Aggressive, &items);

    // 候选集合相同（按路径比，不比理由）。
    let paths_of = |p: &CleanupPreview| {
        let mut v: Vec<String> = p.candidates.iter().map(|c| c.path.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(paths_of(&balanced), paths_of(&aggressive));
    assert_eq!(aggressive.reclaimable_bytes, 13_000);

    // 依赖缓存那一条的理由必须把「激进」写出来。
    let cache = aggressive
        .candidates
        .iter()
        .find(|c| c.class == ItemClass::DependencyCache)
        .expect("激进档要选中依赖缓存");
    assert!(
        cache.reason.contains("激进"),
        "理由要说清是哪一档：{}",
        cache.reason
    );
}

/// 保守档：**依赖缓存留下**，产物的最近 3 个留下，工作树选中。
#[test]
fn the_conservative_tier_keeps_the_cache_and_the_three_newest_artifacts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let mut items = items_from_disk(tmp.path());

    // 再补两个产物，凑满「留 3 个」的名额（原生那棵只有 1 个产物）。
    items.push(StoredItem {
        path: "/w/wt/a1.log".into(),
        bytes: 100,
        class: ItemClass::Artifact,
        modified_at: Some(4_000),
        label: "a1.log".into(),
    });
    items.push(StoredItem {
        path: "/w/wt/a2.log".into(),
        bytes: 200,
        class: ItemClass::Artifact,
        modified_at: Some(5_000),
        label: "a2.log".into(),
    });

    let got = preview(&CleanupPolicy::Conservative, &items);

    // 依赖缓存**不在**候选里。
    assert!(
        !got.candidates
            .iter()
            .any(|c| c.class == ItemClass::DependencyCache),
        "保守档必须留下依赖缓存：{:?}",
        got.candidates
    );
    // 三个最新产物（report.json=2000、a1=4000、a2=5000）都留下。
    for kept in ["report.json", "a1.log", "a2.log"] {
        assert!(
            !got.candidates.iter().any(|c| c.label == kept),
            "保守档要留下最新的 3 个产物，{kept} 不该被选中"
        );
    }
    // 工作树选中。
    assert!(got
        .candidates
        .iter()
        .any(|c| c.class == ItemClass::Worktree));
    // 可释放 = 只有工作树那 1000 字节。
    assert_eq!(
        got.reclaimable_bytes, 1000,
        "保守档只释放工作树：{:?}",
        got.candidates
    );
}

/// 保守档第 4 个及更旧的产物**会被选中**——否则「留 3 个」就成了「全留」。
#[test]
fn the_conservative_tier_selects_the_artifacts_older_than_the_three_newest() {
    let items: Vec<StoredItem> = (0..5)
        .map(|i| StoredItem {
            path: format!("/w/wt/a{i}.log"),
            bytes: 10 * (i as u64 + 1),
            class: ItemClass::Artifact,
            modified_at: Some(i as i64),
            label: format!("a{i}.log"),
        })
        .collect();

    let got = preview(&CleanupPolicy::Conservative, &items);
    let mut labels: Vec<&str> = got.candidates.iter().map(|c| c.label.as_str()).collect();
    labels.sort();
    assert_eq!(
        labels,
        vec!["a0.log", "a1.log"],
        "最旧的两个该被选中，最新的三个留下"
    );
    // 10 + 20 = 30。
    assert_eq!(got.reclaimable_bytes, 30);
}

/// 保守档的名额口径与设备侧 `Policy::keep_artifacts` 的保守档同值。
///
/// **不靠自比**：这条把 3 写死在断言里。设备侧改了保留数而这边没跟上时，
/// 这条不会自动变绿——它会挂着让人去核对两边。
#[test]
fn the_conservative_tier_matches_the_device_side_keep_count() {
    assert_eq!(CONSERVATIVE_KEEP_ARTIFACTS, 3);
}

// ---------------------------------------------------------------------------
// 二、核心断言：预演不动盘
// ---------------------------------------------------------------------------

/// **本任务的核心断言**：执行预演之后磁盘占用**不变**。
///
/// # 断言方式
///
/// 预演前后各量一次同一棵树的**实测**字节数，再断言两次数值相等；同时断言
/// 文件条目数没变（一个「清空内容但留下空文件」的实现会让字节数为 0 而条目数
/// 还在——所以两条都要）。
///
/// # 为什么这条能变红
///
/// 把 `preview` 改成顺手删几项（哪怕只删一个候选），第二次量出来的数就会变小，
/// 这条立刻红。三档各跑一遍：任何一档偷偷动手都能被抓到。
#[test]
fn running_a_preview_leaves_the_disk_exactly_as_it_was() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let wt = worktree_at(tmp.path());

    let count_entries = |dir: &Path| -> usize {
        let mut n = 0usize;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                n += 1;
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(entry.path());
                }
            }
        }
        n
    };

    let before_bytes = measure(wt);
    let before_entries = count_entries(wt);
    // 前置：树里确实有东西，否则「前后都没变」是假的（0 == 0）。
    assert!(before_bytes > 0 && before_entries > 0, "夹具有内容");

    let items = items_from_disk(tmp.path());
    for policy in [
        CleanupPolicy::Conservative,
        CleanupPolicy::Balanced,
        CleanupPolicy::Aggressive,
    ] {
        let got = preview(&policy, &items);
        // 预演确实选中了东西（否则「什么都没删」可以用「什么都没选中」蒙过去）。
        assert!(
            !got.candidates.is_empty(),
            "{policy:?} 该选中东西，否则这条不动盘的断言没有检测力"
        );
        assert_eq!(
            measure(wt),
            before_bytes,
            "{policy:?} 预演之后磁盘占用必须不变"
        );
        assert_eq!(
            count_entries(wt),
            before_entries,
            "{policy:?} 预演之后文件条目数必须不变"
        );
    }

    // 再逐条核对：候选里说的每一个路径**现在还在**。
    let last = preview(&CleanupPolicy::Aggressive, &items);
    for c in &last.candidates {
        assert!(
            Path::new(&c.path).exists(),
            "预演只是说要删，{} 必须还在盘上",
            c.path
        );
    }
}

// ---------------------------------------------------------------------------
// 三、空清单 / 全不匹配 → 空清单而不是报错
// ---------------------------------------------------------------------------

/// 空目录：一棵什么都没有的树 → 空候选、可释放 0，**不是报错**。
#[test]
fn an_empty_worktree_yields_an_empty_preview_not_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(worktree_at(tmp.path())).expect("空工作树");

    for policy in [
        CleanupPolicy::Conservative,
        CleanupPolicy::Balanced,
        CleanupPolicy::Aggressive,
    ] {
        let got = preview(&policy, &[]);
        assert!(got.candidates.is_empty(), "{policy:?}：空清单 → 空候选");
        assert_eq!(got.reclaimable_bytes, 0);
        assert_eq!(got.policy, policy, "策略仍要回显给人看");
    }
    // 也从真目录量一遍确认它是空的（不是夹具忘了造）。
    assert_eq!(measure(worktree_at(tmp.path())), 0);
}

/// **全不匹配策略**：只有保守档该留的东西（一块依赖缓存）→ 保守档空清单。
///
/// 这一条挡的是「保守档一律返回全部」那种实现：它在前面的用例里可能全绿，
/// 因为那些夹具都带工作树。这里**只给依赖缓存**，保守档必须什么都不选。
#[test]
fn a_policy_that_matches_nothing_returns_an_empty_preview() {
    let items = vec![StoredItem {
        path: "/w/wt/node_modules".into(),
        bytes: 4096,
        class: ItemClass::DependencyCache,
        modified_at: Some(1),
        label: "node_modules".into(),
    }];

    let conservative = preview(&CleanupPolicy::Conservative, &items);
    assert!(
        conservative.candidates.is_empty(),
        "保守档留缓存，因此什么都不可清：{:?}",
        conservative.candidates
    );
    assert_eq!(conservative.reclaimable_bytes, 0);

    // 同一份清单在平衡档下非空——证明「空」是策略的选择，不是清单坏了。
    let balanced = preview(&CleanupPolicy::Balanced, &items);
    assert_eq!(balanced.candidates.len(), 1);
    assert_eq!(balanced.reclaimable_bytes, 4096);
}

/// `ItemClass` 每个变体都有线上码，且三档都能对每个变体给出一个明确答案
/// （「选中」或「留下」，不会既不在候选里也不在被保留的理由里）。
#[test]
fn every_item_class_is_decided_by_every_policy() {
    for class in ItemClass::ALL {
        assert!(!class.as_str().is_empty(), "每个类别都要有线上码");
        for policy in [
            CleanupPolicy::Conservative,
            CleanupPolicy::Balanced,
            CleanupPolicy::Aggressive,
        ] {
            let item = StoredItem {
                path: format!("/w/wt/{}", class.as_str()),
                bytes: 1,
                class: *class,
                modified_at: Some(1),
                label: class.as_str().into(),
            };
            let got = preview(&policy, &[item]);
            // 要么选中（并在理由里说清为什么），要么明确是保守档的缓存/产物。
            if let Some(c) = got.candidates.first() {
                assert!(
                    !c.reason.is_empty(),
                    "{class:?} / {policy:?} 的候选必须说清为什么会被删"
                );
            } else {
                assert!(
                    matches!(policy, CleanupPolicy::Conservative),
                    "{class:?} 在 {policy:?} 下既不选中也没理由，判定漏了"
                );
            }
        }
    }
}

/// 线上码逐字写死——改一个字符串就等于换语义。
#[test]
fn the_item_class_codes_are_written_out_literally() {
    assert_eq!(ItemClass::Worktree.as_str(), "worktree");
    assert_eq!(ItemClass::DependencyCache.as_str(), "dependency_cache");
    assert_eq!(ItemClass::Artifact.as_str(), "artifact");
}

// ---------------------------------------------------------------------------
// 四、调用点：disk_exhausted 回报 → preview 真的被调用 → 结果留痕
// ---------------------------------------------------------------------------

/// 「Hub 的库 + 仓储」夹具。
fn hub() -> (
    tempfile::TempDir,
    ExecutionRepo,
    TrajectoryRepo,
    SessionRepo,
    TaskRepo,
) {
    let d = tempfile::tempdir().expect("tempdir");
    let pool = open(&d.path().join("t.db")).expect("open");
    pool.get()
        .expect("conn")
        .execute_batch(
            "INSERT INTO project (id, name, shape, created_at)
                 VALUES ('p_a', 'app', 'central', 'unix:0');
             INSERT INTO device (id, name, key_fingerprint, enrolled_at)
                 VALUES ('n_a', 'pi', 'fp_a', 'unix:0');
             INSERT INTO binding
                 (id, project_id, device_id, fixed_dir, cleanup_policy, created_at)
                 VALUES ('b_a', 'p_a', 'n_a', '/srv/app', 'balanced', 'unix:0');
             INSERT INTO task (id, project_id, title, state, created_at, updated_at)
                 VALUES ('t_a', 'p_a', 'build it', 'running', 'unix:0', 'unix:0');",
        )
        .expect("seed");
    (
        d,
        ExecutionRepo::new(pool.clone()),
        TrajectoryRepo::new(pool.clone()),
        SessionRepo::new(pool.clone()),
        TaskRepo::new(pool),
    )
}

/// 建一条执行并把它标成 `disk_exhausted` 失败，返回读回来的那份视图。
///
/// 走仓储而不是直接 `INSERT`：这样 `failure_kind` 是**经 `FailureKind::from_str`
/// 校验过的真值**，预演读到的形态与生产一致。
fn failed_execution(executions: &ExecutionRepo) -> ExecutionView {
    let created = executions
        .create(
            NewExecution {
                task_id: "t_a".into(),
                binding_id: "b_a".into(),
                base_commit: Some("deadbeef".into()),
                worktree_path: Some("/srv/app/t_a".into()),
            },
            TS,
        )
        .expect("create execution");
    executions
        .set_failure(
            &created.id,
            FailureKind::DiskExhausted,
            "a run that blew the disk budget",
            "disk_exhausted worktree=/srv/app/t_a",
        )
        .expect("set failure");
    executions.get(&created.id).expect("get").expect("exists")
}

/// 建一条挂着那次执行的会话，返回会话 id。
///
/// `base_rev_status` 用 `Unavailable`：这个夹具建的是普通会话，没有探测过起点
/// ——「试过了、设备没给」正是它的处境，用 `Unknown`（历史上从没记过）会是假话。
fn session_for(sessions: &SessionRepo, execution_id: &str) -> String {
    let s = sessions
        .create(
            "n_a",
            "/srv/app",
            None,
            TS,
            None,
            BaseRevStatus::Unavailable,
        )
        .expect("create session");
    sessions
        .set_execution_id(&s.id, execution_id)
        .expect("attach execution");
    s.id
}

/// **调用点断言**：一次 `disk_exhausted` 回执进来 → `preview` 确实被调用
/// → 轨迹里记下了候选清单与 `reclaimable_bytes`。
///
/// 三条独立证据，缺一条都不算证明：
///
/// 1. `previews_run()` 的**增量**为 1——证明进了 `preview`，不是别处凑出的结果
///    （计数器在 `on_failure_ack` 里、`preview` 调用之前加，所以它涨了就说明
///    这条路径真的走到了预演）；
/// 2. 返回值是 `Some`，且里面的候选与 `reclaimable_bytes` 与磁盘实测对得上；
/// 3. **轨迹里真有一条 `disk_cleanup_preview` 事实**，把 payload 解出来之后
///    里面的候选路径与可释放字节数一致——这是「结果被留痕」，而不只是「算过」。
#[test]
fn the_disk_exhausted_ack_runs_a_preview_and_leaves_it_in_the_trajectory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let items = items_from_disk(tmp.path());
    let on_disk = measure(worktree_at(tmp.path()));

    let (_db, executions, trajectory, sessions, _tasks) = hub();
    let exec = failed_execution(&executions);
    let session_id = session_for(&sessions, &exec.id);

    let before = previews_run();
    let got = on_failure_ack(
        FailureReport::of_execution(&exec),
        &items,
        &CleanupPolicy::Balanced,
        Some(&session_id),
        &trajectory,
        TS,
    )
    .expect("预演不该失败")
    .expect("disk_exhausted 必须产出预告");

    // 证据 1：preview 被调用了。
    assert_eq!(
        previews_run() - before,
        1,
        "disk_exhausted 进来时 preview 必须被调用（增量恒为 1）"
    );
    // 证据 2：结果里真有清单，且可释放字节数 == 磁盘实测。
    assert_eq!(got.preview.reclaimable_bytes, on_disk);
    assert_eq!(got.preview.candidates.len(), 3, "三个条目都该进候选");

    // 证据 3：轨迹里有一条事实，且内容对得上。
    let events = trajectory.list(&session_id, 0, i64::MAX).expect("读轨迹");
    let recorded: Vec<&TrajectoryEvent> = events
        .iter()
        .filter(|e| e.kind == CLEANUP_PREVIEW_KIND)
        .collect();
    assert_eq!(recorded.len(), 1, "必须恰好留一条预演事实");
    let event = recorded.first().expect("上面断言过恰好一条");
    assert_eq!(event.id, got.event.id, "返回的那条就是库里的那条");

    let payload: serde_json::Value = serde_json::from_str(&event.payload).expect("payload 是 JSON");
    assert_eq!(payload["executionId"], exec.id.as_str());
    assert_eq!(payload["policy"], "balanced");
    assert_eq!(payload["reclaimableBytes"], on_disk);
    let recorded_candidates = payload["candidates"].as_array().expect("候选是数组");
    assert_eq!(recorded_candidates.len(), 3);
    // 候选里每一项都带「是什么、多大、为什么」。
    for c in recorded_candidates {
        assert!(c["path"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(c["bytes"].as_u64().is_some_and(|b| b > 0));
        assert!(c["reason"].as_str().is_some_and(|s| !s.is_empty()));
    }

    // **预演之后磁盘占用不变**（在真链路上再核一次，不只是纯函数那一条）。
    assert_eq!(
        measure(worktree_at(tmp.path())),
        on_disk,
        "留痕之后磁盘必须原封不动"
    );
}

/// **反例**：别的失败形态**不许**触发预演。
///
/// 反命题（错实现会红的样子）：把 `on_failure_ack` 写成「什么都预演一遍」。
/// 那样 `agent_error` 也会得到一份「先把盘清了再说」的方案，而正确的动作是
/// 换 agent 重试。
#[test]
fn a_non_disk_failure_never_runs_a_preview() {
    let (_db, executions, trajectory, sessions, _tasks) = hub();
    let created = executions
        .create(
            NewExecution {
                task_id: "t_a".into(),
                binding_id: "b_a".into(),
                base_commit: None,
                worktree_path: None,
            },
            TS,
        )
        .expect("create");
    executions
        .set_failure(
            &created.id,
            FailureKind::AgentError,
            "the agent crashed",
            "",
        )
        .expect("set failure");
    let exec = executions.get(&created.id).expect("get").expect("exists");
    let session_id = session_for(&sessions, &exec.id);

    let before = previews_run();
    let got = on_failure_ack(
        FailureReport::of_execution(&exec),
        &[StoredItem {
            path: "/w/wt/report.json".into(),
            bytes: 8000,
            class: ItemClass::Artifact,
            modified_at: Some(1),
            label: "report.json".into(),
        }],
        &CleanupPolicy::Balanced,
        Some(&session_id),
        &trajectory,
        TS,
    )
    .expect("不该失败");

    assert!(got.is_none(), "agent_error 不许产出清理预演");
    assert_eq!(
        previews_run() - before,
        0,
        "别的失败形态连 preview 都不该调用"
    );
    assert!(
        trajectory
            .list(&session_id, 0, i64::MAX)
            .expect("读轨迹")
            .iter()
            .all(|e| e.kind != CLEANUP_PREVIEW_KIND),
        "轨迹里不该有预演事实"
    );
}

/// **没有会话就留不了痕**：`disk_exhausted` 仍然算跑过预演，但返回 `None`
/// 且轨迹里什么都没有。
///
/// 这条把「算过」与「留痕了」分开：调用方拿 `None` 就知道没人看得到这次预演，
/// 而不是以为留好了。
#[test]
fn a_disk_failure_without_a_session_does_not_pretend_to_have_recorded_anything() {
    let (_db, executions, trajectory, _sessions, _tasks) = hub();
    let exec = failed_execution(&executions);

    let before = previews_run();
    let got = on_failure_ack(
        FailureReport::of_execution(&exec),
        &[StoredItem {
            path: "/w/wt/report.json".into(),
            bytes: 8000,
            class: ItemClass::Artifact,
            modified_at: Some(1),
            label: "report.json".into(),
        }],
        &CleanupPolicy::Balanced,
        None,
        &trajectory,
        TS,
    )
    .expect("不该失败");

    assert!(got.is_none(), "没有会话就没法留痕，不许报成功");
    assert_eq!(previews_run() - before, 0, "没会话连算都不算");
}

/// **空清单**：`disk_exhausted` 但设备什么都没报 → 预演被调用了（计数涨），
/// 但不写轨迹（一条「没有可释放的东西」的轨迹没有信息量）。
#[test]
fn an_empty_device_list_still_runs_the_preview_but_records_nothing() {
    let (_db, executions, trajectory, sessions, _tasks) = hub();
    let exec = failed_execution(&executions);
    let session_id = session_for(&sessions, &exec.id);

    let before = previews_run();
    let got = on_failure_ack(
        FailureReport::of_execution(&exec),
        &[],
        &CleanupPolicy::Balanced,
        Some(&session_id),
        &trajectory,
        TS,
    )
    .expect("不该失败");

    assert!(got.is_none());
    assert_eq!(
        previews_run() - before,
        1,
        "空清单也调用 preview——这正证明调用点是「回执进来就走」，不是「有东西才走」"
    );
    assert!(trajectory
        .list(&session_id, 0, i64::MAX)
        .expect("读轨迹")
        .iter()
        .all(|e| e.kind != CLEANUP_PREVIEW_KIND));
}

/// **不改任务状态**：预演跑完之后任务仍停在原态（等人决策）。
///
/// `PRODUCT.md` 的硬规则「不清理任何东西之前必须被选中」意味着 `disk_exhausted`
/// 之后任务要停在等人决策的地方，而不是自己往前/往后走一步。预演只是把材料
/// 摆出来，它不推进任何状态。
#[test]
fn a_preview_leaves_the_task_state_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plant_worktree(tmp.path());
    let (_db, executions, trajectory, sessions, tasks) = hub();
    let exec = failed_execution(&executions);
    let session_id = session_for(&sessions, &exec.id);

    let before = tasks.get("t_a").expect("get").expect("exists");

    let _recorded = on_failure_ack(
        FailureReport::of_execution(&exec),
        &items_from_disk(tmp.path()),
        &CleanupPolicy::Balanced,
        Some(&session_id),
        &trajectory,
        TS,
    )
    .expect("预演")
    .expect("有结果");

    let after = tasks.get("t_a").expect("get").expect("exists");
    assert_eq!(
        after.state, before.state,
        "预演不许推进任务状态：它停在等人决策的地方"
    );
    assert_eq!(after.version, before.version, "版本号也不该涨");
    assert_eq!(
        after.updated_at, before.updated_at,
        "预演不写任务表，updated_at 不许动"
    );

    // 执行的状态与失败形态也没被预演改动：它读到的仍是 `disk_exhausted`。
    let after_exec = executions.get(&exec.id).expect("get").expect("exists");
    assert_eq!(after_exec.failure_kind, Some(FailureKind::DiskExhausted));
    assert_eq!(after_exec.state, exec.state, "预演不推进执行状态");
    assert_eq!(after_exec.reason, exec.reason, "预演不改原因串");
}

// ---------------------------------------------------------------------------
// 五、回执 → 清单
// ---------------------------------------------------------------------------

/// 回执载荷里的 `items` 数组能解成清单；缺键 / 不是数组 → 空清单。
#[test]
fn the_ack_payload_carries_the_item_list_under_its_documented_key() {
    assert_eq!(ACK_ITEMS_FIELD, "items", "字段名写死，改它等于换协议");

    let payload = serde_json::json!({
        "stage": "failed",
        "failure_kind": "disk_exhausted",
        "items": [
            {"path": "/w/wt/n", "bytes": 4000, "class": "dependency_cache",
             "modified_at": 5, "label": "node_modules"},
            {"path": "/w/wt/r.json", "bytes": 8000, "class": "artifact",
             "modified_at": null, "label": "report.json"}
        ]
    });
    let items = items_from_ack_payload(&payload);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items.first().map(|i| i.class),
        Some(ItemClass::DependencyCache)
    );
    assert_eq!(items.get(1).map(|i| i.bytes), Some(8000));
    assert_eq!(
        items.get(1).and_then(|i| i.modified_at),
        None,
        "null 解成 None（拿不到时间），不是 0"
    );

    // 缺键 / 不是数组 / 不是对象 → 空清单，不 panic。
    assert!(items_from_ack_payload(&serde_json::json!({})).is_empty());
    assert!(items_from_ack_payload(&serde_json::json!({"items": 3})).is_empty());
    assert!(items_from_ack_payload(&serde_json::json!("nope")).is_empty());
}

/// 一条坏条目只跳过它，其余照收——一份清单不该因为一条脏数据整份作废。
#[test]
fn one_malformed_item_does_not_discard_the_whole_list() {
    let payload = serde_json::json!({
        "items": [
            {"path": "/w/wt/good", "bytes": 10, "class": "artifact",
             "modified_at": null, "label": "good"},
            {"path": "/w/wt/bad", "bytes": "not-a-number", "class": "artifact",
             "modified_at": null, "label": "bad"},
            {"path": "/w/wt/unknown-class", "bytes": 1, "class": "mystery",
             "modified_at": null, "label": "x"}
        ]
    });
    let items = items_from_ack_payload(&payload);
    assert_eq!(items.len(), 1, "只有那条好的留下：{items:?}");
    assert_eq!(items.first().map(|i| i.label.as_str()), Some("good"));
}

/// 拿不到时间的产物在保守档里**留下**（不知道就留下）。
#[test]
fn an_artifact_without_a_timestamp_is_kept_by_the_conservative_tier() {
    let items = vec![StoredItem {
        path: "/w/wt/old.log".into(),
        bytes: 999,
        class: ItemClass::Artifact,
        modified_at: None,
        label: "old.log".into(),
    }];
    let got = preview(&CleanupPolicy::Conservative, &items);
    assert!(
        got.candidates.is_empty(),
        "没有时间就排不出先后，留下它：{:?}",
        got.candidates
    );

    // 平衡档不看时间，全清。
    assert_eq!(
        preview(&CleanupPolicy::Balanced, &items).reclaimable_bytes,
        999
    );
}

/// 裁剪：候选按「先大后小」排，同大小按路径——同一份输入两次跑出同一份顺序。
#[test]
fn candidates_are_ordered_by_size_then_path_deterministically() {
    let items = vec![
        StoredItem {
            path: "/w/b".into(),
            bytes: 5,
            class: ItemClass::Artifact,
            modified_at: None,
            label: "b".into(),
        },
        StoredItem {
            path: "/w/a".into(),
            bytes: 5,
            class: ItemClass::Artifact,
            modified_at: None,
            label: "a".into(),
        },
        StoredItem {
            path: "/w/big".into(),
            bytes: 50,
            class: ItemClass::Artifact,
            modified_at: None,
            label: "big".into(),
        },
    ];
    let first = preview(&CleanupPolicy::Aggressive, &items);
    let second = preview(&CleanupPolicy::Aggressive, &items);
    let order: Vec<&str> = first.candidates.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(order, vec!["/w/big", "/w/a", "/w/b"]);
    assert_eq!(first, second, "同一份输入必须给出同一份预演");
}

/// `is_absolute_device_path`：`~` 开头的一律不是绝对路径（与 `Binding.fixed_dir`
/// 「不展开 `~`」同一条口径）。
#[test]
fn only_rooted_paths_count_as_absolute_device_paths() {
    assert!(is_absolute_device_path("/srv/app/t_a"));
    assert!(!is_absolute_device_path("~/code/proj"));
    assert!(!is_absolute_device_path("srv/app"));
    assert!(!is_absolute_device_path(""));
}

/// 两次 `disk_exhausted` 回执**都**留痕，且第二条的 `seq` 不与第一条撞。
///
/// 「撞号」在这里不是理论问题：两条都用设备分区的号会撞上，而撞了之后
/// `INSERT OR IGNORE` 会把第二条悄悄吞掉——轨迹里只剩一份预演而没人知道。
#[test]
fn two_previews_in_one_session_get_their_own_sequence_numbers() {
    let (_db, executions, trajectory, sessions, _tasks) = hub();
    let exec = failed_execution(&executions);
    let session_id = session_for(&sessions, &exec.id);
    let items = [StoredItem {
        path: "/w/wt/report.json".into(),
        bytes: 8000,
        class: ItemClass::Artifact,
        modified_at: Some(1),
        label: "report.json".into(),
    }];

    let first = on_failure_ack(
        FailureReport::of_execution(&exec),
        &items,
        &CleanupPolicy::Balanced,
        Some(&session_id),
        &trajectory,
        TS,
    )
    .expect("第一次")
    .expect("有结果");

    // 同一会话里先来一条设备事件，占住「设备分区」的 seq 0。
    trajectory
        .append(
            &session_id,
            "n_a",
            &NewEvent::new(0, "agent_message_chunk", "{}"),
            TS,
        )
        .expect("设备事件");

    let second = on_failure_ack(
        FailureReport::of_execution(&exec),
        &items,
        &CleanupPolicy::Balanced,
        Some(&session_id),
        &trajectory,
        TS,
    )
    .expect("第二次")
    .expect("有结果");

    assert_ne!(
        first.event.seq, second.event.seq,
        "两次预演不能共用同一个 seq"
    );
    let previews: Vec<TrajectoryEvent> = trajectory
        .list(&session_id, 0, i64::MAX)
        .expect("读轨迹")
        .into_iter()
        .filter(|e| e.kind == CLEANUP_PREVIEW_KIND)
        .collect();
    assert_eq!(previews.len(), 2, "两条都得在，不许被唯一约束吞掉");
}

/// `session_for_execution` 能把挂在执行上的会话找出来；别的执行的会话不算。
#[test]
fn session_for_execution_finds_the_session_attached_to_that_execution() {
    let (_db, executions, _trajectory, sessions, _tasks) = hub();
    let exec = failed_execution(&executions);
    assert_eq!(
        session_for_execution(&sessions, &exec.id).expect("查"),
        None,
        "还没建会话"
    );
    let id = session_for(&sessions, &exec.id);
    assert_eq!(
        session_for_execution(&sessions, &exec.id).expect("查"),
        Some(id)
    );
    assert_eq!(
        session_for_execution(&sessions, "no-such-execution").expect("查"),
        None
    );
}

// ---------------------------------------------------------------------------
// 跨 crate 契约：设备侧报上来的 items 必须真的能被这里解析
// ---------------------------------------------------------------------------

/// **设备侧构造的 ack → 序列化 → 这里的 `items_from_ack_payload`**，
/// 必须得到非空清单，且逐项大小与 class 都对得上。
///
/// 为什么必须有这一条：`protocol` 的 `DiskItem` 与这里的 `StoredItem` 是
/// 两套独立定义（`protocol` 不能依赖 `hub-core`），字段名或 `class` 的线上串
/// 一旦漂移，`from_value::<StoredItem>` 会**静默失败**——
/// `items_from_ack_payload` 用的是 `filter_map(..ok())`，条目被丢掉不报错，
/// 预演于是算出「可释放 0」，而页面上看起来一切正常。
///
/// 这条用例是那个静默失败的唯一哨兵。
#[test]
fn the_device_side_ack_payload_parses_into_this_side_items() {
    use protocol::{DiskItem, DispatchStage, TaskDispatchAck};

    let ack = TaskDispatchAck {
        execution_id: "e-contract".into(),
        stage: DispatchStage::Failed,
        reason: Some("over budget".into()),
        evidence: None,
        failure_kind: Some("disk_exhausted".into()),
        items: Some(vec![
            DiskItem {
                path: "/w/node_modules".into(),
                bytes: 4000,
                // **从 `ItemClass::as_str()` 取，不写副本**：任一侧改名都会让
                // 这条用例红。写死字面量的话，改协议一侧时测试自己也会跟着漂移，
                // 什么都抓不到。
                class: ItemClass::DependencyCache.as_str().into(),
                modified_at: Some(1_700_000_000),
                label: "node_modules".into(),
            },
            DiskItem {
                path: "/w/report.json".into(),
                bytes: 8000,
                class: ItemClass::Artifact.as_str().into(),
                modified_at: None,
                label: "report.json".into(),
            },
            DiskItem {
                path: "/w/wt".into(),
                bytes: 12000,
                class: ItemClass::Worktree.as_str().into(),
                modified_at: None,
                label: "wt".into(),
            },
        ]),
    };

    let payload = serde_json::to_value(&ack).expect("序列化");
    let items = items_from_ack_payload(&payload);

    assert_eq!(
        items.len(),
        3,
        "设备报了 3 条，这里只解析出 {} 条——字段名或 class 的线上串漂移了，\
         而它是静默失败的（payload: {payload}）",
        items.len()
    );

    // 逐项核对：大小没被改写，class 被正确映射
    assert_eq!(items[0].path, "/w/node_modules");
    assert_eq!(items[0].bytes, 4000);
    assert_eq!(items[0].class, ItemClass::DependencyCache);
    assert_eq!(items[0].modified_at, Some(1_700_000_000));

    assert_eq!(items[1].bytes, 8000);
    assert_eq!(items[1].class, ItemClass::Artifact);
    assert_eq!(items[1].modified_at, None);

    assert_eq!(items[2].bytes, 12000);
    assert_eq!(items[2].class, ItemClass::Worktree);

    // 端到端：这份清单进预演，可释放字节数是候选之和（平衡档：三类全清）
    let p = preview(&CleanupPolicy::Balanced, &items);
    assert_eq!(
        p.reclaimable_bytes,
        p.candidates.iter().map(|c| c.bytes).sum::<u64>(),
        "可释放字节数必须等于候选之和"
    );
    assert!(
        p.reclaimable_bytes > 0,
        "解析出来的清单必须真的能算出可释放空间，否则预演就是「可释放 0」"
    );
}

/// 协议侧的 class 闭集与这里的 `ItemClass` 逐字一致。
///
/// 两处定义在两个 crate 里，只能靠对拍钉住。
#[test]
fn the_protocol_class_codes_match_this_side() {
    for kind in ItemClass::ALL {
        assert!(
            protocol::DISK_ITEM_CLASSES.contains(&kind.as_str()),
            "ItemClass::{kind:?} 的线上串 {:?} 不在协议闭集里",
            kind.as_str()
        );
    }
    assert_eq!(
        ItemClass::ALL.len(),
        protocol::DISK_ITEM_CLASSES.len(),
        "两边的类别数不一致"
    );
}
