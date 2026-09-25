# MCP client components

`autostamp-mcp` turns a complete MCP tool catalog into typed WIT and a standalone
Rust client component. The native CLI can discover a live server or read an
offline catalog. Generation itself is pure and offline; the generated client
uses a **static schema snapshot**, not dynamic tool discovery.

The scope is **MCP 2025-11-25 tools over Streamable HTTP**, with JSON and SSE
responses. Other protocol versions, stdio, legacy HTTP+SSE transport, prompts,
resources, OAuth flows and task execution are out of scope. Discovery never
sends `tools/call`: it initializes a session, lists every tools page and closes
the session. Tool execution happens only when a generated export is invoked.

## Generate from a live server

```sh
cargo run -p autostamp-mcp --features cli -- \
  --server https://mcp.example.com/mcp \
  --package example:service@0.1.0 \
  --output target/generated-service
```

The native CLI requires the opt-in `cli` feature. Its command shape is:

```text
cargo run -p autostamp-mcp --features cli -- \
  (--server URL | --catalog PATH --endpoint URL) \
  --package namespace:name@version --output PATH [OPTIONS]
```

Choose exactly one of `--server` and `--catalog`. In live mode, the server URL
is also the generated runtime endpoint by default. An optional `--endpoint URL`
overrides **only the generated endpoint**; discovery still contacts `--server`.
This lets a build-time discovery address differ from the deployed address.
URLs must be absolute HTTP(S) endpoints without embedded credentials.

The CLI follows opaque pagination cursors until `nextCursor` is absent.
Repeated cursors, incomplete catalogs, malformed responses and unsupported
schemas fail rather than producing partial bindings. A complete empty catalog
is valid to discover, parse and generate: it produces a client with no tool
exports, without inventing dummy functions.

## Generate from an offline catalog

```sh
cargo run -p autostamp-mcp --features cli -- \
  --catalog crates/autostamp-mcp/tests/fixtures/tools.json \
  --endpoint https://mcp.example.com/mcp \
  --package example:service@0.1.0 \
  --output "target/generated service"
```

Offline mode requires `--endpoint` and performs no discovery requests.
The catalog is a complete `tools/list` **result object**, not a JSON-RPC envelope:

```json
{
  "tools": [{
    "name": "greet",
    "inputSchema": {
      "type": "object",
      "properties": { "name": { "type": "string" } },
      "required": ["name"],
      "additionalProperties": false
    }
  }]
}
```

Collect every page before providing a snapshot; omit `nextCursor` entirely.
Even `nextCursor: ""` means another page. Catalog bytes are bounded while
reading the file, not just after loading it into memory. Keep snapshots under
your own source control if repeatable generation is needed; the CLI has no
catalog-saving flag.

See the [crate reference](../crates/autostamp-mcp/README.md) for the supported
JSON Schema subset, typed result envelopes and diagnostic locations. Unsupported
schemas are errors, not silently stringified values. Constraints such as
`pattern` and `minimum` are retained, but the generated codecs are not a full
JSON Schema validator.

## Discovery credentials and runtime credentials

These are separate settings:

| Flag | Purpose |
| --- | --- |
| `--token-env ENV_VAR` | Live discovery only: read a bearer token from this environment variable in the native CLI process. Rejected with `--catalog`. |
| `--bearer-secret KEY` | Generate a client that looks up this secret **key** through `wasmcloud:secrets@1.0.0` at runtime. Accepted in either mode. |

For example, after provisioning `MCP_DISCOVERY_TOKEN` in your environment:

```sh
cargo run -p autostamp-mcp --features cli -- \
  --server https://mcp.example.com/mcp \
  --token-env MCP_DISCOVERY_TOKEN \
  --bearer-secret service-bearer \
  --package example:service@0.1.0 \
  --output target/generated-service
```

