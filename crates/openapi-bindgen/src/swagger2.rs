//! Swagger 2.0 -> OpenAPI 3.0 normalization.
//!
//! The rest of the crate is typed against `openapiv3`, so we accept Swagger 2.0
//! ("OpenAPI 2") documents by structurally rewriting them into the OpenAPI 3.0 shape
//! *before* deserialization. This module operates purely on [`serde_json::Value`]: it
//! moves `definitions` into `components/schemas`, rewrites `$ref` targets, lifts v2
//! `body`/`formData` parameters into a `requestBody`, and nests the inline type of every
//! other parameter under a `schema`.
//!
//! Only the subset the generator actually consumes is translated; constructs it ignores
//! (security, servers, `host`/`basePath`/`schemes`, content negotiation) are dropped to
//! keep the OpenAPI 3 deserializer happy and avoid v2/v3 shape mismatches.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

/// HTTP method keys that may appear on a Path Item Object.
const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Keys carried inline on a Swagger 2.0 non-body parameter that describe its type. In
/// OpenAPI 3 these live nested under the parameter's `schema` object instead.
const PARAM_SCHEMA_KEYS: &[&str] = &[
    "type",
    "format",
    "items",
    "enum",
    "default",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "multipleOf",
];

/// Whether `doc` is a Swagger 2.0 document, i.e. it carries a `swagger: "2.x"` field.
#[must_use]
pub(crate) fn is_v2(doc: &Value) -> bool {
    doc.get("swagger")
        .and_then(Value::as_str)
        .is_some_and(|v| v.starts_with("2."))
}

/// Convert a Swagger 2.0 document into an OpenAPI 3.0 document, rewriting `doc` in place.
///
/// The transformation is intentionally partial: it covers paths, operations, parameters,
/// request bodies, responses, and `definitions`, plus the `$ref` rewrites those imply. It
/// is the caller's responsibility to have established that `doc` is a v2 document (see
/// [`is_v2`]).
pub(crate) fn convert(doc: &mut Value) -> Result<()> {
    let obj = doc
        .as_object_mut()
        .context("OpenAPI document must be a JSON object")?;

    // Swap the version discriminator.
    obj.remove("swagger");
    obj.insert("openapi".into(), Value::String("3.0.0".into()));

    // Lift `definitions` / `parameters` / `responses` into a `components` object.
    let mut components = Map::new();

    if let Some(Value::Object(defs)) = obj.remove("definitions") {
        components.insert("schemas".into(), Value::Object(defs));
    }

    if let Some(Value::Object(params)) = obj.remove("parameters") {
        let mut dst = Map::new();
        for (name, mut param) in params {
            // A shared body/formData parameter has no OpenAPI 3 `Parameter` representation;
            // drop it. References to it are skipped by the generator anyway.
            match param.get("in").and_then(Value::as_str) {
                Some("body" | "formData") => {}
                _ => {
                    convert_parameter(&mut param);
                    dst.insert(name, param);
                }
            }
        }
        if !dst.is_empty() {
            components.insert("parameters".into(), Value::Object(dst));
        }
    }

    if let Some(Value::Object(resps)) = obj.remove("responses") {
        let mut dst = Map::new();
        for (name, mut resp) in resps {
            convert_response(&mut resp);
            dst.insert(name, resp);
        }
        if !dst.is_empty() {
            components.insert("responses".into(), Value::Object(dst));
        }
    }

    if !components.is_empty() {
        obj.insert("components".into(), Value::Object(components));
    }

    // Drop top-level v2-only keys the generator ignores. `security` is dropped with its
    // definitions; `host`/`basePath`/`schemes` are the v3 `servers` the generator discards.
    for key in [
        "host",
        "basePath",
        "schemes",
        "consumes",
        "produces",
        "securityDefinitions",
        "security",
    ] {
        obj.remove(key);
    }

    // Convert every path item and its operations.
    if let Some(paths) = obj.get_mut("paths").and_then(Value::as_object_mut) {
        for (path, item) in paths.iter_mut() {
            convert_path_item(path, item);
        }
    }

    // Finally, rewrite `$ref` targets and normalize schema quirks across the whole
    // document (covering the structures we just moved into `components`).
    normalize(doc);

    Ok(())
}

