//! The [`InterfaceModel`]: the intermediate model of a single WIT interface.
//!
//! `InterfaceModel` owns the operations, records, and enums generated for one OpenAPI
//! tag. It is built up from OpenAPI schemas (the lowering methods) and then rendered to
//! WIT source (the [`InterfaceModel::to_wit`] method).

use anyhow::Result;
use heck::{ToKebabCase, ToSnakeCase};
use openapiv3::{
    AdditionalProperties, AnySchema, MediaType, ObjectType, Operation, Parameter,
    ParameterSchemaOrContent, ReferenceOr, RequestBody, Response, Responses, Schema, SchemaKind,
    StatusCode, Type,
};
use std::collections::BTreeSet;

use crate::enum_model::EnumModel;
use crate::field::Field;
use crate::location::Location;
use crate::naming::{
    sanitize_wit_name, strip_scope_prefix, synthesize_operation_id, to_rust_ident, unique_name,
};
use crate::operation_model::OperationModel;
use crate::package_name::PackageName;
use crate::record_model::RecordModel;
use crate::schema_ctx::SchemaCtx;
use crate::security::{AuthApply, AuthKind};
use crate::variant_model::{CaseStatus, VariantCase, VariantModel};
use crate::wit_type::WitType;

/// Borrowed `(field-name, field-schema)` entries from an object/`any` schema.
type PropertyEntries<'a> = Vec<(&'a String, &'a ReferenceOr<Box<Schema>>)>;

/// Backstop on how deeply nested-record emission may recurse before a schema is degraded to an
/// opaque `string`. Real-world schemas nest only a handful of records deep; a run beyond this is
/// a pathological or cyclic schema (e.g. a self-reference the `$ref`-cycle guard can't see because
/// it is wrapped in an inline `allOf`). Bounding it keeps such a schema from expanding without
/// limit — an unmodelable body always falls back to the raw-body `string` rather than hanging.
const MAX_RECORD_DEPTH: usize = 24;

/// The generated model for a single WIT interface (one per OpenAPI tag).
#[derive(Debug)]
pub(crate) struct InterfaceModel {
    pub(crate) name_kebab: String,
    pub(crate) operations: Vec<OperationModel>,
    pub(crate) records: Vec<RecordModel>,
    pub(crate) enums: Vec<EnumModel>,
    /// Per-operation error variants enumerating each operation's declared error responses.
    pub(crate) variants: Vec<VariantModel>,
    /// Count of request fields dropped by the duplicate-credential pruning heuristic, surfaced
    /// in the generated README's diagnostics.
    pub(crate) pruned_credential_fields: usize,
    /// Secret keys synthesized by the API-key inference heuristic (a naked `key`-style parameter
    /// lifted into a host-injected credential). Named in the generated README's diagnostics so a
    /// reader can see the component now expects auth the spec never declared.
    pub(crate) inferred_api_key_secrets: Vec<String>,
    /// Records currently being emitted (the recursion stack). A `$ref` to a
    /// record in this set is a cycle and gets degraded to `string`.
    emitting: BTreeSet<String>,
    /// Depth of the nested-record emission recursion, used as a backstop against pathologically
    /// deep or cyclic schemas that escape the `emitting` `$ref`-cycle guard.
    record_depth: usize,
    /// Stable WIT name assigned to each `#/components/schemas/{simple_name}` reference lowered
    /// into this interface. The name is the schema name with the redundant enclosing-interface
    /// prefix stripped; memoizing it keeps every reference to one schema resolving to the same
    /// name and keeps distinct schemas that strip to the same tail from colliding.
    schema_names: std::collections::BTreeMap<String, String>,
}

impl InterfaceModel {
    pub(crate) fn new(name_kebab: String) -> Self {
        Self {
            name_kebab,
            operations: vec![],
            records: vec![],
            enums: vec![],
            variants: vec![],
            pruned_credential_fields: 0,
            inferred_api_key_secrets: vec![],
            emitting: BTreeSet::new(),
            record_depth: 0,
            schema_names: std::collections::BTreeMap::new(),
        }
    }

    pub(crate) fn is_enum(&self, name_kebab: &str) -> bool {
        self.enums.iter().any(|e| e.name_kebab == name_kebab)
    }

    pub(crate) fn is_record(&self, name_kebab: &str) -> bool {
        self.records.iter().any(|r| r.name_kebab == name_kebab)
    }

    /// Whether `name_kebab` is already claimed by a record, enum, operation function, or params
    /// record in this interface. WIT gives types and functions a single shared namespace, so any
    /// of these colliding is a "defined more than once" error; route every generated name through
    /// here to keep them mutually unique.
    fn name_in_use(&self, name_kebab: &str) -> bool {
        self.is_record(name_kebab)
            || self.is_enum(name_kebab)
            || self.variants.iter().any(|v| v.name_kebab == name_kebab)
            || self
                .operations
                .iter()
                .any(|o| o.op_kebab == name_kebab || o.params_record == name_kebab)
    }

