//! OpenAPI -> WIT + Rust impl codegen.
//!
//! Reads an OpenAPI 3 document and emits:
//! - a `.wit` file: one interface per tag, with per-operation parameter records
//! - a Rust source file: `Guest` trait impls plus manual `*_to_json` / `*_to_str` helpers
//!
//! Inputs, outputs, and the WIT package name are configured on the command line;
//! run with `--help` for usage.
//!
//! We deliberately avoid `additional_derives: [Serialize]` on the wit-bindgen macro because
//! it conflicts with imported wasi resource types. Instead, codegen emits an explicit
//! `<record>_to_json` per WIT record and `<enum>_to_str` per WIT enum.

use anyhow::{Context, Result, bail};
use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};
use indexmap::IndexMap;
use openapiv3::{
    AnySchema, OpenAPI, Operation, Parameter, ParameterSchemaOrContent, ReferenceOr, Schema,
    SchemaKind, Type,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Command-line configuration

const USAGE: &str = "\
Usage: openapi-bindgen --spec <PATH> --wit <PATH> --rust <PATH> --package <NS:NAME[@VER]> [options]

Generate a WIT interface and a Rust `Guest` implementation from an OpenAPI 3 document.

Required:
  --spec <PATH>             Input OpenAPI 3 JSON document
  --wit <PATH>              Output path for the generated `.wit` file
  --rust <PATH>             Output path for the generated Rust source file
  --package <NS:NAME[@VER]> WIT package name, e.g. `incidentio:api@0.1.0`

Options:
  --world <PATH>            WIT world file whose `export` list is rewritten in place
  --tag <TAG>               Only include operations carrying <TAG> (repeatable;
                            default: every operation, grouped by its first tag)
  -h, --help                Print this help and exit
";

/// Parsed command-line configuration.
#[derive(Debug)]
struct Config {
    spec: PathBuf,
    wit_out: PathBuf,
    rust_out: PathBuf,
    world: Option<PathBuf>,
    package: PackageName,
    /// When `Some`, only operations carrying one of these tags are emitted.
    tags: Option<Vec<String>>,
}

/// A WIT package identifier: `namespace:name` with an optional `@version`.
#[derive(Debug)]
struct PackageName {
    namespace: String,
    name: String,
    version: Option<String>,
}

impl PackageName {
    /// Render the `package ...;` declaration line for a `.wit` file.
    fn wit_decl(&self) -> String {
        match &self.version {
            Some(version) => format!("package {}:{}@{};", self.namespace, self.name, version),
            None => format!("package {}:{};", self.namespace, self.name),
        }
    }

    /// Render the Rust module path (`namespace::name`) that wit-bindgen generates.
    fn rust_path(&self) -> String {
        format!(
            "{}::{}",
            self.namespace.replace('-', "_"),
            self.name.replace('-', "_")
        )
    }
}

fn parse_package(raw: &str) -> Result<PackageName> {
    let (path, version) = match raw.split_once('@') {
        Some((path, version)) => (path, Some(version.to_string())),
        None => (raw, None),
    };
    let (namespace, name) = path.split_once(':').with_context(|| {
        format!("invalid --package `{raw}`: expected `namespace:name[@version]`")
    })?;
    if namespace.is_empty() || name.is_empty() {
        bail!("invalid --package `{raw}`: namespace and name must both be non-empty");
    }
    Ok(PackageName {
        namespace: namespace.to_string(),
        name: name.to_string(),
        version,
    })
}

fn parse_args() -> Result<Config> {
    let mut spec: Option<PathBuf> = None;
    let mut wit_out: Option<PathBuf> = None;
    let mut rust_out: Option<PathBuf> = None;
    let mut world: Option<PathBuf> = None;
    let mut package: Option<PackageName> = None;
    let mut tags: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // Split `--flag=value` into its parts; `--flag value` leaves `inline` as None.
        let mut parts = arg.splitn(2, '=');
        let flag = parts.next().unwrap_or_default().to_string();
        let inline = parts.next().map(str::to_string);
        match flag.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--spec" => spec = Some(take_value(&flag, inline, &mut args)?.into()),
            "--wit" => wit_out = Some(take_value(&flag, inline, &mut args)?.into()),
            "--rust" => rust_out = Some(take_value(&flag, inline, &mut args)?.into()),
            "--world" => world = Some(take_value(&flag, inline, &mut args)?.into()),
            "--package" => package = Some(parse_package(&take_value(&flag, inline, &mut args)?)?),
            "--tag" => tags.push(take_value(&flag, inline, &mut args)?),
            other => bail!("unexpected argument `{other}`; run with --help for usage"),
        }
    }

    Ok(Config {
        spec: spec.context("missing required --spec <PATH>; run with --help for usage")?,
        wit_out: wit_out.context("missing required --wit <PATH>; run with --help for usage")?,
        rust_out: rust_out.context("missing required --rust <PATH>; run with --help for usage")?,
        world,
        package: package
            .context("missing required --package <NS:NAME[@VER]>; run with --help for usage")?,
        tags: if tags.is_empty() { None } else { Some(tags) },
    })
}

