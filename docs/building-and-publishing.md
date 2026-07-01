# Building and publishing

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
memory while compiling — see [Known limitations](../README.md#known-limitations).

**GHCR auth.** Publishing uses Docker credentials. Set `GHCR_TOKEN` (or `GH_TOKEN_CLASSIC`)
to a **classic** PAT with `write:packages`, or run `docker login ghcr.io` first. CI uses a
`GHCR_PAT` secret.

## `component` CLI requirements and gaps

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
