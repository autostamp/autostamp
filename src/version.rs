//! Derive a publishable component version from a generated package and its source document.
//!
//! Each generated component carries two distinct versions:
//!
//! * a **base** SemVer (`0.1.0` by default), set when the generator is invoked
//!   (`autostamp:nasa@0.1.0`), and stamped onto the WIT package decl and `Cargo.toml`; and
//! * a **provenance marker** attached as SemVer build metadata: the provider name followed by
//!   the OpenAPI document's `info.version` (`0.1.0+nasa-1.0.0`), so the published artifact
//!   records both which provider and exactly which schema revision the bindings came from.
//!
//! Only `[package].version` in `wasm.toml` carries the `+metadata`. Build metadata is ignored
//! for SemVer precedence, so it never changes how the package resolves — it is purely a
//! provenance marker.

/// The maximum length of the composed build metadata (`<name>-<schema>`). OCI tags are capped
/// at 128 characters (`<base>_<metadata>`), so we keep the metadata comfortably short.
const MAX_METADATA_LEN: usize = 64;

/// Compose the publishable `[package].version`: the `base` SemVer with build metadata that
/// records the provider and the source document's schema version.
///
/// `base` is used verbatim (it is already valid SemVer). When `info_version` sanitizes to a
/// non-empty build-metadata string, the metadata `<name>-<schema>` is appended after `+`
/// (e.g. `0.1.0+vonage-1.11.8`) — the `name` prefix makes each component's metadata
/// self-identifying. When `info_version` yields nothing usable there is no schema revision to
/// record, so `base` is returned unchanged.
pub(crate) fn publish_version(base: &str, name: &str, info_version: &str) -> String {
    match schema_build_metadata(info_version) {
        Some(schema) => {
            let meta = cap_metadata(format!("{}-{schema}", sanitize_identifier(name)));
            format!("{base}+{meta}")
        }
        None => base.to_string(),
    }
}

/// Sanitize an arbitrary OpenAPI `info.version` into a valid SemVer build-metadata string.
///
/// SemVer build metadata is a dot-separated list of identifiers, each a non-empty run of
/// `[0-9A-Za-z-]`. Real-world `info.version` values are wildly inconsistent (`1.0`,
/// `2022-11-28`, `v3`, `1.0 beta`, ...), so we map them onto that grammar: `.` separates
/// identifiers, every other non-`[0-9A-Za-z-]` character becomes `-`, runs of `-` are
/// collapsed, and empty identifiers are dropped. Returns `None` when nothing usable remains.
fn schema_build_metadata(info_version: &str) -> Option<String> {
    let identifiers: Vec<String> = info_version
        .split('.')
        .map(sanitize_identifier)
        .filter(|id| !id.is_empty())
        .collect();

    if identifiers.is_empty() {
        None
    } else {
        Some(identifiers.join("."))
    }
}

/// Truncate composed build metadata to [`MAX_METADATA_LEN`] and strip any leading or trailing
/// separator, which truncation (or an empty name) can otherwise leave behind as an invalid
/// identifier.
fn cap_metadata(meta: String) -> String {
    // Metadata is pure ASCII (every identifier char is `[0-9A-Za-z-]`), so a byte-index
    // truncation never splits a multibyte character.
    let capped = if meta.len() > MAX_METADATA_LEN {
        &meta[..MAX_METADATA_LEN]
    } else {
        meta.as_str()
    };
    capped.trim_matches(['.', '-']).to_string()
}

/// Map a single dot-delimited segment onto one SemVer build-metadata identifier: keep
/// `[0-9A-Za-z-]`, turn every other character into `-`, collapse repeated `-`, and trim
/// leading/trailing `-`.
fn sanitize_identifier(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut last_dash = false;
    for ch in segment.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '-' {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(mapped);
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::{publish_version, schema_build_metadata};

    #[test]
    fn passes_through_clean_versions() {
        assert_eq!(
            schema_build_metadata("2022-11-28").as_deref(),
            Some("2022-11-28")
        );
        assert_eq!(schema_build_metadata("1.0").as_deref(), Some("1.0"));
        assert_eq!(schema_build_metadata("1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(schema_build_metadata("v3").as_deref(), Some("v3"));
    }

    #[test]
    fn sanitizes_invalid_characters() {
        // Spaces and other punctuation collapse to a single dash per run.
        assert_eq!(
            schema_build_metadata("1.0 beta").as_deref(),
            Some("1.0-beta")
        );
        // A slash is not a separator: it is an invalid char and collapses to a dash.
        assert_eq!(schema_build_metadata("2024/03").as_deref(), Some("2024-03"));
        assert_eq!(schema_build_metadata("a  b").as_deref(), Some("a-b"));
        // A `+` (which is itself illegal in build metadata) is rewritten to `-`.
        assert_eq!(schema_build_metadata("1.0+exp").as_deref(), Some("1.0-exp"));
    }

    #[test]
    fn drops_empty_identifiers_and_unusable_input() {
        assert_eq!(schema_build_metadata(""), None);
        assert_eq!(schema_build_metadata("   "), None);
        assert_eq!(schema_build_metadata("..."), None);
        // Leading/trailing dots produce empty identifiers that are dropped.
        assert_eq!(schema_build_metadata(".1.0.").as_deref(), Some("1.0"));
    }

    #[test]
    fn truncates_overlong_metadata_without_trailing_separator() {
        let long = "a".repeat(100);
        let version = publish_version("0.1.0", "vonage", &long);
        let meta = version.strip_prefix("0.1.0+").expect("has metadata");
        assert_eq!(meta.len(), 64);
        assert!(meta.starts_with("vonage-"));
        assert!(!meta.ends_with('-') && !meta.ends_with('.'));
    }

    #[test]
    fn publish_version_prefixes_metadata_with_provider_name() {
        assert_eq!(
            publish_version("0.1.0", "vonage", "1.11.8"),
            "0.1.0+vonage-1.11.8"
        );
        assert_eq!(
            publish_version("0.1.0", "github", "2022-11-28"),
            "0.1.0+github-2022-11-28"
        );
        // A renamed keyword package keeps its published name (`box-api`) in the metadata prefix.
        assert_eq!(
            publish_version("0.1.0", "box-api", "1.0"),
            "0.1.0+box-api-1.0"
        );
    }

    #[test]
    fn publish_version_falls_back_to_base_when_unusable() {
        // With no usable schema version there is no revision to record, so the provider prefix
        // is dropped too and the base is returned clean.
        assert_eq!(publish_version("0.1.0", "vonage", ""), "0.1.0");
        assert_eq!(publish_version("0.1.0", "vonage", "   "), "0.1.0");
    }
}
