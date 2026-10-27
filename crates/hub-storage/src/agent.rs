//! agent 运行时事实与配置的仓储（M2 子任务 3 落库侧 + 子任务 4 配置）。
//!
//! # 两件事，两张表，互不依赖
//!
//! - [`AgentRuntimeRepo`]：设备**扫描**到了什么（心跳上报，整体覆盖）。
//! - [`AgentConfigRepo`]：用户给这个 agent 配的**启动方式**（Hub 存，设备读）。
//!
//! 两者**没有外键**，也不要求另一张表有对应行：用户可以先配好再装，
//! 也可能装了但没配（用默认启动方式）。
//!
//! # 为什么运行时事实必须落库，不能只留在内存
//!
//! 心跳上报原先只进内存 `hub_core::Reports`，而那份表是**缺省即保留**
//! （`merge_report` 在 `capabilities` 为 `null` 时沿用旧值，且没有 `forget`）。
//! 于是「设备卸载了 agent」永远传不到 Hub：下一次心跳只要不带
//! `capabilities`（协议明确允许），旧清单就留在内存里。
//!
//! `agent_runtime` 是那条链路的落库侧，且带 `detected_at`：
//! 读者能自己判「这份事实有多旧」，而不是继承一个沉默的旧值。

use r2d2_sqlite::rusqlite::{params, OptionalExtension, Row};

use crate::error::Result;
use crate::Db;

/// `agent_runtime` 的一行：设备上扫描到的一个 agent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRuntime {
    pub node_id: String,
    pub agent_id: String,
    pub name: String,
    /// 没探到版本时是 `None`（表里是空串：列是 `NOT NULL`）。
    ///
    /// **对外的 `Option` 与库里的空串要分清**：空串是「这一列必须有值」时的
    /// 占位，而 `None` 才是「根本没有版本可报」。把空串直接漏给页面会渲染成
    /// 「版本：（空白）」，看起来像 bug 而不是「未知」。
    pub version: Option<String>,
    /// 现在能不能对话。
    pub available: bool,
    /// 「装了但跑不起来」的可读原因（退出码、缺动态库）。能跑时为 `None`。
    pub probe_error: Option<String>,
    pub detected_at: String,
}

/// `replace_detected` 的一行：`(agent_id, name, version, available, probe_error)`。
///
/// 起别名而不是直接写五元组：`&[(String, String, Option<String>, bool,
/// Option<String>)]` 作为参数类型读者要停下来数位，而且改一位就得在两处一起改。
pub type DetectedRow = (String, String, Option<String>, bool, Option<String>);

fn row_to_runtime(r: &Row<'_>) -> std::result::Result<AgentRuntime, r2d2_sqlite::rusqlite::Error> {
    let version: String = r.get(3)?;
    Ok(AgentRuntime {
        node_id: r.get(0)?,
        agent_id: r.get(1)?,
        name: r.get(2)?,
        version: if version.is_empty() {
            None
        } else {
            Some(version)
        },
        available: r.get::<_, i64>(4)? != 0,
        probe_error: r.get(5)?,
        detected_at: r.get(6)?,
    })
}

const RUNTIME_COLUMNS: &str =
    "node_id, agent_id, name, version, available, probe_error, detected_at";

/// `agent_runtime` 的读写。
#[derive(Debug, Clone)]
pub struct AgentRuntimeRepo {
    pool: Db,
}

