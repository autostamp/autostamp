//! Audit tool: compare generated bindings against their source OpenAPI document.
//!
//! For each spec it reports how many operation inputs the generator *should* emit (path,
//! query, and header parameters — resolving `#/components/parameters/*` refs and merging
//! path-item-level parameters — plus request-body fields for any media type) against how many
//! it *actually* emits, and how many operations produce a broken URL: a `{placeholder}` in the
//! path template with no path field to fill it.
//!
//! The "emitted" side is read from the generated Rust `OpSpec`/`FieldSpec` tables, which are
//! authoritative (they carry each field's request location), so broken-path detection is exact.
//!
//! ```sh
//! cargo run --example audit -- vendor/schemas/APIs/github.com/api.github.com/1.1.4/openapi.yaml
//! cargo run --example audit -- vendor/schemas/APIs/stripe.com vendor/schemas/APIs/kubernetes.io
//! ```

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use openapi_bindgen::{NoOperations, PackageName, generate};
use openapiv3::{OpenAPI, Parameter, ReferenceOr, RequestBody, Schema, SchemaKind, Type};

#[path = "common/spec_loader.rs"]
mod spec_loader;
use spec_loader::load_spec;

/// One audited spec's tallies.
#[derive(Default)]
struct Row {
    name: String,
    spec_ops: usize,
    emitted_ops: usize,
    expected_inputs: usize,
    emitted_fields: usize,
    broken_paths: usize,
    cookie_params: usize,
    note: Option<String>,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        bail!("usage: audit <openapi-path-or-dir> [<openapi-path-or-dir> ...]");
    }
    let single = args.len() == 1;

    let mut rows: Vec<Row> = Vec::new();
    for arg in &args {
        let Some(spec_path) = resolve_spec_path(arg) else {
            rows.push(Row {
                name: arg.clone(),
                note: Some("no OpenAPI/Swagger document found".into()),
                ..Row::default()
            });
            continue;
        };
        rows.push(audit_one(&spec_path, single));
    }

    print_table(&rows);
    Ok(())
}

/// Audit a single spec file.
fn audit_one(spec_path: &Path, detail: bool) -> Row {
    let name = provider_label(spec_path);
    let spec = match load_spec(&spec_path.to_string_lossy()) {
        Ok(spec) => spec,
        Err(err) => {
            return Row {
                name,
                note: Some(format!("parse error: {}", root_cause(&err))),
                ..Row::default()
            };
        }
    };

    let package = PackageName::parse("audit:x@0.1.0").expect("static package name");
    let generated = match generate(&spec, &package, None) {
        Ok(generated) => generated,
        Err(err) if err.downcast_ref::<NoOperations>().is_some() => {
            return Row {
                name,
                note: Some("no operations".into()),
                ..Row::default()
            };
        }
        Err(err) => {
            return Row {
                name,
                note: Some(format!("generate error: {}", root_cause(&err))),
                ..Row::default()
            };
        }
    };

    let (expected_inputs, spec_ops, cookie_params) = expected_from_spec(&spec);
    let emitted = parse_emitted(&generated.rust);

    if detail && !emitted.broken.is_empty() {
        eprintln!("\nbroken paths in {name} (placeholder with no path field):");
        for (path, missing) in emitted.broken.iter().take(40) {
            eprintln!("  {path}  missing: {}", missing.join(", "));
        }
        if emitted.broken.len() > 40 {
            eprintln!("  ... and {} more", emitted.broken.len() - 40);
        }
    }

    if detail && std::env::var("AUDIT_DUMP").is_ok() {
        dump_body_drop_causes(&spec);
        dump_param_drop_detail(&spec, &generated.rust);
    }

    Row {
        name,
        spec_ops,
        emitted_ops: emitted.ops,
        expected_inputs,
        emitted_fields: emitted.fields,
        broken_paths: emitted.broken.len(),
        cookie_params,
        note: None,
    }
}

// ---------------------------------------------------------------------------
// Expected side: what a faithful generator should emit, read from the spec.

