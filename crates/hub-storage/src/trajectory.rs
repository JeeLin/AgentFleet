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

/// 一轮对话的开始标记 `kind`。
///
/// 设备在一轮的工作开始之前、从与 `batch` / `turn_end` **同一条**设备侧计数器
/// 取号写这一条。它存在的理由只有一个：`turn_end` 缺席有两种含义——「还在跑」
/// 与「丢了」——而只看 `turn_end` 分不开。有了开始标记，「开过且没关」才是
/// 一个可查的事实（见 [`TrajectoryRepo::turn_bounds`]）。
///
/// 它**不**证明 agent 还活着：开始标记写下来之后设备掉了，这一轮同样是「开过
/// 且没关」。
pub const TURN_START_KIND: &str = "turn_start";

/// `duration_source` 的取值：**设备真的掐了表**（`duration_ms` 非 `NULL`）。
///
/// 常量而不是散在的字符串字面量：`none` 与 `measured` 是一对互斥的凭据，
/// 写错一个字母的行在库里照样存得下（schema 的 `CHECK` 只认这两个词，拼错
/// 会在**插入时**才炸，那时这一轮已经跑完了）。散写的字面量在 grep 里也找
/// 不全——本模块此前 `measured` 零赋值点，正是因为只有一处注释提到它。
pub const MEASURED: &str = "measured";

/// `duration_source` 的取值：**没被计时**。此时 `duration_ms` 必须是 `NULL`。
///
/// 这条配对（`none` ⇒ `NULL`）由 `scripts/e2e-m2.sh` 的耗时列断言与
/// [`NewEvent::new`] 的构造共同守住：本 crate 只写这两个值，不写第三种。
pub const NONE: &str = "none";

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
    /// 耗时。**没被计时就必须是 `None`**。
    ///
    /// 「没被计时」有两种，都落 `None`，`duration_source` 才分得开：
    /// - **结构性无耗时**：实测 ACP 事件全都不带时间戳，拿「收到时刻」相邻
    ///   差分得到的是**事件间隔**而不是**执行耗时**。
    /// - **没测到**：设备**有**计时（`session/prompt` 的往返），但这次没拿到
    ///   凭据（超时 / 出错），于是没有可写的值。
    ///
    /// 两种都不许填 0：那会把「没有数」说成「0 毫秒」。
    pub duration_ms: Option<i64>,
    /// `duration_ms` 的来源，让审计看得懂这一列。
    ///
    /// - `measured`：**设备真的掐了表**（`session/prompt` 发出到响应回来）。
    ///   `duration_ms` 此刻非 `NULL`，`0` 也是诚实的值（一条瞬间返回的命令）。
    /// - `none`：没被计时。`duration_ms` **必须** 是 `NULL` —— 一条
    ///   `duration_ms = 0, source = none` 的行是「一面说没测、一面给个数」。
    pub duration_source: String,
    /// token 记账。**当前恒 `NULL`**：没有任何生产者填它——ACP 的
    /// `usage_update` 带 `used`/`size`/`cost`，那是**上下文窗口的占用量**，
    /// 不是「这一轮的输入/输出 token」，把前者写进这两列会是假数据。
    /// 页面的用量提示直接读 `usage_update`，不经过这里。
    pub tokens_in: Option<i64>,
    /// 见 [`Self::tokens_in`]。
    pub tokens_out: Option<i64>,
    /// 子 agent 事件挂到父事件上。**当前恒 `NULL`**：Hub 只观测 agent 运行时
    /// 内部的父子关系（`AGENTS.md` 约束 8），不编排它，所以没有可信的父子
    /// 映射可写。留着这一列是给「将来 ACP 给出父子关系」用的槽位，
    /// **不读它判任何事**。
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
    /// 本轮第一条的 seq。**不一定是 `turn_start`**：它只是本轮的物理第一条。
    pub first_seq: i64,
    /// 本轮第一条 `turn_start` 事件的 seq；`None` 表示本轮**没有开始标记**。
    ///
    /// `None` 有两种来路，本模块分不开、也不假装分得开：这次改动之前写下的
    /// 历史行（设备那时还不会发 `turn_start`），或者这一轮的开始标记丢了。
    pub start_seq: Option<i64>,
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

