//! Narrow, MCP-local counterparts of the OpenAPI generator's naming helpers.

use std::collections::BTreeSet;

use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};

pub(crate) fn sanitize_wit_name(raw: &str) -> String {
    let ascii: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let words = ascii.to_kebab_case();
    let mut parts = Vec::new();
    for word in words.split('-').filter(|word| !word.is_empty()) {
        parts.push(if word.starts_with(|c: char| c.is_ascii_digit()) {
            format!("v{word}")
        } else {
            word.to_owned()
        });
    }
    let name = if parts.is_empty() {
        "x".to_owned()
    } else {
        parts.join("-")
    };
    if is_wit_keyword(&name) || is_rust_keyword(&name) {
        format!("{name}-value")
    } else {
        name
    }
}

pub(crate) fn rust_ident(name: &str) -> String {
    if is_rust_keyword(name) {
        format!("{name}_")
    } else {
        name.to_snake_case()
    }
}

pub(crate) fn rust_type_name(name: &str) -> String {
    if name == "guest" {
        "Guest_".into()
    } else {
        name.to_upper_camel_case()
    }
}

pub(crate) fn rust_case_name(name: &str) -> String {
    name.to_upper_camel_case()
}

pub(crate) fn is_rust_keyword(name: &str) -> bool {
    RUST_KEYWORDS.contains(&name)
}

pub(crate) fn is_wit_keyword(name: &str) -> bool {
    WIT_KEYWORDS.contains(&name)
}

/// One scope guards WIT names and both Rust binding namespaces.
#[derive(Debug, Default)]
pub(crate) struct Names {
    wit: BTreeSet<String>,
    rust_values: BTreeSet<String>,
    rust_types: BTreeSet<String>,
    rust_cases: BTreeSet<String>,
}

impl Names {
    pub(crate) fn reserve(&mut self, name: &str) {
        self.wit.insert(name.to_owned());
        self.rust_values.insert(rust_ident(name));
        self.rust_types.insert(rust_type_name(name));
        self.rust_cases.insert(rust_case_name(name));
    }

    pub(crate) fn allocate(&mut self, raw: &str) -> String {
        let base = sanitize_wit_name(raw);
        let mut name = base.clone();
        let mut suffix = 2;
        while self.wit.contains(&name)
            || self.rust_values.contains(&rust_ident(&name))
            || self.rust_types.contains(&rust_type_name(&name))
            || self.rust_cases.contains(&rust_case_name(&name))
        {
            name = format!("{base}-v{suffix}");
            suffix += 1;
        }
        self.reserve(&name);
        name
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
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen", "union",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_names_are_ascii_and_keyword_safe() {
        for (raw, expected) in [
            ("HelloWorld", "hello-world"),
            ("v1/123", "v1-v123"),
            ("", "x"),
            ("☃", "x"),
            ("déjà-vu", "d-j-vu"),
            ("type", "type-value"),
            ("self", "self-value"),
            ("gen", "gen-value"),
        ] {
            assert_eq!(sanitize_wit_name(raw), expected);
        }
    }

    #[test]
    fn rust_binding_names_follow_wit_bindgen() {
        assert_eq!(rust_ident("match"), "match_");
        assert_eq!(rust_ident("user-id"), "user_id");
        assert_eq!(rust_type_name("guest"), "Guest_");
        assert_eq!(rust_case_name("guest"), "Guest");
        assert_eq!(rust_type_name("user-profile"), "UserProfile");
    }

    #[test]
    fn collisions_include_rust_type_names_and_reserved_support_names() {
        let mut names = Names::default();
        names.reserve("error");
        assert_eq!(names.allocate("error"), "error-v2");
        assert_eq!(names.allocate("fooBar"), "foo-bar");
        assert_eq!(names.allocate("foo_bar"), "foo-bar-v2");
        let mut names = Names::default();
        names.reserve("FOO-BAR");
        assert_eq!(rust_type_name("FOO-BAR"), rust_type_name("foo-bar"));
        assert_eq!(names.allocate("foo-bar"), "foo-bar-v2");
        names.reserve("GUEST");
        assert_eq!(names.allocate("guest"), "guest-v2");
    }
}
