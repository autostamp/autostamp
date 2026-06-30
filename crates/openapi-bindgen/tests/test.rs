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

/// An OpenAPI 3 document exercising security schemes: a global `bearer` default, a
/// per-operation `apiKey` override (header OR query alternatives), a server URL, and a
/// header parameter.
const AUTH_SPEC: &str = r#"{
  "openapi": "3.0.0",
  "info": { "title": "demo", "version": "1.0.0" },
  "servers": [{ "url": "https://api.example.com/v1/" }],
  "components": {
    "securitySchemes": {
      "bearerAuth": { "type": "http", "scheme": "bearer" },
      "apiKeyHeader": { "type": "apiKey", "in": "header", "name": "X-API-Key" },
      "apiKeyQuery": { "type": "apiKey", "in": "query", "name": "api_key" }
    }
  },
  "security": [{ "bearerAuth": [] }],
  "paths": {
    "/widgets/{widgetId}": {
      "get": {
        "tags": ["widgets"],
        "operationId": "getWidget",
        "parameters": [
          { "name": "widgetId", "in": "path", "required": true, "schema": { "type": "string" } },
          { "name": "X-Trace", "in": "header", "required": false, "schema": { "type": "string" } }
        ],
        "responses": { "200": { "description": "ok" } }
      }
    },
    "/search": {
      "get": {
        "tags": ["widgets"],
        "operationId": "searchWidgets",
        "security": [{ "apiKeyHeader": [] }, { "apiKeyQuery": [] }],
        "responses": { "200": { "description": "ok" } }
      }
    }
  }
}"#;

#[test]
fn emits_world_importing_http_and_secrets() {
    let spec = parse_openapi(AUTH_SPEC).unwrap();
    let package = PackageName::parse("widget:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The generated world imports the host HTTP + secrets capabilities and exports the
    // generated interface; operations themselves stay auth-free.
    assert!(generated.wit.contains("world bindgen {"));
    assert!(generated.wit.contains("import wasi:http/outgoing-handler"));
    assert!(generated.wit.contains("import wasmcloud:secrets/store"));
    assert!(generated.wit.contains("import wasmcloud:secrets/reveal"));
    assert!(generated.wit.contains("export widgets;"));

    // The wasm.toml declares the interface dependencies for the component build.
    assert!(generated.wasm_toml.contains("wasi:http"));
    assert!(generated.wasm_toml.contains("wasmcloud:secrets@1.0.0"));
}

#[test]
fn emits_base_url_and_auth_tables() {
    let spec = parse_openapi(AUTH_SPEC).unwrap();
    let package = PackageName::parse("widget:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();
    let rust = &generated.rust;

    // The base URL is resolved from `servers` (trailing slash stripped).
    assert!(rust.contains(r#"const BASE_URL: &str = "https://api.example.com/v1";"#));

    // The globally-defaulted operation carries the bearer scheme, keyed by the security
    // scheme's name.
    assert!(rust.contains(r#"AuthApply { secret_key: "bearerAuth", kind: AuthKind::Bearer }"#));

    // The per-operation override resolves the *first* alternative (header), carrying the
    // real wire header name.
    assert!(rust.contains(
        r#"AuthApply { secret_key: "apiKeyHeader", kind: AuthKind::ApiKeyHeader("X-API-Key") }"#
    ));

    // The path placeholder is snake-cased to match the runtime's field key, and the header
    // parameter is located in the header (not miscategorized as a query).
    assert!(rust.contains(r#"path_template: "/widgets/{widget_id}""#));
    assert!(rust.contains("location: FieldLocation::Header"));
}

/// An OpenAPI 3 document where a security credential (`secret`, an apiKey header) is *also*
/// declared as a redundant request-body property — the Plaid pattern. A second body field
/// (`amount`) is a legitimate, non-credential input that must survive pruning.
const DEDUP_SPEC: &str = r#"{
  "openapi": "3.0.0",
  "info": { "title": "pay", "version": "1.0.0" },
  "servers": [{ "url": "https://api.pay.test" }],
  "components": {
    "securitySchemes": {
      "secret": { "type": "apiKey", "in": "header", "name": "X-Secret" }
    }
  },
  "security": [{ "secret": [] }],
  "paths": {
    "/charge": {
      "post": {
        "tags": ["billing"],
        "operationId": "createCharge",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "required": ["amount"],
                "properties": {
                  "amount": { "type": "integer", "format": "int64" },
                  "secret": { "type": "string" }
                }
              }
            }
          }
        },
        "responses": { "200": { "description": "ok" } }
      }
    }
  }
}"#;

#[test]
fn prunes_request_fields_that_duplicate_injected_credentials() {
    let spec = parse_openapi(DEDUP_SPEC).unwrap();
    let package = PackageName::parse("pay:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The params record keeps the real input but drops the body field that merely duplicates
    // the injected `secret` credential.
    let wit = &generated.wit;
    let start = wit
        .find("record create-charge-params")
        .expect("params record present");
    let record = &wit[start..start + wit[start..].find('}').expect("record closes")];
    assert!(record.contains("amount"), "non-credential field retained");
    assert!(
        !record.contains("secret"),
        "duplicated credential field pruned from params record:\n{record}"
    );

    // The credential is still injected centrally: the OpSpec carries the auth table even
    // though the body field is gone.
    assert!(
        generated.rust.contains(
            r#"AuthApply { secret_key: "secret", kind: AuthKind::ApiKeyHeader("X-Secret") }"#
        ),
        "auth table retained after pruning"
    );

    // The now-orphaned credential field leaves no dangling record behind.
    assert!(generated.wit.contains("amount"));
}

#[test]
fn generates_readme_with_generator_diagnostics() {
    // The dedup spec prunes exactly one duplicated credential field, so the README's
    // diagnostics table should report the prune heuristic as triggered.
    let spec = parse_openapi(DEDUP_SPEC).unwrap();
    let package = PackageName::parse("pay:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // Title matches the component name; the diagnostics section documents the options and
    // heuristics this component was generated with.
    assert!(generated.readme.starts_with("# api\n"));
    assert!(generated.readme.contains("## Generator Diagnostics"));
    assert!(generated.readme.contains("| Package | `pay:api@0.1.0` |"));
    assert!(generated.readme.contains("| Tag filter | all tags |"));
    assert!(generated.readme.contains(
        "| Prune duplicate credential fields | enabled — **triggered**, 1 field pruned |"
    ));

    // A spec with nothing to prune reports the heuristic as not triggered.
    let plain = parse_openapi(MINIMAL_SPEC).unwrap();
    let plain_pkg = PackageName::parse("demo:things@0.1.0").unwrap();
    let plain_readme = generate(&plain, &plain_pkg, None).unwrap().readme;
    assert!(plain_readme.starts_with("# things\n"));
    assert!(
        plain_readme.contains("| Prune duplicate credential fields | enabled — not triggered |")
    );
}
