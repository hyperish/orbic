//! `orbic-openapi` — the app's HTTP API as an OpenAPI 3.1 document.
//!
//! The document describes the wire as `orbic-adapter-http` produces it
//! (`docs/01-dx-and-dsl.md` §1 조립 계약, OpenAPI): the `{"data": ...}`
//! envelope and list pages, `204` for handlers without a value, the error
//! envelope for every status a route can answer with (framework, guard and
//! gate errors by code; the handler's own `Domain` errors as `default`),
//! credentials (cookie session or Bearer) where a guard authenticates, and
//! the `Idempotency-Key`, `X-CSRF-Token`, `Retry-After` and `RateLimit-*`
//! headers. Schemas come from `hyve-validate` with the JSON codec's rules:
//! 64-bit integers may travel as decimal strings, non-finite floats as
//! `"NaN"`/`"Infinity"`/`"-Infinity"`.
//!
//! Serving (decision 3 in `docs/04-open-questions.md`): `.openapi("/docs")`
//! on the app, then [`attach`] puts the JSON document at that path, outside
//! the pipeline.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hyve::api::Method;
use hyve::validate::{Constraint, Definition, FieldSchema, SchemaNode, SchemaRegistry};
use hyve::{HyveError, Value};
use orbic_adapter_http::{HttpApp, HttpError, status_of};
use orbic_core::{OrbicApp, ParamsFrom, Pipeline, RouteInfo, RouteShape};
use utoipa::openapi::path::{
    HttpMethod, Operation, OperationBuilder, Parameter, ParameterBuilder, ParameterIn,
};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::{
    AdditionalProperties, AnyOfBuilder, ArrayBuilder, KnownFormat, ObjectBuilder, Ref, Schema,
    SchemaFormat, SchemaType, Type,
};
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::openapi::{
    ComponentsBuilder, Content, Header, Info, OpenApi, OpenApiBuilder, Paths, RefOr, Required,
    ResponseBuilder, SecurityRequirement,
};

pub use utoipa::openapi;

/// The document's `info`; provide one at the app's root to set it
/// (default: `"API"`, `"0.0.0"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApiInfo {
    /// `info.title`.
    pub title: String,
    /// `info.version`.
    pub version: String,
}

impl Default for OpenApiInfo {
    fn default() -> Self {
        OpenApiInfo {
            title: "API".into(),
            version: "0.0.0".into(),
        }
    }
}

/// Where the document is served, set by [`OpenApiExt::openapi`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApiPath(pub String);

/// `.openapi("/docs")` on the app (`docs/01-dx-and-dsl.md` §1 sketch).
pub trait OpenApiExt {
    /// Serves the OpenAPI document at `GET path`.
    fn openapi(self, path: &str) -> Self;
}

impl OpenApiExt for OrbicApp {
    fn openapi(self, path: &str) -> Self {
        self.provide(OpenApiPath(path.to_owned()))
    }
}

/// Adds the document to `http` when the app asked for it with
/// [`OpenApiExt::openapi`].
pub fn attach(http: HttpApp) -> Result<HttpApp, HttpError> {
    let Some(path) = http.pipeline().provided::<OpenApiPath>() else {
        return Ok(http);
    };
    let json = document(http.pipeline(), http.session_cookie())
        .to_json()
        .expect("an OpenAPI document always serializes");
    let path = path.0.clone();
    http.document(&path, "application/json", json)
}

const ENVELOPE: &str = "ErrorEnvelope";
const ERROR: &str = "ErrorBody";
const PAGE: &str = "PageInfo";
const SESSION: &str = "session";
const BEARER: &str = "bearer";

