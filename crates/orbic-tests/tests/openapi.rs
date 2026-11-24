//! `orbic-openapi`: the generated document matches the real answers —
//! envelopes, errors, authentication (`docs/05-implementation-plan.md` §3,
//! OpenAPI). Real HTTP responses are checked against the documented schema
//! of their status with a small JSON Schema validator.
#![cfg(feature = "server")]

use std::net::SocketAddr;

use bytes::Bytes;
use hyve::api::Method;
use hyve::prelude::*;
use hyve::serde::{from_json, to_json};
use orbic::__private::auth::{self, Client, SignedIn};
use orbic::{Cache, RateLimit};
use orbic_adapter_http::{HttpApp, HttpConfig};
use orbic_core::{OrbicApp, component};
use orbic_openapi::{OpenApiExt, OpenApiInfo};
use orbic_tests::{
    AuthGuard, CurrentUser, Note, Store, add_note, delete_note, find_notes, get_note, rename_note,
};

#[model]
#[derive(Debug, Clone)]
struct Login {
    name: String,
}

async fn login(input: Valid<Login>) -> HyveResult<SignedIn> {
    let user = CurrentUser {
        id: Id::new(),
        name: input.into_inner().name,
        admin: false,
    };
    auth::sign_in(&user, Client::Web).await
}

async fn me(user: Ctx<CurrentUser>) -> HyveResult<String> {
    Ok(user.name.clone())
}

async fn ping() -> HyveResult<String> {
    Ok("pong".into())
}

fn app() -> HttpApp {
    let pipeline = OrbicApp::new()
        .provide(Cache::in_memory())
        .provide(Store {
            prefix: "> ".into(),
        })
        .provide(HttpConfig::default().max_body(64))
        .provide(OpenApiInfo {
            title: "Notes".into(),
            version: "1.2.3".into(),
        })
        .openapi("/docs")
        .route(Method::Post, "/login", login)
        .mount(
            "/v1",
            component()
                .handle(add_note)
                .handle(get_note)
                .handle(find_notes)
                .handle(rename_note)
                .handle(delete_note),
        )
        .mount(
            "/limited",
            component()
                .wrap(RateLimit::per_minute(2))
                .get("/ping", ping),
        )
        .mount(
            "/me",
            component()
                .wrap(AuthGuard::require_user())
                .get("/", me)
                .post("/", me),
        )
        .build()
        .unwrap();
    orbic_openapi::attach(HttpApp::new(pipeline).unwrap().allow_plaintext(true)).unwrap()
}

fn peer() -> SocketAddr {
    "127.0.0.1:40000".parse().unwrap()
}

fn req(method: &str, path: &str) -> http::request::Builder {
    http::Request::builder().method(method).uri(path)
}

fn json_req(method: &str, path: &str, body: &str) -> http::Request<Bytes> {
    req(method, path)
        .header("content-type", "application/json")
        .body(Bytes::from(body.to_owned()))
        .unwrap()
}

fn get(path: &str) -> http::Request<Bytes> {
    req("GET", path).body(Bytes::new()).unwrap()
}

fn at<'v>(v: &'v Value, path: &[&str]) -> &'v Value {
    path.iter().fold(v, |v, k| {
        v.get(k)
            .unwrap_or_else(|| panic!("missing `{k}` in {}", to_json(v)))
    })
}

fn s(v: &Value) -> &str {
    match v {
        Value::String(s) => s,
        other => panic!("not a string: {other:?}"),
    }
}

/// The JSON Schema subset the generator emits.
fn check(doc: &Value, schema: &Value, v: &Value, at_: &str) -> Result<(), String> {
    if let Some(r) = schema.get("$ref") {
        let name = s(r).trim_start_matches("#/components/schemas/");
        return check(doc, at(doc, &["components", "schemas", name]), v, at_);
    }
    if let Some(Value::List(alts)) = schema.get("anyOf") {
        return if alts.iter().any(|a| check(doc, a, v, at_).is_ok()) {
            Ok(())
        } else {
            Err(format!("{at_}: {v:?} matches no `anyOf` branch"))
        };
    }
    if let Some(t) = schema.get("type") {
        let types: Vec<&str> = match t {
            Value::List(l) => l.iter().map(s).collect(),
            t => vec![s(t)],
        };
        let ok = types.iter().any(|t| {
            matches!(
                (*t, v),
                ("null", Value::Null)
                    | ("boolean", Value::Bool(_))
                    | ("integer", Value::Int(_))
                    | ("number", Value::Int(_) | Value::Float(_))
                    | ("string", Value::String(_))
                    | ("array", Value::List(_))
                    | ("object", Value::Map(_))
            )
        });
        if !ok {
            return Err(format!("{at_}: {v:?} is not {types:?}"));
        }
    }
    if let Some(Value::List(allowed)) = schema.get("enum")
        && !allowed.contains(v)
    {
        return Err(format!("{at_}: {v:?} not in enum"));
    }
    if let (Value::String(text), Some(Value::Int(min))) = (v, schema.get("minLength"))
        && (text.chars().count() as i64) < *min
    {
        return Err(format!("{at_}: shorter than {min}"));
    }
    if let Value::List(items) = v
        && let Some(item) = schema.get("items")
    {
        for (i, x) in items.iter().enumerate() {
            check(doc, item, x, &format!("{at_}[{i}]"))?;
        }
    }
    if let Value::Map(fields) = v {
        if let Some(Value::List(required)) = schema.get("required") {
            for r in required {
                if !fields.iter().any(|(k, _)| k == s(r)) {
                    return Err(format!("{at_}: missing required `{}`", s(r)));
                }
            }
        }
        let props = schema.get("properties");
        for (k, x) in fields {
            match props.and_then(|p| p.get(k)) {
                Some(p) => check(doc, p, x, &format!("{at_}.{k}"))?,
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(true)) => {}
                    Some(extra @ Value::Map(_)) => check(doc, extra, x, &format!("{at_}.{k}"))?,
                    _ if props.is_some() => return Err(format!("{at_}: undocumented `{k}`")),
                    _ => {}
                },
            }
        }
    }
    Ok(())
}

