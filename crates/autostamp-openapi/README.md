# autostamp-openapi

Convert OpenAPI 2 (Swagger) and OpenAPI 3 documents into WebAssembly component
source crates: typed WIT interfaces, Rust bindings, manifests, and documentation.

This crate was previously named `openapi-bindgen`. Its component interface remains
`openapi-bindgen:generator@0.1.0`; its Rust import is now `autostamp_openapi`.

From the repository root:

```sh
cargo run -p autostamp-openapi --example run -- \
  path/to/openapi.json components/my-api autostamp:my-api@0.1.0
cargo test -p autostamp-openapi
cargo build -p autostamp-openapi --lib --target wasm32-wasip2
```

See the repository's [building guide](../../docs/building-and-publishing.md),
[authentication guide](../../docs/auth.md), and
[known limitations](../../docs/known-limitations.md).
