//! 子任务 4「绑定 = `(project, device) → 固定目录`」的端到端测试。
//!
//! 这一组要证的是**接口层没有把存储层已经判对的事做丢**，尤其是两个入口
//! 写的是同一行。存储层自己已经有单测（`binding.rs` 的 15 个），所以这里
//! 走真路由（`build_app` + `oneshot`），断言的是用户能看见的那一面：
//! 行数有没有涨、两侧读不读得到同一份、错误码能不能区分。
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
    /// 直连库的句柄：建执行记录走真路由造不出来（见 `seed_execution`）。
    pool: hub_storage::db::Db,
}

impl Harness {
    async fn new() -> Self {
        let hub = common::Hub::boot().await;
        Self::from_hub(&hub).await
    }

    /// 在一个**已经准备好前置数据**的 Hub 上建 harness。
    ///
    /// 删设备的用例需要先有一行 `device`（否则 `delete_and_mark_removed` 删 0 行
    /// 直接 404，测不到前置检查）。前置数据必须在 boot 之后、登录之前插——
    /// 反过来的话测试要么自己重登一次，要么得把 `Harness` 的字段拆开。
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
        // 带上 session 与 CSRF 两个 cookie：少了 session 一律 401（中间件在
        // 任何路由之前），少了 CSRF 一律 403。抄漏一个，全文件的失败原因
        // 都会变成「认证没过」，而被测的东西一次也没跑到。
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

    /// 从项目侧写一行绑定。
    async fn put_from_project(&self, project_id: &str, device_id: &str, body: Value) -> Reply {
        self.mutate(
            "PUT",
            &format!("/api/projects/{project_id}/bindings/{device_id}"),
            Some(body),
        )
        .await
    }

    /// 从设备侧写**同一行**。
    async fn put_from_device(&self, project_id: &str, device_id: &str, body: Value) -> Reply {
        self.mutate(
            "PUT",
            &format!("/api/devices/{device_id}/bindings/{project_id}"),
            Some(body),
        )
        .await
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

// ---- 测试标准 1：重复 PUT 更新同一行，行数不变 ---------------------------

/// 同一 `(project, device)` 第二次 PUT 是**更新**而不是插新行。
///
/// 这是「PUT 是全仓唯一允许 upsert 的地方」的具体含义。允许插新行的话，
/// 同一个项目在同一台设备上会有两个固定目录——配额算的「这个项目在这台设备
/// 上用掉多少」立刻失去主语。
#[tokio::test]
async fn putting_twice_updates_the_same_row_and_does_not_grow_the_table() {
    let h = Harness::new().await;
    let project = h.project("指标台").await;

    let first = h
        .put_from_project(
            &project,
            "device-a",
            json!({"fixed_dir": "/srv/a", "cleanup_policy": "balanced"}),
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "body: {}", first.body);

    let second = h
        .put_from_project(
            &project,
            "device-a",
            json!({"fixed_dir": "/srv/a", "cleanup_policy": "aggressive"}),
        )
        .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "第二次是「改」不是「建」，状态码要说出来，body: {}",
        second.body
    );
    assert_eq!(
        second.json()["id"],
        first.json()["id"],
        "必须是同一行：id 变了说明插了新记录"
    );
    assert_eq!(second.json()["cleanup_policy"], json!("aggressive"));

    let list = h
        .get(&format!("/api/projects/{project}/bindings"))
        .await
        .json();
    assert_eq!(
        list.as_array().map(|a| a.len()),
        Some(1),
        "行数必须还是 1，实际 {list}"
    );
}

// ---- 测试标准 2：同一项目两台设备可以绑不同目录 ---------------------------

/// 唯一约束是 `(project_id, device_id)`，**不是** `project_id`。
///
/// 这条用例防的是一个很容易写错的索引：只按项目唯一的话，同一个项目就只能
/// 跑在一台设备上，而「多设备并行」正是这个产品的核心卖点之一。
#[tokio::test]
async fn one_project_can_pin_different_directories_on_different_devices() {
    let h = Harness::new().await;
    let project = h.project("多机并行").await;

    for (device, dir) in [("device-a", "/srv/a"), ("device-b", "/srv/b")] {
        let r = h
            .put_from_project(&project, device, json!({"fixed_dir": dir}))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
        assert_eq!(r.json()["fixed_dir"], json!(dir));
    }

    let list = h
        .get(&format!("/api/projects/{project}/bindings"))
        .await
        .json();
    assert_eq!(list.as_array().map(|a| a.len()), Some(2));
}

// ---- 测试标准 4：双向等价 ------------------------------------------------

/// 从项目侧写、从设备侧读；再从设备侧改、从项目侧读。
///
/// **这是本子任务最核心的一条**：`PRODUCT.md` §6.1 要求两侧都能改，而「两侧
/// 都能改」的错误实现方式是两套存储——那样用户在设备页改完之后，项目页还
/// 显示旧目录，而且没有任何地方会报错。
#[tokio::test]
async fn both_entry_points_write_the_same_row() {
    let h = Harness::new().await;
    let project = h.project("双向").await;

    // 项目侧写。
    let created = h
        .put_from_project(
            &project,
            "device-a",
            json!({"fixed_dir": "/srv/from-project"}),
        )
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );

