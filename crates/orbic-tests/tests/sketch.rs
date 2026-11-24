//! The documented sketches compile and pass (`docs/05-implementation-plan.md`
//! §3: 문서 예시, 단일 파이프라인, 두 레일). The code between the `BEGIN` and
//! `END` markers is `docs/01-dx-and-dsl.md` §1 verbatim except the `.room`
//! and `.every` lines (Phase 2/3); the app's own definitions the sketch
//! leaves out (`User`, `CurrentUser`, `AuthGuard`, `test_user()`) come from
//! the fixture crate, and `Project` is the fixture crate's copy of the
//! sketch's model (shared with the §2 functions).
#![cfg(feature = "server")]
#![allow(missing_docs)]

use orbic_tests::{AuthGuard, CurrentUser, Project, User, test_user};

// BEGIN docs/01-dx-and-dsl.md §1
use hyve::prelude::*;
use orbic::prelude::*;

#[model]
pub struct CreateProjectInput {
    #[validate(len = 1..=80)]
    pub title: String,
    pub is_public: bool,
}

// 2) 도메인 컴포넌트: 상태·가드·라우트·실시간 룸·Cron 워커를 한 단위로 캡슐화
pub fn project_component() -> BackendComponent {
    component()
        .wrap(AuthGuard::require_user())
        .wrap(RateLimit::per_minute(60))
        .post("/projects", create_project)
        .get("/projects", list_projects)
}

// 3) DB 체이닝 + 컴파일 타임 sql! 매크로 + L1/L2 Singleflight 캐시 활용
async fn list_projects(
    db: Ctx<Db>,
    cache: Ctx<Cache>,
    user: Ctx<CurrentUser>,
    cursor: Query<CursorParams>,
) -> OrbicResult<Page<Project>> {
    // Singleflight: 동시 캐시 미스 시 DB 쿼리는 단 1회만 실행하고 태그를 함께 등록
    cache
        .get_or_try_init(
            key!("projects", user.id, cursor.after),
            CacheOpts::ttl(ttl!("30s")).tag(tag!("projects", user.id)),
            || async {
                db.query::<Project>()
                    .where_eq(Project::OWNER_ID, user.id)
                    .cursor_after(cursor.after.clone(), 50)
                    .fetch_page()
                    .await
            },
        )
        .await
}

async fn create_project(
    db: Ctx<Db>,
    cache: Ctx<Cache>,
    user: Ctx<CurrentUser>,
    input: Valid<CreateProjectInput>,
) -> OrbicResult<Project> {
    let created = db
        .tx(|tx| async move {
            let data = input.into_inner();
            // 컴파일 타임 sql!은 레포에 커밋된 오프라인 메타데이터 캐시로 검증됨 (§4)
            // (운영 방언 전용 쿼리는 테스트 시 임시 Postgres 하네스 사용, SQLite에서도 검증된 쿼리와 체이닝 쿼리는 Db::in_memory() 호환)
            sql!(
                Project,
                "INSERT INTO projects (id, owner_id, title, is_public, version)
             VALUES ($1, $2, $3, $4, 1) RETURNING *",
                Id::<Project>::new(),
                user.id,
                data.title,
                data.is_public
            )
            .fetch_one(tx)
            .await
        })
        .await?;

    cache
        .invalidate_tag(tag!("projects", created.owner_id))
        .await?;
    Ok(created)
}

// 4) 루트 앱 조립 및 포트 없는 인메모리 TestClient 검증
pub fn app(db: Db, cache: Cache) -> OrbicApp {
    OrbicApp::new()
        .provide(db)
        .provide(cache)
        .openapi("/docs")
        .mount("/v1", project_component())
}

#[hyve::test]
async fn create_project_rejects_empty_title() {
    let client = TestClient::new(app(Db::in_memory().await, Cache::in_memory()));

    let res = client
        .post("/v1/projects")
        .as_user(test_user())
        .json(&CreateProjectInput {
            title: "".into(),
            is_public: false,
        })
        .send()
        .await;

    assert_eq!(res.status(), 400);
}
// END docs/01-dx-and-dsl.md §1

