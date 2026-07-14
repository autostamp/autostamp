//! Integration tests for `openapi-bindgen`.

use openapi_bindgen::{Generated, NoOperations, PackageName, generate, parse_openapi};

/// Join every generated Rust source file into one string. The generator splits its output
/// across a `lib.rs` crate root plus one `iface_*.rs` module per interface; assertions that
/// only care about the emitted code search the concatenation.
fn rust_src(generated: &Generated) -> String {
    generated
        .rust
        .iter()
        .map(|f| f.contents.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

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
fn strips_redundant_interface_prefix_from_operation_names() {
    // GitHub's operationIds repeat the tag (`migrations/list-for-org` under tag `migrations`),
    // so the naive lowering produced `migrations-list-for-org` inside `interface migrations` —
    // the enclosing scope stuttered into every function, params record, and param-derived enum.
    // The prefix must be stripped so members read `list-for-org` / `list-for-org-params` /
    // `list-for-org-exclude-item-enum`.
    let spec_json = r#"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/orgs/{org}/migrations": {
          "get": {
            "tags": ["migrations"],
            "operationId": "migrations/list-for-org",
            "parameters": [
              { "name": "org", "in": "path", "required": true, "schema": { "type": "string" } },
              { "name": "exclude", "in": "query", "schema": {
                  "type": "array", "items": { "type": "string", "enum": ["repositories"] } } }
            ],
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"#;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("autostamp:github@0.1.0").unwrap();
    // `generate` validates the WIT internally, so an Ok result also proves the stripped names
    // stayed valid and unique.
    let generated = generate(&spec, &package, None).unwrap();

    assert!(generated.interfaces.iter().any(|i| i == "migrations"));

    // The de-duplicated names are present ...
    assert!(
        generated.wit.contains("list-for-org: func("),
        "function should drop the interface prefix:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("record list-for-org-params"),
        "params record should drop the interface prefix:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("list-for-org-exclude-item-enum"),
        "param-derived enum should drop the interface prefix:\n{}",
        generated.wit
    );

    // ... and the stuttering forms are gone entirely.
    assert!(
        !generated.wit.contains("migrations-list-for-org"),
        "redundant `migrations-` prefix should not appear:\n{}",
        generated.wit
    );
}

#[test]
fn strips_redundant_interface_prefix_from_schema_types() {
    // Named `#/components/schemas` types stutter the tag too: GitHub's `migrations-*` schemas
    // under `interface migrations` should surface as bare types (`settings`, not
    // `migrations-settings`). A nested `$ref` proves a *standalone* record — not just an
    // operation-derived one — drops the prefix, so both types and functions are de-duplicated.
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/orgs/{org}/migrations": {
          "post": {
            "tags": ["migrations"],
            "operationId": "migrations/start-for-org",
            "requestBody": {
              "required": true,
              "content": { "application/json": {
                "schema": { "$ref": "#/components/schemas/migrations-start-request" } } }
            },
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "schemas": {
          "migrations-start-request": {
            "type": "object",
            "required": ["lock"],
            "properties": {
              "lock": { "type": "boolean" },
              "settings": { "$ref": "#/components/schemas/migrations-settings" }
            }
          },
          "migrations-settings": {
            "type": "object",
            "properties": { "exclude-attachments": { "type": "boolean" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("autostamp:github@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The nested `migrations-settings` schema surfaces as a standalone `record settings`,
    // referenced from the params record by its stripped name.
    assert!(
        generated.wit.contains("record settings {"),
        "schema-derived type should drop the interface prefix:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("settings: option<settings>"),
        "reference to the stripped type should use the stripped name:\n{}",
        generated.wit
    );
    // No `migrations-`-prefixed type or function survives.
    assert!(
        !generated.wit.contains("migrations-"),
        "no member should stutter the interface name:\n{}",
        generated.wit
    );
}

#[test]
fn disambiguates_schema_types_that_collide_after_prefix_stripping() {
    // Stripping the interface prefix can collapse two distinct schemas onto one tail:
    // `migrations-config` and `config` both want to become `config` under `interface
    // migrations`. They must stay separate records (not silently merge), or a caller's data
    // gets the wrong shape.
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/orgs/{org}/migrations": {
          "post": {
            "tags": ["migrations"],
            "operationId": "migrations/start",
            "requestBody": {
              "required": true,
              "content": { "application/json": {
                "schema": { "$ref": "#/components/schemas/migrations-wrapper" } } }
            },
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "schemas": {
          "migrations-wrapper": {
            "type": "object",
            "properties": {
              "a": { "$ref": "#/components/schemas/migrations-config" },
              "b": { "$ref": "#/components/schemas/config" }
            }
          },
          "migrations-config": { "type": "object", "properties": { "x": { "type": "boolean" } } },
          "config": { "type": "object", "properties": { "y": { "type": "string" } } }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("autostamp:github@0.1.0").unwrap();
    // A successful `generate` already proves the WIT validated — i.e. the two records did not
    // collide into one "defined more than once" error.
    let generated = generate(&spec, &package, None).unwrap();

    // Both distinct shapes survive under distinct names, and the wrapper points at each.
    assert!(
        generated.wit.contains("record config {"),
        "{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("record config-v2 {"),
        "colliding schema should be disambiguated, not merged:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("a: option<config>"),
        "{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("b: option<config-v2>"),
        "the second schema keeps its own identity:\n{}",
        generated.wit
    );
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
fn emits_world_and_wasm_toml_importing_only_secrets() {
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

    // The wasm.toml carries a publishable [package] section plus the single interface dep the
    // build vendors: `wasmcloud:secrets`. The built component also imports `wasi:http`, but
    // `wstd` supplies it whole, so — like `wasi:cli`/`clocks`/`io`/`random` — it is not declared.
    assert!(generated.wasm_toml.contains("[package]"));
    assert!(
        generated
            .wasm_toml
            .contains("registry = \"ghcr.io/widget/api\"")
    );
    assert!(!generated.wasm_toml.contains("wasi:http"));
    assert!(generated.wasm_toml.contains(
        "\"wasmcloud:secrets\" = { registry = \"ghcr.io\", namespace = \"wasmcloud\", package = \"interfaces/wasmcloud/secrets\", version = \"1.0.0\" }"
    ));
}

#[test]
fn emits_base_url_and_auth_tables() {
    let spec = parse_openapi(AUTH_SPEC).unwrap();
    let package = PackageName::parse("widget:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();
    let rust = rust_src(&generated);

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
        rust_src(&generated).contains(
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

/// Mirrors the `ip2location`/`ip2whois` shape: a required query param named `key` (described as
/// an API key) with no `securitySchemes` and no `security`. Also carries an unrelated required
/// input (`ip`) and an optional pagination `token` that must NOT be mistaken for a credential.
const NAKED_KEY_SPEC: &str = r#"{
  "openapi": "3.0.0",
  "info": { "title": "geo", "version": "1.0.0" },
  "servers": [{ "url": "https://api.example.com/v2" }],
  "paths": {
    "/": {
      "get": {
        "tags": ["geo"],
        "operationId": "lookup",
        "parameters": [
          { "name": "ip", "in": "query", "required": true, "schema": { "type": "string" } },
          { "name": "key", "in": "query", "required": true, "description": "API Key. Please sign up free trial license key.", "schema": { "type": "string" } },
          { "name": "token", "in": "query", "required": false, "description": "Pagination token for the next page.", "schema": { "type": "string" } }
        ],
        "responses": { "200": { "description": "ok" } }
      }
    }
  }
}"#;

#[test]
fn infers_api_key_credential_from_naked_param() {
    let spec = parse_openapi(NAKED_KEY_SPEC).unwrap();
    let package = PackageName::parse("geo:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The naked `key` param is lifted off the operation surface: the params record keeps the
    // real input `ip` and the optional `token` (not an API key), but drops `key`.
    let wit = &generated.wit;
    let start = wit
        .find("record lookup-params")
        .expect("params record present");
    let record = &wit[start..start + wit[start..].find('}').expect("record closes")];
    assert!(record.contains("ip"), "unrelated required input retained");
    assert!(
        record.contains("token"),
        "optional pagination token retained, not treated as a credential:\n{record}"
    );
    assert!(
        !record.contains("key"),
        "inferred API-key param lifted out of the params record:\n{record}"
    );

    // It is injected centrally instead: the op carries a synthesized query-key auth entry, keyed
    // by the param's kebab name, applying the secret to the `key` query param verbatim.
    assert!(
        rust_src(&generated)
            .contains(r#"AuthApply { secret_key: "key", kind: AuthKind::ApiKeyQuery("key") }"#),
        "synthesized auth entry present:\n{}",
        rust_src(&generated)
    );

    // The inferred secret is documented in the Authentication table in both surfaces …
    assert!(generated.readme.contains("| `key` | query `key` |"));
    assert!(generated.wit.contains("/// | `key` | query `key` |"));

    // … and the Generator Diagnostics row reports the heuristic fired AND names the secret.
    assert!(
        generated.readme.contains(
            "| Infer API-key credentials | enabled — **triggered**, inferred 1 secret: `key` |"
        ),
        "diagnostics names the inferred secret:\n{}",
        generated.readme
    );

    // A spec with a real scheme does not infer anything: the heuristic reports not-triggered.
    let declared = parse_openapi(AUTH_SPEC).unwrap();
    let declared_pkg = PackageName::parse("widget:api@0.1.0").unwrap();
    let declared = generate(&declared, &declared_pkg, None).unwrap();
    assert!(
        declared
            .readme
            .contains("| Infer API-key credentials | enabled — not triggered |"),
        "declared-scheme spec must not infer credentials"
    );

    // A spec with no params at all also reports not-triggered.
    let plain = parse_openapi(MINIMAL_SPEC).unwrap();
    let plain_pkg = PackageName::parse("demo:things@0.1.0").unwrap();
    let plain = generate(&plain, &plain_pkg, None).unwrap();
    assert!(
        plain
            .readme
            .contains("| Infer API-key credentials | enabled — not triggered |")
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

// r[verify codegen.request-body.non-json-media]
// Many production specs never use `application/json` for request bodies: Stripe and Twilio
// model every write as `application/x-www-form-urlencoded`. The old code read only
// `content["application/json"]`, so those operations emitted a param-less function and their
// entire body silently vanished. The generator must fall back to a non-JSON media type and
// inline its schema's fields exactly as it would for JSON.
#[test]
fn handles_non_json_request_body() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/account_links": {
          "post": {
            "tags": ["account"],
            "operationId": "createAccountLink",
            "requestBody": {
              "required": true,
              "content": {
                "application/x-www-form-urlencoded": {
                  "schema": {
                    "type": "object",
                    "required": ["account"],
                    "properties": {
                      "account": { "type": "string" },
                      "return_url": { "type": "string" }
                    }
                  }
                }
              }
            },
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The form-urlencoded body's fields are inlined into the params record, just like JSON:
    // the required field is non-optional, the optional field is `option<...>`.
    assert!(
        generated.wit.contains("account: string"),
        "form-urlencoded body's required field should be inlined:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("return-url: option<string>"),
        "form-urlencoded body's optional field should be inlined:\n{}",
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
        rust_src(&generated).contains("::exports as iface_exports;"),
        "exports interface module should be aliased:\n{}",
        rust_src(&generated)
    );
    assert!(rust_src(&generated).contains("impl iface_exports::Guest"));
    assert!(
        !rust_src(&generated).contains("::demo::exports;"),
        "must not emit a bare `exports` import that collides with wit-bindgen's module"
    );
}

// r[verify codegen.output.multi-file]
// The generator splits its Rust output across files — a `lib.rs` crate root plus one
// `iface_<name>.rs` module per interface — so no single file grows too large for `rustc` to
// parse. The crate-level scaffolding stays in `lib.rs`; each interface's `Guest` impl lives in
// its own module file.
#[test]
fn splits_rust_into_lib_and_interface_modules() {
    let spec = parse_openapi(MINIMAL_SPEC).unwrap();
    let package = PackageName::parse("demo:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // `lib.rs` is always the first emitted file.
    let lib = &generated.rust[0];
    assert_eq!(lib.path, "lib.rs");

    // It carries the shared crate scaffolding …
    for needle in [
        "#![allow(",
        "wit_bindgen::generate!",
        "const BASE_URL",
        "struct Component;",
        "mod runtime {",
        "mod iface_things;",
        "export!(Component);",
    ] {
        assert!(
            lib.contents.contains(needle),
            "lib.rs should contain `{needle}`:\n{}",
            lib.contents
        );
    }
    // … but the interface's `Guest` impl lives in its own module, not the crate root.
    assert!(
        !lib.contents.contains("impl iface_things::Guest"),
        "the Guest impl must not be in lib.rs:\n{}",
        lib.contents
    );

    // The `things` interface has its own `iface_things.rs` module carrying its `Guest` impl and
    // the imports it needs to compile standalone.
    let module = generated
        .rust
        .iter()
        .find(|f| f.path == "iface_things.rs")
        .expect("iface_things.rs module should be emitted");
    for needle in [
        "impl iface_things::Guest for crate::Component",
        "use crate::runtime::{dispatch,",
        "use serde_json::{Map, Value};",
        "as iface_things;",
    ] {
        assert!(
            module.contents.contains(needle),
            "iface_things.rs should contain `{needle}`:\n{}",
            module.contents
        );
    }
}

// r[verify codegen.output.one-module-per-interface]
// Every interface is emitted as its own `iface_<name>.rs` module, and `lib.rs` declares each
// with a `mod` item so they all compile into the crate.
#[test]
fn emits_one_module_file_per_interface() {
    let spec_json = r#"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/things": {
          "get": { "tags": ["things"], "operationId": "listThings",
            "responses": { "200": { "description": "ok" } } }
        },
        "/widgets": {
          "get": { "tags": ["widgets"], "operationId": "listWidgets",
            "responses": { "200": { "description": "ok" } } }
        }
      }
    }"#;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("demo:api@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // lib.rs plus one module per interface.
    assert_eq!(generated.interfaces.len(), 2);
    assert_eq!(generated.rust.len(), generated.interfaces.len() + 1);
    assert_eq!(generated.rust[0].path, "lib.rs");

    for iface in &generated.interfaces {
        let module_path = format!("iface_{iface}.rs");
        assert!(
            generated.rust.iter().any(|f| f.path == module_path),
            "expected a module file `{module_path}`; got {:?}",
            generated.rust.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert!(
            generated.rust[0]
                .contents
                .contains(&format!("mod iface_{iface};")),
            "lib.rs should declare `mod iface_{iface};`:\n{}",
            generated.rust[0].contents
        );
    }

    // Every interface module targets the shared crate-level `Component`.
    for module in generated.rust.iter().skip(1) {
        assert!(module.contents.contains("for crate::Component"));
    }
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
        rust_src(&generated).contains(r#"=> "\"Ashburn, VA\"","#),
        "enum wire value should be escaped:\n{}",
        rust_src(&generated)
    );
    assert!(
        !rust_src(&generated).contains(r#"=> ""Ashburn"#),
        "must not emit the doubled-quote form that fails to lex"
    );
}

// r[verify codegen.params.resolve-component-ref]
// A `$ref` to `#/components/parameters/*` must resolve to the shared parameter rather than
// being dropped (`ReferenceOr::Reference { .. } => continue`). The field surfaces on the params
// record, and a shared *path* parameter fills its `{placeholder}` in the URL template —
// otherwise the request path carries a literal `{owner}`/`{repo}` no field can substitute
// (GitHub `$ref`s `#/components/parameters/*` over 2000 times, incl. `owner`/`repo`).
#[test]
fn resolves_component_parameter_refs() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/repos/{owner}/{repo}": {
          "get": {
            "tags": ["repos"],
            "operationId": "repos/get",
            "parameters": [
              { "$ref": "#/components/parameters/owner" },
              { "$ref": "#/components/parameters/repo" },
              { "$ref": "#/components/parameters/per-page" }
            ],
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "parameters": {
          "owner": { "name": "owner", "in": "path", "required": true, "schema": { "type": "string" } },
          "repo": { "name": "repo", "in": "path", "required": true, "schema": { "type": "string" } },
          "per-page": { "name": "per_page", "in": "query", "required": false, "schema": { "type": "integer" } }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // All three shared params surface on the operation's params record.
    assert!(generated.wit.contains("owner: string"), "{}", generated.wit);
    assert!(generated.wit.contains("repo: string"), "{}", generated.wit);
    assert!(
        generated.wit.contains("per-page: option<s32>"),
        "resolved query param should be typed and optional:\n{}",
        generated.wit
    );
    // The path placeholders have matching `FieldLocation::Path` fields in the runtime OpSpec, so
    // `{owner}`/`{repo}` are fillable rather than emitted verbatim into the request URL.
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"owner\", wire: \"owner\", location: FieldLocation::Path }"
        ),
        "{}",
        rust_src(&generated)
    );
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"repo\", wire: \"repo\", location: FieldLocation::Path }"
        ),
        "{}",
        rust_src(&generated)
    );
}

// r[verify codegen.params.path-item-level]
// Parameters declared at the *path-item* level apply to every operation of that path (Kubernetes
// declares them on 372 of 428 paths). They must be merged into each operation, and an
// operation-level parameter overrides a path-level one with the same (name, location) — never
// duplicated.
#[test]
fn merges_path_item_level_parameters() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/things/{id}": {
          "parameters": [
            { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
            { "name": "trace", "in": "query", "required": false, "schema": { "type": "string" } }
          ],
          "get": {
            "tags": ["things"],
            "operationId": "getThing",
            "responses": { "200": { "description": "ok" } }
          },
          "delete": {
            "tags": ["things"],
            "operationId": "deleteThing",
            "parameters": [
              { "name": "trace", "in": "query", "required": true, "schema": { "type": "string" } }
            ],
            "responses": { "204": { "description": "gone" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The shared path-item `id` reaches both operations and fills `{id}` for each.
    let id_path_fields = rust_src(&generated)
        .matches("FieldSpec { snake: \"id\", wire: \"id\", location: FieldLocation::Path }")
        .count();
    assert_eq!(
        id_path_fields,
        2,
        "both operations should carry the shared path-item `id` field:\n{}",
        rust_src(&generated)
    );
    // `trace` appears once per operation (2 total): the delete op's operation-level `trace`
    // overrides — rather than duplicates — the path-level one.
    let trace_fields = rust_src(&generated).matches("snake: \"trace\"").count();
    assert_eq!(
        trace_fields,
        2,
        "operation-level param must override, not duplicate, the path-level param:\n{}",
        rust_src(&generated)
    );
    // getThing keeps the path-level optional `trace`; deleteThing's override is required.
    assert!(
        generated.wit.contains("trace: option<string>"),
        "path-level trace is optional on getThing:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("trace: string"),
        "overriding trace is required on deleteThing:\n{}",
        generated.wit
    );
}

// r[verify codegen.params.same-name-distinct-location]
// OpenAPI keys parameter uniqueness on the (name, `in`) pair, so an operation may legitimately
// carry a path parameter and a query parameter that share a name — Kubernetes' proxy endpoints
// expose the `{path}` URL segment *and* a `path` query string on the same operation. The old
// dedup collapsed them by WIT name and silently dropped the query parameter (and, for other
// specs, request-body fields that restate a parameter). Both distinct inputs must survive: the
// path field to fill `{path}`, the query field to carry the query string.
#[test]
fn keeps_same_named_path_and_query_parameters() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/nodes/{name}/proxy/{path}": {
          "get": {
            "tags": ["proxy"],
            "operationId": "proxyNode",
            "parameters": [
              { "name": "name", "in": "path", "required": true, "schema": { "type": "string" } },
              { "name": "path", "in": "path", "required": true, "schema": { "type": "string" } },
              { "name": "path", "in": "query", "required": false, "schema": { "type": "string" } }
            ],
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The path `{path}` segment is still fillable: a path-location field named `path` survives.
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"path\", wire: \"path\", location: FieldLocation::Path }"
        ),
        "the path `{{path}}` segment must keep its path field:\n{}",
        rust_src(&generated)
    );
    // The distinct query parameter `path` is not dropped: it survives as a query-location field.
    // Its internal params key (`snake`) is disambiguated to keep the params record unambiguous,
    // but its wire name stays `path`, so the runtime still sends `?path=…`.
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"path_v2\", wire: \"path\", location: FieldLocation::Query }"
        ),
        "the same-named query parameter must be carried, not dropped:\n{}",
        rust_src(&generated)
    );
    // Nothing is left unfillable: the params record exposes two distinct fields for the two
    // `path` inputs (`path` and a suffix-disambiguated sibling).
    assert!(
        generated.wit.contains("path: string") && generated.wit.contains("path-v2: option<string>"),
        "both `path` inputs should surface as distinct WIT fields:\n{}",
        generated.wit
    );
}

// r[verify codegen.params.wire-name-fidelity]
// The name a field takes on the HTTP wire must be the source name verbatim, not the snake_cased
// WIT/plumbing name. The runtime keys query strings, headers and body properties off `FieldSpec.
// wire`; keying off the internal snake key would corrupt every API whose parameters aren't already
// snake_case — Twilio's `PhoneNumber`/`DateCreated`, Kubernetes' `dryRun`, etc. — and would also
// collapse distinct inputs that happen to share a snake key. This checks that (a) a camelCase
// query parameter keeps its wire name while carrying a snake_case params key, and (b) Twilio's
// `DateCreated` / `DateCreated<` range filters survive as two fields with distinct wire names.
#[test]
fn preserves_verbatim_wire_names() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/messages": {
          "get": {
            "tags": ["msg"],
            "operationId": "listMessages",
            "parameters": [
              { "name": "phoneNumber", "in": "query", "required": false, "schema": { "type": "string" } },
              { "name": "DateCreated", "in": "query", "required": false, "schema": { "type": "string" } },
              { "name": "DateCreated<", "in": "query", "required": false, "schema": { "type": "string" } }
            ],
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // A camelCase parameter travels under its verbatim wire name, keyed internally by snake_case.
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"phone_number\", wire: \"phoneNumber\", location: FieldLocation::Query }"
        ),
        "camelCase query parameter must keep its verbatim wire name:\n{}",
        rust_src(&generated)
    );
    // The params record the guest serializes keys by the internal snake name, not the wire name;
    // the runtime re-keys to `wire` during dispatch.
    assert!(
        rust_src(&generated).contains("m.insert(\"phone_number\".into()"),
        "params record should key by the internal snake name:\n{}",
        rust_src(&generated)
    );
    // `DateCreated` and `DateCreated<` are distinct inputs: both survive, each under its own wire
    // name, with disambiguated internal snake keys so they can't clobber each other.
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"date_created\", wire: \"DateCreated\", location: FieldLocation::Query }"
        ),
        "first DateCreated filter must keep its wire name:\n{}",
        rust_src(&generated)
    );
    assert!(
        rust_src(&generated).contains(
            "FieldSpec { snake: \"date_created_v2\", wire: \"DateCreated<\", location: FieldLocation::Query }"
        ),
        "the `DateCreated<` range filter must survive with its distinct wire name:\n{}",
        rust_src(&generated)
    );
}

// r[verify codegen.schema.all-of-merge]
// A request body defined by `allOf` composition (the common `[{$ref: Base}, {inline extension}]`
// shape) must merge its members' properties into one record. The old mapping degraded any
// `allOf` to an opaque `string`, dropping every field — DigitalOcean models many request bodies
// this way. `oneOf`/`anyOf` stay opaque `string` (a deliberate, documented choice), so the same
// spec confirms an `anyOf` field does *not* become a record.
#[test]
fn merges_all_of_body_into_record() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/widgets": {
          "post": {
            "tags": ["widgets"],
            "operationId": "createWidget",
            "requestBody": {
              "required": true,
              "content": {
                "application/json": {
                  "schema": {
                    "allOf": [
                      { "$ref": "#/components/schemas/Base" },
                      {
                        "type": "object",
                        "required": ["color"],
                        "properties": {
                          "color": { "type": "string" },
                          "flavor": {
                            "anyOf": [
                              { "type": "string" },
                              { "type": "integer" }
                            ]
                          }
                        }
                      }
                    ]
                  }
                }
              }
            },
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "schemas": {
          "Base": {
            "type": "object",
            "required": ["id"],
            "properties": {
              "id": { "type": "string" },
              "name": { "type": "string" }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // Fields from the `$ref` base member survive (required stays non-optional, optional stays
    // `option<...>`) ...
    assert!(
        generated.wit.contains("id: string"),
        "allOf base member's required field must be inlined:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("name: option<string>"),
        "allOf base member's optional field must be inlined:\n{}",
        generated.wit
    );
    // ... alongside the inline extension member's fields, in one merged record.
    assert!(
        generated.wit.contains("color: string"),
        "allOf inline member's required field must be inlined:\n{}",
        generated.wit
    );
    // The nested `anyOf` field is present but stays an opaque `string` (deliberate choice), not a
    // record — so it must not have degraded the whole `allOf` to a string.
    assert!(
        generated.wit.contains("flavor: option<string>"),
        "anyOf field should remain an opaque optional string within the merged record:\n{}",
        generated.wit
    );
}

// r[verify codegen.parameter.shared-ref]
// GitHub's API (and many others) declare their operation parameters once under
// `#/components/parameters` and reference them by `$ref` from every operation (`owner`, `repo`,
// `username`, `per-page`, ...). The generator must resolve those `$ref`s into fields; the
// original code skipped reference parameters outright, so e.g. "list repositories for a user"
// generated with an empty argument list instead of taking the user to list for.
#[test]
fn resolves_shared_ref_parameters() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/users/{username}/repos": {
          "get": {
            "tags": ["repos"],
            "operationId": "listForUser",
            "parameters": [
              { "$ref": "#/components/parameters/username" },
              { "$ref": "#/components/parameters/per-page" }
            ],
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "parameters": {
          "username": {
            "name": "username",
            "in": "path",
            "required": true,
            "schema": { "type": "string" }
          },
          "per-page": {
            "name": "per_page",
            "in": "query",
            "required": false,
            "schema": { "type": "integer" }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(generated.interfaces.iter().any(|i| i == "repos"));
    // The `$ref` path parameter resolves into a required field ...
    assert!(
        generated.wit.contains("username: string"),
        "shared `$ref` path parameter should resolve into a field:\n{}",
        generated.wit
    );
    // ... and the `$ref` query parameter resolves into an optional field.
    assert!(
        generated.wit.contains("per-page: option<"),
        "shared `$ref` query parameter should resolve into a field:\n{}",
        generated.wit
    );
}

// r[verify codegen.response.success-body-typed]
// Issue #6: a success (2xx) response body with a JSON schema must be lowered to a typed WIT
// value so the operation returns `result<{ok}, ...>` instead of the old `result<string, ...>`.
// An object schema referenced by `$ref` becomes a named record, and the operation's `ok` arm
// is that record — not a raw string.
#[test]
fn types_success_response_body_as_record() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets/{id}": {
          "get": {
            "tags": ["pets"],
            "operationId": "getPet",
            "parameters": [
              { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }
            ],
            "responses": {
              "200": {
                "description": "ok",
                "content": {
                  "application/json": { "schema": { "$ref": "#/components/schemas/Pet" } }
                }
              }
            }
          }
        }
      },
      "components": {
        "schemas": {
          "Pet": {
            "type": "object",
            "required": ["name"],
            "properties": {
              "name": { "type": "string" },
              "age": { "type": "integer", "format": "int32" }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The 2xx schema becomes a named record ...
    assert!(
        generated.wit.contains("record pet {"),
        "success body schema should be lowered to a record:\n{}",
        generated.wit
    );
    // ... and the operation returns it on the `ok` arm (no declared errors -> `string` err).
    assert!(
        generated.wit.contains("-> result<pet, string>"),
        "operation should return the typed success record:\n{}",
        generated.wit
    );
    // The generated Rust deserializes the raw body into the record type.
    assert!(
        rust_src(&generated).contains("__ok(body: String)"),
        "a typed ok body should emit a `__ok` decoder over the raw body:\n{}",
        rust_src(&generated)
    );
}

// r[verify codegen.response.success-body-list]
// A 2xx body whose schema is an array of a `$ref` becomes `list<{item}>`, mirroring how request
// bodies and parameters lower arrays.
#[test]
fn types_success_response_body_as_list() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets": {
          "get": {
            "tags": ["pets"],
            "operationId": "listPets",
            "responses": {
              "200": {
                "description": "ok",
                "content": {
                  "application/json": {
                    "schema": { "type": "array", "items": { "$ref": "#/components/schemas/Pet" } }
                  }
                }
              }
            }
          }
        }
      },
      "components": {
        "schemas": {
          "Pet": {
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "type": "string" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated.wit.contains("-> result<list<pet>, string>"),
        "array success body should lower to `list<...>`:\n{}",
        generated.wit
    );
}

// r[verify codegen.named-array-schema]
// A *named* schema that is itself `type: array` (e.g. CircleCI's `Builds: { type:
// array, items: $ref Build }`, referenced from a response as `$ref: "#/components/
// schemas/Builds"` rather than declared inline) used to fall into
// `emit_record_from_schema`'s scalar fallback, which only special-cases object-shaped
// schemas and degrades everything else — including arrays — to an opaque
// `{ value: string }` record. That silently discarded the item type. It must resolve
// to `list<pet>` exactly like the inline-array case above, with the `$ref`ed item
// schema resolved recursively.
#[test]
fn types_success_response_body_as_list_via_named_array_schema() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets": {
          "get": {
            "tags": ["pets"],
            "operationId": "listPets",
            "responses": {
              "200": {
                "description": "ok",
                "content": {
                  "application/json": {
                    "schema": { "$ref": "#/components/schemas/Pets" }
                  }
                }
              }
            }
          }
        }
      },
      "components": {
        "schemas": {
          "Pets": { "type": "array", "items": { "$ref": "#/components/schemas/Pet" } },
          "Pet": {
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "type": "string" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated.wit.contains("-> result<list<pet>, string>"),
        "named array schema should lower to `list<...>`, not degrade to an opaque record:\n{}",
        generated.wit
    );
    assert!(
        !generated.wit.contains("record pets"),
        "named array schema must not emit an opaque wrapper record:\n{}",
        generated.wit
    );
}

// r[verify codegen.response.errors-enumerated]
// Issue #7: declared non-2xx responses must be enumerated into a per-operation error `variant`,
// one case per status (named from the standard reason phrase), plus a trailing `other(string)`
// catch-all. The operation's `err` arm is that variant instead of a flat `string`.
#[test]
fn enumerates_declared_error_responses() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets/{id}": {
          "get": {
            "tags": ["pets"],
            "operationId": "getPet",
            "parameters": [
              { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }
            ],
            "responses": {
              "200": { "description": "ok" },
              "404": { "description": "missing" },
              "500": { "description": "boom" }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // A per-operation error variant with a case per declared status plus a catch-all.
    assert!(
        generated.wit.contains("variant get-pet-error {"),
        "declared errors should become a per-op variant:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("not-found(string)"),
        "404 should map to a `not-found` case:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("internal-server-error(string)"),
        "500 should map to an `internal-server-error` case:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("other(string)"),
        "an `other` catch-all case is always present:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("-> result<string, get-pet-error>"),
        "the operation's err arm should be the variant:\n{}",
        generated.wit
    );
    // The generated Rust maps each HTTP status onto its case, with a catch-all for the rest.
    assert!(
        rust_src(&generated).contains("404u16 =>")
            && rust_src(&generated).contains("::NotFound(body)"),
        "404 should map onto the NotFound case in Rust:\n{}",
        rust_src(&generated)
    );
    assert!(
        rust_src(&generated).contains("500u16 =>")
            && rust_src(&generated).contains("::InternalServerError(body)"),
        "500 should map onto the InternalServerError case in Rust:\n{}",
        rust_src(&generated)
    );
    assert!(
        rust_src(&generated).contains("_ =>") && rust_src(&generated).contains("::Other(body)"),
        "undeclared statuses fall through to the Other case:\n{}",
        rust_src(&generated)
    );
}

// r[verify codegen.response.errors-range]
// A non-2xx *range* response (`4XX`, `5XX`) becomes a single variant case keyed by the range,
// and the Rust maps the whole 100-code window onto it via a guarded match arm.
#[test]
fn maps_error_status_ranges_to_variant_cases() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets": {
          "get": {
            "tags": ["pets"],
            "operationId": "listPets",
            "responses": {
              "200": { "description": "ok" },
              "4XX": { "description": "client error" },
              "5XX": { "description": "server error" }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated.wit.contains("client-error(string)")
            && generated.wit.contains("server-error(string)"),
        "status ranges should map to `client-error`/`server-error` cases:\n{}",
        generated.wit
    );
    assert!(
        rust_src(&generated).contains("(400u16..500u16).contains(&s)"),
        "a 4XX range should map onto a guarded 400..500 match arm:\n{}",
        rust_src(&generated)
    );
    assert!(
        rust_src(&generated).contains("(500u16..600u16).contains(&s)"),
        "a 5XX range should map onto a guarded 500..600 match arm:\n{}",
        rust_src(&generated)
    );
}

// r[verify codegen.response.no-errors-string]
// An operation that declares no *specific* non-2xx response keeps a flat `string` error arm
// (its `result` stays `result<ok, string>`): a single-case variant would carry no more
// information than the string it wraps. `default` alone does not create a variant.
#[test]
fn keeps_string_error_without_declared_error_responses() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets": {
          "get": {
            "tags": ["pets"],
            "operationId": "listPets",
            "responses": {
              "200": { "description": "ok" },
              "default": { "description": "fallback" }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        !generated.wit.contains("-error {"),
        "no specific error responses should emit no error variant:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("-> result<string, string>"),
        "with no typed body or declared errors the result stays `result<string, string>`:\n{}",
        generated.wit
    );
}

// r[verify codegen.response.ref-resolved]
// Issue #5: a response declared via `#/components/responses/*` `$ref` must be resolved so its
// body schema is typed exactly as an inline response would be — the response half of `$ref`
// codegen that the parameter/request-body work already covers.
#[test]
fn resolves_component_response_ref() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/pets/{id}": {
          "get": {
            "tags": ["pets"],
            "operationId": "getPet",
            "parameters": [
              { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }
            ],
            "responses": {
              "200": { "$ref": "#/components/responses/PetResponse" }
            }
          }
        }
      },
      "components": {
        "responses": {
          "PetResponse": {
            "description": "a pet",
            "content": {
              "application/json": { "schema": { "$ref": "#/components/schemas/Pet" } }
            }
          }
        },
        "schemas": {
          "Pet": {
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "type": "string" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    // The shared response `$ref` resolves and its body schema is typed like an inline one.
    assert!(
        generated.wit.contains("record pet {"),
        "shared response `$ref` should resolve and type its body schema:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("-> result<pet, string>"),
        "operation should return the resolved response's typed body:\n{}",
        generated.wit
    );
}

// r[verify codegen.response.no-schema-string]
// A success response with no content or no schema (`204 No Content`, a bare `description`)
// keeps `string` on the `ok` arm: the raw response body, preserving the original
// always-return-the-body behavior so an unmodelable response never regresses an operation.
#[test]
fn keeps_string_success_without_response_schema() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/ping": {
          "get": {
            "tags": ["health"],
            "operationId": "ping",
            "responses": { "200": { "description": "ok" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated.wit.contains("-> result<string, string>"),
        "a response without a schema keeps the raw-body `string` ok arm:\n{}",
        generated.wit
    );
}

// r[verify codegen.response.self-referential-allof]
// A single-member `allOf` that wraps a `$ref` back to its own enclosing schema (Jira's
// `NotificationEvent.templateEvent = allOf[$ref NotificationEvent]`) must not send the record
// emitter into unbounded recursion. An inline single-member `allOf` never enters the `$ref`
// cycle guard directly, so it is delegated to its sole member; when that member closes a cycle
// it degrades to `string` exactly as a direct self-`$ref` already does. Reachable through a
// response body (the schema is only ever returned) — the path that first exposed the hang.
#[test]
fn does_not_recurse_on_self_referential_single_member_allof() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/events/{id}": {
          "get": {
            "tags": ["events"],
            "operationId": "getEvent",
            "parameters": [
              { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }
            ],
            "responses": {
              "200": {
                "description": "an event",
                "content": {
                  "application/json": { "schema": { "$ref": "#/components/schemas/NotificationEvent" } }
                }
              }
            }
          }
        }
      },
      "components": {
        "schemas": {
          "NotificationEvent": {
            "type": "object",
            "properties": {
              "eventType": { "type": "string" },
              "templateEvent": { "allOf": [ { "$ref": "#/components/schemas/NotificationEvent" } ] }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    // The real assertion is simply that this returns (does not hang / overflow the stack).
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated.wit.contains("record notification-event {"),
        "the self-referential schema should still emit one bounded record:\n{}",
        generated.wit
    );
    // The cyclic self-reference is broken by degrading it to `string`, never a recursive record.
    assert!(
        generated.wit.contains("template-event: option<string>"),
        "the single-member allOf self-`$ref` should degrade to `string`, not recurse:\n{}",
        generated.wit
    );
}

// r[verify codegen.response.op-name-collision]
// A success response whose body is a schema named after the operation (Plaid's
// `transferIntentCreate` returns `TransferIntentCreateResponse`, whose `transfer_intent` field
// is `$ref TransferIntentCreate` -> record `transfer-intent-create`) must not let the response
// record claim the operation's own function name. WIT shares one namespace for functions and
// types, so the operation reserves its name before responses are lowered; the colliding record
// is disambiguated with a numeric suffix instead of producing a "defined more than once" error.
#[test]
fn response_record_named_after_operation_does_not_collide() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/intents": {
          "post": {
            "tags": ["intents"],
            "operationId": "createIntent",
            "responses": {
              "200": {
                "description": "created",
                "content": {
                  "application/json": { "schema": { "$ref": "#/components/schemas/CreateIntentResponse" } }
                }
              }
            }
          }
        }
      },
      "components": {
        "schemas": {
          "CreateIntentResponse": {
            "type": "object",
            "required": ["intent"],
            "properties": { "intent": { "$ref": "#/components/schemas/CreateIntent" } }
          },
          "CreateIntent": {
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    // Must generate at all: a name collision here surfaces as a wit-parser "defined more than
    // once" error out of `generate`.
    let generated = generate(&spec, &package, None).unwrap();

    // The function keeps the clean operation name.
    assert!(
        generated.wit.contains("create-intent: func("),
        "the operation should keep its function name:\n{}",
        generated.wit
    );
    // The response body record named after the operation yields, taking a suffixed name.
    assert!(
        generated.wit.contains("record create-intent-v2 {"),
        "the response record colliding with the function name should be disambiguated:\n{}",
        generated.wit
    );
}

// r[verify codegen.free-form-object-as-map]
// A `type: object` schema that declares no `properties` is an open map (free-form object),
// e.g. CircleCI's `BuildParameters: { type: object }` used for arbitrary env-var name/value
// pairs. It used to degrade to an opaque `record build-parameters { data: option<string> }`
// wrapper, discarding the map shape. It must instead lower to a `list<{name}-entry>` where
// `{name}-entry` is a `record { key: string, value: V }` named after the containing type
// (string values, since a bare `type: object` gives no value type), and a `$ref` to it must
// resolve to that map type rather than minting an opaque wrapper record.
#[test]
fn types_free_form_object_as_string_map() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/build": {
          "post": {
            "tags": ["pipelines"],
            "operationId": "startBuild",
            "requestBody": {
              "content": {
                "application/json": {
                  "schema": {
                    "type": "object",
                    "properties": {
                      "build_parameters": { "$ref": "#/components/schemas/BuildParameters" }
                    }
                  }
                }
              }
            },
            "responses": { "200": { "description": "ok" } }
          }
        }
      },
      "components": {
        "schemas": {
          "BuildParameters": { "type": "object" }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated
            .wit
            .contains("build-parameters: option<list<build-parameters-entry>>"),
        "a free-form `type: object` should lower to a `list<{{name}}-entry>` map:\n{}",
        generated.wit
    );
    assert!(
        generated
            .wit
            .contains("record build-parameters-entry {\n    key: string,\n    value: string\n  }"),
        "the map must emit a `key`/`value` entry record named after the containing type:\n{}",
        generated.wit
    );
    // The map serializes to a JSON *object*, not serde's default array-of-`{key, value}` objects.
    assert!(
        generated.rust.contains(
            ".iter().map(|e| (e.key.clone(), Value::String((&e.value).clone()))).collect()"
        ),
        "the map must serialize into a JSON object:\n{}",
        generated.rust
    );
}

// r[verify codegen.free-form-object-typed-values]
// When a free-form object declares `additionalProperties: <schema>`, the value type is known,
// so the map is typed: `additionalProperties: { type: integer }` -> `list<tuple<string, s32>>`.
#[test]
fn types_free_form_object_with_typed_additional_properties() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/counts": {
          "get": {
            "tags": ["counts"],
            "operationId": "getCounts",
            "responses": {
              "200": {
                "description": "ok",
                "content": {
                  "application/json": {
                    "schema": { "type": "object", "additionalProperties": { "type": "integer" } }
                  }
                }
              }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        generated
            .wit
            .contains("-> result<list<get-counts-response-entry>, string>"),
        "a free-form object response should return a `list<{{name}}-entry>` map:\n{}",
        generated.wit
    );
    assert!(
        generated
            .wit
            .contains("record get-counts-response-entry {\n    key: string,\n    value: s32\n  }"),
        "a typed `additionalProperties` schema should type the entry's `value` field:\n{}",
        generated.wit
    );
}

// r[verify codegen.closed-empty-object-keeps-record]
// A *closed* empty object (`additionalProperties: false`, no properties) is not a map — it has
// no entries at all — so it keeps the opaque escape-hatch record rather than becoming a map.
#[test]
fn types_closed_empty_object_keeps_escape_hatch_record() {
    let spec_json = r##"{
      "openapi": "3.0.0",
      "info": { "title": "demo", "version": "1.0.0" },
      "paths": {
        "/thing": {
          "get": {
            "tags": ["thing"],
            "operationId": "getThing",
            "responses": {
              "200": {
                "description": "ok",
                "content": {
                  "application/json": {
                    "schema": { "type": "object", "additionalProperties": false }
                  }
                }
              }
            }
          }
        }
      }
    }"##;
    let spec = parse_openapi(spec_json).unwrap();
    let package = PackageName::parse("wilted:demo@0.1.0").unwrap();
    let generated = generate(&spec, &package, None).unwrap();

    assert!(
        !generated.wit.contains("tuple<string"),
        "a closed empty object must not become a map:\n{}",
        generated.wit
    );
    assert!(
        generated.wit.contains("data: option<string>"),
        "a closed empty object should keep the JSON-blob escape hatch:\n{}",
        generated.wit
    );
}