/// Returns `(expected_input_fields, operation_count, cookie_param_count)` summed over every
/// operation in the document.
fn expected_from_spec(spec: &OpenAPI) -> (usize, usize, usize) {
    let mut expected = 0usize;
    let mut ops = 0usize;
    let mut cookies = 0usize;

    for (_path, item_ref) in spec.paths.paths.iter() {
        let ReferenceOr::Item(item) = item_ref else {
            continue;
        };
        for op in [&item.get, &item.post, &item.put, &item.patch, &item.delete]
            .into_iter()
            .flatten()
        {
            ops += 1;

            // Path-item params first, then operation params; operation-level entries override
            // path-level ones with the same (name, location), matching OpenAPI semantics.
            let mut keys: BTreeSet<(String, String)> = BTreeSet::new();
            for p in item.parameters.iter().chain(op.parameters.iter()) {
                let Some(param) = resolve_param(spec, p) else {
                    continue;
                };
                match param_loc_name(param) {
                    Some((loc, n)) => {
                        keys.insert((loc.to_string(), n));
                    }
                    None => cookies += 1,
                }
            }
            expected += keys.len();

            if let Some(body_ref) = &op.request_body {
                expected += expected_body_fields(spec, body_ref);
            }
        }
    }
    (expected, ops, cookies)
}

/// The `(location, name)` of a path/query/header parameter, or `None` for a cookie parameter
/// (counted separately).
fn param_loc_name(p: &Parameter) -> Option<(&'static str, String)> {
    let (loc, data) = match p {
        Parameter::Path { parameter_data, .. } => ("path", parameter_data),
        Parameter::Query { parameter_data, .. } => ("query", parameter_data),
        Parameter::Header { parameter_data, .. } => ("header", parameter_data),
        Parameter::Cookie { .. } => return None,
    };
    Some((loc, data.name.clone()))
}

/// How many request-body input fields a faithful generator would inline for `body_ref`:
/// the top-level property count of the selected media type's (object / `allOf`) schema, a
/// single `body` field for a non-object schema, or `0` when there is no usable body schema.
fn expected_body_fields(spec: &OpenAPI, body_ref: &ReferenceOr<RequestBody>) -> usize {
    let Some(body) = resolve_request_body(spec, body_ref) else {
        return 0;
    };
    let Some(media) = select_body_media(body) else {
        return 0;
    };
    let Some(schema_ref) = &media.schema else {
        return 0;
    };
    let Some(schema) = resolve_schema(spec, schema_ref) else {
        return 1;
    };
    count_top_level_fields(spec, schema)
}

/// Count the top-level properties an object/`allOf` schema contributes; `1` for anything else
/// (a non-object body becomes a single `body` field, matching the generator).
fn count_top_level_fields(spec: &OpenAPI, schema: &Schema) -> usize {
    match &schema.schema_kind {
        SchemaKind::Type(Type::Object(o)) => o.properties.len().max(1),
        SchemaKind::Any(a) if !a.properties.is_empty() => a.properties.len(),
        SchemaKind::AllOf { all_of } => {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for member in all_of {
                if let Some(m) = resolve_schema(spec, member) {
                    for key in schema_property_names(m) {
                        names.insert(key);
                    }
                }
            }
            names.len().max(1)
        }
        _ => 1,
    }
}

