//! 轨迹仓储：ACP 的 `session/update` 事件流（append-only）。
//!
//! # 「完整」在这个模块里是什么意思
//!
//! **本模块不承诺轨迹完整，它承诺让不完整可见。** 这两件事差别很大，
//! 而混淆它们会让页面把一段缺了中间的对话渲染成看起来连续的一整段。
//!
//! 三层，各自能做什么：
//!
//! 1. **不重不漏**（[`TrajectoryRepo::append`]）：`UNIQUE(session_id, node_id, seq)`
//!    让同一事件写两遍被数据库拒掉。这是**幂等**，与完整无关——重连重发不会
//!    产生重复行，但对「漏了第 7 条」毫无感知。
//! 2. **不完整可见**（[`TrajectoryRepo::gaps`]）：`seq` 是设备侧只增的会话内序号，
//!    所以**断裂处就是丢失处**。查询时按 `seq` 比对相邻值，不连续即报出区间。
//!    「本来就只有 6 条」与「有 7 条但第 7 条丢了」在库里长得很像（都是 seq 到 6），
//!    **靠的是 [`TrajectoryRepo::turn_bounds`]** 区分——见第 3 层。
//! 3. **一轮有没有走完**（[`TurnMark`]）：`session/prompt` 的 response 带 `stopReason`
//!    （实测 `end_turn`），设备侧据此写一条 `turn_end` 事件。有它才能说
//!    「这一轮跑到第 5 条就断了」而不只是「后面没有了」。
//!
//! # 为什么不试图补齐
//!
//! ACP **没有「重发第 N 条通知」的机制**。`session/load` 的重放实测是**重建流**
//! （同进程实测 live 9 条 → replay 4 条，中间态丢失，且重放事件没有 `seq`、
//! 没有时间戳）。所以断线窗口内产生的事件**永久丢失**，这不是实现选择，
//! 是协议能力边界。里程碑已裁决选 (c)「不承诺」，而不是在设备上加持久队列。
//!
//! 本模块的职责因此收窄成：**把丢失的事实记录下来，让页面画得出来。**

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};

use crate::error::Result;
use crate::Db;

/// 一轮对话的结束标记 `kind`。
///
/// 设备侧在 `session/prompt` 的 response 到达时写这一条，`payload` 里带
/// `stopReason`（实测取值 `end_turn`）。**这是「这一轮走完了」的唯一可靠凭据**：
/// chunk 流自己不会说它结束了。
pub const TURN_END_KIND: &str = "turn_end";

/// `trajectory_event` 的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryEvent {
    pub id: i64,
    pub session_id: String,
    pub node_id: String,
    /// 设备侧会话内序号，只增。**断裂处即丢失处**。
    pub seq: i64,
    /// 事件类型。**不加白名单**：新版本 agent 会推我们不认识的事件，
    /// 丢弃等于让它们静默消失。
    pub kind: String,
    /// 原始 JSON（输入 + 输出），原样存。
    pub payload: String,
    /// 耗时。**结构性无耗时时为 `None`**——实测 ACP 事件全都不带时间戳，
    /// 拿「收到时刻」相邻差分得到的是**事件间隔**而不是**执行耗时**。
    pub duration_ms: Option<i64>,
    /// `duration_ms` 的来源（`measured` / `none`），让审计看得懂这一列。
    pub duration_source: String,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub parent_event_id: Option<i64>,
    pub created_at: String,
}

/// 一段序号空洞：`[from, to)` 区间内的 seq 都不在库里。
///
/// 用半开区间是因为相邻两条都**在**：`seq` 1 和 3 都在、2 不在，则 `from=2, to=3`。
/// 写成闭区间会让「差 1」变成两个端点相等，读的人容易算错条数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub from: i64,
    pub to: i64,
}