/// Convert a single Path Item Object: its shared parameters and each operation.
fn convert_path_item(path: &str, item: &mut Value) {
    let Some(obj) = item.as_object_mut() else {
        return;
    };

    // Shared path-item parameters apply to every operation. Body/formData parameters have
    // no place at the path-item level in OpenAPI 3, so drop them (vanishingly rare in
    // practice); nest the rest.
    if let Some(Value::Array(arr)) = obj.remove("parameters") {
        let mut kept = Vec::with_capacity(arr.len());
        for mut param in arr {
            match param.get("in").and_then(Value::as_str) {
                Some("body" | "formData") => {}
                _ => {
                    convert_parameter(&mut param);
                    kept.push(param);
                }
            }
        }
        if !kept.is_empty() {
            obj.insert("parameters".into(), Value::Array(kept));
        }
    }

    for &method in HTTP_METHODS {
        if let Some(op) = obj.get_mut(method) {
            convert_operation(method, path, op);
        }
    }
}

/// Convert a single Operation Object: split its parameters into v3 parameters plus a
/// synthesized `requestBody`, and normalize its responses.
fn convert_operation(method: &str, path: &str, op: &mut Value) {
    let Some(obj) = op.as_object_mut() else {
        return;
    };

    obj.remove("consumes");
    obj.remove("produces");

    // `operationId` is optional in OpenAPI, but the generator requires one to name each
    // function. Synthesize a deterministic id from the method and path (unique per document)
    // when it is absent so such operations generate instead of being dropped.
    if obj
        .get("operationId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        obj.insert(
            "operationId".into(),
            Value::String(synthesize_operation_id(method, path)),
        );
    }

    if let Some(Value::Array(arr)) = obj.remove("parameters") {
        let mut kept = Vec::with_capacity(arr.len());
        let mut form_props = Map::new();
        let mut form_required: Vec<Value> = vec![];
        let mut body_schema: Option<Value> = None;
        let mut body_required = false;
        let mut body_description: Option<Value> = None;

        for mut param in arr {
            match param.get("in").and_then(Value::as_str) {
                Some("body") => {
                    if let Some(po) = param.as_object_mut() {
                        body_required =
                            po.get("required").and_then(Value::as_bool).unwrap_or(false);
                        body_description = po.remove("description");
                        body_schema = po.remove("schema");
                    }
                }
                Some("formData") => {
                    if let Some(po) = param.as_object() {
                        let Some(name) = po.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let name = name.to_string();
                        if po.get("required").and_then(Value::as_bool).unwrap_or(false) {
                            form_required.push(Value::String(name.clone()));
                        }
                        form_props.insert(name, formdata_property_schema(po));
                    }
                }
                _ => {
                    convert_parameter(&mut param);
                    kept.push(param);
                }
            }
        }

        if !kept.is_empty() {
            obj.insert("parameters".into(), Value::Array(kept));
        }

        // Prefer an explicit `in: body` parameter; otherwise fold `in: formData`
        // parameters into a synthesized object body so the generator picks up their fields.
        let request_body = if let Some(schema) = body_schema {
            let mut rb = Map::new();
            if let Some(desc) = body_description {
                rb.insert("description".into(), desc);
            }
            rb.insert("required".into(), Value::Bool(body_required));
            rb.insert(
                "content".into(),
                json!({ "application/json": { "schema": schema } }),
            );
            Some(Value::Object(rb))
        } else if !form_props.is_empty() {
            let any_required = !form_required.is_empty();
            let mut schema = Map::new();
            schema.insert("type".into(), Value::String("object".into()));
            schema.insert("properties".into(), Value::Object(form_props));
            if any_required {
                schema.insert("required".into(), Value::Array(form_required));
            }
            Some(json!({
                "required": any_required,
                "content": { "application/json": { "schema": Value::Object(schema) } },
            }))
        } else {
            None
        };

        if let Some(rb) = request_body {
            obj.insert("requestBody".into(), rb);
        }
    }

    if let Some(resps) = obj.get_mut("responses").and_then(Value::as_object_mut) {
        for (_code, resp) in resps.iter_mut() {
            convert_response(resp);
        }
    }
}

