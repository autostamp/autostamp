use std::{collections::BTreeSet, fmt};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::{
    Error,
    model::{Schema, Tool},
    naming::Names,
    schema::{Compiler, child_pointer},
};

/// A validated, complete MCP `tools/list` result.
///
/// Construction validates all tool schemas, including unused `$defs`. Tools,
/// properties, definitions and enum values are lowered in sorted wire-name order.
/// Original catalog order and annotations remain available through [`Self::as_json`].
#[derive(Debug, Clone)]
pub struct Catalog {
    pub(crate) tools: Vec<Tool>,
    pub(crate) schemas: Vec<Schema>,
    source: Value,
}

impl Catalog {
    /// Number of tools, including tools with no arguments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the complete result contains no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Original, unmodified catalog, including metadata and schema constraints.
    #[must_use]
    pub fn as_json(&self) -> &Value {
        &self.source
    }
}

/// Parse a complete `tools/list` result, not a JSON-RPC response envelope.
///
/// Any pagination cursor (including an empty string), duplicate tool names, duplicate JSON keys,
/// unsupported schemas and tools requiring task execution are errors.
pub fn parse_catalog(json: &str) -> Result<Catalog, Error> {
    let value: UniqueValue = serde_json::from_str(json)
        .map_err(|err| Error::new(None, "", format!("invalid catalog JSON: {err}")))?;
    from_json_value(value.0)
}

/// Validate an already parsed complete `tools/list` result.
///
/// Use [`parse_catalog`] when duplicate JSON object keys must be detected; a
/// [`Value`] has already discarded that information.
pub fn from_json_value(source: Value) -> Result<Catalog, Error> {
    let root = object(&source, None, "")?;
    let tools = root.get("tools").and_then(Value::as_array).ok_or_else(|| {
        Error::new(
            None,
            "/tools",
            "expected a tools array in a complete tools/list result",
        )
    })?;
    known_keys(root, &["tools", "nextCursor", "_meta"], None, "")?;
    if let Some(cursor) = root.get("nextCursor") {
        cursor.as_str().ok_or_else(|| {
            Error::new(None, "/nextCursor", "expected a string pagination cursor")
        })?;
        return Err(Error::new(
            None,
            "/nextCursor",
            "incomplete tools/list result: collect every page and omit nextCursor before generation",
        ));
    }
    optional_object(root, "_meta", None, "")?;
    let mut ordered = Vec::new();
    let mut originals = BTreeSet::new();
    for (index, value) in tools.iter().enumerate() {
        let pointer = format!("/tools/{index}");
        let tool = object(value, None, &pointer)?;
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::new(
                    None,
                    &child_pointer(&pointer, "name"),
                    "expected a nonempty tool name",
                )
            })?;
        if !originals.insert(name) {
            return Err(Error::new(
                Some(name),
                &child_pointer(&pointer, "name"),
                "duplicate original tool name",
            ));
        }
        ordered.push((name, pointer, tool));
    }
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    let mut names = Names::default();
    for support in ["error", "result-envelope-json", "guest", "tool", "client"] {
        names.reserve(support);
    }
    let function_names: Vec<_> = ordered
        .iter()
        .map(|(name, _, _)| names.allocate(name))
        .collect();
    let mut schemas = Vec::new();
    let mut lowered = Vec::new();
    for ((wire_name, pointer, tool), name) in ordered.into_iter().zip(function_names) {
        validate_metadata(tool, wire_name, &pointer)?;
        let input_pointer = child_pointer(&pointer, "inputSchema");
        let input = tool
            .get("inputSchema")
            .ok_or_else(|| Error::new(Some(wire_name), &input_pointer, "missing inputSchema"))?;
        let input = Compiler::compile_root(
            input,
            wire_name,
            &input_pointer,
            &format!("{name}-arguments"),
            &mut names,
            &mut schemas,
        )?;
        let output = tool
            .get("outputSchema")
            .map(|value| {
                Compiler::compile_root(
                    value,
                    wire_name,
                    &child_pointer(&pointer, "outputSchema"),
                    &format!("{name}-output"),
                    &mut names,
                    &mut schemas,
                )
            })
            .transpose()?;
        let result_name = names.allocate(&format!("{name}-result"));
        lowered.push(Tool {
            wire_name: wire_name.to_owned(),
            name,
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            input,
            output,
            result_name,
        });
    }
    Ok(Catalog {
        tools: lowered,
        schemas,
        source,
    })
}

