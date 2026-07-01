<h1 align="center">autostamp-openapi</h1>
<div align="center">
  <strong>
    Convert OpenAPI schema definitions to WebAssembly Components
  </strong>
</div>

<br />

## About

[WebAssembly Components][component] are universal, portable libraries which can linked to
any other language. This project exists to automate library creation by
taking OpenAPI schema definitions and creating WebAssembly guest components from
them.

The goal of this project is to _automate_ Component-based SDK creation as much
as possible. Hand-crafted components can probably be nicer than what we can
achieve with automation here. But those SDKs currently don't exist, and we believe that it's better to have something that works than nothing at all.

## Usage

Generate component crates for the curated provider set from the vendored schemas:

```sh
just gen
```

This writes one buildable crate per provider into `components/<name>/`:

```text
components/nasa/
  Cargo.toml      # clean SemVer (0.1.0); builds a cdylib for wasm32-wasip2
  wasm.toml       # [package] publish metadata + explicit WIT interface deps
  README.md       # generated usage + diagnostics
  src/lib.rs      # generated Rust (wit-bindgen guest)
  wit/world.wit   # generated WIT world (deps resolved into wit/deps/ at build time)
```

To generate a single component directly (what `just gen` calls under the hood):

```sh
cargo run --example run -- <openapi-path> components/<name> autostamp:<name>@0.1.0
```

## Building and publishing

Generated crates are compiled to `wasm32-wasip2` components and published as OCI artifacts to
`ghcr.io/autostamp/<name>` with the
[`component` CLI](https://github.com/yoshuawuyts/component-registry), via two `just` recipes:
`just build` (regenerates the crates, then compiles the generator and each component) and
`just publish-components`. See the
[building and publishing guide](./docs/building-and-publishing.md) for the full flow, versioning,
build profile, GHCR auth, and the `component` CLI requirements and gaps.

## Known limitations

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

## Documentation

- [Building and publishing](./docs/building-and-publishing.md)
- [Authentication](./docs/auth.md)

## Contributing
Want to join us? Check out our ["Contributing" guide][contributing] and take a
look at some of these issues:

- [Issues labeled "good first issue"][good-first-issue]
- [Issues labeled "help wanted"][help-wanted]

[contributing]: https://github.com/yoshuawuyts/openapi-bindgen/blob/master/.github/CONTRIBUTING.md
[good-first-issue]: https://github.com/yoshuawuyts/openapi-bindgen/labels/good%20first%20issue
[help-wanted]: https://github.com/yoshuawuyts/openapi-bindgen/labels/help%20wanted

## License

<sup>
Licensed under the <a href="LICENSE-APACHE">Apache License, Version 2.0 with the
LLVM exception</a>.
</sup>

<br/>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

[component]: https://component-model.bytecodealliance.org/
