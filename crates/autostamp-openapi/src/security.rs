//! Security-scheme modeling.
//!
//! Lowers an OpenAPI document's `securitySchemes` and per-operation `security`
//! requirements into a small [`AuthApply`] model. Codegen emits this model as a static
//! table on each operation, and the generated runtime consumes it: for every
//! [`AuthApply`] it fetches the named secret from `wasmcloud:secrets` and applies the
//! revealed value to the outgoing request according to the [`AuthKind`].
//!
//! Credentials never appear in operation signatures. The secret *key* (the name under
//! which a host provisions the value) is the security scheme's name in
//! `components.securitySchemes`; this is the contract documented for component hosts.

use openapiv3::{APIKeyLocation, OpenAPI, Operation, ReferenceOr, SecurityScheme};

/// How a resolved security scheme is applied to an outgoing HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthKind {
    /// `Authorization: Bearer <secret>`. Also used for `oauth2` / `openIdConnect`, where
    /// the host is expected to provision a pre-acquired access token.
    Bearer,
    /// `Authorization: Basic base64(<secret>)`, where the secret holds `user:pass`.
    Basic,
    /// An API key carried in a request header named `name`.
    ApiKeyHeader { name: String },
    /// An API key carried in a URL query parameter named `name`.
    ApiKeyQuery { name: String },
    /// An API key carried in a cookie named `name`.
    ApiKeyCookie { name: String },
}

impl AuthKind {
    /// A short, markdown-flavored description of how the revealed secret is attached to each
    /// outgoing request. Shared by the README's authentication table and the WIT package
    /// comment so the two never drift. The wire name (for `apiKey` schemes) is wrapped in
    /// backticks so it renders as code.
    pub(crate) fn applied_as(&self) -> String {
        match self {
            AuthKind::Bearer => "`Authorization: Bearer <secret>`".to_string(),
            AuthKind::Basic => "`Authorization: Basic base64(<secret>)`".to_string(),
            AuthKind::ApiKeyHeader { name } => format!("header `{name}`"),
            AuthKind::ApiKeyQuery { name } => format!("query `{name}`"),
            AuthKind::ApiKeyCookie { name } => format!("cookie `{name}`"),
        }
    }
}

/// A single security scheme to apply to a request, paired with the secret key the runtime
/// fetches from `wasmcloud:secrets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthApply {
    /// The key passed to `wasmcloud:secrets` `store.get(...)`. Equal to the security
    /// scheme's name in `components.securitySchemes`.
    pub(crate) secret_key: String,
    /// How the revealed secret value is attached to the request.
    pub(crate) kind: AuthKind,
}

/// The named security schemes declared in a document's `components.securitySchemes`,
/// lowered to [`AuthKind`]s.
#[derive(Debug, Default)]
pub(crate) struct SecurityRegistry {
    schemes: Vec<(String, AuthKind)>,
}

impl SecurityRegistry {
    /// Build the registry from an OpenAPI document. Schemes that cannot be resolved or
    /// that we do not support contribute nothing (they are simply skipped).
    pub(crate) fn from_spec(spec: &OpenAPI) -> Self {
        let mut schemes = Vec::new();
        if let Some(components) = &spec.components {
            for (name, scheme_ref) in &components.security_schemes {
                if let ReferenceOr::Item(scheme) = scheme_ref
                    && let Some(kind) = scheme_to_kind(scheme)
                {
                    schemes.push((name.clone(), kind));
                }
            }
        }
        Self { schemes }
    }

    fn lookup(&self, name: &str) -> Option<&AuthKind> {
        self.schemes.iter().find(|(n, _)| n == name).map(|(_, k)| k)
    }

    /// Resolve the effective auth for an operation. The operation's own `security`
    /// overrides the document-level default; an explicit empty list (`security: []`)
    /// disables auth for that operation.
    pub(crate) fn operation_requirement(&self, spec: &OpenAPI, op: &Operation) -> Vec<AuthApply> {
        match op.security.as_deref() {
            Some(reqs) => self.first_satisfiable(reqs),
            None => match spec.security.as_deref() {
                Some(reqs) => self.first_satisfiable(reqs),
                None => vec![],
            },
        }
    }

    /// From a list of alternative requirements (OR semantics), return the schemes of the
    /// first requirement whose every scheme we can resolve (AND semantics within a
    /// requirement). An empty requirement object means "no auth" and short-circuits.
    fn first_satisfiable(&self, reqs: &[openapiv3::SecurityRequirement]) -> Vec<AuthApply> {
        for req in reqs {
            if req.is_empty() {
                return vec![];
            }
            let mut applies = Vec::with_capacity(req.len());
            let mut all_resolved = true;
            for name in req.keys() {
                match self.lookup(name) {
                    Some(kind) => applies.push(AuthApply {
                        secret_key: name.clone(),
                        kind: kind.clone(),
                    }),
                    None => {
                        all_resolved = false;
                        break;
                    }
                }
            }
            if all_resolved {
                return applies;
            }
        }
        vec![]
    }
}

