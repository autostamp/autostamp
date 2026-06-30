//! The [`PackageName`] WIT package identifier.

use anyhow::{Context, Result, bail};

/// A WIT package identifier: `namespace:name` with an optional `@version`.
#[derive(Debug)]
pub struct PackageName {
    /// The package namespace: the part before `:`.
    pub namespace: String,
    /// The package name: the part after `:`.
    pub name: String,
    /// The optional package version: the part after `@`, if present.
    pub version: Option<String>,
}

impl PackageName {
    /// Parse a `namespace:name[@version]` identifier such as `incidentio:api@0.1.0`.
    pub fn parse(raw: &str) -> Result<PackageName> {
        let (path, version) = match raw.split_once('@') {
            Some((path, version)) => (path, Some(version.to_string())),
            None => (raw, None),
        };
        let (namespace, name) = path.split_once(':').with_context(|| {
            format!("invalid package `{raw}`: expected `namespace:name[@version]`")
        })?;
        if namespace.is_empty() || name.is_empty() {
            bail!("invalid package `{raw}`: namespace and name must both be non-empty");
        }
        Ok(PackageName {
            namespace: namespace.to_string(),
            name: name.to_string(),
            version,
        })
    }

    /// Render the `package ...;` declaration line for a `.wit` file.
    pub(crate) fn wit_decl(&self) -> String {
        match &self.version {
            Some(version) => format!("package {}:{}@{};", self.namespace, self.name, version),
            None => format!("package {}:{};", self.namespace, self.name),
        }
    }

    /// Render the Rust module path (`namespace::name`) that wit-bindgen generates.
    pub(crate) fn rust_path(&self) -> String {
        format!(
            "{}::{}",
            self.namespace.replace('-', "_"),
            self.name.replace('-', "_")
        )
    }
}
