//! Handler arguments: `Ctx<T>` (injected), `Valid<T>` (body), `Query<T>` and
//! `Path<T>` (`docs/01-dx-and-dsl.md` §1, 조립 계약).

use std::any::Any;

use hyve_api::{ContextNeed, Path, Query};
use hyve_core::{Ctx, HyveError, Scope, Value};
use hyve_serde::json::from_json_bytes;
use hyve_serde::{Binary, DecodeError, Json, from_json, to_json};
use hyve_validate::{Definition, Schema, SchemaNode, SchemaRegistry, Valid, Validate};

use crate::request::{Body, Rail, RequestMeta};

/// What an extractor can read from a request.
pub struct Parts {
    /// The request scope: values provided by enclosing components and by
    /// guards (`CurrentUser`).
    pub scope: Scope,
    /// The rail.
    pub rail: Rail,
    /// Path parameters by placeholder name.
    pub params: Vec<(String, String)>,
    /// The raw query string.
    pub query: String,
    /// The body; the first body extractor takes it.
    pub body: Body,
    /// Request metadata.
    pub meta: RequestMeta,
}

impl Parts {
    /// Takes the body, leaving [`Body::Empty`].
    pub fn take_body(&mut self) -> Body {
        std::mem::replace(&mut self.body, Body::Empty)
    }
}

/// Where a route's parameters come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsFrom {
    /// Path placeholders (`Path<T>`).
    Path,
    /// The query string (`Query<T>`).
    Query,
    /// Path placeholders with the same name, the query string otherwise
    /// (`#[server]` functions bound to `GET`/`DELETE`).
    PathOrQuery,
}

/// A group of parameters: an object's fields by their JSON names, or one
/// value called `name` (the route's only placeholder when `None`).
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// Where they come from.
    pub from: ParamsFrom,
    /// The name of a single non-object value.
    pub name: Option<String>,
    /// Their schema.
    pub schema: SchemaNode,
}

/// What a route reads from the request and answers, in protocol-neutral
/// schemas, for documentation generators (`orbic-openapi`).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteShape {
    /// Parameters.
    pub params: Vec<Params>,
    /// The request body.
    pub body: Option<SchemaNode>,
    /// The response model (`Null` for `()`, answered without a value).
    pub response: SchemaNode,
}

impl Default for RouteShape {
    fn default() -> Self {
        RouteShape {
            params: Vec::new(),
            body: None,
            response: SchemaNode::Null,
        }
    }
}

/// A handler argument.
pub trait Extract: Sized + Send + 'static {
    /// The injected value this argument needs, checked when the app is
    /// assembled.
    fn need() -> Option<ContextNeed> {
        None
    }

    /// Adds what this argument reads from the request to the route's
    /// description.
    fn describe(registry: &mut SchemaRegistry, shape: &mut RouteShape) {
        let _ = (registry, shape);
    }

    /// Reads the argument.
    fn extract(parts: &mut Parts) -> Result<Self, HyveError>;
}

impl<T: Send + Sync + 'static> Extract for Ctx<T> {
    fn need() -> Option<ContextNeed> {
        Some(ContextNeed::of::<T>())
    }

    fn extract(parts: &mut Parts) -> Result<Self, HyveError> {
        parts.scope.require::<T>()
    }
}

impl<T: Binary + Json + Validate + Schema + Send + 'static> Extract for Valid<T> {
    fn describe(registry: &mut SchemaRegistry, shape: &mut RouteShape) {
        shape.body = Some(T::schema(registry));
    }

    fn extract(parts: &mut Parts) -> Result<Self, HyveError> {
        Valid::new(decode_body::<T>(parts.rail, parts.take_body())?)
    }
}

impl<T: Json + Schema + Send + 'static> Extract for Query<T> {
    fn describe(registry: &mut SchemaRegistry, shape: &mut RouteShape) {
        shape.params.push(Params {
            from: ParamsFrom::Query,
            name: None,
            schema: T::schema(registry),
        });
    }

    fn extract(parts: &mut Parts) -> Result<Self, HyveError> {
        let pairs = parse_query(&parts.query)?;
        from_params::<T>(&pairs, None, "query").map(Query)
    }
}

impl<T: Json + Schema + Send + 'static> Extract for Path<T> {
    fn describe(registry: &mut SchemaRegistry, shape: &mut RouteShape) {
        shape.params.push(Params {
            from: ParamsFrom::Path,
            name: None,
            schema: T::schema(registry),
        });
    }

    fn extract(parts: &mut Parts) -> Result<Self, HyveError> {
        from_params::<T>(&parts.params, None, "path").map(Path)
    }
}

/// Decodes a request model from a body on `rail`.
pub fn decode_body<T: Binary + Json + 'static>(rail: Rail, body: Body) -> Result<T, HyveError> {
    match (rail, body) {
        (Rail::InProcess, Body::Value(v)) => downcast(v),
        (Rail::Fast, Body::Bytes(b)) => T::from_binary(&b).map_err(DecodeError::into_request_error),
        (Rail::Fast, Body::Empty) => T::from_binary(&[]).map_err(DecodeError::into_request_error),
        (Rail::Standard, Body::Bytes(b)) => {
            from_json_bytes::<T>(&b, hyve_serde::JsonLimits::DEFAULT)
                .map_err(DecodeError::into_request_error)
        }
        (Rail::Standard, Body::Empty) => {
            Err(HyveError::invalid_request("the request needs a JSON body"))
        }
        (_, Body::Value(_)) | (Rail::InProcess, _) => Err(HyveError::internal(
            "in-process requests carry typed values, other rails carry bytes",
        )),
    }
}