/// Map an OpenAPI [`SecurityScheme`] to an [`AuthKind`], or `None` when unsupported.
fn scheme_to_kind(scheme: &SecurityScheme) -> Option<AuthKind> {
    match scheme {
        SecurityScheme::APIKey { location, name, .. } => Some(match location {
            APIKeyLocation::Header => AuthKind::ApiKeyHeader { name: name.clone() },
            APIKeyLocation::Query => AuthKind::ApiKeyQuery { name: name.clone() },
            APIKeyLocation::Cookie => AuthKind::ApiKeyCookie { name: name.clone() },
        }),
        SecurityScheme::HTTP { scheme, .. } => {
            if scheme.eq_ignore_ascii_case("basic") {
                Some(AuthKind::Basic)
            } else {
                // `bearer`, and any other token-style HTTP scheme, carry an opaque token
                // in the Authorization header; treat them as bearer.
                Some(AuthKind::Bearer)
            }
        }
        // OAuth2 / OIDC: we do not run token flows; the host provisions a pre-acquired
        // access token which is sent as a bearer token.
        SecurityScheme::OAuth2 { .. } | SecurityScheme::OpenIDConnect { .. } => {
            Some(AuthKind::Bearer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> OpenAPI {
        crate::parse_openapi(json).unwrap()
    }

    fn op(spec: &OpenAPI, path: &str) -> Operation {
        spec.paths
            .paths
            .get(path)
            .and_then(|p| p.as_item())
            .and_then(|p| p.get.clone())
            .expect("operation")
    }

    const BEARER: &str = r#"{
      "openapi": "3.0.0",
      "info": { "title": "t", "version": "1" },
      "components": {
        "securitySchemes": {
          "bearerAuth": { "type": "http", "scheme": "bearer" },
          "basicAuth": { "type": "http", "scheme": "basic" },
          "apiKeyHeader": { "type": "apiKey", "in": "header", "name": "X-API-Key" },
          "apiKeyQuery": { "type": "apiKey", "in": "query", "name": "api_key" }
        }
      },
      "security": [{ "bearerAuth": [] }],
      "paths": {
        "/default": { "get": { "tags": ["t"], "operationId": "useDefault", "responses": { "200": { "description": "ok" } } } },
        "/override": { "get": { "tags": ["t"], "operationId": "useOverride", "security": [{ "apiKeyHeader": [] }], "responses": { "200": { "description": "ok" } } } },
        "/none": { "get": { "tags": ["t"], "operationId": "useNone", "security": [], "responses": { "200": { "description": "ok" } } } },
        "/query": { "get": { "tags": ["t"], "operationId": "useQuery", "security": [{ "apiKeyQuery": [] }], "responses": { "200": { "description": "ok" } } } },
        "/basic": { "get": { "tags": ["t"], "operationId": "useBasic", "security": [{ "basicAuth": [] }], "responses": { "200": { "description": "ok" } } } }
      }
    }"#;

    #[test]
    fn operation_inherits_global_security() {
        let s = spec(BEARER);
        let reg = SecurityRegistry::from_spec(&s);
        let auth = reg.operation_requirement(&s, &op(&s, "/default"));
        assert_eq!(
            auth,
            vec![AuthApply {
                secret_key: "bearerAuth".into(),
                kind: AuthKind::Bearer
            }]
        );
    }

    #[test]
    fn operation_security_overrides_global() {
        let s = spec(BEARER);
        let reg = SecurityRegistry::from_spec(&s);
        let auth = reg.operation_requirement(&s, &op(&s, "/override"));
        assert_eq!(
            auth,
            vec![AuthApply {
                secret_key: "apiKeyHeader".into(),
                kind: AuthKind::ApiKeyHeader {
                    name: "X-API-Key".into()
                }
            }]
        );
    }

    #[test]
    fn empty_operation_security_disables_auth() {
        let s = spec(BEARER);
        let reg = SecurityRegistry::from_spec(&s);
        assert!(reg.operation_requirement(&s, &op(&s, "/none")).is_empty());
    }

    #[test]
    fn api_key_query_and_basic_resolve() {
        let s = spec(BEARER);
        let reg = SecurityRegistry::from_spec(&s);
        assert_eq!(
            reg.operation_requirement(&s, &op(&s, "/query")),
            vec![AuthApply {
                secret_key: "apiKeyQuery".into(),
                kind: AuthKind::ApiKeyQuery {
                    name: "api_key".into()
                }
            }]
        );
        assert_eq!(
            reg.operation_requirement(&s, &op(&s, "/basic")),
            vec![AuthApply {
                secret_key: "basicAuth".into(),
                kind: AuthKind::Basic
            }]
        );
    }

    #[test]
    fn no_security_anywhere_is_empty() {
        let s = spec(
            r#"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "paths": {
                "/x": { "get": { "tags": ["t"], "operationId": "x", "responses": { "200": { "description": "ok" } } } }
              }
            }"#,
        );
        let reg = SecurityRegistry::from_spec(&s);
        assert!(reg.operation_requirement(&s, &op(&s, "/x")).is_empty());
    }
}
