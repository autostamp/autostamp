//! Dev helper: run the `openapi-bindgen` library natively and write a buildable component crate.
//!
//! Reads an OpenAPI 2 (Swagger) or 3 document, generates the WIT + Rust bindings, and lays
//! them out as a ready-to-build component crate in the output directory:
//!
//! ```text
//! <out-dir>/
//!   Cargo.toml
//!   wasm.toml      # [package] publish metadata + explicit WIT interface deps
//!   README.md
//!   src/lib.rs     # generated Rust (wit-bindgen guest)
//!   wit/world.wit  # generated WIT (deps resolved into wit/deps/ at build time)
//! ```
//!
//! It calls the library's `generate` directly rather than driving the component, which is how
//! `just gen` produces the `components/` tree; `just build` then resolves the WIT
//! deps and compiles each crate for `wasm32-wasip2`.
//!
//! ```sh
//! cargo run --example run -- <openapi-path> <out-dir> <package>
//! # e.g.
//! cargo run --example run -- vendor/schemas/APIs/svix.com/1.4/openapi.yaml components/svix autostamp:svix@0.1.0
//! ```

use std::path::Path;

use anyhow::{Context, Result, bail};
use openapi_bindgen::{NoOperations, PackageName, generate};

#[path = "common/spec_loader.rs"]
mod spec_loader;
use spec_loader::load_spec;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("run");
    let [spec_path, out_dir, package_raw] = args.get(1..).unwrap_or_default() else {
        bail!("usage: {prog} <openapi-path> <out-dir> <package>");
    };

    let spec = load_spec(spec_path)?;
    let package = PackageName::parse(package_raw)?;
    let generated = match generate(&spec, &package, None) {
        Ok(generated) => generated,
        // An empty document (no operations to bind) is a benign skip, not a failure. Signal it
        // with a distinct exit code so the corpus runner can tally it separately.
        Err(err) if err.downcast_ref::<NoOperations>().is_some() => {
            eprintln!("skip: {err}");
            std::process::exit(3);
        }
        Err(err) => return Err(err),
    };

    let out_dir = Path::new(out_dir);
    let src_dir = out_dir.join("src");
    let wit_dir = out_dir.join("wit");
    std::fs::create_dir_all(&src_dir)
        .with_context(|| format!("failed to create source dir `{}`", src_dir.display()))?;
    std::fs::create_dir_all(&wit_dir)
        .with_context(|| format!("failed to create wit dir `{}`", wit_dir.display()))?;

    let wit_path = wit_dir.join("world.wit");
    let rust_path = src_dir.join("lib.rs");
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
