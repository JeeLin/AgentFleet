//! 子任务 7「设备页 / agent 页扩展」的端到端测试。
//!
//! 三条测试标准各自对应一组：
//!
//! 1. 设备页**不减少**原有内容 —— `summary` 里原有字段一个不少
//! 2. 未绑任何项目的设备显示「还没有绑定项目」而不是空区块 —— `bindings: []`
//!    + `disk: null`（**不是** `{used: 0}`）
//! 3. 成功率在 0 次执行时显示「—」而不是 0% —— `success_rate: null`
//!
//! 第 2、3 条是同一个形状的错误：**用 0 表示「没有」**。0 是一个合法的测量
//! 结果（绑了 0 个字节、跑成 0 次），拿它兼职表示「还没发生过」之后就再也
//! 分不开了。所以这两条用例断的都是 `null` 而不是 0。
//!
//! Clippy 的 allow-*-in-tests 覆盖不到本文件的顶层 helper（见 clippy.toml 与 AGENTS.md）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use hub_server::{build_app, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Harness {
    app: Router,
    session: String,
    csrf: String,
    pool: hub_storage::db::Db,
}

impl Harness {
    async fn from_hub(hub: &common::Hub) -> Self {
        let app = build_app(hub.state.clone());
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"password": PASSWORD}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let session = cookie_of(&login, SESSION_COOKIE).expect("session cookie");
        let csrf = cookie_of(&login, CSRF_COOKIE).expect("csrf cookie");
        Self {
            app,
            session,
            csrf,
            pool: hub.pool(),
        }
    }

    async fn send(&self, req: Request<Body>) -> Reply {
        let mut req = req;
        req.headers_mut().insert(
            "cookie",
            format!(
                "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                self.session, self.csrf
            )
            .parse()
            .unwrap(),
        );
        Reply::new(self.app.clone().oneshot(req).await.expect("app responds")).await
    }

    async fn get(&self, path: &str) -> Reply {
        self.send(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
    }

    async fn mutate(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(CSRF_HEADER, &self.csrf);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        self.send(b.body(body).unwrap()).await
    }

    async fn project(&self, name: &str) -> String {
        let r = self
            .mutate(
                "POST",
                "/api/projects",
                Some(json!({"name": name, "shape": "isolated"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
        r.json()["id"].as_str().unwrap().to_string()
    }

    async fn bind(&self, project_id: &str, device_id: &str, body: Value) -> String {
        let r = self
            .mutate(
                "PUT",
                &format!("/api/projects/{project_id}/bindings/{device_id}"),
                Some(body),
            )
            .await;
        assert!(r.status.is_success(), "绑不上，后面全是假的：{}", r.body);
        r.json()["id"].as_str().unwrap().to_string()
    }
}

fn cookie_of(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|raw| {
            let (k, v) = raw.split(';').next()?.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

struct Reply {
    status: StatusCode,
    body: String,
}

impl Reply {
    async fn new(resp: axum::response::Response) -> Self {
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Self {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// 造一条执行记录。`state` 用 `execution` 表认的词。
fn seed_execution(h: &Harness, binding_id: &str, id: &str, state: &str, task_id: &str) {
    let conn = h.pool.get().expect("取连接");
    conn.execute(
        "INSERT INTO execution (id, task_id, binding_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, 'unix:1700000000')",
        (id, task_id, binding_id, state),
    )
    .expect("写执行记录");
}

/// 造一条任务。
fn seed_task(h: &Harness, id: &str, project_id: &str, title: &str) {
    let conn = h.pool.get().expect("取连接");
    conn.execute(
        "INSERT INTO task (id, project_id, title, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'done', 'unix:1', 'unix:1')",
        (id, project_id, title),
    )
    .expect("写任务");
}

/// 让一台设备上「装了某个 agent」。
fn seed_agent(h: &Harness, node_id: &str, name: &str, available: bool) {
    let conn = h.pool.get().expect("取连接");
    conn.execute(
        "INSERT INTO agent_runtime
             (node_id, agent_id, name, version, available, probe_error, detected_at)
         VALUES (?1, ?2, ?2, '1.0.0', ?3, NULL, 'unix:1700000000')",
        (node_id, name, i64::from(available)),
    )
    .expect("写 agent_runtime");
}

// ---- 测试标准 1：设备页原有内容不减少 ------------------------------------

/// `summary` 里的字段一个不少。
///
/// 这条用例存在的理由是**子任务的名字里就有「不砍反扩」**：新主轴很容易让
/// 人把设备页当成「项目页的重复」而砍掉几块。断言字段名而不是数量，
/// 因为数量会在加字段时变，而字段名只在**真的砍了**的时候才不见。
#[tokio::test]
async fn the_device_detail_keeps_every_field_the_list_had() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let r = h.get("/api/devices/device-a/detail").await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let s = &r.json()["summary"];

    for key in [
        "id",
        "name",
        "platform",
        "node_version",
        "status",
        "last_seen",
        "cpu",
        "mem",
        "gpu",
        "agents",
        "usable_agents",
        "derived",
        "key_fingerprint",
    ] {
        assert!(
            s.get(key).is_some(),
            "设备页原有的字段 {key} 不见了——这个子任务只许加，不许减；实际 {s}"
        );
    }
    assert_eq!(s["id"], json!("device-a"));
}

/// 新加的三块都在。缺任何一块，页面上就会有一块空白而不报错。
#[tokio::test]
async fn the_device_detail_adds_bindings_disk_and_failure_breakdown() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let v = h.get("/api/devices/device-a/detail").await.json();
    for key in ["bindings", "disk", "failure_breakdown", "deletable"] {
        assert!(v.get(key).is_some(), "缺了 {key}：{v}");
    }
}

/// 绑了项目之后，设备页看得到项目名与固定目录。
///
/// 只有 `project_id` 的话，设备页要再去拉一次项目列表才能显示名字——
/// 而那是一次 N+1 请求，且设备页本来就有这份数据可聚合。
#[tokio::test]
async fn bindings_carry_the_project_name_and_the_fixed_dir() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let project = h.project("指标台").await;
    h.bind(
        &project,
        "device-a",
        json!({"fixed_dir": "/srv/metrics", "disk_quota_bytes": 1000, "cleanup_policy": "conservative"}),
    )
    .await;

    let v = h.get("/api/devices/device-a/detail").await.json();
    let list = v["bindings"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    let b = &list[0];
    assert_eq!(
        b["project_name"],
        json!("指标台"),
        "设备页要能直接显示项目名"
    );
    assert_eq!(b["fixed_dir"], json!("/srv/metrics"));
    assert_eq!(b["cleanup_policy"], json!("conservative"));
    assert_eq!(b["disk_quota_bytes"], json!(1000));
    // 用量 Hub 算不出来：**是 null 不是 0**。
    assert!(
        b["used_bytes"].is_null(),
        "Hub 没有那台设备的文件系统，用量只能是 null；实际 {}",
        b["used_bytes"]
    );
    // 有绑定 → 有 disk 块，配额求和。
    assert_eq!(v["disk"]["quota_bytes"], json!(1000));
}

// ---- 测试标准 2：未绑项目的设备 -------------------------------------------

/// 没绑任何项目时：`bindings` 是空数组、`disk` 是 **null**。
///
/// `disk` 回 `{quota: null, used: 0}` 的话，页面会渲染成「已用 0 B」——
/// 那句话把一个「这台机器还没被用起来」说成了「用光了但很整齐」。
/// 页面据 `disk === null` 显示「还没有绑定项目」。
#[tokio::test]
async fn a_device_with_no_bindings_says_so_instead_of_showing_an_empty_block() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let v = h.get("/api/devices/device-a/detail").await.json();
    assert_eq!(
        v["bindings"].as_array().map(|a| a.len()),
        Some(0),
        "没绑就是空数组（页面据此说「还没有绑定项目」）：{v}"
    );
    assert!(
        v["disk"].is_null(),
        "没有绑定就没有「磁盘使用」这句话可说，必须是 null 而不是 {{used: 0}}；实际 {}",
        v["disk"]
    );
}

/// 绑定挂在**别的**设备上时，这台设备仍然是「还没绑」。
///
/// 这条防的是把 `list_for_device` 写成 `list()` 那类错误——症状是每台设备
/// 都显示所有项目的绑定，用户看到的是一张张一样的表。
#[tokio::test]
async fn bindings_of_another_device_do_not_leak_in() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    common::insert_legacy_device(&hub, "device-b");
    let h = Harness::from_hub(&hub).await;

    let project = h.project("只跑在 b 上").await;
    h.bind(&project, "device-b", json!({"fixed_dir": "/srv/b"}))
        .await;

    let a = h.get("/api/devices/device-a/detail").await.json();
    assert_eq!(
        a["bindings"].as_array().map(|v| v.len()),
        Some(0),
        "device-a 上不该出现 device-b 的绑定：{a}"
    );

    let b = h.get("/api/devices/device-b/detail").await.json();
    assert_eq!(b["bindings"].as_array().map(|v| v.len()), Some(1));
}

/// 不存在的设备 → 404，不是一个空壳。
#[tokio::test]
async fn an_unknown_device_is_404() {
    let hub = common::Hub::boot().await;
    let h = Harness::from_hub(&hub).await;

    let r = h.get("/api/devices/nope/detail").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
    assert_eq!(r.json()["error"], json!("not_found"));
}

// ---- 失败归因 ------------------------------------------------------------

/// 失败归因按状态分三类，**中止不算失败**。
///
/// 混在一起的话，一台只是被用户主动停了两次的机器，页面上会写「失败 2 次」，
/// 于是用户去查一个根本不存在的问题。`ExecutionState::Aborted` 的注释写死了
/// 「不是失败——我们主动停的与它自己挂的在失败归因里是两件事」。
#[tokio::test]
async fn aborted_executions_are_not_counted_as_failures() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let project = h.project("归因").await;
    let binding = h
        .bind(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;

    seed_execution(&h, &binding, "e1", "succeeded", "t1");
    seed_execution(&h, &binding, "e2", "failed", "t2");
    seed_execution(&h, &binding, "e3", "aborted", "t3");
    seed_execution(&h, &binding, "e4", "running", "t4");

    let v = h.get("/api/devices/device-a/detail").await.json();
    let f = &v["failure_breakdown"];
    assert_eq!(f["succeeded"], json!(1), "{v}");
    assert_eq!(f["failed"], json!(1), "中止不能被算进失败：{v}");
    assert_eq!(f["aborted"], json!(1));
    assert_eq!(
        f["in_flight"],
        json!(1),
        "还在跑的单独一档：算进分母会让成功率随「现在有几条在跑」抖动"
    );
}

/// 有历史执行 → `deletable.allowed` 为假，且理由里带条数。
///
/// **与 `DELETE` 端点同一个数**：页面上说「能删」、真去删却 409，是最让人
/// 困惑的一种不一致。所以这里断的条数必须与 `execution_count_for_device` 一致。
#[tokio::test]
async fn the_deletable_block_agrees_with_the_delete_endpoint() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let project = h.project("要删").await;
    let binding = h
        .bind(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;
    seed_execution(&h, &binding, "e1", "succeeded", "t1");

    let v = h.get("/api/devices/device-a/detail").await.json();
    assert_eq!(v["deletable"]["allowed"], json!(false));
    assert_eq!(v["deletable"]["executions"], json!(1));
    let reason = v["deletable"]["reason"].as_str().unwrap();
    assert!(reason.contains('1'), "理由里要带条数：{reason}");

    // 端点也必须拒。
    let del = h.mutate("DELETE", "/api/devices/device-a", None).await;
    assert_eq!(del.status, StatusCode::CONFLICT, "body: {}", del.body);
}

/// 没有历史执行 → 能删，且没有理由那句话。
#[tokio::test]
async fn a_clean_device_is_deletable() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let v = h.get("/api/devices/device-a/detail").await.json();
    assert_eq!(v["deletable"]["allowed"], json!(true));
    assert!(
        v["deletable"]["reason"].is_null(),
        "能删的时候不该有理由：{}",
        v["deletable"]["reason"]
    );
}

// ---- 测试标准 3：0 次执行的成功率是 null --------------------------------

/// 一次执行都没有 → `success_rate` 是 **null**，不是 0。
///
/// 0/0 与 0/10 是两句话：「还没跑过」与「一次都没成」。合成 0% 之后，
/// 一个刚装好的 agent 会立刻显示「成功率 0%」，用户会以为它坏了。
#[tokio::test]
async fn zero_executions_report_no_rate_at_all() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", true);

    let r = h.get("/api/agents/claude-code").await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let v = r.json();
    assert!(
        v["success_rate"].is_null(),
        "0 次执行必须报 null（页面显示「—」），不能是 0%；实际 {}",
        v["success_rate"]
    );
    assert_eq!(
        v["task_history"].as_array().map(|a| a.len()),
        Some(0),
        "没跑过就是空数组"
    );
}

/// 跑过之后成功率按**已结束**的执行算，在跑的不进分母。
#[tokio::test]
async fn the_success_rate_counts_only_settled_executions() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", true);

    let project = h.project("成功率").await;
    let binding = h
        .bind(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;
    seed_task(&h, "t1", &project, "加个健康检查");
    seed_execution(&h, &binding, "e1", "succeeded", "t1");
    seed_execution(&h, &binding, "e2", "succeeded", "t1");
    seed_execution(&h, &binding, "e3", "failed", "t1");
    // 还在跑：**不该进分母**，否则成功率会随「现在有几条在跑」抖动。
    seed_execution(&h, &binding, "e4", "running", "t1");

    let v = h.get("/api/agents/claude-code").await.json();
    assert_eq!(
        v["success_rate"],
        json!(66),
        "2 成 1 败 = 66%；running 那条不进分母。实际 {v}"
    );
    assert_eq!(v["breakdown"]["in_flight"], json!(1));

    // 历史里带得上任务标题，页面不用再去拉一遍。
    let first = &v["task_history"][0];
    assert_eq!(first["task_title"], json!("加个健康检查"));
    assert_eq!(first["project_name"], json!("成功率"));
}

/// 只跑过一次且成功 → 100%；一次都没成 → 0%。
///
/// 这是与上一条对照的：`0%` 是一个**真实结果**，不能被误伤成 `null`。
#[tokio::test]
async fn a_real_zero_percent_is_reported_as_zero() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", true);

    let project = h.project("全败").await;
    let binding = h
        .bind(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;
    seed_execution(&h, &binding, "e1", "failed", "t1");

    let v = h.get("/api/agents/claude-code").await.json();
    assert_eq!(
        v["success_rate"],
        json!(0),
        "跑过但全败是真实的 0%，不能报 null；实际 {v}"
    );
}

/// 四种状态各计一档，且 `aborted` 单列。
///
/// 上一条只跑了 succeeded，所以 `Failed` / `Aborted` / `in_flight` 三个分支
/// 在覆盖率里是空的。这条把它们一次走全：**每个臂都是一条一行的算术**，
/// 而算错一格会让「成功率 50%」变成一个假的数字。
#[tokio::test]
async fn every_execution_state_lands_in_its_own_bucket() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", true);

    let project = h.project("四档").await;
    let binding = h
        .bind(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;
    seed_execution(&h, &binding, "e1", "succeeded", "t1");
    seed_execution(&h, &binding, "e2", "failed", "t1");
    seed_execution(&h, &binding, "e3", "aborted", "t1");
    seed_execution(&h, &binding, "e4", "pending", "t1");

    let v = h.get("/api/agents/claude-code").await.json();
    let b = &v["breakdown"];
    assert_eq!(b["succeeded"], json!(1), "{v}");
    assert_eq!(b["failed"], json!(1));
    assert_eq!(b["aborted"], json!(1), "中止单列，不并进失败：{v}");
    assert_eq!(b["in_flight"], json!(1), "pending 也是「还没结束」");
    // 3 条已结束、1 条成功 → 33%；pending 不进分母。
    assert_eq!(v["success_rate"], json!(33), "{v}");
    // 四条都进历史，包括还在跑的那条。
    assert_eq!(v["task_history"].as_array().map(|a| a.len()), Some(4));
}

/// 没装的 agent → 404。
///
/// 回一个「全都不可用」的空壳会让用户去查一台根本没装它的机器。
#[tokio::test]
async fn an_agent_that_is_not_installed_anywhere_is_404() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let r = h.get("/api/agents/ghost").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
}

/// 全都不行、且**哪台都没报原因** → `error` 是 `null`，不编一句。
///
/// 反过来，只要有原因就要拼出来（见上一条）。两条一起把「汇总各台原因」
/// 那段代码的两头都走到：全 `None` 时它是空集，不能变成空串 `""`——
/// 空串在页面上是一条没有内容的红条。
#[tokio::test]
async fn an_unavailable_agent_without_reasons_reports_none() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", false);

    let v = h.get("/api/agents/claude-code").await.json();
    assert_eq!(v["available"], json!(false));
    assert!(
        v["error"].is_null(),
        "设备没给原因时页面不该收到一个编出来的理由；实际 {}",
        v["error"]
    );
}

/// 两台机器都跑不起来、各自有原因 → 拼成一句，去重。
#[tokio::test]
async fn reasons_from_several_devices_are_joined() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    common::insert_legacy_device(&hub, "device-b");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", false);
    seed_agent(&h, "device-b", "claude-code", false);
    // 两台报同一个原因：用户要修的是同一件事，说两遍只是噪音。
    for node in ["device-a", "device-b"] {
        h.pool
            .get()
            .unwrap()
            .execute(
                "UPDATE agent_runtime SET probe_error = '没有 acp 子命令' WHERE node_id = ?1",
                [node],
            )
            .unwrap();
    }

    let v = h.get("/api/agents/claude-code").await.json();
    assert_eq!(v["error"], json!("没有 acp 子命令"), "{v}");
    assert_eq!(v["devices"].as_array().map(|a| a.len()), Some(2));
}