impl Gap {
    /// 缺了几条。**这是页面上要显示的数字**，所以单列一个方法而不是让调用方算。
    pub fn missing(&self) -> i64 {
        (self.to - self.from).max(0)
    }
}

/// 一轮对话的范围（按 `turn_end` 切）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBounds {
    /// 本轮第一条的 seq。
    pub first_seq: i64,
    /// 本轮 `turn_end` 的 seq；`None` 表示**这一轮没有走完**。
    pub end_seq: Option<i64>,
}

fn row_to_event(r: &Row<'_>) -> std::result::Result<TrajectoryEvent, r2d2_sqlite::rusqlite::Error> {
    Ok(TrajectoryEvent {
        id: r.get(0)?,
        session_id: r.get(1)?,
        node_id: r.get(2)?,
        seq: r.get(3)?,
        kind: r.get(4)?,
        payload: r.get(5)?,
        duration_ms: r.get(6)?,
        duration_source: r.get(7)?,
        tokens_in: r.get(8)?,
        tokens_out: r.get(9)?,
        parent_event_id: r.get(10)?,
        created_at: r.get(11)?,
    })
}

const COLUMNS: &str = "id, session_id, node_id, seq, kind, payload, duration_ms, \
                       duration_source, tokens_in, tokens_out, parent_event_id, created_at";

/// 一条待写入的事件。
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub seq: i64,
    pub kind: String,
    pub payload: String,
    pub duration_ms: Option<i64>,
    pub duration_source: String,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub parent_event_id: Option<i64>,
}

impl NewEvent {
    /// 一条普通事件：无耗时（结构性无），计时来源标 `none`。
    pub fn new(seq: i64, kind: impl Into<String>, payload: impl Into<String>) -> Self {
        Self {
            seq,
            kind: kind.into(),
            payload: payload.into(),
            duration_ms: None,
            duration_source: "none".into(),
            tokens_in: None,
            tokens_out: None,
            parent_event_id: None,
        }
    }

    /// 一轮的结束标记。`stop_reason` 原样存进 payload。
    pub fn turn_end(seq: i64, stop_reason: &str) -> Self {
        Self::new(
            seq,
            TURN_END_KIND,
            serde_json::json!({ "stopReason": stop_reason }).to_string(),
        )
    }
}

/// `trajectory_event` 的读写。
#[derive(Debug, Clone)]
pub struct TrajectoryRepo {
    pool: Db,
}

