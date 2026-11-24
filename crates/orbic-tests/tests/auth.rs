//! `orbic-auth`: web sessions with CSRF, Bearer tokens with refresh rotation,
//! revocation, expiry (`docs/03-adapters-and-pipeline.md` §2, 인증·세션).
#![cfg(feature = "server")]

use std::time::Duration;

use hyve::api::Method;
use hyve::prelude::*;
use hyve::serde::{Json, from_json_data, to_json};
use orbic::__private::auth::{self, Client, SignedIn};
use orbic::{AuthConfig, Cache};
use orbic_cache::ManualClock;
use orbic_core::{
    Credential, CredentialUpdate, OrbicApp, Payload, Pipeline, Request, Response, component,
};
use orbic_tests::{AuthGuard, CurrentUser, User};

#[model]
#[derive(Debug, Clone)]
struct Login {
    name: String,
    bearer: bool,
}

#[model]
#[derive(Debug, Clone)]
struct RefreshInput {
    refresh_token: String,
}

fn ada() -> CurrentUser {
    CurrentUser {
        id: Id::<User>::new(),
        name: "ada".into(),
        admin: false,
    }
}

async fn login(input: Valid<Login>) -> HyveResult<SignedIn> {
    let input = input.into_inner();
    let user = CurrentUser {
        id: Id::new(),
        name: input.name,
        admin: false,
    };
    auth::sign_in(
        &user,
        if input.bearer {
            Client::Bearer
        } else {
            Client::Web
        },
    )
    .await
}

async fn refresh(input: Valid<RefreshInput>) -> HyveResult<SignedIn> {
    auth::refresh::<CurrentUser>(&input.into_inner().refresh_token).await
}

async fn session() -> HyveResult<SignedIn> {
    auth::current().await
}

async fn logout() -> HyveResult<()> {
    auth::sign_out().await
}

async fn me(user: Ctx<CurrentUser>) -> HyveResult<String> {
    Ok(user.name.clone())
}

async fn admin_only(user: Ctx<CurrentUser>) -> HyveResult<bool> {
    Ok(user.admin)
}

fn app(cache: Cache) -> Pipeline {
    OrbicApp::new()
        .provide(cache)
        .provide(AuthConfig {
            idle: Duration::from_secs(30 * 60),
            ..AuthConfig::default()
        })
        .route(Method::Post, "/login", login)
        .route(Method::Post, "/refresh", refresh)
        .mount(
            "/v1",
            component()
                .wrap(AuthGuard::require_user())
                .get("/me", me)
                .post("/me", me)
                .post("/logout", logout)
                .get("/session", session)
                .mount(
                    "/admin",
                    component()
                        .wrap(AuthGuard::require(|u: &CurrentUser| u.admin))
                        .get("/", admin_only),
                ),
        )
        .build()
        .unwrap()
}

fn json<T: Json>(r: &Response) -> T {
    match &r.outcome {
        Ok(Payload::Json(s)) => from_json_data(s).unwrap(),
        other => panic!("expected JSON, got {other:?}"),
    }
}

fn code(r: &Response) -> &str {
    r.outcome
        .as_ref()
        .err()
        .map(HyveError::code)
        .unwrap_or_else(|| panic!("expected an error, got {:?}", r.outcome))
}

fn with(method: Method, path: &str, credential: Option<Credential>) -> Request {
    let mut r = Request::new(method, path);
    r.meta.credential = credential;
    r
}

async fn sign_in(app: &Pipeline, bearer: bool) -> (Response, SignedIn) {
    let r = app
        .dispatch(Request::new(Method::Post, "/login").json(to_json(&Login {
            name: "ada".into(),
            bearer,
        })))
        .await;
    let s = json::<SignedIn>(&r);
    (r, s)
}

