use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::{
    Error,
    catalog::object,
    model::{Additional, EnumCase, Kind, Number, Property, Schema, SchemaId},
    naming::Names,
};

const ANNOTATIONS: &[&str] = &[
    "title",
    "description",
    "$comment",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
];
const CONSTRAINTS: &[&str] = &[
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];
const SHAPE: &[&str] = &[
    "type",
    "enum",
    "properties",
    "required",
    "additionalProperties",
    "items",
];

pub(crate) fn child_pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

#[derive(Debug)]
pub(crate) struct Compiler<'a> {
    root: &'a Value,
    tool: &'a str,
    base: &'a str,
    names: &'a mut Names,
    schemas: &'a mut Vec<Schema>,
    cache: BTreeMap<String, SchemaId>,
    active: BTreeSet<String>,
    pending_defs: BTreeMap<String, String>,
}

impl<'a> Compiler<'a> {
    pub(crate) fn compile_root(
        root: &'a Value,
        tool: &'a str,
        base: &'a str,
        name: &str,
        names: &'a mut Names,
        schemas: &'a mut Vec<Schema>,
    ) -> Result<SchemaId, Error> {
        let value = object(root, Some(tool), base)?;
        if value.get("type").and_then(Value::as_str) != Some("object") {
            return Err(Error::new(
                Some(tool),
                &child_pointer(base, "type"),
                "MCP inputSchema and outputSchema must declare type: object",
            ));
        }
        let mut compiler = Self {
            root,
            tool,
            base,
            names,
            schemas,
            cache: BTreeMap::new(),
            active: BTreeSet::new(),
            pending_defs: BTreeMap::new(),
        };
        let id = compiler.compile("", name)?;
        while let Some((path, name)) = compiler.pending_defs.pop_first() {
            compiler.compile(&path, &name)?;
        }
        Ok(id)
    }

    fn error(&self, path: &str, message: impl Into<String>) -> Error {
        Error::new(Some(self.tool), &format!("{}{path}", self.base), message)
    }