impl AgentRuntimeRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 用一次心跳的扫描结果**整体覆盖**这台设备的行。
    ///
    /// # 为什么是「整体覆盖」而不是逐行 upsert
    ///
    /// 扫描的语义是「这一份清单就是设备上现在的全部事实」。逐行 upsert 只会
    /// 增加、**不会删**：用户卸载了 agent，那一行就永远留在库里，门禁永远
    /// 放行。这正是内存 `Reports` 的「结构性粘滞」，不能在落库层重犯一遍。
    /// 所以在同一个事务里 `DELETE` 再插入。
    ///
    /// # 配置行不受影响
    ///
    /// 设备卸载 agent 之后，它的配置**保留**（`agent_config` 不在这条事务里）：
    /// 用户可能装回来，配置重填一遍没有理由。这与「删设备不删会话」同一个取向。
    pub fn replace_detected(&self, node_id: &str, rows: &[DetectedRow], now: &str) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM agent_runtime WHERE node_id = ?1",
            params![node_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO agent_runtime
                     (node_id, agent_id, name, version, available, probe_error, detected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (id, name, version, available, error) in rows {
                stmt.execute(params![
                    node_id,
                    id,
                    name,
                    version.as_deref().unwrap_or(""),
                    i64::from(*available),
                    error,
                    now,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 这台设备上的全部 agent 行。
    pub fn list_for_node(&self, node_id: &str) -> Result<Vec<AgentRuntime>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM agent_runtime WHERE node_id = ?1 ORDER BY agent_id"
        ))?;
        let rows = stmt.query_map(params![node_id], row_to_runtime)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 查一行。
    pub fn find(&self, node_id: &str, agent_id: &str) -> Result<Option<AgentRuntime>> {
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {RUNTIME_COLUMNS} FROM agent_runtime WHERE node_id = ?1 AND agent_id = ?2"
                ),
                params![node_id, agent_id],
                row_to_runtime,
            )
            .optional()?;
        Ok(row)
    }

    /// 删掉一台设备的全部行（设备被删除时调）。
    ///
    /// **会话与轨迹不跟着删**：删除设备只删设备行，会话保留（用户裁定）。
    /// 这条清的是「agent 扫描事实」——它描述的是那台设备，设备没了就无意义。
    ///
    /// 配置行也一并删：配置是「这台设备上的这个 agent」，设备没了它的
    /// 启动命令没有意义（而且 `command` 里可能写着那台机器的绝对路径）。
    pub fn delete_for_node(&self, node_id: &str) -> Result<usize> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM agent_config WHERE node_id = ?1",
            params![node_id],
        )?;
        let n = tx.execute(
            "DELETE FROM agent_runtime WHERE node_id = ?1",
            params![node_id],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// 会话就绪门禁的**唯一**数据源。
    ///
    /// 判据：库里有一行且 `available = 1`。
    ///
    /// # 与 M1 的 connector 门禁显式不同
    ///
    /// `stream.rs` 的 `unreported_connector` 对「从没上报过」是**放行**
    /// （理由：没上报过不代表不支持）。这里**必须拒**：agent 是会话的
    /// **必要**前提而不是可选项，放行的后果是「建会话 201，开 ACP 时才发现
    /// binary 不存在」——用户拿到一个永远无法对话的死会话。
    pub fn usable_for_session(
        &self,
        node_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentRuntime>> {
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {RUNTIME_COLUMNS} FROM agent_runtime
                     WHERE node_id = ?1 AND agent_id = ?2 AND available = 1"
                ),
                params![node_id, agent_id],
                row_to_runtime,
            )
            .optional()?;
        Ok(row)
    }

    /// 这台设备上所有可用于建会话的 agent（页面下拉用）。
    ///
    /// 与 [`Self::usable_for_session`] 同一套判据：下拉里能选到的，建会话时
    /// 一定过得了门禁。两处判据若各写一份，症状是「页面上选得到、点创建 409」。
    pub fn usable_list(&self, node_id: &str) -> Result<Vec<AgentRuntime>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM agent_runtime
             WHERE node_id = ?1 AND available = 1 ORDER BY agent_id"
        ))?;
        let rows = stmt.query_map(params![node_id], row_to_runtime)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

/// 一个 agent 的启动配置。
///
/// 全是字符串与字符串数组：**配置不是代码**。Hub 下发的是「怎么启动一个
/// 用户已经装好的程序」，不是「一段要被执行的程序」——这正是取消安装后
/// 风险等级降下来的地方。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    pub node_id: String,
    pub agent_id: String,
    /// 启动命令。空串 = 用扫描到的那个可执行文件。
    ///
    /// 允许覆盖是因为用户可能把它装在 `PATH` 之外（`~/.local/bin`、
    /// 某个 nvm 目录）。留空则走探测结果，那是绝大多数情况。
    pub command: String,
    /// 启动参数（如 `["--acp"]`）。
    pub args: Vec<String>,
    /// 环境变量。
    ///
    /// **值是明文**：密钥类的不该放这里——本里程碑没有加密存储，也不能
    /// 假装有。写在这里等于让它们以明文躺在库里与日志里。
    pub env: std::collections::BTreeMap<String, String>,
    pub updated_at: String,
}

