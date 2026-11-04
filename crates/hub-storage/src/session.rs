//! 工作台会话仓储。
//!
//! 会话 = **一个设备上的一个工作区 + （M3 起）一个 agent**（设计规范 §5.3）。
//! 它不是登录 cookie：登录态的 HMAC 密钥在 `secret` 表里，两者是两回事
//! （M0 的 `session_table_is_deliberately_absent` 说的是后者）。
//!
//! 建会话**即挂载工作区**：一次事务里 upsert workspace + insert session
//! （`workspace.rs` 的 `upsert_in` 是同一条事务的另一半）。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{Result, StorageError};
use crate::workspace::{row_to_workspace_at, Workspace, WorkspaceRepo};
use crate::Db;

/// 会话状态。**只有两值**——与 `DeviceStatus` 同一个理由：
/// 「重连中」是页面上由心跳年龄自推的瞬时态，不落库（设计规范 §6.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    Ended,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ended => "ended",
        }
    }
}

impl FromStr for SessionState {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(Self::Running),
            "ended" => Ok(Self::Ended),
            other => Err(StorageError::InvalidSessionState(other.to_string())),
        }
    }
}

/// 会话起点为什么有（或没有）。
///
/// **五态，不是三态**。前四态由建会话时的探测产生，`Unknown` 只由迁移 v4 的
/// 列默认值产生——它是「v0.6.0 之前建的会话，那时还没有这个功能」。
///
/// **`Unknown` 与 `Unavailable` 绝不合并**：前者是历史事实（没什么可排查），
/// 后者是「试过了、设备没给」（有东西可看）。合并会把一个历史事实显示成一次
/// 故障，把用户引去查一个并不存在的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseRevStatus {
    /// 取到了，`base_rev` 是完整 hash。
    Ok,
    /// 工作区不是 git 仓库。
    NotARepo,
    /// 仓库还没有提交。
    NoHead,
    /// 问了但没拿到：设备离线 / 超时 / 通道打不开 / connector 没上报。
    Unavailable,
    /// 迁移前的老会话，从没记过。**只有迁移默认值产生这个值。**
    Unknown,
}

impl BaseRevStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotARepo => "not_a_repo",
            Self::NoHead => "no_head",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

impl FromStr for BaseRevStatus {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "ok" => Ok(Self::Ok),
            "not_a_repo" => Ok(Self::NotARepo),
            "no_head" => Ok(Self::NoHead),
            "unavailable" => Ok(Self::Unavailable),
            "unknown" => Ok(Self::Unknown),
            other => Err(StorageError::InvalidBaseRevStatus(other.to_string())),
        }
    }
}

/// 会话行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    /// 建会话时选定的 agent。**M2 起是必填项**（`POST /api/sessions` 缺它直接 400），
    /// 所以新行不会是 `None`；M2 之前建的老行仍是 `None`，那类会话无法对话
    /// （用户 2026-10-27 裁定：老行不补选，只约束新建）。
    pub agent_id: Option<String>,
    /// ACP 侧的会话 id（opencode 形如 `ses_...`）。**第一次开对话时为空**，
    /// 由设备侧 `session/new` 现取、Hub 收到 `ready` 帧后回填。
    ///
    /// 它存在只有一个用途：关掉页面再回来、或 Hub/worker 换代之后，下发给设备
    /// 让它走 `session/load` 而不是 `session/new`。**实测（M2 里程碑文档 V-2）：
    /// 新进程 load 同一 id 之后 agent 记得上一轮的内容**；直接拿同一 id 去
    /// `session/prompt` 不成立。所以这个字段决定「对话还能不能接上」。
    ///
    /// 没有 agent 的会话恒 `None`。
    #[serde(default)]
    pub acp_session_id: Option<String>,
    pub state: SessionState,
    pub created_at: String,
    /// `running` 时恒 `None`。
    pub ended_at: Option<String>,
    /// 会话建立那一刻工作区的 HEAD（完整 40 位）。`None` = 没有起点，
    /// 原因看 `base_rev_status`。
    ///
    /// **完整 hash 而不是短 hash**：短 hash 的长度会随仓库规模增长而变化
    /// （`core.abbrev` 与对象数共同决定），拿它当基准的话同一个提交在不同
    /// 时间可能给出不同的串。
    pub base_rev: Option<String>,
    /// 起点为什么有／没有（五态）。见 [`BaseRevStatus`]。
    #[serde(default = "unknown_base_rev")]
    pub base_rev_status: BaseRevStatus,
}