fn validate_metadata(tool: &Map<String, Value>, name: &str, pointer: &str) -> Result<(), Error> {
    known_keys(
        tool,
        &[
            "name",
            "title",
            "description",
            "inputSchema",
            "outputSchema",
            "annotations",
            "execution",
            "icons",
            "_meta",
        ],
        Some(name),
        pointer,
    )?;
    for key in ["title", "description"] {
        optional_string(tool, key, Some(name), pointer)?;
    }
    optional_object(tool, "_meta", Some(name), pointer)?;
    if let Some(annotations) = optional_object(tool, "annotations", Some(name), pointer)? {
        let path = child_pointer(pointer, "annotations");
        known_keys(
            annotations,
            &[
                "title",
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ],
            Some(name),
            &path,
        )?;
        optional_string(annotations, "title", Some(name), &path)?;
        for key in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            if annotations.get(key).is_some_and(|v| !v.is_boolean()) {
                return Err(Error::new(
                    Some(name),
                    &child_pointer(&path, key),
                    "expected a boolean",
                ));
            }
        }
    }
    if let Some(execution) = optional_object(tool, "execution", Some(name), pointer)? {
        let path = child_pointer(pointer, "execution");
        known_keys(execution, &["taskSupport"], Some(name), &path)?;
        match execution.get("taskSupport") {
            None => {}
            Some(Value::String(s)) if s == "optional" || s == "forbidden" => {}
            Some(Value::String(s)) if s == "required" => {
                return Err(Error::new(
                    Some(name),
                    &child_pointer(&path, "taskSupport"),
                    "tool requires task execution; only ordinary tools/call is supported",
                ));
            }
            Some(_) => {
                return Err(Error::new(
                    Some(name),
                    &child_pointer(&path, "taskSupport"),
                    "expected forbidden, optional, or required",
                ));
            }
        }
    }
    if let Some(icons) = tool.get("icons") {
        let path = child_pointer(pointer, "icons");
        let icons = icons
            .as_array()
            .ok_or_else(|| Error::new(Some(name), &path, "expected an array"))?;
        for (index, icon) in icons.iter().enumerate() {
            let path = child_pointer(&path, &index.to_string());
            let icon = object(icon, Some(name), &path)?;
            known_keys(
                icon,
                &["src", "mimeType", "sizes", "theme"],
                Some(name),
                &path,
            )?;
            if icon.get("src").and_then(Value::as_str).is_none() {
                return Err(Error::new(
                    Some(name),
                    &child_pointer(&path, "src"),
                    "expected a string",
                ));
            }
            optional_string(icon, "mimeType", Some(name), &path)?;
            if let Some(sizes) = icon.get("sizes")
                && !sizes
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string))
            {
                return Err(Error::new(
                    Some(name),
                    &child_pointer(&path, "sizes"),
                    "expected an array of strings",
                ));
            }
            if let Some(theme) = icon.get("theme")
                && !matches!(theme.as_str(), Some("light" | "dark"))
            {
                return Err(Error::new(
                    Some(name),
                    &child_pointer(&path, "theme"),
                    "expected light or dark",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn object<'a>(
    value: &'a Value,
    tool: Option<&str>,
    pointer: &str,
) -> Result<&'a Map<String, Value>, Error> {
    value
        .as_object()
        .ok_or_else(|| Error::new(tool, pointer, "expected an object"))
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    tool: Option<&str>,
    pointer: &str,
) -> Result<Option<&'a Map<String, Value>>, Error> {
    object
        .get(key)
        .map(|value| self::object(value, tool, &child_pointer(pointer, key)))
        .transpose()
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    tool: Option<&str>,
    pointer: &str,
) -> Result<(), Error> {
    if object.get(key).is_some_and(|v| !v.is_string()) {
        return Err(Error::new(
            tool,
            &child_pointer(pointer, key),
            "expected a string",
        ));
    }
    Ok(())
}

fn known_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    tool: Option<&str>,
    pointer: &str,
) -> Result<(), Error> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(Error::new(
                tool,
                &child_pointer(pointer, key),
                "unsupported catalog field",
            ));
        }
    }
    Ok(())
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|n| UniqueValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.to_owned())))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut array = Vec::new();
                while let Some(UniqueValue(value)) = seq.next_element()? {
                    array.push(value);
                }
                Ok(UniqueValue(Value::Array(array)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut object = Map::new();
                while let Some((key, UniqueValue(value))) =
                    map.next_entry::<String, UniqueValue>()?
                {
                    if object.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
                    }
                }
                Ok(UniqueValue(Value::Object(object)))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}
