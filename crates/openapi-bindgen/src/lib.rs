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
use crate::security::SecurityRegistry;

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
    serde_json::from_value(doc).context("failed to parse OpenAPI document")
}

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
            let matched_tag = match tags {
                Some(allow) => op.tags.iter().find(|t| allow.iter().any(|a| a == *t)),
                None => op.tags.first(),
            };
            let Some(tag) = matched_tag else { continue };

            // Tags become interface names, so they must be valid WIT identifiers: a tag
            // like `Export` or `Lists` would otherwise emit `interface export`/`list`
            // (reserved words) or an empty name.
            let iface_name = sanitize_wit_name(tag);
            let iface = by_iface
                .entry(iface_name.clone())
                .or_insert_with(|| InterfaceModel::new(iface_name.clone()));
            let auth = registry.operation_requirement(spec, op);
            iface.process_operation(&ctx, method, path, op, auth)?;
        }
    }

    let mut ifaces: Vec<InterfaceModel> = by_iface.into_values().collect();

    if ifaces.is_empty() {
        bail!(
            "no interfaces generated: the document has no tagged operations to group into interfaces"
        );
    }

    // Drop request-body records whose fields were inlined into params records and which
    // nothing else references, so they don't surface as dead WIT types.
    for iface in &mut ifaces {
        iface.prune_unused_records();
    }

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

    let base_url = servers::resolve_base_url(spec).unwrap_or_default();
    let rust = emit_rust(&ifaces, package, &base_url);

    let interfaces: Vec<String> = ifaces.iter().map(|i| i.name_kebab.clone()).collect();

    // Validate the assembled interface WIT as the final step of generation: a parse +
    // resolve with `wit-parser` rejects malformed packages, duplicate or invalid
    // identifiers, and dangling type references before we ever hand the source back to a
    // caller. We validate the interfaces alone — before appending the world, whose external
    // imports are not resolvable in this self-contained, in-memory check.
    validate_wit(&wit)?;

    // Append the `bindgen` world importing `wasi:http` + `wasmcloud:secrets` and exporting
    // the generated interfaces. It is deliberately excluded from `validate_wit`, because its
    // imports reference packages that are only fetched into `wit/deps/` at component-build
    // time.
    wit.push('\n');
    wit.push_str(&emit_world(&interfaces));

    let cargo_toml = manifest::render(package);
    let wasm_toml = manifest::render_wasm_deps();

    let diagnostics = readme::Diagnostics {
        tag_filter: tags.map(<[String]>::to_vec),
        operations: ifaces.iter().map(|i| i.operations.len()).sum(),
        pruned_credential_fields: ifaces.iter().map(|i| i.pruned_credential_fields).sum(),
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

/// Emit the generated `bindgen` world: import the `wasi:http` outgoing-request surface and
/// the `wasmcloud:secrets` store/reveal interfaces the runtime uses, and export every
/// generated interface.
fn emit_world(interfaces: &[String]) -> String {
    let mut world = String::from("/// The component world: imports the HTTP + secrets host\n");
    world.push_str("/// capabilities the runtime uses and exports the generated interfaces.\n");
    world.push_str("world bindgen {\n");
    world.push_str("  import wasi:http/outgoing-handler@0.2.3;\n");
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
