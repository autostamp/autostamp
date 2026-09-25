//! End-to-end catalog diagnostics and resolved WIT regression coverage.

use autostamp_mcp::{Error, PackageName, from_json_value, generate_wit, parse_catalog};
use serde_json::{Value, json};
use wit_parser::{Resolve, Type, TypeDefKind};

fn package() -> PackageName {
    PackageName::parse("example:service@0.1.0").unwrap()
}

fn closed(properties: Value) -> Value {
    json!({"type":"object", "properties":properties, "additionalProperties":false})
}

fn catalog(input: Value) -> Value {
    json!({"tools":[{"name":"test_tool", "inputSchema":input}]})
}

fn emit(value: Value) -> String {
    let catalog = from_json_value(value).unwrap();
    let wit = generate_wit(&catalog, &package()).unwrap();
    resolve(&wit);
    wit
}

fn resolve(wit: &str) -> Resolve {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("fixture.wit", wit).unwrap();
    let bytes = wit_component::encode(&resolve, package).unwrap();
    wasmparser::Validator::new().validate_all(&bytes).unwrap();
    resolve
}

fn kind<'a>(resolve: &'a Resolve, name: &str) -> &'a TypeDefKind {
    &resolve
        .types
        .iter()
        .find(|(_, ty)| ty.name.as_deref() == Some(name))
        .unwrap()
        .1
        .kind
}

fn shape(resolve: &Resolve, ty: Type) -> String {
    match ty {
        Type::Id(id) => match &resolve.types.get(id).unwrap().kind {
            TypeDefKind::Type(ty) => shape(resolve, *ty),
            TypeDefKind::Option(ty) => format!("option<{}>", shape(resolve, *ty)),
            TypeDefKind::List(ty) => format!("list<{}>", shape(resolve, *ty)),
            other => format!("{other:?}"),
        },
        other => format!("{other:?}").to_lowercase(),
    }
}

fn assert_schema_error(schema: Value, suffix: &str) -> Error {
    let err = from_json_value(catalog(closed(json!({"value":schema})))).unwrap_err();
    assert_eq!(err.tool.as_deref(), Some("test_tool"), "{err}");
    assert_eq!(
        err.pointer,
        format!("/tools/0/inputSchema/properties/value{suffix}"),
        "{err}"
    );
    assert!(err.to_string().contains("test_tool"));
    err
}

#[test]
fn working_fixture_preserves_catalog_and_resolves_typed_outputs() {
    let source = include_str!("fixtures/tools.json");
    let parsed = parse_catalog(source).unwrap();
    assert_eq!(parsed.len(), 2);
    assert!(!parsed.is_empty());
    assert_eq!(
        parsed.as_json(),
        &serde_json::from_str::<Value>(source).unwrap()
    );
    let wit = generate_wit(&parsed, &package()).unwrap();
    let resolved = resolve(&wit);
    let TypeDefKind::Record(record) = kind(&resolved, "find-items-result") else {
        panic!()
    };
    assert_eq!(
        record
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["structured-content", "envelope-json"]
    );
    assert!(wit.contains("find-items: func(arguments: find-items-arguments)"));
    assert!(wit.contains("ping: func() -> result<result-envelope-json, error>"));
    assert!(wit.contains("not enforced by WIT"));
    assert!(wit.contains("\"maxLength\":100"));
    assert!(wit.contains("export tool;"));
}

