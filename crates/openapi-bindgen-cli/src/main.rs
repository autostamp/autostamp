//! CLI for openapi-bindgen.

use clap::Parser;

/// Convert OpenAPI schema definitions to WebAssembly Components
#[derive(Debug, Parser)]
#[command(name = "openapi-bindgen", version, about)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
