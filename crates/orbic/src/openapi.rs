//! `orbic::openapi` — Automatic OpenAPI 3.1 document generation from mounted routes and `hyve-validate` schemas.

use hyve::prelude::*;

/// Route metadata entry collected for `.openapi("/docs")` generation.
#[derive(Debug, Clone)]
pub struct OpenApiRouteMeta {
    /// HTTP method (`"GET"`, `"POST"`, etc.).
    pub method: String,
    /// Full mounted URL path (`"/v1/projects"`).
    pub path: String,
    /// Optional request model schema.
    pub request_schema: Option<ModelSchema>,
    /// Optional response model schema.
    pub response_schema: Option<ModelSchema>,
}

fn schema_to_json(schema: &ModelSchema) -> JsonValue {
    let mut props = Vec::new();
    let mut required = Vec::new();
    for field in &schema.fields {
        let mut field_obj = Vec::new();
        let json_ty = if field.ty.contains("bool") {
            "boolean"
        } else if field.ty.contains("i32")
            || field.ty.contains("i64")
            || field.ty.contains("u32")
            || field.ty.contains("u64")
            || field.ty.contains("usize")
        {
            "integer"
        } else if field.ty.contains("f32") || field.ty.contains("f64") {
            "number"
        } else {
            "string"
        };
        field_obj.push(("type".to_owned(), JsonValue::Str(json_ty.to_owned())));
        for constraint in &field.constraints {
            match constraint {
                FieldConstraint::Len { min, max } => {
                    field_obj.push(("minLength".to_owned(), JsonValue::Number(*min as f64)));
                    field_obj.push(("maxLength".to_owned(), JsonValue::Number(*max as f64)));
                }
                FieldConstraint::Range { min, max } => {
                    field_obj.push(("minimum".to_owned(), JsonValue::Number(*min as f64)));
                    field_obj.push(("maximum".to_owned(), JsonValue::Number(*max as f64)));
                }
                FieldConstraint::NonEmpty => {
                    field_obj.push(("minLength".to_owned(), JsonValue::Number(1.0)));
                }
            }
        }
        let api_name = hyve::serde::snake_to_camel(field.name);
        if !field.ty.starts_with("Option") {
            required.push(JsonValue::Str(api_name.clone()));
        }
        props.push((api_name, JsonValue::Object(field_obj)));
    }
    JsonValue::Object(vec![
        ("type".to_owned(), JsonValue::Str("object".to_owned())),
        ("properties".to_owned(), JsonValue::Object(props)),
        ("required".to_owned(), JsonValue::Array(required)),
    ])
}

/// Builds an OpenAPI 3.1 JSON document from all mounted routes in an [`crate::OrbicApp`].
#[must_use]
pub fn build_openapi_document(routes: &[OpenApiRouteMeta]) -> JsonValue {
    let mut paths: Vec<(String, Vec<(String, JsonValue)>)> = Vec::new();
    let mut components: Vec<(String, JsonValue)> = Vec::new();

    for route in routes {
        let mut op = vec![(
            "operationId".to_owned(),
            JsonValue::Str(format!(
                "{}_{}",
                route.method.to_ascii_lowercase(),
                route.path.trim_start_matches('/').replace('/', "_")
            )),
        )];

        if let Some(req) = &route.request_schema {
            if !components.iter().any(|(n, _)| n == req.name) {
                components.push((req.name.to_owned(), schema_to_json(req)));
            }
            op.push((
                "requestBody".to_owned(),
                JsonValue::Object(vec![(
                    "content".to_owned(),
                    JsonValue::Object(vec![(
                        "application/json".to_owned(),
                        JsonValue::Object(vec![(
                            "schema".to_owned(),
                            JsonValue::Object(vec![(
                                "$ref".to_owned(),
                                JsonValue::Str(format!("#/components/schemas/{}", req.name)),
                            )]),
                        )]),
                    )]),
                )]),
            ));
        }

        let resp_schema = route.response_schema.as_ref().map(|res| {
            if !components.iter().any(|(n, _)| n == res.name) {
                components.push((res.name.to_owned(), schema_to_json(res)));
            }
            JsonValue::Object(vec![(
                "$ref".to_owned(),
                JsonValue::Str(format!("#/components/schemas/{}", res.name)),
            )])
        });

        let ok_resp = match resp_schema {
            Some(s) => JsonValue::Object(vec![
                ("description".to_owned(), JsonValue::Str("OK".to_owned())),
                (
                    "content".to_owned(),
                    JsonValue::Object(vec![(
                        "application/json".to_owned(),
                        JsonValue::Object(vec![("schema".to_owned(), s)]),
                    )]),
                ),
            ]),
            None => JsonValue::Object(vec![(
                "description".to_owned(),
                JsonValue::Str("OK".to_owned()),
            )]),
        };
        op.push((
            "responses".to_owned(),
            JsonValue::Object(vec![("200".to_owned(), ok_resp)]),
        ));

        let method_key = route.method.to_ascii_lowercase();
        if let Some((_, ops)) = paths.iter_mut().find(|(p, _)| p == &route.path) {
            ops.push((method_key, JsonValue::Object(op)));
        } else {
            paths.push((
                route.path.clone(),
                vec![(method_key, JsonValue::Object(op))],
            ));
        }
    }

    let path_objs = paths
        .into_iter()
        .map(|(p, ops)| (p, JsonValue::Object(ops)))
        .collect();

    JsonValue::Object(vec![
        ("openapi".to_owned(), JsonValue::Str("3.1.0".to_owned())),
        (
            "info".to_owned(),
            JsonValue::Object(vec![
                ("title".to_owned(), JsonValue::Str("Orbic API".to_owned())),
                ("version".to_owned(), JsonValue::Str("0.1.0".to_owned())),
            ]),
        ),
        ("paths".to_owned(), JsonValue::Object(path_objs)),
        (
            "components".to_owned(),
            JsonValue::Object(vec![("schemas".to_owned(), JsonValue::Object(components))]),
        ),
    ])
}
