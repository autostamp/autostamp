//! Integration tests for `openapi-bindgen`.

use openapi_bindgen::{NoOperations, PackageName, generate, parse_openapi};

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
fn synthesizes_function_name_for_operation_without_operation_id() {
    // `operationId` is optional in OpenAPI; an operation without one must still generate,
    // with a function name synthesized from its method and path (instead of panicking).
    let spec_json = r#"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets/{id}": {
          "get": {
            "tags": ["pets"],
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"#;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();
    assert!(generated.interfaces.iter().any(|i| i == "pets"));
    // method + path -> `get pets id` -> kebab `get-pets-id`.
    assert!(
        generated.wit.contains("get-pets-id:"),
        "expected synthesized function name in WIT:\n{}",
        generated.wit
    );
}

#[test]
fn groups_untagged_operations_by_path_segment() {
    // A document whose operations have no tags still generates: each operation groups into an
    // interface named after its first meaningful path segment (the `v1` version prefix is
    // skipped), instead of bailing with "no interfaces".
    let spec_json = r#"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/v1/charges": {
          "get": { "operationId": "listCharges", "responses": { "200": { "description": "ok" } } }
        },
        "/v1/customers": {
          "get": { "operationId": "listCustomers", "responses": { "200": { "description": "ok" } } }
        }
      }
    }"#;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();
    assert!(generated.interfaces.iter().any(|i| i == "charges"));
    assert!(generated.interfaces.iter().any(|i| i == "customers"));
}

