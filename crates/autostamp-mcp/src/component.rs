//! Unsafe ABI glue is isolated from the pure generator.

wit_bindgen::generate!({
    world: "bindgen",
    path: "wit",
});

use self::exports::autostamp::mcp::generator as wit;

struct Component;

impl From<crate::Error> for wit::Error {
    fn from(error: crate::Error) -> Self {
        Self {
            tool: error.tool,
            pointer: error.pointer,
            message: error.message,
        }
    }
}

impl wit::Guest for Component {
    fn generate(
        catalog_json: String,
        target_package: String,
        options: wit::GenerateOptions,
    ) -> Result<wit::Generated, wit::Error> {
        let catalog = crate::parse_catalog(&catalog_json)?;
        let package = crate::PackageName::parse(&target_package)?;
        let options = crate::GenerateOptions {
            endpoint: options.endpoint,
            bearer_secret: options.bearer_secret,
            limits: options
                .limits
                .map_or_else(crate::runtime::Limits::default, |limits| {
                    crate::runtime::Limits {
                        request_timeout_ms: limits.request_timeout_ms,
                        max_response_bytes: limits.max_response_bytes as usize,
                        max_event_bytes: limits.max_event_bytes as usize,
                    }
                }),
        };
        let generated = crate::generate(&catalog, &package, &options)?;
        Ok(wit::Generated {
            wit: generated.wit,
            rust: generated
                .rust
                .into_iter()
                .map(|source| wit::RustSource {
                    path: source.path,
                    contents: source.contents,
                })
                .collect(),
            cargo_toml: generated.cargo_toml,
            wasm_toml: generated.wasm_toml,
            readme: generated.readme,
            wit_dependencies: generated
                .wit_dependencies
                .into_iter()
                .map(|dependency| wit::WitDependency {
                    path: dependency.path,
                    contents: dependency.contents,
                })
                .collect(),
        })
    }
}

export!(Component);