    /// The stable WIT name for a `#/components/schemas/{simple_name}` reference lowered into this
    /// interface. The schema name is sanitized and then has the redundant enclosing-interface
    /// prefix stripped, so GitHub's `code-scanning-alert-state` schema surfaces as `alert-state`
    /// inside `interface code-scanning` rather than stuttering the interface into every type.
    ///
    /// The assignment is memoized: a `$ref` is looked up (and referenced) by schema name many
    /// times, and every hit must resolve to one WIT name. Because stripping can collapse two
    /// distinct schemas onto the same tail — or onto an operation/enum name — the first
    /// assignment for a fresh tail is disambiguated with a numeric suffix against every name
    /// already taken or already assigned here, then remembered.
    fn schema_record_name(&mut self, simple_name: &str) -> String {
        if let Some(name) = self.schema_names.get(simple_name) {
            return name.clone();
        }
        let base = strip_scope_prefix(&sanitize_wit_name(simple_name), &self.name_kebab);
        let name = unique_name(&base, |n| {
            self.name_in_use(n) || self.schema_names.values().any(|v| v == n)
        });
        self.schema_names
            .insert(simple_name.to_string(), name.clone());
        name
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
        if self.record_depth > MAX_RECORD_DEPTH {
            return Ok(WitType::String);
        }
        if let ReferenceOr::Reference { reference } = schema_ref
            && let Some(simple_name) = reference.strip_prefix("#/components/schemas/")
        {
            let record_name = self.schema_record_name(simple_name);
            // Cycle: break by degrading to opaque string.
            if self.emitting.contains(&record_name) {
                return Ok(WitType::String);
            }
            if self.is_record(&record_name) || self.is_enum(&record_name) {
                return Ok(WitType::Named(record_name));
            }
            let schema = ctx.resolve_schema_boxed(schema_ref)?;
            return self.lower_named_schema(ctx, schema, &record_name);
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
        if self.record_depth > MAX_RECORD_DEPTH {
            return Ok(WitType::String);
        }
        if let ReferenceOr::Reference { reference } = schema_ref
            && let Some(simple_name) = reference.strip_prefix("#/components/schemas/")
        {
            let record_name = self.schema_record_name(simple_name);
            if self.emitting.contains(&record_name) {
                return Ok(WitType::String);
            }
            if self.is_record(&record_name) || self.is_enum(&record_name) {
                return Ok(WitType::Named(record_name));
            }
            let schema = ctx.resolve_schema(schema_ref)?;
            return self.lower_named_schema(ctx, schema, &record_name);
        }
        let schema = ctx.resolve_schema(schema_ref)?;
        self.map_schema_kind(ctx, schema, name_hint)
    }

    /// Lower the resolved target of a `#/components/schemas/*` reference, naming the emitted WIT
    /// type after the schema.
    ///
    /// A `$ref` whose target is itself a container — an array (e.g. CircleCI's
    /// `Builds: { type: array, items: $ref Build }`) or a free-form `object` map
    /// (`BuildParameters: { type: object }`) — resolves inline to `list<...>` rather than minting
    /// an opaque wrapper record. A named string schema with an `enum` list (CircleCI's `Status`,
    /// `Scope`, …) becomes a WIT enum carrying the schema's name. Other scalar shapes (`Sha1:
    /// type: string`) lower structurally like inline schemas — a bare scalar matches what is on
    /// the wire, where the old `{ value: string }` wrapper record never could. Only object-shaped
    /// schemas become records.
    fn lower_named_schema(
        &mut self,
        ctx: &SchemaCtx,
        schema: &Schema,
        record_name: &str,
    ) -> Result<WitType> {
        self.emitting.insert(record_name.to_string());
        let ty = self.lower_named_schema_inner(ctx, schema, record_name);
        self.emitting.remove(record_name);
        ty
    }

    fn lower_named_schema_inner(
        &mut self,
        ctx: &SchemaCtx,
        schema: &Schema,
        record_name: &str,
    ) -> Result<WitType> {
        if let Some(direct) = self.direct_container_type(ctx, schema, record_name)? {
            return Ok(direct);
        }
        if let SchemaKind::Type(Type::String(s)) = &schema.schema_kind
            && !s.enumeration.is_empty()
        {
            self.enums.push(EnumModel {
                name_kebab: record_name.to_string(),
                cases: string_enum_cases(s),
            });
            return Ok(WitType::Named(record_name.to_string()));
        }
        if !matches!(
            &schema.schema_kind,
            SchemaKind::Type(Type::Object(_)) | SchemaKind::Any(_) | SchemaKind::AllOf { .. }
        ) {
            return self.map_schema_kind(ctx, schema, record_name);
        }
        self.emit_record_from_schema(ctx, schema, record_name)?;
        Ok(WitType::Named(record_name.to_string()))
    }

    /// If `schema` is a *container* shape that lowers to an inline (unnamed) WIT type rather
    /// than a named record, return that type; otherwise `None`. Containers are arrays
    /// (`list<item>`) and free-form `object` maps (`list<tuple<string, value>>`). Callers use
    /// this so a `$ref` to such a schema resolves to the container type directly instead of
    /// minting an opaque wrapper record around it.
    ///
    /// The caller is responsible for the `emitting` cycle guard; this method recurses into the
    /// item / value schema, which may reference the container's own name.
    fn direct_container_type(
        &mut self,
        ctx: &SchemaCtx,
        schema: &Schema,
        name_hint: &str,
    ) -> Result<Option<WitType>> {
        match &schema.schema_kind {
            SchemaKind::Type(Type::Array(a)) => {
                let item_type = match &a.items {
                    Some(items) => {
                        self.map_schema_to_wit_type(ctx, items, &format!("{name_hint}-item"))?
                    }
                    None => WitType::String,
                };
                Ok(Some(WitType::List(Box::new(item_type))))
            }
            SchemaKind::Type(Type::Object(o)) if o.properties.is_empty() => {
                self.free_form_map_type(ctx, o, name_hint)
            }
            _ => Ok(None),
        }
    }

    /// Lower an `object` schema that declares no `properties` — an open map / free-form object,
    /// e.g. CircleCI's `BuildParameters: { type: object }` — to `list<{hint}-entry>`, emitting a
    /// `record {hint}-entry { key: string, value: V }` alongside it.
    ///
    /// The value type `V` comes from `additionalProperties` when it carries a schema; a bare
    /// `type: object` (or `additionalProperties: true`) has an unknown value type and defaults to
    /// `string` (raw JSON-encoded values). A *closed* empty object (`additionalProperties: false`)
    /// is not a map — it returns `None` so the caller keeps the opaque escape-hatch record.
    fn free_form_map_type(
        &mut self,
        ctx: &SchemaCtx,
        obj: &ObjectType,
        name_hint: &str,
    ) -> Result<Option<WitType>> {
        let value_ty = match &obj.additional_properties {
            Some(AdditionalProperties::Any(false)) => return Ok(None),
            Some(AdditionalProperties::Schema(value_schema)) => self
                .map_schema_to_wit_type_unboxed(ctx, value_schema, &format!("{name_hint}-value"))?,
            _ => WitType::String,
        };
        // Name the map's `key`/`value` pair after the containing type. The record is a normal
        // member of `self.records` so WIT rendering, name-uniquing, and prune-liveness all treat
        // it like any other record; only the map field's JSON (de)serialization is special-cased
        // (it stays a JSON object on the wire, never a list of `{key, value}` objects).
        let entry_name = unique_name(&format!("{name_hint}-entry").to_kebab_case(), |n| {
            self.name_in_use(n)
        });
        self.records.push(RecordModel {
            name_kebab: entry_name.clone(),
            description: Some("one entry of a string-keyed map (free-form object)".into()),
            fields: vec![
                Field {
                    name_kebab: "key".into(),
                    name_snake: "key".into(),
                    wire_name: "key".into(),
                    description: None,
                    ty: WitType::String,
                    location: Location::Body,
                },
                Field {
                    name_kebab: "value".into(),
                    name_snake: "value".into(),
                    wire_name: "value".into(),
                    description: None,
                    ty: value_ty.clone(),
                    location: Location::Body,
                },
            ],
        });
        Ok(Some(WitType::Map {
            entry: entry_name,
            value: Box::new(value_ty),
        }))
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
                        let cases = string_enum_cases(s);
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
                                self.name_in_use(n)
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
                Type::Object(o) => {
                    // A `type: object` with no declared properties is an open map, not a record.
                    if o.properties.is_empty()
                        && let Some(map_ty) = self.free_form_map_type(ctx, o, name_hint)?
                    {
                        return Ok(map_ty);
                    }
                    let record_name =
                        unique_name(&name_hint.to_kebab_case(), |n| self.name_in_use(n));
                    self.emit_record_from_schema(ctx, schema, &record_name)?;
                    Ok(WitType::Named(record_name))
                }
            },
            SchemaKind::AllOf { all_of } => {
                // A single-member `allOf` is exactly its member. Delegate to the member so a
                // `$ref` member travels the cycle-guarded reference path: some specs wrap a
                // self-referential schema as `allOf: [$ref Self]` (e.g. Jira's
                // `NotificationEvent.templateEvent`), and lowering that as an anonymous merged
                // record instead mints a fresh record name at every level, recursing without
                // bound. As a plain reference it resolves to the named record (or degrades to
                // `string` when it closes a cycle) exactly like a direct `$ref`.
                if let [only] = all_of.as_slice() {
                    return self.map_schema_to_wit_type_unboxed(ctx, only, name_hint);
                }
                // `allOf` composes one object from its members, typically the common
                // `[{$ref: Base}, {inline extension}]` shape. Merge the members' properties into
                // a single record instead of degrading to an opaque `string`, so a body or field
                // defined by composition keeps every field. (DigitalOcean models many request
                // bodies this way; the old mapping dropped all of their fields.)
                let record_name = unique_name(&name_hint.to_kebab_case(), |n| self.name_in_use(n));
                self.emit_record_from_schema(ctx, schema, &record_name)?;
                Ok(WitType::Named(record_name))
            }
            // `oneOf`/`anyOf` describe a value that is exactly one of / at least one of several
            // alternatives; no single record represents that faithfully, so they remain an
            // opaque JSON `string` — a deliberate, documented simplification. `not` has no record
            // form either.
            SchemaKind::OneOf { .. } | SchemaKind::AnyOf { .. } => Ok(WitType::String),
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
            let record_name = unique_name(&name_hint.to_kebab_case(), |n| self.name_in_use(n));
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

        // Only object-shaped schemas contribute named fields. `allOf` is object-shaped once its
        // members are merged; everything else (scalars, `oneOf`/`anyOf`/`not`) has no record form
        // and gets a single raw-JSON `value` escape hatch.
        let structured = matches!(
            &schema.schema_kind,
            SchemaKind::Type(Type::Object(_)) | SchemaKind::Any(_) | SchemaKind::AllOf { .. }
        );
        if !structured {
            self.records.push(RecordModel {
                name_kebab: record_name.to_string(),
                description,
                fields: vec![Field {
                    name_kebab: "value".into(),
                    name_snake: "value".into(),
                    wire_name: "value".into(),
                    description: Some("opaque schema; raw JSON".into()),
                    ty: WitType::String,
                    location: Location::Body,
                }],
            });
            return Ok(());
        }

        // Reserve the record slot before lowering fields so a self-referential member resolves
        // back to this record rather than recursing forever.
        self.records.push(RecordModel {
            name_kebab: record_name.to_string(),
            description,
            fields: vec![],
        });
        let idx = self.records.len() - 1;

        let mut fields = Vec::new();
        self.record_depth += 1;
        let merged = self.merge_schema_fields(ctx, record_name, schema, &mut fields, 0);
        self.record_depth -= 1;
        merged?;
        dedupe_field_names(&mut fields);
        if fields.is_empty() {
            // WIT records must have at least one field. For schemas with no named
            // properties (free-form `object`s, `additionalProperties: true`), provide
            // a JSON-blob escape hatch so callers can still pass arbitrary content.
            fields.push(Field {
                name_kebab: "data".into(),
                name_snake: "data".into(),
                wire_name: "data".into(),
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

    /// Collect the record fields contributed by `schema` into `out`, flattening `allOf`.
    ///
    /// An object/`any` schema contributes its own properties. An `allOf` contributes the union of
    /// its members' properties — the whole point of `allOf` composition — resolving each member
    /// `$ref` and recursing so a base that is itself composed still flattens. `oneOf`/`anyOf` and
    /// scalar members name no fields and are skipped. `depth` bounds pathological cyclic `allOf`.
    fn merge_schema_fields(
        &mut self,
        ctx: &SchemaCtx,
        record_name: &str,
        schema: &Schema,
        out: &mut Vec<Field>,
        depth: usize,
    ) -> Result<()> {
        if depth > 8 {
            return Ok(());
        }
        match &schema.schema_kind {
            SchemaKind::Type(Type::Object(o)) => {
                self.push_object_fields(
                    ctx,
                    record_name,
                    o.properties.iter().collect(),
                    &o.required,
                    out,
                )?;
            }
            SchemaKind::Any(any) => {
                self.push_object_fields(
                    ctx,
                    record_name,
                    any.properties.iter().collect(),
                    &any.required,
                    out,
                )?;
            }
            SchemaKind::AllOf { all_of } => {
                for member in all_of {
                    match ctx.resolve_schema(member) {
                        Ok(member_schema) => {
                            self.merge_schema_fields(
                                ctx,
                                record_name,
                                member_schema,
                                out,
                                depth + 1,
                            )?;
                        }
                        Err(err) => eprintln!(
                            "openapi-bindgen: skipping unresolvable allOf member in `{record_name}`: {err}"
                        ),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Lower one object/`any` schema's `properties` into `out`, wrapping each non-`required`
    /// field in `option<...>`. Shared by plain object records and `allOf` merging.
    fn push_object_fields(
        &mut self,
        ctx: &SchemaCtx,
        record_name: &str,
        properties: PropertyEntries<'_>,
        required: &[String],
        out: &mut Vec<Field>,
    ) -> Result<()> {
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
            out.push(Field {
                name_kebab,
                name_snake: field_name.to_snake_case(),
                wire_name: field_name.clone(),
                description: desc,
                ty,
                location: Location::Body,
            });
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
        self.record_depth += 1;
        let lowered = (|| -> Result<()> {
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
                    wire_name: field_name.clone(),
                    description: desc,
                    ty,
                    location: Location::Body,
                });
            }
            Ok(())
        })();
        self.record_depth -= 1;
        lowered?;
        if fields.is_empty() {
            fields.push(Field {
                name_kebab: "data".into(),
                name_snake: "data".into(),
                wire_name: "data".into(),
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

    /// Lower a single resolved parameter into a [`Field`], or `None` for a cookie parameter
    /// (not yet represented in the request runtime). Shared by the operation- and
    /// path-item-level parameter passes in [`process_operation`].
    fn lower_parameter(
        &mut self,
        ctx: &SchemaCtx,
        op_kebab: &str,
        param: &Parameter,
    ) -> Result<Option<Field>> {
        let (data, location) = match param {
            Parameter::Path { parameter_data, .. } => (parameter_data, Location::Path),
            Parameter::Query { parameter_data, .. } => (parameter_data, Location::Query),
            Parameter::Header { parameter_data, .. } => (parameter_data, Location::Header),
            Parameter::Cookie { parameter_data, .. } => {
                // Cookie parameters aren't represented in the request runtime yet (see
                // docs/known-limitations.md). They're vanishingly rare in practice — no provider
                // in the curated corpus uses one — so rather than model them we drop them, but
                // never silently: surface a diagnostic so the omission is visible.
                eprintln!(
                    "openapi-bindgen: dropping unsupported cookie parameter `{}` on `{op_kebab}`",
                    parameter_data.name
                );
                return Ok(None);
            }
        };
        let name_kebab = sanitize_wit_name(&data.name.to_kebab_case());
        let name_snake = data.name.to_snake_case();
        let wire_name = data.name.clone();
        let hint = format!("{op_kebab}-{name_kebab}");

        let schema_ref = match &data.format {
            ParameterSchemaOrContent::Schema(s) => s,
            ParameterSchemaOrContent::Content(_) => {
                return Ok(Some(Field {
                    name_kebab,
                    name_snake,
                    wire_name,
                    description: data.description.clone(),
                    ty: if data.required {
                        WitType::String
                    } else {
                        WitType::Option(Box::new(WitType::String))
                    },
                    location,
                }));
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
        Ok(Some(Field {
            name_kebab,
            name_snake,
            wire_name,
            description: data.description.clone(),
            ty,
            location,
        }))
    }

    pub(crate) fn process_operation<'a>(
        &mut self,
        ctx: &SchemaCtx<'a>,
        method: &str,
        path: &str,
        path_item_params: &'a [ReferenceOr<Parameter>],
        op: &'a Operation,
        mut auth: Vec<AuthApply>,
    ) -> Result<()> {
        // `operationId` is optional in OpenAPI; synthesize a deterministic id from the
        // method and path (unique per document) when it is absent or empty so the operation
        // still generates instead of crashing.
        let synthesized;
        let raw_id = match op.operation_id.as_deref() {
            Some(id) if !id.is_empty() => id,
            _ => {
                synthesized = synthesize_operation_id(method, path);
                &synthesized
            }
        };
        // Keep every segment after the first `#` so operations like
        // "Heartbeat V2#Ping" and "Heartbeat V2#Ping#1" don't collide.
        let op_kebab_raw: String = match raw_id.split_once('#') {
            Some((_tag, rest)) => rest.replace('#', "-").to_kebab_case(),
            None => raw_id.to_kebab_case(),
        };
        let op_kebab = sanitize_wit_name(&op_kebab_raw);
        // The interface already scopes every member, so a name that repeats the enclosing
        // interface is redundant: under `interface migrations`, the operationId
        // `migrations/list-for-org` should surface as `list-for-org`, not
        // `migrations-list-for-org`. Strip the prefix here, at the source, so it also drops
        // from the params record (`{op_kebab}-params`) and the param-derived enum name hints
        // built from `op_kebab` below.
        let op_kebab = strip_scope_prefix(&op_kebab, &self.name_kebab);

        let mut fields: Vec<Field> = vec![];

        // Gather the effective parameter list: path-item-level parameters (shared by every
        // method of the path) merged with the operation's own, resolving any
        // `#/components/parameters/*` `$ref`. Operation-level parameters override a path-level
        // one with the same (name, location), per the OpenAPI spec; path-item params that the
        // operation does not redefine are lowered first, then the operation's own. An
        // unresolvable `$ref` is reported and skipped rather than silently dropped.
        let mut op_keys: BTreeSet<(String, String)> = BTreeSet::new();
        for p in &op.parameters {
            if let Some(param) = resolve_param_ref(ctx, p) {
                op_keys.insert(param_key(param));
            }
        }
        for p in path_item_params {
            if let Some(param) = resolve_param_ref(ctx, p) {
                if op_keys.contains(&param_key(param)) {
                    continue;
                }
                if let Some(field) = self.lower_parameter(ctx, &op_kebab, param)? {
                    fields.push(field);
                }
            }
        }
        for p in &op.parameters {
            if let Some(param) = resolve_param_ref(ctx, p)
                && let Some(field) = self.lower_parameter(ctx, &op_kebab, param)?
            {
                fields.push(field);
            }
        }

        if let Some(body_ref) = &op.request_body {
            // Resolve a `$ref` body (a shared `#/components/requestBodies/...`) instead of
            // dropping the operation outright. Many specs point several operations at one
            // shared body (e.g. Azure Cognitive Services' `ImageUrl`); dropping them left
            // function-less interfaces, which wit-bindgen emits no `Guest` trait for (an
            // uncompilable component). If resolution fails, skip just the body and still emit
            // the operation.
            let resolved: Option<RequestBody> = match body_ref {
                ReferenceOr::Item(b) => Some(b.clone()),
                ReferenceOr::Reference { reference } => {
                    ctx.resolve_request_body(reference).ok().cloned()
                }
            };
            if let Some(body) = &resolved
                && let Some(media) = select_body_media(body)
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
                            // The body record's fields are inlined into the params record.
                            // Don't drop it here: another record may still reference it
                            // (e.g. a nested `$ref`). `prune_unused_records` removes it
                            // later only if nothing references it.
                        }
                    }
                    other => {
                        fields.push(Field {
                            name_kebab: "body".into(),
                            name_snake: "body".into(),
                            wire_name: "body".into(),
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

        // Some specs carry their API key as an ordinary query/header parameter and declare no
        // security scheme (e.g. ip2location's required `key`). When an operation resolves to no
        // auth, infer such a parameter as a host-injected credential: lift it off the operation
        // surface and record the synthesized scheme, so callers never pass a secret the runtime
        // already supplies from `wasmcloud:secrets`. Runs before the duplicate-credential prune
        // below so the two heuristics' diagnostics stay distinct.
        if auth.is_empty() {
            for cred in infer_api_key_credentials(op) {
                let before = fields.len();
                fields
                    .retain(|f| !(f.location == cred.location && f.name_snake == cred.field_snake));
                if fields.len() < before {
                    self.inferred_api_key_secrets
                        .push(cred.apply.secret_key.clone());
                    auth.push(cred.apply);
                }
            }
        }

        // Drop request fields that merely duplicate a credential we already inject centrally
        // for this operation. Some APIs (e.g. Plaid) declare a credential both as a security
        // scheme *and* as a redundant request-body/query/header property; carrying both would
        // force every caller to pass a secret the runtime already supplies. Path fields are
        // structural and never pruned.
        let before = fields.len();
        fields.retain(|f| !is_injected_credential(f, &auth));
        self.pruned_credential_fields += before - fields.len();

        // Collapse duplicate fields gathered from repeated parameters or an inlined body so the
        // emitted record (and the wit-bindgen struct it produces) has unique field names.
        dedupe_field_names(&mut fields);

        // Finalize identifiers so they're unique within the interface. WIT shares one namespace
        // for record/enum *types* and *functions*, so an operation's function name and its params
        // record must not collide with each other, with other operations, or with any generated
        // record/enum (e.g. Telnyx declares two `validateAddress` operations under one tag).
        let op_kebab = unique_name(&op_kebab, |n| self.name_in_use(n));
        let op_snake = to_rust_ident(&op_kebab);
        let params_record = unique_name(&format!("{op_kebab}-params"), |n| {
            op_kebab == n || self.name_in_use(n)
        });

        // Reserve the operation's function and params-record names *before* lowering the
        // response surface. WIT shares one namespace for functions and types, and a success
        // response is frequently a schema named after the operation (e.g. Plaid's
        // `transferIntentCreate` returns `TransferIntentCreateResponse`, whose `transfer_intent`
        // field is `$ref TransferIntentCreate` -> record `transfer-intent-create`). Without this
        // reservation `name_in_use` would not yet know the operation's own name, so that response
        // record would claim `transfer-intent-create` and then collide with the function when it
        // is emitted ("defined more than once"). Push the model now with placeholder result types
        // so response records and error variants are disambiguated against it, then fill in the
        // real `ok`/`err` types once lowered.
        self.operations.push(OperationModel {
            op_kebab: op_kebab.clone(),
            op_snake,
            method: method.to_uppercase(),
            path_template: path.to_string(),
            summary: op.summary.clone(),
            params_record,
            fields,
            ok_ty: WitType::String,
            err_ty: WitType::String,
            auth,
        });
        let op_index = self.operations.len() - 1;

        // Model the response surface: the typed 2xx success body (the `ok` arm) and an
        // enumerated error variant over the declared error responses (the `err` arm).
        let (ok_ty, err_ty) = self.lower_responses(ctx, &op_kebab, &op.responses)?;
        if let Some(model) = self.operations.get_mut(op_index) {
            model.ok_ty = ok_ty;
            model.err_ty = err_ty;
        }
        Ok(())
    }

    /// Lower an operation's `responses` into the `(ok, err)` types of its returned `result`.
    ///
    /// The `ok` type is the typed body of the primary success (2xx) response; the `err` type is
    /// an enumerated variant over the declared error responses. See [`Self::lower_success_response`]
    /// and [`Self::lower_error_responses`] for the exact selection and fallback rules.
    fn lower_responses<'a>(
        &mut self,
        ctx: &SchemaCtx<'a>,
        op_kebab: &str,
        responses: &'a Responses,
    ) -> Result<(WitType, WitType)> {
        let ok_ty = self.lower_success_response(ctx, op_kebab, responses)?;
        let err_ty = self.lower_error_responses(op_kebab, responses)?;
        Ok((ok_ty, err_ty))
    }

    /// Lower the primary success (2xx) response body to the `ok` arm of the operation's `result`.
    ///
    /// The success response is chosen by [`select_success_response`] (preferring `200`, then
    /// `201`, then the lowest 2xx, then a `2XX` range). Its JSON media type's schema is lowered
    /// exactly like a request body — an object becomes a named record, an array a `list`, and so
    /// on. Operations with no success response, no content, or no schema (`204 No Content`, a
    /// bare `description`) keep [`WitType::String`]: the raw response body, preserving the
    /// original always-return-the-body behavior.
    fn lower_success_response<'a>(
        &mut self,
        ctx: &SchemaCtx<'a>,
        op_kebab: &str,
        responses: &'a Responses,
    ) -> Result<WitType> {
        let Some(resp_ref) = select_success_response(responses) else {
            return Ok(WitType::String);
        };
        let Some(resp) = resolve_response_ref(ctx, resp_ref) else {
            return Ok(WitType::String);
        };
        let Some(media) = select_response_media(resp) else {
            return Ok(WitType::String);
        };
        let Some(schema_ref) = &media.schema else {
            return Ok(WitType::String);
        };
        let hint = format!("{op_kebab}-response");
        self.map_schema_to_wit_type_unboxed(ctx, schema_ref, &hint)
    }

    /// Enumerate the declared error responses into the `err` arm of the operation's `result`.
    ///
    /// Every specific non-2xx status (`401`, `404`, …) and non-2xx status range (`4XX`, `5XX`)
    /// becomes a named variant case carrying the raw response body as a `string`, plus a trailing
    /// `other(string)` catch-all for undeclared statuses and transport-level failures. `default`
    /// (a catch-all fallback) and any success codes fold into `other` rather than a named case.
    /// An operation that declares no specific error responses keeps [`WitType::String`] — a raw
    /// error message — so its `result` stays `result<ok, string>` rather than a single-case
    /// variant that would carry no more information than the string it wraps.
    fn lower_error_responses(&mut self, op_kebab: &str, responses: &Responses) -> Result<WitType> {
        let mut cases: Vec<VariantCase> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for code in responses.responses.keys() {
            let (base_name, status) = match code {
                StatusCode::Code(c) if (200..300).contains(c) => continue,
                StatusCode::Code(c) => (status_case_name(*c), CaseStatus::Code(*c)),
                StatusCode::Range(2) => continue,
                StatusCode::Range(r) => (range_case_name(*r), CaseStatus::Range(*r as u8)),
            };
            let name = unique_name(&base_name, |n| seen.contains(n));
            seen.insert(name.clone());
            cases.push(VariantCase {
                name_kebab: name,
                payload: Some(WitType::String),
                status,
            });
        }
        if cases.is_empty() {
            return Ok(WitType::String);
        }
        let other = unique_name("other", |n| seen.contains(n));
        cases.push(VariantCase {
            name_kebab: other,
            payload: Some(WitType::String),
            status: CaseStatus::Other,
        });
        let variant_name = unique_name(&format!("{op_kebab}-error"), |n| self.name_in_use(n));
        self.variants.push(VariantModel {
            name_kebab: variant_name.clone(),
            cases,
        });
        Ok(WitType::Named(variant_name))
    }

    /// Drop records that no operation can reach, so request-body records whose fields were
    /// don't linger as dead WIT. Records exist solely to support operation params/bodies, so
    /// the live set is everything reachable from an operation's fields by following
    /// record→record edges to a fixpoint. Anything outside that set is unreferenced and safe to
    /// remove; pruning can never create a dangling reference.
    pub(crate) fn prune_unused_records(&mut self) {
        let mut reachable: BTreeSet<String> = BTreeSet::new();
        let mut worklist: Vec<String> = Vec::new();
        let mark = |ty: &WitType, reachable: &mut BTreeSet<String>, work: &mut Vec<String>| {
            let mut names = BTreeSet::new();
            collect_named(ty, &mut names);
            for n in names {
                if reachable.insert(n.clone()) {
                    work.push(n);
                }
            }
        };
        for op in &self.operations {
            for f in &op.fields {
                mark(&f.ty, &mut reachable, &mut worklist);
            }
            // Response types keep their records alive too: a success body lowered to a record
            // (and everything reachable from it) is referenced only through `ok_ty`, never a
            // field, so without marking it here `prune_unused_records` would delete it.
            mark(&op.ok_ty, &mut reachable, &mut worklist);
            mark(&op.err_ty, &mut reachable, &mut worklist);
        }
        while let Some(name) = worklist.pop() {
            if let Some(rec) = self.records.iter().find(|r| r.name_kebab == name).cloned() {
                for f in &rec.fields {
                    mark(&f.ty, &mut reachable, &mut worklist);
                }
            }
        }
        self.records.retain(|r| reachable.contains(&r.name_kebab));
    }
}

/// Resolve a parameter reference to the concrete [`Parameter`], following
/// `#/components/parameters/*` `$ref`s. Returns `None` — after printing a diagnostic — when a
/// reference can't be resolved, so a bad `$ref` skips just that parameter instead of the whole
/// operation, and never vanishes silently.
/// Build a WIT enum's `(case-name, wire-value)` pairs from a string schema's `enum` list.
///
/// Enum case names must be valid, non-empty, and unique within the enum. A value like `/`
/// kebab-collapses to empty and distinct values can collide after sanitizing, so route each
/// through `sanitize_wit_name` (non-empty) and `unique_name` (deduped).
fn string_enum_cases(s: &openapiv3::StringType) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for v in s.enumeration.iter().flatten() {
        let base = if v.is_empty() {
            "empty".to_string()
        } else {
            sanitize_wit_name(&v.to_kebab_case())
        };
        let name = unique_name(&base, |n| seen.contains(n));
        seen.insert(name.clone());
        cases.push((name, v.clone()));
    }
    cases
}

fn resolve_param_ref<'a>(
    ctx: &SchemaCtx<'a>,
    p: &'a ReferenceOr<Parameter>,
) -> Option<&'a Parameter> {
    match p {
        ReferenceOr::Item(param) => Some(param),
        ReferenceOr::Reference { reference } => match ctx.resolve_parameter(reference) {
            Ok(param) => Some(param),
            Err(err) => {
                eprintln!(
                    "openapi-bindgen: skipping unresolved parameter $ref `{reference}`: {err}"
                );
                None
            }
        },
    }
}

/// The `(location, name)` identity of a parameter, used to dedupe path-item-level against
/// operation-level parameters (operation-level wins). OpenAPI keys parameter uniqueness on the
/// `name` + `in` pair, so both are part of the key.
fn param_key(p: &Parameter) -> (String, String) {
    let (loc, data) = match p {
        Parameter::Path { parameter_data, .. } => ("path", parameter_data),
        Parameter::Query { parameter_data, .. } => ("query", parameter_data),
        Parameter::Header { parameter_data, .. } => ("header", parameter_data),
        Parameter::Cookie { parameter_data, .. } => ("cookie", parameter_data),
    };
    (loc.to_string(), data.name.clone())
}

/// Pick which success (2xx) response to bind as the operation's `ok` type.
///
/// Preference order: `200`, then `201`, then the lowest declared 2xx status code, then a `2XX`
/// range. `default` is deliberately excluded — it is a catch-all that conventionally carries an
/// *error* body — so an operation whose only response is `default` returns the raw body string.
fn select_success_response(responses: &Responses) -> Option<&ReferenceOr<Response>> {
    if let Some(r) = responses.responses.get(&StatusCode::Code(200)) {
        return Some(r);
    }
    if let Some(r) = responses.responses.get(&StatusCode::Code(201)) {
        return Some(r);
    }
    let mut best: Option<(u16, &ReferenceOr<Response>)> = None;
    for (code, r) in &responses.responses {
        if let StatusCode::Code(c) = code
            && (200..300).contains(c)
            && best.is_none_or(|(b, _)| *c < b)
        {
            best = Some((*c, r));
        }
    }
    if let Some((_, r)) = best {
        return Some(r);
    }
    responses.responses.get(&StatusCode::Range(2))
}

/// Resolve a response reference to the concrete [`Response`], following
/// `#/components/responses/*` `$ref`s. Returns `None` — after a diagnostic — when a reference
/// can't be resolved, so a bad `$ref` degrades the response to the raw body string instead of
/// failing the whole operation.
fn resolve_response_ref<'a>(
    ctx: &SchemaCtx<'a>,
    r: &'a ReferenceOr<Response>,
) -> Option<&'a Response> {
    match r {
        ReferenceOr::Item(resp) => Some(resp),
        ReferenceOr::Reference { reference } => match ctx.resolve_response(reference) {
            Ok(resp) => Some(resp),
            Err(err) => {
                eprintln!(
                    "openapi-bindgen: skipping unresolved response $ref `{reference}`: {err}"
                );
                None
            }
        },
    }
}

/// Pick which response media type to bind, preferring `application/json`, then any structured
/// `*+json`, then whatever is listed first. Mirrors [`select_body_media`] for responses.
fn select_response_media(resp: &Response) -> Option<&MediaType> {
    let content = &resp.content;
    content
        .get("application/json")
        .or_else(|| {
            content
                .iter()
                .find(|(name, _)| name.ends_with("+json"))
                .map(|(_, media)| media)
        })
        .or_else(|| content.iter().next().map(|(_, media)| media))
}

/// The canonical kebab-case case name for a specific HTTP error status code (the standard
/// reason phrase, kebab-cased). Unknown codes fall back to `status-{code}`.
fn status_case_name(code: u16) -> String {
    let name = match code {
        400 => "bad-request",
        401 => "unauthorized",
        402 => "payment-required",
        403 => "forbidden",
        404 => "not-found",
        405 => "method-not-allowed",
        406 => "not-acceptable",
        407 => "proxy-authentication-required",
        408 => "request-timeout",
        409 => "conflict",
        410 => "gone",
        411 => "length-required",
        412 => "precondition-failed",
        413 => "payload-too-large",
        414 => "uri-too-long",
        415 => "unsupported-media-type",
        416 => "range-not-satisfiable",
        417 => "expectation-failed",
        418 => "im-a-teapot",
        421 => "misdirected-request",
        422 => "unprocessable-entity",
        423 => "locked",
        424 => "failed-dependency",
        425 => "too-early",
        426 => "upgrade-required",
        428 => "precondition-required",
        429 => "too-many-requests",
        431 => "request-header-fields-too-large",
        451 => "unavailable-for-legal-reasons",
        500 => "internal-server-error",
        501 => "not-implemented",
        502 => "bad-gateway",
        503 => "service-unavailable",
        504 => "gateway-timeout",
        505 => "http-version-not-supported",
        506 => "variant-also-negotiates",
        507 => "insufficient-storage",
        508 => "loop-detected",
        510 => "not-extended",
        511 => "network-authentication-required",
        300 => "multiple-choices",
        301 => "moved-permanently",
        302 => "found",
        303 => "see-other",
        304 => "not-modified",
        305 => "use-proxy",
        307 => "temporary-redirect",
        308 => "permanent-redirect",
        _ => return format!("status-{code}"),
    };
    name.to_string()
}

/// The kebab-case case name for a status *range* (`4XX` → `client-error`, etc.), keyed by its
/// leading digit.
fn range_case_name(leading_digit: u16) -> String {
    let name = match leading_digit {
        1 => "informational",
        3 => "redirect",
        4 => "client-error",
        5 => "server-error",
        other => return format!("status-{other}xx"),
    };
    name.to_string()
}

/// Pick which request-body media type to bind.
///
/// Preference order: `application/json`, then any structured `*+json` (e.g.
/// `application/vnd.github+json`), then `application/x-www-form-urlencoded`, then
/// `multipart/form-data`, then whatever is listed first. Historically only `application/json`
/// was read, which silently dropped the *entire* request body for the many real specs that
/// never use it — Stripe and Twilio model every write as `application/x-www-form-urlencoded`,
/// so operations like `post-account-links` emitted a param-less function. Once selected, the
/// media type's schema is lowered exactly as a JSON body would be (object → inline fields,
/// otherwise a single opaque `body: string`).
fn select_body_media(body: &RequestBody) -> Option<&MediaType> {
    let content = &body.content;
    content
        .get("application/json")
        .or_else(|| {
            content
                .iter()
                .find(|(name, _)| name.ends_with("+json"))
                .map(|(_, media)| media)
        })
        .or_else(|| content.get("application/x-www-form-urlencoded"))
        .or_else(|| content.get("multipart/form-data"))
        .or_else(|| content.iter().next().map(|(_, media)| media))
}

/// Whether `field` duplicates a security credential the runtime injects for this operation.
///
/// Matches a non-`Path` field whose snake-cased name equals either the security scheme's name
/// (the key the host provisions the secret under) or, for `apiKey` schemes, the credential's
/// wire name. This is a name-based heuristic: a legitimate request field that happens to share
/// a name with an active security scheme would also be pruned. That trade-off is intentional —
/// the dedup pass is on by default so generated operations stay free of credentials the host
/// already supplies. `Path` fields are structural routing segments and are never pruned.
fn is_injected_credential(field: &Field, auth: &[AuthApply]) -> bool {
    if field.location == Location::Path {
        return false;
    }
    auth.iter().any(|a| {
        if field.name_snake == a.secret_key.to_snake_case() {
            return true;
        }
        match &a.kind {
            AuthKind::ApiKeyHeader { name }
            | AuthKind::ApiKeyQuery { name }
            | AuthKind::ApiKeyCookie { name } => field.name_snake == name.to_snake_case(),
            AuthKind::Bearer | AuthKind::Basic => false,
        }
    })
}

/// A credential inferred from a naked request parameter that no security scheme covers: the
/// synthesized [`AuthApply`] plus the identity of the field to lift out of the operation.
struct InferredCredential {
    apply: AuthApply,
    field_snake: String,
    location: Location,
}

/// Infer host-injected API-key credentials from an operation's *query/header* parameters, for
/// specs that model the key as an ordinary parameter and declare no applicable security scheme.
///
/// Returns one entry per matched parameter; the caller lifts each field off the operation and
/// records the synthesized scheme on the operation's auth table, so the credential is sourced
/// from `wasmcloud:secrets` like any declared scheme. Path and cookie parameters are out of
/// scope (a path key would require substituting the secret into the URL template at runtime).
/// The secret key is the parameter's kebab-cased name; the wire name preserves the original
/// spelling so the request is reproduced verbatim.
fn infer_api_key_credentials(op: &Operation) -> Vec<InferredCredential> {
    let mut out = Vec::new();
    for p in &op.parameters {
        let ReferenceOr::Item(p) = p else { continue };
        let (data, location) = match p {
            Parameter::Query { parameter_data, .. } => (parameter_data, Location::Query),
            Parameter::Header { parameter_data, .. } => (parameter_data, Location::Header),
            Parameter::Path { .. } | Parameter::Cookie { .. } => continue,
        };
        let name_snake = data.name.to_snake_case();
        if !looks_like_api_key(&name_snake, data.required, data.description.as_deref()) {
            continue;
        }
        let kind = match location {
            Location::Header => AuthKind::ApiKeyHeader {
                name: data.name.clone(),
            },
            _ => AuthKind::ApiKeyQuery {
                name: data.name.clone(),
            },
        };
        out.push(InferredCredential {
            apply: AuthApply {
                secret_key: data.name.to_kebab_case(),
                kind,
            },
            field_snake: name_snake,
            location,
        });
    }
    out
}

/// Whether a query/header parameter named `name_snake` looks like an API key that should be
/// lifted into a host-injected secret.
///
/// Two tiers keep false positives low. Unambiguous key names (`api_key`, `apikey`, …) match
/// outright. The generic `key` / `token` match only when the parameter is *required* **and** its
/// description corroborates ("API key", "license key", …); pagination tokens — optional and
/// described as such — are therefore left as ordinary inputs. The lists are intentionally
/// conservative and easy to extend.
fn looks_like_api_key(name_snake: &str, required: bool, description: Option<&str>) -> bool {
    const STRONG: &[&str] = &[
        "api_key",
        "apikey",
        "x_api_key",
        "api_token",
        "access_key",
        "subscription_key",
        "app_key",
        "your_api_key_here",
    ];
    if STRONG.contains(&name_snake) {
        return true;
    }
    if matches!(name_snake, "key" | "token") {
        if !required {
            return false;
        }
        let d = description.unwrap_or_default().to_ascii_lowercase();
        const NEEDLES: &[&str] = &[
            "api key",
            "api-key",
            "api_key",
            "api token",
            "api-token",
            "api_token",
            "access key",
            "license key",
            "subscription key",
        ];
        return NEEDLES.iter().any(|n| d.contains(n));
    }
    false
}

/// Ensure a record's fields have unique WIT names.
///
/// Colliding fields arise three ways: an OpenAPI document can declare the same input twice (TfL
/// repeats `startDate`/`endDate` as "automatically added" query parameters), two distinct source
/// names can sanitize to the same WIT identifier, or two *genuinely different* inputs can share a
/// name — a path parameter and a query parameter both called `path` (Kubernetes' proxy endpoints:
/// the `{path}` URL segment plus a `path` query string), Twilio's `DateCreated`, `DateCreated<`
/// and `DateCreated>` range filters, or a request-body field restating a query/header parameter.
/// OpenAPI keys parameter uniqueness on the (`name`, `in`) pair, and body fields live in yet
/// another location, so these are distinct request inputs that must both be carried — dropping one
/// silently loses an argument and, for a `{placeholder}` collision, can leave the URL unfillable.
///
/// A field carries three names: `wire_name` (verbatim on the HTTP wire), `name_snake` (the internal
/// key of the params record the guest serializes and the runtime reads back) and `name_kebab` (the
/// WIT/struct identifier). We treat two fields as the *same* input — dropping the later — only when
/// they share both `wire_name` and `location`; such a pair is indistinguishable to the server.
/// Otherwise we keep both and repair any name clash without touching `wire_name` or `location`, so
/// each field still travels under its correct name in its correct place:
///
/// * `name_kebab` is made unique with a numeric suffix (wit-bindgen rejects duplicate struct field
///   names, `E0124`); renaming the identifier is always safe.
/// * `name_snake` is made unique so two fields can't clobber each other's value in the params
///   record. A path field's snake must equal its `{placeholder}` (see `snake_path_template`), so
///   when a path field collides with a non-path field we rename the non-path field instead.
fn dedupe_field_names(fields: &mut Vec<Field>) {
    let mut kept: Vec<Field> = Vec::with_capacity(fields.len());
    for mut f in std::mem::take(fields) {
        // Pass 1 — drop true duplicates: same wire name in the same location.
        if kept
            .iter()
            .any(|g| g.location == f.location && g.wire_name == f.wire_name)
        {
            continue;
        }
        // Pass 2 — unique WIT identifier.
        if kept.iter().any(|g| g.name_kebab == f.name_kebab) {
            f.name_kebab = unique_name(&f.name_kebab, |n| kept.iter().any(|g| g.name_kebab == n));
        }
        // Pass 3 — unique internal JSON key, keeping path fields' placeholder-matching snake.
        if f.location == Location::Path {
            if let Some(pos) = kept
                .iter()
                .position(|g| g.name_snake == f.name_snake && g.location != Location::Path)
            {
                let mut taken: Vec<String> = kept.iter().map(|g| g.name_snake.clone()).collect();
                taken.push(f.name_snake.clone());
                if let Some(g) = kept.get_mut(pos) {
                    g.name_snake = unique_snake(&g.name_kebab, &taken);
                }
            }
        } else if kept.iter().any(|g| g.name_snake == f.name_snake) {
            let taken: Vec<String> = kept.iter().map(|g| g.name_snake.clone()).collect();
            f.name_snake = unique_snake(&f.name_kebab, &taken);
        }
        kept.push(f);
    }
    *fields = kept;
}

/// Derive a `name_snake` not present in `taken`, based on the (already unique) WIT identifier
/// `base_kebab`. Reuses `unique_name`'s suffixing so a unique kebab yields a unique snake.
fn unique_snake(base_kebab: &str, taken: &[String]) -> String {
    let unique_kebab = unique_name(base_kebab, |cand| {
        let cand_snake = cand.replace('-', "_");
        taken.contains(&cand_snake)
    });
    unique_kebab.replace('-', "_")
}

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

        for v in &self.variants {
            out.push_str(&format!("  variant {} {{\n", v.name_kebab));
            for (i, case) in v.cases.iter().enumerate() {
                let comma = if i + 1 < v.cases.len() { "," } else { "" };
                match &case.payload {
                    Some(ty) => out.push_str(&format!(
                        "    {}({}){}\n",
                        case.name_kebab,
                        ty.render(),
                        comma
                    )),
                    None => out.push_str(&format!("    {}{}\n", case.name_kebab, comma)),
                }
            }
            out.push_str("  }\n\n");
        }

        for op in &self.operations {
            if let Some(s) = &op.summary {
                for line in s.lines() {
                    out.push_str(&format!("  /// {line}\n"));
                }
            }
            let ret = render_result_type(&op.ok_ty, &op.err_ty);
            if op.fields.is_empty() {
                // No params — no record, no argument.
                out.push_str(&format!("  {}: func() -> {ret};\n\n", op.op_kebab));
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
                "  {}: func(params: {}) -> {ret};\n\n",
                op.op_kebab, op.params_record
            ));
        }

        out.push_str("}\n");
        out
    }
}

/// Render the WIT return type of an operation: `result<{ok}, {err}>`.
fn render_result_type(ok: &WitType, err: &WitType) -> String {
    format!("result<{}, {}>", ok.render(), err.render())
}

fn ref_or_to_box(r: &ReferenceOr<Schema>) -> ReferenceOr<Box<Schema>> {
    match r {
        ReferenceOr::Reference { reference } => ReferenceOr::Reference {
            reference: reference.clone(),
        },
        ReferenceOr::Item(s) => ReferenceOr::Item(Box::new(s.clone())),
    }
}

/// Collect the names of every `WitType::Named` reachable from `ty` into `out`.
fn collect_named(ty: &WitType, out: &mut BTreeSet<String>) {
    match ty {
        WitType::Named(name) => {
            out.insert(name.clone());
        }
        WitType::Option(inner) | WitType::List(inner) => collect_named(inner, out),
        // Keep the map's entry record alive through the prune, and follow its value type.
        WitType::Map { entry, value } => {
            out.insert(entry.clone());
            collect_named(value, out);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_api_key;

    #[test]
    fn strong_key_names_match_regardless_of_corroboration() {
        for name in [
            "api_key",
            "apikey",
            "x_api_key",
            "access_key",
            "your_api_key_here",
        ] {
            assert!(looks_like_api_key(name, false, None), "{name} should match");
        }
    }

    #[test]
    fn generic_key_matches_only_with_required_and_description() {
        // ip2location's shape: a required `key` whose description says "API Key".
        assert!(looks_like_api_key(
            "key",
            true,
            Some("API Key. Please sign up free trial license key at ip2location.com")
        ));
        // Required but no corroborating description → not enough signal.
        assert!(!looks_like_api_key(
            "key",
            true,
            Some("A sort key for results")
        ));
        // Corroborating description but optional → left as an ordinary input.
        assert!(!looks_like_api_key("key", false, Some("API key")));
    }

    #[test]
    fn optional_pagination_token_is_not_an_api_key() {
        assert!(!looks_like_api_key(
            "token",
            false,
            Some("The pagination token for the next page")
        ));
        // Even a required `token` needs its description to mention a key/token credential.
        assert!(!looks_like_api_key(
            "token",
            true,
            Some("Opaque cursor for the next page")
        ));
    }

    #[test]
    fn unrelated_parameters_never_match() {
        assert!(!looks_like_api_key(
            "ip",
            true,
            Some("IP address to look up")
        ));
        assert!(!looks_like_api_key("format", false, None));
    }
}
