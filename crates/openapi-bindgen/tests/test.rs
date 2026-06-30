//! Integration tests for `openapi-bindgen`.

use openapi_bindgen::{PackageName, generate, parse_openapi};

/// A minimal OpenAPI 3 document with a single tagged operation.
const MINIMAL_SPEC: &str = r#"{
  "openapi": "3.0.0",
  "info": { "title": "demo", "version": "1.0.0" },
  "paths": {
    "/things": {
      "get": {
        "tags": ["things"],
        "operationId": "listThings",
        "responses": { "200": { "description": "ok" } }
      }
    }
  }
}"#;

/// A Swagger 2.0 document exercising path/query parameters, a `$ref` body parameter, and a
/// shared `definitions` schema.
const V2_SPEC: &str = r##"{
  "swagger": "2.0",
  "info": { "title": "demo", "version": "1.0.0" },
  "host": "api.example.com",
  "basePath": "/v2",
  "schemes": ["https"],
  "produces": ["application/json"],
  "paths": {
    "/orgs/{org}/things": {
      "get": {
        "tags": ["things"],
        "operationId": "listThings",
        "parameters": [
          { "name": "org", "in": "path", "type": "string" },
          { "name": "limit", "in": "query", "type": "integer", "format": "int64" }
        ],
        "responses": { "200": { "description": "ok", "schema": { "$ref": "#/definitions/Thing" } } }
      },
      "post": {
        "tags": ["things"],
        "operationId": "createThing",
        "parameters": [
          { "name": "org", "in": "path", "type": "string" },
          { "name": "thing", "in": "body", "required": true, "schema": { "$ref": "#/definitions/Thing" } }
        ],
        "responses": { "201": { "description": "created" } }
      }
    }
  },
  "definitions": {
    "Thing": {
      "type": "object",
      "required": ["name"],
      "properties": {
        "name": { "type": "string" },
        "size": { "type": "integer", "format": "int64" }
      }
    }
  }
}"##;

/// A Swagger 2.0 document whose only inputs are `formData` parameters, including an enum.
const V2_FORMDATA_SPEC: &str = r#"{
  "swagger": "2.0",
  "info": { "title": "lt", "version": "1.0.0" },
  "consumes": ["application/x-www-form-urlencoded"],
  "paths": {
    "/check": {
      "post": {
        "tags": ["checker"],
        "operationId": "checkText",
        "parameters": [
          { "name": "text", "in": "formData", "type": "string", "required": true },
          { "name": "language", "in": "formData", "type": "string", "required": true },
          { "name": "mode", "in": "formData", "type": "string", "enum": ["fast", "careful"] }
        ],
        "responses": { "200": { "description": "ok" } }
      }
    }
  }
}"#;

#[test]
fn smoke() {}

#[test]
fn generates_valid_wit_and_manifest() {
    let spec: openapiv3::OpenAPI = serde_json::from_str(MINIMAL_SPEC).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();

    // `generate` validates the WIT with wit-parser internally, so a successful return
    // means the generated package parsed and resolved cleanly.
    let generated = generate(&spec, &package, None).unwrap();

    assert!(generated.wit.contains("package wilted:demo@0.1.0;"));
    assert!(generated.interfaces.iter().any(|i| i == "things"));

    // The manifest is emitted as part of generation.
    assert!(generated.cargo_toml.contains("name = \"demo\""));
    assert!(generated.cargo_toml.contains("version = \"0.1.0\""));
    assert!(generated.cargo_toml.contains("crate-type = [\"cdylib\"]"));
    assert!(generated.cargo_toml.contains("wit-bindgen"));
}

#[test]
fn parses_openapi_v3_through_parse_openapi() {
    // `parse_openapi` must leave a v3 document untouched and still drive generation.
    let spec = parse_openapi(MINIMAL_SPEC).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();
    assert!(generated.interfaces.iter().any(|i| i == "things"));
}

#[test]
fn generates_from_swagger_v2() {
    // A Swagger 2.0 document is normalized to v3 by `parse_openapi`, then drives generation
    // through the same pipeline. A successful `generate` means the WIT validated.
    let spec = parse_openapi(V2_SPEC).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(generated.interfaces.iter().any(|i| i == "things"));

    // Path params are rendered as required strings; the int64 query param becomes an
    // optional `s64`.
    assert!(generated.wit.contains("list-things-params"));
    assert!(generated.wit.contains("org: string"));
    assert!(generated.wit.contains("limit: option<s64>"));

    // The `$ref` body parameter's object fields are inlined into the params record: a
    // required `name` and an optional int64 `size`.
    assert!(generated.wit.contains("create-thing-params"));
    assert!(generated.wit.contains("name: string"));
    assert!(generated.wit.contains("size: option<s64>"));
}

#[test]
fn generates_from_swagger_v2_formdata() {
    // `formData` parameters are folded into a JSON request body so their fields surface as
    // body parameters on the operation.
    let spec = parse_openapi(V2_FORMDATA_SPEC).unwrap();
    let package = PackageName::parse("wilted:lt@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(generated.interfaces.iter().any(|i| i == "checker"));
    assert!(generated.wit.contains("check-text-params"));
    assert!(generated.wit.contains("text: string"));
    assert!(generated.wit.contains("language: string"));

    // The enum-typed `mode` field emits a WIT enum carrying its cases.
    assert!(generated.wit.contains("enum "));
    assert!(generated.wit.contains("fast"));
    assert!(generated.wit.contains("careful"));
}