/// Resolve a flag's value: the inline `--flag=value` form if present, otherwise
/// the following argument.
fn take_value(
    flag: &str,
    inline: Option<String>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String> {
    match inline {
        Some(value) => Ok(value),
        None => args
            .next()
            .with_context(|| format!("flag `{flag}` expects a value")),
    }
}

/// Create the parent directory of `path` if it does not already exist.
fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Intermediate model

#[derive(Debug, Clone)]
enum WitType {
    String,
    Bool,
    S32,
    S64,
    F64,
    Option(Box<WitType>),
    List(Box<WitType>),
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Body,
}

#[derive(Debug, Clone)]
struct Field {
    name_kebab: String,
    name_snake: String,
    description: Option<String>,
    ty: WitType,
    location: Location,
}

#[derive(Debug, Clone)]
struct OperationModel {
    op_kebab: String,
    op_snake: String,
    method: String,
    path_template: String,
    summary: Option<String>,
    params_record: String, // kebab-case
    fields: Vec<Field>,
}

#[derive(Debug, Clone)]
struct RecordModel {
    name_kebab: String,
    description: Option<String>,
    fields: Vec<Field>,
}

#[derive(Debug, Clone)]
struct EnumModel {
    name_kebab: String,
    /// (kebab-case for WIT, original-case for the API wire)
    cases: Vec<(String, String)>,
}

#[derive(Debug)]
struct InterfaceModel {
    name_kebab: String,
    operations: Vec<OperationModel>,
    records: Vec<RecordModel>,
    enums: Vec<EnumModel>,
    /// Records currently being emitted (the recursion stack). A `$ref` to a
    /// record in this set is a cycle and gets degraded to `string`.
    emitting: BTreeSet<String>,
}

impl InterfaceModel {
    fn new(name_kebab: String) -> Self {
        Self {
            name_kebab,
            operations: vec![],
            records: vec![],
            enums: vec![],
            emitting: BTreeSet::new(),
        }
    }

    fn is_enum(&self, name_kebab: &str) -> bool {
        self.enums.iter().any(|e| e.name_kebab == name_kebab)
    }

    fn is_record(&self, name_kebab: &str) -> bool {
        self.records.iter().any(|r| r.name_kebab == name_kebab)
    }
}

// ---------------------------------------------------------------------------
// Schema resolver

struct SchemaCtx<'a> {
    spec: &'a OpenAPI,
}