/// 这个 kind 是「状态快照」吗——即记一遍就够、重复的没有信息量。
///
/// # 白名单而不是黑名单
///
/// 只把**已知**的快照型事件列进来。写「除了消息 chunk 之外都算快照」会让新
/// 版本 agent 推来的未知事件默认被按内容去重，而那些事件可能是真实发生的事
/// （`tool_call` 之类）。宁可多一行，也不要因为不认识就吞掉一条。
///
/// 目前只有 `available_commands_update`：实测 `session/load` 回执之后 opencode
/// 会再推一份同样的命令列表，与 load 之前那条**剔除 `seq` 后逐字相同**。
///
/// `usage_update` **不在**里面：它带 `used` / `cost` 计数，两份「内容相同」
/// 只是恰好同值，而它的意义是「到此刻为止用了多少」——按内容去重会让计数
/// 长时间不变的那段时间看起来像「没有事件」。
///
/// # 往这里加一个 kind 之前必须确认它不带 `messageId`
///
/// 第 3 层去重的 SQL 是 `... AND json_extract(payload, '$.messageId') IS NULL`：
/// 带 `messageId` 的行归第 2 层管（按消息 id 去重），第 3 层**主动跳过**它们
/// 以免两层抢同一批行。所以一个既在快照白名单里、又带 `messageId` 的 kind
/// 会两位不管——同一份快照换了新 `seq` 就会双写。
/// 今天没有这种 kind（`available_commands_update` 不带 `messageId`），
/// 加新的那条时按这句核一遍即可。
fn is_state_snapshot(kind: &str) -> bool {
    matches!(kind, "available_commands_update")
}

/// 把 payload 里的 `seq` 剔掉，用于「状态快照内容是否相同」的比对。
///
/// `seq` 是设备侧编号，每份快照的都不一样；带上它比对永远不相等。剔掉之后
/// 剩下的才是「这份快照描述的状态」。
///
/// 返回 `None` 表示 payload 不是 JSON 对象（数组、标量、坏 JSON）：那种情况下
/// **不做内容去重**，照原样落库。理由与 [`message_id_of`] 相同——看不懂就不
/// 假装看懂了，宁可多一行也不要因为解析失败而丢掉一条真实事件。
fn payload_without_seq(payload: &str) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match &mut v {
        serde_json::Value::Object(o) => {
            o.remove("seq");
            // `BTreeMap` 序列化键有序，所以两份 JSON 只要内容相同就得到同一串。
            Some(serde_json::to_string(&v).ok()?)
        }
        _ => None,
    }
}