    // 设备侧读得到同一行。
    let via_device = h
        .get(&format!("/api/devices/device-a/bindings/{project}"))
        .await;
    assert_eq!(
        via_device.status,
        StatusCode::OK,
        "body: {}",
        via_device.body
    );
    assert_eq!(via_device.json()["id"], created.json()["id"]);
    assert_eq!(via_device.json()["fixed_dir"], json!("/srv/from-project"));

    // 设备侧改（目录没变，只改配额——改目录在有执行记录时会被拒，见下一条）。
    let updated = h
        .put_from_device(
            &project,
            "device-a",
            json!({"fixed_dir": "/srv/from-project", "disk_quota_bytes": 12345}),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK, "body: {}", updated.body);

    // 项目侧看到的是同一次改动。
    let via_project = h
        .get(&format!("/api/projects/{project}/bindings/device-a"))
        .await;
    assert_eq!(via_project.json()["id"], created.json()["id"], "同一行");
    assert_eq!(
        via_project.json()["disk_quota_bytes"],
        json!(12345),
        "设备侧改的配额，项目侧必须看得见——看不见就说明有两套存储"
    );

    // 设备侧的列表也只该有一行。
    let device_list = h.get("/api/devices/device-a/bindings").await.json();
    assert_eq!(device_list.as_array().map(|a| a.len()), Some(1));
}

// ---- 测试标准 5：有历史执行的设备不可删 ----------------------------------

/// 无历史执行 → 删除成功（204）。
#[tokio::test]
async fn a_device_without_executions_can_be_deleted() {
    let hub = common::Hub::boot().await;
    // 必须先有一行设备：`delete_and_mark_removed` 删 0 行就是 404，
    // 而「设备不存在」与「设备不许删」是两条完全不同的路。
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let project = h.project("干净设备").await;
    // 先绑一次，确认绑定本身不构成「历史执行」。
    h.put_from_project(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;

    let r = h.mutate("DELETE", "/api/devices/device-a", None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "body: {}", r.body);
}

/// 有历史执行 → 409，且响应体里带条数。
///
/// `PRODUCT.md` §6.1 原文：「若该设备上有历史执行记录，不允许删除设备（先归档
/// 或转移项目）；只有无历史时才可删除，历史执行记录永久保留」。
///
/// **带条数是这条用例的重点**：只说「不能删」，用户判断不了要不要等；
/// 说「有 N 条历史执行」，他才知道这一步没有重试的价值。
#[tokio::test]
async fn a_device_with_executions_is_refused_with_the_count() {
    let hub = common::Hub::boot().await;
    common::insert_legacy_device(&hub, "device-a");
    let h = Harness::from_hub(&hub).await;

    let project = h.project("有历史").await;
    h.put_from_project(&project, "device-a", json!({"fixed_dir": "/srv/a"}))
        .await;
    seed_execution(&h, &project, "device-a");

    let r = h.mutate("DELETE", "/api/devices/device-a", None).await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "有历史执行必须是 409，不是 500 也不是 204；body: {}",
        r.body
    );
    let v = r.json();
    assert_eq!(v["error"], json!("device_has_executions"));
    // 附加字段落在 `detail` 里，与 `shape_transition_refused` 同一口径：
    // 顶层的 `error` 永远只有一个固定 code，页面按它选文案。
    assert_eq!(
        v["detail"]["executions"],
        json!(1),
        "响应里必须带条数，页面才能说清「有 N 条历史执行」；实际 {v}"
    );
}

// ---- `fixed_dir` 有执行记录之后不能改 ------------------------------------

