//! The [`InterfaceModel`]: the intermediate model of a single WIT interface.
//!
//! `InterfaceModel` owns the operations, records, and enums generated for one OpenAPI
//! tag. It is built up from OpenAPI schemas (the lowering methods) and then rendered to
//! WIT source (the [`InterfaceModel::to_wit`] method).

use anyhow::Result;
use heck::{ToKebabCase, ToSnakeCase};
use openapiv3::{
    AnySchema, Operation, Parameter, ParameterSchemaOrContent, ReferenceOr, Schema, SchemaKind,
    Type,
};
use std::collections::BTreeSet;

use crate::enum_model::EnumModel;
use crate::field::Field;
use crate::location::Location;
use crate::naming::{sanitize_wit_name, unique_name};
use crate::operation_model::OperationModel;
use crate::package_name::PackageName;
use crate::record_model::RecordModel;
use crate::schema_ctx::SchemaCtx;
use crate::wit_type::WitType;

/// Borrowed `(field-name, field-schema)` entries from an object/`any` schema.
type PropertyEntries<'a> = Vec<(&'a String, &'a ReferenceOr<Box<Schema>>)>;

/// The generated model for a single WIT interface (one per OpenAPI tag).
#[derive(Debug)]
pub(crate) struct InterfaceModel {
    pub(crate) name_kebab: String,
    pub(crate) operations: Vec<OperationModel>,
    pub(crate) records: Vec<RecordModel>,
    pub(crate) enums: Vec<EnumModel>,
    /// Records currently being emitted (the recursion stack). A `$ref` to a
    /// record in this set is a cycle and gets degraded to `string`.
    emitting: BTreeSet<String>,
}

impl InterfaceModel {
    pub(crate) fn new(name_kebab: String) -> Self {
        Self {
            name_kebab,
            operations: vec![],
            records: vec![],
            enums: vec![],
            emitting: BTreeSet::new(),
        }
    }

    pub(crate) fn is_enum(&self, name_kebab: &str) -> bool {
        self.enums.iter().any(|e| e.name_kebab == name_kebab)
    }

    pub(crate) fn is_record(&self, name_kebab: &str) -> bool {
        self.records.iter().any(|r| r.name_kebab == name_kebab)
    }
}

// ---------------------------------------------------------------------------
// Lowering: OpenAPI schemas -> intermediate model

impl InterfaceModel {
    fn map_schema_to_wit_type(
        &mut self,
        ctx: &SchemaCtx,
        schema_ref: &ReferenceOr<Box<Schema>>,
        name_hint: &str,
    ) -> Result<WitType> {
        if let ReferenceOr::Reference { reference } = schema_ref
            && let Some(simple_name) = reference.strip_prefix("#/components/schemas/")
        {
            let record_name = simple_name.to_kebab_case();
            // Cycle: break by degrading to opaque string.
            if self.emitting.contains(&record_name) {
                return Ok(WitType::String);
            }
            if self.is_record(&record_name) {
                return Ok(WitType::Named(record_name));
            }
            self.emitting.insert(record_name.clone());
            let schema = ctx.resolve_schema_boxed(schema_ref)?;
            self.emit_record_from_schema(ctx, schema, &record_name)?;
            self.emitting.remove(&record_name);
            return Ok(WitType::Named(record_name));
        }
        let schema = ctx.resolve_schema_boxed(schema_ref)?;
        self.map_schema_kind(ctx, schema, name_hint)
    }

    fn map_schema_to_wit_type_unboxed(
        &mut self,
        ctx: &SchemaCtx,
        schema_ref: &ReferenceOr<Schema>,
        name_hint: &str,
    ) -> Result<WitType> {
        if let ReferenceOr::Reference { reference } = schema_ref
            && let Some(simple_name) = reference.strip_prefix("#/components/schemas/")
        {
            let record_name = simple_name.to_kebab_case();
            if self.emitting.contains(&record_name) {
                return Ok(WitType::String);
            }
            if self.is_record(&record_name) {
                return Ok(WitType::Named(record_name));
            }
            self.emitting.insert(record_name.clone());
            let schema = ctx.resolve_schema(schema_ref)?;
            self.emit_record_from_schema(ctx, schema, &record_name)?;
            self.emitting.remove(&record_name);
            return Ok(WitType::Named(record_name));
        }
        let schema = ctx.resolve_schema(schema_ref)?;
        self.map_schema_kind(ctx, schema, name_hint)
    }