/// 从事件 payload 里取 agent 给的 `messageId`；没有、或不是非空字符串，就是 `None`。
///
/// 只认**非空**字符串：空串在 JSON 里是合法值，但拿它当消息身份会把两条
/// 本来无关的事件判成重复（`json_extract` 返回 `''` 与「没有这个键」在
/// `= ?` 比较里都命中空串）。
fn message_id_of(payload: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(v) => match v.get("messageId").and_then(|m| m.as_str()) {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        },
        // payload 不是合法 JSON 时**不因此拒写**：`append` 的契约是「存原始
        // JSON」，校验形状是调用方的事。这里只是拿不到消息身份，退化成
        // 只按 `seq` 去重——与加这一层之前的行为一致。
        Err(_) => None,
    }
}

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
    /// 一条普通事件：**没有计时**，来源标 [`NONE`]，`duration_ms` 为 `None`。
    ///
    /// 这条是「结构性无耗时」与「这次没测到」共同的落点：两者在库里都必须是
    /// `none` + `NULL`，而区分它们的信息本来就不在这一行上。
    pub fn new(seq: i64, kind: impl Into<String>, payload: impl Into<String>) -> Self {
        Self {
            seq,
            kind: kind.into(),
            payload: payload.into(),
            duration_ms: None,
            duration_source: NONE.into(),
            tokens_in: None,
            tokens_out: None,
            parent_event_id: None,
        }
    }

    /// 一轮的结束标记。`stop_reason` 原样存进 payload。
    ///
    /// **这一条不带计时**：`stopReason` 只说「这一轮怎么结束的」，没说花了多久。
    /// 带计时的那一版看 [`Self::measured_turn_end`]。
    pub fn turn_end(seq: i64, stop_reason: &str) -> Self {
        Self::new(
            seq,
            TURN_END_KIND,
            serde_json::json!({ "stopReason": stop_reason }).to_string(),
        )
    }

    /// 一轮的开始标记。**不带计时**：这一条在轮次开始时写下，此刻还不知道
    /// 这一轮要跑多久（耗时只有 `turn_end` 上才有，见 [`Self::measured_turn_end`]）。
    ///
    /// `payload` 里也写一份 `seq`：轨迹表按 `seq` 列判断裂，而页面只看得见
    /// payload——两处都有，画图的那一方才不用回头查列。
    pub fn turn_start(seq: i64) -> Self {
        Self::new(
            seq,
            TURN_START_KIND,
            serde_json::json!({ "seq": seq }).to_string(),
        )
    }

    /// **设备掐过表的一轮的结束标记**：`duration_ms` 是 `session/prompt` 发出到
    /// 响应回来的毫秒数，由设备侧测量后随 `turn_end` 帧送上来。
    ///
    /// # 为什么调用方传的是 `Option<i64>`，而不是一个 `i64`
    ///
    /// 「这一轮走完了」与「测到了它花了多久」不是同一件事：`stopReason` 是
    /// prompt 响应里的字段，而耗时只有**设备还活着把两个数都拿齐**时才存在。
    /// 拿不到（`None`）就退回 `none` —— 宁可少一列，也不许把「没测到」写成
    /// `measured` + `0`（那句话的意思是「它是零毫秒跑完的」）。
    ///
    /// # 为什么它单独一个构造器，而不是让调用方自己填两个字段
    ///
    /// `measured` 的意思就是「设备真的掐了表」。让调用方自己填两个字段，等于
    /// 让每一处各自决定「什么时候算是测过」，而那些决定会分叉——分叉的那一处
    /// 写出的行，与旁边那些长得一模一样、意思却相反。所以「值 + 来源」必须在
    /// 同一个函数体里成对产出：本模块此前 `measured` 零赋值点（全表恒 `none`）
    /// 正是这种「谁都能填、于是没人填」的形状。
    pub fn measured_turn_end(seq: i64, stop_reason: &str, duration_ms: Option<i64>) -> Self {
        let Some(ms) = duration_ms.filter(|v| *v >= 0) else {
            return Self::turn_end(seq, stop_reason);
        };
        Self {
            seq,
            kind: TURN_END_KIND.into(),
            // `durationMs` 与列一起存：payload 是给页面画的原始 JSON，而列是
            // 给审计查的。两处都写，是因为页面与审计读的不是同一处——只写列
            // 的话页面拿不到它，只写 payload 的话审计得解析 JSON。
            payload: serde_json::json!({ "stopReason": stop_reason, "durationMs": ms }).to_string(),
            duration_ms: Some(ms),
            duration_source: MEASURED.into(),
            tokens_in: None,
            tokens_out: None,
            parent_event_id: None,
        }
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

    /// 写入一条事件。
    ///
    /// 三层各管一种重复，**都需要**：
    ///
    /// 1. **`(session_id, node_id, seq)` 唯一约束**（`INSERT OR IGNORE`）——
    ///    「同一号写两遍」。这是数据库层的能力，不靠应用层先查再插：那在并发
    ///    与重连下是 TOCTOU（两个任务都查到「不存在」然后都去插），而设备重连
    ///    时重发一批未确认的事件是**正常行为**，重复不是错误。
    /// 2. **agent 的 `messageId`**——「同一消息换了新号」。
    /// 3. **状态快照按内容**——「同一份快照换了新号，且它没有消息身份」。
    ///
    /// # 第 2 层：重放事件按 agent 自己的 `messageId` 认
    ///
    /// `seq` 只增解决的是「新事件的号被旧事件占了」。它管不了**同一事件被
    /// 重发**：`session/load` 会把历史重放一遍，而重放的每条都带着**与实时
    /// 那条逐字相同的 `messageId`**（实测：实时 `seq=2` 与重放 `seq=6` 都是
    /// `msg_17af87a19001xcrudnts0tSVl3`）。续号之后它们不再撞号，于是全部
    /// 落库——轨迹里同一段回答出现两遍。
    ///
    /// 所以这里多查一次 `(session_id, kind, messageId)`：已经有过就跳过。
    /// 判据用 agent 自带的 `messageId` 而不是「payload 完全相等」，因为重放
    /// 的 payload 可能与实时**不完全一样**（`usage_update` 的计数、重放里
    /// 才出现的事件顺序），而 `messageId` 是 agent 对「这是同一条消息」的
    /// 明确声明。
    ///
    /// # 第 3 层：没有 `messageId` 的状态快照按内容去重
    ///
    /// `available_commands_update` 这类事件没有消息身份——它不是「一条消息」，
    /// 是「此刻可用的命令列表」。实测：`session/load` 的回执之后 opencode 会再
    /// 推一条同样的快照（内容与 load 之前那条**剔除 `seq` 后逐字相同**），于是
    /// 轨迹里多一行毫无信息量的重复。
    ///
    /// 判据 = **同会话、同 kind、剔除 `seq` 后 payload 相同**。`seq` 必须剔除：
    /// 它是设备侧编号，每次都不一样，带上它比对永远不相等。
    ///
    /// **只在 [`is_state_snapshot`] 白名单里的 kind 上生效**，不扩展到所有事件：
    /// 那会把「agent 说了两句一模一样的话」当成重复吞掉一条。
    ///
    /// 返回 `true` 表示真的写进去了，`false` 表示这条 seq 已经在了（或这条
    /// 消息/这份快照已经落过库）。
    pub fn append(&self, session_id: &str, node_id: &str, e: &NewEvent, now: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        // 第 2 层。**只有 agent 给了 `messageId` 才查**：拿空串去查会把「两条
        // 都没有 messageId 的事件」误判成重复，而那正是 `available_commands_update`
        // 这种合法重复事件。
        if let Some(mid) = message_id_of(&e.payload) {
            // **`node_id` 必须在判据里**：`session_id` 不是全局唯一的命名空间，
            // 同一张表按 `(node_id, …)` 分区（第 1 层的唯一约束与 `list()` 都带）。
            // 少了这一条，一台设备上的去重会跨设备咬人。
            let seen: i64 = conn.query_row(
                "SELECT COUNT(*) FROM trajectory_event
                 WHERE session_id = ?1 AND node_id = ?2 AND kind = ?3
                   AND json_extract(payload, '$.messageId') = ?4",
                params![session_id, node_id, e.kind, mid],
                |r| r.get(0),
            )?;
            if seen > 0 {
                return Ok(false);
            }
        } else if is_state_snapshot(&e.kind) {
            if let Some(norm) = payload_without_seq(&e.payload) {
                // 第 3 层。见上方文档。
                //
                // 在 SQL 里逐行取出来比对而不是用 `json_extract`：要剔掉 `seq`
                // 再比整份 JSON，SQLite 的 JSON 函数做不了「删一个键再整体比较」，
                // 而 `REPLACE(payload, ...)` 这种字符串替换会在值里恰好含 `"seq"`
                // 时错位。这一层的量级是「同一会话的同类快照」，不是全表扫描。
                let mut stmt = conn.prepare(
                    "SELECT payload FROM trajectory_event
                     WHERE session_id = ?1 AND node_id = ?2 AND kind = ?3
                       AND json_extract(payload, '$.messageId') IS NULL",
                )?;
                let rows = stmt.query_map(params![session_id, node_id, e.kind], |r| {
                    r.get::<_, String>(0)
                })?;
                for existing in rows {
                    let existing = existing?;
                    if payload_without_seq(&existing).as_deref() == Some(norm.as_str()) {
                        return Ok(false);
                    }
                }
            }
        }
        // **`INSERT OR IGNORE` 仍然是最后一道闸**（`seq` 的唯一约束）：
        // 先查再插在并发与重连下是 TOCTOU——两个任务可能都查到「不存在」，
        // 然后都去插。上面那两层查询挡的是「同一消息/同一快照换了新号」，
        // 这里挡的是「同一号写两遍」。三层各管一种重复，都需要。
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
    pub fn list(
        &self,
        session_id: &str,
        after_id: i64,
        limit: i64,
    ) -> Result<Vec<TrajectoryEvent>> {
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
        let mut stmt = conn
            .prepare("SELECT seq FROM trajectory_event WHERE session_id = ?1 ORDER BY seq ASC")?;
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
            // coverage: excluded — llvm-cov 区域产物：`if let [a, b]` 块尾的
            // 闭合括号被单独算作一个区域，块内语句均已覆盖。
        }
        Ok(out)
    }

    /// 按 `turn_end` 切出一轮一轮的范围。
    ///
    /// 每条 `turn_end` 收掉它之前所有还没有归属的事件。最后一个 `turn_end`
    /// 之后若还有事件，那一段是**尚未走完（或已丢失结尾）的一轮**，
    /// `end_seq` 为 `None`。
    ///
    /// # `start_seq` 是什么，不是什么
    ///
    /// 它是**本轮在这个区间里第一条 `turn_start` 的 seq**（`ORDER BY id` 下
    /// 首次出现的那条）。两种情形会得到 `None`：
    /// - 区间里没有开始标记——本次改动之前写下的历史行就是这样；
    /// - 开始标记按物理顺序落在了区间外：`seq` 由设备侧计数，重连后可能从 0
    ///   重数，所以「按 `seq` 排序」与「按写入顺序」不是同一个顺序。
    ///
    /// 取第一条而不是最后一条：库里若同时躺着多条开始标记，那是「开了两轮、
    /// 只关了一次」，中间那段衔接没有凭据；报最早的那条比报最新的一条更不
    /// 容易把一轮说短。
    ///
    /// 顺带把「本轮开始标记」的记账简化成了「遇到过就开始记」：每一条
    /// `turn_start` 都写，`turn_end` 之后清空。两者在「`turn_end` 之前出现
    /// 多条 `turn_start`」时只差取哪一条，而今天设备侧一轮只发一条——所以
    /// 没有测试能分开它们，这里写的只是最短的那份。
    pub fn turn_bounds(&self, session_id: &str) -> Result<Vec<TurnBounds>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT seq, kind FROM trajectory_event WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out: Vec<TurnBounds> = Vec::new();
        // 当前这一轮的 `first_seq`；它与 `start_seq` 各自记账：一轮可能没有
        // 开始标记（历史行），也可能开始标记不是它的物理第一条。
        let mut open: Option<i64> = None;
        let mut start: Option<i64> = None;
        for (seq, kind) in rows {
            if open.is_none() {
                open = Some(seq);
            }
            if kind == TURN_START_KIND {
                start = Some(seq);
            }
            if kind == TURN_END_KIND {
                if let Some(first) = open.take() {
                    out.push(TurnBounds {
                        first_seq: first,
                        start_seq: start.take(),
                        end_seq: Some(seq),
                    });
                }
                // `turn_end` 一并收掉本轮的开始标记：它在下一轮之前是唯一的
                // 收口点，下一轮若没写开始标记，`start_seq` 必须是 `None` 而
                // 不是继承上一轮的。
                start = None;
            }
        }
        if let Some(first) = open {
            out.push(TurnBounds {
                first_seq: first,
                start_seq: start,
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

    /// **去重的两层都按 `(session_id, node_id)` 判定，不能只按 `session_id`**。
    ///
    /// `session_id` 不是全局命名空间——Hub 侧连通道都按 `(node_id, channel_id)`
    /// 记账（`stream.rs`）。少了 `node_id` 这层过滤，设备 A 写过的同名快照会把
    /// 设备 B 的同一条判成重复而丢掉：两台设备各自的轨迹互相吃掉。
    #[test]
    fn dedup_does_not_leak_across_nodes() {
        let (_d, t) = setup();
        let snap = r#"{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"init"}]}"#;
        let mk = |seq| NewEvent::new(seq, "available_commands_update", snap.to_string());

        // 第 3 层（状态快照按内容）：同一 session_id 下两台设备各自的快照都要落库。
        assert!(t.append("s1", "nodeA", &mk(1), TS).unwrap());
        assert!(
            t.append("s1", "nodeB", &mk(2), TS).unwrap(),
            "别的设备的同名快照不该把我这条吃掉"
        );

        // 第 2 层（按 `messageId`）：同理。
        let msg = || {
            NewEvent::new(
                3,
                "agent_message_chunk",
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "m1",
                    "content": {"type": "text", "text": "hi"},
                })
                .to_string(),
            )
        };
        assert!(t.append("s2", "nodeA", &msg(), TS).unwrap());
        assert!(
            t.append("s2", "nodeB", &msg(), TS).unwrap(),
            "别的设备的同 messageId 不该把我这条吃掉"
        );
    }

    /// 两个「看不懂就退化成不判」的兜底：payload **不是 JSON 对象**时
    /// 取不出消息身份，也剔不掉 `seq`。
    ///
    /// 这两条不是「不可能发生」：`append` 的契约是**存原始 payload**，形状校验
    /// 是调用方的事（见两个函数的注释）。坏形状要能落库，只是去重那一层
    /// 退化成「只按 seq」。若让它们 panic 或拒写，一条形状意外的事件就会
    /// 把整轮对话的轨迹写不进去——那比不去重严重得多。
    ///
    /// 反向验证：把 `payload_without_seq` 的 `_ => None` 改成
    /// `_ => Some(payload.to_string())`，本用例的红点会落在「非对象要返回 None」
    /// 那条断言上。
    #[test]
    fn a_payload_that_is_not_an_object_falls_back_to_no_identity_and_no_seq_strip() {
        // 合法 JSON，但不是对象：取不出 messageId，也剔不掉 seq。
        assert_eq!(message_id_of("[1,2,3]"), None, "数组没有 messageId");
        assert_eq!(message_id_of("42"), None, "标量没有 messageId");
        assert_eq!(
            payload_without_seq("[1,2,3]"),
            None,
            "非对象没法「剔除 seq 再比较」，只能退化成不去重"
        );

        // 压根不是 JSON。
        assert_eq!(
            message_id_of("not json"),
            None,
            "坏 JSON 不许拒写，只是拿不到身份"
        );
        assert_eq!(payload_without_seq("not json"), None);
    }

    /// 带 `messageId` 的 chunk——真机形状（`session/update` 里每条消息都带它）。
    fn chunk_with_mid(seq: i64, text: &str, mid: &str) -> NewEvent {
        NewEvent::new(
            seq,
            "agent_message_chunk",
            serde_json::json!({
                "content": { "type": "text", "text": text },
                "messageId": mid,
            })
            .to_string(),
        )
    }

    /// **`session/load` 的重放不产生重复行——按 agent 的 `messageId` 认。**
    ///
    /// 真机实测：实时那条是 `seq=2 / msg_17af87...`，重放那条是
    /// `seq=6 / msg_17af87...`（同一个 `messageId`，序号不同）。
    /// 续号之后它们不再撞 `seq` 的唯一约束，于是**必须**由这一层挡住，
    /// 否则同一段回答在轨迹里出现两遍（实测行数 4 → 7）。
    ///
    /// 反向验证：把 `append` 里那段 `messageId` 查询删掉，这条会红在
    /// 「重放不该产生第二行」。
    #[test]
    fn a_replayed_event_with_the_same_message_id_is_not_stored_twice() {
        let (_d, t) = setup();
        assert!(t
            .append("s1", "n1", &chunk_with_mid(2, "好的", "msg_A"), TS)
            .unwrap());
        assert!(
            !t.append("s1", "n1", &chunk_with_mid(6, "好的", "msg_A"), TS)
                .unwrap(),
            "同一个 messageId 换了序号再来一次，是重放，不该写第二行"
        );
        assert_eq!(t.count("s1").unwrap(), 1, "轨迹里只能有一条这条消息");

        // **不同 messageId 必须各自落库**：只按「文字相同」去重会把两次
        // 恰好说了同一句话的真实对话吞掉一条。
        assert!(t
            .append("s1", "n1", &chunk_with_mid(7, "好的", "msg_B"), TS)
            .unwrap());
        assert_eq!(t.count("s1").unwrap(), 2);
    }

    /// **没有 `messageId` 的事件不受那层去重影响**：`available_commands_update`
    /// 这种状态快照本来就没有消息身份，重放时再记一遍是合法的。
    ///
    /// 反向验证：把 `message_id_of` 的空串特判去掉（空串也当身份），
    /// 这条会红在「第二条被当成重复」。
    #[test]
    fn events_without_a_message_id_are_not_deduped_by_it() {
        let (_d, t) = setup();
        // 两份**内容不同**的快照（命令列表不一样）：都必须落库。
        // 这一条挡住「把状态快照一律当重复」那种过度去重——那样会把真实的状态
        // 变化吞掉，页面上的命令列表就永远停在第一次的样子。
        let snapshot = |seq: i64, cmd: &str| {
            NewEvent::new(
                seq,
                "available_commands_update",
                serde_json::json!({ "availableCommands": [{ "name": cmd }], "seq": seq })
                    .to_string(),
            )
        };
        assert!(t.append("s1", "n1", &snapshot(1, "init"), TS).unwrap());
        assert!(
            t.append("s1", "n1", &snapshot(5, "review"), TS).unwrap(),
            "内容不同的两份快照都要落库，否则状态变化会被吞掉"
        );
        assert_eq!(t.count("s1").unwrap(), 2);

        // **内容相同（剔除 seq 后）的快照只留一行**：实测 `session/load` 的回执
        // 之后 opencode 会再推一份同样的命令列表，那份是纯噪音。
        assert!(
            !t.append("s1", "n1", &snapshot(9, "review"), TS).unwrap(),
            "剔除 seq 后内容相同的快照是重复，不该再落一行"
        );
        assert_eq!(t.count("s1").unwrap(), 2);

        // `messageId: ""` 也不能被当成「同一条消息」：它没有身份，而
        // `agent_message_chunk` **不是**状态快照（见 `is_state_snapshot`），
        // 于是走不进内容去重那一路——内容不同的两条都要落库。
        let empty_mid = |seq: i64, text: &str| {
            NewEvent::new(
                seq,
                "agent_message_chunk",
                serde_json::json!({ "content": { "type": "text", "text": text }, "messageId": "" })
                    .to_string(),
            )
        };
        assert!(t.append("s1", "n1", &empty_mid(10, "x"), TS).unwrap());
        assert!(
            t.append("s1", "n1", &empty_mid(11, "y"), TS).unwrap(),
            "空串 messageId 不是消息身份，内容不同的两条都要落库"
        );
        // **内容相同也要落库**：这是「按内容去重只作用于快照」的证据。
        // 若把它扩展到所有事件，同一句话被说两次（agent 没给 messageId）
        // 就会被静默吞掉一条。
        assert!(
            t.append("s1", "n1", &empty_mid(12, "x"), TS).unwrap(),
            "非快照事件不做内容去重——重复的话也是两次真实发言"
        );
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
    ///
    /// 这一轮**没有** `turn_start`（历史行形状），所以 `start_seq` 是 `None`：
    /// 那条判据不能拿 `start_seq.is_some()` 反推「有没有走完」。
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
        assert_eq!(turns[0].start_seq, None, "这一轮没有开始标记");
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
        assert_eq!(
            turns[0].start_seq, None,
            "也没有开始标记，这一轮无从判断状态"
        );
    }

    /// **有开始标记、没有结束标记的一轮**：`start_seq` 有、`end_seq` 无。
    ///
    /// 这正是「一轮开了还没关」在库里的样子，也是路由把 `running` 与
    /// `incomplete` 分开报的唯一依据。反向验证：把 `turn_bounds` 里
    /// `start = Some(seq)` 那行删掉，这条会红在「start_seq 该是 Some(2)」。
    #[test]
    fn a_turn_with_a_start_marker_and_no_end_marker_reports_only_its_start() {
        let (_d, t) = setup();
        t.append("s1", "n1", &NewEvent::turn_start(1), TS).unwrap();
        t.append("s1", "n1", &chunk(2, "hello"), TS).unwrap();

        let turns = t.turn_bounds("s1").unwrap();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(turns[0].first_seq, 1);
        assert_eq!(
            turns[0].start_seq,
            Some(1),
            "开始标记写下来了，就必须读得回来：{turns:?}"
        );
        assert_eq!(turns[0].end_seq, None, "没有 turn_end 就是没关：{turns:?}");
    }

    /// **开始与结束都有的一轮两个都报**：`start_seq` 与 `end_seq` 各自是它们
    /// 自己的 seq，不许互相顶替。
    #[test]
    fn a_turn_with_both_markers_reports_both() {
        let (_d, t) = setup();
        t.append("s1", "n1", &NewEvent::turn_start(1), TS).unwrap();
        t.append("s1", "n1", &chunk(2, "hello"), TS).unwrap();
        t.append("s1", "n1", &NewEvent::turn_end(3, "end_turn"), TS)
            .unwrap();

        let turns = t.turn_bounds("s1").unwrap();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(turns[0].first_seq, 1);
        assert_eq!(turns[0].start_seq, Some(1), "{turns:?}");
        assert_eq!(turns[0].end_seq, Some(3), "{turns:?}");
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
        assert!(
            turns.iter().all(|t| t.start_seq.is_none()),
            "这三轮都没写开始标记：{turns:?}"
        );
    }

    /// **开始标记只属于它开的那一轮**：上一轮的开始标记不能被算进下一轮，
    /// 否则「上一轮已收口」会被读成「下一轮也开了」。
    #[test]
    fn a_start_marker_belongs_only_to_the_turn_it_opened() {
        let (_d, t) = setup();
        t.append("s1", "n1", &NewEvent::turn_start(1), TS).unwrap();
        t.append("s1", "n1", &chunk(2, "t1"), TS).unwrap();
        t.append("s1", "n1", &NewEvent::turn_end(3, "end_turn"), TS)
            .unwrap();
        t.append("s1", "n1", &chunk(4, "t2"), TS).unwrap();

        let turns = t.turn_bounds("s1").unwrap();
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].start_seq, Some(1), "{turns:?}");
        assert_eq!(turns[0].end_seq, Some(3), "{turns:?}");
        assert_eq!(
            turns[1].start_seq, None,
            "第二轮没写开始标记，不许继承上一轮的：{turns:?}"
        );
        assert_eq!(turns[1].first_seq, 4, "{turns:?}");
    }

    // ---- 耗时列的来源 ----

    /// **`measured` 真的被写出来**：带计时的那一轮结束标记必须把毫秒数落进
    /// `duration_ms` 那一列，而不是「这一列永远为 NULL」。
    ///
    /// 这条的存在理由就是那个缺陷本身：`measured` 此前在生产代码里**零赋值点**，
    /// 于是 `duration_ms` 恒为 NULL。反向验证：把构造器里的 `MEASURED` 改回
    /// `NONE`，这条会红在「来源该是 measured」。
    #[test]
    fn a_measured_turn_end_carries_its_duration_into_the_column() {
        let (_d, t) = setup();
        let e = NewEvent::measured_turn_end(4, "end_turn", Some(1_234));
        assert_eq!(e.duration_source, MEASURED, "这一列决定审计能不能信它");
        assert_eq!(e.duration_ms, Some(1_234), "毫秒数要进列，不只是进 payload");
        assert!(t.append("s1", "n1", &e, TS).unwrap());

        let rows = t.list("s1", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, TURN_END_KIND);
        assert_eq!(rows[0].duration_ms, Some(1_234), "读回来也得在");
        assert_eq!(rows[0].duration_source, "measured");
        // payload 里也有一份：页面拿的是 payload，审计查的是列，两处都要有。
        let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(payload["stopReason"], serde_json::json!("end_turn"));
        assert_eq!(payload["durationMs"], serde_json::json!(1_234));
    }

    /// **`0` 毫秒是诚实的值，不许被当成「没测」扔掉**。
    ///
    /// 反向验证：把 `filter(|v| *v >= 0)` 改成 `filter(|v| *v > 0)`，这条会红
    /// 在「0 是 measured，不是 none」。
    #[test]
    fn a_zero_duration_is_measured_not_missing() {
        let e = NewEvent::measured_turn_end(1, "end_turn", Some(0));
        assert_eq!(
            e.duration_source, MEASURED,
            "0 毫秒是「它是瞬间回来的」，不是「没人掐表」"
        );
        assert_eq!(e.duration_ms, Some(0));
    }

    /// **没测到就退回 `none` + NULL**：`turn_end` 是「这一轮走完了」的凭据，它
    /// 的存在不依赖耗时有没有测到——但不能为了看起来有数就填 0。
    ///
    /// 反向验证：把 `filter(|v| *v >= 0)` 删掉（`duration_ms` 直接 Some(d)），
    /// 这条会红在「None 不该被写成 measured」。
    #[test]
    fn a_turn_end_without_a_measurement_stays_none_and_null() {
        let e = NewEvent::measured_turn_end(1, "end_turn", None);
        assert_eq!(e.duration_source, NONE, "没测到就是没测到");
        assert_eq!(e.duration_ms, None, "不许填 0 冒充测过");
        // payload 里也就不该多出一个 `durationMs`：那会让页面看见一个
        // 「列上写没测、payload 里有数」的自相矛盾的事件。
        let payload: serde_json::Value = serde_json::from_str(&e.payload).unwrap();
        assert_eq!(payload["stopReason"], serde_json::json!("end_turn"));
        assert!(payload.get("durationMs").is_none(), "{payload}");

        // 负的毫秒数不是「测到了一段负的时间」，是上游算错了。
        let neg = NewEvent::measured_turn_end(2, "end_turn", Some(-5));
        assert_eq!(neg.duration_source, NONE, "负数不是测量值，是算错了");
        assert_eq!(neg.duration_ms, None);
    }

    /// **普通事件始终是 `none` + NULL**：没人为它们掐表，不许因为新出现了一个
    /// 带测量的构造器就顺手给它们带上 `measured`。
    #[test]
    fn an_ordinary_event_is_never_measured() {
        let e = NewEvent::new(1, "agent_message_chunk", r#"{"n":1}"#);
        assert_eq!(e.duration_source, NONE);
        assert_eq!(e.duration_ms, None);
        assert_eq!(NewEvent::turn_end(2, "end_turn").duration_source, NONE);
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
