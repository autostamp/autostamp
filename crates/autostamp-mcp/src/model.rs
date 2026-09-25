//! Schema arena shared by WIT emission and subsequent wire-codec generation.

use std::collections::BTreeMap;

use serde_json::Value;

pub(crate) type SchemaId = usize;

#[derive(Debug, Clone)]
pub(crate) struct Tool {
    pub(crate) wire_name: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) input: SchemaId,
    pub(crate) output: Option<SchemaId>,
    pub(crate) result_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Schema {
    pub(crate) name: String,
    pub(crate) value_name: String,
    pub(crate) pointer: String,
    pub(crate) description: Option<String>,
    pub(crate) constraints: BTreeMap<String, Value>,
    pub(crate) nullable: bool,
    pub(crate) kind: Kind,
}

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Boolean,
    String,
    Number(Number),
    Enum(Vec<EnumCase>),
    Array(SchemaId),
    Object {
        properties: Vec<Property>,
        additional: Option<Additional>,
    },
    Ref(SchemaId),
}

#[derive(Debug, Clone)]
pub(crate) struct Property {
    pub(crate) wire_name: String,
    pub(crate) name: String,
    pub(crate) required: bool,
    pub(crate) schema: SchemaId,
}

#[derive(Debug, Clone)]
pub(crate) struct Additional {
    pub(crate) name: String,
    pub(crate) schema: SchemaId,
}

#[derive(Debug, Clone)]
pub(crate) struct EnumCase {
    pub(crate) wire_value: String,
    pub(crate) name: String,
}

/// Codecs must reject non-finite floats and out-of-range conversions. Schema
/// validation bounds are separate annotations, not a replacement for these bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Number {
    S8,
    S16,
    S32,
    S64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

impl Number {
    pub(crate) fn wit(self) -> &'static str {
        match self {
            Self::S8 => "s8",
            Self::S16 => "s16",
            Self::S32 => "s32",
            Self::S64 => "s64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{from_json_value, parse_catalog};

    #[test]
    fn ir_preserves_wire_names_paths_constraints_and_reference_identity() {
        let catalog = parse_catalog(include_str!("../tests/fixtures/tools.json")).unwrap();
        let tool = catalog.tools.first().unwrap();
        assert_eq!(tool.wire_name, "find_items");
        assert_eq!(tool.name, "find-items");
        let input = catalog.schemas.get(tool.input).unwrap();
        assert_eq!(input.pointer, "/tools/0/inputSchema");
        let Kind::Object { properties, .. } = &input.kind else {
            panic!()
        };
        let query = properties.iter().find(|p| p.wire_name == "query").unwrap();
        assert!(query.required);
        let query = catalog.schemas.get(query.schema).unwrap();
        assert!(!query.nullable);
        assert_eq!(query.constraints.get("minLength"), Some(&json!(1)));
        assert_eq!(
            catalog
                .as_json()
                .pointer(&query.pointer)
                .unwrap()
                .get("default"),
            Some(&json!("all"))
        );
        let status = properties.iter().find(|p| p.wire_name == "status").unwrap();
        assert!(!status.required);
        let reference = catalog.schemas.get(status.schema).unwrap();
        let Kind::Ref(target) = reference.kind else {
            panic!()
        };
        let target = catalog.schemas.get(target).unwrap();
        assert_eq!(target.pointer, "/tools/0/inputSchema/$defs/status~1value~0");
        assert!(target.nullable);
        let Kind::Enum(cases) = &target.kind else {
            panic!()
        };
        assert_eq!(
            cases
                .iter()
                .map(|c| c.wire_value.as_str())
                .collect::<Vec<_>>(),
            ["done", "in-progress"]
        );
        let limit = properties.iter().find(|p| p.wire_name == "limit").unwrap();
        assert!(matches!(
            catalog.schemas.get(limit.schema).unwrap().kind,
            Kind::Number(Number::U16)
        ));
    }

    #[test]
    fn ir_preserves_extras_separately_from_a_same_named_declared_property() {
        let catalog = from_json_value(json!({"tools":[{
            "name":"a",
            "inputSchema":{
                "type":"object", "additionalProperties":{"type":"boolean"},
                "properties":{"additional-properties":{"type":"string"}}
            }
        }]}))
        .unwrap();
        let input = catalog
            .schemas
            .get(catalog.tools.first().unwrap().input)
            .unwrap();
        let Kind::Object {
            properties,
            additional: Some(additional),
        } = &input.kind
        else {
            panic!()
        };
        let property = properties.first().unwrap();
        assert_eq!(property.wire_name, "additional-properties");
        assert_eq!(property.name, "additional-properties-v2");
        assert_eq!(additional.name, "additional-properties");
        assert!(matches!(
            catalog.schemas.get(additional.schema).unwrap().kind,
            Kind::Boolean
        ));
        assert!(matches!(
            catalog.schemas.get(property.schema).unwrap().kind,
            Kind::String
        ));
    }
}