/// 改了固定目录但已有执行 → 409，且带上条数。
///
/// worktree 建在固定目录下，改目录会让正在跑（以及已跑完但 worktree 还在）
/// 的执行失去归属——它们的 `worktree_path` 指向旧目录，而清理与配额此后会去
/// 新目录里找。
#[tokio::test]
async fn changing_the_fixed_dir_after_executions_is_refused() {
    let h = Harness::new().await;
    let project = h.project("目录锁定").await;
    h.put_from_project(&project, "device-a", json!({"fixed_dir": "/srv/old"}))
        .await;
    seed_execution(&h, &project, "device-a");

    let r = h
        .put_from_project(&project, "device-a", json!({"fixed_dir": "/srv/new"}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["error"], json!("fixed_dir_locked"));
    assert_eq!(v["detail"]["executions"], json!(1));

    // 目录没变，只改配额——放行。这正是「目录锁定」不等于「整行锁死」。
    let ok = h
        .put_from_project(
            &project,
            "device-a",
            json!({"fixed_dir": "/srv/old", "disk_quota_bytes": 999}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "body: {}", ok.body);
    assert_eq!(ok.json()["fixed_dir"], json!("/srv/old"), "旧目录不变");
}

// ---- 输入校验 ------------------------------------------------------------

/// 空的 `fixed_dir` 要拒：worktree 会建在设备的根上。
#[tokio::test]
async fn an_empty_fixed_dir_is_rejected() {
    let h = Harness::new().await;
    let project = h.project("空目录").await;

    for body in [json!({"fixed_dir": ""}), json!({"fixed_dir": "   "})] {
        let r = h.put_from_project(&project, "device-a", body.clone()).await;
        assert_eq!(
            r.status,
            StatusCode::BAD_REQUEST,
            "body: {} {:?}",
            r.body,
            body
        );
        assert_eq!(r.json()["error"], json!("invalid_request_body"));
    }
}

/// 策略是闭集：表外的词报错，**不回落默认值**。
///
/// 静默回落的后果是「用户以为设成了保守，实际在跑完即清」——而清理策略决定
/// 的是会不会丢产物，猜错一次就没了。
#[tokio::test]
async fn an_unknown_cleanup_policy_is_rejected() {
    let h = Harness::new().await;
    let project = h.project("策略闭集").await;

    let r = h
        .put_from_project(
            &project,
            "device-a",
            json!({"fixed_dir": "/srv/a", "cleanup_policy": "yolo"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    assert_eq!(r.json()["error"], json!("invalid_cleanup_policy"));
}

/// 项目不存在时 404，而不是一个空列表。
///
/// 空列表会让页面显示「还没有绑定任何设备」——那句话在项目不存在时是错的，
/// 用户会以为是自己还没绑。
#[tokio::test]
async fn bindings_of_a_missing_project_are_404() {
    let h = Harness::new().await;

    let list = h.get("/api/projects/p_nope/bindings").await;
    assert_eq!(list.status, StatusCode::NOT_FOUND);
    assert_eq!(list.json()["error"], json!("not_found"));

    let one = h.get("/api/projects/p_nope/bindings/device-a").await;
    assert_eq!(one.status, StatusCode::NOT_FOUND);
}

/// 往一个**不存在**的项目上写绑定 → 404（而不是建出一行孤儿）。
///
/// 与上一条分开是因为写路径多一层：读只查一次项目，写要查项目、查设备、
/// 再 upsert。漏掉项目那一查的话，绑定行会挂在一个空的项目 id 上——
/// 它在设备页显示成一个没有名字的分组，而项目页根本看不到它。
#[tokio::test]
async fn binding_a_missing_project_is_404_and_creates_nothing() {
    let h = Harness::new().await;

    let put = h
        .put_from_project("p_nope", "device-a", json!({ "fixed_dir": "/srv/x" }))
        .await;
    assert_eq!(put.status, StatusCode::NOT_FOUND, "body: {}", put.body);
    assert_eq!(put.json()["error"], json!("not_found"));

    // 设备视角那边也不该多出一行。
    let dev = h.get("/api/devices/device-a/bindings").await;
    assert_eq!(
        dev.json().as_array().map(|a| a.len()),
        Some(0),
        "写失败不该在库里留下半行：{}",
        dev.body
    );
}

/// 一条绑定都不存在时，读单行是 404（而不是一个空壳）。
#[tokio::test]
async fn reading_a_missing_binding_is_404() {
    let h = Harness::new().await;
    let project = h.project("没绑过").await;

    let r = h
        .get(&format!("/api/projects/{project}/bindings/device-none"))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
}

/// 所有绑定端点都在认证之后（与 `/api/projects` 同一口径）。
#[tokio::test]
async fn every_binding_endpoint_is_behind_auth() {
    let hub = common::Hub::boot().await;
    let app = build_app(hub.state.clone());

    for (method, path) in [
        ("GET", "/api/projects/p/bindings"),
        ("GET", "/api/projects/p/bindings/d"),
        ("PUT", "/api/projects/p/bindings/d"),
        ("GET", "/api/devices/d/bindings"),
        ("GET", "/api/devices/d/bindings/p"),
        ("PUT", "/api/devices/d/bindings/p"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} 没有 session 时必须 401"
        );
    }
}

/// 直接在库里造一条执行记录，挂在 `(project, device)` 的绑定下。
///
/// 走真路由造不出来：建执行需要一条完整的「任务 → 派活 → 设备回话」链路，
/// 而那归后续里程碑。这里只写库，因为被测的是**删除与改目录的前置检查**，
/// 不是那条链路。
///
/// `task_id` 在表上是 `NOT NULL` 但没有外键（v5 刻意如此：任务与执行是两代
/// 实体，老执行没有任务）。所以这里写一个裸 id，不造假任务行。
fn seed_execution(h: &Harness, project_id: &str, device_id: &str) {
    let conn = h.pool.get().expect("取连接");
    let n = conn
        .execute(
            "INSERT INTO execution (id, task_id, binding_id, state, created_at)
             SELECT 'e1', 't_placeholder', id, 'succeeded', 'unix:0'
               FROM binding WHERE project_id = ?1 AND device_id = ?2",
            (&project_id, &device_id),
        )
        .expect("写执行记录");
    assert_eq!(
        n, 1,
        "必须真的挂上了那条绑定，否则「有历史执行」的前提不成立"
    );
}