/// The top-level property names of an object/`any` schema (empty for other kinds).
fn schema_property_names(schema: &Schema) -> Vec<String> {
    match &schema.schema_kind {
        SchemaKind::Type(Type::Object(o)) => o.properties.keys().cloned().collect(),
        SchemaKind::Any(a) => a.properties.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// Classify a body schema by `SchemaKind` and estimate how many fields the *current* generator
/// inlines for it: `object` / `any`-with-properties inline each property (a record), while
/// everything else — `allOf`/`oneOf`/`anyOf`/`not`, empty objects, and scalar/array bodies —
/// collapses to a single opaque `body: string`.
fn body_kind_and_emit(schema: &Schema) -> (&'static str, usize) {
    match &schema.schema_kind {
        SchemaKind::Type(Type::Object(o)) => ("object", o.properties.len()),
        SchemaKind::Type(Type::String(_)) => ("string", 1),
        SchemaKind::Type(Type::Number(_)) => ("number", 1),
        SchemaKind::Type(Type::Integer(_)) => ("integer", 1),
        SchemaKind::Type(Type::Boolean(_)) => ("boolean", 1),
        SchemaKind::Type(Type::Array(_)) => ("array", 1),
        SchemaKind::OneOf { .. } => ("oneOf", 1),
        SchemaKind::AnyOf { .. } => ("anyOf", 1),
        SchemaKind::AllOf { .. } => ("allOf", 1),
        SchemaKind::Not { .. } => ("not", 1),
        SchemaKind::Any(a) if a.properties.is_empty() => ("any-empty", 1),
        SchemaKind::Any(a) => ("any-props", a.properties.len()),
    }
}

/// Env-gated (`AUDIT_DUMP=1`, single-spec mode) breakdown of where request-body input drops
/// originate, bucketed by the body schema's `SchemaKind`. For each kind it prints the operation
/// count, the fields a faithful generator should inline, the fields the current generator emits,
/// and the resulting drop. This isolates real bugs (`allOf` degrading to `string`, empty-object
/// bodies) from the intentional `oneOf`/`anyOf` -> `string` choice, which shows zero drop.
fn dump_body_drop_causes(spec: &OpenAPI) {
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<&'static str, (usize, usize, usize)> = BTreeMap::new();
    for (_path, item_ref) in spec.paths.paths.iter() {
        let ReferenceOr::Item(item) = item_ref else {
            continue;
        };
        for op in [&item.get, &item.post, &item.put, &item.patch, &item.delete]
            .into_iter()
            .flatten()
        {
            let Some(body_ref) = &op.request_body else {
                continue;
            };
            let Some(body) = resolve_request_body(spec, body_ref) else {
                continue;
            };
            let Some(media) = select_body_media(body) else {
                continue;
            };
            let Some(schema_ref) = &media.schema else {
                continue;
            };
            let Some(schema) = resolve_schema(spec, schema_ref) else {
                let entry = buckets.entry("<unresolved-ref>").or_default();
                entry.0 += 1;
                entry.1 += 1;
                entry.2 += 1;
                continue;
            };
            let (kind, emit) = body_kind_and_emit(schema);
            let expect = count_top_level_fields(spec, schema);
            let entry = buckets.entry(kind).or_default();
            entry.0 += 1;
            entry.1 += expect;
            entry.2 += emit;
        }
    }
    eprintln!("\nbody-field drop causes (kind | ops | expect | emitted | drop):");
    let mut total_drop = 0usize;
    for (kind, (ops, expect, emit)) in &buckets {
        let dropped = expect.saturating_sub(*emit);
        total_drop += dropped;
        eprintln!("  {kind:<18} {ops:>5} {expect:>7} {emit:>8} {dropped:>7}");
    }
    eprintln!("  body-driven drop total: {total_drop}");
}

/// Collapse a path template's `{placeholder}` names to bare `{}` so a spec path and the
/// generator's (placeholder-snake-cased) `path_template` compare equal.
fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(rest.get(..open).unwrap_or(""));
        out.push_str("{}");
        let after = rest.get(open + 1..).unwrap_or("");
        rest = match after.find('}') {
            Some(close) => after.get(close + 1..).unwrap_or(""),
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Env-gated (`AUDIT_DUMP=1`, single-spec mode) per-operation diff of the *parameter* fields the
/// spec declares against those the generator emits. Correlates by `(METHOD, path)` (placeholders
/// normalized to `{}`) and prints operations where the counts differ, listing each side's
/// `location:name`. This surfaces exactly which parameters the generator drops (e.g. via
/// credential pruning or kebab-name dedup) versus what a faithful mapping should carry.
fn dump_param_drop_detail(spec: &OpenAPI, rust: &str) {
    let mut emitted: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for block in rust.split("= OpSpec {").skip(1) {
        let body = block.split("\n};").next().unwrap_or(block);
        let method = extract_str_field(body, "method:").unwrap_or_default();
        let path = extract_str_field(body, "path_template:").unwrap_or_default();
        let mut fields: Vec<(String, String)> = Vec::new();
        for fs in body.split("FieldSpec {").skip(1) {
            let seg = fs.split('}').next().unwrap_or(fs);
            let loc = if seg.contains("FieldLocation::Path") {
                "path"
            } else if seg.contains("FieldLocation::Query") {
                "query"
            } else if seg.contains("FieldLocation::Header") {
                "header"
            } else {
                continue; // body fields aren't parameters
            };
            if let Some(snake) = extract_str_field(seg, "snake:") {
                fields.push((loc.to_string(), snake));
            }
        }
        emitted
            .entry((method.to_uppercase(), normalize_path(&path)))
            .or_default()
            .extend(fields);
    }

    eprintln!("\nparameter drops (method path: expect/emit, then each side's location:name):");
    let mut printed = 0usize;
    let mut total_gap = 0usize;
    for (path, item_ref) in spec.paths.paths.iter() {
        let ReferenceOr::Item(item) = item_ref else {
            continue;
        };
        for (method, op) in [
            ("GET", &item.get),
            ("POST", &item.post),
            ("PUT", &item.put),
            ("PATCH", &item.patch),
            ("DELETE", &item.delete),
        ] {
            let Some(op) = op else {
                continue;
            };
            let mut keys: BTreeSet<(String, String)> = BTreeSet::new();
            for p in item.parameters.iter().chain(op.parameters.iter()) {
                let Some(param) = resolve_param(spec, p) else {
                    continue;
                };
                if let Some((loc, n)) = param_loc_name(param) {
                    keys.insert((loc.to_string(), n));
                }
            }
            let key = (method.to_string(), normalize_path(path));
            let emitted_fields = emitted.get(&key);
            let emitted_count = emitted_fields.map_or(0, Vec::len);
            if keys.len() == emitted_count {
                continue;
            }
            total_gap += keys.len().saturating_sub(emitted_count);
            if printed < 25 {
                printed += 1;
                let expected: Vec<String> = keys.iter().map(|(l, n)| format!("{l}:{n}")).collect();
                let got: Vec<String> = emitted_fields
                    .map(|v| v.iter().map(|(l, n)| format!("{l}:{n}")).collect())
                    .unwrap_or_default();
                eprintln!(
                    "  {method} {path}: expect {} emit {}",
                    keys.len(),
                    emitted_count
                );
                eprintln!("    expected: {expected:?}");
                eprintln!("    emitted:  {got:?}");
            }
        }
    }
    eprintln!("  parameter drop total (expect-emit over mismatched ops): {total_gap}");
}

/// Resolve a parameter, following `#/components/parameters/*` refs to a fixed depth.
fn resolve_param<'a>(spec: &'a OpenAPI, r: &'a ReferenceOr<Parameter>) -> Option<&'a Parameter> {
    let mut reference = match r {
        ReferenceOr::Item(p) => return Some(p),
        ReferenceOr::Reference { reference } => reference.as_str(),
    };
    let components = spec.components.as_ref()?;
    for _ in 0..8 {
        let name = reference.strip_prefix("#/components/parameters/")?;
        match components.parameters.get(name)? {
            ReferenceOr::Item(p) => return Some(p),
            ReferenceOr::Reference { reference: next } => reference = next.as_str(),
        }
    }
    None
}

/// Resolve a request body, following `#/components/requestBodies/*` refs to a fixed depth.
fn resolve_request_body<'a>(
    spec: &'a OpenAPI,
    r: &'a ReferenceOr<RequestBody>,
) -> Option<&'a RequestBody> {
    let mut reference = match r {
        ReferenceOr::Item(b) => return Some(b),
        ReferenceOr::Reference { reference } => reference.as_str(),
    };
    let components = spec.components.as_ref()?;
    for _ in 0..8 {
        let name = reference.strip_prefix("#/components/requestBodies/")?;
        match components.request_bodies.get(name)? {
            ReferenceOr::Item(b) => return Some(b),
            ReferenceOr::Reference { reference: next } => reference = next.as_str(),
        }
    }
    None
}

/// Resolve a schema ref, following `#/components/schemas/{name}` (first segment only) to a
/// fixed depth. Deep JSON-pointer tails aren't needed for top-level field counting.
fn resolve_schema<'a>(spec: &'a OpenAPI, r: &'a ReferenceOr<Schema>) -> Option<&'a Schema> {
    let mut reference = match r {
        ReferenceOr::Item(s) => return Some(s),
        ReferenceOr::Reference { reference } => reference.as_str(),
    };
    let components = spec.components.as_ref()?;
    for _ in 0..8 {
        let tail = reference.strip_prefix("#/components/schemas/")?;
        let name = tail.split('/').next()?;
        match components.schemas.get(name)? {
            ReferenceOr::Item(s) => return Some(s),
            ReferenceOr::Reference { reference: next } => reference = next.as_str(),
        }
    }
    None
}

/// Pick the request-body media type a faithful generator would bind: JSON first, then any
/// `*+json`, then form-urlencoded, then multipart, then whatever comes first.
fn select_body_media(body: &RequestBody) -> Option<&openapiv3::MediaType> {
    let content = &body.content;
    if let Some(m) = content.get("application/json") {
        return Some(m);
    }
    if let Some(m) = content
        .iter()
        .find(|(k, _)| k.ends_with("+json"))
        .map(|(_, m)| m)
    {
        return Some(m);
    }
    if let Some(m) = content.get("application/x-www-form-urlencoded") {
        return Some(m);
    }
    if let Some(m) = content.get("multipart/form-data") {
        return Some(m);
    }
    content.iter().next().map(|(_, m)| m)
}

// ---------------------------------------------------------------------------
// Emitted side: parse the generated Rust `OpSpec`/`FieldSpec` tables.

#[derive(Default)]
struct Emitted {
    ops: usize,
    fields: usize,
    /// `(path_template, missing_placeholders)` for each operation with a broken URL.
    broken: Vec<(String, Vec<String>)>,
}

/// Parse the generated Rust for `OpSpec` blocks and tally operations, emitted fields, and
/// operations whose path template has a `{placeholder}` with no matching path field.
fn parse_emitted(rust: &str) -> Emitted {
    let mut out = Emitted::default();
    for block in rust.split("= OpSpec {").skip(1) {
        let body = block.split("\n};").next().unwrap_or(block);
        out.ops += 1;

        let path = extract_str_field(body, "path_template:");
        let mut path_fields: BTreeSet<String> = BTreeSet::new();
        for fs in body.split("FieldSpec {").skip(1) {
            out.fields += 1;
            let seg = fs.split('}').next().unwrap_or(fs);
            if seg.contains("FieldLocation::Path")
                && let Some(snake) = extract_str_field(seg, "snake:")
            {
                path_fields.insert(snake);
            }
        }

        if let Some(path) = path {
            let missing: Vec<String> = placeholders(&path)
                .into_iter()
                .filter(|ph| !path_fields.contains(ph))
                .collect();
            if !missing.is_empty() {
                out.broken.push((path, missing));
            }
        }
    }
    out
}

/// Extract the string literal following `key` (e.g. `path_template:` or `snake:`) in `hay`.
fn extract_str_field(hay: &str, key: &str) -> Option<String> {
    let idx = hay.find(key)?;
    let after = hay.get(idx + key.len()..)?;
    let q1 = after.find('"')?;
    let rest = after.get(q1 + 1..)?;
    let q2 = rest.find('"')?;
    Some(rest.get(..q2)?.to_string())
}

/// Extract `{placeholder}` names from a path template.
fn placeholders(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        let Some(after) = rest.get(open + 1..) else {
            break;
        };
        let Some(close) = after.find('}') else {
            break;
        };
        if let Some(name) = after.get(..close) {
            out.push(name.to_string());
        }
        rest = after.get(close + 1..).unwrap_or("");
    }
    out
}