    fn map_schema_kind(
        &mut self,
        ctx: &SchemaCtx,
        schema: &Schema,
        name_hint: &str,
    ) -> Result<WitType> {
        match &schema.schema_kind {
            SchemaKind::Type(t) => match t {
                Type::String(s) => {
                    if s.enumeration.is_empty() {
                        Ok(WitType::String)
                    } else {
                        let cases: Vec<(String, String)> = s
                            .enumeration
                            .iter()
                            .flatten()
                            .map(|v| {
                                let kebab = if v.is_empty() {
                                    "empty".to_string()
                                } else {
                                    v.to_kebab_case()
                                };
                                (sanitize_wit_name(&kebab), v.clone())
                            })
                            .collect();
                        // Dedupe by kebab case set
                        if let Some(existing) = self
                            .enums
                            .iter()
                            .find(|e| {
                                e.cases
                                    .iter()
                                    .map(|(k, _)| k)
                                    .eq(cases.iter().map(|(k, _)| k))
                            })
                            .map(|e| e.name_kebab.clone())
                        {
                            return Ok(WitType::Named(existing));
                        }
                        let final_name =
                            unique_name(&format!("{name_hint}-enum").to_kebab_case(), |n| {
                                self.is_enum(n) || self.is_record(n)
                            });
                        self.enums.push(EnumModel {
                            name_kebab: final_name.clone(),
                            cases,
                        });
                        Ok(WitType::Named(final_name))
                    }
                }
                Type::Boolean(_) => Ok(WitType::Bool),
                Type::Integer(i) => match i.format {
                    openapiv3::VariantOrUnknownOrEmpty::Item(openapiv3::IntegerFormat::Int64) => {
                        Ok(WitType::S64)
                    }
                    _ => Ok(WitType::S32),
                },
                Type::Number(_) => Ok(WitType::F64),
                Type::Array(a) => {
                    let item_type = match &a.items {
                        Some(items) => {
                            self.map_schema_to_wit_type(ctx, items, &format!("{name_hint}-item"))?
                        }
                        None => WitType::String,
                    };
                    Ok(WitType::List(Box::new(item_type)))
                }
                Type::Object(_) => {
                    let record_name = unique_name(&name_hint.to_kebab_case(), |n| {
                        self.is_record(n) || self.is_enum(n)
                    });
                    self.emit_record_from_schema(ctx, schema, &record_name)?;
                    Ok(WitType::Named(record_name))
                }
            },
            SchemaKind::OneOf { .. } | SchemaKind::AnyOf { .. } | SchemaKind::AllOf { .. } => {
                Ok(WitType::String)
            }
            SchemaKind::Not { .. } => Ok(WitType::String),
            SchemaKind::Any(any) => self.map_any_schema(ctx, any, name_hint),
        }
    }

    fn map_any_schema(
        &mut self,
        ctx: &SchemaCtx,
        any: &AnySchema,
        name_hint: &str,
    ) -> Result<WitType> {
        if !any.properties.is_empty() {
            let record_name = unique_name(&name_hint.to_kebab_case(), |n| {
                self.is_record(n) || self.is_enum(n)
            });
            self.emit_record_from_any(ctx, any, &record_name)?;
            return Ok(WitType::Named(record_name));
        }
        Ok(WitType::String)
    }

    fn emit_record_from_schema(
        &mut self,
        ctx: &SchemaCtx,
        schema: &Schema,
        record_name: &str,
    ) -> Result<()> {
        if self.is_record(record_name) {
            return Ok(());
        }
        let description = schema.schema_data.description.clone();

        let (properties, required): (PropertyEntries<'_>, Vec<String>) = match &schema.schema_kind {
            SchemaKind::Type(Type::Object(o)) => {
                (o.properties.iter().collect(), o.required.clone())
            }
            SchemaKind::Any(any) => (any.properties.iter().collect(), any.required.clone()),
            _ => {
                self.records.push(RecordModel {
                    name_kebab: record_name.to_string(),
                    description,
                    fields: vec![Field {
                        name_kebab: "value".into(),
                        name_snake: "value".into(),
                        description: Some("opaque schema; raw JSON".into()),
                        ty: WitType::String,
                        location: Location::Body,
                    }],
                });
                return Ok(());
            }
        };

        self.records.push(RecordModel {
            name_kebab: record_name.to_string(),
            description,
            fields: vec![],
        });
        let idx = self.records.len() - 1;

        let mut fields = Vec::with_capacity(properties.len());
        for (field_name, schema_ref) in properties {
            let name_kebab = sanitize_wit_name(&field_name.to_kebab_case());
            let nested_hint = format!("{record_name}-{name_kebab}");
            let inner_ty = self.map_schema_to_wit_type(ctx, schema_ref, &nested_hint)?;
            let req = required.iter().any(|r| r == field_name);
            let ty = if req {
                inner_ty
            } else {
                WitType::Option(Box::new(inner_ty))
            };
            let desc = match schema_ref {
                ReferenceOr::Item(s) => s.schema_data.description.clone(),
                _ => None,
            };
            fields.push(Field {
                name_kebab,
                name_snake: field_name.to_snake_case(),
                description: desc,
                ty,
                location: Location::Body,
            });
        }
        if fields.is_empty() {
            // WIT records must have at least one field. For schemas with no named
            // properties (free-form `object`s, `additionalProperties: true`), provide
            // a JSON-blob escape hatch so callers can still pass arbitrary content.
            fields.push(Field {
                name_kebab: "data".into(),
                name_snake: "data".into(),
                description: Some("JSON-encoded free-form object payload".into()),
                ty: WitType::Option(Box::new(WitType::String)),
                location: Location::Body,
            });
        }
        if let Some(record) = self.records.get_mut(idx) {
            record.fields = fields;
        }
        Ok(())
    }