/// `base_rev_status` 的反序列化缺省值。
///
/// 取 `Unknown` 而不是 `Ok`：反序列化缺这个字段时，最保守的解释是
/// 「我们不知道」，不是「它是好的」。
fn unknown_base_rev() -> BaseRevStatus {
    BaseRevStatus::Unknown
}

/// 会话 + 它的 `(node_id, path)`。工作区是只读定位（建会话时选定即锁定），
/// 路径**不存在会话行上**——那是 `workspace_and-sync.md` §2.1 的原话：
/// 「路径属于设备工作区，不属于会话」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: Session,
    pub workspace: Workspace,
}

/// 会话行的一行**原始列**。
///
/// 状态解析刻意留在行映射之外：`query_row` 的行映射签名只收
/// `rusqlite::Error`，在里面做业务校验会把 `InvalidSessionState` 包成一个
/// 说不清的 `FromSqlConversionFailure`，调用方再也分不清「库里没有这一行」
/// 与「库被改坏了」。
pub(crate) struct RawSession {
    id: String,
    workspace_id: String,
    agent_id: Option<String>,
    acp_session_id: Option<String>,
    state: String,
    created_at: String,
    ended_at: Option<String>,
    base_rev: Option<String>,
    base_rev_status: String,
}

impl RawSession {
    /// 从行映射里的 `offset` 列起读**九列**会话列。
    ///
    /// 联表查询（工作区 × 会话）用它把两段列拼回各自的形状：SQL 层选出来的
    /// 列是平铺的，而这里是强类型的，两者靠**列序**对齐，所以那几条联表 SQL
    /// 的 SELECT 列表必须与下面这九个 `r.get` 逐字同序。
    ///
    /// **v0.6.0 从七列扩到九列**（加 `base_rev` / `base_rev_status`）。凡是
    /// 按 `offset` 往后取工作区列的地方（`detail` 的 `row_to_workspace_at`、
    /// `list_sessions` 的 `r.get`），偏移都要同步 +2——写错的话读到的是隔壁
    /// 那个**类型也对得上**的列，于是静默错位而不是报错。
    pub(crate) fn from_row(
        r: &Row<'_>,
        offset: usize,
    ) -> std::result::Result<Self, r2d2_sqlite::rusqlite::Error> {
        Ok(Self {
            id: r.get(offset)?,
            workspace_id: r.get(offset + 1)?,
            agent_id: r.get(offset + 2)?,
            acp_session_id: r.get(offset + 3)?,
            state: r.get(offset + 4)?,
            created_at: r.get(offset + 5)?,
            ended_at: r.get(offset + 6)?,
            base_rev: r.get(offset + 7)?,
            base_rev_status: r.get(offset + 8)?,
        })
    }

    pub(crate) fn into_session(self) -> Result<Session> {
        Ok(Session {
            id: self.id,
            workspace_id: self.workspace_id,
            agent_id: self.agent_id,
            acp_session_id: self.acp_session_id,
            state: SessionState::from_str(&self.state)?,
            created_at: self.created_at,
            ended_at: self.ended_at,
            base_rev: self.base_rev,
            base_rev_status: BaseRevStatus::from_str(&self.base_rev_status)?,
        })
    }
}

fn raw_session(r: &Row<'_>) -> std::result::Result<RawSession, r2d2_sqlite::rusqlite::Error> {
    RawSession::from_row(r, 0)
}

/// 会话仓储。`Clone` 只复制连接池句柄（`r2d2::Pool` 本身是 `Arc`）。
#[derive(Clone)]
pub struct SessionRepo {
    pool: Db,
    workspaces: WorkspaceRepo,
}

impl SessionRepo {
    pub fn new(pool: Db) -> Self {
        Self {
            workspaces: WorkspaceRepo::new(pool.clone()),
            pool,
        }
    }

