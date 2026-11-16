//! 任务状态机：`task.state` 六态之间哪些转移合法。
//!
//! 规格真相源是 `docs/architecture/task-model.md:89-103`（状态图 + 三条硬规则）。
//! 本模块是该表的**唯一定义处**：`TaskRepo::set_state` 与 HTTP 层都调它，
//! 不允许在别处再写一份「如果 state 是 X 才能变成 Y」。
//!
//! # 与 `is_terminal` 的区别（下一轮最容易踩的坑）
//!
//! `TaskState::is_terminal` 回答的是「**还需不需要自动推进**」，不是
//! 「**还能不能变**」。`failed` 是终态（不会再自动往下走），但 `failed → pending`
//! 与 `failed → queued` 都是**合法**的人工动作。所以：
//!
//! - 不要拿 `is_terminal()` 去挡转移；
//! - 也不要因为「`failed` 能变」就认为 `is_terminal()` 写错了。
//!
//! 两个概念不同，各自服务不同的调用方。

use crate::error::{Result, StorageError};
use crate::task::TaskState;

/// 一条合法转移：`from → to`，附规格依据与给人看的原因。
///
/// **逐条列出，不写通配**。写成 `from == Running => true` 这样的规则会让
/// 「`running` 还能去哪」变成一个需要推理的问题，而它应该是一眼能数出来的。
struct Edge {
    from: TaskState,
    to: TaskState,
    /// `task-model.md` 的行号，说明这条边从哪来。
    spec: &'static str,
}

/// 合法转移表，共 10 条。**这是唯一定义处。**
///
/// 按 `from` 分组读：
/// - `pending` → `queued`（调度器选中；本里程碑由 API 手动推进）
/// - `pending` → `failed`（**人主动撤回 = 关闭**，`reason` 必填）
/// - `queued`  → `pending`（设备一直不接活、超时退回——**不是失败**）
/// - `queued`  → `running`（设备接活）
/// - `running` → `review`（分支产出）
/// - `running` → `failed`（含 `interrupted` / `disk_exhausted`）
/// - `review`  → `done`（验收通过 + 已合入）
/// - `review`  → `pending`（**打回不是失败**）
/// - `failed`  → `pending`（人工回待办 / 误关重开）
/// - `failed`  → `queued`（放宽约束重派）
const EDGES: &[Edge] = &[
    Edge {
        from: TaskState::Pending,
        to: TaskState::Queued,
        spec: "task-model.md:90",
    },
    Edge {
        from: TaskState::Pending,
        to: TaskState::Failed,
        spec: "task-model.md:95",
    },
    Edge {
        from: TaskState::Queued,
        to: TaskState::Pending,
        spec: "task-model.md:103",
    },
    Edge {
        from: TaskState::Queued,
        to: TaskState::Running,
        spec: "task-model.md:90",
    },
    Edge {
        from: TaskState::Running,
        to: TaskState::Review,
        spec: "task-model.md:90",
    },
    Edge {
        from: TaskState::Running,
        to: TaskState::Failed,
        spec: "task-model.md:97",
    },
    Edge {
        from: TaskState::Review,
        to: TaskState::Done,
        spec: "task-model.md:90",
    },
    Edge {
        from: TaskState::Review,
        to: TaskState::Pending,
        spec: "task-model.md:101",
    },
    Edge {
        from: TaskState::Failed,
        to: TaskState::Pending,
        spec: "task-model.md:95",
    },
    Edge {
        from: TaskState::Failed,
        to: TaskState::Queued,
        spec: "task-model.md:81-85",
    },
];

/// `from → to` 是不是合法转移。
///
/// # 规格里画了、本轮**不实现**的一条边
///
/// `task-model.md:93` 的状态图里确有 `queued ──→ failed` 的箭头。但同一节的
/// `:103` 文字明确写「`queued` 超时可退回 `pending`，**不是** `failed`」。
/// 两者不冲突：图表达的是「**排队期间被判定为不可执行**」这种终局（设备侧
/// 明确拒绝该任务，或人工在排队时直接关掉），而**不是超时**。
///
/// 本里程碑没有调度器、也没有设备会拒接，`queued` 之后只可能是超时；因此
/// 这里**只放 `queued → pending`**，`queued → failed` 留到 v0.8.2 引入
/// 调度器时随「设备拒接」一起加。
///
/// 对应的 API 取舍：`POST /failure` 的 `action: "close"` 在 `queued` 态
/// **先走 `queued → pending` 再关单**（关单用的是 `pending → failed`），
/// 不新增 `queued → failed` 边。
pub fn can_transition(from: TaskState, to: TaskState) -> bool {
    EDGES.iter().any(|e| e.from == from && e.to == to)
}

