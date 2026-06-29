//! Identifier-mangling utilities for WIT and Rust output.

use heck::ToKebabCase;

/// Find a name that is not already `taken`, appending `-2`, `-3`, ... as needed.
pub(crate) fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let candidate = sanitize_wit_name(base);
    if !taken(&candidate) {
        return candidate;
    }
    for i in 2.. {
        let attempt = format!("{candidate}-{i}");
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
    if WIT_KEYWORDS.contains(&k.as_str()) {
        format!("{k}-op")
    } else {
        k
    }
}

/// Convert a kebab-case WIT field name to the Rust identifier wit-bindgen generates.
pub(crate) fn wit_field_to_rust_ident(kebab: &str) -> String {
    // wit-bindgen converts kebab -> snake; rust reserved words get `r#` prefix.
    let snake = kebab.replace('-', "_");
    if RUST_KEYWORDS.contains(&snake.as_str()) {
        format!("r#{snake}")
    } else {
        snake
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

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
];
