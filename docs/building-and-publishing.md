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

## Automated regeneration & publishing (CI)

The `.github/workflows/regen-and-publish.yaml` workflow runs the whole regenerate → build →
publish loop on a schedule (weekly, Mondays 06:00 UTC) and on demand (`workflow_dispatch`).
Each run:

1. **regen** — fetches the vendored schemas and, on scheduled runs, re-pins them to the
   latest upstream with `just vendor-update`; bumps the shared base version (`just bump`,
   default `minor`); regenerates every component (`just gen`); commits `components/`,
   `version.toml`, and the schema pin to a fresh `auto/regen-v<version>-<timestamp>` branch;
   and opens an **intermediate PR** with the diff.
2. **await-ci** — waits for that PR's checks to go green (and fails the pipeline if they
   don't).
3. **publish** — checks out the branch, runs `just build` then `just publish-components`
   (GHCR auth via `GHCR_PAT`).
4. **merge** — squash-merges the PR so `main` stays in sync with what was published.

**Why the PR is closed and reopened.** A pull request opened with the built-in `GITHUB_TOKEN`
does *not* trigger `on: pull_request` workflow runs — GitHub suppresses downstream runs from
the built-in token to prevent loops — so CI would never run on the bot's PR. The workflow
therefore opens the PR with `GITHUB_TOKEN` and then **closes and reopens it with a PAT**
(`REGEN_PAT`); the `reopened` activity, performed as a real user, triggers `ci.yaml`.

**What actually gates the publish.** `ci.yaml` only builds and tests the *generator* crate —
the repo root is a single crate, not a workspace, so the generated component crates are never
compiled there. The substantive component gate is the pipeline's own `just build` step (a
per-component `wasm32-wasip2` compile); the reopened-PR CI provides the visible green check
and the generator's cross-platform validation.

**Manual dispatch inputs.** `bump_level` (`patch`/`minor`/`major`/`X.Y.Z`, default `minor`),
`vendor_update` (default `true`), and `dry_run` (default `false`). A dry run still opens the
PR and runs CI, but passes `--dry-run` to the publish step and skips the merge.

**Prerequisites.**

- **`REGEN_PAT` secret** — a **classic** PAT with the `repo` scope (add `workflow` if the
  regen ever needs to touch `.github/`). Used for the close/reopen and the final merge. This
  is separate from `GHCR_PAT` (which stays `write:packages` for the publish step).
- Enable **Settings → Actions → General → "Allow GitHub Actions to create and approve pull
  requests"**, or the `GITHUB_TOKEN`-created PR is rejected.
- `main` branch protection must let the `REGEN_PAT` owner squash-merge (or exempt them), or
  the merge step fails and the PR is left open for a human.

**Notes.** Because the base version is bumped every run and `gen` stamps it into every
component, **each run republishes all components** — change the "always bump" behavior if you
only want to publish when the bindings actually changed. The workflow deliberately does *not*
push a `v*` tag (that would double-trigger `publish-components.yaml`, which remains for manual
tag-driven releases).

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
