//! The [`Generated`] output of [`crate::generate`].

/// A single generated Rust source file: its path (relative to the component crate's `src/`
/// directory) and contents.
///
/// The generator splits its Rust output across files — a `lib.rs` crate root plus one
/// `iface_<name>.rs` module per interface — so no single file grows too large for `rustc` to
/// parse. `lib.rs` is always the first entry.
#[derive(Debug, Clone)]
pub struct RustSource {
    /// The file path relative to the component crate's `src/` directory, e.g. `lib.rs` or
    /// `iface_charges.rs`.
    pub path: String,
    /// The file contents.
    pub contents: String,
}

/// The output of [`crate::generate`]: the generated WIT and Rust sources.
#[derive(Debug, Clone)]
pub struct Generated {
    /// The generated WIT source, with one interface per OpenAPI tag.
    pub wit: String,
    /// The generated Rust source files: a `lib.rs` crate root (shared runtime, bindings, and
    /// `mod` declarations) plus one `iface_<name>.rs` module per interface, each holding that
    /// interface's `Guest` impl and its `*_to_json` / `*_to_str` helpers. `lib.rs` is first.
    pub rust: Vec<RustSource>,
    /// The generated `Cargo.toml` manifest for the component crate.
    pub cargo_toml: String,
    /// The generated `wasm.toml` declaring the component's WIT interface dependency
    /// (`wasmcloud:secrets`), resolved into `wit/deps/` at build time.
    pub wasm_toml: String,
    /// The generated `README.md`: a title matching the component name plus a "Generator
    /// Diagnostics" section recording the options and heuristics this component was generated
    /// with.
    pub readme: String,
    /// The kebab-case names of the generated interfaces, in emission order.
    ///
    /// Pass these to [`crate::rewrite_world_exports`] to update a WIT world's `export` list.
    pub interfaces: Vec<String>,
}