    fn compile(&mut self, path: &str, suggested: &str) -> Result<SchemaId, Error> {
        if let Some(id) = self.cache.get(path) {
            return Ok(*id);
        }
        if !self.active.insert(path.to_owned()) {
            return Err(self.error(path, "recursive local reference"));
        }
        if self.active.len() > 128 || self.schemas.len() >= 10_000 {
            return Err(self.error(
                path,
                "schema complexity limit exceeded (128 levels / 10000 types)",
            ));
        }
        let value = self
            .root
            .pointer(path)
            .ok_or_else(|| self.error(path, "missing schema"))?;
        let schema = object(value, Some(self.tool), &format!("{}{path}", self.base))?;
        self.audit(schema, path)?;
        let name = self.names.allocate(suggested);
        let (kind, nullable) = if let Some(reference) = schema.get("$ref") {
            let ref_path = child_pointer(path, "$ref");
            for key in schema.keys() {
                if SHAPE.contains(&key.as_str()) || CONSTRAINTS.contains(&key.as_str()) {
                    return Err(self.error(
                        &child_pointer(path, key),
                        "structural or validation siblings of $ref are unsupported",
                    ));
                }
            }
            let reference = reference
                .as_str()
                .ok_or_else(|| self.error(&ref_path, "expected a string"))?;
            let target = self.reference_target(reference, &ref_path)?;
            if self.active.contains(&target) {
                return Err(self.error(
                    &ref_path,
                    format!("recursive local reference {reference:?}"),
                ));
            }
            let id = self.compile(&target, &format!("{name}-target"))?;
            (Kind::Ref(id), false)
        } else {
            let (ty, mut nullable) = self.schema_type(schema, path)?;
            self.check_applicability(schema, &ty, path)?;
            let kind = if let Some(values) = schema.get("enum") {
                if ty != "string" {
                    return Err(self.error(
                        &child_pointer(path, "enum"),
                        "only string enums are supported",
                    ));
                }
                let (cases, includes_null) = self.enum_cases(values, path, nullable)?;
                nullable = includes_null;
                Kind::Enum(cases)
            } else {
                match ty.as_str() {
                    "boolean" => Kind::Boolean,
                    "string" => Kind::String,
                    "integer" | "number" => Kind::Number(self.number(schema, &ty, path)?),
                    "array" => {
                        let items = child_pointer(path, "items");
                        if !schema.contains_key("items") {
                            return Err(
                                self.error(&items, "homogeneous arrays require an items schema")
                            );
                        }
                        Kind::Array(self.compile(&items, &format!("{name}-item"))?)
                    }
                    "object" => self.object_kind(schema, path, &name)?,
                    _ => {
                        return Err(self.error(
                            &child_pointer(path, "type"),
                            format!("unsupported JSON Schema type {ty:?}"),
                        ));
                    }
                }
            };
            (kind, nullable)
        };
        let value_name = if nullable {
            self.names.allocate(&format!("{name}-value"))
        } else {
            name.clone()
        };
        let constraints = schema
            .iter()
            .filter(|(key, _)| CONSTRAINTS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if self.schemas.len() >= 10_000 {
            return Err(self.error(path, "schema complexity limit exceeded (10000 types)"));
        }
        let id = self.schemas.len();
        self.schemas.push(Schema {
            name: name.clone(),
            value_name,
            pointer: format!("{}{path}", self.base),
            description: schema
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            constraints,
            nullable,
            kind,
        });
        self.cache.insert(path.to_owned(), id);
        self.active.remove(path);
        // Audit unused definitions after the actual type graph is complete:
        // containment of an unused definition is not a recursive dependency.
        if let Some(defs) = schema.get("$defs") {
            let defs_path = child_pointer(path, "$defs");
            let defs = object(defs, Some(self.tool), &format!("{}{defs_path}", self.base))?;
            let mut keys: Vec<_> = defs.keys().collect();
            keys.sort();
            for key in keys {
                self.pending_defs
                    .insert(child_pointer(&defs_path, key), format!("{name}-{key}"));
            }
        }
        Ok(id)
    }

    fn reference_target(&self, reference: &str, path: &str) -> Result<String, Error> {
        let fragment = reference.strip_prefix('#').ok_or_else(|| {
            self.error(path, "external references are unsupported; use local $defs")
        })?;
        let bytes = fragment.as_bytes();
        let mut decoded = Vec::new();
        let mut pos = 0;
        while let Some(byte) = bytes.get(pos) {
            if *byte == b'%' {
                let high = bytes.get(pos + 1).copied().and_then(hex);
                let low = bytes.get(pos + 2).copied().and_then(hex);
                match (high, low) {
                    (Some(high), Some(low)) => decoded.push(high * 16 + low),
                    _ => return Err(self.error(path, "invalid percent escape in reference")),
                }
                pos += 3;
            } else {
                decoded.push(*byte);
                pos += 1;
            }
        }
        let pointer =
            String::from_utf8(decoded).map_err(|_| self.error(path, "reference is not UTF-8"))?;
        let mut tokens = Vec::new();
        for token in pointer
            .strip_prefix('/')
            .ok_or_else(|| {
                self.error(
                    path,
                    "only JSON Pointer references into local $defs are supported",
                )
            })?
            .split('/')
        {
            let mut chars = token.chars();
            let mut decoded = String::new();
            while let Some(c) = chars.next() {
                decoded.push(if c == '~' {
                    match chars.next() {
                        Some('0') => '~',
                        Some('1') => '/',
                        _ => return Err(self.error(path, "invalid JSON Pointer escape")),
                    }
                } else {
                    c
                });
            }
            tokens.push(decoded);
        }
        if tokens.iter().rev().nth(1).map(String::as_str) != Some("$defs") {
            return Err(self.error(path, "references must target an entry in local $defs"));
        }
        let mut route = tokens.iter();
        while let Some(token) = route.next() {
            match token.as_str() {
                "$defs" | "properties" if route.next().is_some() => {}
                "items" | "additionalProperties" => {}
                _ => return Err(self.error(path, "reference traverses a non-schema location")),
            }
        }
        let target = tokens
            .iter()
            .fold(String::new(), |path, token| child_pointer(&path, token));
        if self.root.pointer(&target).is_none() {
            return Err(self.error(path, format!("unresolved local reference {reference:?}")));
        }
        Ok(target)
    }

    fn audit(&self, schema: &Map<String, Value>, path: &str) -> Result<(), Error> {
        for (key, value) in schema {
            let at = child_pointer(path, key);
            if !ANNOTATIONS.contains(&key.as_str())
                && !CONSTRAINTS.contains(&key.as_str())
                && !SHAPE.contains(&key.as_str())
                && !["$schema", "$defs", "$ref"].contains(&key.as_str())
            {
                return Err(self.error(&at, "unsupported JSON Schema keyword"));
            }
            let valid = match key.as_str() {
                "$schema" => matches!(
                    value.as_str(),
                    Some(
                        "https://json-schema.org/draft/2020-12/schema"
                            | "https://json-schema.org/draft/2020-12/schema#"
                    )
                ),
                "title" | "description" | "$comment" | "pattern" | "format" | "$ref" => {
                    value.is_string()
                }
                "readOnly" | "writeOnly" | "deprecated" | "uniqueItems" => value.is_boolean(),
                "examples" => value.is_array(),
                "$defs" | "properties" => value.is_object(),
                "required" | "enum" => value.is_array(),
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                    value.is_number()
                }
                "multipleOf" => value.as_f64().is_some_and(|v| v > 0.0),
                "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
                | "maxProperties" => value.as_u64().is_some(),
                _ => true,
            };
            if !valid {
                return Err(self.error(
                    &at,
                    if key == "$schema" {
                        "unsupported schema dialect; expected JSON Schema 2020-12"
                    } else {
                        "malformed schema keyword value"
                    },
                ));
            }
        }
        Ok(())
    }