/// Checks a real answer against the document: its status (or `default`) is
/// documented, the body fits that schema, documented headers are sent.
fn conforms(doc: &Value, template: &str, method: &str, r: &http::Response<Bytes>) {
    let op = at(doc, &["paths", template, method]);
    let status = r.status().as_str().to_owned();
    let responses = at(op, &["responses"]);
    let documented = responses
        .get(&status)
        .or_else(|| {
            assert!(
                r.status().is_client_error(),
                "{method} {template}: undocumented {status}"
            );
            responses.get("default")
        })
        .unwrap_or_else(|| panic!("{method} {template}: no response for {status}"));
    match documented.get("content") {
        None => assert!(r.body().is_empty(), "{method} {template} {status}: body"),
        Some(content) => {
            let schema = at(content, &["application/json", "schema"]);
            let body: Value = from_json(std::str::from_utf8(r.body()).unwrap()).unwrap();
            check(doc, schema, &body, "$")
                .unwrap_or_else(|e| panic!("{method} {template} {status}: {e}"));
        }
    }
    if let Some(Value::Map(headers)) = documented.get("headers") {
        for (name, _) in headers {
            assert!(
                r.headers().contains_key(name.as_str()),
                "{method} {template} {status}: documented header {name} missing"
            );
        }
    }
}

