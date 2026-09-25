//! Discover MCP tools or read a complete catalog and generate a standalone client.

#[cfg(not(target_arch = "wasm32"))]
#[path = "../cli.rs"]
mod cli;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    cli::main()
}

#[cfg(target_arch = "wasm32")]
fn main() {
    eprintln!("autostamp-mcp is a native-only CLI; build --lib for the WASI generator");
    std::process::exit(1);
}
