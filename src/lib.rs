//! Convert OpenAPI schema definitions to WebAssembly Components.
//!
//! Reads an OpenAPI 2 (Swagger) or 3 document and produces:
//! - WIT source: one interface per tag, with per-operation parameter records
//! - Rust source: `Guest` trait impls plus manual `*_to_json` / `*_to_str` helpers
//!
//! [`parse_openapi`] (and [`from_json_value`]) read a document, transparently normalizing
//! Swagger 2.0 into the OpenAPI 3 model so the rest of the pipeline only ever sees v3. The
//! entry point is [`generate`], which takes a parsed [`OpenAPI`] document and a
//! [`PackageName`] and returns the generated [`Generated::wit`] and [`Generated::rust`]
//! sources. [`rewrite_world_exports`] updates the `export` list of a WIT world so it
//! matches the generated interfaces.
//!
//! We deliberately avoid `additional_derives: [Serialize]` on the wit-bindgen macro because
//! it conflicts with imported wasi resource types. Instead, codegen emits an explicit
//! `<record>_to_json` per WIT record and `<enum>_to_str` per WIT enum.
//!
//! # Examples
//!
//! ```no_run
//! use openapi_bindgen::{PackageName, generate, parse_openapi};
//!
//! # fn main() -> anyhow::Result<()> {
//! let json = std::fs::read_to_string("openapi.json")?;
//! // Accepts OpenAPI 2 (Swagger) or 3; v2 is normalized to v3 internally.
//! let spec = parse_openapi(&json)?;
//! let package = PackageName::parse("incidentio:api@0.1.0")?;
//! let generated = generate(&spec, &package, None)?;
//! std::fs::write("api.wit", &generated.wit)?;
//! std::fs::write("api.rs", &generated.rust)?;
//! std::fs::write("Cargo.toml", &generated.cargo_toml)?;
//! std::fs::write("README.md", &generated.readme)?;
//! # Ok(())
//! # }
//! ```

mod auth_doc;
mod compat;
mod enum_model;
mod field;
mod generated;
mod interface_model;
mod location;
mod manifest;
mod naming;
mod operation_model;
mod package_name;
mod readme;
mod record_model;
mod rust_codegen;
mod schema_ctx;
mod security;
mod servers;
mod swagger2;
mod version;
mod wit_type;

pub use generated::Generated;
pub use package_name::PackageName;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use openapiv3::{OpenAPI, ReferenceOr};
use wit_parser::Resolve;

use crate::interface_model::InterfaceModel;
use crate::naming::sanitize_wit_name;
use crate::rust_codegen::emit_rust;
use crate::schema_ctx::SchemaCtx;
use crate::security::{AuthApply, SecurityRegistry};

/// Parse an OpenAPI document from a JSON string, accepting either OpenAPI 2 (Swagger) or
/// OpenAPI 3.
///
/// Swagger 2.0 documents are detected by their `swagger` field and normalized into the
/// OpenAPI 3 model before being returned, so callers only ever deal with [`OpenAPI`].
pub fn parse_openapi(spec_json: &str) -> Result<OpenAPI> {
    let doc: serde_json::Value =
        serde_json::from_str(spec_json).context("failed to parse OpenAPI document as JSON")?;
    from_json_value(doc)
}

/// Build an [`OpenAPI`] document from an already-parsed JSON [`Value`](serde_json::Value),
/// accepting either OpenAPI 2 (Swagger) or OpenAPI 3.
///
/// This is the value-level counterpart to [`parse_openapi`]; use it when the document was
/// read from a non-JSON source (for example YAML deserialized into a `serde_json::Value`).
/// Swagger 2.0 documents are normalized into the OpenAPI 3 model in place.
pub fn from_json_value(mut doc: serde_json::Value) -> Result<OpenAPI> {
    if swagger2::is_v2(&doc) {
        swagger2::convert(&mut doc).context("failed to normalize Swagger 2.0 document")?;
    }
    compat::inline_nonlocal_refs(&mut doc);
    strip_numeric_bounds(&mut doc);
    serde_json::from_value(doc).context("failed to parse OpenAPI document")
}

