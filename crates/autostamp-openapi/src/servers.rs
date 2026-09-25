//! Base-URL resolution from an OpenAPI document's `servers` list.
//!
//! The generated runtime needs a concrete base URL to build absolute request URLs. We
//! take the document-level first server, substitute any `{variable}` templates with their
//! declared defaults, and strip a trailing slash so it concatenates cleanly with the
//! `/`-prefixed operation paths.

use openapiv3::OpenAPI;

/// Resolve the base URL for `spec`, or `None` when the document declares no usable server.
///
/// Server-variable templates (`{var}`) are replaced with each variable's `default`. A
/// trailing `/` is removed so `base + path` does not double the separator.
pub(crate) fn resolve_base_url(spec: &OpenAPI) -> Option<String> {
    let server = spec.servers.first()?;
    let mut url = server.url.clone();

    if let Some(vars) = &server.variables {
        for (name, var) in vars {
            url = url.replace(&format!("{{{name}}}"), &var.default);
        }
    }

    let url = url.trim_end_matches('/').to_string();
    if url.is_empty() { None } else { Some(url) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> OpenAPI {
        crate::parse_openapi(json).unwrap()
    }

    #[test]
    fn plain_server_url() {
        let s = spec(
            r#"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "servers": [{ "url": "https://api.example.com/v1/" }],
              "paths": {}
            }"#,
        );
        assert_eq!(
            resolve_base_url(&s).as_deref(),
            Some("https://api.example.com/v1")
        );
    }

    #[test]
    fn substitutes_server_variables() {
        let s = spec(
            r#"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "servers": [{
                "url": "https://{host}/{basePath}",
                "variables": {
                  "host": { "default": "api.example.com" },
                  "basePath": { "default": "v2" }
                }
              }],
              "paths": {}
            }"#,
        );
        assert_eq!(
            resolve_base_url(&s).as_deref(),
            Some("https://api.example.com/v2")
        );
    }

    #[test]
    fn no_servers_is_none() {
        let s = spec(
            r#"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "paths": {}
            }"#,
        );
        assert_eq!(resolve_base_url(&s), None);
    }
}
