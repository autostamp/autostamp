# autostamp-mcp

Generate typed WIT and standalone Rust client components from a complete MCP
`tools/list` result. Library generation is pure and offline: it does not discover
servers or call tools. The opt-in native `cli` feature adds bounded live discovery
and ownership-safe output. It is independent of `autostamp-openapi`;
the workspace intentionally contains only these two generator crates.

## Native CLI

```sh
cargo run -p autostamp-mcp --features cli -- \
  --server https://mcp.example.com/mcp \
  --package example:service@0.1.0 --output target/generated-service

cargo run -p autostamp-mcp --features cli -- \
  --catalog crates/autostamp-mcp/tests/fixtures/tools.json \
  --endpoint https://mcp.example.com/mcp \
  --package example:service@0.1.0 --output target/generated-service
```

See the [MCP guide](../../docs/mcp.md) for all flags, discovery limits,
authentication, output ownership, standalone builds and mock-only integration
tests. Live `--endpoint` overrides the generated endpoint, not discovery.
`--token-env ENV_VAR` supplies only the live discovery bearer token;
`--bearer-secret KEY` separately selects the generated runtime secret key.
Discovery never calls tools. Generated clients are static snapshots supporting
MCP **2025-11-25 Streamable HTTP** with JSON/SSE responses.