/// agent 详情要带「口径说明」，且那句话不是空的。
///
/// 执行记录里**没有**记「这条是哪个 agent 跑的」，所以 `task_history` 是按
/// 「那台设备上装了这个 agent」反推的。用户看到成功率时会当真，那句话必须
/// 显示在页面上——所以它得随响应发下来，而不是只活在代码注释里。
#[tokio::test]
async fn the_agent_detail_explains_where_the_history_comes_from() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;
    seed_agent(&h, "device-a", "claude-code", true);

    let v = h.get("/api/agents/claude-code").await.json();
    let caveat = v["task_history_caveat"].as_str().expect("必须有口径说明");
    assert!(
        caveat.contains("agent") && caveat.len() > 20,
        "口径说明要真的说清楚，实际是 {caveat:?}"
    );
    // 装在哪几台设备上也要能看到。
    assert_eq!(v["devices"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(v["devices"][0]["node_id"], json!("device-a"));
}

// ---- 认证 ----------------------------------------------------------------

/// 两条详情端点都在认证之后。
#[tokio::test]
async fn both_detail_endpoints_are_behind_auth() {
    let hub = common::Hub::boot().await;
    let app = build_app(hub.state.clone());

    for path in ["/api/devices/d/detail", "/api/agents/claude-code"] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET {path} 没有 session 时必须 401"
        );
    }
}
