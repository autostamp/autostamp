<h1 align="center">autostamp-openapi</h1>
<div align="center">
  <strong>
    Convert OpenAPI schema definitions to WebAssembly Components
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

## Usage

With the [component CLI](https://github.com/yoshuawuyts/component-registry) installed:
```sh
# Generate an `acme:api` component from an OpenAPI schema
$ component run autostamp:openapi acme.json build/acme acme:api
```

## Known limitations

The generator accepts OpenAPI 2 (Swagger) and 3.0 documents and normalizes a range of
real-world quirks — missing `operationId`s, untagged operations, deep `$ref`s into a component's
`properties`/`items`, whole-document `$ref`s into `#/paths/...`, out-of-range numeric bounds, and
stray control characters. A document with no operations (e.g. an empty `paths: {}`) has nothing to
bind and is reported as a skip. A few input shapes are still unsupported and will fail to generate:

- **OpenAPI 3.1 documents.** The underlying `openapiv3` parser targets 3.0; 3.1-only constructs
  (type arrays, `const`, sibling `$ref`s) aren't deserialized yet.
- **Malformed source documents.** Specs whose YAML is structurally invalid for a strict parser
  (e.g. inconsistent block-scalar indentation) can't be loaded.

## Documentation

- [Authentication](./docs/auth.md)

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
