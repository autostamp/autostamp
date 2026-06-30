//! The [`Generated`] output of [`crate::generate`].

/// The output of [`crate::generate`]: the generated WIT and Rust sources.
#[derive(Debug, Clone)]
pub struct Generated {
    /// The generated WIT source, with one interface per OpenAPI tag.
    pub wit: String,
    /// The generated Rust source: `Guest` impls plus `*_to_json` / `*_to_str` helpers.
    pub rust: String,
    /// The generated `Cargo.toml` manifest for the component crate.
    pub cargo_toml: String,
    /// The kebab-case names of the generated interfaces, in emission order.
    ///
    /// Pass these to [`crate::rewrite_world_exports`] to update a WIT world's `export` list.
    pub interfaces: Vec<String>,
}
