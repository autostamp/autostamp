<h1 align="center">autostamp-openapi</h1>
<div align="center">
  <strong>
    Convert OpenAPI schema definitions to WebAssembly Components
  </strong>
</div>

<br />

## About

[WebAssembly Components][component] are universal, portable libraries which can be linked to
by any other language. This project exists to automate library creation by
taking OpenAPI schema definitions and creating WebAssembly guest components from
them.

The goal of this project is to _automate_ Component-based SDK creation as much
as possible. Hand-crafted Components are likely to be nicer than what we can
achieve with automation here. But 

## Usage

With the [component CLI](https://github.com/yoshuawuyts/component-registry) installed:
```sh
# Generate an `acme:api` component from an OpenAPI schema
$ component run autostamp:openapi acme.json build/acme acme:api
```


## Authentication

Many APIs require a credential on every request. Rather than threading a token through
every operation signature, generated components keep operations **auth-free** and inject
credentials centrally at runtime. Each generated component:

- imports [`wasmcloud:secrets`][secrets] and reads its credentials from the host, and
- imports `wasi:http` and applies the credential to each outgoing request.

The generator reads the document's `securitySchemes` / `security` and emits a per-operation
auth table that the embedded runtime consumes. Operation arguments never carry a token.

**Secret-key contract.** The key the runtime fetches from `wasmcloud:secrets` is the
**security scheme's name** in `components.securitySchemes`. A host provisioning credentials
must store each secret under that name. For example, given:

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


## Safety
This crate denies `unsafe_code` throughout. The single exception is the
wit-bindgen-generated component bindings, which require the component-model FFI glue and
are confined to one module behind `#[allow(unsafe_code)]`.

## Contributing
Want to join us? Check out our ["Contributing" guide][contributing] and take a
look at some of these issues:

- [Issues labeled "good first issue"][good-first-issue]
- [Issues labeled "help wanted"][help-wanted]

[contributing]: https://github.com/yoshuawuyts/openapi-bindgen/blob/master/.github/CONTRIBUTING.md
[good-first-issue]: https://github.com/yoshuawuyts/openapi-bindgen/labels/good%20first%20issue
[help-wanted]: https://github.com/yoshuawuyts/openapi-bindgen/labels/help%20wanted

## License

<sup>
Licensed under the <a href="LICENSE-APACHE">Apache License, Version 2.0 with the
LLVM exception</a>.
</sup>

<br/>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

[component]: https://component-model.bytecodealliance.org/
