use crate::{
    Catalog, Error, PackageName,
    model::{Kind, Schema, SchemaId},
};

/// Emit and resolve a complete WIT package containing the `client` world and
/// exported `tool` interface. No HTTP requests or tool calls are made.
///
/// Tools without an output schema return the full MCP result-envelope JSON.
/// Declared outputs return both typed structured content and that same envelope.
/// JSON Schema validation constraints are documented, not enforced by WIT.
pub fn generate_wit(catalog: &Catalog, package: &PackageName) -> Result<String, Error> {
    let mut out = format!("{}\n\n", package.wit_decl());
    docs(
        &mut out,
        "",
        "Typed MCP tools from a complete, offline tools/list catalog.",
    );
    out.push_str("interface tool {\n");
    docs(
        &mut out,
        "    ",
        "The complete standard MCP CallToolResult envelope, serialized as JSON.",
    );
    docs(
        &mut out,
        "    ",
        "Preserves content blocks, structuredContent, isError and metadata.",
    );
    docs(
        &mut out,
        "    ",
        "This is the declared result when outputSchema is absent, not a schema fallback.",
    );
    out.push_str("    type result-envelope-json = string;\n\n");
    docs(
        &mut out,
        "    ",
        "Failures are distinct from successful typed tool results.",
    );
    out.push_str("    variant error {\n");
    for (name, payload, description) in [
        ("transport", "string", "HTTP or transport failure."),
        ("protocol", "string", "MCP protocol or session failure."),
        (
            "tool-execution",
            "result-envelope-json",
            "A tool result with isError set; preserves its full envelope.",
        ),
        (
            "result-decoding",
            "string",
            "The result does not match the declared structural schema or numeric representation.",
        ),
    ] {
        docs(&mut out, "        ", description);
        out.push_str(&format!("        {name}({payload}),\n"));
    }
    out.push_str("    }\n\n");
    for schema in &catalog.schemas {
        emit_schema(&mut out, catalog, schema);
    }
    for tool in &catalog.tools {
        let result = if let Some(id) = tool.output {
            docs(
                &mut out,
                "    ",
                &format!("Successful result of MCP tool {:?}.", tool.wire_name),
            );
            out.push_str(&format!("    record {} {{\n", tool.result_name));
            docs(
                &mut out,
                "        ",
                "Typed structuredContent, decoded only after checking isError.",
            );
            out.push_str(&format!(
                "        structured-content: {},\n",
                schema(catalog, id).name
            ));
            docs(
                &mut out,
                "        ",
                "Original complete result envelope, including content and metadata.",
            );
            out.push_str("        envelope-json: result-envelope-json,\n    }\n\n");
            &tool.result_name
        } else {
            "result-envelope-json"
        };
        docs(
            &mut out,
            "    ",
            &format!("Invoke MCP tool {:?}.", tool.wire_name),
        );
        if let Some(description) = &tool.description {
            docs(&mut out, "    ", description);
        }
        if tool.output.is_none() {
            docs(
                &mut out,
                "    ",
                "No outputSchema was declared; success returns the full CallToolResult JSON envelope.",
            );
        }
        let input = schema(catalog, tool.input);
        let arguments = match &input.kind {
            Kind::Object {
                properties,
                additional: None,
            } if properties.is_empty() => String::new(),
            _ => format!("arguments: {}", input.name),
        };
        out.push_str(&format!(
            "    {}: func({arguments}) -> result<{result}, error>;\n\n",
            tool.name
        ));
    }
    out.push_str("}\n\n");
    docs(
        &mut out,
        "",
        "A synchronous WASI 0.2-compatible client exporting the catalog's tools.",
    );
    out.push_str("world client {\n    export tool;\n}\n");
    wit_parser::Resolve::default()
        .push_str("client.wit", &out)
        .map_err(|err| {
            Error::new(
                None,
                "",
                format!("generated WIT failed to resolve: {err:#}"),
            )
        })?;
    Ok(out)
}

