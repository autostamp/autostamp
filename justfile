# List available recipes.
default:
    @just --list

# Bootstrap a fresh checkout: vendored schemas + crate dependencies.
init: vendor-init
    cargo fetch

# Build the workspace.
build:
    cargo build --all

# Type-check the workspace, including bins and examples.
check:
    cargo check --all --bins --examples

# Run the test suite.
test:
    cargo test --all

# Format the source tree.
fmt:
    cargo fmt --all

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check

# Lint with clippy, denying warnings.
clippy:
    cargo clippy --all --bins --examples -- -D warnings

# Build the documentation.
doc *args:
    cargo doc {{ args }}

# Run a binary in the workspace.
run *args:
    cargo run {{ args }}

# Run the xtask helper.
xtask *args:
    cargo xtask {{ args }}

# Remove build artifacts.
clean:
    cargo clean

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

# Show the vendored schemas pin and active sparse-checkout paths.
vendor-status:
    git submodule status vendor/schemas
    git -C vendor/schemas sparse-checkout list

# Remove the vendored schemas working tree to reclaim disk (re-fetch with `just vendor-init`).
vendor-deinit:
    git submodule deinit -f vendor/schemas

# Format, lint, and test — run before pushing.
all: fmt-check clippy test