/// Synthesize a deterministic `operationId` for an operation that lacks one, derived from its
/// HTTP method and path. The method+path pair is unique within a document, so the result is
/// collision-free; the generator kebab-cases it downstream (e.g. `get /pets/{id}` becomes
/// `get-pets-id`).
fn synthesize_operation_id(method: &str, path: &str) -> String {
    let mut id = String::from(method);
    for segment in path.split('/') {
        let segment = segment.trim_matches(|c| c == '{' || c == '}');
        if !segment.is_empty() {
            id.push(' ');
            id.push_str(segment);
        }
    }
    id
}

/// Convert a single non-body parameter in place, nesting its inline type fields under a
/// `schema` object as OpenAPI 3 requires.
fn convert_parameter(param: &mut Value) {
    let Some(obj) = param.as_object_mut() else {
        return;
    };

    // A `$ref` parameter carries nothing to nest.
    if obj.contains_key("$ref") {
        return;
    }

    // In OpenAPI 3, a path parameter's `required` MUST be present and `true`.
    if obj.get("in").and_then(Value::as_str) == Some("path") {
        obj.insert("required".into(), Value::Bool(true));
    }

    obj.remove("collectionFormat");

    // Already in v3 shape (an explicit `schema`/`content`); leave it.
    if obj.contains_key("schema") || obj.contains_key("content") {
        return;
    }

    let mut schema = Map::new();
    for &key in PARAM_SCHEMA_KEYS {
        if let Some(value) = obj.remove(key) {
            schema.insert(key.to_string(), value);
        }
    }
    if schema.is_empty() {
        schema.insert("type".into(), Value::String("string".into()));
    }
    obj.insert("schema".into(), Value::Object(schema));
}

/// Build the OpenAPI 3 property schema for a single Swagger 2.0 `formData` parameter,
/// preserving its description so the generated record field keeps its docs.
fn formdata_property_schema(param: &Map<String, Value>) -> Value {
    let mut schema = Map::new();
    for &key in PARAM_SCHEMA_KEYS {
        if let Some(value) = param.get(key) {
            schema.insert(key.to_string(), value.clone());
        }
    }
    if let Some(desc) = param.get("description") {
        schema.insert("description".into(), desc.clone());
    }
    if schema.is_empty() {
        schema.insert("type".into(), Value::String("string".into()));
    }
    Value::Object(schema)
}

/// Convert a single Response Object: ensure the required `description`, move a v2 `schema`
/// under `content`, and drop response constructs the generator ignores.
fn convert_response(resp: &mut Value) {
    let Some(obj) = resp.as_object_mut() else {
        return;
    };

    if obj.contains_key("$ref") {
        return;
    }

    // OpenAPI 3 requires a (string) description on every response.
    if !obj.get("description").is_some_and(Value::is_string) {
        obj.insert("description".into(), Value::String(String::new()));
    }

    if let Some(schema) = obj.remove("schema") {
        obj.insert(
            "content".into(),
            json!({ "application/json": { "schema": schema } }),
        );
    }

    // v2 `headers` and `examples` differ in shape from v3 and are unused; drop them.
    obj.remove("headers");
    obj.remove("examples");
}