/// The document for `pipeline`; `session_cookie` names the cookie scheme.
pub fn document(pipeline: &Pipeline, session_cookie: &str) -> OpenApi {
    let info = pipeline
        .provided::<OpenApiInfo>()
        .map(|i| (*i).clone())
        .unwrap_or_default();
    let mut registry = SchemaRegistry::new();
    let routes: Vec<(RouteInfo<'_>, RouteShape)> = pipeline
        .routes()
        .map(|r| {
            let shape = r.shape(&mut registry);
            (r, shape)
        })
        .collect();
    let names = Names::new(&registry);
    let mut paths = Paths::new();
    let mut ids = HashSet::new();
    let mut authenticated = false;
    for (route, shape) in &routes {
        authenticated |= route.guards.iter().any(|g| g.authenticates);
        let op = operation(route, shape, &registry, &names, &mut ids);
        paths.add_path_operation(template(route.pattern), vec![http_method(route.method)], op);
    }

    let mut components = ComponentsBuilder::new();
    for (name, def) in registry.definitions() {
        components = components.schema(name, definition(def));
    }
    components = components
        .schema(&names.error, error_schema())
        .schema(&names.envelope, envelope_schema(&names))
        .schema(&names.page, page_schema());
    if authenticated {
        components = components
            .security_scheme(
                SESSION,
                SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(session_cookie))),
            )
            .security_scheme(
                BEARER,
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
    }
    OpenApiBuilder::new()
        .info(Info::new(info.title, info.version))
        .paths(paths)
        .components(Some(components.build()))
        .build()
}

/// Component names for the fixed schemas, avoiding the app's own.
struct Names {
    error: String,
    envelope: String,
    page: String,
}

impl Names {
    fn new(registry: &SchemaRegistry) -> Names {
        let free = |base: &str| {
            let mut name = base.to_owned();
            while registry.get(&name).is_some() {
                name.insert_str(0, "Orbic");
            }
            name
        };
        Names {
            error: free(ERROR),
            envelope: free(ENVELOPE),
            page: free(PAGE),
        }
    }
}

fn http_method(m: Method) -> HttpMethod {
    match m {
        Method::Get => HttpMethod::Get,
        Method::Post => HttpMethod::Post,
        Method::Patch => HttpMethod::Patch,
        Method::Delete => HttpMethod::Delete,
    }
}

/// `matchit` patterns to OpenAPI templates (`{*rest}` → `{rest}`).
fn template(pattern: &str) -> String {
    pattern.replace("{*", "{")
}

fn placeholders(pattern: &str) -> Vec<String> {
    pattern
        .split('{')
        .skip(1)
        .filter_map(|s| s.split_once('}'))
        .map(|(name, _)| name.trim_start_matches('*').to_owned())
        .collect()
}