/// 校验转移，不合法时给出**能指导下一步**的错误。
///
/// 错误信息必须含「应该先走哪个态」——`running → done` 要说 `review`，
/// `review → failed` 要说 `pending`。只说「非法操作」等于把问题丢回给用户。
pub fn check_transition(from: TaskState, to: TaskState) -> Result<()> {
    if can_transition(from, to) {
        return Ok(());
    }
    Err(StorageError::IllegalTaskTransition {
        from: from.as_str(),
        to: to.as_str(),
        hint: legal_next_hint(from),
    })
}

/// `from` 的所有合法去处，写成一句人话。
///
/// **必须同时带英文态名**：前端与 API 的错误 `detail` 用的是 `pending` /
/// `review` 这类字面值，提示里只写中文「待验收」会让调用方对不上号。
fn legal_next_hint(from: TaskState) -> &'static str {
    match from {
        TaskState::Pending => "待办只能 `pending → queued`（进排队），或填写原因后 `pending → failed`（关闭）",
        TaskState::Queued => "排队中只能 `queued → running`（设备接活），或超时 `queued → pending` 退回待办；超时不是失败",
        TaskState::Running => {
            "执行中只能 `running → review`（产出进待验收），或执行失败后走失败处置；\
             不能直接到 `done`——跳过 `review` 就是跳过价值锚点"
        }
        TaskState::Review => {
            "待验收只能 `review → done`（合入完成），或 `review → pending`（打回待办）；\
             不能到 `failed`——打回不是失败"
        }
        TaskState::Done => "`done` 是终态，没有任何出口",
        TaskState::Failed => "失败任务只能 `failed → pending`（回待办）或 `failed → queued`（改条件后重派）",
    }
}

/// 本模块的边表，供测试与文档核对使用。
///
/// 返回 `(from, to)` 对，顺序与 `EDGES` 一致。
pub fn legal_edges() -> Vec<(TaskState, TaskState)> {
    EDGES.iter().map(|e| (e.from, e.to)).collect()
}