#[hyve::test]
async fn web_sessions_use_a_cookie_and_csrf() {
    let app = app(Cache::in_memory());
    let (r, signed) = sign_in(&app, false).await;
    let Some(CredentialUpdate::SetSession { token, csrf }) = r.meta.credential_update.clone()
    else {
        panic!("the adapter must set the cookie: {:?}", r.meta)
    };
    assert_eq!(signed.csrf_token.as_deref(), Some(csrf.as_str()));
    assert!(
        signed.access_token.is_none(),
        "the session token never reaches the body"
    );
    assert_eq!((token.len(), csrf.len()), (43, 43));
    assert_ne!(token, csrf);

    let session = |csrf: Option<&str>| {
        Some(Credential::Session {
            token: token.clone(),
            csrf: csrf.map(Into::into),
        })
    };
    assert_eq!(
        json::<String>(
            &app.dispatch(with(Method::Get, "/v1/me", session(None)))
                .await
        ),
        "ada"
    );
    // A reloaded page recovers its CSRF token; the session token is not echoed.
    let looked_up: SignedIn = json(
        &app.dispatch(with(Method::Get, "/v1/session", session(None)))
            .await,
    );
    assert_eq!(looked_up.csrf_token.as_deref(), Some(csrf.as_str()));
    assert!(looked_up.access_token.is_none() && looked_up.refresh_token.is_none());
    assert_eq!(
        code(
            &app.dispatch(with(Method::Post, "/v1/me", session(None)))
                .await
        ),
        "csrf_token_invalid"
    );
    assert_eq!(
        code(
            &app.dispatch(with(Method::Post, "/v1/me", session(Some("wrong"))))
                .await
        ),
        "csrf_token_invalid"
    );
    assert!(
        app.dispatch(with(Method::Post, "/v1/me", session(Some(&csrf))))
            .await
            .outcome
            .is_ok()
    );
    assert_eq!(
        code(&app.dispatch(with(Method::Get, "/v1/me", None)).await),
        "unauthenticated"
    );
    // The CSRF token is not a session token.
    let fake = Some(Credential::Session {
        token: csrf.clone(),
        csrf: None,
    });
    assert_eq!(
        code(&app.dispatch(with(Method::Get, "/v1/me", fake)).await),
        "unauthenticated"
    );

    let r = app
        .dispatch(with(Method::Post, "/v1/logout", session(Some(&csrf))))
        .await;
    assert_eq!(
        r.meta.credential_update,
        Some(CredentialUpdate::ClearSession)
    );
    assert_eq!(
        code(
            &app.dispatch(with(Method::Get, "/v1/me", session(None)))
                .await
        ),
        "unauthenticated",
        "revoked"
    );
}

#[hyve::test]
async fn bearer_tokens_rotate_and_reuse_revokes_the_family() {
    let app = app(Cache::in_memory());
    let (r, first) = sign_in(&app, true).await;
    assert!(
        r.meta.credential_update.is_none(),
        "no cookie for Bearer clients"
    );
    let bearer = |t: &Option<String>| Some(Credential::Bearer(t.clone().unwrap()));
    assert_eq!(
        json::<String>(
            &app.dispatch(with(Method::Post, "/v1/me", bearer(&first.access_token)))
                .await
        ),
        "ada"
    );

    let refresh = |t: &Option<String>| {
        app.dispatch(
            Request::new(Method::Post, "/refresh").json(to_json(&RefreshInput {
                refresh_token: t.clone().unwrap(),
            })),
        )
    };
    let second: SignedIn = json(&refresh(&first.refresh_token).await);
    assert_ne!(second.access_token, first.access_token);
    assert!(
        app.dispatch(with(Method::Get, "/v1/me", bearer(&second.access_token)))
            .await
            .outcome
            .is_ok()
    );

    // The first refresh token again: someone has a copy. Everything goes.
    assert_eq!(
        code(&refresh(&first.refresh_token).await),
        "unauthenticated"
    );
    assert_eq!(
        code(
            &app.dispatch(with(Method::Get, "/v1/me", bearer(&second.access_token)))
                .await
        ),
        "unauthenticated"
    );
    assert_eq!(
        code(&refresh(&second.refresh_token).await),
        "unauthenticated"
    );
}