// ---------------------------------------------------------------------------
// Spec discovery + reporting.

/// Resolve a CLI argument to a spec file: a file is used directly; a directory is searched for a
/// representative document (preferring OpenAPI 3, then Swagger 2), matching `just gen`.
fn resolve_spec_path(arg: &str) -> Option<PathBuf> {
    let path = Path::new(arg);
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.is_dir() {
        let mut openapi: Vec<PathBuf> = Vec::new();
        let mut swagger: Vec<PathBuf> = Vec::new();
        collect_specs(path, &mut openapi, &mut swagger);
        openapi.sort();
        swagger.sort();
        return openapi
            .into_iter()
            .next()
            .or_else(|| swagger.into_iter().next());
    }
    None
}

/// Recursively collect `openapi.{yaml,json}` and `swagger.{yaml,json}` under `dir`.
fn collect_specs(dir: &Path, openapi: &mut Vec<PathBuf>, swagger: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_specs(&path, openapi, swagger);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            match name {
                "openapi.yaml" | "openapi.json" => openapi.push(path.clone()),
                "swagger.yaml" | "swagger.json" => swagger.push(path.clone()),
                _ => {}
            }
        }
    }
}

/// A short provider label derived from the vendored path (e.g. `.../APIs/github.com/...` ->
/// `github.com`), falling back to the file's parent directory name.
fn provider_label(spec_path: &Path) -> String {
    let components: Vec<&str> = spec_path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if let Some(pos) = components.iter().position(|c| *c == "APIs")
        && let Some(name) = components.get(pos + 1)
    {
        return (*name).to_string();
    }
    spec_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("spec")
        .to_string()
}

