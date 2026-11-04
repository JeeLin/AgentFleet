//! 子任务 2「项目实体三形态」的端到端测试。
//!
//! 这些用例走**真路由**（`build_app` + `oneshot`），不是直接调存储层——
//! 存储层已经有自己的 15 个单测，本文件要证的是**接口层没有把它做丢**：
//! 非法值是否真的以 400 出去、形态转换被拒时错误体里有没有说明、归档之后
//! 列表里是否真的看不见。这些都是「单测绿而 API 红」的典型位置。
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
}

impl Harness {
    async fn new() -> Self {
        let hub = common::Hub::boot().await;
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
            .expect("login responds");
        assert_eq!(login.status(), StatusCode::OK, "登录必须成功");
        let session = cookie_of(&login, SESSION_COOKIE).expect("af_session");
        let csrf = cookie_of(&login, CSRF_COOKIE).expect("af_csrf");
        // `hub` 被 drop 在这里：本文件的用例不需要设备连接，只要路由。
        Self { app, session, csrf }
    }

    async fn send(&self, mut req: Request<Body>) -> Reply {
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

    /// 带 CSRF 双提交的状态变更请求。
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

    async fn create(&self, body: Value) -> Reply {
        self.mutate("POST", "/api/projects", Some(body)).await
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

// ---- 建项目 --------------------------------------------------------------

/// 建一个隔离式项目：形态原样回来，且**没有** `archived_at`。
#[tokio::test]
async fn creating_an_isolated_project_round_trips_its_shape() {
    let h = Harness::new().await;
    let r = h
        .create(json!({"name": "内部指标台", "shape": "isolated"}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["name"], json!("内部指标台"));
    assert_eq!(v["shape"], json!("isolated"));
    assert_eq!(v["theme_color"], Value::Null, "没选颜色就是 null");
    assert_eq!(v["archived_at"], Value::Null, "新建的项目不该是已归档");
    assert!(v["id"].as_str().is_some_and(|s| !s.is_empty()), "必须给 id");
}

/// 测试标准 1：非法的 `shape` **被拒**（400），不是静默存成默认值。
///
/// 这条是子任务 2 的核心断言之一。静默回落的后果很具体：
/// 库里存了 `central` 而磁盘上什么都没有，下次读取时页面会声称
/// 「代码在 Hub 上」。
#[tokio::test]
async fn an_unknown_shape_is_rejected_with_400_not_stored_as_default() {
    let h = Harness::new().await;
    let r = h.create(json!({"name": "x", "shape": "sneaky"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    assert_eq!(r.json()["error"], json!("invalid_project_shape"));

    // 而且**什么都没建**：列表仍然空。
    let list = h.get("/api/projects").await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json(), json!([]), "被拒的建项目请求不得留下任何行");
}

/// 缺省形态落在能反悔的那一侧（`isolated`）。
///
/// 理由写在 handler 里：把 `central` 猜成 `isolated` 只是多点一次升级，
/// 而反方向按 `PRODUCT.md` §3.2 **不能改回去**。
#[tokio::test]
async fn a_missing_shape_defaults_to_isolated_the_reversible_side() {
    let h = Harness::new().await;
    let r = h.create(json!({"name": "没写形态"})).await;
    assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    assert_eq!(r.json()["shape"], json!("isolated"));
}

/// 非法的 `theme_color` 同样被拒，而不是当成「没选」。
#[tokio::test]
async fn an_unknown_theme_color_is_rejected_with_400() {
    let h = Harness::new().await;
    let r = h
        .create(json!({"name": "x", "shape": "central", "theme_color": "chartreuse"}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    assert_eq!(r.json()["error"], json!("invalid_theme_color"));
}

/// 空名字被拒：`""` 与只有空格都是 400。
///
/// 不做「空名字就当没填、自动生成一个」——项目名是用户在列表里唯一的
/// 辨认依据，自动生成的名字（`project-1`）会让列表变成一串编号。
#[tokio::test]
async fn an_empty_name_is_rejected() {
    let h = Harness::new().await;
    for name in ["", "   ", "\t\n"] {
        let r = h.create(json!({"name": name, "shape": "isolated"})).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "name={name:?}");
    }
}

// ---- 读与列 --------------------------------------------------------------

#[tokio::test]
async fn a_missing_project_is_404_with_a_fixed_code() {
    let h = Harness::new().await;
    let r = h.get("/api/projects/nope").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"], json!("not_found"));
}

/// 列表按 `created_at` 降序。
///
/// **不断言同一秒内的具体次序**：三个项目在同一个测试里往往落在同一秒，
/// 那时 `ORDER BY created_at DESC, id DESC` 的次序由 id 决定，而 id 是随机的。
/// 断言具体名字顺序会变成一条随机红的用例。这里断言的是契约本身：
/// 全部都在，且 `created_at` 单调不增。
#[tokio::test]
async fn listing_keeps_every_project_and_orders_by_created_at_desc() {
    let h = Harness::new().await;
    for name in ["第一", "第二", "第三"] {
        let r = h.create(json!({"name": name, "shape": "isolated"})).await;
        assert_eq!(r.status, StatusCode::CREATED, "body: {}", r.body);
    }
    let arr = h.get("/api/projects").await.json();
    let arr = arr.as_array().expect("array");
    assert_eq!(arr.len(), 3, "三个项目一个都不能少");

    let mut names: Vec<&str> = arr
        .iter()
        .map(|v| v["name"].as_str().expect("name"))
        .collect();
    names.sort_unstable();
    let mut want = vec!["第一", "第二", "第三"];
    want.sort_unstable();
    assert_eq!(names, want, "名字集合必须完整");

    let stamps: Vec<&str> = arr
        .iter()
        .map(|v| v["created_at"].as_str().expect("created_at"))
        .collect();
    for w in stamps.windows(2) {
        assert!(w[0] >= w[1], "created_at 必须单调不增：{stamps:?}");
    }
}

// ---- 改名称与颜色 ---------------------------------------------------------

/// `PATCH` 只改名称时，颜色保持原样。
#[tokio::test]
async fn patch_renames_without_touching_the_color() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "旧名", "shape": "isolated", "theme_color": "blue"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate(
            "PATCH",
            &format!("/api/projects/{id}"),
            Some(json!({"name": "新名"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    let v = r.json();
    assert_eq!(v["name"], json!("新名"));
    assert_eq!(v["theme_color"], json!("blue"), "没给颜色就该保持原值");
    assert_eq!(v["shape"], json!("isolated"), "PATCH 不许动形态");
}

/// 显式 `null` 清除颜色——这是「没给」与「给 null」必须分开的证明。
#[tokio::test]
async fn patch_with_explicit_null_clears_the_color() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "有颜色", "shape": "isolated", "theme_color": "purple"}))
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate(
            "PATCH",
            &format!("/api/projects/{id}"),
            Some(json!({"theme_color": null})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    assert_eq!(
        r.json()["theme_color"],
        Value::Null,
        "显式 null 必须清掉颜色，不能当成「别动」"
    );
}

/// `PATCH` 带 `shape` 不改变形态——那条路走 `POST .../shape`。
///
/// 把形态混进 PATCH 会让调用方以为改字段就够了，而
/// `isolated → central` 要求真实建仓。
#[tokio::test]
async fn patch_cannot_smuggle_a_shape_change() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "隔离的", "shape": "isolated"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate(
            "PATCH",
            &format!("/api/projects/{id}"),
            Some(json!({"shape": "central"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "未知字段应被忽略而不是报错");
    assert_eq!(
        r.json()["shape"],
        json!("isolated"),
        "形态不许被 PATCH 改掉"
    );
}

/// 显式给一个颜色要真写进去（与「没给」「给 null」三分）。
///
/// 三条分支各自对应一句不同的话：没给 = 别动、`null` = 清掉、给了值 = 设成它。
/// 少测「给了值」这条，`Some(Some(s))` 就只剩解析失败那条路径被走到——
/// 颜色能设但设不进去，页面上会表现成「选了颜色，刷新又变回去」。
#[tokio::test]
async fn patch_sets_an_explicit_color_and_rejects_a_bogus_one() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "换色", "shape": "isolated", "theme_color": "blue"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let ok = h
        .mutate(
            "PATCH",
            &format!("/api/projects/{id}"),
            Some(json!({"theme_color": "orange"})),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "body: {}", ok.body);
    assert_eq!(ok.json()["theme_color"], json!("orange"));

    // 颜色表是闭集：写错一个词的后果是它在四套主题下都找不到对应令牌。
    let bad = h
        .mutate(
            "PATCH",
            &format!("/api/projects/{id}"),
            Some(json!({"theme_color": "warm"})),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "body: {}", bad.body);
    assert_eq!(
        h.get(&format!("/api/projects/{id}")).await.json()["theme_color"],
        json!("orange"),
        "被拒的改名不该留下半截写入"
    );
}

/// 名字**只由空白组成**要拒——`is_empty()` 拦不住它，`trim()` 才拦得住。
///
/// 放过去的话，列表里会出现一行看不见名字的项目：用户点得到、说不出是哪条。
#[tokio::test]
async fn patch_rejects_a_whitespace_only_name() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "有名字", "shape": "isolated"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate(
            "PATCH",
            &format!("/api/projects/{id}"),
            Some(json!({"name": "   "})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    assert_eq!(
        h.get(&format!("/api/projects/{id}")).await.json()["name"],
        json!("有名字"),
        "被拒的改名不该落库"
    );
}

// ---- 形态转换 ------------------------------------------------------------

/// 测试标准 2c（允许方向）：`isolated → central` 成功。
#[tokio::test]
async fn isolated_can_upgrade_to_central() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "要升级", "shape": "isolated"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate(
            "POST",
            &format!("/api/projects/{id}/shape"),
            Some(json!({"shape": "central"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    assert_eq!(r.json()["shape"], json!("central"));
}

/// 测试标准 2c（拒绝方向）：`central → isolated` 与 `central → mirror` 都是 400，
/// **且错误体里带两端**——只说「不允许」用户不知道该改成什么。
#[tokio::test]
async fn central_cannot_go_back_to_isolated_or_to_mirror() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "中心式", "shape": "central"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    for to in ["isolated", "mirror"] {
        let r = h
            .mutate(
                "POST",
                &format!("/api/projects/{id}/shape"),
                Some(json!({"shape": to})),
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::BAD_REQUEST,
            "to={to} body: {}",
            r.body
        );
        let v = r.json();
        assert_eq!(v["error"], json!("shape_transition_refused"));
        assert_eq!(v["detail"]["from"], json!("central"), "要说明从哪来");
        assert_eq!(v["detail"]["to"], json!(to), "要说明到哪去");
    }

    // 形态确实没被改掉。
    let now = h.get(&format!("/api/projects/{id}")).await;
    assert_eq!(now.json()["shape"], json!("central"));
}

/// `mirror → *` 一律拒绝：镜像式要指一个上游，改回中心式等于凭空造一个上游。
#[tokio::test]
async fn mirror_cannot_transition_to_anything() {
    let h = Harness::new().await;
    let created = h.create(json!({"name": "镜像式", "shape": "mirror"})).await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    for to in ["central", "isolated"] {
        let r = h
            .mutate(
                "POST",
                &format!("/api/projects/{id}/shape"),
                Some(json!({"shape": to})),
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::BAD_REQUEST,
            "to={to} body: {}",
            r.body
        );
    }
}

/// `central → central` 也是被拒的：不是「合法转换」，是「没变」。
///
/// 让它成功会让调用方以为「转换做过了」，而实际上（子任务 3 之后）
/// 转换里要做的建仓与 fetch 一次都没执行。
#[tokio::test]
async fn changing_to_the_same_shape_is_refused_not_a_noop_success() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "中心式", "shape": "central"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate(
            "POST",
            &format!("/api/projects/{id}/shape"),
            Some(json!({"shape": "central"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {}", r.body);
    assert_eq!(r.json()["error"], json!("shape_transition_refused"));
}

/// 改形态时给一个不存在的项目 → 404，不是 400（「项目不存在」与「转换不允许」
/// 是两件事，页面给的下一步不同）。
#[tokio::test]
async fn changing_shape_of_a_missing_project_is_404() {
    let h = Harness::new().await;
    let r = h
        .mutate(
            "POST",
            "/api/projects/nope/shape",
            Some(json!({"shape": "central"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "body: {}", r.body);
}

// ---- 归档生命周期 --------------------------------------------------------

/// 测试标准 3 的接口面：归档后**默认列表里看不见**，`include_archived=1` 才看得见。
#[tokio::test]
async fn archiving_hides_the_project_from_the_list_but_keeps_it_readable() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "要归档", "shape": "isolated"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate("POST", &format!("/api/projects/{id}/archive"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    assert!(
        r.json()["archived_at"].as_str().is_some(),
        "归档必须留时间点，页面要显示「什么时候归档的」"
    );

    // 默认列表看不见。
    assert_eq!(h.get("/api/projects").await.json(), json!([]));
    // 显式要归档的才看得见。
    let with = h.get("/api/projects?include_archived=1").await.json();
    assert_eq!(with.as_array().map(|a| a.len()), Some(1));

    // **单个项目仍可读**：归档不是删除，历史执行记录要靠它挂。
    let one = h.get(&format!("/api/projects/{id}")).await;
    assert_eq!(one.status, StatusCode::OK, "归档的项目必须仍可单独读取");
}

/// 测试标准 4：取消归档后恢复。
#[tokio::test]
async fn unarchiving_brings_the_project_back() {
    let h = Harness::new().await;
    let created = h.create(json!({"name": "来回", "shape": "isolated"})).await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    h.mutate("POST", &format!("/api/projects/{id}/archive"), None)
        .await;
    let r = h
        .mutate("POST", &format!("/api/projects/{id}/unarchive"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "body: {}", r.body);
    assert_eq!(r.json()["archived_at"], Value::Null);
    assert_eq!(
        h.get("/api/projects")
            .await
            .json()
            .as_array()
            .map(|a| a.len()),
        Some(1),
        "取消归档后必须回到默认列表"
    );
}

/// 归档 / 取消归档一个**不存在**的项目：404，而不是 200 的空壳。
///
/// 存储层的 `archive` 用的是「读回一行」来区分「不存在」与「已归档」，
/// 所以这条路径在 repo 层是通的；但接口层的 `map_err` 若写漏，
/// 症状是**删掉的项目还能被归档**——返回 200 一个凭空造出来的 ProjectView，
/// 用户以为它还在。
#[tokio::test]
async fn archiving_or_unarchiving_a_missing_project_is_404() {
    let h = Harness::new().await;

    for (verb, path) in [
        ("POST", "/api/projects/p_nope/archive"),
        ("POST", "/api/projects/p_nope/unarchive"),
    ] {
        let r = h.mutate(verb, path, None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{verb} {path}：{}", r.body);
        assert_eq!(
            r.json()["error"],
            json!("not_found"),
            "{verb} {path} 必须是固定的错误码，前端按它选文案"
        );
    }
}

/// **没有删除路径**（测试标准 5 的接口面）：`DELETE /api/projects/{id}` 不存在。
///
/// 断言的是 404/405 而不是 403：403 意味着路由存在只是被拦住，
/// 那仍然是一条「以后放开就能删」的路径，而产品文档说的是**不提供**。
#[tokio::test]
async fn there_is_no_way_to_delete_a_project() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "不许删", "shape": "isolated"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let r = h
        .mutate("DELETE", &format!("/api/projects/{id}"), None)
        .await;
    assert!(
        r.status == StatusCode::METHOD_NOT_ALLOWED || r.status == StatusCode::NOT_FOUND,
        "DELETE 不该可用，拿到 {}（body: {}）",
        r.status,
        r.body
    );

    // 而且项目还在。
    assert_eq!(
        h.get(&format!("/api/projects/{id}")).await.status,
        StatusCode::OK
    );
}

// ---- 认证面 --------------------------------------------------------------

/// 每个项目端点都要有 **401**（不是 404）。
///
/// 404 说明路由压根没挂上，200 说明 `.layer(require_auth)` 没覆盖到它——
/// 两种「看起来像通过」的回归。与 `tests/api.rs` 同一条铁律。
#[tokio::test]
async fn every_project_endpoint_is_behind_auth() {
    let h = Harness::new().await;
    let created = h
        .create(json!({"name": "存在的", "shape": "isolated"}))
        .await;
    let id = created.json()["id"].as_str().unwrap().to_string();

    let cases: Vec<(&str, String)> = vec![
        ("GET", "/api/projects".to_string()),
        ("POST", "/api/projects".to_string()),
        ("GET", format!("/api/projects/{id}")),
        ("PATCH", format!("/api/projects/{id}")),
        ("POST", format!("/api/projects/{id}/shape")),
        ("POST", format!("/api/projects/{id}/archive")),
        ("POST", format!("/api/projects/{id}/unarchive")),
    ];

    for (method, path) in cases {
        // 不带 cookie、不带 CSRF：应被认证层拦下。
        let resp = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&path)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .expect("app responds");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} 必须 401（404 说明路由没挂上）"
        );
    }
}
