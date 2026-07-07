# Known limitations

The generator accepts OpenAPI 2 (Swagger) and 3.0 documents and normalizes a range of
real-world quirks — missing `operationId`s, untagged operations, deep `$ref`s into a component's
`properties`/`items`, whole-document `$ref`s into `#/paths/...`, out-of-range numeric bounds, and
stray control characters. A document with no operations (e.g. an empty `paths: {}`) has nothing to
bind and is reported as a skip. A few input shapes are still unsupported and will fail to generate:

- **OpenAPI 3.1 documents.** The underlying `openapiv3` parser targets 3.0; 3.1-only constructs
  (type arrays, `const`, sibling `$ref`s) aren't deserialized yet.
- **Malformed source documents.** Specs whose YAML is structurally invalid for a strict parser
  (e.g. inconsistent block-scalar indentation) can't be loaded.
- **Very large specs are memory-bound to compile.** The generated code is correct, but a spec
  with hundreds of operations produces a very large single crate, and `rustc` can exhaust the
  memory of a small machine (~16 GB) while compiling it, regardless of `opt-level` or
  `codegen-units`. This is a compiler-memory ceiling, not a codegen defect: the crate
  type-checks; the build is killed (OOM) deep in code generation. DocuSign's API (~400
  operations → a ~90k-line `lib.rs`) hits this, and is excluded from the curated provider set
  for that reason. To bind a spec this large, build on a host with more RAM (a 32 GB+ CI
  runner) or reduce the surface with a trimmed input spec.

## Request modeling

Every request input a spec declares is carried onto the wire. Operation parameters are resolved
through `#/components/parameters/*` `$ref`s and merged with path-item-level `parameters` (which
apply to every method of a path), with operation-level entries overriding by `(name, in)`. Request
bodies are inlined regardless of media type — `application/json`, `*+json`,
`application/x-www-form-urlencoded`, `multipart/form-data`, and others — and a body defined by
`allOf` composition is flattened into a merged record rather than degrading to an opaque blob. Each
field travels under its **verbatim** source name (its query key, header name, or body property),
independent of the snake-case identifier used internally, so APIs whose names aren't snake_case
(`dryRun`, `PhoneNumber`) and distinct inputs that share a normalized name (Twilio's `DateCreated`,
`DateCreated<`, `DateCreated>` range filters) are all represented faithfully.

The following mappings are **deliberate simplifications**, not accidental drops:

- **`oneOf` / `anyOf` schemas become an opaque `string`.** A union of alternatives can't be
  represented as a single WIT record without losing fidelity, so the field is passed through as a
  JSON string the caller encodes. (`allOf`, by contrast, *is* merged into a record.)
- **Responses are modeled as `result<string, string>`.** Every operation returns the raw response
  body as a string (or an error message); response schemas are not turned into typed records.
- **Path parameters are typed as `string`.** They exist only to fill `{placeholder}` segments in
  the URL, so they're always interpolated as strings regardless of their declared schema.
- **Cookie parameters are dropped** (with a diagnostic on stderr). They aren't represented in the
  request runtime; they're vanishingly rare in practice — no provider in the curated corpus uses
  one.