The discovery token is not the runtime secret key, is not embedded in generated
source, and does not provision the runtime host. Provision `service-bearer`
separately with the runtime credential. No token value is accepted as a CLI
flag. Without `--bearer-secret`, generated clients are anonymous and import no
secrets interfaces. Authenticated clients include the required secret WIT
dependencies and resolve string or UTF-8 byte credentials at call time.
See [Authentication](./auth.md#mcp-credentials) for the host contract.

## Limits and validation

Request limits apply to discovery HTTP exchanges and are embedded in the
generated client for runtime calls:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--timeout-ms` | `30000` | Total deadline per HTTP exchange, not an idle timeout; progress does not reset it. |
| `--max-response-bytes` | `4194304` (4 MiB) | Maximum response bytes consumed before the matching reply. |
| `--max-event-bytes` | `1048576` (1 MiB) | Maximum SSE event and physical-line bytes. |

These match `runtime::Limits::default()`. Catalog limits additionally bound
discovery pagination and live or offline input:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--max-pages` | `100` | Maximum tools-list pages. |
| `--max-tools` | `10000` | Maximum tool count, live or offline. |
| `--max-catalog-bytes` | `16777216` (16 MiB) | Maximum serialized discovery catalog bytes or offline input file bytes. |

All limits must be positive. Event bytes cannot exceed response bytes; the
timeout must fit WASI nanoseconds (`u64::MAX / 1_000_000` milliseconds), and
generated response/event byte limits must fit the wasm32 address space
(`u32::MAX`). Invalid mode combinations, package names, endpoints, secret keys
and limits are rejected before network access or output writes. The complete
catalog must pass parsing and generation before files are emitted.

## Output ownership and regeneration

`--output` names a standalone Cargo crate, containing `Cargo.toml`, `wasm.toml`,
`README.md`, Rust sources, WIT and any required WIT dependencies. Generated
crates do not depend on either autostamp generator and include their own empty
`[workspace]`.

The writer keeps an ownership ledger at `.autostamp-files.json`:

- Unrelated files are preserved; an existing unowned file is not overwritten
  just because its path is needed by the new generation.
- Previously generated files must still match their recorded content. Editing
  an owned file makes regeneration fail, **even if that file is now stale**.
- Only unchanged stale owned files are removed. For example, switching from
  authenticated to anonymous generation removes the previously owned secret
  WIT dependencies without deleting unrelated files.
- The ledger also tracks WIT dependency package directories created by the
  generator. Stale directories are pruned only when empty, so removed secret
  packages do not leave empty directories that break WIT dependency resolution.
  Pre-existing user directories and directories containing user files are kept.

Keep hand-written extensions in separate, unowned files or a consuming crate.
Back up and reconcile edits before regenerating; deleting the ledger is not a
force-overwrite mechanism. The writer assumes **exclusive filesystem access**
to the output during generation and a filesystem with hard-link support. It
does not promise crash-atomic multi-file updates or safety against adversarial
filesystem races.

Regenerate deliberately when the server's tool schemas change, review the
resulting API diff, and rebuild. A running generated client does not update
its schemas automatically.

## Build and test

```sh
# Forward CLI arguments without losing spaces in paths.
just gen-mcp --catalog crates/autostamp-mcp/tests/fixtures/tools.json \
  --endpoint https://mcp.example.com/mcp \
  --package example:service@0.1.0 --output "target/generated service"

# Build that standalone client.
cargo build --manifest-path "target/generated service/Cargo.toml" \
  --release --target wasm32-wasip2

# Build the offline MCP generator component itself, without native CLI features.
just build-mcp

# MCP native tests plus both mock-only integration harnesses.
just test-mcp
```

The Rust integration tests require Cargo with `wasm32-wasip2` and the following
tools on `PATH` (these exact versions are used in CI):

```sh
rustup target add wasm32-wasip2
cargo install --locked --version 1.240.0 wasm-tools
cargo install --locked --version 0.10.0 wac-cli
# Install Wasmtime 45.0.1, then verify the selected binaries:
wasm-tools --version
wac --version
wasmtime --version
```

If Wasmtime is not on `PATH`, set `WASMTIME` to its executable path. Run the
harnesses directly with:

```sh
cargo test -p autostamp-mcp --all-features \
  --test cli_e2e --test component_runtime -- --ignored --nocapture
```

These tool-dependent tests are ignored by the ordinary native test suite and
explicitly enabled in the component CI job. They use loopback-only mock servers
and test credentials, not real MCP services. Subprocesses have 240-second
deadlines; test teardown stops and joins the fixture server and its workers.
Fixtures and generated test artifacts stay outside `components/`. CI also
builds **both** generator libraries for `wasm32-wasip2` with native features
disabled; native workspace checks and tests retain `--all-features`.

The workspace still contains exactly `autostamp-openapi` and `autostamp-mcp`.
The existing `just gen`, `just build`, versioning and publishing recipes remain
the OpenAPI corpus workflow; MCP does not add providers to that corpus or change
scheduled publishing. See [Building and publishing](./building-and-publishing.md).
