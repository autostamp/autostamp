<h1 align="center">autostamp</h1>
<div align="center">
  <strong>
    Generate WebAssembly component libraries from API and tool definitions
  </strong>
</div>

<br />

## About

[WebAssembly Components][component] are universal, portable libraries which can linked to
any other language. This project exists to automate library creation by
taking OpenAPI schema definitions and creating WebAssembly guest components from
them.

The goal of this project is to _automate_ Component-based SDK creation as much
as possible. Hand-crafted components can probably be nicer than what we can
achieve with automation here. But those SDKs currently don't exist, and we believe that it's better to have something that works than nothing at all.

> [!IMPORTANT]
> This project is not affiliated with any of the projects or entities we publish Wasm Components for. We take publicly available schemas and use open source tools to turn them into publicly available libraries. 

## Documentation

- [Building and publishing](./docs/building-and-publishing.md)
- [MCP discovery and client generation](./docs/mcp.md)
- [Authentication](./docs/auth.md)
- [Known limitations](./docs/known-limitations.md)

## Workspace

The two generators live under `crates/`:

- [`autostamp-openapi`](./crates/autostamp-openapi/): OpenAPI 2 and 3 to component
  source crates, previously named `openapi-bindgen`.
- [`autostamp-mcp`](./crates/autostamp-mcp/): complete MCP tool catalogs to typed
  WIT and standalone Rust clients for MCP 2025-11-25 Streamable HTTP.
  The opt-in native CLI discovers live servers or reads offline catalogs;
  see the [MCP guide](./docs/mcp.md).

Generated provider crates in `components/` are standalone packages, deliberately
excluded from the workspace. `just test` checks the generators; `just build`
regenerates and builds the OpenAPI component corpus.
`just gen-mcp`, `just build-mcp` and `just test-mcp` are separate MCP entry points;
they do not add fixtures to `components/` or change the OpenAPI publish flow.

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