#[test]
fn no_arguments_golden_and_empty_catalog_have_no_dummy_functions() {
    let wit = emit(json!({"tools":[{"name":"ping", "inputSchema":closed(json!({}))}]}));
    assert_eq!(wit, include_str!("fixtures/ping.wit"));
    let parsed = parse_catalog(r#"{"tools":[]}"#).unwrap();
    assert!(parsed.is_empty());
    assert_eq!(parsed.len(), 0);
    let wit = generate_wit(&parsed, &package()).unwrap();
    let resolve = resolve(&wit);
    let (_, interface) = resolve
        .interfaces
        .iter()
        .find(|(_, i)| i.name.as_deref() == Some("tool"))
        .unwrap();
    assert!(interface.functions.is_empty());
    assert_eq!(resolve.worlds.len(), 1);
}

#[test]
fn nested_empty_objects_have_a_component_model_representation() {
    let wit = emit(json!({"tools":[{
        "name":"test_tool",
        "inputSchema":{
            "type":"object",
            "additionalProperties":false,
            "$defs":{"empty":closed(json!({}))},
            "properties":{
                "empty":closed(json!({})),
                "nullable":{"type":["object","null"],"additionalProperties":false},
                "array":{"type":"array","items":closed(json!({}))},
                "map":{"type":"object","additionalProperties":closed(json!({}))},
                "reference":{"$ref":"#/$defs/empty"}
            }
        },
        "outputSchema":closed(json!({}))
    }]}));
    let resolve = resolve(&wit);
    for name in [
        "test-tool-arguments-empty",
        "test-tool-arguments-nullable-value",
        "test-tool-arguments-array-item",
        "test-tool-arguments-map-additional-value",
        "test-tool-arguments-reference-target",
        "test-tool-output",
    ] {
        let TypeDefKind::Enum(value) = kind(&resolve, name) else {
            panic!("{name} must represent the empty JSON object as a singleton enum");
        };
        assert_eq!(value.cases.len(), 1);
        assert_eq!(value.cases.first().unwrap().name, "empty");
    }
    let TypeDefKind::Option(Type::Id(inner)) = kind(&resolve, "test-tool-arguments-nullable")
    else {
        panic!("nullable empty objects must retain their option");
    };
    assert!(matches!(
        resolve.types.get(*inner).unwrap().kind,
        TypeDefKind::Enum(_)
    ));
    let TypeDefKind::Record(input) = kind(&resolve, "test-tool-arguments") else {
        panic!();
    };
    let nullable = input
        .fields
        .iter()
        .find(|field| field.name == "nullable")
        .unwrap();
    let Type::Id(optional) = nullable.ty else {
        panic!()
    };
    let TypeDefKind::Option(Type::Id(nullable)) = resolve.types.get(optional).unwrap().kind else {
        panic!("an optional nullable empty object must retain both option layers");
    };
    assert!(matches!(
        resolve.types.get(nullable).unwrap().kind,
        TypeDefKind::Option(_)
    ));
}

#[test]
fn optional_and_nullable_preserve_all_three_wire_states() {
    let mut input = closed(json!({
        "both":{"type":["string","null"]},
        "optional":{"type":"string"},
        "nullable":{"type":["null","string"]},
        "required":{"type":"string"}
    }));
    input
        .as_object_mut()
        .unwrap()
        .insert("required".into(), json!(["nullable", "required"]));
    let wit = emit(catalog(input));
    let resolve = resolve(&wit);
    let TypeDefKind::Record(record) = kind(&resolve, "test-tool-arguments") else {
        panic!()
    };
    let fields: Vec<_> = record
        .fields
        .iter()
        .map(|f| (f.name.as_str(), shape(&resolve, f.ty)))
        .collect();
    assert_eq!(
        fields,
        [
            ("both", "option<option<string>>".into()),
            ("nullable", "option<string>".into()),
            ("optional", "option<string>".into()),
            ("required", "string".into()),
        ]
    );
}

#[test]
fn homogeneous_arrays_and_all_numeric_representations_are_typed() {
    for (ty, format, expected) in [
        ("integer", None, "s64"),
        ("number", None, "f64"),
        ("integer", Some("int8"), "s8"),
        ("integer", Some("int16"), "s16"),
        ("integer", Some("int32"), "s32"),
        ("integer", Some("int64"), "s64"),
        ("integer", Some("uint8"), "u8"),
        ("integer", Some("uint16"), "u16"),
        ("integer", Some("uint32"), "u32"),
        ("integer", Some("uint64"), "u64"),
        ("number", Some("float"), "f32"),
        ("number", Some("float32"), "f32"),
        ("number", Some("double"), "f64"),
        ("number", Some("float64"), "f64"),
    ] {
        let mut item = json!({"type":ty});
        if let Some(format) = format {
            item.as_object_mut()
                .unwrap()
                .insert("format".into(), json!(format));
        }
        let wit = emit(catalog(closed(
            json!({"values":{"type":"array","items":item}}),
        )));
        assert!(wit.contains(&format!(
            "type test-tool-arguments-values-item = {expected};"
        )));
        assert!(
            wit.contains(
                "type test-tool-arguments-values = list<test-tool-arguments-values-item>;"
            )
        );
    }
    assert_schema_error(json!({"type":"integer","format":"decimal"}), "/format");
    assert_schema_error(json!({"type":"number","format":"int32"}), "/format");
}

#[test]
fn typed_maps_and_mixed_objects_keep_extra_keys() {
    let wit = emit(catalog(json!({
        "type":"object", "additionalProperties":{"type":"boolean"}
    })));
    assert!(wit.contains(
        "type test-tool-arguments = list<tuple<string, test-tool-arguments-additional-value>>;"
    ));
    assert!(wit.contains("test-tool: func(arguments: test-tool-arguments)"));
    let wit = emit(catalog(json!({
        "type":"object",
        "properties":{"additional-properties":{"type":"string"},"known":{"type":"integer"}},
        "additionalProperties":{"type":"boolean"}
    })));
    let resolve = resolve(&wit);
    let TypeDefKind::Record(record) = kind(&resolve, "test-tool-arguments") else {
        panic!()
    };
    assert_eq!(
        record
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["additional-properties-v2", "known", "additional-properties"]
    );
    assert!(wit.contains("flattened into the object"));
}

#[test]
fn untyped_open_objects_are_never_silently_closed() {
    let error = parse_catalog(include_str!("fixtures/open-object.json")).unwrap_err();
    assert_eq!(error.tool.as_deref(), Some("unsafe_shape"));
    assert_eq!(error.pointer, "/tools/0/inputSchema/additionalProperties");
    for extra in [
        json!(true),
        json!({}),
        json!(null),
        json!("false"),
        json!([]),
    ] {
        let err = from_json_value(catalog(
            json!({"type":"object","additionalProperties":extra}),
        ))
        .unwrap_err();
        assert!(
            err.pointer
                .starts_with("/tools/0/inputSchema/additionalProperties")
        );
        assert_eq!(err.tool.as_deref(), Some("test_tool"));
    }
}

#[test]
fn required_fields_need_explicit_property_schemas() {
    for extra in [json!(false), json!({"type":"string"})] {
        let error = from_json_value(catalog(json!({
            "type":"object","required":["missing"],"additionalProperties":extra
        })))
        .unwrap_err();
        assert_eq!(error.pointer, "/tools/0/inputSchema/required/0");
        assert!(error.message.contains("has no properties schema"));
    }
    let error = from_json_value(catalog(json!({
        "type":"object","properties":{"a":{"type":"string"}},
        "required":["a","a"],"additionalProperties":false
    })))
    .unwrap_err();
    assert_eq!(error.pointer, "/tools/0/inputSchema/required/1");
}

#[test]
fn references_support_pointer_and_uri_escaping_and_share_types() {
    let wit = emit(catalog(json!({
        "type":"object","additionalProperties":false,
        "$defs":{"a/b~c☃":{"type":"string"}},
        "properties":{
            "first":{"$ref":"#/$defs/a~1b~0c%E2%98%83"},
            "second":{"$ref":"#/%24defs/a~1b~0c☃"}
        }
    })));
    assert!(wit.contains("type test-tool-arguments-second = test-tool-arguments-first-target;"));
    let wit = emit(catalog(json!({
        "type":"object","additionalProperties":false,
        "$defs":{
            "outer":{
                "type":"string",
                "$defs":{"inner":{"type":["integer","null"]}}
            }
        },
        "properties":{"value":{"$ref":"#/$defs/outer/$defs/inner"}}
    })));
    assert!(wit.contains("option<test-tool-arguments-value-target-value>"));
}

#[test]
fn recursive_external_missing_and_invalid_references_report_the_ref_site() {
    let error = parse_catalog(include_str!("fixtures/recursive.json")).unwrap_err();
    assert_eq!(error.tool.as_deref(), Some("recursive"));
    assert_eq!(
        error.pointer,
        "/tools/0/inputSchema/$defs/node/properties/next/$ref"
    );
    for reference in [
        "https://example.invalid/schema.json#/$defs/a",
        "#",
        "#name",
        "#/properties/value",
        "#/$defs/missing",
        "#/$defs/a~2b",
        "#/$defs/a%",
        "#/$defs/%FF",
    ] {
        let error = assert_schema_error(json!({"$ref":reference}), "/$ref");
        assert!(!error.message.is_empty());
    }
    let error = from_json_value(catalog(json!({
        "type":"object","additionalProperties":false,
        "$defs":{"a":{"$ref":"#/$defs/b"},"b":{"$ref":"#/$defs/a"}},
        "properties":{"value":{"$ref":"#/$defs/a"}}
    })))
    .unwrap_err();
    assert_eq!(error.pointer, "/tools/0/inputSchema/$defs/b/$ref");
}

#[test]
fn annotations_cannot_masquerade_as_schema_definitions() {
    let error = from_json_value(catalog(json!({
        "type":"object", "additionalProperties":false,
        "default":{"$defs":{"fake":{"type":"string"}}},
        "properties":{"value":{"$ref":"#/default/$defs/fake"}}
    })))
    .unwrap_err();
    assert_eq!(error.pointer, "/tools/0/inputSchema/properties/value/$ref");
    assert!(error.message.contains("non-schema"));
}

#[test]
fn unused_definitions_are_audited_without_false_recursion() {
    let error = from_json_value(catalog(json!({
        "type":"object","additionalProperties":false,"$defs":{"unused":{"oneOf":[]}}
    })))
    .unwrap_err();
    assert_eq!(error.pointer, "/tools/0/inputSchema/$defs/unused/oneOf");
    emit(catalog(json!({
        "type":"object","additionalProperties":false,
        "$defs":{"a":{
            "type":"object","additionalProperties":false,
            "properties":{"value":{
                "type":"string","$defs":{"unused":{"$ref":"#/$defs/a"}}
            }}
        }},
        "properties":{"root":{"$ref":"#/$defs/a"}}
    })));
}

#[test]
fn duplicate_tools_are_distinct_from_normalization_collisions() {
    let error = from_json_value(json!({"tools":[
        {"name":"same","inputSchema":closed(json!({}))},
        {"name":"same","inputSchema":closed(json!({}))}
    ]}))
    .unwrap_err();
    assert_eq!(error.tool.as_deref(), Some("same"));
    assert_eq!(error.pointer, "/tools/1/name");
    assert!(error.message.contains("duplicate original"));
    let tools: Vec<_> = [
        "foo-bar",
        "foo_bar",
        "FooBar",
        "foo-bar-v2",
        "error",
        "result-envelope-json",
        "guest",
        "self",
        "type",
        "gen",
        "123",
        "☃",
        "!!!",
        "ping",
        "ping-arguments",
    ]
    .into_iter()
    .map(|name| json!({"name":name,"inputSchema":closed(json!({}))}))
    .collect();
    let wit = emit(json!({"tools":tools}));
    let resolve = resolve(&wit);
    let (_, interface) = resolve
        .interfaces
        .iter()
        .find(|(_, i)| i.name.as_deref() == Some("tool"))
        .unwrap();
    assert_eq!(interface.functions.len(), 15);
    assert!(interface.functions.contains_key("error-v2"));
    assert!(interface.functions.contains_key("self-value"));
    assert!(interface.functions.contains_key("foo-bar-v3"));
    assert!(wit.contains("enum ping-arguments-v2"));
}

#[test]
fn property_and_enum_names_keep_distinct_wire_spellings() {
    let wit = emit(catalog(closed(json!({
        "fooBar":{"type":"string","enum":["in-progress","in_progress","self","☃",""]},
        "foo-bar":{"type":"boolean"},
        "type":{"type":"string"},
        "":{"type":"boolean"}
    }))));
    let resolve = resolve(&wit);
    let TypeDefKind::Record(record) = kind(&resolve, "test-tool-arguments") else {
        panic!()
    };
    assert_eq!(
        record
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["x", "foo-bar", "foo-bar-v2", "type-value"]
    );
    assert!(wit.contains("JSON property \"fooBar\""));
    assert!(wit.contains("JSON string value \"in_progress\""));
    assert!(wit.contains("in-progress-v2,"));
    assert!(wit.contains("self-value,"));
    assert!(wit.contains("x-v2,"));
}

#[test]
fn output_is_independent_of_tool_property_definition_and_enum_order() {
    let a = r##"{"tools":[
        {"name":"z","inputSchema":{"type":"object","additionalProperties":false}},
        {"name":"a","inputSchema":{"type":"object","additionalProperties":false,
            "$defs":{"b":{"type":"string"},"a":{"type":"boolean"}},
            "properties":{"z":{"type":"string","enum":["b","a"]},"a":{"$ref":"#/$defs/b"}},
            "required":["z","a"]}}
    ]}"##;
    let b = r##"{"tools":[
        {"inputSchema":{"required":["a","z"],"properties":{"a":{"$ref":"#/$defs/b"},"z":{"enum":["a","b"],"type":"string"}},
            "$defs":{"a":{"type":"boolean"},"b":{"type":"string"}},
            "additionalProperties":false,"type":"object"},"name":"a"},
        {"inputSchema":{"additionalProperties":false,"type":"object"},"name":"z"}
    ]}"##;
    let a = generate_wit(&parse_catalog(a).unwrap(), &package()).unwrap();
    let b = generate_wit(&parse_catalog(b).unwrap(), &package()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn every_source_description_line_is_documentation_not_code() {
    let injection = "Safe\n}\rworld injected {}\r\n/*\u{2028}type bad = string;\u{2029}\0\u{202e}";
    let wit = emit(json!({"tools":[{
        "name":"test_tool", "description":injection,
        "inputSchema":{
            "type":"object","additionalProperties":false,
            "title":injection,"description":injection,
            "properties":{"value":{"type":"string","description":injection}}
        }
    }]}));
    let resolve = resolve(&wit);
    assert_eq!(resolve.worlds.len(), 1);
    assert!(
        wit.lines()
            .filter(|line| line.contains("injected"))
            .all(|line| line.trim_start().starts_with("///"))
    );
    assert!(!wit.contains('\0'));
    assert!(!wit.contains('\u{202e}'));
}

#[test]
fn rejected_structural_keywords_never_weaken_the_schema() {
    for keyword in [
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "const",
        "patternProperties",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "unevaluatedProperties",
        "unevaluatedItems",
        "prefixItems",
        "contains",
        "minContains",
        "maxContains",
        "additionalItems",
        "$id",
        "$anchor",
        "$dynamicRef",
        "$dynamicAnchor",
        "$recursiveRef",
        "$vocabulary",
        "definitions",
        "nullable",
        "contentSchema",
        "contentEncoding",
        "unknownKeyword",
    ] {
        let mut schema = json!({"type":"string"});
        schema
            .as_object_mut()
            .unwrap()
            .insert(keyword.into(), json!(true));
        assert_schema_error(schema, &format!("/{keyword}"));
    }
    assert_schema_error(
        json!({"type":"string","$schema":"http://json-schema.org/draft-07/schema#"}),
        "/$schema",
    );
    assert_schema_error(json!({"$ref":"#/$defs/a","type":"string"}), "/type");
}

#[test]
fn malformed_schema_fields_have_precise_locations() {
    for (schema, suffix) in [
        (json!(true), ""),
        (json!(false), ""),
        (json!({}), "/type"),
        (json!({"type":null}), "/type"),
        (json!({"type":[]}), "/type"),
        (json!({"type":["null"]}), "/type"),
        (json!({"type":["string","number"]}), "/type"),
        (json!({"type":["string","string"]}), "/type"),
        (json!({"type":[true]}), "/type/0"),
        (json!({"type":"null"}), "/type"),
        (json!({"type":"file"}), "/type"),
        (json!({"type":"array"}), "/items"),
        (json!({"type":"array","items":[]}), "/items"),
        (json!({"type":"string","enum":[]}), "/enum"),
        (json!({"type":"string","enum":["a","a"]}), "/enum/1"),
        (json!({"type":"string","enum":[1]}), "/enum/0"),
        (json!({"type":"string","enum":[null]}), "/enum/0"),
        (json!({"type":"integer","enum":[1]}), "/enum"),
        (json!({"type":"string","description":true}), "/description"),
        (json!({"type":"string","minLength":-1}), "/minLength"),
        (json!({"type":"string","minLength":1.5}), "/minLength"),
        (json!({"type":"integer","multipleOf":0}), "/multipleOf"),
        (json!({"type":"number","maximum":"10"}), "/maximum"),
        (json!({"type":"string","properties":{}}), "/properties"),
        (json!({"type":"string","items":{"type":"string"}}), "/items"),
        (json!({"type":"boolean","enum":[true]}), "/enum"),
        (json!({"type":"string","$defs":[]}), "/$defs"),
        (json!({"type":"object","properties":[]}), "/properties"),
        (json!({"type":"object","required":"a"}), "/required"),
        (json!({"type":"object","required":[1]}), "/required/0"),
        (
            json!({"type":"array","items":{"type":"string"},"uniqueItems":"yes"}),
            "/uniqueItems",
        ),
    ] {
        assert_schema_error(schema, suffix);
    }
}

#[test]
fn nullable_enum_is_the_intersection_of_type_and_values() {
    for (values, is_nullable) in [(json!(["a", "b", null]), true), (json!(["a", "b"]), false)] {
        let wit = emit(catalog(closed(
            json!({"value":{"type":["string","null"],"enum":values}}),
        )));
        assert_eq!(
            wit.contains("type test-tool-arguments-value = option<"),
            is_nullable
        );
    }
    assert_schema_error(json!({"type":["string","null"],"enum":[null]}), "/enum");
    assert_schema_error(
        json!({"type":["string","null"],"enum":["a",null,null]}),
        "/enum/2",
    );
}

#[test]
fn malformed_catalogs_and_partial_pages_are_errors_not_empty_successes() {
    for (value, pointer) in [
        (json!(null), ""),
        (json!([]), ""),
        (json!({}), "/tools"),
        (json!({"tools":null}), "/tools"),
        (json!({"tools":{}}), "/tools"),
        (json!({"tools":[null]}), "/tools/0"),
        (json!({"tools":[{}]}), "/tools/0/name"),
        (json!({"tools":[{"name":""}]}), "/tools/0/name"),
        (json!({"tools":[{"name":"a"}]}), "/tools/0/inputSchema"),
        (json!({"tools":[],"nextCursor":"next"}), "/nextCursor"),
        (json!({"tools":[],"nextCursor":""}), "/nextCursor"),
        (json!({"tools":[],"nextCursor":null}), "/nextCursor"),
        (json!({"tools":[],"nextCursor":123}), "/nextCursor"),
        (json!({"tools":[],"_meta":[]}), "/_meta"),
        (
            json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}}),
            "/tools",
        ),
    ] {
        let error = from_json_value(value).unwrap_err();
        assert_eq!(error.pointer, pointer, "{error}");
    }
    for json in [
        "{",
        r#"{"tools":[],"tools":[]}"#,
        r#"{"tools":[{"name":"x","name":"y"}]}"#,
    ] {
        assert!(parse_catalog(json).is_err());
    }
}

