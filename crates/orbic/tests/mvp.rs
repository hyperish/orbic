//! Phase 1 MVP integration tests for `orbic`.

use hyve::prelude::*;
use orbic::prelude::*;
use std::time::Duration;

/// Project domain model directly bound to the `projects` SQL table.
#[model]
#[table("projects")]
pub struct Project {
    /// Primary key.
    #[pk]
    pub id: Id<Project>,
    /// Owner user identifier.
    pub owner_id: Id<User>,
    /// Validated project title.
    #[validate(len = 1..=80)]
    pub title: String,
    /// Public visibility flag.
    pub is_public: bool,
    /// Optimistic locking version.
    pub version: i64,
}

/// Validated input model for creating a project.
#[model]
pub struct CreateProjectInput {
    /// Project title (1..=80 chars).
    #[validate(len = 1..=80)]
    pub title: String,
    /// Public visibility flag.
    pub is_public: bool,
}

async fn project_live_room(_hub: RealtimeHub) -> OrbicResult<()> {
    Ok(())
}

async fn cleanup_orphan_uploads() -> OrbicResult<()> {
    Ok(())
}

/// Domain component assembling auth, rate limit, routes, realtime room, and periodic job.
pub fn project_component() -> BackendComponent {
    component()
        .wrap(AuthGuard::require_user())
        .wrap(RateLimit::per_minute(60))
        .post("/projects", create_project)
        .get("/projects", list_projects)
        .room("/projects/{id}/live", project_live_room)
        .every("1h", cleanup_orphan_uploads)
}

async fn list_projects(
    db: Ctx<Db>,
    cache: Ctx<Cache>,
    user: Ctx<CurrentUser>,
    cursor: Query<CursorParams>,
) -> OrbicResult<Page<Project>> {
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
            sql!(
                Project,
                "INSERT INTO projects (id, owner_id, title, is_public, version)
                 VALUES ($1, $2, $3, $4, 1) RETURNING *",
                Id::new(),
                user.id,
                data.title,
                data.is_public
            )
            .fetch_one(tx)
            .await
        })
        .await?;

    cache.invalidate_tag(tag!("projects", user.id)).await?;
    Ok(created)
}

/// Inline `#[server]` RPC handler explicitly mounted onto a `BackendComponent`.
#[server(POST "/rpc-projects")]
pub async fn create_project_rpc(
    #[server_ctx] db: Ctx<Db>,
    #[server_ctx] user: Ctx<CurrentUser>,
    input: Valid<CreateProjectInput>,
) -> HyveResult<Project> {
    db.insert(Project {
        id: Id::new(),
        owner_id: user.id,
        title: input.into_inner().title,
        is_public: false,
        version: 1,
    })
    .await
}

/// Assembles the root [`OrbicApp`] matching `orbic/docs/01-dx-and-dsl.md`.
pub fn app(db: Db, cache: Cache) -> OrbicApp {
    let rpc_component = component()
        .wrap(AuthGuard::require_user())
        .handle(create_project_rpc);

    OrbicApp::new()
        .provide(db)
        .provide(cache)
        .openapi("/docs")
        .mount("/v1", project_component())
        .mount("/v1", rpc_component)
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
    assert!(res.text().contains("invalid_request"));
}