#[test]
fn dedupes_colliding_operation_names() {
    // Two operations whose ids kebab to the same `validate-address` under one tag would, without
    // disambiguation, emit two same-named WIT functions — a "defined more than once" error.
    let spec_json = r#"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/a": { "post": { "tags": ["things"], "operationId": "validateAddress",
          "responses": { "200": { "description": "ok" } } } },
        "/b": { "get": { "tags": ["things"], "operationId": "ValidateAddress",
          "responses": { "200": { "description": "ok" } } } }
      }
    }"#;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    // `generate` validates the WIT internally, so an Ok result means the names were disambiguated.
    let generated = generate(&spec, &package, None).unwrap();
    assert!(generated.wit.contains("validate-address:"));
    assert!(generated.wit.contains("validate-address-v2:"));
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
fn emits_world_importing_secrets_and_declaring_http_dep() {
    let spec = parse_openapi(AUTH_SPEC).unwrap();
    let package = PackageName::parse("widget:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The generated world imports only the secrets host capability and exports the generated
    // interface; operations themselves stay auth-free. The HTTP host import is contributed by
    // the `wstd` crate's own bindings at build time, so the world does not declare it.
    assert!(generated.wit.contains("world client {"));
    assert!(!generated.wit.contains("import wasi:http"));
    assert!(generated.wit.contains("import wasmcloud:secrets/store"));
    assert!(generated.wit.contains("import wasmcloud:secrets/reveal"));
    assert!(generated.wit.contains("export widgets;"));

    // The wasm.toml carries a publishable [package] section plus explicit interface deps. The
    // built component still imports `wasi:http` (via `wstd`), so it stays declared here.
    assert!(generated.wasm_toml.contains("[package]"));
    assert!(
        generated
            .wasm_toml
            .contains("registry = \"ghcr.io/widget/api\"")
    );
    assert!(generated.wasm_toml.contains(
        "\"wasi:http\" = { registry = \"ghcr.io\", namespace = \"webassembly\", package = \"wasi/http\", version = \"0.2.3\" }"
    ));
    assert!(generated.wasm_toml.contains(
        "\"wasmcloud:secrets\" = { registry = \"ghcr.io\", namespace = \"wasmcloud\", package = \"interfaces/wasmcloud/secrets\", version = \"1.0.0\" }"
    ));
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

    // The dedup spec requires the `secret` apiKey header, so the README documents it in an
    // Authentication section above the diagnostics.
    let auth_at = generated
        .readme
        .find("## Authentication")
        .expect("authentication section present");
    let diag_at = generated.readme.find("## Generator Diagnostics").unwrap();
    assert!(auth_at < diag_at, "auth section precedes diagnostics");
    assert!(
        generated
            .readme
            .contains("| `secret` | header `X-Secret` |")
    );

    // A spec with nothing to prune reports the heuristic as not triggered.
    let plain = parse_openapi(MINIMAL_SPEC).unwrap();
    let plain_pkg = PackageName::parse("demo:things@0.1.0").unwrap();
    let plain_readme = generate(&plain, &plain_pkg, None).unwrap().readme;
    assert!(plain_readme.starts_with("# things\n"));
    assert!(
        plain_readme.contains("| Prune duplicate credential fields | enabled — not triggered |")
    );
    // No security schemes means no Authentication section at all.
    assert!(!plain_readme.contains("## Authentication"));
}

#[test]
fn documents_required_auth_secrets_in_readme_and_wit() {
    let spec = parse_openapi(AUTH_SPEC).unwrap();
    let package = PackageName::parse("widget:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The same authentication table is rendered into both the README and the WIT package
    // comment, listing each named secret and how it is applied to requests.
    let bearer_row = "| `bearerAuth` | `Authorization: Bearer <secret>` |";
    let header_row = "| `apiKeyHeader` | header `X-API-Key` |";
    assert!(generated.readme.contains("## Authentication"));
    assert!(generated.readme.contains(bearer_row));
    assert!(generated.readme.contains(header_row));

    // The WIT carries the same rows as a doc comment leading the `package` declaration.
    assert!(generated.wit.contains("/// ## Authentication"));
    assert!(generated.wit.contains(&format!("/// {bearer_row}")));
    assert!(generated.wit.contains(&format!("/// {header_row}")));
    let comment_at = generated.wit.find("/// ## Authentication").unwrap();
    let package_at = generated.wit.find("package widget:api").unwrap();
    assert!(comment_at < package_at, "auth comment leads the package");

    // `searchWidgets` declares `apiKeyHeader OR apiKeyQuery`; only the resolved first
    // alternative is documented, so the query-key secret is omitted from both surfaces.
    assert!(!generated.readme.contains("apiKeyQuery"));
    assert!(!generated.readme.contains("api_key"));
    assert!(!generated.wit.contains("apiKeyQuery"));
    assert!(!generated.wit.contains("api_key"));

    // A spec with no security schemes gets no auth comment: the WIT begins at `package`.
    let plain = parse_openapi(MINIMAL_SPEC).unwrap();
    let plain_pkg = PackageName::parse("demo:things@0.1.0").unwrap();
    let plain = generate(&plain, &plain_pkg, None).unwrap();
    assert!(!plain.wit.contains("## Authentication"));
    assert!(
        plain.wit.trim_start().starts_with("package"),
        "WIT with no auth starts at the package declaration"
    );
}

/// A document whose `paths` is empty defines no operations to bind. `generate` reports this with
/// the distinct `NoOperations` error so callers can treat it as a benign skip rather than a
/// failure (e.g. ipinfodb.com ships `paths: {}`).
#[test]
fn empty_document_reports_no_operations() {
    const EMPTY_SPEC: &str = r#"{
  "openapi": "3.0.0",
  "info": { "title": "empty", "version": "1.0.0" },
  "paths": {}
}"#;
    let spec = parse_openapi(EMPTY_SPEC).unwrap();
    let package = PackageName::parse("demo:empty@0.1.0").unwrap();

    let err = generate(&spec, &package, None).expect_err("empty document should not generate");
    assert!(
        err.downcast_ref::<NoOperations>().is_some(),
        "expected NoOperations, got: {err}"
    );
}

/// Some specs (notably DigitalOcean) reuse inline schemas via whole-document `$ref`s into
/// `#/paths/...` rather than `#/components/...`. The schema resolver rejects those directly, so
/// they are inlined before parsing. Here `createWidget`'s request body has a `color` property
/// that references a schema defined under the `/palette` path; generation must resolve it to the
/// concrete `string` type (without inlining, the resolver fails with `unsupported ref`).
#[test]
fn inlines_paths_ref_into_concrete_schema() {
    const PATHS_REF_SPEC: &str = r##"{
  "openapi": "3.0.0",
  "info": { "title": "demo", "version": "1.0.0" },
  "paths": {
    "/widgets": {
      "post": {
        "tags": ["widgets"],
        "operationId": "createWidget",
        "requestBody": {
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "properties": {
                  "color": {
                    "$ref": "#/paths/~1palette/get/responses/200/content/application~1json/schema"
                  }
                }
              }
            }
          }
        },
        "responses": { "200": { "description": "ok" } }
      }
    },
    "/palette": {
      "get": {
        "tags": ["palette"],
        "operationId": "getPalette",
        "responses": {
          "200": {
            "description": "ok",
            "content": {
              "application/json": { "schema": { "type": "string" } }
            }
          }
        }
      }
    }
  }
}"##;
    let spec = parse_openapi(PATHS_REF_SPEC).unwrap();
    let package = PackageName::parse("demo:widgets@0.1.0").unwrap();

    let generated =
        generate(&spec, &package, None).expect("paths `$ref` should inline and resolve");
    assert!(
        generated.wit.contains("color: option<string>"),
        "inlined `#/paths` ref should resolve to a concrete string field:\n{}",
        generated.wit
    );
}

