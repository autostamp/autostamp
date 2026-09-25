//! Render the authentication documentation shared by the generated README and the WIT
//! package comment.
//!
//! A generated component reads its credentials from the host via `wasmcloud:secrets`; the
//! operations never take a token. Both the README and the leading `package` doc comment
//! surface the named secrets a host must provision, using the *same* markdown table rendered
//! here so the two can never drift.

use crate::security::AuthApply;

/// Render the `## Authentication` markdown block — intro prose plus a table of each named
/// secret and how it is applied to requests — for the given resolved schemes.
///
/// Returns `None` when `schemes` is empty, so callers omit the section entirely for components
/// that require no authentication. `schemes` is expected to be deduplicated and ordered by the
/// caller.
pub(crate) fn section(schemes: &[AuthApply]) -> Option<String> {
    if schemes.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Authentication\n\
        \n\
        Reads credentials from the host via `wasmcloud:secrets`; operations never take a token.\n\
        Provision these named secrets:\n\
        \n\
        | Secret name | Applied to each request as |\n\
        | --- | --- |\n",
    );
    for scheme in schemes {
        out.push_str(&format!(
            "| `{}` | {} |\n",
            scheme.secret_key,
            scheme.kind.applied_as()
        ));
    }
    Some(out)
}

/// Render the authentication section as a WIT doc comment: the same block as [`section`], with
/// every line prefixed `///` (blank lines become a bare `///`) so it can lead the generated
/// `package` declaration. Returns `None` when there are no schemes.
pub(crate) fn wit_comment(schemes: &[AuthApply]) -> Option<String> {
    let section = section(schemes)?;
    let mut out = String::new();
    for line in section.trim_end().lines() {
        if line.is_empty() {
            out.push_str("///\n");
        } else {
            out.push_str(&format!("/// {line}\n"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AuthKind;

    fn schemes() -> Vec<AuthApply> {
        vec![
            AuthApply {
                secret_key: "bearerAuth".into(),
                kind: AuthKind::Bearer,
            },
            AuthApply {
                secret_key: "apiKeyHeader".into(),
                kind: AuthKind::ApiKeyHeader {
                    name: "X-API-Key".into(),
                },
            },
        ]
    }

    #[test]
    fn section_tabulates_each_secret_and_its_application() {
        let md = section(&schemes()).unwrap();
        assert!(md.starts_with("## Authentication\n"));
        assert!(md.contains("| Secret name | Applied to each request as |"));
        assert!(md.contains("| `bearerAuth` | `Authorization: Bearer <secret>` |"));
        assert!(md.contains("| `apiKeyHeader` | header `X-API-Key` |"));
    }

    #[test]
    fn empty_schemes_render_nothing() {
        assert!(section(&[]).is_none());
        assert!(wit_comment(&[]).is_none());
    }

    #[test]
    fn wit_comment_prefixes_every_line() {
        let comment = wit_comment(&schemes()).unwrap();
        assert!(comment.starts_with("/// ## Authentication\n"));
        // Blank separator lines become a bare `///`, never a trailing-space `/// `.
        assert!(comment.contains("\n///\n"));
        assert!(comment.contains("/// | `bearerAuth` | `Authorization: Bearer <secret>` |"));
        // Every non-empty line is a doc comment.
        assert!(comment.lines().all(|l| l.starts_with("///")));
    }
}