#[hyve::test]
async fn a_revoked_session_cannot_be_revived_by_its_refresh_token() {
    let app = app(Cache::in_memory());
    let (_, s) = sign_in(&app, true).await;
    let r = app
        .dispatch(with(
            Method::Post,
            "/v1/logout",
            Some(Credential::Bearer(s.access_token.clone().unwrap())),
        ))
        .await;
    assert!(r.outcome.is_ok());
    let again = app
        .dispatch(
            Request::new(Method::Post, "/refresh").json(to_json(&RefreshInput {
                refresh_token: s.refresh_token.unwrap(),
            })),
        )
        .await;
    assert_eq!(code(&again), "unauthenticated");
}

#[hyve::test]
async fn sessions_slide_and_expire() {
    let time = ManualClock::new();
    let cache = Cache::in_memory_with(100, time.clock());
    let app = app(cache.clone());
    let (_, s) = sign_in(&app, true).await;
    let me = || {
        app.dispatch(with(
            Method::Get,
            "/v1/me",
            Some(Credential::Bearer(s.access_token.clone().unwrap())),
        ))
    };
    for _ in 0..3 {
        time.advance(Duration::from_secs(20 * 60));
        assert!(
            me().await.outcome.is_ok(),
            "each use extends the idle timeout"
        );
    }
    time.advance(Duration::from_secs(31 * 60));
    assert_eq!(code(&me().await), "unauthenticated", "idle too long");

    // The absolute lifetime ends a session however it is used.
    let cfg = AuthConfig {
        absolute: Duration::ZERO,
        ..AuthConfig::default()
    };
    let issued = auth::issue(&cache, &cfg, &ada(), Client::Bearer).unwrap();
    assert_eq!(
        code(
            &app.dispatch(with(
                Method::Get,
                "/v1/me",
                Some(Credential::Bearer(issued.session))
            ))
            .await
        ),
        "unauthenticated"
    );
}

#[hyve::test]
async fn rules_and_stored_values() {
    let cache = Cache::in_memory();
    let app = app(cache.clone());
    let cfg = AuthConfig::default();
    let user = auth::issue(&cache, &cfg, &ada(), Client::Bearer).unwrap();
    let admin = auth::issue(
        &cache,
        &cfg,
        &CurrentUser {
            admin: true,
            ..ada()
        },
        Client::Bearer,
    )
    .unwrap();
    let get = |t: &str| {
        app.dispatch(with(
            Method::Get,
            "/v1/admin",
            Some(Credential::Bearer(t.to_owned())),
        ))
    };
    assert_eq!(code(&get(&user.session).await), "forbidden");
    assert!(json::<bool>(&get(&admin.session).await));
    // Only hashes are stored.
    assert!(
        cache.sessions().get(&user.session).is_none()
            && cache
                .sessions()
                .get(&format!("s:{}", user.session))
                .is_none()
    );
}

#[test]
fn assembly_checks_the_guard() {
    let e = OrbicApp::new()
        .mount(
            "/v1",
            component().wrap(AuthGuard::require_user()).get("/me", me),
        )
        .build()
        .err()
        .unwrap();
    assert!(
        e.to_string()
            .contains("guard `AuthGuard` wrapped at `/v1` takes `orbic_cache::cache::Cache`"),
        "{e}"
    );
    let e = OrbicApp::new()
        .provide(Cache::in_memory())
        .get_me()
        .build()
        .err()
        .unwrap();
    assert!(
        e.to_string()
            .contains("takes `Ctx<orbic_tests::CurrentUser>`"),
        "{e}"
    );
}

trait GetMe {
    fn get_me(self) -> Self;
}

impl GetMe for OrbicApp {
    fn get_me(self) -> Self {
        self.route(Method::Get, "/me", me)
    }
}