    fn emit_record_from_any(
        &mut self,
        ctx: &SchemaCtx,
        any: &AnySchema,
        record_name: &str,
    ) -> Result<()> {
        if self.is_record(record_name) {
            return Ok(());
        }
        self.records.push(RecordModel {
            name_kebab: record_name.to_string(),
            description: None,
            fields: vec![],
        });
        let idx = self.records.len() - 1;

        let mut fields = vec![];
        for (field_name, schema_ref) in &any.properties {
            let name_kebab = sanitize_wit_name(&field_name.to_kebab_case());
            let nested_hint = format!("{record_name}-{name_kebab}");
            let inner_ty = self.map_schema_to_wit_type(ctx, schema_ref, &nested_hint)?;
            let req = any.required.iter().any(|r| r == field_name);
            let ty = if req {
                inner_ty
            } else {
                WitType::Option(Box::new(inner_ty))
            };
            let desc = match schema_ref {
                ReferenceOr::Item(s) => s.schema_data.description.clone(),
                _ => None,
            };
            fields.push(Field {
                name_kebab,
                name_snake: field_name.to_snake_case(),
                description: desc,
                ty,
                location: Location::Body,
            });
        }
        if fields.is_empty() {
            fields.push(Field {
                name_kebab: "data".into(),
                name_snake: "data".into(),
                description: Some("JSON-encoded free-form object payload".into()),
                ty: WitType::Option(Box::new(WitType::String)),
                location: Location::Body,
            });
        }
        if let Some(record) = self.records.get_mut(idx) {
            record.fields = fields;
        }
        Ok(())
    }