/// 一条边的规格依据，供测试断言「每条边都能指回文档」。
pub fn spec_of(from: TaskState, to: TaskState) -> Option<&'static str> {
    EDGES
        .iter()
        .find(|e| e.from == from && e.to == to)
        .map(|e| e.spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [TaskState; 6] = [
        TaskState::Pending,
        TaskState::Queued,
        TaskState::Running,
        TaskState::Review,
        TaskState::Done,
        TaskState::Failed,
    ];

    /// 36 组逐个走一遍，合法集合**恰好**是表里的 10 条。
    ///
    /// 判红方式：往 `EDGES` 里多写一条边，或删掉一条，这条立刻红——
    /// 因为它比的是两个集合，不是「至少包含」。
    #[test]
    fn only_the_ten_legal_edges_are_allowed() {
        let expected: Vec<(TaskState, TaskState)> = vec![
            (TaskState::Pending, TaskState::Queued),
            (TaskState::Pending, TaskState::Failed),
            (TaskState::Queued, TaskState::Pending),
            (TaskState::Queued, TaskState::Running),
            (TaskState::Running, TaskState::Review),
            (TaskState::Running, TaskState::Failed),
            (TaskState::Review, TaskState::Done),
            (TaskState::Review, TaskState::Pending),
            (TaskState::Failed, TaskState::Pending),
            (TaskState::Failed, TaskState::Queued),
        ];

        let mut actual = Vec::new();
        for from in ALL {
            for to in ALL {
                if can_transition(from, to) {
                    actual.push((from, to));
                }
            }
        }
        assert_eq!(actual.len(), 10, "合法边必须恰好 10 条，实际 {actual:?}");
        // 按集合比，不按发现顺序比：`ALL` 的枚举顺序是实现细节，
        // 让它变一下就红属于测试自己脆，不是 bug。
        let mut actual_sorted = actual.clone();
        let mut expected_sorted = expected.clone();
        actual_sorted.sort_by_key(|(f, t)| (f.as_str(), t.as_str()));
        expected_sorted.sort_by_key(|(f, t)| (f.as_str(), t.as_str()));
        assert_eq!(actual_sorted, expected_sorted);
    }

    /// `running` 不能直接到 `done`：跳过 `review` 就是跳过价值锚点。
    ///
    /// 错误信息必须含 `review`——用户要知道**下一步做什么**，不是只知道自己错了。
    #[test]
    fn running_cannot_jump_to_done() {
        let e = check_transition(TaskState::Running, TaskState::Done).unwrap_err();
        let msg = e.to_string();
        assert!(
            matches!(e, StorageError::IllegalTaskTransition { .. }),
            "{e:?}"
        );
        assert!(msg.contains("review"), "错误信息要指出先走 review：{msg}");
    }

    /// `review` 不能到 `failed`：打回进 `pending`，打回不是失败。
    #[test]
    fn review_cannot_go_to_failed_a_reject_is_not_a_failure() {
        let e = check_transition(TaskState::Review, TaskState::Failed).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("pending"),
            "错误信息要指出打回进 pending：{msg}"
        );
    }

    /// `queued` 超时只能退 `pending`，不能直接退 `failed`。
    ///
    /// 规格 `:93` 画了 `queued ─→ failed`，但那是「设备拒接」的终局，
    /// 本里程碑没有调度器也没有设备拒接，故不实现（理由见 `can_transition` 文档）。
    #[test]
    fn queued_cannot_go_to_failed() {
        let e = check_transition(TaskState::Queued, TaskState::Failed).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("pending"), "错误信息要指出退回 pending：{msg}");
    }

    /// `failed` 的两条出口都合法。
    ///
    /// 注释写在这里是给下一轮看的：**`is_terminal` 是「不再自动推进」，
    /// 不是「不可再变」**。`failed` 是终态，但人工可以把它拉回来。
    #[test]
    fn failed_can_go_back_to_pending_and_to_queued() {
        assert!(can_transition(TaskState::Failed, TaskState::Pending));
        assert!(can_transition(TaskState::Failed, TaskState::Queued));
        // 同时确认它确实是「终态」——两个概念并存，不矛盾。
        assert!(TaskState::Failed.is_terminal());
    }

    /// `done` 是真终态：没有任何出口。
    #[test]
    fn done_has_no_outgoing_edges() {
        assert!(TaskState::Done.is_terminal());
        for to in ALL {
            assert!(
                !can_transition(TaskState::Done, to),
                "done 不该能变成 {to:?}"
            );
        }
    }

    /// 六个态各自的 `hint` 都要能说出来，且**必须带英文态名**。
    ///
    /// 只写「非法操作」等于把问题丢回给用户：他不知道下一步该做什么。而只写
    /// 中文「待验收」会让调用方对不上 API 的 `detail.from` / `detail.to`
    /// （那两个是 `pending` / `review` 这类字面值）。
    ///
    /// 判红方式：把任一 `legal_next_hint` 臂改成空串 → `!hint.is_empty()` 红；
    /// 改成只写中文 → 「含至少一个英文态名」那句红。
    #[test]
    fn every_state_has_a_hint_that_names_a_state() {
        // 每个态挑一条非法边，凑齐六个 `from`。
        let illegal = [
            (TaskState::Pending, TaskState::Running),
            (TaskState::Queued, TaskState::Failed),
            (TaskState::Running, TaskState::Done),
            (TaskState::Review, TaskState::Failed),
            (TaskState::Done, TaskState::Pending),
            (TaskState::Failed, TaskState::Review),
        ];
        let mut seen = Vec::new();
        for (from, to) in illegal {
            let e = check_transition(from, to).unwrap_err();
            let StorageError::IllegalTaskTransition { hint, .. } = &e else {
                panic!("{from:?} → {to:?} 必须报 IllegalTaskTransition：{e:?}");
            };
            assert!(
                !hint.is_empty(),
                "{from:?} 的提示是空串——用户只知道「错了」，不知道下一步"
            );
            assert!(
                ALL.iter().any(|s| hint.contains(s.as_str())),
                "{from:?} 的提示里没有任何英文态名，前端对不上 detail.from/detail.to：{hint}"
            );
            seen.push(from);
        }
        // 六个 `from` 一个不漏：漏一个就有一条 hint 臂永远没人读。
        seen.sort_by_key(|s| s.as_str());
        let mut all = ALL.to_vec();
        all.sort_by_key(|s| s.as_str());
        assert_eq!(seen, all, "六个态必须各覆盖一次");
    }

    /// 每条边都能指回规格行号——没有「凭感觉加的边」。
    ///
    /// **不止比前缀**：早先这条只断言 `starts_with("task-model.md:")`，于是把任意
    /// 锚点改成 `task-model.md:99999` 仍然全绿。现在把每条锚点指的那一行真的读
    /// 出来，并要求它含有能认出这条边的字样（`expected`），锚点漂移即红。
    #[test]
    fn every_edge_cites_the_spec() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/architecture/task-model.md"),
        )
        .expect("规格文档必须可读");
        let lines: Vec<&str> = doc.lines().collect();

        // 每条边「凭什么成立」的那一行里，必须出现的字样。
        let expectations: &[((TaskState, TaskState), &str)] = &[
            // 图里的主链一行写出四段转移。
            (
                (TaskState::Pending, TaskState::Queued),
                "pending ──→ queued",
            ),
            (
                (TaskState::Queued, TaskState::Running),
                "queued ──→ running",
            ),
            (
                (TaskState::Running, TaskState::Review),
                "running ──→ review",
            ),
            ((TaskState::Review, TaskState::Done), "review ──→ done"),
            // 人主动撤回那条支线。
            ((TaskState::Pending, TaskState::Failed), "人主动撤回"),
            // 失败是人工的三个出口。
            ((TaskState::Failed, TaskState::Pending), "人主动撤回"),
            ((TaskState::Failed, TaskState::Queued), "放宽约束重派"),
            // `running` 掉线这条：图里只有箭头，真值在下面那行散文。
            ((TaskState::Running, TaskState::Failed), "设备掉线"),
            // 两条「退回」各有自己的散文行。
            (
                (TaskState::Review, TaskState::Pending),
                "退回时进 `pending`",
            ),
            (
                (TaskState::Queued, TaskState::Pending),
                "超时可退回 `pending`",
            ),
        ];

        for (from, to) in legal_edges() {
            let spec =
                spec_of(from, to).unwrap_or_else(|| panic!("{from:?} → {to:?} 缺少规格依据"));
            let n: usize = spec
                .strip_prefix("task-model.md:")
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("{from:?} → {to:?} 的锚点格式不对：{spec}"));
            assert!(
                n >= 1 && n <= lines.len(),
                "{from:?} → {to:?} 的锚点 {spec} 越界（文档共 {} 行）",
                lines.len()
            );
            let (_, expected) = expectations
                .iter()
                .find(|((f, t), _)| *f == from && *t == to)
                .unwrap_or_else(|| panic!("{from:?} → {to:?} 没有登记期望字样"));
            // 锚点是一段区间时，区间内任意一行含有期望字样即可。
            let range = spec.strip_prefix("task-model.md:").unwrap();
            let (lo, hi) = match range.split_once('-') {
                Some((a, b)) => (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()),
                None => (n, n),
            };
            let hit = (lo..=hi).any(|i| lines[i - 1].contains(expected));
            assert!(
                hit,
                "{from:?} → {to:?} 的锚点 {spec} 指向的行里没有 `{expected}`——\
                 锚点漂了或这条边是凭感觉加的。实际内容：{:?}",
                lines[n - 1]
            );
        }
    }

    /// 打回与撤回都能对回待办，但依据不同（`:101` vs `:95`）——
    /// 防止有人把两条边合并成一条「任意态 → 待办」。
    #[test]
    fn reject_and_close_have_distinct_spec_lines() {
        let reject = spec_of(TaskState::Review, TaskState::Pending).unwrap();
        let close = spec_of(TaskState::Pending, TaskState::Failed).unwrap();
        assert_ne!(reject, close);
        assert_eq!(reject, "task-model.md:101", "打回的依据");
        assert_eq!(close, "task-model.md:95", "人主动撤回的依据");
    }
}