fn operation(
    route: &RouteInfo<'_>,
    shape: &RouteShape,
    registry: &SchemaRegistry,
    names: &Names,
    ids: &mut HashSet<String>,
) -> Operation {
    let mut id = route.name.replace("::", ".");
    if !ids.insert(id.clone()) {
        id = format!("{id}.{}", route.method.as_str().to_ascii_lowercase());
        let base = id.clone();
        let mut n = 2;
        while !ids.insert(id.clone()) {
            id = format!("{base}{n}");
            n += 1;
        }
    }
    let mut op = OperationBuilder::new().operation_id(Some(id));
    let holes = placeholders(route.pattern);
    let mut documented = BTreeSet::new();
    for p in &shape.params {
        let location = |name: &str| match p.from {
            ParamsFrom::Path => ParameterIn::Path,
            ParamsFrom::Query => ParameterIn::Query,
            ParamsFrom::PathOrQuery if holes.iter().any(|h| h == name) => ParameterIn::Path,
            ParamsFrom::PathOrQuery => ParameterIn::Query,
        };
        match object_fields(registry, &p.schema) {
            Some(fields) => {
                for f in fields {
                    let at = location(f.json_name);
                    let required = matches!(at, ParameterIn::Path) || f.required;
                    documented.insert(f.json_name.to_owned());
                    let value = schema(present(&f.schema), &f.constraints);
                    op = op.parameter(param(f.json_name, at, required, value));
                }
            }
            None => {
                let name = p
                    .name
                    .clone()
                    .or_else(|| (holes.len() == 1).then(|| holes[0].clone()))
                    .unwrap_or_else(|| "value".into());
                let at = location(&name);
                documented.insert(name.clone());
                op = op.parameter(param(&name, at, true, schema(present(&p.schema), &[])));
            }
        }
    }
    for hole in holes.iter().filter(|h| !documented.contains(*h)) {
        op = op.parameter(param(hole, ParameterIn::Path, true, string(None)));
    }

    let guarded = route.guards.iter().any(|g| g.authenticates);
    let changes_state = matches!(route.method, Method::Post | Method::Patch | Method::Delete);
    if route.method == Method::Post {
        op = op.parameter(header_param(
            "Idempotency-Key",
            "Repeating a key with the same body returns the first answer without repeating its effects (authenticated requests).",
        ));
    }
    if guarded && changes_state {
        op = op.parameter(header_param(
            "X-CSRF-Token",
            "Required with cookie sessions: the token the sign-in or session response returned.",
        ));
    }
    if let Some(body) = &shape.body {
        op = op.request_body(Some(
            RequestBodyBuilder::new()
                .content("application/json", Content::new(Some(schema(body, &[]))))
                .required(Some(Required::True))
                .build(),
        ));
    }

    // Errors this route can answer with, by status.
    let mut codes: BTreeSet<&'static str> = ["internal_error", "service_unavailable"].into();
    if !shape.params.is_empty() || !holes.is_empty() || shape.body.is_some() {
        codes.insert("invalid_request");
    }
    if shape.body.is_some() {
        codes.extend(["payload_too_large", "unsupported_media_type"]);
    }
    if route.method == Method::Post {
        codes.extend([
            "invalid_request",
            "idempotency_key_mismatch",
            "request_in_progress",
        ]);
    }
    for g in &route.guards {
        codes.extend(g.errors.iter().copied());
    }
    let rate_limited = codes.contains("rate_limited");
    let mut by_status: BTreeMap<u16, Vec<&'static str>> = BTreeMap::new();
    for code in &codes {
        let e = match *code {
            "internal_error" => HyveError::internal(""),
            "service_unavailable" => HyveError::infra("service_unavailable", ""),
            c => HyveError::domain(c, ""),
        };
        by_status
            .entry(status_of(&e).as_u16())
            .or_default()
            .push(code);
    }

    let mut success = if shape.response == SchemaNode::Null {
        ResponseBuilder::new().description("Success, no value.")
    } else {
        ResponseBuilder::new().description("Success.").content(
            "application/json",
            Content::new(Some(success_schema(&shape.response, registry, names))),
        )
    };
    if rate_limited {
        success = rate_limit_headers(success);
    }
    let ok = if shape.response == SchemaNode::Null {
        "204"
    } else {
        "200"
    };
    op = op.response(ok, success.build());
    for (status, codes) in by_status {
        let list = codes
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut r = ResponseBuilder::new().description(list).content(
            "application/json",
            Content::new(Some(Ref::from_schema_name(&names.envelope))),
        );
        if status == 429 || status == 503 || codes.contains(&"request_in_progress") {
            r = r.header(
                "Retry-After",
                described(
                    Header::new(integer(32, false)),
                    "Seconds to wait; present exactly when retrying may succeed.",
                ),
            );
        }
        if status == 429 {
            r = rate_limit_headers(r);
        }
        op = op.response(status.to_string(), r.build());
    }
    op = op.response(
        "default",
        ResponseBuilder::new()
            .description("A `Domain` error of the handler (`422` unless the protocol table assigns the code a status; `*_not_found` is `404`).")
            .content("application/json", Content::new(Some(Ref::from_schema_name(&names.envelope))))
            .build(),
    );
    if guarded {
        op = op
            .security(SecurityRequirement::new(SESSION, Vec::<String>::new()))
            .security(SecurityRequirement::new(BEARER, Vec::<String>::new()));
    }
    op.build()
}

fn described(mut h: Header, text: &str) -> Header {
    h.description = Some(text.to_owned());
    h
}

fn rate_limit_headers(r: ResponseBuilder) -> ResponseBuilder {
    r.header(
        "RateLimit-Limit",
        described(
            Header::new(integer(32, false)),
            "Requests allowed per window.",
        ),
    )
    .header(
        "RateLimit-Remaining",
        described(
            Header::new(integer(32, false)),
            "Requests left in this window.",
        ),
    )
    .header(
        "RateLimit-Reset",
        described(
            Header::new(integer(64, false)),
            "Seconds until the window resets.",
        ),
    )
}