    /// 建会话即挂载工作区：`upsert workspace` + `insert session` 在**同一个
    /// 事务**里，任一步失败都整体回滚——半截工作区（只有 workspace 没有
    /// session）会让概览里出现一个点进去什么都没有的条目。
    ///
    /// # `state` 为什么在这里就写死 `'running'`
    ///
    /// 新会话当然是 running。但这一条**不只是语义**：`v0.6.0` 起建会话时要在
    /// 刚插入的会话上开一条 git 通道探起点，而开通道的入口
    /// （`stream.rs::open_device_channel`）第一件事就查
    /// `detail.session.state != Running` → 409。所以这行的 `'running'` 是
    /// 「起点能探到」的**唯一前提**。改它之前先看那个门禁。
    ///
    /// # 为什么起点是建会话时的入参、不是事后回填
    ///
    /// `base_rev` 必须与「会话诞生」同一时刻产生，否则定义上就偏了（页面先问
    /// 再带上，中间 agent 可能自己 commit 过）。代价是两条入库路径：`create`
    /// 写初值（这里是 `None` + `Unknown`），探测完由 `set_base_rev` 回填。
    /// 那个几十毫秒的窗口里 `status` 是 `unknown` 而不是 `ok`——`unknown` 是
    /// 「不知道」，不会显示成一个错的起点。
    pub fn create(
        &self,
        node_id: &str,
        path: &str,
        agent_id: Option<&str>,
        now: &str,
        base_rev: Option<&str>,
        base_rev_status: BaseRevStatus,
    ) -> Result<Session> {
        let mut conn = self.pool.get()?;
        // `transaction()` 而非 `unchecked_transaction()`：见
        // `DeviceRepo::set_public_key` 的同名注释——库开着 `foreign_keys=ON`，
        // deferred 的嵌套事务会撞「cannot start a transaction within a
        // transaction」，`transaction()` 会复用已在进行的那一层。
        let tx = conn.transaction()?;
        let ws = self.workspaces.upsert_in(&tx, node_id, path, now)?;
        let id = crate::workspace::new_id();
        tx.execute(
            "INSERT INTO session
                 (id, workspace_id, agent_id, state, created_at, base_rev, base_rev_status)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6)",
            params![id, ws.id, agent_id, now, base_rev, base_rev_status.as_str()],
        )?;
        // 从事务里读回：刚写的行与其余行走同一套映射，`state` 的 CHECK 与
        // 解析器口径不一致时不会静默放过。
        let row = tx
            .query_row(
                "SELECT id, workspace_id, agent_id, acp_session_id, state, created_at, ended_at,
                        base_rev, base_rev_status
                 FROM session WHERE id = ?1",
                params![id],
                raw_session,
            )?
            .into_session()?;
        tx.commit()?;
        Ok(row)
    }

    /// 回填写入会话起点。建会话时探测完成（或探测失败）后调用。
    ///
    /// 单独一个方法而不是在 `create` 里等探测结果：探测要开通道，而开通道要求
    /// 会话行已存在（见 `create` 的文档注释）。顺序只能是「先落行 → 探测 →
    /// 回填」。
    /// 回填的会话不存在时报错，**不静默成功**——与 `set_acp_session_id` 同一条
    /// 口径。静默成功的话「回填给了一个已被删掉的会话」永远不被发现，
    /// 而它的症状要到下次打开页面看起点时才浮现，已经离现场很远。
    pub fn set_base_rev(&self, id: &str, rev: Option<&str>, status: BaseRevStatus) -> Result<()> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE session SET base_rev = ?2, base_rev_status = ?3 WHERE id = ?1",
            params![id, rev, status.as_str()],
        )?;
        if n == 0 {
            return Err(StorageError::NoSuchSession(id.to_string()));
        }
        Ok(())
    }

    /// 结束会话。
    ///
    /// **只有影响 0 行才报 `NoSuchSession`**：WS 断线与「用户关掉会话」在
    /// Hub 眼里都是「结束」（任务连贯性 §4.2），而 WS 断线随时会走到这里。
    /// 让「没有这一行」变成错误，等于给每一次重连握手埋一个必须处理的分支。
    pub fn end(&self, id: &str, now: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE session SET state = 'ended', ended_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        if n == 0 {
            return Err(StorageError::NoSuchSession(id.to_string()));
        }
        Ok(())
    }

    /// 按 id 取会话，**不带工作区**。
    ///
    /// # 生产侧不用它：那里要的是 [`Self::detail`]
    ///
    /// 开会话、开对话、`open_channel` 都要工作区的 `node_id` 与 `path`
    /// （通道键是 `(node_id, channel_id)`），所以生产读单行一律走 `detail`
    /// （`conversation.rs` / `stream.rs`）。本函数的列与 `detail` 的会话段
    /// 重叠，单看它没有额外语义——它是 `detail` 的「只要会话那一段」版本。
    ///
    /// 留着是给测试用的：集成测试要在固定 sleep 之外等一个时序条件
    /// （`hub-server/tests/api.rs` 用它轮询 `acp_session_id` 是否已回填），
    /// 同文件的单元测试也用它读回刚写的行。删掉它那批读回就得自己拼 SQL。
    pub fn find(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, workspace_id, agent_id, acp_session_id, state, created_at, ended_at,
                    base_rev, base_rev_status
             FROM session WHERE id = ?1",
            params![id],
            raw_session,
        )
        .optional()?
        .map(RawSession::into_session)
        .transpose()
    }

    /// 记下 agent 给的 ACP 会话 id。
    ///
    /// 只在设备回过 `channel_ready` 之后调用：那时 `session/new` 或
    /// `session/load` 已经成功，`ready` 帧里那个 id 是 agent 认的。
    ///
    /// # 为什么是「写」而不是「第一次建会话时就填」
    ///
    /// 这个 id 只有设备侧的 agent 知道——Hub 在建会话时无从得知，它来自
    /// `session/new` 的响应。所以只能等设备上报后再回填。而回填之前这条会话的
    /// `acp_session_id` 是 `NULL`，含义是「还没开过对话」，设备侧据此走
    /// `session/new`。**不要**给它填一个 Hub 自己编的占位值：那会让第二次开对话
    /// 去 load 一个 agent 从来不认识的 id。
    ///
    /// # 幂等
    ///
    /// 同一条会话重复回填同一个 id 是正常现象（断线重连、页面刷新都会再开一次
    /// 通道），所以这里用 `UPDATE` 而不是 `INSERT`，且不改 `updated_at` 之外
    /// 的任何东西。会话不存在时回 [`StorageError::NoSuchSession`]，与 `end`
    /// 同口径——静默成功会让「回填给了一条已被删掉的会话」永远不被发现。
    pub fn set_acp_session_id(&self, id: &str, acp_session_id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE session SET acp_session_id = ?2 WHERE id = ?1",
            params![id, acp_session_id],
        )?;
        if n == 0 {
            return Err(StorageError::NoSuchSession(id.to_string()));
        }
        Ok(())
    }

    /// 会话 + 工作区，供开通道时取 `(node_id, path)` 与页面渲染。
    pub fn detail(&self, id: &str) -> Result<Option<SessionDetail>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT s.id, s.workspace_id, s.agent_id, s.acp_session_id, s.state,
                    s.created_at, s.ended_at, s.base_rev, s.base_rev_status,
                    w.id, w.node_id, w.removed_node_id, w.path, w.created_at
             FROM session s JOIN workspace w ON w.id = s.workspace_id
             WHERE s.id = ?1",
            params![id],
            // 9 而不是 6 或 7：会话那一段现在是九列（v0.6.0 加了
            // `base_rev` / `base_rev_status`）。**这个数字与上面 SELECT 里
            // `s.` 开头的列数必须相等**，差一位就会把 `w.id` 当成
            // `base_rev_status` 读——两者都能读成字符串，于是静默错位。
            |r| Ok((raw_session(r)?, row_to_workspace_at(r, 9)?)),
        )
        .optional()?
        .map(|(s, w)| {
            Ok(SessionDetail {
                session: s.into_session()?,
                workspace: w,
            })
        })
        .transpose()
    }

    /// 全部会话（新的在前），各带工作区的 `(node_id, path)`。
    pub fn list(&self) -> Result<Vec<crate::workspace::SessionListItem>> {
        crate::workspace::list_sessions(&self.workspaces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;
    use crate::repo::DeviceRepo;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, SessionRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        let conn = DeviceRepo::new(pool.clone()).pool().get().unwrap();
        conn.execute(
            "INSERT INTO device (id, name, key_fingerprint, enrolled_at)
             VALUES ('n_a', 'pi', 'fp_a', 'unix:0')",
            [],
        )
        .unwrap();
        (d, SessionRepo::new(pool))
    }

    /// 关掉 CHECK 后塞一个第三态（模拟库被外部改坏）。
    ///
    /// `execute_batch` 每条语句**各自一个事务**，所以 `PRAGMA ignore_check_constraints`
    /// 的作用范围只到下一条语句为止——分号不能省。
    fn corrupt_state(sr: &SessionRepo) {
        sr.workspaces
            .pool()
            .get()
            .unwrap()
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE session SET state = 'paused';",
            )
            .unwrap();
    }

    /// 半个事务都做完（工作区已 upsert、session 已插入）之后才可能失败的一次查询。
    /// CHECK 约束保证 `state` 只会是 `running`，所以这条路径在库里没有制造方；
    /// 真要制造得先破坏 CHECK —— 而那会同时让「库被改坏」这件事本身失去参照。
    #[test]
    fn re_reading_the_fresh_session_from_inside_the_transaction_works() {
        let (_d, sr) = setup();
        // 与 `create` 里那次读回同一条语句、同一套映射，只是这里显式跑一遍：
        // 它锁住「事务内读回刚写的行」这条约定（列序与 `raw_session` 一致）。
        let mut conn = sr.workspaces.pool().get().unwrap();
        let tx = conn.transaction().unwrap();
        let ws = sr.workspaces.upsert_in(&tx, "n_a", "/tx", TS).unwrap();
        tx.execute(
            "INSERT INTO session (id, workspace_id, agent_id, state, created_at)
             VALUES ('s_tx', ?1, NULL, 'running', ?2)",
            params![ws.id, TS],
        )
        .unwrap();
        let got: Session = tx
            .query_row(
                "SELECT id, workspace_id, agent_id, acp_session_id, state, created_at, ended_at,
                        base_rev, base_rev_status
                 FROM session WHERE id = 's_tx'",
                [],
                raw_session,
            )
            .unwrap()
            .into_session()
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(got.id, "s_tx");
        assert_eq!(got.state, SessionState::Running);
        // 上面那条 INSERT **故意不写**这两个新列：它验的正是列默认值——
        // `base_rev` 落 NULL、`base_rev_status` 落 'unknown'。如果默认值被写成
        // 'ok'，所有不显式给起点的写入路径都会声称「取到了」，而 `base_rev`
        // 是 NULL——一个看起来正常的谎话。
        assert_eq!(got.base_rev, None);
        assert_eq!(got.base_rev_status, BaseRevStatus::Unknown);
        assert_eq!(got.workspace_id, ws.id);
    }

    #[test]
    fn sessions_are_listed_newest_first() {
        let (_d, sr) = setup();
        let a = sr
            .create("n_a", "/a", None, "unix:10", None, BaseRevStatus::Unknown)
            .unwrap();
        let b = sr
            .create("n_a", "/b", None, "unix:20", None, BaseRevStatus::Unknown)
            .unwrap();
        sr.create("n_a", "/c", None, "unix:20", None, BaseRevStatus::Unknown)
            .unwrap();

        let ids: Vec<String> = sr
            .list()
            .unwrap()
            .into_iter()
            .map(|i| i.session.id)
            .collect();
        let again: Vec<String> = sr
            .list()
            .unwrap()
            .into_iter()
            .map(|i| i.session.id)
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&b.id));
        assert_eq!(ids.last().map(String::as_str), Some(a.id.as_str()));
        // 同一时刻建的两条也有确定顺序（id 兜底）
        assert_eq!(ids, again);
    }

    #[test]
    fn a_broken_row_is_reported_instead_of_dropped_from_the_list() {
        let (_d, sr) = setup();
        let id = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Unknown)
            .unwrap()
            .id;
        corrupt_state(&sr);

        // 丢掉这一行会让页面少一个条目，而页面上没有任何地方能提示用户
        let e = sr.list().unwrap_err();
        assert!(
            matches!(e, StorageError::InvalidSessionState(_)),
            "got {e:?}"
        );
        // 单条查询是同一个口径
        assert!(matches!(
            sr.find(&id).unwrap_err(),
            StorageError::InvalidSessionState(_)
        ));
    }

    #[test]
    fn ending_a_session_records_when_it_ended() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Unknown)
            .unwrap();
        sr.end(&s.id, "unix:1700000099").unwrap();

        let got = sr.find(&s.id).unwrap().unwrap();
        assert_eq!(got.state, SessionState::Ended);
        assert_eq!(got.ended_at.as_deref(), Some("unix:1700000099"));
    }

    #[test]
    fn ending_an_unknown_session_is_an_error() {
        let (_d, sr) = setup();
        let e = sr.end("s_missing", TS).unwrap_err();
        assert!(matches!(e, StorageError::NoSuchSession(_)), "got {e:?}");
    }

    /// WS 断开与「用户关掉会话」在 Hub 眼里都是「结束」，而前者随时会重复走到：
    /// 重复 `end` 必须是无害的刷新，而不是把重连握手变成错误。
    #[test]
    fn ending_an_already_ended_session_is_not_an_error() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Unknown)
            .unwrap();
        sr.end(&s.id, "unix:10").unwrap();
        sr.end(&s.id, "unix:20").unwrap();
        assert_eq!(
            sr.find(&s.id).unwrap().unwrap().ended_at.as_deref(),
            Some("unix:20"),
            "重复 end 刷新结束时间"
        );
    }

    #[test]
    fn a_session_reports_its_device_and_path() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/srv/app", None, TS, None, BaseRevStatus::Unknown)
            .unwrap();
        let d = sr.detail(&s.id).unwrap().expect("detail");
        assert_eq!(d.workspace.node_id.as_deref(), Some("n_a"));
        assert_eq!(d.workspace.removed_node_id, None, "设备还在时不填 removed");
        assert_eq!(d.workspace.path, "/srv/app");
        assert_eq!(d.workspace.created_at, TS);
        assert_eq!(d.session.workspace_id, d.workspace.id);
        // 存储层不拦「没有 agent」：400 门禁在 API 层（`POST /api/sessions`）。
        // 老行本来就是 `None`，读回来仍是 `None`，这是这里要钉住的事实。
        assert_eq!(d.session.agent_id, None, "没给 agent 的会话读回来仍是 None");
        assert_eq!(d.session.created_at, TS);
        assert!(sr.detail("s_missing").unwrap().is_none());
    }

    #[test]
    fn detail_also_reports_a_broken_row() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Unknown)
            .unwrap();
        corrupt_state(&sr);
        assert!(matches!(
            sr.detail(&s.id).unwrap_err(),
            StorageError::InvalidSessionState(_)
        ));
    }

    #[test]
    fn session_state_round_trips_and_rejects_unknown() {
        assert_eq!(SessionState::Running.as_str(), "running");
        assert_eq!(SessionState::Ended.as_str(), "ended");
        for s in [SessionState::Running, SessionState::Ended] {
            assert_eq!(s.as_str().parse::<SessionState>().unwrap(), s);
        }
        assert!(matches!(
            "paused".parse::<SessionState>(),
            Err(StorageError::InvalidSessionState(_))
        ));
    }
    /// `acp_session_id` 写入后能被读回，且**新建的会话一开始是 `None`**。
    ///
    /// 两个方向都要：只验「写进去能读到」不能证明「新建时是空的」——
    /// 而后者才是设备侧走 `session/new` 的判据（`Some` 会走 `load`）。
    #[test]
    fn the_acp_session_id_is_none_until_it_is_recorded_then_round_trips() {
        let (_d, sr) = setup();
        let s = sr
            .create(
                "n_a",
                "/srv/app",
                Some("opencode"),
                TS,
                None,
                BaseRevStatus::Unknown,
            )
            .unwrap();
        assert_eq!(
            s.acp_session_id, None,
            "刚建的会话还没有 ACP id，设备侧据此走 session/new"
        );

        sr.set_acp_session_id(&s.id, "ses_abc").unwrap();

        // 三条读路径都要看到它：`find` 是设备/API 用的单条读，
        // `detail` 是开对话时用的联表读（列偏移不同），`list` 是页面用的。
        assert_eq!(
            sr.find(&s.id).unwrap().unwrap().acp_session_id.as_deref(),
            Some("ses_abc")
        );
        assert_eq!(
            sr.detail(&s.id)
                .unwrap()
                .unwrap()
                .session
                .acp_session_id
                .as_deref(),
            Some("ses_abc")
        );
        let listed = sr.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session.acp_session_id.as_deref(), Some("ses_abc"));
    }

    /// 重复回填同一个 id 不算错（断线重连、页面刷新都会再开一次通道）。
    #[test]
    fn recording_the_same_acp_session_id_twice_is_not_an_error() {
        let (_d, sr) = setup();
        let s = sr
            .create(
                "n_a",
                "/srv/app",
                Some("opencode"),
                TS,
                None,
                BaseRevStatus::Unknown,
            )
            .unwrap();
        sr.set_acp_session_id(&s.id, "ses_abc").unwrap();
        sr.set_acp_session_id(&s.id, "ses_abc").unwrap();
        assert_eq!(
            sr.find(&s.id).unwrap().unwrap().acp_session_id.as_deref(),
            Some("ses_abc")
        );
    }

    /// 回填给一条不存在的会话要报错，不能静默成功。
    ///
    /// 静默成功的话，「回填给了一条已被删掉的会话」永远不被发现，而它的
    /// 症状（对话接不上）在下一次开通道时才出现，已经离现场很远。
    #[test]
    fn recording_an_acp_session_id_on_an_unknown_session_is_an_error() {
        let (_d, sr) = setup();
        let err = sr.set_acp_session_id("s_missing", "ses_abc").unwrap_err();
        assert!(
            matches!(err, StorageError::NoSuchSession(ref id) if id == "s_missing"),
            "实际 {err:?}"
        );
    }

    /// 起点与状态**三条读取路径读到同一个值**。
    ///
    /// 这一条是「九列对齐」的实际守卫：`find` / `detail` / `list` 各有一份手写
    /// SELECT，任何一处偏移写错一位，读到的就是隔壁那列。而 `base_rev` 与
    /// `base_rev_status` 都是文本，**类型对得上，所以只会静默错位**——
    /// 三条路径的值一旦不一致，这条立刻红。
    #[test]
    fn create_persists_the_base_rev_and_its_status() {
        let (_d, sr) = setup();
        let rev = "3f2a1b0c4d5e6f708192a3b4c5d6e7f809a1b2c3";
        let s = sr
            .create(
                "n_a",
                "/srv/app",
                Some("opencode"),
                TS,
                Some(rev),
                BaseRevStatus::Ok,
            )
            .unwrap();
        // ① create 的事务内读回
        assert_eq!(s.base_rev.as_deref(), Some(rev));
        assert_eq!(s.base_rev_status, BaseRevStatus::Ok);

        // ② find
        let f = sr.find(&s.id).unwrap().unwrap();
        assert_eq!(f.base_rev.as_deref(), Some(rev), "find 读到的起点不对");
        assert_eq!(f.base_rev_status, BaseRevStatus::Ok, "find 读到的状态不对");

        // ③ detail（`row_to_workspace_at(r, 9)` 的偏移）
        let d = sr.detail(&s.id).unwrap().unwrap();
        assert_eq!(
            d.session.base_rev.as_deref(),
            Some(rev),
            "detail 读到的起点不对"
        );
        assert_eq!(
            d.session.base_rev_status,
            BaseRevStatus::Ok,
            "detail 读到的状态不对"
        );
        // detail 的工作区段也必须仍然对（偏移 +2 之后没把 w.* 读歪）
        assert_eq!(d.workspace.path, "/srv/app", "detail 的工作区段错位了");
        assert_eq!(d.workspace.node_id.as_deref(), Some("n_a"));

        // ④ list（`r.get(9..=11)` 的偏移）
        let ws = crate::workspace::list_sessions(&sr.workspaces).unwrap();
        let one = ws.iter().find(|w| w.session.id == s.id).expect("在列表里");
        assert_eq!(
            one.session.base_rev.as_deref(),
            Some(rev),
            "list 读到的起点不对"
        );
        assert_eq!(
            one.session.base_rev_status,
            BaseRevStatus::Ok,
            "list 读到的状态不对"
        );
        // list 的工作区三列也要对：偏移 9/10/11 写错时这里先红
        assert_eq!(
            one.node_id.as_deref(),
            Some("n_a"),
            "list 的 node_id 错位了"
        );
        assert_eq!(one.path, "/srv/app", "list 的 path 错位了");
        assert_eq!(one.removed_node_id, None);
    }

    /// 没有 rev 不等于没有 status：`Unavailable` 时 `base_rev` 是 NULL，
    /// 而 `status` 必须照实读回——否则页面无从知道「为什么没有起点」。
    #[test]
    fn create_with_unavailable_status_has_no_rev() {
        let (_d, sr) = setup();
        let s = sr
            .create(
                "n_a",
                "/srv/app",
                None,
                TS,
                None,
                BaseRevStatus::Unavailable,
            )
            .unwrap();
        assert_eq!(s.base_rev, None);
        let f = sr.find(&s.id).unwrap().unwrap();
        assert_eq!(f.base_rev, None);
        assert_eq!(f.base_rev_status, BaseRevStatus::Unavailable);
    }

    /// 库里的状态不是五态之一 → 报错，**不回落成 Unknown**。
    ///
    /// 回落的话，「库里存了一个我们不认识的值」（数据损坏 / 有人手工改库）
    /// 会被显示成「这是 v0.6.0 之前的老会话」——一个看起来正常的谎话。
    /// 与 `SessionState` 对第三态的处理口径一致（`corrupt_state` 那条用例）。
    #[test]
    fn an_unknown_status_cannot_be_parsed_as_ok() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Ok)
            .unwrap();
        let conn = sr.pool.get().unwrap();
        conn.execute(
            "UPDATE session SET base_rev_status = 'okayish' WHERE id = ?1",
            params![s.id],
        )
        .unwrap();
        let err = sr.find(&s.id).unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidBaseRevStatus(ref v) if v == "okayish"),
            "必须报 InvalidBaseRevStatus，实际 {err:?}"
        );
    }

    /// `set_base_rev` 回填后，`find` 读到的是新值——建会话「先落行 → 探测 →
    /// 回填」的第二步真的落库了。
    #[test]
    fn set_base_rev_backfills_the_row() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Unknown)
            .unwrap();
        let rev = "aaaa1111bbbb2222cccc3333dddd4444eeee5555";
        sr.set_base_rev(&s.id, Some(rev), BaseRevStatus::Ok)
            .unwrap();

        let f = sr.find(&s.id).unwrap().unwrap();
        assert_eq!(f.base_rev.as_deref(), Some(rev));
        assert_eq!(f.base_rev_status, BaseRevStatus::Ok);
    }

    /// 回填给不存在的会话要报错（与 `set_acp_session_id` 同一条口径）。
    #[test]
    fn set_base_rev_on_an_unknown_session_is_an_error() {
        let (_d, sr) = setup();
        let err = sr
            .set_base_rev("s_missing", None, BaseRevStatus::Unavailable)
            .unwrap_err();
        assert!(
            matches!(err, StorageError::NoSuchSession(ref id) if id == "s_missing"),
            "实际 {err:?}"
        );
    }

    /// **`create` 落下的行必须是 `Running`。**
    ///
    /// 这条锁的是子任务 3 的隐式前提：建会话时要在刚插入的会话上开 git 通道探
    /// 起点，而开通道的入口（`stream.rs::open_device_channel`）第一件事就查
    /// `state != Running` → 409。`create` 里那个 `'running'` 字面量一旦被删或
    /// 被改，**每一个**会话的起点都会变成 `unavailable`，而症状会出现在很远
    /// 的地方（建会话的集成测试里）。在存储层这里先钉住，报错能直接指根因。
    #[test]
    fn create_leaves_the_session_running() {
        let (_d, sr) = setup();
        let s = sr
            .create("n_a", "/a", None, TS, None, BaseRevStatus::Unknown)
            .unwrap();
        assert_eq!(
            s.state,
            SessionState::Running,
            "新会话必须是 Running——它是「建会话时能探起点」的唯一前提"
        );
        // 走一遍库也不变（不是只在返回值上改的）
        assert_eq!(
            sr.find(&s.id).unwrap().unwrap().state,
            SessionState::Running
        );
    }

    /// 五态的字面量往返：`as_str` / `from_str` 一一对应。
    ///
    /// 加一态却忘了改其中一个方向时，这一条会红——否则新态在库里存得进、
    /// 读不出（或反之），而错误只会在某个真实会话上出现。
    #[test]
    fn every_base_rev_status_round_trips_through_its_literal() {
        for st in [
            BaseRevStatus::Ok,
            BaseRevStatus::NotARepo,
            BaseRevStatus::NoHead,
            BaseRevStatus::Unavailable,
            BaseRevStatus::Unknown,
        ] {
            let lit = st.as_str();
            assert_eq!(
                BaseRevStatus::from_str(lit).unwrap(),
                st,
                "{lit:?} 必须能读回同一个态"
            );
        }
        // 五个字面量必须互不相同（否则两个态在库里不可区分）
        let mut all: Vec<&str> = [
            BaseRevStatus::Ok,
            BaseRevStatus::NotARepo,
            BaseRevStatus::NoHead,
            BaseRevStatus::Unavailable,
            BaseRevStatus::Unknown,
        ]
        .iter()
        .map(|s| s.as_str())
        .collect();
        all.sort_unstable();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n, "五态的字面量不得重复");
    }
}
