use crate::{
    Error,
    naming::{is_rust_keyword, is_wit_keyword},
};

/// A WIT package identifier, `namespace:name[@version]`.
///
/// Namespace and package names must be lowercase ASCII kebab-case. WIT and Rust
/// keywords are suffixed with `-api` so later wit-bindgen output remains valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageName {
    namespace: String,
    name: String,
    version: Option<String>,
}

impl PackageName {
    /// Parse and validate a package identity. Prefer a versioned identity.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let (path, version) = match raw.split_once('@') {
            Some((path, version)) => {
                let version = semver::Version::parse(version).map_err(|err| {
                    Error::new(None, "", format!("invalid package version: {err}"))
                })?;
                (path, Some(version.to_string()))
            }
            None => (raw, None),
        };
        let (namespace, name) = path
            .split_once(':')
            .ok_or_else(|| Error::new(None, "", "expected package namespace:name[@version]"))?;
        for part in [namespace, name] {
            if !valid_part(part) {
                return Err(Error::new(
                    None,
                    "",
                    format!("invalid package segment {part:?}"),
                ));
            }
        }
        let module_name = |part: &str| {
            if is_rust_keyword(part) || is_wit_keyword(part) {
                format!("{part}-api")
            } else {
                part.to_owned()
            }
        };
        let package = Self {
            namespace: module_name(namespace),
            name: module_name(name),
            version,
        };
        // Resolve the identity too, so package validation uses the same WIT
        // grammar as generation.
        wit_parser::Resolve::default()
            .push_str(
                "package.wit",
                &format!("{}\nworld client {{}}\n", package.wit_decl()),
            )
            .map_err(|err| Error::new(None, "", format!("invalid package: {err:#}")))?;
        Ok(package)
    }

    /// The validated, Rust-module-safe namespace.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The validated, Rust-module-safe package name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The optional SemVer package version.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub(crate) fn wit_decl(&self) -> String {
        let version = self
            .version
            .as_ref()
            .map_or(String::new(), |v| format!("@{v}"));
        format!("package {}:{}{version};", self.namespace, self.name)
    }
}

fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part.split('-').all(|word| {
            word.starts_with(|c: char| c.is_ascii_lowercase())
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_are_validated_and_module_keywords_renamed() {
        let package = PackageName::parse("self:box@0.1.0").unwrap();
        assert_eq!(package.namespace(), "self-api");
        assert_eq!(package.name(), "box-api");
        assert_eq!(package.version(), Some("0.1.0"));
        assert_eq!(PackageName::parse("example:api").unwrap().version(), None);
        assert_eq!(PackageName::parse("example:gen").unwrap().name(), "gen-api");
        assert!(PackageName::parse("example:api@0.1.0-rc.1").is_ok());
        for invalid in [
            "",
            "missing",
            ":api",
            "example:",
            "a:b:c",
            "a:b@",
            "a:b@nope",
            "a:b@1.0.0; world injected {}",
            "a:b@1.0.0; world injected {} //",
            "a:b@1.0.0;\nworld injected {}\n//",
            "a:b\n",
            "A:b",
            "a:1b",
            "a:b--c",
            "a:résumé",
            "a:b_c",
            "a:b@1.0.0@2.0.0",
        ] {
            assert!(PackageName::parse(invalid).is_err(), "{invalid}");
        }
    }
}