// BEGIN docs/01-dx-and-dsl.md §2 (server side; the shared part is in the
// fixture crate)
pub fn project_rpc_component() -> BackendComponent {
    orbic::component()
        .wrap(AuthGuard::require_user())
        .handle(create_project_rpc)
}
// END docs/01-dx-and-dsl.md §2

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use hyve::ErrorKind;
use hyve::api::{Rail, TestClient as _, provide_transport};
use orbic_tests::{CreateProject, create_project_rpc};

fn input(title: &str) -> CreateProjectInput {
    CreateProjectInput {
        title: title.into(),
        is_public: true,
    }
}

#[hyve::test]
async fn the_sketch_creates_lists_and_guards() {
    let client = TestClient::new(app(Db::in_memory().await, Cache::in_memory()));
    let ada = test_user();
    let as_ada = client.as_user(ada.clone());

    let r = client.get("/v1/projects").send().await;
    assert_eq!(
        (r.status().as_u16(), r.error().code()),
        (401, "unauthenticated")
    );

    let first: Project = as_ada
        .post("/v1/projects")
        .json(&input("one"))
        .send()
        .await
        .json();
    assert_eq!(
        (first.owner_id, first.title.as_str(), first.version),
        (ada.id, "one", 1)
    );
    let page: Page<Project> = as_ada.get("/v1/projects").send().await.json();
    assert_eq!(page.items, std::slice::from_ref(&first));

    // The tag invalidation in `create_project` drops the cached first page.
    let second: Project = as_ada
        .post("/v1/projects")
        .json(&input("two"))
        .send()
        .await
        .json();
    let r = as_ada.get("/v1/projects").send().await;
    assert_eq!(r.header("ratelimit-limit"), Some("60"));
    let page: Page<Project> = r.json();
    assert_eq!(page.items.len(), 2);
    assert!(page.items.contains(&second) && !page.has_more);

    // Someone else sees none of them.
    let other: Page<Project> = client
        .get("/v1/projects")
        .as_user(test_user())
        .send()
        .await
        .json();
    assert!(other.items.is_empty());

    // The OpenAPI document is served and describes the routes.
    let docs = client.get("/docs").send().await;
    assert_eq!(docs.status(), 200);
    assert!(docs.text().contains("\"/v1/projects\""));
}

#[hyve::test]
async fn as_user_does_not_skip_guards() {
    let app = OrbicApp::new().provide(Cache::in_memory()).mount(
        "/admin",
        component()
            .wrap(AuthGuard::require(|u: &CurrentUser| u.admin))
            .get("/", |u: Ctx<CurrentUser>| async move { Ok(u.name.clone()) }),
    );
    let client = TestClient::new(app);
    let r = client.get("/admin").as_user(test_user()).send().await;
    assert_eq!((r.status().as_u16(), r.error().code()), (403, "forbidden"));
    let admin = CurrentUser {
        admin: true,
        ..test_user()
    };
    assert_eq!(
        client
            .get("/admin")
            .as_user(admin)
            .send()
            .await
            .json::<String>(),
        "ada"
    );
}

