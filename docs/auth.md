# Authentication

Many APIs require a credential on every request. Rather than threading a token through
every operation signature, generated components keep operations **auth-free** and inject
credentials centrally at runtime. Each generated component:

- imports [`wasmcloud:secrets`][secrets] and reads its credentials from the host, and
- imports `wasi:http` and applies the credential to each outgoing request.

The generator reads the document's `securitySchemes` / `security` and emits a per-operation
auth table that the embedded runtime consumes. Operation arguments never carry a token.

## Secret-key contract

The key the runtime fetches from `wasmcloud:secrets` is the **security scheme's name** in
`components.securitySchemes`. A host provisioning credentials must store each secret under
that name. For example, given:

```jsonc
"securitySchemes": {
  "bearerAuth":   { "type": "http",   "scheme": "bearer" },
  "apiKeyHeader": { "type": "apiKey", "in": "header", "name": "X-API-Key" }
}
```

the host provisions secrets named `bearerAuth` and `apiKeyHeader`. They are applied as:

| OpenAPI scheme | Applied to the request as |
|---|---|
| `http` `bearer` (and `oauth2` / `openIdConnect`, pre-acquired token) | `Authorization: Bearer <secret>` |
| `http` `basic` | `Authorization: Basic base64(<secret>)`, secret = `user:pass` |
| `apiKey` `in: header` | header `<name>: <secret>` |
| `apiKey` `in: query` | query `<name>=<secret>` |
| `apiKey` `in: cookie` | `Cookie: <name>=<secret>` |

A per-operation `security` overrides the document default; `security: []` disables auth for
that operation. When an operation lists several alternative requirements (OR semantics), the
first is used. OAuth2/OIDC token flows are not run — the host supplies a pre-acquired access
token as a bearer secret.

[secrets]: https://github.com/wasmCloud/wasmCloud/tree/main/wit/secrets
