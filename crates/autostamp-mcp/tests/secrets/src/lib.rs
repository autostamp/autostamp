wit_bindgen::generate!({
    world: "provider",
    path: "wit",
    generate_all,
});

use exports::wasmcloud::secrets::{reveal, store};

struct Component;
struct TestSecret;

impl store::GuestSecret for TestSecret {}

impl store::Guest for Component {
    type Secret = TestSecret;

    fn get(key: String) -> Result<store::Secret, store::SecretsError> {
        if key == "mcp-test-token" {
            Ok(store::Secret::new(TestSecret))
        } else {
            Err(store::SecretsError::NotFound)
        }
    }
}

impl reveal::Guest for Component {
    fn reveal(_secret: store::SecretBorrow<'_>) -> store::SecretValue {
        store::SecretValue::String("fixture-token".to_string())
    }
}

export!(Component);
