//! Throwaway dev helper: run the `openapi-bindgen` library natively.
//!
//! Reads an OpenAPI 2 (Swagger) or 3 document, generates WIT + Rust bindings, and writes
//! them to an output directory. This deliberately skips the WebAssembly component path and
//! calls the library's `generate` directly — we're cheating to iterate quickly. It's
//! temporary and will be deleted once the component workflow is wired up.
//!
//! ```sh
//! cargo run --example run -- <openapi-path> <out-dir> <package>
//! # e.g.
//! cargo run --example run -- vendor/schemas/APIs/svix.com/1.4/openapi.yaml out/svix svix:api@0.1.0
//! ```

use std::path::Path;

use anyhow::{Context, Result, bail};
use openapi_bindgen::{PackageName, from_json_value, generate};
use openapiv3::OpenAPI;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("run");
    let [spec_path, out_dir, package_raw] = args.get(1..).unwrap_or_default() else {
        bail!("usage: {prog} <openapi-path> <out-dir> <package>");
    };

    let spec = load_spec(spec_path)?;
    let package = PackageName::parse(package_raw)?;
    let generated = generate(&spec, &package, None)?;

    let out_dir = Path::new(out_dir);
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create output dir `{}`", out_dir.display()))?;

    let wit_path = out_dir.join(format!("{}.wit", package.name));
    let rust_path = out_dir.join(format!("{}.rs", package.name));
    let cargo_path = out_dir.join("Cargo.toml");
    let wasm_path = out_dir.join("wasm.toml");
    let readme_path = out_dir.join("README.md");
    std::fs::write(&wit_path, &generated.wit)
        .with_context(|| format!("failed to write `{}`", wit_path.display()))?;
    std::fs::write(&rust_path, &generated.rust)
        .with_context(|| format!("failed to write `{}`", rust_path.display()))?;
    std::fs::write(&cargo_path, &generated.cargo_toml)
        .with_context(|| format!("failed to write `{}`", cargo_path.display()))?;
    std::fs::write(&wasm_path, &generated.wasm_toml)
        .with_context(|| format!("failed to write `{}`", wasm_path.display()))?;
    std::fs::write(&readme_path, &generated.readme)
        .with_context(|| format!("failed to write `{}`", readme_path.display()))?;

    println!("wrote {}", wit_path.display());
    println!("wrote {}", rust_path.display());
    println!("wrote {}", cargo_path.display());
    println!("wrote {}", wasm_path.display());
    println!("wrote {}", readme_path.display());
    println!("interfaces: {}", generated.interfaces.join(", "));
    Ok(())
}

/// Read and parse an OpenAPI document, choosing JSON or YAML by file extension. Accepts
/// OpenAPI 2 (Swagger) or 3; v2 documents are normalized to v3 by `from_json_value`.
fn load_spec(path: &str) -> Result<OpenAPI> {
    let bytes = std::fs::read(path).with_context(|| format!("failed to read `{path}`"))?;
    let is_json = Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let doc: serde_json::Value = if is_json {
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse `{path}` as JSON"))?
    } else {
        serde_yaml::from_slice(&bytes)
            .with_context(|| format!("failed to parse `{path}` as YAML"))?
    };
    from_json_value(doc).with_context(|| format!("failed to load OpenAPI document `{path}`"))
}
