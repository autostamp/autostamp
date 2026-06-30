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

## Credential deduplication

Some documents declare a credential **twice**: once as a `securityScheme` and again as a
redundant request property (body field, query, or header) on many operations. The Plaid API,
for instance, declares `clientId` and `secret` as `apiKey` headers *and* repeats them as
optional body properties on nearly every request.

Because the runtime already injects those credentials, carrying them in operation arguments
too would force every caller to supply a secret the host provides. The generator therefore
**prunes request fields that duplicate an injected credential**, on by default. A non-path
field is dropped when its name matches the security scheme's name or, for `apiKey` schemes,
the credential's wire name (compared in `snake_case`). The operation's auth table is
unaffected — the credential is still injected centrally.

This is a name-based heuristic: a legitimate request field that happens to share a name with
an active security scheme would also be pruned. Path parameters are structural and never
pruned.

[secrets]: https://github.com/wasmCloud/wasmCloud/tree/main/wit/secrets
