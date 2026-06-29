# List available recipes.
default:
    @just --list

# Bootstrap a fresh checkout: vendored schemas + crate dependencies.
init: vendor-init
    cargo fetch

# Build the workspace.
build:
    cargo build --all

run:
    cargo run --example run -- vendor/schemas/APIs/svix.com/1.4/openapi.yaml components/svix wilted:svix@0.1.0

# Build the Wasm component (wasm32-wasip2 reactor exporting `openapi-bindgen:generator`).
component:
    cargo build -p openapi-bindgen --target wasm32-wasip2 --release
    @echo "component: target/wasm32-wasip2/release/openapi_bindgen.wasm"

# Format-check, lint, and run the test suite.
test:
    cargo fmt --all -- --check
    cargo clippy --all --bins --examples -- -D warnings
    cargo test --all

# Fetch the vendored OpenAPI schemas submodule (sparse: only APIs/, shallow + blobless).
vendor-init:
    git submodule update --init --filter=blob:none vendor/schemas
    git -C vendor/schemas sparse-checkout init --cone
    git -C vendor/schemas sparse-checkout set APIs

# Update the vendored schemas to the latest upstream commit (re-pin to origin/main).
vendor-update:
    git -C vendor/schemas fetch --depth 1 origin main
    git -C vendor/schemas checkout -B main FETCH_HEAD
    @echo "vendor/schemas now at $(git -C vendor/schemas rev-parse --short HEAD) — run 'git add vendor/schemas' to stage the new pin."