fn emit_schema(out: &mut String, catalog: &Catalog, value: &Schema) {
    docs(out, "    ", "A typed JSON Schema value.");
    let source = catalog
        .as_json()
        .pointer(&value.pointer)
        .expect("validated schema pointer");
    if let Some(title) = source.get("title").and_then(serde_json::Value::as_str) {
        docs(out, "    ", title);
    }
    if let Some(description) = &value.description {
        docs(out, "    ", description);
    }
    if !value.constraints.is_empty() {
        docs(
            out,
            "    ",
            &format!(
                "JSON Schema constraints/format (not enforced by WIT): {}",
                serde_json::to_string(&value.constraints).expect("JSON constraints serialize"),
            ),
        );
    }
    if value.nullable {
        docs(
            out,
            "    ",
            "None means explicit JSON null, not an absent object property.",
        );
        out.push_str(&format!(
            "    type {} = option<{}>;\n\n",
            value.name, value.value_name
        ));
        docs(out, "    ", "The non-null value of the nullable schema.");
    }
    let name = &value.value_name;
    match &value.kind {
        Kind::Boolean => alias(out, name, "bool"),
        Kind::String => alias(out, name, "string"),
        Kind::Number(number) => {
            docs(
                out,
                "    ",
                "Wire codecs must range-check conversions and reject non-finite floating-point values.",
            );
            alias(out, name, number.wit());
        }
        Kind::Ref(id) => alias(out, name, &schema(catalog, *id).name),
        Kind::Array(id) => alias(out, name, &format!("list<{}>", schema(catalog, *id).name)),
        Kind::Enum(cases) => {
            out.push_str(&format!("    enum {name} {{\n"));
            for case in cases {
                docs(
                    out,
                    "        ",
                    &format!("JSON string value {:?}.", case.wire_value),
                );
                out.push_str(&format!("        {},\n", case.name));
            }
            out.push_str("    }\n\n");
        }
        Kind::Object {
            properties,
            additional,
        } => {
            if properties.is_empty() && additional.is_none() {
                docs(
                    out,
                    "    ",
                    "The empty JSON object. Component Model records cannot have zero fields.",
                );
                out.push_str(&format!(
                    "    enum {name} {{\n        /// The only valid value: {{}}.\n        empty,\n    }}\n\n"
                ));
                return;
            }
            if properties.is_empty()
                && let Some(additional) = additional
            {
                docs(
                    out,
                    "    ",
                    "JSON object entries. Wire codecs must reject duplicate keys.",
                );
                alias(
                    out,
                    name,
                    &format!(
                        "list<tuple<string, {}>>",
                        schema(catalog, additional.schema).name
                    ),
                );
                return;
            }
            out.push_str(&format!("    record {name} {{\n"));
            for property in properties {
                docs(
                    out,
                    "        ",
                    &format!("JSON property {:?}.", property.wire_name),
                );
                let ty = &schema(catalog, property.schema).name;
                let ty = if property.required {
                    docs(
                        out,
                        "        ",
                        "Required: the wire property must be present.",
                    );
                    ty.clone()
                } else {
                    docs(
                        out,
                        "        ",
                        "Optional: the outer option distinguishes absent from present (including null).",
                    );
                    format!("option<{ty}>")
                };
                out.push_str(&format!("        {}: {ty},\n", property.name));
            }
            if let Some(additional) = additional {
                docs(
                    out,
                    "        ",
                    "Other JSON properties, flattened into the object on the wire.",
                );
                docs(
                    out,
                    "        ",
                    "Keys must be unique and must not match any declared property name.",
                );
                out.push_str(&format!(
                    "        {}: list<tuple<string, {}>>,\n",
                    additional.name,
                    schema(catalog, additional.schema).name
                ));
            }
            out.push_str("    }\n\n");
        }
    }
}

fn alias(out: &mut String, name: &str, ty: &str) {
    out.push_str(&format!("    type {name} = {ty};\n\n"));
}

fn schema(catalog: &Catalog, id: SchemaId) -> &Schema {
    catalog.schemas.get(id).expect("validated schema arena ID")
}

fn docs(out: &mut String, indent: &str, text: &str) {
    for line in text.split(['\n', '\r', '\u{2028}', '\u{2029}']) {
        out.push_str(indent);
        out.push_str("/// ");
        for c in line.chars() {
            if c.is_control()
                || matches!(
                    c,
                    '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{0149}'
                        | '\u{0673}'
                        | '\u{0f77}'
                        | '\u{0f79}'
                        | '\u{17a3}'..='\u{17a4}'
                        | '\u{17b4}'..='\u{17b5}'
                )
            {
                out.extend(c.escape_default());
            } else {
                out.push(c);
            }
        }
        out.push('\n');
    }
}