/// Remove numeric validation keywords from every schema in `doc` before it reaches
/// `openapiv3`.
///
/// `openapiv3`'s `IntegerType` bounds are `i64`, so out-of-range values that some specs
/// carry (e.g. DigitalOcean's `maximum: 18446744073709552000`) fail to deserialize. The WIT
/// generator never reads numeric bounds — only `type`, `$ref`, and `properties` — so dropping
/// `maximum` / `minimum` / `exclusiveMaximum` / `exclusiveMinimum` / `multipleOf` is lossless
/// for our purposes. Only *number*-valued occurrences are removed, so a schema property
/// literally named `maximum` (whose value is a schema object) is preserved.
fn strip_numeric_bounds(value: &mut serde_json::Value) {
    const NUMERIC_KEYWORDS: [&str; 5] = [
        "maximum",
        "minimum",
        "exclusiveMaximum",
        "exclusiveMinimum",
        "multipleOf",
    ];
    match value {
        serde_json::Value::Object(map) => {
            for keyword in NUMERIC_KEYWORDS {
                if map.get(keyword).is_some_and(serde_json::Value::is_number) {
                    map.remove(keyword);
                }
            }
            for child in map.values_mut() {
                strip_numeric_bounds(child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                strip_numeric_bounds(child);
            }
        }
        _ => {}
    }
}

/// Error returned by [`generate`] when a document defines no operations to bind.
///
/// A specification whose `paths` is empty (or whose path items carry no operations) has nothing
/// to generate. This is distinct from a document that *has* operations but none could be grouped
/// into an interface. Callers can downcast the [`anyhow::Error`] to this type to treat an empty
/// document as a benign skip rather than a hard failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoOperations;

impl std::fmt::Display for NoOperations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the document defines no operations to bind")
    }
}

impl std::error::Error for NoOperations {}