pub(crate) fn downcast<T: 'static>(v: Box<dyn Any + Send>) -> Result<T, HyveError> {
    v.downcast::<T>().map(|b| *b).map_err(|_| {
        HyveError::internal(format!(
            "in-process request is not a `{}`",
            std::any::type_name::<T>()
        ))
    })
}

/// Splits and percent-decodes `a=1&b=x`.
pub fn parse_query(query: &str) -> Result<Vec<(String, String)>, HyveError> {
    let mut out = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.push((percent_decode(k)?, percent_decode(v)?));
    }
    Ok(out)
}

fn percent_decode(s: &str) -> Result<String, HyveError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                let Some(byte) = hex else {
                    return Err(HyveError::invalid_request(format!(
                        "invalid percent-encoding in `{s}`"
                    )));
                };
                out.push(byte);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out)
        .map_err(|_| HyveError::invalid_request(format!("`{s}` is not valid UTF-8 once decoded")))
}

/// Builds `T` from text parameters, guided by its schema. A model takes its
/// fields by JSON name (lists repeat the parameter); any other type reads
/// the single parameter `single` (or the only one present).
pub fn from_params<T: Json + Schema>(
    pairs: &[(String, String)],
    single: Option<&str>,
    what: &str,
) -> Result<T, HyveError> {
    let mut reg = SchemaRegistry::new();
    let node = T::schema(&mut reg);
    let value = match object_fields(&reg, &node) {
        Some(fields) => {
            let mut map = Vec::new();
            for f in fields {
                let values: Vec<&str> = pairs
                    .iter()
                    .filter(|(k, _)| k == f.json_name)
                    .map(|(_, v)| v.as_str())
                    .collect();
                if values.is_empty() {
                    continue;
                }
                map.push((
                    f.json_name.to_owned(),
                    param_value(&f.schema, &values, f.json_name, what)?,
                ));
            }
            Value::Map(map)
        }
        None => {
            let values: Vec<&str> = match single {
                Some(name) => pairs
                    .iter()
                    .filter(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
                    .collect(),
                None => pairs.iter().map(|(_, v)| v.as_str()).collect(),
            };
            if values.is_empty() {
                return Err(HyveError::invalid_request(format!(
                    "missing {what} parameter `{}`",
                    single.unwrap_or("value")
                )));
            }
            param_value(&node, &values, single.unwrap_or("value"), what)?
        }
    };
    from_json::<T>(&to_json(&value)).map_err(|e| {
        let at = e.path().strip_prefix("$.").unwrap_or(e.path());
        HyveError::invalid_request(format!("invalid {what} parameter `{at}`: {}", e.message()))
    })
}

fn object_fields<'r>(
    reg: &'r SchemaRegistry,
    node: &SchemaNode,
) -> Option<&'r [hyve_validate::FieldSchema]> {
    match node {
        SchemaNode::Ref(name) => match reg.get(name) {
            Some(Definition::Object(fields)) => Some(fields),
            _ => None,
        },
        _ => None,
    }
}

fn param_value(
    node: &SchemaNode,
    values: &[&str],
    name: &str,
    what: &str,
) -> Result<Value, HyveError> {
    let bad = |expected: &str, v: &str| {
        HyveError::invalid_request(format!(
            "invalid {what} parameter `{name}`: expected {expected}, found `{v}`"
        ))
    };
    if let SchemaNode::List { items, .. } = node {
        return values
            .iter()
            .map(|v| param_value(items, &[v], name, what))
            .collect::<Result<_, _>>()
            .map(Value::List);
    }
    let [v] = values else {
        return Err(HyveError::invalid_request(format!(
            "{what} parameter `{name}` is repeated"
        )));
    };
    Ok(match node {
        SchemaNode::Nullable(inner) => return param_value(inner, values, name, what),
        SchemaNode::Bool => match *v {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(bad("`true` or `false`", v)),
        },
        SchemaNode::Integer { signed: true, .. } => v
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| bad("an integer", v))?,
        SchemaNode::Integer { signed: false, .. } => match v.parse::<u64>() {
            Ok(n) => i64::try_from(n).map(Value::Int).unwrap_or(Value::UInt(n)),
            Err(_) => return Err(bad("a non-negative integer", v)),
        },
        SchemaNode::Float { .. } => v
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| bad("a number", v))?,
        _ => Value::String((*v).to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_are_split_and_decoded() {
        let pairs = parse_query("a=1&b=x+y%2Fz&&c&d=%E2%9C%93").unwrap();
        let pairs: Vec<_> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(pairs, [("a", "1"), ("b", "x y/z"), ("c", ""), ("d", "✓")]);
        assert!(parse_query("a=%zz").is_err());
        assert!(parse_query("a=%ff").is_err());
    }

    #[test]
    fn single_values_use_their_own_schema() {
        let p = |v: &str| vec![("n".to_string(), v.to_string())];
        assert_eq!(from_params::<u32>(&p("42"), Some("n"), "path").unwrap(), 42);
        assert_eq!(
            from_params::<u64>(&p("18446744073709551615"), Some("n"), "path").unwrap(),
            u64::MAX
        );
        assert!(from_params::<u8>(&p("300"), Some("n"), "path").is_err());
        assert!(
            from_params::<u32>(&p("x"), Some("n"), "path")
                .unwrap_err()
                .message()
                .contains("expected a non-negative integer")
        );
        assert!(from_params::<bool>(&p("true"), Some("n"), "query").unwrap());
        assert_eq!(
            from_params::<Vec<i32>>(
                &[("n".into(), "1".into()), ("n".into(), "-2".into())],
                Some("n"),
                "query"
            )
            .unwrap(),
            [1, -2]
        );
        assert!(from_params::<u32>(&[], Some("n"), "path").is_err());
    }
}
