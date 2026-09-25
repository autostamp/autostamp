# autostamp-mcp

Generate typed WIT from a complete MCP `tools/list` result. This library is an
offline schema/catalog layer: it does not discover servers, call tools, emit Rust
codecs, or build client components yet. It is independent of `autostamp-openapi`;
the workspace intentionally contains only these two generator crates.

## Library API

```rust
use autostamp_mcp::{PackageName, generate_wit, parse_catalog};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let json = std::fs::read_to_string("tools.json")?;
    let catalog = parse_catalog(&json)?;
    let package = PackageName::parse("example:service@0.1.0")?;
    let wit = generate_wit(&catalog, &package)?;
    std::fs::write("client.wit", wit)?;
    Ok(())
}
```

`from_json_value` accepts an existing `serde_json::Value`. `Catalog::as_json`
retains the unmodified catalog, including descriptions, annotations and schema
constraints. `Catalog::len` and `is_empty` distinguish an empty valid catalog
from a parse failure. The string parser also rejects duplicate JSON object keys;
those keys cannot be detected after another parser has converted them to `Value`.

Input is the **result object**, not a JSON-RPC response envelope:

```json
{
  "tools": [{
    "name": "greet",
    "description": "Greet a named person.",
    "inputSchema": {
      "type": "object",
      "properties": { "name": { "type": "string" } },
      "required": ["name"],
      "additionalProperties": false
    }
  }]
}
```

Nonempty `nextCursor` means the input is incomplete and is rejected. Collect all
pages first and remove the cursor (an empty string is also accepted). A complete
empty `tools` array generates an empty `tool` interface with no dummy functions.
Tools requiring task execution (`execution.taskSupport: "required"`) are rejected;
`"optional"` and `"forbidden"` permit ordinary `tools/call`.

All supported tool metadata is retained, not executed. Metadata fields are
shape-checked; unknown catalog/tool fields are rejected rather than guessed at.

## WIT contract

Every generated package is resolved by `wit-parser` before it is returned. The
`client` world exports one `tool` interface with synchronous functions:

```wit
greet: func(arguments: greet-arguments) -> result<result-envelope-json, error>;
```

A closed empty input object becomes a no-argument function. Other inputs are one
typed argument record or map, not separate positional JSON properties.
Closed empty objects used as values (including nested fields and typed outputs)
use a singleton enum with the case `empty`, representing exactly `{}` on the
wire. Component Model records cannot have zero fields. This representation does
not turn the JSON object into a string enum; codecs must still encode/decode `{}`.

When **`outputSchema` is absent**, the success value is
`type result-envelope-json = string`: the entire standard MCP `CallToolResult`
JSON envelope, not just its text content. This is an explicit API contract, not
a fallback for unsupported schemas.

When **`outputSchema` is present**, the success value is a generated result
record with `structured-content` (the typed schema) and `envelope-json` (the same
complete result envelope). This preserves content blocks and metadata. Declaring
an unsupported output schema is an error, even if a tool also returns text.
Both MCP input and output schemas must declare `type: "object"`.

The shared `error` variant distinguishes `transport(string)`, `protocol(string)`,
`tool-execution(result-envelope-json)` and `result-decoding(string)`. These are
interface definitions only; the runtime and codecs belong to subsequent layers.
Future codecs must check `isError` before attempting typed success decoding.

Names are deterministic lowercase ASCII kebab-case. Tools, properties, `$defs`
and enum cases are sorted by their original wire names. Exact duplicate tool
names fail; normalized collisions get `-v2`, `-v3`, etc., with WIT and Rust
binding namespaces checked separately. Support names are reserved. Original tool
names, field keys and enum values remain distinct in the internal model and WIT
documentation. Descriptions are emitted only as safely prefixed doc lines.

Package identities use `namespace:name[@version]` with validated lowercase
kebab-case segments and SemVer versions. A version is recommended. WIT/Rust
keywords in either module segment get `-api`, for example
`example:box@0.1.0` becomes `example:box-api@0.1.0`.

## Supported JSON Schema subset

The default and only accepted explicit dialect is JSON Schema **2020-12**
(`https://json-schema.org/draft/2020-12/schema`, optionally ending in `#`).

| Schema | WIT representation |
| --- | --- |
| `boolean`, `string` | `bool`, `string` aliases |
| `integer` | `s64` by default |
| Integer `format: int8/int16/int32/int64` | `s8/s16/s32/s64` |
| Integer `format: uint8/uint16/uint32/uint64` | `u8/u16/u32/u64` |
| `number` | `f64` by default |
| Number `format: float/float32` or `double/float64` | `f32` or `f64` |
| Array with one supported `items` schema | `list<item>` |
| Nonempty closed object | Record |
| Empty closed object | Singleton enum `{ empty }`, representing JSON `{}` |
| Object with only typed `additionalProperties` | `list<tuple<string, value>>` |
| Properties plus typed `additionalProperties` | Record plus a typed extra-properties list |
| String `enum` | Enum with original JSON values retained |
| One concrete type plus `"null"` | `option<value>` |
| Acyclic local `$ref` into `$defs` | Alias to a shared resolved schema type |

