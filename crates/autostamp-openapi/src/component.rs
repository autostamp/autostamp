//! Component bindings.
//!
//! The wit-bindgen-generated component glue requires `unsafe` (the component-model ABI
//! trampolines and `cabi_realloc`), which is denied everywhere else in this crate. We
//! isolate it — together with the `Guest` implementation that delegates to the safe API
//! above — behind a single `#[allow(unsafe_code)]` on this private module. The module is
//! gated on `wasm32` because the component exports only link as a Wasm component; on other
//! targets the crate is a plain Rust library.

wit_bindgen::generate!({
    world: "bindgen",
    path: "wit",
});

use self::exports::openapi_bindgen::generator::generator as wit;

/// The component entry point implementing the exported `generator` interface.
struct Component;

impl From<wit::PackageName> for crate::PackageName {
    fn from(value: wit::PackageName) -> Self {
        crate::PackageName {
            namespace: value.namespace,
            name: value.name,
            version: value.version,
        }
    }
}

impl From<crate::PackageName> for wit::PackageName {
    fn from(value: crate::PackageName) -> Self {
        wit::PackageName {
            namespace: value.namespace,
            name: value.name,
            version: value.version,
        }
    }
}

impl From<crate::Generated> for wit::Generated {
    fn from(value: crate::Generated) -> Self {
        wit::Generated {
            wit: value.wit,
            rust: value.rust.into_iter().map(Into::into).collect(),
            cargo_toml: value.cargo_toml,
            wasm_toml: value.wasm_toml,
            readme: value.readme,
            interfaces: value.interfaces,
        }
    }
}

impl From<crate::RustSource> for wit::RustSource {
    fn from(value: crate::RustSource) -> Self {
        wit::RustSource {
            path: value.path,
            contents: value.contents,
        }
    }
}

impl wit::Guest for Component {
    fn parse_package(raw: String) -> Result<wit::PackageName, wit::Error> {
        crate::PackageName::parse(&raw)
            .map(Into::into)
            .map_err(|err| wit::Error::InvalidPackage(format!("{err:#}")))
    }

    fn generate(
        spec_json: String,
        target_package: wit::PackageName,
        tags: Option<Vec<String>>,
    ) -> Result<wit::Generated, wit::Error> {
        let spec = crate::parse_openapi(&spec_json)
            .map_err(|err| wit::Error::InvalidDocument(format!("{err:#}")))?;
        let package = crate::PackageName::from(target_package);
        crate::generate(&spec, &package, tags.as_deref())
            .map(Into::into)
            .map_err(|err| wit::Error::GenerationFailed(format!("{err:#}")))
    }

    fn rewrite_world_exports(src: String, interfaces: Vec<String>) -> String {
        crate::rewrite_world_exports(&src, &interfaces)
    }
}

export!(Component);