fn param(name: &str, at: ParameterIn, required: bool, schema: RefOr<Schema>) -> Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(at)
        .required(if required {
            Required::True
        } else {
            Required::False
        })
        .schema(Some(schema))
        .build()
}

fn header_param(name: &str, description: &str) -> Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Header)
        .required(Required::False)
        .description(Some(description))
        .schema(Some(string(None)))
        .build()
}

fn object_fields<'r>(registry: &'r SchemaRegistry, node: &SchemaNode) -> Option<&'r [FieldSchema]> {
    match node {
        SchemaNode::Ref(name) => match registry.get(name) {
            Some(Definition::Object(fields)) => Some(fields),
            _ => None,
        },
        _ => None,
    }
}

/// `{"data": ...}`, or for `Page<T>` `{"data": [...], "page": {...}}`
/// (`Json::encode_data`).
fn success_schema(node: &SchemaNode, registry: &SchemaRegistry, names: &Names) -> RefOr<Schema> {
    if let SchemaNode::Ref(name) = node
        && name.starts_with("Page_")
        && let Some(fields) = object_fields(registry, node)
        && let [items, cursor, more] = fields
        && (items.json_name, cursor.json_name, more.json_name) == ("items", "nextCursor", "hasMore")
    {
        return ObjectBuilder::new()
            .property("data", schema(&items.schema, &[]))
            .required("data")
            .property("page", Ref::from_schema_name(&names.page))
            .required("page")
            .into();
    }
    ObjectBuilder::new()
        .property("data", schema(node, &[]))
        .required("data")
        .into()
}

fn page_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("nextCursor", nullable(string(None)))
        .required("nextCursor")
        .property("hasMore", ObjectBuilder::new().schema_type(Type::Boolean))
        .required("hasMore")
        .into()
}

fn error_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .description(Some("`hyperish/docs` `03-api-protocol.md` §4."))
        .property("code", string(None))
        .required("code")
        .property("message", string(None))
        .required("message")
        .property(
            "details",
            ObjectBuilder::new().additional_properties(Some(AdditionalProperties::FreeForm(true))),
        )
        .required("details")
        .property("requestId", string(None))
        .required("requestId")
        .into()
}

fn envelope_schema(names: &Names) -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("error", Ref::from_schema_name(&names.error))
        .required("error")
        .into()
}

fn definition(def: &Definition) -> RefOr<Schema> {
    match def {
        Definition::Object(fields) => {
            let mut o = ObjectBuilder::new();
            for f in fields {
                o = o.property(f.json_name, field_schema(f));
                if f.required {
                    o = o.required(f.json_name);
                }
            }
            o.into()
        }
        Definition::Enum(values) => ObjectBuilder::new()
            .schema_type(Type::String)
            .enum_values(Some(values.iter().copied()))
            .into(),
    }
}

/// A parameter's value when it is sent (an absent one is `None`; a
/// parameter is never `null`).
fn present(node: &SchemaNode) -> &SchemaNode {
    match node {
        SchemaNode::Nullable(inner) => inner,
        other => other,
    }
}

fn field_schema(f: &FieldSchema) -> RefOr<Schema> {
    schema(&f.schema, &f.constraints)
}

fn string(format: Option<&str>) -> RefOr<Schema> {
    let format = format.map(|f| match f {
        "date-time" => SchemaFormat::KnownFormat(KnownFormat::DateTime),
        other => SchemaFormat::Custom(other.to_owned()),
    });
    ObjectBuilder::new()
        .schema_type(Type::String)
        .format(format)
        .into()
}

fn integer(bits: u8, signed: bool) -> RefOr<Schema> {
    schema(&SchemaNode::Integer { bits, signed }, &[])
}

fn nullable(inner: RefOr<Schema>) -> RefOr<Schema> {
    AnyOfBuilder::new()
        .item(inner)
        .item(ObjectBuilder::new().schema_type(Type::Null))
        .into()
}

fn number(v: &Value) -> Option<utoipa::Number> {
    match v {
        Value::Int(i) => Some((*i).into()),
        Value::Float(f) => Some((*f).into()),
        _ => None,
    }
}