#[hyve::test]
async fn create_list_cache_invalidation_and_idempotency_work() {
    let db = Db::in_memory().await;
    let cache = Cache::in_memory();
    let application = app(db.clone(), cache.clone());
    assert_eq!(application.scheduled_jobs().len(), 1);
    let client = TestClient::new(application);

    // 1) Initial list populates Singleflight cache
    let first_list = client.get("/v1/projects").as_user(test_user()).send().await;
    assert_eq!(first_list.status(), 200);
    assert_eq!(first_list.header("RateLimit-Limit"), Some("60"));
    let p1: Page<Project> = first_list.json().unwrap();
    assert!(p1.data.is_empty());
    assert_eq!(cache.misses(), 1);

    // 2) Second list hits cache
    let cached_list = client.get("/v1/projects").as_user(test_user()).send().await;
    assert_eq!(cached_list.status(), 200);
    assert_eq!(cache.hits(), 1);

    // 3) Create with Idempotency-Key succeeds and invalidates cache tag
    let payload = CreateProjectInput {
        title: "Atrium Renderer".into(),
        is_public: true,
    };
    let created = client
        .post("/v1/projects")
        .as_user(test_user())
        .idempotency_key("idem-001")
        .json(&payload)
        .send()
        .await;
    assert_eq!(created.status(), 200);
    let proj: Project = created.json().unwrap();
    assert_eq!(proj.title, "Atrium Renderer");

    // Same key + same payload replays identical project ID without inserting twice
    let replay = client
        .post("/v1/projects")
        .as_user(test_user())
        .idempotency_key("idem-001")
        .json(&payload)
        .send()
        .await;
    assert_eq!(replay.status(), 200);
    let replayed: Project = replay.json().unwrap();
    assert_eq!(replayed.id, proj.id);

    // Same key + mismatched payload returns 409 idempotency_key_mismatch
    let mismatch = client
        .post("/v1/projects")
        .as_user(test_user())
        .idempotency_key("idem-001")
        .json(&CreateProjectInput {
            title: "Different Title".into(),
            is_public: false,
        })
        .send()
        .await;
    assert_eq!(mismatch.status(), 409);
    assert!(mismatch.text().contains("idempotency_key_mismatch"));

    // 4) List reflects newly created project because tag was invalidated
    let after_create = client.get("/v1/projects").as_user(test_user()).send().await;
    let p2: Page<Project> = after_create.json().unwrap();
    assert_eq!(p2.data.len(), 1);
    assert_eq!(p2.data[0].id, proj.id);

    // 5) OpenAPI /docs and /health checks
    let docs = client.get("/docs").send().await;
    assert_eq!(docs.status(), 200);
    assert!(docs.text().contains("/v1/projects"));
    assert!(docs.text().contains("CreateProjectInput"));

    let health = client.get("/health").send().await;
    assert_eq!(health.status(), 200);
    assert!(health.text().contains("\"status\":\"ok\""));
}

#[hyve::test]
async fn l2_opaque_session_instant_revocation_and_csrf_work() {
    let db = Db::in_memory().await;
    let cache = Cache::in_memory();
    let sessions = SessionStore::new(cache.clone(), Duration::from_secs(300));
    let app_instance = OrbicApp::new()
        .provide(db)
        .provide(cache)
        .provide(sessions.clone())
        .mount("/v1", project_component());
    let client = TestClient::new(app_instance);

    let tokens = sessions.create_session(&test_user());

    // Native Bearer token succeeds
    let bearer_ok = client
        .get("/v1/projects")
        .bearer(&tokens.token)
        .send()
        .await;
    assert_eq!(bearer_ok.status(), 200);

    // Web Cookie mutation without CSRF token fails with 403
    let csrf_fail = client
        .post("/v1/projects")
        .cookie(&tokens.token)
        .json(&CreateProjectInput {
            title: "Web Project".into(),
            is_public: false,
        })
        .send()
        .await;
    assert_eq!(csrf_fail.status(), 403);

    // Web Cookie mutation with matching X-CSRF-Token succeeds
    let csrf_ok = client
        .post("/v1/projects")
        .cookie(&tokens.token)
        .csrf(&tokens.csrf_token)
        .json(&CreateProjectInput {
            title: "Web Project".into(),
            is_public: false,
        })
        .send()
        .await;
    assert_eq!(csrf_ok.status(), 200);

    // Immediate revocation blocks next request immediately (L2 direct)
    sessions.revoke_session(&tokens.token);
    let revoked = client
        .get("/v1/projects")
        .bearer(&tokens.token)
        .send()
        .await;
    assert_eq!(revoked.status(), 401);

    // Refresh rotation reuse detection revokes all active sessions for the user
    let pair1 = sessions.create_session(&test_user());
    let pair2 = sessions.rotate_refresh_token(&pair1.refresh_token).unwrap();
    assert!(sessions.verify_bearer(&pair2.token).is_ok());
    assert!(sessions.rotate_refresh_token(&pair1.refresh_token).is_err());
    assert!(sessions.verify_bearer(&pair2.token).is_err());
}

