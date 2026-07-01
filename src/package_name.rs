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
            // Rename packages whose name is a Rust keyword (e.g. `box`): wit-bindgen emits an
            // uncompilable `pub mod <name>` for them. See `naming::sanitize_package_name`.
            name: crate::naming::sanitize_package_name(name),
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

    /// Render the Rust module path (`namespace::name`) that wit-bindgen generates for
    /// this package's exported interfaces, mirroring wit-bindgen's `compute_module_path` /
    /// `name_package_module`: the namespace segment is escaped with `to_rust_ident`, while
    /// the package-name segment is only snake-cased (wit-bindgen does **not** keyword-escape
    /// it — a package whose name is a Rust keyword, like `box`, is a wit-bindgen limitation).
    pub(crate) fn rust_path(&self) -> String {
        use heck::ToSnakeCase;
        format!(
            "{}::{}",
            crate::naming::to_rust_ident(&self.namespace),
            self.name.to_snake_case()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::PackageName;

    // r[verify package.name.keyword-rename]
    // A package whose name is a Rust keyword is renamed (`box` -> `box-api`) so wit-bindgen's
    // generated `pub mod` is valid; this flows into the WIT decl, OCI registry, and rust path.
    #[test]
    fn parse_renames_rust_keyword_package_names() {
        let pkg = PackageName::parse("autostamp:box@0.1.0").unwrap();
        assert_eq!(pkg.name, "box-api");
        assert_eq!(pkg.wit_decl(), "package autostamp:box-api@0.1.0;");
        assert_eq!(pkg.rust_path(), "autostamp::box_api");
    }

    #[test]
    fn parse_leaves_non_keyword_names_unchanged() {
        let pkg = PackageName::parse("autostamp:github@0.1.0").unwrap();
        assert_eq!(pkg.name, "github");
        assert_eq!(pkg.rust_path(), "autostamp::github");
    }
}