// r[verify codegen.request-body.shared-ref]
// Several real specs (e.g. Azure Cognitive Services' Computer Vision) point many operations
// at one shared `#/components/requestBodies/...` body. The generator must resolve that `$ref`
// and still emit the operation; the original code dropped such operations, leaving
// function-less interfaces that wit-bindgen generates no `Guest` trait for (E0405).
#[test]
fn resolves_shared_ref_request_body() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/analyze": {
          "post": {
            "tags": ["analyze"],
            "operationId": "analyzeImage",
            "requestBody": { "$ref": "#/components/requestBodies/ImageUrl" },
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "requestBodies": {
          "ImageUrl": {
            "required": true,
            "content": {
              "application/json": {
                "schema": { "type": "object", "properties": { "url": { "type": "string" } } }
              }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The operation survives (the interface is not dropped as empty) ...
    assert!(generated.interfaces.iter().any(|i| i == "analyze"));
    // ... and the shared body's field is inlined into the operation's params record.
    assert!(
        generated.wit.contains("url: option<string>"),
        "shared `$ref` request body should resolve and inline its fields:\n{}",
        generated.wit
    );
}

// r[verify codegen.interface.exports-collision]
// An interface whose name maps to `exports` (mandrillapp and zuora both expose an `Exports`
// tag) collides with the synthetic top-level `exports` module wit-bindgen generates for
// exported interfaces. The generated Rust must alias the interface module to a non-colliding
// local name rather than `use ...::exports;` (which is E0255 "defined multiple times").
#[test]
fn exports_tag_module_is_aliased() {
    let spec_json = r#"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/exports": {
          "get": {
            "tags": ["exports"],
            "operationId": "listExports",
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"#;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(generated.interfaces.iter().any(|i| i == "exports"));
    // The module import is aliased, and the `Guest` impl targets the alias — never a bare
    // `exports` that would shadow wit-bindgen's top-level `exports` module.
    assert!(
        generated.rust.contains("::exports as iface_exports;"),
        "exports interface module should be aliased:\n{}",
        generated.rust
    );
    assert!(generated.rust.contains("impl iface_exports::Guest"));
    assert!(
        !generated.rust.contains("::demo::exports;"),
        "must not emit a bare `exports` import that collides with wit-bindgen's module"
    );
}

// r[verify codegen.enum.escape-wire-value]
// Enum wire values are emitted into a Rust `&str` literal. A value containing characters that
// are special in a Rust literal — Telnyx ships enum values *with embedded double quotes* like
// `"Ashburn, VA"` — must be escaped, or the glue emits `""Ashburn, VA""` (a reserved-prefix
// lex error: "prefix `VA` is unknown").
#[test]
fn escapes_enum_wire_values_with_quotes() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/sites": {
          "get": {
            "tags": ["sites"],
            "operationId": "listSites",
            "parameters": [
              {
                "name": "region",
                "in": "query",
                "schema": { "type": "string", "enum": ["\"Ashburn, VA\"", "Latency"] }
              }
            ],
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The escaped literal is present; the doubled-quote form that fails to lex is not.
    assert!(
        generated.rust.contains(r#"=> "\"Ashburn, VA\"","#),
        "enum wire value should be escaped:\n{}",
        generated.rust
    );
    assert!(
        !generated.rust.contains(r#"=> ""Ashburn"#),
        "must not emit the doubled-quote form that fails to lex"
    );
}
