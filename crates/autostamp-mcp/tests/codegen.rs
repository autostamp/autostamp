//! Standalone artifacts and generator-component contract checks.

use autostamp_mcp::{GenerateOptions, Generated, PackageName, generate, parse_catalog};

fn fixture(secret: Option<&str>) -> Generated {
    generate(
        &parse_catalog(include_str!("fixtures/tools.json")).unwrap(),
        &PackageName::parse("example:service@0.1.0").unwrap(),
        &GenerateOptions {
            endpoint: "https://example.invalid/mcp".into(),
            bearer_secret: secret.map(str::to_owned),
            ..GenerateOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn artifacts_are_deterministic_standalone_and_structural() {
    let generated = fixture(None);
    assert_eq!(generated, fixture(None));
    assert_eq!(generated.rust.first().unwrap().path, "lib.rs");
    assert!(generated.cargo_toml.contains("[workspace]"));
    assert!(generated.cargo_toml.contains("doctest = false"));
    assert!(generated.cargo_toml.contains("wit-bindgen = \"0.41\""));
    assert!(!generated.cargo_toml.contains("autostamp-openapi"));
    assert!(!generated.cargo_toml.contains("autostamp-mcp ="));
    assert!(
        generated
            .rust
            .iter()
            .any(|source| source.path == "runtime/mod.rs")
    );
    assert!(
        generated
            .rust
            .iter()
            .any(|source| source.path == "runtime/wire.rs")
    );
    assert!(
        generated
            .rust
            .iter()
            .any(|source| source.path == "runtime/wasi.rs")
    );
    let codec = &generated
        .rust
        .iter()
        .find(|source| source.path == "codecs.rs")
        .unwrap()
        .contents;
    assert!(codec.contains(".transpose()?"));
    assert!(codec.contains("unknown property in closed object"));
    assert!(codec.contains("extra key conflicts with a declared property"));
    assert!(codec.contains("duplicate object key"));
    assert!(!codec.contains("serde_json::from_value"));
    assert!(!codec.contains("derive(Deserialize"));
    let client = &generated
        .rust
        .iter()
        .find(|source| source.path == "client.rs")
        .unwrap()
        .contents;
    assert!(client.contains("crate::wasi::call"));
    assert_eq!(
        generated
            .rust
            .iter()
            .find(|source| source.path == "runtime/mod.rs")
            .unwrap()
            .contents,
        include_str!("../src/runtime/mod.rs")
    );
    assert!(client.contains("structured_content, envelope_json"));
    assert!(client.contains("fn ping() -> Result<String, tool::Error>"));
}

#[test]
fn anonymous_clients_do_not_import_secrets() {
    let generated = fixture(None);
    assert!(generated.wit_dependencies.is_empty());
    assert!(!generated.wit.contains("wasmcloud:secrets"));
    assert!(
        !generated
            .rust
            .iter()
            .find(|source| source.path == "credentials.rs")
            .unwrap()
            .contents
            .contains("wasmcloud")
    );
}

#[test]
fn authenticated_clients_vendor_and_resolve_secret_interfaces() {
    let generated = fixture(Some("mcp-bearer"));
    assert_eq!(generated.wit_dependencies.len(), 1);
    let mut resolve = wit_parser::Resolve::default();
    for dependency in &generated.wit_dependencies {
        resolve
            .push_str(&dependency.path, &dependency.contents)
            .unwrap();
    }
    resolve.push_str("world.wit", &generated.wit).unwrap();
    assert!(
        generated
            .wit
            .contains("import wasmcloud:secrets/store@1.0.0")
    );
    let credentials = &generated
        .rust
        .iter()
        .find(|source| source.path == "credentials.rs")
        .unwrap()
        .contents;
    assert!(credentials.contains("store::get"));
    assert!(credentials.contains("reveal::reveal"));
    assert!(credentials.contains("SecretValue::Bytes"));
}

#[test]
fn options_are_validated_before_emission() {
    let catalog = parse_catalog(r#"{"tools":[]}"#).unwrap();
    let package = PackageName::parse("example:empty").unwrap();
    for endpoint in [
        "",
        "file:///catalog",
        "https://user:password@example.test",
        "https://example.test/#fragment",
        "https://",
    ] {
        let options = GenerateOptions {
            endpoint: endpoint.into(),
            ..GenerateOptions::default()
        };
        assert!(
            generate(&catalog, &package, &options).is_err(),
            "{endpoint}"
        );
    }
    let options = GenerateOptions {
        endpoint: "https://example.test/mcp".into(),
        bearer_secret: Some(String::new()),
        ..GenerateOptions::default()
    };
    assert!(generate(&catalog, &package, &options).is_err());
    let mut options = GenerateOptions {
        endpoint: "https://example.test/mcp".into(),
        ..GenerateOptions::default()
    };
    options.limits.request_timeout_ms = 0;
    assert!(generate(&catalog, &package, &options).is_err());
    options.limits.request_timeout_ms = u64::MAX;
    assert!(generate(&catalog, &package, &options).is_err());
    options.limits = autostamp_mcp::runtime::Limits::default();
    if let Ok(oversized) = usize::try_from(u64::from(u32::MAX) + 1) {
        options.limits.max_response_bytes = oversized;
        assert!(generate(&catalog, &package, &options).is_err());
        options.limits.max_event_bytes = oversized;
        assert!(generate(&catalog, &package, &options).is_err());
    }
}

#[test]
fn generator_component_contract_resolves() {
    wit_parser::Resolve::default()
        .push_str("world.wit", include_str!("../wit/world.wit"))
        .unwrap();
}
