//! Convert OpenAPI schema definitions to WebAssembly Components.
//!
//! Reads an OpenAPI 3 document and produces:
//! - WIT source: one interface per tag, with per-operation parameter records
//! - Rust source: `Guest` trait impls plus manual `*_to_json` / `*_to_str` helpers
//!
//! The entry point is [`generate`], which takes a parsed [`OpenAPI`] document and a
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
//! use openapi_bindgen::{PackageName, generate};
//!
//! # fn main() -> anyhow::Result<()> {
//! let spec: openapiv3::OpenAPI = serde_json::from_slice(&std::fs::read("openapi.json")?)?;
//! let package = PackageName::parse("incidentio:api@0.1.0")?;
//! let generated = generate(&spec, &package, None)?;
//! std::fs::write("api.wit", &generated.wit)?;
//! std::fs::write("api.rs", &generated.rust)?;
//! # Ok(())
//! # }
//! ```

mod enum_model;
mod field;
mod generated;
mod interface_model;
mod location;
mod naming;
mod operation_model;
mod package_name;
mod record_model;
mod rust_codegen;
mod schema_ctx;
mod wit_type;

pub use generated::Generated;
pub use package_name::PackageName;

use anyhow::Result;
use heck::ToKebabCase;
use indexmap::IndexMap;
use openapiv3::{OpenAPI, ReferenceOr};

use crate::interface_model::InterfaceModel;
use crate::rust_codegen::emit_rust;
use crate::schema_ctx::SchemaCtx;

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

            let iface_name = tag.to_kebab_case();
            let iface = by_iface
                .entry(iface_name.clone())
                .or_insert_with(|| InterfaceModel::new(iface_name.clone()));
            iface.process_operation(&ctx, method, path, op)?;
        }
    }

    let ifaces: Vec<InterfaceModel> = by_iface.into_values().collect();

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

    let rust = emit_rust(&ifaces, package);

    let interfaces = ifaces.iter().map(|i| i.name_kebab.clone()).collect();

    Ok(Generated {
        wit,
        rust,
        interfaces,
    })
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