    fn schema_type(
        &self,
        schema: &Map<String, Value>,
        path: &str,
    ) -> Result<(String, bool), Error> {
        let at = child_pointer(path, "type");
        match schema.get("type") {
            Some(Value::String(ty)) => Ok((ty.clone(), false)),
            Some(Value::Array(types)) => {
                let mut seen = BTreeSet::new();
                let mut nullable = false;
                let mut ty = None;
                for (index, value) in types.iter().enumerate() {
                    let value = value.as_str().ok_or_else(|| {
                        self.error(
                            &child_pointer(&at, &index.to_string()),
                            "expected a type name",
                        )
                    })?;
                    if !seen.insert(value) {
                        return Err(self.error(&at, "duplicate type in union"));
                    }
                    if value == "null" {
                        nullable = true;
                    } else if ty.replace(value.to_owned()).is_some() {
                        return Err(
                            self.error(&at, "only one concrete type plus null is supported")
                        );
                    }
                }
                ty.map(|ty| (ty, nullable))
                    .ok_or_else(|| self.error(&at, "null-only or empty types are unsupported"))
            }
            _ => Err(self.error(&at, "an explicit type or local $ref is required")),
        }
    }

    fn check_applicability(
        &self,
        schema: &Map<String, Value>,
        ty: &str,
        path: &str,
    ) -> Result<(), Error> {
        for key in schema.keys() {
            let applies = match key.as_str() {
                "properties"
                | "required"
                | "additionalProperties"
                | "minProperties"
                | "maxProperties" => ty == "object",
                "items" | "minItems" | "maxItems" | "uniqueItems" => ty == "array",
                "minLength" | "maxLength" | "pattern" => ty == "string",
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf" => {
                    ty == "integer" || ty == "number"
                }
                "format" => ["string", "integer", "number"].contains(&ty),
                _ => true,
            };
            if !applies {
                return Err(self.error(
                    &child_pointer(path, key),
                    format!("keyword does not apply to type {ty:?} in the supported subset"),
                ));
            }
        }
        Ok(())
    }

    fn number(&self, schema: &Map<String, Value>, ty: &str, path: &str) -> Result<Number, Error> {
        let format = schema.get("format").and_then(Value::as_str);
        match (ty, format) {
            ("integer", None | Some("int64")) => Ok(Number::S64),
            ("integer", Some("int8")) => Ok(Number::S8),
            ("integer", Some("int16")) => Ok(Number::S16),
            ("integer", Some("int32")) => Ok(Number::S32),
            ("integer", Some("uint8")) => Ok(Number::U8),
            ("integer", Some("uint16")) => Ok(Number::U16),
            ("integer", Some("uint32")) => Ok(Number::U32),
            ("integer", Some("uint64")) => Ok(Number::U64),
            ("number", None | Some("double" | "float64")) => Ok(Number::F64),
            ("number", Some("float" | "float32")) => Ok(Number::F32),
            _ => Err(self.error(
                &child_pointer(path, "format"),
                "unsupported numeric representation",
            )),
        }
    }

    fn enum_cases(
        &self,
        value: &Value,
        path: &str,
        nullable: bool,
    ) -> Result<(Vec<EnumCase>, bool), Error> {
        let at = child_pointer(path, "enum");
        let values = value
            .as_array()
            .ok_or_else(|| self.error(&at, "expected an array"))?;
        let mut strings = BTreeSet::new();
        let mut includes_null = false;
        for (index, value) in values.iter().enumerate() {
            let at = child_pointer(&at, &index.to_string());
            if value.is_null() && nullable {
                if includes_null {
                    return Err(self.error(&at, "duplicate enum value"));
                }
                includes_null = true;
            } else if let Some(value) = value.as_str() {
                if !strings.insert(value) {
                    return Err(self.error(&at, "duplicate enum value"));
                }
            } else {
                return Err(self.error(
                    &at,
                    "expected a string enum value (or null for a nullable string)",
                ));
            }
        }
        if strings.is_empty() {
            return Err(self.error(&at, "enum must contain at least one string value"));
        }
        let mut names = Names::default();
        let cases = strings
            .into_iter()
            .map(|wire_value| EnumCase {
                wire_value: wire_value.to_owned(),
                name: names.allocate(wire_value),
            })
            .collect();
        Ok((cases, includes_null))
    }

    fn object_kind(
        &mut self,
        schema: &Map<String, Value>,
        path: &str,
        name: &str,
    ) -> Result<Kind, Error> {
        let properties = schema.get("properties").and_then(Value::as_object);
        let mut required = BTreeSet::new();
        if let Some(values) = schema.get("required") {
            let at = child_pointer(path, "required");
            let values = values
                .as_array()
                .ok_or_else(|| self.error(&at, "expected an array"))?;
            for (index, value) in values.iter().enumerate() {
                let at = child_pointer(&at, &index.to_string());
                let value = value
                    .as_str()
                    .ok_or_else(|| self.error(&at, "expected a property name"))?;
                if !required.insert(value) {
                    return Err(self.error(&at, "duplicate required property"));
                }
                if !properties.is_some_and(|p| p.contains_key(value)) {
                    return Err(self.error(
                        &at,
                        format!("required property {value:?} has no properties schema"),
                    ));
                }
            }
        }
        let mut field_names = Names::default();
        let additional_path = child_pointer(path, "additionalProperties");
        let additional = match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => None,
            Some(Value::Object(_)) => {
                let schema = self.compile(&additional_path, &format!("{name}-additional-value"))?;
                Some(Additional { name: field_names.allocate("additional-properties"), schema })
            }
            None | Some(Value::Bool(true)) => return Err(self.error(&additional_path,
                "untyped open objects are unsupported; set additionalProperties to false or a supported schema")),
            Some(_) => return Err(self.error(&additional_path, "expected false or a typed additionalProperties schema")),
        };
        let mut fields = Vec::new();
        if let Some(properties) = properties {
            let mut keys: Vec<_> = properties.keys().collect();
            keys.sort();
            for wire_name in keys {
                let field_name = field_names.allocate(wire_name);
                let schema = self.compile(
                    &child_pointer(&child_pointer(path, "properties"), wire_name),
                    &format!("{name}-{field_name}"),
                )?;
                fields.push(Property {
                    wire_name: wire_name.clone(),
                    name: field_name,
                    required: required.contains(wire_name.as_str()),
                    schema,
                });
            }
        }
        Ok(Kind::Object {
            properties: fields,
            additional,
        })
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