#[test]
fn only_an_omitted_cursor_marks_a_complete_catalog() {
    for tools in [
        json!([]),
        json!([{"name":"ping","inputSchema":closed(json!({}))}]),
    ] {
        for cursor in ["", "next", " ", "0"] {
            let error =
                parse_catalog(&json!({"tools":tools,"nextCursor":cursor}).to_string()).unwrap_err();
            assert_eq!(error.pointer, "/nextCursor");
            assert!(error.message.contains("incomplete tools/list result"));
        }
        for cursor in [Value::Null, json!(false), json!(0), json!([]), json!({})] {
            let error =
                parse_catalog(&json!({"tools":tools,"nextCursor":cursor}).to_string()).unwrap_err();
            assert_eq!(error.pointer, "/nextCursor");
            assert_eq!(error.message, "expected a string pagination cursor");
        }
        assert!(parse_catalog(&json!({"tools":tools}).to_string()).is_ok());
    }
}

#[test]
fn declared_unsupported_outputs_never_become_json_fallbacks() {
    for output in [
        json!(null),
        json!({}),
        json!({"type":"object"}),
        closed(json!({"value":{"oneOf":[]}})),
        json!({"type":"array","items":{"type":"string"}}),
    ] {
        let error = from_json_value(json!({"tools":[{
            "name":"output", "inputSchema":closed(json!({})), "outputSchema":output
        }]}))
        .unwrap_err();
        assert_eq!(error.tool.as_deref(), Some("output"));
        assert!(error.pointer.starts_with("/tools/0/outputSchema"));
    }
}