/// A schema node as JSON travels (`hyve/docs/02-architecture.md` JSON 어댑터).
fn schema(node: &SchemaNode, constraints: &[Constraint]) -> RefOr<Schema> {
    let len = constraints.iter().find_map(|c| match c {
        Constraint::Len { min, max } => Some((*min, *max)),
        Constraint::Range { .. } => None,
    });
    let size = |n: Option<u64>| n.and_then(|n| usize::try_from(n).ok());
    match node {
        SchemaNode::Null => ObjectBuilder::new().schema_type(Type::Null).into(),
        SchemaNode::Bool => ObjectBuilder::new().schema_type(Type::Boolean).into(),
        SchemaNode::Integer { bits, signed } => {
            let mut o = ObjectBuilder::new().schema_type(Type::Integer).format(Some(
                // Unsigned: the signed format plus `minimum: 0`.
                SchemaFormat::KnownFormat(if *bits >= 64 {
                    KnownFormat::Int64
                } else {
                    KnownFormat::Int32
                }),
            ));
            if !signed {
                o = o.minimum(Some(0));
            }
            for c in constraints {
                if let Constraint::Range {
                    min,
                    exclusive_min,
                    max,
                    exclusive_max,
                } = c
                {
                    let (lo, hi) = (min.as_ref().and_then(number), max.as_ref().and_then(number));
                    o = if *exclusive_min {
                        o.exclusive_minimum(lo)
                    } else if lo.is_some() {
                        o.minimum(lo)
                    } else {
                        o
                    };
                    o = if *exclusive_max {
                        o.exclusive_maximum(hi)
                    } else {
                        o.maximum(hi)
                    };
                }
            }
            if *bits < 64 {
                return o.into();
            }
            // Beyond 2^53 the JSON codec writes a decimal string.
            AnyOfBuilder::new()
                .item(o)
                .item(
                    ObjectBuilder::new()
                        .schema_type(Type::String)
                        .pattern(Some("^-?[0-9]+$")),
                )
                .into()
        }
        SchemaNode::Float { bits } => {
            let mut o = ObjectBuilder::new().schema_type(Type::Number).format(Some(
                SchemaFormat::KnownFormat(if *bits == 32 {
                    KnownFormat::Float
                } else {
                    KnownFormat::Double
                }),
            ));
            for c in constraints {
                if let Constraint::Range {
                    min,
                    exclusive_min,
                    max,
                    exclusive_max,
                } = c
                {
                    let (lo, hi) = (min.as_ref().and_then(number), max.as_ref().and_then(number));
                    o = if *exclusive_min {
                        o.exclusive_minimum(lo)
                    } else if lo.is_some() {
                        o.minimum(lo)
                    } else {
                        o
                    };
                    o = if *exclusive_max {
                        o.exclusive_maximum(hi)
                    } else {
                        o.maximum(hi)
                    };
                }
            }
            AnyOfBuilder::new()
                .item(o)
                .item(
                    ObjectBuilder::new()
                        .schema_type(Type::String)
                        .enum_values(Some(["NaN", "Infinity", "-Infinity"])),
                )
                .into()
        }
        SchemaNode::String { format } => {
            let format = format.map(|f| match f {
                "date-time" => SchemaFormat::KnownFormat(KnownFormat::DateTime),
                other => SchemaFormat::Custom(other.to_owned()),
            });
            let (min, max) = len.unwrap_or_default();
            ObjectBuilder::new()
                .schema_type(Type::String)
                .format(format)
                .min_length(size(min))
                .max_length(size(max))
                .into()
        }
        SchemaNode::List { items, len: fixed } => {
            let (min, max) = match fixed {
                Some(n) => (Some(*n), Some(*n)),
                None => len.unwrap_or_default(),
            };
            ArrayBuilder::new()
                .items(schema(items, &[]))
                .min_items(size(min))
                .max_items(size(max))
                .into()
        }
        SchemaNode::Map { values } => ObjectBuilder::new()
            .additional_properties(Some(AdditionalProperties::RefOr(schema(values, &[]))))
            .into(),
        SchemaNode::Nullable(inner) => nullable(schema(inner, constraints)),
        SchemaNode::Any => ObjectBuilder::new()
            .schema_type(SchemaType::AnyValue)
            .into(),
        SchemaNode::Ref(name) => Ref::from_schema_name(name).into(),
    }
}