impl TrajectoryRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 写入一条事件。**重复的 `(session, node, seq)` 被忽略，不报错**。
    ///
    /// # 为什么用 `INSERT OR IGNORE` 而不是「先查再插」
    ///
    /// 先查再插在并发与重连下是 TOCTOU：两个任务可能都查到「不存在」，然后
    /// 都去插。而设备重连时重发一批未确认的事件是**正常行为**（协议里纯推送
    /// 帧不重传，但设备侧重连后可能重发自己缓存的那几条），所以重复不是错误，
    /// 是预期内的事。唯一约束是数据库层的能力，就让它在那里生效。
    ///
    /// 返回 `true` 表示真的写进去了，`false` 表示那条 seq 已经在了。
    pub fn append(&self, session_id: &str, node_id: &str, e: &NewEvent, now: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO trajectory_event
                 (session_id, node_id, seq, kind, payload, duration_ms, duration_source,
                  tokens_in, tokens_out, parent_event_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                session_id,
                node_id,
                e.seq,
                e.kind,
                e.payload,
                e.duration_ms,
                e.duration_source,
                e.tokens_in,
                e.tokens_out,
                e.parent_event_id,
                now
            ],
        )?;
        Ok(n > 0)
    }

    /// 按物理顺序（= `id` 升序）读一个会话的轨迹。
    ///
    /// **回放顺序是物理顺序，不是 `seq` 顺序**：`seq` 是设备侧数的，重连后
    /// 会从 0 重数（ACP 子进程重启了），所以 `seq` 只在**一次进程生命周期内**
    /// 单调。物理顺序才是「事情真实发生的顺序」。
    pub fn list(&self, session_id: &str, after_id: i64, limit: i64) -> Result<Vec<TrajectoryEvent>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM trajectory_event
             WHERE session_id = ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![session_id, after_id, limit], row_to_event)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 一个会话的事件总数。
    pub fn count(&self, session_id: &str) -> Result<i64> {
        let conn = self.pool.get()?;
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM trajectory_event WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// **序号断裂区间**——「哪些地方丢了事件」。
    ///
    /// # 判据与它不管的事
    ///
    /// `seq` 是设备侧只增的，所以相邻两条的差 > 1 就是中间丢了。本方法只报
    /// **段内**断裂，不报「开头缺」（库里从 seq 5 开始说明前 4 条没到，但那
    /// 也可能是「这个会话的轨迹从 seq 5 才开始记」，两者分不开）。
    ///
    /// **也不报「末尾缺」**：`seq` 到 6 而实际有 7 条，与「本来就只有 6 条」
    /// 在本方法看来一样。末尾的完整性靠 [`Self::turn_bounds`]——那一轮的
    /// `turn_end` 在不在，才是末尾有没有丢的判据。
    pub fn gaps(&self, session_id: &str) -> Result<Vec<Gap>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT seq FROM trajectory_event WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let seqs: Vec<i64> = stmt
            .query_map(params![session_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::new();
        for w in seqs.windows(2) {
            // windows(2) 保证有两个元素，但生产代码禁裸索引，用解构。
            if let [a, b] = w {
                if b - a > 1 {
                    out.push(Gap {
                        from: a + 1,
                        to: *b,
                    });
                }
            }
        }
        Ok(out)
    }

    /// 按 `turn_end` 切出一轮一轮的范围。
    ///
    /// 每条 `turn_end` 收掉它之前所有还没有归属的事件。最后一个 `turn_end`
    /// 之后若还有事件，那一段是**尚未走完（或已丢失结尾）的一轮**，
    /// `end_seq` 为 `None`。
    pub fn turn_bounds(&self, session_id: &str) -> Result<Vec<TurnBounds>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT seq, kind FROM trajectory_event WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out: Vec<TurnBounds> = Vec::new();
        let mut open: Option<i64> = None;
        for (seq, kind) in rows {
            if open.is_none() {
                open = Some(seq);
            }
            if kind == TURN_END_KIND {
                if let Some(first) = open.take() {
                    out.push(TurnBounds {
                        first_seq: first,
                        end_seq: Some(seq),
                    });
                }
            }
        }
        if let Some(first) = open {
            out.push(TurnBounds {
                first_seq: first,
                end_seq: None,
            });
        }
        Ok(out)
    }

    /// 会话最近一条事件的 seq。没有事件时为 `None`。
    pub fn last_seq(&self, session_id: &str) -> Result<Option<i64>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                "SELECT MAX(seq) FROM trajectory_event WHERE session_id = ?1",
                params![session_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, TrajectoryRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        (d, TrajectoryRepo::new(pool))
    }

    fn chunk(seq: i64, text: &str) -> NewEvent {
        NewEvent::new(
            seq,
            "agent_message_chunk",
            serde_json::json!({ "content": { "type": "text", "text": text } }).to_string(),
        )
    }

    #[test]
    fn events_are_stored_and_read_back_in_write_order() {
        let (_d, t) = setup();
        t.append("s1", "n1", &chunk(1, "PO"), TS).unwrap();
        t.append("s1", "n1", &chunk(2, "NG"), TS).unwrap();

        let rows = t.list("s1", 0, 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[1].seq, 2);
        assert_eq!(rows[0].kind, "agent_message_chunk");
        assert_eq!(rows[0].duration_ms, None, "ACP 事件不带时间戳");
        assert_eq!(rows[0].duration_source, "none");
    }

    /// **同一 seq 写两次不产生两行，且第二次报 `false`**。
    ///
    /// 反向验证：把 `INSERT OR IGNORE` 改成 `INSERT` → 这条会因唯一约束报错而红。
    #[test]
    fn writing_the_same_seq_twice_is_ignored_not_duplicated() {
        let (_d, t) = setup();
        assert!(t.append("s1", "n1", &chunk(1, "a"), TS).unwrap());
        assert!(
            !t.append("s1", "n1", &chunk(1, "a"), TS).unwrap(),
            "第二次应报 false"
        );
        assert_eq!(t.count("s1").unwrap(), 1, "不许有两行");
    }

    /// **唯一约束的键含 node_id**：同一会话不同节点各自数 seq，不能互相吞掉。
    #[test]
    fn the_same_seq_from_a_different_node_is_a_different_event() {
        let (_d, t) = setup();
        assert!(t.append("s1", "n1", &chunk(1, "a"), TS).unwrap());
        assert!(
            t.append("s1", "n2", &chunk(1, "b"), TS).unwrap(),
            "换一台设备，seq 1 是另一件事"
        );
        assert_eq!(t.count("s1").unwrap(), 2);
    }

    // ---- 断裂检测 ----

    #[test]
    fn a_contiguous_stream_has_no_gaps() {
        let (_d, t) = setup();
        for i in 1..=5 {
            t.append("s1", "n1", &chunk(i, "x"), TS).unwrap();
        }
        assert!(t.gaps("s1").unwrap().is_empty());
    }

    /// **中间丢掉一条必须被报出来**，并说清缺了几条。
    ///
    /// 这是整个模块存在的主要理由：没有它，「缺了第 3 条」与「本来就只有
    /// 4 条」在库里长得一模一样。
    #[test]
    fn a_missing_event_in_the_middle_is_reported_as_a_gap() {
        let (_d, t) = setup();
        for i in [1, 2, 4, 5] {
            t.append("s1", "n1", &chunk(i, "x"), TS).unwrap();
        }
        let gaps = t.gaps("s1").unwrap();
        assert_eq!(gaps.len(), 1, "应有 1 段断裂：{gaps:?}");
        assert_eq!(gaps[0].from, 3, "缺的是 seq 3");
        assert_eq!(gaps[0].to, 4);
        assert_eq!(gaps[0].missing(), 1, "页面要显示「缺 1 条」");
    }

    #[test]
    fn several_separate_gaps_are_all_reported() {
        let (_d, t) = setup();
        for i in [1, 3, 6] {
            t.append("s1", "n1", &chunk(i, "x"), TS).unwrap();
        }
        let gaps = t.gaps("s1").unwrap();
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert_eq!(gaps[0].missing(), 1, "seq 2");
        assert_eq!(gaps[1].missing(), 2, "seq 4,5");
    }

    /// 一个会话的断裂不影响另一个。
    #[test]
    fn gaps_are_scoped_per_session() {
        let (_d, t) = setup();
        for i in [1, 3] {
            t.append("s1", "n1", &chunk(i, "x"), TS).unwrap();
        }
        for i in [1, 2] {
            t.append("s2", "n1", &chunk(i, "x"), TS).unwrap();
        }
        assert_eq!(t.gaps("s1").unwrap().len(), 1);
        assert!(t.gaps("s2").unwrap().is_empty());
    }

    // ---- 一轮的完整性 ----

    /// 有 `turn_end` 的一轮 = 走完了。
    #[test]
    fn a_turn_with_its_end_marker_is_complete() {
        let (_d, t) = setup();
        t.append("s1", "n1", &chunk(1, "a"), TS).unwrap();
        t.append("s1", "n1", &chunk(2, "b"), TS).unwrap();
        t.append("s1", "n1", &NewEvent::turn_end(3, "end_turn"), TS)
            .unwrap();

        let turns = t.turn_bounds("s1").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].first_seq, 1);
        assert_eq!(turns[0].end_seq, Some(3), "这一轮走完了");
    }

    /// **没有 `turn_end` 的一轮 = 没走完**（或结尾丢了）。
    ///
    /// 这是「末尾缺了」的唯一判据——`gaps()` 看不见末尾。
    #[test]
    fn a_turn_without_its_end_marker_is_reported_as_incomplete() {
        let (_d, t) = setup();
        t.append("s1", "n1", &chunk(1, "a"), TS).unwrap();
        t.append("s1", "n1", &chunk(2, "b"), TS).unwrap();

        let turns = t.turn_bounds("s1").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].end_seq, None,
            "没有 turn_end 就是没走完，页面要能画出来"
        );
    }

    /// 多轮：每一轮各自收口，最后一轮未收口。
    #[test]
    fn several_turns_are_split_by_their_end_markers() {
        let (_d, t) = setup();
        t.append("s1", "n1", &chunk(1, "t1a"), TS).unwrap();
        t.append("s1", "n1", &NewEvent::turn_end(2, "end_turn"), TS)
            .unwrap();
        t.append("s1", "n1", &chunk(3, "t2a"), TS).unwrap();
        t.append("s1", "n1", &NewEvent::turn_end(4, "end_turn"), TS)
            .unwrap();
        t.append("s1", "n1", &chunk(5, "t3a"), TS).unwrap();

        let turns = t.turn_bounds("s1").unwrap();
        assert_eq!(turns.len(), 3, "{turns:?}");
        assert_eq!(turns[0].end_seq, Some(2));
        assert_eq!(turns[1].end_seq, Some(4));
        assert_eq!(turns[2].end_seq, None, "第三轮还在跑或断了");
    }

    /// `turn_end` 的 payload 里要有 `stopReason` —— 那是「为什么结束」的唯一记录。
    #[test]
    fn the_turn_end_marker_carries_the_stop_reason() {
        let (_d, t) = setup();
        t.append("s1", "n1", &NewEvent::turn_end(1, "end_turn"), TS)
            .unwrap();
        let rows = t.list("s1", 0, 10).unwrap();
        let v: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(v["stopReason"], serde_json::json!("end_turn"));
    }

    /// 未知事件类型必须原样存下来（新版本 agent 会推我们不认识的事件）。
    #[test]
    fn an_unknown_event_kind_is_stored_verbatim() {
        let (_d, t) = setup();
        let e = NewEvent::new(1, "some_future_event", r#"{"whatever":true}"#);
        t.append("s1", "n1", &e, TS).unwrap();
        let rows = t.list("s1", 0, 10).unwrap();
        assert_eq!(rows[0].kind, "some_future_event");
        assert_eq!(rows[0].payload, r#"{"whatever":true}"#);
    }

    #[test]
    fn last_seq_is_none_for_an_empty_session_then_tracks_the_maximum() {
        let (_d, t) = setup();
        assert_eq!(t.last_seq("s1").unwrap(), None);
        t.append("s1", "n1", &chunk(4, "a"), TS).unwrap();
        t.append("s1", "n1", &chunk(7, "b"), TS).unwrap();
        assert_eq!(t.last_seq("s1").unwrap(), Some(7));
    }

    /// 分页：`after_id` 是物理 id，不是 seq。
    #[test]
    fn listing_pages_by_physical_id() {
        let (_d, t) = setup();
        for i in 1..=5 {
            t.append("s1", "n1", &chunk(i, "x"), TS).unwrap();
        }
        let first = t.list("s1", 0, 2).unwrap();
        assert_eq!(first.len(), 2);
        let last_id = first.last().map(|e| e.id).unwrap_or(0);
        let second = t.list("s1", last_id, 10).unwrap();
        assert_eq!(second.len(), 3);
        assert_eq!(second[0].seq, 3);
    }
}