/// Recursively rewrite `$ref` targets from their Swagger 2.0 locations to the OpenAPI 3
/// `components` layout, and normalize two schema quirks: the v2-only `type: file` (mapped
/// to `string`) and the v2 string-valued `discriminator` (dropped, since v3 expects an
/// object).
fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("$ref") {
                if let Some(rest) = reference.strip_prefix("#/definitions/") {
                    *reference = format!("#/components/schemas/{rest}");
                } else if let Some(rest) = reference.strip_prefix("#/parameters/") {
                    *reference = format!("#/components/parameters/{rest}");
                } else if let Some(rest) = reference.strip_prefix("#/responses/") {
                    *reference = format!("#/components/responses/{rest}");
                }
            }

            if matches!(map.get("type"), Some(Value::String(t)) if t == "file") {
                map.insert("type".into(), Value::String("string".into()));
            }

            if matches!(map.get("discriminator"), Some(Value::String(_))) {
                map.remove("discriminator");
            }

            for (_key, child) in map.iter_mut() {
                normalize(child);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                normalize(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_value(mut doc: Value) -> Value {
        convert(&mut doc).expect("conversion should succeed");
        doc
    }

    #[test]
    fn detects_v2_documents() {
        assert!(is_v2(&json!({ "swagger": "2.0" })));
        assert!(is_v2(&json!({ "swagger": "2.0.1" })));
        assert!(!is_v2(&json!({ "openapi": "3.0.0" })));
        assert!(!is_v2(&json!({})));
    }

    #[test]
    fn swaps_version_discriminator() {
        let out = convert_value(json!({ "swagger": "2.0", "paths": {} }));
        assert_eq!(
            out.pointer("/openapi").and_then(Value::as_str),
            Some("3.0.0")
        );
        assert!(out.pointer("/swagger").is_none());
    }

    #[test]
    fn moves_definitions_and_rewrites_refs() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "definitions": {
                "User": {
                    "type": "object",
                    "properties": { "manager": { "$ref": "#/definitions/User" } },
                },
            },
            "paths": {},
        }));
        assert!(out.pointer("/components/schemas/User").is_some());
        assert!(out.pointer("/definitions").is_none());
        assert_eq!(
            out.pointer("/components/schemas/User/properties/manager/$ref")
                .and_then(Value::as_str),
            Some("#/components/schemas/User"),
        );
    }

    #[test]
    fn nests_query_parameter_type_under_schema() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/things": {
                    "get": {
                        "operationId": "listThings",
                        "parameters": [
                            { "name": "limit", "in": "query", "type": "integer", "format": "int64" },
                        ],
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
        }));
        let param = out
            .pointer("/paths/~1things/get/parameters/0")
            .expect("parameter present");
        assert_eq!(param.pointer("/in").and_then(Value::as_str), Some("query"));
        assert_eq!(
            param.pointer("/schema/type").and_then(Value::as_str),
            Some("integer")
        );
        assert_eq!(
            param.pointer("/schema/format").and_then(Value::as_str),
            Some("int64"),
        );
        assert!(param.pointer("/type").is_none());
    }

    #[test]
    fn forces_path_parameter_required() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/things/{id}": {
                    "get": {
                        "operationId": "getThing",
                        "parameters": [{ "name": "id", "in": "path", "type": "string" }],
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
        }));
        assert_eq!(
            out.pointer("/paths/~1things~1{id}/get/parameters/0/required")
                .and_then(Value::as_bool),
            Some(true),
        );
    }

    #[test]
    fn lifts_body_parameter_to_request_body() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/things": {
                    "post": {
                        "operationId": "createThing",
                        "parameters": [{
                            "name": "thing",
                            "in": "body",
                            "required": true,
                            "schema": { "$ref": "#/definitions/Thing" },
                        }],
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
            "definitions": { "Thing": { "type": "object", "properties": { "name": { "type": "string" } } } },
        }));
        let op = out
            .pointer("/paths/~1things/post")
            .expect("operation present");
        assert!(op.pointer("/parameters").is_none());
        assert_eq!(
            op.pointer("/requestBody/required").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            op.pointer("/requestBody/content/application~1json/schema/$ref")
                .and_then(Value::as_str),
            Some("#/components/schemas/Thing"),
        );
    }

    #[test]
    fn folds_formdata_into_object_request_body() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/check": {
                    "post": {
                        "operationId": "check",
                        "parameters": [
                            { "name": "text", "in": "formData", "type": "string", "required": true },
                            { "name": "language", "in": "formData", "type": "string" },
                        ],
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
        }));
        let schema = out
            .pointer("/paths/~1check/post/requestBody/content/application~1json/schema")
            .expect("request body schema present");
        assert_eq!(
            schema.pointer("/type").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            schema
                .pointer("/properties/text/type")
                .and_then(Value::as_str),
            Some("string"),
        );
        assert_eq!(
            schema
                .pointer("/properties/language/type")
                .and_then(Value::as_str),
            Some("string"),
        );
        let required = schema
            .pointer("/required")
            .and_then(Value::as_array)
            .expect("required list");
        assert_eq!(required, &vec![Value::String("text".into())]);
    }

    #[test]
    fn moves_response_schema_to_content() {
        let mut resp = json!({ "description": "ok", "schema": { "type": "string" } });
        convert_response(&mut resp);
        assert_eq!(
            resp.pointer("/content/application~1json/schema/type")
                .and_then(Value::as_str),
            Some("string"),
        );
        assert!(resp.pointer("/schema").is_none());
    }

    #[test]
    fn synthesizes_missing_response_description() {
        let mut resp = json!({ "schema": { "type": "string" } });
        convert_response(&mut resp);
        assert_eq!(
            resp.pointer("/description").and_then(Value::as_str),
            Some("")
        );
    }

    #[test]
    fn maps_type_file_to_string() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/upload": {
                    "post": {
                        "operationId": "upload",
                        "parameters": [{ "name": "file", "in": "formData", "type": "file" }],
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
        }));
        assert_eq!(
            out.pointer("/paths/~1upload/post/requestBody/content/application~1json/schema/properties/file/type")
                .and_then(Value::as_str),
            Some("string"),
        );
    }

    #[test]
    fn drops_string_discriminator() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "definitions": { "Pet": { "type": "object", "discriminator": "petType" } },
            "paths": {},
        }));
        assert!(
            out.pointer("/components/schemas/Pet/discriminator")
                .is_none()
        );
    }

    #[test]
    fn synthesizes_missing_operation_id_from_method_and_path() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/pets/{id}": {
                    "get": {
                        "tags": ["pets"],
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
        }));
        assert_eq!(
            out.pointer("/paths/~1pets~1{id}/get/operationId")
                .and_then(Value::as_str),
            Some("get pets id"),
        );
    }

    #[test]
    fn keeps_existing_operation_id() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "responses": { "200": { "description": "ok" } },
                    },
                },
            },
        }));
        assert_eq!(
            out.pointer("/paths/~1pets/get/operationId")
                .and_then(Value::as_str),
            Some("listPets"),
        );
    }

    #[test]
    fn converted_document_deserializes_as_openapi_v3() {
        let out = convert_value(json!({
            "swagger": "2.0",
            "info": { "title": "demo", "version": "1.0.0" },
            "host": "api.example.com",
            "basePath": "/v2",
            "schemes": ["https"],
            "produces": ["application/json"],
            "securityDefinitions": { "key": { "type": "apiKey", "name": "X-Key", "in": "header" } },
            "paths": {
                "/things/{id}": {
                    "get": {
                        "operationId": "getThing",
                        "tags": ["things"],
                        "parameters": [
                            { "name": "id", "in": "path", "type": "string" },
                            { "name": "verbose", "in": "query", "type": "boolean" },
                        ],
                        "responses": { "200": { "description": "ok", "schema": { "$ref": "#/definitions/Thing" } } },
                    },
                    "post": {
                        "operationId": "createThing",
                        "tags": ["things"],
                        "parameters": [{ "name": "body", "in": "body", "schema": { "$ref": "#/definitions/Thing" } }],
                        "responses": { "201": { "description": "created" } },
                    },
                },
            },
            "definitions": {
                "Thing": {
                    "type": "object",
                    "required": ["name"],
                    "properties": { "name": { "type": "string" }, "size": { "type": "integer", "format": "int64" } },
                },
            },
        }));
        let spec: openapiv3::OpenAPI =
            serde_json::from_value(out).expect("converted doc should parse as OpenAPI 3");
        assert_eq!(spec.openapi, "3.0.0");
        assert!(spec.paths.paths.contains_key("/things/{id}"));
    }
}
