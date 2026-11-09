//! 路由单测共用这一套夹具：启动参数与「表改名」的故障注入语义必须在各模块
//! 一致，否则「库坏了」的断言会在某一处悄悄失去前提。

use crate::{AppState, AuthParams, LoginRateLimiter};
use std::time::Duration;

pub(crate) fn state() -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let pool = hub_storage::db::open_in(dir.path()).unwrap().1;
    let state = AppState::bootstrap_with(
        pool,
        dir.path(),
        AuthParams {
            m_cost: 8192,
            t_cost: 1,
            p_cost: 1,
        },
        Duration::from_millis(200),
        LoginRateLimiter::new(10, 60),
    )
    .unwrap();
    (state, dir)
}

pub(crate) fn break_table(state: &AppState, table: &str) {
    state
        .pool()
        .get()
        .unwrap()
        .execute_batch(&format!("ALTER TABLE {table} RENAME TO {table}_moved"))
        .unwrap();
}