fn row_to_config(r: &Row<'_>) -> std::result::Result<AgentConfig, r2d2_sqlite::rusqlite::Error> {
    let args_json: String = r.get(2)?;
    let env_json: String = r.get(3)?;
    // 缺字段或形状不对都读成空参数/空环境：JSON 列是我们自己写的，坏掉只可能
    // 是外来改动。给一份「能启动」的默认配置比让整页 500 好——用户看到的只是
    // 「参数没了」，改一下就能恢复。
    let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
    let env: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&env_json).unwrap_or_default();
    Ok(AgentConfig {
        node_id: r.get(0)?,
        agent_id: r.get(1)?,
        command: r.get(4)?,
        args,
        env,
        updated_at: r.get(5)?,
    })
}

const CONFIG_COLUMNS: &str = "node_id, agent_id, args_json, env_json, command, updated_at";

/// `agent_config` 的读写。
#[derive(Debug, Clone)]
pub struct AgentConfigRepo {
    pool: Db,
}

impl AgentConfigRepo {
    pub fn new(pool: Db) -> Self {
        Self { pool }
    }

    /// 写入（或覆盖）一份配置。
    pub fn upsert(&self, c: &AgentConfig) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO agent_config (node_id, agent_id, command, args_json, env_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(node_id, agent_id) DO UPDATE SET
                 command = excluded.command,
                 args_json = excluded.args_json,
                 env_json = excluded.env_json,
                 updated_at = excluded.updated_at",
            params![
                c.node_id,
                c.agent_id,
                c.command,
                serde_json::to_string(&c.args)?,
                serde_json::to_string(&c.env)?,
                c.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn find(&self, node_id: &str, agent_id: &str) -> Result<Option<AgentConfig>> {
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {CONFIG_COLUMNS} FROM agent_config WHERE node_id = ?1 AND agent_id = ?2"
                ),
                params![node_id, agent_id],
                row_to_config,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_for_node(&self, node_id: &str) -> Result<Vec<AgentConfig>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CONFIG_COLUMNS} FROM agent_config WHERE node_id = ?1 ORDER BY agent_id"
        ))?;
        let rows = stmt.query_map(params![node_id], row_to_config)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 删掉一份配置。之后该 agent 走默认启动方式。
    pub fn delete(&self, node_id: &str, agent_id: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn.execute(
            "DELETE FROM agent_config WHERE node_id = ?1 AND agent_id = ?2",
            params![node_id, agent_id],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    const TS: &str = "unix:1700000000";

    fn setup() -> (tempfile::TempDir, AgentRuntimeRepo, AgentConfigRepo) {
        let d = tempfile::tempdir().unwrap();
        let pool = open(&d.path().join("t.db")).unwrap();
        (
            d,
            AgentRuntimeRepo::new(pool.clone()),
            AgentConfigRepo::new(pool),
        )
    }

    fn row(
        id: &str,
        version: Option<&str>,
        available: bool,
    ) -> (String, String, Option<String>, bool, Option<String>) {
        (
            id.to_string(),
            id.to_string(),
            version.map(str::to_string),
            available,
            None,
        )
    }

    #[test]
    fn scanned_agents_are_stored_with_their_facts() {
        let (_d, rt, _c) = setup();
        rt.replace_detected("n_a", &[row("opencode", Some("2.1.4"), true)], TS)
            .unwrap();

        let rows = rt.list_for_node("n_a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "opencode");
        assert_eq!(rows[0].version.as_deref(), Some("2.1.4"));
        assert!(rows[0].available);
        assert_eq!(rows[0].detected_at, TS);
    }

    /// **卸载掉的 agent 必须从库里消失**——这是内存 `Reports` 做不到的那件事。
    ///
    /// 反向验证：把 `replace_detected` 里的 `DELETE` 换成逐行 upsert，这条会红。
    #[test]
    fn an_agent_that_disappears_from_a_later_scan_is_deleted_not_kept() {
        let (_d, rt, _c) = setup();
        rt.replace_detected(
            "n_a",
            &[
                row("opencode", Some("2.1.4"), true),
                row("claude", Some("1.0"), true),
            ],
            TS,
        )
        .unwrap();
        assert_eq!(rt.list_for_node("n_a").unwrap().len(), 2);

        rt.replace_detected("n_a", &[row("claude", Some("1.0"), true)], TS)
            .unwrap();

        let rows = rt.list_for_node("n_a").unwrap();
        assert_eq!(rows.len(), 1, "got {rows:?}");
        assert_eq!(rows[0].agent_id, "claude");
        assert!(
            rt.find("n_a", "opencode").unwrap().is_none(),
            "卸掉的 agent 不能留在库里"
        );
    }

    /// **卸载 agent 不删它的配置**：用户可能装回来，配置重填一遍没有理由。
    #[test]
    fn removing_an_agent_from_a_scan_keeps_its_config() {
        let (_d, rt, cfg) = setup();
        rt.replace_detected("n_a", &[row("opencode", Some("2.1.4"), true)], TS)
            .unwrap();
        cfg.upsert(&AgentConfig {
            node_id: "n_a".into(),
            agent_id: "opencode".into(),
            command: "/opt/opencode/bin/opencode".into(),
            args: vec!["--acp".into()],
            env: Default::default(),
            updated_at: TS.into(),
        })
        .unwrap();

        // 用户卸了 agent
        rt.replace_detected("n_a", &[], TS).unwrap();

        assert!(rt.list_for_node("n_a").unwrap().is_empty());
        assert!(
            cfg.find("n_a", "opencode").unwrap().is_some(),
            "配置要留着：装回来时不必重配"
        );
    }

    /// **「从没上报过」必须拒**——与 M1 的 connector 门禁显式相反。
    #[test]
    fn an_agent_that_was_never_reported_is_refused_not_waved_through() {
        let (_d, rt, _c) = setup();
        assert!(
            rt.usable_for_session("n_a", "opencode").unwrap().is_none(),
            "库里没有行就是不可用；放行会让用户拿到一个永远无法对话的会话"
        );
    }

    /// 探到了但**跑不起来**：行在，但不可用，且原因存得住。
    #[test]
    fn a_detected_but_broken_agent_is_stored_and_unusable() {
        let (_d, rt, _c) = setup();
        rt.replace_detected(
            "n_a",
            &[(
                "opencode".into(),
                "opencode".into(),
                None,
                false,
                Some("退出码 3".into()),
            )],
            TS,
        )
        .unwrap();
        assert!(rt.usable_for_session("n_a", "opencode").unwrap().is_none());
        assert!(rt.usable_list("n_a").unwrap().is_empty());
        let all = rt.list_for_node("n_a").unwrap();
        assert_eq!(all.len(), 1, "跑不起来也要有行，页面要能显示它");
        assert_eq!(all[0].probe_error.as_deref(), Some("退出码 3"));
    }

    #[test]
    fn a_usable_agent_appears_in_both_the_gate_and_the_dropdown() {
        let (_d, rt, _c) = setup();
        rt.replace_detected("n_a", &[row("opencode", Some("2.1.4"), true)], TS)
            .unwrap();
        assert!(rt.usable_for_session("n_a", "opencode").unwrap().is_some());
        let list = rt.usable_list("n_a").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].agent_id, "opencode");
    }

    /// 没读到版本 = `None`，不是空串（空串渲染成「版本：（空白）」）。
    #[test]
    fn a_versionless_row_reports_none_not_an_empty_string() {
        let (_d, rt, _c) = setup();
        rt.replace_detected("n_a", &[row("opencode", None, false)], TS)
            .unwrap();
        let rows = rt.list_for_node("n_a").unwrap();
        assert_eq!(rows[0].version, None);
    }

    #[test]
    fn agents_are_scoped_per_device() {
        let (_d, rt, _c) = setup();
        rt.replace_detected("n_a", &[row("opencode", Some("2.1.4"), true)], TS)
            .unwrap();
        assert!(rt.usable_for_session("n_b", "opencode").unwrap().is_none());
        assert!(rt.list_for_node("n_b").unwrap().is_empty());
    }

    /// 删设备 → 运行时事实与配置都清；别的设备不受影响。
    #[test]
    fn deleting_a_device_removes_its_rows_and_configs_but_not_other_devices() {
        let (_d, rt, cfg) = setup();
        for n in ["n_a", "n_b"] {
            rt.replace_detected(n, &[row("opencode", Some("2.1.4"), true)], TS)
                .unwrap();
            cfg.upsert(&AgentConfig {
                node_id: n.into(),
                agent_id: "opencode".into(),
                command: String::new(),
                args: vec![],
                env: Default::default(),
                updated_at: TS.into(),
            })
            .unwrap();
        }

        assert_eq!(rt.delete_for_node("n_a").unwrap(), 1);
        assert!(rt.list_for_node("n_a").unwrap().is_empty());
        assert!(cfg.list_for_node("n_a").unwrap().is_empty(), "配置也要清");
        assert_eq!(
            rt.list_for_node("n_b").unwrap().len(),
            1,
            "不许误删别的设备"
        );
        assert_eq!(cfg.list_for_node("n_b").unwrap().len(), 1);
    }

    // ---- 配置 ----

    #[test]
    fn a_config_round_trips_with_args_and_env() {
        let (_d, _rt, cfg) = setup();
        let c = AgentConfig {
            node_id: "n_a".into(),
            agent_id: "opencode".into(),
            command: "/opt/opencode".into(),
            args: vec!["--acp".into(), "--verbose".into()],
            env: [("FOO".to_string(), "bar".to_string())]
                .into_iter()
                .collect(),
            updated_at: TS.into(),
        };
        cfg.upsert(&c).unwrap();
        assert_eq!(cfg.find("n_a", "opencode").unwrap().expect("在"), c);
    }

    #[test]
    fn upserting_the_same_config_twice_does_not_create_a_second_row() {
        let (_d, _rt, cfg) = setup();
        let mut c = AgentConfig {
            node_id: "n_a".into(),
            agent_id: "opencode".into(),
            command: "old".into(),
            args: vec![],
            env: Default::default(),
            updated_at: TS.into(),
        };
        cfg.upsert(&c).unwrap();
        c.command = "new".into();
        cfg.upsert(&c).unwrap();

        assert_eq!(cfg.list_for_node("n_a").unwrap().len(), 1);
        assert_eq!(
            cfg.find("n_a", "opencode").unwrap().expect("在").command,
            "new"
        );
    }

    /// 空命令是**合法**的：意思是「用扫描到的那个可执行文件」。
    ///
    /// 不能把空串当成「没配置」而拒绝写入——那会让「用默认启动方式」
    /// 这个最常见的配置无法表达。
    #[test]
    fn an_empty_command_is_a_valid_config_meaning_use_the_scanned_binary() {
        let (_d, _rt, cfg) = setup();
        cfg.upsert(&AgentConfig {
            node_id: "n_a".into(),
            agent_id: "opencode".into(),
            command: String::new(),
            args: vec![],
            env: Default::default(),
            updated_at: TS.into(),
        })
        .unwrap();
        let got = cfg.find("n_a", "opencode").unwrap().expect("在");
        assert_eq!(got.command, "");
        assert!(got.args.is_empty());
    }

    /// 配置可以**先于** agent 存在（装之前先配好）。
    #[test]
    fn a_config_can_exist_for_an_agent_that_was_never_scanned() {
        let (_d, rt, cfg) = setup();
        cfg.upsert(&AgentConfig {
            node_id: "n_a".into(),
            agent_id: "opencode".into(),
            command: "/opt/opencode".into(),
            args: vec![],
            env: Default::default(),
            updated_at: TS.into(),
        })
        .unwrap();
        assert!(rt.find("n_a", "opencode").unwrap().is_none());
        assert!(cfg.find("n_a", "opencode").unwrap().is_some());
    }

    #[test]
    fn deleting_a_config_is_reported_by_whether_a_row_existed() {
        let (_d, _rt, cfg) = setup();
        cfg.upsert(&AgentConfig {
            node_id: "n_a".into(),
            agent_id: "opencode".into(),
            command: String::new(),
            args: vec![],
            env: Default::default(),
            updated_at: TS.into(),
        })
        .unwrap();
        assert!(cfg.delete("n_a", "opencode").unwrap());
        assert!(!cfg.delete("n_a", "opencode").unwrap(), "第二次没有行了");
    }
}
