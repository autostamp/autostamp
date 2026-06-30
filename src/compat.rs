//! Pre-processing passes that normalize a raw OpenAPI document (as a [`serde_json::Value`])
//! into a shape the `openapiv3` parser and the schema resolver accept.
//!
//! These run inside [`from_json_value`](crate::from_json_value), after any Swagger 2.0
//! conversion and before deserialization into the typed model.

use serde_json::Value;

/// A safety net against pathological self-referential documents: stop expanding once a single
/// reference chain grows past this many hops. Real specs nest only a handful deep.
const MAX_REF_DEPTH: usize = 128;

/// Inline every JSON-Pointer `$ref` that does not target `#/components/...`.
///
/// `openapiv3` (and our [`SchemaCtx`](crate::schema_ctx::SchemaCtx)) only resolve references
/// into `#/components`. Some published specs — notably DigitalOcean — instead define schemas,
/// headers, and responses inline under path items and reuse them via whole-document pointers
/// such as `#/paths/~1v2~1account~1keys/get/responses/200/.../items`. The resolver rejects
/// those with `unsupported ref`.
///
/// This pass resolves each such pointer against the document and replaces the `{"$ref": ...}`
/// node with a deep copy of its target, so the parser only ever sees concrete objects.
/// `#/components/...` references are left untouched (the parser resolves them natively), and
/// external/URL references we cannot resolve in-document are left as-is.
///
/// References are resolved against an immutable snapshot of the original document, so chains of
/// references (a target that itself contains another non-local `$ref`) expand transitively while
/// cycles are broken by the active-pointer guard.
pub(crate) fn inline_nonlocal_refs(doc: &mut Value) {
    let root = doc.clone();
    let mut active: Vec<String> = Vec::new();
    inline_node(doc, &root, &mut active, 0);
}

fn inline_node(node: &mut Value, root: &Value, active: &mut Vec<String>, depth: usize) {
    if depth > MAX_REF_DEPTH {
        return;
    }

    if let Some(pointer) = nonlocal_ref_pointer(node) {
        // A reference that is already being expanded higher up the stack is a cycle; leaving the
        // node in place lets the parser report it rather than looping forever.
        if active.iter().any(|active_ptr| active_ptr == &pointer) {
            return;
        }
        let resolved = pointer
            .strip_prefix('#')
            .map(percent_decode)
            .and_then(|json_pointer| root.pointer(&json_pointer).cloned());
        if let Some(mut expanded) = resolved {
            active.push(pointer);
            inline_node(&mut expanded, root, active, depth + 1);
            active.pop();
            *node = expanded;
        }
        return;
    }

    match node {
        Value::Object(map) => {
            for child in map.values_mut() {
                inline_node(child, root, active, depth + 1);
            }
        }
        Value::Array(items) => {
            for child in items {
                inline_node(child, root, active, depth + 1);
            }
        }
        _ => {}
    }
}

/// If `node` is a JSON Reference object (`{"$ref": "#/..."}`) whose pointer targets the document
/// itself but *not* `#/components/`, return that pointer (including the leading `#`). Returns
/// `None` for component references, external/URL references, and non-reference objects.
fn nonlocal_ref_pointer(node: &Value) -> Option<String> {
    let reference = node.as_object()?.get("$ref")?.as_str()?;
    if !reference.starts_with("#/") || reference.starts_with("#/components/") {
        return None;
    }
    Some(reference.to_owned())
}

/// Percent-decode a URI fragment (the part after `#`) into a JSON Pointer.
///
/// A `$ref` fragment is a URI fragment, so reserved characters in path keys are percent-encoded —
/// DigitalOcean writes `/v2/apps/{app_id}` as `~1v2~1apps~1%7Bapp_id%7D`. RFC 6901's `~0`/`~1`
/// escapes (which carry no `%`) are left intact for [`serde_json::Value::pointer`] to handle.
/// Invalid `%`-sequences are passed through unchanged.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&byte) = bytes.get(i) {
        if let Some(decoded) = decode_percent_octet(bytes, i) {
            out.push(decoded);
            i += 3;
        } else {
            out.push(byte);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode a `%XX` escape starting at `i`, returning the byte it denotes, or `None` when `bytes`
/// does not hold a valid percent-escape at that position.
fn decode_percent_octet(bytes: &[u8], i: usize) -> Option<u8> {
    if bytes.get(i).copied() != Some(b'%') {
        return None;
    }
    let hi = hex_digit(bytes.get(i + 1).copied()?)?;
    let lo = hex_digit(bytes.get(i + 2).copied()?)?;
    Some(hi * 16 + lo)
}

/// Value of a single ASCII hex digit, or `None` if `byte` is not `0-9`/`a-f`/`A-F`.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inlines_pointer_into_paths() {
        let mut doc = json!({
            "paths": {
                "/things": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": { "type": "string" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "other": {
                "$ref": "#/paths/~1things/get/responses/200/content/application~1json/schema"
            }
        });
        inline_nonlocal_refs(&mut doc);
        assert_eq!(doc.pointer("/other"), Some(&json!({ "type": "string" })));
    }

    #[test]
    fn leaves_component_refs_untouched() {
        let mut doc = json!({
            "components": { "schemas": { "Foo": { "type": "object" } } },
            "here": { "$ref": "#/components/schemas/Foo" }
        });
        inline_nonlocal_refs(&mut doc);
        assert_eq!(
            doc.pointer("/here"),
            Some(&json!({ "$ref": "#/components/schemas/Foo" }))
        );
    }

    #[test]
    fn resolves_chained_references_transitively() {
        let mut doc = json!({
            "paths": {
                "/a": { "x": { "$ref": "#/paths/~1b/y" } },
                "/b": { "y": { "type": "integer" } }
            },
            "use": { "$ref": "#/paths/~1a/x" }
        });
        inline_nonlocal_refs(&mut doc);
        assert_eq!(doc.pointer("/use"), Some(&json!({ "type": "integer" })));
    }

    #[test]
    fn breaks_reference_cycles_without_looping() {
        let mut doc = json!({
            "paths": {
                "/a": { "self": { "$ref": "#/paths/~1a/self" } }
            },
            "use": { "$ref": "#/paths/~1a/self" }
        });
        // The cycle guard must keep this from recursing forever; the unresolved ref is left as-is.
        inline_nonlocal_refs(&mut doc);
        assert_eq!(
            doc.pointer("/use"),
            Some(&json!({ "$ref": "#/paths/~1a/self" }))
        );
    }

    #[test]
    fn resolves_percent_encoded_pointers() {
        // DigitalOcean encodes `{` / `}` in templated path keys as `%7B` / `%7D`.
        let mut doc = json!({
            "paths": {
                "/v2/apps/{app_id}": { "schema": { "type": "object" } }
            },
            "use": { "$ref": "#/paths/~1v2~1apps~1%7Bapp_id%7D/schema" }
        });
        inline_nonlocal_refs(&mut doc);
        assert_eq!(doc.pointer("/use"), Some(&json!({ "type": "object" })));
    }

    #[test]
    fn leaves_external_refs_untouched() {
        let mut doc = json!({ "here": { "$ref": "common.yaml#/Foo" } });
        inline_nonlocal_refs(&mut doc);
        assert_eq!(
            doc.pointer("/here"),
            Some(&json!({ "$ref": "common.yaml#/Foo" }))
        );
    }
}