#[hyve::test]
async fn inline_server_fn_dual_rail_db_optimistic_lock_and_realtime_work() {
    let db = Db::in_memory().await;
    let cache = Cache::in_memory();
    let client = TestClient::new(app(db.clone(), cache))
        .with_rpc_prefix("/v1")
        .with_default_user(test_user());

    // 1) Direct async #[server] invocation on client over FastBinary Dual-Rail
    hyve::api::set_thread_transport(Some(client.clone().into_transport()));
    let created = create_project_rpc(CreateProjectInput {
        title: "Dual-Rail RPC".into(),
        is_public: false,
    })
    .await
    .unwrap();
    hyve::api::set_thread_transport(None);
    assert_eq!(created.title, "Dual-Rail RPC");

    // 2) Optimistic locking conflict detection
    let updated = db
        .update_optimistic(&created.id, 1, |p| p.title = "Updated".into())
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    let stale_err = db
        .update_optimistic(&created.id, 1, |p| p.title = "Stale".into())
        .await
        .unwrap_err();
    assert_eq!(stale_err.code(), "version_conflict");
    assert_eq!(stale_err.http_status(), 409);

    // 3) Cancelled transaction auto-rollback
    let token = CancellationToken::new();
    let cancel_token = token.clone();
    let cancel_err = db
        .tx_with_cancel(token, |tx| async move {
            let p = sql!(
                Project,
                "INSERT INTO projects (id, owner_id, title, is_public, version) VALUES ($1, $2, $3, $4, 1) RETURNING *",
                Id::new(),
                test_user().id,
                String::from("Rolled Back"),
                false
            )
            .fetch_one(tx)
            .await?;
            cancel_token.cancel();
            Ok(p)
        })
        .await
        .unwrap_err();
    assert_eq!(cancel_err.code(), "cancelled");
    let page = db.query::<Project>().fetch_page().await.unwrap();
    assert_eq!(page.data.len(), 1); // Only the RPC-created project remains

    // 4) RealtimeHub one-time ?ticket=, dedup across fallback, and seq replay
    let hub = RealtimeHub::new();
    let ticket1 = hub.issue_ticket(test_user().id);
    assert!(
        hub.connect_with_ticket("conn_1", &ticket1, RealtimeTransport::WebTransport)
            .is_ok()
    );
    // Reusing consumed ticket fails with 401
    assert_eq!(
        hub.connect_with_ticket("conn_1b", &ticket1, RealtimeTransport::WebSocketFallback)
            .unwrap_err()
            .http_status(),
        401
    );

    let ev1 = hub
        .publish_client_frame(
            ClientFrame {
                msg_type: "project.rename".into(),
                id: "cli_msg_1".into(),
                payload: "alpha".into(),
            },
            "project.renamed",
        )
        .unwrap()
        .unwrap();
    assert_eq!(ev1.seq, 1);

    // Client retries same id across WebSocketFallback -> deduplicated cleanly
    let dup = hub
        .publish_client_frame(
            ClientFrame {
                msg_type: "project.rename".into(),
                id: "cli_msg_1".into(),
                payload: "alpha".into(),
            },
            "project.renamed",
        )
        .unwrap();
    assert!(dup.is_none());

    let ev2 = hub
        .publish_client_frame(
            ClientFrame {
                msg_type: "project.rename".into(),
                id: "cli_msg_2".into(),
                payload: "beta".into(),
            },
            "project.renamed",
        )
        .unwrap()
        .unwrap();
    assert_eq!(ev2.seq, 2);

    // Reconnect from seq=1 over WebSocketFallback replays only seq=2
    let ticket2 = hub.issue_ticket(test_user().id);
    let missed = hub
        .reconnect_from_seq("conn_1", &ticket2, RealtimeTransport::WebSocketFallback, 1)
        .unwrap();
    assert_eq!(missed.len(), 1);
    assert_eq!(missed[0].seq, 2);
}