#[test]
fn task_required_and_malformed_tool_metadata_are_rejected() {
    for (key, value, suffix) in [
        (
            "execution",
            json!({"taskSupport":"required"}),
            "/execution/taskSupport",
        ),
        (
            "execution",
            json!({"taskSupport":"unknown"}),
            "/execution/taskSupport",
        ),
        ("execution", json!(null), "/execution"),
        (
            "execution",
            json!({"futureMode":true}),
            "/execution/futureMode",
        ),
        ("description", json!(42), "/description"),
        (
            "annotations",
            json!({"readOnlyHint":"yes"}),
            "/annotations/readOnlyHint",
        ),
        ("annotations", json!([]), "/annotations"),
        ("icons", json!([{"src":123}]), "/icons/0/src"),
        ("_meta", json!("bad"), "/_meta"),
        ("unknown", json!(true), "/unknown"),
    ] {
        let mut tool = json!({"name":"test_tool","inputSchema":closed(json!({}))});
        tool.as_object_mut().unwrap().insert(key.into(), value);
        let error = from_json_value(json!({"tools":[tool]})).unwrap_err();
        assert_eq!(error.pointer, format!("/tools/0{suffix}"));
        assert_eq!(error.tool.as_deref(), Some("test_tool"));
    }
    for support in ["forbidden", "optional"] {
        emit(json!({"tools":[{
            "name":"ordinary", "inputSchema":closed(json!({})),
            "execution":{"taskSupport":support},
            "icons":[{"src":"https://example.invalid/icon.png","sizes":["16x16"],"theme":"light"}]
        }]}));
    }
}