/// The same scenario through `TestClient` and a real server answers alike
/// (`docs/05-implementation-plan.md` §3, 단일 파이프라인).
#[hyve::test]
async fn test_client_and_server_answer_alike() {
    let cache = Cache::in_memory();
    let client = TestClient::new(app(Db::in_memory().await, Cache::in_memory()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(orbic::serve(
        app(Db::in_memory().await, cache.clone()),
        listener,
        async {
            let _ = stopped.await;
        },
    ));
    // The same user with a session in each app.
    let ada = test_user();
    let token = |cache: &Cache| {
        orbic::__private::auth::issue(cache, &AuthConfig::default(), &ada, Client::Bearer)
            .unwrap()
            .session
    };
    let served_token = token(&cache);
    let local_token = token(&client.pipeline().provided::<Cache>().unwrap());

    let requests = |token: &str| {
        let auth = format!("Bearer {token}");
        let with = |b: http::request::Builder| b.header("x-request-id", "same").header("host", "x");
        vec![
            with(http::Request::post("/v1/projects"))
                .header("authorization", &auth)
                .header("content-type", "application/json")
                .body(Bytes::from(r#"{"title":"","isPublic":false}"#))
                .unwrap(),
            with(http::Request::get("/v1/projects"))
                .body(Bytes::new())
                .unwrap(),
            with(http::Request::get("/v1/projects"))
                .header("authorization", &auth)
                .body(Bytes::new())
                .unwrap(),
            with(http::Request::get("/nowhere"))
                .body(Bytes::new())
                .unwrap(),
            with(http::Request::get("/docs"))
                .body(Bytes::new())
                .unwrap(),
            with(http::Request::get("/health"))
                .body(Bytes::new())
                .unwrap(),
        ]
    };
    for (local, remote) in requests(&local_token)
        .into_iter()
        .zip(requests(&served_token))
    {
        let path = local.uri().to_string();
        let a = client.send_http(local).await.into_inner();
        let b = over_socket(addr, remote).await;
        let strip = |h: &http::HeaderMap| {
            let mut h = h.clone();
            for n in ["date", "content-length", "ratelimit-reset"] {
                h.remove(n);
            }
            h
        };
        assert_eq!(a.status(), b.status(), "{path}");
        assert_eq!(strip(a.headers()), strip(b.headers()), "{path}");
        assert_eq!(a.body(), b.body(), "{path}");
    }
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}

async fn over_socket(addr: SocketAddr, req: http::Request<Bytes>) -> http::Response<Bytes> {
    use http_body_util::{BodyExt, Full};
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let (parts, body) = req.into_parts();
    let res = sender
        .send_request(http::Request::from_parts(parts, Full::new(body)))
        .await
        .unwrap();
    let (parts, body) = res.into_parts();
    http::Response::from_parts(parts, body.collect().await.unwrap().to_bytes())
}

/// Runs a stub call with `scope`'s transport (as `TestClient::on` does).
fn within<C: std::future::IntoFuture>(scope: &Scope, call: C) -> hyve::Scoped<C::IntoFuture> {
    let future = scope.enter(|| call.into_future());
    scope.scoped(future)
}

fn descriptor<F: hyve::api::StubFn<A>, A>(_: F) -> &'static hyve::api::ServerFnDescriptor {
    <F::Marker as hyve::api::ServerFn>::descriptor()
}

fn rpc_app(db: Db) -> OrbicApp {
    OrbicApp::new()
        .provide(db)
        .provide(Cache::in_memory())
        .mount("/v1", project_rpc_component())
}

/// `#[server]` stubs on each rail, and in process, give the same results and
/// errors; a schema mismatch falls back to the standard rail
/// (`docs/05-implementation-plan.md` §3, 두 레일).
#[hyve::test]
async fn stubs_answer_alike_on_both_rails_and_in_process() {
    let client = TestClient::new(rpc_app(Db::in_memory().await));
    let ada = test_user();
    let as_ada = client.as_user(ada.clone());
    let create = |title: &str| {
        create_project_rpc(CreateProject {
            title: title.into(),
        })
    };

    let fast = as_ada.on(Rail::Fast, create("fast")).await.unwrap();
    let standard = as_ada.on(Rail::Standard, create("standard")).await.unwrap();
    let scope = Scope::current().unwrap().child();
    provide_transport(&scope, Arc::new(as_ada.in_process()));
    let local = within(&scope, create("local")).await.unwrap();
    for (p, title) in [(&fast, "fast"), (&standard, "standard"), (&local, "local")] {
        assert_eq!(
            (p.owner_id, p.title.as_str(), p.is_public, p.version),
            (ada.id, title, false, 1)
        );
    }
    assert_eq!(client.fallbacks(), 0);

    // The same errors: validation (checked by the stub before sending, then
    // by the server), and the guard's refusal.
    let errors = [
        as_ada.on(Rail::Fast, create("")).await.unwrap_err(),
        as_ada.on(Rail::Standard, create("")).await.unwrap_err(),
    ];
    for e in &errors {
        assert_eq!((e.kind(), e.code()), (ErrorKind::Domain, "invalid_request"));
    }
    assert_eq!(errors[0].details(), errors[1].details());
    let refused = [
        client.on(Rail::Fast, create("x")).await.unwrap_err(),
        client.on(Rail::Standard, create("x")).await.unwrap_err(),
    ];
    let scope = Scope::current().unwrap().child();
    provide_transport(&scope, Arc::new(client.in_process()));
    let in_process = within(&scope, create("x")).await.unwrap_err();
    for e in refused.iter().chain([&in_process]) {
        assert_eq!(
            (e.kind(), e.code(), e.message()),
            (ErrorKind::Domain, "unauthenticated", "sign in to continue")
        );
        assert!(e.request_id().is_some());
    }

    // A client built against another schema falls back once and succeeds.
    let scope = Scope::current().unwrap().child();
    let d = descriptor(create_project_rpc);
    provide_transport(&scope, as_ada.transport_with_fingerprint(d.fingerprint ^ 1));
    let fallen_back = within(&scope, create("fallback")).await.unwrap();
    assert_eq!(fallen_back.title, "fallback");
    assert_eq!(client.fallbacks(), 1);

    // All four projects exist once each.
    let db = client.pipeline().provided::<Db>().unwrap();
    let mine = db
        .query::<Project>()
        .where_eq(Project::OWNER_ID, ada.id)
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(mine.len(), 4);
}

/// The sketch's `Project.owner_id` is an `Id<User>`.
const _: fn(&Project) -> Id<User> = |p| p.owner_id;

// HTTP contract checks on the sketch's app (`docs/05-implementation-plan.md`
// §3, HTTP 규약 and 보안): cursor pages, optimistic locking, a key still
// being processed, and no internal names in error messages.

#[model]
pub struct RenameProject {
    pub title: String,
    pub expected_version: i64,
}

async fn rename_project(
    db: Ctx<Db>,
    user: Ctx<CurrentUser>,
    id: Path<Id<Project>>,
    input: Valid<RenameProject>,
) -> OrbicResult<Project> {
    let input = input.into_inner();
    let changed = db
        .update::<Project>()
        .set(Project::TITLE, input.title)
        .set(Project::VERSION, input.expected_version + 1)
        .where_eq(Project::ID, *id)
        .where_eq(Project::OWNER_ID, user.id)
        .where_eq(Project::VERSION, input.expected_version)
        .execute()
        .await?;
    if changed == 0 {
        return Err(HyveError::domain(
            "version_conflict",
            "the project changed since it was read",
        ));
    }
    db.query::<Project>()
        .where_eq(Project::ID, *id)
        .fetch_one()
        .await
}

async fn page_projects(
    db: Ctx<Db>,
    user: Ctx<CurrentUser>,
    cursor: Query<CursorParams>,
) -> OrbicResult<Page<Project>> {
    db.query::<Project>()
        .where_eq(Project::OWNER_ID, user.id)
        .cursor_after(cursor.after.clone(), cursor.limit.unwrap_or(2))
        .fetch_page()
        .await
}

static RELEASE: tokio::sync::Notify = tokio::sync::Notify::const_new();

async fn held(input: Valid<CreateProjectInput>) -> OrbicResult<String> {
    RELEASE.notified().await;
    Ok(input.into_inner().title)
}

fn contract_app(db: Db) -> OrbicApp {
    app(db, Cache::in_memory()).mount(
        "/x",
        component()
            .wrap(AuthGuard::require_user())
            .get("/page", page_projects)
            .patch("/projects/{id}", rename_project)
            .post("/held", held),
    )
}

#[hyve::test]
async fn cursor_pages_walk_every_item_once() {
    let client = TestClient::new(contract_app(Db::in_memory().await)).as_user(test_user());
    let mut created = Vec::new();
    for i in 0..5 {
        let p: Project = client
            .post("/v1/projects")
            .json(&input(&format!("p{i}")))
            .send()
            .await
            .json();
        created.push(p.id);
    }
    let (mut seen, mut after, mut pages) = (Vec::new(), None::<String>, 0);
    loop {
        let path = match &after {
            Some(c) => format!("/x/page?limit=2&after={c}"),
            None => "/x/page?limit=2".to_owned(),
        };
        let r = client.get(&path).send().await;
        assert!(r.text().starts_with(r#"{"data":["#), "{}", r.text());
        assert!(
            r.text().contains(r#","page":{"nextCursor":"#),
            "{}",
            r.text()
        );
        let page: Page<Project> = r.json();
        pages += 1;
        seen.extend(page.items.iter().map(|p| p.id));
        if !page.has_more {
            assert_eq!(page.next_cursor, None);
            break;
        }
        after = page.next_cursor;
    }
    created.sort_by_key(|id| id.to_string());
    let mut sorted = seen.clone();
    sorted.sort_by_key(|id| id.to_string());
    assert_eq!((pages, sorted), (3, created), "no item twice, none missing");
}

#[hyve::test]
async fn optimistic_locking_answers_version_conflict() {
    let client = TestClient::new(contract_app(Db::in_memory().await)).as_user(test_user());
    let p: Project = client
        .post("/v1/projects")
        .json(&input("a"))
        .send()
        .await
        .json();
    let rename = |expected: i64| {
        client
            .patch(&format!("/x/projects/{}", p.id))
            .json(&RenameProject {
                title: "b".into(),
                expected_version: expected,
            })
            .send()
    };
    let renamed: Project = rename(1).await.json();
    assert_eq!((renamed.title.as_str(), renamed.version), ("b", 2));
    let r = rename(1).await;
    assert_eq!(
        (r.status().as_u16(), r.error().code()),
        (409, "version_conflict")
    );
    assert_eq!(r.header("retry-after"), None);
}

#[hyve::test]
async fn a_key_still_being_processed_answers_409_with_retry_after() {
    let client = TestClient::new(contract_app(Db::in_memory().await)).as_user(test_user());
    let send = || {
        client
            .post("/x/held")
            .idempotency_key("k")
            .json(&input("t"))
            .send()
    };
    let second = async {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let r = send().await;
        RELEASE.notify_one();
        r
    };
    let (first, second) = tokio::join!(send(), second);
    assert_eq!(first.json::<String>(), "t");
    assert_eq!(
        (second.status().as_u16(), second.error().code()),
        (409, "request_in_progress")
    );
    assert_eq!(second.header("retry-after"), Some("1"));
    assert!(second.error().is_retryable());
    // Afterwards the key replays the first answer.
    assert_eq!(send().await.json::<String>(), "t");
}

#[hyve::test]
async fn error_messages_carry_no_internal_names() {
    let client = TestClient::new(contract_app(Db::in_memory().await)).as_user(test_user());
    let bad = [
        client
            .post("/v1/projects")
            .body("application/json", r#"{"title":5,"isPublic":false}"#)
            .send()
            .await,
        client
            .post("/v1/projects")
            .body("application/json", "not json")
            .send()
            .await,
        client.get("/x/page?limit=many").send().await,
        client
            .patch("/x/projects/not-an-id")
            .json(&RenameProject {
                title: "b".into(),
                expected_version: 1,
            })
            .send()
            .await,
    ];
    for r in bad {
        assert_eq!(r.status(), 400, "{}", r.text());
        for internal in ["::", "orbic", "hyve", "CreateProjectInput", "CursorParams"] {
            assert!(!r.text().contains(internal), "`{internal}` in {}", r.text());
        }
    }
}
