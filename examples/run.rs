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
use openapi_bindgen::{NoOperations, PackageName, from_json_value, generate};
use openapiv3::OpenAPI;

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

/// Read and parse an OpenAPI document, choosing JSON or YAML by file extension. Accepts
/// OpenAPI 2 (Swagger) or 3; v2 documents are normalized to v3 by `from_json_value`.
fn load_spec(path: &str) -> Result<OpenAPI> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("failed to read `{path}`"))?;
    // Some published specs carry stray Unicode control characters inside string values (usually
    // mojibake) and oversized integer bounds; both make serde_json/serde_yaml reject the document.
    // Normalize them away before parsing — `from_json_value` then strips the numeric bounds.
    let text = widen_oversized_integers(&strip_control_chars(&raw));
    let is_json = Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let doc: serde_json::Value = if is_json {
        serde_json::from_str(&text).with_context(|| format!("failed to parse `{path}` as JSON"))?
    } else {
        serde_yaml::from_str(&text).with_context(|| format!("failed to parse `{path}` as YAML"))?
    };
    from_json_value(doc).with_context(|| format!("failed to load OpenAPI document `{path}`"))
}

/// Drop Unicode control characters (other than tab, newline, and carriage return) from `text`.
/// Stray C0/C1 controls embedded in string values are illegal in both JSON and YAML, so removing
/// them lets otherwise-valid documents parse. They never appear in well-formed specs, so this is a
/// no-op for clean inputs.
fn strip_control_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\t' || c == '\n' || c == '\r' || !c.is_control())
        .collect()
}

/// Append `.0` to any unquoted decimal integer literal that overflows `u64`, turning it into a
/// float literal both serde_json and serde_yaml accept. Runs preceded by an identifier
/// character or `.`, or followed by `.`/`e`/`E`/another digit, are left alone so existing
/// floats and identifiers are untouched.
fn widen_oversized_integers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        if !c.is_ascii_digit() {
            out.push(c);
            prev = Some(c);
            continue;
        }
        let mut run = String::from(c);
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                run.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let next = chars.peek().copied();
        let prev_ok = !matches!(prev, Some(p) if p.is_alphanumeric() || p == '.' || p == '_');
        let next_ok =
            !matches!(next, Some(n) if n == '.' || n == 'e' || n == 'E' || n.is_ascii_digit());
        out.push_str(&run);
        if prev_ok && next_ok && run.len() >= 20 && run.parse::<u64>().is_err() {
            out.push_str(".0");
        }
        prev = Some('0');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{strip_control_chars, widen_oversized_integers};

    #[test]
    fn strips_control_characters_except_whitespace() {
        // C0 (`\u{7}`) and C1 (`\u{9f}`) controls are removed; tab/newline/CR and text stay.
        assert_eq!(strip_control_chars("a\u{7}b\u{9f}c\td\ne"), "abc\td\ne");
    }

    #[test]
    fn widens_only_oversized_integers() {
        // Above `u64::MAX` -> widened to a float literal.
        assert_eq!(
            widen_oversized_integers("maximum: 18446744073709552000"),
            "maximum: 18446744073709552000.0"
        );
        // Exactly `u64::MAX` still fits -> untouched.
        assert_eq!(
            widen_oversized_integers("x: 18446744073709551615"),
            "x: 18446744073709551615"
        );
        // Small integers -> untouched.
        assert_eq!(widen_oversized_integers("a: 42, b: 7"), "a: 42, b: 7");
        // Fractional digits (preceded by `.`) -> untouched.
        assert_eq!(
            widen_oversized_integers("v: 1.18446744073709552000"),
            "v: 1.18446744073709552000"
        );
        // Identifier-adjacent digits -> untouched.
        assert_eq!(
            widen_oversized_integers("id99999999999999999999x"),
            "id99999999999999999999x"
        );
    }
}