#[test]
fn schema_error_pointers_escape_property_names() {
    let error = from_json_value(catalog(closed(json!({"a/b~c":{"oneOf":[]}})))).unwrap_err();
    assert_eq!(
        error.pointer,
        "/tools/0/inputSchema/properties/a~1b~0c/oneOf"
    );
}

#[test]
fn constraints_and_annotations_are_retained_without_claiming_full_validation() {
    let schema = json!({
        "type":"string", "minLength":10, "maxLength":2,
        "pattern":"[", "format":"custom-format",
        "default":null, "examples":[123], "readOnly":true, "writeOnly":true,
        "deprecated":true, "$comment":"These annotations do not change wire structure."
    });
    let value = catalog(closed(json!({"value":schema})));
    let parsed = from_json_value(value.clone()).unwrap();
    assert_eq!(parsed.as_json(), &value);
    let wit = generate_wit(&parsed, &package()).unwrap();
    assert!(wit.contains("not enforced by WIT"));
    assert!(wit.contains("\"minLength\":10"));
    assert!(wit.contains("\"maxLength\":2"));
    assert!(wit.contains("value: option<test-tool-arguments-value>"));
}

#[test]
fn schema_complexity_is_bounded_with_actionable_errors() {
    let mut value = json!({"type":"string"});
    for _ in 0..140 {
        value = json!({"type":"array","items":value});
    }
    let error = from_json_value(catalog(closed(json!({"deep":value})))).unwrap_err();
    assert_eq!(error.tool.as_deref(), Some("test_tool"));
    assert!(
        error
            .pointer
            .starts_with("/tools/0/inputSchema/properties/deep/items")
    );
    assert!(error.message.contains("complexity limit"));
}