Native discovery uses the official `rmcp` SDK for its client lifecycle and
sessions, with a bounded HTTP adapter that preserves raw tool definitions.
Generated WASI clients retain custom session code because the SDK's HTTP/worker
combination does not integrate with the existing `wstd` reactor, and its typed
results do not preserve the required complete result envelope. See the
[SDK boundary](../../docs/mcp.md#mcp-sdk-boundary) for the evaluated APIs,
compatibility constraints, and the scope of custom code.

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

### Generate a standalone component

```rust
use autostamp_mcp::{GenerateOptions, PackageName, generate, parse_catalog};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let catalog = parse_catalog(&std::fs::read_to_string("tools.json")?)?;
    let package = PackageName::parse("example:service@0.1.0")?;
    let generated = generate(&catalog, &package, &GenerateOptions {
        endpoint: "https://mcp.example.com/mcp".into(),
        bearer_secret: Some("mcp-bearer".into()), // Secret key, not its value.
        ..GenerateOptions::default()
    })?;
    println!("Generated {} Rust files", generated.rust.len());
    Ok(())
}
```

`Generated` contains `wit`, `rust: Vec<RustSource>`, `cargo_toml`, `wasm_toml`,
`readme`, and `wit_dependencies: Vec<WitDependency>`. Write the main WIT to
`wit/world.wit`, Rust files relative to `src/`, and WIT dependencies relative to
`wit/` (their paths already include `deps/`). Runtime sources are included
verbatim; generated clients do not depend on either autostamp crate. Their
dependencies are wit-bindgen 0.41, wstd 0.6, http 1, serde 1 and serde_json 1.
The standalone Cargo manifest includes its own empty `[workspace]`.

The offline example writes those artifacts:

```sh
cargo run -p autostamp-mcp --example generate -- \
  crates/autostamp-mcp/tests/fixtures/tools.json example:service@0.1.0 \
  https://mcp.example.com/mcp target/generated-service
cargo build --manifest-path target/generated-service/Cargo.toml \
  --release --target wasm32-wasip2
```

Append a secret key to the example invocation for bearer authentication. Anonymous
clients import no secrets interfaces and need no secrets provider. Authenticated
clients vendor `wasmcloud:secrets@1.0.0` and load string or UTF-8 byte credentials
through its store/reveal interfaces at call time. Generated `credentials.rs`
keeps these two cases separate. Credential values are never embedded.

The example accepts three final numeric arguments to override the timeout and
response/event byte limits, with or without a preceding secret key. For example,
append `250 2048 1024` for an anonymous test client or
`fixture-key 250 2048 1024` for an authenticated one. `-` is also accepted as an
explicit anonymous key placeholder.

`GenerateOptions::limits` uses `runtime::Limits::default()` unless overridden:
30,000 ms per request, 4 MiB response bodies and 1 MiB SSE events. Invalid
endpoints, embedded URL credentials, empty secret keys and unusable limits fail
before artifact emission.

The generator also builds as an offline component with package
`autostamp:mcp@0.1.0`, world `bindgen`. Its `generator.generate` export accepts
catalog JSON, a target package string, and generation options, returning the
same artifacts or a structured error (`tool`, `pointer`, `message`). Omitting
the WIT options' `limits` selects runtime defaults. Only the private component
bindings module permits unsafe ABI glue.

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

Any present string `nextCursor`, including `""`, means the input is incomplete
and is rejected: cursors are opaque, not truthy/falsey sentinels. Collect every
page until the server omits `nextCursor`, then omit it from the snapshot.
Non-string cursors (including `null`) are malformed. A complete empty `tools`
array is valid to discover, parse and generate: it produces a client with no
tool exports and an empty `tool` interface, without dummy functions.
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
Envelope numbers retain their exact values, including metadata outside Rust's
numeric ranges; the runtime never rounds large integers or underflows tiny
metadata numbers to zero. JSON object order and whitespace may be normalized.

When **`outputSchema` is present**, the success value is a generated result
record with `structured-content` (the typed schema) and `envelope-json` (the same
complete result envelope). This preserves content blocks and metadata. Declaring
an unsupported output schema is an error, even if a tool also returns text.
Both MCP input and output schemas must declare `type: "object"`.

The shared `error` variant distinguishes `transport(string)`, `protocol(string)`,
`tool-execution(result-envelope-json)` and `result-decoding(string)`. These are
implemented by the shared runtime and generated codecs. The runtime checks
`isError` before the generated client attempts typed success decoding.

Names are deterministic lowercase ASCII kebab-case. Tools, properties, `$defs`
and enum cases are sorted by their original wire names. Exact duplicate tool
names fail; normalized collisions get `-v2`, `-v3`, etc., with WIT and Rust
binding namespaces checked separately. Support names are reserved. Original tool
names, field keys and enum values remain distinct in the internal model and WIT
documentation. Descriptions are emitted only as safely prefixed doc lines.
Control, bidi, and Unicode-discouraged codepoints rejected by the WIT parser
are escaped literally in documentation; valid multilingual text is preserved.

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
formats fail. Numeric JSON tokens remain lossless until typed conversion.
Integer schemas accept exact integral decimal/exponent spellings, such as
`-128.0`, `1e2`, and `-0`; conversion never rounds through floating point.
Codecs reject fractional integers, out-of-range integer conversions, non-finite
floats, floating-point overflow, and nonzero numbers that underflow to zero.
Typed numeric results remain bounded by their WIT representations; arbitrary
precision is used to preserve input tokens, not to extend those ranges.
Schema `minimum`/`maximum` do not silently select a
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
into the JSON object, not sent under the synthetic WIT field name. Codecs
reject duplicate map keys and extra keys matching declared properties, including
optional declared properties that are absent.
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

Generated codecs enforce required fields, nullability, JSON wire types, enum
membership, closed objects, map key uniqueness, and numeric conversion bounds.
They use explicit structural conversion, not serde-derived records, so an
optional nullable field never loses its missing/null distinction. References to
nullable schemas retain that distinction too. Every supported schema has an
encoder and decoder, including unused definitions and closed empty objects.
Floating-point overflow, f32/f64 underflow to zero, and non-finite values fail
explicitly. These codecs are not a full JSON Schema constraint validator.

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
cargo test -p autostamp-mcp --all-features
cargo build -p autostamp-mcp --lib --no-default-features --target wasm32-wasip2
cargo test -p autostamp-mcp --lib generated_clients_compile_and_codecs_execute -- --ignored
```

The explicit codegen test compiles anonymous, authenticated and empty generated
clients natively and for wasm32-wasip2. It executes generated-codec tests for all
numeric widths, nullable references, optional/null states, arrays, enums, maps,
mixed objects, closed objects, duplicate keys and extras conflicting with declared
properties. Nested Cargo builds use a separate target directory.

### Real component runtime fixtures

The Rust end-to-end tests require Cargo with the `wasm32-wasip2` target
installed, and `wasm-tools`, `wac`, and `wasmtime` on `PATH` (or `WASMTIME`
pointing to the Wasmtime executable). Exact CI tool versions and the CLI harness
are documented in the [MCP guide](../../docs/mcp.md#build-and-test):

```sh
rustup target add wasm32-wasip2
cargo test -p autostamp-mcp --all-features \
  --test cli_e2e --test component_runtime -- --ignored --nocapture
```

The harness builds anonymous and authenticated fixture components and exercises
their generated exports against a loopback-only MCP mock. It does not contact a
live MCP service. These are fixture builds, not generation or publication of the
component corpus; nothing is published.

The offline example's exact positional contract is:

```text
generate CATALOG_JSON TARGET_PACKAGE ENDPOINT OUTPUT_DIRECTORY [BEARER_SECRET_KEY] [TIMEOUT_MS MAX_RESPONSE_BYTES MAX_EVENT_BYTES]
```

Use `-` for an explicit anonymous credential placeholder. For example,
`fixture:runtime@0.1.0` selects the Cargo crate name `runtime` and produces
`runtime.wasm` when built for `wasm32-wasip2`.

The library and offline generator work natively and for WASI 0.2; native CLI
dependencies are feature-gated and excluded from the component build. Generated
clients use bounded Streamable HTTP; stdio, prompts, resources, OAuth and task
execution are outside this tools-only client scope.
