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
