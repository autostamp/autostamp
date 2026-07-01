//! Identifier-mangling utilities for WIT and Rust output.

use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};

/// Find a name that is not already `taken`, appending `-2`, `-3`, ... as needed.
pub(crate) fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let candidate = sanitize_wit_name(base);
    if !taken(&candidate) {
        return candidate;
    }
    for i in 2.. {
        // Re-sanitize so the numeric suffix doesn't introduce a bare-digit segment
        // (`foo-2` is invalid WIT); sanitizing turns it into `foo-v2`.
        let attempt = sanitize_wit_name(&format!("{candidate}-{i}"));
        if !taken(&attempt) {
            return attempt;
        }
    }
    unreachable!()
}

/// Turn an arbitrary string into a valid WIT identifier.
pub(crate) fn sanitize_wit_name(s: &str) -> String {
    // WIT identifiers are dash-separated segments; each segment must start with an
    // ASCII letter. heck's kebab_case happily produces segments like `ping-1` where
    // the `1` is a leading digit. Prepend `v` to such segments.
    let mut segments = Vec::new();
    for seg in s.to_kebab_case().split('-') {
        if seg.is_empty() {
            continue;
        }
        let first = seg.chars().next().unwrap();
        if first.is_ascii_digit() {
            segments.push(format!("v{seg}"));
        } else {
            segments.push(seg.to_string());
        }
    }
    let k = segments.join("-");
    // A WIT identifier can't be empty: when every segment was stripped (e.g. the source
    // was `/`, punctuation, or already empty) fall back to a placeholder so callers always
    // get a usable identifier. Callers that need uniqueness route this through `unique_name`.
    let k = if k.is_empty() { "x".to_string() } else { k };
    if WIT_KEYWORDS.contains(&k.as_str()) {
        format!("{k}-op")
    } else {
        k
    }
}

/// Synthesize a deterministic `operationId` for an operation that lacks one, derived from its
/// HTTP method and path. `operationId` is optional in OpenAPI, but the generator needs one to
/// name each function; the method+path pair is unique within a document, so the result is
/// collision-free. The output is kebab-cased downstream (e.g. `get /pets/{id}` becomes
/// `get-pets-id`).
pub(crate) fn synthesize_operation_id(method: &str, path: &str) -> String {
    let mut id = String::from(method);
    for segment in path.split('/') {
        let segment = segment.trim_matches(|c| c == '{' || c == '}');
        if !segment.is_empty() {
            id.push(' ');
            id.push_str(segment);
        }
    }
    id
}

/// Map a WIT identifier to the **value-level** Rust identifier wit-bindgen generates
/// (struct fields, function/method names). Mirrors wit-bindgen 0.41's `to_rust_ident`
/// verbatim: Rust keywords get a trailing underscore (`match` -> `match_`), everything
/// else is snake-cased. wit-bindgen never emits raw (`r#`) identifiers, so neither do we;
/// a mismatch here surfaces as `E0609 no field` or bare-keyword parse errors at build time.
pub(crate) fn to_rust_ident(name: &str) -> String {
    match name {
        // Escape Rust keywords.
        // Source: https://doc.rust-lang.org/reference/keywords.html
        "as" => "as_".into(),
        "break" => "break_".into(),
        "const" => "const_".into(),
        "continue" => "continue_".into(),
        "crate" => "crate_".into(),
        "else" => "else_".into(),
        "enum" => "enum_".into(),
        "extern" => "extern_".into(),
        "false" => "false_".into(),
        "fn" => "fn_".into(),
        "for" => "for_".into(),
        "if" => "if_".into(),
        "impl" => "impl_".into(),
        "in" => "in_".into(),
        "let" => "let_".into(),
        "loop" => "loop_".into(),
        "match" => "match_".into(),
        "mod" => "mod_".into(),
        "move" => "move_".into(),
        "mut" => "mut_".into(),
        "pub" => "pub_".into(),
        "ref" => "ref_".into(),
        "return" => "return_".into(),
        "self" => "self_".into(),
        "static" => "static_".into(),
        "struct" => "struct_".into(),
        "super" => "super_".into(),
        "trait" => "trait_".into(),
        "true" => "true_".into(),
        "type" => "type_".into(),
        "unsafe" => "unsafe_".into(),
        "use" => "use_".into(),
        "where" => "where_".into(),
        "while" => "while_".into(),
        "async" => "async_".into(),
        "await" => "await_".into(),
        "dyn" => "dyn_".into(),
        "abstract" => "abstract_".into(),
        "become" => "become_".into(),
        "box" => "box_".into(),
        "do" => "do_".into(),
        "final" => "final_".into(),
        "macro" => "macro_".into(),
        "override" => "override_".into(),
        "priv" => "priv_".into(),
        "typeof" => "typeof_".into(),
        "unsized" => "unsized_".into(),
        "virtual" => "virtual_".into(),
        "yield" => "yield_".into(),
        "try" => "try_".into(),
        s => s.to_snake_case(),
    }
}

