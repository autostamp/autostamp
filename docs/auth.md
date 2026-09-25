# Authentication

This page describes OpenAPI credentials first. MCP discovery tokens and generated
runtime secrets have a separate [MCP credential contract](#mcp-credentials).

Many APIs require a credential on every request. Rather than threading a token through
every operation signature, generated components keep operations **auth-free** and inject
credentials centrally at runtime. Each generated component:

- imports [`wasmcloud:secrets`][secrets] and reads its credentials from the host, and
- performs each request with the [`wstd`][wstd] HTTP client (which imports `wasi:http`) and
  applies the credential to every outgoing request.

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

## MCP credentials

The [MCP CLI](./mcp.md#discovery-credentials-and-runtime-credentials) deliberately
separates discovery authentication from the generated client's runtime credentials:

- `--token-env ENV_VAR` reads a bearer token from the native process environment
  for **live discovery only**. It is rejected in offline `--catalog` mode. The
  token value is not embedded in generated artifacts or provisioned to the host.
- `--bearer-secret KEY` embeds only a **secret key name**, not a credential.
  The generated client imports `wasmcloud:secrets@1.0.0` and obtains a string or
  UTF-8 byte token from the host at call time, applying it as a bearer credential.
  Provision this key independently of the discovery environment variable.

These options are independent and may be used together. A client generated
without `--bearer-secret` is anonymous and has no secrets imports or secret WIT
dependencies, even if discovery needed `--token-env`. Conversely, offline
generation may request a bearer-secret key without any discovery credential.
Unlike OpenAPI, the key is explicitly chosen rather than derived from a
`securitySchemes` name. OAuth token acquisition is not performed in either case.

[secrets]: https://github.com/wasmCloud/wasmCloud/tree/main/wit/secrets
[wstd]: https://github.com/bytecodealliance/wstd