#[hyve::test]
async fn the_document_describes_the_wire() {
    let app = app();
    let r = app.handle(get("/docs"), peer()).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("content-type").unwrap(), "application/json");
    let doc: Value = from_json(std::str::from_utf8(r.body()).unwrap()).unwrap();
    if std::env::var_os("ORBIC_DUMP_OPENAPI").is_some() {
        println!("{}", std::str::from_utf8(r.body()).unwrap());
    }
    assert_eq!(s(at(&doc, &["openapi"])), "3.1.0");
    assert_eq!(s(at(&doc, &["info", "title"])), "Notes");
    assert_eq!(s(at(&doc, &["info", "version"])), "1.2.3");

    // Parameters: path placeholders and the query fields of a GET-bound
    // `#[server]` function.
    let params = |template: &str, method: &str| -> Vec<(String, String, bool)> {
        match at(&doc, &["paths", template, method]).get("parameters") {
            Some(Value::List(l)) => l
                .iter()
                .map(|p| {
                    (
                        s(at(p, &["name"])).to_owned(),
                        s(at(p, &["in"])).to_owned(),
                        at(p, &["required"]) == &Value::Bool(true),
                    )
                })
                .collect(),
            _ => vec![],
        }
    };
    assert_eq!(
        params("/v1/notes/{id}", "get"),
        [("id".to_owned(), "path".to_owned(), true)]
    );
    let find = params("/v1/notes", "get");
    assert!(
        find.contains(&("pinned".into(), "query".into(), false)),
        "{find:?}"
    );
    assert!(
        find.contains(&("limit".into(), "query".into(), false)),
        "{find:?}"
    );
    assert!(params("/v1/notes", "post").contains(&(
        "Idempotency-Key".into(),
        "header".into(),
        false
    )));
    assert!(params("/me", "post").contains(&("X-CSRF-Token".into(), "header".into(), false)));

    // Bodies and constraints come from the models.
    assert_eq!(
        s(at(
            &doc,
            &[
                "paths",
                "/v1/notes",
                "post",
                "requestBody",
                "content",
                "application/json",
                "schema",
                "$ref"
            ]
        )),
        "#/components/schemas/Note"
    );
    let text = at(
        &doc,
        &["components", "schemas", "Note", "properties", "text"],
    );
    assert_eq!(
        (at(text, &["minLength"]), at(text, &["maxLength"])),
        (&Value::Int(1), &Value::Int(20))
    );

    // Authentication: the guarded routes and only those.
    let security = |template: &str, method: &str| {
        at(&doc, &["paths", template, method])
            .get("security")
            .is_some()
    };
    assert!(security("/me", "get") && security("/me", "post"));
    assert!(!security("/v1/notes", "post") && !security("/limited/ping", "get"));
    let has = |template: &str, method: &str, status: &str| {
        at(&doc, &["paths", template, method, "responses"])
            .get(status)
            .is_some()
    };
    assert!(has("/me", "get", "401") && has("/me", "post", "403"));
    assert!(!has("/v1/notes", "post", "401"));
    assert!(has("/limited/ping", "get", "429") && !has("/v1/notes", "post", "429"));
    assert!(has("/v1/notes/{id}", "delete", "204"));
    assert_eq!(
        s(at(
            &doc,
            &["components", "securitySchemes", "session", "name"]
        )),
        "hyperish_session"
    );

    // The validator is not vacuous.
    let note = at(&doc, &["components", "schemas", "Note"]);
    let bad: Value = from_json(r#"{"text":5,"pinned":false}"#).unwrap();
    assert!(check(&doc, note, &bad, "$").is_err());
    let missing: Value = from_json(r#"{"text":"a"}"#).unwrap();
    assert!(check(&doc, note, &missing, "$").is_err());
    let envelope = at(&doc, &["components", "schemas", "ErrorEnvelope"]);
    let no_id: Value = from_json(r#"{"error":{"code":"x","message":"m","details":{}}}"#).unwrap();
    assert!(
        check(&doc, envelope, &no_id, "$").is_err(),
        "requestId is required"
    );

    // Real answers fit what the document says.
    let id = Id::<Note>::new();
    let cases: Vec<(&str, &str, http::Request<Bytes>)> = vec![
        (
            "/v1/notes",
            "post",
            json_req("POST", "/v1/notes", r#"{"text":"hi","pinned":false}"#),
        ),
        (
            "/v1/notes",
            "post",
            json_req("POST", "/v1/notes", r#"{"text":"taken","pinned":false}"#),
        ),
        (
            "/v1/notes",
            "post",
            json_req("POST", "/v1/notes", r#"{"text":"","pinned":false}"#),
        ),
        (
            "/v1/notes",
            "post",
            req("POST", "/v1/notes")
                .header("content-type", "text/plain")
                .body(Bytes::from_static(b"x"))
                .unwrap(),
        ),
        (
            "/v1/notes",
            "post",
            json_req(
                "POST",
                "/v1/notes",
                &format!(r#"{{"text":"{}"}}"#, "x".repeat(80)),
            ),
        ),
        ("/v1/notes", "get", get("/v1/notes?limit=2&pinned=true")),
        ("/v1/notes/{id}", "get", get(&format!("/v1/notes/{id}"))),
        (
            "/v1/notes/{id}",
            "patch",
            json_req(
                "PATCH",
                &format!("/v1/notes/{id}"),
                &format!(r#"{{"id":"{id}","text":"new"}}"#),
            ),
        ),
        (
            "/v1/notes/{id}",
            "delete",
            req("DELETE", &format!("/v1/notes/{id}"))
                .body(Bytes::new())
                .unwrap(),
        ),
        ("/me", "get", get("/me")),
        ("/limited/ping", "get", get("/limited/ping")),
        ("/limited/ping", "get", get("/limited/ping")),
        ("/limited/ping", "get", get("/limited/ping")),
    ];
    let mut statuses = Vec::new();
    for (template, method, request) in cases {
        let r = app.handle(request, peer()).await;
        statuses.push(r.status().as_u16());
        conforms(&doc, template, method, &r);
    }
    assert_eq!(
        statuses,
        [
            200, 422, 400, 415, 413, 200, 404, 200, 204, 401, 200, 200, 429
        ]
    );

    // A cookie session: sign-in body, the guarded answer, a CSRF refusal.
    let r = app
        .handle(json_req("POST", "/login", r#"{"name":"ada"}"#), peer())
        .await;
    conforms(&doc, "/login", "post", &r);
    let cookie = r.headers().get("set-cookie").unwrap().to_str().unwrap();
    let pair = cookie.split(';').next().unwrap().to_owned();
    let with = |method: &str| {
        req(method, "/me")
            .header("cookie", &pair)
            .body(Bytes::new())
            .unwrap()
    };
    let r = app.handle(with("GET"), peer()).await;
    assert_eq!(r.status(), 200);
    conforms(&doc, "/me", "get", &r);
    let r = app.handle(with("POST"), peer()).await;
    assert_eq!(r.status(), 403);
    conforms(&doc, "/me", "post", &r);
}

#[test]
fn without_openapi_nothing_is_served() {
    let pipeline = OrbicApp::new()
        .route(Method::Get, "/ping", ping)
        .build()
        .unwrap();
    let app = orbic_openapi::attach(HttpApp::new(pipeline).unwrap()).unwrap();
    let doc = orbic_openapi::document(app.pipeline(), app.session_cookie());
    assert!(
        doc.components.unwrap().security_schemes.is_empty(),
        "no guard authenticates"
    );
}