/// Map a WIT identifier to the **type-level** Rust name wit-bindgen generates (records,
/// variants, enums, flags — the type's own name). Mirrors wit-bindgen 0.41's free function
/// `to_upper_camel_case`: the WIT name `guest` is remapped to `Guest_` (because `Guest` is
/// reserved for the traits generated by exported interfaces); everything else is
/// upper-camel-cased.
///
/// NOTE: this remap applies only to *type names*. Enum/variant **case** names use
/// [`to_rust_case_name`] instead — wit-bindgen names those with heck's `to_upper_camel_case`
/// method directly (no `guest` special case).
pub(crate) fn to_rust_type_name(name: &str) -> String {
    match name {
        "guest" => "Guest_".into(),
        s => s.to_upper_camel_case(),
    }
}

/// Map a WIT enum/variant **case** name to the Rust variant identifier wit-bindgen generates.
/// Mirrors wit-bindgen 0.41, which emits enum/variant cases via heck's `to_upper_camel_case`
/// method directly (`case.name.to_upper_camel_case()`) — crucially *without* the `guest` →
/// `Guest_` remap that [`to_rust_type_name`] applies to type names. So a case named `guest`
/// becomes `Guest`, not `Guest_`.
pub(crate) fn to_rust_case_name(name: &str) -> String {
    name.to_upper_camel_case()
}

/// Return whether `s` is a Rust keyword (the exact set wit-bindgen escapes). Used to detect
/// WIT package names that would break wit-bindgen's module generation; see
/// [`sanitize_package_name`].
pub(crate) fn is_rust_keyword(s: &str) -> bool {
    // `to_rust_ident` maps a keyword `kw` to `kw_` and every other single token to its
    // snake-case form (identity for an already-snake token), so `s` is a keyword iff
    // escaping it appends exactly a trailing underscore. Keywords never contain `_`, so a
    // multi-word snake string can never spuriously match.
    to_rust_ident(s) == format!("{s}_")
}

/// Sanitize a WIT package *name* so it survives wit-bindgen's Rust code generation.
///
/// wit-bindgen names the package module `name.to_snake_case()` **without** keyword-escaping it
/// (`wit_bindgen_core::name_package_module`), so a package literally named `box` produces an
/// uncompilable `pub mod box { … }`. When the snake-cased name is a Rust keyword we append
/// `-api` (e.g. `box` -> `box-api`): the published OCI name stays readable and the generated
/// module (`box_api`) is valid. Non-keyword names are returned unchanged, and the transform is
/// idempotent (`box-api` is not a keyword).
pub(crate) fn sanitize_package_name(name: &str) -> String {
    if is_rust_keyword(&name.to_snake_case()) {
        format!("{name}-api")
    } else {
        name.to_string()
    }
}

