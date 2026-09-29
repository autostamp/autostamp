//! Convert complete MCP `tools/list` results to typed WIT and client components.
//!
//! Generation is pure and offline: it neither discovers nor invokes tools.
//! Unsupported schema shapes fail with the tool name and a JSON Pointer into the
//! catalog. The supported structural subset and constraints not encoded in WIT
//! are described in the crate's README.
//!
//! ```
//! use autostamp_mcp::{PackageName, generate_wit, parse_catalog};
//!
//! let catalog = parse_catalog(r#"{"tools":[{
//!   "name":"ping",
//!   "inputSchema":{"type":"object","additionalProperties":false}
//! }]}"#)?;
//! let package = PackageName::parse("example:service@0.1.0")?;
//! let wit = generate_wit(&catalog, &package)?;
//! assert!(wit.contains("ping: func()"));
//! # Ok::<(), autostamp_mcp::Error>(())
//! ```

mod catalog;
mod error;
mod generate;
mod model;
mod naming;
mod package_name;
mod rust_codegen;
mod schema;
mod wit;

/// Bounded MCP transport and session primitives shared by generated clients.
pub mod runtime;

/// Safe, ownership-tracked publication of standalone generated crates.
#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod artifact_writer;

/// Bounded, tools-only discovery using the official Rust MCP SDK.
#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod discovery;

/// Native streaming HTTP transport for discovery.
#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod native;

#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
mod sdk_http;

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component;

pub use catalog::{Catalog, from_json_value, parse_catalog};
pub use error::Error;
pub use generate::{GenerateOptions, Generated, RustSource, WitDependency, generate};
pub use package_name::PackageName;
pub use wit::generate_wit;
