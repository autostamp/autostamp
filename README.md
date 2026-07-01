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

Generated crates are compiled and published as OCI artifacts to `ghcr.io/autostamp/<name>`
with the [`component` CLI](https://github.com/yoshuawuyts/component-registry). The flow is
three `just` recipes — the same ones CI runs:

```sh
just gen                 # 1. generate components/<name>/ from the vendored schemas
just build-components    # 2. compile each crate to components/<name>/build/<name>.wasm
just publish-components  # 3. push each component to ghcr.io/autostamp
```

`build-components` resolves the WIT interface dependencies (`wasi:http`, `wasmcloud:secrets`)
with `component install`, bridges the vendored WIT into `wit/deps/`, and builds for
`wasm32-wasip2` with a shared target directory so the common crates compile once. Pass a
provider name to act on one component (`just build-components nasa`); pass `dry_run=1` to
preview a publish without pushing (`just publish-components nasa 1`).

**Versioning.** Each component's published version is the base SemVer plus the OpenAPI
document's `info.version` as SemVer build metadata — e.g. `0.1.0+1.0.0`. The base version
stays on the WIT package decl and `Cargo.toml`; only `wasm.toml`'s `[package].version`
carries the metadata, recording exactly which schema revision the bindings came from.

**Build profile.** Generated crates ship a `[profile.release]` tuned for WebAssembly:
`opt-level = "s"` keeps the `.wasm` small, and `codegen-units = 256` splits code generation
into many small LLVM units so the backend parallelizes and its peak memory stays bounded.
Large APIs produce large crates, and the biggest ones can still exhaust a small machine's
memory while compiling — see [Known limitations](#known-limitations).

**GHCR auth.** Publishing uses Docker credentials. Set `GHCR_TOKEN` (or `GH_TOKEN_CLASSIC`)
to a **classic** PAT with `write:packages`, or run `docker login ghcr.io` first. CI uses a
`GHCR_PAT` secret.

### `component` CLI requirements and gaps

- **OCI-tag build metadata (required).** `component publish` maps a `+` in the version onto
  `_` in the OCI tag (`0.1.0+1.0.0` → tag `0.1.0_1.0.0`), keeping the full SemVer in the
  `org.opencontainers.image.version` annotation — the same convention Helm uses. Install a
  `component` build that includes this fix.
- **`wit/deps` bridge.** `component install` vendors WIT to `vendor/wit/`, but wit-bindgen
  reads `wit/deps/`; `build-components` copies between them. A native `wit/deps` output
  would remove the step.
- **Keyword package names (wit-bindgen, not `component`).** wit-bindgen names a package's
  Rust module `name.to_snake_case()` *without* keyword-escaping it
  (`wit_bindgen_core::name_package_module`), so a package literally named `box` emits an
  uncompilable `pub mod box { … }`. The generator works around this by renaming packages whose
  name is a Rust keyword — `box` becomes `box-api`, published as `ghcr.io/autostamp/box-api` —
  keeping the full name in the WIT `package` decl. A wit-bindgen-side escape (e.g. `box_`)
  would remove the need for the rename.
- **Offline dependency resolution.** Short manifest keys (`"wasi:http@0.2.3" = "0.2.3"`)
  need a running meta-registry; the generator emits explicit `{ registry, namespace,
  package, version }` tables so `component install` resolves straight from GHCR.
- **Ergonomics (nice-to-have).** No `[package]` scaffolding, no `component build` (we shell
  out to cargo), and no batch/workspace publish (we loop in `just`).

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