const WIT_KEYWORDS: &[&str] = &[
    "record",
    "variant",
    "enum",
    "flags",
    "type",
    "interface",
    "func",
    "world",
    "package",
    "use",
    "import",
    "export",
    "include",
    "as",
    "from",
    "with",
    "result",
    "option",
    "list",
    "tuple",
    "string",
    "bool",
    "char",
    "u8",
    "u16",
    "u32",
    "u64",
    "s8",
    "s16",
    "s32",
    "s64",
    "f32",
    "f64",
    "resource",
    "static",
    "constructor",
    "borrow",
    "own",
    "stream",
    "future",
    "error-context",
    "async",
];

#[cfg(test)]
mod tests {
    use super::{to_rust_case_name, to_rust_ident, to_rust_type_name};

    // r[verify naming.rust-ident.keyword-suffix]
    // wit-bindgen 0.41 escapes Rust keywords with a trailing underscore, never `r#`.
    // The generator must predict the exact same identifiers or struct field access and
    // method names won't line up (E0609 / bare-keyword parse errors).
    #[test]
    fn rust_ident_maps_keywords_to_trailing_underscore() {
        assert_eq!(to_rust_ident("match"), "match_");
        assert_eq!(to_rust_ident("type"), "type_");
        assert_eq!(to_rust_ident("self"), "self_");
        // `box` is the case that produced a bare `fn box(..)` parse error.
        assert_eq!(to_rust_ident("box"), "box_");
        assert_eq!(to_rust_ident("async"), "async_");
    }

    #[test]
    fn rust_ident_snake_cases_non_keywords() {
        assert_eq!(to_rust_ident("user-id"), "user_id");
        assert_eq!(to_rust_ident("list-tv-shows"), "list_tv_shows");
        // A multi-word name that merely contains a keyword is not a keyword itself.
        assert_eq!(to_rust_ident("match-id"), "match_id");
        assert_eq!(to_rust_ident("plain"), "plain");
    }

    // r[verify naming.rust-type.guest-remap]
    // wit-bindgen remaps the WIT type name `guest` to `Guest_` because `Guest` is reserved
    // for the traits it generates for exported interfaces.
    #[test]
    fn rust_type_name_remaps_guest() {
        assert_eq!(to_rust_type_name("guest"), "Guest_");
        assert_eq!(to_rust_type_name("box"), "Box");
        assert_eq!(to_rust_type_name("user-profile"), "UserProfile");
    }

    // r[verify naming.rust-case.no-guest-remap]
    // Enum/variant CASE names are NOT type names: wit-bindgen emits them with heck's
    // `to_upper_camel_case` method directly, with no `guest` -> `Guest_` remap. A case named
    // `guest` therefore becomes `Guest` (the netlify `list-sites-filter-enum::guest` case);
    // applying the type-name remap here produced `Guest_`, an E0599 unknown-variant error.
    #[test]
    fn rust_case_name_does_not_remap_guest() {
        assert_eq!(to_rust_case_name("guest"), "Guest");
        assert_eq!(to_rust_case_name("all"), "All");
        assert_eq!(to_rust_case_name("read-write"), "ReadWrite");
    }

    // r[verify naming.package.keyword-rename]
    // wit-bindgen does not keyword-escape the package module name, so a package named after a
    // Rust keyword (`box`) must be renamed at the source to stay buildable + publishable.
    #[test]
    fn sanitize_package_name_renames_keyword_packages() {
        use super::{is_rust_keyword, sanitize_package_name};
        assert!(is_rust_keyword("box"));
        assert!(is_rust_keyword("match"));
        assert!(!is_rust_keyword("github"));
        assert!(!is_rust_keyword("box_api"));
        assert_eq!(sanitize_package_name("box"), "box-api");
        // Non-keyword names are untouched, and the rename is idempotent.
        assert_eq!(sanitize_package_name("github"), "github");
        assert_eq!(sanitize_package_name("box-api"), "box-api");
    }
}
