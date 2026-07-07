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
- **Path parameters are typed as `string`.** They exist only to fill `{placeholder}` segments in
  the URL, so they're always interpolated as strings regardless of their declared schema.
- **Cookie parameters are dropped** (with a diagnostic on stderr). They aren't represented in the
  request runtime; they're vanishingly rare in practice — no provider in the curated corpus uses
  one.

## Response modeling

Every operation returns `result<ok, err>`. The **success** arm is typed from the primary 2xx
response body: the generator picks a response (preferring `200`, then `201`, then the lowest 2xx
code, then a `2XX` range), selects its JSON media type, and lowers that schema exactly like a
request body — an object becomes a named record, an array a `list<...>`, and so on. Response
bodies shared via `#/components/responses/*` `$ref`s are resolved first, so a referenced response
is typed just like an inline one. The **error** arm enumerates the declared non-2xx responses into
a per-operation `variant`: each specific status (`404` → `not-found`, `500` →
`internal-server-error`) and each status range (`4XX` → `client-error`, `5XX` → `server-error`)
becomes a named case, plus a trailing `other(string)` catch-all for undeclared statuses and
transport-level failures. The runtime returns the response status and raw body to the generated
per-operation wrapper, which deserializes the success body into its typed value or routes the
failure onto the matching error case.

The following mappings are **deliberate simplifications**:

- **Success bodies with no typeable schema stay `string`.** A response with no content or no
  schema (`204 No Content`, a bare `description`), or a `oneOf`/`anyOf` body, keeps the raw
  response body as a `string` on the `ok` arm — an unmodelable response never regresses an
  operation to un-generatable.
- **Error case payloads are always the raw body `string`.** Each error variant case carries the
  response body verbatim rather than a typed error schema; typed error bodies are a future
  refinement.
- **Operations with no declared error responses keep `result<ok, string>`.** When a spec declares
  no specific non-2xx response (only a 2xx, or a bare `default`), the error arm stays a flat
  `string` — a single-case variant would carry no more information than the string it wraps.