Numeric formats are explicit representation hints. Unknown or mismatched numeric
formats fail. Arbitrary-precision JSON integers/numbers cannot be represented;
future codecs must reject out-of-range integer conversions, non-finite floats
and floating-point overflow. Schema `minimum`/`maximum` do not silently select a
different representation or substitute for conversion checks.

`required` must name explicitly declared `properties`; otherwise parsing fails
at that array entry, even with typed extras. A required nullable field is
`option<T>` (null or value). An optional non-nullable field is also `option<T>`
(missing or value). An **optional nullable** field is `option<option<T>>`,
preserving all three states: missing, present null, present value. Nullable string
enums admit null only when both the type and enum permit it. Defaults do not
change requiredness or fill missing values.

An object is not closed unless `additionalProperties` is explicitly `false`.
The JSON Schema default is `true`, which would allow untyped values: **both
omission and `true` are rejected**, never silently narrowed. An empty schema
`{}` for additional properties is also unsupported. Typed extras are flattened
into the JSON object, not sent under the synthetic WIT field name. Future codecs
must reject duplicate map keys and extra keys matching declared properties.
The synthetic field is collision-safe even when a real JSON property is named
`additional-properties`.

Local references use JSON Pointer with `~0`/`~1` escaping and UTF-8 URI-fragment
percent decoding. References may traverse schema locations to nested `$defs`;
they cannot target examples/default data or another input/output schema.
Definitions are audited even when unused. Cycles and unresolved/external
references are errors. Structural and validation siblings of `$ref` are rejected;
annotation siblings are retained. Compilation is limited to 128 active schema
levels and 10,000 schema types per catalog.

### Constraints are not a complete validator

The parser validates the supported **structure** and keyword value shapes, not
all JSON Schema semantics or instance constraints. The following keywords are
retained and documented but not encoded as WIT refinements:

- Numeric `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`.
- String `minLength`, `maxLength`, `pattern`, and string `format`.
- Array `minItems`, `maxItems`, `uniqueItems`.
- Object `minProperties`, `maxProperties`.

Bounds consistency, regex syntax/matching, format validation, and example/default
validity are not checked. Constraints must occur on an applicable supported type.
`title`, `description`, `$comment`, `default`, `examples`, `deprecated`, `readOnly`
and `writeOnly` are retained as annotations; they do not drop fields, synthesize
defaults, or alter input/output shape.

This layer does not encode or decode instances. Future generated codecs must
enforce required fields, nullability, JSON wire types, enum membership, closed
objects, map key uniqueness, and numeric conversion bounds. WIT alone does not
provide those JSON-level checks.

### Rejected structures

Boolean/unconstrained schemas, null-only schemas, non-null unions, non-string
enums, `const`, composition (`allOf`, `oneOf`, `anyOf`, `not`), conditionals,
tuple/prefix arrays, `contains`, pattern/dependent/unevaluated properties,
`propertyNames`, schema IDs/anchors/dynamic references, other dialects, and
unknown schema keywords are rejected. This allowlist deliberately rejects
unrecognized structural features instead of silently weakening the schema.

Errors contain public `tool`, `pointer`, and `message` fields. Schema pointers
are RFC 6901 locations in the original catalog, for example:

```text
tool "greet" at /tools/0/inputSchema/additionalProperties:
untyped open objects are unsupported; set additionalProperties to false or a supported schema
```

Malformed JSON cannot reliably identify a tool or pointer and reports a
catalog-level error instead.

## Fixtures and scope

[`tests/fixtures/tools.json`](tests/fixtures/tools.json) is a complete working
catalog with typed output, references, nullable fields, maps and mixed objects.
The adjacent open-object and recursive fixtures demonstrate explicit failures.
Tests resolve and binary-validate generated Component Model types, and check
missing/null states, wire mappings, determinism, collisions, malformed inputs
and keyword rejection.

```sh
cargo test -p autostamp-mcp
cargo build -p autostamp-mcp --lib --target wasm32-wasip2
```

The current library works natively and for WASI 0.2 without HTTP/CLI dependencies.
Streamable HTTP transport, discovery, Rust/component generation and runtime
credentials are not implemented here. Stdio, prompts, resources, OAuth and task
execution are outside the planned tools-only client scope.
