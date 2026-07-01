# Building and publishing

Generated crates are compiled and published as OCI artifacts to `ghcr.io/autostamp/<name>`
with the [`component` CLI](https://github.com/yoshuawuyts/component-registry). The flow is
two `just` recipes — the same ones CI runs:

```sh
just build               # 1. regenerate components/<name>/, then compile the generator + each
                         #    crate to a wasm32-wasip2 component
just publish-components  # 2. push each component to ghcr.io/autostamp
```

To cut a release in one step, `just publish <level>` bumps the shared version, then runs both
recipes above (regenerate → build → publish) at the new version. A level is required — a bare
`just publish` fails and asks you to choose, so a release is always a deliberate bump:

```sh
just publish minor       # bump version.toml minor, then regenerate + build + publish all
just publish patch       # patch release
just publish major       # major release
just publish minor 1     # preview: bump + build, but pass dry_run to the publish step
```

`build` first regenerates the component crates from the vendored schemas (the `gen` recipe runs
as a dependency), then compiles the generator itself to a component, then builds each generated
crate: it resolves the WIT interface dependency (`wasmcloud:secrets`) with
`component install`, bridges the vendored WIT into `wit/deps/`, and builds for `wasm32-wasip2`
with a shared target directory so the common crates compile once. (The component also imports
`wasi:http` — and `wasi:cli`/`clocks`/`io`/`random` — but the `wstd` crate contributes those
bindings whole, so they are not declared in `wasm.toml`.) Pass a provider name to
regenerate and build a single component (`just build nasa`); pass `dry_run=1` to
`publish-components` to preview a publish without pushing (`just publish-components nasa 1`). Run
`just gen` on its own to regenerate without compiling.

**Versioning.** Each component's published version is the base SemVer plus build metadata made
of the provider name and the OpenAPI document's `info.version` — e.g. `0.1.0+vonage-1.11.8`.
The base version stays on the WIT package decl and `Cargo.toml`; only `wasm.toml`'s
`[package].version` carries the metadata, recording which provider and exactly which schema
revision the bindings came from.

All components share one base SemVer, kept in `version.toml` at the repo root; `gen` reads it
once and stamps the same base onto every component (defaulting to `0.1.0` when the file is
absent). Bump it for all components at once with the `bump` recipe, then regenerate:

```sh
just bump           # patch bump in version.toml: 0.1.0 -> 0.1.1
just bump minor     # 0.1.1 -> 0.2.0
just bump 1.0.0     # set explicitly
just gen            # regenerate every component with the new base
```

Because build metadata is ignored for SemVer precedence, bumping the base is the only way to
publish a version consumers can actually resolve to — the `+provider-schemaversion` metadata is
provenance only.

**Build profile.** Generated crates ship a `[profile.release]` tuned for WebAssembly:
`opt-level = "s"` keeps the `.wasm` small, and `codegen-units = 256` splits code generation
into many small LLVM units so the backend parallelizes and its peak memory stays bounded.
Large APIs produce large crates, and the biggest ones can still exhaust a small machine's
memory while compiling — see [Known limitations](./known-limitations.md).

**GHCR auth.** Publishing uses Docker credentials. Set `GHCR_TOKEN` (or `GH_TOKEN_CLASSIC`)
to a **classic** PAT with `write:packages`, or run `docker login ghcr.io` first. CI uses a
`GHCR_PAT` secret.

## `component` CLI requirements and gaps

- **OCI-tag build metadata (required).** `component publish` maps a `+` in the version onto
  `_` in the OCI tag (`0.1.0+vonage-1.11.8` → tag `0.1.0_vonage-1.11.8`), keeping the full
  SemVer in the `org.opencontainers.image.version` annotation — the same convention Helm uses.
  Install a `component` build that includes this fix.
- **`wit/deps` bridge.** `component install` vendors WIT to `vendor/wit/`, but wit-bindgen
  reads `wit/deps/`; `build` copies between them. A native `wit/deps` output
  would remove the step.
- **Keyword package names (wit-bindgen, not `component`).** wit-bindgen names a package's
  Rust module `name.to_snake_case()` *without* keyword-escaping it
  (`wit_bindgen_core::name_package_module`), so a package literally named `box` emits an
  uncompilable `pub mod box { … }`. The generator works around this by renaming packages whose
  name is a Rust keyword — `box` becomes `box-api`, published as `ghcr.io/autostamp/box-api` —
  keeping the full name in the WIT `package` decl. A wit-bindgen-side escape (e.g. `box_`)
  would remove the need for the rename.
- **Offline dependency resolution.** Short manifest keys
  (`"wasmcloud:secrets@1.0.0" = "1.0.0"`) need a running meta-registry; the generator emits
  explicit `{ registry, namespace, package, version }` tables so `component install` resolves
  straight from GHCR.
- **Ergonomics (nice-to-have).** No `[package]` scaffolding, no `component build` (we shell
  out to cargo), and no batch/workspace publish (we loop in `just`).
