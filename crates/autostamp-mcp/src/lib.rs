//! Convert complete MCP `tools/list` results to typed WIT.
//!
//! This is a pure, offline library: it neither discovers nor invokes tools.
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
mod model;
mod naming;
mod package_name;
mod schema;
mod wit;

pub use catalog::{Catalog, from_json_value, parse_catalog};
pub use error::Error;
pub use package_name::PackageName;
pub use wit::generate_wit;