/// The deepest `Caused by:` message of an error chain, for a compact one-line note.
fn root_cause(err: &anyhow::Error) -> String {
    err.chain()
        .last()
        .map(ToString::to_string)
        .unwrap_or_else(|| err.to_string())
}

/// Print the per-spec table and grand totals.
fn print_table(rows: &[Row]) {
    println!(
        "\n{:<26} {:>5} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "provider", "ops", "expect", "emitted", "dropped", "broken", "cookie"
    );
    println!("{}", "-".repeat(78));

    let mut totals = HashMap::<&str, usize>::new();
    for row in rows {
        if let Some(note) = &row.note {
            println!("{:<26} {note}", row.name);
            continue;
        }
        let dropped = row.expected_inputs.saturating_sub(row.emitted_fields);
        println!(
            "{:<26} {:>5} {:>8} {:>8} {:>8} {:>8} {:>7}",
            truncate(&row.name, 26),
            row.emitted_ops,
            row.expected_inputs,
            row.emitted_fields,
            dropped,
            row.broken_paths,
            row.cookie_params,
        );
        *totals.entry("ops").or_default() += row.emitted_ops;
        *totals.entry("expect").or_default() += row.expected_inputs;
        *totals.entry("emitted").or_default() += row.emitted_fields;
        *totals.entry("dropped").or_default() += dropped;
        *totals.entry("broken").or_default() += row.broken_paths;
        *totals.entry("cookie").or_default() += row.cookie_params;
        let _ = row.spec_ops;
    }

    if rows.len() > 1 {
        println!("{}", "-".repeat(78));
        println!(
            "{:<26} {:>5} {:>8} {:>8} {:>8} {:>8} {:>7}",
            "TOTAL",
            totals.get("ops").copied().unwrap_or_default(),
            totals.get("expect").copied().unwrap_or_default(),
            totals.get("emitted").copied().unwrap_or_default(),
            totals.get("dropped").copied().unwrap_or_default(),
            totals.get("broken").copied().unwrap_or_default(),
            totals.get("cookie").copied().unwrap_or_default(),
        );
    }
    println!(
        "\nexpect = path+query+header params (refs + path-item resolved) + body fields; \
         dropped = expect - emitted; broken = ops with an unfillable {{placeholder}}."
    );
}

/// Truncate `s` to at most `max` characters for table display.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}
