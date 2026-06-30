//! Integration tests for `openapi-bindgen`.

use openapi_bindgen::{PackageName, generate};

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