/// Generate WIT and Rust bindings from a parsed OpenAPI 3 document.
///
/// Operations are grouped into one interface per tag. When `tags` is `Some`, only
/// operations carrying one of the listed tags are emitted; when `None`, every operation
/// is emitted and grouped by its first tag.
pub fn generate(
    spec: &OpenAPI,
    package: &PackageName,
    tags: Option<&[String]>,
) -> Result<Generated> {
    let ctx = SchemaCtx::new(spec);
    let registry = SecurityRegistry::from_spec(spec);

    let mut by_iface: IndexMap<String, InterfaceModel> = IndexMap::new();
    let mut total_operations: usize = 0;

    for (path, item_ref) in spec.paths.paths.iter() {
        let item = match item_ref {
            ReferenceOr::Item(p) => p,
            _ => continue,
        };
        for (method, op_opt) in [
            ("get", &item.get),
            ("post", &item.post),
            ("put", &item.put),
            ("patch", &item.patch),
            ("delete", &item.delete),
        ] {
            let Some(op) = op_opt else { continue };
            total_operations += 1;
            let matched_tag = match tags {
                Some(allow) => op.tags.iter().find(|t| allow.iter().any(|a| a == *t)),
                None => op.tags.first(),
            };

            // Tags become interface names, so they must be valid WIT identifiers: a tag
            // like `Export` or `Lists` would otherwise emit `interface export`/`list`
            // (reserved words) or an empty name.
            let iface_name = match matched_tag {
                Some(tag) => sanitize_wit_name(tag),
                // An untagged operation has no tag to group under. With an explicit tag
                // allowlist the caller asked to filter, so it stays excluded; otherwise group
                // it by path segment so it still generates (e.g. `/v1/charges` -> `charges`).
                None if tags.is_some() => continue,
                None => interface_name_from_path(path, &spec.info.title),
            };
            let iface = by_iface
                .entry(iface_name.clone())
                .or_insert_with(|| InterfaceModel::new(iface_name.clone()));
            let auth = registry.operation_requirement(spec, op);
            iface.process_operation(&ctx, method, path, &item.parameters, op, auth)?;
        }
    }

    let mut ifaces: Vec<InterfaceModel> = by_iface.into_values().collect();

    if ifaces.is_empty() {
        if total_operations == 0 {
            // A document with no operations (e.g. `paths: {}`) has nothing to bind. This is a
            // benign, recognizable condition rather than a generation failure — callers can
            // downcast to `NoOperations` and treat it as a skip.
            return Err(NoOperations.into());
        }
        bail!(
            "no interfaces generated: the document has no tagged operations to group into interfaces"
        );
    }

    // Drop interfaces that ended up with no operations. wit-bindgen emits a `Guest` trait only
    // for an interface that has at least one function, so exporting a function-less interface
    // (and generating an `impl Guest` for it) would not compile. An interface can end up empty
    // when every operation grouped under its tag was skipped — e.g. an operation whose request
    // body `$ref` couldn't be resolved still seeded the interface (and any param-derived enum)
    // before being dropped. If nothing bindable remains, treat the document as a skip.
    ifaces.retain(|i| !i.operations.is_empty());
    if ifaces.is_empty() {
        return Err(NoOperations.into());
    }

    // Drop request-body records whose fields were inlined into params records and which
    // nothing else references, so they don't surface as dead WIT types.
    for iface in &mut ifaces {
        iface.prune_unused_records();
    }

    // The named secrets this component expects the host to provision: the union of every
    // operation's resolved auth requirement, deduplicated by secret key and ordered. Surfaced
    // in both the README and the WIT package comment.
    let auth_schemes = collect_auth_schemes(&ifaces);

    let mut wit = String::new();
    for (i, iface) in ifaces.iter().enumerate() {
        let body = iface.to_wit(package);
        if i == 0 {
            wit.push_str(&body);
        } else {
            wit.push('\n');
            wit.push_str(
                &body
                    .lines()
                    .skip_while(|l| l.starts_with("package"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    // Lead the package declaration with an authentication doc comment naming the secrets the
    // host must provision. Prepended before validation so the comment is checked too.
    if let Some(comment) = auth_doc::wit_comment(&auth_schemes) {
        wit = format!("{comment}{wit}");
    }

    let base_url = servers::resolve_base_url(spec).unwrap_or_default();
    let rust = emit_rust(&ifaces, package, &base_url);

    let interfaces: Vec<String> = ifaces.iter().map(|i| i.name_kebab.clone()).collect();

    // Validate the assembled interface WIT as the final step of generation: a parse +
    // resolve with `wit-parser` rejects malformed packages, duplicate or invalid
    // identifiers, and dangling type references before we ever hand the source back to a
    // caller. We validate the interfaces alone — before appending the world, whose external
    // imports are not resolvable in this self-contained, in-memory check.
    validate_wit(&wit)?;

    // Append the `client` world importing `wasmcloud:secrets` and exporting the generated
    // interfaces. It is deliberately excluded from `validate_wit`, because its import
    // references a package that is only fetched into `wit/deps/` at component-build time.
    wit.push('\n');
    wit.push_str(&emit_world(&interfaces));

    let cargo_toml = manifest::render(package);

    // The publish version carries the provider name and the document's `info.version` as
    // SemVer build metadata (e.g. `0.1.0+github-2022-11-28`) so the published artifact records
    // which provider and schema revision it was generated from. The base version (and the
    // WIT/`Cargo.toml` versions) stay clean.
    let base_version = package.version.as_deref().unwrap_or("0.1.0");
    let publish_version = version::publish_version(base_version, &package.name, &spec.info.version);
    let wasm_toml = manifest::render_wasm_manifest(package, &publish_version);

    // Secret keys the API-key inference heuristic synthesized, across all interfaces, deduped and
    // ordered — named in the diagnostics so a reader sees which credentials were inferred rather
    // than declared. (They already flow into the auth tables via `collect_auth_schemes`.)
    let inferred_api_key_secrets = {
        let mut v: Vec<String> = ifaces
            .iter()
            .flat_map(|i| i.inferred_api_key_secrets.iter().cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    };

    let diagnostics = readme::Diagnostics {
        published_version: publish_version,
        tag_filter: tags.map(<[String]>::to_vec),
        operations: ifaces.iter().map(|i| i.operations.len()).sum(),
        pruned_credential_fields: ifaces.iter().map(|i| i.pruned_credential_fields).sum(),
        inferred_api_key_secrets,
        auth: auth_schemes,
    };
    let readme = readme::render(package, &diagnostics);

    Ok(Generated {
        wit,
        rust,
        cargo_toml,
        wasm_toml,
        readme,
        interfaces,
    })
}

/// Derive an interface name for an untagged operation from its request path: the first path
/// segment that is a real resource — not a version marker (`v1`, `v2.1`, ...), a `{template}`
/// parameter, or a `#`-fragment pseudo-path (AWS-style `/#X-Amz-Target=...` RPC routes) —
/// matching the convention that `/v1/charges` groups under `charges`. Falls back to the
/// sanitized API title, then `api`, when the path offers no usable segment (e.g. `/`, an
/// all-version/template path, or a fragment route).
/// Collect the named security schemes a component expects the host to provision: the union of
/// every operation's *resolved* auth requirement, deduplicated by secret key (keeping the first
/// occurrence) and sorted by secret key for deterministic output. Because the requirement is
/// already resolved to a single alternative, non-selected OR branches are naturally excluded.
fn collect_auth_schemes(ifaces: &[InterfaceModel]) -> Vec<AuthApply> {
    let mut schemes: Vec<AuthApply> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for iface in ifaces {
        for op in &iface.operations {
            for apply in &op.auth {
                if seen.insert(apply.secret_key.clone()) {
                    schemes.push(apply.clone());
                }
            }
        }
    }
    schemes.sort_by(|a, b| a.secret_key.cmp(&b.secret_key));
    schemes
}

fn interface_name_from_path(path: &str, title: &str) -> String {
    for segment in path.split('/') {
        let segment = segment.trim();
        if segment.is_empty()
            || segment.starts_with('{')
            || segment.starts_with('#')
            || is_version_segment(segment)
        {
            continue;
        }
        return sanitize_wit_name(segment);
    }
    let title = title.trim();
    if title.is_empty() {
        "api".to_string()
    } else {
        sanitize_wit_name(title)
    }
}

/// Whether a path segment is a version marker like `v1`, `v2`, or `v1.2`. Such segments make
/// poor interface names, so untagged grouping skips them in favor of the following segment.
fn is_version_segment(segment: &str) -> bool {
    match segment.strip_prefix(['v', 'V']) {
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.'),
        None => false,
    }
}

/// Emit the generated `client` world: import the `wasmcloud:secrets` store/reveal interfaces
/// the runtime uses, and export every generated interface. The HTTP host import
/// (`wasi:http/outgoing-handler`) is contributed by the `wstd` crate's own bindings at build
/// time, so it is not declared here.
fn emit_world(interfaces: &[String]) -> String {
    let mut world = String::from("/// The component world: imports the secrets host capability\n");
    world.push_str("/// the runtime uses and exports the generated interfaces. The HTTP host\n");
    world.push_str("/// capability is imported by the `wstd` crate's bindings at build time.\n");
    world.push_str("world client {\n");
    world.push_str("  import wasmcloud:secrets/store@1.0.0;\n");
    world.push_str("  import wasmcloud:secrets/reveal@1.0.0;\n");
    for iface in interfaces {
        world.push_str(&format!("  export {iface};\n"));
    }
    world.push_str("}\n");
    world
}

/// Parse and resolve `wit` with `wit-parser`, returning an error if it is not a valid,
/// self-contained WIT package.
fn validate_wit(wit: &str) -> Result<()> {
    let mut resolve = Resolve::new();
    resolve
        .push_str("generated.wit", wit)
        .context("generated WIT failed validation")?;
    Ok(())
}

/// Rewrite the `export` list of a WIT world, replacing any existing `export <iface>;`
/// lines with one per entry in `interfaces`.
///
/// If the world has no existing exports, the new lines are inserted before the world's
/// closing brace. Returns the rewritten source.
#[must_use]
pub fn rewrite_world_exports(src: &str, interfaces: &[String]) -> String {
    let mut out = String::new();
    let mut in_exports = false;
    let mut emitted = false;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("export ") && trimmed.ends_with(';') {
            if !emitted {
                for iface in interfaces {
                    out.push_str(&format!("  export {iface};\n"));
                }
                emitted = true;
            }
            in_exports = true;
            continue;
        }
        if in_exports && !trimmed.starts_with("export ") {
            in_exports = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !emitted && let Some(idx) = out.rfind("}\n") {
        let (head, tail) = out.split_at(idx);
        let mut prefix = head.to_string();
        for iface in interfaces {
            prefix.push_str(&format!("  export {iface};\n"));
        }
        prefix.push_str(tail);
        return prefix;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{interface_name_from_path, is_version_segment, strip_numeric_bounds};

    #[test]
    fn strips_only_number_valued_numeric_keywords() {
        let mut doc: serde_json::Value = serde_json::from_str(
            r#"{
              "outer": {
                "type": "integer",
                "maximum": 18446744073709552000,
                "minimum": 0,
                "multipleOf": 2,
                "properties": { "maximum": { "type": "number" } }
              }
            }"#,
        )
        .unwrap();
        strip_numeric_bounds(&mut doc);
        // Number-valued bounds are removed (including the out-of-range `maximum`)...
        assert!(doc.pointer("/outer/maximum").is_none());
        assert!(doc.pointer("/outer/minimum").is_none());
        assert!(doc.pointer("/outer/multipleOf").is_none());
        // ...but a schema *property* literally named `maximum` (object value) is preserved.
        assert!(doc.pointer("/outer/properties/maximum").is_some());
        assert!(doc.pointer("/outer/type").is_some());
    }

    #[test]
    fn recognizes_version_segments() {
        assert!(is_version_segment("v1"));
        assert!(is_version_segment("v2"));
        assert!(is_version_segment("v1.2"));
        assert!(!is_version_segment("version"));
        assert!(!is_version_segment("v"));
        assert!(!is_version_segment("v1beta"));
        assert!(!is_version_segment("charges"));
    }

    #[test]
    fn path_grouping_skips_version_and_template_segments() {
        assert_eq!(interface_name_from_path("/v1/charges", "Demo"), "charges");
        assert_eq!(
            interface_name_from_path("/v1/charges/{id}", "Demo"),
            "charges"
        );
        assert_eq!(
            interface_name_from_path("/customers/{id}", "Demo"),
            "customers"
        );
    }

    #[test]
    fn path_grouping_falls_back_to_title_then_api() {
        // Path made only of a template parameter -> fall back to the sanitized title.
        assert_eq!(interface_name_from_path("/{id}", "Demo API"), "demo-api");
        // AWS-style `#`-fragment RPC route -> no usable segment -> title.
        assert_eq!(
            interface_name_from_path("/#X-Amz-Target=Hub.DoThing", "AWS Migration Hub"),
            "aws-migration-hub"
        );
        // Root path with an empty title -> the `api` constant.
        assert_eq!(interface_name_from_path("/", ""), "api");
    }
}

// The component bindings require `unsafe` and other lints that are denied across the rest
// of the crate; they are isolated behind this single gated module. See `component.rs`.
#[cfg(target_arch = "wasm32")]
#[allow(
    unsafe_code,
    unreachable_pub,
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::indexing_slicing,
    clippy::must_use_candidate
)]
mod component;