    pub(crate) fn process_operation(
        &mut self,
        ctx: &SchemaCtx,
        method: &str,
        path: &str,
        op: &Operation,
    ) -> Result<()> {
        let raw_id = op
            .operation_id
            .as_deref()
            .unwrap_or_else(|| panic!("operation at {method} {path} missing operationId"));
        // Keep every segment after the first `#` so operations like
        // "Heartbeat V2#Ping" and "Heartbeat V2#Ping#1" don't collide.
        let op_kebab_raw: String = match raw_id.split_once('#') {
            Some((_tag, rest)) => rest.replace('#', "-").to_kebab_case(),
            None => raw_id.to_kebab_case(),
        };
        let op_kebab = sanitize_wit_name(&op_kebab_raw);
        let op_snake = op_kebab.replace('-', "_");

        let mut fields: Vec<Field> = vec![];

        for p in &op.parameters {
            let p = match p {
                ReferenceOr::Item(p) => p,
                ReferenceOr::Reference { .. } => continue,
            };
            let (data, location) = match p {
                Parameter::Path { parameter_data, .. } => (parameter_data, Location::Path),
                Parameter::Query { parameter_data, .. } => (parameter_data, Location::Query),
                Parameter::Header { parameter_data, .. } => (parameter_data, Location::Query),
                Parameter::Cookie { .. } => continue,
            };
            let name_kebab = sanitize_wit_name(&data.name.to_kebab_case());
            let name_snake = data.name.to_snake_case();
            let hint = format!("{op_kebab}-{name_kebab}");

            let schema_ref = match &data.format {
                ParameterSchemaOrContent::Schema(s) => s,
                ParameterSchemaOrContent::Content(_) => {
                    fields.push(Field {
                        name_kebab,
                        name_snake,
                        description: data.description.clone(),
                        ty: if data.required {
                            WitType::String
                        } else {
                            WitType::Option(Box::new(WitType::String))
                        },
                        location,
                    });
                    continue;
                }
            };

            let raw_ty = if location == Location::Path || ctx.is_object_schema(schema_ref) {
                WitType::String
            } else {
                self.map_schema_to_wit_type_unboxed(ctx, schema_ref, &hint)?
            };
            let ty = if data.required {
                raw_ty
            } else {
                WitType::Option(Box::new(raw_ty))
            };
            fields.push(Field {
                name_kebab,
                name_snake,
                description: data.description.clone(),
                ty,
                location,
            });
        }

        if let Some(body_ref) = &op.request_body {
            let body = match body_ref {
                ReferenceOr::Item(b) => b,
                ReferenceOr::Reference { .. } => return Ok(()),
            };
            if let Some(media) = body.content.get("application/json")
                && let Some(body_schema_ref) = &media.schema
            {
                let hint = format!("{op_kebab}-body");
                let body_schema_ref_wrapped = ref_or_to_box(body_schema_ref);
                let body_ty = self.map_schema_to_wit_type(ctx, &body_schema_ref_wrapped, &hint)?;
                match body_ty {
                    WitType::Named(record_name) => {
                        if let Some(rec) = self
                            .records
                            .iter()
                            .find(|r| r.name_kebab == record_name)
                            .cloned()
                        {
                            for f in &rec.fields {
                                let mut nf = f.clone();
                                nf.location = Location::Body;
                                fields.push(nf);
                            }
                            // We inlined the body record's fields. Remove the body record
                            // from emission since we don't need it as a separate WIT type.
                            self.records.retain(|r| r.name_kebab != record_name);
                        }
                    }
                    other => {
                        fields.push(Field {
                            name_kebab: "body".into(),
                            name_snake: "body".into(),
                            description: None,
                            ty: if body.required {
                                other
                            } else {
                                WitType::Option(Box::new(other))
                            },
                            location: Location::Body,
                        });
                    }
                }
            }
        }

        let params_record = sanitize_wit_name(&format!("{op_kebab}-params"));

        self.operations.push(OperationModel {
            op_kebab,
            op_snake,
            method: method.to_uppercase(),
            path_template: path.to_string(),
            summary: op.summary.clone(),
            params_record,
            fields,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WIT emission: intermediate model -> WIT source

impl InterfaceModel {
    /// Render this interface to WIT source, prefixed with `package`'s declaration.
    pub(crate) fn to_wit(&self, package: &PackageName) -> String {
        let mut out = String::new();
        out.push_str(&format!("{}\n\n", package.wit_decl()));
        out.push_str(&format!("interface {} {{\n", self.name_kebab));

        for e in &self.enums {
            out.push_str(&format!("  enum {} {{\n", e.name_kebab));
            for (kebab, _) in &e.cases {
                out.push_str(&format!("    {kebab},\n"));
            }
            out.push_str("  }\n\n");
        }

        for r in &self.records {
            if let Some(desc) = &r.description {
                for line in desc.lines() {
                    out.push_str(&format!("  /// {}\n", line));
                }
            }
            out.push_str(&format!("  record {} {{\n", r.name_kebab));
            for (i, f) in r.fields.iter().enumerate() {
                if let Some(d) = &f.description {
                    for line in d.lines().take(2) {
                        out.push_str(&format!("    /// {}\n", line));
                    }
                }
                let ty = f.ty.render();
                let comma = if i + 1 < r.fields.len() { "," } else { "" };
                out.push_str(&format!("    {}: {}{}\n", f.name_kebab, ty, comma));
            }
            out.push_str("  }\n\n");
        }

        for op in &self.operations {
            if let Some(s) = &op.summary {
                out.push_str(&format!("  /// {}\n", s));
            }
            if op.fields.is_empty() {
                // No params — no record, no argument.
                out.push_str(&format!(
                    "  {}: func() -> result<string, string>;\n\n",
                    op.op_kebab
                ));
                continue;
            }
            out.push_str(&format!("  record {} {{\n", op.params_record));
            for (i, f) in op.fields.iter().enumerate() {
                if let Some(d) = &f.description {
                    for line in d.lines().take(2) {
                        out.push_str(&format!("    /// {}\n", line));
                    }
                }
                let ty = f.ty.render();
                let comma = if i + 1 < op.fields.len() { "," } else { "" };
                out.push_str(&format!("    {}: {}{}\n", f.name_kebab, ty, comma));
            }
            out.push_str("  }\n");
            out.push_str(&format!(
                "  {}: func(params: {}) -> result<string, string>;\n\n",
                op.op_kebab, op.params_record
            ));
        }

        out.push_str("}\n");
        out
    }
}

fn ref_or_to_box(r: &ReferenceOr<Schema>) -> ReferenceOr<Box<Schema>> {
    match r {
        ReferenceOr::Reference { reference } => ReferenceOr::Reference {
            reference: reference.clone(),
        },
        ReferenceOr::Item(s) => ReferenceOr::Item(Box::new(s.clone())),
    }
}