impl<'a> SchemaCtx<'a> {
    fn resolve_schema(&self, r: &'a ReferenceOr<Schema>) -> Result<&'a Schema> {
        match r {
            ReferenceOr::Item(s) => Ok(s),
            ReferenceOr::Reference { reference } => {
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .with_context(|| format!("unsupported ref: {reference}"))?;
                let schemas = self
                    .spec
                    .components
                    .as_ref()
                    .context("spec missing components section")?
                    .schemas
                    .get(name)
                    .with_context(|| format!("unknown schema $ref: {name}"))?;
                self.resolve_schema(schemas)
            }
        }
    }

    fn resolve_schema_boxed(&self, r: &'a ReferenceOr<Box<Schema>>) -> Result<&'a Schema> {
        match r {
            ReferenceOr::Item(s) => Ok(s.as_ref()),
            ReferenceOr::Reference { reference } => {
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .with_context(|| format!("unsupported ref: {reference}"))?;
                let schemas = self
                    .spec
                    .components
                    .as_ref()
                    .context("spec missing components section")?
                    .schemas
                    .get(name)
                    .with_context(|| format!("unknown schema $ref: {name}"))?;
                self.resolve_schema(schemas)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Schema -> WitType mapping

fn map_schema_to_wit_type(
    ctx: &SchemaCtx,
    schema_ref: &ReferenceOr<Box<Schema>>,
    iface: &mut InterfaceModel,
    name_hint: &str,
) -> Result<WitType> {
    if let ReferenceOr::Reference { reference } = schema_ref
        && let Some(simple_name) = reference.strip_prefix("#/components/schemas/")
    {
        let record_name = simple_name.to_kebab_case();
        // Cycle: break by degrading to opaque string.
        if iface.emitting.contains(&record_name) {
            return Ok(WitType::String);
        }
        if iface.is_record(&record_name) {
            return Ok(WitType::Named(record_name));
        }
        iface.emitting.insert(record_name.clone());
        let schema = ctx.resolve_schema_boxed(schema_ref)?;
        emit_record_from_schema(ctx, schema, iface, &record_name)?;
        iface.emitting.remove(&record_name);
        return Ok(WitType::Named(record_name));
    }
    let schema = ctx.resolve_schema_boxed(schema_ref)?;
    map_schema_kind(ctx, schema, iface, name_hint)
}

fn map_schema_to_wit_type_unboxed(
    ctx: &SchemaCtx,
    schema_ref: &ReferenceOr<Schema>,
    iface: &mut InterfaceModel,
    name_hint: &str,
) -> Result<WitType> {
    if let ReferenceOr::Reference { reference } = schema_ref
        && let Some(simple_name) = reference.strip_prefix("#/components/schemas/")
    {
        let record_name = simple_name.to_kebab_case();
        if iface.emitting.contains(&record_name) {
            return Ok(WitType::String);
        }
        if iface.is_record(&record_name) {
            return Ok(WitType::Named(record_name));
        }
        iface.emitting.insert(record_name.clone());
        let schema = ctx.resolve_schema(schema_ref)?;
        emit_record_from_schema(ctx, schema, iface, &record_name)?;
        iface.emitting.remove(&record_name);
        return Ok(WitType::Named(record_name));
    }
    let schema = ctx.resolve_schema(schema_ref)?;
    map_schema_kind(ctx, schema, iface, name_hint)
}

fn map_schema_kind(
    ctx: &SchemaCtx,
    schema: &Schema,
    iface: &mut InterfaceModel,
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
                    if let Some(existing) = iface
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
                            iface.is_enum(n) || iface.is_record(n)
                        });
                    iface.enums.push(EnumModel {
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
                        map_schema_to_wit_type(ctx, items, iface, &format!("{name_hint}-item"))?
                    }
                    None => WitType::String,
                };
                Ok(WitType::List(Box::new(item_type)))
            }
            Type::Object(_) => {
                let record_name = unique_name(&name_hint.to_kebab_case(), |n| {
                    iface.is_record(n) || iface.is_enum(n)
                });
                emit_record_from_schema(ctx, schema, iface, &record_name)?;
                Ok(WitType::Named(record_name))
            }
        },
        SchemaKind::OneOf { .. } | SchemaKind::AnyOf { .. } | SchemaKind::AllOf { .. } => {
            Ok(WitType::String)
        }
        SchemaKind::Not { .. } => Ok(WitType::String),
        SchemaKind::Any(any) => map_any_schema(ctx, any, iface, name_hint),
    }
}

fn map_any_schema(
    ctx: &SchemaCtx,
    any: &AnySchema,
    iface: &mut InterfaceModel,
    name_hint: &str,
) -> Result<WitType> {
    if !any.properties.is_empty() {
        let record_name = unique_name(&name_hint.to_kebab_case(), |n| {
            iface.is_record(n) || iface.is_enum(n)
        });
        emit_record_from_any(ctx, any, iface, &record_name)?;
        return Ok(WitType::Named(record_name));
    }
    Ok(WitType::String)
}

/// Borrowed `(field-name, field-schema)` entries from an object/`any` schema.
type PropertyEntries<'a> = Vec<(&'a String, &'a ReferenceOr<Box<Schema>>)>;

fn emit_record_from_schema(
    ctx: &SchemaCtx,
    schema: &Schema,
    iface: &mut InterfaceModel,
    record_name: &str,
) -> Result<()> {
    if iface.is_record(record_name) {
        return Ok(());
    }
    let description = schema.schema_data.description.clone();

    let (properties, required): (PropertyEntries<'_>, Vec<String>) = match &schema.schema_kind {
        SchemaKind::Type(Type::Object(o)) => (o.properties.iter().collect(), o.required.clone()),
        SchemaKind::Any(any) => (any.properties.iter().collect(), any.required.clone()),
        _ => {
            iface.records.push(RecordModel {
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

    iface.records.push(RecordModel {
        name_kebab: record_name.to_string(),
        description,
        fields: vec![],
    });
    let idx = iface.records.len() - 1;

    let mut fields = Vec::with_capacity(properties.len());
    for (field_name, schema_ref) in properties {
        let name_kebab = sanitize_wit_name(&field_name.to_kebab_case());
        let nested_hint = format!("{record_name}-{name_kebab}");
        let inner_ty = map_schema_to_wit_type(ctx, schema_ref, iface, &nested_hint)?;
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
    if let Some(record) = iface.records.get_mut(idx) {
        record.fields = fields;
    }
    Ok(())
}

fn emit_record_from_any(
    ctx: &SchemaCtx,
    any: &AnySchema,
    iface: &mut InterfaceModel,
    record_name: &str,
) -> Result<()> {
    if iface.is_record(record_name) {
        return Ok(());
    }
    iface.records.push(RecordModel {
        name_kebab: record_name.to_string(),
        description: None,
        fields: vec![],
    });
    let idx = iface.records.len() - 1;

    let mut fields = vec![];
    for (field_name, schema_ref) in &any.properties {
        let name_kebab = sanitize_wit_name(&field_name.to_kebab_case());
        let nested_hint = format!("{record_name}-{name_kebab}");
        let inner_ty = map_schema_to_wit_type(ctx, schema_ref, iface, &nested_hint)?;
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
    if let Some(record) = iface.records.get_mut(idx) {
        record.fields = fields;
    }
    Ok(())
}

fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
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

fn sanitize_wit_name(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// Operation processing

fn process_operation(
    ctx: &SchemaCtx,
    iface: &mut InterfaceModel,
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

        let raw_ty = if location == Location::Path || is_object_schema(ctx, schema_ref) {
            WitType::String
        } else {
            map_schema_to_wit_type_unboxed(ctx, schema_ref, iface, &hint)?
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
            let body_ty = map_schema_to_wit_type(ctx, &body_schema_ref_wrapped, iface, &hint)?;
            match body_ty {
                WitType::Named(record_name) => {
                    if let Some(rec) = iface
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
                        iface.records.retain(|r| r.name_kebab != record_name);
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

    iface.operations.push(OperationModel {
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

fn ref_or_to_box(r: &ReferenceOr<Schema>) -> ReferenceOr<Box<Schema>> {
    match r {
        ReferenceOr::Reference { reference } => ReferenceOr::Reference {
            reference: reference.clone(),
        },
        ReferenceOr::Item(s) => ReferenceOr::Item(Box::new(s.clone())),
    }
}

fn is_object_schema(ctx: &SchemaCtx, r: &ReferenceOr<Schema>) -> bool {
    let Ok(s) = ctx.resolve_schema(r) else {
        return false;
    };
    matches!(s.schema_kind, SchemaKind::Type(Type::Object(_)))
}

// ---------------------------------------------------------------------------
// WIT emission

fn emit_wit(iface: &InterfaceModel, package: &PackageName) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n\n", package.wit_decl()));
    out.push_str(&format!("interface {} {{\n", iface.name_kebab));

    for e in &iface.enums {
        out.push_str(&format!("  enum {} {{\n", e.name_kebab));
        for (kebab, _) in &e.cases {
            out.push_str(&format!("    {kebab},\n"));
        }
        out.push_str("  }\n\n");
    }

    for r in &iface.records {
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
            let ty = render_wit_type(&f.ty);
            let comma = if i + 1 < r.fields.len() { "," } else { "" };
            out.push_str(&format!("    {}: {}{}\n", f.name_kebab, ty, comma));
        }
        out.push_str("  }\n\n");
    }

    for op in &iface.operations {
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
            let ty = render_wit_type(&f.ty);
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

fn render_wit_type(t: &WitType) -> String {
    match t {
        WitType::String => "string".into(),
        WitType::Bool => "bool".into(),
        WitType::S32 => "s32".into(),
        WitType::S64 => "s64".into(),
        WitType::F64 => "f64".into(),
        WitType::Option(inner) => format!("option<{}>", render_wit_type(inner)),
        WitType::List(inner) => format!("list<{}>", render_wit_type(inner)),
        WitType::Named(name) => name.clone(),
    }
}

// ---------------------------------------------------------------------------
// generated.rs emission

fn emit_rust(ifaces: &[InterfaceModel], package: &PackageName) -> String {
    let mut out = String::new();
    out.push_str("// AUTOGENERATED by codegen. Do not edit by hand.\n");
    out.push_str("#![allow(unused, clippy::all, non_snake_case)]\n\n");
    out.push_str("use crate::runtime::{dispatch, FieldLocation, FieldSpec, OpSpec};\n");
    out.push_str("use serde_json::{Map, Value};\n\n");

    let rust_path = package.rust_path();
    for iface in ifaces {
        let mod_snake = iface.name_kebab.replace('-', "_");
        out.push_str(&format!(
            "use crate::exports::{rust_path}::{mod_snake};\n\n"
        ));

        // OpSpec consts
        for op in &iface.operations {
            let const_name = op_const_name(iface, op);
            out.push_str(&format!("const {const_name}: OpSpec = OpSpec {{\n"));
            out.push_str(&format!("    method: \"{}\",\n", op.method));
            out.push_str(&format!("    path_template: \"{}\",\n", op.path_template));
            out.push_str("    fields: &[\n");
            for f in &op.fields {
                let loc = match f.location {
                    Location::Path => "FieldLocation::Path",
                    Location::Query => "FieldLocation::Query",
                    Location::Body => "FieldLocation::Body",
                };
                out.push_str(&format!(
                    "        FieldSpec {{ snake: \"{}\", location: {loc} }},\n",
                    f.name_snake
                ));
            }
            out.push_str("    ],\n");
            out.push_str("};\n\n");
        }

        // Enum -> &'static str helpers (namespaced by interface to avoid collisions)
        for e in &iface.enums {
            let fn_name = helper_name(&mod_snake, &e.name_kebab, "to_str");
            let pascal = e.name_kebab.to_upper_camel_case();
            out.push_str(&format!(
                "fn {fn_name}(e: &{mod_snake}::{pascal}) -> &'static str {{\n"
            ));
            out.push_str("    match e {\n");
            for (kebab, raw) in &e.cases {
                let case_pascal = kebab.to_upper_camel_case();
                out.push_str(&format!(
                    "        {mod_snake}::{pascal}::{case_pascal} => \"{raw}\",\n"
                ));
            }
            out.push_str("    }\n");
            out.push_str("}\n\n");
        }

        // Record -> Value helpers (for both supporting records and per-op params records)
        for r in &iface.records {
            emit_record_to_json(&mut out, &mod_snake, &r.name_kebab, &r.fields, iface);
        }
        for op in &iface.operations {
            if !op.fields.is_empty() {
                emit_record_to_json(&mut out, &mod_snake, &op.params_record, &op.fields, iface);
            }
        }

        // Guest impl
        out.push_str(&format!(
            "impl {mod_snake}::Guest for crate::Component {{\n"
        ));
        for op in &iface.operations {
            let const_name = op_const_name(iface, op);
            if op.fields.is_empty() {
                out.push_str(&format!(
                    "    fn {}() -> Result<String, String> {{\n",
                    op.op_snake
                ));
                out.push_str(&format!(
                    "        dispatch(&{const_name}, Value::Object(Map::new()))\n"
                ));
                out.push_str("    }\n");
                continue;
            }
            let params_pascal = op.params_record.to_upper_camel_case();
            let to_json = helper_name(&mod_snake, &op.params_record, "to_json");
            out.push_str(&format!(
                "    fn {}(params: {mod_snake}::{params_pascal}) -> Result<String, String> {{\n",
                op.op_snake
            ));
            out.push_str(&format!("        let json = {to_json}(&params);\n"));
            out.push_str(&format!("        dispatch(&{const_name}, json)\n"));
            out.push_str("    }\n");
        }
        out.push_str("}\n");
    }

    out
}

fn op_const_name(iface: &InterfaceModel, op: &OperationModel) -> String {
    format!(
        "OP_{}_{}",
        iface.name_kebab.replace('-', "_").to_uppercase(),
        op.op_snake.to_uppercase()
    )
}

/// Build an interface-scoped helper name. `kebab` is the kebab-case type name;
/// `suffix` is e.g. "to_json" or "to_str".
fn helper_name(mod_snake: &str, kebab: &str, suffix: &str) -> String {
    let body = kebab.replace('-', "_");
    format!("{mod_snake}__{body}__{suffix}")
}

fn emit_record_to_json(
    out: &mut String,
    mod_snake: &str,
    record_kebab: &str,
    fields: &[Field],
    iface: &InterfaceModel,
) {
    let fn_name = helper_name(mod_snake, record_kebab, "to_json");
    let pascal = record_kebab.to_upper_camel_case();
    out.push_str(&format!(
        "fn {fn_name}(p: &{mod_snake}::{pascal}) -> Value {{\n"
    ));
    out.push_str("    let mut m = Map::new();\n");
    for f in fields {
        let field_snake = wit_field_to_rust_ident(&f.name_kebab);
        let expr = field_to_json_expr(&format!("&p.{field_snake}"), &f.ty, iface, mod_snake);
        out.push_str(&format!(
            "    m.insert(\"{}\".into(), {});\n",
            f.name_snake, expr
        ));
    }
    out.push_str("    Value::Object(m)\n");
    out.push_str("}\n\n");
}

/// Render an expression that converts `expr` (always a `&T` reference) into a `Value`.
fn field_to_json_expr(expr: &str, ty: &WitType, iface: &InterfaceModel, mod_snake: &str) -> String {
    match ty {
        WitType::String => format!("Value::String(({expr}).clone())"),
        WitType::Bool => format!("Value::Bool(*({expr}))"),
        WitType::S32 | WitType::S64 => {
            format!("Value::Number(serde_json::Number::from(*({expr})))")
        }
        WitType::F64 => format!(
            "serde_json::Number::from_f64(*({expr})).map(Value::Number).unwrap_or(Value::Null)"
        ),
        WitType::Option(inner) => {
            let inner_expr = field_to_json_expr("v", inner, iface, mod_snake);
            format!("match ({expr}) {{ Some(v) => {inner_expr}, None => Value::Null }}")
        }
        WitType::List(inner) => {
            let inner_expr = field_to_json_expr("v", inner, iface, mod_snake);
            format!("Value::Array(({expr}).iter().map(|v| {inner_expr}).collect())")
        }
        WitType::Named(name) => {
            if iface.is_enum(name) {
                let fn_name = helper_name(mod_snake, name, "to_str");
                format!("Value::String({fn_name}({expr}).into())")
            } else if iface.is_record(name) {
                let fn_name = helper_name(mod_snake, name, "to_json");
                format!("{fn_name}({expr})")
            } else {
                "Value::Null".into()
            }
        }
    }
}

fn wit_field_to_rust_ident(kebab: &str) -> String {
    // wit-bindgen converts kebab -> snake; rust reserved words get `r#` prefix.
    let snake = kebab.replace('-', "_");
    if RUST_KEYWORDS.contains(&snake.as_str()) {
        format!("r#{snake}")
    } else {
        snake
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
];

// ---------------------------------------------------------------------------
// Driver

fn main() -> Result<()> {
    let config = parse_args()?;

    eprintln!("reading {}", config.spec.display());
    let bytes =
        fs::read(&config.spec).with_context(|| format!("reading {}", config.spec.display()))?;
    let spec: OpenAPI = serde_json::from_slice(&bytes).context("parsing OpenAPI JSON")?;

    let ctx = SchemaCtx { spec: &spec };

    let mut by_iface: IndexMap<String, InterfaceModel> = IndexMap::new();

    for (path, item_ref) in spec.paths.paths.iter() {
        let item = match item_ref {
            ReferenceOr::Item(p) => p,
            _ => continue,
        };
        for (method, op_opt) in [
            ("get", &item.get),
            ("post", &item.post),
            ("put", &item.put),
            ("patch", &item.patch),
            ("delete", &item.delete),
        ] {
            let Some(op) = op_opt else { continue };
            let matched_tag = match &config.tags {
                Some(allow) => op.tags.iter().find(|t| allow.iter().any(|a| a == *t)),
                None => op.tags.first(),
            };
            let Some(tag) = matched_tag else { continue };

            let iface_name = tag.to_kebab_case();
            let iface = by_iface
                .entry(iface_name.clone())
                .or_insert_with(|| InterfaceModel::new(iface_name.clone()));
            process_operation(&ctx, iface, method, path, op)?;
        }
    }

    let ifaces: Vec<InterfaceModel> = by_iface.into_values().collect();

    let mut wit_body = String::new();
    for (i, iface) in ifaces.iter().enumerate() {
        let body = emit_wit(iface, &config.package);
        if i == 0 {
            wit_body.push_str(&body);
        } else {
            wit_body.push('\n');
            wit_body.push_str(
                &body
                    .lines()
                    .skip_while(|l| l.starts_with("package"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    ensure_parent_dir(&config.wit_out)?;
    fs::write(&config.wit_out, &wit_body)
        .with_context(|| format!("writing {}", config.wit_out.display()))?;
    eprintln!("wrote {}", config.wit_out.display());

    let rust_src = emit_rust(&ifaces, &config.package);
    ensure_parent_dir(&config.rust_out)?;
    fs::write(&config.rust_out, rust_src)
        .with_context(|| format!("writing {}", config.rust_out.display()))?;
    eprintln!("wrote {}", config.rust_out.display());

    if let Some(world_path) = &config.world {
        let world = fs::read_to_string(world_path)
            .with_context(|| format!("reading {}", world_path.display()))?;
        let new_world = rewrite_world_exports(
            &world,
            &ifaces
                .iter()
                .map(|i| i.name_kebab.clone())
                .collect::<Vec<_>>(),
        );
        if new_world != world {
            fs::write(world_path, new_world)
                .with_context(|| format!("writing {}", world_path.display()))?;
            eprintln!("updated {}", world_path.display());
        }
    }
    Ok(())
}

fn rewrite_world_exports(src: &str, interfaces: &[String]) -> String {
    let mut out = String::new();
    let mut in_exports = false;
    let mut emitted = false;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("export ") && trimmed.ends_with(';') {
            if !emitted {
                for iface in interfaces {
                    out.push_str(&format!("  export {iface};\n"));
                }
                emitted = true;
            }
            in_exports = true;
            continue;
        }
        if in_exports && !trimmed.starts_with("export ") {
            in_exports = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !emitted && let Some(idx) = out.rfind("}\n") {
        let (head, tail) = out.split_at(idx);
        let mut prefix = head.to_string();
        for iface in interfaces {
            prefix.push_str(&format!("  export {iface};\n"));
        }
        prefix.push_str(tail);
        return prefix;
    }
    out
}
